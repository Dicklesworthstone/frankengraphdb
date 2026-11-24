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
    BranchId, CanonicalPropertyValue, CanonicalScalar, DatabaseSecurityNamespaceId, GraphId,
    PurposeContexts, VId,
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
        props: vec![(
            P,
            CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value)),
        )],
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
        // Publication order keeps every block's upper frontier
        // non-decreasing (2, 3, 4, 5), as the root's validation requires.
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
            vec![
                entry(2, 2, 1, S, 1, None),
                entry(0, 2, 2, S, 1, None),
                entry(8, 2, 2, S, 4, None),
            ],
            vec![20, 0, 80],
            None,
        ),
        (
            vec![entry(4, 1, 2, R, 3, Some(5)), entry(4, 1, 2, R, 5, None)],
            vec![43, 45],
            Some(1usize),
        ),
    ];
    let mut ids: Vec<DeltaBlockVersion> = Vec::new();
    let mut blocks = Vec::new();
    for (entries, values, predecessor) in definitions {
        let props: Vec<_> = values
            .into_iter()
            .map(|v| vec![(P, CanonicalPropertyValue::Scalar(CanonicalScalar::Int(v)))])
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
            props: vec![(
                P,
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value)),
            )],
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
fn compact_orders_restore_full_width_history_and_original_property_positions() {
    let mut seed = 19u128;
    for count in 0..=crate::MAX_BLOCK_ENTRIES as usize {
        let entries: Vec<_> = (0..count)
            .map(|at| {
                seed = seed
                    .wrapping_mul(0x2360_ed05_1fc6_5da4_4385_df64_9fcc_f645)
                    .wrapping_add(1);
                entry(
                    match at % 7 {
                        0 => u128::MAX,
                        1 => 0,
                        2 | 3 => 1u128 << 100,
                        _ => seed,
                    },
                    (at % 3) as u128,
                    (at % 5) as u128,
                    R,
                    (at % 4) as u64,
                    None,
                )
            })
            .collect();
        // Whole-root, outgoing and incoming incidence selections. A retained
        // incidence's predicate is fixed even as its after-position advances.
        for filter in 0..3 {
            let mut expected: Vec<Key> = entries
                .iter()
                .enumerate()
                .filter(|(_, row)| match filter {
                    1 => row.src == VId(1),
                    2 => row.dst == VId(2),
                    _ => true,
                })
                .map(|(at, row)| (row.eid, row.created_at, at))
                .collect();
            expected.sort_unstable();
            let positions =
                compact_positions(&expected, 256, &mut || Ok::<_, BufferedScanError<()>>(()))
                    .unwrap();
            assert_eq!(positions.len(), expected.len());
            let mut actual = Vec::with_capacity(count);
            restore_positions(&entries, &positions, &mut actual, &mut || {
                Ok::<_, BufferedScanError<()>>(())
            })
            .unwrap();
            assert_eq!(actual, expected, "count {count}, selection {filter}");
            // Property locators still address the original encoded image,
            // including equal (identity, birth) statements and position255.
            for (key, position) in actual.iter().zip(positions) {
                assert_eq!(key.2, usize::from(position));
                assert_eq!(
                    (key.0, key.1),
                    (entries[key.2].eid, entries[key.2].created_at)
                );
            }
        }
    }
}

#[test]
fn compact_order_controls_refuse_every_prefix_without_publishing_a_memo() {
    let entries: Vec<_> = (0..256)
        .map(|at| entry(255 - at, 1, at, R, 1, None))
        .collect();
    let order: Vec<Key> = (0..256)
        .rev()
        .map(|at| (entries[at].eid, entries[at].created_at, at))
        .collect();
    let positions =
        compact_positions(&order, 256, &mut || Ok::<_, BufferedScanError<usize>>(())).unwrap();
    for stop in 0..256 {
        let mut calls = 0;
        let result = compact_positions(&order, 256, &mut || {
            let at = calls;
            calls += 1;
            if at == stop {
                Err(BufferedScanError::Control(at))
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(BufferedScanError::Control(at)) if at == stop));
        assert_eq!(calls, stop + 1);
        let mut restored = Vec::with_capacity(256);
        let mut calls = 0;
        let result = restore_positions(&entries, &positions, &mut restored, &mut || {
            let at = calls;
            calls += 1;
            if at == stop {
                Err(BufferedScanError::Control(at))
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(BufferedScanError::Control(at)) if at == stop));
        assert_eq!(calls, stop + 1);
        assert_eq!(restored, order[..stop]);
    }
}

#[test]
fn compact_order_refuses_short_reservations_and_invalid_positions() {
    let mut control = || Ok::<_, BufferedScanError<()>>(());
    let max: Vec<Key> = (0..256)
        .map(|at| (EId(at as u128), CommitSeq(1), at))
        .collect();
    assert!(matches!(
        compact_positions(&max, 255, &mut control),
        Err(BufferedScanError::Read(BufferedReadError::SizeOverflow))
    ));
    assert!(matches!(
        compact_positions(&[(EId(0), CommitSeq(1), 256)], 256, &mut control),
        Err(BufferedScanError::Read(BufferedReadError::SizeOverflow))
    ));
    let entries = [entry(0, 1, 2, R, 1, None)];
    assert!(matches!(
        restore_positions(&entries, &[1], &mut Vec::new(), &mut control),
        Err(BufferedScanError::Read(BufferedReadError::Buffer(
            BufferError::InvalidLoad
        )))
    ));
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
                match scan.state.as_ref() {
                    Some(state) => {
                        assert_ne!(candidate.eid, EId(u128::MAX));
                        state.check_retained_bytes().unwrap();
                        if candidate.eid == EId(0) {
                            assert_eq!(state.order_bytes, 0, "first load retains no memo");
                            assert!(state.orders.iter().all(Option::is_none));
                        }
                        if candidate.eid == EId(4) {
                            assert!(
                                state.order_bytes > 0,
                                "interleaved unfinished blocks reuse orders"
                            );
                        }
                    }
                    None => {
                        // The last candidate owns its value independently;
                        // an empty merge releases its heads before delivery.
                        assert_eq!(candidate.eid, EId(u128::MAX));
                        assert!(scan.is_finished());
                    }
                }
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
fn relation_roots_preserve_complete_histories_and_reduce_payload_faults() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        for relation in [R, S, RelationId(u64::MAX)] {
            let scan = view
                .edge_scan_relation(&cx, CommitSeq(5), relation)
                .unwrap();
            assert_eq!(scan.state.as_ref().unwrap().heads.capacity(), 0);
            assert!(scan.state.as_ref().unwrap()._head_charge.is_none());
            assert_eq!(pool.used(), baseline, "unused roots retain no workspace");
            assert_eq!(scan.work_used(), 0);
            drop(scan);
            assert_eq!(view.stats().misses, 0, "opening never faults a payload");
            assert_eq!(pool.used(), baseline);
        }
        for cut in 0..=5 {
            let before = view.stats();
            let mut all = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
            let mut all_rows = Vec::new();
            while let Some(row) = all.next(&cx).await.unwrap() {
                all_rows.push(row.as_ref().clone());
            }
            let all_work = all.work_used();
            drop(all);
            let all_faults = view.stats().misses - before.misses;
            assert_eq!(all_rows, expected(cut));
            for relation in [R, S, RelationId(u64::MAX)] {
                let before = view.stats();
                let mut scan = view
                    .edge_scan_relation(&cx, CommitSeq(cut), relation)
                    .unwrap();
                let mut admitted = Vec::new();
                let mut visible = Vec::new();
                while let Some(candidate) = scan
                    .next_candidate_with(&cx, &mut |event| {
                        if let BufferedEdgeScanEvent::Identity(eid) = event {
                            admitted.push(eid);
                        }
                        Ok::<_, ()>(())
                    })
                    .await
                    .unwrap()
                {
                    if let Some(row) = candidate.row {
                        visible.push(row.as_ref().clone());
                    }
                    if let Some(state) = &scan.state {
                        let selected_blocks = if relation == R { 3 } else { 1 };
                        assert_eq!(
                            state._head_charge.as_ref().unwrap().bytes(),
                            1024 + selected_blocks * 512
                        );
                    }
                }
                let expected_ids = if relation == R {
                    vec![EId(4), EId(6), EId(90), EId(u128::MAX)]
                } else if relation == S {
                    vec![EId(0), EId(2), EId(8)]
                } else {
                    vec![]
                };
                assert_eq!(admitted, expected_ids, "all histories at cut {cut}");
                assert_eq!(
                    visible,
                    expected(cut)
                        .into_iter()
                        .filter(|row| row.entry.relation == relation)
                        .collect::<Vec<_>>(),
                );
                assert!(scan.work_used() < all_work);
                assert!(scan.is_finished());
                drop(scan);
                let faults = view.stats().misses - before.misses;
                assert!(faults < all_faults, "cut {cut}, relation {relation:?}");
                if relation == RelationId(u64::MAX) {
                    assert_eq!(faults, 0, "absent relations read no edge payloads");
                }
                assert_eq!(pool.used(), baseline);
            }
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn absent_relation_root_needs_no_head_reservation_and_fuses_without_payloads() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let before = view.stats();
        // Leave no room for even a fixed root-head reservation. An absent
        // relation still performs its controlled directory seek and reaches EOF.
        let held = pool.reserve(&cx, pool.available()).unwrap();
        let occupied = pool.used();
        let mut scan = view
            .edge_scan_relation(&cx, CommitSeq(5), RelationId(u64::MAX))
            .unwrap();
        assert_eq!(scan.state.as_ref().unwrap().heads.capacity(), 0);
        assert!(scan.state.as_ref().unwrap()._head_charge.is_none());
        assert!(
            scan.next_candidate_with(&cx, &mut |event| {
                assert_eq!(event, BufferedEdgeScanEvent::Work);
                assert_eq!(pool.used(), occupied);
                Ok::<_, ()>(())
            })
            .await
            .unwrap()
            .is_none()
        );
        assert!(scan.work_used() > 0);
        assert!(scan.is_finished());
        assert!(
            scan.next_candidate_with(&cx, &mut |_| -> Result<(), ()> {
                panic!("fused absent relation scan")
            })
            .await
            .unwrap()
            .is_none()
        );
        drop(scan);
        assert_eq!(view.stats().misses, before.misses);
        assert_eq!(pool.used(), occupied);
        drop(held);
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn relation_head_admission_reclaims_cached_extents_before_refusing_memory() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        // Ordinary point reads leave an unpinned resident extent. A root's
        // selected heap must be able to reclaim it, just like the full heap.
        drop(view.edge_at(&cx, EId(4), CommitSeq(5)).await.unwrap());
        let cached = pool.used() - baseline;
        assert!(cached > 0);
        let selected_bytes = 1024 + 3 * 512;
        let available = selected_bytes - cached.min(256);
        let held = pool.reserve(&cx, pool.available() - available).unwrap();
        let before = view.stats();
        let mut scan = view.edge_scan_relation(&cx, CommitSeq(5), R).unwrap();
        assert!(matches!(
            scan.next_candidate_with(&cx, &mut |event| {
                if matches!(event, BufferedEdgeScanEvent::Identity(_)) {
                    Err(()) // Stop after complete head admission, before payloads.
                } else {
                    Ok(())
                }
            })
            .await,
            Err(BufferedScanError::Control(()))
        ));
        assert!(scan.is_finished());
        drop(scan);
        assert!(view.stats().evictions > before.evictions);
        assert_eq!(view.stats().misses, before.misses);
        drop(held);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn relation_root_does_not_restrict_independent_join_incidence() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let mut scan = view
            .edge_scan_relation(&cx, CommitSeq(5), R)
            .unwrap()
            .into_join_scan();
        let mut roots = Vec::new();
        while let Some(root) = scan
            .next_with_endpoints(&cx, None, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
        {
            roots.push(root.eid);
            let nested = scan
                .next_incident_retained_with_endpoints(
                    &cx,
                    VId(2),
                    Some(S),
                    BufferedEdgeDirection::Outgoing,
                    None,
                    &mut |_| Ok::<_, ()>(()),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(nested.eid, EId(0));
            assert_eq!(nested.row.unwrap().edge().entry.relation, S);
        }
        assert_eq!(roots, [EId(4), EId(6), EId(90), EId(u128::MAX)]);
        drop(scan);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn nested_incidence_eof_returns_to_each_live_root_memory_baseline() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let opened = pool.used();
        let mut scan = view
            .edge_scan_relation(&cx, CommitSeq(5), R)
            .unwrap()
            .into_join_scan();
        let mut roots = Vec::new();
        while let Some(root) = scan
            .next_with_endpoints(&cx, None, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
        {
            roots.push(root.eid);
            // Keep the parent edge/endpoints alive. Its heap and decoder
            // position are legitimate retained state, not child allocations.
            let parent = pool.used();
            let mut after = None;
            for expected in [EId(0), EId(2), EId(8)] {
                let nested = scan
                    .next_incident_retained_with_endpoints(
                        &cx,
                        VId(2),
                        Some(S),
                        BufferedEdgeDirection::Outgoing,
                        after,
                        &mut |_| Ok::<_, ()>(()),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(nested.eid, expected);
                assert_eq!(nested.row.as_ref().unwrap().edge().entry.relation, S);
                after = Some(nested.eid);
                drop(nested);
            }
            assert!(
                scan.next_incident_retained_with_endpoints(
                    &cx,
                    VId(2),
                    Some(S),
                    BufferedEdgeDirection::Outgoing,
                    after,
                    &mut |_| Ok::<_, ()>(()),
                )
                .await
                .unwrap()
                .is_none()
            );
            assert_eq!(
                pool.used(),
                parent,
                "child EOF retains no allocation beside root {:?}",
                root.eid
            );
        }
        assert_eq!(roots, [EId(4), EId(6), EId(90), EId(u128::MAX)]);
        drop(scan);
        assert_eq!(
            pool.used(),
            opened,
            "closing the join releases every cursor"
        );
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn every_relation_root_control_refuses_and_fuses_without_leaking_reservations() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        for relation in [R, RelationId(u64::MAX)] {
            let mut total = 0;
            {
                let mut scan = view
                    .edge_scan_relation(&cx, CommitSeq(5), relation)
                    .unwrap();
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
                let mut scan = view
                    .edge_scan_relation(&cx, CommitSeq(5), relation)
                    .unwrap();
                let mut seen = 0;
                loop {
                    match scan
                        .next_with_endpoints(&cx, None, &mut |_| {
                            let at = seen;
                            seen += 1;
                            if at == stop { Err(stop) } else { Ok(()) }
                        })
                        .await
                    {
                        Ok(Some(_)) => {}
                        Err(BufferedScanError::Control(at)) => {
                            assert_eq!(at, stop);
                            assert_eq!(seen, stop + 1);
                            break;
                        }
                        other => panic!("expected relation refusal {stop}, got {other:?}"),
                    }
                }
                assert!(scan.is_finished());
                assert!(
                    scan.next_candidate_with(&cx, &mut |_| -> Result<(), ()> {
                        panic!("fused relation scan")
                    })
                    .await
                    .unwrap()
                    .is_none()
                );
                drop(scan);
                assert_eq!(pool.used(), baseline, "control {stop}");
            }
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn topology_pulls_match_full_history_and_upgrade_properties_when_interleaved() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        for cut in 0..=5 {
            let before = view.stats().misses;
            let mut full = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
            let mut full_ids = Vec::new();
            let mut full_rows = Vec::new();
            while let Some(candidate) = full
                .next_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                .await
                .unwrap()
            {
                full_ids.push(candidate.eid);
                if let Some(row) = candidate.row {
                    full_rows.push(row.as_ref().clone());
                }
            }
            drop(full);
            let full_faults = view.stats().misses - before;
            assert_eq!(full_rows, expected(cut), "complete oracle at {cut}");
            assert_eq!(pool.used(), baseline);

            let before = view.stats().misses;
            let mut topology = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
            let mut ids = Vec::new();
            let mut rows = Vec::new();
            while let Some(candidate) = topology
                .next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                .await
                .unwrap()
            {
                ids.push(candidate.eid);
                if let Some(row) = candidate.row {
                    rows.push(*row.entry());
                }
            }
            drop(topology);
            let topology_faults = view.stats().misses - before;
            assert_eq!(ids, full_ids, "including invisible identities at {cut}");
            assert_eq!(
                rows,
                expected(cut)
                    .into_iter()
                    .map(|row| row.entry)
                    .collect::<Vec<_>>()
            );
            assert!(topology_faults > 0);
            // Every fixture block has a sidecar. Both live paths perform the
            // same topology faults; only the full incumbent fetches sidecars.
            assert_eq!(full_faults, 2 * topology_faults, "cut {cut}");
            assert_eq!(pool.used(), baseline);
        }

        // All three S identities share one active block. A complete pull after
        // a topology pull must upgrade that block instead of returning [] props.
        let mut scan = view.edge_scan_relation(&cx, CommitSeq(5), S).unwrap();
        let first = scan
            .next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.eid, EId(0));
        drop(first);
        let second = scan
            .next_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.eid, EId(2));
        assert_eq!(
            second.row.as_ref().unwrap().props,
            [(P, CanonicalPropertyValue::Scalar(CanonicalScalar::Int(20)))]
        );
        drop(second);
        let held = scan
            .next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap()
            .row
            .unwrap();
        assert_eq!(held.entry().eid, EId(8));
        drop(scan);
        drop(view);
        assert!(pool.used() > 0, "returned topology keeps its reservation");
        drop(held);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn topology_skips_sidecar_faults_but_authenticates_topology_and_initial_admission() {
    run(|contexts, path| async move {
        for property_patch in [false, true] {
            let path = path.join(if property_patch {
                "property"
            } else {
                "topology"
            });
            std::fs::create_dir(&path).unwrap();
            let (store, id) = fixture(&contexts, &path, false).await;
            let cx = contexts.query();
            let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
            let mut view = store
                .open_buffered_root(&cx, id, pool.clone(), limits())
                .await
                .unwrap();
            let block = view.root().blocks[2].block_id;
            let corrupt = if property_patch {
                let bytes = store
                    .get_bytes(&cx, DeltaBlockVersion(block))
                    .await
                    .unwrap();
                crate::decode_block_with_properties(&bytes)
                    .unwrap()
                    .1
                    .unwrap()
                    .0
            } else {
                block
            };
            let fault_path = store.path(corrupt);
            let mut bytes = std::fs::read(&fault_path).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 1;
            std::fs::write(&fault_path, bytes).unwrap();
            let baseline = pool.used();
            let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
            if property_patch {
                let mut rows = Vec::new();
                while let Some(candidate) = scan
                    .next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                    .await
                    .unwrap()
                {
                    if let Some(row) = candidate.row {
                        rows.push(*row.entry());
                    }
                }
                assert_eq!(
                    rows,
                    expected(5)
                        .into_iter()
                        .map(|row| row.entry)
                        .collect::<Vec<_>>()
                );
            } else {
                assert!(matches!(
                    scan.next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                        .await,
                    Err(BufferedScanError::Read(BufferedReadError::Buffer(
                        BufferError::ChecksumMismatch
                    )))
                ));
                assert!(scan.is_finished());
            }
            drop(scan);
            assert_eq!(pool.used(), baseline);
            let mut full = view.edge_scan(&cx, CommitSeq(5)).unwrap();
            assert!(matches!(
                full.next(&cx).await,
                Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
            ));
            drop(full);
            for key in [P, PropertyKeyId(12)] {
                let mut selected = view.edge_scan(&cx, CommitSeq(5)).unwrap().into_join_scan();
                assert!(matches!(
                    selected
                        .next_properties_with_endpoints(
                            &cx,
                            None,
                            PropertySelection::one(key),
                            &mut |_| Ok::<_, ()>(())
                        )
                        .await,
                    Err(BufferedScanError::Read(BufferedReadError::Buffer(
                        BufferError::ChecksumMismatch
                    )))
                ));
                assert!(selected.is_closed());
                assert!(
                    selected
                        .next_with_endpoints(&cx, None, &mut |_| -> Result<(), ()> {
                            panic!("a selected source failure closes every driver entry point")
                        })
                        .await
                        .unwrap()
                        .is_none()
                );
                drop(selected);
                assert_eq!(pool.used(), baseline);
            }
            drop(view);
            assert_eq!(pool.used(), 0);
            assert!(
                store
                    .open_buffered_root(&cx, id, pool.clone(), limits())
                    .await
                    .is_err()
            );
            assert_eq!(
                pool.used(),
                0,
                "initial admission still verifies every sidecar"
            );
        }
    });
}

#[test]
fn topology_controls_refuse_at_every_prefix_and_release_the_charged_merge() {
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
                .next_topology_candidate_with(&cx, &mut |_| {
                    total += 1;
                    Ok::<_, usize>(())
                })
                .await
                .unwrap()
                .is_some()
            {}
        }
        assert!(total > 0);
        for stop in 0..total {
            let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
            let mut seen = 0;
            loop {
                match scan
                    .next_topology_candidate_with(&cx, &mut |_| {
                        let at = seen;
                        seen += 1;
                        if at == stop { Err(stop) } else { Ok(()) }
                    })
                    .await
                {
                    Ok(Some(_)) => {}
                    Err(BufferedScanError::Control(at)) => {
                        assert_eq!(at, stop);
                        assert_eq!(seen, stop + 1);
                        break;
                    }
                    other => panic!("expected topology refusal {stop}, got {other:?}"),
                }
            }
            assert!(scan.is_finished());
            assert!(
                scan.next_topology_candidate_with(&cx, &mut |_| -> Result<(), ()> {
                    panic!("fused topology source")
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
fn relation_pruning_skips_excluded_faults_but_never_skips_initial_admission() {
    run(|contexts, path| async move {
        for property_patch in [false, true] {
            let path = path.join(if property_patch {
                "property"
            } else {
                "topology"
            });
            std::fs::create_dir(&path).unwrap();
            let (store, id) = fixture(&contexts, &path, false).await;
            let cx = contexts.query();
            let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
            let mut view = store
                .open_buffered_root(&cx, id, pool.clone(), limits())
                .await
                .unwrap();
            let block = view.root().blocks[2].block_id; // the unrelated S family
            let corrupt = if property_patch {
                let bytes = store
                    .get_bytes(&cx, DeltaBlockVersion(block))
                    .await
                    .unwrap();
                crate::decode_block_with_properties(&bytes)
                    .unwrap()
                    .1
                    .unwrap()
                    .0
            } else {
                block
            };
            let fault_path = store.path(corrupt);
            let mut bytes = std::fs::read(&fault_path).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 1;
            std::fs::write(&fault_path, bytes).unwrap();
            let baseline = pool.used();
            let mut selected = view.edge_scan_relation(&cx, CommitSeq(5), R).unwrap();
            let mut rows = Vec::new();
            while let Some(row) = selected.next(&cx).await.unwrap() {
                rows.push(row.as_ref().clone());
            }
            assert_eq!(
                rows,
                expected(5)
                    .into_iter()
                    .filter(|row| row.entry.relation == R)
                    .collect::<Vec<_>>()
            );
            drop(selected);
            assert_eq!(pool.used(), baseline);
            let mut all = view.edge_scan(&cx, CommitSeq(5)).unwrap();
            assert!(matches!(
                all.next(&cx).await,
                Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
            ));
            assert!(all.next(&cx).await.unwrap().is_none());
            drop(all);
            drop(view);
            assert_eq!(pool.used(), 0);
            assert!(
                store
                    .open_buffered_root(&cx, id, pool.clone(), limits())
                    .await
                    .is_err(),
                "a new open must authenticate even the excluded relation"
            );
            assert_eq!(pool.used(), 0);
        }
    });
}

#[test]
fn relation_scan_never_returns_an_old_version_over_a_corrupt_later_restatement() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let fault_path = store.path(view.root().blocks[3].block_id);
        let mut bytes = std::fs::read(&fault_path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&fault_path, bytes).unwrap();
        for cut in [1, 5] {
            let mut scan = view.edge_scan_relation(&cx, CommitSeq(cut), R).unwrap();
            assert!(matches!(
                scan.next(&cx).await,
                Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
            ));
            assert!(scan.next(&cx).await.unwrap().is_none());
            drop(scan);
            assert_eq!(pool.used(), baseline);
        }
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
                    assert_eq!(
                        row.props,
                        vec![(
                            P,
                            CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value))
                        )]
                    );
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
        let held_baseline = pool.used();
        let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
        assert_eq!(pool.used(), held_baseline, "opening reserves no root heap");
        drop(scan.next(&cx));
        assert!(!scan.is_finished());
        assert_eq!(scan.work_used(), 0);
        assert_eq!(pool.used(), held_baseline, "unpolled pull has no demand");
        assert!(matches!(
            scan.next_candidate_with(&cx, &mut |_| -> Result<(), ()> {
                panic!("root heap admission precedes head consumption")
            })
            .await,
            Err(BufferedScanError::Read(BufferedReadError::Buffer(
                BufferError::Memory(_)
            )))
        ));
        assert!(scan.is_finished());
        assert!(scan.next(&cx).await.unwrap().is_none());
        drop(scan);
        assert_eq!(pool.used(), held_baseline);
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
fn selective_incidence_does_not_reserve_an_unused_global_root_heap() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let endpoint = VId(u128::MAX); // absent from every endpoint fence
        let root_bytes = view.root().blocks.len() * 512 + 1024;
        let incidence_bytes = 1024
            + size_of_val(&view.edge_scan_incidence_blocks(
                endpoint,
                None,
                BufferedEdgeDirection::Outgoing,
            ));
        assert!(incidence_bytes < root_bytes);
        let held = pool
            .reserve(&cx, pool.available() - (root_bytes - 1))
            .unwrap();
        let held_baseline = pool.used();
        for direction in [
            BufferedEdgeDirection::Outgoing,
            BufferedEdgeDirection::Incoming,
            BufferedEdgeDirection::Undirected,
        ] {
            let mut join = view.edge_scan(&cx, CommitSeq(5)).unwrap().into_join_scan();
            assert_eq!(pool.used(), held_baseline);
            assert!(
                join.next_incident_with_endpoints(
                    &cx,
                    endpoint,
                    None,
                    direction,
                    None,
                    &mut |_| Ok::<_, ()>(()),
                )
                .await
                .unwrap()
                .is_none()
            );
            assert_eq!(pool.used(), held_baseline, "selective scratch is refunded");
            assert!(matches!(
                join.next_with_endpoints(&cx, None, &mut |_| -> Result<(), ()> {
                    panic!("whole-root heap must refuse before consuming a head")
                })
                .await,
                Err(BufferedScanError::Read(BufferedReadError::Buffer(
                    BufferError::Memory(_)
                )))
            ));
            assert!(join.is_closed());
            drop(join);
            assert_eq!(pool.used(), held_baseline);

            let mut incidence = view
                .edge_scan(&cx, CommitSeq(5))
                .unwrap()
                .into_incidence_scan(endpoint, None, direction, None);
            drop(incidence.next(&cx));
            assert!(!incidence.is_finished());
            assert_eq!(pool.used(), held_baseline, "conversion has no root charge");
            assert!(incidence.next(&cx).await.unwrap().is_none());
            assert!(incidence.is_finished());
            drop(incidence);
            assert_eq!(pool.used(), held_baseline);
        }
        assert_eq!(view.stats().misses, 0, "no unrelated payload was faulted");
        drop(held);
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

            let mut scan = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
            let mut failed = false;
            loop {
                match scan
                    .next_topology_with_endpoints(&cx, relation, &mut |_| Ok::<_, ()>(()))
                    .await
                {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(BufferedScanError::Read(BufferedReadError::DanglingEndpoint)) => {
                        failed = true;
                        break;
                    }
                    other => panic!("unexpected topology endpoints {other:?}"),
                }
            }
            assert_eq!(failed, refuses);
            assert!(scan.is_finished());
            drop(scan);
            assert_eq!(pool.used(), baseline);

            for key in [P, PropertyKeyId(12)] {
                let mut scan = view
                    .edge_scan(&cx, CommitSeq(cut))
                    .unwrap()
                    .into_join_scan();
                let mut failed = false;
                loop {
                    match scan
                        .next_properties_with_endpoints(
                            &cx,
                            relation,
                            PropertySelection::one(key),
                            &mut |_| Ok::<_, ()>(()),
                        )
                        .await
                    {
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(BufferedScanError::Read(BufferedReadError::DanglingEndpoint)) => {
                            failed = true;
                            break;
                        }
                        other => panic!("unexpected selected-property endpoints {other:?}"),
                    }
                }
                assert_eq!(failed, refuses, "property {key:?}, cut {cut}");
                if failed {
                    assert!(scan.is_closed());
                }
                drop(scan);
                assert_eq!(pool.used(), baseline);
            }

            // The topology-only count still proves both endpoint lifetimes.
            // It must refuse the same visible dangling edge as the complete
            // record stream, while an excluded relation stays unobserved.
            let count = view
                .edge_scan(&cx, CommitSeq(cut))
                .unwrap()
                .into_incidence_scan(VId(1), relation, BufferedEdgeDirection::Outgoing, None)
                .count_visible(&cx)
                .await;
            assert_eq!(
                matches!(&count, Err(BufferedReadError::DanglingEndpoint)),
                refuses
            );
            if !refuses {
                let wanted = expected(cut)
                    .into_iter()
                    .filter(|edge| {
                        edge.entry.src == VId(1)
                            && relation.is_none_or(|selected| selected == edge.entry.relation)
                    })
                    .count();
                assert_eq!(count.unwrap(), wanted as u64);
            }
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
        // This V6 inline tail includes authenticated descriptors. Its encoded
        // frame no longer fits the old fixture's 16KiB root allowance. Keep
        // that refusal and the exact one-byte-short refusal before admitting
        // the whole frame; the scan's block, pool and work bounds stay fixed.
        assert!(count < crate::root_segment::SEGMENT_REFS);
        let root_bytes = store.get_root_bytes(&cx, id).await.unwrap();
        let root_bytes_len = root_bytes.len();
        drop(root_bytes);
        assert!(root_bytes_len > bounds.max_root_bytes);
        // Open admission builds four charged descriptor directories, including
        // the relation/publication index. Allow their controlled sorts here;
        // restore the scan's previous linear allowance after authenticated open.
        bounds.max_work = count * (12 * count.ilog2() as usize + 16);
        let pool = MemoryPool::new(4 * 1024 * 1024, 0).unwrap();
        assert!(matches!(
            store
                .open_buffered_root(&cx, id, pool.clone(), bounds)
                .await,
            Err(BufferedReadError::Store(error))
                if matches!(error.as_ref(), crate::store::StoreError::ObjectTooLarge {
                    limit: 16_384,
                    observed: 16_385,
                })
        ));
        assert_eq!(pool.used(), 0);
        bounds.max_root_bytes = root_bytes_len - 1;
        assert!(matches!(
            store.open_buffered_root(&cx, id, pool.clone(), bounds).await,
            Err(BufferedReadError::Store(error))
                if matches!(error.as_ref(), crate::store::StoreError::ObjectTooLarge {
                    limit,
                    observed,
                } if (*limit, *observed) == ((root_bytes_len - 1) as u64, root_bytes_len as u64))
        ));
        assert_eq!(pool.used(), 0);
        bounds.max_root_bytes = root_bytes_len;
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), bounds)
            .await
            .unwrap();
        let baseline = pool.used();
        let mut scan = view.edge_scan(&cx, CommitSeq(1)).unwrap();
        scan.work.limit = count * 50;
        for expected in 1..=count {
            let found = scan.next(&cx).await.unwrap().unwrap();
            assert_eq!(found.entry.eid, EId(expected as u128));
        }
        assert!(scan.next(&cx).await.unwrap().is_none());
        let total = scan.work_used();
        // Well below a point lookup per EId's N*N visits (measured 4,140).
        assert!(total < count * 40);
        drop(scan);
        assert_eq!(view.stats().misses, count as u64);
        assert_eq!(view.stats().bypasses, count as u64);
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
        // Admission is charged cumulatively against the same allowance. Since
        // 68bf6b14 it costs more than this whole scan, so the scan's own
        // refusal is unreachable through the shared limit here; the exact
        // first-unit-over refusal is observed at open instead.
        let (mut below, mut cost) = (0, bounds.max_work);
        while below + 1 < cost {
            let mid = (below + cost) / 2;
            bounds.max_work = mid;
            match store
                .open_buffered_root(&cx, id, pool.clone(), bounds)
                .await
            {
                Ok(_) => cost = mid,
                Err(_) => below = mid,
            }
        }
        assert!(total < cost && cost < count * count);
        bounds.max_work = cost - 1;
        assert!(matches!(
            store.open_buffered_root(&cx, id, pool.clone(), bounds).await,
            Err(BufferedReadError::Limit { requested, limit, .. })
                if (requested, limit) == (cost, cost - 1)
        ));
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn selected_property_candidates_preserve_history_and_mode_upgrades() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let absent = PropertyKeyId(12);
        for cut in 0..=5 {
            for key in [P, absent] {
                let mut scan = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
                let mut ids = Vec::new();
                let mut admitted = Vec::new();
                let mut rows = Vec::new();
                while let Some(candidate) = scan
                    .next_properties_candidate_with(
                        &cx,
                        PropertySelection::one(key),
                        &mut |event| {
                            if let BufferedEdgeScanEvent::Identity(eid) = event {
                                admitted.push(eid);
                            }
                            Ok::<_, ()>(())
                        },
                    )
                    .await
                    .unwrap()
                {
                    ids.push(candidate.eid);
                    if let Some(row) = candidate.row {
                        rows.push((*row.entry(), row.value(key).cloned()));
                    }
                }
                assert_eq!(
                    ids,
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
                assert_eq!(
                    admitted, ids,
                    "one admission per identity, including invisible histories"
                );
                let oracle: Vec<_> = expected(cut)
                    .into_iter()
                    .map(|row| {
                        let value = row
                            .props
                            .into_iter()
                            .find(|(candidate, _)| *candidate == key)
                            .map(|(_, value)| value);
                        (row.entry, value)
                    })
                    .collect();
                assert_eq!(rows, oracle, "cut {cut}, property {key:?}");
                drop(scan);
                assert_eq!(pool.used(), baseline);
            }
        }

        use EdgePropertyAccess::{Properties, Read, Skip};
        let selected = PropertySelection::one(P);
        let missing = PropertySelection::one(absent);
        let cases = [
            [Properties(selected), Properties(missing), Read],
            [Read, Properties(missing), Properties(selected)],
            [Skip, Properties(selected), Properties(missing)],
            [Properties(selected), Skip, Properties(selected)],
            [Properties(missing), Properties(selected), Read],
            [Properties(missing), Read, Properties(missing)],
        ];
        for (case, modes) in cases.into_iter().enumerate() {
            let mut scan = view.edge_scan_relation(&cx, CommitSeq(5), S).unwrap();
            for (ordinal, mode) in modes.into_iter().enumerate() {
                let before = scan.partition.stats().misses;
                let (eid, row) = match mode {
                    Read => {
                        let candidate = scan
                            .next_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                            .await
                            .unwrap()
                            .unwrap();
                        (
                            candidate.eid,
                            candidate.row.map(|row| (row.entry, row.props.clone())),
                        )
                    }
                    Properties(keys) => {
                        let candidate = scan
                            .next_properties_candidate_with(&cx, keys, &mut |_| Ok::<_, ()>(()))
                            .await
                            .unwrap()
                            .unwrap();
                        (
                            candidate.eid,
                            candidate
                                .row
                                .map(|row| (*row.entry(), row.properties().to_vec())),
                        )
                    }
                    Skip => {
                        let candidate = scan
                            .next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                            .await
                            .unwrap()
                            .unwrap();
                        (
                            candidate.eid,
                            candidate.row.map(|row| (*row.entry(), Vec::new())),
                        )
                    }
                };
                let expected_id = [EId(0), EId(2), EId(8)][ordinal];
                assert_eq!(eid, expected_id);
                let mut expected = expected(5)
                    .into_iter()
                    .find(|row| row.entry.eid == expected_id)
                    .unwrap();
                match mode {
                    Read => {}
                    Properties(keys) => expected
                        .props
                        .retain(|(candidate, _)| keys.contains(*candidate)),
                    Skip => expected.props.clear(),
                }
                assert_eq!(
                    row,
                    Some((expected.entry, expected.props)),
                    "case {case}, row {ordinal}"
                );
                if case == 3 && ordinal != 0 {
                    assert_eq!(
                        scan.partition.stats().misses,
                        before,
                        "same-key/topology reuse"
                    );
                }
            }
            assert!(
                scan.next_properties_candidate_with(&cx, PropertySelection::one(P), &mut |_| Ok::<
                    _,
                    (),
                >(
                    ()
                ))
                .await
                .unwrap()
                .is_none()
            );
            drop(scan);
            assert_eq!(pool.used(), baseline);
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn selected_property_controls_and_work_limits_refuse_without_leaking_state() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        // Exercise the actual QueryCx checkpoint boundary before the first
        // payload read; this simulation probe does not cancel the whole task.
        let probe =
            std::sync::Arc::new(fgdb_types::context::SimulationCheckpointProbe::new(Some(1)));
        let interrupted = cx.with_checkpoint_probe(probe.clone());
        let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
        assert!(matches!(
            scan.next_properties_candidate_with(
                &interrupted,
                PropertySelection::one(P),
                &mut |_| -> Result<(), ()> {
                    panic!("the first checkpoint must refuse before source consumption")
                }
            )
            .await,
            Err(BufferedScanError::Read(_))
        ));
        assert_eq!(probe.calls(), 1);
        assert!(scan.is_finished());
        drop(scan);
        assert_eq!(view.stats().misses, 0);
        assert_eq!(pool.used(), baseline);

        // Refuse the first identity before either topology or sidecar I/O.
        let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
        {
            let mut observe = |_| Ok::<_, ()>(());
            drop(scan.next_properties_candidate_with(&cx, PropertySelection::one(P), &mut observe));
        }
        assert_eq!(scan.work_used(), 0);
        assert!(!scan.is_finished());
        assert!(matches!(
            scan.next_properties_candidate_with(&cx, PropertySelection::one(P), &mut |event| {
                if event == BufferedEdgeScanEvent::Identity(EId(0)) {
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

        for key in [P, PropertyKeyId(12)] {
            let mut total = 0;
            let work = {
                let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
                while scan
                    .next_properties_candidate_with(&cx, PropertySelection::one(key), &mut |_| {
                        total += 1;
                        Ok::<_, usize>(())
                    })
                    .await
                    .unwrap()
                    .is_some()
                {}
                scan.work_used()
            };
            assert!(total > 0 && work > 0);
            for stop in 0..total {
                let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
                let mut seen = 0;
                loop {
                    match scan
                        .next_properties_candidate_with(
                            &cx,
                            PropertySelection::one(key),
                            &mut |_| {
                                let at = seen;
                                seen += 1;
                                if at == stop { Err(stop) } else { Ok(()) }
                            },
                        )
                        .await
                    {
                        Ok(Some(_)) => {}
                        Err(BufferedScanError::Control(at)) => {
                            assert_eq!(at, stop);
                            assert_eq!(seen, stop + 1);
                            break;
                        }
                        other => panic!("expected selected-property refusal {stop}, got {other:?}"),
                    }
                }
                assert!(scan.is_finished());
                assert!(
                    scan.next_properties_candidate_with(
                        &cx,
                        PropertySelection::one(key),
                        &mut |_| -> Result<(), ()> { panic!("fused selected-property source") }
                    )
                    .await
                    .unwrap()
                    .is_none()
                );
                drop(scan);
                assert_eq!(pool.used(), baseline, "property {key:?}, control {stop}");
            }
            for limit in [work - 1, work] {
                let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
                scan.work.limit = limit;
                let mut refused = false;
                loop {
                    match scan
                        .next_properties_candidate_with(
                            &cx,
                            PropertySelection::one(key),
                            &mut |_| Ok::<_, ()>(()),
                        )
                        .await
                    {
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(BufferedScanError::Read(BufferedReadError::Limit {
                            resource: "buffered source work",
                            requested,
                            limit: found,
                        })) => {
                            assert_eq!((requested, found), (work, work - 1));
                            refused = true;
                            break;
                        }
                        other => panic!("unexpected selected-property source result {other:?}"),
                    }
                }
                assert_eq!(refused, limit < work);
                assert!(scan.is_finished());
                drop(scan);
                assert_eq!(pool.used(), baseline);
            }
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn selected_properties_rows_clear_old_values_and_retain_only_requested_payloads() {
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
        let unrelated = PropertyKeyId(10); // Before P: selecting the first value would be wrong.
        let high = PropertyKeyId(u64::MAX);
        let selected_keys = PropertySelection::try_from_slice(&[high, P, high]).unwrap();
        let mut capacity_keys = selected_keys;
        for key in 100..114 {
            capacity_keys.insert(PropertyKeyId(key)).unwrap();
        }
        assert_eq!(capacity_keys.len(), PropertySelection::CAPACITY);
        let wide =
            || CanonicalPropertyValue::from(CanonicalScalar::bytes(vec![0x6d; 2048]).unwrap());
        let properties = |value: CanonicalPropertyValue| {
            vec![
                (unrelated, wide()),
                (P, value),
                (high, CanonicalScalar::Null.into()),
            ]
        };
        let definitions = [
            (
                vec![
                    entry(0, 1, 1, R, 1, None),
                    entry(1, 1, 2, R, 1, None),
                    entry(2, 1, 2, R, 1, None),
                    entry(3, 1, 3, R, 1, None),
                    entry(u128::MAX, 1, 3, R, 1, None),
                ],
                vec![1, 2, 0, 3, 4],
                vec![
                    properties(CanonicalScalar::Int(0).into()),
                    properties(CanonicalScalar::Null.into()),
                    properties(CanonicalScalar::Int(3).into()),
                    properties(CanonicalScalar::Int(99).into()),
                ],
            ),
            (
                vec![
                    entry(3, 1, 3, R, 1, Some(3)),
                    entry(3, 1, 3, R, 3, None),
                    entry(u128::MAX, 1, 3, R, 1, Some(3)),
                    entry(u128::MAX, 1, 3, R, 3, None),
                ],
                vec![1, 2, 3, 0],
                vec![
                    properties(CanonicalScalar::Int(3).into()),
                    vec![(unrelated, wide())],
                    properties(CanonicalScalar::Int(99).into()),
                ],
            ),
        ];
        let mut predecessor = None;
        let mut blocks = Vec::new();
        for (entries, locators, rows) in definitions {
            let bytes = encode_property_patch(&rows).unwrap();
            assert!(bytes.len() <= limits().buffer.max_extent_bytes);
            let patch = store
                .put_edge_property_patch(&commit, &bytes)
                .await
                .unwrap();
            let encoded =
                encode_block_with_properties(0, predecessor, &entries, patch.0, &locators, &rows)
                    .unwrap();
            let block = store.put(&commit, &encoded).await.unwrap();
            blocks.push(BlockRef {
                block_id: block.0,
                first_seq: CommitSeq(1),
                last_seq: entries
                    .iter()
                    .map(|row| row.retired_at.unwrap_or(row.created_at))
                    .max()
                    .unwrap(),
            });
            predecessor = Some(block);
        }
        let vertices = [
            vertex(1, 1, None, 10),
            vertex(2, 1, None, 20),
            vertex(3, 1, None, 30),
        ];
        let patch = store
            .put_patch(&commit, &encode_patch(&vertices).unwrap())
            .await
            .unwrap();
        let id = store
            .put_root(
                &commit,
                &PartitionRoot {
                    graph: GraphId(1),
                    branch: BranchId(2),
                    partition: 0,
                    published_at: CommitSeq(3),
                    blocks,
                    vertex_patches: vec![PatchRef {
                        patch_id: patch.0,
                        first_seq: CommitSeq(1),
                        last_seq: CommitSeq(1),
                    }],
                },
            )
            .await
            .unwrap();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        for cut in [1, 3, 1] {
            let mut scan = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
            let mut found = Vec::new();
            while let Some(candidate) = scan
                .next_properties_candidate_with(&cx, PropertySelection::one(P), &mut |_| {
                    Ok::<_, ()>(())
                })
                .await
                .unwrap()
            {
                let row = candidate.row.unwrap();
                let changed = matches!(candidate.eid, EId(3) | EId(u128::MAX));
                assert_eq!(
                    row.entry().created_at,
                    CommitSeq(if cut >= 3 && changed { 3 } else { 1 })
                );
                assert_eq!(
                    row.entry().retired_at,
                    (cut < 3 && changed).then_some(CommitSeq(3))
                );
                found.push((candidate.eid, row.value(P).cloned()));
            }
            assert_eq!(
                found,
                vec![
                    (EId(0), Some(CanonicalScalar::Int(0).into())),
                    (EId(1), Some(CanonicalScalar::Null.into())),
                    (EId(2), None),
                    (EId(3), (cut < 3).then(|| CanonicalScalar::Int(3).into())),
                    (
                        EId(u128::MAX),
                        (cut < 3).then(|| CanonicalScalar::Int(99).into())
                    ),
                ],
                "a newer missing key or locator zero must clear the earlier value"
            );
            drop(scan);
            assert_eq!(pool.used(), baseline);

            let mut projected = view
                .edge_scan(&cx, CommitSeq(cut))
                .unwrap()
                .into_join_scan();
            let mut joined = Vec::new();
            while let Some(candidate) = projected
                .next_properties_with_endpoints(
                    &cx,
                    Some(R),
                    PropertySelection::one(P),
                    &mut |_| Ok::<_, ()>(()),
                )
                .await
                .unwrap()
            {
                let row = candidate.row.unwrap();
                assert_eq!(
                    row.edge().properties().len(),
                    usize::from(row.edge().value(P).is_some())
                );
                assert!(row.edge().properties().iter().all(|(key, _)| *key == P));
                assert_eq!(row.source_vertex().vid(), row.edge().entry().src);
                assert_eq!(row.target_vertex().vid(), row.edge().entry().dst);
                joined.push((candidate.eid, row.edge().value(P).cloned()));
            }
            assert_eq!(
                joined, found,
                "endpoint projection must preserve missing versus NULL"
            );
            drop(projected);
            assert_eq!(pool.used(), baseline);

            for keys in [selected_keys, capacity_keys, PropertySelection::EMPTY] {
                let mut projected = view
                    .edge_scan(&cx, CommitSeq(cut))
                    .unwrap()
                    .into_join_scan();
                let mut actual = Vec::new();
                while let Some(candidate) = projected
                    .next_properties_with_endpoints(&cx, Some(R), keys, &mut |_| Ok::<_, ()>(()))
                    .await
                    .unwrap()
                {
                    let row = candidate.row.unwrap();
                    if row.edge().entry().src == row.edge().entry().dst {
                        assert!(std::ptr::eq(row.source_vertex(), row.target_vertex()));
                    }
                    assert_eq!(row.edge().value(unrelated), None);
                    actual.push((candidate.eid, row.edge().properties().to_vec()));
                }
                let expected: Vec<_> = found
                    .iter()
                    .map(|(eid, value)| {
                        let properties = if keys.is_empty() {
                            Vec::new()
                        } else {
                            value
                                .as_ref()
                                .map(|value| {
                                    vec![(P, value.clone()), (high, CanonicalScalar::Null.into())]
                                })
                                .unwrap_or_default()
                        };
                        (*eid, properties)
                    })
                    .collect();
                assert_eq!(actual, expected, "cut {cut}, keys {keys:?}");
                drop(projected);
                assert_eq!(pool.used(), baseline);
            }
        }

        let mut complete = view.edge_scan(&cx, CommitSeq(1)).unwrap();
        let full = complete
            .next_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap()
            .row
            .unwrap();
        let full_charge = full.charged_bytes();
        // A full active image serves a selected pull without leaking the wide value.
        let next = complete
            .next_properties_candidate_with(&cx, selected_keys, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap()
            .row
            .unwrap();
        assert_eq!(next.entry().eid, EId(1));
        assert_eq!(next.properties().len(), 2);
        assert_eq!(
            next.value(P),
            Some(&CanonicalPropertyValue::Scalar(CanonicalScalar::Null))
        );
        drop(next);
        drop(full);
        drop(complete);
        assert_eq!(pool.used(), baseline);
        let mut selected = view.edge_scan(&cx, CommitSeq(1)).unwrap();
        let held = selected
            .next_properties_candidate_with(&cx, capacity_keys, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap()
            .row
            .unwrap();
        assert_eq!(held.entry().eid, EId(0));
        assert_eq!(held.properties().len(), 2);
        assert_eq!(
            held.value(P),
            Some(&CanonicalPropertyValue::Scalar(CanonicalScalar::Int(0)))
        );
        assert!(
            held.charged_bytes() + 2048 <= full_charge,
            "unrelated payload stays unmaterialized"
        );
        assert_eq!(format!("{held:?}"), "BufferedEdgeProperties([REDACTED])");
        drop(selected);
        assert_eq!(pool.used(), baseline + held.charged_bytes());
        drop(view);
        assert_eq!(
            pool.used(),
            held.charged_bytes(),
            "returned value owns its reservation"
        );
        drop(held);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn selected_property_endpoint_images_share_historical_labels_and_owners() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut root = store.get_root(&commit, id).await.unwrap();
        let mut changed = vertex(2, 4, None, 22);
        changed.labels = vec![LabelId(10), LabelId(11)];
        let patch = store
            .put_patch(
                &commit,
                &encode_patch(&[vertex(2, 1, Some(4), 20), changed]).unwrap(),
            )
            .await
            .unwrap();
        root.vertex_patches.push(PatchRef {
            patch_id: patch.0,
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(4),
        });
        let id = store.put_root(&commit, &root).await.unwrap();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        for key in [P, PropertyKeyId(12)] {
            for cut in 0..=5 {
                let mut scan = view
                    .edge_scan(&cx, CommitSeq(cut))
                    .unwrap()
                    .into_join_scan();
                let mut admissions = Vec::new();
                let mut rows = Vec::new();
                while let Some(candidate) = scan
                    .next_properties_with_endpoints(
                        &cx,
                        None,
                        PropertySelection::one(key),
                        &mut |event| {
                            if let BufferedEdgeScanEvent::Identity(eid) = event {
                                admissions.push(eid);
                            }
                            Ok::<_, ()>(())
                        },
                    )
                    .await
                    .unwrap()
                {
                    let Some(row) = candidate.row else {
                        continue;
                    };
                    let edge = row.edge();
                    assert_eq!(candidate.eid, edge.entry().eid);
                    assert_eq!(row.source_vertex().vid(), edge.entry().src);
                    assert_eq!(row.target_vertex().vid(), edge.entry().dst);
                    assert_eq!(
                        core::ptr::eq(row.source_vertex(), row.target_vertex()),
                        edge.entry().src == edge.entry().dst
                    );
                    for endpoint in [row.source_vertex(), row.target_vertex()] {
                        let labels = if endpoint.vid() == VId(2) && cut >= 4 {
                            vec![LabelId(10), LabelId(11)]
                        } else {
                            vec![LabelId(9)]
                        };
                        assert_eq!(endpoint.labels(), labels);
                        assert!(endpoint.created_at() <= CommitSeq(cut));
                        assert!(
                            endpoint
                                .retired_at()
                                .is_none_or(|retired| CommitSeq(cut) < retired)
                        );
                    }
                    rows.push((*edge.entry(), edge.properties().to_vec()));
                }
                assert_eq!(
                    admissions,
                    [0, 2, 4, 6, 8, 90, u128::MAX].map(EId),
                    "candidate identity admission at {cut}, {key:?}"
                );
                let wanted: Vec<_> = expected(cut)
                    .into_iter()
                    .map(|row| {
                        let properties = if key == P { row.props } else { Vec::new() };
                        (row.entry, properties)
                    })
                    .collect();
                assert_eq!(rows, wanted, "same-cut selected property {key:?} at {cut}");
                drop(scan);
                assert_eq!(pool.used(), baseline);
            }
        }

        let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap().into_join_scan();
        let held = scan
            .next_properties_with_endpoints(&cx, Some(S), PropertySelection::one(P), &mut |_| {
                Ok::<_, ()>(())
            })
            .await
            .unwrap()
            .unwrap()
            .row
            .unwrap();
        assert_eq!(held.edge().entry().eid, EId(0));
        assert!(core::ptr::eq(held.source_vertex(), held.target_vertex()));
        let retained = held.edge().charged_bytes() + held.source_vertex().charged_bytes();
        assert!(retained > 0);
        drop(scan);
        drop(view);
        assert_eq!(
            pool.used(),
            retained,
            "the self-loop retains one endpoint owner"
        );
        assert_eq!(
            held.edge().properties(),
            [(P, CanonicalPropertyValue::Scalar(CanonicalScalar::Int(0)))]
        );
        assert_eq!(held.source_vertex().labels(), [LabelId(10), LabelId(11)]);
        assert_eq!(
            format!("{held:?}"),
            "BufferedEdgePropertiesEndpoints([REDACTED])"
        );
        drop(held);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn selected_properties_endpoint_controls_close_the_driver_without_leaking_state() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let keys = PropertySelection::try_from_slice(&[P, PropertyKeyId(u64::MAX)]).unwrap();
        let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap().into_join_scan();
        {
            let mut observe = |_| Ok::<_, ()>(());
            drop(scan.next_properties_with_endpoints(&cx, None, keys, &mut observe));
        }
        assert_eq!(scan.work_used(), 0);
        assert!(!scan.is_closed());
        let probe =
            std::sync::Arc::new(fgdb_types::context::SimulationCheckpointProbe::new(Some(1)));
        let interrupted = cx.with_checkpoint_probe(probe.clone());
        assert!(matches!(
            scan.next_properties_with_endpoints(&interrupted, None, keys, &mut |_| -> Result<
                (),
                (),
            > {
                panic!("cancellation precedes the first selected source event")
            })
            .await,
            Err(BufferedScanError::Read(_))
        ));
        assert_eq!(probe.calls(), 1);
        assert!(scan.is_closed());
        drop(scan);
        assert_eq!(view.stats().misses, 0);
        assert_eq!(pool.used(), baseline);

        let mut total = 0;
        let work = {
            let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap().into_join_scan();
            while scan
                .next_properties_with_endpoints(&cx, None, keys, &mut |_| {
                    total += 1;
                    Ok::<_, usize>(())
                })
                .await
                .unwrap()
                .is_some()
            {}
            scan.work_used()
        };
        assert!(total > 0 && work > 0);
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
        // Each comparison starts from the same admitted cold owner. Source
        // work and control prefixes must not depend on an earlier scan's cache.
        for stop in 0..total {
            let mut view = store
                .open_buffered_root(&cx, id, pool.clone(), limits())
                .await
                .unwrap();
            let baseline = pool.used();
            let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap().into_join_scan();
            let mut seen = 0;
            loop {
                match scan
                    .next_properties_with_endpoints(&cx, None, keys, &mut |_| {
                        let at = seen;
                        seen += 1;
                        if at == stop { Err(stop) } else { Ok(()) }
                    })
                    .await
                {
                    Ok(Some(_)) => {}
                    Err(BufferedScanError::Control(at)) => {
                        assert_eq!((at, seen), (stop, stop + 1));
                        break;
                    }
                    other => panic!("expected endpoint refusal {stop}, got {other:?}"),
                }
            }
            assert!(scan.is_closed());
            assert!(
                scan.next_topology_with_endpoints(&cx, None, &mut |_| -> Result<(), ()> {
                    panic!("the selected-property failure closes every root mode")
                })
                .await
                .unwrap()
                .is_none()
            );
            drop(scan);
            assert_eq!(pool.used(), baseline, "selected endpoint control {stop}");
            drop(view);
            assert_eq!(pool.used(), 0);
        }

        for limit in [work - 1, work] {
            let mut view = store
                .open_buffered_root(&cx, id, pool.clone(), limits())
                .await
                .unwrap();
            let baseline = pool.used();
            let mut root = view.edge_scan(&cx, CommitSeq(5)).unwrap();
            root.work.limit = limit;
            let mut scan = root.into_join_scan();
            let mut refused = false;
            loop {
                match scan
                    .next_properties_with_endpoints(&cx, None, keys, &mut |_| Ok::<_, ()>(()))
                    .await
                {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(BufferedScanError::Read(BufferedReadError::Limit {
                        resource: "buffered source work",
                        requested,
                        limit: found,
                    })) => {
                        assert_eq!((requested, found), (work, work - 1));
                        refused = true;
                        break;
                    }
                    other => panic!("unexpected selected endpoint work result {other:?}"),
                }
            }
            assert_eq!(refused, limit < work);
            if refused {
                assert!(scan.is_closed());
            }
            drop(scan);
            assert_eq!(pool.used(), baseline);
            drop(view);
            assert_eq!(pool.used(), 0);
        }
    });
}

#[test]
fn staged_edge_admission_precedes_selected_sidecar_faults() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let bytes = store
            .get_bytes(&cx, DeltaBlockVersion(view.root().blocks[2].block_id))
            .await
            .unwrap();
        let sidecar = crate::decode_block_with_properties(&bytes)
            .unwrap()
            .1
            .unwrap()
            .0;
        let sidecar_path = store.path(sidecar);
        let original = std::fs::read(&sidecar_path).unwrap();
        let mut damaged = original.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        drop(view.reserve_scan_bytes(&cx, pool.limit()));
        let baseline = pool.used();
        std::fs::write(&sidecar_path, &damaged).unwrap();

        let mut scan = view.edge_scan(&cx, CommitSeq(5)).unwrap();
        let mut identities = Vec::new();
        let mut inspected = Vec::new();
        let mut rows = Vec::new();
        while let Some(candidate) = scan
            .next_admitted_properties_with_endpoints(
                &cx,
                None,
                PropertySelection::one(P),
                &mut |row| {
                    inspected.push(row.edge().entry().eid);
                    assert_eq!(row.source_vertex().vid(), row.edge().entry().src);
                    assert_eq!(row.target_vertex().vid(), row.edge().entry().dst);
                    assert_eq!(row.source_vertex().labels(), [LabelId(9)]);
                    assert_eq!(row.target_vertex().labels(), [LabelId(9)]);
                    Ok(row.edge().entry().relation != S)
                },
                &mut |event| {
                    if let BufferedEdgeScanEvent::Identity(eid) = event {
                        identities.push(eid);
                    }
                    Ok::<_, ()>(())
                },
            )
            .await
            .unwrap()
        {
            if let Some(row) = candidate.row {
                rows.push((*row.edge().entry(), row.edge().properties().to_vec()));
            }
        }
        assert_eq!(identities, [0, 2, 4, 6, 8, 90, u128::MAX].map(EId));
        assert_eq!(
            inspected,
            expected(5)
                .iter()
                .map(|row| row.entry.eid)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            rows,
            expected(5)
                .into_iter()
                .filter(|row| row.entry.relation == R)
                .map(|row| (row.entry, row.props))
                .collect::<Vec<_>>()
        );
        drop(scan);
        drop(view.reserve_scan_bytes(&cx, pool.limit()));
        assert_eq!(pool.used(), baseline);

        // A callback error wins over the unread sidecar fault; allowing the
        // same candidate then reports the authentic source failure.
        for allow in [false, true] {
            let mut scan = view.edge_scan_relation(&cx, CommitSeq(5), S).unwrap();
            let mut calls = 0;
            let mut identities = Vec::new();
            let result = scan
                .next_admitted_properties_with_endpoints(
                    &cx,
                    Some(S),
                    PropertySelection::one(P),
                    &mut |_| {
                        calls += 1;
                        if allow {
                            Ok(true)
                        } else {
                            Err("admission refused")
                        }
                    },
                    &mut |event| {
                        if let BufferedEdgeScanEvent::Identity(eid) = event {
                            identities.push(eid);
                        }
                        Ok(())
                    },
                )
                .await;
            assert_eq!(calls, 1);
            assert_eq!(identities, [EId(0)]);
            if allow {
                assert!(matches!(
                    result,
                    Err(BufferedScanError::Read(BufferedReadError::Buffer(
                        BufferError::ChecksumMismatch
                    )))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(BufferedScanError::Control("admission refused"))
                ));
            }
            assert!(scan.is_finished());
            let work = scan.work_used();
            let position = scan.into_state();
            let before = view.stats();
            let mut resumed = view.resume_edge_scan(&cx, position).unwrap();
            assert_eq!(resumed.work_used(), work);
            assert!(resumed.is_finished());
            assert!(
                resumed
                    .next_admitted_properties_with_endpoints(
                        &cx,
                        None,
                        PropertySelection::one(P),
                        &mut |_| -> Result<bool, ()> { panic!("terminal admission") },
                        &mut |_| -> Result<(), ()> { panic!("terminal source") },
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            drop(resumed);
            let after = view.stats();
            assert_eq!(after.hits + after.misses, before.hits + before.misses);
            drop(view.reserve_scan_bytes(&cx, pool.limit()));
            assert_eq!(pool.used(), baseline);
        }

        // An empty property selection still admits topology, without demanding
        // even a damaged sidecar.
        let mut scan = view.edge_scan_relation(&cx, CommitSeq(5), S).unwrap();
        let mut ids = Vec::new();
        while let Some(candidate) = scan
            .next_admitted_properties_with_endpoints(
                &cx,
                None,
                PropertySelection::EMPTY,
                &mut |_| Ok::<_, ()>(true),
                &mut |_| Ok(()),
            )
            .await
            .unwrap()
        {
            let row = candidate.row.unwrap();
            assert!(row.edge().properties().is_empty());
            ids.push(candidate.eid);
        }
        assert_eq!(ids, [EId(0), EId(2), EId(8)]);
        drop(scan);
        std::fs::write(&sidecar_path, &original).unwrap();
        let mut scan = view.edge_scan_relation(&cx, CommitSeq(5), S).unwrap();
        let mut values = Vec::new();
        while let Some(candidate) = scan
            .next_admitted_properties_with_endpoints(
                &cx,
                None,
                PropertySelection::one(P),
                &mut |_| Ok::<_, ()>(true),
                &mut |_| Ok(()),
            )
            .await
            .unwrap()
        {
            values.push((
                candidate.eid,
                candidate.row.unwrap().edge().value(P).cloned(),
            ));
        }
        assert_eq!(
            values,
            [0, 2, 8].map(|id| (
                EId(id),
                Some(CanonicalPropertyValue::from(CanonicalScalar::Int(
                    id as i64 * 10
                ))),
            ))
        );
        drop(scan);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn staged_edge_winners_keep_original_positions_and_clear_newer_missing_keys() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut root = store.get_root(&commit, id).await.unwrap();
        let entries = [entry(4, 1, 2, R, 5, Some(6)), entry(4, 1, 2, R, 6, None)];
        let missing = PropertyKeyId(12);
        let properties = vec![
            vec![(P, CanonicalScalar::Int(45).into())],
            vec![(missing, CanonicalScalar::Int(900).into())],
        ];
        let patch = store
            .put_edge_property_patch(&commit, &encode_property_patch(&properties).unwrap())
            .await
            .unwrap();
        let bytes = encode_block_with_properties(
            0,
            Some(DeltaBlockVersion(root.blocks.last().unwrap().block_id)),
            &entries,
            patch.0,
            &[1, 2],
            &properties,
        )
        .unwrap();
        let block = store.put(&commit, &bytes).await.unwrap();
        root.blocks.push(BlockRef {
            block_id: block.0,
            first_seq: CommitSeq(5),
            last_seq: CommitSeq(6),
        });
        root.published_at = CommitSeq(6);
        let mut changed = vertex(2, 4, None, 22);
        changed.labels = vec![LabelId(10), LabelId(11)];
        let patch = store
            .put_patch(
                &commit,
                &encode_patch(&[vertex(2, 1, Some(4), 20), changed]).unwrap(),
            )
            .await
            .unwrap();
        root.vertex_patches.push(PatchRef {
            patch_id: patch.0,
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(4),
        });
        let id = store.put_root(&commit, &root).await.unwrap();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        drop(view.reserve_scan_bytes(&cx, pool.limit()));
        let baseline = pool.used();
        for cut in 0..=6 {
            let mut oracle = expected(cut.min(5));
            if cut >= 5 {
                let row = oracle
                    .iter_mut()
                    .find(|row| row.entry.eid == EId(4))
                    .unwrap();
                if cut == 5 {
                    row.entry.retired_at = Some(CommitSeq(6));
                } else {
                    row.entry = entry(4, 1, 2, R, 6, None);
                    row.props = properties[1].clone();
                }
            }
            for key in [P, missing] {
                let mut scan = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
                let mut identities = Vec::new();
                let mut admitted = Vec::new();
                let mut rows = Vec::new();
                while let Some(candidate) = scan
                    .next_admitted_properties_with_endpoints(
                        &cx,
                        None,
                        PropertySelection::one(key),
                        &mut |topology| {
                            admitted.push(topology.edge().entry().eid);
                            for endpoint in [topology.source_vertex(), topology.target_vertex()] {
                                let wanted = if endpoint.vid() == VId(2) && cut >= 4 {
                                    vec![LabelId(10), LabelId(11)]
                                } else {
                                    vec![LabelId(9)]
                                };
                                assert_eq!(endpoint.labels(), wanted);
                            }
                            assert_eq!(
                                core::ptr::eq(topology.source_vertex(), topology.target_vertex()),
                                topology.edge().entry().src == topology.edge().entry().dst
                            );
                            Ok(true)
                        },
                        &mut |event| {
                            if let BufferedEdgeScanEvent::Identity(eid) = event {
                                identities.push(eid);
                            }
                            Ok::<_, ()>(())
                        },
                    )
                    .await
                    .unwrap()
                {
                    if let Some(row) = candidate.row {
                        rows.push((*row.edge().entry(), row.edge().value(key).cloned()));
                    }
                }
                assert_eq!(identities, [0, 2, 4, 6, 8, 90, u128::MAX].map(EId));
                assert_eq!(
                    admitted,
                    oracle.iter().map(|row| row.entry.eid).collect::<Vec<_>>()
                );
                let wanted = oracle
                    .iter()
                    .map(|row| {
                        (
                            row.entry,
                            row.props
                                .iter()
                                .find(|(candidate, _)| *candidate == key)
                                .map(|(_, value)| value.clone()),
                        )
                    })
                    .collect::<Vec<_>>();
                assert_eq!(rows, wanted, "cut {cut}, selected key {key:?}");
                drop(scan);
                drop(view.reserve_scan_bytes(&cx, pool.limit()));
                assert_eq!(pool.used(), baseline);
            }
        }

        // The same S block is visited with each staged/full/topology ordering.
        // Staging must neither consume an extra identity nor poison its next
        // full-row upgrade.
        for offset in 0..3 {
            let mut scan = view.edge_scan_relation(&cx, CommitSeq(6), S).unwrap();
            for (ordinal, eid) in [EId(0), EId(2), EId(8)].into_iter().enumerate() {
                let mode = (ordinal + offset) % 3;
                let (found, value) = match mode {
                    0 => {
                        let candidate = scan
                            .next_admitted_properties_with_endpoints(
                                &cx,
                                None,
                                PropertySelection::one(P),
                                &mut |_| Ok::<_, ()>(true),
                                &mut |_| Ok(()),
                            )
                            .await
                            .unwrap()
                            .unwrap();
                        (
                            candidate.eid,
                            candidate.row.unwrap().edge().value(P).cloned(),
                        )
                    }
                    1 => {
                        let candidate = scan
                            .next_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                            .await
                            .unwrap()
                            .unwrap();
                        let row = candidate.row.unwrap();
                        (
                            candidate.eid,
                            row.props
                                .iter()
                                .find(|(key, _)| *key == P)
                                .map(|(_, value)| value.clone()),
                        )
                    }
                    _ => {
                        let candidate = scan
                            .next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                            .await
                            .unwrap()
                            .unwrap();
                        assert_eq!(candidate.row.unwrap().entry().eid, eid);
                        (candidate.eid, None)
                    }
                };
                assert_eq!(found, eid);
                let wanted = (mode != 2).then(|| {
                    CanonicalPropertyValue::from(CanonicalScalar::Int([0, 20, 80][ordinal]))
                });
                assert_eq!(value, wanted);
            }
            assert!(scan.is_finished());
            drop(scan);
            drop(view.reserve_scan_bytes(&cx, pool.limit()));
            assert_eq!(pool.used(), baseline);
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn detached_edge_positions_keep_owner_progress_work_and_terminal_state() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path, false).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let other_pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let mut other = store
            .open_buffered_root(&cx, id, other_pool.clone(), limits())
            .await
            .unwrap();
        drop(view.reserve_scan_bytes(&cx, pool.limit()));
        drop(other.reserve_scan_bytes(&cx, other_pool.limit()));
        let baseline = pool.used();
        let other_baseline = other_pool.used();

        let mut scan = view.edge_scan_relation(&cx, CommitSeq(5), S).unwrap();
        {
            let mut admit = |_: &BufferedEdgeTopologyEndpoints| Ok::<_, ()>(true);
            let mut observe = |_| Ok(());
            drop(scan.next_admitted_properties_with_endpoints(
                &cx,
                None,
                PropertySelection::one(P),
                &mut admit,
                &mut observe,
            ));
        }
        assert_eq!(scan.work_used(), 0);
        assert!(!scan.is_finished());
        let position = scan.into_state();
        let before = view.stats();
        let mut scan = view.resume_edge_scan(&cx, position).unwrap();
        assert_eq!(
            scan.partition.stats().hits + scan.partition.stats().misses,
            before.hits + before.misses
        );
        let held = scan
            .next_admitted_properties_with_endpoints(
                &cx,
                None,
                PropertySelection::one(P),
                &mut |_| Ok::<_, ()>(true),
                &mut |_| Ok(()),
            )
            .await
            .unwrap()
            .unwrap()
            .row
            .unwrap();
        assert_eq!(held.edge().entry().eid, EId(0));
        let work = scan.work_used();
        let position = scan.into_state();
        assert_eq!(position.snapshot_seq(), CommitSeq(5));
        assert_eq!(position.work_used(), work);
        assert!(!position.is_finished());
        let before = view.stats();
        let mut scan = view.resume_edge_scan(&cx, position).unwrap();
        assert_eq!(scan.work_used(), work);
        assert_eq!(
            scan.partition.stats().hits + scan.partition.stats().misses,
            before.hits + before.misses
        );
        let next = scan
            .next_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.eid, EId(2));
        assert_eq!(
            next.row.unwrap().props,
            [(P, CanonicalScalar::Int(20).into())]
        );
        let position = scan.into_state();
        let mut scan = view.resume_edge_scan(&cx, position).unwrap();
        let last = scan
            .next_admitted_properties_with_endpoints(
                &cx,
                None,
                PropertySelection::one(P),
                &mut |_| Ok::<_, ()>(true),
                &mut |_| Ok(()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(last.eid, EId(8));
        assert!(scan.is_finished());
        drop(last);
        drop(scan);
        drop(held);
        drop(view.reserve_scan_bytes(&cx, pool.limit()));
        assert_eq!(pool.used(), baseline);

        // Identical content opened in a different pool is a different source
        // owner. Consuming a refused position releases its original pool.
        let mut scan = view.edge_scan_relation(&cx, CommitSeq(5), S).unwrap();
        drop(
            scan.next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                .await
                .unwrap(),
        );
        let position = scan.into_state();
        assert!(pool.used() > baseline);
        let before = other.stats();
        assert!(matches!(
            other.resume_edge_scan(&cx, position),
            Err(BufferedReadError::Buffer(BufferError::InvalidLoad))
        ));
        let after = other.stats();
        assert_eq!(after.hits + after.misses, before.hits + before.misses);
        assert_eq!(other_pool.used(), other_baseline);
        drop(view.reserve_scan_bytes(&cx, pool.limit()));
        assert_eq!(pool.used(), baseline);

        // Reapplying the same source profile preserves the owner token, but a
        // different profile cannot transplant a live root position.
        let position = view
            .edge_scan_relation(&cx, CommitSeq(5), S)
            .unwrap()
            .into_state();
        view.set_native_run_limits(&cx, None).unwrap();
        let position = view.resume_edge_scan(&cx, position).unwrap().into_state();
        view.set_native_run_limits(&cx, Some(crate::tiered::sealed::SealedLimits::default()))
            .unwrap();
        let before = view.stats();
        assert!(matches!(
            view.resume_edge_scan(&cx, position),
            Err(BufferedReadError::Buffer(BufferError::InvalidLoad))
        ));
        let after = view.stats();
        assert_eq!(after.hits + after.misses, before.hits + before.misses);
        view.set_native_run_limits(&cx, None).unwrap();

        enum Stop {
            Close,
            Refuse,
            Exhaust,
        }
        for stop in [Stop::Close, Stop::Refuse, Stop::Exhaust] {
            let mut scan = view.edge_scan_relation(&cx, CommitSeq(5), S).unwrap();
            match stop {
                Stop::Close => scan.close(),
                Stop::Refuse => {
                    assert!(matches!(
                        scan.next_admitted_properties_with_endpoints(
                            &cx,
                            None,
                            PropertySelection::one(P),
                            &mut |_| Err("refused"),
                            &mut |_| Ok(()),
                        )
                        .await,
                        Err(BufferedScanError::Control("refused"))
                    ));
                }
                Stop::Exhaust => {
                    while scan
                        .next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                        .await
                        .unwrap()
                        .is_some()
                    {}
                }
            }
            assert!(scan.is_finished());
            let work = scan.work_used();
            let position = scan.into_state();
            assert!(position.is_finished());
            let before = view.stats();
            let mut scan = view.resume_edge_scan(&cx, position).unwrap();
            assert_eq!(scan.work_used(), work);
            assert!(
                scan.next_admitted_properties_with_endpoints(
                    &cx,
                    None,
                    PropertySelection::one(P),
                    &mut |_| -> Result<bool, ()> { panic!("terminal admission") },
                    &mut |_| -> Result<(), ()> { panic!("terminal work") },
                )
                .await
                .unwrap()
                .is_none()
            );
            assert_eq!(scan.work_used(), work);
            drop(scan);
            let after = view.stats();
            assert_eq!(after.hits + after.misses, before.hits + before.misses);
            drop(view.reserve_scan_bytes(&cx, pool.limit()));
            assert_eq!(pool.used(), baseline);
        }

        // Measure one complete staged stream, then split it at every candidate
        // under the same exact and one-short cumulative source allowances.
        let mut scan = view.edge_scan_relation(&cx, CommitSeq(5), S).unwrap();
        while scan
            .next_admitted_properties_with_endpoints(
                &cx,
                None,
                PropertySelection::one(P),
                &mut |_| Ok::<_, ()>(true),
                &mut |_| Ok(()),
            )
            .await
            .unwrap()
            .is_some()
        {}
        let complete_work = scan.work_used();
        assert!(complete_work > 0);
        drop(scan);
        for limit in [complete_work - 1, complete_work] {
            drop(view.reserve_scan_bytes(&cx, pool.limit()));
            let mut position = view
                .edge_scan_relation(&cx, CommitSeq(5), S)
                .unwrap()
                .into_state();
            position.work.limit = limit;
            let mut refused = false;
            let mut rows = Vec::new();
            loop {
                let prefix = position.work_used();
                let mut scan = view.resume_edge_scan(&cx, position).unwrap();
                assert_eq!(scan.work_used(), prefix);
                let result = scan
                    .next_admitted_properties_with_endpoints(
                        &cx,
                        None,
                        PropertySelection::one(P),
                        &mut |_| Ok::<_, ()>(true),
                        &mut |_| Ok(()),
                    )
                    .await;
                position = scan.into_state();
                assert!(position.work_used() >= prefix);
                match result {
                    Ok(Some(candidate)) => rows.push(candidate.eid),
                    Ok(None) => break,
                    Err(BufferedScanError::Read(BufferedReadError::Limit {
                        resource: "buffered source work",
                        requested,
                        limit: found,
                    })) => {
                        assert_eq!((requested, found), (complete_work, complete_work - 1));
                        refused = true;
                        break;
                    }
                    other => panic!("unexpected resumed source result {other:?}"),
                }
            }
            assert_eq!(refused, limit < complete_work);
            assert!(position.is_finished());
            if !refused {
                assert_eq!(rows, [EId(0), EId(2), EId(8)]);
                assert_eq!(position.work_used(), complete_work);
            }
            drop(position);
            drop(view.reserve_scan_bytes(&cx, pool.limit()));
            assert_eq!(pool.used(), baseline);
        }
        drop(view);
        drop(other);
        assert_eq!(pool.used(), 0);
        assert_eq!(other_pool.used(), 0);
    });
}

#[test]
fn staged_edge_properties_never_materialize_hidden_sidecar_siblings() {
    use fgdb_types::{
        CanonicalTimestamp, CollationResolver, CollationResolverError, NonBinaryTextBinding,
        ObjectId, TzdbResolver,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    struct Resolver {
        available: AtomicBool,
        calls: AtomicUsize,
    }
    impl CollationResolver for Resolver {
        fn artifact_available(&self, _: &ObjectId) -> bool {
            false
        }
        fn canonical_sort_key_len(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
        ) -> Result<usize, CollationResolverError> {
            Err(CollationResolverError::new(1))
        }
        fn write_canonical_sort_key(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
            _: &mut [u8],
        ) -> Result<usize, CollationResolverError> {
            Err(CollationResolverError::new(1))
        }
        fn canonical_sort_key_matches(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
            _: &[u8],
        ) -> Result<bool, CollationResolverError> {
            Err(CollationResolverError::new(1))
        }
    }
    impl TzdbResolver for Resolver {
        fn contains_tzdb(&self, oid: &ObjectId) -> bool {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.available.load(Ordering::Relaxed) && *oid == ObjectId([9; 32])
        }
        fn canonical_utc_offset_seconds(
            &self,
            oid: &ObjectId,
            zone: &str,
            instant: i128,
        ) -> Option<i32> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            (self.available.load(Ordering::Relaxed)
                && *oid == ObjectId([9; 32])
                && zone == "America/New_York"
                && instant == 1_735_689_600_123_456_789)
                .then_some(-18_000)
        }
    }

    run(|contexts, path| async move {
        let commit = contexts.commit();
        let cx = contexts.query();
        let resolver = Arc::new(Resolver {
            available: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
        });
        let store = BlockStore::open(
            &commit,
            &path,
            [0x37; 32],
            DatabaseSecurityNamespaceId([0x63; 32]),
        )
        .await
        .unwrap()
        .with_scalar_resolver(resolver.clone());
        // The hidden edge is first in adjacency/sidecar order, while the
        // visible edge is first in candidate EId order.
        let entries = [entry(9, 1, 2, R, 1, None), entry(1, 1, 3, R, 1, None)];
        let hidden = CanonicalScalar::Timestamp(
            CanonicalTimestamp::zoned(
                1_735_689_600_123_456_789,
                -18_000,
                "America/New_York",
                ObjectId([9; 32]),
                resolver.as_ref(),
            )
            .unwrap(),
        );
        let rows = vec![
            vec![(P, hidden.into())],
            vec![(P, CanonicalScalar::Int(7).into())],
        ];
        let sidecar = store
            .put_edge_property_patch(&commit, &encode_property_patch(&rows).unwrap())
            .await
            .unwrap();
        let bytes =
            encode_block_with_properties(0, None, &entries, sidecar.0, &[1, 2], &rows).unwrap();
        let block = store.put(&commit, &bytes).await.unwrap();
        let mut hidden_vertex = vertex(2, 1, None, 20);
        hidden_vertex.labels = vec![LabelId(77)];
        let vertices = [
            vertex(1, 1, None, 10),
            hidden_vertex,
            vertex(3, 1, None, 30),
        ];
        let patch = store
            .put_patch(&commit, &encode_patch(&vertices).unwrap())
            .await
            .unwrap();
        let id = store
            .put_root(
                &commit,
                &PartitionRoot {
                    graph: GraphId(1),
                    branch: BranchId(2),
                    partition: 0,
                    published_at: CommitSeq(1),
                    blocks: vec![BlockRef {
                        block_id: block.0,
                        first_seq: CommitSeq(1),
                        last_seq: CommitSeq(1),
                    }],
                    vertex_patches: vec![PatchRef {
                        patch_id: patch.0,
                        first_seq: CommitSeq(1),
                        last_seq: CommitSeq(1),
                    }],
                },
            )
            .await
            .unwrap();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        drop(view.reserve_scan_bytes(&cx, pool.limit()));
        let baseline = pool.used();
        resolver.available.store(false, Ordering::Relaxed);
        resolver.calls.store(0, Ordering::Relaxed);

        let mut scan = view.edge_scan(&cx, CommitSeq(1)).unwrap();
        let mut identities = Vec::new();
        let mut visible = Vec::new();
        while let Some(candidate) = scan
            .next_admitted_properties_with_endpoints(
                &cx,
                None,
                PropertySelection::one(P),
                &mut |row| Ok(!row.target_vertex().labels().contains(&LabelId(77))),
                &mut |event| {
                    if let BufferedEdgeScanEvent::Identity(eid) = event {
                        identities.push(eid);
                    }
                    Ok::<_, ()>(())
                },
            )
            .await
            .unwrap()
        {
            if let Some(row) = candidate.row {
                visible.push((candidate.eid, row.edge().value(P).cloned()));
            }
        }
        assert_eq!(identities, [EId(1), EId(9)]);
        assert_eq!(visible, [(EId(1), Some(CanonicalScalar::Int(7).into()))]);
        assert_eq!(resolver.calls.load(Ordering::Relaxed), 0);
        drop(scan);
        drop(view.reserve_scan_bytes(&cx, pool.limit()));
        assert_eq!(pool.used(), baseline);

        // Accepting that sibling makes its unavailable payload a real read
        // error, after the earlier visible row was delivered without it.
        let mut scan = view.edge_scan(&cx, CommitSeq(1)).unwrap();
        let first = scan
            .next_admitted_properties_with_endpoints(
                &cx,
                None,
                PropertySelection::one(P),
                &mut |_| Ok::<_, ()>(true),
                &mut |_| Ok(()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.eid, EId(1));
        assert_eq!(resolver.calls.load(Ordering::Relaxed), 0);
        drop(first);
        let mut admitted = Vec::new();
        assert!(matches!(
            scan.next_admitted_properties_with_endpoints(
                &cx,
                None,
                PropertySelection::one(P),
                &mut |row| {
                    admitted.push(row.edge().entry().eid);
                    Ok::<_, ()>(true)
                },
                &mut |_| Ok(()),
            )
            .await,
            Err(BufferedScanError::Read(_))
        ));
        assert_eq!(admitted, [EId(9)]);
        assert!(resolver.calls.load(Ordering::Relaxed) > 0);
        assert!(scan.is_finished());
        drop(scan);
        drop(view.reserve_scan_bytes(&cx, pool.limit()));
        assert_eq!(pool.used(), baseline);

        // Re-enabling the resolver makes both selected rows readable; no
        // failed prefix altered the immutable source.
        resolver.available.store(true, Ordering::Relaxed);
        let mut scan = view.edge_scan(&cx, CommitSeq(1)).unwrap();
        let mut found = Vec::new();
        while let Some(candidate) = scan
            .next_admitted_properties_with_endpoints(
                &cx,
                None,
                PropertySelection::one(P),
                &mut |_| Ok::<_, ()>(true),
                &mut |_| Ok(()),
            )
            .await
            .unwrap()
        {
            found.push(candidate.eid);
        }
        assert_eq!(found, [EId(1), EId(9)]);
        drop(scan);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}
