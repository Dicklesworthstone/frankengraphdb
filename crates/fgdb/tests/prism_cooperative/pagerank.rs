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
    assert_eq!(
        expected.certificate.execution_kernel,
        "fgdb-prism/sealed-pagerank-nonempty-rows-v3"
    );
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
        let actual = actual.unwrap();
        assert_eq!(
            actual.certificate.execution_kernel,
            "fgdb-prism/sealed-pagerank-nonempty-rows-cooperative-v3"
        );
        compare_execution(&actual, &expected);
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

#[test]
fn sealed_pagerank_omits_admitted_empty_history_rows_and_retains_exact_results() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(
            &commit,
            DatabaseKeys::new(
                [0x94; 32],
                DatabaseSecurityNamespaceId([0x95; 32]),
                [0x96; 32],
            ),
        )
        .await
        .unwrap();
        let vertices = [
            VId(0),
            VId(17),
            VId(1 << 100),
            VId((1 << 100) + 1),
            VId(u128::MAX),
        ];
        const HISTORY: usize = 256;
        const ITERATIONS: usize = 2;
        let mut initial = WriteBatch::new(RelationId(1));
        for vertex in vertices {
            initial.create_vertex(vertex, vec![LabelId(1)], vec![]);
        }
        initial.create_vertex(VId(99), vec![LabelId(2)], vec![]);
        for (id, source, target, weight) in [(1, 0, 1, 0), (2, 1, 0, 3)] {
            initial.add_edge(
                EId(id),
                vertices[source],
                vertices[target],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(weight))],
            );
        }
        let mut retire = Vec::new();
        for source in 2..vertices.len() {
            let target = 2 + (source - 1) % (vertices.len() - 2);
            for offset in 0..HISTORY {
                let suffix = (source * HISTORY + offset) as u128;
                let edge = EId(1000 + suffix);
                initial.add_edge(
                    edge,
                    vertices[source],
                    vertices[target],
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
                );
                retire.push(edge);
                // Excluded endpoints must be masked before their deliberately
                // nonnumeric weights are read, on both incidence faces.
                for (base, from, to) in [
                    (10_000, vertices[source], VId(99)),
                    (20_000, VId(99), vertices[source]),
                ] {
                    initial.add_edge(
                        EId(base + suffix),
                        from,
                        to,
                        vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
                    );
                }
            }
        }
        let original = db.write(&commit, initial).await.unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        for edge in retire {
            change.delete_edge(edge);
        }
        let current = db.write(&commit, change).await.unwrap();
        let mut retained = Vec::new();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            for at in [original, current] {
                let mut opt = options(direction);
                opt.as_of = Some(at);
                let graph = projection(&db, &cx, opt).await;
                assert_eq!(graph.node_count(), vertices.len());
                let mut omitted_raw_work = 0;
                for source in 2..vertices.len() {
                    if at == original {
                        assert!(graph.degree(source).unwrap() > 0);
                    } else {
                        assert_eq!(graph.degree(source), Some(0));
                        let (neighbors, spent, _) = row(&graph, &cx, source, None, 7);
                        assert!(neighbors.is_empty());
                        assert!(spent >= HISTORY, "an empty row still has real source work");
                        omitted_raw_work += spent;
                    }
                }
                let mut last = None;
                for weighted in [false, true] {
                    for source in [None, Some(VId(u128::MAX))] {
                        let rank =
                            PageRankOptions::fixed_iterations(0.85, ITERATIONS, weighted).unwrap();
                        let rank = source.map_or(rank, |source| rank.with_source(source));
                        let call = FnxCallSpec::pagerank(rank);
                        let (expected, checkpoints) = assert_normalizers(
                            &db,
                            &cx,
                            &graph,
                            opt,
                            &call,
                            if weighted { 3 } else { 2 },
                        )
                        .await;
                        assert_eq!(
                            expected.certificate.witness.edges_scanned,
                            ITERATIONS * graph.adjacency_entry_count(),
                            "nonempty zero-weight rows remain actual edge visits"
                        );
                        if at == current {
                            // This counts the entire controlled execution, not
                            // wall time. Even one scan of the admitted empty
                            // rows costs more raw work than the whole kernel.
                            assert!(
                                checkpoints < omitted_raw_work,
                                "direction={direction:?} weighted={weighted} source={source:?}: \
                                 checkpoints={checkpoints}, empty-row raw work={omitted_raw_work}"
                            );
                        }
                        if weighted && source.is_some() {
                            if at == current {
                                // Omission changes actual source work, not
                                // conservative admission of retained history.
                                let requested = expected.certificate.estimated_work;
                                let exact = FnxExecutionLimits {
                                    max_estimated_work: requested,
                                    ..opt.execution_limits
                                };
                                assert_eq!(
                                    call.execute_sealed(&cx, &graph, exact, memory()).unwrap(),
                                    expected
                                );
                                let denied = FnxExecutionLimits {
                                    max_estimated_work: requested - 1,
                                    ..exact
                                };
                                for result in [
                                    call.execute_sealed(&cx, &graph, denied, memory()),
                                    call.execute_sealed_cooperative(
                                        &cx,
                                        &graph,
                                        denied,
                                        memory(),
                                        quantum(1),
                                        || -> std::future::Ready<()> {
                                            panic!("work admission must precede yielding")
                                        },
                                    )
                                    .await,
                                ] {
                                    assert!(matches!(
                                        result,
                                        Err(FnxSealedExecutionError::Execution(
                                            FnxExecutionError::LimitExceeded {
                                                resource: "estimated work",
                                                limit,
                                                requested: actual,
                                            }
                                        )) if limit == requested - 1 && actual == requested
                                    ));
                                }
                            }
                            for _ in 0..2 {
                                assert_eq!(
                                    db.execute_fnx_sealed(
                                        &cx,
                                        &call,
                                        opt,
                                        memory(),
                                        SealedLimits::default(),
                                    )
                                    .await
                                    .unwrap()
                                    .analytics,
                                    expected
                                );
                            }
                            last = Some(expected);
                        }
                    }
                }
                retained.push((graph, opt.execution_limits, last.unwrap()));
            }
        }
        db.compact(&commit).await.unwrap();
        drop(db);
        let call = FnxCallSpec::pagerank(
            PageRankOptions::fixed_iterations(0.85, ITERATIONS, true)
                .unwrap()
                .with_source(VId(u128::MAX)),
        );
        for (graph, limits, expected) in retained {
            assert_eq!(
                call.execute_sealed(&cx, &graph, limits, memory()).unwrap(),
                expected
            );
            compare_execution(
                &call
                    .execute_sealed_cooperative(
                        &cx,
                        &graph,
                        limits,
                        memory(),
                        quantum(7),
                        yield_now,
                    )
                    .await
                    .unwrap(),
                &expected,
            );
        }
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
