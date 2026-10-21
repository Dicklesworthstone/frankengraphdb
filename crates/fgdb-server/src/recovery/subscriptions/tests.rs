//! Real private native circuits, the server admission cap and its host worker.
use super::*;
use crate::execute::{poll, subscribe, Refusal};
use crate::recovery::tests::{served, settled, vertex};
use crate::{Server, Served};
use asupersync::lab::run_async_under_lab;
use fgdb_protocol::body::{Execute, ExecuteMode, WireValue};
use fgdb_warden::Restriction;

fn request() -> Execute {
    Execute {
        mode: ExecuteMode::Subscribe,
        statement: "SUBSCRIBE TO MATCH (n) RETURN n".into(),
        parameters: vec![],
    }
}

fn success<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{:?}: {}", error.code, error.message))
}

fn cap(server: &mut Server, count: usize) {
    Arc::get_mut(server.databases.get_mut("test").unwrap())
        .unwrap()
        .max_subscriptions = count;
}

async fn released(db: &Served, expected: usize) {
    let mut watcher = db.commits.watcher();
    poll_fn(|task| loop {
        assert!(matches!(db.db.status(), DatabaseStatus::Ready { .. }));
        if db.subscriptions.load(Ordering::Acquire) == expected {
            return Poll::Ready(());
        }
        if !watcher.poll_changed(task) {
            return Poll::Pending;
        }
    })
    .await;
}

#[test]
fn two_hundred_reconnects_reuse_a_single_live_slot_and_keep_the_cap() {
    let ((), report) = run_async_under_lab(0x79a0_0401, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-release-200").await;
        cap(&mut server, 1);
        let db = &server.databases["test"];
        for completed in 0..200_u64 {
            // No intervening cleanup call: admission itself must not race the
            // worker and mistake its pending retirement for a lifetime cap.
            let mut active = success(subscribe(&root, db, &token, &request()).await);
            assert_eq!(server.subscription_counts(&root, "test"), Some((1, completed)));
            let refusal = subscribe(&root, db, &token, &request()).await.err().unwrap();
            assert_eq!(refusal.code, ErrorCode::Budget);
            let initial = success(poll(&root, db, &token, &mut active).await).unwrap();
            assert!(initial.batch.is_snapshot());
            if completed % 2 == 0 {
                active.consumer.acknowledge(initial.batch.receipt()).unwrap();
                active.consumer.close();
                active.consumer.close();
                assert!(matches!(
                    active.consumer.acknowledge(initial.batch.receipt()),
                    Err(SubscriptionError::Closed)
                ));
            }
            // The other half abandon a pending baseline. Neither exit may ACK
            // it implicitly or leave its native producer/replay sink running.
            drop(active);
        }
        released(db, 0).await;
        assert_eq!(server.subscription_counts(&root, "test"), Some((0, 200)));
        assert!(db.db.shared.gate.state.read().unwrap().retired.is_none());
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn idle_worker_reclaims_without_a_followup_request_and_never_refunds_before_release() {
    let ((), report) = run_async_under_lab(0x79a0_0402, |root| async move {
        let (server, token, _) = served(&root, "subscription-release-idle").await;
        let db = &server.databases["test"];
        let mut active = success(subscribe(&root, db, &token, &request()).await);
        let initial = success(poll(&root, db, &token, &mut active).await).unwrap();
        let retained = Arc::clone(&initial.batch);
        let held = db.db.read(&root).await.unwrap();
        drop(active);
        asupersync::runtime::yield_now().await;
        assert_eq!(server.subscription_counts(&root, "test"), Some((1, 0)));
        assert!(db.db.shared.gate.state.read().unwrap().retired.is_some());
        drop(held);
        released(db, 0).await;
        assert_eq!(server.subscription_counts(&root, "test"), Some((0, 1)));
        assert!(Arc::ptr_eq(&retained, &initial.batch));
        assert!(retained.is_snapshot());
        assert!(retained.rows().is_empty());
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn interior_release_preserves_other_producers_and_receipt_isolation() {
    let ((), report) = run_async_under_lab(0x79a0_0403, |root| async move {
        let (mut server, token, _) = served(&root, "subscription-release-interior").await;
        cap(&mut server, 2);
        let db = &server.databases["test"];
        let mut first = success(subscribe(&root, db, &token, &request()).await);
        let mut survivor = success(subscribe(&root, db, &token, &request()).await);
        let old = success(poll(&root, db, &token, &mut first).await).unwrap();
        let base = success(poll(&root, db, &token, &mut survivor).await).unwrap();
        survivor.consumer.acknowledge(base.batch.receipt()).unwrap();
        drop(first);
        let mut next = success(subscribe(&root, db, &token, &request()).await);
        assert_eq!(server.subscription_counts(&root, "test"), Some((2, 1)));
        assert!(matches!(
            next.consumer.acknowledge(old.batch.receipt()),
            Err(SubscriptionError::InvalidReceipt)
        ));
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let at = db.db.write(&root).await.unwrap()
            .write(&contexts.commit(), vertex(9)).await.unwrap();
        let change = success(poll(&root, db, &token, &mut survivor).await).unwrap();
        assert_eq!(change.batch.frontier(), at);
        assert_eq!(change.entries, [(1, vec![WireValue::Vertex(9)])]);
        assert!(!change.batch.is_snapshot());
        survivor.consumer.acknowledge(change.batch.receipt()).unwrap();
        drop(next);
        drop(survivor);
        released(db, 0).await;
        assert_eq!(server.subscription_counts(&root, "test"), Some((0, 3)));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn recovery_consumes_queued_old_state_and_late_old_drop_cannot_refund_new_slots() {
    let ((), report) = run_async_under_lab(0x79a0_0404, |root| async move {
        let (server, token, _) = served(&root, "subscription-release-generation").await;
        let db = &server.databases["test"];
        let mut old = success(subscribe(&root, db, &token, &request()).await);
        let queued = success(subscribe(&root, db, &token, &request()).await);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let operation = db.db.enter().unwrap();
        let mut guard = db.db.write(&root).await.unwrap();
        // Queue AFTER acquiring the lock, so ordinary admission cannot drain
        // it before this deliberately induced authoritative recovery.
        drop(queued);
        let prepared = guard.prepare_atomic_writes(vec![vertex(7)]).unwrap();
        let error = guard.commit_prepared_with_crash(
            &contexts.commit(), prepared, Some(fgdb::CrashPoint::AfterMarkerBeforeD2),
        ).await.unwrap_err();
        assert_eq!(crate::execute::write_refusal(&error).code, ErrorCode::OutcomeUnknown);
        drop(guard);
        assert_eq!(db.db.status(), DatabaseStatus::Recovering { generation: 2 });
        drop(operation);
        assert_eq!(settled(db).await, DatabaseStatus::Ready { generation: 2 });
        assert_eq!(server.subscription_counts(&root, "test"), Some((0, 2)));
        let replacement = success(subscribe(&root, db, &token, &request()).await);
        old.consumer.close();
        drop(old);
        assert_eq!(server.subscription_counts(&root, "test"), Some((1, 2)));
        drop(replacement);
        released(db, 0).await;
        assert_eq!(server.subscription_counts(&root, "test"), Some((0, 3)));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retirement_needs_no_live_client_grant_and_failed_maintenance_is_releasable() {
    let ((), report) = run_async_under_lab(0x79a0_0405, |root| async move {
        let (server, token, _) = served(&root, "subscription-release-authority").await;
        let db = &server.databases["test"];
        let bounded = token.attenuate(Restriction::MaxWork(4096)).unwrap()
            .attenuate(Restriction::MaxNodes(64)).unwrap();
        let mut failed = success(subscribe(&root, db, &bounded, &request()).await);
        let initial = success(poll(&root, db, &bounded, &mut failed).await).unwrap();
        failed.consumer.acknowledge(initial.batch.receipt()).unwrap();
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut batch = vertex(1);
        for id in 2..=200 {
            batch.create_vertex(fgdb_types::VId(id), vec![], vec![]);
        }
        db.db.write(&root).await.unwrap().write(&contexts.commit(), batch).await.unwrap();
        let error = poll(&root, db, &token, &mut failed).await.err().unwrap();
        assert_eq!(error.code, ErrorCode::Budget);
        drop(failed);
        let mut active = success(subscribe(&root, db, &token, &request()).await);
        assert_eq!(server.subscription_counts(&root, "test"), Some((1, 1)));
        db.authority.retire();
        assert!(poll(&root, db, &token, &mut active).await.is_err());
        drop(active);
        released(db, 0).await;
        assert_eq!(server.subscription_counts(&root, "test"), Some((0, 2)));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropped_outer_future_and_unwind_transfer_cleanup_without_an_async_destructor() {
    use std::future::{pending, Future};
    let ((), report) = run_async_under_lab(0x79a0_0406, |root| async move {
        let (server, token, _) = served(&root, "subscription-release-abandoned").await;
        let db = &server.databases["test"];
        let mut future = Box::pin(async {
            let active = success(subscribe(&root, db, &token, &request()).await);
            pending::<()>().await;
            drop(active);
        });
        poll_fn(|task| {
            assert!(future.as_mut().poll(task).is_pending());
            if db.subscriptions.load(Ordering::Acquire) == 1 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }).await;
        drop(future);
        released(db, 0).await;
        let active = success(subscribe(&root, db, &token, &request()).await);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _owned = active;
            panic!("injected adapter unwind");
        })).is_err());
        released(db, 0).await;
        assert_eq!(server.subscription_counts(&root, "test"), Some((0, 2)));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
