use super::*;

#[test]
fn selected_roots_merge_complete_histories_once_in_eid_order_for_every_direction() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        for cut in 0..=3 {
            for relation in [None, Some(R), Some(S)] {
                for direction in [
                    BufferedEdgeDirection::Outgoing,
                    BufferedEdgeDirection::Incoming,
                    BufferedEdgeDirection::Undirected,
                ] {
                    for vertices in [
                        vec![],
                        vec![VId(1)],
                        vec![VId(2)],
                        vec![VId(1), VId(2), VId(3)],
                        vec![VId(9), VId(10)],
                        vec![VId(42)],
                        vec![VId(u128::MAX)],
                    ] {
                        let expected: Vec<_> = TOPOLOGY
                            .iter()
                            .filter(|&&(_, src, dst, rel)| {
                                relation.is_none_or(|r| r == rel)
                                    && match direction {
                                        BufferedEdgeDirection::Outgoing => {
                                            vertices.contains(&VId(src))
                                        }
                                        BufferedEdgeDirection::Incoming => {
                                            vertices.contains(&VId(dst))
                                        }
                                        BufferedEdgeDirection::Undirected => {
                                            vertices.contains(&VId(src))
                                                || vertices.contains(&VId(dst))
                                        }
                                    }
                            })
                            .map(|&(eid, _, _, _)| EId(eid))
                            .collect();
                        let root = match relation {
                            Some(relation) => {
                                view.edge_scan_relation(&cx, CommitSeq(cut), relation)
                            }
                            None => view.edge_scan(&cx, CommitSeq(cut)),
                        };
                        let mut scan = root.unwrap().into_join_scan();
                        scan.select_root_vertices_with(&cx, &vertices, direction, &mut |event| {
                            assert_eq!(event, BufferedEdgeScanEvent::Work);
                            Ok::<_, ()>(())
                        })
                        .await
                        .unwrap();
                        if let Some(state) = &scan.driver.as_ref().unwrap().root.state {
                            assert!(state.active.is_none());
                            assert!(state.orders.iter().all(Option::is_some));
                            state.check_retained_bytes().unwrap();
                        }
                        let mut admissions = Vec::new();
                        let mut actual = Vec::new();
                        while let Some(candidate) = scan
                            .next_with_endpoints(&cx, relation, &mut |event| {
                                if let BufferedEdgeScanEvent::Identity(eid) = event {
                                    admissions.push(eid);
                                }
                                Ok::<_, ()>(())
                            })
                            .await
                            .unwrap()
                        {
                            actual.push(candidate.eid);
                            assert_eq!(
                                candidate.row.as_ref().map(|row| row.edge().props.clone()),
                                visible(candidate.eid.0, cut).map(|value| vec![(
                                    P,
                                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value)),
                                )]),
                                "complete history at cut {cut}: {:?}",
                                candidate.eid,
                            );
                        }
                        assert_eq!(actual, expected, "{vertices:?} {direction:?} {relation:?}");
                        assert_eq!(admissions, expected);
                        assert!(actual.windows(2).all(|pair| pair[0] < pair[1]));
                        // Root selection/exhaustion must not constrain later
                        // incidence, even to another relation or endpoint.
                        let child = scan
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
                        assert_eq!(child.eid, EId(0));
                    }
                }
            }
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn selection_routes_only_matching_blocks_and_refuses_candidates_before_payloads() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let before = view.stats();
        let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
        // Both endpoints route the same one-row block. Deduplication must
        // happen before its topology fault and before any heap insertion.
        scan.select_root_vertices_with(
            &cx,
            &[VId(9), VId(10)],
            BufferedEdgeDirection::Undirected,
            &mut |_| Ok::<_, ()>(()),
        )
        .await
        .unwrap();
        let driver = scan.driver.as_ref().unwrap();
        let after = driver.root.partition.stats();
        assert_eq!(after.hits + after.misses, before.hits + before.misses + 1);
        let state = driver.root.state.as_ref().unwrap();
        assert_eq!(state.heads.len(), 1);
        assert_eq!(state.orders.len(), 1);
        assert_eq!(state.orders[0].as_ref().unwrap().len(), 1);
        let refused = scan
            .next_with_endpoints(&cx, None, &mut |event| match event {
                BufferedEdgeScanEvent::Work => Ok(()),
                BufferedEdgeScanEvent::Identity(eid) => {
                    assert_eq!(eid, EId(7));
                    Err("candidate quota")
                }
            })
            .await;
        assert!(matches!(
            refused,
            Err(BufferedScanError::Control("candidate quota"))
        ));
        assert!(scan.is_closed());
        drop(scan);
        assert_eq!(
            view.stats(),
            after,
            "no full sidecar or endpoint lookup before admission"
        );
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn sparse_root_scratch_and_selected_payloads_do_not_grow_with_unrelated_blocks() {
    run(|contexts, path| async move {
        let (store, base) = fixture(&contexts, &path).await;
        let commit = contexts.commit();
        let mut root = store.get_root(&commit, base).await.unwrap();
        let mut vertices = Vec::new();
        // Cross a machine-word boundary in the admitted block population.
        // These are real authenticated blocks with disjoint endpoint domains,
        // not directory-only doubles or duplicate root references.
        for at in 0..64u128 {
            let source = 1000 + at * 2;
            let rows = [entry(1000 + at, source, source + 1, R, 3, None)];
            let block = store
                .put(&commit, &crate::encode_block(0, None, &rows).unwrap())
                .await
                .unwrap();
            root.blocks.push(BlockRef {
                block_id: block.0,
                first_seq: CommitSeq(3),
                last_seq: CommitSeq(3),
            });
            vertices.push(vertex(source, 3, None, 0));
            vertices.push(vertex(source + 1, 3, None, 0));
        }
        let patch = store
            .put_patch(&commit, &encode_patch(&vertices).unwrap())
            .await
            .unwrap();
        root.vertex_patches.push(PatchRef {
            patch_id: patch.0,
            first_seq: CommitSeq(3),
            last_seq: CommitSeq(3),
        });
        let expanded = store.put_root(&commit, &root).await.unwrap();
        let cx = contexts.query();
        let mut source_limits = limits();
        source_limits.max_root_bytes = 64 * 1024;
        source_limits.max_blocks = 128;
        let mut scratch_requests = Vec::new();
        for id in [base, expanded] {
            let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
            let mut view = store
                .open_buffered_root(&cx, id, pool.clone(), source_limits)
                .await
                .unwrap();
            // Reclaim admission's unpinned frames before measuring query
            // scratch. The source's resident directory is already charged.
            drop(view.reserve_scan_bytes(&cx, pool.limit()));
            let baseline = pool.used();
            let before = view.stats();
            let hold = pool.reserve(&cx, pool.limit() - baseline - 512).unwrap();
            let held = pool.used();
            let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
            assert_eq!(pool.used(), held, "root heads stay lazy before selection");
            let result = scan
                .select_root_vertices_with(
                    &cx,
                    &[VId(9), VId(10)],
                    BufferedEdgeDirection::Undirected,
                    &mut |_| Ok::<_, ()>(()),
                )
                .await;
            let Err(BufferedScanError::Read(BufferedReadError::Buffer(BufferError::Memory(
                MemoryError::ResourceExhausted { requested, .. },
            )))) = result
            else {
                panic!("routing scratch must refuse before a selected block is read: {result:?}");
            };
            scratch_requests.push(requested);
            assert!(scan.is_closed());
            drop(scan);
            drop(hold);
            assert_eq!(pool.used(), baseline, "refused setup refunds every charge");
            assert_eq!(view.stats(), before, "refused setup never faults payloads");

            let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
            scan.select_root_vertices_with(
                &cx,
                &[VId(9), VId(10)],
                BufferedEdgeDirection::Undirected,
                &mut |_| Ok::<_, ()>(()),
            )
            .await
            .unwrap();
            let after = scan.driver.as_ref().unwrap().root.partition.stats();
            assert_eq!(after.hits + after.misses, before.hits + before.misses + 1);
            let mut admitted = Vec::new();
            let candidate = scan
                .next_with_endpoints(&cx, None, &mut |event| {
                    if let BufferedEdgeScanEvent::Identity(eid) = event {
                        admitted.push(eid);
                    }
                    Ok::<_, ()>(())
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(candidate.eid, EId(7));
            assert_eq!(
                candidate.row.unwrap().edge().props,
                vec![(P, CanonicalPropertyValue::Scalar(CanonicalScalar::Int(70)))],
            );
            assert!(
                scan.next_with_endpoints(&cx, None, &mut |event| {
                    if let BufferedEdgeScanEvent::Identity(eid) = event {
                        admitted.push(eid);
                    }
                    Ok::<_, ()>(())
                })
                .await
                .unwrap()
                .is_none()
            );
            assert_eq!(admitted, [EId(7)], "one selected global root identity");
            drop(scan);
            drop(view);
            assert_eq!(pool.used(), 0);
        }
        assert_eq!(
            scratch_requests[0], scratch_requests[1],
            "query routing scratch is independent of admitted corpus size"
        );
    });
}

#[test]
fn selected_root_routing_memos_and_history_share_terminal_controls_and_work_limits() {
    run(|contexts, path| async move {
        let (store, id) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let vertices = [VId(1), VId(2)];
        let direction = BufferedEdgeDirection::Undirected;
        let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
        let mut total = 0usize;
        scan.select_root_vertices_with(&cx, &vertices, direction, &mut |_| {
            total += 1;
            Ok::<_, usize>(())
        })
        .await
        .unwrap();
        let setup_work = scan.work_used();
        let setup_events = total;
        let mut candidate_at = 0;
        drop(
            scan.next_with_endpoints(&cx, None, &mut |event| {
                total += 1;
                if matches!(event, BufferedEdgeScanEvent::Identity(_)) {
                    candidate_at = total;
                }
                Ok::<_, usize>(())
            })
            .await
            .unwrap(),
        );
        drop(scan);
        let mut stops: Vec<_> = (0..=24).map(|n| 1 + (total - 1) * n / 24).collect();
        stops.extend([
            setup_events,
            setup_events + 1,
            candidate_at,
            candidate_at + 1,
            total,
        ]);
        stops.sort();
        stops.dedup();
        for stop in stops {
            let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
            let mut seen = 0;
            let mut control = |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            };
            let result = scan
                .select_root_vertices_with(&cx, &vertices, direction, &mut control)
                .await;
            let result = match result {
                Ok(()) => scan
                    .next_with_endpoints(&cx, None, &mut control)
                    .await
                    .map(|_| ()),
                Err(error) => Err(error),
            };
            assert!(matches!(result, Err(BufferedScanError::Control(at)) if at == stop));
            assert_eq!(seen, stop);
            assert!(scan.is_closed());
            assert!(
                scan.next_with_endpoints(&cx, None, &mut |_| Err::<(), _>("closed"))
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                scan.next_vertex_candidate(&cx, &mut |_| Err::<(), _>("closed"))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        for allowed in [setup_work - 1, setup_work] {
            let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
            scan.driver.as_mut().unwrap().root.work.limit = allowed;
            let result = scan
                .select_root_vertices_with(&cx, &vertices, direction, &mut |_| Ok::<_, ()>(()))
                .await;
            let result = match result {
                Ok(()) => {
                    assert_eq!(allowed, setup_work);
                    scan.next_with_endpoints(&cx, None, &mut |_| Ok::<_, ()>(()))
                        .await
                        .map(|_| ())
                }
                Err(error) => Err(error),
            };
            assert!(matches!(result,
                Err(BufferedScanError::Read(BufferedReadError::Limit { resource: "buffered source work", requested, limit }))
                    if requested == allowed + 1 && limit == allowed));
            assert_eq!(scan.work_used(), allowed);
            assert!(scan.is_closed());
        }
        for invalid in [[VId(1), VId(1)], [VId(2), VId(1)]] {
            let before = view.stats();
            let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
            assert!(matches!(
                scan.select_root_vertices_with(&cx, &invalid, direction, &mut |_| Ok::<_, ()>(()))
                    .await,
                Err(BufferedScanError::Read(BufferedReadError::Buffer(
                    BufferError::InvalidLoad
                )))
            ));
            assert!(scan.is_closed());
            drop(scan);
            assert_eq!(view.stats(), before);
        }
        drop(view.reserve_scan_bytes(&cx, pool.limit()));
        let baseline = pool.used();
        let hold = pool.reserve(&cx, pool.limit() - baseline - 512).unwrap();
        let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
        let result = scan
            .select_root_vertices_with(&cx, &vertices, direction, &mut |_| Ok::<_, ()>(()))
            .await;
        assert!(matches!(
            result,
            Err(BufferedScanError::Read(BufferedReadError::Buffer(
                BufferError::Memory(MemoryError::ResourceExhausted { .. })
            )))
        ));
        assert!(scan.is_closed());
        drop(scan);
        drop(hold);
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}
