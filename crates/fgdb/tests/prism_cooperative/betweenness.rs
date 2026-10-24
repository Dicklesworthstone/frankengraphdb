//! Exact Brandes through authenticated historical compressed projections.

use super::*;
use fgdb_prism::{AdapterPath, FnxGraphKind, FnxValue};

fn call() -> FnxCallSpec {
    FnxCallSpec::bind(
        "CALL fnx.betweenness_centrality() YIELD score AS rank,vertex AS id",
        &FnxParameters::new(),
    )
    .unwrap()
}

#[test]
fn compressed_betweenness_preserves_native_bits_history_reductions_and_full_width_ids() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (mut db, original) = database(&contexts.commit()).await;
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(3), vec![LabelId(1)], vec![]);
        batch.add_edge(
            EId(7),
            VId(2),
            VId(0),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(-4))],
        );
        db.write(&contexts.commit(), batch).await.unwrap();
        let current = db.frontier().unwrap();
        let view = db.read_session().unwrap();
        let call = call();
        assert!(call.supports_sealed_execution());
        assert!(call.supports_cooperative_sealed_execution());
        let mut previous_cut = None;
        for at in [original, current] {
            let mut previous_reduction = None;
            for reduction in [
                ParallelEdgePolicy::Sum,
                ParallelEdgePolicy::Minimum,
                ParallelEdgePolicy::Maximum,
                ParallelEdgePolicy::CollapseUnit,
            ] {
                let mut opt = options(Directedness::Undirected);
                opt.as_of = Some(at);
                opt.projection.parallel_edges = reduction;
                opt.projection.self_loops = SelfLoopPolicy::Drop;
                let graph = projection(&db, &cx, opt).await;
                let expected = call
                    .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                    .unwrap();
                let native = view.execute_fnx(&cx, &call, opt).unwrap().analytics;
                assert_eq!(expected.columns, native.columns);
                assert_eq!(expected.rows, native.rows);
                assert_eq!(
                    expected.certificate.result_digest,
                    native.certificate.result_digest
                );
                assert_eq!(
                    expected.certificate.numeric_profile,
                    native.certificate.numeric_profile
                );
                assert_eq!(expected.certificate.adapter, AdapterPath::CompressedCursor);
                assert_eq!(
                    expected.certificate.execution_kernel,
                    "fgdb-prism/sealed-betweenness-brandes-v1"
                );
                assert_eq!(
                    expected.certificate.kernel_workspace_bytes,
                    graph.node_count() * (2 * size_of::<usize>() + 3 * size_of::<f64>())
                );
                assert_eq!(
                    expected.rows.last().unwrap()[1],
                    FnxValue::Vertex(VId(u128::MAX))
                );
                if at == current {
                    assert_eq!(
                        expected
                            .rows
                            .iter()
                            .find(|row| row[1] == FnxValue::Vertex(VId(3)))
                            .unwrap()[0],
                        FnxValue::Score(0.0)
                    );
                }
                if let Some(previous) = previous_reduction {
                    assert_eq!(
                        expected.certificate.result_digest, previous,
                        "unweighted path multiplicities must ignore finite edge weights"
                    );
                }
                previous_reduction = Some(expected.certificate.result_digest);
                let mut previous_quantum = None;
                for q in [1, 7, 257] {
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
                    compare_execution(&actual, &expected);
                    assert_eq!(
                        actual.certificate.execution_kernel,
                        "fgdb-prism/sealed-brandes-source-ordered-cooperative-v1"
                    );
                    if q == 1 {
                        assert!(
                            pending > 700,
                            "both raw-history passes must spend scheduling fuel"
                        );
                    }
                    if let Some(previous) = &previous_quantum {
                        assert_eq!(&actual, previous);
                    }
                    previous_quantum = Some(actual);
                }
                assert_eq!(
                    db.execute_fnx_sealed_cooperative(
                        &cx,
                        &call,
                        opt,
                        memory(),
                        SealedLimits::default(),
                        quantum(7)
                    )
                    .await
                    .unwrap()
                    .analytics,
                    previous_quantum.unwrap()
                );
                assert_eq!(
                    db.execute_fnx_sealed(&cx, &call, opt, memory(), SealedLimits::default())
                        .await
                        .unwrap()
                        .analytics,
                    expected
                );
            }
            if let Some(previous) = previous_cut {
                assert_ne!(previous_reduction, Some(previous));
            }
            previous_cut = previous_reduction;
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_betweenness_admits_exact_limits_and_refuses_incompatible_graphs_before_yield() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, true).await;
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        let call = call();
        let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
        let limits = FnxExecutionLimits { max_iterations: 0, max_result_rows: graph.node_count(),
            max_estimated_work: expected.certificate.estimated_work };
        let mem = FnxMemoryLimits {
            max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
            max_result_bytes: call.outputs().len() * size_of::<String>()
                + call.outputs().iter().map(|column| column.name.len()).sum::<usize>()
                + graph.node_count() * (size_of::<Vec<FnxValue>>() + call.outputs().len() * size_of::<FnxValue>()),
        };
        assert_eq!(call.execute_sealed(&cx, &graph, limits, mem).unwrap(), expected);
        compare_execution(&call.execute_sealed_cooperative(&cx, &graph, limits, mem,
            quantum(1), yield_now).await.unwrap(), &expected);
        for resource in ["estimated work", "kernel workspace bytes", "result rows", "result bytes"] {
            let mut limited = limits;
            let mut memory = mem;
            match resource {
                "estimated work" => limited.max_estimated_work -= 1,
                "kernel workspace bytes" => memory.max_kernel_workspace_bytes -= 1,
                "result rows" => limited.max_result_rows -= 1,
                "result bytes" => memory.max_result_bytes -= 1,
                _ => unreachable!(),
            }
            for cooperative in [false, true] {
                let result = if cooperative {
                    call.execute_sealed_cooperative(&cx, &graph, limited, memory, quantum(1),
                        || -> std::future::Ready<()> { panic!("admission precedes yielding") }).await
                } else { call.execute_sealed(&cx, &graph, limited, memory) };
                assert!(matches!(result, Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::LimitExceeded { resource: actual, .. })) if actual == resource));
            }
        }
        let (looped, _) = database(&contexts.commit()).await;
        let graph = projection(&looped, &cx, opt).await;
        for cooperative in [false, true] {
            let result = if cooperative {
                call.execute_sealed_cooperative(&cx, &graph, opt.execution_limits, memory(), quantum(1),
                    || -> std::future::Ready<()> { panic!("self-loop refusal precedes yielding") }).await
            } else { call.execute_sealed(&cx, &graph, opt.execution_limits, memory()) };
            assert!(matches!(result, Err(FnxSealedExecutionError::Execution(FnxExecutionError::SelfLoopRefused))));
            for direction in [Directedness::Directed, Directedness::Reversed] {
                let mut invalid = opt;
                invalid.projection.directedness = direction;
                let sealing = SealedLimits { max_image_bytes: 0, ..SealedLimits::default() };
                let result = if cooperative {
                    db.execute_fnx_sealed_cooperative(&cx, &call, invalid, memory(), sealing, quantum(1)).await
                } else { db.execute_fnx_sealed(&cx, &call, invalid, memory(), sealing).await };
                assert!(matches!(result, Err(FnxSealedReadError::Execution(FnxSealedExecutionError::Execution(
                    FnxExecutionError::GraphKind { required: FnxGraphKind::Undirected })))));
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_betweenness_empty_and_isolated_populations_keep_linear_actual_work() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let opt = options(Directedness::Undirected);
        let call = call();
        for n in [0, 1, 512] {
            let db = small_database(&contexts.commit(), n, false).await;
            let graph = projection(&db, &cx, opt).await;
            let expected = call
                .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                .unwrap();
            let native = db
                .read_session()
                .unwrap()
                .execute_fnx(&cx, &call, opt)
                .unwrap()
                .analytics;
            assert_eq!(
                expected.certificate.result_digest,
                native.certificate.result_digest
            );
            assert!(
                expected
                    .rows
                    .iter()
                    .all(|row| row[0] == FnxValue::Score(0.0))
            );
            assert_eq!(expected.certificate.witness.nodes_touched, n);
            assert_eq!(expected.certificate.witness.edges_scanned, 0);
            assert!(
                expected.certificate.estimated_work < 128 * n + 32,
                "isolates also need a linear admitted work envelope"
            );
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
            compare_execution(&actual.unwrap(), &expected);
            assert!(pending >= n);
            assert!(
                pending < 16 * n + 32,
                "do not clear the entire population for each isolate"
            );
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_betweenness_cancels_or_drops_at_every_checkpoint_and_suspend() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        let root_before = db.read_session().unwrap().partition_root();
        let call = call();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let (expected, yields) = drive(
            call.execute_sealed_cooperative(
                &controlled,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(7),
                yield_now,
            ),
            &probe,
            7,
        );
        let expected = expected.unwrap();
        for stop in 1..=probe.calls() {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let (actual, _) = drive(
                call.execute_sealed_cooperative(
                    &controlled,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(7),
                    yield_now,
                ),
                &probe,
                7,
            );
            assert!(matches!(actual, Err(FnxSealedExecutionError::Cancelled(_))));
            assert_eq!(probe.calls(), stop);
        }
        for stop in 0..=yields {
            let revoked = Cell::new(false);
            let waker = Waker::from(Arc::new(Wakes::default()));
            let mut task = Context::from_waker(&waker);
            let mut future = Box::pin(call.execute_sealed_cooperative_with_checkpoint(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(7),
                yield_now,
                || {
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
            revoked.set(true);
            let Poll::Ready(result) = future.as_mut().poll(&mut task) else {
                panic!("revocation must precede more source or scalar work");
            };
            live_refusal(result.unwrap_err(), stop);
            drop(future);
            let mut abandoned = Box::pin(call.execute_sealed_cooperative(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(7),
                yield_now,
            ));
            for _ in 0..stop {
                assert!(abandoned.as_mut().poll(&mut task).is_pending());
            }
            drop(abandoned);
            assert_eq!(contexts.outstanding_obligations(), 0);
            assert_eq!(db.read_session().unwrap().partition_root(), root_before);
        }
        assert_eq!(
            call.execute_sealed_cooperative(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(13),
                yield_now
            )
            .await
            .unwrap(),
            expected
        );
    });
}
