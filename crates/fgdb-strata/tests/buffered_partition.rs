//! A cold source consumes the same durable objects as reopen, with independently
//! specified point/adjacency answers and a one-frame eviction/refault boundary.

use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_strata::edge_props::encode_property_patch;
use fgdb_strata::root::{BlockRef, PartitionRoot, PatchRef};
use fgdb_strata::store::{BlockStore, BufferedReadError, BufferedReadLimits, StoreError};
use fgdb_strata::tiered::buffer::{BufferError, BufferLimits};
use fgdb_strata::tiered::memory::MemoryPool;
use fgdb_strata::vertex::{VertexRow, encode_patch};
use fgdb_strata::{
    AdjacencyEntry, PartitionRootVersion, encode_block, encode_block_with_properties,
};
use fgdb_types::{
    BranchId, CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, GraphId,
    PurposeContexts, VId,
};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const KEY: [u8; 32] = [0x31; 32];
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x52; 32]);
const REL: RelationId = RelationId(5);
static NEXT: AtomicU64 = AtomicU64::new(0);

fn run<Fut>(test: impl FnOnce(PurposeContexts, PathBuf) -> Fut + Send + 'static)
where
    Fut: Future<Output = ()> + Send,
{
    let dir = std::env::temp_dir().join(format!(
        "fgdb-cold-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).unwrap();
    let (_, report) = run_async_under_lab(0xb0_0f, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root), dir).await;
    });
    assert!(report.invariant_violations.is_empty(), "{report:?}");
}

fn limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 4096,
        max_source_bytes: 64 * 1024,
        max_blocks: 32,
        max_vertex_patches: 32,
        max_work: 1024,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}

fn edge(eid: u128, src: u128, dst: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        eid: EId(eid),
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

fn vertex(created: u64, retired: Option<u64>, value: i64) -> VertexRow {
    VertexRow {
        vid: VId(1),
        birth_ordinal: 7,
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
        labels: vec![LabelId(9)],
        props: vec![(PropertyKeyId(11), CanonicalScalar::Int(value))],
    }
}

async fn fixture(
    contexts: &PurposeContexts,
    dir: &std::path::Path,
) -> (BlockStore, PartitionRootVersion) {
    let cx = contexts.commit();
    let store = BlockStore::open(&cx, dir, KEY, NS).await.unwrap();
    let first = store
        .put(
            &cx,
            &encode_block(0, None, &[edge(1, 1, 2, 1, None)]).unwrap(),
        )
        .await
        .unwrap();
    let other = store
        .put(
            &cx,
            &encode_block(0, None, &[edge(2, 3, 2, 2, None)]).unwrap(),
        )
        .await
        .unwrap();
    let rows = vec![vec![(PropertyKeyId(11), CanonicalScalar::Int(33))]];
    let properties = store
        .put_edge_property_patch(&cx, &encode_property_patch(&rows).unwrap())
        .await
        .unwrap();
    let last = store
        .put(
            &cx,
            &encode_block_with_properties(
                0,
                Some(first),
                &[edge(1, 1, 2, 1, Some(3)), edge(1, 1, 2, 3, None)],
                properties.0,
                &[0, 1],
                &rows,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let old = store
        .put_patch(&cx, &encode_patch(&[vertex(1, None, 10)]).unwrap())
        .await
        .unwrap();
    let new = store
        .put_patch(
            &cx,
            &encode_patch(&[vertex(1, Some(3), 10), vertex(3, None, 30)]).unwrap(),
        )
        .await
        .unwrap();
    let root = PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(2),
        partition: 0,
        published_at: CommitSeq(3),
        blocks: vec![
            BlockRef {
                block_id: first.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            },
            BlockRef {
                block_id: other.0,
                first_seq: CommitSeq(2),
                last_seq: CommitSeq(2),
            },
            BlockRef {
                block_id: last.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(3),
            },
        ],
        vertex_patches: vec![
            PatchRef {
                patch_id: old.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            },
            PatchRef {
                patch_id: new.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(3),
            },
        ],
    };
    let id = store.put_root(&cx, &root).await.unwrap();
    (store, id)
}

#[test]
fn cold_history_points_and_reverse_adjacency_survive_one_frame_refaults() {
    run(|contexts, dir| async move {
        let (store, id) = fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        assert_eq!(view.root_id(), id);
        assert_eq!(
            view.stats().misses,
            0,
            "admission did not retain payload frames"
        );
        let baseline = pool.used();
        for cut in [1, 3, 2, 3] {
            let row = view
                .vertex_at(&cx, VId(1), CommitSeq(cut))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                row.as_ref(),
                &vertex(
                    if cut < 3 { 1 } else { 3 },
                    if cut < 3 { Some(3) } else { None },
                    if cut < 3 { 10 } else { 30 }
                )
            );
            drop(row);
            let found = view
                .edge_at(&cx, EId(1), CommitSeq(cut))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                found.entry,
                edge(
                    1,
                    1,
                    2,
                    if cut < 3 { 1 } else { 3 },
                    if cut < 3 { Some(3) } else { None }
                )
            );
            assert_eq!(
                found.props,
                if cut < 3 {
                    vec![]
                } else {
                    vec![(PropertyKeyId(11), CanonicalScalar::Int(33))]
                }
            );
        }
        let found = view
            .adjacency_at(&cx, VId(2), None, true, CommitSeq(3), 2)
            .await
            .unwrap();
        assert_eq!(
            found.as_ref(),
            &vec![edge(1, 1, 2, 3, None), edge(2, 3, 2, 2, None)]
        );
        drop(found);
        assert!(view.stats().evictions > 0);
        assert!(view.stats().misses > 1);
        assert!(
            pool.used() < baseline + 128 * 1024,
            "only one small resident extent remains"
        );
        let held = view
            .vertex_at(&cx, VId(1), CommitSeq(3))
            .await
            .unwrap()
            .unwrap();
        drop(view);
        assert!(
            pool.used() > 0,
            "answer owns its charge independently of the view"
        );
        drop(held);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn admission_memory_history_and_result_limits_never_return_a_partial_source() {
    run(|contexts, dir| async move {
        let (store, id) = fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let too_small = MemoryPool::new(1, 0).unwrap();
        assert!(matches!(
            store
                .open_buffered_root(&cx, id, too_small.clone(), limits())
                .await,
            Err(BufferedReadError::Memory(_))
        ));
        assert_eq!(too_small.used(), 0);
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut small = limits();
        small.max_blocks = 2;
        assert!(
            matches!(store.open_buffered_root(&cx, id, pool.clone(), small).await,
            Err(BufferedReadError::Store(error)) if matches!(*error, StoreError::RootReferenceLimit { .. }))
        );
        assert_eq!(pool.used(), 0);
        small = limits();
        small.max_source_bytes = 1;
        assert!(matches!(
            store.open_buffered_root(&cx, id, pool.clone(), small).await,
            Err(BufferedReadError::Limit { .. })
        ));
        assert_eq!(pool.used(), 0);
        small = limits();
        small.max_work = 1;
        assert!(matches!(
            store.open_buffered_root(&cx, id, pool.clone(), small).await,
            Err(BufferedReadError::Limit { .. })
        ));
        assert_eq!(pool.used(), 0);
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        assert!(matches!(
            view.adjacency_at(&cx, VId(2), None, true, CommitSeq(3), 1)
                .await,
            Err(BufferedReadError::Limit {
                resource: "buffered adjacency identities",
                ..
            })
        ));
        assert!(matches!(
            view.vertex_at(&cx, VId(1), CommitSeq(4)).await,
            Err(BufferedReadError::BeyondPublication { .. })
        ));
        assert!(
            view.edge_at(&cx, EId(99), CommitSeq(3))
                .await
                .unwrap()
                .is_none()
        );
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn corrupt_cold_refault_is_rejected_and_refunds_the_pending_frame() {
    run(|contexts, dir| async move {
        let (store, id) = fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let block = view.root().blocks[0].block_id;
        // The corruption is fixture injection, never a database write path.
        let path = store.path(block);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        let before = pool.used();
        assert!(matches!(
            view.edge_at(&cx, EId(1), CommitSeq(3)).await,
            Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
        ));
        assert_eq!(pool.used(), before);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn propertied_durable_source_can_exceed_the_shared_resident_ceiling() {
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let mut blocks = Vec::new();
        let mut source_bytes = 0usize;
        // Distinct payload identities prevent content-addressed deduplication
        // from making this merely a small object referenced many times.
        for index in 0..240u128 {
            let mut payload = vec![0x55; 15_000];
            payload[..16].copy_from_slice(&index.to_le_bytes());
            let rows = vec![vec![(
                PropertyKeyId(11),
                CanonicalScalar::bytes(payload).unwrap(),
            )]];
            let property_bytes = encode_property_patch(&rows).unwrap();
            let property = store
                .put_edge_property_patch(&commit, &property_bytes)
                .await
                .unwrap();
            let bytes = encode_block_with_properties(
                0,
                None,
                &[edge(index + 1, index + 1, 999, 1, None)],
                property.0,
                &[1],
                &rows,
            )
            .unwrap();
            source_bytes += property_bytes.len() + bytes.len();
            let block = store.put(&commit, &bytes).await.unwrap();
            blocks.push(BlockRef {
                block_id: block.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            });
        }
        let root = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(2),
            partition: 0,
            published_at: CommitSeq(1),
            blocks,
            vertex_patches: vec![],
        };
        let id = store.put_root(&commit, &root).await.unwrap();
        let cap = 3 * 1024 * 1024;
        assert!(source_bytes > cap);
        let pool = MemoryPool::new(cap, 0).unwrap();
        let mut bounded = limits();
        bounded.max_root_bytes = 16 * 1024;
        bounded.max_source_bytes = source_bytes;
        bounded.max_blocks = 240;
        bounded.max_work = 240 * 4;
        let mut view = store
            .open_buffered_root(&query, id, pool.clone(), bounded)
            .await
            .unwrap();
        for eid in [1, 240, 1] {
            let found = view
                .edge_at(&query, EId(eid), CommitSeq(1))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(found.entry, edge(eid, eid, 999, 1, None));
            let mut expected = vec![0x55; 15_000];
            expected[..16].copy_from_slice(&(eid - 1).to_le_bytes());
            assert_eq!(
                found.props,
                vec![(PropertyKeyId(11), CanonicalScalar::bytes(expected).unwrap())]
            );
            assert!(pool.used() <= cap);
        }
        assert!(view.stats().evictions > 0);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn a_keyed_root_cannot_bypass_cross_object_identity_history() {
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let first = store
            .put(
                &commit,
                &encode_block(0, None, &[edge(9, 1, 2, 1, None)]).unwrap(),
            )
            .await
            .unwrap();
        let alias = store
            .put(
                &commit,
                &encode_block(0, None, &[edge(9, 3, 4, 1, None)]).unwrap(),
            )
            .await
            .unwrap();
        let old = store
            .put_patch(&commit, &encode_patch(&[vertex(1, None, 10)]).unwrap())
            .await
            .unwrap();
        let changed = store
            .put_patch(&commit, &encode_patch(&[vertex(1, None, 99)]).unwrap())
            .await
            .unwrap();
        for edge_conflict in [true, false] {
            let root = PartitionRoot {
                graph: GraphId(1),
                branch: BranchId(2),
                partition: 0,
                published_at: CommitSeq(1),
                blocks: if edge_conflict {
                    [first, alias]
                        .into_iter()
                        .map(|id| BlockRef {
                            block_id: id.0,
                            first_seq: CommitSeq(1),
                            last_seq: CommitSeq(1),
                        })
                        .collect()
                } else {
                    vec![]
                },
                vertex_patches: if edge_conflict {
                    vec![]
                } else {
                    [old, changed]
                        .into_iter()
                        .map(|id| PatchRef {
                            patch_id: id.0,
                            first_seq: CommitSeq(1),
                            last_seq: CommitSeq(1),
                        })
                        .collect()
                },
            };
            // Bypass publication in the fixture to prove open itself enforces
            // history. Both objects and the root have valid keyed identities.
            let bytes = fgdb_strata::root::encode_root(&root).unwrap();
            let id = fgdb_strata::root::root_id(&KEY, NS, &bytes);
            std::fs::write(store.path(id), bytes).unwrap();
            let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
            let result = store
                .open_buffered_root(&query, PartitionRootVersion(id), pool.clone(), limits())
                .await;
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("conflicting keyed history must refuse"),
            };
            if edge_conflict {
                assert!(matches!(&error, BufferedReadError::Store(error)
                    if matches!(**error, StoreError::MalformedRoot(fgdb_strata::root::RootError::EdgeIdentityMismatch { .. }))));
            } else {
                assert!(matches!(
                    &error,
                    BufferedReadError::VertexHistoryConflict { vid: VId(1) }
                ));
            }
            assert_eq!(pool.used(), 0);
            // The compact error can outlive the reader without holding graph
            // payloads whose reservations have already been refunded.
            drop(error);
        }
    });
}
