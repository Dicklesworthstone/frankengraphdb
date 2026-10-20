//! Compaction trusts what is already published (fgdb-l89gt).
//!
//! A compaction publishes its consolidated generation through the shared
//! publication tail. That publication now starts from the handle's receipts,
//! the objects the fgdb-ibbuq "trust publication" ruling already treats as
//! durable, so an object the published generation holds is neither re-read nor
//! re-synced. These laws pin the observable cost: what a recompaction does to
//! Strata objects, and that it does not grow with the partition. Snapshot
//! assembly decodes the exact sealed bytes those receipts admitted, so there
//! is no second full-partition read after publication. Durable readback and
//! existing-object collision checks remain load-bearing before the slot swap.

use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir, Vfs};
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, DatabaseState, MemVfs, MemVfsFile, RebuildError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_strata::store::{BLOCK_DIR, PACK_DIR, StoreError};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Counts what a publication does to Strata objects: every `open` (staging
/// a new inode, or reopening an existing canonical one to prove and sync
/// it) and every `symlink_metadata` probe of a canonical name. Reads of
/// already published objects go through `open_read` and have their own
/// counter/refusal. `read` is the VFS durable-backing probe used by publication,
/// so it stays available while redundant snapshot reads are refused.
#[derive(Clone, Debug)]
struct ObjectIoVfs {
    memory: MemVfs,
    touched: Arc<AtomicUsize>,
    snapshot_reads: Arc<AtomicUsize>,
    refuse_snapshot_reads: Arc<AtomicBool>,
    corrupt_durable_read: Arc<AtomicBool>,
    inject_existing_corruption: Arc<AtomicBool>,
}

impl ObjectIoVfs {
    fn new() -> Self {
        Self {
            memory: MemVfs::new().unwrap(),
            touched: Arc::new(AtomicUsize::new(0)),
            snapshot_reads: Arc::new(AtomicUsize::new(0)),
            refuse_snapshot_reads: Arc::new(AtomicBool::new(false)),
            corrupt_durable_read: Arc::new(AtomicBool::new(false)),
            inject_existing_corruption: Arc::new(AtomicBool::new(false)),
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

    fn strata_path(path: &Path) -> bool {
        path.parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == BLOCK_DIR || name == PACK_DIR)
    }

    fn refuse_snapshot_reads(&self, refuse: bool) {
        self.snapshot_reads.store(0, Ordering::SeqCst);
        self.refuse_snapshot_reads.store(refuse, Ordering::SeqCst);
    }
}

impl Vfs for ObjectIoVfs {
    type File = MemVfsFile;

    async fn open(&self, path: &Path, options: &OpenOptions) -> io::Result<Self::File> {
        self.observe(path);
        self.memory.open(path, options).await
    }
    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        if Self::strata_path(path) {
            self.snapshot_reads.fetch_add(1, Ordering::SeqCst);
            if self.refuse_snapshot_reads.load(Ordering::SeqCst) {
                return Err(io::Error::other("redundant post-publication snapshot read"));
            }
        }
        self.memory.open_read(path).await
    }
    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.memory.metadata(path).await
    }
    async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.observe(path);
        let metadata = self.memory.symlink_metadata(path).await;
        if Self::strata_path(path)
            && path
                .extension()
                .is_some_and(|extension| extension == "block")
            && metadata
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            && self
                .inject_existing_corruption
                .swap(false, Ordering::SeqCst)
        {
            // Simulate a damaged orphan under the candidate's canonical name.
            // Existing published objects are never changed by this fault.
            self.memory.write(path, b"damaged existing object").await?;
            return self.memory.symlink_metadata(path).await;
        }
        metadata
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
        let mut bytes = self.memory.read(path).await?;
        if Self::strata_path(path) && self.corrupt_durable_read.swap(false, Ordering::SeqCst) {
            // The fsync/readback contract must reject backing bytes different
            // from the offered sealed bytes before publishing their name.
            if let Some(first) = bytes.first_mut() {
                *first ^= 0xff;
            } else {
                bytes.push(0xff);
            }
        }
        Ok(bytes)
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
            // Durability readback through Vfs::read remains available. A
            // store.reopen here would fail on its first root/payload read.
            vfs.refuse_snapshot_reads(true);
            db.compact(&cx).await.unwrap();
            assert_eq!(vfs.snapshot_reads.load(Ordering::SeqCst), 0);
            let compacted = db.partition_root().unwrap();
            let first = vfs.take();
            db.compact(&cx).await.unwrap();
            assert_eq!(vfs.snapshot_reads.load(Ordering::SeqCst), 0);
            let again = vfs.take();
            assert_eq!(db.partition_root().unwrap(), compacted);
            assert!(
                first > again,
                "the first compaction writes new objects: {first} vs {again}"
            );
            touched.push(again);
            drop(db);
            vfs.refuse_snapshot_reads(false);
            let db = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(db.partition_root().unwrap(), compacted);
            assert_eq!(db.edges().unwrap().len(), spokes as usize);
            assert_eq!(db.vertices().unwrap().len(), spokes as usize + 1);
            assert!(
                vfs.snapshot_reads.load(Ordering::SeqCst) > 0,
                "a cold open still authenticates its root and immutable objects"
            );
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

/// The in-memory refresh follows the encoded statement history, including
/// retirement restatements, hosted properties and full-width identities. A
/// pin survives replacement, a later write uses the retained fold and version
/// heads, and a cold open independently recovers the same complete history.
#[test]
fn sealed_compaction_snapshot_preserves_history_pins_and_subsequent_writes() {
    let ((), report) = run_async_under_lab(0x1892, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let vfs = ObjectIoVfs::new();
        let path = vfs.memory.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let relation = RelationId(u64::MAX);
        let a = VId(u128::MAX - 2);
        let b = VId(1 << 97);
        let c = VId(1);
        let forward = EId(u128::MAX - 10);
        let parallel = EId(1 << 103);
        let reverse = EId(11);
        let self_loop = EId(12);
        let property = PropertyKeyId(1);
        let mut first = WriteBatch::new(relation);
        first.create_vertex(
            a,
            vec![LabelId(1)],
            vec![(property, CanonicalScalar::Int(1))],
        );
        first.create_vertex(b, vec![], vec![]);
        first.create_vertex(c, vec![], vec![]);
        first.add_edge(forward, a, b, vec![(property, CanonicalScalar::Int(10))]);
        first.add_edge(parallel, a, b, vec![]);
        first.add_edge(reverse, b, a, vec![(property, CanonicalScalar::Int(20))]);
        first.add_edge(self_loop, c, c, vec![(property, CanonicalScalar::Int(30))]);
        db.write(&cx, first).await.unwrap();

        let mut second = WriteBatch::new(relation);
        second.set_edge_property(forward, property, Some(CanonicalScalar::Int(11)));
        second.delete_edge(parallel);
        second.set_vertex_property(a, property, Some(CanonicalScalar::Int(2)));
        second.set_vertex_label(b, LabelId(2), true);
        db.write(&cx, second).await.unwrap();
        let mut third = WriteBatch::new(relation);
        third.delete_vertex(c);
        db.write(&cx, third).await.unwrap();

        let history = |database: &Database<ObjectIoVfs>| {
            (0..=database.frontier().unwrap().0)
                .map(|sequence| {
                    let as_of = CommitSeq(sequence);
                    (
                        database.vertices_at(as_of).unwrap(),
                        database.edges_at(as_of).unwrap(),
                        database.neighbours_at(a, relation, as_of).unwrap(),
                        database.in_neighbours_at(a, relation, as_of).unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let before = history(&db);
        let pinned = db.pinned_read_view().unwrap();
        vfs.refuse_snapshot_reads(true);
        db.compact(&cx).await.unwrap();
        assert_eq!(history(&db), before);
        assert_eq!(pinned.vertices().unwrap(), before[3].0);
        assert_eq!(pinned.edges().unwrap(), before[3].1);

        let mut fourth = WriteBatch::new(relation);
        fourth.set_edge_property(forward, property, Some(CanonicalScalar::Int(12)));
        fourth.set_vertex_property(b, property, Some(CanonicalScalar::Int(4)));
        let fourth_seq = db.write(&cx, fourth).await.unwrap();
        assert_eq!(
            db.edge_at(forward, fourth_seq).unwrap().unwrap().props,
            vec![(property, CanonicalScalar::Int(12).into())]
        );
        let after_write = history(&db);
        assert_eq!(&after_write[..before.len()], before.as_slice());
        db.compact(&cx).await.unwrap();
        assert_eq!(history(&db), after_write);
        assert_eq!(pinned.vertices().unwrap(), before[3].0);
        assert_eq!(pinned.edges().unwrap(), before[3].1);
        assert_eq!(vfs.snapshot_reads.load(Ordering::SeqCst), 0);
        let compacted_root = db.partition_root().unwrap();
        let compacted_manifest = db.manifest().unwrap();
        drop(db);

        vfs.refuse_snapshot_reads(false);
        let reopened = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.partition_root().unwrap(), compacted_root);
        assert_eq!(reopened.manifest().unwrap(), compacted_manifest);
        assert_eq!(history(&reopened), after_write);
        assert!(vfs.snapshot_reads.load(Ordering::SeqCst) > 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Removing the post-publication read must not remove the earlier durable
/// backing or existing-object proofs. Both faults refuse before the slot can
/// name the candidate, preserving the predecessor's readable generation.
#[test]
fn sealed_snapshot_refresh_still_refuses_unproven_durable_objects() {
    let ((), report) = run_async_under_lab(0x1893, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for damaged_existing in [false, true] {
            let vfs = ObjectIoVfs::new();
            let path = vfs.memory.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut first = WriteBatch::new(RelationId(1));
            first.create_vertex(VId(1), vec![], vec![]);
            db.write(&cx, first).await.unwrap();
            db.write(&cx, spoke(0)).await.unwrap();
            let old_root = db.partition_root().unwrap();
            let old_manifest = db.manifest().unwrap();
            let old_frontier = db.frontier().unwrap();
            let old_vertices = db.vertices().unwrap();
            let old_edges = db.edges().unwrap();
            if damaged_existing {
                vfs.inject_existing_corruption.store(true, Ordering::SeqCst);
            } else {
                vfs.corrupt_durable_read.store(true, Ordering::SeqCst);
            }
            let error = db.compact(&cx).await.unwrap_err();
            if damaged_existing {
                assert!(
                    matches!(
                        error,
                        RebuildError::Store(StoreError::DamagedExisting { .. })
                    ),
                    "existing canonical bytes still require identity verification: {error:?}"
                );
            } else {
                assert!(
                    matches!(
                        error,
                        RebuildError::Store(StoreError::Io(ref error))
                            if error.kind() == io::ErrorKind::InvalidData
                    ),
                    "publication still verifies its durable backing: {error:?}"
                );
            }
            assert_eq!(db.partition_root().unwrap(), old_root);
            assert_eq!(db.manifest().unwrap(), old_manifest);
            assert_eq!(db.frontier().unwrap(), old_frontier);
            assert_eq!(db.vertices().unwrap(), old_vertices);
            assert_eq!(db.edges().unwrap(), old_edges);
            assert_eq!(
                db.state(),
                DatabaseState::Healthy {
                    published_frontier: old_frontier
                }
            );
            drop(db);
            let reopened = Database::open_with_vfs(&cx, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.partition_root().unwrap(), old_root);
            assert_eq!(reopened.manifest().unwrap(), old_manifest);
            assert_eq!(reopened.vertices().unwrap(), old_vertices);
            assert_eq!(reopened.edges().unwrap(), old_edges);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
