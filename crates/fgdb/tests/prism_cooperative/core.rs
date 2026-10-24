//! Core peeling through real stored history and compressed directional cursors.

use super::*;
use fgdb_prism::{AdapterPath, FnxGraphKind, FnxValue};

#[test]
fn compressed_core_preserves_history_reductions_full_width_ids_and_hosted_execution() {
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
        let call = core_call();
        assert!(call.supports_sealed_execution());
        assert!(call.supports_cooperative_sealed_execution());
        for at in [original, current] {
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
                let independent = view.execute_fnx(&cx, &call, opt).unwrap();
                assert_eq!(expected.rows, independent.analytics.rows);
                assert_eq!(
                    expected.certificate.result_digest,
                    independent.analytics.certificate.result_digest
                );
                assert_eq!(expected.certificate.adapter, AdapterPath::CompressedCursor);
                assert_eq!(
                    expected.certificate.witness.edges_scanned,
                    graph.adjacency_entry_count()
                );
                assert_eq!(
                    expected.certificate.kernel_workspace_bytes,
                    4 * graph.node_count() * size_of::<usize>()
                );
                for row in &expected.rows {
                    let FnxValue::Vertex(VId(id)) = row[1] else {
                        panic!("stable vertex ID")
                    };
                    let core = if id == 3 {
                        0
                    } else if at == current && id <= 2 {
                        2
                    } else {
                        1
                    };
                    assert_eq!(row[0], FnxValue::Integer(core));
                }
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
                        "fgdb-prism/sealed-core-bucket-cooperative-v1"
                    );
                    if q == 1 {
                        assert!(
                            pending > 700,
                            "excluded/history work must consume source fuel"
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
fn compressed_core_admits_exact_workspace_work_and_output_before_peeling() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, true).await;
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        let call = core_call();
        let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
        let columns = call.outputs().len() * size_of::<String>() + call.outputs().iter().map(|column| column.name.len()).sum::<usize>();
        let bytes = columns + graph.node_count() * (size_of::<Vec<FnxValue>>() + call.outputs().len() * size_of::<FnxValue>());
        let exact = FnxExecutionLimits { max_iterations: 0, max_result_rows: graph.node_count(), max_estimated_work: expected.certificate.estimated_work };
        let mem = FnxMemoryLimits { max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes, max_result_bytes: bytes };
        assert_eq!(call.execute_sealed(&cx, &graph, exact, mem).unwrap(), expected);
        compare_execution(&call.execute_sealed_cooperative(&cx, &graph, exact, mem, quantum(1), yield_now).await.unwrap(), &expected);
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
                    call.execute_sealed_cooperative(&cx, &graph, limits, memory, quantum(1), || -> std::future::Ready<()> { panic!("admission failure precedes yielding") }).await
                } else {
                    call.execute_sealed(&cx, &graph, limits, memory)
                };
                assert!(matches!(result, Err(FnxSealedExecutionError::Execution(FnxExecutionError::LimitExceeded { resource: actual, .. })) if actual == resource));
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_core_handles_empty_isolated_populations_and_refuses_loops_and_directed_inputs() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let call = core_call();
        let opt = options(Directedness::Undirected);
        for n in [0, 1, 512] {
            let db = small_database(&contexts.commit(), n, false).await;
            let graph = projection(&db, &cx, opt).await;
            let expected = call
                .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                .unwrap();
            assert!(
                expected
                    .rows
                    .iter()
                    .all(|row| row[0] == FnxValue::Integer(0))
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
            assert!(
                pending >= n * 7,
                "preparation, peeling, conversion and output all yield"
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
