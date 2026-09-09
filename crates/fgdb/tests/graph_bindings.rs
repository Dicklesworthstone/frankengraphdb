//! Correlated tuples through the real source, evaluator and transaction paths.
//! The reference joins ordinary owned storage rows, never per-column queries.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphBindingRow, GraphPatternBuilder, IntegerComparison, PreparedGraphPattern,
    VertexPredicate,
};
use fgdb_gql::{GlaLimitDimension, GqlBudgetDimension, GqlQueryError, GqlQueryPolicy};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::BTreeSet;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const N: PropertyKeyId = PropertyKeyId(1);
const HIGH: VId = VId((1_u128 << 100) + 7);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut entities = WriteBatch::new(RelationId(9));
    for vid in [
        VId(1),
        VId(2),
        VId(3),
        VId(10),
        VId(11),
        VId(20),
        VId(21),
        VId(22),
        HIGH,
    ] {
        entities.create_vertex(
            vid,
            vec![LabelId(1)],
            vec![(N, CanonicalScalar::Int(if vid == VId(10) { 3 } else { 7 }))],
        );
    }
    let mut r = WriteBatch::new(R);
    for (eid, src, dst) in [
        (1, VId(1), VId(10)),
        (2, VId(1), VId(11)),
        (3, VId(2), VId(10)),
        (4, VId(2), VId(10)),
        (5, HIGH, VId(11)),
    ] {
        r.add_edge(EId(eid), src, dst, vec![]);
    }
    let mut s = WriteBatch::new(S);
    for (eid, src, dst) in [
        (10, VId(10), VId(20)),
        (11, VId(11), VId(21)),
        (12, VId(11), VId(22)),
    ] {
        s.add_edge(EId(eid), src, dst, vec![]);
    }
    db.write_atomic(cx, vec![entities, s, r]).await.unwrap();
}
async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    seed(&mut db, cx).await;
    db
}
fn builder(minimum: i64) -> GraphPatternBuilder {
    let mut b = GraphPatternBuilder::new();
    for name in ["person", "friend", "company"] {
        b.vertex(name).unwrap();
    }
    b.edge("person", R, GlaDirection::Forward, "friend")
        .unwrap();
    b.edge("company", S, GlaDirection::Reverse, "friend")
        .unwrap();
    b.filter(
        "friend",
        VertexPredicate::IntegerProperty {
            key: N,
            comparison: IntegerComparison::GreaterOrEqual,
            value: minimum,
        },
    )
    .unwrap();
    b
}
fn pattern(
    columns: &[&str],
    offset: u64,
    count: Option<u64>,
) -> PreparedGraphPattern<GraphBindingRow> {
    builder(3).prepare_bindings(columns, offset, count).unwrap()
}
fn values(rows: &[GraphBindingRow]) -> Vec<Vec<VId>> {
    rows.iter().map(|r| r.values().to_vec()).collect()
}

fn oracle(
    vertices: &[VertexRow],
    edges: &[EdgeRecord],
    columns: &[usize],
    offset: usize,
    count: usize,
) -> Vec<Vec<VId>> {
    let mut rows = BTreeSet::new();
    for left in edges.iter().filter(|e| e.entry.relation == R) {
        let friend = left.entry.dst;
        let qualifying = vertices.iter().find(|v| v.vid == friend).is_some_and(|v| {
            v.props.iter().any(|(key, scalar)| {
                *key == N && matches!(scalar, CanonicalScalar::Int(n) if *n >= 3)
            })
        });
        if !qualifying {
            continue;
        }
        for right in edges
            .iter()
            .filter(|e| e.entry.relation == S && e.entry.src == friend)
        {
            let assignment = [left.entry.src, friend, right.entry.dst];
            rows.insert(
                columns
                    .iter()
                    .map(|column| assignment[*column])
                    .collect::<Vec<_>>(),
            );
        }
    }
    rows.into_iter().skip(offset).take(count).collect()
}

#[test]
fn tuples_preserve_correlations_column_order_and_full_row_distinctness_on_all_surfaces() {
    let ((), report) = run_async_under_lab(0xb1ad_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let at = db.frontier().unwrap();
        let view = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        for (names, columns) in [
            (vec!["person", "friend", "company"], vec![0, 1, 2]),
            (vec!["company", "person"], vec![2, 0]),
            (vec!["person"], vec![0]),
        ] {
            for (offset, count) in [(0, None), (1, Some(3)), (99, Some(1)), (0, Some(0))] {
                let query = pattern(&names, offset, count);
                let expected = oracle(
                    &vertices,
                    &edges,
                    &columns,
                    offset as usize,
                    count.unwrap_or(u64::MAX) as usize,
                );
                let live = db
                    .execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap();
                assert_eq!(values(&live.value), expected);
                assert_eq!(live.rows.snapshot_records, 8);
                assert_eq!(live.rows.result_rows, expected.len() as u64);
                assert_eq!(
                    db.execute_graph_pattern_governed_at(&cx, &query, at, policy())
                        .unwrap(),
                    live
                );
                assert_eq!(
                    view.execute_graph_pattern_governed(&cx, &query, policy())
                        .unwrap(),
                    live
                );
                assert_eq!(
                    view.execute_graph_pattern_governed_at(&cx, &query, at, policy())
                        .unwrap(),
                    live
                );
                let overlay = txn
                    .execute_graph_pattern_governed(&db, &cx, &query, policy())
                    .unwrap();
                assert_eq!(values(&overlay.value), expected);
                assert_eq!(overlay.rows, live.rows);
                assert!(live.value.iter().all(|r| r.len() == names.len()));
            }
        }
        let query = pattern(&["person", "company"], 0, None);
        let rows = values(
            &db.execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap()
                .value,
        );
        assert_eq!(
            rows,
            vec![
                vec![VId(1), VId(20)],
                vec![VId(1), VId(21)],
                vec![VId(1), VId(22)],
                vec![VId(2), VId(20)],
                vec![HIGH, VId(21)],
                vec![HIGH, VId(22)]
            ]
        );
        assert!(
            !rows.contains(&vec![VId(2), VId(21)]),
            "independent column sets would invent this relationship"
        );
        let single = builder(3).prepare("person", 0, None).unwrap();
        let singleton_rows = pattern(&["person"], 0, None);
        let scalar = db
            .execute_graph_pattern_governed(&cx, &single, policy())
            .unwrap();
        let tuples = db
            .execute_graph_pattern_governed(&cx, &singleton_rows, policy())
            .unwrap();
        assert_eq!(
            tuples
                .value
                .iter()
                .map(|r| r.get(0).unwrap())
                .collect::<Vec<_>>(),
            scalar.value
        );
        assert!(!format!("{tuples:?} {:?}", tuples.value).contains("VId"));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn tuple_limits_count_rows_and_cells_without_a_second_source_allowance() {
    let ((), report) = run_async_under_lab(0xb1ad_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let db = seeded(&commit).await;
        let query = pattern(&["person", "friend", "company"], 0, None);
        let full = db
            .execute_graph_pattern_governed(&cx, &query, policy())
            .unwrap();
        assert_eq!(full.rows.result_rows, 6);
        let exact = GqlQueryPolicy::new(
            8,
            6,
            full.evaluator.work_units,
            full.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_pattern_governed(&cx, &query, exact)
                .unwrap(),
            full
        );
        for (cap, dimension, observed) in [
            (
                GqlQueryPolicy::new(7, 6, 1_000_000, 1_000_000),
                GqlBudgetDimension::SnapshotRecords,
                8,
            ),
            (
                GqlQueryPolicy::new(8, 5, 1_000_000, 1_000_000),
                GqlBudgetDimension::ResultRows,
                6,
            ),
        ] {
            assert!(
                matches!(db.execute_graph_pattern_governed(&cx, &query, cap),
                Err(GqlQueryError::Rows(error)) if error.dimension == dimension && error.observed == observed)
            );
        }
        for (work, scratch, dimension) in [
            (
                full.evaluator.work_units - 1,
                full.evaluator.scratch_entries,
                GlaLimitDimension::WorkUnits,
            ),
            (
                full.evaluator.work_units,
                full.evaluator.scratch_entries - 1,
                GlaLimitDimension::ScratchEntries,
            ),
        ] {
            let cap = GqlQueryPolicy::new(8, 6, work, scratch);
            assert!(
                matches!(db.execute_graph_pattern_governed(&cx, &query, cap), Err(GqlQueryError::Evaluator(error))
                if error.dimension == dimension && error.observed == u128::from(error.limit) + 1)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_binding_rows_match_owned_storage_and_survive_history_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xb1ad_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let query = pattern(&["person", "friend", "company"], 0, None);
        let old = db
            .execute_graph_pattern_governed(&cx, &query, policy())
            .unwrap()
            .value;
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(2));
        change.ensure_edge_by_triple(EId(999), VId(1), VId(10), vec![]);
        change.add_edge(EId(50), VId(3), VId(10), vec![]);
        change.set_vertex_property(VId(11), N, Some(CanonicalScalar::Int(0)));
        txn.write(&mut db, change).unwrap();
        let expected = oracle(
            &txn.vertices(&db).unwrap(),
            &txn.edges(&db).unwrap(),
            &[0, 1, 2],
            0,
            usize::MAX,
        );
        assert_eq!(
            expected,
            vec![
                vec![VId(1), VId(10), VId(20)],
                vec![VId(2), VId(10), VId(20)],
                vec![VId(3), VId(10), VId(20)]
            ]
        );
        let staged = txn
            .execute_graph_pattern_governed(&db, &cx, &query, policy())
            .unwrap();
        assert_eq!(values(&staged.value), expected);
        assert_eq!(staged.rows.snapshot_records, 8);
        assert_eq!(
            db.execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap()
                .value,
            old
        );
        let published = txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            values(
                &db.execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            db.execute_graph_pattern_governed_at(&cx, &query, before, policy())
                .unwrap()
                .value,
            old
        );
        assert_eq!(
            pinned
                .execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap()
                .value,
            old
        );
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.frontier().unwrap(), published);
        assert_eq!(
            values(
                &reopened
                    .execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            reopened
                .execute_graph_pattern_governed_at(&cx, &query, before, policy())
                .unwrap()
                .value,
            old
        );
        assert!(reopened.edge(EId(999)).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn tuple_refusal_retains_unprojected_dependencies_and_phantoms_without_global_vertex_fencing() {
    let ((), report) = run_async_under_lab(0xb1ad_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        for winner_kind in 0..3 {
            let mut db = seeded(&commit).await;
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut stage = WriteBatch::new(R);
            stage.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, stage).unwrap();
            let query = pattern(&["person", "company"], 0, None);
            assert!(matches!(
                txn.execute_graph_pattern_governed(
                    &db,
                    &cx,
                    &query,
                    GqlQueryPolicy::new(100, 0, 1_000_000, 1_000_000)
                ),
                Err(GqlQueryError::Rows(_))
            ));
            let mut winner = WriteBatch::new(R);
            match winner_kind {
                0 => {
                    winner.set_vertex_property(VId(11), N, Some(CanonicalScalar::Int(0)));
                }
                1 => {
                    winner.add_edge(EId(77), VId(3), VId(11), vec![]);
                }
                _ => {
                    winner.create_vertex(VId(77), vec![], vec![]);
                }
            }
            db.write(&commit, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            let result = txn.commit(&mut db, &commit).await;
            if winner_kind == 2 {
                result.unwrap();
                assert!(db.vertex(VId(99)).unwrap().is_some());
            } else {
                assert!(matches!(
                    result,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(99)).unwrap().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn tuples_keep_source_refusal_precedence_and_labeled_node_scans() {
    let ((), report) = run_async_under_lab(0xb1ad_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let foreign = seeded(&commit).await;
        let query = pattern(&["person", "company"], 0, None);
        let view = db.read_session().unwrap();
        let future = CommitSeq(db.frontier().unwrap().0 + 1);
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        assert!(matches!(
            db.execute_graph_pattern_governed_at(&cx, &query, future, zero),
            Err(GqlQueryError::Source(GqlError::Read(
                ReadError::BeyondFrontier { .. }
            )))
        ));
        assert!(matches!(
            view.execute_graph_pattern_governed_at(&cx, &query, future, zero),
            Err(GqlQueryError::Source(GqlError::Read(
                ReadError::BeyondFrontier { .. }
            )))
        ));
        let txn = db.begin(&txn_cx).unwrap();
        assert!(matches!(
            txn.execute_graph_pattern_governed(&foreign, &cx, &query, zero),
            Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))
        ));
        let mut b = GraphPatternBuilder::new();
        b.vertex("node").unwrap();
        b.filter("node", VertexPredicate::HasLabel(LabelId(1)))
            .unwrap();
        let node = b.prepare_bindings(&["node"], 0, None).unwrap();
        let expected: Vec<_> = db
            .vertices()
            .unwrap()
            .into_iter()
            .map(|v| vec![v.vid])
            .collect();
        assert_eq!(
            values(
                &db.execute_graph_pattern_governed(&cx, &node, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            values(
                &txn.execute_graph_pattern_governed(&db, &cx, &node, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_tuple_evaluator_checkpoint_and_late_predicate_error_returns_no_result() {
    let query = pattern(&["person", "friend", "company"], 0, None);
    let edges = [
        (VId(1), R, VId(10)),
        (VId(2), R, VId(10)),
        (VId(10), S, VId(20)),
        (VId(10), S, VId(21)),
    ];
    let mut total = 0;
    let completed = query
        .plan()
        .execute_governed(
            4,
            [],
            edges,
            |_, _| Ok::<_, &str>(true),
            policy(),
            || {
                total += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    assert_eq!(completed.value.len(), 4);
    for stop in 1..=total {
        let mut calls = 0;
        let result = query.plan().execute_governed(
            4,
            [],
            edges,
            |_, _| Ok::<_, &str>(true),
            policy(),
            || {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(calls, stop);
    }
    let mut b = builder(3);
    b.filter("company", VertexPredicate::HasLabel(LabelId(1)))
        .unwrap();
    let query = b.prepare_bindings(&["person", "company"], 0, None).unwrap();
    let result = query.plan().execute_governed(
        4,
        [],
        edges,
        |vid, _| {
            if vid == VId(21) {
                Err("late predicate read")
            } else {
                Ok(true)
            }
        },
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source("late predicate read"))
    ));
}
