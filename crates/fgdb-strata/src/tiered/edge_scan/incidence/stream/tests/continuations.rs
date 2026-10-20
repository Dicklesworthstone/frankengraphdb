//! Successor continuations against the shared authenticated storage fixture.

use super::*;

// These exercise the successor API used by joins, not the standalone stream.
#[test]
fn retained_join_requests_preserve_restarts_seeks_and_literal_history() {
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
            let mut scan = view
                .edge_scan(&cx, CommitSeq(cut))
                .unwrap()
                .into_join_scan();
            for endpoint in [VId(1), VId(2), VId(3), VId(99)] {
                for relation in [None, Some(R), Some(S)] {
                    for direction in [
                        BufferedEdgeDirection::Outgoing,
                        BufferedEdgeDirection::Incoming,
                        BufferedEdgeDirection::Undirected,
                    ] {
                        // Repeat, continue, skip ahead, move backwards, restart,
                        // and repeat EOF. None is never a numeric sentinel.
                        for after in [
                            None,
                            None,
                            Some(EId(0)),
                            Some(EId(4)),
                            Some(EId(0)),
                            None,
                            Some(EId(u128::MAX)),
                            Some(EId(u128::MAX)),
                        ] {
                            let wanted = expected(endpoint, relation, direction, after)
                                .first()
                                .copied();
                            let mut observed = Vec::new();
                            let got = scan
                                .next_incident_retained_with_endpoints(
                                    &cx,
                                    endpoint,
                                    relation,
                                    direction,
                                    after,
                                    &mut |event| {
                                        if let BufferedEdgeScanEvent::Identity(eid) = event {
                                            observed.push(eid);
                                        }
                                        Ok::<_, ()>(())
                                    },
                                )
                                .await
                                .unwrap();
                            assert_eq!(got.as_ref().map(|row| row.eid), wanted);
                            assert_eq!(observed, wanted.into_iter().collect::<Vec<_>>());
                            if let Some(candidate) = got {
                                assert_eq!(
                                    candidate.row.as_ref().map(|row| row.edge().props.clone()),
                                    visible(candidate.eid, cut)
                                        .map(|value| vec![(P, CanonicalScalar::Int(value).into())])
                                );
                                if let Some(row) = candidate.row {
                                    assert_eq!(row.source_vertex().vid, row.edge().entry.src);
                                    assert_eq!(row.target_vertex().vid, row.edge().entry.dst);
                                }
                            }
                            assert!(!scan.is_closed());
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 1152);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn retained_join_continuations_do_not_advance_roots_or_retain_eof_workspaces() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
        let first = scan
            .next_incident_retained_with_endpoints(
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
        assert_eq!(first.eid, EId(0));
        assert!(
            first.row.is_none(),
            "an invisible identity still advances the position"
        );
        let used = scan.work_used();
        drop(scan.next_incident_retained_with_endpoints(
            &cx,
            VId(1),
            Some(R),
            BufferedEdgeDirection::Outgoing,
            Some(EId(0)),
            &mut |_| Ok::<_, ()>(()),
        ));
        assert_eq!(scan.work_used(), used, "unpolled pulls change nothing");
        assert!(scan.continuation.as_ref().unwrap().state.is_some());
        let root_first = scan
            .next_with_endpoints(&cx, None, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(root_first.eid, EId(0));
        assert!(
            scan.continuation.is_none(),
            "root advance releases its old child"
        );
        drop(root_first);
        let next = scan
            .next_incident_retained_with_endpoints(
                &cx,
                VId(1),
                Some(R),
                BufferedEdgeDirection::Outgoing,
                Some(EId(0)),
                &mut |_| Ok::<_, ()>(()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.eid, EId(4));
        drop(next);
        let last = scan
            .next_incident_retained_with_endpoints(
                &cx,
                VId(1),
                Some(R),
                BufferedEdgeDirection::Outgoing,
                Some(EId(4)),
                &mut |_| Ok::<_, ()>(()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(last.eid, EId(u128::MAX));
        assert!(
            scan.continuation.as_ref().unwrap().state.is_none(),
            "no heap/block at EOF"
        );
        assert!(
            scan.next_incident_retained_with_endpoints(
                &cx,
                VId(1),
                Some(R),
                BufferedEdgeDirection::Outgoing,
                Some(EId(u128::MAX)),
                &mut |_| Ok::<_, ()>(())
            )
            .await
            .unwrap()
            .is_none()
        );
        let mut remaining_roots = Vec::new();
        while let Some(candidate) = scan
            .next_with_endpoints(&cx, None, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
        {
            remaining_roots.push(candidate.eid);
        }
        assert_eq!(remaining_roots, [EId(4), EId(6), EId(8), EId(u128::MAX)]);
        // Root exhaustion is not closure: the final root can still expand.
        let restarted = scan
            .next_incident_retained_with_endpoints(
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
        assert_eq!(restarted.eid, EId(0));
        let independent = scan
            .next_incident_with_endpoints(
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
        assert_eq!(independent.eid, EId(6));
        assert!(scan.continuation.is_none());
        drop(independent);
        drop(restarted);
        scan.close();
        assert!(
            scan.next_incident_retained_with_endpoints(
                &cx,
                VId(1),
                None,
                BufferedEdgeDirection::Undirected,
                None,
                &mut |_| Err::<(), _>("closed")
            )
            .await
            .unwrap()
            .is_none()
        );
        drop(scan);
        drop(view);
        assert!(
            pool.used() > 0,
            "delivered endpoints keep independent reservations"
        );
        drop(last);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn retained_join_successors_compare_source_work_against_the_live_independent_reader() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let mut results = Vec::new();
        for retained in [false, true] {
            let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
            let mut after = None;
            let mut rows = Vec::new();
            loop {
                let next = if retained {
                    scan.next_incident_retained_with_endpoints(
                        &cx,
                        VId(1),
                        None,
                        BufferedEdgeDirection::Undirected,
                        after,
                        &mut |_| Ok::<_, ()>(()),
                    )
                    .await
                } else {
                    scan.next_incident_with_endpoints(
                        &cx,
                        VId(1),
                        None,
                        BufferedEdgeDirection::Undirected,
                        after,
                        &mut |_| Ok::<_, ()>(()),
                    )
                    .await
                }
                .unwrap();
                let Some(candidate) = next else { break };
                after = Some(candidate.eid);
                rows.push((
                    candidate.eid,
                    candidate.row.as_ref().map(|row| row.edge().props.clone()),
                ));
            }
            results.push((rows, scan.work_used()));
        }
        assert_eq!(results[0].0, results[1].0);
        assert_eq!(
            results[1].0.iter().map(|row| row.0).collect::<Vec<_>>(),
            expected(VId(1), None, BufferedEdgeDirection::Undirected, None)
        );
        assert!(
            results[1].1 < results[0].1,
            "same-source work, not wall-time: {results:?}"
        );
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn retained_join_every_continuation_checkpoint_and_exact_work_ceiling_are_terminal() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
        drop(
            scan.next_incident_retained_with_endpoints(
                &cx,
                VId(1),
                Some(R),
                BufferedEdgeDirection::Outgoing,
                None,
                &mut |_| Ok::<_, ()>(()),
            )
            .await
            .unwrap(),
        );
        let mut total = 0;
        drop(
            scan.next_incident_retained_with_endpoints(
                &cx,
                VId(1),
                Some(R),
                BufferedEdgeDirection::Outgoing,
                Some(EId(0)),
                &mut |_| {
                    total += 1;
                    Ok::<_, usize>(())
                },
            )
            .await
            .unwrap(),
        );
        let exact = scan.work_used();
        drop(scan);
        assert!(total > 1);
        for stop in 1..=total {
            let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
            drop(
                scan.next_incident_retained_with_endpoints(
                    &cx,
                    VId(1),
                    Some(R),
                    BufferedEdgeDirection::Outgoing,
                    None,
                    &mut |_| Ok::<_, ()>(()),
                )
                .await
                .unwrap(),
            );
            let mut seen = 0;
            let result = scan
                .next_incident_retained_with_endpoints(
                    &cx,
                    VId(1),
                    Some(R),
                    BufferedEdgeDirection::Outgoing,
                    Some(EId(0)),
                    &mut |_| {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    },
                )
                .await;
            assert!(matches!(result, Err(BufferedScanError::Control(at)) if at == stop));
            assert_eq!(seen, stop);
            assert!(scan.is_closed());
            assert!(scan.continuation.is_none());
            assert!(
                scan.next_incident_retained_with_endpoints(
                    &cx,
                    VId(1),
                    None,
                    BufferedEdgeDirection::Undirected,
                    None,
                    &mut |_| Err::<(), _>("failed")
                )
                .await
                .unwrap()
                .is_none()
            );
        }
        for limit in [exact, exact - 1] {
            let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
            drop(
                scan.next_incident_retained_with_endpoints(
                    &cx,
                    VId(1),
                    Some(R),
                    BufferedEdgeDirection::Outgoing,
                    None,
                    &mut |_| Ok::<_, ()>(()),
                )
                .await
                .unwrap(),
            );
            scan.driver.as_mut().unwrap().root.work.limit = limit;
            let result = scan
                .next_incident_retained_with_endpoints(
                    &cx,
                    VId(1),
                    Some(R),
                    BufferedEdgeDirection::Outgoing,
                    Some(EId(0)),
                    &mut |_| Ok::<_, ()>(()),
                )
                .await;
            if limit == exact {
                assert_eq!(result.unwrap().unwrap().eid, EId(4));
            } else {
                assert!(matches!(
                    result,
                    Err(BufferedScanError::Read(BufferedReadError::Limit { .. }))
                ));
                assert!(scan.is_closed());
            }
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn retained_join_cached_eof_still_checks_control_and_can_restart_before_failure() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let mut scan = view.edge_scan(&cx, CommitSeq(3)).unwrap().into_join_scan();
        for _ in 0..2 {
            assert!(
                scan.next_incident_retained_with_endpoints(
                    &cx,
                    VId(1),
                    Some(R),
                    BufferedEdgeDirection::Outgoing,
                    Some(EId(u128::MAX)),
                    &mut |_| Ok::<_, ()>(())
                )
                .await
                .unwrap()
                .is_none()
            );
            assert!(scan.continuation.as_ref().unwrap().state.is_none());
        }
        let row = scan
            .next_incident_retained_with_endpoints(
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
        assert_eq!(row.eid, EId(0));
        drop(row);
        assert!(
            scan.next_incident_retained_with_endpoints(
                &cx,
                VId(1),
                Some(R),
                BufferedEdgeDirection::Outgoing,
                Some(EId(u128::MAX)),
                &mut |_| Ok::<_, ()>(())
            )
            .await
            .unwrap()
            .is_none()
        );
        let mut calls = 0;
        let failure = scan
            .next_incident_retained_with_endpoints(
                &cx,
                VId(1),
                Some(R),
                BufferedEdgeDirection::Outgoing,
                Some(EId(u128::MAX)),
                &mut |_| {
                    calls += 1;
                    Err::<(), _>("stop at cached EOF")
                },
            )
            .await;
        assert!(matches!(
            failure,
            Err(BufferedScanError::Control("stop at cached EOF"))
        ));
        assert_eq!(calls, 1);
        assert!(scan.is_closed());
        assert!(scan.continuation.is_none());
        drop(scan);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}
