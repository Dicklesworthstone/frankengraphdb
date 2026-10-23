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
        // Open admission now also builds the charged descriptor directory
        // (68bf6b14): 6,925 units for these 160 blocks, measured. The scan
        // keeps its own linear allowance, asserted below.
        bounds.max_work = count * 50;
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
