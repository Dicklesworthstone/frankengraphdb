//! HITS through real authenticated history and each compressed direction.

use super::*;

fn call() -> FnxCallSpec {
    FnxCallSpec::bind(
        "CALL fnx.hits_centrality() YIELD authority AS rank,node AS id,hub",
        &FnxParameters::new(),
    )
    .unwrap()
}

#[test]
fn compressed_hits_preserves_history_masks_reductions_and_score_bits() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, original) = database(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let call = call();
        for at in [original, db.frontier().unwrap()] {
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let mut previous_reduction = None;
                for reduction in [
                    ParallelEdgePolicy::Sum,
                    ParallelEdgePolicy::Minimum,
                    ParallelEdgePolicy::Maximum,
                    ParallelEdgePolicy::CollapseUnit,
                ] {
                    let mut opt = options(direction);
                    opt.as_of = Some(at);
                    opt.projection.parallel_edges = reduction;
                    let graph = projection(&db, &cx, opt).await;
                    let expected = call
                        .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                        .unwrap();
                    let native = view.execute_fnx(&cx, &call, opt).unwrap().analytics;
                    assert_eq!(expected.columns, ["rank", "id", "hub"]);
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
                    assert_eq!(
                        expected.certificate.kernel_workspace_bytes,
                        3 * graph.node_count() * size_of::<f64>()
                    );
                    assert_eq!(
                        expected.rows.last().unwrap()[1],
                        FnxValue::Vertex(VId(u128::MAX))
                    );
                    assert!(
                        expected
                            .rows
                            .iter()
                            .all(|row| row[1] != FnxValue::Vertex(VId(99)))
                    );
                    if let Some(previous) = previous_reduction {
                        assert_eq!(
                            expected.certificate.result_digest, previous,
                            "HITS ignores projected weights"
                        );
                    }
                    previous_reduction = Some(expected.certificate.result_digest);
                    let probe = Arc::new(SimulationCheckpointProbe::new(None));
                    let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                    let (actual, pending) = drive(
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
                    let actual = actual.unwrap();
                    compare_execution(&actual, &expected);
                    assert!(pending > 0, "raw history and scalar phases must yield");
                    assert_eq!(
                        actual.certificate.execution_kernel,
                        "fgdb-prism/sealed-hits-source-order-cooperative-v1"
                    );
                    if reduction == ParallelEdgePolicy::CollapseUnit {
                        for q in [1, 257] {
                            let probe = Arc::new(SimulationCheckpointProbe::new(None));
                            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                            let (resumed, _) = drive(
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
                            assert_eq!(resumed.unwrap(), actual);
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
                        assert_eq!(hosted.analytics, actual);
                    }
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_hits_admits_iterations_workspace_work_and_both_output_columns() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let call = call();
        for n in [0, 1, 3, 129] {
            let db = small_database(&contexts.commit(), n, true).await;
            let opt = options(Directedness::Directed);
            let graph = projection(&db, &cx, opt).await;
            let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
            let native = db.read_session().unwrap().execute_fnx(&cx, &call, opt).unwrap().analytics;
            assert_eq!(expected.rows, native.rows);
            assert_eq!(expected.certificate.result_digest, native.certificate.result_digest);
            let columns = call.outputs().len() * size_of::<String>()
                + call.outputs().iter().map(|column| column.name.len()).sum::<usize>();
            let bytes = columns + n * (size_of::<Vec<FnxValue>>() + call.outputs().len() * size_of::<FnxValue>());
            let limits = FnxExecutionLimits {
                max_iterations: 100,
                max_result_rows: n,
                max_estimated_work: expected.certificate.estimated_work,
            };
            let memory = FnxMemoryLimits {
                max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
                max_result_bytes: bytes,
            };
            assert_eq!(memory.max_kernel_workspace_bytes, n * if n <= 1 { 2 } else { 3 } * size_of::<f64>());
            assert_eq!(call.execute_sealed(&cx, &graph, limits, memory).unwrap(), expected);
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let (actual, pending) = drive(
                call.execute_sealed_cooperative(&controlled, &graph, limits, memory, quantum(1), yield_now),
                &probe,
                1,
            );
            compare_execution(&actual.unwrap(), &expected);
            assert!(pending >= n, "isolates still yield during scalar passes");
            if n != 3 {
                for row in &expected.rows {
                    assert_eq!(row[0], FnxValue::Score(1.0 / n as f64));
                    assert_eq!(row[2], FnxValue::Score(1.0 / n as f64));
                }
            }
            for resource in ["iterations", "estimated work", "result bytes", "kernel workspace bytes", "result rows"] {
                let mut refused = limits;
                let mut denied = memory;
                match resource {
                    "iterations" => refused.max_iterations -= 1,
                    "estimated work" => refused.max_estimated_work -= 1,
                    "result bytes" => denied.max_result_bytes -= 1,
                    "kernel workspace bytes" if n > 0 => denied.max_kernel_workspace_bytes -= 1,
                    "result rows" if n > 0 => refused.max_result_rows -= 1,
                    _ => continue,
                }
                for cooperative in [false, true] {
                    let result = if cooperative {
                        call.execute_sealed_cooperative(&cx, &graph, refused, denied, quantum(1),
                            || -> std::future::Ready<()> { panic!("admission refusal precedes yielding"); }).await
                    } else {
                        call.execute_sealed(&cx, &graph, refused, denied)
                    };
                    assert!(matches!(result, Err(FnxSealedExecutionError::Execution(FnxExecutionError::LimitExceeded { resource: actual, .. })) if actual == resource));
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn cooperative_hits_checks_live_guard_before_every_resume_and_final_delivery() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let opt = options(Directedness::Directed);
        let graph = projection(&db, &cx, opt).await;
        let call = call();
        let calls = Cell::new(0);
        let expected = call
            .execute_sealed_cooperative_with_checkpoint(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(3),
                yield_now,
                || {
                    calls.set(calls.get() + 1);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert!(calls.get() > 100);
        for stop in 1..=calls.get() {
            let seen = Cell::new(0);
            let stopped = call
                .execute_sealed_cooperative_rows_with_checkpoint(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(3),
                    yield_now,
                    || {
                        seen.set(seen.get() + 1);
                        if seen.get() == stop {
                            Err(SealedProjectionError::Guard(Box::new(LiveRefusal(stop))))
                        } else {
                            Ok(())
                        }
                    },
                )
                .await
                .unwrap_err();
            live_refusal(stopped, stop);
            assert_eq!(seen.get(), stop, "no further control after refusal");
        }
        let cursor = call
            .execute_sealed_cooperative_rows(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(3),
                yield_now,
            )
            .await
            .unwrap();
        compare_retained(cursor, &expected, graph.node_count());
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
