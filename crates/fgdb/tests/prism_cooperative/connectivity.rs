//! Connectivity through the real stored-source and cooperative execution APIs.
//! Reachability is an independent partition oracle, not another union-find.

use super::*;
use fgdb_prism::FnxValue;

fn modes() -> Vec<(FnxCallSpec, Directedness)> {
    vec![
        (
            FnxCallSpec::connected_components(),
            Directedness::Undirected,
        ),
        (
            FnxCallSpec::weakly_connected_components(),
            Directedness::Directed,
        ),
        (
            FnxCallSpec::weakly_connected_components(),
            Directedness::Reversed,
        ),
        (
            FnxCallSpec::strongly_connected_components(),
            Directedness::Directed,
        ),
        (
            FnxCallSpec::strongly_connected_components(),
            Directedness::Reversed,
        ),
    ]
}

fn identity(vertex: usize, n: usize) -> VId {
    VId(u128::MAX - (n - 1 - vertex) as u128)
}

async fn stored(cx: &CommitCx, n: usize, edges: &[(usize, usize)]) -> Database<MemVfs> {
    let mut db = Database::<MemVfs>::open_memory(
        cx,
        DatabaseKeys::new(
            [0xa1; 32],
            DatabaseSecurityNamespaceId([0xa2; 32]),
            [0xa3; 32],
        ),
    )
    .await
    .unwrap();
    if n != 0 {
        let mut batch = WriteBatch::new(RelationId(1));
        for vertex in 0..n {
            batch.create_vertex(identity(vertex, n), vec![LabelId(1)], vec![]);
        }
        for (eid, &(source, target)) in edges.iter().enumerate() {
            batch.add_edge(
                EId(eid as u128),
                identity(source, n),
                identity(target, n),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
            );
        }
        db.write(cx, batch).await.unwrap();
    }
    db
}

fn reference(n: usize, edges: &[(usize, usize)], strong: bool) -> Vec<Vec<FnxValue>> {
    let mut reach = vec![vec![false; n]; n];
    for (i, row) in reach.iter_mut().enumerate() {
        row[i] = true;
    }
    for &(s, t) in edges {
        reach[s][t] = true;
        if !strong {
            reach[t][s] = true;
        }
    }
    for via in 0..n {
        for s in 0..n {
            for t in 0..n {
                let connected = reach[s][t] || (reach[s][via] && reach[via][t]);
                reach[s][t] = connected;
            }
        }
    }
    (0..n)
        .map(|vertex| {
            let label = (0..n)
                .find(|&other| reach[vertex][other] && reach[other][vertex])
                .unwrap();
            vec![
                FnxValue::Vertex(identity(vertex, n)),
                FnxValue::Vertex(identity(label, n)),
            ]
        })
        .collect()
}

#[test]
fn every_three_vertex_topology_has_identical_canonical_components_at_every_quantum() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        for mask in 0u16..512 {
            let edges: Vec<_> = (0..9)
                .filter(|bit| mask & (1 << bit) != 0)
                .map(|bit| (bit / 3, bit % 3))
                .collect();
            let db = stored(&contexts.commit(), 3, &edges).await;
            for (call, direction) in modes() {
                let opt = options(direction);
                let graph = projection(&db, &cx, opt).await;
                let strong = matches!(call.algorithm(), FnxAlgorithm::StronglyConnectedComponents);
                let expected = reference(3, &edges, strong);
                let sync = call
                    .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                    .unwrap();
                assert_eq!(sync.rows, expected);
                let mut certificate = None;
                for q in [1, 2, 7] {
                    let probe = Arc::new(SimulationCheckpointProbe::new(None));
                    let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                    let (actual, pending) = drive(
                        call.execute_sealed_cooperative(
                            &controlled,
                            &graph,
                            opt.execution_limits,
                            memory(),
                            quantum(q),
                            yield_now,
                        ),
                        &probe,
                        q,
                    );
                    let actual = actual.unwrap();
                    compare_execution(&actual, &sync);
                    assert_eq!(
                        actual.rows, expected,
                        "mask={mask}, direction={direction:?}, q={q}"
                    );
                    if q == 1 {
                        assert!(pending >= 3 * 2);
                    }
                    if let Some(previous) = &certificate {
                        assert_eq!(&actual.certificate, previous);
                    }
                    certificate = Some(actual.certificate);
                }
            }
        }
    });
}

#[test]
fn canonical_minima_are_not_union_roots_and_scalar_only_populations_still_yield() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        // Union-by-size selects representative 1, NOT the required minimum 0.
        let edges = [(0, 3), (1, 2), (1, 4), (1, 3)];
        let db = stored(&contexts.commit(), 6, &edges).await;
        for (call, direction) in modes() {
            let opt = options(direction);
            let graph = projection(&db, &cx, opt).await;
            let result = call
                .execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(1),
                    yield_now,
                )
                .await
                .unwrap();
            assert_eq!(
                result.rows,
                reference(
                    6,
                    &edges,
                    matches!(call.algorithm(), FnxAlgorithm::StronglyConnectedComponents)
                )
            );
        }
        for n in [0, 512] {
            let db = stored(&contexts.commit(), n, &[]).await;
            for (call, direction) in modes() {
                let opt = options(direction);
                let graph = projection(&db, &cx, opt).await;
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let (result, pending) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(1),
                        yield_now,
                    ),
                    &probe,
                    1,
                );
                let result = result.unwrap();
                assert_eq!(result.rows.len(), n);
                for (vertex, row) in result.rows.iter().enumerate() {
                    assert_eq!(row, &vec![FnxValue::Vertex(identity(vertex, n)); 2]);
                }
                assert!(
                    pending >= n * 5,
                    "no adjacency can supply these scheduling boundaries"
                );
            }
        }
    });
}

#[test]
fn component_admission_is_exact_and_graph_kind_refusal_precedes_source_preparation() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = stored(&contexts.commit(), 3, &[(0, 1)]).await;
        for (call, direction) in modes() {
            let opt = options(direction);
            let graph = projection(&db, &cx, opt).await;
            let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
            let bytes = call.outputs().len() * size_of::<String>()
                + call.outputs().iter().map(|output| output.name.len()).sum::<usize>()
                + 3 * (size_of::<Vec<FnxValue>>() + call.outputs().len() * size_of::<FnxValue>());
            let limits = FnxExecutionLimits { max_iterations: 0, max_result_rows: 3,
                max_estimated_work: expected.certificate.estimated_work };
            let mem = FnxMemoryLimits { max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
                max_result_bytes: bytes };
            let actual = call.execute_sealed_cooperative(&cx, &graph, limits, mem, quantum(1), yield_now).await.unwrap();
            compare_execution(&actual, &expected);
            for (resource, limits, memory) in [
                ("result rows", FnxExecutionLimits { max_result_rows: 2, ..limits }, mem),
                ("result bytes", limits, FnxMemoryLimits { max_result_bytes: bytes - 1, ..mem }),
                ("estimated work", FnxExecutionLimits { max_estimated_work: limits.max_estimated_work - 1, ..limits }, mem),
                ("kernel workspace bytes", limits, FnxMemoryLimits { max_kernel_workspace_bytes: mem.max_kernel_workspace_bytes - 1, ..mem }),
            ] {
                let error = call.execute_sealed_cooperative(&cx, &graph, limits, memory, quantum(1),
                    || -> std::future::Ready<()> { panic!("rejected admission must not enter the kernel") })
                    .await.unwrap_err();
                assert!(matches!(error, FnxSealedExecutionError::Execution(
                    FnxExecutionError::LimitExceeded { resource: actual, .. }) if actual == resource));
            }
            let wrong = if direction == Directedness::Undirected {
                Directedness::Directed
            } else { Directedness::Undirected };
            let mut denied = options(wrong);
            denied.source_limits.max_work_units = 0;
            let result = db.execute_fnx_sealed_cooperative(&cx, &call, denied, memory(),
                SealedLimits { max_image_bytes: 0, ..SealedLimits::default() }, quantum(1)).await;
            assert!(matches!(result, Err(FnxSealedReadError::Execution(
                FnxSealedExecutionError::Execution(FnxExecutionError::GraphKind { .. })))));
        }
    });
}

#[test]
fn every_component_checkpoint_and_suspension_retains_live_refusals_and_drop_safety() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = stored(
            &contexts.commit(),
            5,
            &[(0, 3), (1, 2), (1, 4), (1, 3), (3, 1)],
        )
        .await;
        let anchor = db.read_session().unwrap().partition_root();
        for (call, direction) in modes() {
            let opt = options(direction);
            let graph = projection(&db, &cx, opt).await;
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let measured = cx.with_checkpoint_probe(Arc::clone(&probe));
            let mut guards = 0usize;
            let (expected, pending) = drive(
                call.execute_sealed_cooperative_with_checkpoint(
                    &measured,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(3),
                    yield_now,
                    || {
                        guards += 1;
                        Ok(())
                    },
                ),
                &probe,
                3,
            );
            let expected = expected.unwrap();
            let calls = probe.calls();
            for stop in 1..=calls {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let (actual, _) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(3),
                        yield_now,
                    ),
                    &probe,
                    3,
                );
                assert!(matches!(actual, Err(FnxSealedExecutionError::Cancelled(_))));
                assert_eq!(probe.calls(), stop);
            }
            for stop in 1..=guards {
                let mut seen = 0;
                let error = call
                    .execute_sealed_cooperative_with_checkpoint(
                        &cx,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(3),
                        yield_now,
                        || {
                            seen += 1;
                            if seen == stop {
                                Err(SealedProjectionError::Guard(Box::new(LiveRefusal(stop))))
                            } else {
                                Ok(())
                            }
                        },
                    )
                    .await
                    .unwrap_err();
                live_refusal(error, stop);
                assert_eq!(seen, stop);
            }
            let waker = Waker::from(Arc::new(Wakes::default()));
            let mut task = Context::from_waker(&waker);
            for stop in 0..=pending {
                let revoked = Cell::new(false);
                let seen = Cell::new(0usize);
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let mut future = Box::pin(call.execute_sealed_cooperative_with_checkpoint(
                    &controlled,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(3),
                    yield_now,
                    || {
                        seen.set(seen.get() + 1);
                        if revoked.get() {
                            Err(SealedProjectionError::Guard(Box::new(LiveRefusal(stop))))
                        } else {
                            Ok(())
                        }
                    },
                ));
                for _ in 0..stop {
                    assert!(future.as_mut().poll(&mut task).is_pending());
                }
                let before = probe.calls();
                let before_guard = seen.get();
                revoked.set(true);
                let Poll::Ready(result) = future.as_mut().poll(&mut task) else {
                    panic!("revocation must be observed before another scan or scalar step");
                };
                live_refusal(result.unwrap_err(), stop);
                assert_eq!(probe.calls(), before + 1);
                assert_eq!(seen.get(), before_guard + 1);
                drop(future);
                let mut abandoned = Box::pin(call.execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(3),
                    yield_now,
                ));
                for _ in 0..stop {
                    assert!(abandoned.as_mut().poll(&mut task).is_pending());
                }
                drop(abandoned);
                assert_eq!(contexts.outstanding_obligations(), 0);
                assert_eq!(db.read_session().unwrap().partition_root(), anchor);
            }
            assert_eq!(
                call.execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(7),
                    yield_now
                )
                .await
                .unwrap(),
                expected
            );
        }
    });
}

#[test]
fn historical_hosted_connectivity_keeps_hidden_endpoints_and_aliases_out_of_the_kernel() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, at) = database(&contexts.commit()).await;
        for (call, direction) in modes() {
            let name = match call.algorithm() {
                FnxAlgorithm::ConnectedComponents => "connected_components",
                FnxAlgorithm::WeaklyConnectedComponents => "weakly_connected_components",
                FnxAlgorithm::StronglyConnectedComponents => "strongly_connected_components",
                _ => unreachable!(),
            };
            let text = format!("CALL fnx.{name}() YIELD component AS group_id,vertex AS id");
            let call = FnxCallSpec::bind(&text, &FnxParameters::new()).unwrap();
            for at in [at, db.frontier().unwrap()] {
                let mut opt = options(direction);
                opt.as_of = Some(at);
                let graph = projection(&db, &cx, opt).await;
                let prepared = call
                    .execute_sealed_cooperative(
                        &cx,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(1),
                        yield_now,
                    )
                    .await
                    .unwrap();
                let hosted = db
                    .call_fnx_sealed_cooperative(
                        &cx,
                        &text,
                        &FnxParameters::new(),
                        opt,
                        memory(),
                        SealedLimits::default(),
                        quantum(7),
                    )
                    .await
                    .unwrap();
                assert_eq!(hosted.analytics, prepared);
                assert_eq!(hosted.selection, opt.selection);
                assert_eq!(prepared.rows.len(), 4);
                assert_eq!(prepared.columns, vec!["group_id", "id"]);
                for row in &prepared.rows {
                    assert!(!row.contains(&FnxValue::Vertex(VId(99))));
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn deep_dfs_and_whole_graph_scc_labeling_preserve_frames_across_actual_pending_polls() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let n = 512;
        for cycle in [false, true] {
            let mut edges: Vec<_> = (0..n - 1).map(|i| (i, i + 1)).collect();
            if cycle {
                edges.push((n - 1, 0));
            }
            let db = stored(&contexts.commit(), n, &edges).await;
            let call = FnxCallSpec::strongly_connected_components();
            for direction in [Directedness::Directed, Directedness::Reversed] {
                let opt = options(direction);
                let graph = projection(&db, &cx, opt).await;
                let expected = call
                    .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                    .unwrap();
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let (actual, pending) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(1),
                        yield_now,
                    ),
                    &probe,
                    1,
                );
                let actual = actual.unwrap();
                compare_execution(&actual, &expected);
                assert!(pending >= 5 * n, "DFS and final-label scans must cooperate");
                assert_eq!(actual.certificate.witness.nodes_touched, n);
                assert_eq!(actual.certificate.witness.edges_scanned, edges.len());
                if cycle || direction == Directedness::Directed {
                    assert_eq!(actual.certificate.witness.queue_peak, n);
                }
                for (vertex, row) in actual.rows.iter().enumerate() {
                    assert_eq!(
                        row,
                        &vec![
                            FnxValue::Vertex(identity(vertex, n)),
                            FnxValue::Vertex(identity(if cycle { 0 } else { vertex }, n))
                        ]
                    );
                }
            }
        }
        // One-way links between completed components must not merge them.
        let edges = [(0, 1), (1, 0), (1, 2), (2, 3), (3, 2), (3, 4), (5, 4)];
        let db = stored(&contexts.commit(), 6, &edges).await;
        let opt = options(Directedness::Directed);
        let graph = projection(&db, &cx, opt).await;
        drop(db);
        let result = FnxCallSpec::strongly_connected_components()
            .execute_sealed_cooperative(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(1),
                yield_now,
            )
            .await
            .unwrap();
        assert_eq!(result.rows, reference(6, &edges, true));
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
