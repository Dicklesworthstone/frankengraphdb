//! Distance centralities through authenticated history and compressed rows.

use super::*;
use fgdb_prism::{AdapterPath, FnxGraphKind, FnxValue, ProjectionError};

fn call(harmonic: bool) -> FnxCallSpec {
    FnxCallSpec::bind(
        if harmonic {
            "CALL fnx.harmonic_centrality() YIELD score AS rank,vertex AS id"
        } else {
            "CALL fnx.closeness_centrality() YIELD score AS rank,vertex AS id"
        },
        &FnxParameters::new(),
    )
    .unwrap()
}

#[test]
fn compressed_distance_centralities_match_native_history_reductions_and_full_width_ids() {
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
        for harmonic in [false, true] {
            let call = call(harmonic);
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
                        if harmonic {
                            "fgdb-prism/sealed-harmonic-fifo-v1"
                        } else {
                            "fgdb-prism/sealed-closeness-fifo-v1"
                        }
                    );
                    assert_eq!(
                        expected.certificate.kernel_workspace_bytes,
                        graph.node_count() * (2 * size_of::<usize>() + size_of::<f64>())
                    );
                    assert_eq!(
                        expected.rows.last().unwrap()[1],
                        FnxValue::Vertex(VId(u128::MAX))
                    );
                    if at == current {
                        let isolate = expected
                            .rows
                            .iter()
                            .find(|row| row[1] == FnxValue::Vertex(VId(3)))
                            .unwrap();
                        assert_eq!(isolate[0], FnxValue::Score(0.0));
                    }
                    if let Some(previous) = previous_reduction {
                        assert_eq!(
                            expected.certificate.result_digest, previous,
                            "finite edge weights are not hop distances"
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
                            if harmonic {
                                "fgdb-prism/sealed-harmonic-fifo-cooperative-v1"
                            } else {
                                "fgdb-prism/sealed-closeness-fifo-cooperative-v1"
                            }
                        );
                        if q == 1 {
                            assert!(
                                pending > 700,
                                "every repeated raw-history pass must spend fuel"
                            );
                        }
                        if let Some(previous) = &previous_quantum {
                            assert_eq!(&actual, previous);
                        }
                        previous_quantum = Some(actual);
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
                    assert_eq!(hosted.analytics, previous_quantum.unwrap());
                    assert_eq!(
                        db.execute_fnx_sealed(&cx, &call, opt, memory(), SealedLimits::default(),)
                            .await
                            .unwrap()
                            .analytics,
                        expected
                    );
                }
                if let Some(previous) = previous_cut {
                    assert_ne!(
                        previous_reduction,
                        Some(previous),
                        "a changed topology/cut must change these scores"
                    );
                }
                previous_cut = previous_reduction;
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_distance_centralities_admit_exact_work_workspace_and_output() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, true).await;
        // The literal source is 0 -> 1 -> 2. Incoming closeness uses the
        // reachable-set correction: both other vertices reach vertex 2,
        // so (2/3) * (2/2) = 2/3. Harmonic adds 1 and 1/2 in that order.
        let two_thirds = 2.0 / 3.0;
        for (direction, call, literal) in [
            (Directedness::Undirected, call(false), None),
            (Directedness::Undirected, call(true), None),
            (Directedness::Undirected, FnxCallSpec::bind(
                "CALL fnx.load_centrality() YIELD score AS rank,vertex AS id",
                &FnxParameters::new(),
            ).unwrap(), None),
            (Directedness::Directed, call(false), Some([0.0, 0.5, two_thirds])),
            (Directedness::Directed, call(true), Some([0.0, 1.0, 1.5])),
            (Directedness::Reversed, call(false), Some([two_thirds, 0.5, 0.0])),
            (Directedness::Reversed, call(true), Some([1.5, 1.0, 0.0])),
        ] {
            let opt = options(direction);
            let graph = projection(&db, &cx, opt).await;
            let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
            if let Some(scores) = literal {
                let rows: Vec<_> = scores.into_iter().enumerate().map(|(vertex, score)| {
                    vec![FnxValue::Score(score), FnxValue::Vertex(VId(vertex as u128))]
                }).collect();
                assert_eq!(expected.rows, rows, "{direction:?}");
            }
            assert_eq!(expected.certificate.snapshot, graph.binding());
            assert_eq!(expected.certificate.projection_digest, graph.digest());
            let opposite = if direction == Directedness::Undirected {
                None
            } else {
                let opposite_direction = if direction == Directedness::Directed {
                    Directedness::Reversed
                } else {
                    Directedness::Directed
                };
                let opposite = projection(&db, &cx, options(opposite_direction)).await;
                assert_eq!(opposite.binding(), graph.binding());
                assert_eq!(opposite.vertex_ids(), graph.vertex_ids());
                assert_ne!(opposite.digest(), graph.digest());
                // An independently admitted opposite face prices the actual
                // source directory/index beside the three scalar vectors.
                let scalar = graph.node_count()
                    * (2 * size_of::<usize>() + size_of::<f64>());
                assert_eq!(expected.certificate.kernel_workspace_bytes,
                    scalar + opposite.charged_workspace_bytes());
                Some(opposite)
            };
            let mandatory_workspace = if let Some(opposite) = &opposite {
                let n = graph.node_count();
                let scalar = n * (2 * size_of::<usize>() + size_of::<f64>());
                let auxiliary = n * (size_of::<VId>() + size_of::<usize>())
                    + opposite.incoming_index_stats()
                        .map_or(0, |stats| stats.charged_workspace_bytes);
                // This three-vertex fixture has no optional endpoint prefix.
                // Incoming degree counts are optional on both directed faces;
                // the mandatory bound includes the incoming construction peak.
                assert_eq!(opposite.charged_workspace_bytes(),
                    auxiliary + n * size_of::<usize>());
                scalar + auxiliary
            } else {
                expected.certificate.kernel_workspace_bytes
            };
            let columns = call.outputs().len() * size_of::<String>()
                + call.outputs().iter().map(|column| column.name.len()).sum::<usize>();
            let exact = FnxExecutionLimits {
                max_iterations: 0,
                max_result_rows: graph.node_count(),
                max_estimated_work: expected.certificate.estimated_work,
            };
            let mem = FnxMemoryLimits {
                max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
                max_result_bytes: columns + graph.node_count()
                    * (size_of::<Vec<FnxValue>>() + call.outputs().len() * size_of::<FnxValue>()),
            };
            assert_eq!(call.execute_sealed(&cx, &graph, exact, mem).unwrap(), expected);
            compare_execution(&call.execute_sealed_cooperative(
                &cx, &graph, exact, mem, quantum(1), yield_now,
            ).await.unwrap(), &expected);
            if opposite.is_some() {
                // One byte below the fully cached peak is still admissible.
                // Both it and the exact mandatory bound omit optional counts,
                // preserving every logical result and certificate field.
                let mut fallback_expected = expected.clone();
                fallback_expected.certificate.kernel_workspace_bytes = mandatory_workspace;
                for allowance in [mem.max_kernel_workspace_bytes - 1, mandatory_workspace] {
                    let fallback = FnxMemoryLimits {
                        max_kernel_workspace_bytes: allowance,
                        ..mem
                    };
                    assert_eq!(call.execute_sealed(&cx, &graph, exact, fallback).unwrap(),
                        fallback_expected);
                    compare_execution(&call.execute_sealed_cooperative(
                        &cx, &graph, exact, fallback, quantum(1), yield_now,
                    ).await.unwrap(), &fallback_expected);
                }
            }
            for resource in ["estimated work", "kernel workspace bytes", "result rows", "result bytes"] {
                let mut limits = exact;
                let mut memory = mem;
                match resource {
                    "estimated work" => limits.max_estimated_work -= 1,
                    "kernel workspace bytes" => {
                        memory.max_kernel_workspace_bytes = mandatory_workspace - 1;
                    }
                    "result rows" => limits.max_result_rows -= 1,
                    "result bytes" => memory.max_result_bytes -= 1,
                    _ => unreachable!(),
                }
                for cooperative in [false, true] {
                    let directed_workspace = resource == "kernel workspace bytes"
                        && direction != Directedness::Undirected;
                    let result = if cooperative {
                        if directed_workspace {
                            // The auxiliary constructor checks its mandatory
                            // bound while running the normal bounded driver.
                            call.execute_sealed_cooperative(&cx, &graph, limits, memory,
                                quantum(1), yield_now).await
                        } else {
                            call.execute_sealed_cooperative(&cx, &graph, limits, memory, quantum(1),
                                || -> std::future::Ready<()> { panic!("admission precedes yield") }).await
                        }
                    } else {
                        call.execute_sealed(&cx, &graph, limits, memory)
                    };
                    if directed_workspace {
                        let opposite = opposite.as_ref().unwrap();
                        match direction {
                            Directedness::Directed => {
                                let incoming = opposite.incoming_index_stats().unwrap()
                                    .charged_workspace_bytes;
                                assert!(matches!(result, Err(FnxSealedExecutionError::Source(
                                    SealedProjectionError::Read(
                                        fgdb_strata::tiered::sealed::SealedError::Limit {
                                            resource: "incoming workspace bytes", requested, limit,
                                        }
                                    )
                                )) if requested == incoming && limit == incoming - 1));
                            }
                            Directedness::Reversed => {
                                let auxiliary = graph.node_count()
                                    * (size_of::<VId>() + size_of::<usize>());
                                assert!(matches!(result, Err(FnxSealedExecutionError::Source(
                                    SealedProjectionError::Projection(ProjectionError::LimitExceeded {
                                        resource: "workspace bytes", observed, limit,
                                    })
                                )) if observed == auxiliary && limit == auxiliary - 1));
                            }
                            Directedness::Undirected => unreachable!(),
                        }
                    } else {
                        assert!(matches!(result, Err(FnxSealedExecutionError::Execution(
                            FnxExecutionError::LimitExceeded { resource: actual, .. })) if actual == resource));
                    }
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_load_preserves_history_flow_bits_quanta_and_every_live_cancellation() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (mut db, old) = database(&contexts.commit()).await;
        let mut change = WriteBatch::new(RelationId(1));
        change.create_vertex(VId(3), vec![LabelId(1)], vec![]);
        change.add_edge(EId(7), VId(2), VId(0), vec![(PropertyKeyId(1), CanonicalScalar::Int(-4))]);
        db.write(&contexts.commit(), change).await.unwrap();
        let current = db.frontier().unwrap();
        let call = FnxCallSpec::bind("CALL fnx.load_centrality() YIELD vertex,score", &FnxParameters::new()).unwrap();
        assert!(call.supports_cooperative_sealed_execution());
        for at in [old, current] {
            for reduction in [ParallelEdgePolicy::Sum, ParallelEdgePolicy::Minimum,
                ParallelEdgePolicy::Maximum, ParallelEdgePolicy::CollapseUnit] {
                let mut opt = options(Directedness::Undirected);
                opt.as_of = Some(at);
                opt.projection.self_loops = SelfLoopPolicy::Drop;
                opt.projection.parallel_edges = reduction;
                let graph = projection(&db, &cx, opt).await;
                assert!(graph.scan_incidence_bound() > graph.adjacency_entry_count());
                let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
                let native = db.execute_fnx(&cx, &call, opt).unwrap().analytics;
                assert_eq!(expected.rows, native.rows);
                assert_eq!(expected.certificate.result_digest, native.certificate.result_digest);
                assert_eq!(expected.certificate.adapter, AdapterPath::CompressedCursor);
                assert_eq!(expected.certificate.execution_kernel, "fgdb-prism/sealed-newman-load-v2");
                assert_eq!(expected.certificate.kernel_workspace_bytes,
                    graph.node_count() * (3 * size_of::<usize>() + 2 * size_of::<f64>()));
                if at == current {
                    assert!(expected.rows.contains(&vec![FnxValue::Vertex(VId(3)), FnxValue::Score(0.0)]));
                }
                for q in [1, 7, 257] {
                    let probe = Arc::new(SimulationCheckpointProbe::new(None));
                    let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                    let (actual, pending) = drive(call.execute_sealed_cooperative(
                        &controlled, &graph, opt.execution_limits, memory(), quantum(q), yield_now,
                    ), &probe, q);
                    let actual = actual.unwrap();
                    compare_execution(&actual, &expected);
                    assert_eq!(actual.certificate.execution_kernel, "fgdb-prism/sealed-newman-load-cooperative-v2");
                    if q == 1 { assert!(pending > 700); }
                }
                assert_eq!(db.execute_fnx_sealed(&cx, &call, opt, memory(), SealedLimits::default()).await.unwrap().analytics, expected);
            }
        }
        // Sweep a small real compressed graph, including scalar-only phases,
        // source cursor pulls, normalization and result conversion.
        let small = small_database(&contexts.commit(), 4, true).await;
        let opt = options(Directedness::Undirected);
        let graph = projection(&small, &cx, opt).await;
        let seen = Cell::new(0);
        call.execute_sealed_with_checkpoint(&cx, &graph, opt.execution_limits, memory(), || {
            seen.set(seen.get() + 1); Ok(())
        }).unwrap();
        for stop in 1..=seen.get() {
            let at = Cell::new(0);
            let error = call.execute_sealed_with_checkpoint(&cx, &graph, opt.execution_limits, memory(), || {
                at.set(at.get() + 1);
                if at.get() == stop { Err(SealedProjectionError::UnknownOrdinal(stop)) } else { Ok(()) }
            }).unwrap_err();
            assert!(matches!(error, FnxSealedExecutionError::Source(SealedProjectionError::UnknownOrdinal(actual)) if actual == stop));
            assert_eq!(at.get(), stop);
        }
        for n in [0, 1, 2] {
            let empty = small_database(&contexts.commit(), n, false).await;
            let graph = projection(&empty, &cx, opt).await;
            let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
            assert_eq!(expected.rows.len(), n);
            assert!(expected.rows.iter().all(|row| row[1] == FnxValue::Score(0.0)));
            compare_execution(&call.execute_sealed_cooperative(&cx, &graph, opt.execution_limits,
                memory(), quantum(1), yield_now).await.unwrap(), &expected);
        }
        let kept = projection(&db, &cx, opt).await;
        assert!(matches!(call.execute_sealed(&cx, &kept, opt.execution_limits, memory()),
            Err(FnxSealedExecutionError::Execution(FnxExecutionError::SelfLoopRefused))));
        for direction in [Directedness::Directed, Directedness::Reversed] {
            assert!(matches!(call.validate_sealed_projection(direction), Err(FnxSealedExecutionError::Execution(
                FnxExecutionError::GraphKind { required: FnxGraphKind::Undirected }))));
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_load_reached_passes_admit_isolates_with_linear_work() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let mut opt = options(Directedness::Undirected);
        let call = FnxCallSpec::bind(
            "CALL fnx.load_centrality() YIELD vertex,score",
            &FnxParameters::new(),
        )
        .unwrap();
        for n in [0, 1, 512] {
            let db = small_database(&contexts.commit(), n, false).await;
            let graph = projection(&db, &cx, opt).await;
            // An edgeless source opens each row only once. The old V-times
            // full-population admission refused 512 isolates under this cap.
            opt.execution_limits.max_estimated_work = 80 * n + 128;
            let expected = call
                .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                .unwrap();
            assert_eq!(
                expected.certificate.execution_kernel,
                "fgdb-prism/sealed-newman-load-v2"
            );
            assert_eq!(expected.rows.len(), n);
            assert!(
                expected
                    .rows
                    .iter()
                    .all(|row| row[1] == FnxValue::Score(0.0))
            );
            let native = db.execute_fnx(&cx, &call, opt).unwrap().analytics;
            assert_eq!(native.rows, expected.rows);
            assert_eq!(
                native.certificate.result_digest,
                expected.certificate.result_digest
            );
            compare_execution(
                &call
                    .execute_sealed_cooperative(
                        &cx,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(17),
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

#[test]
fn compressed_distance_centralities_preserve_empty_isolate_and_graph_kind_laws() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let opt = options(Directedness::Undirected);
        for harmonic in [false, true] {
            let call = call(harmonic);
            for n in [0, 1, 512] {
                let db = small_database(&contexts.commit(), n, false).await;
                let graph = projection(&db, &cx, opt).await;
                // This real compressed call has only linear work. The old
                // all-source times whole-population bound refused n=512
                // under this allowance despite opening every empty row once.
                let linear = FnxExecutionLimits {
                    max_estimated_work: 64 * n + 128,
                    ..opt.execution_limits
                };
                let expected = call.execute_sealed(&cx, &graph, linear, memory()).unwrap();
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
                assert_eq!(expected.rows, native.rows);
                assert!(
                    expected
                        .rows
                        .iter()
                        .all(|row| row[0] == FnxValue::Score(0.0))
                );
                assert_eq!(expected.certificate.witness.nodes_touched, n);
                assert_eq!(expected.certificate.witness.edges_scanned, 0);
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let (actual, pending) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        linear,
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
                    "isolates must not repeatedly clear the whole population"
                );
                if n == 512 {
                    let mut bounded = opt;
                    bounded.execution_limits = linear;
                    assert_eq!(
                        db.execute_fnx_sealed(
                            &cx,
                            &call,
                            bounded,
                            memory(),
                            SealedLimits::default(),
                        )
                        .await
                        .unwrap()
                        .analytics,
                        expected
                    );
                    compare_execution(
                        &db.execute_fnx_sealed_cooperative(
                            &cx,
                            &call,
                            bounded,
                            memory(),
                            SealedLimits::default(),
                            quantum(7),
                        )
                        .await
                        .unwrap()
                        .analytics,
                        &expected,
                    );
                }
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
                        panic!("loop refusal precedes allocation/yield")
                    },
                )
                .await,
                Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::SelfLoopRefused
                ))
            ));
            for direction in [Directedness::Directed, Directedness::Reversed] {
                let mut invalid = opt;
                invalid.projection.directedness = direction;
                for cooperative in [false, true] {
                    let sealing = SealedLimits {
                        max_image_bytes: 0,
                        ..SealedLimits::default()
                    };
                    let error = if cooperative {
                        db.execute_fnx_sealed_cooperative(
                            &cx,
                            &call,
                            invalid,
                            memory(),
                            sealing,
                            quantum(1),
                        )
                        .await
                    } else {
                        db.execute_fnx_sealed(&cx, &call, invalid, memory(), sealing)
                            .await
                    }
                    .unwrap_err();
                    // Directed admission now reaches the zero-byte image limit.
                    assert!(matches!(
                        error,
                        FnxSealedReadError::Seal(
                            fgdb_strata::tiered::sealed::SealedError::Identity(_)
                        )
                    ));
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn sparse_distance_admission_retains_excluded_incidence_cost_and_exact_refusals() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let n = 257;
        let pairs = 8;
        let mut db = small_database(&contexts.commit(), n, false).await;
        let mut edges = WriteBatch::new(RelationId(1));
        edges.create_vertex(VId(u128::MAX), vec![LabelId(2)], vec![]);
        for pair in 0..pairs {
            edges.add_edge(
                EId(pair as u128),
                VId((2 * pair) as u128),
                VId((2 * pair + 1) as u128),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(-7))],
            );
        }
        for vertex in 0..16 {
            edges.add_edge(
                EId(100 + vertex),
                VId(vertex),
                VId(u128::MAX),
                vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
            );
        }
        db.write(&contexts.commit(), edges).await.unwrap();
        let mut opt = options(Directedness::Undirected);
        opt.execution_limits.max_estimated_work = 100_000;
        let graph = projection(&db, &cx, opt).await;
        assert_eq!(graph.node_count(), n);
        assert_eq!(graph.edge_count(), pairs);
        assert!(graph.scan_incidence_bound() > graph.adjacency_entry_count());
        for harmonic in [false, true] {
            let call = call(harmonic);
            let checkpoints = Cell::new(0usize);
            let expected = call
                .execute_sealed_with_checkpoint(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    || {
                        checkpoints.set(checkpoints.get() + 1);
                        Ok(())
                    },
                )
                .unwrap();
            assert!(checkpoints.get() <= expected.certificate.estimated_work);
            assert_eq!(expected.certificate.witness.nodes_touched, n + 2 * pairs);
            assert_eq!(expected.certificate.witness.edges_scanned, 4 * pairs);
            let connected_score = if harmonic { 1.0 } else { 1.0 / (n - 1) as f64 };
            for (vertex, row) in expected.rows.iter().enumerate() {
                assert_eq!(row[1], FnxValue::Vertex(VId(vertex as u128)));
                assert_eq!(
                    row[0],
                    FnxValue::Score(if vertex < 2 * pairs { connected_score } else { 0.0 })
                );
            }
            let exact = FnxExecutionLimits {
                max_estimated_work: expected.certificate.estimated_work,
                ..opt.execution_limits
            };
            assert_eq!(call.execute_sealed(&cx, &graph, exact, memory()).unwrap(), expected);
            compare_execution(
                &call
                    .execute_sealed_cooperative(&cx, &graph, exact, memory(), quantum(7), yield_now)
                    .await
                    .unwrap(),
                &expected,
            );
            let below = FnxExecutionLimits {
                max_estimated_work: exact.max_estimated_work - 1,
                ..exact
            };
            for cooperative in [false, true] {
                let result = if cooperative {
                    call.execute_sealed_cooperative(
                        &cx,
                        &graph,
                        below,
                        memory(),
                        quantum(1),
                        || -> std::future::Ready<()> { panic!("admission precedes yield") },
                    )
                    .await
                } else {
                    call.execute_sealed(&cx, &graph, below, memory())
                };
                assert!(matches!(result, Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::LimitExceeded {
                        resource: "estimated work", limit, requested,
                    })) if limit == below.max_estimated_work && requested == exact.max_estimated_work));
            }
            assert_eq!(
                db.execute_fnx_sealed(&cx, &call, opt, memory(), SealedLimits::default())
                    .await
                    .unwrap()
                    .analytics,
                expected
            );
            compare_execution(
                &db.execute_fnx_sealed_cooperative(
                    &cx,
                    &call,
                    opt,
                    memory(),
                    SealedLimits::default(),
                    quantum(7),
                )
                .await
                .unwrap()
                .analytics,
                &expected,
            );
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_distance_centralities_cancel_or_drop_at_every_checkpoint_and_suspend() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let root_before = db.read_session().unwrap().partition_root();
        for (direction, call) in [
            (Directedness::Undirected, call(false)),
            (Directedness::Undirected, call(true)),
            (
                Directedness::Undirected,
                FnxCallSpec::bind(
                    "CALL fnx.load_centrality() YIELD score AS rank,vertex AS id",
                    &FnxParameters::new(),
                )
                .unwrap(),
            ),
            (Directedness::Directed, call(false)),
            (Directedness::Directed, call(true)),
            (Directedness::Reversed, call(false)),
            (Directedness::Reversed, call(true)),
        ] {
            let opt = options(direction);
            let graph = projection(&db, &cx, opt).await;
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
            assert_eq!(expected.certificate.snapshot, graph.binding());
            assert_eq!(expected.certificate.projection_digest, graph.digest());
            if direction != Directedness::Undirected {
                // Count every reached live guard in the real opposite-face
                // constructor, raw pulls and scalar/output phases. The
                // original suspension and QueryCx sweeps below stay intact.
                let total_guards = Cell::new(0);
                let guarded = call
                    .execute_sealed_cooperative_with_checkpoint(
                        &cx,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(7),
                        yield_now,
                        || {
                            total_guards.set(total_guards.get() + 1);
                            Ok(())
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(guarded, expected);
                assert!(total_guards.get() > 0);
                for stop in 1..=total_guards.get() {
                    let seen = Cell::new(0);
                    let error = call
                        .execute_sealed_cooperative_with_checkpoint(
                            &cx,
                            &graph,
                            opt.execution_limits,
                            memory(),
                            quantum(7),
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
                    live_refusal(error, stop);
                    assert_eq!(seen.get(), stop);
                    assert_eq!(contexts.outstanding_obligations(), 0);
                    assert_eq!(db.read_session().unwrap().partition_root(), root_before);
                }
            }
            for stop in 1..=probe.calls() {
                let stop_probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&stop_probe));
                let (actual, _) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(7),
                        yield_now,
                    ),
                    &stop_probe,
                    7,
                );
                assert!(matches!(actual, Err(FnxSealedExecutionError::Cancelled(_))));
                assert_eq!(stop_probe.calls(), stop);
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
                    panic!("revocation must precede another source or scalar step");
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
                    yield_now,
                )
                .await
                .unwrap(),
                expected
            );
        }
    });
}

#[test]
fn compressed_distance_batches_preserve_native_scores_quanta_and_scalar_allowances() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let n = 129;
        let mut db = small_database(&contexts.commit(), n, false).await;
        let mut edges = WriteBatch::new(RelationId(1));
        // A relabelled hub puts source 0 at eccentricity two. Reciprocal
        // records exercise the reduced undirected compressed faces.
        for leaf in 0..n - 1 {
            edges.add_edge(
                EId((2 * leaf + 1) as u128),
                VId(leaf as u128),
                VId((n - 1) as u128),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(-7))],
            );
            edges.add_edge(
                EId((2 * leaf + 2) as u128),
                VId((n - 1) as u128),
                VId(leaf as u128),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(3))],
            );
        }
        db.write(&contexts.commit(), edges).await.unwrap();
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        assert_eq!(graph.node_count(), n);
        let scalar_bytes = n * (2 * size_of::<usize>() + size_of::<f64>());
        let batch_bytes = scalar_bytes + 3 * n * size_of::<u64>() + 128 * size_of::<usize>();
        for harmonic in [false, true] {
            let call = call(harmonic);
            let expected = call
                .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                .unwrap();
            assert_eq!(
                expected.certificate.execution_kernel,
                if harmonic {
                    "fgdb-prism/sealed-harmonic-batch64-v1"
                } else {
                    "fgdb-prism/sealed-closeness-batch64-v1"
                }
            );
            assert_eq!(expected.certificate.kernel_workspace_bytes, batch_bytes);
            assert_eq!(expected.certificate.witness.nodes_touched, n * n);
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
            let exact = FnxExecutionLimits {
                max_estimated_work: expected.certificate.estimated_work,
                ..opt.execution_limits
            };
            let mem = FnxMemoryLimits {
                max_kernel_workspace_bytes: batch_bytes,
                ..memory()
            };
            for q in [1, 7, 65] {
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let (actual, pending) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        exact,
                        mem,
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
                    if harmonic {
                        "fgdb-prism/sealed-harmonic-batch64-cooperative-v1"
                    } else {
                        "fgdb-prism/sealed-closeness-batch64-cooperative-v1"
                    }
                );
                assert!(pending > n / q);
            }
            // Both the original scalar minimum and a one-byte-short optional
            // allowance stay successful. Neither changes admission or bits.
            for bytes in [scalar_bytes, batch_bytes - 1] {
                let mem = FnxMemoryLimits {
                    max_kernel_workspace_bytes: bytes,
                    ..memory()
                };
                let scalar = call.execute_sealed(&cx, &graph, exact, mem).unwrap();
                assert_eq!(scalar.rows, expected.rows);
                assert_eq!(
                    scalar.certificate.result_digest,
                    expected.certificate.result_digest
                );
                assert_eq!(scalar.certificate.estimated_work, exact.max_estimated_work);
                assert_eq!(scalar.certificate.kernel_workspace_bytes, scalar_bytes);
                assert_eq!(
                    scalar.certificate.execution_kernel,
                    if harmonic {
                        "fgdb-prism/sealed-harmonic-fifo-v1"
                    } else {
                        "fgdb-prism/sealed-closeness-fifo-v1"
                    }
                );
                assert!(
                    expected.certificate.witness.edges_scanned * 4
                        < scalar.certificate.witness.edges_scanned
                );
                compare_execution(
                    &call
                        .execute_sealed_cooperative(&cx, &graph, exact, mem, quantum(65), yield_now)
                        .await
                        .unwrap(),
                    &scalar,
                );
            }
            let hosted = db
                .execute_fnx_sealed(&cx, &call, opt, mem, SealedLimits::default())
                .await
                .unwrap();
            assert_eq!(hosted.analytics, expected);
            let refused = FnxExecutionLimits {
                max_estimated_work: exact.max_estimated_work - 1,
                ..exact
            };
            assert!(matches!(
                call.execute_sealed(&cx, &graph, refused, mem),
                Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::LimitExceeded {
                        resource: "estimated work",
                        ..
                    }
                ))
            ));
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_distance_batches_stop_at_scalar_row_and_output_checkpoints() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let n = 65;
        let mut db = small_database(&contexts.commit(), n, false).await;
        let mut edges = WriteBatch::new(RelationId(1));
        for leaf in 1..n {
            edges.add_edge(
                EId(leaf as u128),
                VId(0),
                VId(leaf as u128),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
            );
        }
        db.write(&contexts.commit(), edges).await.unwrap();
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        let before_root = db.read_session().unwrap().partition_root();
        for harmonic in [false, true] {
            let call = call(harmonic);
            for q in [1, 7, 65] {
                let mut total = 0;
                let expected = call
                    .execute_sealed_cooperative_with_checkpoint(
                        &cx,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(q),
                        yield_now,
                        || {
                            total += 1;
                            Ok(())
                        },
                    )
                    .await
                    .unwrap();
                assert!(expected.certificate.execution_kernel.contains("batch64"));
                // Private tests sweep every raw/scalar boundary. Here spread
                // live-guard refusals over the real compressed source, word
                // setup, bit accumulation, scattering and result conversion.
                for stop in (1..=total).step_by((total / 31).max(1)).chain([total]) {
                    let mut seen = 0;
                    let error = call
                        .execute_sealed_cooperative_with_checkpoint(
                            &cx,
                            &graph,
                            opt.execution_limits,
                            memory(),
                            quantum(q),
                            yield_now,
                            || {
                                seen += 1;
                                if seen == stop {
                                    Err(SealedProjectionError::UnknownOrdinal(stop))
                                } else {
                                    Ok(())
                                }
                            },
                        )
                        .await
                        .unwrap_err();
                    assert!(matches!(
                        error,
                        FnxSealedExecutionError::Source(
                            SealedProjectionError::UnknownOrdinal(actual)
                        ) if actual == stop
                    ));
                    assert_eq!(seen, stop);
                }
                let cancelled = cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(
                    Some(total / 2),
                )));
                assert!(matches!(
                    call.execute_sealed_cooperative(
                        &cancelled,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(q),
                        yield_now,
                    )
                    .await,
                    Err(FnxSealedExecutionError::Cancelled(_))
                ));
                assert_eq!(
                    call.execute_sealed_cooperative(
                        &cx,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(q),
                        yield_now,
                    )
                    .await
                    .unwrap(),
                    expected
                );
                assert_eq!(db.read_session().unwrap().partition_root(), before_root);
                assert_eq!(contexts.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn directed_distance_centralities_preserve_incoming_scores_history_and_projection_identity() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (mut db, original) = database(&contexts.commit()).await;
        let mut change = WriteBatch::new(RelationId(1));
        change.create_vertex(VId(3), vec![LabelId(1)], vec![]);
        change.add_edge(
            EId(7),
            VId(2),
            VId(0),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(-4))],
        );
        db.write(&contexts.commit(), change).await.unwrap();
        let mut unrelated = WriteBatch::new(RelationId(2));
        unrelated.add_edge(
            EId(8),
            VId(0),
            VId(u128::MAX),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
        );
        db.write(&contexts.commit(), unrelated).await.unwrap();
        let current = db.frontier().unwrap();
        let view = db.read_session().unwrap();
        // Original arcs: 0->1, 1->0, 1->2, 2->MAX. The current cut adds
        // 2->0 and isolated vertex 3. Excluded label 99 and relation 2 must
        // not affect either orientation or expose excluded Bool weights.
        let third = 1.0 / 3.0;
        let four_ninths = (2.0 / 3.0) * (2.0 / 3.0);
        let harmonic_three = (1.0 + 0.5) + 1.0 / 3.0;
        for harmonic in [false, true] {
            let call = call(harmonic);
            assert_eq!(call.signature().graph_kind, FnxGraphKind::Any);
            assert_eq!(
                FnxReadOptions::whole_graph_for(&call, None)
                    .projection
                    .directedness,
                Directedness::Undirected
            );
            for direction in [Directedness::Directed, Directedness::Reversed] {
                for at in [original, current] {
                    let mut opt = options(direction);
                    opt.as_of = Some(at);
                    opt.projection.self_loops = SelfLoopPolicy::Drop;
                    let values = match (harmonic, direction, at == original) {
                        (false, Directedness::Directed, true) => {
                            vec![third, third, four_ninths, 0.5]
                        }
                        (false, Directedness::Reversed, true) => {
                            vec![0.5, 0.75, third, 0.0]
                        }
                        (false, Directedness::Directed, false) => {
                            vec![0.5, third, third, 0.0, 0.375]
                        }
                        (false, Directedness::Reversed, false) => {
                            vec![0.375, 0.5625, 0.5625, 0.0, 0.0]
                        }
                        (true, Directedness::Directed, true) => {
                            vec![1.0, 1.0, 1.5, harmonic_three]
                        }
                        (true, Directedness::Reversed, true) => {
                            vec![harmonic_three, 2.5, 1.0, 0.0]
                        }
                        (true, Directedness::Directed, false) => {
                            vec![2.0, 1.5, 1.5, 0.0, harmonic_three]
                        }
                        (true, Directedness::Reversed, false) => {
                            vec![harmonic_three, 2.5, 2.5, 0.0, 0.0]
                        }
                        _ => unreachable!(),
                    };
                    let ids = if at == original {
                        vec![0, 1, 2, u128::MAX]
                    } else {
                        vec![0, 1, 2, 3, u128::MAX]
                    };
                    let literal: Vec<_> = ids
                        .into_iter()
                        .zip(values)
                        .map(|(id, score)| vec![FnxValue::Score(score), FnxValue::Vertex(VId(id))])
                        .collect();
                    let graph = projection(&db, &cx, opt).await;
                    let expected = call
                        .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                        .unwrap();
                    assert_eq!(expected.rows, literal, "{direction:?}, {at:?}");
                    assert_eq!(expected.certificate.projection_digest, graph.digest());
                    assert_eq!(expected.certificate.snapshot, graph.binding());
                    let native = view.execute_fnx(&cx, &call, opt).unwrap().analytics;
                    assert_eq!(native.rows, literal);
                    assert_eq!(
                        native.certificate.result_digest,
                        expected.certificate.result_digest
                    );
                    let exact = expected.certificate.estimated_work;
                    assert!(matches!(
                        call.execute_sealed(
                            &cx,
                            &graph,
                            FnxExecutionLimits {
                                max_estimated_work: exact - 1,
                                ..opt.execution_limits
                            },
                            memory(),
                        ),
                        Err(FnxSealedExecutionError::Execution(
                            FnxExecutionError::LimitExceeded {
                                resource: "estimated work",
                                requested,
                                limit,
                            }
                        )) if requested == exact && limit == exact - 1
                    ));
                    for q in [1, 7] {
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
                        compare_execution(&actual.unwrap(), &expected);
                        assert!(pending > 0);
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
                    compare_execution(&hosted.analytics, &expected);
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
