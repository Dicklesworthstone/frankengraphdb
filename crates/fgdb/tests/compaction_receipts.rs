//! Compaction trusts what is already published (fgdb-l89gt).
//!
//! A compaction publishes its consolidated generation through the shared
//! publication tail. That publication now starts from the handle's receipts,
//! the objects the fgdb-ibbuq "trust publication" ruling already treats as
//! durable, so an object the published generation holds is neither re-read nor
//! re-synced. These laws pin the observable cost: what a recompaction does to
//! Strata objects, and that it does not grow with the partition.

use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir, Vfs};
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, MemVfsFile, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts what a publication does to Strata objects: every `open` (staging
/// a new inode, or reopening an existing canonical one to prove and sync
/// it) and every `symlink_metadata` probe of a canonical name. Reads of
/// already published objects go through `open_read` and are not counted.
#[derive(Clone, Debug)]
struct ObjectIoVfs {
    memory: MemVfs,
    touched: Arc<AtomicUsize>,
}

impl ObjectIoVfs {
    fn new() -> Self {
        Self {
            memory: MemVfs::new().unwrap(),
            touched: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn observe(&self, path: &Path) {
        let object = path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == fgdb_strata::store::BLOCK_DIR);
        if object {
            self.touched.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn take(&self) -> usize {
        self.touched.swap(0, Ordering::SeqCst)
    }
}

impl Vfs for ObjectIoVfs {
    type File = MemVfsFile;

    async fn open(&self, path: &Path, options: &OpenOptions) -> io::Result<Self::File> {
        self.observe(path);
        self.memory.open(path, options).await
    }
    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        self.memory.open_read(path).await
    }
    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.memory.metadata(path).await
    }
    async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.observe(path);
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
        self.memory.read(path).await
    }
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.memory.read_to_string(path).await
    }
    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.memory.write(path, bytes).await
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb6; 32],
        DatabaseSecurityNamespaceId([0xb7; 32]),
        [0xb8; 32],
    )
}

/// One commit: a fresh source vertex and a propertied edge to the hub, so
/// every commit adds its own block family and a hosted property patch.
fn spoke(i: u128) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(
        VId(1_000 + i),
        vec![],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(i as i64))],
    );
    batch.add_edge(
        EId(10_000 + i),
        VId(1_000 + i),
        VId(1),
        vec![(PropertyKeyId(2), CanonicalScalar::Int(i as i64))],
    );
    batch
}

/// **Recompaction touches no published object, however large the
/// partition (fgdb-l89gt).** Compaction's publication starts from the
/// handle's receipts (the fgdb-ibbuq "trust publication" ruling), so
/// recompacting an already compacted generation re-reads and re-syncs no
/// block or patch object: its object I/O is its root and manifest, the
/// same for 16 spokes as for 64. The recompacted root is the same root,
/// and a cold reopen of the result answers the same graph.
#[test]
fn recompaction_touches_no_published_object_however_large_the_partition() {
    let ((), report) = run_async_under_lab(0x1891, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut touched = Vec::new();
        for spokes in [16_u128, 64] {
            let vfs = ObjectIoVfs::new();
            let path = vfs.memory.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut hub = WriteBatch::new(RelationId(1));
            hub.create_vertex(VId(1), vec![], vec![]);
            db.write(&cx, hub).await.unwrap();
            for i in 0..spokes {
                db.write(&cx, spoke(i)).await.unwrap();
            }
            db.compact(&cx).await.unwrap();
            let compacted = db.partition_root().unwrap();
            let first = vfs.take();
            db.compact(&cx).await.unwrap();
            let again = vfs.take();
            assert_eq!(db.partition_root().unwrap(), compacted);
            assert!(
                first > again,
                "the first compaction writes new objects: {first} vs {again}"
            );
            touched.push(again);
            drop(db);
            let db = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(db.partition_root().unwrap(), compacted);
            assert_eq!(db.edges().unwrap().len(), spokes as usize);
            assert_eq!(db.vertices().unwrap().len(), spokes as usize + 1);
        }
        assert_eq!(
            touched[0], touched[1],
            "recompaction object I/O must not grow with the partition: {touched:?}"
        );
        // Only the root and the manifest: an existing canonical object is
        // probed once and opened twice (one handle re-reads and proves it,
        // the other is synced), so 2 x 3.
        assert!(touched[0] <= 6, "only the root and manifest: {touched:?}");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
