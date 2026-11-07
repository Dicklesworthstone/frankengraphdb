//! Real Warden sessions drive the existing indexed probe cursor over committed
//! MemVfs generations. These tests do not establish physical noninterference.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error, Grant, LimitDimension, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x73; 32]);
const BRANCH: &str = "host-main";
fn issuer() -> Authority {
    Authority::new(
        AuthKey::from_seed(7301),
        NS,
        "host-graph",
        SchemaEpoch(0),
        1,
    )
    .unwrap()
}
fn grant() -> Grant {
    let mut value = Grant::read_only(
        BRANCH,
        1000,
        QueryLimits {
            max_nodes: 1_000_000,
            max_work: 10_000_000,
            max_rows: 1000,
        },
    );
    value.labels = Scope::only([LabelId(1)]);
    value.relations = Scope::only([RelationId(1)]);
    value.properties = Scope::only([PropertyKeyId(1)]);
    value
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1000, 10_000_000, 10_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "H") => Some(GraphSymbol::Label(LabelId(99))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x18; 32], NS, [0x29; 32]))
        .await
        .unwrap();
    let mut seed = WriteBatch::new(RelationId(1));
    for (id, labels) in [
        (1, vec![LabelId(1), LabelId(99)]),
        (2, vec![LabelId(99)]),
        (3, vec![LabelId(1)]),
        (4, vec![LabelId(1)]),
    ] {
        seed.create_vertex(
            VId(id),
            labels,
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(id as i64 * 10)),
                (PropertyKeyId(2), CanonicalScalar::Int(999)),
            ],
        );
    }
    for (eid, a, b) in [
        (10, 1, 2),
        (11, 2, 4),
        (12, 1, 3),
        (13, 1, 3),
        (14, 3, 1),
        (15, 3, 3),
    ] {
        seed.add_edge(
            EId(eid),
            VId(a),
            VId(b),
            vec![(PropertyKeyId(2), CanonicalScalar::Int(999))],
        );
    }
    db.write(cx, seed).await.unwrap();
    let mut forbidden = WriteBatch::new(RelationId(2));
    forbidden.add_edge(EId(20), VId(1), VId(4), vec![]);
    db.write(cx, forbidden).await.unwrap();
    db
}
fn expected(ids: &[u128]) -> Vec<GraphValueRow> {
    ids.iter()
        .map(|&id| {
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::Scalar(CanonicalScalar::Int(id as i64 * 10)),
            ])
        })
        .collect()
}
fn native(columns: Vec<String>, rows: &[GraphValueRow]) -> QueryResult {
    QueryResult::Rows {
        columns,
        rows: rows
            .iter()
            .map(|row| {
                row.values()
                    .iter()
                    .cloned()
                    .map(QueryValue::Value)
                    .collect()
            })
            .collect(),
    }
}

#[test]
fn scoped_fixed_and_independent_probes_match_goldens_and_eager_results_before_paging() {
    let ((), report) = run_async_under_lab(0x5ec0_7301, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let args = GqlParameters::new();
        for (condition, answer) in [
            ("EXISTS { MATCH (a)-[:R]->(b) }", vec![1, 3]),
            ("EXISTS { MATCH (a)-->(b) }", vec![1, 3]),
            ("NOT EXISTS { MATCH (a)-->(b) }", vec![4]),
            ("EXISTS { MATCH (a)<--(b) }", vec![1, 3]),
            ("EXISTS { MATCH (a)--(b) }", vec![1, 3]),
            ("EXISTS { MATCH (a)-->(b)-[:R]->(c) }", vec![1, 3]),
            ("NOT EXISTS { MATCH (a)-[:R]->(b) }", vec![4]),
            ("EXISTS { MATCH (a)<-[:R]-(b) }", vec![1, 3]),
            ("EXISTS { MATCH (a)-[:R]-(b) }", vec![1, 3]),
            ("EXISTS { MATCH (a)-[:S]->(b) }", vec![]),
            ("NOT EXISTS { MATCH (a)-[:S]->(b) }", vec![1, 3, 4]),
            ("EXISTS { MATCH (a)-[:R]->(b:H) }", vec![]),
            (
                "EXISTS { MATCH (a)-[:R]->(b) WHERE b.hidden IS NULL }",
                vec![1, 3],
            ),
            (
                "EXISTS { MATCH (a)-[:R]->(b) WHERE b.hidden = 999 }",
                vec![],
            ),
            (
                "EXISTS { MATCH (a)-[:R]->(b) WHERE NOT (b.hidden = 999) }",
                vec![],
            ),
            (
                "NOT EXISTS { MATCH (a)-[:R]->(b) WHERE b.hidden = 999 }",
                vec![1, 3, 4],
            ),
            ("EXISTS { MATCH (a)-[:R]->(b) WHERE b.p > a.p }", vec![1]),
            ("EXISTS { MATCH (b) WHERE b.p = 20 }", vec![]),
            ("EXISTS { MATCH (b) WHERE b.p = 40 }", vec![1, 3, 4]),
            (
                "NOT EXISTS { MATCH (b) WHERE b.hidden = 999 }",
                vec![1, 3, 4],
            ),
        ] {
            for (skip, take) in [(0, 10), (1, 1), (0, 0)] {
                let text = format!(
                    "MATCH (a) WHERE {condition} RETURN a AS id, a.p AS p SKIP {skip} LIMIT {take}"
                );
                let prepared = session.prepare(&cx, &text, &args).unwrap();
                let eager = session.execute(&cx, &prepared, &args).unwrap();
                let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
                let columns = cursor.columns().to_vec();
                let got = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                let wanted = expected(&answer)
                    .into_iter()
                    .skip(skip)
                    .take(take)
                    .collect::<Vec<_>>();
                assert_eq!(got, wanted, "{text}");
                assert_eq!(native(columns, &got), eager);
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
                assert!(cursor.next().is_none());
            }
        }
        // Independent golden: a privileged negative control sees the private
        // property and hidden transit vertex that the scoped source removes.
        let mut broad = grant();
        broad.labels = Scope::All;
        broad.relations = Scope::All;
        broad.properties = Scope::All;
        let broad = authority.issue_at(&broad, 100).unwrap();
        let mut raw = db
            .authorized_read_session(&cx, &authority, &broad, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let text = "MATCH (a) WHERE EXISTS { MATCH (b) WHERE b.p = 20 } RETURN a AS id, a.p AS p";
        assert_ne!(
            raw.query(&cx, text, &args).unwrap(),
            session.query(&cx, text, &args).unwrap()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bounded_walks_do_not_cross_hidden_transit_vertices_and_zero_hops_need_no_relation() {
    let ((), report) = run_async_under_lab(0x5ec0_7302, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let args = GqlParameters::new();
        for (condition, answer) in [
            (
                "EXISTS { MATCH (a)-[:R]->(x)-[:R]->(b) WHERE b.p = 40 }",
                vec![],
            ),
            ("EXISTS { MATCH (a)-[:R*2..3]->(b) WHERE b.p = 40 }", vec![]),
            (
                "NOT EXISTS { MATCH (a)-[:R*2..3]->(b) WHERE b.p = 40 }",
                vec![1, 3, 4],
            ),
            (
                "EXISTS { MATCH (a)-[:R*1..3]->(b) WHERE b.hidden IS NULL }",
                vec![1, 3],
            ),
            ("EXISTS { MATCH (a)-[:S*1..3]->(b) }", vec![]),
            ("EXISTS { MATCH (a)-[:S*0..0]->(b) }", vec![1, 3, 4]),
            (
                "EXISTS { MATCH (a)-[:S*0..3]->(b) WHERE b = a }",
                vec![1, 3, 4],
            ),
        ] {
            let text = format!("MATCH (a) WHERE {condition} RETURN a AS id, a.p AS p");
            let prepared = session.prepare(&cx, &text, &args).unwrap();
            let eager = session.execute(&cx, &prepared, &args).unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            let columns = cursor.columns().to_vec();
            let got = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(got, expected(&answer), "{text}");
            assert_eq!(native(columns, &got), eager);
        }
        let low = GqlParameters::new().with_int64("wanted", 30).unwrap();
        let high = GqlParameters::new().with_int64("wanted", 40).unwrap();
        let query = session.prepare(&cx,
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) WHERE b.p = $wanted } RETURN a AS id, a.p AS p", &low).unwrap();
        for (args, answer) in [(&low, vec![1, 3]), (&high, vec![]), (&low, vec![1, 3])] {
            let got = session
                .stream(&cx, &query, args)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(got, expected(&answer));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_paused_probe_cursor_retains_its_historical_generation_through_writes_compaction_and_drop() {
    // The cursor is deliberately !Send. Keep it on this thread while ordinary
    // production-runtime block_on drives separate writer futures; do not hold
    // it across an await inside the Send-requiring lab runner.
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let c = PurposeContexts::narrow_runtime_root(&root);
    let cx = c.query();
    let commit = c.commit();
    let mut db = runtime.block_on(database(&commit));
    let authority = issuer();
    let token = authority.issue_at(&grant(), 100).unwrap();
    let clock_calls = Cell::new(0usize);
    let mut session = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || {
            clock_calls.set(clock_calls.get() + 1);
            100
        })
        .unwrap();
    let args = GqlParameters::new();
    let text = "MATCH (a) WHERE EXISTS { MATCH (a)-[:R*2..3]->(b) WHERE b.p = 40 } RETURN a AS id, a.p AS p";
    let query = session.prepare(&cx, text, &args).unwrap();
    let mut cursor = session.stream(&cx, &query, &args).unwrap();
    let basis = cursor.snapshot_seq();
    drop(query);
    let before = clock_calls.get();
    let mut change = WriteBatch::new(RelationId(1));
    change.set_vertex_label(VId(2), LabelId(1), true);
    runtime.block_on(db.write(&commit, change)).unwrap();
    runtime.block_on(db.compact(&commit)).unwrap();
    assert_eq!(
        clock_calls.get(),
        before,
        "paused cursor performed query work"
    );
    assert!(db.frontier().unwrap() > basis);
    let mut fresh = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
        .unwrap();
    let prepared = fresh.prepare(&cx, text, &args).unwrap();
    let now = fresh
        .stream(&cx, &prepared, &args)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(now, expected(&[1, 3]));
    let past = format!(
        "MATCH (a) FOR SYSTEM_TIME AS OF SEQ {} WHERE EXISTS {{ MATCH (a)-[:R*2..3]->(b) WHERE b.p = 40 }} RETURN a AS id, a.p AS p",
        basis.0
    );
    let prepared = fresh.prepare(&cx, &past, &args).unwrap();
    assert!(
        fresh
            .stream(&cx, &prepared, &args)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .is_empty()
    );
    drop(db);
    drop(token);
    assert!(cursor.next().is_none());
    assert_eq!(cursor.state(), VertexScanState::Exhausted);
    drop(cursor);
    assert!(!session.is_closed());
}

#[test]
fn probe_node_record_and_delivery_limits_are_cumulative_and_retryable_after_failure() {
    let ((), report) = run_async_under_lab(0x5ec0_7304, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let authority = issuer();
        let args = GqlParameters::new();
        for (signed, native_policy, expected_dimension) in [
            (
                QueryLimits {
                    max_nodes: 1,
                    ..grant().limits
                },
                policy(),
                Some(LimitDimension::Nodes),
            ),
            (
                QueryLimits {
                    max_rows: 0,
                    ..grant().limits
                },
                policy(),
                Some(LimitDimension::Rows),
            ),
            (
                grant().limits,
                GqlQueryPolicy::new(1, 1000, 10_000_000, 10_000_000),
                None,
            ),
        ] {
            let mut allowed = grant();
            allowed.limits = signed;
            let token = authority.issue_at(&allowed, 100).unwrap();
            let mut session = db
                .authorized_read_session(
                    &cx,
                    &authority,
                    &token,
                    BRANCH,
                    symbols,
                    native_policy,
                    || 100,
                )
                .unwrap();
            let prepared = session.prepare(&cx, "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) } RETURN a AS id, a.p AS p LIMIT 1", &args).unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            match (cursor.next(), expected_dimension) {
                (
                    Some(Err(QueryError::Authorization(Error::LimitExceeded(actual)))),
                    Some(wanted),
                ) => assert_eq!(actual, wanted),
                (Some(Err(QueryError::Stream(GqlQueryError::Rows(_)))), None) => {}
                (other, _) => panic!("wrong probe admission failure: {other:?}"),
            }
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
            drop(cursor);
            assert!(!session.is_closed());
            let zero = session
                .prepare(
                    &cx,
                    "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) } RETURN a AS id LIMIT 0",
                    &args,
                )
                .unwrap();
            assert!(session.stream(&cx, &zero, &args).unwrap().next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_probe_poll_expiry_cut_retains_only_delivered_prefixes_and_closes_the_session() {
    let ((), report) = run_async_under_lab(0x5ec0_7305, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let calls = Cell::new(0usize);
        let active = Cell::new(false);
        let stop = Cell::new(usize::MAX);
        let clock = || {
            if active.get() {
                calls.set(calls.get() + 1);
            }
            if active.get() && calls.get() == stop.get() {
                1000
            } else {
                100
            }
        };
        let text = "MATCH (a) WHERE EXISTS { MATCH (a)-[:R*1..2]->(b) WHERE b.hidden IS NULL } RETURN a AS id, a.p AS p";
        let count = {
            let mut session = db
                .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
                .unwrap();
            let prepared = session.prepare(&cx, text, &args).unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            active.set(true);
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected(&[1, 3])
            );
            active.set(false);
            calls.get()
        };
        assert!(count > 0);
        for cut in 1..=count {
            active.set(false);
            calls.set(0);
            stop.set(cut);
            let mut session = db
                .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
                .unwrap();
            let prepared = session.prepare(&cx, text, &args).unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            active.set(true);
            let mut prefix = Vec::new();
            loop {
                match cursor.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(QueryError::Authorization(Error::Expired))) => break,
                    other => panic!("expected expiry at {cut}, got {other:?}"),
                }
            }
            assert!(expected(&[1, 3]).starts_with(&prefix));
            assert_eq!(calls.get(), cut);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
            assert_eq!(calls.get(), cut);
            active.set(false);
            drop(cursor);
            assert!(session.is_closed());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nested_existence_applies_capability_masks_at_every_scope() {
    let ((), report) = run_async_under_lab(0x5ec0_7309, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let authority = issuer();
        let limited = authority.issue_at(&grant(), 100).unwrap();
        let mut broad = grant();
        broad.labels = Scope::All;
        broad.relations = Scope::All;
        broad.properties = Scope::All;
        let broad = authority.issue_at(&broad, 100).unwrap();
        let mut scoped = db
            .authorized_read_session(&cx, &authority, &limited, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let mut raw = db
            .authorized_read_session(&cx, &authority, &broad, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let args = GqlParameters::new();
        for (condition, permitted, privileged) in [
            (
                "EXISTS { MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (b)-[:R]->(c) WHERE c.p = 40 } }",
                vec![],
                vec![1],
            ),
            (
                "EXISTS { MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (b)-[:S]->(c) } }",
                vec![],
                vec![3],
            ),
            (
                "EXISTS { MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (c) WHERE c=b AND c.hidden=999 } }",
                vec![],
                vec![1, 2, 3],
            ),
            (
                "NOT EXISTS { MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (b)-[:S]->(c) } }",
                vec![1, 3, 4],
                vec![1, 2, 4],
            ),
            (
                "EXISTS { MATCH (b) WHERE EXISTS { MATCH (a)-[:R]->(b) } }",
                vec![1, 3],
                vec![1, 2, 3],
            ),
        ] {
            let text = format!("MATCH (a) WHERE {condition} RETURN a AS id,a.p AS p");
            let columns = vec!["id".to_owned(), "p".to_owned()];
            assert_eq!(
                scoped.query(&cx, &text, &args).unwrap(),
                native(columns.clone(), &expected(&permitted)),
                "{text}"
            );
            assert_eq!(
                raw.query(&cx, &text, &args).unwrap(),
                native(columns, &expected(&privileged)),
                "{text}"
            );
            // This change admits the native buffered Probe executor. The
            // separately bounded streaming profile still refuses nested probes
            // at preparation instead of publishing a partial interpretation.
            let prepared = scoped.prepare(&cx, &text, &args).unwrap();
            assert!(matches!(
                scoped.stream(&cx, &prepared, &args),
                Err(QueryError::Stream(GqlQueryError::Source(_))),
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}


// Each visible edge is a distinct witness, including parallel edges and a
// self-loop. The slim database is built without private records or fields;
// its oracle never invokes the production authorization mask.
const COLLECTION_EDGES: [(u128, u128, u128, Option<i64>); 5] = [
    (12, 1, 3, Some(2)),
    (13, 1, 3, Some(4)),
    (14, 3, 1, Some(8)),
    (15, 3, 3, None),
    (16, 1, 4, Some(16)),
];

async fn collection_database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x38; 32], NS, [0x49; 32]))
        .await
        .unwrap();
    let mut seed = WriteBatch::new(RelationId(1));
    for id in [1_u128, 3, 4] {
        let mut labels = vec![LabelId(1)];
        let mut properties = vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64 * 10))];
        if hidden {
            labels.push(LabelId(99));
            properties.push((PropertyKeyId(2), CanonicalScalar::Int(999)));
        }
        seed.create_vertex(VId(id), labels, properties);
    }
    for (eid, from, to, weight) in COLLECTION_EDGES {
        let mut properties: Vec<_> = weight
            .into_iter()
            .map(|weight| (PropertyKeyId(1), CanonicalScalar::Int(weight)))
            .collect();
        if hidden {
            properties.push((PropertyKeyId(2), CanonicalScalar::Int(999)));
        }
        seed.add_edge(EId(eid), VId(from), VId(to), properties);
    }
    if hidden {
        seed.create_vertex(
            VId(2),
            vec![LabelId(99)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(20))],
        );
        for (eid, from, to) in [(10, 1, 2), (11, 2, 4), (17, 2, 1)] {
            seed.add_edge(EId(eid), VId(from), VId(to), vec![]);
        }
    }
    db.write(cx, seed).await.unwrap();
    if hidden {
        let mut forbidden = WriteBatch::new(RelationId(2));
        forbidden.add_edge(
            EId(20),
            VId(3),
            VId(4),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(64))],
        );
        db.write(cx, forbidden).await.unwrap();
    }
    db
}

fn collection_scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}

fn collection_list(values: impl IntoIterator<Item = GraphValue>) -> GraphValue {
    GraphValue::List(values.into_iter().collect::<Vec<_>>().into_boxed_slice())
}

fn collection_expected(outer: usize, inner: usize) -> Vec<GraphValueRow> {
    let mut rows = Vec::new();
    for (eid, from, to, _) in COLLECTION_EDGES {
        let roots = match outer {
            0 => vec![(from, to)],
            1 => vec![(to, from)],
            _ if from == to => vec![(from, to)],
            _ => vec![(from.min(to), from.max(to)), (from.max(to), from.min(to))],
        };
        for (a, b) in roots {
            let witnesses: Vec<_> = COLLECTION_EDGES
                .iter()
                .filter_map(|&(_, from, to, weight)| {
                    let target = match inner {
                        0 if from == b => to,
                        1 if to == b => from,
                        2 if from == b => to,
                        2 if to == b => from,
                        _ => return None,
                    };
                    Some((target, weight))
                })
                .collect();
            rows.push(GraphValueRow::from_owned_values(vec![
                GraphValue::Edge(EId(eid)),
                GraphValue::Vertex(VId(a)),
                GraphValue::Vertex(VId(b)),
                collection_list(
                    witnesses
                        .iter()
                        .map(|&(target, _)| collection_scalar(Some(target as i64 * 10))),
                ),
                collection_list(witnesses.iter().map(|&(_, weight)| collection_scalar(weight))),
                collection_list(witnesses.iter().map(|_| collection_scalar(None))),
                collection_list(witnesses.iter().map(|_| collection_scalar(None))),
            ]));
        }
    }
    rows
}

const COLLECTION_TEXT: &str = "MATCH (a)-[r]->(b) RETURN r,a,b,\
    [(b)-[s]->(c) | c.p] AS values,\
    [(b)-[s]->(c) | s.p] AS weights,\
    [(b)-[s]->(c) | c.hidden] AS hidden_vertices,\
    [(b)-[s]->(c) | s.hidden] AS hidden_edges";

#[test]
fn authorized_projected_collections_match_a_physically_masked_graph_in_every_direction() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let commit = contexts.commit();
    let db = runtime.block_on(collection_database(&commit, true));
    let slim = runtime.block_on(collection_database(&commit, false));
    let authority = issuer();
    let token = authority.issue_at(&grant(), 100).unwrap();
    let args = GqlParameters::new();
    let calls = Cell::new(0_u64);
    let clock = || {
        calls.set(calls.get() + 1);
        100
    };
    let mut scoped = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
        .unwrap();
    let mut visible = slim
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
        .unwrap();
    for (outer, root_pattern) in [
        (0, "(a)-[r]->(b)"),
        (1, "(a)<-[r]-(b)"),
        (2, "(a)-[r]-(b)"),
    ] {
        for (inner, pattern) in [
            (0, "(b)-[s]->(c)"),
            (1, "(b)<-[s]-(c)"),
            (2, "(b)-[s]-(c)"),
        ] {
            let text = format!(
                "MATCH {root_pattern} RETURN r,a,b,\
                 [{pattern} | c.p] AS values,[{pattern} | s.p] AS weights,\
                 [{pattern} | c.hidden] AS hidden_vertices,\
                 [{pattern} | s.hidden] AS hidden_edges"
            );
            let expected = collection_expected(outer, inner);
            let prepared = scoped.prepare(&cx, &text, &args).unwrap();
            let eager = scoped.execute(&cx, &prepared, &args).unwrap();
            let before = calls.get();
            let mut cursor = scoped.stream(&cx, &prepared, &args).unwrap();
            let columns = cursor.columns().to_vec();
            let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            let checkpoints = calls.get() - before;
            assert_eq!(rows, expected, "{text}");
            assert_eq!(native(columns, &rows), eager, "{text}");
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert!(cursor.next().is_none());
            drop(cursor);
            let prepared = visible.prepare(&cx, &text, &args).unwrap();
            let before = calls.get();
            let mut cursor = visible.stream(&cx, &prepared, &args).unwrap();
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected,
                "{text}"
            );
            assert_eq!(
                calls.get() - before,
                checkpoints,
                "private topology or fields changed signed work: {text}"
            );
        }
    }
    for collected in [
        "[(b)-[s:R]->(c:H) | c.p]",
        "[(b)-[s:S]->(c) | c.p]",
        "[(b)-[s:R]->(c) WHERE c.hidden = 999 | c.p]",
        "[(b)-[s:R]->(c) WHERE s.hidden = 999 | s.p]",
    ] {
        let text = format!("MATCH (a)-[r:R]->(b) RETURN r,a,b,{collected} AS values");
        let prepared = scoped.prepare(&cx, &text, &args).unwrap();
        let rows = scoped
            .stream(&cx, &prepared, &args)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let expected: Vec<_> = COLLECTION_EDGES
            .iter()
            .map(|&(eid, from, to, _)| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Edge(EId(eid)),
                    GraphValue::Vertex(VId(from)),
                    GraphValue::Vertex(VId(to)),
                    collection_list([]),
                ])
            })
            .collect();
        assert_eq!(rows, expected, "{text}");
    }
    let mut send = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
        .unwrap();
    let prepared = send.prepare(&cx, COLLECTION_TEXT, &args).unwrap();
    let mut cursor = send.stream_send(&cx, &prepared, &args).unwrap();
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&cursor);
    assert_eq!(
        cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        collection_expected(0, 0)
    );
    drop(cursor);

    let mut broad = grant();
    broad.labels = Scope::All;
    broad.relations = Scope::All;
    broad.properties = Scope::All;
    let broad = authority.issue_at(&broad, 100).unwrap();
    let mut raw = db
        .authorized_read_session(&cx, &authority, &broad, BRANCH, symbols, policy(), || 100)
        .unwrap();
    let prepared = raw.prepare(&cx, COLLECTION_TEXT, &args).unwrap();
    assert_ne!(
        raw.stream(&cx, &prepared, &args)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        collection_expected(0, 0),
        "the full graph must differ from the masked oracle"
    );
}

#[test]
fn projected_collection_limits_and_late_errors_never_publish_partial_lists() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let db = runtime.block_on(collection_database(&contexts.commit(), true));
    let authority = issuer();
    let args = GqlParameters::new();
    for (signed, native_policy, signed_dimension) in [
        (
            QueryLimits {
                max_nodes: 1,
                ..grant().limits
            },
            policy(),
            Some(LimitDimension::Nodes),
        ),
        (
            QueryLimits {
                max_work: 32,
                ..grant().limits
            },
            policy(),
            Some(LimitDimension::Work),
        ),
        (
            QueryLimits {
                max_rows: 0,
                ..grant().limits
            },
            policy(),
            Some(LimitDimension::Rows),
        ),
        (
            grant().limits,
            GqlQueryPolicy::new(1, 1000, 10_000_000, 10_000_000),
            None,
        ),
        (
            grant().limits,
            GqlQueryPolicy::new(1_000_000, 1000, 1, 10_000_000),
            None,
        ),
        (
            grant().limits,
            GqlQueryPolicy::new(1_000_000, 1000, 10_000_000, 1),
            None,
        ),
    ] {
        let mut allowed = grant();
        allowed.limits = signed;
        let token = authority.issue_at(&allowed, 100).unwrap();
        let mut session = db
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                BRANCH,
                symbols,
                native_policy,
                || 100,
            )
            .unwrap();
        let prepared = session.prepare(&cx, COLLECTION_TEXT, &args).unwrap();
        let error = match session.stream(&cx, &prepared, &args) {
            Err(error) => error,
            Ok(mut cursor) => {
                let error = cursor
                    .next()
                    .expect("budget refusal is not EOF")
                    .unwrap_err();
                assert_eq!(cursor.state(), VertexScanState::Failed);
                assert!(cursor.next().is_none());
                error
            }
        };
        match (error, signed_dimension) {
            (QueryError::Authorization(Error::LimitExceeded(actual)), Some(expected)) => {
                assert_eq!(actual, expected);
            }
            (
                QueryError::EdgeStream(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)),
                None,
            ) => {}
            (error, dimension) => panic!("wrong collection refusal: {error:?}, {dimension:?}"),
        }
    }

    let token = authority.issue_at(&grant(), 100).unwrap();
    let mut session = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
        .unwrap();
    let prepared = session
        .prepare(
            &cx,
            "MATCH (a)-[r:R]->(b) RETURN r,a,b,\
             [(b)-[s:R]->(c) | 100 / (c.p - 40)] AS values",
            &args,
        )
        .unwrap();
    let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
    for eid in [12, 13] {
        let row = cursor.next().unwrap().unwrap();
        assert_eq!(row.values()[0], GraphValue::Edge(EId(eid)));
    }
    assert!(matches!(
        cursor.next(),
        Some(Err(QueryError::Set(GqlQueryError::Source(
            fgdb_gql::GraphSetExecutionError::Projection { row: 2, .. }
        ))))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
}

#[test]
fn projected_collections_keep_live_expiry_and_cancellation_at_the_source_boundary() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let db = runtime.block_on(collection_database(&contexts.commit(), true));
    let authority = issuer();
    let token = authority.issue_at(&grant(), 100).unwrap();
    let args = GqlParameters::new();
    let active = Cell::new(false);
    let calls = Cell::new(0_usize);
    let stop = Cell::new(usize::MAX);
    let clock = || {
        if active.get() {
            calls.set(calls.get() + 1);
        }
        if active.get() && calls.get() == stop.get() {
            1000
        } else {
            100
        }
    };
    let first_row_calls = {
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
            .unwrap();
        let prepared = session.prepare(&cx, COLLECTION_TEXT, &args).unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        active.set(true);
        assert_eq!(cursor.next().unwrap().unwrap(), collection_expected(0, 0)[0]);
        active.set(false);
        calls.get()
    };
    assert!(first_row_calls > 0);
    for cut in 1..=first_row_calls {
        calls.set(0);
        stop.set(cut);
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
            .unwrap();
        let prepared = session.prepare(&cx, COLLECTION_TEXT, &args).unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        active.set(true);
        assert!(
            matches!(
                cursor.next(),
                Some(Err(QueryError::Authorization(Error::Expired)))
            ),
            "expiry checkpoint {cut}"
        );
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
        assert_eq!(calls.get(), cut);
        active.set(false);
        drop(cursor);
        assert!(session.is_closed());
    }

    let mut session = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
        .unwrap();
    let prepared = session.prepare(&cx, COLLECTION_TEXT, &args).unwrap();
    let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), collection_expected(0, 0)[0]);
    root.set_cancel_reason(
        asupersync::CancelReason::new(asupersync::CancelKind::User)
            .with_message("cancel projected collection after its first complete row"),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(QueryError::Pattern(GqlQueryError::Interrupted(_))))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
}
