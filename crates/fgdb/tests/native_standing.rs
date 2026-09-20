//! Native text, bound maintenance and final native cells must agree at every cut.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, PreparedNativeRead, QueryError, QueryResult, QueryValue,
    StandingQueryError, StandingQueryFailure, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_gql::algebra::GraphValue;
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::cell::Cell;

const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x41; 32], DatabaseSecurityNamespaceId([0x42; 32]), [0x43; 32])
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "R") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn insert(id: u128, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(VId(id), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(value))]);
    batch
}
fn integers(values: &[i64]) -> QueryResult {
    QueryResult::Rows { columns: vec!["p".into()], rows: values.iter().map(|&n|
        vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n)))]).collect() }
}

#[test]
fn native_patterns_aggregates_and_pipelines_stay_equal_after_each_commit() {
    let ((), report) = run_async_under_lab(0x6e73_7101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let text = [
            "MATCH (n:L) RETURN n.p AS p ORDER BY p DESC SKIP 1 LIMIT 3",
            "MATCH (n:L) RETURN DISTINCT n.p AS p ORDER BY p DESC",
            "MATCH (n:L) RETURN SUM(n.p) AS total, n.p AS key, COUNT(*) AS amount ORDER BY key DESC",
            "MATCH (n:L) RETURN COUNT(*) AS amount, SUM(n.p) AS total, AVG(n.p) AS mean",
            "MATCH (n:L) WITH n.p AS p RETURN SUM(p) AS total, COUNT(*) AS amount",
        ];
        let params = GqlParameters::new();
        let handles: Vec<_> = text.iter().map(|q|
            db.register_standing_native(&cx, q, &params, symbols, policy()).unwrap()).collect();
        for (id, value) in [(1, 4), (2, 4), (3, 9), (4, 2)] {
            db.write(&commit, insert(id, value)).await.unwrap();
            for (q, handle) in text.iter().zip(&handles) {
                let (at, actual) = db.standing_native_query(&cx, handle, policy()).unwrap();
                assert_eq!(at, db.frontier().unwrap());
                assert_eq!(actual, db.query(&cx, q, &params, symbols, policy()).unwrap(), "{q}");
            }
        }
        // The named order is not the Z-set's ascending key order, and ALL
        // occurrences must not be silently collapsed by the native adapter.
        assert_eq!(db.standing_native_query(&cx, &handles[0], policy()).unwrap().1, integers(&[4, 4, 2]));
        assert_eq!(db.standing_native_query(&cx, &handles[1], policy()).unwrap().1, integers(&[9, 4, 2]));
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(1));
        edit.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(4)));
        db.write(&commit, edit).await.unwrap();
        for (q, handle) in text.iter().zip(&handles) {
            assert_eq!(db.standing_native_query(&cx, handle, policy()).unwrap().1,
                db.query(&cx, q, &params, symbols, policy()).unwrap(), "{q}");
        }
        assert_eq!(db.standing_native_columns(&cx, &handles[2]).unwrap(), &["total", "key", "amount"]);
        db.rebuild_standing_query(&cx, &handles[2], policy()).unwrap();
        assert_eq!(db.standing_native_query(&cx, &handles[2], policy()).unwrap().1,
            db.query(&cx, text[2], &params, symbols, policy()).unwrap());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_registration_freezes_arguments_without_retaining_resolver_or_template() {
    let ((), report) = run_async_under_lab(0x6e73_7102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, insert(1, 4)).await.unwrap();
        let low = GqlParameters::new().with_int64("floor", 3).unwrap();
        let high = GqlParameters::new().with_int64("floor", 8).unwrap();
        let calls = Cell::new(0);
        let text = String::from("MATCH (n) WHERE n.p > $floor RETURN n.p AS p ORDER BY p");
        let prepared = PreparedNativeRead::prepare(&text, &low, |kind, name: &str| {
            calls.set(calls.get() + 1); symbols(kind, name)
        }).unwrap();
        let resolved = calls.get();
        assert!(resolved > 0);
        let a = prepared.register_standing(&mut db, &cx, &low, policy()).unwrap();
        let b = prepared.register_standing(&mut db, &cx, &high, policy()).unwrap();
        assert!(matches!(prepared.register_standing(&mut db, &cx, &GqlParameters::new(), policy()),
            Err(StandingQueryError::NativePrepare(error)) if matches!(*error, QueryError::PatternText(_))));
        drop(prepared); drop(text); drop(low); drop(high);
        db.write(&commit, insert(2, 11)).await.unwrap();
        assert_eq!(db.standing_native_query(&cx, &a, policy()).unwrap().1, integers(&[4, 11]));
        assert_eq!(db.standing_native_query(&cx, &b, policy()).unwrap().1, integers(&[11]));
        assert_eq!(calls.get(), resolved);
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(other.standing_native_query(&cx, &a, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(StandingQueryError::ForeignHandle)));
        assert!(matches!(other.standing_native_columns(&cx, &a), Err(StandingQueryError::ForeignHandle)));
        assert!(!format!("{a:?}").contains("floor"));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn delivery_refusals_do_not_fence_views_and_rebuild_retains_native_layout() {
    let ((), report) = run_async_under_lab(0x6e73_7103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        let text = "MATCH (n) RETURN n.p AS p ORDER BY p";
        let bounded = db.register_standing_native(&cx, text, &params, symbols,
            GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000)).unwrap();
        let healthy = db.register_standing_native(&cx, text, &params, symbols, policy()).unwrap();
        db.write(&commit, insert(1, 4)).await.unwrap();
        for allowance in [GqlQueryPolicy::new(0, 0, 100_000, 100_000),
            GqlQueryPolicy::new(0, 10, 0, 100_000), GqlQueryPolicy::new(0, 10, 100_000, 0)] {
            assert!(matches!(db.standing_native_query(&cx, &healthy, allowance),
                Err(StandingQueryError::Delivery(_))));
            assert_eq!(db.standing_native_query(&cx, &healthy, policy()).unwrap().1, integers(&[4]));
        }
        db.write(&commit, insert(2, 7)).await.unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert!(matches!(db.standing_native_query(&cx, &bounded, policy()),
            Err(StandingQueryError::Unavailable { reason: StandingQueryFailure::ResultBudget, .. })));
        assert_eq!(db.standing_native_query(&cx, &healthy, policy()).unwrap().1, integers(&[4, 7]));
        assert!(db.rebuild_standing_query(&cx, &bounded, GqlQueryPolicy::new(0, 0, 0, 0)).is_err());
        assert!(matches!(db.standing_native_query(&cx, &bounded, policy()), Err(StandingQueryError::Unavailable { .. })));
        db.rebuild_standing_query(&cx, &bounded, policy()).unwrap();
        assert_eq!(db.standing_native_columns(&cx, &bounded).unwrap(), &["p"]);
        assert_eq!(db.standing_native_query(&cx, &bounded, policy()).unwrap().1, integers(&[4, 7]));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
