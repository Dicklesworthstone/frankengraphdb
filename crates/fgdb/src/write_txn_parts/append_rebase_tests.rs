use super::*;
use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}

async fn seed(database: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
        batch.create_vertex(
            VId(id), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(0))],
        );
    }
    database.write(cx, batch).await.unwrap();
}

fn edge(id: u128, target: u128) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.add_edge(EId(id), VId(1), VId(target), vec![(P, CanonicalScalar::Int(7))]);
    batch
}

async fn drifted(cx: &CommitCx, txcx: &TxnCx) -> (Database<MemVfs>, WriteTxn) {
    let mut database = Database::open_memory(cx, keys()).await.unwrap();
    seed(&mut database, cx).await;
    let mut transaction = database.begin(txcx).unwrap();
    transaction.write(&mut database, edge(50, 2)).unwrap();
    database.write(cx, edge(10, 3)).await.unwrap();
    (database, transaction)
}

#[test]
fn independent_shared_endpoint_appends_commit_once_and_reopen() {
    let ((), report) = run_async_under_lab(0xa99e_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut database = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
            .await.unwrap();
        seed(&mut database, &cx).await;
        let pinned = database.read_session().unwrap();
        let mut left = database.begin(&txcx).unwrap();
        let mut right = database.begin(&txcx).unwrap();
        let mut ordinary = database.begin(&txcx).unwrap();
        left.write(&mut database, edge(10, 2)).unwrap();
        right.write(&mut database, edge(20, 3)).unwrap();
        ordinary.write(&mut database, edge(30, 3)).unwrap();
        let first = left.commit(&mut database, &cx).await.unwrap();
        assert!(matches!(
            ordinary.commit(&mut database, &cx).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
        ));
        // Refresh intentionally keeps the old conservative behavior too.
        assert!(matches!(
            right.refresh_snapshot(&database, &txcx),
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
        ));
        let second = right.commit_append_only_rebased(&mut database, &cx, 1).await.unwrap();
        assert_eq!(second, CommitSeq(first.0 + 1));
        assert_eq!(database.delta_since(first).unwrap().count(), 1);
        assert_eq!(right.state(), EmbeddedTxnState::Completed(
            EmbeddedTxnCompletion::WriteCommitted { commit_seq: second },
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
        assert!(pinned.edge(EId(10)).unwrap().is_none());
        assert!(pinned.edge(EId(20)).unwrap().is_none());
        let expected = database.edges().unwrap();
        assert_eq!(expected.len(), 2);
        assert!(database.edge(EId(30)).unwrap().is_none());
        drop(database);
        let database = Database::open_with_vfs(&cx, vfs, &path, keys()).await.unwrap();
        assert_eq!(database.frontier().unwrap(), second);
        assert_eq!(database.edges().unwrap(), expected);
        assert_eq!(database.edge(EId(20)).unwrap().unwrap().entry.created_at, second);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dependent_multi_relation_creations_preserve_births_and_exact_row_admission() {
    let ((), report) = run_async_under_lab(0xa99e_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for limit in [5, 6, 7] {
            let mut database = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut database, &cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            let mut a = WriteBatch::new(RelationId(9));
            a.create_vertex(VId(5), vec![], vec![(P, CanonicalScalar::Int(5))]);
            a.add_edge(EId(50), VId(1), VId(5), vec![]);
            let mut b = WriteBatch::new(RelationId(2));
            b.create_vertex(VId(6), vec![], vec![]);
            b.add_edge(EId(60), VId(5), VId(6), vec![]);
            transaction.write_ordered(&mut database, vec![a, b]).unwrap();
            let mut concurrent = edge(10, 3);
            concurrent.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(100)));
            database.write(&cx, concurrent).await.unwrap();
            let frontier = database.frontier().unwrap();
            let result = transaction.commit_append_only_rebased(&mut database, &cx, limit).await;
            if limit == 5 {
                assert!(matches!(result, Err(WriteTxnError::OrderedWriteBudgetExceeded {
                    limit: 5, required: 6,
                })));
                assert_eq!(database.frontier().unwrap(), frontier);
                assert!(database.vertex(VId(5)).unwrap().is_none());
                assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
            } else {
                assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                assert_eq!(database.vertex(VId(5)).unwrap().unwrap().birth_ordinal, 1);
                assert_eq!(database.vertex(VId(6)).unwrap().unwrap().birth_ordinal, 3);
                assert_eq!(database.edge(EId(50)).unwrap().unwrap().entry.relation, RelationId(9));
                assert_eq!(database.edge(EId(60)).unwrap().unwrap().entry.relation, RelationId(2));
                assert_eq!(database.delta_since(frontier).unwrap().count(), 1);
            }
            assert_eq!(database.vertex(VId(1)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(100))]);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn raw_conditionals_updates_and_deletes_never_become_blind_appends() {
    let ((), report) = run_async_under_lab(0xa99e_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..6 {
            let mut database = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut database, &cx).await;
            let frontier = database.frontier().unwrap();
            let mut transaction = database.begin(&txcx).unwrap();
            let mut batch = edge(50, 2);
            match case {
                0 => { batch.ensure_vertex(VId(1), vec![], vec![]); }
                1 => { batch.ensure_edge_by_triple(EId(51), VId(1), VId(2), vec![]); }
                2 => { batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0))); }
                3 => {
                    batch.compare_and_set_vertex_property(
                        VId(1), P, Some(CanonicalScalar::Int(999)), CanonicalScalar::Int(0),
                        WriteMismatchPolicy::NoOp,
                    );
                }
                4 => { batch.delete_edge_if_present(EId(99)); }
                _ => {
                    batch.create_vertex(VId(5), vec![], vec![]);
                    batch.delete_vertex(VId(5));
                }
            }
            transaction.write(&mut database, batch).unwrap();
            assert!(matches!(
                transaction.commit_append_only_rebased(&mut database, &cx, 100).await,
                Err(WriteTxnError::AppendRebaseIneligible)
            ));
            assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
            assert_eq!(database.frontier().unwrap(), frontier);
            assert!(database.edges().unwrap().is_empty());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn point_negative_scan_and_savepoint_observations_refuse_rebase() {
    let ((), report) = run_async_under_lab(0xa99e_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..6 {
            let (mut database, mut transaction) = drifted(&cx, &txcx).await;
            let frontier = database.frontier().unwrap();
            match case {
                0 => { transaction.vertex(&database, VId(1)).unwrap(); }
                1 => { assert!(transaction.vertex(&database, VId(99)).unwrap().is_none()); }
                2 => { transaction.vertices(&database).unwrap(); }
                3 => { transaction.edges(&database).unwrap(); }
                4 => {
                    transaction.execute_gql(
                        &database, "MATCH (n:L) RETURN n",
                        &RelationBind::new().with_label("L", LabelId(1)),
                    ).unwrap();
                }
                _ => { transaction.savepoint(&database, "keep").unwrap(); }
            }
            assert!(matches!(
                transaction.commit_append_only_rebased(&mut database, &cx, 10).await,
                Err(WriteTxnError::AppendRebaseIneligible)
            ));
            assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
            assert_eq!(database.frontier().unwrap(), frontier);
            assert!(database.edge(EId(50)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn identity_write_delete_races_and_retired_endpoints_still_conflict() {
    let ((), report) = run_async_under_lab(0xa99e_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..3 {
            let mut database = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut database, &cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            let mut staged = edge(50, 2);
            staged.create_vertex(VId(5), vec![], vec![]);
            transaction.write(&mut database, staged).unwrap();
            let mut other = WriteBatch::new(R);
            match case {
                0 => {
                    database.write(&cx, edge(50, 3)).await.unwrap();
                    other.delete_edge(EId(50));
                }
                1 => {
                    let mut creation = WriteBatch::new(R);
                    creation.create_vertex(VId(5), vec![], vec![]);
                    database.write(&cx, creation).await.unwrap();
                    other.delete_vertex(VId(5));
                }
                _ => { other.delete_vertex(VId(2)); }
            }
            database.write(&cx, other).await.unwrap();
            let frontier = database.frontier().unwrap();
            assert!(matches!(
                transaction.commit_append_only_rebased(&mut database, &cx, 10).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-01", .. }))
            ));
            assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
            assert_eq!(database.frontier().unwrap(), frontier);
            assert!(database.edge(EId(50)).unwrap().is_none());
            assert!(database.vertex(VId(5)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn wrong_owner_and_unpolled_future_preserve_the_real_owner_workspace() {
    let ((), report) = run_async_under_lab(0xa99e_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let (mut database, mut transaction) = drifted(&cx, &txcx).await;
        let old = transaction.prepared.as_ref().unwrap().template.clone();
        let basis = transaction.basis();
        drop(transaction.commit_append_only_rebased(&mut database, &cx, 1));
        let mut foreign = Database::open_memory(&cx, keys()).await.unwrap();
        assert!(matches!(
            transaction.commit_append_only_rebased(&mut foreign, &cx, 1).await,
            Err(WriteTxnError::WrongDatabase)
        ));
        assert_eq!(transaction.state(), EmbeddedTxnState::Active);
        assert_eq!(transaction.basis(), basis);
        assert_eq!(transaction.prepared.as_ref().unwrap().template, old);
        assert_eq!(txcx.outstanding_obligations(), 1);
        transaction.commit_append_only_rebased(&mut database, &cx, 1).await.unwrap();
        assert!(matches!(
            transaction.commit_append_only_rebased(&mut database, &cx, 1).await,
            Err(WriteTxnError::Finished)
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_prepublication_checkpoint_refusal_aborts_without_a_new_marker() {
    let ((), report) = run_async_under_lab(0xa99e_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let (mut database, mut transaction) = drifted(&cx, &txcx).await;
        let mut checkpoints = 0;
        transaction.complete_with_basis_controlled(
            &mut database, &cx, None, true, Some(1), || {
                checkpoints += 1;
                Ok(())
            },
        ).await.unwrap();
        assert!(checkpoints > 10);
        for stop in 1..=checkpoints {
            let (mut database, mut transaction) = drifted(&cx, &txcx).await;
            let frontier = database.frontier().unwrap();
            let mut seen = 0;
            let result = transaction.complete_with_basis_controlled(
                &mut database, &cx, None, true, Some(1), || {
                    seen += 1;
                    if seen == stop { Err(WriteTxnError::NoPreparedWrite) } else { Ok(()) }
                },
            ).await;
            assert!(matches!(result, Err(WriteTxnError::NoPreparedWrite)), "stop {stop}");
            assert_eq!(seen, stop);
            assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
            assert!(transaction.pin.is_none());
            assert!(transaction.prepared.is_none());
            assert!(transaction.staged.is_empty());
            assert_eq!(database.frontier().unwrap(), frontier);
            assert!(database.edge(EId(50)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rebased_completion_keeps_the_existing_unknown_outcome_fence() {
    use fgdb_chronicle::commit::CrashPoint;
    let ((), report) = run_async_under_lab(0xa99e_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for crash in [CrashPoint::BeforeCapsule, CrashPoint::AfterMarkerBeforeD2] {
            let (mut database, mut transaction) = drifted(&cx, &txcx).await;
            let frontier = database.frontier().unwrap();
            let result = transaction.complete_with_basis_controlled(
                &mut database, &cx, Some(crash), true, Some(1), || Ok(()),
            ).await;
            assert!(result.is_err());
            let expected = if crash == CrashPoint::BeforeCapsule {
                EmbeddedTxnState::Aborted
            } else {
                EmbeddedTxnState::CommitOutcomeUnknown { published_frontier: frontier }
            };
            assert_eq!(transaction.state(), expected);
            assert!(transaction.prepared.is_none());
            assert!(transaction.pin.is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unwind_after_re_evaluation_still_discards_the_whole_workspace() {
    use std::future::Future;
    use std::task::Poll;
    let ((), report) = run_async_under_lab(0xa99e_0009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let (mut database, mut transaction) = drifted(&cx, &txcx).await;
        let mut checkpoints = 0;
        transaction.complete_with_basis_controlled(
            &mut database, &cx, None, true, Some(1), || {
                checkpoints += 1;
                Ok(())
            },
        ).await.unwrap();
        let (mut database, mut transaction) = drifted(&cx, &txcx).await;
        let frontier = database.frontier().unwrap();
        let mut seen = 0;
        let mut future = Box::pin(transaction.complete_with_basis_controlled(
            &mut database, &cx, None, true, Some(1), || {
                seen += 1;
                assert_ne!(seen, checkpoints, "injected final prepublication unwind");
                Ok(())
            },
        ));
        let panicked = std::future::poll_fn(|task| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                future.as_mut().poll(task)
            }));
            Poll::Ready(result.is_err())
        }).await;
        drop(future);
        assert!(panicked);
        assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
        assert!(transaction.staged.is_empty());
        assert!(transaction.prepared.is_none());
        assert_eq!(database.frontier().unwrap(), frontier);
        assert!(database.edge(EId(50)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
