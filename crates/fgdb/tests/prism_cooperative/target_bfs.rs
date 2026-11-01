//! Exact target hop distances over authenticated history and cooperative pulls.

use super::*;

#[test]
fn target_hops_preserve_history_masks_directions_and_all_source_bfs_answers() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, original) = database(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        for at in [original, db.frontier().unwrap()] {
            for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
                for reduction in [ParallelEdgePolicy::Sum, ParallelEdgePolicy::Minimum, ParallelEdgePolicy::Maximum, ParallelEdgePolicy::CollapseUnit] {
                    let mut opt = options(direction);
                    opt.as_of = Some(at);
                    opt.projection.parallel_edges = reduction;
                    let graph = projection(&db, &cx, opt).await;
                    let decoded = view.prism_projection_at(&cx, at, opt.selection, opt.projection,
                        opt.projection_limits, opt.source_limits).unwrap();
                    for (source, target, cutoff) in [
                        (VId(0), VId(u128::MAX), None),
                        (VId(u128::MAX), VId(0), None),
                        (VId(1), VId(2), Some(1)),
                        (VId(0), VId(u128::MAX), Some(2)),
                        (VId(u128::MAX), VId(u128::MAX), Some(0)),
                        (VId(2), VId(0), None),
                    ] {
                        let call = FnxCallSpec::shortest_path_length(source, target, cutoff);
                        let full = FnxCallSpec::single_source_shortest_path_length(source, cutoff)
                            .execute(&decoded, opt.execution_limits, || cx.checkpoint()).unwrap();
                        let filtered: Vec<_> = full.rows.into_iter()
                            .filter(|row| row[0] == FnxValue::Vertex(target)).collect();
                        let limits = FnxExecutionLimits { max_result_rows: filtered.len(), ..opt.execution_limits };
                        let expected = call.execute_sealed(&cx, &graph, limits, memory()).unwrap();
                        let native = call.execute(&decoded, limits, || cx.checkpoint()).unwrap();
                        assert_eq!(expected.rows, filtered);
                        assert_eq!(expected.rows, native.rows);
                        assert_eq!(expected.certificate.result_digest, native.certificate.result_digest);
                        assert_eq!(expected.certificate.witness.nodes_touched, native.certificate.witness.nodes_touched);
                        assert_eq!(expected.certificate.witness.edges_scanned, native.certificate.witness.edges_scanned);
                        for q in [1, 7, 257] {
                            let probe = Arc::new(SimulationCheckpointProbe::new(None));
                            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                            let (actual, _) = drive(call.execute_sealed_cooperative(
                                &controlled, &graph, limits, memory(), quantum(q), yield_now,
                            ), &probe, q);
                            let actual = actual.unwrap();
                            compare_execution(&actual, &expected);
                            assert_eq!(actual.certificate.execution_kernel,
                                "fgdb-prism/sealed-target-bfs-bitmap-fifo-cooperative-v1");
                            if q == 7 {
                                let mut cursor = call.execute_sealed_cooperative_rows(
                                    &cx, &graph, limits, memory(), quantum(q), yield_now,
                                ).await.unwrap();
                                // The finished cursor scans at most one scalar, not V ordinals.
                                if actual.rows.is_empty() {
                                    assert_eq!(cursor.certificate(), &actual.certificate);
                                    assert_eq!(cursor.remaining(), 0);
                                    assert_eq!(cursor.max_integer(), None);
                                    assert!(!cursor.next_row(&mut Vec::new(), || -> Result<(), LiveRefusal> {
                                        panic!("an empty target result has no ordinal left to scan")
                                    }).unwrap());
                                } else {
                                    compare_retained(cursor, &actual, 1);
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn target_hops_admit_exact_workspace_result_caps_and_zero_allocation_trivial_queries() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        for n in [1, 2, 3, 63, 64, 65, 129] {
            // Signed finite weights must not change unweighted hop distance.
            let db = small_database(&contexts.commit(), n, true).await;
            let opt = options(Directedness::Directed);
            let graph = projection(&db, &cx, opt).await;
            for (target, cutoff) in [(VId((n - 1) as u128), None), (VId(0), None), (VId((n - 1) as u128), Some(0))] {
                let call = FnxCallSpec::shortest_path_length(VId(0), target, cutoff);
                let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
                let rows = expected.rows.len();
                let columns = call.outputs().len() * size_of::<String>()
                    + call.outputs().iter().map(|column| column.name.len()).sum::<usize>();
                let bytes = columns + rows * (size_of::<Vec<FnxValue>>() + call.outputs().len() * size_of::<FnxValue>());
                let workspace = if target == VId(0) || cutoff == Some(0) { 0 }
                    else { n * size_of::<usize>() + n.div_ceil(64) * size_of::<u64>() };
                assert_eq!(expected.certificate.kernel_workspace_bytes, workspace);
                let limits = FnxExecutionLimits { max_iterations: 0, max_result_rows: rows,
                    max_estimated_work: expected.certificate.estimated_work };
                let memory = FnxMemoryLimits { max_kernel_workspace_bytes: workspace, max_result_bytes: bytes };
                assert_eq!(call.execute_sealed(&cx, &graph, limits, memory).unwrap(), expected);
                let actual = call.execute_sealed_cooperative(&cx, &graph, limits, memory, quantum(1), yield_now)
                    .await.unwrap();
                compare_execution(&actual, &expected);
                for resource in ["estimated work", "result bytes", "kernel workspace bytes", "result rows"] {
                    let mut denied = memory;
                    let mut refused = limits;
                    match resource {
                        "estimated work" => refused.max_estimated_work -= 1,
                        "result bytes" => denied.max_result_bytes -= 1,
                        "kernel workspace bytes" if workspace > 0 => denied.max_kernel_workspace_bytes -= 1,
                        "result rows" if rows > 0 => refused.max_result_rows -= 1,
                        _ => continue,
                    }
                    for cooperative in [false, true] {
                        let result = if cooperative {
                            call.execute_sealed_cooperative(&cx, &graph, refused, denied, quantum(1), yield_now).await
                        } else { call.execute_sealed(&cx, &graph, refused, denied) };
                        assert!(matches!(result,
                            Err(FnxSealedExecutionError::Execution(FnxExecutionError::LimitExceeded {
                                resource: actual, ..
                            })) if actual == resource));
                    }
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn target_hops_preserve_every_live_cut_and_require_full_projection_admission() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = small_database(&commit, 3, true).await;
        let opt = options(Directedness::Directed);
        let graph = projection(&db, &cx, opt).await;
        let call = FnxCallSpec::shortest_path_length(VId(0), VId(2), None);
        let checks = Cell::new(0);
        let completed = call
            .execute_sealed_cooperative_with_checkpoint(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(3),
                yield_now,
                || {
                    checks.set(checks.get() + 1);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(
            completed.rows,
            vec![vec![FnxValue::Vertex(VId(2)), FnxValue::Integer(2)]]
        );
        for stop in 1..=checks.get() {
            let seen = Cell::new(0);
            let result = call
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
            live_refusal(result, stop);
            assert_eq!(seen.get(), stop);
        }
        for call in [
            FnxCallSpec::shortest_path_length(VId(u128::MAX), VId(0), Some(0)),
            FnxCallSpec::shortest_path_length(VId(0), VId(u128::MAX), Some(0)),
        ] {
            assert!(matches!(
                call.execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(1),
                    || -> std::future::Ready<()> {
                        panic!("endpoint validation precedes yielding")
                    }
                )
                .await,
                Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::UnknownSource(_) | FnxExecutionError::UnknownTarget(_)
                ))
            ));
        }

        // Even source==target cannot skip malformed weights on unreachable
        // selected rows: complete projection admission remains mandatory.
        let mut change = WriteBatch::new(RelationId(1));
        for vertex in [10, 11] {
            change.create_vertex(VId(vertex), vec![LabelId(1)], vec![]);
        }
        change.add_edge(
            EId(99),
            VId(10),
            VId(11),
            vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
        );
        db.write(&commit, change).await.unwrap();
        let trivial = FnxCallSpec::shortest_path_length(VId(0), VId(0), Some(0));
        assert!(matches!(
            db.execute_fnx_sealed_cooperative(
                &cx,
                &trivial,
                opt,
                memory(),
                SealedLimits::default(),
                quantum(1)
            )
            .await,
            Err(FnxSealedReadError::Projection(
                SealedProjectionError::Weight { edge: EId(99), .. }
            ))
        ));
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
