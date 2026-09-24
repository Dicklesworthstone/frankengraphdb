//! Public Chronicle -> Strata -> Prism checks. No fabricated source anchors.
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::*;
use fgdb_strata::tiered::sealed::{SealedError, SealedLimits};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::mem::size_of;
use std::sync::Arc;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}
fn options(direction: Directedness) -> FnxReadOptions {
    FnxReadOptions {
        as_of: None,
        selection: FnxSelection {
            vertex_label: Some(LabelId(1)),
            relation: Some(RelationId(1)),
            weight: FnxWeightSpec::Property {
                key: PropertyKeyId(1),
                missing: MissingWeightPolicy::Reject,
            },
        },
        projection: ProjectionSpec {
            directedness: direction,
            parallel_edges: ParallelEdgePolicy::Sum,
            self_loops: SelfLoopPolicy::Keep,
        },
        source_limits: FnxSourceLimits {
            max_work_units: 100_000,
            max_scratch_entries: 10_000,
            max_staging_bytes: 1 << 20,
        },
        projection_limits: ProjectionLimits {
            max_vertices: 100,
            max_input_edges: 1000,
            max_adjacency_entries: 2000,
            max_workspace_bytes: 1 << 20,
        },
        execution_limits: FnxExecutionLimits {
            max_iterations: 1000,
            max_result_rows: 100,
            max_estimated_work: 1 << 26,
        },
    }
}
fn memory() -> FnxMemoryLimits {
    FnxMemoryLimits {
        max_kernel_workspace_bytes: 1 << 20,
        max_result_bytes: 1 << 20,
    }
}
fn fixture() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for v in [1, 2, 3, 4, u128::MAX] {
        batch.create_vertex(VId(v), vec![LabelId(1)], vec![]);
    }
    batch.create_vertex(VId(99), vec![LabelId(2)], vec![]);
    // Canonical sum 1 + 2^53 + 1 = 2^53. Summing outgoing and incoming groups
    // separately changes one face to 2^53 + 2 and destroys symmetry.
    for (eid, s, t, weight) in [
        (1, 1, 2, 1),
        (2, 2, 1, 1i64 << 53),
        (3, 1, 2, 1),
        (4, 2, 3, 0),
        (5, 3, 4, 2),
        (6, 4, 2, 1),
        (7, 2, 2, 3),
        (8, 4, u128::MAX, 4),
    ] {
        batch.add_edge(
            EId(eid),
            VId(s),
            VId(t),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(weight))],
        );
    }
    // More than eight raw incidences forces the existing compressed CSR tier.
    for eid in 100..110 {
        batch.add_edge(
            EId(eid),
            VId(1),
            VId(3),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
        );
    }
    for (eid, s, t) in [(900, 99, 2), (901, 3, 99)] {
        batch.add_edge(
            EId(eid),
            VId(s),
            VId(t),
            vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
        );
    }
    batch
}
async fn project(db: &Database<MemVfs>, cx: &QueryCx, opt: FnxReadOptions) -> SealedGraphView {
    db.prism_sealed_projection_at(
        cx,
        opt.as_of,
        opt.selection,
        opt.projection,
        opt.projection_limits,
        opt.source_limits,
        SealedLimits::default(),
    )
    .await
    .unwrap()
}
fn row(
    graph: &SealedGraphView,
    cx: &QueryCx,
    source: usize,
    lower: Option<VId>,
) -> Vec<(usize, u64)> {
    let mut cursor = graph.neighbor_cursor_from(cx, source, lower).unwrap();
    let mut values = Vec::new();
    while let Some((target, weight)) = cursor.next(cx).unwrap() {
        values.push((target, weight.to_bits()));
    }
    assert!(cursor.next(cx).unwrap().is_none());
    values
}
fn call(text: &str) -> FnxCallSpec {
    FnxCallSpec::bind(text, &FnxParameters::new()).unwrap()
}
const RANK: &str = "CALL fnx.pagerank(0.85,1000,1e-10,true) YIELD score,vertex";
const BFS: &str = "CALL fnx.single_source_shortest_path_length(1) YIELD vertex,distance";
const CC: &str = "CALL fnx.connected_components() YIELD vertex,component";
const WCC: &str = "CALL fnx.weakly_connected_components() YIELD vertex,component";
const SCC: &str = "CALL fnx.strongly_connected_components() YIELD vertex,component";

#[test]
fn all_directions_reductions_loops_and_full_width_seeks_match_decoded_rows() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            for policy in [
                ParallelEdgePolicy::Sum,
                ParallelEdgePolicy::Minimum,
                ParallelEdgePolicy::Maximum,
                ParallelEdgePolicy::CollapseUnit,
            ] {
                for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                    let mut opt = options(direction);
                    opt.projection.parallel_edges = policy;
                    opt.projection.self_loops = loops;
                    let graph = project(&db, &query, opt).await;
                    let oracle = view
                        .prism_projection_at(
                            &query,
                            view.frontier(),
                            opt.selection,
                            opt.projection,
                            opt.projection_limits,
                            opt.source_limits,
                        )
                        .unwrap();
                    assert_eq!(graph.vertex_ids(), oracle.vertex_ids());
                    assert_eq!(graph.edge_count(), oracle.edge_count());
                    assert_eq!(graph.input_edge_count(), 18); // original selected EIds, not faces
                    let mut arcs = 0;
                    for source in 0..graph.node_count() {
                        let (targets, weights) = oracle.projected_row(source).unwrap();
                        let expected: Vec<_> = targets
                            .iter()
                            .copied()
                            .zip(weights.iter().map(|w| w.to_bits()))
                            .collect();
                        arcs += targets.len();
                        assert_eq!(
                            row(&graph, &query, source, None),
                            expected,
                            "{direction:?} {policy:?} {loops:?} source={source}"
                        );
                        assert_eq!(graph.degree(source), Some(targets.len()));
                        for lower in [VId(0), VId(1), VId(2), VId(3), VId(5), VId(u128::MAX)] {
                            let suffix: Vec<_> = expected
                                .iter()
                                .copied()
                                .filter(|(index, _)| graph.vertex_id(*index).unwrap() >= lower)
                                .collect();
                            assert_eq!(row(&graph, &query, source, Some(lower)), suffix);
                        }
                    }
                    assert_eq!(graph.adjacency_entry_count(), arcs);
                    assert_eq!(
                        graph.incoming_index_stats().is_some(),
                        direction != Directedness::Directed
                    );
                    if direction == Directedness::Undirected && policy == ParallelEdgePolicy::Sum {
                        assert_eq!(
                            row(&graph, &query, 0, None)[0],
                            (1, ((1u64 << 53) as f64).to_bits())
                        );
                        assert_eq!(
                            row(&graph, &query, 1, None)[0],
                            (0, ((1u64 << 53) as f64).to_bits())
                        );
                    }
                }
            }
        }
    });
}

#[test]
fn registered_procedures_reach_directional_images_from_database_and_reusable_projection() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let opt = options(direction);
            let graph = project(&db, &query, opt).await;
            let mut texts = vec![
                RANK,
                BFS,
                "CALL fnx.single_source_shortest_path_length(4,0) YIELD distance,vertex",
                "CALL fnx.single_source_dijkstra_path_length(1,NULL,true) YIELD vertex,distance",
                "CALL fnx.single_source_dijkstra_path_length(4,3,false) YIELD distance,vertex",
            ];
            if direction == Directedness::Undirected {
                texts.push(CC);
            } else {
                texts.extend([WCC, SCC]);
            }
            for text in texts {
                let call = call(text);
                let expected = view.execute_fnx(&query, &call, opt).unwrap();
                let output = db
                    .execute_fnx_sealed(&query, &call, opt, memory(), SealedLimits::default())
                    .await
                    .unwrap();
                assert_eq!(output.analytics.columns, expected.analytics.columns);
                assert_eq!(
                    output.analytics.rows, expected.analytics.rows,
                    "{direction:?} {text}"
                );
                assert_eq!(
                    output.analytics.certificate.result_digest,
                    expected.analytics.certificate.result_digest
                );
                assert_eq!(
                    output.analytics.certificate.adapter,
                    AdapterPath::CompressedCursor
                );
                assert_eq!(output.analytics.certificate.edges, graph.edge_count());
                assert_eq!(
                    output.analytics,
                    call.execute_sealed(&query, &graph, opt.execution_limits, memory())
                        .unwrap()
                );
                if text == CC {
                    assert_eq!(
                        output.analytics.certificate.execution_kernel,
                        "fgdb-prism/sealed-union-find-v1"
                    );
                    assert!(
                        output
                            .analytics
                            .rows
                            .iter()
                            .all(|r| r[1] == FnxValue::Vertex(VId(1)))
                    );
                    assert_eq!(
                        output.analytics.rows.last().unwrap()[0],
                        FnxValue::Vertex(VId(u128::MAX))
                    );
                }
            }
        }
    });
}

#[test]
fn graph_kind_preflight_refuses_before_source_budgets_or_incoming_construction() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit(); let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        for (text, direction, required) in [
            (CC, Directedness::Directed, FnxGraphKind::Undirected),
            (CC, Directedness::Reversed, FnxGraphKind::Undirected),
            (WCC, Directedness::Undirected, FnxGraphKind::Directed),
            (SCC, Directedness::Undirected, FnxGraphKind::Directed),
        ] {
            let mut opt = options(direction);
            opt.source_limits.max_work_units = 0; opt.projection_limits.max_workspace_bytes = 0;
            assert!(matches!(db.execute_fnx_sealed(&query, &call(text), opt, memory(), SealedLimits::default()).await,
                Err(FnxSealedReadError::Execution(FnxSealedExecutionError::Execution(
                    FnxExecutionError::GraphKind { required: actual }))) if actual == required));
            let graph = project(&db, &query, options(direction)).await;
            assert!(matches!(call(text).execute_sealed(&query, &graph, opt.execution_limits, memory()),
                Err(FnxSealedExecutionError::Execution(FnxExecutionError::GraphKind { required: actual }))
                    if actual == required));
        }
        // These kernels are still unimplemented on compressed rows. New graph
        // directions must not implicitly expose them or fall back to decoding.
        for text in ["CALL fnx.triangles()", "CALL fnx.clustering_coefficient()"] {
            assert!(matches!(call(text).validate_sealed_projection(Directedness::Undirected),
                Err(FnxSealedExecutionError::UnsupportedAlgorithm(_))));
        }
    });
}

#[test]
fn incoming_build_peak_and_undirected_arcs_are_admitted_not_just_simple_edges() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        for direction in [Directedness::Reversed, Directedness::Undirected] {
            let mut opt = options(direction);
            let graph = project(&db, &query, opt).await;
            let stats = graph.incoming_index_stats().unwrap();
            let directory = graph.node_count() * (size_of::<VId>() + size_of::<usize>());
            assert_eq!(
                graph.charged_workspace_bytes(),
                directory + stats.charged_workspace_bytes
            );
            assert!(stats.charged_workspace_bytes > stats.charged_resident_bytes);
            assert_eq!(stats.incidences, 20); // includes both excluded endpoints
            opt.projection_limits.max_workspace_bytes = graph.charged_workspace_bytes();
            assert_eq!(project(&db, &query, opt).await.digest(), graph.digest());
            opt.projection_limits.max_workspace_bytes -= 1;
            assert!(matches!(
                db.prism_sealed_projection_at(
                    &query,
                    None,
                    opt.selection,
                    opt.projection,
                    opt.projection_limits,
                    opt.source_limits,
                    SealedLimits::default()
                )
                .await,
                Err(FnxSealedReadError::Projection(SealedProjectionError::Read(
                    SealedError::Limit { .. }
                )))
            ));
            opt = options(direction);
            opt.projection_limits.max_adjacency_entries = graph.adjacency_entry_count();
            project(&db, &query, opt).await;
            opt.projection_limits.max_adjacency_entries -= 1;
            assert!(matches!(
                db.prism_sealed_projection_at(
                    &query,
                    None,
                    opt.selection,
                    opt.projection,
                    opt.projection_limits,
                    opt.source_limits,
                    SealedLimits::default()
                )
                .await,
                Err(FnxSealedReadError::Projection(
                    SealedProjectionError::Projection(ProjectionError::LimitExceeded {
                        resource: "adjacency entries",
                        ..
                    })
                ))
            ));
            opt = options(direction);
            opt.projection_limits.max_input_edges = 19;
            assert!(matches!(
                db.prism_sealed_projection_at(
                    &query,
                    None,
                    opt.selection,
                    opt.projection,
                    opt.projection_limits,
                    opt.source_limits,
                    SealedLimits::default()
                )
                .await,
                Err(FnxSealedReadError::Projection(
                    SealedProjectionError::Projection(ProjectionError::LimitExceeded {
                        resource: "source incidences",
                        ..
                    })
                ))
            ));
            if direction == Directedness::Undirected {
                assert_eq!(graph.adjacency_entry_count(), 2 * graph.edge_count() - 1);
            }
            // H is the physical retained population. The executor charges both
            // faces separately; do not change this source bound into an arc count.
            assert_eq!(graph.scan_incidence_bound(), 20);
            let output = call(BFS)
                .execute_sealed(&query, &graph, opt.execution_limits, memory())
                .unwrap();
            let exact = FnxExecutionLimits {
                max_estimated_work: output.certificate.estimated_work,
                ..opt.execution_limits
            };
            assert_eq!(
                call(BFS)
                    .execute_sealed(&query, &graph, exact, memory())
                    .unwrap(),
                output
            );
            assert!(matches!(
                call(BFS).execute_sealed(
                    &query,
                    &graph,
                    FnxExecutionLimits {
                        max_estimated_work: exact.max_estimated_work - 1,
                        ..exact
                    },
                    memory()
                ),
                Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::LimitExceeded {
                        resource: "estimated work",
                        ..
                    }
                ))
            ));
        }
    });
}

#[test]
fn reciprocal_eids_are_parallel_but_a_self_loop_is_never_its_own_parallel_edge() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let mut initial = WriteBatch::new(RelationId(1));
        for v in [1, 2] {
            initial.create_vertex(VId(v), vec![LabelId(1)], vec![]);
        }
        initial.add_edge(
            EId(1),
            VId(1),
            VId(1),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(3))],
        );
        db.write(&commit, initial).await.unwrap();
        let mut opt = options(Directedness::Undirected);
        opt.projection.parallel_edges = ParallelEdgePolicy::Reject;
        let graph = project(&db, &query, opt).await;
        assert_eq!(row(&graph, &query, 0, None), vec![(0, 3.0f64.to_bits())]);
        assert_eq!(
            (
                graph.edge_count(),
                graph.input_edge_count(),
                graph.adjacency_entry_count()
            ),
            (1, 1, 1)
        );
        let mut update = WriteBatch::new(RelationId(1));
        update.delete_edge(EId(1));
        for (eid, s, t) in [(2, 1, 2), (3, 2, 1)] {
            update.add_edge(
                EId(eid),
                VId(s),
                VId(t),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(2))],
            );
        }
        db.write(&commit, update).await.unwrap();
        opt.projection.self_loops = SelfLoopPolicy::Reject;
        for direction in [Directedness::Directed, Directedness::Reversed] {
            opt.projection.directedness = direction;
            assert_eq!(project(&db, &query, opt).await.edge_count(), 2);
        }
        opt.projection.directedness = Directedness::Undirected;
        assert!(matches!(
            db.prism_sealed_projection_at(
                &query,
                None,
                opt.selection,
                opt.projection,
                opt.projection_limits,
                opt.source_limits,
                SealedLimits::default()
            )
            .await,
            Err(FnxSealedReadError::Projection(
                SealedProjectionError::Projection(ProjectionError::ParallelEdge { .. })
            ))
        ));
    });
}

#[test]
fn historical_directional_projections_remain_immutable_after_writer_progress_and_drop() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let old = db.read_session().unwrap();
        let mut pinned = Vec::new();
        for direction in [Directedness::Reversed, Directedness::Undirected] {
            let opt = options(direction);
            let graph = project(&db, &query, opt).await;
            let expected = call(BFS)
                .execute_sealed(&query, &graph, opt.execution_limits, memory())
                .unwrap();
            pinned.push((direction, graph, expected));
        }
        let mut update = WriteBatch::new(RelationId(1));
        update.set_edge_property(EId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(4)));
        update.delete_vertex(VId(4));
        db.write(&commit, update).await.unwrap();
        let latest = db.read_session().unwrap();
        for (direction, graph, expected) in &pinned {
            let mut opt = options(*direction);
            opt.as_of = Some(old.frontier());
            let historical = project(&db, &query, opt).await;
            assert_ne!(historical.binding().root, graph.binding().root);
            assert_eq!(historical.vertex_ids(), graph.vertex_ids());
            for source in 0..graph.node_count() {
                assert_eq!(
                    row(&historical, &query, source, None),
                    row(graph, &query, source, None)
                );
            }
            assert_eq!(
                call(BFS)
                    .execute_sealed(&query, &historical, opt.execution_limits, memory())
                    .unwrap()
                    .rows,
                expected.rows
            );
            opt.as_of = None;
            let current = project(&db, &query, opt).await;
            assert_eq!(current.node_count(), 4);
            assert_eq!(
                call(BFS)
                    .execute_sealed(&query, &current, opt.execution_limits, memory())
                    .unwrap()
                    .rows,
                latest
                    .execute_fnx(&query, &call(BFS), opt)
                    .unwrap()
                    .analytics
                    .rows
            );
        }
        drop(db);
        drop(latest);
        drop(old);
        for (direction, graph, expected) in pinned {
            assert_eq!(
                call(BFS)
                    .execute_sealed(
                        &query,
                        &graph.clone(),
                        options(direction).execution_limits,
                        memory()
                    )
                    .unwrap(),
                expected
            );
        }
    });
}

#[test]
fn every_directional_checkpoint_refuses_atomically_and_failed_row_cursors_are_fused() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for v in 1..=3 {
            batch.create_vertex(VId(v), vec![LabelId(1)], vec![]);
        }
        for (id, s, t) in [(1, 1, 2), (2, 2, 1), (3, 2, 3)] {
            batch.add_edge(
                EId(id),
                VId(s),
                VId(t),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let before = db.read_session().unwrap();
        for direction in [Directedness::Reversed, Directedness::Undirected] {
            let opt = options(direction);
            let procedure = call(if direction == Directedness::Undirected {
                CC
            } else {
                WCC
            });
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let measured = query.with_checkpoint_probe(Arc::clone(&probe));
            let expected = db
                .execute_fnx_sealed(
                    &measured,
                    &procedure,
                    opt,
                    memory(),
                    SealedLimits::default(),
                )
                .await
                .unwrap();
            let total = probe.calls();
            assert!(total > 20);
            for stop in 1..=total {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
                assert!(
                    db.execute_fnx_sealed(
                        &controlled,
                        &procedure,
                        opt,
                        memory(),
                        SealedLimits::default()
                    )
                    .await
                    .is_err()
                );
                assert_eq!(probe.calls(), stop);
                assert_eq!(
                    db.read_session().unwrap().partition_root(),
                    before.partition_root()
                );
            }
            assert_eq!(
                db.execute_fnx_sealed(&query, &procedure, opt, memory(), SealedLimits::default())
                    .await
                    .unwrap(),
                expected
            );
            let graph = project(&db, &query, opt).await;
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let measured = query.with_checkpoint_probe(Arc::clone(&probe));
            let mut cursor = graph.neighbor_cursor(&query, 1).unwrap();
            while cursor.next(&measured).unwrap().is_some() {}
            for stop in 1..=probe.calls() {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
                let mut cursor = graph.neighbor_cursor(&query, 1).unwrap();
                loop {
                    match cursor.next(&controlled) {
                        Ok(Some(_)) => {}
                        Ok(None) => panic!("cancellation became EOF"),
                        Err(SealedProjectionError::Read(SealedError::Interrupted(_))) => break,
                        Err(error) => panic!("wrong error: {error:?}"),
                    }
                }
                assert_eq!(probe.calls(), stop);
                assert!(cursor.next(&query).unwrap().is_none());
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn empty_images_and_isolates_work_in_every_direction_without_invented_sources() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let mut opt = options(direction);
            opt.projection_limits = ProjectionLimits {
                max_vertices: 0,
                max_input_edges: 0,
                max_adjacency_entries: 0,
                max_workspace_bytes: 0,
            };
            let graph = project(&db, &query, opt).await;
            assert_eq!(graph.charged_workspace_bytes(), 0);
            assert!(
                call(RANK)
                    .execute_sealed(&query, &graph, opt.execution_limits, memory())
                    .unwrap()
                    .rows
                    .is_empty()
            );
            assert!(matches!(
                graph.neighbor_cursor(&query, usize::MAX),
                Err(SealedProjectionError::UnknownOrdinal(_))
            ));
        }
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(u128::MAX), vec![LabelId(1)], vec![]);
        db.write(&commit, batch).await.unwrap();
        let graph = project(&db, &query, options(Directedness::Undirected)).await;
        let output = call(CC)
            .execute_sealed(
                &query,
                &graph,
                options(Directedness::Undirected).execution_limits,
                memory(),
            )
            .unwrap();
        assert_eq!(
            output.rows,
            vec![vec![
                FnxValue::Vertex(VId(u128::MAX)),
                FnxValue::Vertex(VId(u128::MAX))
            ]]
        );
    });
}
