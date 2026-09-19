//! Use the actual database APIs; the oracle enumerates complete owned-edge
//! walks and applies route-wide mode predicates before selecting any K-prefix.

use super::*;
use fgdb_gql::GraphCheapestPathMode;
use std::collections::BTreeSet;

const MODES: [GraphCheapestPathMode; 3] = [
    GraphCheapestPathMode::Trail,
    GraphCheapestPathMode::Acyclic,
    GraphCheapestPathMode::Simple,
];
fn expected(edges: &[EdgeRecord], q: &PreparedGraphCheapestPath, count: usize) -> Vec<Answer> {
    oracle(edges, q, usize::MAX)
        .into_iter()
        .filter(|(_, steps)| {
            if q.mode() == GraphCheapestPathMode::Trail {
                return steps
                    .iter()
                    .map(|step| step.0)
                    .collect::<BTreeSet<_>>()
                    .len()
                    == steps.len();
            }
            let mut vertices: Vec<_> = std::iter::once(q.source())
                .chain(steps.iter().map(|step| step.1))
                .collect();
            if q.mode() == GraphCheapestPathMode::Simple
                && vertices.len() > 1
                && vertices.last() == Some(&q.source())
            {
                vertices.pop();
            }
            vertices.iter().collect::<BTreeSet<_>>().len() == vertices.len()
        })
        .take(count)
        .collect()
}

#[test]
fn constrained_modes_share_pinned_history_canonical_overlays_and_recovered_sources() {
    let ((), report) = run_async_under_lab(0xc0a5_1011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let before = db.edges().unwrap();
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        for mode in MODES {
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                let (start, end) = if direction == GlaDirection::Reverse {
                    (3, 0)
                } else {
                    (0, 3)
                };
                let q = query(start, end, direction, 1, 4).with_mode(mode);
                let want = expected(&before, &q, 7);
                assert!(!want.is_empty());
                for rows in [
                    db.execute_graph_cheapest_paths_governed(&cx, &q, 7, policy())
                        .unwrap(),
                    db.execute_graph_cheapest_paths_governed_at(&cx, &q, 7, basis, policy())
                        .unwrap(),
                    pinned
                        .execute_graph_cheapest_paths_governed(&cx, &q, 7, policy())
                        .unwrap(),
                    pinned
                        .execute_graph_cheapest_paths_governed_at(&cx, &q, 7, basis, policy())
                        .unwrap(),
                    txn.execute_graph_cheapest_paths_governed(&db, &cx, &q, 7, policy())
                        .unwrap(),
                ] {
                    assert_eq!(plain(&rows.value), want, "{mode:?} {direction:?}");
                }
                for rows in [
                    db.execute_graph_cheapest_path_governed(&cx, &q, policy())
                        .unwrap(),
                    pinned
                        .execute_graph_cheapest_path_governed(&cx, &q, policy())
                        .unwrap(),
                    txn.execute_graph_cheapest_path_governed(&db, &cx, &q, policy())
                        .unwrap(),
                ] {
                    assert_eq!(plain(&rows.value), want[..1]);
                }
            }
        }
        let mut stage = WriteBatch::new(R);
        stage.set_edge_property(EId(9), W, Some(CanonicalScalar::Int(-12)));
        stage.delete_edge(EId(4));
        stage.ensure_edge_by_triple(EId(888), VId(0), VId(1), vec![]);
        stage.add_edge(EId(8), VId(0), VId(0), vec![(W, CanonicalScalar::Int(-2))]);
        txn.write(&mut db, stage).unwrap();
        let staged = txn.edges(&db).unwrap();
        assert!(staged.iter().all(|edge| edge.entry.eid != EId(888)));
        for mode in MODES {
            let q = query(0, 3, GlaDirection::Forward, 0, 4).with_mode(mode);
            assert_eq!(
                plain(
                    &txn.execute_graph_cheapest_paths_governed(&db, &cx, &q, 7, policy())
                        .unwrap()
                        .value
                ),
                expected(&staged, &q, 7)
            );
            assert_eq!(
                plain(
                    &db.execute_graph_cheapest_paths_governed(&cx, &q, 7, policy())
                        .unwrap()
                        .value
                ),
                expected(&before, &q, 7)
            );
        }
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for mode in MODES {
            let q = query(0, 3, GlaDirection::Forward, 0, 4).with_mode(mode);
            assert_eq!(
                plain(
                    &db.execute_graph_cheapest_paths_governed(&cx, &q, 7, policy())
                        .unwrap()
                        .value
                ),
                expected(&staged, &q, 7)
            );
            assert_eq!(
                plain(
                    &db.execute_graph_cheapest_paths_governed_at(&cx, &q, 7, basis, policy())
                        .unwrap()
                        .value
                ),
                expected(&before, &q, 7)
            );
            assert_eq!(
                plain(
                    &pinned
                        .execute_graph_cheapest_paths_governed(&cx, &q, 7, policy())
                        .unwrap()
                        .value
                ),
                expected(&before, &q, 7)
            );
        }
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(1));
        db.write(&commit, cascade).await.unwrap();
        let cascaded = db.edges().unwrap();
        for mode in MODES {
            let q = query(0, 3, GlaDirection::Forward, 0, 4).with_mode(mode);
            assert_eq!(
                plain(
                    &db.execute_graph_cheapest_paths_governed(&cx, &q, 7, policy())
                        .unwrap()
                        .value
                ),
                expected(&cascaded, &q, 7)
            );
            assert_eq!(
                plain(
                    &pinned
                        .execute_graph_cheapest_paths_governed(&cx, &q, 7, policy())
                        .unwrap()
                        .value
                ),
                expected(&before, &q, 7)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mode_pruning_never_prunes_transaction_read_or_insertion_witnesses() {
    let ((), report) = run_async_under_lab(0xc0a5_1012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        for mode in MODES {
            for outcome in 0..5 {
                for change in 0..3 {
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    seed(&mut db, &commit).await;
                    if outcome == 4 {
                        let mut invalid = WriteBatch::new(R);
                        invalid.set_edge_property(EId(9), W, None);
                        db.write(&commit, invalid).await.unwrap();
                    }
                    let mut txn = db.begin(&txcx).unwrap();
                    let mut stage = WriteBatch::new(R);
                    stage.create_vertex(VId(777), vec![], vec![]);
                    txn.write(&mut db, stage).unwrap();
                    let q = query(
                        0,
                        if outcome == 2 { 99 } else { 3 },
                        GlaDirection::Forward,
                        1,
                        4,
                    )
                    .with_mode(mode);
                    let result = txn.execute_graph_cheapest_paths_governed(
                        &db,
                        &cx,
                        &q,
                        if outcome == 1 { 0 } else { 3 },
                        GqlQueryPolicy::new(
                            1000,
                            if outcome == 3 { 0 } else { 1000 },
                            5_000_000,
                            1_000_000,
                        ),
                    );
                    match outcome {
                        0 => assert!(!result.unwrap().value.is_empty()),
                        1 | 2 => assert!(result.unwrap().value.is_empty()),
                        3 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                        _ => assert!(matches!(
                            result,
                            Err(GqlQueryError::Source(GraphCheapestPathError::Cost(
                                GraphPathCostError::MissingWeight
                            )))
                        )),
                    }
                    let mut winner = WriteBatch::new(R);
                    match change {
                        0 => {
                            winner.set_edge_property(EId(9), W, Some(CanonicalScalar::Int(-20)));
                        }
                        1 => {
                            winner.add_edge(
                                EId(90),
                                VId(0),
                                VId(3),
                                vec![(W, CanonicalScalar::Int(-30))],
                            );
                        }
                        _ => {
                            winner.delete_vertex(VId(2));
                        }
                    }
                    db.write(&commit, winner).await.unwrap();
                    let frontier = db.frontier().unwrap();
                    // No second graph read may supply missing conflict witnesses.
                    assert!(matches!(
                        txn.commit(&mut db, &commit).await,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn constrained_mode_limits_and_owner_frontier_fences_share_the_original_database_boundaries() {
    let ((), report) = run_async_under_lab(0xc0a5_1013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let txn = db.begin(&txcx).unwrap();
        for mode in MODES {
            let q = query(0, 3, GlaDirection::Forward, 1, 4).with_mode(mode);
            let baseline = db
                .execute_graph_cheapest_paths_governed(&cx, &q, 3, policy())
                .unwrap();
            let exact = GqlQueryPolicy::new(
                baseline.rows.snapshot_records,
                baseline.rows.result_rows,
                baseline.evaluator.work_units,
                baseline.evaluator.scratch_entries,
            );
            assert_eq!(
                db.execute_graph_cheapest_paths_governed(&cx, &q, 3, exact)
                    .unwrap(),
                baseline
            );
            for bad in [
                GqlQueryPolicy::new(baseline.rows.snapshot_records - 1, 3, u64::MAX, u64::MAX),
                GqlQueryPolicy::new(
                    baseline.rows.snapshot_records,
                    baseline.rows.result_rows - 1,
                    u64::MAX,
                    u64::MAX,
                ),
                GqlQueryPolicy::new(
                    baseline.rows.snapshot_records,
                    3,
                    baseline.evaluator.work_units - 1,
                    u64::MAX,
                ),
                GqlQueryPolicy::new(
                    baseline.rows.snapshot_records,
                    3,
                    u64::MAX,
                    baseline.evaluator.scratch_entries - 1,
                ),
            ] {
                assert!(
                    db.execute_graph_cheapest_paths_governed(&cx, &q, 3, bad)
                        .is_err()
                );
            }
            assert_eq!(
                db.execute_graph_cheapest_paths_governed(&cx, &q, 3, exact)
                    .unwrap(),
                baseline
            );
            assert!(matches!(
                db.execute_graph_cheapest_paths_governed_at(
                    &cx,
                    &q,
                    0,
                    CommitSeq(basis.0 + 1),
                    GqlQueryPolicy::new(0, 0, 0, 0)
                ),
                Err(GqlQueryError::Source(GraphCheapestPathError::Source(
                    GqlError::Read(_)
                )))
            ));
            assert!(matches!(
                txn.execute_graph_cheapest_paths_governed(
                    &foreign,
                    &cx,
                    &q,
                    0,
                    GqlQueryPolicy::new(0, 0, 0, 0)
                ),
                Err(GqlQueryError::Source(GraphCheapestPathError::Source(_)))
            ));
        }
        let mut invalid = WriteBatch::new(R);
        invalid.set_edge_property(EId(9), W, None);
        db.write(&commit, invalid).await.unwrap();
        for mode in MODES {
            let q = query(0, 3, GlaDirection::Forward, 1, 4).with_mode(mode);
            assert!(matches!(
                db.execute_graph_cheapest_paths_governed(&cx, &q, 0, policy()),
                Err(GqlQueryError::Source(GraphCheapestPathError::Cost(
                    GraphPathCostError::MissingWeight
                )))
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
