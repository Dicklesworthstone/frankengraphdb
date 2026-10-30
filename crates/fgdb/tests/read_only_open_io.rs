//! What a read-only open does to the filesystem (fgdb-xxxnb).
//!
//! The resident read view's fast path and the buffered read view take no
//! writer lease and write nothing, so they read beside a live writer. These
//! laws record every filesystem call that is not a pure read: none may happen
//! on those paths. A read-only open that must heal a lagging root reads the
//! partition once, through the full open that heals it.

use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir, Vfs};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedOpenError, BufferedReadLimits, Database, DatabaseKeys,
    DerivedPublicationStage, MemVfs, MemVfsFile, MemoryPool, OpenError, SealedProjectionOpenError,
    WriteBatch, WriteError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadOptions, FnxResult,
    FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy, SealedGraphView,
};
use fgdb_strata::{store::RootReadLimits, tiered::sealed::SealedLimits};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const PROPERTY: PropertyKeyId = PropertyKeyId(1);
const RELATION: RelationId = RelationId(3);

/// Records every call that can create, write, sync, lock, move or remove
/// anything: `open` (the only way to get a writable or syncable handle, a
/// directory's included) and every namespace mutation. Pure reads pass
/// through unrecorded, except that `open_read` of a Strata object is counted.
#[derive(Clone, Debug)]
struct RecordingVfs {
    memory: MemVfs,
    writes: Arc<Mutex<Vec<String>>>,
    object_reads: Arc<Mutex<Vec<PathBuf>>>,
}

impl RecordingVfs {
    fn new() -> Self {
        Self {
            memory: MemVfs::new().unwrap(),
            writes: Arc::new(Mutex::new(Vec::new())),
            object_reads: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn record(&self, op: &str, path: &Path) {
        self.writes
            .lock()
            .unwrap()
            .push(format!("{op} {}", path.display()));
    }
    fn take_writes(&self) -> Vec<String> {
        std::mem::take(&mut *self.writes.lock().unwrap())
    }
    fn take_object_reads(&self) -> usize {
        self.take_object_paths().len()
    }
    fn take_object_paths(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.object_reads.lock().unwrap())
    }
}

impl Vfs for RecordingVfs {
    type File = MemVfsFile;

    async fn open(&self, path: &Path, options: &OpenOptions) -> io::Result<Self::File> {
        self.record("open", path);
        self.memory.open(path, options).await
    }
    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        let object = path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == fgdb_strata::store::BLOCK_DIR);
        if object {
            self.object_reads.lock().unwrap().push(path.to_path_buf());
        }
        self.memory.open_read(path).await
    }
    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.memory.metadata(path).await
    }
    async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.memory.symlink_metadata(path).await
    }
    async fn set_permissions(&self, path: &Path, permissions: Permissions) -> io::Result<()> {
        self.record("set_permissions", path);
        self.memory.set_permissions(path, permissions).await
    }
    async fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.record("create_dir", path);
        self.memory.create_dir(path).await
    }
    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.record("create_dir_all", path);
        self.memory.create_dir_all(path).await
    }
    async fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.record("remove_dir", path);
        self.memory.remove_dir(path).await
    }
    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.record("remove_file", path);
        self.memory.remove_file(path).await
    }
    async fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        self.memory.read_dir(path).await
    }
    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.record("remove_dir_all", path);
        self.memory.remove_dir_all(path).await
    }
    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.record("rename", from);
        self.memory.rename(from, to).await
    }
    async fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        self.record("copy", to);
        self.memory.copy(from, to).await
    }
    async fn hard_link(&self, original: &Path, link: &Path) -> io::Result<()> {
        self.record("hard_link", link);
        self.memory.hard_link(original, link).await
    }
    async fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.memory.canonicalize(path).await
    }
    async fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.memory.read_link(path).await
    }
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.memory.read(path).await
    }
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.memory.read_to_string(path).await
    }
    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.record("write", path);
        self.memory.write(path, bytes).await
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}

fn buffered_limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 64 * 1024,
        max_source_bytes: 4 * 1024 * 1024,
        max_blocks: 512,
        max_vertex_patches: 512,
        max_work: 100_000,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}

/// Commit `i`: vertex `i` with a property, and an edge from vertex 1 to it.
fn spoke(i: u64) -> WriteBatch {
    let mut batch = WriteBatch::new(RELATION);
    batch.create_vertex(
        VId(u128::from(i)),
        vec![],
        vec![(PROPERTY, CanonicalScalar::Int(i as i64))],
    );
    if i > 1 {
        batch.add_edge(
            EId(u128::from(1000 + i)),
            VId(1),
            VId(u128::from(i)),
            vec![],
        );
    }
    batch
}

/// A database over `vfs` with `commits` committed spokes, every one published.
async fn spokes(commit: &CommitCx, vfs: &RecordingVfs, commits: u64) -> Database<RecordingVfs> {
    let path = vfs.memory.database_dir();
    let mut db = Database::create_with_vfs(commit, vfs.clone(), &path, keys())
        .await
        .unwrap();
    for i in 1..=commits {
        db.write(commit, spoke(i)).await.unwrap();
    }
    db
}

#[test]
fn read_only_opens_beside_a_live_writer_create_write_or_sync_nothing() {
    let ((), report) = run_async_under_lab(0x7669_6601, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let vfs = RecordingVfs::new();
        let path = vfs.memory.database_dir();
        // The writer stays open, holding the lease, for the whole law.
        let writer = spokes(&commit, &vfs, 6).await;
        let frontier = writer.frontier().unwrap();
        vfs.take_writes();

        let view = Database::open_read_view_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        assert_eq!(view.frontier(), frontier);
        assert_eq!(
            vfs.take_writes(),
            Vec::<String>::new(),
            "resident read view"
        );

        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let buffered = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            &path,
            keys(),
            pool,
            buffered_limits(),
        )
        .await
        .unwrap();
        assert_eq!(buffered.frontier(), frontier);
        assert_eq!(
            vfs.take_writes(),
            Vec::<String>::new(),
            "buffered read view"
        );
        drop(writer);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn read_only_prefix_selection_refuses_future_and_foreign_checkpoint_histories() {
    let ((), report) = run_async_under_lab(0x7669_6630, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let vfs = RecordingVfs::new();
        let path = vfs.memory.database_dir();
        let writer = spokes(&commit, &vfs, 2).await;
        let frontier = writer.frontier().unwrap();
        drop(writer);
        let log_path = path.join(fgdb_chronicle::commit::COMMIT_LOG_NAME);
        let original = vfs.memory.read(&log_path).await.unwrap();
        let first_body_len = u32::from_be_bytes(original[4..8].try_into().unwrap()) as usize;
        let first_frame_len = 8 + first_body_len + 32 + 8;

        // This is another authentic history under the SAME namespace and keys,
        // at the SAME frontier. Object identity and sequence alone cannot bind
        // our checkpoint to its chain.
        let foreign_vfs = RecordingVfs::new();
        let foreign_path = foreign_vfs.memory.database_dir();
        let mut foreign =
            Database::create_with_vfs(&commit, foreign_vfs.clone(), &foreign_path, keys())
                .await
                .unwrap();
        for id in [999, 1000] {
            let mut batch = WriteBatch::new(RELATION);
            batch.create_vertex(VId(id), vec![], vec![]);
            foreign.write(&commit, batch).await.unwrap();
        }
        assert_eq!(foreign.frontier().unwrap(), frontier);
        drop(foreign);
        let foreign_log = foreign_vfs
            .memory
            .read(&foreign_path.join(fgdb_chronicle::commit::COMMIT_LOG_NAME))
            .await
            .unwrap();
        assert_ne!(foreign_log, original);

        for (case, log) in [
            ("future checkpoint", &original[..first_frame_len]),
            ("foreign history", foreign_log.as_slice()),
        ] {
            vfs.memory.write(&log_path, log).await.unwrap();
            vfs.take_writes();
            assert!(
                matches!(
                    Database::open_read_view_with_vfs(&commit, vfs.clone(), &path, keys()).await,
                    Err(OpenError::SlotDisagreesWithStream { .. })
                ),
                "resident selector: {case}"
            );
            let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
            assert!(
                matches!(
                    Database::open_buffered_read_view_with_vfs(
                        &commit,
                        vfs.clone(),
                        &path,
                        keys(),
                        pool.clone(),
                        buffered_limits(),
                    )
                    .await,
                    Err(BufferedOpenError::Open(
                        OpenError::SlotDisagreesWithStream { .. }
                    ))
                ),
                "buffered selector: {case}"
            );
            assert_eq!(pool.used(), 0, "refused selection releases metadata");
            assert!(vfs.take_writes().is_empty(), "{case} must not heal");
            assert_eq!(vfs.memory.read(&log_path).await.unwrap(), log);
        }

        vfs.memory.write(&log_path, &original).await.unwrap();
        let resident = Database::open_read_view_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let buffered = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            &path,
            keys(),
            MemoryPool::new(16 * 1024 * 1024, 0).unwrap(),
            buffered_limits(),
        )
        .await
        .unwrap();
        assert_eq!(resident.frontier(), frontier);
        assert_eq!(buffered.frontier(), frontier);
        assert!(vfs.take_writes().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn read_only_prefix_selection_checks_complete_entries_after_a_lagging_root() {
    let ((), report) = run_async_under_lab(0x7669_6631, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let vfs = RecordingVfs::new();
        let path = vfs.memory.database_dir();
        let mut writer = spokes(&commit, &vfs, 1).await;
        let slot_path = path.join("manifest.root");
        let old_slot = vfs.memory.read(&slot_path).await.unwrap();
        let log_path = path.join(fgdb_chronicle::commit::COMMIT_LOG_NAME);
        let first_len = vfs.memory.read(&log_path).await.unwrap().len();
        writer.write(&commit, spoke(2)).await.unwrap();
        writer.write(&commit, spoke(3)).await.unwrap();
        drop(writer);
        vfs.memory.write(&slot_path, &old_slot).await.unwrap();

        // The selected cut (1) is fully valid, but the second complete frame's
        // stored commitment is damaged. Returning as soon as cut 1 was found
        // would incorrectly admit it or enter a healing path.
        let mut log = vfs.memory.read(&log_path).await.unwrap();
        let second_body_len =
            u32::from_be_bytes(log[first_len + 4..first_len + 8].try_into().unwrap()) as usize;
        log[first_len + 8 + second_body_len] ^= 1;
        vfs.memory.write(&log_path, &log).await.unwrap();
        vfs.take_writes();
        assert!(matches!(
            Database::open_read_view_with_vfs(&commit, vfs.clone(), &path, keys()).await,
            Err(OpenError::Commit(
                fgdb_chronicle::CommitError::ChainDiverged { commit_seq: 2 }
            ))
        ));
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        assert!(matches!(
            Database::open_buffered_read_view_with_vfs(
                &commit,
                vfs.clone(),
                &path,
                keys(),
                pool.clone(),
                buffered_limits(),
            )
            .await,
            Err(BufferedOpenError::Open(OpenError::Commit(
                fgdb_chronicle::CommitError::ChainDiverged { commit_seq: 2 }
            )))
        ));
        assert_eq!(pool.used(), 0);
        assert!(vfs.take_writes().is_empty());
        assert_eq!(vfs.memory.read(&log_path).await.unwrap(), log);
        assert_eq!(vfs.memory.read(&slot_path).await.unwrap(), old_slot);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn live_admission_buffered_open_reads_descriptors_and_faults_only_selected_payloads() {
    let ((), report) = run_async_under_lab(0x7669_6620, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = RecordingVfs::new();
        let path = vfs.memory.database_dir();
        let mut writer = spokes(&commit, &vfs, 6).await;
        let root_id = writer.read_session().unwrap().partition_root();
        let store = fgdb_strata::store::BlockStore::open_existing_for_read_with_vfs(
            &commit,
            vfs.clone(),
            &path,
            [0xc1; 32],
            keys().namespace,
        )
        .await
        .unwrap();
        let membership = store.get_root(&commit, root_id).await.unwrap();
        let payloads: Vec<_> = membership
            .blocks
            .iter()
            .map(|entry| store.path(entry.block_id))
            .chain(
                membership
                    .vertex_patches
                    .iter()
                    .map(|entry| store.path(entry.patch_id)),
            )
            .collect();
        assert_eq!(
            payloads.len(),
            11,
            "five edge blocks and six vertex patches"
        );
        let selected_patch = membership
            .vertex_patches
            .iter()
            .find(|entry| entry.first_seq == CommitSeq(6))
            .unwrap();
        let selected_block = membership
            .blocks
            .iter()
            .find(|entry| entry.first_seq == CommitSeq(6))
            .unwrap();
        vfs.take_object_paths();
        vfs.take_writes();

        // Zero graph-admission bytes are available. Real VFS reads, not a
        // logical work counter, must show that open visits only root metadata.
        let mut limits = buffered_limits();
        limits.max_source_bytes = 0;
        let mut view = writer
            .buffered_read_view(
                &query,
                MemoryPool::new(16 * 1024 * 1024, 0).unwrap(),
                limits,
            )
            .await
            .unwrap();
        let opened = vfs.take_object_paths();
        assert!(
            !opened.is_empty(),
            "the selected immutable root is read cold"
        );
        assert!(
            opened.iter().all(|path| !payloads.contains(path)),
            "payload reads at open: {opened:?}"
        );
        assert!(vfs.take_writes().is_empty());

        assert!(
            view.vertex_at(&query, VId(6), CommitSeq(2))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            vfs.take_object_paths().is_empty(),
            "the admitted sequence fence prunes future rows"
        );
        let vertex = view.vertex(&query, VId(6)).await.unwrap().unwrap();
        assert_eq!(vertex.vid, VId(6));
        assert_eq!(
            vertex.props,
            vec![(PROPERTY, CanonicalScalar::Int(6).into())]
        );
        assert_eq!(
            vfs.take_object_paths(),
            vec![store.path(selected_patch.patch_id)]
        );
        drop(vertex);
        let edge = view.edge(&query, EId(1006)).await.unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.dst), (VId(1), VId(6)));
        assert_eq!(
            vfs.take_object_paths(),
            vec![store.path(selected_block.block_id)]
        );
        assert!(vfs.take_writes().is_empty());

        // Possession of a keyed root ID, including this genuine current ID,
        // does not mint published-source authority for the generic API.
        let error = store
            .open_buffered_root(
                &query,
                root_id,
                MemoryPool::new(16 * 1024 * 1024, 0).unwrap(),
                limits,
            )
            .await
            .err()
            .expect("an arbitrary root must earn full admission");
        assert!(
            matches!(
                error,
                fgdb::BufferedReadError::Limit {
                    resource: "buffered source bytes",
                    ..
                }
            ),
            "{error:?}"
        );

        // A raw slot selected by a fresh native opener carries no full
        // history proof. The cold path retains its existing admission law.
        let cold = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            &path,
            keys(),
            MemoryPool::new(16 * 1024 * 1024, 0).unwrap(),
            limits,
        )
        .await
        .expect_err("a cold source still proves the complete history");
        assert!(
            matches!(
                cold,
                fgdb::BufferedOpenError::Read(fgdb::BufferedReadError::Limit {
                    resource: "buffered source bytes",
                    ..
                })
            ),
            "{cold:?}"
        );
        let admission = store
            .reopen_sealed(&commit, root_id)
            .await
            .unwrap()
            .admission;
        let receipts = fgdb_strata::store::PublishReceipts::for_published_root(admission);
        writer.write(&commit, spoke(7)).await.unwrap();
        let advanced = writer.partition_root().unwrap();
        let stale = store
            .open_buffered_root_with_receipts(
                &query,
                advanced,
                &receipts,
                MemoryPool::new(16 * 1024 * 1024, 0).unwrap(),
                limits,
            )
            .await
            .err()
            .expect("an older complete admission cannot authorize a different root");
        assert!(
            matches!(
                stale,
                fgdb::BufferedReadError::Limit {
                    resource: "buffered source bytes",
                    ..
                }
            ),
            "{stale:?}"
        );
        drop(writer);
        assert_eq!(view.frontier(), CommitSeq(6));
        assert!(
            view.vertex(&query, VId(7)).await.unwrap().is_none(),
            "the owned source keeps its exact generation after writer advance and drop"
        );

        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut no_work = limits;
        no_work.max_work = 0;
        vfs.take_object_paths();
        assert!(store.open_buffered_root_with_receipts(
            &query, root_id, &receipts, pool.clone(), no_work,
        ).await.is_err(), "reused metadata still obeys source work limits");
        assert_eq!(
            pool.used(),
            0,
            "a refused source releases every reservation"
        );
        assert!(
            vfs.take_object_paths()
                .iter()
                .all(|path| !payloads.contains(path))
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn live_admission_descriptor_open_defers_faults_but_never_serves_corruption() {
    let ((), report) = run_async_under_lab(0x7669_6621, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = RecordingVfs::new();
        let path = vfs.memory.database_dir();
        let writer = spokes(&commit, &vfs, 6).await;
        let root_id = writer.read_session().unwrap().partition_root();
        let store = fgdb_strata::store::BlockStore::open_existing_for_read_with_vfs(
            &commit,
            vfs.clone(),
            &path,
            [0xc1; 32],
            keys().namespace,
        )
        .await
        .unwrap();
        let membership = store.get_root(&commit, root_id).await.unwrap();
        let damaged = membership
            .vertex_patches
            .iter()
            .find(|entry| entry.first_seq == CommitSeq(6))
            .unwrap();
        let damaged_path = store.path(damaged.patch_id);
        let original = vfs.memory.read(&damaged_path).await.unwrap();
        let mut corrupted = original.clone();
        let last = corrupted.last_mut().unwrap();
        *last ^= 0x40;
        vfs.memory.write(&damaged_path, &corrupted).await.unwrap();
        vfs.take_writes();
        vfs.take_object_paths();

        let mut limits = buffered_limits();
        limits.max_source_bytes = 0;
        let mut view = writer
            .buffered_read_view(
                &query,
                MemoryPool::new(16 * 1024 * 1024, 0).unwrap(),
                limits,
            )
            .await
            .unwrap();
        assert!(!vfs.take_object_paths().contains(&damaged_path));
        assert_eq!(
            view.vertex(&query, VId(1)).await.unwrap().unwrap().vid,
            VId(1)
        );
        assert!(
            matches!(
                view.vertex(&query, VId(6)).await,
                Err(fgdb::BufferedReadError::Buffer(
                    fgdb_strata::tiered::buffer::BufferError::ChecksumMismatch
                ))
            ),
            "the first fault must authenticate payload bytes"
        );
        assert!(
            vfs.take_writes().is_empty(),
            "read faults never repair storage"
        );
        assert!(
            store.reopen_sealed(&commit, root_id).await.is_err(),
            "full recovery admission still visits and rejects the damaged extent"
        );
        assert!(
            Database::open_buffered_read_view_with_vfs(
                &commit,
                vfs.clone(),
                &path,
                keys(),
                MemoryPool::new(16 * 1024 * 1024, 0).unwrap(),
                buffered_limits(),
            )
            .await
            .is_err(),
            "fresh native opens keep complete history/payload admission"
        );

        vfs.memory.write(&damaged_path, &original).await.unwrap();
        assert!(
            store.reopen_sealed(&commit, root_id).await.is_ok(),
            "restoring the exact immutable bytes restores complete admission"
        );
        drop(writer);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_recovery_fenced_writer_cannot_lend_an_old_admission_as_current() {
    let ((), report) = run_async_under_lab(0x7669_6622, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = RecordingVfs::new();
        let mut writer = spokes(&commit, &vfs, 1).await;
        let mut limits = buffered_limits();
        limits.max_source_bytes = 0;
        let mut pinned = writer
            .buffered_read_view(
                &query,
                MemoryPool::new(16 * 1024 * 1024, 0).unwrap(),
                limits,
            )
            .await
            .unwrap();
        let error = writer
            .write_with_publication_failure(
                &commit,
                spoke(2),
                DerivedPublicationStage::FoldCommittedTemplate,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, WriteError::CommittedNeedsRecovery { .. }));
        vfs.take_object_paths();
        vfs.take_writes();
        assert!(matches!(
            writer
                .buffered_read_view(
                    &query,
                    MemoryPool::new(16 * 1024 * 1024, 0).unwrap(),
                    limits
                )
                .await,
            Err(fgdb::BufferedOpenError::RecoveryRequired)
        ));
        assert!(
            vfs.take_object_paths().is_empty(),
            "the health fence precedes I/O"
        );
        assert!(vfs.take_writes().is_empty());
        assert_eq!(pinned.frontier(), CommitSeq(1));
        assert_eq!(
            pinned.vertex(&query, VId(1)).await.unwrap().unwrap().vid,
            VId(1)
        );
        assert!(pinned.vertex(&query, VId(2)).await.unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A crash left the last commit durable with its root unpublished, and no
/// writer is live, so the read-only open heals through a full open. It reads
/// the partition once: the published root it selected is not decoded too.
#[test]
fn a_read_only_open_that_heals_reads_the_partition_once() {
    let ((), report) = run_async_under_lab(0x7669_6602, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let mut reads = Vec::new();
        for read_only in [false, true] {
            let vfs = RecordingVfs::new();
            let path = vfs.memory.database_dir();
            let mut db = spokes(&commit, &vfs, 24).await;
            let published = db.frontier().unwrap();
            let error = db
                .write_with_publication_failure(
                    &commit,
                    spoke(25),
                    DerivedPublicationStage::PublishPartitionRoot,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, WriteError::CommittedNeedsRecovery { .. }));
            drop(db);
            vfs.take_object_reads();
            let frontier = if read_only {
                Database::open_read_view_with_vfs(&commit, vfs.clone(), &path, keys())
                    .await
                    .unwrap()
                    .frontier()
            } else {
                Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                    .await
                    .unwrap()
                    .frontier()
                    .unwrap()
            };
            assert_eq!(
                frontier.0,
                published.0 + 1,
                "the healed generation includes spoke 25"
            );
            reads.push(vfs.take_object_reads());
        }
        let [full, read_only] = reads[..] else {
            unreachable!()
        };
        // The read-only open first reads the slot's manifest and root to find
        // the lag; everything else is the full open's own single pass.
        assert!(
            full >= 40 && read_only <= full + 4,
            "object reads: full open {full}, healing read-only open {read_only}"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A create interrupted after its commit stream existed but before its block
/// directory did: the read-only open heals it like a full open would.
#[test]
fn a_read_only_open_heals_a_create_interrupted_before_its_block_directory() {
    let ((), report) = run_async_under_lab(0x7669_6603, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let vfs = RecordingVfs::new();
        let path = vfs.memory.database_dir();
        vfs.memory.create_dir_all(&path).await.unwrap();
        let coordinator = fgdb_chronicle::commit::CommitCoordinator::open_with_vfs(
            &commit,
            vfs.memory.clone(),
            &path,
            keys().capsule_keys(),
        )
        .await
        .unwrap();
        drop(coordinator);
        assert!(
            vfs.memory
                .symlink_metadata(&path.join(fgdb_strata::store::BLOCK_DIR))
                .await
                .is_err(),
            "the fixture has no block directory"
        );

        let view = Database::open_read_view_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        assert_eq!(view.frontier(), CommitSeq::ORIGIN, "nothing was committed");
        // Healed: the next read-only open is the lease-free fast path.
        vfs.take_writes();
        let again = Database::open_read_view_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        assert_eq!(again.frontier(), view.frontier());
        assert_eq!(again.partition_root(), view.partition_root());
        assert_eq!(vfs.take_writes(), Vec::<String>::new());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn sealed_options(call: &FnxCallSpec, as_of: Option<CommitSeq>) -> FnxReadOptions {
    let mut options = FnxReadOptions::whole_graph_for(call, as_of);
    options.selection.relation = Some(RELATION);
    options.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
    options
}

fn sealed_memory() -> FnxMemoryLimits {
    FnxMemoryLimits {
        max_kernel_workspace_bytes: 1 << 24,
        max_result_bytes: 1 << 24,
    }
}

fn object_path(directory: &Path, id: fgdb_types::ids::ObjectId) -> PathBuf {
    let hex: String = id.0.iter().map(|byte| format!("{byte:02x}")).collect();
    directory
        .join(fgdb_strata::store::BLOCK_DIR)
        .join(format!("{hex}.block"))
}

fn read_counts(paths: Vec<PathBuf>) -> BTreeMap<PathBuf, usize> {
    let mut counts = BTreeMap::new();
    for path in paths {
        *counts.entry(path).or_insert(0) += 1;
    }
    counts
}

fn execute_sealed(
    query: &fgdb_types::QueryCx,
    graph: &SealedGraphView,
    call: &FnxCallSpec,
    options: FnxReadOptions,
) -> FnxResult {
    call.execute_sealed(query, graph, options.execution_limits, sealed_memory())
        .unwrap()
}

#[test]
fn composed_sealed_open_is_read_only_and_pins_kernel_pattern_and_property_reads() {
    let ((), report) = run_async_under_lab(0x7669_6614, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = RecordingVfs::new();
        let path = vfs.memory.database_dir();
        let mut writer = spokes(&commit, &vfs, 6).await;
        let root_id = writer.read_session().unwrap().partition_root();
        vfs.take_writes();
        vfs.take_object_paths();
        let decoded = Database::open_read_view_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let ordinary_reads = read_counts(vfs.take_object_paths());
        let root_path = object_path(&path, root_id.0);
        assert_eq!(decoded.partition_root(), root_id);
        drop(decoded);
        let view = Database::open_sealed_read_with_vfs(
            &commit,
            &query,
            vfs.clone(),
            &path,
            keys(),
            Some(CommitSeq(3)),
            RELATION,
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
        let composed_reads = read_counts(vfs.take_object_paths());
        assert_eq!(
            composed_reads, ordinary_reads,
            "the composed owner must reuse one complete authenticated source walk"
        );
        assert!(
            composed_reads.len() > 6,
            "the fixture must span multiple source objects"
        );
        for (object, count) in &composed_reads {
            if *object != root_path {
                assert_eq!(*count, 1, "payload or manifest was reread: {object:?}");
            }
        }
        assert!(vfs.take_writes().is_empty(), "no writer lease or repair");
        assert_eq!(view.snapshot_seq(), CommitSeq(3));
        assert_eq!(view.frontier(), CommitSeq(6));
        assert_eq!(view.partition_root(), root_id);
        assert_eq!(view.relation(), RELATION);
        let params = fgdb_gql::GqlParameters::new();
        let prepared = fgdb::PreparedNativeRead::prepare(
            "CALL fnx.single_source_shortest_path_length(args => [1], relation => 'R', \
             parallel => 'collapse') YIELD vertex AS n, distance \
             MATCH (n) RETURN n.p AS p, distance ORDER BY p",
            &params,
            |kind, name: &str| match (kind, name) {
                (fgdb_gql::GraphSymbolKind::Relation, "R") => {
                    Some(fgdb_gql::GraphSymbol::Relation(RELATION))
                }
                (fgdb_gql::GraphSymbolKind::Property, "p") => {
                    Some(fgdb_gql::GraphSymbol::Property(PROPERTY))
                }
                _ => None,
            },
        )
        .unwrap();
        let policy = fgdb_gql::GqlQueryPolicy::new(100_000, 1000, 10_000_000, 1_000_000);
        let expected = fgdb::QueryResult::Rows {
            columns: vec!["p".into(), "distance".into()],
            rows: [(1, 0), (2, 1), (3, 1)]
                .into_iter()
                .map(|(property, distance)| {
                    vec![
                        fgdb::QueryValue::Value(fgdb_gql::algebra::GraphValue::Scalar(
                            CanonicalScalar::Int(property),
                        )),
                        fgdb::QueryValue::Value(fgdb_gql::algebra::GraphValue::Scalar(
                            CanonicalScalar::Int(distance),
                        )),
                    ]
                })
                .collect(),
        };
        assert_eq!(
            view.execute_prepared(&query, &prepared, &params, policy, sealed_memory())
                .unwrap(),
            expected
        );
        writer.write(&commit, spoke(7)).await.unwrap();
        let mut change = WriteBatch::new(RELATION);
        change.set_vertex_property(VId(2), PROPERTY, Some(CanonicalScalar::Int(200)));
        writer.write(&commit, change).await.unwrap();
        drop(writer);
        vfs.take_writes();
        vfs.take_object_paths();
        assert_eq!(
            view.execute_prepared(&query, &prepared, &params, policy, sealed_memory())
                .unwrap(),
            expected
        );
        assert!(
            vfs.take_object_paths().is_empty(),
            "execution reuses the pinned owner"
        );
        assert!(vfs.take_writes().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn composed_sealed_open_retains_unselected_history_and_rejects_its_corruption() {
    let ((), report) = run_async_under_lab(0x7669_6615, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = RecordingVfs::new();
        let path = vfs.memory.database_dir();
        let mut writer = spokes(&commit, &vfs, 3).await;
        let other = RelationId(99);
        let target = VId((1u128 << 100) + 9);
        let edge = EId((1u128 << 96) + 1);
        let mut initial = WriteBatch::new(other);
        initial.create_vertex(target, vec![], vec![(PROPERTY, CanonicalScalar::Int(9))]);
        initial.add_edge(
            edge,
            VId(1),
            target,
            vec![(PROPERTY, CanonicalScalar::Int(71))],
        );
        writer.write(&commit, initial).await.unwrap();
        let old_cut = writer.frontier().unwrap();
        let mut change = WriteBatch::new(other);
        change.set_vertex_property(target, PROPERTY, Some(CanonicalScalar::Int(109)));
        change.set_edge_property(edge, PROPERTY, Some(CanonicalScalar::Int(72)));
        writer.write(&commit, change).await.unwrap();
        let current_cut = writer.frontier().unwrap();
        let root_id = writer.read_session().unwrap().partition_root();
        let params = fgdb_gql::GqlParameters::new();
        let prepared = fgdb::PreparedNativeRead::prepare(
            "CALL fnx.single_source_shortest_path_length(args => [1], relation => 'R', \
             parallel => 'collapse') YIELD vertex AS n, distance \
             MATCH (n)-[e:OTHER]->(m) \
             RETURN n.p AS source, m.p AS target, e.p AS weight, distance",
            &params,
            |kind, name: &str| match (kind, name) {
                (fgdb_gql::GraphSymbolKind::Relation, "R") => {
                    Some(fgdb_gql::GraphSymbol::Relation(RELATION))
                }
                (fgdb_gql::GraphSymbolKind::Relation, "OTHER") => {
                    Some(fgdb_gql::GraphSymbol::Relation(other))
                }
                (fgdb_gql::GraphSymbolKind::Property, "p") => {
                    Some(fgdb_gql::GraphSymbol::Property(PROPERTY))
                }
                _ => None,
            },
        )
        .unwrap();
        let policy = fgdb_gql::GqlQueryPolicy::new(100_000, 1000, 10_000_000, 1_000_000);
        for (cut, target_property, edge_property) in [(old_cut, 9, 71), (current_cut, 109, 72)] {
            vfs.take_writes();
            let view = Database::open_sealed_read_with_vfs(
                &commit,
                &query,
                vfs.clone(),
                &path,
                keys(),
                Some(cut),
                RELATION,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
            assert_eq!(view.partition_root(), root_id);
            assert_eq!(view.snapshot_seq(), cut);
            let expected = fgdb::QueryResult::Rows {
                columns: vec![
                    "source".into(),
                    "target".into(),
                    "weight".into(),
                    "distance".into(),
                ],
                rows: vec![
                    [1, target_property, edge_property, 0]
                        .into_iter()
                        .map(|value| {
                            fgdb::QueryValue::Value(fgdb_gql::algebra::GraphValue::Scalar(
                                CanonicalScalar::Int(value),
                            ))
                        })
                        .collect(),
                ],
            };
            assert_eq!(
                view.execute_prepared(&query, &prepared, &params, policy, sealed_memory())
                    .unwrap(),
                expected,
                "ordinary patterns and properties must retain unselected relation history"
            );
            assert!(vfs.take_writes().is_empty());
        }

        let store = fgdb_strata::store::BlockStore::open_existing_for_read_with_vfs(
            &commit,
            vfs.clone(),
            &path,
            [0xc1; 32],
            keys().namespace,
        )
        .await
        .unwrap();
        let membership = store.get_root(&commit, root_id).await.unwrap();
        let damaged = membership
            .blocks
            .iter()
            .find(|entry| entry.last_seq == current_cut)
            .unwrap();
        let rows = store
            .get(&commit, fgdb_strata::DeltaBlockVersion(damaged.block_id))
            .await
            .unwrap();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|entry| entry.relation == other));
        let damaged_path = store.path(damaged.block_id);
        let mut corrupted = vfs.memory.read(&damaged_path).await.unwrap();
        corrupted[0] ^= 0x40;
        vfs.memory.write(&damaged_path, &corrupted).await.unwrap();
        vfs.take_object_paths();
        vfs.take_writes();
        // A historical cut and a different retained relation do not authorize
        // skipping any untrusted block from the selected immutable root.
        let result = Database::open_sealed_read_with_vfs(
            &commit,
            &query,
            vfs.clone(),
            &path,
            keys(),
            Some(old_cut),
            RELATION,
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await;
        assert!(matches!(
            result,
            Err(SealedProjectionOpenError::Read(
                fgdb_prism::FnxSealedReadError::Seal(
                    fgdb_strata::tiered::sealed::SealedError::Store(_)
                )
            ))
        ));
        assert!(vfs.take_object_paths().contains(&damaged_path));
        assert!(
            Database::open_read_view_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .is_err(),
            "the ordinary read oracle must independently reject the same corrupt source"
        );
        assert!(vfs.take_writes().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sealed_open_reads_every_payload_once_beside_a_writer_and_owns_its_generation() {
    let ((), report) = run_async_under_lab(0x7669_6610, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = RecordingVfs::new();
        let path = vfs.memory.database_dir();
        let mut writer = spokes(&commit, &vfs, 6).await;
        // This relation must be authenticated even though the projection omits it.
        let mut unrelated = WriteBatch::new(RelationId(99));
        unrelated.add_edge(EId(9999), VId(6), VId(1), vec![]);
        writer.write(&commit, unrelated).await.unwrap();
        let bfs = FnxCallSpec::bind(
            "CALL fnx.single_source_shortest_path_length(1) YIELD vertex,distance",
            &FnxParameters::new(),
        )
        .unwrap();
        let wcc = FnxCallSpec::bind(
            "CALL fnx.weakly_connected_components() YIELD vertex,component",
            &FnxParameters::new(),
        )
        .unwrap();
        let options = sealed_options(&bfs, None);
        vfs.take_object_paths();
        vfs.take_writes();
        let decoded = Database::open_read_view_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let root_path = object_path(&path, decoded.partition_root().0);
        let ordinary_reads = read_counts(vfs.take_object_paths());
        let expected_bfs = decoded.execute_fnx(&query, &bfs, options).unwrap();
        let expected_wcc = decoded.execute_fnx(&query, &wcc, options).unwrap();
        assert_eq!(vfs.take_writes(), Vec::<String>::new());
        let graph = Database::open_prism_sealed_projection_with_vfs(
            &commit,
            &query,
            vfs.clone(),
            &path,
            keys(),
            options,
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
        let sealed_reads = read_counts(vfs.take_object_paths());
        assert_eq!(
            sealed_reads, ordinary_reads,
            "same complete authenticated source"
        );
        assert!(
            sealed_reads.len() > 6,
            "the fixture must span multiple source objects"
        );
        for (object, count) in &sealed_reads {
            if *object != root_path {
                assert_eq!(*count, 1, "payload or manifest was reread: {object:?}");
            }
        }
        assert_eq!(vfs.take_writes(), Vec::<String>::new());
        assert_eq!(graph.binding().root, decoded.partition_root().0);
        assert_eq!(graph.binding().as_of, decoded.frontier());
        assert_eq!(graph.node_count(), 6);
        for (call, expected) in [(&bfs, &expected_bfs), (&wcc, &expected_wcc)] {
            let actual = execute_sealed(&query, &graph, call, options);
            assert_eq!(actual.columns, expected.analytics.columns);
            assert_eq!(actual.rows, expected.analytics.rows);
            assert_eq!(
                actual.certificate.result_digest,
                expected.analytics.certificate.result_digest
            );
        }

        writer.write(&commit, spoke(7)).await.unwrap();
        let newer = Database::open_prism_sealed_projection_with_vfs(
            &commit,
            &query,
            vfs.clone(),
            &path,
            keys(),
            options,
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
        assert_eq!(newer.node_count(), 7);
        assert_ne!(newer.binding().root, graph.binding().root);
        drop(newer);
        drop(decoded);
        drop(writer);
        vfs.take_object_paths();
        vfs.take_writes();
        let again = execute_sealed(&query, &graph, &bfs, options);
        assert_eq!(again.rows, expected_bfs.analytics.rows);
        assert_eq!(graph.node_count(), 6);
        assert!(
            vfs.take_object_paths().is_empty(),
            "execution must reuse its owned image"
        );
        assert!(vfs.take_writes().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sealed_open_preserves_historical_weighted_label_selection_and_directions() {
    let ((), report) = run_async_under_lab(0x7669_6611, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = RecordingVfs::new();
        let path = vfs.memory.database_dir();
        let mut writer = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut initial = WriteBatch::new(RELATION);
        for id in 1..=4 {
            initial.create_vertex(VId(id), vec![LabelId(1)], vec![]);
        }
        initial.create_vertex(VId(99), vec![LabelId(2)], vec![]);
        initial.add_edge(
            EId(1),
            VId(1),
            VId(2),
            vec![(PROPERTY, CanonicalScalar::Int(2))],
        );
        initial.add_edge(
            EId(2),
            VId(2),
            VId(3),
            vec![(PROPERTY, CanonicalScalar::Int(3))],
        );
        // An invalid numeric weight outside the induced label selection is ignored.
        initial.add_edge(
            EId(3),
            VId(2),
            VId(99),
            vec![(PROPERTY, CanonicalScalar::Bool(true))],
        );
        writer.write(&commit, initial).await.unwrap();
        let old_cut = writer.frontier().unwrap();
        let mut change = WriteBatch::new(RELATION);
        change.set_edge_property(EId(1), PROPERTY, Some(CanonicalScalar::Int(11)));
        writer.write(&commit, change).await.unwrap();
        let view = writer.read_session().unwrap();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let source = if direction == Directedness::Reversed {
                3
            } else {
                1
            };
            let call = FnxCallSpec::bind(
                &format!(
                    "CALL fnx.single_source_dijkstra_path_length({source}) YIELD vertex,distance"
                ),
                &FnxParameters::new(),
            )
            .unwrap();
            let mut historical_rows = None;
            for cut in [old_cut, view.frontier()] {
                let mut options = sealed_options(&call, Some(cut));
                options.selection.vertex_label = Some(LabelId(1));
                options.selection.weight = FnxWeightSpec::Property {
                    key: PROPERTY,
                    missing: MissingWeightPolicy::Reject,
                };
                options.projection.directedness = direction;
                options.projection.parallel_edges = ParallelEdgePolicy::Sum;
                let expected = view.execute_fnx(&query, &call, options).unwrap();
                vfs.take_writes();
                let graph = Database::open_prism_sealed_projection_with_vfs(
                    &commit,
                    &query,
                    vfs.clone(),
                    &path,
                    keys(),
                    options,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap();
                assert_eq!(graph.node_count(), 4, "selected isolates must survive");
                assert_eq!(graph.binding().as_of, cut);
                let actual = execute_sealed(&query, &graph, &call, options);
                assert_eq!(actual.rows, expected.analytics.rows);
                assert_eq!(
                    actual.certificate.result_digest,
                    expected.analytics.certificate.result_digest
                );
                if let Some(old) = historical_rows.replace(actual.rows) {
                    assert_ne!(
                        old, expected.analytics.rows,
                        "the weight change must affect the answer"
                    );
                }
                assert!(vfs.take_writes().is_empty());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sealed_open_refuses_missing_and_lagging_checkpoints_without_healing() {
    let ((), report) = run_async_under_lab(0x7669_6612, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let call = FnxCallSpec::bind(
            "CALL fnx.weakly_connected_components() YIELD vertex,component",
            &FnxParameters::new(),
        )
        .unwrap();
        for lagging in [false, true] {
            let vfs = RecordingVfs::new();
            let path = vfs.memory.database_dir();
            if lagging {
                let mut writer = spokes(&commit, &vfs, 3).await;
                let error = writer
                    .write_with_publication_failure(
                        &commit,
                        spoke(4),
                        DerivedPublicationStage::PublishPartitionRoot,
                    )
                    .await
                    .unwrap_err();
                assert!(matches!(error, WriteError::CommittedNeedsRecovery { .. }));
                drop(writer);
            } else {
                vfs.memory.create_dir_all(&path).await.unwrap();
                let coordinator = fgdb_chronicle::commit::CommitCoordinator::open_with_vfs(
                    &commit,
                    vfs.memory.clone(),
                    &path,
                    keys().capsule_keys(),
                )
                .await
                .unwrap();
                drop(coordinator);
            }
            for composed in [false, true] {
                vfs.take_writes();
                let result = if composed {
                    Database::open_sealed_read_with_vfs(
                        &commit,
                        &query,
                        vfs.clone(),
                        &path,
                        keys(),
                        None,
                        RELATION,
                        SealedLimits::default(),
                        RootReadLimits::default(),
                    )
                    .await
                    .map(|_| ())
                } else {
                    Database::open_prism_sealed_projection_with_vfs(
                        &commit,
                        &query,
                        vfs.clone(),
                        &path,
                        keys(),
                        sealed_options(&call, None),
                        SealedLimits::default(),
                        RootReadLimits::default(),
                    )
                    .await
                    .map(|_| ())
                };
                assert!(matches!(
                    result,
                    Err(SealedProjectionOpenError::RecoveryRequired)
                ));
                assert!(
                    vfs.take_writes().is_empty(),
                    "refusal must not heal storage"
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sealed_open_applies_source_caps_and_future_cut_before_payload_reads() {
    let ((), report) = run_async_under_lab(0x7669_6613, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = RecordingVfs::new();
        let path = vfs.memory.database_dir();
        let writer = spokes(&commit, &vfs, 3).await;
        let view = writer.read_session().unwrap();
        let metadata = [
            object_path(&path, view.manifest().0),
            object_path(&path, view.partition_root().0),
        ];
        let call = FnxCallSpec::bind(
            "CALL fnx.weakly_connected_components() YIELD vertex,component",
            &FnxParameters::new(),
        )
        .unwrap();
        let options = sealed_options(&call, None);
        let caps = [
            RootReadLimits {
                max_root_bytes: 0,
                ..RootReadLimits::default()
            },
            RootReadLimits {
                max_blocks: 0,
                ..RootReadLimits::default()
            },
            RootReadLimits {
                max_vertex_patches: 0,
                ..RootReadLimits::default()
            },
            RootReadLimits {
                max_source_bytes: 0,
                ..RootReadLimits::default()
            },
        ];
        for cap in caps {
            for composed in [false, true] {
                vfs.take_object_paths();
                vfs.take_writes();
                let result = if composed {
                    Database::open_sealed_read_with_vfs(
                        &commit,
                        &query,
                        vfs.clone(),
                        &path,
                        keys(),
                        None,
                        RELATION,
                        SealedLimits::default(),
                        cap,
                    )
                    .await
                    .map(|_| ())
                } else {
                    Database::open_prism_sealed_projection_with_vfs(
                        &commit,
                        &query,
                        vfs.clone(),
                        &path,
                        keys(),
                        options,
                        SealedLimits::default(),
                        cap,
                    )
                    .await
                    .map(|_| ())
                };
                assert!(result.is_err(), "zero source cap must refuse: {cap:?}");
                assert!(
                    vfs.take_object_paths()
                        .iter()
                        .all(|object| metadata.contains(object)),
                    "metadata refusal must precede source payload reads"
                );
                assert!(vfs.take_writes().is_empty());
            }
        }
        let mut future = options;
        future.as_of = Some(CommitSeq(view.frontier().0 + 1));
        for composed in [false, true] {
            vfs.take_object_paths();
            let result = if composed {
                Database::open_sealed_read_with_vfs(
                    &commit,
                    &query,
                    vfs.clone(),
                    &path,
                    keys(),
                    future.as_of,
                    RELATION,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .map(|_| ())
            } else {
                Database::open_prism_sealed_projection_with_vfs(
                    &commit,
                    &query,
                    vfs.clone(),
                    &path,
                    keys(),
                    future,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .map(|_| ())
            };
            assert!(matches!(result, Err(SealedProjectionOpenError::Read(_))));
            assert!(
                vfs.take_object_paths()
                    .iter()
                    .all(|object| metadata.contains(object))
            );
            assert!(vfs.take_writes().is_empty());
        }

        let result = Database::open_prism_sealed_projection_with_vfs(
            &commit,
            &query,
            vfs.clone(),
            &path,
            keys(),
            options,
            SealedLimits::default(),
            RootReadLimits {
                max_vertex_versions: 0,
                ..RootReadLimits::default()
            },
        )
        .await;
        assert!(
            result.is_err(),
            "raw vertex history must consume its own cap"
        );
        assert!(vfs.take_writes().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
