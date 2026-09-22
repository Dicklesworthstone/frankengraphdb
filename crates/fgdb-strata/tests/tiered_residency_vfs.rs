//! Composition tests for the real Tier-D admission, sealed I/R image, resident
//! budget, asynchronous extent loader, and query-private UnixVfs scratch path.
//! These tests do not promote a derived image into durable manifest authority.

use asupersync::fs::{OpenOptions, UnixVfs, Vfs, VfsFile};
use asupersync::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::RelationId;
use fgdb_strata::root::{BlockRef, PartitionRoot};
use fgdb_strata::store::BlockStore;
use fgdb_strata::tiered::buffer::{
    Admission, BufferError, BufferLimits, ExtentBuffer, ExtentKey, PendingExtent, extent_checksum,
};
use fgdb_strata::tiered::memory::{MemoryPool, SpillError, SpillFile, SpillLimits, SpillableBytes};
use fgdb_strata::tiered::sealed::{RowStorageKind, SealedLimits, SealedPartition};
use fgdb_strata::{AdjacencyEntry, encode_block};
use fgdb_types::{
    BranchId, CommitSeq, DatabaseSecurityNamespaceId, EId, GraphId, ObjectId, PurposeContexts,
    QueryCx, VId,
};
use std::future::Future;
use std::io::{self, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const REL: RelationId = RelationId(1);
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

/// Only test-fixture namespace setup/teardown uses std::fs. Storage reads,
/// writes, seeks, and truncation below use the production asupersync Vfs.
struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        for _ in 0..100 {
            let ordinal = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fgdb-tiered-residency-{}-{ordinal}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create private test fixture: {error}"),
            }
        }
        panic!("could not obtain an exclusive test fixture directory");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn under_lab<Fut>(test: impl FnOnce(PurposeContexts, Fixture) -> Fut + Send + 'static)
where
    Fut: Future<Output = ()> + Send,
{
    let fixture = Fixture::new();
    let (_, report) = run_async_under_lab(0xb2_2026, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root), fixture).await;
    });
    assert!(report.invariant_violations.is_empty(), "{report:?}");
}

fn sample_edges() -> Vec<AdjacencyEntry> {
    let mut edges: Vec<_> = (0..40u128)
        .map(|i| AdjacencyEntry {
            src: VId(1),
            relation: REL,
            dst: VId(10 + i / 3),
            eid: EId(i + 1),
            created_at: CommitSeq(1),
            retired_at: (i % 5 == 0).then_some(CommitSeq(7)),
        })
        .collect();
    edges.extend((0..4u128).map(|i| AdjacencyEntry {
        src: VId(2),
        relation: REL,
        dst: VId(100 + i),
        eid: EId(100 + i),
        created_at: CommitSeq(2),
        retired_at: None,
    }));
    edges
}

async fn admitted_image(contexts: &PurposeContexts, dir: &Path) -> SealedPartition {
    let commit = contexts.commit();
    let query = contexts.query();
    let store = BlockStore::open(&commit, dir, K_OID, NAMESPACE)
        .await
        .unwrap();
    let bytes = encode_block(0, None, &sample_edges()).unwrap();
    let block = store.put(&commit, &bytes).await.unwrap();
    let root = PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(2),
        partition: 0,
        published_at: CommitSeq(10),
        blocks: vec![BlockRef {
            block_id: block.0,
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(7),
        }],
        vertex_patches: vec![],
    };
    let root_id = store.put_root(&commit, &root).await.unwrap();
    store
        .seal_partition(&query, root_id, CommitSeq(2), SealedLimits::default())
        .await
        .unwrap()
}

fn assert_snapshots(query: &QueryCx, image: &SealedPartition) {
    assert_eq!(
        image.storage_kind(VId(1), REL),
        Some(RowStorageKind::SealedCsr)
    );
    assert_eq!(
        image.storage_kind(VId(2), REL),
        Some(RowStorageKind::Inline)
    );
    let edges = sample_edges();
    for cut in 2..=10 {
        for src in [VId(1), VId(2)] {
            let expected: Vec<_> = edges
                .iter()
                .copied()
                .filter(|entry| entry.src == src && entry.visible_at(CommitSeq(cut)))
                .collect();
            let mut cursor = image.row(query, src, REL, CommitSeq(cut)).unwrap();
            let mut actual = Vec::new();
            while let Some(edge) = cursor.next(query).unwrap() {
                assert!(edge.properties.is_empty());
                actual.push(edge.entry);
            }
            assert_eq!(actual, expected, "source {src:?}, cut {cut}");
        }
    }
    assert!(image.row(query, VId(1), REL, CommitSeq(1)).is_err());
    assert!(image.row(query, VId(1), REL, CommitSeq(11)).is_err());
}

async fn load_extent(path: &Path, mut pending: PendingExtent) -> io::Result<PendingExtent> {
    let vfs = UnixVfs;
    let mut file = vfs.open_read(path).await?;
    let expected = pending.key().offset();
    let actual = file.seek(SeekFrom::Start(expected)).await?;
    if actual != expected {
        return Err(io::Error::other(
            "extent seek did not reach its requested offset",
        ));
    }
    file.read_exact(pending.as_mut()).await?;
    Ok(pending)
}

#[test]
fn admitted_inline_and_csr_images_survive_real_file_spill_and_memory_readmission() {
    under_lab(|contexts, fixture| async move {
        let query = contexts.query();
        let image = admitted_image(&contexts, &fixture.0).await;
        assert_snapshots(&query, &image);
        let anchor = image.anchor();
        let encoded = image.encode(&query, SealedLimits::default()).unwrap();
        let len = encoded.len();
        assert!(len > 1);
        let root_pool = MemoryPool::new(len * 4, 0).unwrap();
        let operator_pool = root_pool.child(len * 2 - 1, 0).unwrap();
        let mut batch = SpillableBytes::new(operator_pool.allocate_zeroed(&query, len).unwrap());
        batch.resident_mut().unwrap().copy_from_slice(&encoded);
        drop(encoded);
        drop(image);

        let vfs = UnixVfs;
        let path = fixture.0.join("query.scratch");
        let file = query
            .with_restriction_async(vfs.open(
                &path,
                &OpenOptions::new().read(true).write(true).create_new(true),
            ))
            .await
            .unwrap();
        let mut scratch = SpillFile::new(
            &query,
            file,
            operator_pool.clone(),
            SpillLimits {
                max_file_bytes: (len * 4) as u64,
                max_runs: 8,
                max_run_bytes: len,
            },
        )
        .await
        .unwrap();

        // Two resident image-sized batches cannot fit. Admission spills the
        // selected old batch, then admits the new one under the same ceiling.
        let replacement = operator_pool
            .allocate_spilling(&query, len, &mut batch, &mut scratch)
            .await
            .unwrap();
        assert!(batch.is_spilled());
        assert_eq!(batch.charged_bytes(), 0);
        assert!(root_pool.used() <= operator_pool.limit());
        assert!(matches!(
            batch.restore(&query, &mut scratch).await,
            Err(SpillError::Memory(_))
        ));
        assert!(batch.is_spilled());
        drop(replacement);
        assert_eq!(root_pool.used(), 0);
        assert!(batch.restore(&query, &mut scratch).await.unwrap());
        let reloaded = SealedPartition::reload(
            &query,
            anchor,
            batch.resident().unwrap(),
            SealedLimits::default(),
            None,
        )
        .unwrap();
        assert_snapshots(&query, &reloaded);
        assert_eq!(reloaded.anchor(), anchor);
        assert_eq!(scratch.stats().published_runs, 1);
        drop(reloaded);
        drop(batch);
        drop(scratch);
        assert_eq!((root_pool.used(), operator_pool.used()), (0, 0));
    });
}

#[test]
fn anchored_images_fault_through_unix_vfs_after_cache_eviction() {
    under_lab(|contexts, fixture| async move {
        let query = contexts.query();
        let image = admitted_image(&contexts, &fixture.0).await;
        let anchor = image.anchor();
        let encoded = image.encode(&query, SealedLimits::default()).unwrap();
        let len = encoded.len();
        let vfs = UnixVfs;
        let path = fixture.0.join("derived-image.cache");
        let other_bytes = [0x55; 32];
        let corrupt_byte = encoded[0] ^ 1;
        let offset = 17;
        // These IDs are fixture-local cache namespaces, NOT newly minted
        // registered graph objects. The opaque anchor retains source authority.
        let image_key =
            ExtentKey::new(ObjectId([0xc1; 32]), offset, len, extent_checksum(&encoded)).unwrap();
        let other_key = ExtentKey::new(
            ObjectId([0xc2; 32]),
            offset + len as u64,
            other_bytes.len(),
            extent_checksum(&other_bytes),
        )
        .unwrap();
        query
            .with_restriction_async(async {
                let mut file = vfs
                    .open(
                        &path,
                        &OpenOptions::new().read(true).write(true).create_new(true),
                    )
                    .await
                    .unwrap();
                file.write_all(&[0x33; 17]).await.unwrap();
                file.write_all(&encoded).await.unwrap();
                file.write_all(&other_bytes).await.unwrap();
                file.flush().await.unwrap();
            })
            .await;
        drop(encoded);
        drop(image);
        let pool = MemoryPool::new(len * 4 + 8192, 0).unwrap();
        let mut cache = ExtentBuffer::new(
            pool.clone(),
            BufferLimits {
                max_frames: 1,
                max_ghost_entries: 2,
                max_extent_bytes: len.max(other_bytes.len()),
            },
        )
        .unwrap();
        let first = cache
            .pin_async(&query, image_key, Admission::Normal, |pending| {
                load_extent(&path, pending)
            })
            .await
            .unwrap();
        let first_image = SealedPartition::reload(
            &query,
            anchor,
            first.as_ref(),
            SealedLimits::default(),
            None,
        )
        .unwrap();
        assert_snapshots(&query, &first_image);
        drop(first_image);
        drop(first);
        let other = cache
            .pin_async(&query, other_key, Admission::Normal, |pending| {
                load_extent(&path, pending)
            })
            .await
            .unwrap();
        assert_eq!(other.as_ref(), &other_bytes);
        drop(other);
        assert_eq!(cache.stats().evictions, 1);
        // Keeping the small anchor must not prevent its former frame's eviction.
        let restored = cache
            .pin_async(&query, image_key, Admission::Normal, |pending| {
                load_extent(&path, pending)
            })
            .await
            .unwrap();
        assert_eq!(cache.stats().evictions, 2);
        let restored_image = SealedPartition::reload(
            &query,
            anchor,
            restored.as_ref(),
            SealedLimits::default(),
            None,
        )
        .unwrap();
        assert_snapshots(&query, &restored_image);
        drop(restored_image);
        cache.forget_object(&query, image_key.object());
        assert_eq!(cache.resident_frames(), 0);
        assert!(pool.used() > 0, "the live handle still owns its charge");
        drop(restored);
        assert_eq!(pool.used(), 0);

        // Real disk corruption/short reads must never publish partially read
        // graph bytes, even when the caller still retains a legitimate anchor.
        query
            .with_restriction_async(async {
                let mut file = vfs
                    .open(&path, &OpenOptions::new().read(true).write(true))
                    .await
                    .unwrap();
                file.seek(SeekFrom::Start(offset)).await.unwrap();
                file.write_all(&[corrupt_byte]).await.unwrap();
                file.flush().await.unwrap();
            })
            .await;
        assert!(matches!(
            cache
                .pin_async(&query, image_key, Admission::Normal, |pending| load_extent(
                    &path, pending
                ))
                .await,
            Err(BufferError::ChecksumMismatch)
        ));
        assert_eq!((pool.used(), cache.resident_frames()), (0, 0));
        query
            .with_restriction_async(async {
                let file = vfs
                    .open(&path, &OpenOptions::new().write(true))
                    .await
                    .unwrap();
                file.set_len(offset + len as u64 - 1).await.unwrap();
            })
            .await;
        assert!(matches!(
            cache
                .pin_async(&query, image_key, Admission::Normal, |pending| load_extent(
                    &path, pending
                ))
                .await,
            Err(BufferError::Load(_))
        ));
        assert_eq!((pool.used(), cache.resident_frames()), (0, 0));
    });
}
