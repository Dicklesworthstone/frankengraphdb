//! Real Chronicle/UnixVfs recovery under the deterministic host scheduler.
//! A surviving unsynced marker models surviving bytes, not a power-loss VFS.

use super::*;
use crate::{DatabaseConfig, MaintenanceError, Served, Server, ServerLimits};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{CrashPoint, DatabaseKeys, DerivedPublicationStage, WriteBatch, WriteError};
use fgdb_delta_types::RelationId;
use fgdb_protocol::body::{Execute, ExecuteMode};
use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, VId};
use fgdb_warden::{CapabilityToken, Grant, QueryLimits, Rights, Scope};
use std::future::Future;
use std::path::PathBuf;

#[derive(Default)]
pub(crate) struct WakeCounter(pub(crate) AtomicUsize);
impl std::task::Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// The real adapters write into this bounded-poll byte sink. Pending writes
/// do not wake themselves, so recovery must supply the wake under test.
pub(crate) struct WireIo {
    state: Arc<WireState>,
    writes: std::collections::VecDeque<usize>,
    flush_pending: bool,
}

pub(crate) struct WireState {
    bytes: std::sync::Mutex<Vec<u8>>,
    pub(crate) writes: AtomicUsize,
    pub(crate) flushes: AtomicUsize,
}

impl WireState {
    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }
}

impl WireIo {
    pub(crate) fn new(
        writes: impl IntoIterator<Item = usize>,
        flush_pending: bool,
    ) -> (Self, Arc<WireState>) {
        let state = Arc::new(WireState {
            bytes: std::sync::Mutex::new(Vec::new()),
            writes: AtomicUsize::new(0),
            flushes: AtomicUsize::new(0),
        });
        (
            Self {
                state: Arc::clone(&state),
                writes: writes.into_iter().collect(),
                flush_pending,
            },
            state,
        )
    }
}

impl asupersync::io::AsyncRead for WireIo {
    fn poll_read(
        self: core::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut asupersync::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
}

impl asupersync::io::AsyncWrite for WireIo {
    fn poll_write(
        mut self: core::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.state.writes.fetch_add(1, Ordering::Relaxed);
        let count = self
            .writes
            .pop_front()
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        if count == 0 {
            return Poll::Pending;
        }
        self.state
            .bytes
            .lock()
            .unwrap()
            .extend_from_slice(&bytes[..count]);
        Poll::Ready(Ok(count))
    }
    fn poll_flush(
        mut self: core::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.state.flushes.fetch_add(1, Ordering::Relaxed);
        if self.flush_pending {
            self.flush_pending = false;
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
    fn poll_shutdown(
        self: core::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

pub(crate) async fn served(cx: &Cx, name: &str) -> (Server, CapabilityToken, PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "fgdb-server-recovery-{}-{name}",
        std::process::id()
    ));
    let keys = DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    );
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    drop(
        Database::create(&contexts.commit(), &path, keys.clone())
            .await
            .unwrap(),
    );
    let mut server = Server::new(cx, ServerLimits::default()).unwrap();
    server
        .open_database(
            cx,
            &path,
            DatabaseConfig::new("test", keys, AuthKey::from_seed(791)),
        )
        .await
        .unwrap();
    let db = &server.databases["test"];
    let token = db
        .authority
        .issue_at(
            &Grant {
                branch: crate::TRUNK.into(),
                labels: Scope::All,
                relations: Scope::All,
                properties: Scope::All,
                rights: Rights::ReadWrite,
                limits: QueryLimits {
                    max_nodes: 1_000_000,
                    max_work: 100_000_000,
                    max_rows: 1_000_000,
                },
                expires_at_ms: u64::MAX / 2,
            },
            crate::unix_millis(),
        )
        .unwrap();
    (server, token, path)
}

pub(crate) fn vertex(id: u128) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(VId(id), vec![], vec![]);
    batch
}

pub(crate) async fn fail_after_marker(cx: &Cx, db: &Served, id: u128) -> WriteError {
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let mut guard = db.db.write(cx).await.unwrap();
    let prepared = guard.prepare_atomic_writes(vec![vertex(id)]).unwrap();
    let error = guard
        .commit_prepared_with_crash(
            &contexts.commit(),
            prepared,
            Some(CrashPoint::AfterMarkerBeforeD2),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, WriteError::CommitOutcomeUnknown { .. }));
    assert_eq!(
        crate::execute::write_refusal(&error).code,
        ErrorCode::OutcomeUnknown
    );
    drop(guard);
    error
}

pub(crate) async fn settled(db: &Served) -> DatabaseStatus {
    let mut watcher = db.commits.watcher();
    poll_fn(|task| {
        loop {
            let state = db.db.status();
            if !matches!(state, DatabaseStatus::Recovering { .. }) {
                return Poll::Ready(state);
            }
            if !watcher.poll_changed(task) {
                return Poll::Pending;
            }
        }
    })
    .await
}

#[test]
fn host_maintenance_preserves_pinned_readers_and_the_committed_frontier() {
    let ((), report) = run_async_under_lab(0x79a0_0010, |root| async move {
        let (server, token, path) = served(&root, "host-maintenance-pins").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(1))
            .await
            .unwrap();
        let (mut pinned, at) = crate::execute::read_session(&root, db, &token)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(at, CommitSeq(1));
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(2))
            .await
            .unwrap();

        let before = std::fs::read(path.join("manifest.root")).unwrap();
        server.compact_database(&root, "test").await.unwrap();
        assert_ne!(std::fs::read(path.join("manifest.root")).unwrap(), before);
        let summary = server.scrub_database(&root, "test").await.unwrap();
        assert_eq!(summary.objects, 2);
        assert_eq!(summary.clean.len() + summary.repaired.len(), 2);
        assert!(summary.metadata_objects >= 2);
        assert!(summary.lost.is_empty());
        assert!(summary.block_lost.is_empty());
        assert!(summary.metadata_lost.is_empty());
        assert_eq!(
            server.database_status(&root, "test"),
            Some(DatabaseStatus::Ready { generation: 1 })
        );
        let (answer, pinned_at) = pinned
            .query_with_snapshot(
                &contexts.query(),
                "MATCH (n) RETURN n",
                &fgdb_gql::GqlParameters::new(),
            )
            .ok()
            .unwrap();
        assert_eq!(pinned_at, CommitSeq(1));
        assert!(matches!(answer, fgdb::QueryResult::Rows { rows, .. } if rows.len() == 1));
        let (fresh, generation) = db.db.read_view(&root).await.unwrap();
        assert_eq!(fresh.frontier(), CommitSeq(2));
        assert!(fresh.vertex(VId(1)).unwrap().is_some());
        assert!(fresh.vertex(VId(2)).unwrap().is_some());
        generation.check().unwrap();
        assert_eq!(
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), vertex(3))
                .await
                .unwrap(),
            CommitSeq(3)
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn host_maintenance_admission_tracks_waiters_cancellation_and_drain() {
    let ((), report) = run_async_under_lab(0x79a0_0011, |root| async move {
        let (server, _, _) = served(&root, "host-maintenance-admission").await;
        assert!(matches!(
            server.compact_database(&root, "unknown").await,
            Err(MaintenanceError::UnknownDatabase(name)) if name == "unknown"
        ));
        assert!(matches!(
            server.scrub_database(&root, "unknown").await,
            Err(MaintenanceError::UnknownDatabase(name)) if name == "unknown"
        ));
        let db = &server.databases["test"];
        let held = db.db.write(&root).await.unwrap();
        let mut maintenance = Box::pin(server.compact_database(&root, "test"));
        assert!(
            maintenance
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        assert_eq!(db.db.shared.gate.state.read().unwrap().active, 1);
        db.db.stop();
        drop(maintenance);
        assert_eq!(db.db.shared.gate.state.read().unwrap().active, 0);
        drop(held);
        server.join_database_workers(&root).await;
        assert!(matches!(
            server.compact_database(&root, "test").await,
            Err(MaintenanceError::DatabaseUnavailable(
                DatabaseStatus::Stopped
            ))
        ));
        assert!(matches!(
            server.scrub_database(&root, "test").await,
            Err(MaintenanceError::DatabaseUnavailable(
                DatabaseStatus::Stopped
            ))
        ));
        let (draining, _, _) = served(&root, "host-maintenance-draining").await;
        draining.shutdown().trigger();
        assert!(matches!(
            draining.compact_database(&root, "test").await,
            Err(MaintenanceError::Draining)
        ));
        assert!(matches!(
            draining.scrub_database(&root, "test").await,
            Err(MaintenanceError::Draining)
        ));
        draining.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn host_scrub_loss_fences_pinned_readers_and_uses_authoritative_recovery() {
    let ((), report) = run_async_under_lab(0x79a0_0012, |root| async move {
        let (server, token, path) = served(&root, "host-scrub-loss").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(1))
            .await
            .unwrap();
        let (mut pinned, _) = crate::execute::read_session(&root, db, &token)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        let admitted = db.db.enter().unwrap();
        // Damage only this test's newly created committed capsule. The scrub
        // must not keep serving the retained graph merely because it is warm.
        let capsules = std::fs::read_dir(path.join("capsules"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(capsules.len(), 1);
        let capsule = &capsules[0];
        let intact = std::fs::read(capsule).unwrap();
        std::fs::write(capsule, b"damaged capsule for host scrub law").unwrap();
        let summary = server.scrub_database(&root, "test").await.unwrap();
        assert_eq!(summary.lost.len(), 1);
        assert_eq!(db.db.status(), DatabaseStatus::Recovering { generation: 2 });
        assert!(matches!(
            pinned.query(
                &contexts.query(),
                "MATCH (n) RETURN n",
                &fgdb_gql::GqlParameters::new(),
            ),
            Err(crate::execute::SessionError::Unavailable(
                Unavailable::Recovering
            ))
        ));
        assert!(matches!(
            server.compact_database(&root, "test").await,
            Err(MaintenanceError::DatabaseUnavailable(
                DatabaseStatus::Recovering { generation: 2 }
            ))
        ));
        drop(admitted);
        assert!(matches!(
            settled(db).await,
            DatabaseStatus::Fenced { generation: 2, .. }
        ));
        std::fs::write(capsule, intact).unwrap();
        assert!(matches!(
            server.scrub_database(&root, "test").await,
            Err(MaintenanceError::DatabaseUnavailable(
                DatabaseStatus::Fenced { generation: 2, .. }
            ))
        ));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn real_marker_boundaries_recover_once_and_preserve_completion_classes() {
    let ((), report) = run_async_under_lab(0x79a0_0001, |root| async move {
        for (name, point, needs_recovery) in [
            ("before-marker", CrashPoint::AfterD1, false),
            ("marker-survived", CrashPoint::AfterMarkerBeforeD2, true),
            (
                "marker-synced",
                CrashPoint::AfterMarkerFileSyncBeforeDirectorySync,
                true,
            ),
        ] {
            let (server, _, _) = served(&root, name).await;
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let old = db.db.generation().unwrap();
            let admitted = db.db.enter().unwrap();
            let mut guard = db.db.write(&root).await.unwrap();
            let prepared = guard.prepare_atomic_writes(vec![vertex(1)]).unwrap();
            let error = guard
                .commit_prepared_with_crash(&contexts.commit(), prepared, Some(point))
                .await
                .unwrap_err();
            drop(guard);
            if needs_recovery {
                assert_eq!(
                    crate::execute::write_refusal(&error).code,
                    ErrorCode::OutcomeUnknown
                );
                assert_eq!(db.db.status(), DatabaseStatus::Recovering { generation: 2 });
                assert!(db.db.shared.gate.state.read().unwrap().published.is_none());
                assert!(matches!(old.enter(), Err(Unavailable::Recovering)));
                assert!(matches!(
                    db.db.read(&root).await,
                    Err(Unavailable::Recovering)
                ));
                assert!(matches!(
                    db.db.read_view(&root).await,
                    Err(Unavailable::Recovering)
                ));
                // The root worker can run, but cannot consume the handle yet.
                asupersync::runtime::yield_now().await;
                assert_eq!(db.db.status(), DatabaseStatus::Recovering { generation: 2 });
            } else {
                assert_ne!(
                    crate::execute::write_refusal(&error).code,
                    ErrorCode::OutcomeUnknown
                );
                assert_eq!(db.db.status(), DatabaseStatus::Ready { generation: 1 });
            }
            drop(admitted);
            let expected_generation = if needs_recovery { 2 } else { 1 };
            assert_eq!(
                settled(db).await,
                DatabaseStatus::Ready {
                    generation: expected_generation
                }
            );
            assert_eq!(old.check().is_err(), needs_recovery);
            let (view, generation) = db.db.read_view(&root).await.unwrap();
            assert_eq!(view.frontier(), CommitSeq(u64::from(needs_recovery)));
            assert_eq!(view.vertex(VId(1)).unwrap().is_some(), needs_recovery);
            generation.check().unwrap();
            drop(view);
            let mut guard = db.db.write(&root).await.unwrap();
            assert_eq!(
                guard.frontier().unwrap(),
                CommitSeq(u64::from(needs_recovery))
            );
            assert_eq!(guard.vertex(VId(1)).unwrap().is_some(), needs_recovery);
            let next = guard.write(&contexts.commit(), vertex(2)).await.unwrap();
            assert_eq!(next, CommitSeq(1 + u64::from(needs_recovery)));
            assert_eq!(
                guard.vertices().unwrap().len(),
                1 + usize::from(needs_recovery)
            );
            drop(guard);
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_d2_recovery_invalidates_pins_and_resets_registration_ownership() {
    let ((), report) = run_async_under_lab(0x79a0_0002, |root| async move {
        let (server, token, _) = served(&root, "post-d2-pins").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (mut pinned, _) = crate::execute::read_session(&root, db, &token)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        let statement = Execute {
            mode: ExecuteMode::Subscribe,
            statement: "SUBSCRIBE TO MATCH (n) RETURN n".into(),
            parameters: vec![],
        };
        let mut old = crate::execute::subscribe(&root, db, &token, &statement)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 1);
        let mut guard = db.db.write(&root).await.unwrap();
        let error = guard
            .write_with_publication_failure(
                &contexts.commit(),
                vertex(7),
                DerivedPublicationStage::FoldCommittedTemplate,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, WriteError::CommittedNeedsRecovery { .. }));
        assert_eq!(
            crate::execute::write_refusal(&error).code,
            ErrorCode::OutcomeUnknown
        );
        drop(guard);
        // Neither retained session owns an active source operation, so both
        // may stay alive while the recovery worker completes.
        assert_eq!(settled(db).await, DatabaseStatus::Ready { generation: 2 });
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 0);
        assert!(matches!(
            pinned.query(
                &contexts.query(),
                "MATCH (n) RETURN n",
                &fgdb_gql::GqlParameters::new()
            ),
            Err(crate::execute::SessionError::Unavailable(
                Unavailable::Stale
            ))
        ));
        let refusal = crate::execute::poll(&root, db, &token, &mut old)
            .await
            .err()
            .unwrap();
        assert_eq!(refusal.code, ErrorCode::DatabaseRecovering);
        assert!(!refusal.message.contains("owner"));
        let mut replacement = crate::execute::subscribe(&root, db, &token, &statement)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        old.consumer.close();
        drop(old);
        assert_eq!(db.subscriptions.load(Ordering::Acquire), 1);
        let baseline = crate::execute::poll(&root, db, &token, &mut replacement)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message))
            .unwrap();
        assert!(baseline.batch.is_snapshot());
        assert_eq!(baseline.batch.frontier(), CommitSeq(1));
        assert_eq!(baseline.batch.rows().len(), 1);
        replacement.consumer.close();
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_authoritative_reopen_stays_fenced_after_storage_is_repaired() {
    let ((), report) = run_async_under_lab(0x79a0_0003, |root| async move {
        let (server, _, path) = served(&root, "sticky-reopen-failure").await;
        let db = &server.databases["test"];
        let admitted = db.db.enter().unwrap();
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut guard = db.db.write(&root).await.unwrap();
        let error = guard
            .write_with_publication_failure(
                &contexts.commit(),
                vertex(9),
                DerivedPublicationStage::FoldCommittedTemplate,
            )
            .await
            .unwrap_err();
        assert_eq!(
            crate::execute::write_refusal(&error).code,
            ErrorCode::OutcomeUnknown
        );
        drop(guard);
        // Only this newly created test database is damaged. The marker is
        // durable, so its capsule must authenticate; no tail truncation is an
        // acceptable replacement for an authoritative recovery failure.
        let capsules = std::fs::read_dir(path.join("capsules"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(capsules.len(), 1);
        let capsule = &capsules[0];
        let intact = std::fs::read(capsule).unwrap();
        std::fs::write(capsule, b"damaged committed test capsule").unwrap();
        drop(admitted);
        let failed = settled(db).await;
        assert!(
            matches!(&failed, DatabaseStatus::Fenced { generation: 2, reason } if !reason.is_empty())
        );
        std::fs::write(capsule, intact).unwrap();
        for _ in 0..3 {
            asupersync::runtime::yield_now().await;
            assert_eq!(db.db.status(), failed);
            assert!(db.db.shared.gate.state.read().unwrap().published.is_none());
            assert!(matches!(db.db.read(&root).await, Err(Unavailable::Fenced)));
            assert!(matches!(db.db.write(&root).await, Err(Unavailable::Fenced)));
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropped_outer_write_future_still_fences_and_recovers_its_marker() {
    let ((), report) = run_async_under_lab(0x79a0_0004, |root| async move {
        let (server, _, _) = served(&root, "dropped-write").await;
        let db = &server.databases["test"];
        let reached = std::sync::atomic::AtomicBool::new(false);
        let mut operation = Box::pin(async {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut guard = db.db.write(&root).await.unwrap();
            let prepared = guard.prepare_atomic_writes(vec![vertex(17)]).unwrap();
            let error = guard
                .commit_prepared_with_crash(
                    &contexts.commit(),
                    prepared,
                    Some(CrashPoint::AfterMarkerBeforeD2),
                )
                .await
                .unwrap_err();
            assert_eq!(
                crate::execute::write_refusal(&error).code,
                ErrorCode::OutcomeUnknown
            );
            reached.store(true, Ordering::Release);
            // Model cancellation before the outer adapter observes/handles
            // the native completion; the guard remains inside this future.
            core::future::pending::<()>().await;
            drop(guard);
        });
        poll_fn(|task| {
            assert!(operation.as_mut().poll(task).is_pending());
            if reached.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        drop(operation);
        assert_eq!(db.db.status(), DatabaseStatus::Recovering { generation: 2 });
        assert_eq!(settled(db).await, DatabaseStatus::Ready { generation: 2 });
        let guard = db.db.read(&root).await.unwrap();
        assert_eq!(guard.frontier().unwrap(), CommitSeq(1));
        assert!(guard.vertex(VId(17)).unwrap().is_some());
        drop(guard);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn published_read_admission_does_not_wait_for_a_writer_or_permanently_pin_history() {
    let ((), report) = run_async_under_lab(0x79a0_0008, |root| async move {
        let (server, token, _) = served(&root, "published-read-overlap").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut held = db.db.write(&root).await.unwrap();
        let mut admission = Box::pin(crate::execute::read_session(&root, db, &token));
        let (mut first, at) = match admission
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            Poll::Ready(Ok(result)) => result,
            Poll::Ready(Err(error)) => panic!("{}", error.message),
            Poll::Pending => panic!("a published read waited behind an unchanged writer"),
        };
        drop(admission);
        assert_eq!(at, CommitSeq(0));
        let (rows, result_at) = first
            .query_with_snapshot(
                &contexts.query(),
                "MATCH (n) RETURN 7 AS value",
                &fgdb_gql::GqlParameters::new(),
            )
            .ok()
            .unwrap();
        assert_eq!(result_at, at);
        assert!(matches!(rows, fgdb::QueryResult::Rows { rows, .. } if rows.is_empty()));
        drop(first);

        // No actual reader remains: the published record must not retain the
        // old allocation after advancement. The weak ownership in production
        // also permits make_mut to dissociate it without copying metadata.
        held.write(&contexts.commit(), vertex(1)).await.unwrap();
        assert!(
            db.db
                .shared
                .gate
                .state
                .read()
                .unwrap()
                .published
                .as_ref()
                .unwrap()
                .upgrade()
                .is_none()
        );
        let mut fallback = Box::pin(db.db.read_view(&root));
        assert!(
            fallback
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        drop(fallback);
        drop(held);

        let (old, generation) = db.db.read_view(&root).await.unwrap();
        assert_eq!(old.frontier(), CommitSeq(1));
        assert!(old.vertex(VId(1)).unwrap().is_some());
        let mut held = db.db.write(&root).await.unwrap();
        held.write(&contexts.commit(), vertex(2)).await.unwrap();
        // A real old pin keeps the published generation available throughout
        // the successor commit. The gate does not expose the successor early.
        let mut overlap = Box::pin(db.db.read_view(&root));
        let (during, during_generation) = match overlap
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            Poll::Ready(Ok(result)) => result,
            _ => panic!("the retained published generation must be immediately pinnable"),
        };
        drop(overlap);
        assert_eq!(during.frontier(), CommitSeq(1));
        assert!(during.vertex(VId(2)).unwrap().is_none());
        generation.check().unwrap();
        during_generation.check().unwrap();

        // Prepared admission and eager execution use the same generation,
        // including their result sequence, without re-entering the writer.
        let prepared = crate::execute::prepare_read(
            &root,
            db,
            &token,
            &fgdb_protocol::body::Prepare {
                statement: "MATCH (n) RETURN $v AS value".into(),
                parameters: vec![("v".into(), fgdb_protocol::body::WireValue::Int(9))],
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        let answer = crate::execute::read_prepared(
            &root,
            db,
            &token,
            &prepared,
            &[("v".into(), fgdb_protocol::body::WireValue::Int(11))],
        )
        .await
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            answer.outcome,
            fgdb_protocol::body::Outcome::Rows { seq: 1 }
        );
        assert_eq!(
            answer.rows,
            vec![vec![fgdb_protocol::body::WireValue::Int(11)]]
        );
        drop(held);
        let (fresh, _) = db.db.read_view(&root).await.unwrap();
        assert_eq!(fresh.frontier(), CommitSeq(2));
        assert!(fresh.vertex(VId(2)).unwrap().is_some());
        assert_eq!(old.frontier(), CommitSeq(1));
        assert!(old.vertex(VId(2)).unwrap().is_none());
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn abort_before_worker_first_poll_fences_instead_of_leaving_a_ready_slot() {
    let ((), report) = run_async_under_lab(0x79a0_0005, |root| async move {
        let (server, _, _) = served(&root, "unpolled-worker").await;
        let db = &server.databases["test"];
        // No await follows spawn inside served(), so this root has not given
        // the worker a first poll. Its captured exit guard must still run.
        db.db.worker.try_lock().unwrap().as_ref().unwrap().abort();
        db.db.stop_and_join(&root).await;
        assert!(matches!(db.db.status(), DatabaseStatus::Fenced { .. }));
        assert!(db.db.shared.gate.state.read().unwrap().published.is_none());
        assert!(matches!(db.db.generation(), Err(Unavailable::Fenced)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unused_server_drop_releases_worker_and_read_polls_do_not_wake_subscriptions() {
    let (weak, report) = run_async_under_lab(0x79a0_0006, |root| async move {
        let (server, token, _) = served(&root, "unused-server").await;
        let db = &server.databases["test"];
        let mut watcher = db.commits.watcher();
        assert!(!watcher.poll_changed(&std::task::Context::from_waker(std::task::Waker::noop())));
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (mut session, _) = crate::execute::read_session(&root, db, &token)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        for _ in 0..3 {
            assert!(
                session
                    .query(
                        &contexts.query(),
                        "MATCH (n) RETURN n",
                        &fgdb_gql::GqlParameters::new()
                    )
                    .is_ok()
            );
            assert!(
                !watcher.poll_changed(&std::task::Context::from_waker(std::task::Waker::noop())),
                "ordinary reads must not manufacture a commit wake"
            );
        }
        let weak = Arc::downgrade(&db.db.shared);
        drop(session);
        drop(watcher);
        drop(server);
        weak
    });
    assert!(report.lab_test_passed(), "{report:?}");
    assert!(
        weak.upgrade().is_none(),
        "dropping a server without a listener must not retain its worker/database"
    );
}

#[test]
fn stop_releases_published_index_ownership_and_late_writer_cannot_restore_it() {
    let ((), report) = run_async_under_lab(0x79a0_0009, |root| async move {
        let (server, _, _) = served(&root, "published-read-stop").await;
        let db = &server.databases["test"];
        let generation = db.db.generation().unwrap();
        let mut held = db.db.write(&root).await.unwrap();
        assert!(generation.gate.state.read().unwrap().published.is_some());
        db.db.stop();
        assert!(generation.gate.state.read().unwrap().published.is_none());
        assert!(matches!(
            db.db.read_view(&root).await,
            Err(Unavailable::Stopping)
        ));
        // An already-admitted writer can finish while shutdown drains. Its
        // healthy Drop must not recreate ownership in a stopped generation.
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        held.write(&contexts.commit(), vertex(1)).await.unwrap();
        drop(held);
        assert!(generation.gate.state.read().unwrap().published.is_none());
        server.join_database_workers(&root).await;
        drop(server);
        // A template or queued completion may retain the generation fence
        // beyond the server; that fence owns no snapshot or search-index pin.
        assert!(generation.gate.state.read().unwrap().published.is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
