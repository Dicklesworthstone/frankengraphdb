use super::*;
use crate::{DatabaseKeys, DatabaseState, MemVfs, MemVfsFile, WriteBatch};
use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::future::{Future, poll_fn};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

/// Observe only the real capsule I/O boundary. Faults alter read observations,
/// never the authoritative bytes or the graph's publication machinery.
#[derive(Clone, Debug)]
struct ReplayVfs {
    memory: MemVfs,
    reads: Arc<Mutex<Vec<PathBuf>>>,
    mode: Arc<AtomicU8>,
    alternate: Arc<Mutex<Option<PathBuf>>>,
}

impl ReplayVfs {
    fn new() -> Self {
        Self {
            memory: MemVfs::new().unwrap(),
            reads: Arc::default(),
            mode: Arc::new(AtomicU8::new(0)),
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
            match self.mode.load(Ordering::SeqCst) {
                1 => return Err(io::Error::other("injected replay read failure")),
                2 => {
                    let alternate = self.alternate.lock().unwrap().clone().unwrap();
                    return self.memory.open_read(&alternate).await;
                }
                3 => return std::future::pending().await,
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
