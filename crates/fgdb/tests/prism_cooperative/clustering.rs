//! Cooperative topology kernels over real immutable Chronicle/Strata sources.
//! Reuse the parent harness so raw history, actual Pending/wake behavior and
//! certificate parity are checked at the same seam as the other kernels.

use super::*;
use fgdb_prism::AdapterPath;

#[test]
fn topology_calls_preserve_history_reductions_aliases_and_certificates_across_quanta() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (mut db, original) = database(&contexts.commit()).await;
        // Close a triangle only in the new snapshot; retain the existing tail,
        // self-loop, reciprocal/parallel edges and 700 excluded incidences.
        // Signed weights are legal for these unweighted topology procedures.
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
        for at in [original, current] {
            for reduction in [
                ParallelEdgePolicy::Sum,
                ParallelEdgePolicy::Minimum,
                ParallelEdgePolicy::Maximum,
                ParallelEdgePolicy::CollapseUnit,
            ] {
                for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                    let mut opt = options(Directedness::Undirected);
                    opt.as_of = Some(at);
                    opt.projection.parallel_edges = reduction;
                    opt.projection.self_loops = loops;
                    let graph = projection(&db, &cx, opt).await;
                    let decoded = view
                        .prism_projection_at(
                            &cx,
                            at,
                            opt.selection,
                            opt.projection,
                            opt.projection_limits,
                            opt.source_limits,
                        )
                        .unwrap();
                    for text in [
                        "CALL fnx.triangles() YIELD triangles AS total,vertex AS id",
                        "CALL fnx.clustering_coefficient() YIELD score AS coefficient,vertex AS id",
                    ] {
                        let call = FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
                        assert!(call.supports_cooperative_sealed_execution());
                        let expected = call
                            .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                            .unwrap();
                        let independent = call
                            .execute(&decoded, opt.execution_limits, || cx.checkpoint())
                            .unwrap();
                        assert_eq!(expected.rows, independent.rows);
                        assert_eq!(
                            expected.certificate.result_digest,
                            independent.certificate.result_digest
                        );
                        // Make the oracle's nonzero case explicit: a traversal
                        // that accidentally drops the new closing edge must fail.
                        for row in &expected.rows {
                            let fgdb_prism::FnxValue::Vertex(VId(id)) = row[1] else {
                                panic!("native vertex identity");
                            };
                            if matches!(call.algorithm(), FnxAlgorithm::Triangles) {
                                let count = u64::from(at == current && id <= 2);
                                assert_eq!(row[0], fgdb_prism::FnxValue::Integer(count));
                            }
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
                            assert_eq!(actual.certificate.adapter, AdapterPath::CompressedCursor);
                            if q == 1 {
                                assert!(pending > 700, "excluded/history work must yield");
                            }
                            if let Some(previous) = &previous {
                                assert_eq!(&actual, previous);
                            }
                            previous = Some(actual);
                        }
                    }
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn topology_admission_matches_exact_sync_work_workspace_rows_and_result_bytes() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let mut db = small_database(&contexts.commit(), 3, true).await;
        let mut batch = WriteBatch::new(RelationId(1));
        batch.add_edge(
            EId(3),
            VId(2),
            VId(0),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(-1))],
        );
        db.write(&contexts.commit(), batch).await.unwrap();
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        for call in [
            FnxCallSpec::triangles(),
            FnxCallSpec::clustering_coefficient(),
        ] {
            let expected = call
                .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                .unwrap();
            let columns = call.outputs().len() * size_of::<String>()
                + call
                    .outputs()
                    .iter()
                    .map(|column| column.name.len())
                    .sum::<usize>();
            let bytes = columns
                + graph.node_count()
                    * (size_of::<Vec<fgdb_prism::FnxValue>>()
                        + call.outputs().len() * size_of::<fgdb_prism::FnxValue>());
            let exact = FnxExecutionLimits {
                max_iterations: 0,
                max_result_rows: graph.node_count(),
                max_estimated_work: expected.certificate.estimated_work,
            };
            let mem = FnxMemoryLimits {
                max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
                max_result_bytes: bytes,
            };
            let actual = call
                .execute_sealed_cooperative(&cx, &graph, exact, mem, quantum(1), yield_now)
                .await
                .unwrap();
            compare_execution(&actual, &expected);
            for (resource, limits, memory) in [
                (
                    "estimated work",
                    FnxExecutionLimits {
                        max_estimated_work: exact.max_estimated_work - 1,
                        ..exact
                    },
                    mem,
                ),
                (
                    "kernel workspace bytes",
                    exact,
                    FnxMemoryLimits {
                        max_kernel_workspace_bytes: mem.max_kernel_workspace_bytes - 1,
                        ..mem
                    },
                ),
                (
                    "result rows",
                    FnxExecutionLimits {
                        max_result_rows: 2,
                        ..exact
                    },
                    mem,
                ),
                (
                    "result bytes",
                    exact,
                    FnxMemoryLimits {
                        max_result_bytes: bytes - 1,
                        ..mem
                    },
                ),
            ] {
                let error = call
                    .execute_sealed_cooperative(&cx, &graph, limits, memory, quantum(1), yield_now)
                    .await
                    .unwrap_err();
                assert!(matches!(
                    error,
                    FnxSealedExecutionError::Execution(FnxExecutionError::LimitExceeded {
                        resource: actual, ..
                    }) if actual == resource
                ));
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn topology_empty_and_isolated_populations_yield_during_scalar_and_result_walks() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let opt = options(Directedness::Undirected);
        for n in [0, 1, 512] {
            let db = small_database(&contexts.commit(), n, false).await;
            let graph = projection(&db, &cx, opt).await;
            for call in [
                FnxCallSpec::triangles(),
                FnxCallSpec::clustering_coefficient(),
            ] {
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
                assert_eq!(actual.rows.len(), n);
                if n != 0 {
                    assert!(pending >= n * 4, "edge-free scalar walks must cooperate");
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
