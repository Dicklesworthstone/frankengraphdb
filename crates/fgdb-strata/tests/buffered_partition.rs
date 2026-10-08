//! A cold source consumes the same durable objects as reopen, with independently
//! specified point/adjacency answers and a one-frame eviction/refault boundary.

use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_strata::edge_props::encode_property_patch;
use fgdb_strata::root::{BlockRef, PartitionRoot, PatchRef};
use fgdb_strata::store::{
    BlockStore, BufferedReadError, BufferedReadLimits, BufferedScanError, BufferedScanEvent,
    StoreError,
};
use fgdb_strata::tiered::buffer::{BufferError, BufferLimits};
use fgdb_strata::tiered::memory::{MemoryError, MemoryPool};
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

fn scan_vertex(vid: u128, created: u64, retired: Option<u64>, value: i64) -> VertexRow {
    VertexRow {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        ..vertex(created, retired, value)
    }
}

async fn vertex_scan_fixture(
    contexts: &PurposeContexts,
    dir: &std::path::Path,
) -> (BlockStore, PartitionRootVersion) {
    let commit = contexts.commit();
    let store = BlockStore::open(&commit, dir, KEY, NS).await.unwrap();
    let mut patches = Vec::new();
    for (rows, first, last) in [
        (
            vec![
                scan_vertex(1, 1, None, 10),
                scan_vertex(3, 2, None, 30),
                scan_vertex(5, 4, None, 50),
            ],
            1,
            4,
        ),
        (
            vec![
                scan_vertex(1, 1, Some(3), 10),
                scan_vertex(1, 3, None, 11),
                scan_vertex(2, 2, None, 20),
                scan_vertex(3, 2, Some(4), 30),
            ],
            1,
            4,
        ),
        (
            vec![
                scan_vertex(1, 3, Some(5), 11),
                scan_vertex(1, 5, None, 12),
                scan_vertex(4, 5, None, 40),
            ],
            3,
            5,
        ),
    ] {
        let patch = store
            .put_patch(&commit, &encode_patch(&rows).unwrap())
            .await
            .unwrap();
        patches.push(PatchRef {
            patch_id: patch.0,
            first_seq: CommitSeq(first),
            last_seq: CommitSeq(last),
        });
    }
    let root = PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(2),
        partition: 0,
        published_at: CommitSeq(5),
        blocks: vec![],
        vertex_patches: patches,
    };
    let id = store.put_root(&commit, &root).await.unwrap();
    (store, id)
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
fn vertex_scan_merges_history_in_identity_order_and_reports_invisible_candidates() {
    run(|contexts, dir| async move {
        let (store, id) = vertex_scan_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let expected = [
            vec![],
            vec![scan_vertex(1, 1, Some(3), 10)],
            vec![
                scan_vertex(1, 1, Some(3), 10),
                scan_vertex(2, 2, None, 20),
                scan_vertex(3, 2, Some(4), 30),
            ],
            vec![
                scan_vertex(1, 3, Some(5), 11),
                scan_vertex(2, 2, None, 20),
                scan_vertex(3, 2, Some(4), 30),
            ],
            vec![
                scan_vertex(1, 3, Some(5), 11),
                scan_vertex(2, 2, None, 20),
                scan_vertex(5, 4, None, 50),
            ],
            vec![
                scan_vertex(1, 5, None, 12),
                scan_vertex(2, 2, None, 20),
                scan_vertex(4, 5, None, 40),
                scan_vertex(5, 4, None, 50),
            ],
        ];
        for (cut, expected) in expected.into_iter().enumerate() {
            let mut scan = view.vertex_scan(&cx, CommitSeq(cut as u64)).unwrap();
            assert_eq!(scan.snapshot_seq(), CommitSeq(cut as u64));
            assert_eq!(scan.work_used(), 0, "construction does not visit history");
            let mut events = 0usize;
            let mut admitted = Vec::new();
            let mut observe = |event| {
                match event {
                    BufferedScanEvent::Work => events += 1,
                    BufferedScanEvent::Identity(vid) => admitted.push(vid),
                }
                Ok::<(), core::convert::Infallible>(())
            };
            let mut identities = Vec::new();
            let mut visible = Vec::new();
            while let Some(candidate) = scan.next_candidate_with(&cx, &mut observe).await.unwrap() {
                identities.push(candidate.vid);
                if let Some(row) = candidate.row {
                    visible.push(row.as_ref().clone());
                }
            }
            assert_eq!(identities, [VId(1), VId(2), VId(3), VId(4), VId(5)]);
            assert_eq!(admitted, identities, "exactly one admission per identity");
            assert_eq!(visible, expected, "historical cut {cut}");
            assert!(events > identities.len());
            assert_eq!(scan.work_used(), events);
            assert!(events <= 3 + 10 * (256 + 2));
            drop(scan);
            assert_eq!(
                pool.used(),
                baseline,
                "scan misses leave no resident frames"
            );
        }
        assert!(view.stats().bypasses > 0);
        assert_eq!(view.stats().evictions, 0);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_scan_is_lazy_and_does_not_rescan_every_patch_for_every_identity() {
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let cx = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let mut patches = Vec::new();
        let count = 32usize;
        for index in 1..=count {
            let patch = store
                .put_patch(
                    &commit,
                    &encode_patch(&[scan_vertex(index as u128, 1, None, index as i64)]).unwrap(),
                )
                .await
                .unwrap();
            patches.push(PatchRef {
                patch_id: patch.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            });
        }
        let id = store
            .put_root(
                &commit,
                &PartitionRoot {
                    graph: GraphId(1),
                    branch: BranchId(2),
                    partition: 0,
                    published_at: CommitSeq(1),
                    blocks: vec![],
                    vertex_patches: patches,
                },
            )
            .await
            .unwrap();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut bounded = limits();
        // Linear admission suffices for the entire cursor. A point-read loop
        // over every candidate would revisit 32 patches per output and refuse.
        bounded.max_work = count * 4;
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), bounded)
            .await
            .unwrap();
        let baseline = pool.used();
        drop(view.vertex_scan(&cx, CommitSeq(1)).unwrap());
        assert_eq!(view.stats().misses, 0);
        assert_eq!(pool.used(), baseline);
        let mut scan = view.vertex_scan(&cx, CommitSeq(1)).unwrap();
        let held = scan.next(&cx).await.unwrap().unwrap();
        assert_eq!(held.vid, VId(1));
        for index in 2..=count {
            let row = scan.next(&cx).await.unwrap().unwrap();
            assert_eq!(
                row.as_ref(),
                &scan_vertex(index as u128, 1, None, index as i64)
            );
        }
        assert!(scan.next(&cx).await.unwrap().is_none());
        assert_eq!(scan.work_used(), count * 4);
        drop(scan);
        assert_eq!(view.stats().misses, count as u64);
        assert_eq!(view.stats().bypasses, count as u64);
        drop(view);
        assert_eq!(pool.used(), held.charged_bytes());
        drop(held);
        assert_eq!(pool.used(), 0);

        bounded.max_work -= 1;
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), bounded)
            .await
            .unwrap();
        let mut scan = view.vertex_scan(&cx, CommitSeq(1)).unwrap();
        for _ in 1..count {
            assert!(scan.next(&cx).await.unwrap().is_some());
        }
        assert!(matches!(
            scan.next(&cx).await,
            Err(BufferedReadError::Limit {
                resource: "buffered source work",
                requested: 128,
                limit: 127,
            })
        ));
        assert!(
            scan.next(&cx).await.unwrap().is_none(),
            "a work error fuses the cursor"
        );
        drop(scan);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_scan_control_failure_at_every_source_event_fuses_and_refunds() {
    run(|contexts, dir| async move {
        let (store, id) = vertex_scan_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let total = {
            let mut scan = view.vertex_scan(&cx, CommitSeq(5)).unwrap();
            while scan.next(&cx).await.unwrap().is_some() {}
            scan.work_used()
        };
        for fail_at in 0..total {
            let mut scan = view.vertex_scan(&cx, CommitSeq(5)).unwrap();
            let mut observed = 0usize;
            let mut observe = |event| {
                if event == BufferedScanEvent::Work {
                    if observed == fail_at {
                        return Err(fail_at);
                    }
                    observed += 1;
                }
                Ok(())
            };
            loop {
                match scan.next_candidate_with(&cx, &mut observe).await {
                    Ok(Some(_)) => {}
                    Err(BufferedScanError::Control(at)) => {
                        assert_eq!(at, fail_at);
                        break;
                    }
                    other => panic!("expected control failure at {fail_at}, got {other:?}"),
                }
            }
            assert_eq!(scan.work_used(), fail_at);
            assert!(
                scan.next_candidate_with(&cx, &mut observe)
                    .await
                    .unwrap()
                    .is_none()
            );
            drop(scan);
            assert_eq!(pool.used(), baseline, "failed source event {fail_at}");
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_scan_heap_admission_and_corrupt_refault_never_expose_a_partial_winner() {
    run(|contexts, dir| async move {
        let (store, id) = vertex_scan_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let held = pool.reserve(&cx, pool.available() - 100).unwrap();
        assert!(matches!(
            view.vertex_scan(&cx, CommitSeq(5)),
            Err(BufferedReadError::Buffer(BufferError::Memory(
                MemoryError::ResourceExhausted { .. }
            )))
        ));
        assert_eq!(view.stats().misses, 0);
        drop(held);
        assert_eq!(pool.used(), baseline);
        let mut scan = view.vertex_scan(&cx, CommitSeq(5)).unwrap();
        let mut observe = |event| match event {
            BufferedScanEvent::Work => Ok(()),
            BufferedScanEvent::Identity(vid) => {
                assert_eq!(vid, VId(1));
                Err(7u8)
            }
        };
        assert!(matches!(
            scan.next_candidate_with(&cx, &mut observe).await,
            Err(BufferedScanError::Control(7))
        ));
        assert_eq!(scan.work_used(), 3, "only authenticated heads were visited");
        assert!(scan.next(&cx).await.unwrap().is_none());
        drop(scan);
        assert_eq!(
            view.stats().misses,
            0,
            "identity admission precedes all payload reads"
        );
        assert_eq!(pool.used(), baseline);
        assert!(matches!(
            view.vertex_scan(&cx, CommitSeq(6)),
            Err(BufferedReadError::BeyondPublication { .. })
        ));
        let path = store.path(view.root().vertex_patches[1].patch_id);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(path, bytes).unwrap();
        let mut scan = view.vertex_scan(&cx, CommitSeq(5)).unwrap();
        assert!(matches!(
            scan.next(&cx).await,
            Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
        ));
        assert!(scan.next(&cx).await.unwrap().is_none());
        drop(scan);
        assert_eq!(
            pool.used(),
            baseline,
            "even the first patch's candidate was released"
        );
        drop(view);
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
    // Each property patch must fit the store's per-object ceiling (16 KiB):
    // a 15,000-byte payload encodes to a 16,911-byte patch the store refuses.
    // 240 patches of this payload still exceed the 3 MiB shared ceiling.
    const PAYLOAD: usize = 14_000;
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let mut blocks = Vec::new();
        let mut source_bytes = 0usize;
        // Distinct payload identities prevent content-addressed deduplication
        // from making this merely a small object referenced many times.
        for index in 0..240u128 {
            let mut payload = vec![0x55; PAYLOAD];
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
            let mut expected = vec![0x55; PAYLOAD];
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

#[test]
fn vertex_property_history_larger_than_the_resident_cap_opens_and_refaults() {
    const COUNT: usize = 240;
    const PAYLOAD: usize = 14_000;
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let mut patches = Vec::new();
        let mut source_bytes = 0usize;
        for index in 1..=COUNT {
            let mut payload = vec![0x5a; PAYLOAD];
            payload[..8].copy_from_slice(&(index as u64).to_le_bytes());
            let row = VertexRow {
                props: vec![(PropertyKeyId(11), CanonicalScalar::bytes(payload).unwrap())],
                ..scan_vertex(index as u128, 1, None, 0)
            };
            let bytes = encode_patch(&[row]).unwrap();
            source_bytes += bytes.len();
            let patch = store.put_patch(&commit, &bytes).await.unwrap();
            patches.push(PatchRef {
                patch_id: patch.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            });
        }
        let id = store
            .put_root(
                &commit,
                &PartitionRoot {
                    graph: GraphId(1),
                    branch: BranchId(2),
                    partition: 0,
                    published_at: CommitSeq(1),
                    blocks: vec![],
                    vertex_patches: patches,
                },
            )
            .await
            .unwrap();
        let cap = 3 * 1024 * 1024;
        assert!(
            source_bytes > cap,
            "distinct durable payloads exceed the cap"
        );
        let pool = MemoryPool::new(cap, 0).unwrap();
        let mut bounded = limits();
        bounded.max_root_bytes = 16 * 1024;
        bounded.max_source_bytes = source_bytes;
        bounded.max_vertex_patches = COUNT;
        bounded.max_work = COUNT * 4;
        let mut view = store
            .open_buffered_root(&query, id, pool.clone(), bounded)
            .await
            .unwrap();
        assert_eq!(pool.used(), bounded.metadata_bytes().unwrap());
        for index in [1, COUNT, 1] {
            let found = view
                .vertex_at(&query, VId(index as u128), CommitSeq(1))
                .await
                .unwrap()
                .unwrap();
            let mut payload = vec![0x5a; PAYLOAD];
            payload[..8].copy_from_slice(&(index as u64).to_le_bytes());
            assert_eq!(
                found.props,
                vec![(PropertyKeyId(11), CanonicalScalar::bytes(payload).unwrap())]
            );
            assert!(pool.used() <= cap);
        }
        assert!(view.stats().evictions > 0);
        let mut scan = view.vertex_scan(&query, CommitSeq(1)).unwrap();
        for index in 1..=COUNT {
            let found = scan.next(&query).await.unwrap().unwrap();
            assert_eq!(found.vid, VId(index as u128));
            let mut payload = vec![0x5a; PAYLOAD];
            payload[..8].copy_from_slice(&(index as u64).to_le_bytes());
            assert_eq!(
                found.props,
                vec![(PropertyKeyId(11), CanonicalScalar::bytes(payload).unwrap())]
            );
        }
        assert!(scan.next(&query).await.unwrap().is_none());
        drop(scan);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_restatements_bound_residency_and_meter_every_authenticated_birth_refault() {
    const COUNT: usize = 240;
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let mut patches = Vec::new();
        let mut source_bytes = 0usize;
        let mut birth_bytes = 0usize;
        for index in 0..COUNT {
            let birth = VertexRow {
                props: vec![(
                    PropertyKeyId(11),
                    CanonicalScalar::bytes(vec![0x6b; 14_000]).unwrap(),
                )],
                ..vertex(1, None, 0)
            };
            // Every patch has a distinct keyed identity, even though its
            // large first statement is an exact restatement of the birth.
            let bytes =
                encode_patch(&[birth, scan_vertex(index as u128 + 2, 1, None, index as i64)])
                    .unwrap();
            source_bytes += bytes.len();
            if index == 0 {
                birth_bytes = bytes.len();
            }
            let patch = store.put_patch(&commit, &bytes).await.unwrap();
            patches.push(PatchRef {
                patch_id: patch.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            });
        }
        let id = store
            .put_root(
                &commit,
                &PartitionRoot {
                    graph: GraphId(1),
                    branch: BranchId(2),
                    partition: 0,
                    published_at: CommitSeq(1),
                    blocks: vec![],
                    vertex_patches: patches,
                },
            )
            .await
            .unwrap();
        let cap = 3 * 1024 * 1024;
        assert!(source_bytes > cap);
        let pool = MemoryPool::new(cap, 0).unwrap();
        let mut bounded = limits();
        bounded.max_root_bytes = 16 * 1024;
        bounded.max_vertex_patches = COUNT;
        bounded.max_source_bytes = source_bytes + (COUNT - 1) * birth_bytes;
        // Each incoming patch and each refault has one object plus two rows.
        bounded.max_work = 3 * COUNT + 3 * (COUNT - 1);
        let mut view = store
            .open_buffered_root(&query, id, pool.clone(), bounded)
            .await
            .unwrap();
        assert_eq!(pool.used(), bounded.metadata_bytes().unwrap());
        let found = view
            .vertex_at(&query, VId(1), CommitSeq(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            found.props,
            vec![(
                PropertyKeyId(11),
                CanonicalScalar::bytes(vec![0x6b; 14_000]).unwrap()
            )]
        );
        drop(found);
        drop(view);
        assert_eq!(pool.used(), 0);

        let mut too_little = bounded;
        too_little.max_work -= 1;
        assert!(matches!(
            store.open_buffered_root(&query, id, pool.clone(), too_little).await,
            Err(BufferedReadError::Limit { resource: "buffered source work", requested, limit })
                if requested == bounded.max_work && limit == bounded.max_work - 1
        ));
        assert_eq!(pool.used(), 0);
        too_little = bounded;
        too_little.max_source_bytes -= 1;
        assert!(matches!(
            store.open_buffered_root(&query, id, pool.clone(), too_little).await,
            Err(BufferedReadError::Limit { resource: "buffered source bytes", requested, limit })
                if requested == bounded.max_source_bytes && limit == bounded.max_source_bytes - 1
        ));
        assert_eq!(pool.used(), 0);
        let starved = MemoryPool::new(2 * 1024 * 1024, 0).unwrap();
        assert!(matches!(
            store
                .open_buffered_root(&query, id, starved.clone(), bounded)
                .await,
            Err(BufferedReadError::Memory(
                MemoryError::ResourceExhausted { .. }
            ))
        ));
        assert_eq!(starved.used(), 0);
    });
}

#[test]
fn compact_vertex_admission_matches_the_canonical_publication_history_validator() {
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        // None is an accepted history; false is an identity conflict; true is
        // a retirement conflict. Each case has its own explicit expected law.
        let cases = [
            (
                "retire then replace",
                vec![
                    vertex(1, None, 10),
                    vertex(1, Some(2), 10),
                    vertex(2, None, 20),
                ],
                None,
            ),
            (
                "repeat retired version",
                vec![vertex(1, Some(2), 10), vertex(1, Some(2), 10)],
                None,
            ),
            (
                "prepend contiguous history",
                vec![vertex(3, None, 30), vertex(1, Some(3), 10)],
                None,
            ),
            (
                "changed property",
                vec![vertex(1, None, 10), vertex(1, None, 99)],
                Some(false),
            ),
            (
                "changed labels",
                vec![
                    vertex(1, None, 10),
                    VertexRow {
                        labels: vec![LabelId(10)],
                        ..vertex(1, None, 10)
                    },
                ],
                Some(false),
            ),
            (
                "changed birth ordinal",
                vec![
                    vertex(1, None, 10),
                    VertexRow {
                        birth_ordinal: 8,
                        ..vertex(1, None, 10)
                    },
                ],
                Some(false),
            ),
            (
                "retirement removed",
                vec![vertex(1, Some(2), 10), vertex(1, None, 10)],
                Some(true),
            ),
            (
                "retirement moved",
                vec![vertex(1, Some(2), 10), vertex(1, Some(3), 10)],
                Some(true),
            ),
            (
                "successor gap",
                vec![vertex(1, Some(2), 10), vertex(3, None, 30)],
                Some(false),
            ),
            (
                "successor overlap",
                vec![vertex(1, Some(3), 10), vertex(2, None, 20)],
                Some(false),
            ),
            (
                "changed successor birth",
                vec![
                    vertex(1, Some(2), 10),
                    VertexRow {
                        birth_ordinal: 8,
                        ..vertex(2, None, 20)
                    },
                ],
                Some(false),
            ),
            (
                "late retirement is not a valid prefix",
                vec![
                    vertex(1, None, 10),
                    vertex(2, None, 20),
                    vertex(1, Some(2), 10),
                ],
                Some(false),
            ),
            (
                "prepended gap",
                vec![vertex(3, None, 30), vertex(1, Some(2), 10)],
                Some(false),
            ),
        ];
        for (name, history, expected) in cases {
            let mut patches = Vec::new();
            for (index, row) in history.into_iter().enumerate() {
                let first_seq = row.created_at;
                // Keep root publication ranges monotone even when testing a
                // malicious retirement removal or a prepended old version.
                let bytes =
                    encode_patch(&[row, scan_vertex(index as u128 + 100, 10, None, 0)]).unwrap();
                let patch = store.put_patch(&commit, &bytes).await.unwrap();
                patches.push(PatchRef {
                    patch_id: patch.0,
                    first_seq,
                    last_seq: CommitSeq(10),
                });
            }
            let root = PartitionRoot {
                graph: GraphId(1),
                branch: BranchId(2),
                partition: 0,
                published_at: CommitSeq(10),
                blocks: vec![],
                vertex_patches: patches,
            };
            let bytes = fgdb_strata::root::encode_root(&root).unwrap();
            let id = fgdb_strata::root::root_id(&KEY, NS, &bytes);
            std::fs::write(store.path(id), bytes).unwrap();
            let id = PartitionRootVersion(id);
            let ordinary = store.admit_root(&query, id).await;
            assert_eq!(ordinary.is_ok(), expected.is_none(), "ordinary: {name}");
            let pool = MemoryPool::new(3 * 1024 * 1024, 0).unwrap();
            let compact = store
                .open_buffered_root(&query, id, pool.clone(), limits())
                .await;
            match expected {
                None => assert!(compact.is_ok(), "compact: {name}"),
                Some(false) => assert!(
                    matches!(
                        &compact,
                        Err(BufferedReadError::VertexHistoryConflict { vid: VId(1) })
                    ),
                    "compact: {name}"
                ),
                Some(true) => assert!(
                    matches!(&compact, Err(BufferedReadError::Store(error))
                    if matches!(**error, StoreError::MalformedRoot(fgdb_strata::root::RootError::VertexRetirementMismatch { vid: VId(1), .. }))),
                    "compact: {name}"
                ),
            }
            drop(compact);
            assert_eq!(pool.used(), 0, "{name}");
        }
    });
}
