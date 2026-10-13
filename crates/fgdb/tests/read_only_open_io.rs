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
    BufferLimits, BufferedReadLimits, Database, DatabaseKeys, DerivedPublicationStage, MemVfs,
    MemVfsFile, MemoryPool, WriteBatch, WriteError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
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
    object_reads: Arc<Mutex<usize>>,
}

impl RecordingVfs {
    fn new() -> Self {
        Self {
            memory: MemVfs::new().unwrap(),
            writes: Arc::new(Mutex::new(Vec::new())),
            object_reads: Arc::new(Mutex::new(0)),
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
            *self.object_reads.lock().unwrap() += 1;
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
