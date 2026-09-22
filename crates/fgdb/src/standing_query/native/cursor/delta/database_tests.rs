use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::collections::BTreeMap;

type Bag = BTreeMap<Vec<QueryValue>, i128>;
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([61; 32], DatabaseSecurityNamespaceId([62; 32]), [63; 32])
}
fn maintain() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn write(id: u128, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(
        VId(id),
        vec![],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
    );
    batch
}
fn bag(result: QueryResult) -> Bag {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("read result")
    };
    let mut bag = Bag::new();
    for row in rows {
        *bag.entry(row).or_default() += 1;
    }
    bag
}
fn snapshot(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle) -> (CommitSeq, Bag) {
    let (at, result) = db.standing_native_query(cx, handle, maintain()).unwrap();
    (at, bag(result))
}
fn integrate(mut old: Bag, delta: Vec<(Vec<QueryValue>, ZWeight)>) -> Bag {
    for (row, weight) in delta {
        *old.entry(row).or_default() += weight.to_i128().unwrap();
    }
    old.retain(|_, weight| *weight != 0);
    assert!(old.values().all(|weight| *weight > 0));
    old
}

#[test]
fn compressed_native_changes_integrate_to_fresh_answers_across_query_families() {
    let ((), report) = run_async_under_lab(0x5c12_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        for (id, value) in [(1, 1), (2, 1), (3, 4)] {
            db.write(&commit, write(id, value)).await.unwrap();
        }
        let params = GqlParameters::new();
        let texts = [
            "MATCH (n) RETURN n.p AS p",
            "MATCH (n) RETURN DISTINCT n.p AS p",
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            "MATCH (n) WITH n.p AS p RETURN p + 1 AS next",
            "MATCH (n) WITH n.p AS p WHERE p > 1 RETURN p",
            "MATCH (n) WITH n.p AS p RETURN p ORDER BY p DESC LIMIT 2",
            "MATCH (n) WITH [n.p,n.p] AS xs UNWIND xs AS x RETURN x",
            "MATCH (n) WITH n.p AS p RETURN SUM(p) AS s,COUNT(*) AS c,AVG(p) AS a",
            "MATCH (n) WITH n.p AS p RETURN COUNT(*) AS c,p AS k GROUP BY p HAVING c >= 1 ORDER BY c DESC LIMIT 2",
            "UNWIND [3,1,3] AS x RETURN x",
            "RETURN 7 AS p UNION ALL MATCH (n) RETURN n.p AS p",
        ];
        let handles: Vec<_> = texts
            .iter()
            .map(|text| {
                db.register_standing_native(&cx, text, &params, symbols, maintain())
                    .unwrap()
            })
            .collect();
        let mut accepted: Vec<_> = handles.iter().map(|h| snapshot(&db, &cx, h)).collect();
        for h in &handles {
            assert!(
                db.standing_native_delta_cursor(&cx, h, db.frontier().unwrap(), policy())
                    .unwrap()
                    .is_none()
            );
        }
        for tick in 0..3 {
            let mut change = write(20 + tick, 9 - tick as i64);
            change.delete_vertex(VId(1 + tick));
            let at = db.write(&commit, change).await.unwrap();
            for ((text, handle), (after, accepted)) in texts.iter().zip(&handles).zip(&mut accepted)
            {
                let temporary = handle.clone();
                let mut cursor = db
                    .standing_native_delta_cursor(&cx, &temporary, *after, policy())
                    .unwrap()
                    .unwrap();
                drop(temporary);
                assert_eq!(cursor.after_seq(), *after);
                assert_eq!(cursor.snapshot_seq(), at);
                assert_eq!(cursor.row_stats().snapshot_records, 0);
                assert_eq!(cursor.row_stats().result_rows, 0);
                assert_eq!(
                    cursor.evaluator_stats(),
                    GlaExecutionStats {
                        work_units: 1,
                        scratch_entries: 1
                    }
                );
                let changes = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(cursor.row_stats().result_rows, changes.len() as u64);
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
                let stats = cursor.evaluator_stats();
                assert!(cursor.next().is_none());
                cursor.close();
                assert_eq!(cursor.evaluator_stats(), stats);
                drop(cursor);
                let old = std::mem::take(accepted);
                *accepted = integrate(old, changes);
                assert_eq!(*accepted, snapshot(&db, &cx, handle).1, "{text}");
                assert_eq!(
                    *accepted,
                    bag(db.query(&cx, text, &params, symbols, maintain()).unwrap()),
                    "{text}"
                );
                *after = at;
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn baselines_empty_ticks_and_missed_commits_remain_distinct_across_rebuilds() {
    let ((), report) = run_async_under_lab(0x5c12_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let handle = db
            .register_standing_native(
                &cx,
                "UNWIND [3,1,3] AS x RETURN x",
                &GqlParameters::new(),
                symbols,
                maintain(),
            )
            .unwrap();
        let baseline = db.frontier().unwrap();
        assert!(
            db.standing_native_delta_cursor(&cx, &handle, baseline, policy())
                .unwrap()
                .is_none()
        );
        let first = db.write(&commit, write(1, 1)).await.unwrap();
        {
            let mut cursor = db
                .standing_native_delta_cursor(
                    &cx,
                    &handle,
                    baseline,
                    GqlQueryPolicy::new(0, 0, 10, 10),
                )
                .unwrap()
                .unwrap();
            assert_eq!(cursor.snapshot_seq(), first);
            assert!(cursor.next().is_none());
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
        let second = db.write(&commit, write(2, 2)).await.unwrap();
        for after in [baseline, second, CommitSeq(u64::MAX)] {
            assert!(
                matches!(db.standing_native_delta_cursor(&cx, &handle, after, policy()),
                Err(StandingQueryError::DeltaGap { after: got, frontier }) if got == after && frontier == second)
            );
        }
        assert!(
            db.standing_native_delta_cursor(&cx, &handle, first, policy())
                .unwrap()
                .unwrap()
                .next()
                .is_none()
        );
        let unchanged = snapshot(&db, &cx, &handle).1;
        db.rebuild_standing_query(&cx, &handle, maintain()).unwrap();
        assert!(
            db.standing_native_delta_cursor(&cx, &handle, first, policy())
                .unwrap()
                .is_none()
        );
        assert_eq!(snapshot(&db, &cx, &handle).1, unchanged);
        db.write(&commit, write(3, 3)).await.unwrap();
        assert!(
            db.standing_native_delta_cursor(&cx, &handle, second, policy())
                .unwrap()
                .unwrap()
                .next()
                .is_none()
        );
        assert_eq!(snapshot(&db, &cx, &handle).1, unchanged);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_and_closed_delta_delivery_is_retryable_without_changing_the_view() {
    let ((), report) = run_async_under_lab(0x5c12_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, write(1, 1)).await.unwrap();
        let params = GqlParameters::new();
        let handle = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n.p AS p",
                &params,
                symbols,
                maintain(),
            )
            .unwrap();
        let direct_aggregate = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN COUNT(*) AS c",
                &params,
                symbols,
                maintain(),
            )
            .unwrap();
        let before = snapshot(&db, &cx, &handle);
        assert!(matches!(
            db.standing_native_delta_cursor(&cx, &direct_aggregate, before.0, policy()),
            Err(StandingQueryError::Unsupported)
        ));
        assert!(matches!(
            other.standing_native_delta_cursor(&cx, &handle, before.0, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
        let raw = fgdb_gql::PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p", symbols)
            .unwrap()
            .bind_parameters(&params)
            .unwrap();
        let raw = db.register_standing_rows(&cx, raw, maintain()).unwrap();
        assert!(matches!(
            db.standing_native_delta_cursor(&cx, &raw, before.0, policy()),
            Err(StandingQueryError::Unsupported)
        ));
        let mut change = write(2, 7);
        change.delete_vertex(VId(1));
        db.write(&commit, change).await.unwrap();
        let expected = snapshot(&db, &cx, &handle);
        let mut short = db
            .standing_native_delta_cursor(
                &cx,
                &handle,
                before.0,
                GqlQueryPolicy::new(0, 1, 1000, 1000),
            )
            .unwrap()
            .unwrap();
        assert!(short.next().unwrap().is_ok());
        assert!(matches!(
            short.next(),
            Some(Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            )))
        ));
        assert_eq!(short.row_stats().result_rows, 1);
        assert!(short.next().is_none());
        drop(short);
        let mut closed = db
            .standing_native_delta_cursor(&cx, &handle, before.0, policy())
            .unwrap()
            .unwrap();
        let stats = closed.evaluator_stats();
        closed.close();
        closed.close();
        assert!(closed.next().is_none());
        assert_eq!(closed.evaluator_stats(), stats);
        assert_eq!(closed.state(), VertexScanState::Closed);
        drop(closed);
        let mut full = db
            .standing_native_delta_cursor(&cx, &handle, before.0, policy())
            .unwrap()
            .unwrap();
        let changes = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let stats = full.evaluator_stats();
        drop(full);
        assert_eq!(integrate(before.1, changes), expected.1);
        let exact = GqlQueryPolicy::new(0, 2, stats.work_units, stats.scratch_entries);
        assert_eq!(
            db.standing_native_delta_cursor(&cx, &handle, before.0, exact)
                .unwrap()
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(snapshot(&db, &cx, &handle), expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
