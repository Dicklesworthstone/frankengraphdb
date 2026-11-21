//! Stored-source DAG order, projection masks and cooperative cancellation.

use super::*;

fn calls() -> [FnxCallSpec; 2] {
    [
        FnxCallSpec::bind(
            "CALL fnx.topological_sort() YIELD rank AS position,node AS dependency",
            &FnxParameters::new(),
        )
        .unwrap(),
        FnxCallSpec::is_directed_acyclic_graph(),
    ]
}

fn drain(mut cursor: FnxRowCursor, expected: &FnxResult) {
    assert_eq!(cursor.certificate(), &expected.certificate);
    assert_eq!(cursor.remaining(), expected.rows.len());
    let mut rows = Vec::new();
    let mut row = Vec::new();
    while cursor
        .next_row(&mut row, || Ok::<(), std::convert::Infallible>(()))
        .unwrap()
    {
        rows.push(row.clone());
    }
    assert_eq!(rows, expected.rows);
    assert_eq!(cursor.remaining(), 0);
}

#[test]
fn dag_compressed_and_cooperative_calls_keep_order_empty_predicates_and_exact_limits() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let commit = contexts.commit();
        for n in [0, 3] {
            let mut db = small_database(&commit, n, false).await;
            if n != 0 {
                let mut more = WriteBatch::new(RelationId(1));
                more.create_vertex(VId(u128::MAX), vec![LabelId(1)], vec![]);
                more.create_vertex(VId(99), vec![LabelId(99)], vec![]);
                // A projected parallel pair and a cycle through an excluded endpoint.
                more.add_edge(
                    EId(3),
                    VId(0),
                    VId(1),
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(-2))],
                );
                more.add_edge(
                    EId(4),
                    VId(2),
                    VId(99),
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
                );
                more.add_edge(
                    EId(5),
                    VId(99),
                    VId(0),
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
                );
                db.write(&commit, more).await.unwrap();
                // Retained updates exercise real compressed history pulls.
                for weight in 3..6 {
                    let mut edit = WriteBatch::new(RelationId(1));
                    edit.set_edge_property(
                        EId(1),
                        PropertyKeyId(1),
                        Some(CanonicalScalar::Int(weight)),
                    );
                    db.write(&commit, edit).await.unwrap();
                }
            }
            for direction in [Directedness::Directed, Directedness::Reversed] {
                let opt = options(direction);
                let graph = projection(&db, &cx, opt).await;
                for call in calls() {
                    let decoded = db.execute_fnx(&cx, &call, opt).unwrap().analytics;
                    let sync = call
                        .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                        .unwrap();
                    assert_eq!(sync.rows, decoded.rows);
                    assert_eq!(
                        sync.certificate.result_digest,
                        decoded.certificate.result_digest
                    );
                    drain(
                        call.execute_sealed_rows(&cx, &graph, opt.execution_limits, memory())
                            .unwrap(),
                        &sync,
                    );
                    let exact = FnxExecutionLimits {
                        max_result_rows: sync.rows.len(),
                        max_estimated_work: sync.certificate.estimated_work,
                        ..opt.execution_limits
                    };
                    let exact_memory = FnxMemoryLimits {
                        max_kernel_workspace_bytes: sync.certificate.kernel_workspace_bytes,
                        ..memory()
                    };
                    for q in [1, 7] {
                        let actual = call
                            .execute_sealed_cooperative(
                                &cx,
                                &graph,
                                exact,
                                exact_memory,
                                quantum(q),
                                yield_now,
                            )
                            .await
                            .unwrap();
                        compare_execution(&actual, &sync);
                        drain(
                            call.execute_sealed_cooperative_rows(
                                &cx,
                                &graph,
                                exact,
                                exact_memory,
                                quantum(q),
                                yield_now,
                            )
                            .await
                            .unwrap(),
                            &actual,
                        );
                    }
                    assert!(matches!(
                        call.execute_sealed(
                            &cx,
                            &graph,
                            FnxExecutionLimits {
                                max_estimated_work: exact.max_estimated_work - 1,
                                ..exact
                            },
                            exact_memory
                        ),
                        Err(FnxSealedExecutionError::Execution(
                            FnxExecutionError::LimitExceeded {
                                resource: "estimated work",
                                ..
                            }
                        ))
                    ));
                    if !sync.rows.is_empty() {
                        assert!(matches!(
                            call.execute_sealed(
                                &cx,
                                &graph,
                                exact,
                                FnxMemoryLimits {
                                    max_result_bytes: 0,
                                    ..exact_memory
                                },
                            ),
                            Err(FnxSealedExecutionError::Execution(
                                FnxExecutionError::LimitExceeded {
                                    resource: "result bytes",
                                    ..
                                }
                            ))
                        ));
                        assert!(matches!(
                            call.execute_sealed(
                                &cx,
                                &graph,
                                FnxExecutionLimits {
                                    max_result_rows: sync.rows.len() - 1,
                                    ..exact
                                },
                                exact_memory
                            ),
                            Err(FnxSealedExecutionError::Execution(
                                FnxExecutionError::LimitExceeded {
                                    resource: "result rows",
                                    ..
                                }
                            ))
                        ));
                    }
                    if exact_memory.max_kernel_workspace_bytes != 0 {
                        assert!(matches!(
                            call.execute_sealed(
                                &cx,
                                &graph,
                                exact,
                                FnxMemoryLimits {
                                    max_kernel_workspace_bytes: exact_memory
                                        .max_kernel_workspace_bytes
                                        - 1,
                                    ..exact_memory
                                }
                            ),
                            Err(FnxSealedExecutionError::Execution(
                                FnxExecutionError::LimitExceeded {
                                    resource: "kernel workspace bytes",
                                    ..
                                }
                            ))
                        ));
                    }
                }
                if n != 0 {
                    let mut unmasked = opt;
                    unmasked.selection.vertex_labels = None.into();
                    let all = projection(&db, &cx, unmasked).await;
                    assert_eq!(
                        FnxCallSpec::is_directed_acyclic_graph()
                            .execute_sealed(&cx, &all, opt.execution_limits, memory())
                            .unwrap()
                            .rows,
                        vec![vec![FnxValue::Boolean(false)]]
                    );
                    assert!(matches!(
                        FnxCallSpec::topological_sort().execute_sealed(
                            &cx,
                            &all,
                            opt.execution_limits,
                            memory()
                        ),
                        Err(FnxSealedExecutionError::Execution(
                            FnxExecutionError::DirectedCycle
                        ))
                    ));
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn dag_cycles_and_self_loops_refuse_without_partial_rows_in_both_sealed_modes() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        for loop_edge in [false, true] {
            let mut db = small_database(&contexts.commit(), 3, false).await;
            let mut close = WriteBatch::new(RelationId(1));
            close.add_edge(
                EId(3),
                VId(2),
                VId(if loop_edge { 2 } else { 0 }),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(0))],
            );
            db.write(&contexts.commit(), close).await.unwrap();
            let opt = options(Directedness::Directed);
            let graph = projection(&db, &cx, opt).await;
            let topo = FnxCallSpec::topological_sort();
            assert!(matches!(
                topo.execute_sealed(&cx, &graph, opt.execution_limits, memory()),
                Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::DirectedCycle
                ))
            ));
            assert!(matches!(
                topo.execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(1),
                    yield_now
                )
                .await,
                Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::DirectedCycle
                ))
            ));
            let predicate = FnxCallSpec::is_directed_acyclic_graph();
            let actual = predicate
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
            assert_eq!(actual.rows, vec![vec![FnxValue::Boolean(false)]]);
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn dag_cooperative_execution_cancels_at_every_checkpoint_and_never_mutates_storage() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let before = db.read_session().unwrap().partition_root();
        let opt = options(Directedness::Directed);
        let graph = projection(&db, &cx, opt).await;
        for call in calls() {
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
            assert!(yields > 0);
            for stop in 1..=probe.calls() {
                let stopping = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&stopping));
                let (actual, _) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(7),
                        yield_now,
                    ),
                    &stopping,
                    7,
                );
                assert!(matches!(actual, Err(FnxSealedExecutionError::Cancelled(_))));
                assert_eq!(stopping.calls(), stop);
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
        assert_eq!(db.read_session().unwrap().partition_root(), before);
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
