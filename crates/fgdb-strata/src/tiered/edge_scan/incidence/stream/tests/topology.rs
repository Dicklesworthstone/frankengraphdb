//! Count projection preserves endpoint history while avoiding property owners.

use super::*;
use fgdb_types::{CanonicalList, CanonicalPropertyValue};

const RICH_EDGES: [(u128, u128, u128); 3] =
    [(0, 1, 1), (4, 1, u128::MAX), (u128::MAX, 1, u128::MAX)];

fn rich_vertex(vid: u128, created: u64, retired: Option<u64>, score: i64) -> VertexRow {
    VertexRow {
        vid: VId(vid),
        birth_ordinal: if vid == 1 { 1 } else { 2 },
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
        labels: vec![LabelId(9)],
        props: vec![
            (P, CanonicalScalar::Int(score).into()),
            (
                PropertyKeyId(12),
                CanonicalPropertyValue::List(
                    CanonicalList::try_new(
                        (0..128).map(|n| CanonicalScalar::Int(n).into()).collect(),
                    )
                    .unwrap(),
                ),
            ),
        ],
    }
}

async fn rich_fixture(
    contexts: &PurposeContexts,
    path: &Path,
) -> (BlockStore, PartitionRootVersion) {
    let commit = contexts.commit();
    let store = BlockStore::open(
        &commit,
        path,
        [0x37; 32],
        DatabaseSecurityNamespaceId([0x63; 32]),
    )
    .await
    .unwrap();
    // No edge properties: any difference in source work below comes from
    // endpoint property ownership, not omitted edge-property sidecar reads.
    let edges: Vec<_> = RICH_EDGES
        .iter()
        .map(|&(id, src, dst)| entry(id, src, dst, R, 1, None))
        .collect();
    let block = store
        .put(&commit, &crate::encode_block(0, None, &edges).unwrap())
        .await
        .unwrap();
    let mut vertex_patches = Vec::new();
    for (rows, last) in [
        (
            vec![
                rich_vertex(1, 1, None, 10),
                rich_vertex(u128::MAX, 1, None, 20),
            ],
            1,
        ),
        (
            vec![
                rich_vertex(u128::MAX, 1, Some(3), 20),
                rich_vertex(u128::MAX, 3, None, 30),
            ],
            3,
        ),
    ] {
        let patch = store
            .put_patch(&commit, &encode_patch(&rows).unwrap())
            .await
            .unwrap();
        vertex_patches.push(PatchRef {
            patch_id: patch.0,
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(last),
        });
    }
    let root = store
        .put_root(
            &commit,
            &PartitionRoot {
                graph: GraphId(1),
                branch: BranchId(2),
                partition: 0,
                published_at: CommitSeq(3),
                blocks: vec![BlockRef {
                    block_id: block.0,
                    first_seq: CommitSeq(1),
                    last_seq: CommitSeq(1),
                }],
                vertex_patches,
            },
        )
        .await
        .unwrap();
    (store, root)
}

#[test]
fn count_projection_matches_full_records_without_retaining_endpoint_properties() {
    run(|contexts, path| async move {
        let (store, root) = rich_fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        use BufferedEdgeDirection::{Incoming, Outgoing, Undirected};
        for (cut, endpoint, relation, direction, after) in [
            (0, VId(1), None, Undirected, None),
            (1, VId(1), None, Undirected, None),
            (3, VId(1), None, Undirected, None),
            (3, VId(u128::MAX), Some(R), Incoming, None),
            (3, VId(u128::MAX), None, Outgoing, None),
            (3, VId(u128::MAX), None, Undirected, None),
            (3, VId(1), Some(S), Outgoing, None),
            (3, VId(99), None, Undirected, None),
            (3, VId(1), Some(R), Outgoing, Some(EId(0))),
            (3, VId(1), None, Outgoing, Some(EId(4))),
            (3, VId(1), None, Outgoing, Some(EId(u128::MAX))),
            (3, VId(1), None, Incoming, None),
        ] {
            let wanted: Vec<_> = RICH_EDGES
                .iter()
                .filter(|&&(id, src, dst)| {
                    after.is_none_or(|after| id > after.0)
                        && relation.is_none_or(|relation| relation == R)
                        && match direction {
                            Outgoing => src == endpoint.0,
                            Incoming => dst == endpoint.0,
                            Undirected => src == endpoint.0 || dst == endpoint.0,
                        }
                })
                .map(|&(id, _, _)| EId(id))
                .collect();
            let mut scan = view
                .edge_scan(&cx, CommitSeq(cut))
                .unwrap()
                .into_incidence_scan(endpoint, relation, direction, after);
            let mut complete = Vec::new();
            while let Some(candidate) = scan
                .next_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                .await
                .unwrap()
            {
                complete.push((candidate.eid, candidate.row.is_some()));
                if let Some(row) = candidate.row {
                    assert!(row.edge().props.is_empty());
                    for vertex in [row.source_vertex(), row.target_vertex()] {
                        let (created, score) = if vertex.vid == VId(1) {
                            (1, 10)
                        } else if cut < 3 {
                            (1, 20)
                        } else {
                            (3, 30)
                        };
                        assert_eq!(vertex.created_at, CommitSeq(created));
                        assert_eq!(
                            vertex.props,
                            rich_vertex(vertex.vid.0, created, None, score).props
                        );
                    }
                }
            }
            let full_work = scan.work_used();
            drop(scan);
            assert_eq!(pool.used(), baseline);
            assert_eq!(
                complete,
                wanted
                    .iter()
                    .map(|&eid| (eid, cut >= 1))
                    .collect::<Vec<_>>()
            );
            let mut scan = view
                .edge_scan(&cx, CommitSeq(cut))
                .unwrap()
                .into_incidence_scan(endpoint, relation, direction, after);
            let mut projected = Vec::new();
            let mut identities = Vec::new();
            while let Some(candidate) = scan
                .next_candidate_with_access(&cx, EdgePropertyAccess::Skip, &mut |event| {
                    if let BufferedEdgeScanEvent::Identity(eid) = event {
                        identities.push(eid);
                    }
                    Ok::<_, ()>(())
                })
                .await
                .unwrap()
            {
                projected.push((candidate.eid, candidate.row.is_some()));
                assert!(matches!(
                    candidate.row,
                    None | Some(IncidenceProjection::Visible)
                ));
            }
            assert_eq!(projected, complete);
            assert_eq!(identities, wanted);
            if cut >= 1 && !wanted.is_empty() {
                assert!(
                    scan.work_used() < full_work,
                    "nested property ownership visits disappear; this is not a timing benchmark"
                );
            }
            drop(scan);
            assert_eq!(pool.used(), baseline);
            let count = view
                .edge_scan(&cx, CommitSeq(cut))
                .unwrap()
                .into_incidence_scan(endpoint, relation, direction, after)
                .count_visible(&cx)
                .await
                .unwrap();
            assert_eq!(count, if cut >= 1 { wanted.len() as u64 } else { 0 });
            assert_eq!(pool.used(), baseline);
        }
        // A projected pull cannot contaminate the next complete endpoint row.
        let mut scan = view
            .edge_scan(&cx, CommitSeq(3))
            .unwrap()
            .into_incidence_scan(VId(1), None, Undirected, None);
        let projected = scan
            .next_candidate_with_access(&cx, EdgePropertyAccess::Skip, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(projected.eid, EId(0));
        assert!(matches!(projected.row, Some(IncidenceProjection::Visible)));
        let complete = scan.next(&cx).await.unwrap().unwrap();
        assert_eq!(complete.edge().entry.eid, EId(4));
        assert_eq!(
            complete.source_vertex().props,
            rich_vertex(1, 1, None, 10).props
        );
        assert_eq!(
            complete.target_vertex().props,
            rich_vertex(u128::MAX, 3, None, 30).props
        );
        drop(complete);
        drop(scan);
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn count_projection_refunds_every_control_and_resource_refusal() {
    run(|contexts, path| async move {
        let (store, root) = rich_fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let mut scan = view
            .edge_scan(&cx, CommitSeq(3))
            .unwrap()
            .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
        drop(
            scan.next_candidate_with_access(
                &cx,
                EdgePropertyAccess::Skip,
                &mut |_| Ok::<_, ()>(()),
            )
            .await
            .unwrap(),
        );
        let mut checkpoints = 0;
        drop(
            scan.next_candidate_with_access(&cx, EdgePropertyAccess::Skip, &mut |_| {
                checkpoints += 1;
                Ok::<_, ()>(())
            })
            .await
            .unwrap(),
        );
        while scan
            .next_candidate_with_access(&cx, EdgePropertyAccess::Skip, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .is_some()
        {}
        let exact = scan.work_used();
        assert!(checkpoints > 0);
        drop(scan);
        assert_eq!(pool.used(), baseline);
        for stop in 1..=checkpoints {
            let mut scan = view
                .edge_scan(&cx, CommitSeq(3))
                .unwrap()
                .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
            drop(
                scan.next_candidate_with_access(&cx, EdgePropertyAccess::Skip, &mut |_| {
                    Ok::<_, ()>(())
                })
                .await
                .unwrap(),
            );
            let mut seen = 0;
            let result = scan
                .next_candidate_with_access(&cx, EdgePropertyAccess::Skip, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                })
                .await;
            assert!(matches!(result, Err(BufferedScanError::Control(at)) if at == stop));
            assert_eq!(seen, stop);
            assert!(scan.is_finished());
            assert!(scan.state.is_none());
            assert!(scan.next(&cx).await.unwrap().is_none());
            drop(scan);
            assert_eq!(pool.used(), baseline);
        }
        for cap in [0, exact - 1, exact] {
            let mut scan = view
                .edge_scan(&cx, CommitSeq(3))
                .unwrap()
                .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
            scan.driver.as_mut().unwrap().root.work.limit = cap;
            let result = scan.count_visible(&cx).await;
            if cap < exact {
                assert!(matches!(result, Err(BufferedReadError::Limit { .. })));
            } else {
                assert_eq!(result.unwrap(), 3);
            }
            assert_eq!(pool.used(), baseline);
        }
        let held = pool.reserve(&cx, pool.available() - 1).unwrap();
        let result = view
            .edge_scan(&cx, CommitSeq(3))
            .unwrap()
            .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None)
            .count_visible(&cx)
            .await;
        assert!(matches!(
            result,
            Err(BufferedReadError::Buffer(BufferError::Memory(
                MemoryError::ResourceExhausted { .. }
            )))
        ));
        drop(held);
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn count_projection_preserves_retirement_and_authenticated_refault_errors() {
    run(|contexts, path| async move {
        let (store, root) = rich_fixture(&contexts, &path).await;
        let commit = contexts.commit();
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let mut retired_root = view.root().clone();
        drop(view);
        let retirement = store
            .put_patch(
                &commit,
                &encode_patch(&[rich_vertex(u128::MAX, 3, Some(4), 30)]).unwrap(),
            )
            .await
            .unwrap();
        retired_root.published_at = CommitSeq(4);
        retired_root.vertex_patches.push(PatchRef {
            patch_id: retirement.0,
            first_seq: CommitSeq(3),
            last_seq: CommitSeq(4),
        });
        let retired_root = store.put_root(&commit, &retired_root).await.unwrap();
        let mut view = store
            .open_buffered_root(&cx, retired_root, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        for cut in [3, 4] {
            let mut scan = view
                .edge_scan(&cx, CommitSeq(cut))
                .unwrap()
                .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
            assert_eq!(
                scan.next(&cx).await.unwrap().unwrap().edge().entry.eid,
                EId(0)
            );
            let next = scan.next(&cx).await;
            if cut == 4 {
                assert!(matches!(next, Err(BufferedReadError::DanglingEndpoint)));
                assert!(scan.is_finished());
            } else {
                assert_eq!(next.unwrap().unwrap().edge().entry.eid, EId(4));
            }
            drop(scan);
            let count = view
                .edge_scan(&cx, CommitSeq(cut))
                .unwrap()
                .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None)
                .count_visible(&cx)
                .await;
            if cut == 4 {
                assert!(matches!(count, Err(BufferedReadError::DanglingEndpoint)));
            } else {
                assert_eq!(count.unwrap(), 3);
            }
            assert_eq!(pool.used(), baseline);
        }
        let patch_path = store.path(view.root().vertex_patches[0].patch_id);
        let mut bytes = std::fs::read(&patch_path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&patch_path, bytes).unwrap();
        let mut scan = view
            .edge_scan(&cx, CommitSeq(1))
            .unwrap()
            .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None);
        assert!(matches!(
            scan.next(&cx).await,
            Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
        ));
        assert!(scan.is_finished());
        drop(scan);
        let count = view
            .edge_scan(&cx, CommitSeq(1))
            .unwrap()
            .into_incidence_scan(VId(1), None, BufferedEdgeDirection::Undirected, None)
            .count_visible(&cx)
            .await;
        assert!(matches!(
            count,
            Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
        ));
        // No visible candidate means no endpoint fault, even if a requested
        // endpoint does not exist or an unrelated admitted patch has changed.
        for (endpoint, relation) in [(VId(99), None), (VId(1), Some(S))] {
            assert_eq!(
                view.edge_scan(&cx, CommitSeq(4))
                    .unwrap()
                    .into_incidence_scan(
                        endpoint,
                        relation,
                        BufferedEdgeDirection::Undirected,
                        None,
                    )
                    .count_visible(&cx)
                    .await
                    .unwrap(),
                0
            );
        }
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}
