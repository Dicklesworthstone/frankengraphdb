//! PageRank normalizers from admitted projected degrees, with the decoded path
//! as an independent scalar-bit oracle.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Weights {
    Unit,
    PropertyUnit,
    UnitSum,
    Zero,
    Negative,
}

async fn normalizer_fixture(cx: &CommitCx, weights: Weights) -> (Database<MemVfs>, FnxReadOptions) {
    let keys = DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    );
    let mut db = Database::<MemVfs>::open_memory(cx, keys).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in [0, 1, 2, u128::MAX] {
        batch.create_vertex(VId(id), vec![LabelId(1)], vec![]);
    }
    for (id, source, target) in [(1, 0, 1), (2, 0, 2), (3, 1, 2)] {
        let weight = match weights {
            Weights::Zero => 0,
            Weights::Negative if id == 1 => -1,
            _ => 1,
        };
        batch.add_edge(
            EId(id),
            VId(source),
            VId(target),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(weight))],
        );
    }
    if weights == Weights::UnitSum {
        batch.add_edge(EId(4), VId(0), VId(1), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    let mut opt = options(Directedness::Directed);
    opt.projection.parallel_edges = ParallelEdgePolicy::Minimum;
    if matches!(weights, Weights::Unit | Weights::UnitSum) {
        opt.selection.weight = FnxWeightSpec::Unit;
    }
    if weights == Weights::UnitSum {
        // Unit selection does not prove unit projected weights: this row
        // reduces to weights 2 and 1, so degree 2 is not its normalizer 3.
        opt.projection.parallel_edges = ParallelEdgePolicy::Sum;
    }
    (db, opt)
}

async fn assert_normalizers(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    graph: &SealedGraphView,
    opt: FnxReadOptions,
    call: &FnxCallSpec,
    vectors: usize,
) -> (FnxResult, usize) {
    let independent = db
        .read_session()
        .unwrap()
        .execute_fnx(cx, call, opt)
        .unwrap()
        .analytics;
    let bytes = vectors * graph.node_count() * size_of::<f64>();
    let exact = FnxMemoryLimits {
        max_kernel_workspace_bytes: bytes,
        ..memory()
    };
    let mut checkpoints = 0;
    let expected = call
        .execute_sealed_with_checkpoint(cx, graph, opt.execution_limits, exact, || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(expected.columns, independent.columns);
    assert_eq!(expected.rows, independent.rows);
    assert_eq!(
        expected.certificate.result_digest,
        independent.certificate.result_digest
    );
    assert_eq!(expected.certificate.kernel_workspace_bytes, bytes);
    assert_eq!(expected.certificate.call_digest, call.digest());
    for q in [1, 7] {
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let (actual, pending) = drive(
            call.execute_sealed_cooperative(
                &controlled,
                graph,
                opt.execution_limits,
                exact,
                quantum(q),
                yield_now,
            ),
            &probe,
            q,
        );
        compare_execution(&actual.unwrap(), &expected);
        assert!(pending > 0, "scalar and row work must remain cooperative");
    }
    let denied = FnxMemoryLimits {
        max_kernel_workspace_bytes: bytes - 1,
        ..exact
    };
    for cooperative in [false, true] {
        let result = if cooperative {
            call.execute_sealed_cooperative(
                cx,
                graph,
                opt.execution_limits,
                denied,
                quantum(1),
                || -> std::future::Ready<()> {
                    panic!("workspace admission must precede yielding")
                },
            )
            .await
        } else {
            call.execute_sealed(cx, graph, opt.execution_limits, denied)
        };
        assert!(matches!(
            result,
            Err(FnxSealedExecutionError::Execution(
                FnxExecutionError::LimitExceeded {
                    resource: "kernel workspace bytes",
                    ..
                }
            ))
        ));
    }
    (expected, checkpoints)
}

#[test]
fn sealed_pagerank_degree_normalizers_preserve_scalar_bits_and_exact_workspace() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        for weights in [
            Weights::Unit,
            Weights::PropertyUnit,
            Weights::UnitSum,
            Weights::Zero,
        ] {
            let (db, opt) = normalizer_fixture(&contexts.commit(), weights).await;
            let graph = projection(&db, &cx, opt).await;
            let mut previous = None;
            for weighted in [false, true] {
                let call = FnxCallSpec::pagerank(
                    PageRankOptions::fixed_iterations(0.85, 4, weighted).unwrap(),
                );
                let vectors =
                    if !weighted || matches!(weights, Weights::Unit | Weights::PropertyUnit) {
                        2
                    } else {
                        3
                    };
                let (result, checkpoints) =
                    assert_normalizers(&db, &cx, &graph, opt, &call, vectors).await;
                if let Some((unweighted, unweighted_checkpoints)) = &previous {
                    if matches!(weights, Weights::Unit | Weights::PropertyUnit) {
                        assert_eq!(&result.rows, unweighted);
                        assert_eq!(
                            checkpoints, *unweighted_checkpoints,
                            "proved unit weights must not reopen rows to sum them"
                        );
                    } else {
                        assert_ne!(&result.rows, unweighted);
                        assert!(
                            checkpoints > *unweighted_checkpoints,
                            "nonunit and zero weights must retain ordered row summation"
                        );
                    }
                } else {
                    previous = Some((result.rows.clone(), checkpoints));
                }
                if weighted && weights == Weights::Zero {
                    assert!(
                        result
                            .rows
                            .iter()
                            .all(|row| matches!(row[1], FnxValue::Score(score)
                                if score.to_bits() == 0.25f64.to_bits()))
                    );
                }
            }
            if weights == Weights::PropertyUnit {
                for options in [
                    PageRankOptions::default(),
                    PageRankOptions::fixed_iterations(0.85, 4, true)
                        .unwrap()
                        .with_source(VId(u128::MAX)),
                ] {
                    assert_normalizers(&db, &cx, &graph, opt, &FnxCallSpec::pagerank(options), 2)
                        .await;
                }
            }
        }
        let (db, opt) = normalizer_fixture(&contexts.commit(), Weights::Negative).await;
        let graph = projection(&db, &cx, opt).await;
        // Personalization at an isolated vertex cannot hide a negative weight
        // in another row from the weighted initialization contract.
        let weighted = FnxCallSpec::pagerank(
            PageRankOptions::fixed_iterations(0.85, 2, true)
                .unwrap()
                .with_source(VId(u128::MAX)),
        );
        for result in [
            weighted.execute_sealed(&cx, &graph, opt.execution_limits, memory()),
            weighted
                .execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(1),
                    yield_now,
                )
                .await,
        ] {
            assert!(matches!(
                result,
                Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::NegativeWeight
                ))
            ));
        }
        assert_normalizers(
            &db,
            &cx,
            &graph,
            opt,
            &FnxCallSpec::pagerank(
                PageRankOptions::fixed_iterations(0.85, 2, false)
                    .unwrap()
                    .with_source(VId(u128::MAX)),
            ),
            2,
        )
        .await;
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

fn run_controlled(
    call: &FnxCallSpec,
    cx: &QueryCx,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    cooperative: bool,
    probe: &SimulationCheckpointProbe,
    guard: impl FnMut() -> std::result::Result<(), SealedProjectionError>,
) -> std::result::Result<FnxResult, FnxSealedExecutionError> {
    if cooperative {
        drive(
            call.execute_sealed_cooperative_with_checkpoint(
                cx,
                graph,
                limits,
                memory(),
                quantum(7),
                yield_now,
                guard,
            ),
            probe,
            7,
        )
        .0
    } else {
        call.execute_sealed_with_checkpoint(cx, graph, limits, memory(), guard)
    }
}

#[test]
fn sealed_pagerank_degree_normalizers_keep_cancellation_and_live_guards() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, opt) = normalizer_fixture(&contexts.commit(), Weights::Unit).await;
        let graph = projection(&db, &cx, opt).await;
        let call = FnxCallSpec::pagerank(
            PageRankOptions::fixed_iterations(0.85, 2, true)
                .unwrap()
                .with_source(VId(u128::MAX)),
        );
        for cooperative in [false, true] {
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let mut guards = 0;
            run_controlled(
                &call,
                &controlled,
                &graph,
                opt.execution_limits,
                cooperative,
                &probe,
                || {
                    guards += 1;
                    Ok(())
                },
            )
            .unwrap();
            for stop in 1..=probe.calls() {
                let refusal = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&refusal));
                let result = run_controlled(
                    &call,
                    &controlled,
                    &graph,
                    opt.execution_limits,
                    cooperative,
                    &refusal,
                    || Ok(()),
                );
                assert!(
                    matches!(result, Err(FnxSealedExecutionError::Cancelled(_))),
                    "cooperative={cooperative} cancellation={stop}"
                );
                assert_eq!(refusal.calls(), stop);
            }
            for stop in 1..=guards {
                let observe = Arc::new(SimulationCheckpointProbe::new(None));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&observe));
                let mut seen = 0;
                let result = run_controlled(
                    &call,
                    &controlled,
                    &graph,
                    opt.execution_limits,
                    cooperative,
                    &observe,
                    || {
                        seen += 1;
                        if seen == stop {
                            Err(SealedProjectionError::Guard(Box::new(LiveRefusal(stop))))
                        } else {
                            Ok(())
                        }
                    },
                );
                live_refusal(result.unwrap_err(), stop);
                assert_eq!(seen, stop);
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
