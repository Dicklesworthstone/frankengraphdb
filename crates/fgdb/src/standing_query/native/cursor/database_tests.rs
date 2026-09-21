use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

fn keys() -> DatabaseKeys { DatabaseKeys::new([51; 32], DatabaseSecurityNamespaceId([52; 32]), [53; 32]) }
fn maintain() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn write(id: u128, value: Option<i64>) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(VId(id), vec![], vec![(PropertyKeyId(1), value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))]);
    batch
}

#[test]
fn every_native_result_family_pulls_the_same_order_cells_and_frontier_as_collection() {
    let ((), report) = run_async_under_lab(0x5c11_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        for (id, value) in [(1, Some(1)), (2, Some(1)), (3, Some(4)), (u128::MAX, None)] {
            db.write(&commit, write(id, value)).await.unwrap();
        }
        let params = GqlParameters::new();
        for text in [
            "MATCH (n) RETURN n.p AS p",
            "MATCH (n) RETURN DISTINCT n.p AS p",
            "MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 3",
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            "MATCH (n) WITH n.p AS p RETURN p + 1 AS next ORDER BY next DESC LIMIT 3",
            "MATCH (n) RETURN SUM(n.p) AS s,n AS id,COUNT(*) AS c GROUP BY n ORDER BY s DESC LIMIT 3",
            "MATCH (n) WITH n.p AS p RETURN p AS k,COUNT(*) AS c GROUP BY p HAVING c >= 1 ORDER BY c DESC LIMIT 2",
            "UNWIND [3,1,3,NULL] AS x RETURN x SKIP 1 LIMIT 2",
            "MATCH (n) WITH [n.p,n.p] AS xs UNWIND xs AS x RETURN x",
            "MATCH (n) WITH [n.p,n.p] AS xs UNWIND xs AS x RETURN SUM(x) AS s,AVG(x) AS a,COUNT(DISTINCT x) AS c",
        ] {
            let handle = db.register_standing_native(&cx, text, &params, symbols, maintain()).unwrap();
            let (at, QueryResult::Rows { columns, rows }) = db.standing_native_query(&cx, &handle, policy()).unwrap() else { panic!("row result") };
            let temporary = handle.clone();
            let mut cursor = db.standing_native_cursor(&cx, &temporary, policy()).unwrap();
            drop(temporary); // The cursor's metadata is not borrowed from the handle.
            assert_eq!(cursor.columns(), columns);
            assert_eq!(cursor.snapshot_seq(), at);
            assert_eq!(cursor.row_stats(), GqlExecutionStats { snapshot_records: 0, result_rows: 0 });
            assert_eq!(cursor.evaluator_stats(), GlaExecutionStats { work_units: 1, scratch_entries: 1 });
            assert_eq!(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), rows, "{text}");
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
            let usage = cursor.evaluator_stats();
            assert!(cursor.next().is_none()); cursor.close();
            assert_eq!(cursor.state(), VertexScanState::Exhausted); assert_eq!(cursor.evaluator_stats(), usage);
            drop(cursor);
            db.rebuild_standing_query(&cx, &handle, maintain()).unwrap();
            assert_eq!(db.standing_native_cursor(&cx, &handle, policy()).unwrap().collect::<Result<Vec<_>, _>>().unwrap(), rows);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_delivery_quotas_and_early_close_do_not_fence_committed_maintenance() {
    let ((), report) = run_async_under_lab(0x5c11_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, write(1, Some(7))).await.unwrap();
        db.write(&commit, write(2, Some(7))).await.unwrap();
        let handle = db.register_standing_native(&cx, "MATCH (n) RETURN n.p AS p",
            &GqlParameters::new(), symbols, maintain()).unwrap();
        let at = db.frontier().unwrap();
        let mut short = db.standing_native_cursor(&cx, &handle, GqlQueryPolicy::new(0, 1, 1000, 1000)).unwrap();
        let mut all = db.standing_native_cursor(&cx, &handle, policy()).unwrap();
        assert_eq!(short.next().unwrap().unwrap(), all.next().unwrap().unwrap());
        assert!(matches!(short.next(), Some(Err(StandingQueryError::Delivery(StandingQueryFailure::ResultBudget)))));
        assert!(short.next().is_none()); assert_eq!(short.state(), VertexScanState::Failed);
        assert!(all.next().unwrap().is_ok()); assert!(all.next().is_none());
        assert_eq!(short.snapshot_seq(), at); assert_eq!(all.snapshot_seq(), at);
        drop(short); drop(all);
        let mut closed = db.standing_native_cursor(&cx, &handle, policy()).unwrap();
        closed.close(); let usage = closed.evaluator_stats();
        assert!(closed.next().is_none()); assert_eq!(closed.evaluator_stats(), usage);
        assert_eq!(closed.state(), VertexScanState::Closed); drop(closed);
        let at = db.write(&commit, write(3, Some(9))).await.unwrap();
        let mut next = db.standing_native_cursor(&cx, &handle, policy()).unwrap();
        assert_eq!(next.snapshot_seq(), at);
        let rows = next.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows, vec![vec![QueryValue::Value(int(7))], vec![QueryValue::Value(int(7))], vec![QueryValue::Value(int(9))]]);
        let stats = next.evaluator_stats(); drop(next);
        let exact = GqlQueryPolicy::new(0, 3, stats.work_units, stats.scratch_entries);
        assert_eq!(db.standing_native_cursor(&cx, &handle, exact).unwrap().collect::<Result<Vec<_>, _>>().unwrap(), rows);
        assert!(db.standing_native_query(&cx, &handle, policy()).is_ok());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn admission_refuses_foreign_unavailable_and_non_native_handles_without_row_delivery() {
    let ((), report) = run_async_under_lab(0x5c11_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, write(1, Some(7))).await.unwrap();
        let params = GqlParameters::new();
        let handle = db.register_standing_native(&cx, "MATCH (n) RETURN n.p AS p", &params,
            symbols, GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000)).unwrap();
        assert!(matches!(other.standing_native_cursor(&cx, &handle, policy()), Err(StandingQueryError::ForeignHandle)));
        let raw = fgdb_gql::PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p", symbols).unwrap().bind_parameters(&params).unwrap();
        let raw = db.register_standing_rows(&cx, raw, maintain()).unwrap();
        assert!(matches!(db.standing_native_cursor(&cx, &raw, policy()), Err(StandingQueryError::Unsupported)));
        for allowance in [GqlQueryPolicy::new(0, 100, 0, 100), GqlQueryPolicy::new(0, 100, 100, 0)] {
            assert!(matches!(db.standing_native_cursor(&cx, &handle, allowance), Err(StandingQueryError::Delivery(_))));
        }
        assert!(db.standing_native_query(&cx, &handle, policy()).is_ok());
        db.write(&commit, write(2, Some(9))).await.unwrap();
        assert!(matches!(db.standing_native_cursor(&cx, &handle, policy()), Err(StandingQueryError::Unavailable { .. })));
        db.rebuild_standing_query(&cx, &handle, maintain()).unwrap();
        assert_eq!(db.standing_native_cursor(&cx, &handle, policy()).unwrap().collect::<Result<Vec<_>, _>>().unwrap().len(), 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
