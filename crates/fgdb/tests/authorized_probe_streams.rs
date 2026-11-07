//! Real Warden sessions drive the existing indexed probe cursor over committed
//! MemVfs generations. These tests do not establish physical noninterference.
#[path = "support/mixed_trail.rs"]
mod mixed_trail;

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
                collection_list(
                    witnesses
                        .iter()
                        .map(|&(_, weight)| collection_scalar(weight)),
                ),
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
fn independent_collection_scans_apply_masks_before_matching_and_value_reads() {
    use std::sync::atomic::{AtomicU64, Ordering};

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
    let calls = AtomicU64::new(0);
    let clock = || {
        calls.fetch_add(1, Ordering::Relaxed);
        100
    };
    let mut scoped = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
        .unwrap();
    let mut visible = slim
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
        .unwrap();
    for edge_root in [false, true] {
        let (head, prefix) = if edge_root {
            ("MATCH (a)-[r]->(b)", "r,a,b")
        } else {
            ("MATCH (a)", "a")
        };
        for (direction, (left, right)) in [("-", "->"), ("<-", "-"), ("-", "-")]
            .into_iter()
            .enumerate()
        {
            let text = format!(
                "{head} RETURN {prefix}, [(x){left}[s]{right}(y) | \
                 [x,y,s,a.p,x.p,y.p,s.p,x.hidden,s.hidden]] AS graph"
            );
            let mut witnesses = Vec::new();
            for (eid, from, to, weight) in COLLECTION_EDGES {
                match direction {
                    0 => witnesses.push((from, eid, to, weight)),
                    1 => witnesses.push((to, eid, from, weight)),
                    _ => {
                        witnesses.push((from, eid, to, weight));
                        if from != to {
                            witnesses.push((to, eid, from, weight));
                        }
                    }
                }
            }
            witnesses.sort_by_key(|&(from, eid, _, _)| (from, eid));
            let collection = |outer| {
                collection_list(witnesses.iter().map(|&(from, eid, to, weight)| {
                    collection_list([
                        GraphValue::Vertex(VId(from)),
                        GraphValue::Vertex(VId(to)),
                        GraphValue::Edge(EId(eid)),
                        collection_scalar(Some(outer as i64 * 10)),
                        collection_scalar(Some(from as i64 * 10)),
                        collection_scalar(Some(to as i64 * 10)),
                        collection_scalar(weight),
                        collection_scalar(None),
                        collection_scalar(None),
                    ])
                }))
            };
            let expected: Vec<_> = if edge_root {
                COLLECTION_EDGES
                    .iter()
                    .map(|&(r, a, b, _)| {
                        GraphValueRow::from_owned_values(vec![
                            GraphValue::Edge(EId(r)),
                            GraphValue::Vertex(VId(a)),
                            GraphValue::Vertex(VId(b)),
                            collection(a),
                        ])
                    })
                    .collect()
            } else {
                [1_u128, 3, 4]
                    .into_iter()
                    .map(|a| {
                        GraphValueRow::from_owned_values(vec![
                            GraphValue::Vertex(VId(a)),
                            collection(a),
                        ])
                    })
                    .collect()
            };
            let prepared = scoped.prepare(&cx, &text, &args).unwrap();
            let eager = scoped.execute(&cx, &prepared, &args).unwrap();
            let before = calls.load(Ordering::Relaxed);
            let mut cursor = scoped.stream_send(&cx, &prepared, &args).unwrap();
            let columns = cursor.columns().to_vec();
            let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(rows, expected, "{text}");
            assert_eq!(native(columns, &rows), eager, "{text}");
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            let checkpoints = calls.load(Ordering::Relaxed) - before;
            assert!(cursor.next().is_none());
            drop(cursor);

            let prepared = visible.prepare(&cx, &text, &args).unwrap();
            let before = calls.load(Ordering::Relaxed);
            let mut cursor = visible.stream_send(&cx, &prepared, &args).unwrap();
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected,
                "{text}"
            );
            assert_eq!(
                calls.load(Ordering::Relaxed) - before,
                checkpoints,
                "{text}"
            );
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
    }
}

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
    for (outer, root_pattern) in [(0, "(a)-[r]->(b)"), (1, "(a)<-[r]-(b)"), (2, "(a)-[r]-(b)")] {
        for (inner, pattern) in [(0, "(b)-[s]->(c)"), (1, "(b)<-[s]-(c)"), (2, "(b)-[s]-(c)")] {
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
        assert_eq!(
            cursor.next().unwrap().unwrap(),
            collection_expected(0, 0)[0]
        );
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
    assert_eq!(
        cursor.next().unwrap().unwrap(),
        collection_expected(0, 0)[0]
    );
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

fn vertex_collection_expected(direction: usize) -> Vec<GraphValueRow> {
    [1_u128, 3, 4]
        .into_iter()
        .map(|a| {
            let witnesses: Vec<_> = COLLECTION_EDGES
                .iter()
                .filter_map(|&(_, from, to, weight)| {
                    let target = match direction {
                        0 if from == a => to,
                        1 if to == a => from,
                        2 if from == a => to,
                        2 if to == a => from,
                        _ => return None,
                    };
                    Some((target, weight))
                })
                .collect();
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(a)),
                collection_scalar(Some(a as i64 * 10)),
                collection_list(
                    witnesses
                        .iter()
                        .map(|&(target, _)| collection_scalar(Some(target as i64 * 10))),
                ),
                collection_list(
                    witnesses
                        .iter()
                        .map(|&(_, weight)| collection_scalar(weight)),
                ),
                collection_list(witnesses.iter().map(|_| collection_scalar(None))),
                collection_list(witnesses.iter().map(|_| collection_scalar(None))),
            ])
        })
        .collect()
}

const VERTEX_COLLECTION_TEXT: &str = "MATCH (a) RETURN a,a.p,\
    [(a)-[s]->(b) | b.p] AS values,\
    [(a)-[s]->(b) | s.p] AS weights,\
    [(a)-[s]->(b) | b.hidden] AS hidden_vertices,\
    [(a)-[s]->(b) | s.hidden] AS hidden_edges";

fn captured_visible_trails(
    anchor: u128,
    direction: usize,
    reverse: bool,
    closed: bool,
) -> GraphValue {
    // Enumerate the finite visible product independently of the production DFS.
    // Intermediate vertices must be visible too, not just a route's endpoints.
    let mut frontier = vec![(Vec::<(EId, VId)>::new(), VId(anchor))];
    let mut accepted = frontier.clone();
    for _ in 1..=3 {
        let mut next = Vec::new();
        for (path, from) in frontier {
            for &(eid, source, target, _) in &COLLECTION_EDGES {
                if path.iter().any(|&(used, _)| used == EId(eid)) {
                    continue;
                }
                let to = match direction {
                    0 if VId(source) == from => VId(target),
                    1 if VId(target) == from => VId(source),
                    2 if VId(source) == from => VId(target),
                    2 if VId(target) == from => VId(source),
                    _ => continue,
                };
                let mut extended = path.clone();
                extended.push((EId(eid), to));
                next.push((extended, to));
            }
        }
        accepted.extend(next.iter().cloned());
        frontier = next;
    }
    accepted.sort_unstable_by(|left, right| {
        left.0
            .iter()
            .map(|step| step.0)
            .cmp(right.0.iter().map(|step| step.0))
    });
    collection_list(accepted.into_iter().filter_map(|(steps, endpoint)| {
        if closed && endpoint != VId(anchor) {
            return None;
        }
        let vertices: Vec<_> = std::iter::once(VId(anchor))
            .chain(steps.iter().map(|step| step.1))
            .collect();
        // Match the eager forward preference when a reversed route is closed.
        let (start, steps) = if reverse && endpoint != VId(anchor) {
            (
                endpoint,
                steps
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(at, &(eid, _))| (eid, vertices[at]))
                    .collect::<Vec<_>>(),
            )
        } else {
            (VId(anchor), steps)
        };
        let nodes = std::iter::once(start)
            .chain(steps.iter().map(|step| step.1))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let relationships = steps
            .iter()
            .map(|step| step.0)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Some(collection_list([
            GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
                start,
                steps.into_boxed_slice(),
            )),
            GraphValue::Vertices(nodes),
            GraphValue::Edges(relationships),
            GraphValue::Vertex(endpoint),
            collection_scalar(Some(endpoint.0 as i64 * 10)),
            collection_scalar(None),
        ]))
    }))
}

#[test]
fn captured_trail_paths_apply_masks_to_every_retained_identity_and_scoped_charge() {
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
    for edge_root in [false, true] {
        let (head, prefix) = if edge_root {
            ("MATCH (x)-[r]->(a)", "r,x,a")
        } else {
            ("MATCH (a)", "a")
        };
        for (direction, (left, right)) in [("-", "->"), ("<-", "-"), ("-", "-")]
            .into_iter()
            .enumerate()
        {
            for (reverse, closed) in [(false, false), (true, false), (false, true)] {
                let endpoint = if closed { "a" } else { "c" };
                let (start, end) = if reverse {
                    (endpoint, "a")
                } else {
                    ("a", endpoint)
                };
                let text = format!(
                    "{head} RETURN {prefix}, [p = ({start}){left}[:R*0..3]{right}({end}) | \
                     [p,nodes(p),relationships(p),{endpoint},{endpoint}.p,{endpoint}.hidden]] AS routes"
                );
                let walk_direction = if reverse && direction < 2 {
                    1 - direction
                } else {
                    direction
                };
                let values = |a| captured_visible_trails(a, walk_direction, reverse, closed);
                let expected: Vec<_> = if edge_root {
                    COLLECTION_EDGES
                        .iter()
                        .map(|&(r, x, a, _)| {
                            GraphValueRow::from_owned_values(vec![
                                GraphValue::Edge(EId(r)),
                                GraphValue::Vertex(VId(x)),
                                GraphValue::Vertex(VId(a)),
                                values(a),
                            ])
                        })
                        .collect()
                } else {
                    [1_u128, 3, 4]
                        .into_iter()
                        .map(|a| {
                            GraphValueRow::from_owned_values(vec![
                                GraphValue::Vertex(VId(a)),
                                values(a),
                            ])
                        })
                        .collect()
                };
                let prepared = scoped.prepare(&cx, &text, &args).unwrap();
                let eager = scoped.execute(&cx, &prepared, &args).unwrap();
                let before = calls.get();
                let mut cursor = scoped.stream(&cx, &prepared, &args).unwrap();
                let columns = cursor.columns().to_vec();
                let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                let checkpoints = calls.get() - before;
                assert_eq!(actual, expected, "{text}");
                assert_eq!(native(columns, &actual), eager, "{text}");
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
                assert!(cursor.next().is_none());
                drop(cursor);
                let prepared = visible.prepare(&cx, &text, &args).unwrap();
                let before = calls.get();
                assert_eq!(
                    visible
                        .stream(&cx, &prepared, &args)
                        .unwrap()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                    expected,
                    "{text}"
                );
                assert_eq!(
                    calls.get() - before,
                    checkpoints,
                    "hidden records changed logical charges: {text}"
                );
                let mut send = db
                    .authorized_read_session(
                        &cx,
                        &authority,
                        &token,
                        BRANCH,
                        symbols,
                        policy(),
                        || 100,
                    )
                    .unwrap();
                let prepared = send.prepare(&cx, &text, &args).unwrap();
                let cursor = send.stream_send(&cx, &prepared, &args).unwrap();
                fn assert_send<T: Send>(_: &T) {}
                assert_send(&cursor);
                assert_eq!(
                    cursor.collect::<Result<Vec<_>, _>>().unwrap(),
                    expected,
                    "{text}"
                );
            }
        }
    }
    // Denied relationships still permit the visible zero-hop path, with no EId.
    for (minimum, include_zero) in [(0, true), (1, false)] {
        let text = format!("MATCH (a) RETURN a,[p = (a)-[:S*{minimum}..3]->(c) | p] AS paths");
        let prepared = scoped.prepare(&cx, &text, &args).unwrap();
        let expected: Vec<_> = [1_u128, 3, 4]
            .into_iter()
            .map(|a| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(a)),
                    collection_list(include_zero.then(|| {
                        GraphValue::Path(fgdb_gql::algebra::GraphPath::new(VId(a), Box::new([])))
                    })),
                ])
            })
            .collect();
        assert_eq!(
            scoped
                .stream(&cx, &prepared, &args)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected,
            "{text}"
        );
    }
}

#[test]
fn captured_trail_values_stay_private_until_a_complete_authorized_row_is_ready() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let db = runtime.block_on(collection_database(&contexts.commit(), true));
    let authority = issuer();
    let token = authority.issue_at(&grant(), 100).unwrap();
    let args = GqlParameters::new();
    let text = "MATCH (a) RETURN a,[p = (a)-[:R*0..3]->(c) | \
                [p,nodes(p),relationships(p),c,c.p,c.hidden]] AS routes";
    let expected = GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(1)),
        captured_visible_trails(1, 0, false, false),
    ]);
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
        let prepared = session.prepare(&cx, text, &args).unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        active.set(true);
        assert_eq!(cursor.next().unwrap().unwrap(), expected);
        active.set(false);
        calls.get()
    };
    assert!(first_row_calls > 3);
    for cut in [1, first_row_calls / 2, first_row_calls] {
        calls.set(0);
        stop.set(cut);
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
            .unwrap();
        let prepared = session.prepare(&cx, text, &args).unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        active.set(true);
        assert!(matches!(
            cursor.next(),
            Some(Err(QueryError::Authorization(Error::Expired)))
        ));
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
    let prepared = session.prepare(&cx, text, &args).unwrap();
    let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), expected);
    root.set_cancel_reason(
        asupersync::CancelReason::new(asupersync::CancelKind::User)
            .with_message("cancel after publishing one complete captured trail list"),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(QueryError::Pattern(GqlQueryError::Interrupted(_))))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
}

fn bounded_trail_collection(
    anchor: u128,
    direction: usize,
    minimum: usize,
    maximum: usize,
    wanted: Option<u128>,
) -> GraphValue {
    // Enumerate the finite product by LENGTH, independently of the production
    // DFS, then order complete identity sequences lexicographically. Prefixes
    // precede descendants. Never collapse distinct paths with one endpoint.
    let mut frontier = vec![(Vec::<u128>::new(), anchor)];
    let mut accepted = if minimum == 0 {
        frontier.clone()
    } else {
        Vec::new()
    };
    for length in 1..=maximum {
        let mut next = Vec::new();
        for (identities, from) in frontier {
            for &(eid, source, target, _) in &COLLECTION_EDGES {
                if identities.contains(&eid) {
                    continue;
                }
                let to = match direction {
                    0 if source == from => target,
                    1 if target == from => source,
                    2 if source == from => target,
                    2 if target == from => source,
                    _ => continue,
                };
                let mut path = identities.clone();
                path.push(eid);
                next.push((path, to));
            }
        }
        if length >= minimum {
            accepted.extend(next.iter().cloned());
        }
        frontier = next;
    }
    accepted.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    collection_list(
        accepted
            .into_iter()
            .filter(|(_, endpoint)| wanted.is_none_or(|wanted| wanted == *endpoint))
            .map(|(_, endpoint)| {
                collection_list([
                    GraphValue::Vertex(VId(endpoint)),
                    collection_scalar(Some(endpoint as i64 * 10)),
                    collection_scalar(None),
                ])
            }),
    )
}

#[test]
fn authorized_bounded_trail_collections_match_the_visible_path_product() {
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
    for edge_root in [false, true] {
        let (head, prefix, anchor) = if edge_root {
            ("MATCH (a)-[r]->(b)", "r,a,b", "b")
        } else {
            ("MATCH (a)", "a", "a")
        };
        for (direction, (left, right)) in [("-", "->"), ("<-", "-"), ("-", "-")]
            .into_iter()
            .enumerate()
        {
            for (minimum, maximum) in [(0, 0), (0, 3), (1, 3), (2, 2)] {
                for wanted in [None, Some(4)] {
                    let predicate = if wanted.is_some() {
                        "WHERE c.p = 40 AND c.hidden IS NULL"
                    } else {
                        ""
                    };
                    let text = format!(
                        "{head} RETURN {prefix}, \
                         [({anchor}){left}[:R*{minimum}..{maximum}]{right}(c) \
                         {predicate} | [c,c.p,c.hidden]] AS routes"
                    );
                    let values =
                        |at| bounded_trail_collection(at, direction, minimum, maximum, wanted);
                    let expected: Vec<_> = if edge_root {
                        COLLECTION_EDGES
                            .iter()
                            .map(|&(r, a, b, _)| {
                                GraphValueRow::from_owned_values(vec![
                                    GraphValue::Edge(EId(r)),
                                    GraphValue::Vertex(VId(a)),
                                    GraphValue::Vertex(VId(b)),
                                    values(b),
                                ])
                            })
                            .collect()
                    } else {
                        [1_u128, 3, 4]
                            .into_iter()
                            .map(|a| {
                                GraphValueRow::from_owned_values(vec![
                                    GraphValue::Vertex(VId(a)),
                                    values(a),
                                ])
                            })
                            .collect()
                    };
                    let query = scoped.prepare(&cx, &text, &args).unwrap();
                    let eager = scoped.execute(&cx, &query, &args).unwrap();
                    let before = calls.get();
                    let mut cursor = scoped.stream(&cx, &query, &args).unwrap();
                    let columns = cursor.columns().to_vec();
                    let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                    let checkpoints = calls.get() - before;
                    assert_eq!(actual, expected, "{text}");
                    assert_eq!(native(columns, &actual), eager, "{text}");
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                    assert!(cursor.next().is_none());
                    drop(cursor);
                    let query = visible.prepare(&cx, &text, &args).unwrap();
                    let before = calls.get();
                    assert_eq!(
                        visible
                            .stream(&cx, &query, &args)
                            .unwrap()
                            .collect::<Result<Vec<_>, _>>()
                            .unwrap(),
                        expected,
                        "{text}"
                    );
                    // Invisible transit vertices, relationships and fields
                    // cannot contribute logical work or authority clock calls.
                    assert_eq!(calls.get() - before, checkpoints, "{text}");
                    let mut send = db
                        .authorized_read_session(
                            &cx,
                            &authority,
                            &token,
                            BRANCH,
                            symbols,
                            policy(),
                            || 100,
                        )
                        .unwrap();
                    let query = send.prepare(&cx, &text, &args).unwrap();
                    let cursor = send.stream_send(&cx, &query, &args).unwrap();
                    fn assert_send<T: Send>(_: &T) {}
                    assert_send(&cursor);
                    assert_eq!(cursor.collect::<Result<Vec<_>, _>>().unwrap(), expected);
                }
            }
        }
    }
    // A zero-hop witness uses no relationship authority. Positive lengths of
    // the denied type have no witness, including hidden-to-visible shortcuts.
    for (pattern, zero_hop) in [
        ("(a)-[:S*0..0]->(c)", true),
        ("(a)-[:S*0..3]->(c)", true),
        ("(a)-[:S*1..3]->(c)", false),
        ("(a)-[:R*0..3]->(c:H)", false),
        ("(a)-[:R*0..3]->(c) WHERE c.hidden = 999", false),
    ] {
        let text = format!("MATCH (a) RETURN a,[{pattern} | [c,c.p,c.hidden]] AS routes");
        let query = scoped.prepare(&cx, &text, &args).unwrap();
        assert_eq!(
            scoped
                .stream(&cx, &query, &args)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            [1_u128, 3, 4].map(|a| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(a)),
                if zero_hop {
                    bounded_trail_collection(a, 0, 0, 0, None)
                } else {
                    collection_list([])
                },
            ])),
            "{text}"
        );
    }
}

#[test]
fn bounded_trail_lists_do_not_escape_after_expiry_cancellation_or_a_late_value_error() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let db = runtime.block_on(collection_database(&contexts.commit(), true));
    let authority = issuer();
    let token = authority.issue_at(&grant(), 100).unwrap();
    let args = GqlParameters::new();
    let text = "MATCH (a) RETURN a,[(a)-[:R*0..3]->(c) | [c,c.p,c.hidden]] AS routes";
    let expected = GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(1)),
        bounded_trail_collection(1, 0, 0, 3, None),
    ]);
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
        let prepared = session.prepare(&cx, text, &args).unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        active.set(true);
        assert_eq!(cursor.next().unwrap().unwrap(), expected);
        active.set(false);
        calls.get()
    };
    assert!(first_row_calls > 3);
    // Early, interior and final authority checkpoints cover a live private
    // trail list as well as its publication boundary. No partial list escapes.
    for cut in [1, first_row_calls / 2, first_row_calls] {
        calls.set(0);
        stop.set(cut);
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
            .unwrap();
        let prepared = session.prepare(&cx, text, &args).unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        active.set(true);
        assert!(matches!(
            cursor.next(),
            Some(Err(QueryError::Authorization(Error::Expired)))
        ));
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
    let prepared = session
        .prepare(
            &cx,
            "MATCH (a) RETURN a,[(a)-[:R*0..3]->(c) | 100 / (c.p - 40)] AS routes",
            &args,
        )
        .unwrap();
    let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
    assert!(matches!(
        cursor.next(),
        Some(Err(QueryError::Set(GqlQueryError::Source(
            fgdb_gql::GraphSetExecutionError::Projection { row: 0, .. }
        ))))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
    drop(cursor);

    let mut session = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
        .unwrap();
    let prepared = session.prepare(&cx, text, &args).unwrap();
    let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), expected);
    root.set_cancel_reason(
        asupersync::CancelReason::new(asupersync::CancelKind::User)
            .with_message("cancel after the first complete bounded trail list"),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(QueryError::Pattern(GqlQueryError::Interrupted(_))))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
}

#[test]
fn authorized_multihop_collections_match_visible_witness_products_and_scoped_charges() {
    fn incident(from: u128, direction: usize) -> Vec<(u128, u128, Option<i64>)> {
        COLLECTION_EDGES
            .iter()
            .filter_map(|&(eid, source, target, weight)| {
                let to = match direction {
                    0 if source == from => target,
                    1 if target == from => source,
                    2 if source == from => target,
                    2 if target == from => source,
                    _ => return None,
                };
                Some((eid, to, weight))
            })
            .collect()
    }
    fn routes(anchor: u128, first: usize, second: usize) -> GraphValue {
        let mut values = Vec::new();
        // Independent finite product over the visible fixture, in EId order.
        // Only the two inner edges share a DifferentEdges domain. An outer
        // relationship may occur again inside its correlated collection.
        for (s, c, sp) in incident(anchor, first) {
            for (t, d, tp) in incident(c, second) {
                if s != t {
                    values.push(collection_list([
                        GraphValue::Vertex(VId(c)),
                        GraphValue::Vertex(VId(d)),
                        GraphValue::Edge(EId(s)),
                        GraphValue::Edge(EId(t)),
                        collection_scalar(Some(c as i64 * 10)),
                        collection_scalar(Some(d as i64 * 10)),
                        collection_scalar(sp),
                        collection_scalar(tp),
                        collection_scalar(None),
                        collection_scalar(None),
                    ]));
                }
            }
        }
        collection_list(values)
    }
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
    let directions = [("-", "->"), ("<-", "-"), ("-", "-")];
    for edge_root in [false, true] {
        let (head, prefix, anchor) = if edge_root {
            ("MATCH (a)-[r]->(b)", "r,a,b", "b")
        } else {
            ("MATCH (a)", "a", "a")
        };
        for (first, (left, right)) in directions.into_iter().enumerate() {
            for (second, (next_left, next_right)) in directions.into_iter().enumerate() {
                let text = format!(
                    "{head} RETURN {prefix}, \
                     [({anchor}){left}[s]{right}(c){next_left}[t]{next_right}(d) | \
                     [c,d,s,t,c.p,d.p,s.p,t.p,c.hidden,t.hidden]] AS routes"
                );
                let expected: Vec<_> = if edge_root {
                    COLLECTION_EDGES
                        .iter()
                        .map(|&(r, a, b, _)| {
                            GraphValueRow::from_owned_values(vec![
                                GraphValue::Edge(EId(r)),
                                GraphValue::Vertex(VId(a)),
                                GraphValue::Vertex(VId(b)),
                                routes(b, first, second),
                            ])
                        })
                        .collect()
                } else {
                    [1_u128, 3, 4]
                        .into_iter()
                        .map(|a| {
                            GraphValueRow::from_owned_values(vec![
                                GraphValue::Vertex(VId(a)),
                                routes(a, first, second),
                            ])
                        })
                        .collect()
                };
                let prepared = scoped.prepare(&cx, &text, &args).unwrap();
                let eager = scoped.execute(&cx, &prepared, &args).unwrap();
                let before = calls.get();
                let mut cursor = scoped.stream(&cx, &prepared, &args).unwrap();
                let columns = cursor.columns().to_vec();
                let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                let checkpoints = calls.get() - before;
                assert_eq!(actual, expected, "{text}");
                assert_eq!(native(columns, &actual), eager, "{text}");
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
                assert!(cursor.next().is_none());
                drop(cursor);

                let slim_query = visible.prepare(&cx, &text, &args).unwrap();
                let before = calls.get();
                assert_eq!(
                    visible
                        .stream(&cx, &slim_query, &args)
                        .unwrap()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                    expected,
                    "{text}"
                );
                assert_eq!(calls.get() - before, checkpoints, "{text}");
                // The Send adapter must use the same masked source on each
                // hop and when the row VM reads both local relationships.
                let mut send = db
                    .authorized_read_session(
                        &cx,
                        &authority,
                        &token,
                        BRANCH,
                        symbols,
                        policy(),
                        || 100,
                    )
                    .unwrap();
                let query = send.prepare(&cx, &text, &args).unwrap();
                let cursor = send.stream_send(&cx, &query, &args).unwrap();
                fn assert_send<T: Send>(_: &T) {}
                assert_send(&cursor);
                assert_eq!(cursor.collect::<Result<Vec<_>, _>>().unwrap(), expected);
            }
        }
    }
    // Hidden intermediate vertices cannot be traversed to reach a visible
    // second endpoint. A denied second relationship or predicate field also
    // contributes no witness, even though that payload exists physically.
    for pattern in [
        "(a)-[s:R]->(c:H)-[t:R]->(d)",
        "(a)-[s:R]->(c)-[t:S]->(d)",
        "(a)-[s:R]->(c)-[t:R]->(d) WHERE t.hidden = 999",
    ] {
        let text = format!("MATCH (a) RETURN a,[{pattern} | d.p] AS values");
        let query = scoped.prepare(&cx, &text, &args).unwrap();
        assert_eq!(
            scoped
                .stream(&cx, &query, &args)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            [1_u128, 3, 4].map(|a| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(a)),
                collection_list([]),
            ])),
            "{text}"
        );
    }
}

#[test]
fn authorized_vertex_collections_admit_every_endpoint_and_field_before_projection() {
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
    for (direction, pattern) in [(0, "(a)-[s]->(b)"), (1, "(a)<-[s]-(b)"), (2, "(a)-[s]-(b)")] {
        let text = format!(
            "MATCH (a) RETURN a,a.p,\
             [{pattern} | b.p] AS values,[{pattern} | s.p] AS weights,\
             [{pattern} | b.hidden] AS hidden_vertices,\
             [{pattern} | s.hidden] AS hidden_edges"
        );
        let expected = vertex_collection_expected(direction);
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
        assert_eq!(calls.get() - before, checkpoints, "{text}");
    }
    // Probe and collection captures have separate local identities. Neither
    // the probe's success nor a captured edge is cached authorization.
    let text = "MATCH (a) WHERE EXISTS { MATCH (a)-[r]->(b) WHERE r.hidden IS NULL } \
                RETURN a,[(a)-[s]->(b) | s] AS edges";
    let prepared = scoped.prepare(&cx, text, &args).unwrap();
    let rows = scoped
        .stream(&cx, &prepared, &args)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let expected: Vec<_> = [1_u128, 3]
        .into_iter()
        .map(|a| {
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(a)),
                collection_list(
                    COLLECTION_EDGES
                        .iter()
                        .filter(|(_, from, _, _)| *from == a)
                        .map(|&(eid, ..)| GraphValue::Edge(EId(eid))),
                ),
            ])
        })
        .collect();
    assert_eq!(rows, expected);
    for collected in [
        "[(a)-[s:R]->(b:H) | b.p]",
        "[(a)-[s:S]->(b) | b.p]",
        "[(a)-[s:R]->(b) WHERE b.hidden = 999 | b.p]",
        "[(a)-[s:R]->(b) WHERE s.hidden = 999 | s.p]",
    ] {
        let text = format!("MATCH (a) RETURN a,{collected} AS values");
        let prepared = scoped.prepare(&cx, &text, &args).unwrap();
        assert_eq!(
            scoped
                .stream(&cx, &prepared, &args)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            [1_u128, 3, 4].map(|a| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(a)),
                collection_list([]),
            ])),
            "{text}"
        );
    }
    let mut limited = db
        .authorized_read_session(
            &cx,
            &authority,
            &token,
            BRANCH,
            symbols,
            GqlQueryPolicy::new(1, 1000, 10_000_000, 10_000_000),
            || 100,
        )
        .unwrap();
    let prepared = limited.prepare(&cx, VERTEX_COLLECTION_TEXT, &args).unwrap();
    let mut cursor = limited.stream(&cx, &prepared, &args).unwrap();
    assert!(matches!(
        cursor.next(),
        Some(Err(QueryError::Stream(GqlQueryError::Rows(_))))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
}

#[test]
fn authorized_collection_property_reads_keep_the_pinned_generation_after_writes_and_compaction() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let commit = contexts.commit();
    let mut db = runtime.block_on(collection_database(&commit, true));
    let authority = issuer();
    let token = authority.issue_at(&grant(), 100).unwrap();
    let args = GqlParameters::new();
    let mut session = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
        .unwrap();
    let prepared = session.prepare(&cx, VERTEX_COLLECTION_TEXT, &args).unwrap();
    let mut paused = session.stream_send(&cx, &prepared, &args).unwrap();
    let basis = paused.snapshot_seq();
    let mut edit = WriteBatch::new(RelationId(1));
    edit.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(300)));
    edit.add_edge(
        EId(30),
        VId(4),
        VId(1),
        vec![(PropertyKeyId(1), CanonicalScalar::Int(32))],
    );
    runtime.block_on(db.write(&commit, edit)).unwrap();
    runtime.block_on(db.compact(&commit)).unwrap();
    {
        let mut current = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let query = current.prepare(&cx, VERTEX_COLLECTION_TEXT, &args).unwrap();
        assert_ne!(
            current
                .stream(&cx, &query, &args)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            vertex_collection_expected(0)
        );
    }
    drop(db);
    assert_eq!(paused.snapshot_seq(), basis);
    assert_eq!(
        paused.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        vertex_collection_expected(0)
    );
    assert_eq!(paused.state(), VertexScanState::Exhausted);
    assert!(paused.next().is_none());
}

#[test]
fn authorized_projected_pages_keep_masks_and_signed_delivery_limits() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let commit = contexts.commit();
    let db = runtime.block_on(collection_database(&commit, true));
    let slim = runtime.block_on(collection_database(&commit, false));
    let authority = issuer();
    let args = GqlParameters::new();
    for (base, full) in [
        (VERTEX_COLLECTION_TEXT, vertex_collection_expected(0)),
        (COLLECTION_TEXT, collection_expected(0, 0)),
    ] {
        for (skip, take) in [(0, 0), (1, 1), (2, 1), (99, 1)] {
            let text = format!("{base} SKIP {skip} LIMIT {take}");
            let expected: Vec<_> = full.iter().skip(skip).take(take).cloned().collect();
            let mut allowed = grant();
            allowed.limits.max_rows = expected.len() as u64;
            let token = authority.issue_at(&allowed, 100).unwrap();
            let allowance =
                GqlQueryPolicy::new(1_000_000, expected.len() as u64, 10_000_000, 10_000_000);
            let calls = Cell::new(0_u64);
            let clock = || {
                calls.set(calls.get() + 1);
                100
            };
            let mut scoped = db
                .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, allowance, clock)
                .unwrap();
            let prepared = scoped.prepare(&cx, &text, &args).unwrap();
            let before = calls.get();
            let mut cursor = scoped.stream(&cx, &prepared, &args).unwrap();
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected,
                "{text}",
            );
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            let checkpoints = calls.get() - before;
            assert!(cursor.next().is_none());
            drop(cursor);

            let mut visible = slim
                .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, allowance, clock)
                .unwrap();
            let prepared = visible.prepare(&cx, &text, &args).unwrap();
            let before = calls.get();
            let mut cursor = visible.stream(&cx, &prepared, &args).unwrap();
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected,
                "{text}",
            );
            assert_eq!(calls.get() - before, checkpoints, "{text}");
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert!(cursor.next().is_none());
        }
    }
}

#[test]
fn authorized_projected_page_suffixes_keep_live_expiry_and_cancellation() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let commit = contexts.commit();
    let db = runtime.block_on(collection_database(&commit, true));
    let authority = issuer();
    let token = authority.issue_at(&grant(), 100).unwrap();
    let args = GqlParameters::new();
    let expired = Cell::new(false);
    let mut session = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || {
            if expired.get() { 1000 } else { 100 }
        })
        .unwrap();
    let text = format!("{VERTEX_COLLECTION_TEXT} LIMIT 1");
    let prepared = session.prepare(&cx, &text, &args).unwrap();
    let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
    assert_eq!(
        cursor.next().unwrap().unwrap(),
        vertex_collection_expected(0)[0]
    );
    assert_eq!(cursor.state(), VertexScanState::Open);
    expired.set(true);
    assert!(matches!(
        cursor.next(),
        Some(Err(QueryError::Authorization(Error::Expired)))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
    drop(cursor);
    assert!(session.is_closed());

    let mut session = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
        .unwrap();
    let text = format!("{COLLECTION_TEXT} LIMIT 1");
    let prepared = session.prepare(&cx, &text, &args).unwrap();
    let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
    assert_eq!(
        cursor.next().unwrap().unwrap(),
        collection_expected(0, 0)[0]
    );
    assert_eq!(cursor.state(), VertexScanState::Open);
    root.set_cancel_reason(
        asupersync::CancelReason::new(asupersync::CancelKind::User)
            .with_message("cancel while validating the suffix after a projected page"),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(QueryError::Pattern(GqlQueryError::Interrupted(_))))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
}

#[test]
fn authorized_collected_paths_keep_every_segment_inside_the_visible_graph() {
    use std::sync::atomic::{AtomicU64, Ordering};

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
    for (edge_root, direction, closed) in [
        (false, 0, false),
        (false, 1, false),
        (false, 2, true),
        (true, 0, false),
    ] {
        let (left, right) = match direction {
            0 => ("-", "->"),
            1 => ("<-", "-"),
            _ => ("-", "-"),
        };
        let end_name = if closed { "a" } else { "c" };
        let body = format!(
            "[p = (a){left}[s]{right}(b){left}[t]{right}({end_name}) | \
             [p,b,{end_name},s,t,nodes(p),relationships(p),b.p,{end_name}.p,s.p,t.p,\
              b.hidden,{end_name}.hidden,s.hidden,t.hidden]] AS routes"
        );
        let text = if edge_root {
            format!("MATCH (x)-[r]->(a) RETURN r,x,a,{body}")
        } else {
            format!("MATCH (a) RETURN a,{body}")
        };
        let routes = |start| {
            let target = |at, from, to| match direction {
                0 if from == at => Some(to),
                1 if to == at => Some(from),
                2 if from == at => Some(to),
                2 if to == at => Some(from),
                _ => None,
            };
            let mut values = Vec::new();
            for &(s, from, to, s_property) in &COLLECTION_EDGES {
                let Some(middle) = target(start, from, to) else {
                    continue;
                };
                for &(t, from, to, t_property) in &COLLECTION_EDGES {
                    if s == t {
                        continue;
                    }
                    let Some(end) = target(middle, from, to) else {
                        continue;
                    };
                    if closed && end != start {
                        continue;
                    }
                    values.push(collection_list([
                        GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
                            VId(start),
                            vec![(EId(s), VId(middle)), (EId(t), VId(end))].into_boxed_slice(),
                        )),
                        GraphValue::Vertex(VId(middle)),
                        GraphValue::Vertex(VId(end)),
                        GraphValue::Edge(EId(s)),
                        GraphValue::Edge(EId(t)),
                        GraphValue::Vertices([VId(start), VId(middle), VId(end)].into()),
                        GraphValue::Edges([EId(s), EId(t)].into()),
                        collection_scalar(Some(middle as i64 * 10)),
                        collection_scalar(Some(end as i64 * 10)),
                        collection_scalar(s_property),
                        collection_scalar(t_property),
                        collection_scalar(None),
                        collection_scalar(None),
                        collection_scalar(None),
                        collection_scalar(None),
                    ]));
                }
            }
            collection_list(values)
        };
        let expected: Vec<_> = if edge_root {
            COLLECTION_EDGES
                .iter()
                .map(|&(id, from, to, _)| {
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Edge(EId(id)),
                        GraphValue::Vertex(VId(from)),
                        GraphValue::Vertex(VId(to)),
                        routes(to),
                    ])
                })
                .collect()
        } else {
            [1_u128, 3, 4]
                .into_iter()
                .map(|id| {
                    GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(id)), routes(id)])
                })
                .collect()
        };
        assert!(
            expected.iter().any(|row| match row.values().last() {
                Some(GraphValue::List(routes)) => !routes.is_empty(),
                _ => false,
            }),
            "{text}"
        );

        // The additional graph contains hidden intermediate vertices, a
        // forbidden relation, and masked fields on otherwise visible elements.
        // A Send cursor still reads every path segment through the scoped source.
        let calls = AtomicU64::new(0);
        let clock = || {
            calls.fetch_add(1, Ordering::Relaxed);
            100
        };
        let mut scoped = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
            .unwrap();
        let prepared = scoped.prepare(&cx, &text, &args).unwrap();
        let eager = scoped.execute(&cx, &prepared, &args).unwrap();
        let before = calls.load(Ordering::Relaxed);
        let mut cursor = scoped.stream_send(&cx, &prepared, &args).unwrap();
        let columns = cursor.columns().to_vec();
        let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(actual, expected, "{text}");
        assert_eq!(native(columns, &actual), eager, "{text}");
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        let checkpoints = calls.load(Ordering::Relaxed) - before;
        assert!(cursor.next().is_none());
        drop(cursor);

        let mut visible = slim
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
            .unwrap();
        let prepared = visible.prepare(&cx, &text, &args).unwrap();
        let before = calls.load(Ordering::Relaxed);
        let mut cursor = visible.stream_send(&cx, &prepared, &args).unwrap();
        assert_eq!(
            cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            expected,
            "{text}",
        );
        assert_eq!(
            calls.load(Ordering::Relaxed) - before,
            checkpoints,
            "{text}"
        );
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert!(cursor.next().is_none());
    }
}

#[test]
fn nested_trail_filters_preserve_visible_route_multiplicity_and_scoped_reads() {
    use std::sync::atomic::{AtomicU64, Ordering};

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
    for edge_root in [false, true] {
        let (head, prefix) = if edge_root {
            ("MATCH (x)-[r]->(a)", "r,x,a")
        } else {
            ("MATCH (a)", "a")
        };
        for (direction, (left, right)) in [("-", "->"), ("<-", "-"), ("-", "-")]
            .into_iter()
            .enumerate()
        {
            for (minimum, maximum) in [(0, 0), (0, 3), (2, 2)] {
                for (predicate, filter) in [
                    "EXISTS { MATCH (b)-[t]->(c) WHERE t.p > 3 }",
                    "NOT EXISTS { MATCH (b)-[t]->(c) WHERE t.p > 3 }",
                    "EXISTS { MATCH (b)-[t]->(c) } \
                     AND NOT EXISTS { MATCH (b)-[u]->(d) WHERE d.p = 10 }",
                    "EXISTS { MATCH (b)-[t:S]->(c) }",
                    "NOT EXISTS { MATCH (b)-[t]->(c:H) }",
                    "EXISTS { MATCH (b)-[t]->(c) WHERE t.hidden = 999 OR c.hidden = 999 }",
                    "NOT EXISTS { MATCH (b)-[t]->(c) WHERE t.hidden = 999 OR c.hidden = 999 }",
                ]
                .into_iter()
                .enumerate()
                {
                    let text = format!(
                        "{head} RETURN {prefix}, \
                         [(a){left}[:R*{minimum}..{maximum}]{right}(b) WHERE {filter} | \
                         [b,b.p,b.hidden]] AS routes"
                    );
                    let values = |anchor| {
                        let GraphValue::List(routes) =
                            bounded_trail_collection(anchor, direction, minimum, maximum, None)
                        else {
                            panic!("literal trail oracle must return a list");
                        };
                        collection_list(routes.into_vec().into_iter().filter(|route| {
                            let GraphValue::List(cells) = route else {
                                panic!("literal trail oracle must return tuples");
                            };
                            let GraphValue::Vertex(endpoint) = &cells[0] else {
                                panic!("literal route tuple must start with its endpoint");
                            };
                            let qualifying_child =
                                COLLECTION_EDGES.iter().any(|&(_, start, _, weight)| {
                                    start == endpoint.0 && weight.is_some_and(|value| value > 3)
                                });
                            match predicate {
                                0 => qualifying_child,
                                1 => !qualifying_child,
                                2 => {
                                    COLLECTION_EDGES
                                        .iter()
                                        .any(|&(_, start, _, _)| start == endpoint.0)
                                        && !COLLECTION_EDGES.iter().any(|&(_, start, end, _)| {
                                            start == endpoint.0 && end == 1
                                        })
                                }
                                3 | 5 => false,
                                _ => true,
                            }
                        }))
                    };
                    let expected: Vec<_> = if edge_root {
                        COLLECTION_EDGES
                            .iter()
                            .map(|&(eid, from, to, _)| {
                                GraphValueRow::from_owned_values(vec![
                                    GraphValue::Edge(EId(eid)),
                                    GraphValue::Vertex(VId(from)),
                                    GraphValue::Vertex(VId(to)),
                                    values(to),
                                ])
                            })
                            .collect()
                    } else {
                        [1_u128, 3, 4]
                            .into_iter()
                            .map(|id| {
                                GraphValueRow::from_owned_values(vec![
                                    GraphValue::Vertex(VId(id)),
                                    values(id),
                                ])
                            })
                            .collect()
                    };
                    let calls = AtomicU64::new(0);
                    let clock = || {
                        calls.fetch_add(1, Ordering::Relaxed);
                        100
                    };
                    let mut scoped = db
                        .authorized_read_session(
                            &cx,
                            &authority,
                            &token,
                            BRANCH,
                            symbols,
                            policy(),
                            clock,
                        )
                        .unwrap();
                    let prepared = scoped.prepare(&cx, &text, &args).unwrap();
                    let eager = scoped.execute(&cx, &prepared, &args).unwrap();
                    let before = calls.load(Ordering::Relaxed);
                    let mut cursor = scoped.stream_send(&cx, &prepared, &args).unwrap();
                    let columns = cursor.columns().to_vec();
                    let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                    assert_eq!(rows, expected, "{text}");
                    assert_eq!(native(columns, &rows), eager, "{text}");
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                    let checkpoints = calls.load(Ordering::Relaxed) - before;
                    assert!(cursor.next().is_none());
                    drop(cursor);
                    let mut visible = slim
                        .authorized_read_session(
                            &cx,
                            &authority,
                            &token,
                            BRANCH,
                            symbols,
                            policy(),
                            clock,
                        )
                        .unwrap();
                    let prepared = visible.prepare(&cx, &text, &args).unwrap();
                    let before = calls.load(Ordering::Relaxed);
                    let mut cursor = visible.stream_send(&cx, &prepared, &args).unwrap();
                    assert_eq!(
                        cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                        expected,
                        "{text}"
                    );
                    assert_eq!(
                        calls.load(Ordering::Relaxed) - before,
                        checkpoints,
                        "{text}"
                    );
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                    assert!(cursor.next().is_none());
                }
            }
        }
    }
}

#[test]
fn nested_collection_filters_keep_all_child_reads_inside_the_authorized_graph() {
    use std::sync::atomic::{AtomicU64, Ordering};

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

    for edge_root in [false, true] {
        let (head, prefix) = if edge_root {
            ("MATCH (x)-[r]->(a)", "r,x,a")
        } else {
            ("MATCH (a)", "a")
        };
        for (direction, (left, right)) in [("-", "->"), ("<-", "-"), ("-", "-")]
            .into_iter()
            .enumerate()
        {
            for (predicate, filter) in [
                "EXISTS { MATCH (b)-[t]->(c) WHERE t.p > 3 }",
                "NOT EXISTS { MATCH (b)-[t]->(c) WHERE t.p > 3 }",
                "EXISTS { MATCH (b)-[t]->(c) } \
                 AND NOT EXISTS { MATCH (b)-[u]->(d) WHERE d.p = 10 }",
                "EXISTS { MATCH (b)-[t:S]->(c) }",
                "NOT EXISTS { MATCH (b)-[t]->(c:H) }",
                "EXISTS { MATCH (b)-[t]->(c) WHERE t.hidden = 999 OR c.hidden = 999 }",
                "NOT EXISTS { MATCH (b)-[t]->(c) WHERE t.hidden = 999 OR c.hidden = 999 }",
            ]
            .into_iter()
            .enumerate()
            {
                let text = format!(
                    "{head} RETURN {prefix}, \
                     [p = (a){left}[s]{right}(b) WHERE {filter} | \
                     [p,s,b,s.p,b.p,s.hidden,b.hidden]] AS values"
                );
                let values = |anchor| {
                    collection_list(COLLECTION_EDGES.iter().filter_map(
                        |&(eid, from, to, weight)| {
                            let target = match direction {
                                0 if from == anchor => to,
                                1 if to == anchor => from,
                                2 if from == anchor => to,
                                2 if to == anchor => from,
                                _ => return None,
                            };
                            // This oracle sees only the literal visible graph.
                            // Child matches contribute truth, never multiplicity
                            // or relationship exclusions to the parent witness.
                            let qualifying_child =
                                COLLECTION_EDGES.iter().any(|&(_, start, _, weight)| {
                                    start == target && weight.is_some_and(|value| value > 3)
                                });
                            let accepted = match predicate {
                                0 => qualifying_child,
                                1 => !qualifying_child,
                                2 => {
                                    COLLECTION_EDGES
                                        .iter()
                                        .any(|&(_, start, _, _)| start == target)
                                        && !COLLECTION_EDGES
                                            .iter()
                                            .any(|&(_, start, end, _)| start == target && end == 1)
                                }
                                3 | 5 => false,
                                _ => true,
                            };
                            accepted.then(|| {
                                collection_list([
                                    GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
                                        VId(anchor),
                                        vec![(EId(eid), VId(target))].into_boxed_slice(),
                                    )),
                                    GraphValue::Edge(EId(eid)),
                                    GraphValue::Vertex(VId(target)),
                                    collection_scalar(weight),
                                    collection_scalar(Some(target as i64 * 10)),
                                    collection_scalar(None),
                                    collection_scalar(None),
                                ])
                            })
                        },
                    ))
                };
                let expected: Vec<_> = if edge_root {
                    COLLECTION_EDGES
                        .iter()
                        .map(|&(eid, from, to, _)| {
                            GraphValueRow::from_owned_values(vec![
                                GraphValue::Edge(EId(eid)),
                                GraphValue::Vertex(VId(from)),
                                GraphValue::Vertex(VId(to)),
                                values(to),
                            ])
                        })
                        .collect()
                } else {
                    [1_u128, 3, 4]
                        .into_iter()
                        .map(|id| {
                            GraphValueRow::from_owned_values(vec![
                                GraphValue::Vertex(VId(id)),
                                values(id),
                            ])
                        })
                        .collect()
                };
                let calls = AtomicU64::new(0);
                let clock = || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    100
                };
                let mut scoped = db
                    .authorized_read_session(
                        &cx,
                        &authority,
                        &token,
                        BRANCH,
                        symbols,
                        policy(),
                        clock,
                    )
                    .unwrap();
                let prepared = scoped.prepare(&cx, &text, &args).unwrap();
                let eager = scoped.execute(&cx, &prepared, &args).unwrap();
                let before = calls.load(Ordering::Relaxed);
                let mut cursor = scoped.stream_send(&cx, &prepared, &args).unwrap();
                fn assert_send<T: Send>(_: &T) {}
                assert_send(&cursor);
                let columns = cursor.columns().to_vec();
                let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(rows, expected, "{text}");
                assert_eq!(native(columns, &rows), eager, "{text}");
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
                let checkpoints = calls.load(Ordering::Relaxed) - before;
                assert!(cursor.next().is_none());
                drop(cursor);

                let mut visible = slim
                    .authorized_read_session(
                        &cx,
                        &authority,
                        &token,
                        BRANCH,
                        symbols,
                        policy(),
                        clock,
                    )
                    .unwrap();
                let prepared = visible.prepare(&cx, &text, &args).unwrap();
                let before = calls.load(Ordering::Relaxed);
                let mut cursor = visible.stream_send(&cx, &prepared, &args).unwrap();
                assert_eq!(
                    cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                    expected,
                    "{text}",
                );
                assert_eq!(
                    calls.load(Ordering::Relaxed) - before,
                    checkpoints,
                    "hidden child data changed logical checkpoints: {text}",
                );
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
                assert!(cursor.next().is_none());
            }
        }
    }
}

#[test]
fn mixed_trail_collections_keep_masks_identity_domains_and_scoped_charges() {
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
    let edges: Vec<_> = COLLECTION_EDGES
        .iter()
        .map(|&(eid, from, to, _)| (EId(eid), VId(from), VId(to)))
        .collect();
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
    for edge_root in [false, true] {
        let (head, prefix) = if edge_root {
            ("MATCH (x)-[r]->(a)", "r,x,a")
        } else {
            ("MATCH (a)", "a")
        };
        for (direction, (left, right)) in [("-", "->"), ("<-", "-"), ("-", "-")]
            .into_iter()
            .enumerate()
        {
            for (variable, atoms) in [(0, 2), (1, 2), (1, 3)] {
                for bounds in [(0, 0), (0, 2)] {
                    let names = ["a", "b", "c", "d"];
                    let end = names[atoms];
                    let mut pattern = String::from("(a)");
                    for atom in 0..atoms {
                        let hops = if atom == variable {
                            format!("*{}..{}", bounds.0, bounds.1)
                        } else {
                            String::new()
                        };
                        pattern.push_str(&format!("{left}[:R{hops}]{right}({})", names[atom + 1]));
                    }
                    for filter in [0, 1, 2] {
                        let predicate = match filter {
                            0 => String::new(),
                            1 => format!("WHERE NOT EXISTS {{ MATCH ({end})-[:S]->(z) }}"),
                            _ => format!(
                                "WHERE EXISTS {{ MATCH ({end})-[child:R]->(z) WHERE child.hidden IS NULL }}"
                            ),
                        };
                        let text = format!(
                            "{head} RETURN {prefix}, [p = {pattern} {predicate} | [p,nodes(p),relationships(p),length(p),{end},{end}.p,{end}.hidden]] AS routes"
                        );
                        let values = |anchor| {
                            mixed_trail::values(
                                mixed_trail::routes(
                                    &edges, anchor, direction, variable, atoms, bounds,
                                )
                                .into_iter()
                                .filter(|path| {
                                    filter != 2
                                        || edges.iter().any(|&(_, source, _)| {
                                            source == mixed_trail::endpoint(path)
                                        })
                                }),
                                |id| id.0 as i64 * 10,
                            )
                        };
                        let expected: Vec<_> = if edge_root {
                            edges
                                .iter()
                                .map(|&(r, x, a)| {
                                    GraphValueRow::from_owned_values(vec![
                                        GraphValue::Edge(r),
                                        GraphValue::Vertex(x),
                                        GraphValue::Vertex(a),
                                        values(a),
                                    ])
                                })
                                .collect()
                        } else {
                            [1_u128, 3, 4]
                                .into_iter()
                                .map(|a| {
                                    GraphValueRow::from_owned_values(vec![
                                        GraphValue::Vertex(VId(a)),
                                        values(VId(a)),
                                    ])
                                })
                                .collect()
                        };
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
                        assert_eq!(
                            visible
                                .stream(&cx, &prepared, &args)
                                .unwrap()
                                .collect::<Result<Vec<_>, _>>()
                                .unwrap(),
                            expected,
                            "{text}"
                        );
                        assert_eq!(
                            calls.get() - before,
                            checkpoints,
                            "hidden graph changed logical charges: {text}"
                        );
                        let mut send = db
                            .authorized_read_session(
                                &cx,
                                &authority,
                                &token,
                                BRANCH,
                                symbols,
                                policy(),
                                || 100,
                            )
                            .unwrap();
                        let prepared = send.prepare(&cx, &text, &args).unwrap();
                        let cursor = send.stream_send(&cx, &prepared, &args).unwrap();
                        fn assert_send<T: Send>(_: &T) {}
                        assert_send(&cursor);
                        assert_eq!(
                            cursor.collect::<Result<Vec<_>, _>>().unwrap(),
                            expected,
                            "{text}"
                        );
                    }
                }
            }
        }
    }
    // A denied variable relation permits its zero-hop witness between visible
    // fixed atoms, but must never contribute a hidden edge or endpoint.
    for minimum in [0, 1] {
        let text = format!(
            "MATCH (a) RETURN a,[p = (a)-[:R]->(b)-[:S*{minimum}..2]->(c)-[:R]->(d) | [p,nodes(p),relationships(p),length(p),d,d.p,d.hidden]] AS routes"
        );
        let expected: Vec<_> = [1_u128, 3, 4]
            .into_iter()
            .map(|a| {
                let paths = if minimum == 0 {
                    mixed_trail::routes(&edges, VId(a), 0, 1, 3, (0, 0))
                } else {
                    Vec::new()
                };
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(a)),
                    mixed_trail::values(paths, |id| id.0 as i64 * 10),
                ])
            })
            .collect();
        let prepared = scoped.prepare(&cx, &text, &args).unwrap();
        let mut cursor = scoped.stream(&cx, &prepared, &args).unwrap();
        let columns = cursor.columns().to_vec();
        let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows, expected, "{text}");
        drop(cursor);
        assert_eq!(
            native(columns, &rows),
            scoped.execute(&cx, &prepared, &args).unwrap()
        );
    }
}

#[test]
fn mixed_capture_lists_remain_private_until_the_authorized_row_is_complete() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let db = runtime.block_on(collection_database(&contexts.commit(), true));
    let authority = issuer();
    let token = authority.issue_at(&grant(), 100).unwrap();
    let args = GqlParameters::new();
    let edges: Vec<_> = COLLECTION_EDGES
        .iter()
        .map(|&(eid, from, to, _)| (EId(eid), VId(from), VId(to)))
        .collect();
    let text = "MATCH (a) RETURN a,[p = (a)-[:R]->(b)-[:R*0..2]->(c)-[:R]->(d) | [p,nodes(p),relationships(p),length(p),d,d.p,d.hidden]] AS routes";
    let expected = GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(1)),
        mixed_trail::values(mixed_trail::routes(&edges, VId(1), 0, 1, 3, (0, 2)), |id| {
            id.0 as i64 * 10
        }),
    ]);
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
        let prepared = session.prepare(&cx, text, &args).unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        active.set(true);
        assert_eq!(cursor.next().unwrap().unwrap(), expected);
        active.set(false);
        calls.get()
    };
    assert!(first_row_calls > 3);
    for cut in [1, first_row_calls / 2, first_row_calls] {
        calls.set(0);
        stop.set(cut);
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
            .unwrap();
        let prepared = session.prepare(&cx, text, &args).unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        active.set(true);
        assert!(matches!(
            cursor.next(),
            Some(Err(QueryError::Authorization(Error::Expired)))
        ));
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
    let prepared = session.prepare(&cx, text, &args).unwrap();
    let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), expected);
    root.set_cancel_reason(
        asupersync::CancelReason::new(asupersync::CancelKind::User)
            .with_message("cancel after one complete mixed path list"),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(QueryError::Pattern(GqlQueryError::Interrupted(_))))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
}

#[test]
fn multiple_trail_collections_keep_every_route_inside_the_authorized_graph() {
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
    let edges: Vec<_> = COLLECTION_EDGES
        .iter()
        .map(|&(eid, from, to, _)| (EId(eid), VId(from), VId(to)))
        .collect();
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
    let shapes: &[(&[&str], &[(usize, usize)])] = &[
        (&["*0..1", "*0..2"], &[(0, 1), (0, 2)]),
        (&["*0..2", "", "*0..2"], &[(0, 2), (1, 1), (0, 2)]),
        (&["*0..1", "*0..1", "*1..2"], &[(0, 1), (0, 1), (1, 2)]),
        (&["*0..0", "*0..0"], &[(0, 0), (0, 0)]),
    ];
    for edge_root in [false, true] {
        let (head, prefix) = if edge_root {
            ("MATCH (x)-[r]->(a)", "r,x,a")
        } else {
            ("MATCH (a)", "a")
        };
        for (direction, (left, right)) in [("-", "->"), ("<-", "-"), ("-", "-")]
            .into_iter()
            .enumerate()
        {
            for &(spellings, bounds) in shapes {
                let names = ["a", "b", "c", "d"];
                let end = names[spellings.len()];
                let mut pattern = String::from("(a)");
                for (at, spelling) in spellings.iter().enumerate() {
                    pattern.push_str(&format!("{left}[:R{spelling}]{right}({})", names[at + 1]));
                }
                for child in [false, true] {
                    let predicate = if child {
                        format!(
                            "WHERE EXISTS {{ MATCH ({end})-[child:R]->(z) WHERE child.hidden IS NULL }} AND NOT EXISTS {{ MATCH ({end})-[:S]->(private) }}"
                        )
                    } else {
                        String::new()
                    };
                    let text = format!(
                        "{head} RETURN {prefix}, [p = {pattern} {predicate} | [p,nodes(p),relationships(p),length(p),{end},{end}.p,{end}.hidden]] AS routes"
                    );
                    let values = |anchor| {
                        mixed_trail::values(
                            mixed_trail::routes_for_bounds(&edges, anchor, direction, bounds)
                                .into_iter()
                                .filter(|path| {
                                    !child
                                        || edges.iter().any(|&(_, from, _)| {
                                            from == mixed_trail::endpoint(path)
                                        })
                                }),
                            |id| id.0 as i64 * 10,
                        )
                    };
                    let expected: Vec<_> = if edge_root {
                        edges
                            .iter()
                            .map(|&(r, x, a)| {
                                GraphValueRow::from_owned_values(vec![
                                    GraphValue::Edge(r),
                                    GraphValue::Vertex(x),
                                    GraphValue::Vertex(a),
                                    values(a),
                                ])
                            })
                            .collect()
                    } else {
                        [1_u128, 3, 4]
                            .into_iter()
                            .map(|a| {
                                GraphValueRow::from_owned_values(vec![
                                    GraphValue::Vertex(VId(a)),
                                    values(VId(a)),
                                ])
                            })
                            .collect()
                    };
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
                    assert_eq!(
                        visible
                            .stream(&cx, &prepared, &args)
                            .unwrap()
                            .collect::<Result<Vec<_>, _>>()
                            .unwrap(),
                        expected,
                        "{text}"
                    );
                    assert_eq!(
                        calls.get() - before,
                        checkpoints,
                        "private graph changed logical charges: {text}"
                    );
                    let mut send = db
                        .authorized_read_session(
                            &cx,
                            &authority,
                            &token,
                            BRANCH,
                            symbols,
                            policy(),
                            || 100,
                        )
                        .unwrap();
                    let prepared = send.prepare(&cx, &text, &args).unwrap();
                    let cursor = send.stream_send(&cx, &prepared, &args).unwrap();
                    fn assert_send<T: Send>(_: &T) {}
                    assert_send(&cursor);
                    assert_eq!(
                        cursor.collect::<Result<Vec<_>, _>>().unwrap(),
                        expected,
                        "{text}"
                    );
                }
            }
        }
    }
    // A denied second trail still contributes exactly its visible zero-hop
    // choice. It cannot expose hidden edge20 or its property.
    for minimum in [0, 1] {
        let text = format!(
            "MATCH (a) RETURN a,[p = (a)-[:R*0..2]->(b)-[:S*{minimum}..2]->(c) | [p,nodes(p),relationships(p),length(p),c,c.p,c.hidden]] AS routes"
        );
        let expected: Vec<_> = [1_u128, 3, 4]
            .into_iter()
            .map(|a| {
                let paths = if minimum == 0 {
                    mixed_trail::routes_for_bounds(&edges, VId(a), 0, &[(0, 2), (0, 0)])
                } else {
                    Vec::new()
                };
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(a)),
                    mixed_trail::values(paths, |id| id.0 as i64 * 10),
                ])
            })
            .collect();
        let prepared = scoped.prepare(&cx, &text, &args).unwrap();
        assert_eq!(
            scoped
                .stream(&cx, &prepared, &args)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected,
            "{text}"
        );
    }
    drop(scoped);
    drop(visible);
    let text = "MATCH (a) RETURN a,[p = (a)-[:R*0..1]->(b)-[:R*0..2]->(c) | [p,nodes(p),relationships(p),length(p),c,c.p,c.hidden]] AS routes";
    let expected = GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(1)),
        mixed_trail::values(
            mixed_trail::routes_for_bounds(&edges, VId(1), 0, &[(0, 1), (0, 2)]),
            |id| id.0 as i64 * 10,
        ),
    ]);
    let mut session = db
        .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
        .unwrap();
    let prepared = session.prepare(&cx, text, &args).unwrap();
    let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), expected);
    root.set_cancel_reason(
        asupersync::CancelReason::new(asupersync::CancelKind::User)
            .with_message("cancel after one complete multiple-trail list"),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(QueryError::Pattern(GqlQueryError::Interrupted(_))))
    ));
    assert_eq!(cursor.state(), VertexScanState::Failed);
    assert!(cursor.next().is_none());
}
