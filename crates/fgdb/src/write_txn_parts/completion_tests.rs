use super::*;
use asupersync::lab::run_async_under_lab;
use crate::{DatabaseKeys, DerivedPublicationStage, MemVfs};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
use std::cell::Cell;
use std::future::Future;
use std::task::Poll;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32])
}

async fn fixture(cx: &CommitCx, txcx: &TxnCx, write: bool) -> (Database<MemVfs>, WriteTxn) {
    let mut database = Database::open_memory(cx, keys()).await.unwrap();
    let mut seed = WriteBatch::new(RelationId(1));
    for id in 1..=3 {
        seed.create_vertex(VId(id), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Int(10))]);
    }
    seed.add_edge(EId(13), VId(1), VId(3), vec![]);
    seed.add_edge(EId(23), VId(2), VId(3), vec![]);
    database.write(cx, seed).await.unwrap();
    let mut transaction = database.begin(txcx).unwrap();
    transaction.vertex(&database, VId(1)).unwrap();
    if write {
        let mut staged = WriteBatch::new(RelationId(1));
        staged.create_vertex(VId(99), vec![], vec![]);
        transaction.write(&mut database, staged).unwrap();
    }
    let mut unrelated = WriteBatch::new(RelationId(1));
    unrelated.create_vertex(VId(4), (1..=5).map(LabelId).collect(), vec![]);
    database.write(cx, unrelated).await.unwrap();
    let mut cascade = WriteBatch::new(RelationId(1));
    cascade.delete_vertex(VId(3));
    database.write(cx, cascade).await.unwrap();
    (database, transaction)
}

#[test]
fn every_validation_checkpoint_refuses_before_acceptance_and_releases_pin() {
    let ((), report) = run_async_under_lab(0xf171_0101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for write in [false, true] {
            let (mut database, mut transaction) = fixture(&cx, &txcx, write).await;
            let mut checkpoints = 0;
            let completed = transaction.complete_controlled(&mut database, &cx, None, false, || {
                checkpoints += 1;
                Ok(())
            }).await.unwrap();
            assert_eq!(completed.commit_seq().is_some(), write);
            assert!(checkpoints > 10, "exercise history, labels, cascades and final acceptance");
            assert_eq!(txcx.outstanding_obligations(), 0);
            for stop in 1..=checkpoints {
                let (mut database, mut transaction) = fixture(&cx, &txcx, write).await;
                let frontier = database.frontier().unwrap();
                let mut seen = 0;
                let result = transaction.complete_controlled(&mut database, &cx, None, false, || {
                    seen += 1;
                    if seen == stop { Err(WriteTxnError::NoPreparedWrite) } else { Ok(()) }
                }).await;
                assert!(matches!(result, Err(WriteTxnError::NoPreparedWrite)), "stop {stop}");
                assert_eq!(seen, stop);
                assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
                assert!(transaction.pin.is_none());
                assert!(transaction.prepared.is_none());
                assert!(transaction.staged.is_empty());
                assert!(transaction.read_set.borrow().is_empty());
                assert_eq!(txcx.outstanding_obligations(), 0);
                assert_eq!(database.frontier().unwrap(), frontier);
                assert!(database.vertex(VId(99)).unwrap().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unwinding_completion_releases_the_pin_without_dropping_the_transaction() {
    let ((), report) = run_async_under_lab(0xf171_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let (mut database, mut transaction) = fixture(&cx, &txcx, true).await;
        let frontier = database.frontier().unwrap();
        let mut future = Box::pin(transaction.complete_controlled(
            &mut database, &cx, None, false, || panic!("injected validation unwind"),
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
        assert_eq!(txcx.outstanding_obligations(), 0);
        assert_eq!(database.frontier().unwrap(), frontier);
        assert!(database.vertex(VId(99)).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropped_guard_classifies_real_durability_fences_and_never_recovers_from_drop() {
    let ((), report) = run_async_under_lab(0xf171_0103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for phase in 0..4 {
            let mut database = Database::open_memory(&cx, keys()).await.unwrap();
            let basis = database.frontier().unwrap();
            let mut transaction = database.begin(&txcx).unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            batch.create_vertex(VId(1), vec![], vec![]);
            transaction.write(&mut database, batch).unwrap();
            let prepared = transaction.prepared.take().unwrap();
            let suspended = Cell::new(false);
            let mut future = Box::pin(async {
                // Exercise the production cleanup guard over real Chronicle
                // states, then suspend at a deterministic test-owned boundary.
                // The published fence is not mocked or assigned by this test.
                let mut guard = TxnCompletionGuard::new(&mut transaction, &mut database);
                guard.entered_commit = true;
                let crash = match phase {
                    0 => Some(fgdb_chronicle::commit::CrashPoint::BeforeCapsule),
                    1 => Some(fgdb_chronicle::commit::CrashPoint::AfterMarkerBeforeD2),
                    _ => None,
                };
                let publication_failure = (phase == 2)
                    .then_some(DerivedPublicationStage::FoldCommittedTemplate);
                let result = guard.database.commit_template(
                    &cx, prepared.template, crash, publication_failure, None,
                ).await;
                assert_eq!(result.is_ok(), phase == 3);
                suspended.set(true);
                std::future::pending::<()>().await;
            });
            std::future::poll_fn(|task| match future.as_mut().poll(task) {
                Poll::Pending if suspended.get() => Poll::Ready(()),
                Poll::Pending => Poll::Pending,
                Poll::Ready(()) => panic!("the explicit suspension must not complete"),
            }).await;
            drop(future);
            let expected = match phase {
                0 => EmbeddedTxnState::Aborted,
                1 => EmbeddedTxnState::CommitOutcomeUnknown { published_frontier: basis },
                2 => EmbeddedTxnState::CommittedNeedsRecovery { commit_seq: CommitSeq(basis.0 + 1) },
                _ => EmbeddedTxnState::Completed(EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: CommitSeq(basis.0 + 1),
                }),
            };
            assert_eq!(transaction.state(), expected);
            assert!(transaction.pin.is_none());
            assert!(transaction.staged.is_empty());
            assert_eq!(txcx.outstanding_obligations(), 0);
            match phase {
                1 => assert!(matches!(database.state(), crate::DatabaseState::CommitOutcomeUnknown { .. })),
                2 => assert!(matches!(database.state(), crate::DatabaseState::NeedsAuthoritativeRecovery(_))),
                _ => assert!(matches!(database.state(), crate::DatabaseState::Healthy { .. })),
            }
            let recovered = database.recover_authoritatively(&cx).await.unwrap();
            if phase == 0 { assert!(recovered.vertex(VId(1)).unwrap().is_none()); }
            if phase >= 2 { assert!(recovered.vertex(VId(1)).unwrap().is_some()); }
            assert_eq!(transaction.state(), expected, "recovery does not rewrite local historical outcomes");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
