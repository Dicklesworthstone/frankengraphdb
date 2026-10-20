//! Eigenvector scores through retained history and authenticated row cursors.

use super::*;
use fgdb_prism::{AdapterPath, FnxGraphKind, FnxReadError, FnxValue};

#[test]
fn compressed_eigenvector_preserves_history_reductions_and_full_width_ids() {
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
        let call = eigenvector_call();
        assert!(call.supports_sealed_execution());
        assert!(call.supports_cooperative_sealed_execution());
        for at in [original, current] {
            let mut across_reductions = None;
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
                    expected.certificate.witness.algorithm,
                    native.certificate.witness.algorithm
                );
                assert_eq!(
                    expected.certificate.witness.nodes_touched,
                    native.certificate.witness.nodes_touched
                );
                assert_eq!(
                    expected.certificate.witness.edges_scanned,
                    native.certificate.witness.edges_scanned
                );
                assert_eq!(expected.certificate.witness.queue_peak, 0);
                assert_eq!(expected.certificate.adapter, AdapterPath::CompressedCursor);
                assert_eq!(
                    expected.certificate.execution_kernel,
                    "fgdb-prism/sealed-eigenvector-gather-v1"
                );
                assert_eq!(
                    expected.certificate.kernel_workspace_bytes,
                    2 * graph.node_count() * size_of::<f64>()
                );
                assert_eq!(
                    expected.rows.last().unwrap()[1],
                    FnxValue::Vertex(VId(u128::MAX))
                );
                if let Some(previous) = across_reductions {
                    assert_eq!(
                        expected.certificate.result_digest, previous,
                        "the registered eigenvector procedure ignores finite weights"
                    );
                }
                across_reductions = Some(expected.certificate.result_digest);
                let mut previous = None;
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
                        "fgdb-prism/sealed-eigenvector-gather-cooperative-v1"
                    );
                    if q == 1 {
                        assert!(
                            pending > 700,
                            "excluded and retained history must consume fuel"
                        );
                    }
                    if let Some(previous) = &previous {
                        assert_eq!(&actual, previous);
                    }
                    previous = Some(actual);
                }
                let hosted = db
                    .execute_fnx_sealed_cooperative(
                        &cx,
                        &call,
                        opt,
                        memory(),
                        SealedLimits::default(),
                        quantum(7),
                    )
                    .await
                    .unwrap();
                assert_eq!(hosted.analytics, previous.unwrap());
                assert_eq!(
                    db.execute_fnx_sealed(&cx, &call, opt, memory(), SealedLimits::default())
                        .await
                        .unwrap()
                        .analytics,
                    expected
                );
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_eigenvector_admits_exact_workspace_work_and_output_before_iteration() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, true).await;
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        let call = eigenvector_call();
        let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
        let columns = call.outputs().len() * size_of::<String>()
            + call.outputs().iter().map(|column| column.name.len()).sum::<usize>();
        let bytes = columns + graph.node_count()
            * (size_of::<Vec<FnxValue>>() + call.outputs().len() * size_of::<FnxValue>());
        let exact = FnxExecutionLimits {
            max_iterations: 0,
            max_result_rows: graph.node_count(),
            max_estimated_work: expected.certificate.estimated_work,
        };
        let mem = FnxMemoryLimits {
            max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
            max_result_bytes: bytes,
        };
        assert_eq!(call.execute_sealed(&cx, &graph, exact, mem).unwrap(), expected);
        compare_execution(&call.execute_sealed_cooperative(&cx, &graph, exact, mem,
            quantum(1), yield_now).await.unwrap(), &expected);
        for resource in ["estimated work", "kernel workspace bytes", "result rows", "result bytes"] {
            let mut limits = exact;
            let mut memory = mem;
            match resource {
                "estimated work" => limits.max_estimated_work -= 1,
                "kernel workspace bytes" => memory.max_kernel_workspace_bytes -= 1,
                "result rows" => limits.max_result_rows -= 1,
                "result bytes" => memory.max_result_bytes -= 1,
                _ => unreachable!(),
            }
            for cooperative in [false, true] {
                let result = if cooperative {
                    call.execute_sealed_cooperative(&cx, &graph, limits, memory, quantum(1),
                        || -> std::future::Ready<()> { panic!("admission failure precedes yielding") }).await
                } else {
                    call.execute_sealed(&cx, &graph, limits, memory)
                };
                assert!(matches!(result, Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::LimitExceeded { resource: actual, .. })) if actual == resource));
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_eigenvector_keeps_small_population_and_projection_laws() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let call = eigenvector_call();
        let opt = options(Directedness::Undirected);
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
            assert_eq!(expected.rows, native.rows);
            assert_eq!(
                expected.certificate.result_digest,
                native.certificate.result_digest
            );
            assert_eq!(
                expected.certificate.kernel_workspace_bytes,
                n * if n <= 1 { 1 } else { 2 } * size_of::<f64>()
            );
            if n <= 1 {
                assert_eq!(expected.certificate.witness.nodes_touched, n);
                assert_eq!(expected.certificate.witness.edges_scanned, 0);
                if n == 1 {
                    assert_eq!(expected.rows[0][0], FnxValue::Score(1.0));
                }
            }
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
            assert!(
                pending >= n,
                "initialization and scalar-only populations must yield"
            );
        }
        let (db, _) = database(&contexts.commit()).await;
        let kept = projection(&db, &cx, opt).await;
        assert!(matches!(
            call.execute_sealed(&cx, &kept, opt.execution_limits, memory()),
            Err(FnxSealedExecutionError::Execution(
                FnxExecutionError::SelfLoopRefused
            ))
        ));
        assert!(matches!(
            call.execute_sealed_cooperative(
                &cx,
                &kept,
                opt.execution_limits,
                memory(),
                quantum(1),
                || -> std::future::Ready<()> {
                    panic!("self-loop refusal precedes allocation/yield")
                }
            )
            .await,
            Err(FnxSealedExecutionError::Execution(
                FnxExecutionError::SelfLoopRefused
            ))
        ));
        for direction in [Directedness::Directed, Directedness::Reversed] {
            let mut invalid = opt;
            invalid.projection.directedness = direction;
            let error = db
                .execute_fnx_sealed(
                    &cx,
                    &call,
                    invalid,
                    memory(),
                    SealedLimits {
                        max_image_bytes: 0,
                        ..SealedLimits::default()
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                FnxSealedReadError::Execution(FnxSealedExecutionError::Execution(
                    FnxExecutionError::GraphKind {
                        required: FnxGraphKind::Undirected
                    }
                ))
            ));
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_eigenvector_never_publishes_unconverged_long_path_scores() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let n = 129;
        let mut db = small_database(&contexts.commit(), n, false).await;
        let mut batch = WriteBatch::new(RelationId(1));
        for v in 1..n {
            batch.add_edge(
                EId(v as u128),
                VId(v as u128 - 1),
                VId(v as u128),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(-3))],
            );
        }
        db.write(&contexts.commit(), batch).await.unwrap();
        let call = eigenvector_call();
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        let native = db
            .read_session()
            .unwrap()
            .execute_fnx(&cx, &call, opt)
            .unwrap_err();
        let FnxReadError::Execution(FnxExecutionError::NotConverged {
            max_iterations: 100,
            witness: expected,
        }) = native
        else {
            panic!("decoded oracle must reject this slow mode");
        };
        for cooperative in [false, true] {
            let result = if cooperative {
                call.execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(7),
                    yield_now,
                )
                .await
            } else {
                call.execute_sealed(&cx, &graph, opt.execution_limits, memory())
            };
            let Err(FnxSealedExecutionError::Execution(FnxExecutionError::NotConverged {
                max_iterations: 100,
                witness,
            })) = result
            else {
                panic!("compressed execution must reject the same slow mode");
            };
            assert_eq!(witness, expected);
        }
        assert_eq!(expected.nodes_touched, 100 * n);
        assert_eq!(expected.edges_scanned, 100 * 2 * (n - 1));
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
