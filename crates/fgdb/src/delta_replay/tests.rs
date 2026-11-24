use super::*;
use crate::{
    DatabaseKeys, DatabaseState, DerivedCheckpointError, DerivedCheckpointOutcome, MemVfs,
    MemVfsFile, PreparedWrite, WriteBatch, WriteError, WriteTxn, WriteTxnError,
};
use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, EmbeddedTxnState,
    PurposeContexts, QueryCx, TxnCx, VId,
};
use std::future::{Future, poll_fn};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

/// Observe only the real capsule I/O boundary. Faults alter read observations,
/// never the authoritative bytes or the graph's publication machinery.
#[derive(Clone, Debug)]
struct ReplayVfs {
    memory: MemVfs,
    reads: Arc<Mutex<Vec<PathBuf>>>,
    mode: Arc<AtomicU8>,
    allowed_reads: Arc<AtomicUsize>,
    alternate: Arc<Mutex<Option<PathBuf>>>,
}

impl ReplayVfs {
    fn new() -> Self {
        Self {
            memory: MemVfs::new().unwrap(),
            reads: Arc::default(),
            mode: Arc::new(AtomicU8::new(0)),
            allowed_reads: Arc::new(AtomicUsize::new(0)),
            alternate: Arc::default(),
        }
    }

    fn capsule(path: &Path) -> bool {
        path.parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "capsules")
    }

    fn count(&self) -> usize {
        self.reads.lock().unwrap().len()
    }

    fn reset(&self) {
        self.reads.lock().unwrap().clear();
        self.mode.store(0, Ordering::SeqCst);
        self.allowed_reads.store(0, Ordering::SeqCst);
    }
}

impl Vfs for ReplayVfs {
    type File = MemVfsFile;

    async fn open(&self, path: &Path, options: &OpenOptions) -> io::Result<Self::File> {
        self.memory.open(path, options).await
    }

    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        if Self::capsule(path) {
            self.reads.lock().unwrap().push(path.to_path_buf());
            let mode = if self.count() > self.allowed_reads.load(Ordering::SeqCst) {
                self.mode.load(Ordering::SeqCst)
            } else {
                0
            };
            match mode {
                1 => return Err(io::Error::other("injected replay read failure")),
                2 => {
                    let alternate = self.alternate.lock().unwrap().clone().unwrap();
                    return self.memory.open_read(&alternate).await;
                }
                3 => {
                    // Tests can resume a suspended read after requesting
                    // cancellation, as well as dropping the pending future.
                    poll_fn(|_| {
                        if self.mode.load(Ordering::SeqCst) == 3 {
                            Poll::Pending
                        } else {
                            Poll::Ready(())
                        }
                    })
                    .await;
                }
                _ => {}
            }
        }
        self.memory.open_read(path).await
    }

    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.memory.metadata(path).await
    }

    async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        if Self::capsule(path) && self.mode.load(Ordering::SeqCst) == 4 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "injected absent capsule",
            ));
        }
        self.memory.symlink_metadata(path).await
    }

    async fn set_permissions(&self, path: &Path, permissions: Permissions) -> io::Result<()> {
        self.memory.set_permissions(path, permissions).await
    }

    async fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.memory.create_dir(path).await
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.memory.create_dir_all(path).await
    }

    async fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.memory.remove_dir(path).await
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.memory.remove_file(path).await
    }

    async fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        self.memory.read_dir(path).await
    }

    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.memory.remove_dir_all(path).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.memory.rename(from, to).await
    }

    async fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        self.memory.copy(from, to).await
    }

    async fn hard_link(&self, original: &Path, link: &Path) -> io::Result<()> {
        self.memory.hard_link(original, link).await
    }

    async fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.memory.canonicalize(path).await
    }

    async fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.memory.read_link(path).await
    }

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if Self::capsule(path) {
            self.reads.lock().unwrap().push(path.to_path_buf());
        }
        self.memory.read(path).await
    }

    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.memory.read_to_string(path).await
    }

    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.memory.write(path, bytes).await
    }
}

fn keys(namespace: u8) -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([namespace; 32]),
        [0x73; 32],
    )
}

fn create(id: u128) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(
        VId(id),
        vec![],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(10))],
    );
    batch
}

async fn database(cx: &CommitCx, vfs: &ReplayVfs, namespace: u8) -> Database<ReplayVfs> {
    Database::create_with_vfs(cx, vfs.clone(), &vfs.memory.database_dir(), keys(namespace))
        .await
        .unwrap()
}

async fn validation_fixture(
    commit: &CommitCx,
    txn_cx: &TxnCx,
    consumer: u8,
    conflict: bool,
) -> (
    Database<ReplayVfs>,
    ReplayVfs,
    Option<PreparedWrite>,
    Option<WriteTxn>,
) {
    let vfs = ReplayVfs::new();
    let mut db = database(commit, &vfs, 0x72).await;
    let mut seed = create(1);
    seed.create_vertex(
        VId(2),
        vec![],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(10))],
    );
    db.write(commit, seed).await.unwrap();
    let mut staged = WriteBatch::new(RelationId(1));
    staged.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
    staged.create_vertex(VId(99), vec![], vec![]);
    let (prepared, transaction) = if consumer == 0 {
        (Some(db.prepare_write(staged).unwrap()), None)
    } else {
        let mut transaction = db.begin(txn_cx).unwrap();
        if consumer != 3 {
            transaction.vertex(&db, VId(1)).unwrap();
        }
        if consumer == 1 {
            transaction.write(&mut db, staged).unwrap();
        }
        (None, Some(transaction))
    };
    for value in [20, 30] {
        let mut changed = WriteBatch::new(RelationId(1));
        changed.set_vertex_property(
            VId(if conflict { 1 } else { 2 }),
            PropertyKeyId(1),
            Some(CanonicalScalar::Int(value)),
        );
        db.write(commit, changed).await.unwrap();
    }
    // A public reopen cannot preserve these same-owner handles. Evict only
    // derived batches to reproduce the exact lazy prefix shape, then append a
    // genuinely resident tail through the ordinary publication path.
    Arc::make_mut(&mut db.snapshot).delta_index =
        crate::empty_delta_window_at(commit, &db.coordinator, CommitSeq(3)).unwrap();
    db.delta_materialized_after = CommitSeq(3);
    db.write(commit, create(3)).await.unwrap();
    vfs.reset();
    let mut cursor = db.delta_replay(CommitSeq(1), CommitSeq(3)).unwrap();
    db.next_delta_replay(commit, &mut cursor)
        .await
        .unwrap()
        .unwrap();
    *vfs.alternate.lock().unwrap() = Some(vfs.reads.lock().unwrap()[0].clone());
    vfs.reset();
    (db, vfs, prepared, transaction)
}

async fn complete_validation_attempt(
    db: &mut Database<ReplayVfs>,
    commit: &CommitCx,
    prepared: &mut Option<PreparedWrite>,
    transaction: &mut Option<WriteTxn>,
    automatic: bool,
) -> Result<EmbeddedTxnCompletion, WriteTxnError> {
    if let Some(prepared) = prepared.take() {
        db.commit_prepared(commit, prepared)
            .await
            .map(|commit_seq| EmbeddedTxnCompletion::WriteCommitted { commit_seq })
            .map_err(WriteTxnError::from)
    } else if automatic {
        transaction.as_mut().unwrap().finish(db, commit).await
    } else {
        transaction
            .as_mut()
            .unwrap()
            .finish_strict(db, commit)
            .await
    }
}

#[test]
fn history_visitor_replays_only_missing_prefix_then_borrows_cached_tail() {
    let ((), report) = run_async_under_lab(0xd31a_1010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let (mut db, vfs, _, transaction) =
            validation_fixture(&commit, &contexts.txn(), 3, false).await;
        transaction.unwrap().abort();
        let snapshot = Arc::clone(&db.snapshot);
        let window = db.delta_index().unwrap().clone();
        let mut visited = Vec::new();
        assert_eq!(
            db.visit_delta_history(&commit, CommitSeq(1), |_, batch| {
                visited.push(batch.commit_seq());
                Ok::<(), ()>(())
            })
            .await
            .unwrap(),
            CommitSeq(4)
        );
        assert_eq!(visited, [CommitSeq(2), CommitSeq(3), CommitSeq(4)]);
        assert_eq!(vfs.count(), 2);
        assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
        assert_eq!(db.delta_index().unwrap(), &window);
        assert_eq!(db.delta_materialized_after, CommitSeq(3));

        vfs.reset();
        visited.clear();
        db.visit_delta_history(&commit, CommitSeq(3), |_, batch| {
            visited.push(batch.commit_seq());
            Ok::<(), ()>(())
        })
        .await
        .unwrap();
        assert_eq!(visited, [CommitSeq(4)]);
        assert_eq!(vfs.count(), 0, "cached batches never reread capsules");
        assert!(matches!(
            db.visit_delta_history(&commit, CommitSeq(5), |_, _| Ok::<(), ()>(()))
                .await,
            Err(DeltaHistoryError::Source(DeltaReplayError::Read(
                ReadError::BeyondFrontier { .. }
            )))
        ));
        Arc::make_mut(&mut db.snapshot)
            .delta_index
            .retire_prefix(CommitSeq(4))
            .unwrap();
        assert!(matches!(
            db.visit_delta_history(&commit, CommitSeq(1), |_, _| Ok::<(), ()>(()))
                .await,
            Err(DeltaHistoryError::Source(DeltaReplayError::Read(
                ReadError::DeltaCursorRetired { .. }
            )))
        ));
        assert_eq!(vfs.count(), 0);
        assert_eq!(db.delta_materialized_after, CommitSeq(3));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_and_strict_completion_do_not_publish_early_conflicts_before_source_authentication() {
    let ((), report) = run_async_under_lab(0xd31a_1011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        // Direct preparation plus strict and automatic write, observed read
        // and empty read all authenticate the complete interval before deciding.
        for (consumer, automatic) in [
            (0, false),
            (1, false),
            (2, false),
            (3, false),
            (1, true),
            (2, true),
            (3, true),
        ] {
            for fault in 0..3 {
                let (mut db, vfs, mut prepared, mut transaction) =
                    validation_fixture(&commit, &txn_cx, consumer, true).await;
                let retry = prepared.clone();
                let snapshot = Arc::clone(&db.snapshot);
                let window = db.delta_index().unwrap().clone();
                let manifest = db.manifest().unwrap();
                vfs.allowed_reads.store(1, Ordering::SeqCst);
                if fault == 0 {
                    // The first missing batch has a genuine conflict, but a
                    // later substituted capsule must retain source-error precedence.
                    vfs.mode.store(2, Ordering::SeqCst);
                    assert!(matches!(
                        complete_validation_attempt(
                            &mut db,
                            &commit,
                            &mut prepared,
                            &mut transaction,
                            automatic,
                        )
                        .await,
                        Err(WriteTxnError::Write(WriteError::HistoryRebuild(_)))
                    ));
                } else {
                    vfs.mode.store(3, Ordering::SeqCst);
                    let mut pending = Box::pin(complete_validation_attempt(
                        &mut db,
                        &commit,
                        &mut prepared,
                        &mut transaction,
                        automatic,
                    ));
                    poll_fn(|cx| {
                        assert!(pending.as_mut().poll(cx).is_pending());
                        assert_eq!(vfs.count(), 2);
                        Poll::Ready(())
                    })
                    .await;
                    if fault == 1 {
                        root.set_cancel_requested(true);
                        vfs.mode.store(0, Ordering::SeqCst);
                        let error = pending.await.unwrap_err();
                        root.set_cancel_requested(false);
                        if consumer == 0 {
                            assert!(matches!(error,
                                WriteTxnError::Write(WriteError::HistoryRebuild(error))
                                    if matches!(*error, RebuildError::Interrupted(_))));
                        } else {
                            assert!(matches!(error, WriteTxnError::Interrupted(_)));
                        }
                    } else {
                        drop(pending);
                    }
                }
                assert_eq!(vfs.count(), 2);
                assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
                assert_eq!(db.delta_index().unwrap(), &window);
                assert_eq!(db.delta_materialized_after, CommitSeq(3));
                assert_eq!(db.frontier().unwrap(), CommitSeq(4));
                assert_eq!(db.manifest().unwrap(), manifest);
                assert_eq!(db.coordinator.chain().entries().len(), 4);
                assert!(matches!(db.state(), DatabaseState::Healthy { .. }));
                assert!(db.vertex(VId(99)).unwrap().is_none());
                if let Some(transaction) = &transaction {
                    assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
                }
                assert_eq!(txn_cx.outstanding_obligations(), 0);

                if let Some(retry) = retry {
                    vfs.reset();
                    assert!(matches!(
                        db.commit_prepared(&commit, retry).await,
                        Err(WriteError::FirstCommitterWins { .. })
                    ));
                    assert_eq!(vfs.count(), 2);
                    assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
                    assert_eq!(db.delta_index().unwrap(), &window);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_and_strict_success_preserve_unloaded_prefix_and_cached_tail() {
    let ((), report) = run_async_under_lab(0xd31a_1012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        for (consumer, automatic) in [
            (0, false),
            (1, false),
            (2, false),
            (3, false),
            (1, true),
            (2, true),
            (3, true),
        ] {
            let (mut db, vfs, mut prepared, mut transaction) =
                validation_fixture(&commit, &txn_cx, consumer, false).await;
            let snapshot = Arc::clone(&db.snapshot);
            let window = db.delta_index().unwrap().clone();
            let completion = complete_validation_attempt(
                &mut db,
                &commit,
                &mut prepared,
                &mut transaction,
                automatic,
            )
            .await
            .unwrap();
            assert_eq!(db.delta_materialized_after, CommitSeq(3));
            assert_eq!(
                db.delta_index().unwrap().retained_after_commit_seq(),
                CommitSeq(3)
            );
            assert!(db.delta_index().unwrap().get(CommitSeq(2)).is_none());
            assert!(db.delta_index().unwrap().get(CommitSeq(3)).is_none());
            assert_eq!(snapshot.delta_index, window, "the prior pin is unchanged");
            if consumer < 2 {
                assert_eq!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted {
                        commit_seq: CommitSeq(5),
                    }
                );
                assert_eq!(db.delta_since(CommitSeq(3)).unwrap().count(), 2);
                assert!(db.vertex(VId(99)).unwrap().is_some());
                // Both policies preserve the sole ordinary publication path:
                // its prepared validator authenticates the missing prefix again.
                assert_eq!(vfs.count(), if consumer == 0 { 2 } else { 4 });
            } else {
                assert_eq!(
                    completion,
                    EmbeddedTxnCompletion::ReadClosed {
                        snapshot_seq: CommitSeq(1),
                        validated_through: CommitSeq(4),
                    }
                );
                assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
                assert_eq!(db.delta_index().unwrap(), &window);
                assert_eq!(vfs.count(), 2);
            }
            if let Some(transaction) = transaction {
                assert_eq!(transaction.state(), EmbeddedTxnState::Completed(completion));
            }
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn automatic_completion_preserves_read_conflicts_and_explicit_retirement() {
    let ((), report) = run_async_under_lab(0xd31a_1013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        for (consumer, retired) in [(1, false), (2, false), (1, true), (2, true), (3, true)] {
            let (mut db, vfs, _, transaction) =
                validation_fixture(&commit, &txn_cx, consumer, true).await;
            let mut transaction = transaction.unwrap();
            if retired {
                Arc::make_mut(&mut db.snapshot)
                    .delta_index
                    .retire_prefix(CommitSeq(4))
                    .unwrap();
            }
            let snapshot = Arc::clone(&db.snapshot);
            let window = db.delta_index().unwrap().clone();
            let manifest = db.manifest().unwrap();
            let error = transaction.finish(&mut db, &commit).await.unwrap_err();
            if retired {
                assert!(matches!(
                    error,
                    WriteTxnError::Write(WriteError::PreparedHistory(
                        crate::IndexError::CursorRetired {
                            asked: CommitSeq(1),
                            retained_after: CommitSeq(4),
                            frontier: CommitSeq(4),
                        }
                    ))
                ));
                assert_eq!(vfs.count(), 0);
            } else {
                assert!(matches!(
                    error,
                    WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    })
                ));
                assert_eq!(vfs.count(), 2);
            }
            assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
            assert_eq!(db.delta_index().unwrap(), &window);
            assert_eq!(db.delta_materialized_after, CommitSeq(3));
            assert_eq!(db.frontier().unwrap(), CommitSeq(4));
            assert_eq!(db.manifest().unwrap(), manifest);
            assert!(db.vertex(VId(99)).unwrap().is_none());
            assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn automatic_write_conflicts_materialize_only_for_replay_and_keep_later_reads() {
    let ((), report) = run_async_under_lab(0xd31a_1014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        for later_read in [false, true] {
            let vfs = ReplayVfs::new();
            let mut db = database(&commit, &vfs, 0x72).await;
            let mut seed = create(1);
            seed.create_vertex(
                VId(2),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(10))],
            );
            db.write(&commit, seed.clone()).await.unwrap();
            let mut transaction = db.begin(&txn_cx).unwrap();
            if later_read {
                transaction.vertex(&db, VId(2)).unwrap();
            }
            let mut staged = WriteBatch::new(RelationId(1));
            staged.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
            transaction.write(&mut db, staged.clone()).unwrap();
            // The earliest overlap is element-level FCW, eligible for the
            // existing disjoint-field rebase law. The next commit can also
            // invalidate a recorded read; replay must never erase that witness.
            let mut first = WriteBatch::new(RelationId(1));
            first.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(20)));
            db.write(&commit, first.clone()).await.unwrap();
            let mut second = WriteBatch::new(RelationId(1));
            second.set_vertex_property(VId(2), PropertyKeyId(1), Some(CanonicalScalar::Int(30)));
            db.write(&commit, second.clone()).await.unwrap();
            Arc::make_mut(&mut db.snapshot).delta_index =
                crate::empty_delta_window_at(&commit, &db.coordinator, CommitSeq(3)).unwrap();
            db.delta_materialized_after = CommitSeq(3);
            db.write(&commit, create(3)).await.unwrap();
            let pin = db.read_session().unwrap();
            vfs.reset();
            let result = transaction.commit(&mut db, &commit).await;
            // Two missing batches were streamed for validation, then admitted
            // for the existing replay evaluator. The cached tail was not read.
            assert_eq!(vfs.count(), 4);
            assert_eq!(db.delta_materialized_after, CommitSeq(1));
            assert_eq!(
                db.delta_index().unwrap().retained_after_commit_seq(),
                CommitSeq(1)
            );
            assert!(db.delta_index().unwrap().get(CommitSeq(2)).is_some());
            assert!(db.delta_index().unwrap().get(CommitSeq(3)).is_some());
            assert!(pin.snapshot.delta_index.get(CommitSeq(2)).is_none());
            if later_read {
                assert!(matches!(
                    result,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert_eq!(db.frontier().unwrap(), CommitSeq(4));
                assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
                assert_eq!(db.vertices().unwrap(), pin.vertices().unwrap());
            } else {
                assert_eq!(result.unwrap(), CommitSeq(5));
                assert_eq!(transaction.basis(), CommitSeq(4));
                let mut serial = Database::open_memory(&commit, keys(0x72)).await.unwrap();
                for batch in [seed, first, second, create(3), staged] {
                    serial.write(&commit, batch).await.unwrap();
                }
                assert_eq!(db.vertices().unwrap(), serial.vertices().unwrap());
                assert_eq!(db.edges().unwrap(), serial.edges().unwrap());
                drop(db);
                let reopened = Database::open_with_vfs(
                    &commit,
                    vfs.clone(),
                    &vfs.memory.database_dir(),
                    keys(0x72),
                )
                .await
                .unwrap();
                assert_eq!(reopened.frontier().unwrap(), CommitSeq(5));
                assert_eq!(reopened.vertices().unwrap(), serial.vertices().unwrap());
                assert_eq!(reopened.edges().unwrap(), serial.edges().unwrap());
            }
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

async fn restore_derived_checkpoint(
    database: &mut Database<ReplayVfs>,
    commit: &CommitCx,
    query: &QueryCx,
    ripple: bool,
) -> Result<DerivedCheckpointOutcome, DerivedCheckpointError> {
    if ripple {
        database
            .load_or_rebuild_standing_reachability_checkpoint(
                commit,
                query,
                "streaming-fault",
                RelationId(1),
                fgdb_gql::GqlQueryPolicy::new(0, 100, 1_000_000, 10_000),
            )
            .await
            .map(|(_, outcome)| outcome)
    } else {
        database
            .load_or_rebuild_beacon_checkpoint(
                commit,
                query,
                "streaming-fault",
                &fgdb_beacon::read::ReadOptions::text(PropertyKeyId(1)),
                fgdb_beacon::read::ReadPolicy::default(),
            )
            .await
            .map(|(_, outcome)| outcome)
    }
}

#[test]
fn derived_checkpoint_replay_drop_and_late_corruption_never_publish_partial_state() {
    let ((), report) = run_async_under_lab(0xd31a_1008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = ReplayVfs::new();
        let mut db = database(&commit, &vfs, 0x72).await;
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=2 {
            seed.create_vertex(
                VId(id),
                vec![],
                vec![(
                    PropertyKeyId(1),
                    CanonicalScalar::ucs_basic_text("checkpoint source").unwrap(),
                )],
            );
        }
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        let watermark = db.write(&commit, seed).await.unwrap();
        let beacon = db
            .register_beacon_index(
                &query,
                &fgdb_beacon::read::ReadOptions::text(PropertyKeyId(1)),
                fgdb_beacon::read::ReadPolicy::default(),
            )
            .unwrap();
        let ripple = db
            .register_standing_reachability(
                &query,
                RelationId(1),
                fgdb_gql::GqlQueryPolicy::new(100, 100, 1_000_000, 10_000),
            )
            .unwrap();
        db.save_beacon_checkpoint(&commit, &query, &beacon, "streaming-fault")
            .await
            .unwrap();
        db.save_standing_reachability_checkpoint(&commit, &query, &ripple, "streaming-fault")
            .await
            .unwrap();
        for id in 3..=4 {
            let mut suffix = WriteBatch::new(RelationId(1));
            suffix.create_vertex(
                VId(id),
                vec![],
                vec![(
                    PropertyKeyId(1),
                    CanonicalScalar::ucs_basic_text("checkpoint suffix").unwrap(),
                )],
            );
            suffix.add_edge(EId(id - 1), VId(id - 1), VId(id), vec![]);
            db.write(&commit, suffix).await.unwrap();
        }
        drop(db);

        for ripple in [false, true] {
            vfs.reset();
            let mut db = Database::open_with_vfs(
                &commit,
                vfs.clone(),
                &vfs.memory.database_dir(),
                keys(0x72),
            )
            .await
            .unwrap();
            assert_eq!(vfs.count(), 0);
            let snapshot = Arc::clone(&db.snapshot);
            let window = db.delta_index().unwrap().clone();
            let floor = db.delta_materialized_after;
            let head = db.frontier().unwrap();
            assert!(window.is_empty());

            // Capture a real, valid capsule that is wrong for the later tick.
            // The fault replaces an I/O observation, not authoritative storage.
            let mut cursor = db.delta_replay(watermark, head).unwrap();
            db.next_delta_replay(&commit, &mut cursor)
                .await
                .unwrap()
                .unwrap();
            *vfs.alternate.lock().unwrap() = Some(vfs.reads.lock().unwrap()[0].clone());

            vfs.reset();
            vfs.allowed_reads.store(1, Ordering::SeqCst);
            vfs.mode.store(3, Ordering::SeqCst);
            let mut pending =
                Box::pin(restore_derived_checkpoint(&mut db, &commit, &query, ripple));
            poll_fn(|cx| {
                assert!(pending.as_mut().poll(cx).is_pending());
                assert_eq!(vfs.count(), 2, "the first tick completed before suspension");
                Poll::Ready(())
            })
            .await;
            drop(pending);
            assert!(db.maintained_beacon.is_empty());
            assert!(db.standing_queries.is_empty());
            assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
            assert_eq!(db.delta_index().unwrap(), &window);
            assert_eq!(db.delta_materialized_after, floor);

            vfs.reset();
            vfs.allowed_reads.store(1, Ordering::SeqCst);
            vfs.mode.store(3, Ordering::SeqCst);
            let mut interrupted =
                Box::pin(restore_derived_checkpoint(&mut db, &commit, &query, ripple));
            poll_fn(|cx| {
                assert!(interrupted.as_mut().poll(cx).is_pending());
                assert_eq!(vfs.count(), 2);
                Poll::Ready(())
            })
            .await;
            root.set_cancel_requested(true);
            vfs.mode.store(0, Ordering::SeqCst);
            assert!(matches!(
                interrupted.await,
                Err(DerivedCheckpointError::Interrupted(_))
            ));
            assert_eq!(vfs.count(), 2);
            root.set_cancel_requested(false);
            assert!(db.maintained_beacon.is_empty());
            assert!(db.standing_queries.is_empty());
            assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
            assert_eq!(db.delta_index().unwrap(), &window);
            assert_eq!(db.delta_materialized_after, floor);

            vfs.reset();
            vfs.allowed_reads.store(1, Ordering::SeqCst);
            vfs.mode.store(2, Ordering::SeqCst);
            assert!(matches!(
                restore_derived_checkpoint(&mut db, &commit, &query, ripple).await,
                Err(DerivedCheckpointError::Replay(_))
            ));
            assert_eq!(vfs.count(), 2);
            assert!(db.maintained_beacon.is_empty());
            assert!(db.standing_queries.is_empty());
            assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
            assert_eq!(db.delta_index().unwrap(), &window);
            assert_eq!(db.delta_materialized_after, floor);

            vfs.reset();
            assert!(matches!(
                restore_derived_checkpoint(&mut db, &commit, &query, ripple)
                    .await
                    .unwrap(),
                DerivedCheckpointOutcome::CatchUp {
                    watermark_seq,
                    head_seq,
                    commits_replayed: 2,
                    ..
                } if watermark_seq == watermark && head_seq == head
            ));
            assert_eq!(vfs.count(), 2);
            assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
            assert_eq!(db.delta_index().unwrap(), &window);
            assert_eq!(db.delta_materialized_after, floor);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn checkpoint_replay_reads_one_batch_per_pull_without_hydrating_or_extending_its_range() {
    let ((), report) = run_async_under_lab(0xd31a_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let vfs = ReplayVfs::new();
        let mut db = database(&cx, &vfs, 0x72).await;
        db.write(&cx, create(1)).await.unwrap();
        let mut noop = WriteBatch::new(RelationId(1));
        noop.ensure_vertex(VId(1), vec![], vec![]);
        db.write(&cx, noop).await.unwrap();
        db.write(&cx, create(2)).await.unwrap();
        db.write(&cx, create(3)).await.unwrap();
        let expected: Vec<_> = db
            .delta_since(CommitSeq::ORIGIN)
            .unwrap()
            .map(|batch| batch.canonical_bytes().unwrap())
            .collect();
        assert_eq!(expected.len(), 4);
        let mut cursor = db.delta_replay(CommitSeq::ORIGIN, CommitSeq(4)).unwrap();
        let mut duplicate = cursor.clone();
        assert_eq!(cursor.from(), CommitSeq::ORIGIN);
        assert_eq!(cursor.position_marker(), None);
        drop(db);
        vfs.reset();
        let mut db =
            Database::open_with_vfs(&cx, vfs.clone(), &vfs.memory.database_dir(), keys(0x72))
                .await
                .unwrap();
        assert_eq!(vfs.count(), 0);
        assert_eq!(db.delta_materialized_after, CommitSeq(4));
        assert!(db.delta_index().unwrap().is_empty());
        // A later durable commit is visible to ordinary readers, but not part
        // of the replay interval admitted before reopening.
        db.write(&cx, create(4)).await.unwrap();
        vfs.reset();
        let snapshot = Arc::clone(&db.snapshot);
        let window = db.delta_index().unwrap().clone();
        let evidence_before = db.crypto_verification_events().len();
        for (offset, bytes) in expected.iter().enumerate() {
            let batch = db
                .next_delta_replay(&cx, &mut cursor)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(batch.canonical_bytes().unwrap(), *bytes);
            assert_eq!(batch.commit_seq(), CommitSeq(offset as u64 + 1));
            assert_eq!(cursor.position(), batch.commit_seq());
            assert_eq!(
                cursor.position_marker(),
                Some(batch.commit_marker_identity())
            );
            assert_eq!(vfs.count(), offset + 1);
            if offset == 1 {
                assert!(
                    batch
                        .coordinate_entries()
                        .iter()
                        .all(|entry| entry.rows.is_empty())
                );
            }
            assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
            assert_eq!(db.delta_index().unwrap(), &window);
            assert_eq!(db.delta_materialized_after, CommitSeq(4));
        }
        assert_eq!(cursor.through(), CommitSeq(4));
        assert!(cursor.is_exhausted());
        assert!(
            db.next_delta_replay(&cx, &mut cursor)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(vfs.count(), 4);
        assert!(db.crypto_verification_events().len() > evidence_before);
        let repeated = db
            .next_delta_replay(&cx, &mut duplicate)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repeated.canonical_bytes().unwrap(), expected[0]);
        assert_eq!(vfs.count(), 5);
        assert_eq!(db.frontier().unwrap(), CommitSeq(5));
        assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
        assert_eq!(db.delta_index().unwrap(), &window);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn replay_refuses_future_foreign_substituted_and_newly_retired_sources_before_io() {
    let ((), report) = run_async_under_lab(0xd31a_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let vfs = ReplayVfs::new();
        let mut db = database(&cx, &vfs, 0x72).await;
        db.write(&cx, create(1)).await.unwrap();
        db.write(&cx, create(2)).await.unwrap();
        vfs.reset();
        assert!(matches!(
            db.delta_replay(CommitSeq(2), CommitSeq(1)),
            Err(DeltaReplayError::InvalidRange { .. })
        ));
        assert!(matches!(
            db.delta_replay(CommitSeq::ORIGIN, CommitSeq(3)),
            Err(DeltaReplayError::Read(ReadError::BeyondFrontier { .. }))
        ));
        let mut empty = db
            .delta_replay(CommitSeq::ORIGIN, CommitSeq::ORIGIN)
            .unwrap();
        assert!(empty.is_exhausted());
        assert!(
            db.next_delta_replay(&cx, &mut empty)
                .await
                .unwrap()
                .is_none()
        );
        let original = db.delta_replay(CommitSeq::ORIGIN, CommitSeq(2)).unwrap();
        for namespace in [0x75, 0x72] {
            let other_vfs = ReplayVfs::new();
            let mut other = database(&cx, &other_vfs, namespace).await;
            other.write(&cx, create(90)).await.unwrap();
            other.write(&cx, create(91)).await.unwrap();
            other_vfs.reset();
            let mut cursor = original.clone();
            let error = other.next_delta_replay(&cx, &mut cursor).await.unwrap_err();
            if namespace == 0x72 {
                assert!(matches!(
                    error,
                    DeltaReplayError::HistoryChanged { at: CommitSeq(2) }
                ));
            } else {
                assert!(matches!(error, DeltaReplayError::ForeignSource));
            }
            assert_eq!(cursor, original);
            assert_eq!(other_vfs.count(), 0);
        }
        let mut changed_position = db.delta_replay(CommitSeq(1), CommitSeq(2)).unwrap();
        changed_position.position.chain_hash = Digest([0x55; 32]);
        let unchanged = changed_position.clone();
        assert!(matches!(
            db.next_delta_replay(&cx, &mut changed_position).await,
            Err(DeltaReplayError::HistoryChanged { at: CommitSeq(1) })
        ));
        assert_eq!(changed_position, unchanged);
        let mut retired = original.clone();
        let mut boundary = db.delta_replay(CommitSeq(1), CommitSeq(2)).unwrap();
        Arc::make_mut(&mut db.snapshot)
            .delta_index
            .retire_prefix(CommitSeq(1))
            .unwrap();
        assert!(matches!(
            db.next_delta_replay(&cx, &mut retired).await,
            Err(DeltaReplayError::Read(ReadError::DeltaCursorRetired { .. }))
        ));
        assert_eq!(retired, original);
        assert!(matches!(
            db.delta_replay(CommitSeq::ORIGIN, CommitSeq(2)),
            Err(DeltaReplayError::Read(ReadError::DeltaCursorRetired { .. }))
        ));
        assert_eq!(vfs.count(), 0);
        let batch = db
            .next_delta_replay(&cx, &mut boundary)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(batch.commit_seq(), CommitSeq(2));
        assert_eq!(vfs.count(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn replay_faults_and_dropped_pending_pulls_preserve_position_and_native_snapshot() {
    let ((), report) = run_async_under_lab(0xd31a_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let vfs = ReplayVfs::new();
        let mut db = database(&cx, &vfs, 0x72).await;
        db.write(&cx, create(1)).await.unwrap();
        db.write(&cx, create(2)).await.unwrap();
        drop(db);
        let mut db =
            Database::open_with_vfs(&cx, vfs.clone(), &vfs.memory.database_dir(), keys(0x72))
                .await
                .unwrap();
        vfs.reset();
        let mut probe = db.delta_replay(CommitSeq::ORIGIN, CommitSeq(2)).unwrap();
        let expected = db
            .next_delta_replay(&cx, &mut probe)
            .await
            .unwrap()
            .unwrap();
        db.next_delta_replay(&cx, &mut probe)
            .await
            .unwrap()
            .unwrap();
        let alternate = vfs.reads.lock().unwrap()[1].clone();
        *vfs.alternate.lock().unwrap() = Some(alternate);
        let mut cursor = db.delta_replay(CommitSeq::ORIGIN, CommitSeq(2)).unwrap();
        let before = cursor.clone();
        let snapshot = Arc::clone(&db.snapshot);
        let evidence_before = db.crypto_verification_events().len();
        for mode in [1, 4, 2] {
            vfs.reset();
            vfs.mode.store(mode, Ordering::SeqCst);
            let error = db.next_delta_replay(&cx, &mut cursor).await.unwrap_err();
            match error {
                DeltaReplayError::Reconstruction(source) => match (mode, *source) {
                    (1, RebuildError::Commit(fgdb_chronicle::CommitError::Io(_)))
                    | (4, RebuildError::MissingCapsule { commit_seq: 1, .. })
                    | (2, RebuildError::Commit(fgdb_chronicle::CommitError::Capsule(_))) => {}
                    other => panic!("unexpected replay fault: {other:?}"),
                },
                other => panic!("expected typed reconstruction refusal: {other:?}"),
            }
            assert_eq!(cursor, before);
            assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
            assert!(db.delta_index().unwrap().is_empty());
            assert_eq!(db.delta_materialized_after, CommitSeq(2));
            assert_eq!(vfs.count(), usize::from(mode != 4));
        }
        assert!(
            db.crypto_verification_events().len() > evidence_before,
            "failed verification still retains its diagnostic evidence"
        );
        vfs.reset();
        vfs.mode.store(3, Ordering::SeqCst);
        let mut pending = db.next_delta_replay(&cx, &mut cursor);
        poll_fn(|context| {
            assert!(pending.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(pending);
        assert_eq!(cursor, before);
        assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
        assert_eq!(vfs.count(), 1);
        vfs.reset();
        let batch = db
            .next_delta_replay(&cx, &mut cursor)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            batch.canonical_bytes().unwrap(),
            expected.canonical_bytes().unwrap()
        );
        assert_eq!(cursor.position(), CommitSeq(1));
        assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
        assert!(db.delta_index().unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn replay_checks_cancellation_and_health_without_advancing_or_reading_capsules() {
    let ((), report) = run_async_under_lab(0xd31a_1004, |root| async move {
        let mut child = root
            .spawn(move |child| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&child);
                let cx = contexts.commit();
                let vfs = ReplayVfs::new();
                let mut db = database(&cx, &vfs, 0x72).await;
                db.write(&cx, create(1)).await.unwrap();
                let mut cursor = db.delta_replay(CommitSeq::ORIGIN, CommitSeq(1)).unwrap();
                let before = cursor.clone();
                let snapshot = Arc::clone(&db.snapshot);
                vfs.reset();
                child
                    .set_cancel_reason(asupersync::CancelReason::new(asupersync::CancelKind::User));
                assert!(matches!(
                    db.next_delta_replay(&cx, &mut cursor).await,
                    Err(DeltaReplayError::Interrupted(_))
                ));
                assert_eq!(cursor, before);
                assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
                assert_eq!(vfs.count(), 0);
            })
            .unwrap();
        assert_eq!(child.join(&root).await, Ok(()));

        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let vfs = ReplayVfs::new();
        let mut db = database(&cx, &vfs, 0x72).await;
        db.write(&cx, create(1)).await.unwrap();
        let mut cursor = db.delta_replay(CommitSeq(1), CommitSeq(1)).unwrap();
        let before = cursor.clone();
        db.write_with_publication_failure(
            &cx,
            create(2),
            crate::DerivedPublicationStage::FoldCommittedTemplate,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            db.state(),
            DatabaseState::NeedsAuthoritativeRecovery(_)
        ));
        vfs.reset();
        assert!(matches!(
            db.next_delta_replay(&cx, &mut cursor).await,
            Err(DeltaReplayError::Read(ReadError::RecoveryRequired(_)))
        ));
        assert!(matches!(
            db.delta_replay(CommitSeq::ORIGIN, CommitSeq(1)),
            Err(DeltaReplayError::Read(ReadError::RecoveryRequired(_)))
        ));
        assert_eq!(cursor, before);
        assert_eq!(vfs.count(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn replay_cut_admission_names_gaps_and_final_sequence_arithmetic_never_wraps() {
    assert!(matches!(
        Cut::capture(&MarkerChain::new(), CommitSeq::FIRST),
        Err(DeltaReplayError::Gap {
            expected: CommitSeq::FIRST
        })
    ));
    let origin = Cut::capture(&MarkerChain::new(), CommitSeq::ORIGIN).unwrap();
    let identity = crate::spine_slot(
        &keys(0x72),
        0,
        crate::ManifestVersion(fgdb_types::ObjectId([0; 32])),
        0,
    )
    .identity_tuple();
    let mut cursor = DeltaReplayCursor {
        source: identity,
        from: CommitSeq::ORIGIN,
        position: Cut {
            sequence: CommitSeq(u64::MAX),
            ..origin
        },
        through: Cut {
            sequence: CommitSeq(u64::MAX),
            ..origin
        },
    };
    assert_eq!(cursor.next_sequence().unwrap(), None);
    cursor.position.sequence = CommitSeq(u64::MAX - 1);
    assert_eq!(cursor.next_sequence().unwrap(), Some(CommitSeq(u64::MAX)));
    cursor.position.sequence = CommitSeq(u64::MAX);
    cursor.through.sequence = CommitSeq(u64::MAX - 1);
    assert!(matches!(
        cursor.next_sequence(),
        Err(DeltaReplayError::InvalidRange { .. })
    ));
    let debug = format!("{cursor:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("chain_hash"));
    assert!(!debug.contains("database_security_namespace_id"));
}
