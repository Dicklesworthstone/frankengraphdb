use super::*;
use crate::edge_props::encode_property_patch;
use crate::root::{BlockRef, PartitionRoot, PatchRef};
use crate::store::{BlockStore, BufferedReadLimits};
use crate::tiered::buffer::BufferLimits;
use crate::tiered::memory::MemoryPool;
use crate::vertex::encode_patch;
use crate::{PartitionRootVersion, encode_block_with_properties};
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
const TOPOLOGY: [(u128, u128, u128, RelationId); 5] = [
    (0, 1, 2, R),
    (4, 1, 3, R),
    (6, 2, 1, S),
    (8, 2, 3, S),
    (u128::MAX, 1, 1, R),
];
fn run<F: Future<Output = ()> + Send>(
    test: impl FnOnce(PurposeContexts, PathBuf) -> F + Send + 'static,
) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fgdb-cold-incidence-stream-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir(&path).unwrap();
    let (_, report) = run_async_under_lab(0xe632, |root| async move {
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
    id: u128,
    src: u128,
    dst: u128,
    relation: RelationId,
    created: u64,
    retired: Option<u64>,
) -> AdjacencyEntry {
    AdjacencyEntry {
        eid: EId(id),
        src: VId(src),
        dst: VId(dst),
        relation,
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
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
    // Physical adjacency order is deliberately NOT identity order. Incoming
    // scans must filter positions while retaining the original property locator.
    let definitions = [
        (
            vec![
                entry(u128::MAX, 1, 1, R, 1, None),
                entry(0, 1, 2, R, 1, None),
                entry(4, 1, 3, R, 1, None),
            ],
            vec![99, 10, 40],
        ),
        (
            vec![
                entry(0, 1, 2, R, 1, Some(3)),
                entry(4, 1, 3, R, 1, Some(3)),
                entry(4, 1, 3, R, 3, None),
            ],
            vec![10, 40, 43],
        ),
        (
            vec![entry(6, 2, 1, S, 3, None), entry(8, 2, 3, S, 3, None)],
            vec![60, 80],
        ),
    ];
    let mut blocks: Vec<BlockRef> = Vec::new();
    for (entries, values) in definitions {
        let props: Vec<_> = values
            .into_iter()
            .map(|n| vec![(P, CanonicalScalar::Int(n))])
            .collect();
        let patch = store
            .put_edge_property_patch(&cx, &encode_property_patch(&props).unwrap())
            .await
            .unwrap();
        let locators: Vec<_> = (1..=entries.len()).map(|n| n as u8).collect();
        let predecessor = if blocks.len() == 1 {
            Some(crate::DeltaBlockVersion(blocks[0].block_id))
        } else {
            None
        };
        let encoded =
            encode_block_with_properties(0, predecessor, &entries, patch.0, &locators, &props)
                .unwrap();
        let id = store.put(&cx, &encoded).await.unwrap();
        blocks.push(BlockRef {
            block_id: id.0,
            first_seq: entries.iter().map(|e| e.created_at).min().unwrap(),
            last_seq: entries
                .iter()
                .map(|e| e.retired_at.unwrap_or(e.created_at))
                .max()
                .unwrap(),
        });
    }
    let vertices: Vec<_> = (1..=3)
        .map(|id| VertexRow {
            vid: VId(id),
            birth_ordinal: id as u64,
            created_at: CommitSeq(1),
            retired_at: None,
            labels: vec![LabelId(9)],
            props: vec![(P, CanonicalScalar::Int(id as i64 * 10))],
        })
        .collect();
    let patch = store
        .put_patch(&cx, &encode_patch(&vertices).unwrap())
        .await
        .unwrap();
    let root = PartitionRoot {
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
    };
    let id = store.put_root(&cx, &root).await.unwrap();
    (store, id)
}
fn expected(
    endpoint: VId,
    relation: Option<RelationId>,
    direction: BufferedEdgeDirection,
    after: Option<EId>,
) -> Vec<EId> {
    TOPOLOGY
        .iter()
        .filter(|&&(id, src, dst, rel)| {
            after.is_none_or(|after| id > after.0)
                && relation.is_none_or(|r| r == rel)
                && match direction {
                    BufferedEdgeDirection::Outgoing => src == endpoint.0,
                    BufferedEdgeDirection::Incoming => dst == endpoint.0,
                    BufferedEdgeDirection::Undirected => src == endpoint.0 || dst == endpoint.0,
                }
        })
        .map(|&(id, _, _, _)| EId(id))
        .collect()
}
fn visible(id: EId, cut: u64) -> Option<i64> {
    match id.0 {
        0 if (1..3).contains(&cut) => Some(10),
        4 if cut >= 3 => Some(43),
        4 if cut >= 1 => Some(40),
        6 if cut >= 3 => Some(60),
        8 if cut >= 3 => Some(80),
        u128::MAX if cut >= 1 => Some(99),
        _ => None,
    }
}

#[test]
fn retained_histories_match_independent_all_direction_cut_and_position_oracles() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let mut cases = 0;
        for cut in 0..=3 {
            for endpoint in [VId(1), VId(2), VId(3), VId(99)] {
                for relation in [None, Some(R), Some(S)] {
                    for direction in [
                        BufferedEdgeDirection::Outgoing,
                        BufferedEdgeDirection::Incoming,
                        BufferedEdgeDirection::Undirected,
                    ] {
                        for after in [None, Some(EId(0)), Some(EId(4)), Some(EId(u128::MAX))] {
                            let wanted = expected(endpoint, relation, direction, after);
                            let mut scan = view
                                .edge_scan(&cx, CommitSeq(cut))
                                .unwrap()
                                .into_incidence_scan(endpoint, relation, direction, after);
                            let mut ids = Vec::new();
                            let mut observations = Vec::new();
                            while let Some(candidate) = scan
                                .next_candidate_with(&cx, &mut |event| {
                                    if let BufferedEdgeScanEvent::Identity(id) = event {
                                        observations.push(id);
                                    }
                                    Ok::<_, ()>(())
                                })
                                .await
                                .unwrap()
                            {
                                ids.push(candidate.eid);
                                assert_eq!(
                                    candidate.row.as_ref().map(|r| r.edge().props.clone()),
                                    visible(candidate.eid, cut)
                                        .map(|value| vec![(P, CanonicalScalar::Int(value))])
                                );
                                if let Some(row) = candidate.row {
                                    assert_eq!(row.edge().entry.src, row.source_vertex().vid);
                                    assert_eq!(row.edge().entry.dst, row.target_vertex().vid);
                                    for vertex in [row.source_vertex(), row.target_vertex()] {
                                        assert_eq!(
                                            vertex.props,
                                            [(P, CanonicalScalar::Int(vertex.vid.0 as i64 * 10))]
                                        );
                                    }
                                    assert_eq!(
                                        core::ptr::eq(row.source_vertex(), row.target_vertex()),
                                        row.edge().entry.src == row.edge().entry.dst
                                    );
                                }
                                if !scan.is_finished() {
                                    assert!(
                                        scan.driver.as_ref().unwrap().root.state.is_none(),
                                        "incidence retains no whole-partition root heap"
                                    );
                                    assert!(scan.state.as_ref().unwrap().incidence.is_some());
                                }
                            }
                            assert_eq!(ids, wanted);
                            assert_eq!(observations, wanted);
                            assert!(scan.is_finished());
                            assert!(
                                scan.next_candidate_with(&cx, &mut |_| Err::<(), _>("finished"))
                                    .await
                                    .unwrap()
                                    .is_none()
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 576);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn retained_merge_does_less_source_work_than_reopening_each_successor() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let mut independent = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
        let mut after = None;
        let mut old = Vec::new();
        while let Some(candidate) = independent
            .next_incident_with_endpoints(
                &cx,
                VId(1),
                None,
                BufferedEdgeDirection::Undirected,
                after,
                &mut |_| Ok::<_, ()>(()),
            )
            .await
            .unwrap()
        {
            old.push(candidate.eid);
            after = Some(candidate.eid);
        }
        let old_work = independent.work_used();
        drop(independent);
        let mut retained = view
            .edge_scan(&cx, CommitSeq(3))
            .unwrap()
            .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
        let mut new = Vec::new();
        while let Some(candidate) = retained
            .next_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
        {
            new.push(candidate.eid);
        }
        assert_eq!(new, old);
        assert!(
            retained.work_used() < old_work,
            "same source and answers, not a wall-clock benchmark"
        );
        drop(retained);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn retained_pull_refusal_at_every_continuation_checkpoint_fuses_and_releases_state() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let mut scan = view
            .edge_scan(&cx, CommitSeq(3))
            .unwrap()
            .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
        drop(
            scan.next_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                .await
                .unwrap(),
        );
        let mut count = 0;
        drop(
            scan.next_candidate_with(&cx, &mut |_| {
                count += 1;
                Ok::<_, ()>(())
            })
            .await
            .unwrap(),
        );
        assert!(count > 0);
        drop(scan);
        for stop in 1..=count {
            let mut scan = view
                .edge_scan(&cx, CommitSeq(3))
                .unwrap()
                .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
            drop(
                scan.next_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                    .await
                    .unwrap(),
            );
            let mut seen = 0;
            let result = scan
                .next_candidate_with(&cx, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                })
                .await;
            assert!(matches!(result, Err(BufferedScanError::Control(at)) if at == stop));
            assert_eq!(seen, stop);
            assert!(scan.is_finished());
            assert!(scan.state.is_none());
            assert!(
                scan.next_candidate_with(&cx, &mut |_| Err::<(), _>("must not retry"))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn admission_and_work_refusals_do_not_publish_a_partial_incidence_stream() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let mut scan = view
            .edge_scan(&cx, CommitSeq(3))
            .unwrap()
            .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
        while scan.next(&cx).await.unwrap().is_some() {}
        let exact = scan.work_used();
        drop(scan);
        for cap in [0, exact - 1, exact] {
            let mut scan = view
                .edge_scan(&cx, CommitSeq(3))
                .unwrap()
                .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
            scan.driver.as_mut().unwrap().root.work.limit = cap;
            let mut refused = false;
            loop {
                match scan.next(&cx).await {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(BufferedReadError::Limit { .. }) => {
                        refused = true;
                        break;
                    }
                    Err(other) => panic!("unexpected source error: {other:?}"),
                }
            }
            assert_eq!(refused, cap < exact);
            assert!(scan.is_finished());
            assert!(scan.next(&cx).await.unwrap().is_none());
        }
        // The initial routing pass is bounded too, before the first identity.
        let mut scan = view
            .edge_scan(&cx, CommitSeq(3))
            .unwrap()
            .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
        let result = scan
            .next_candidate_with(&cx, &mut |_| Err::<(), _>("admission"))
            .await;
        assert!(matches!(
            result,
            Err(BufferedScanError::Control("admission"))
        ));
        assert!(scan.is_finished());
        drop(scan);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn close_unpolled_pull_and_exhausted_source_preserve_ownership_and_full_width_bounds() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let mut scan = view
            .edge_scan(&cx, CommitSeq(1))
            .unwrap()
            .into_incidence_scan(
                VId(1),
                Some(R),
                BufferedEdgeDirection::Outgoing,
                Some(EId(4)),
            );
        drop(scan.next(&cx));
        assert_eq!(scan.work_used(), 0);
        assert!(!scan.is_finished());
        let held = scan.next(&cx).await.unwrap().unwrap();
        assert_eq!(held.edge().entry.eid, EId(u128::MAX));
        assert!(scan.is_finished());
        scan.close();
        scan.close();
        assert!(scan.next(&cx).await.unwrap().is_none());
        drop(scan);
        let mut closed = view.edge_scan(&cx, CommitSeq(1)).unwrap();
        closed.close();
        let mut closed =
            closed.into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
        assert!(closed.is_finished());
        assert!(closed.next(&cx).await.unwrap().is_none());
        drop(closed);
        drop(view);
        assert!(
            pool.used() > 0,
            "output must retain its own storage reservation"
        );
        assert_eq!(held.source_vertex().vid, VId(1));
        drop(held);
        assert_eq!(pool.used(), 0);
    });
}

mod continuations;
