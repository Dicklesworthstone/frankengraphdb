use super::*;
use crate::edge_props::encode_property_patch;
use crate::root::{BlockRef, PartitionRoot, PatchRef};
use crate::store::{BlockStore, BufferedReadLimits};
use crate::tiered::buffer::BufferLimits;
use crate::tiered::memory::MemoryPool;
use crate::vertex::encode_patch;
use crate::{DeltaBlockVersion, PartitionRootVersion, encode_block, encode_block_with_properties};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{
    BranchId, CanonicalScalar, DatabaseSecurityNamespaceId, GraphId, PurposeContexts, VId,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(5);
const S: RelationId = RelationId(8);
const P: PropertyKeyId = PropertyKeyId(11);
static NEXT: AtomicU64 = AtomicU64::new(0);

fn run<F: Future<Output = ()> + Send>(
    test: impl FnOnce(PurposeContexts, PathBuf) -> F + Send + 'static,
) {
    let path = std::env::temp_dir().join(format!(
        "fgdb-edge-merge-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).unwrap();
    let (_, report) = run_async_under_lab(0xe6_01, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root), path).await;
    });
    assert!(report.invariant_violations.is_empty(), "{report:?}");
}
fn limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 4096,
        max_source_bytes: 128 * 1024,
        max_blocks: 32,
        max_vertex_patches: 32,
        max_work: 20_000,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}
fn entry(
    eid: u128,
    src: u128,
    dst: u128,
    relation: RelationId,
    created: u64,
    retired: Option<u64>,
) -> AdjacencyEntry {
    AdjacencyEntry {
        eid: EId(eid),
        src: VId(src),
        dst: VId(dst),
        relation,
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}
fn vertex(id: u128, created: u64, retired: Option<u64>, value: i64) -> VertexRow {
    VertexRow {
        vid: VId(id),
        birth_ordinal: id as u64,
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
        labels: vec![LabelId(9)],
        props: vec![(P, CanonicalScalar::Int(value))],
    }
}

async fn fixture(
    contexts: &PurposeContexts,
    path: &Path,
    missing_target: bool,
) -> (BlockStore, PartitionRootVersion) {
    let cx = contexts.commit();
    let store = BlockStore::open(
        &cx,
        path,
        [0x37; 32],
        DatabaseSecurityNamespaceId([0x63; 32]),
    )
    .await
    .unwrap();
    // Physical adjacency order deliberately differs from EId order in BOTH
    // families. Properties are positional, including the restated versions.
    let definitions = [
        (
            vec![
                entry(90, 1, 1, R, 1, None),
                entry(4, 1, 2, R, 1, None),
                entry(6, 1, 2, R, 2, None),
                entry(u128::MAX, 1, 3, R, 2, None),
            ],
            vec![90, 40, 60, 99],
            None,
        ),
        (
            vec![
                entry(2, 2, 1, S, 1, None),
                entry(0, 2, 2, S, 1, None),
                entry(8, 2, 2, S, 4, None),
            ],
            vec![20, 0, 80],
            None,
        ),
        (
            vec![
                entry(90, 1, 1, R, 1, Some(3)),
                entry(4, 1, 2, R, 1, Some(3)),
                entry(4, 1, 2, R, 3, None),
            ],
            vec![90, 40, 43],
            Some(0usize),
        ),
        (
            vec![entry(4, 1, 2, R, 3, Some(5)), entry(4, 1, 2, R, 5, None)],
            vec![43, 45],
            Some(2usize),
        ),
    ];
    let mut ids: Vec<DeltaBlockVersion> = Vec::new();
    let mut blocks = Vec::new();
    for (entries, values, predecessor) in definitions {
        let props: Vec<_> = values
            .into_iter()
            .map(|v| vec![(P, CanonicalScalar::Int(v))])
            .collect();
        let patch = store
            .put_edge_property_patch(&cx, &encode_property_patch(&props).unwrap())
            .await
            .unwrap();
        let locators: Vec<_> = (1..=entries.len()).map(|v| v as u8).collect();
        let encoded = encode_block_with_properties(
            0,
            predecessor.map(|at| ids[at]),
            &entries,
            patch.0,
            &locators,
            &props,
        )
        .unwrap();
        let block = store.put(&cx, &encoded).await.unwrap();
        blocks.push(BlockRef {
            block_id: block.0,
            first_seq: entries.iter().map(|r| r.created_at).min().unwrap(),
            last_seq: entries
                .iter()
                .map(|r| r.retired_at.unwrap_or(r.created_at).max(r.created_at))
                .max()
                .unwrap(),
        });
        ids.push(block);
    }
    let mut initial = vec![vertex(1, 1, None, 10), vertex(2, 1, None, 20)];
    if !missing_target {
        initial.push(vertex(3, 1, None, 30));
    }
    let mut patches = Vec::new();
    for rows in [
        initial,
        vec![vertex(1, 1, Some(3), 10), vertex(1, 3, None, 11)],
    ] {
        let patch = store
            .put_patch(&cx, &encode_patch(&rows).unwrap())
            .await
            .unwrap();
        patches.push(PatchRef {
            patch_id: patch.0,
            first_seq: rows.iter().map(|r| r.created_at).min().unwrap(),
            last_seq: rows
                .iter()
                .map(|r| r.retired_at.unwrap_or(r.created_at).max(r.created_at))
                .max()
                .unwrap(),
        });
    }
    let root = PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(2),
        partition: 0,
        published_at: CommitSeq(5),
        blocks,
        vertex_patches: patches,
    };
    let id = store.put_root(&cx, &root).await.unwrap();
    (store, id)
}

// Expected latest versions are stated independently, not computed by another
// heap/merge or by the point-read implementation under test.
fn expected(cut: u64) -> Vec<BufferedEdge> {
    let mut rows = Vec::new();
    if cut >= 1 {
        rows.push((entry(0, 2, 2, S, 1, None), 0));
        rows.push((entry(2, 2, 1, S, 1, None), 20));
        rows.push(if cut < 3 {
            (entry(4, 1, 2, R, 1, Some(3)), 40)
        } else if cut < 5 {
            (entry(4, 1, 2, R, 3, Some(5)), 43)
        } else {
            (entry(4, 1, 2, R, 5, None), 45)
        });
    }
    if cut >= 2 {
        rows.push((entry(6, 1, 2, R, 2, None), 60));
    }
    if cut >= 4 {
        rows.push((entry(8, 2, 2, S, 4, None), 80));
    }
    if (1..3).contains(&cut) {
        rows.push((entry(90, 1, 1, R, 1, Some(3)), 90));
    }
    if cut >= 2 {
        rows.push((entry(u128::MAX, 1, 3, R, 2, None), 99));
    }
    rows.into_iter()
        .map(|(entry, value)| BufferedEdge {
            entry,
            props: vec![(P, CanonicalScalar::Int(value))],
        })
        .collect()
}

#[test]
fn controlled_heap_and_local_permutations_match_independent_standard_order() {
    let mut seed = 17u64;
    for count in 0..=256usize {
        let mut values: Vec<Key> = (0..count)
            .map(|at| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                (
                    EId(if at == 0 {
                        u128::MAX
                    } else {
                        u128::from(seed % 17)
                    }),
                    CommitSeq(seed % 5),
                    at,
                )
            })
            .collect();
        let mut expected = values.clone();
        expected.sort_unstable();
        let mut heap = Vec::with_capacity(count);
        for value in values.iter().copied() {
            heap_push(&mut heap, Reverse(value), &mut || Ok::<_, ()>(())).unwrap();
        }
        let mut actual = Vec::new();
        while !heap.is_empty() {
            actual.push(heap_pop(&mut heap, &mut || Ok::<_, ()>(())).unwrap().0);
        }
        assert_eq!(actual, expected);
        sort(&mut values, &mut || Ok::<_, ()>(())).unwrap();
        assert_eq!(values, expected);
    }
}

#[test]
fn snapshots_preserve_properties_parallel_edges_and_invisible_candidate_admission() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        drop(view.edge_scan(&cx, CommitSeq(5)).unwrap());
        assert_eq!(view.stats().misses, 0);
        assert_eq!(pool.used(), baseline);
        for cut in 0..=5 {
            let mut scan = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
            assert_eq!(scan.work_used(), 0);
            let (mut work, mut admissions) = (0, Vec::new());
            let mut returned = Vec::new();
            let mut visible = Vec::new();
            while let Some(candidate) = scan
                .next_candidate_with(&cx, &mut |event| {
                    match event {
                        BufferedEdgeScanEvent::Work => work += 1,
                        BufferedEdgeScanEvent::Identity(eid) => admissions.push(eid),
                    }
                    Ok::<_, ()>(())
                })
                .await
                .unwrap()
            {
                returned.push(candidate.eid);
                if let Some(row) = candidate.row {
                    visible.push(row.as_ref().clone());
                }
            }
            assert_eq!(
                returned,
                [
                    EId(0),
                    EId(2),
                    EId(4),
                    EId(6),
                    EId(8),
                    EId(90),
                    EId(u128::MAX)
                ]
            );
            assert_eq!(admissions, returned);
            assert_eq!(visible, expected(cut), "cut {cut}");
            assert_eq!(scan.work_used(), work);
            assert!(scan.is_finished());
            assert!(
                scan.next_candidate_with(&cx, &mut |_| Err::<(), _>("must not run"))
                    .await
                    .unwrap()
                    .is_none()
            );
            drop(scan);
            assert_eq!(
                pool.used(),
                baseline,
                "scan bypass leaves no resident payload"
            );
        }
        assert!(view.stats().bypasses > 0);
        assert_eq!(view.stats().evictions, 0);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn endpoint_images_share_the_cut_and_self_loops_share_one_allocation() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        for cut in 0..=5 {
            let mut scan = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
            let mut edges = Vec::new();
            while let Some(candidate) = scan
                .next_with_endpoints(&cx, None, &mut |_| Ok::<_, ()>(()))
                .await
                .unwrap()
            {
                let Some(record) = candidate.row else {
                    continue;
                };
                edges.push(record.edge().clone());
                for row in [record.source_vertex(), record.target_vertex()] {
                    let value = if row.vid == VId(1) && cut >= 3 {
                        11
                    } else {
                        row.vid.0 as i64 * 10
                    };
                    assert_eq!(row.props, vec![(P, CanonicalScalar::Int(value))]);
                    assert!(row.visible_at(CommitSeq(cut)));
                }
                assert_eq!(record.source_vertex().vid, record.edge().entry.src);
                assert_eq!(record.target_vertex().vid, record.edge().entry.dst);
                assert_eq!(
                    core::ptr::eq(record.source_vertex(), record.target_vertex()),
                    record.edge().entry.src == record.edge().entry.dst
                );
            }
            assert_eq!(edges, expected(cut));
        }
        let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
        let held = scan
            .next_with_endpoints(&cx, None, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap()
            .row
            .unwrap();
        drop(scan);
        drop(view);
        assert!(
            pool.used() > 0,
            "record owns all three possible reservations"
        );
        assert_eq!(held.edge().entry.eid, EId(0));
        drop(held);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn every_history_heap_property_and_endpoint_control_refuses_without_resuming_a_prefix() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let mut total = 0;
        {
            let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
            while scan
                .next_with_endpoints(&cx, None, &mut |_| {
                    total += 1;
                    Ok::<_, usize>(())
                })
                .await
                .unwrap()
                .is_some()
            {}
        }
        for stop in 0..total {
            let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
            let mut count = 0;
            loop {
                let result = scan
                    .next_with_endpoints(&cx, None, &mut |_| {
                        let at = count;
                        count += 1;
                        if at == stop { Err(stop) } else { Ok(()) }
                    })
                    .await;
                match result {
                    Ok(Some(_)) => {}
                    Err(BufferedScanError::Control(at)) => {
                        assert_eq!(at, stop);
                        break;
                    }
                    other => panic!("expected refusal {stop}, got {other:?}"),
                }
            }
            assert!(scan.is_finished());
            assert!(
                scan.next_candidate_with(&cx, &mut |_| -> Result<(), ()> {
                    panic!("fused source")
                })
                .await
                .unwrap()
                .is_none()
            );
            drop(scan);
            assert_eq!(pool.used(), baseline, "control {stop}");
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn metadata_and_candidate_admission_precede_payloads_and_future_cuts_refuse() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let held = pool.reserve(&cx, pool.available() - 100).unwrap();
        assert!(matches!(
            view.edge_scan(&cx, CommitSeq(5)),
            Err(BufferedReadError::Buffer(BufferError::Memory(_)))
        ));
        drop(held);
        assert_eq!(pool.used(), baseline);
        assert!(matches!(
            view.edge_scan(&cx, CommitSeq(6)),
            Err(BufferedReadError::BeyondPublication { .. })
        ));
        let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
        drop(scan.next(&cx)); // unpolled: not terminal and no storage demand
        assert!(!scan.is_finished());
        assert_eq!(scan.work_used(), 0);
        assert!(matches!(
            scan.next_candidate_with(&cx, &mut |event| {
                if let BufferedEdgeScanEvent::Identity(eid) = event {
                    assert_eq!(eid, EId(0));
                    Err(7)
                } else {
                    Ok(())
                }
            })
            .await,
            Err(BufferedScanError::Control(7))
        ));
        assert!(scan.is_finished());
        drop(scan);
        assert_eq!(view.stats().misses, 0);
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn late_corrupt_restatement_never_publishes_an_older_winning_edge() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let path = store.path(view.root().blocks[3].block_id);
        let mut bytes = std::fs::read(&path).unwrap();
        let end = bytes.len() - 1;
        bytes[end] ^= 1;
        std::fs::write(&path, bytes).unwrap(); // isolated fixture fault only
        let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
        assert_eq!(scan.next(&cx).await.unwrap().unwrap().entry.eid, EId(0));
        assert_eq!(scan.next(&cx).await.unwrap().unwrap().entry.eid, EId(2));
        assert!(matches!(
            scan.next(&cx).await,
            Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
        ));
        assert!(scan.next(&cx).await.unwrap().is_none());
        drop(scan);
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn missing_endpoints_are_errors_only_for_visible_selected_edges() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, true).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        for (cut, relation, refuses) in [
            (0, None, false),
            (1, None, false),
            (2, Some(S), false),
            (2, None, true),
        ] {
            let mut scan = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
            let mut failed = false;
            loop {
                match scan
                    .next_with_endpoints(&cx, relation, &mut |_| Ok::<_, ()>(()))
                    .await
                {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(BufferedScanError::Read(BufferedReadError::DanglingEndpoint)) => {
                        failed = true;
                        break;
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
            assert_eq!(failed, refuses);
            assert!(scan.is_finished());
            drop(scan);
            assert_eq!(pool.used(), baseline);
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn one_row_blocks_are_read_once_and_the_source_budget_is_cumulative() {
    run(|contexts, path| async move {
        let commit = contexts.commit();
        let cx = contexts.query();
        let store = BlockStore::open(
            &commit,
            &path,
            [0x37; 32],
            DatabaseSecurityNamespaceId([0x63; 32]),
        )
        .await
        .unwrap();
        let count = 160usize;
        let mut blocks = Vec::new();
        for at in 0..count {
            let row = entry((count - at) as u128, at as u128 + 1, 999, R, 1, None);
            let block = store
                .put(&commit, &encode_block(0, None, &[row]).unwrap())
                .await
                .unwrap();
            blocks.push(BlockRef {
                block_id: block.0,
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
                    blocks,
                    vertex_patches: vec![],
                },
            )
            .await
            .unwrap();
        let mut bounds = limits();
        bounds.max_blocks = count;
        bounds.max_root_bytes = 16 * 1024;
        bounds.max_work = count * 40; // well below a point lookup per EId's N*N visits
        let pool = MemoryPool::new(4 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), bounds)
            .await
            .unwrap();
        let baseline = pool.used();
        let mut scan = view.edge_scan(&cx, CommitSeq(1)).unwrap();
        for expected in 1..=count {
            let found = scan.next(&cx).await.unwrap().unwrap();
            assert_eq!(found.entry.eid, EId(expected as u128));
        }
        assert!(scan.next(&cx).await.unwrap().is_none());
        let total = scan.work_used();
        assert!(total < count * count);
        drop(scan);
        assert_eq!(view.stats().misses, count as u64);
        assert_eq!(view.stats().bypasses, count as u64);
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
        bounds.max_work = total - 1;
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), bounds)
            .await
            .unwrap();
        let mut scan = view.edge_scan(&cx, CommitSeq(1)).unwrap();
        let mut delivered = 0;
        loop {
            match scan.next(&cx).await {
                Ok(Some(_)) => delivered += 1,
                Err(BufferedReadError::Limit {
                    requested, limit, ..
                }) => {
                    assert_eq!((requested, limit), (total, total - 1));
                    break;
                }
                other => panic!("expected cumulative refusal, got {other:?}"),
            }
        }
        assert!(delivered < count);
        assert!(scan.next(&cx).await.unwrap().is_none());
        drop(scan);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}
