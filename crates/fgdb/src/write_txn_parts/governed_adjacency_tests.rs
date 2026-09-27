use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::{GlaLimitDimension, GqlBudgetDimension, GqlQueryError, GqlQueryPolicy};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=4 {
        batch.create_vertex(VId(id), vec![], vec![]);
    }
    for (id, src, dst) in [(10, 1, 2), (11, 1, 2), (12, 3, 1), (13, 1, 1)] {
        batch.add_edge(EId(id), VId(src), VId(dst), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    let mut other = WriteBatch::new(S);
    other.add_edge(EId(20), VId(1), VId(4), vec![]);
    other.add_edge(EId(21), VId(4), VId(1), vec![]);
    db.write(cx, other).await.unwrap();
}
fn changes() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.delete_edge(EId(10));
    batch.add_edge(EId(40), VId(1), VId(3), vec![]);
    batch.add_edge(EId(41), VId(4), VId(1), vec![]);
    batch.delete_vertex(VId(2));
    batch.set_edge_property(EId(12), P, Some(CanonicalScalar::Int(42)));
    batch
}

#[test]
fn governed_topology_matches_native_commit_and_reopen_with_exact_limits() {
    let ((), report) = run_async_under_lab(0xad60_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let qcx = contexts.query();
        let tcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &cx).await;
        let mut txn = db.begin(&tcx).unwrap();
        txn.write(&mut db, changes()).unwrap();
        txn.savepoint(&db, "unchanged").unwrap();
        let template = txn.prepared.as_ref().unwrap().template.clone();
        let basis = txn.basis();
        for incoming in [false, true] {
            let expected = if incoming {
                vec![VId(1), VId(3), VId(4)]
            } else {
                vec![VId(1), VId(3)]
            };
            let complete = if incoming {
                txn.in_neighbours_governed(&db, &qcx, VId(1), R, policy())
            } else {
                txn.neighbours_governed(&db, &qcx, VId(1), R, policy())
            }
            .unwrap();
            assert_eq!(complete.value, expected);
            assert_eq!(complete.rows.snapshot_records, if incoming { 2 } else { 3 });
            assert_eq!(complete.rows.result_rows, expected.len() as u64);
            assert!(complete.evaluator.work_units > complete.rows.snapshot_records);
            assert!(complete.evaluator.scratch_entries > complete.rows.result_rows);
            let exact = GqlQueryPolicy::new(
                complete.rows.snapshot_records,
                complete.rows.result_rows,
                complete.evaluator.work_units,
                complete.evaluator.scratch_entries,
            );
            // Warm witnesses do not discount admission or create another budget.
            assert_eq!(
                txn.adjacency_governed_with_checkpoint(&db, VId(1), R, incoming, exact, || Ok::<
                    (),
                    (),
                >(
                    ()
                ),)
                    .unwrap(),
                complete
            );
            for dimension in 0..4 {
                let limited = GqlQueryPolicy::new(
                    complete.rows.snapshot_records - u64::from(dimension == 0),
                    complete.rows.result_rows - u64::from(dimension == 1),
                    complete.evaluator.work_units - u64::from(dimension == 2),
                    complete.evaluator.scratch_entries - u64::from(dimension == 3),
                );
                let error = txn
                    .adjacency_governed_with_checkpoint(&db, VId(1), R, incoming, limited, || {
                        Ok::<(), ()>(())
                    })
                    .unwrap_err();
                match (dimension, error) {
                    (0, GqlQueryError::Rows(error)) => {
                        assert_eq!(error.dimension, GqlBudgetDimension::SnapshotRecords);
                        assert_eq!(error.limit + 1, complete.rows.snapshot_records);
                    }
                    (1, GqlQueryError::Rows(error)) => {
                        assert_eq!(error.dimension, GqlBudgetDimension::ResultRows);
                        assert_eq!(error.limit + 1, complete.rows.result_rows);
                    }
                    (2, GqlQueryError::Evaluator(error)) => {
                        assert_eq!(error.dimension, GlaLimitDimension::WorkUnits);
                        assert_eq!(error.limit + 1, complete.evaluator.work_units);
                    }
                    (3, GqlQueryError::Evaluator(error)) => {
                        assert_eq!(error.dimension, GlaLimitDimension::ScratchEntries);
                        assert_eq!(error.limit + 1, complete.evaluator.scratch_entries);
                    }
                    (dimension, error) => panic!("wrong refusal {dimension}: {error:?}"),
                }
                assert_eq!(txn.prepared.as_ref().unwrap().template, template);
                assert_eq!(txn.savepoints.len(), 1);
                assert_eq!(txn.state(), EmbeddedTxnState::Active);
                assert_eq!(txn.basis(), basis);
                assert_eq!(tcx.outstanding_obligations(), 1);
                assert_eq!(db.frontier().unwrap(), basis);
            }
        }
        let seq = txn.commit(&mut db, &cx).await.unwrap();
        assert_eq!(seq, CommitSeq(basis.0 + 1));
        assert_eq!(db.neighbours(VId(1), R).unwrap(), vec![VId(1), VId(3)]);
        assert_eq!(
            db.in_neighbours(VId(1), R).unwrap(),
            vec![VId(1), VId(3), VId(4)]
        );
        assert!(txn.point_reads.borrow().is_empty());
        drop(db);
        let db = Database::open_with_vfs(&cx, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(db.neighbours(VId(1), R).unwrap(), vec![VId(1), VId(3)]);
        assert_eq!(
            db.in_neighbours(VId(1), R).unwrap(),
            vec![VId(1), VId(3), VId(4)]
        );
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_read_checkpoint_refuses_without_losing_workspace_or_prior_witnesses() {
    let ((), report) = run_async_under_lab(0xad60_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        for incoming in [false, true] {
            let mut total = 0;
            for stop in std::iter::once(None).chain((1..).map(Some)) {
                if stop.is_some_and(|stop| stop > total) {
                    break;
                }
                let mut txn = db.begin(&tcx).unwrap();
                txn.write(&mut db, changes()).unwrap();
                txn.savepoint(&db, "before-read").unwrap();
                txn.vertex_property(&db, VId(4), P).unwrap();
                let saved = txn.prepared.as_ref().unwrap().template.clone();
                let basis = txn.basis();
                let mut calls = 0;
                let result = txn.adjacency_governed_with_checkpoint(
                    &db,
                    VId(1),
                    R,
                    incoming,
                    policy(),
                    || {
                        calls += 1;
                        if stop == Some(calls) {
                            Err(calls)
                        } else {
                            Ok(())
                        }
                    },
                );
                if let Some(stop) = stop {
                    assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
                    assert_eq!(calls, stop);
                    assert_eq!(
                        txn.point_reads.borrow().1,
                        (stop > 1).then_some(ElementId::Vertex(VId(1)))
                    );
                    let retry = txn
                        .adjacency_governed_with_checkpoint(
                            &db,
                            VId(1),
                            R,
                            incoming,
                            policy(),
                            || Ok::<(), ()>(()),
                        )
                        .unwrap();
                    assert!(!retry.value.is_empty());
                    txn.rollback_to_savepoint(&db, "before-read").unwrap();
                    assert_eq!(
                        txn.point_reads.borrow().1,
                        (stop > 1).then_some(ElementId::Vertex(VId(1)))
                    );
                } else {
                    assert!(result.is_ok());
                    total = calls;
                    assert!(total > 20);
                    assert!(txn.point_reads.borrow().1.is_none());
                }
                assert!(
                    txn.point_reads
                        .borrow()
                        .contains(ElementId::Vertex(VId(4)), PointReadField::Property(P))
                );
                assert_eq!(txn.prepared.as_ref().unwrap().template, saved);
                assert_eq!(txn.state(), EmbeddedTxnState::Active);
                assert_eq!(txn.basis(), basis);
                assert_eq!(db.frontier().unwrap(), basis);
                assert_eq!(tcx.outstanding_obligations(), 1);
                txn.abort();
                assert_eq!(tcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refusal_before_precise_witness_allocation_still_invalidates_after_a_write() {
    let ((), report) =
        run_async_under_lab(0xad60_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let tcx = contexts.txn();
            for limit in [
                GqlQueryPolicy::new(0, 100, 10_000, 10_000),
                GqlQueryPolicy::new(100, 100, 10_000, 0),
                GqlQueryPolicy::new(100, 0, 10_000, 10_000),
            ] {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx).await;
                let mut txn = db.begin(&tcx).unwrap();
                txn.savepoint(&db, "before").unwrap();
                assert!(
                    txn.adjacency_governed_with_checkpoint(&db, VId(1), R, false, limit, || Ok::<
                        (),
                        (),
                    >(
                        ()
                    ),)
                        .is_err()
                );
                assert!(txn.point_reads.borrow().0.is_empty());
                assert_eq!(txn.point_reads.borrow().1, Some(ElementId::Vertex(VId(1))));
                txn.rollback_to_savepoint(&db, "before").unwrap();
                txn.release_savepoint(&db, "before").unwrap();
                // A successful retry cannot erase what the earlier failure exposed.
                txn.neighbours(&db, VId(1), R).unwrap();
                let mut other = WriteBatch::new(R);
                other.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(8)));
                db.write(&cx, other).await.unwrap();
                let basis = txn.basis();
                assert!(matches!(
                    txn.refresh_snapshot(&db, &tcx),
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert_eq!(txn.basis(), basis);
                assert!(matches!(
                    txn.finish(&mut db, &cx).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert!(txn.point_reads.borrow().is_empty());
                assert_eq!(tcx.outstanding_obligations(), 0);
            }
        });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unwinding_during_source_or_witness_admission_keeps_a_coarse_dependency() {
    let ((), report) = run_async_under_lab(0xad60_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let reference = db.begin(&tcx).unwrap();
        let mut total = 0;
        reference
            .adjacency_governed_with_checkpoint(&db, VId(1), R, false, policy(), || {
                total += 1;
                Ok::<(), ()>(())
            })
            .unwrap();
        reference.abort();
        // total-1 is INSIDE the last point_reads mutable borrow; an unwind must
        // release that borrow before the outer refusal guard coarsens the set.
        for stop in [2, total - 1, total] {
            let txn = db.begin(&tcx).unwrap();
            let mut calls = 0;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                txn.adjacency_governed_with_checkpoint(&db, VId(1), R, false, policy(), || {
                    calls += 1;
                    assert_ne!(calls, stop, "injected governed-read unwind");
                    Ok::<(), ()>(())
                })
            }));
            assert!(result.is_err());
            assert_eq!(calls, stop);
            assert_eq!(txn.state(), EmbeddedTxnState::Active);
            assert_eq!(txn.point_reads.borrow().1, Some(ElementId::Vertex(VId(1))));
            assert_eq!(tcx.outstanding_obligations(), 1);
            txn.abort();
        }
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owner_health_and_initial_cancellation_precede_all_observations() {
    let ((), report) = run_async_under_lab(0xad60_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let foreign = Database::open_memory(&cx, keys()).await.unwrap();
        let mut txn = db.begin(&tcx).unwrap();
        assert!(matches!(
            txn.adjacency_governed_with_checkpoint(
                &foreign,
                VId(1),
                R,
                false,
                policy(),
                || -> Result<(), ()> { panic!("foreign owner reached control") },
            ),
            Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))
        ));
        assert!(txn.point_reads.borrow().is_empty());
        assert!(matches!(
            txn.adjacency_governed_with_checkpoint(&db, VId(1), R, false, policy(), || Err(
                "cancelled before traversal"
            ),),
            Err(GqlQueryError::Interrupted("cancelled before traversal"))
        ));
        assert!(txn.point_reads.borrow().is_empty());
        txn.finish(&mut db, &cx).await.unwrap();
        assert!(matches!(
            txn.adjacency_governed_with_checkpoint(
                &db,
                VId(1),
                R,
                false,
                policy(),
                || -> Result<(), ()> { panic!("finished owner reached control") },
            ),
            Err(GqlQueryError::Source(WriteTxnError::Finished))
        ));
        let txn = db.begin(&tcx).unwrap();
        let mut write = WriteBatch::new(R);
        write.create_vertex(VId(9), vec![], vec![]);
        let prepared = db.prepare_write(write).unwrap();
        assert!(
            db.commit_template(
                &cx,
                prepared.template,
                Some(fgdb_chronicle::commit::CrashPoint::AfterMarkerBeforeD2),
                None,
                None,
            )
            .await
            .is_err()
        );
        assert!(matches!(
            txn.adjacency_governed_with_checkpoint(
                &db,
                VId(1),
                R,
                false,
                policy(),
                || -> Result<(), ()> { panic!("unhealthy source reached control") },
            ),
            Err(GqlQueryError::Source(WriteTxnError::Read(
                ReadError::CommitOutcomeUnknown { .. }
            )))
        ));
        assert!(txn.point_reads.borrow().is_empty());
        txn.abort();
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn matching_source_records_and_parallel_edges_are_not_discounted_by_output() {
    let ((), report) = run_async_under_lab(0xad60_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let qcx = contexts.query();
        let tcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(0), vec![], vec![]);
        seed.create_vertex(VId(u128::MAX), vec![], vec![]);
        for eid in 0..64 {
            seed.add_edge(EId(eid), VId(0), VId(u128::MAX), vec![]);
        }
        seed.add_edge(EId(u128::MAX), VId(0), VId(0), vec![]);
        db.write(&cx, seed).await.unwrap();
        let txn = db.begin(&tcx).unwrap();
        let result = txn
            .neighbours_governed(&db, &qcx, VId(0), R, policy())
            .unwrap();
        assert_eq!(result.value, vec![VId(0), VId(u128::MAX)]);
        assert_eq!(result.rows.snapshot_records, 65);
        assert_eq!(result.rows.result_rows, 2);
        assert!(result.evaluator.scratch_entries >= 65 * 4);
        // Another type consumes seek work but no matching snapshot-record quota.
        let empty = txn
            .neighbours_governed(
                &db,
                &qcx,
                VId(0),
                S,
                GqlQueryPolicy::new(0, 0, 10_000, 10_000),
            )
            .unwrap();
        assert!(empty.value.is_empty());
        assert_eq!(empty.rows.snapshot_records, 0);
        assert!(empty.evaluator.work_units > 65);
        // A negative domain is retained even when no source or result row exists.
        assert!(txn.point_reads.borrow().contains(
            ElementId::Vertex(VId(0)),
            PointReadField::Adjacency {
                relation: S,
                incoming: false
            }
        ));
        txn.abort();
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
