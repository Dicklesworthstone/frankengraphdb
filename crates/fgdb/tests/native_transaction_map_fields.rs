//! MAP field operands in transaction reads use the same argument lowering as
//! ordinary native reads (fgdb-8srlm), while retaining the transaction's source.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, QueryError, QueryResult, QueryValue, ReadError, StatementClock,
    WriteBatch, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalScalar, CanonicalTimestamp, CommitSeq, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, SimulationCheckpointProbe, VId,
};
use std::cell::Cell;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const W: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const NODE: &str = "MATCH (n:Person {p:$m.filter.value})";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn map(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
    .unwrap()
}

fn arguments(value: i64, weight: i64) -> GqlParameters {
    GqlParameters::new()
        .with_map(
            "m",
            vec![
                ("filter".into(), map(vec![("value", int(value))])),
                ("edge".into(), map(vec![("weight", int(weight))])),
            ],
        )
        .unwrap()
}

fn read_text(pattern: &str, tail: &str, historical: bool) -> String {
    let selector = if historical {
        " FOR SYSTEM_TIME AS OF SEQ $at"
    } else {
        ""
    };
    format!("{pattern}{selector} {tail}")
}

fn values(columns: &[&str], rows: Vec<Vec<GraphValue>>) -> QueryResult {
    QueryResult::Rows {
        columns: columns.iter().map(|name| (*name).into()).collect(),
        rows: rows
            .into_iter()
            .map(|row| row.into_iter().map(QueryValue::Value).collect())
            .collect(),
    }
}

fn vertices(rows: &[(u128, i64)]) -> QueryResult {
    values(
        &["id", "p"],
        rows.iter()
            .map(|(id, value)| vec![GraphValue::Vertex(VId(*id)), int(*value)])
            .collect(),
    )
}

fn integers(column: &str, rows: &[i64]) -> QueryResult {
    values(
        &[column],
        rows.iter().map(|value| vec![int(*value)]).collect(),
    )
}

fn edges(rows: &[(u128, u128)]) -> QueryResult {
    values(
        &["edge", "id"],
        rows.iter()
            .map(|(edge, vertex)| {
                vec![
                    GraphValue::Edge(EId(*edge)),
                    GraphValue::Vertex(VId(*vertex)),
                ]
            })
            .collect(),
    )
}

fn graph() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, value) in [(1, 10), (2, 20), (3, 30)] {
        batch.create_vertex(
            VId(id),
            vec![PERSON],
            vec![(P, CanonicalScalar::Int(value))],
        );
    }
    for (id, target, weight) in [(101, 2, 7), (102, 3, 9)] {
        batch.add_edge(
            EId(id),
            VId(1),
            VId(target),
            vec![(W, CanonicalScalar::Int(weight))],
        );
    }
    batch
}

fn staged() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(40)));
    batch.create_vertex(VId(4), vec![PERSON], vec![(P, CanonicalScalar::Int(20))]);
    batch.delete_edge(EId(101));
    batch.add_edge(EId(103), VId(1), VId(4), vec![(W, CanonicalScalar::Int(7))]);
    batch
}

#[test]
fn nested_map_fields_rebind_over_staged_and_committed_history_sources() {
    let ((), report) = run_async_under_lab(0x6d61_7001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, graph()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, staged()).unwrap();
        let effect = txn.staged_effect_digest().unwrap();
        let tail = "RETURN n AS id,n.p AS p ORDER BY id";
        let current = read_text(NODE, tail, false);
        let history = read_text(NODE, tail, true);

        // Each call supplies a newly constructed map. The matching identities
        // differ between the staged workspace and the exact committed basis.
        for (value, staged_rows, historical_rows) in [
            (20, vertices(&[(2, 20), (4, 20)]), vertices(&[(2, 20)])),
            (40, vertices(&[(1, 40)]), vertices(&[])),
            (10, vertices(&[]), vertices(&[(1, 10)])),
            (30, vertices(&[(3, 30)]), vertices(&[(3, 30)])),
            (20, vertices(&[(2, 20), (4, 20)]), vertices(&[(2, 20)])),
        ] {
            let args = arguments(value, 7);
            let historical_args = args.clone().with_uint64("at", basis.0).unwrap();
            let original = args.clone();
            assert_eq!(
                txn.query(&db, &cx, &current, &args, symbols, policy())
                    .unwrap(),
                staged_rows
            );
            assert_eq!(
                txn.query(&db, &cx, &history, &historical_args, symbols, policy())
                    .unwrap(),
                historical_rows
            );
            assert_eq!(
                db.query(&cx, &current, &args, symbols, policy()).unwrap(),
                historical_rows
            );
            assert_eq!(args, original);
            assert_eq!(txn.staged_effect_digest().unwrap(), effect);
        }

        let args = arguments(20, 7);
        let historical_args = args.clone().with_uint64("at", basis.0).unwrap();
        for (tail, staged_rows, historical_rows) in [
            (
                "RETURN count(*) AS total",
                QueryResult::Rows {
                    columns: vec!["total".into()],
                    rows: vec![vec![QueryValue::Count(2)]],
                },
                QueryResult::Rows {
                    columns: vec!["total".into()],
                    rows: vec![vec![QueryValue::Count(1)]],
                },
            ),
            (
                "WITH n.p AS p RETURN sum(p) AS total",
                QueryResult::Rows {
                    columns: vec!["total".into()],
                    rows: vec![vec![QueryValue::Integer(40)]],
                },
                QueryResult::Rows {
                    columns: vec!["total".into()],
                    rows: vec![vec![QueryValue::Integer(20)]],
                },
            ),
            (
                "RETURN n.p AS p UNION ALL MATCH (o:Person {p:$m.filter.value}) RETURN o.p AS p",
                integers("p", &[20, 20, 20, 20]),
                integers("p", &[20, 20]),
            ),
        ] {
            assert_eq!(
                txn.query(
                    &db,
                    &cx,
                    &read_text(NODE, tail, false),
                    &args,
                    symbols,
                    policy()
                )
                .unwrap(),
                staged_rows,
                "{tail}"
            );
            assert_eq!(
                txn.query(
                    &db,
                    &cx,
                    &read_text(NODE, tail, true),
                    &historical_args,
                    symbols,
                    policy()
                )
                .unwrap(),
                historical_rows,
                "{tail}"
            );
        }

        let edge_pattern = "MATCH (a:Person)-[r:R {w:$m.edge.weight}]->(b)";
        let tail = "RETURN r AS edge,b AS id ORDER BY edge";
        for (weight, staged_rows, historical_rows) in [
            (7, edges(&[(103, 4)]), edges(&[(101, 2)])),
            (9, edges(&[(102, 3)]), edges(&[(102, 3)])),
            (88, edges(&[]), edges(&[])),
        ] {
            let args = arguments(20, weight);
            let historical_args = args.clone().with_uint64("at", basis.0).unwrap();
            assert_eq!(
                txn.query(
                    &db,
                    &cx,
                    &read_text(edge_pattern, tail, false),
                    &args,
                    symbols,
                    policy()
                )
                .unwrap(),
                staged_rows
            );
            assert_eq!(
                txn.query(
                    &db,
                    &cx,
                    &read_text(edge_pattern, tail, true),
                    &historical_args,
                    symbols,
                    policy()
                )
                .unwrap(),
                historical_rows
            );
        }

        // The original map remains available outside the pattern. A real
        // parameter with the natural generated name must keep its own value.
        let collision = read_text(
            NODE,
            "RETURN n.p + $m.filter.value + $m_filter_value AS total ORDER BY total",
            false,
        );
        for (value, expected) in [
            (20, &[1040, 1040][..]),
            (40, &[1080][..]),
            (20, &[1040, 1040][..]),
        ] {
            let args = arguments(value, 7)
                .with_int64("m_filter_value", 1000)
                .unwrap();
            assert_eq!(
                txn.query(&db, &cx, &collision, &args, symbols, policy())
                    .unwrap(),
                integers("total", expected)
            );
        }
        assert_eq!(txn.staged_effect_digest().unwrap(), effect);
        assert_eq!(db.frontier().unwrap(), basis);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.query(&cx, &current, &args, symbols, policy()).unwrap(),
            vertices(&[(2, 20), (4, 20)])
        );
        assert_eq!(
            db.query(&cx, &history, &historical_args, symbols, policy())
                .unwrap(),
            vertices(&[(2, 20)])
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn missing_fields_and_refusals_preserve_workspace_and_owner_precedence() {
    let ((), report) = run_async_under_lab(0x6d61_7002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, graph()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, staged()).unwrap();
        let effect = txn.staged_effect_digest().unwrap();
        let text = read_text(NODE, "RETURN n AS id,n.p AS p ORDER BY id", false);

        for entries in [
            vec![],
            vec![("filter".into(), map(vec![]))],
            vec![("filter".into(), GraphValue::Scalar(CanonicalScalar::Null))],
            vec![(
                "filter".into(),
                map(vec![("value", GraphValue::Scalar(CanonicalScalar::Null))]),
            )],
            vec![(
                "filter".into(),
                map(vec![(
                    "value",
                    GraphValue::Scalar(CanonicalScalar::ucs_basic_text("20").unwrap()),
                )]),
            )],
        ] {
            let args = GqlParameters::new().with_map("m", entries).unwrap();
            assert_eq!(
                txn.query(&db, &cx, &text, &args, symbols, policy())
                    .unwrap(),
                vertices(&[])
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), effect);
        }

        // Unsuitable inputs stay on the ordinary parser's refusal path. In
        // particular, a whole parameter map is not accepted as a MATCH map.
        for (statement, args) in [
            (text.as_str(), GqlParameters::new()),
            (
                text.as_str(),
                GqlParameters::new().with_int64("m", 20).unwrap(),
            ),
            (
                text.as_str(),
                GqlParameters::new()
                    .with_map("m", vec![("filter".into(), int(20))])
                    .unwrap(),
            ),
            (
                text.as_str(),
                GqlParameters::new()
                    .with_map(
                        "m",
                        vec![(
                            "filter".into(),
                            map(vec![("value", GraphValue::Vertex(VId(2)))]),
                        )],
                    )
                    .unwrap(),
            ),
            ("MATCH (n:Person $m) RETURN n.p AS p", arguments(20, 7)),
        ] {
            let original = args.clone();
            let ordinary = db
                .query(&cx, statement, &args, symbols, policy())
                .unwrap_err();
            let error = txn
                .query(&db, &cx, statement, &args, symbols, policy())
                .unwrap_err();
            assert_eq!(error.to_string(), ordinary.to_string(), "{statement}");
            assert_eq!(args, original);
            assert_eq!(txn.staged_effect_digest().unwrap(), effect);
        }

        let malformed = GqlParameters::new().with_int64("m", 20).unwrap();
        let calls = Cell::new(0);
        let counted = |kind: GraphSymbolKind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        };
        assert!(matches!(
            txn.query(&other, &cx, &text, &malformed, counted, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(QueryError::Transaction(error)) if matches!(error.as_ref(), WriteTxnError::WrongDatabase)
        ));
        assert_eq!(calls.get(), 0);

        let args = arguments(20, 7);
        let error = txn
            .query(
                &db,
                &cx,
                &text,
                &args,
                symbols,
                GqlQueryPolicy::new(10_000, 0, 1_000_000, 1_000_000),
            )
            .unwrap_err();
        assert!(
            matches!(error, QueryError::TransactionPattern(error) if matches!(error.as_ref(), GqlQueryError::Rows(_)))
        );
        let cancelled = cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
        assert!(matches!(
            txn.query(&db, &cancelled, &text, &args, symbols, policy()),
            Err(QueryError::TransactionPattern(error)) if matches!(error.as_ref(), GqlQueryError::Interrupted(_))
        ));
        let future = CommitSeq(basis.0 + 1);
        let future_args = args.clone().with_uint64("at", future.0).unwrap();
        let historical = read_text(NODE, "RETURN n.p AS p LIMIT 0", true);
        assert!(matches!(
            txn.query(&db, &cx, &historical, &future_args, symbols, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(QueryError::Read(ReadError::BeyondFrontier { asked, frontier })) if asked == future && frontier == basis
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), effect);
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertex(VId(4)).unwrap().is_none());
        assert_eq!(
            txn.query(&db, &cx, &text, &args, symbols, policy())
                .unwrap(),
            vertices(&[(2, 20), (4, 20)])
        );
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(4)).unwrap().is_some());
        assert!(matches!(
            txn.query(&db, &cx, &text, &malformed, counted, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(QueryError::Transaction(error)) if matches!(error.as_ref(), WriteTxnError::Finished)
        ));
        assert_eq!(calls.get(), 0);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn lowering_preserves_one_statement_clock_sample_and_explicit_time() {
    let ((), report) = run_async_under_lab(0x6d61_7003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, graph()).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        db.set_statement_clock(Some(StatementClock::new(move |_| {
            let milliseconds = observed.fetch_add(1, Ordering::SeqCst) + 1;
            Some(CanonicalTimestamp::offset_only(milliseconds as i128 * 1_000_000, 0).unwrap())
        })));
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, staged()).unwrap();
        let args = arguments(20, 7);
        assert_eq!(
            txn.query(
                &db,
                &cx,
                &read_text(NODE, "RETURN n.p AS p", false),
                &args,
                symbols,
                policy()
            )
            .unwrap(),
            integers("p", &[20, 20])
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let timed = read_text(
            NODE,
            "RETURN timestamp() AS first,timestamp() AS second",
            false,
        );
        for milliseconds in [1, 2] {
            assert_eq!(
                txn.query(&db, &cx, &timed, &args, symbols, policy())
                    .unwrap(),
                values(
                    &["first", "second"],
                    vec![
                        vec![int(milliseconds), int(milliseconds)],
                        vec![int(milliseconds), int(milliseconds)]
                    ]
                )
            );
            assert_eq!(calls.load(Ordering::SeqCst), milliseconds as usize);
        }
        let explicit = args
            .with_statement_time(CanonicalTimestamp::offset_only(99_000_000, 0).unwrap())
            .unwrap();
        assert_eq!(
            txn.query(&db, &cx, &timed, &explicit, symbols, policy())
                .unwrap(),
            values(
                &["first", "second"],
                vec![vec![int(99), int(99)], vec![int(99), int(99)]]
            )
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
