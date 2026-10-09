use super::*;
use crate::edge_props::encode_property_patch;
use crate::root::{BlockRef, PartitionRoot, PatchRef};
use crate::store::{BlockStore, BufferedReadLimits};
use crate::tiered::buffer::BufferLimits;
use crate::tiered::memory::MemoryPool;
use crate::vertex::encode_patch;
use crate::{DeltaBlockVersion, PartitionRootVersion, encode_block_with_properties};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{
    BranchId, CanonicalScalar, DatabaseSecurityNamespaceId, GraphId, PurposeContexts,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(5);
const S: RelationId = RelationId(8);
const P: PropertyKeyId = PropertyKeyId(11);
const TOPOLOGY: [(u128, u128, u128, RelationId); 7] = [
    (0, 2, 2, S),
    (2, 2, 1, S),
    (4, 1, 2, R),
    (6, 1, 2, R),
    (7, 9, 10, R),
    (90, 1, 1, R),
    (u128::MAX, 1, 3, R),
];
fn run<F: Future<Output = ()> + Send>(
    test: impl FnOnce(PurposeContexts, PathBuf) -> F + Send + 'static,
) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fgdb-cold-incidence-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).unwrap();
    let (_, report) = run_async_under_lab(0xe6_31, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root), path).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
fn limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 4096,
        max_source_bytes: 1024 * 1024,
        max_blocks: 32,
        max_vertex_patches: 32,
        max_work: 2_000_000,
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
fn vertex(id: u128, created: u64, retired: Option<u64>, p: i64) -> VertexRow {
    VertexRow {
        vid: VId(id),
        birth_ordinal: id as u64,
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
        labels: vec![LabelId(9)],
        props: vec![(P, CanonicalScalar::Int(p))],
    }
}
async fn fixture(contexts: &PurposeContexts, path: &Path) -> (BlockStore, PartitionRootVersion) {
    let cx = contexts.commit();
    let store = BlockStore::open(
        &cx,
        path,
        [0x37; 32],
        DatabaseSecurityNamespaceId([0x63; 32]),
    )
    .await
    .unwrap();
    // Adjacency order differs from EId order. The second block restates a
    // deletion and property update; the fourth block is unrelated to 1/2/3.
    let definitions = [
        (
            vec![
                entry(90, 1, 1, R, 1, None),
                entry(4, 1, 2, R, 1, None),
                entry(6, 1, 2, R, 1, None),
                entry(u128::MAX, 1, 3, R, 1, None),
            ],
            vec![90, 40, 60, 99],
            None,
        ),
        (
            vec![
                entry(90, 1, 1, R, 1, Some(3)),
                entry(4, 1, 2, R, 1, Some(3)),
                entry(4, 1, 2, R, 3, None),
                entry(u128::MAX, 1, 3, R, 1, Some(3)),
            ],
            vec![90, 40, 43, 99],
            Some(0usize),
        ),
        (
            vec![entry(2, 2, 1, S, 3, None), entry(0, 2, 2, S, 3, None)],
            vec![20, 0],
            None,
        ),
        (vec![entry(7, 9, 10, R, 3, None)], vec![70], None),
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
                .map(|r| r.retired_at.unwrap_or(r.created_at))
                .max()
                .unwrap(),
        });
        ids.push(block);
    }
    let mut patches = Vec::new();
    for rows in [
        vec![
            vertex(1, 1, None, 10),
            vertex(2, 1, None, 20),
            vertex(3, 1, None, 30),
            vertex(9, 1, None, 90),
            vertex(10, 1, None, 100),
        ],
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
                .map(|r| r.retired_at.unwrap_or(r.created_at))
                .max()
                .unwrap(),
        });
    }
    let root = PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(2),
        partition: 0,
        published_at: CommitSeq(3),
        blocks,
        vertex_patches: patches,
    };
    let id = store.put_root(&cx, &root).await.unwrap();
    (store, id)
}
fn visible(eid: u128, cut: u64) -> Option<i64> {
    match eid {
        0 if cut >= 3 => Some(0),
        2 if cut >= 3 => Some(20),
        7 if cut >= 3 => Some(70),
        4 if cut >= 3 => Some(43),
        4 if cut >= 1 => Some(40),
        6 if cut >= 1 => Some(60),
        90 if (1..3).contains(&cut) => Some(90),
        u128::MAX if (1..3).contains(&cut) => Some(99),
        _ => None,
    }
}
fn expected(request: Incidence) -> Option<(EId, VId, VId)> {
    TOPOLOGY
        .iter()
        .find(|&&(id, src, dst, rel)| {
            request.after.is_none_or(|after| id > after.0)
                && request.relation.is_none_or(|r| rel == r)
                && match request.direction {
                    BufferedEdgeDirection::Outgoing => src == request.endpoint.0,
                    BufferedEdgeDirection::Incoming => dst == request.endpoint.0,
                    BufferedEdgeDirection::Undirected => {
                        src == request.endpoint.0 || dst == request.endpoint.0
                    }
                }
        })
        .map(|&(id, src, dst, _)| (EId(id), VId(src), VId(dst)))
}

#[test]
fn cold_successors_match_900_independent_cut_direction_relation_and_position_cases() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let mut cases = 0;
        for cut in 0..=3 {
            let mut scan = view
                .edge_scan(&cx, CommitSeq(cut))
                .unwrap()
                .into_join_scan();
            for endpoint in [1, 2, 3, 9, 42] {
                for relation in [None, Some(R), Some(S)] {
                    for direction in [
                        BufferedEdgeDirection::Outgoing,
                        BufferedEdgeDirection::Incoming,
                        BufferedEdgeDirection::Undirected,
                    ] {
                        for after in [
                            None,
                            Some(EId(0)),
                            Some(EId(4)),
                            Some(EId(89)),
                            Some(EId(u128::MAX)),
                        ] {
                            let request = Incidence {
                                endpoint: VId(endpoint),
                                relation,
                                direction,
                                after,
                            };
                            let wanted = expected(request);
                            let mut admissions = Vec::new();
                            let got = scan
                                .next_incident_with_endpoints(
                                    &cx,
                                    VId(endpoint),
                                    relation,
                                    direction,
                                    after,
                                    &mut |event| {
                                        if let BufferedEdgeScanEvent::Identity(eid) = event {
                                            admissions.push(eid);
                                        }
                                        Ok::<_, ()>(())
                                    },
                                )
                                .await
                                .unwrap();
                            assert_eq!(got.as_ref().map(|r| r.eid), wanted.map(|r| r.0));
                            assert_eq!(admissions, wanted.map(|r| vec![r.0]).unwrap_or_default());
                            if let Some(candidate) = got {
                                assert_eq!(
                                    candidate.row.as_ref().map(|r| r.edge().props.clone()),
                                    visible(candidate.eid.0, cut)
                                        .map(|p| vec![(P, CanonicalScalar::Int(p))])
                                );
                                if let Some(row) = candidate.row {
                                    let (_, src, dst) = wanted.unwrap();
                                    assert_eq!(
                                        (row.source_vertex().vid, row.target_vertex().vid),
                                        (src, dst)
                                    );
                                    for vertex in [row.source_vertex(), row.target_vertex()] {
                                        let p = if vertex.vid == VId(1) && cut >= 3 {
                                            11
                                        } else {
                                            vertex.vid.0 as i64 * 10
                                        };
                                        assert_eq!(
                                            vertex.props,
                                            vec![(P, CanonicalScalar::Int(p))]
                                        );
                                    }
                                    assert_eq!(
                                        core::ptr::eq(row.source_vertex(), row.target_vertex()),
                                        src == dst
                                    );
                                }
                            }
                            cases += 1;
                        }
                    }
                }
            }
            drop(scan);
            assert_eq!(pool.used(), baseline);
        }
        assert_eq!(cases, 900);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn nested_positions_leave_root_unchanged_and_survive_its_last_candidate() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
        assert_eq!(scan.work_used(), 0);
        drop(scan.next_incident_with_endpoints(
            &cx,
            VId(1),
            None,
            BufferedEdgeDirection::Outgoing,
            None,
            &mut |_| Ok::<_, ()>(()),
        ));
        assert_eq!(scan.work_used(), 0);
        assert!(scan.driver.as_ref().unwrap().routes.is_none());
        let nested = scan
            .next_incident_with_endpoints(
                &cx,
                VId(1),
                Some(R),
                BufferedEdgeDirection::Outgoing,
                None,
                &mut |_| Ok::<_, ()>(()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(nested.eid, EId(4));
        drop(nested);
        let first_work = scan.work_used();
        let second = scan
            .next_incident_with_endpoints(
                &cx,
                VId(1),
                Some(R),
                BufferedEdgeDirection::Outgoing,
                None,
                &mut |_| Ok::<_, ()>(()),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            scan.work_used() - first_work < first_work,
            "directory is not rebuilt"
        );
        drop(second);
        let before = scan
            .driver
            .as_ref()
            .unwrap()
            .root
            .partition
            .stats()
            .bypasses;
        assert!(
            scan.next_incident_with_endpoints(
                &cx,
                VId(42),
                None,
                BufferedEdgeDirection::Undirected,
                None,
                &mut |_| Ok::<_, ()>(())
            )
            .await
            .unwrap()
            .is_none()
        );
        assert_eq!(
            scan.driver
                .as_ref()
                .unwrap()
                .root
                .partition
                .stats()
                .bypasses,
            before,
            "an absent route never faults unrelated block payloads"
        );
        let mut roots = Vec::new();
        while let Some(row) = scan
            .next_with_endpoints(&cx, None, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
        {
            roots.push(row.eid);
        }
        assert_eq!(roots, TOPOLOGY.iter().map(|r| EId(r.0)).collect::<Vec<_>>());
        let held = scan
            .next_incident_with_endpoints(
                &cx,
                VId(2),
                Some(S),
                BufferedEdgeDirection::Undirected,
                None,
                &mut |_| Ok::<_, ()>(()),
            )
            .await
            .unwrap()
            .unwrap()
            .row
            .unwrap();
        assert_eq!(held.edge().entry.eid, EId(0));
        scan.close();
        assert!(scan.is_closed());
        assert!(
            scan.next_with_endpoints(&cx, None, &mut |_| Err::<(), _>("closed"))
                .await
                .unwrap()
                .is_none()
        );
        drop(scan);
        assert!(pool.used() > baseline);
        drop(view);
        assert!(pool.used() > 0);
        drop(held);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn routing_and_history_refusals_drop_the_whole_driver_and_keep_exact_work_limits() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
        let (mut total, mut candidate_at) = (0, 0);
        let row = scan
            .next_incident_with_endpoints(
                &cx,
                VId(1),
                Some(R),
                BufferedEdgeDirection::Outgoing,
                None,
                &mut |event| {
                    total += 1;
                    if matches!(event, BufferedEdgeScanEvent::Identity(_)) {
                        candidate_at = total;
                    }
                    Ok::<_, usize>(())
                },
            )
            .await
            .unwrap();
        drop(row);
        let exact_work = scan.work_used();
        drop(scan);
        let mut stops: Vec<_> = (0..=16).map(|n| 1 + (total - 1) * n / 16).collect();
        stops.extend([candidate_at, candidate_at + 1, total]);
        stops.sort();
        stops.dedup();
        for stop in stops {
            let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
            let mut seen = 0;
            let result = scan
                .next_incident_with_endpoints(
                    &cx,
                    VId(1),
                    Some(R),
                    BufferedEdgeDirection::Outgoing,
                    None,
                    &mut |_| {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    },
                )
                .await;
            assert!(matches!(result,Err(BufferedScanError::Control(at)) if at==stop));
            assert_eq!(seen, stop);
            assert!(scan.is_closed());
            assert_eq!(pool.used(), baseline);
            assert!(
                scan.next_incident_with_endpoints(
                    &cx,
                    VId(1),
                    None,
                    BufferedEdgeDirection::Outgoing,
                    None,
                    &mut |_| Err::<(), _>("must not resume")
                )
                .await
                .unwrap()
                .is_none()
            );
        }
        for allowed in [exact_work, exact_work - 1] {
            let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
            scan.driver.as_mut().unwrap().root.work.limit = allowed;
            let result = scan
                .next_incident_with_endpoints(
                    &cx,
                    VId(1),
                    Some(R),
                    BufferedEdgeDirection::Outgoing,
                    None,
                    &mut |_| Ok::<_, ()>(()),
                )
                .await;
            if allowed == exact_work {
                assert!(result.unwrap().is_some());
            } else {
                assert!(matches!(
                    result,
                    Err(BufferedScanError::Read(BufferedReadError::Limit { .. }))
                ));
            }
            drop(scan);
            assert_eq!(pool.used(), baseline);
        }
        let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
        let hold = pool
            .reserve(&cx, pool.limit() - pool.used() - 32 * 1024)
            .unwrap();
        assert!(matches!(
            scan.next_incident_with_endpoints(
                &cx,
                VId(1),
                None,
                BufferedEdgeDirection::Outgoing,
                None,
                &mut |_| Ok::<_, ()>(())
            )
            .await,
            Err(BufferedScanError::Read(BufferedReadError::Memory(_)))
        ));
        assert!(scan.is_closed());
        drop(scan);
        drop(hold);
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}
