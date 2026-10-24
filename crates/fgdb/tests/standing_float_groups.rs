//! Ordinary native numeric subscriptions use the same exact group circuit as
//! explicit relational pipelines. Snapshot execution is an independent oracle.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, QueryValue,
    StandingQueryHandle, WriteBatch,
};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZSet};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};

const P: PropertyKeyId = PropertyKeyId(1);
const G: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const NUMERIC: &str = "MATCH (n) RETURN SUM(n.p) AS total, AVG(n.p) AS mean, SUM(DISTINCT n.p) AS unique_total, AVG(DISTINCT n.p) AS unique_mean";
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 50_000_000, 50_000_000)
}
fn float(value: f64) -> CanonicalScalar {
    CanonicalScalar::Float(CanonicalF64::new(value))
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "g") => Some(GraphSymbol::Property(G)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
async fn fixture(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    );
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, group, value) in [
        (1, 1, float(1e16)),
        (2, 1, float(1.0)),
        (3, 1, float(-1e16)),
        (4, 2, CanonicalScalar::Int(3)),
        (5, 2, CanonicalScalar::Null),
    ] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![(P, value), (G, CanonicalScalar::Int(group))],
        );
    }
    for (id, source, destination) in [(1, 1, 2), (2, 1, 2), (3, 2, 3), (4, 4, 5)] {
        batch.add_edge(EId(id), VId(source), VId(destination), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn register(db: &mut Database<MemVfs>, cx: &QueryCx, text: &str) -> StandingQueryHandle {
    db.register_standing_native(cx, text, &GqlParameters::new(), symbols, policy())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}
fn differential(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    handle: &StandingQueryHandle,
) -> Vec<Vec<QueryValue>> {
    let (at, actual) = db.standing_native_query(cx, handle, policy()).unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(
        actual,
        db.query(cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap(),
        "{text}"
    );
    let QueryResult::Rows { rows, .. } = actual else {
        panic!("read returned effects")
    };
    rows
}
fn copy<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
        .unwrap()
}
fn f(value: f64) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(float(value)))
}

#[test]
fn native_numeric_graph_shapes_match_snapshots_through_changes_and_empty_input() {
    let ((), report) = run_async_under_lab(0x666c_0101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = fixture(&commit).await;
        let texts = [
            NUMERIC,
            "MATCH (n) RETURN n.g AS bucket, SUM(n.p) AS total, AVG(n.p) AS mean GROUP BY n.g HAVING COUNT(*) > 0 ORDER BY total DESC LIMIT 2",
            "MATCH (n) RETURN SUM(n.p * 2) AS total, AVG(n.p * 2) AS mean",
            "MATCH (n) WITH n.p AS value RETURN SUM(value) AS total, AVG(value) AS mean",
            "MATCH (a)-[:R]->(b) RETURN SUM(b.p) AS total, AVG(DISTINCT b.p) AS mean",
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN SUM(c.p) AS total, AVG(c.p) AS mean",
            "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN SUM(b.p) AS total, COUNT(*) AS copies",
            "MATCH (n) RETURN collect(n.p) AS values, SUM(n.p) AS total, AVG(n.p) AS mean",
            "MATCH (n) WITH [n.p, n.p] AS xs UNWIND xs AS x RETURN SUM(x) AS total, AVG(DISTINCT x) AS mean",
        ];
        let handles: Vec<_> = texts
            .iter()
            .map(|text| register(&mut db, &cx, text))
            .collect();
        assert_eq!(
            differential(&db, &cx, NUMERIC, &handles[0]),
            vec![vec![f(4.0), f(1.0), f(4.0), f(1.0)]]
        );
        let mut bags: Vec<_> = handles
            .iter()
            .map(|h| copy(db.standing_query(&cx, h).unwrap().rows()))
            .collect();
        for step in 0..5 {
            let mut edit = WriteBatch::new(R);
            match step {
                0 => {
                    edit.set_vertex_property(VId(2), P, Some(float(2.0)));
                }
                1 => {
                    edit.set_vertex_property(VId(4), P, Some(float(0.25)));
                }
                2 => {
                    edit.delete_vertex(VId(1));
                    edit.delete_vertex(VId(3));
                }
                3 => {
                    edit.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(2)));
                    edit.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(3)));
                }
                _ => {
                    for id in [2, 4, 5] {
                        edit.delete_vertex(VId(id));
                    }
                }
            }
            let at = db.write(&commit, edit).await.unwrap();
            for ((text, handle), bag) in texts.iter().zip(&handles).zip(&mut bags) {
                let delta = db.standing_group_delta(&cx, handle).unwrap().unwrap();
                assert_eq!(delta.frontier(), at);
                bag.integrate(delta.rows(), LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(&*bag, db.standing_query(&cx, handle).unwrap().rows());
                differential(&db, &cx, text, handle);
            }
            if step == 3 {
                let values = differential(&db, &cx, NUMERIC, &handles[0]);
                assert_eq!(values[0][0].as_integer(), Some(5));
                let mean = values[0][1].as_average().unwrap();
                assert_eq!((mean.numerator(), mean.denominator()), (5, 2));
            }
        }
        assert!(
            differential(&db, &cx, NUMERIC, &handles[0])[0]
                .iter()
                .all(QueryValue::is_null)
        );
        for (text, handle) in texts.iter().zip(&handles) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_group_delta(&cx, handle).unwrap().is_none());
            differential(&db, &cx, text, handle);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn footprint_accounts_for_the_numeric_source_projection_and_group_before_admission() {
    let ((), report) = run_async_under_lab(0x666c_0102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = fixture(&c.commit()).await;
        let args = GqlParameters::new();
        for (text, expected) in [
            (NUMERIC, (2, 1)),
            ("MATCH (n) RETURN SUM(n.p * 2) AS total", (3, 1)),
            ("MATCH (n) RETURN COUNT(*) AS count", (1, 1)),
            (
                "MATCH (n) RETURN collect(n.p) AS values, SUM(n.p) AS total",
                (3, 1),
            ),
        ] {
            let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
            assert_eq!(
                prepared.subscription_footprint(&cx, &args).unwrap(),
                expected,
                "{text}"
            );
            prepared
                .register_standing(&mut db, &cx, &args, policy())
                .unwrap();
        }
        let healthy = register(&mut db, &cx, NUMERIC);
        let expected = differential(&db, &cx, NUMERIC, &healthy);
        for denied in [
            GqlQueryPolicy::new(100_000, 100_000, 0, 50_000_000),
            GqlQueryPolicy::new(100_000, 100_000, 50_000_000, 0),
            GqlQueryPolicy::new(0, 100_000, 50_000_000, 50_000_000),
        ] {
            assert!(
                db.register_standing_native(&cx, NUMERIC, &args, symbols, denied)
                    .is_err()
            );
            assert_eq!(differential(&db, &cx, NUMERIC, &healthy), expected);
        }
        let retried = register(&mut db, &cx, NUMERIC);
        assert_eq!(differential(&db, &cx, NUMERIC, &retried), expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn numeric_input_failures_remain_visible_under_limit_zero_and_rebuild_is_explicit() {
    let ((), report) = run_async_under_lab(0x666c_0103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = fixture(&commit).await;
        let text = "MATCH (n) RETURN SUM(10 / n.p) AS total LIMIT 0";
        let handle = register(&mut db, &cx, text);
        let healthy = register(&mut db, &cx, NUMERIC);
        assert!(differential(&db, &cx, text, &handle).is_empty());
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(2), P, Some(float(0.0)));
        let at = db.write(&commit, edit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(
            db.query(&cx, text, &GqlParameters::new(), symbols, policy())
                .is_err()
        );
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        assert!(db.rebuild_standing_query(&cx, &handle, policy()).is_err());
        differential(&db, &cx, NUMERIC, &healthy);
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(2), P, Some(float(2.0)));
        db.write(&commit, repair).await.unwrap();
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert!(differential(&db, &cx, text, &handle).is_empty());
        let count_text = "MATCH (n) RETURN COUNT(*) AS count";
        let count = register(&mut db, &cx, count_text);
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(2), P, Some(CanonicalScalar::Bool(true)));
        db.write(&commit, edit).await.unwrap();
        assert!(
            db.query(&cx, NUMERIC, &GqlParameters::new(), symbols, policy())
                .is_err()
        );
        assert!(db.standing_native_query(&cx, &healthy, policy()).is_err());
        differential(&db, &cx, count_text, &count);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn numeric_subscription_replay_delivers_exact_float_and_type_transition_deltas() {
    let ((), report) = run_async_under_lab(0x666c_0104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = fixture(&commit).await;
        let handle = register(&mut db, &cx, NUMERIC);
        let mut subscription = db.open_standing_subscription(&cx, &handle).unwrap();
        subscription
            .enable_replay(&mut db, &cx, 8, 1000, 1_000_000, policy())
            .unwrap();
        let baseline = subscription.poll(&db, &cx, policy()).unwrap().unwrap();
        let mut accepted = copy(baseline.rows());
        subscription.acknowledge(baseline.receipt()).unwrap();
        let mut expected = Vec::new();
        for value in [
            float(f64::NAN),
            float(f64::INFINITY),
            CanonicalScalar::Int(2),
        ] {
            let mut edit = WriteBatch::new(R);
            edit.set_vertex_property(VId(2), P, Some(value));
            edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
            edit.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(-1)));
            let at = db.write(&commit, edit).await.unwrap();
            differential(&db, &cx, NUMERIC, &handle);
            expected.push((
                at,
                db.standing_native_bag(&cx, &handle, policy()).unwrap().1,
            ));
        }
        for (at, expected) in expected {
            let frame = subscription.poll(&db, &cx, policy()).unwrap().unwrap();
            assert!(!frame.is_snapshot());
            assert_eq!(frame.frontier(), at);
            let redelivered = subscription.poll(&db, &cx, policy()).unwrap().unwrap();
            assert_eq!(redelivered.frontier(), frame.frontier());
            assert_eq!(redelivered.rows(), frame.rows());
            accepted
                .integrate(frame.rows(), LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                .unwrap();
            assert_eq!(accepted, expected);
            // A receipt is an ACK capability with no equality. The redelivered
            // frame's receipt names this same delivery: it acknowledges `at`.
            assert_eq!(subscription.acknowledge(redelivered.receipt()).unwrap(), at);
        }
        assert!(subscription.poll(&db, &cx, policy()).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
