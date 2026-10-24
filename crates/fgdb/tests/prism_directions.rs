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

fn exact_sealed_limits(
    procedure: &FnxCallSpec,
    query: &QueryCx,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    expected: &FnxResult,
) {
    let result_bytes = procedure.outputs().len() * size_of::<String>()
        + procedure
            .outputs()
            .iter()
            .map(|column| column.name.len())
            .sum::<usize>()
        + expected.rows.len()
            * (size_of::<Vec<FnxValue>>() + procedure.outputs().len() * size_of::<FnxValue>());
    let exact = FnxExecutionLimits {
        max_estimated_work: expected.certificate.estimated_work,
        max_result_rows: expected.rows.len(),
        ..limits
    };
    let memory = FnxMemoryLimits {
        max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
        max_result_bytes: result_bytes,
    };
    assert_eq!(
        procedure
            .execute_sealed(query, graph, exact, memory)
            .unwrap(),
        *expected
    );
    for (limits, memory) in [
        (
            FnxExecutionLimits {
                max_estimated_work: exact.max_estimated_work - 1,
                ..exact
            },
            memory,
        ),
        (
            FnxExecutionLimits {
                max_result_rows: exact.max_result_rows - 1,
                ..exact
            },
            memory,
        ),
        (
            exact,
            FnxMemoryLimits {
                max_kernel_workspace_bytes: memory.max_kernel_workspace_bytes - 1,
                ..memory
            },
        ),
        (
            exact,
            FnxMemoryLimits {
                max_result_bytes: memory.max_result_bytes - 1,
                ..memory
            },
        ),
    ] {
        assert!(
            procedure
                .execute_sealed(query, graph, limits, memory)
                .is_err()
        );
    }
}

#[test]
fn sealed_cdlp_matches_decoded_calls_across_history_directions_and_reductions() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys()).await.unwrap();
        let original = db.write(&commit, fixture()).await.unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.create_vertex(VId(40), vec![LabelId(1)], vec![]);
        update.delete_edge(EId(6));
        update.add_edge(EId(3000), VId(1), VId(u128::MAX),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7))]);
        let current = db.write(&commit, update).await.unwrap();
        let view = db.read_session().unwrap();
        for at in [original, current] {
            for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
                for reduction in [ParallelEdgePolicy::Sum, ParallelEdgePolicy::Minimum,
                    ParallelEdgePolicy::Maximum, ParallelEdgePolicy::CollapseUnit] {
                    let mut opt = options(direction);
                    opt.as_of = Some(at);
                    opt.projection.parallel_edges = reduction;
                    opt.projection.self_loops = SelfLoopPolicy::Drop;
                    let graph = project(&db, &query, opt).await;
                    for iterations in [0, 1, 2, 5] {
                        let text = format!("CALL fnx.cdlp(max_iter => {iterations}) YIELD component AS community,vertex AS node");
                        let procedure = call(&text);
                        assert!(procedure.supports_sealed_execution());
                        let expected = view.execute_fnx(&query, &procedure, opt).unwrap();
                        let actual = procedure.execute_sealed(&query, &graph, opt.execution_limits, memory()).unwrap();
                        assert_eq!(actual.columns, expected.analytics.columns);
                        assert_eq!(actual.rows, expected.analytics.rows,
                            "{at:?} {direction:?} {reduction:?} {iterations}");
                        assert_eq!(actual.certificate.result_digest, expected.analytics.certificate.result_digest);
                        assert_eq!(actual.certificate.adapter, AdapterPath::CompressedCursor);
                        assert_eq!(actual.certificate.snapshot, graph.binding());
                        assert_eq!(actual.rows.len(), graph.node_count());
                        if iterations == 0 {
                            for row in &actual.rows {
                                assert_eq!(row[0], row[1], "zero rounds retain each stable VId");
                            }
                        }
                        exact_sealed_limits(&procedure, &query, &graph, opt.execution_limits, &actual);
                        if iterations == 2 && reduction == ParallelEdgePolicy::Sum {
                            let hosted = db.call_fnx_sealed(&query, &text, &FnxParameters::new(),
                                opt, memory(), SealedLimits::default()).await.unwrap();
                            assert_eq!(hosted.analytics, actual);
                        }
                    }
                }
                let opt = options(direction);
                let graph = project(&db, &query, opt).await;
                // The registered loop refusal applies even with zero rounds.
                for iterations in [0, 2] {
                    assert!(matches!(FnxCallSpec::cdlp(iterations).execute_sealed(
                        &query, &graph, opt.execution_limits, memory()),
                        Err(FnxSealedExecutionError::Execution(FnxExecutionError::SelfLoopRefused))));
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn sealed_cdlp_auxiliary_face_and_output_guards_preserve_the_source_after_refusal() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let mut opt = options(Directedness::Directed);
        opt.projection.self_loops = SelfLoopPolicy::Drop;
        let graph = project(&db, &query, opt).await;
        let procedure = FnxCallSpec::cdlp(2);
        let before = graph.digest();
        let steps = std::cell::Cell::new(0usize);
        let expected = procedure
            .execute_sealed_with_checkpoint(&query, &graph, opt.execution_limits, memory(), || {
                steps.set(steps.get() + 1);
                Ok(())
            })
            .unwrap();
        let total = steps.get();
        assert!(total > graph.input_edge_count());
        // Every boundary includes incoming-index build, parallel-edge reduction,
        // iteration and final public-row encoding. A refusal publishes no rows.
        for stop in 1..=total {
            steps.set(0);
            assert!(
                procedure
                    .execute_sealed_with_checkpoint(
                        &query,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        || {
                            steps.set(steps.get() + 1);
                            if steps.get() == stop {
                                Err(SealedProjectionError::UnknownOrdinal(usize::MAX))
                            } else {
                                Ok(())
                            }
                        }
                    )
                    .is_err(),
                "guard at {stop}/{total} must prevent publication"
            );
            assert_eq!(steps.get(), stop);
            assert_eq!(graph.digest(), before);
        }
        assert_eq!(
            procedure
                .execute_sealed(&query, &graph, opt.execution_limits, memory())
                .unwrap(),
            expected
        );
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

const DIRECTED_LCC: &str =
    "CALL fnx.directed_clustering_coefficient() YIELD score AS coefficient,vertex AS node";

#[test]
fn sealed_directed_clustering_matches_decoded_history_reductions_and_exact_limits() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let original = db.write(&commit, fixture()).await.unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.create_vertex(VId(40), vec![LabelId(1)], vec![]);
        update.delete_edge(EId(6));
        update.set_edge_property(EId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(4)));
        update.add_edge(
            EId(3000),
            VId(1),
            VId(u128::MAX),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
        );
        let current = db.write(&commit, update).await.unwrap();
        let view = db.read_session().unwrap();
        let procedure = call(DIRECTED_LCC);
        assert!(procedure.supports_sealed_execution());
        for at in [original, current] {
            for direction in [Directedness::Directed, Directedness::Reversed] {
                for reduction in [
                    ParallelEdgePolicy::Sum,
                    ParallelEdgePolicy::Minimum,
                    ParallelEdgePolicy::Maximum,
                    ParallelEdgePolicy::CollapseUnit,
                ] {
                    for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                        let mut opt = options(direction);
                        opt.as_of = Some(at);
                        opt.projection.parallel_edges = reduction;
                        opt.projection.self_loops = loops;
                        let graph = project(&db, &query, opt).await;
                        assert!(graph.scan_incidence_bound() > 8);
                        let expected = view.execute_fnx(&query, &procedure, opt).unwrap();
                        let actual = procedure
                            .execute_sealed(&query, &graph, opt.execution_limits, memory())
                            .unwrap();
                        assert_eq!(actual.columns, expected.analytics.columns);
                        assert_eq!(
                            actual.rows, expected.analytics.rows,
                            "{at:?} {direction:?} {reduction:?} {loops:?}"
                        );
                        assert_eq!(
                            actual.certificate.result_digest,
                            expected.analytics.certificate.result_digest
                        );
                        assert_eq!(actual.certificate.adapter, AdapterPath::CompressedCursor);
                        assert_eq!(actual.certificate.snapshot, graph.binding());
                        assert_eq!(actual.certificate.snapshot.as_of, at);
                        assert_eq!(
                            actual.certificate.execution_kernel,
                            "fgdb-prism/sealed-directed-clustering-retained-cost-v1"
                        );
                        assert_eq!(actual.rows.len(), graph.node_count());
                        if at == current {
                            assert!(
                                actual.rows.contains(&vec![
                                    FnxValue::Score(0.0),
                                    FnxValue::Vertex(VId(40)),
                                ])
                            );
                        }
                        exact_sealed_limits(
                            &procedure,
                            &query,
                            &graph,
                            opt.execution_limits,
                            &actual,
                        );
                        if reduction == ParallelEdgePolicy::Sum {
                            let hosted = db
                                .call_fnx_sealed(
                                    &query,
                                    DIRECTED_LCC,
                                    &FnxParameters::new(),
                                    opt,
                                    memory(),
                                    SealedLimits::default(),
                                )
                                .await
                                .unwrap();
                            assert_eq!(hosted.analytics, actual);
                        }
                    }
                }
            }
        }

        // A missing relation retains the selected population as isolates.
        // It is distinct from an empty vertex selection, which has no rows.
        for direction in [Directedness::Directed, Directedness::Reversed] {
            let mut opt = options(direction);
            opt.selection.relation = Some(RelationId(999));
            let graph = project(&db, &query, opt).await;
            assert_eq!(graph.scan_incidence_bound(), 0);
            assert_eq!(graph.node_count(), 6);
            let expected = view.execute_fnx(&query, &procedure, opt).unwrap();
            let isolated = procedure
                .execute_sealed(&query, &graph, opt.execution_limits, memory())
                .unwrap();
            assert_eq!(isolated.rows, expected.analytics.rows);
            assert_eq!(
                isolated.certificate.result_digest,
                expected.analytics.certificate.result_digest
            );
            assert!(
                isolated
                    .rows
                    .iter()
                    .all(|row| row[0] == FnxValue::Score(0.0))
            );
            exact_sealed_limits(&procedure, &query, &graph, opt.execution_limits, &isolated);

            opt.selection.relation = Some(RelationId(1));
            opt.selection.vertex_label = Some(LabelId(999));
            opt.execution_limits.max_estimated_work = 0;
            opt.execution_limits.max_result_rows = 0;
            let graph = project(&db, &query, opt).await;
            assert_eq!(graph.node_count(), 0);
            assert!(graph.scan_incidence_bound() > 8);
            let result_bytes = procedure.outputs().len() * size_of::<String>()
                + procedure
                    .outputs()
                    .iter()
                    .map(|column| column.name.len())
                    .sum::<usize>();
            let empty_memory = FnxMemoryLimits {
                max_kernel_workspace_bytes: 0,
                max_result_bytes: result_bytes,
            };
            // Retained source history must not trigger an auxiliary incoming
            // index or a copied vertex directory when no vertex is selected.
            let empty = procedure
                .execute_sealed(&query, &graph, opt.execution_limits, empty_memory)
                .unwrap();
            assert!(empty.rows.is_empty());
            assert_eq!(empty.certificate.snapshot, graph.binding());
            assert_eq!(empty.certificate.estimated_work, 0);
            assert_eq!(empty.certificate.kernel_workspace_bytes, 0);
            assert_eq!(empty.certificate.witness.nodes_touched, 0);
            assert_eq!(empty.certificate.witness.edges_scanned, 0);
            assert!(matches!(
                procedure.execute_sealed(
                    &query,
                    &graph,
                    opt.execution_limits,
                    FnxMemoryLimits {
                        max_result_bytes: result_bytes - 1,
                        ..empty_memory
                    },
                ),
                Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::LimitExceeded {
                        resource: "result bytes",
                        ..
                    }
                ))
            ));
            let hosted = db
                .call_fnx_sealed(
                    &query,
                    DIRECTED_LCC,
                    &FnxParameters::new(),
                    opt,
                    empty_memory,
                    SealedLimits::default(),
                )
                .await
                .unwrap();
            assert_eq!(hosted.analytics, empty);
        }

        let mut invalid = options(Directedness::Undirected);
        let graph = project(&db, &query, invalid).await;
        invalid.source_limits.max_work_units = 0;
        invalid.projection_limits.max_workspace_bytes = 0;
        invalid.execution_limits.max_estimated_work = 0;
        invalid.execution_limits.max_result_rows = 0;
        let no_memory = FnxMemoryLimits {
            max_kernel_workspace_bytes: 0,
            max_result_bytes: 0,
        };
        assert!(matches!(
            procedure.execute_sealed(&query, &graph, invalid.execution_limits, no_memory),
            Err(FnxSealedExecutionError::Execution(
                FnxExecutionError::GraphKind {
                    required: FnxGraphKind::Directed
                }
            ))
        ));
        assert!(matches!(
            db.call_fnx_sealed(
                &query,
                DIRECTED_LCC,
                &FnxParameters::new(),
                invalid,
                no_memory,
                SealedLimits::default(),
            )
            .await,
            Err(FnxSealedReadError::Execution(
                FnxSealedExecutionError::Execution(FnxExecutionError::GraphKind {
                    required: FnxGraphKind::Directed
                })
            ))
        ));
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn sealed_directed_clustering_live_guards_preserve_both_faces_after_every_refusal() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let procedure = call(DIRECTED_LCC);
        for direction in [Directedness::Directed, Directedness::Reversed] {
            let opt = options(direction);
            let graph = project(&db, &query, opt).await;
            let before = graph.digest();
            let binding = graph.binding();
            let steps = std::cell::Cell::new(0usize);
            let expected = procedure
                .execute_sealed_with_checkpoint(
                    &query,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    || {
                        steps.set(steps.get() + 1);
                        Ok(())
                    },
                )
                .unwrap();
            let total = steps.get();
            assert!(total > graph.scan_incidence_bound());
            // Interrupt each auxiliary-projection, raw-cursor, scalar-count,
            // and final row-encoding boundary, with no public result prefix.
            for stop in 1..=total {
                steps.set(0);
                assert!(
                    procedure
                        .execute_sealed_with_checkpoint(
                            &query,
                            &graph,
                            opt.execution_limits,
                            memory(),
                            || {
                                steps.set(steps.get() + 1);
                                if steps.get() == stop {
                                    Err(SealedProjectionError::UnknownOrdinal(usize::MAX))
                                } else {
                                    Ok(())
                                }
                            },
                        )
                        .is_err(),
                    "{direction:?}: guard at {stop}/{total} must prevent publication"
                );
                assert_eq!(steps.get(), stop);
                assert_eq!(graph.digest(), before);
                assert_eq!(graph.binding(), binding);
            }
            assert_eq!(
                procedure
                    .execute_sealed(&query, &graph, opt.execution_limits, memory())
                    .unwrap(),
                expected
            );
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

const BETWEENNESS: &str = "CALL fnx.betweenness_centrality() YIELD score,vertex";

#[test]
fn sealed_betweenness_matches_decoded_history_reductions_and_exact_limits() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let original = db.write(&commit, fixture()).await.unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.create_vertex(VId(40), vec![LabelId(1)], vec![]);
        update.delete_edge(EId(6));
        update.set_edge_property(EId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(4)));
        update.add_edge(
            EId(3000),
            VId(1),
            VId(u128::MAX),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
        );
        let current = db.write(&commit, update).await.unwrap();
        let view = db.read_session().unwrap();
        let procedure = call(BETWEENNESS);
        assert!(procedure.supports_sealed_execution());
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
                let graph = project(&db, &query, opt).await;
                assert!(graph.scan_incidence_bound() > 8);
                let expected = view.execute_fnx(&query, &procedure, opt).unwrap();
                let actual = procedure
                    .execute_sealed(&query, &graph, opt.execution_limits, memory())
                    .unwrap();
                assert_eq!(actual.columns, expected.analytics.columns);
                assert_eq!(actual.rows, expected.analytics.rows, "{at:?} {reduction:?}");
                for (actual, expected) in actual.rows.iter().zip(&expected.analytics.rows) {
                    match (&actual[0], &expected[0]) {
                        (FnxValue::Score(actual), FnxValue::Score(expected)) => {
                            assert_eq!(actual.to_bits(), expected.to_bits());
                        }
                        _ => panic!("betweenness must return a score in its first column"),
                    }
                }
                assert_eq!(
                    actual.certificate.result_digest,
                    expected.analytics.certificate.result_digest
                );
                assert!(actual.rows.iter().any(|row| {
                    matches!(row.first(), Some(FnxValue::Score(score)) if *score > 0.0)
                }));
                assert_eq!(
                    actual.rows.last().unwrap()[1],
                    FnxValue::Vertex(VId(u128::MAX))
                );
                assert_eq!(actual.rows.len(), graph.node_count());
                if at == current {
                    assert!(
                        actual
                            .rows
                            .contains(&vec![FnxValue::Score(0.0), FnxValue::Vertex(VId(40)),])
                    );
                }
                assert_eq!(actual.certificate.adapter, AdapterPath::CompressedCursor);
                assert_eq!(actual.certificate.snapshot, graph.binding());
                assert_eq!(actual.certificate.snapshot.as_of, at);
                assert_eq!(
                    actual.certificate.execution_kernel,
                    "fgdb-prism/sealed-betweenness-brandes-v1"
                );
                exact_sealed_limits(&procedure, &query, &graph, opt.execution_limits, &actual);
                let hosted = db
                    .call_fnx_sealed(
                        &query,
                        BETWEENNESS,
                        &FnxParameters::new(),
                        opt,
                        memory(),
                        SealedLimits::default(),
                    )
                    .await
                    .unwrap();
                assert_eq!(hosted.analytics, actual);

                // The catalog refuses a retained loop even though weights
                // are ignored and every non-loop parallel group is reduced.
                opt.projection.self_loops = SelfLoopPolicy::Keep;
                let with_loop = project(&db, &query, opt).await;
                assert!(matches!(
                    procedure.execute_sealed(&query, &with_loop, opt.execution_limits, memory(),),
                    Err(FnxSealedExecutionError::Execution(
                        FnxExecutionError::SelfLoopRefused
                    ))
                ));
                assert!(matches!(
                    db.call_fnx_sealed(
                        &query,
                        BETWEENNESS,
                        &FnxParameters::new(),
                        opt,
                        memory(),
                        SealedLimits::default(),
                    )
                    .await,
                    Err(FnxSealedReadError::Execution(
                        FnxSealedExecutionError::Execution(FnxExecutionError::SelfLoopRefused)
                    ))
                ));
            }
        }

        // An empty selected population still binds the authenticated image.
        // Derive the exact work ceiling from the kernel's public certificate:
        // scalar admission and output metadata may have a fixed nonzero cost.
        let mut opt = options(Directedness::Undirected);
        opt.projection.self_loops = SelfLoopPolicy::Drop;
        opt.selection.vertex_label = Some(LabelId(999));
        opt.execution_limits.max_result_rows = 0;
        let graph = project(&db, &query, opt).await;
        assert_eq!(graph.node_count(), 0);
        assert!(graph.scan_incidence_bound() > 8);
        let decoded = view.execute_fnx(&query, &procedure, opt).unwrap();
        let empty = procedure
            .execute_sealed(&query, &graph, opt.execution_limits, memory())
            .unwrap();
        assert_eq!(empty.columns, decoded.analytics.columns);
        assert_eq!(empty.rows, decoded.analytics.rows);
        assert!(empty.rows.is_empty());
        assert_eq!(
            empty.certificate.result_digest,
            decoded.analytics.certificate.result_digest
        );
        assert_eq!(empty.certificate.snapshot, graph.binding());
        assert_eq!(empty.certificate.kernel_workspace_bytes, 0);
        assert_eq!(empty.certificate.witness.nodes_touched, 0);
        assert_eq!(empty.certificate.witness.edges_scanned, 0);
        let result_bytes = procedure.outputs().len() * size_of::<String>()
            + procedure
                .outputs()
                .iter()
                .map(|column| column.name.len())
                .sum::<usize>();
        opt.execution_limits.max_estimated_work = empty.certificate.estimated_work;
        let empty_memory = FnxMemoryLimits {
            max_kernel_workspace_bytes: 0,
            max_result_bytes: result_bytes,
        };
        assert_eq!(
            procedure
                .execute_sealed(&query, &graph, opt.execution_limits, empty_memory)
                .unwrap(),
            empty
        );
        if opt.execution_limits.max_estimated_work != 0 {
            let mut short = opt.execution_limits;
            short.max_estimated_work -= 1;
            assert!(matches!(
                procedure.execute_sealed(&query, &graph, short, empty_memory),
                Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::LimitExceeded {
                        resource: "estimated work",
                        ..
                    }
                ))
            ));
        }
        assert!(matches!(
            procedure.execute_sealed(
                &query,
                &graph,
                opt.execution_limits,
                FnxMemoryLimits {
                    max_result_bytes: result_bytes - 1,
                    ..empty_memory
                },
            ),
            Err(FnxSealedExecutionError::Execution(
                FnxExecutionError::LimitExceeded {
                    resource: "result bytes",
                    ..
                }
            ))
        ));
        let hosted = db
            .call_fnx_sealed(
                &query,
                BETWEENNESS,
                &FnxParameters::new(),
                opt,
                empty_memory,
                SealedLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(hosted.analytics, empty);

        for direction in [Directedness::Directed, Directedness::Reversed] {
            let mut invalid = options(direction);
            invalid.projection.self_loops = SelfLoopPolicy::Drop;
            let graph = project(&db, &query, invalid).await;
            invalid.source_limits.max_work_units = 0;
            invalid.projection_limits.max_workspace_bytes = 0;
            invalid.execution_limits.max_estimated_work = 0;
            invalid.execution_limits.max_result_rows = 0;
            let no_memory = FnxMemoryLimits {
                max_kernel_workspace_bytes: 0,
                max_result_bytes: 0,
            };
            assert!(matches!(
                procedure.execute_sealed(&query, &graph, invalid.execution_limits, no_memory),
                Err(FnxSealedExecutionError::Execution(
                    FnxExecutionError::GraphKind {
                        required: FnxGraphKind::Undirected
                    }
                ))
            ));
            assert!(matches!(
                db.call_fnx_sealed(
                    &query,
                    BETWEENNESS,
                    &FnxParameters::new(),
                    invalid,
                    no_memory,
                    SealedLimits::default(),
                )
                .await,
                Err(FnxSealedReadError::Execution(
                    FnxSealedExecutionError::Execution(FnxExecutionError::GraphKind {
                        required: FnxGraphKind::Undirected
                    })
                ))
            ));
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn sealed_betweenness_live_guards_preserve_the_source_after_every_refusal() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let mut opt = options(Directedness::Undirected);
        opt.projection.self_loops = SelfLoopPolicy::Drop;
        let graph = project(&db, &query, opt).await;
        let procedure = call(BETWEENNESS);
        let before = graph.digest();
        let binding = graph.binding();
        let steps = std::cell::Cell::new(0usize);
        let expected = procedure
            .execute_sealed_with_checkpoint(&query, &graph, opt.execution_limits, memory(), || {
                steps.set(steps.get() + 1);
                Ok(())
            })
            .unwrap();
        assert!(
            expected
                .rows
                .iter()
                .any(|row| { matches!(row.first(), Some(FnxValue::Score(score)) if *score > 0.0) })
        );
        let total = steps.get();
        assert!(total > graph.scan_incidence_bound());
        // Cover initialization, each source BFS, reverse dependencies,
        // source-ordered accumulation, normalization and public row encoding.
        for stop in 1..=total {
            steps.set(0);
            assert!(
                procedure
                    .execute_sealed_with_checkpoint(
                        &query,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        || {
                            steps.set(steps.get() + 1);
                            if steps.get() == stop {
                                Err(SealedProjectionError::UnknownOrdinal(usize::MAX))
                            } else {
                                Ok(())
                            }
                        },
                    )
                    .is_err(),
                "guard at {stop}/{total} must prevent publication"
            );
            assert_eq!(steps.get(), stop);
            assert_eq!(graph.digest(), before);
            assert_eq!(graph.binding(), binding);
        }
        assert_eq!(
            procedure
                .execute_sealed(&query, &graph, opt.execution_limits, memory())
                .unwrap(),
            expected
        );
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

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
        // 0c285a92 implemented these kernels on compressed rows for undirected
        // projections only. Other directions must refuse on graph kind, never
        // silently serve a directed projection or fall back to decoding.
        for text in ["CALL fnx.triangles()", "CALL fnx.clustering_coefficient()"] {
            assert!(call(text).validate_sealed_projection(Directedness::Undirected).is_ok());
            for direction in [Directedness::Directed, Directedness::Reversed] {
                assert!(matches!(call(text).validate_sealed_projection(direction),
                    Err(FnxSealedExecutionError::Execution(FnxExecutionError::GraphKind {
                        required: FnxGraphKind::Undirected }))));
            }
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
            let mut cold_total = 0;
            // Keep full cold construction coverage and separately exercise
            // every live checkpoint on a proven retained-cache hit.
            for retained in [false, true] {
                db.set_read_cache_limits(
                    &query,
                    fgdb::ReadCacheLimits {
                        sealed_projection_bytes: if retained {
                            fgdb::ReadCacheLimits::default().sealed_projection_bytes
                        } else {
                            0
                        },
                        ..fgdb::ReadCacheLimits::default()
                    },
                )
                .unwrap();
                if retained {
                    db.execute_fnx_sealed(
                        &query,
                        &procedure,
                        opt,
                        memory(),
                        SealedLimits::default(),
                    )
                    .await
                    .unwrap();
                    assert!(
                        db.read_cache_status(&query)
                            .unwrap()
                            .sealed_projections
                            .entries
                            > 0
                    );
                }
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
                if retained {
                    assert!(total > 0 && total < cold_total);
                } else {
                    assert!(total > 20);
                    cold_total = total;
                }
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
                    db.execute_fnx_sealed(
                        &query,
                        &procedure,
                        opt,
                        memory(),
                        SealedLimits::default()
                    )
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

const DEGREE: &str = "CALL fnx.degree_centrality() YIELD score AS rank, vertex AS node";

#[test]
fn sealed_degree_directory_preserves_reductions_history_pins_and_exact_limits() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let original = db.write(&commit, fixture()).await.unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.create_vertex(VId(40), vec![LabelId(1)], vec![]);
        update.delete_edge(EId(6));
        update.add_edge(
            EId(3000),
            VId(1),
            VId(u128::MAX),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
        );
        let current = db.write(&commit, update).await.unwrap();
        let view = db.read_session().unwrap();
        let procedure = call(DEGREE);
        assert!(procedure.supports_sealed_execution());
        assert!(procedure.supports_cooperative_sealed_execution());
        let mut retained = Vec::new();
        for at in [original, current] {
            for reduction in [
                ParallelEdgePolicy::Sum,
                ParallelEdgePolicy::Minimum,
                ParallelEdgePolicy::CollapseUnit,
            ] {
                let mut opt = options(Directedness::Undirected);
                opt.as_of = Some(at);
                opt.projection.parallel_edges = reduction;
                opt.projection.self_loops = SelfLoopPolicy::Drop;
                let expected = view.execute_fnx(&query, &procedure, opt).unwrap();
                let graph = project(&db, &query, opt).await;
                let n = graph.node_count();
                let workspace = n * size_of::<f64>();
                let work = n + 2;
                let result_bytes = procedure.outputs().len() * size_of::<String>()
                    + procedure
                        .outputs()
                        .iter()
                        .map(|column| column.name.len())
                        .sum::<usize>()
                    + n * (size_of::<Vec<FnxValue>>()
                        + procedure.outputs().len() * size_of::<FnxValue>());
                let exact_memory = FnxMemoryLimits {
                    max_kernel_workspace_bytes: workspace,
                    max_result_bytes: result_bytes,
                };
                let mut exact_limits = opt.execution_limits;
                exact_limits.max_result_rows = n;
                exact_limits.max_estimated_work = work;
                // Historical raw edges and their incoming locators were already
                // admitted by projection. The degree kernel does not scan them.
                assert!(graph.scan_incidence_bound() > n);
                let actual = procedure
                    .execute_sealed(&query, &graph, exact_limits, exact_memory)
                    .unwrap();
                assert_eq!(actual.columns, expected.analytics.columns);
                assert_eq!(actual.rows, expected.analytics.rows);
                assert_eq!(
                    actual.certificate.result_digest,
                    expected.analytics.certificate.result_digest
                );
                assert_eq!(actual.certificate.adapter, AdapterPath::CompressedCursor);
                assert_eq!(
                    actual.certificate.execution_kernel,
                    "fgdb-prism/sealed-degree-directory-v1"
                );
                assert_eq!(actual.certificate.kernel_workspace_bytes, workspace);
                assert_eq!(actual.certificate.estimated_work, work);
                assert_eq!(actual.certificate.witness.nodes_touched, n);
                assert_eq!(actual.certificate.witness.edges_scanned, 0);
                assert!(
                    !actual
                        .certificate
                        .witness
                        .complexity_claim
                        .contains("incoming")
                );
                for resource in [
                    "result rows",
                    "estimated work",
                    "kernel workspace bytes",
                    "result bytes",
                ] {
                    let mut limits = exact_limits;
                    let mut memory = exact_memory;
                    match resource {
                        "result rows" => limits.max_result_rows -= 1,
                        "estimated work" => limits.max_estimated_work -= 1,
                        "kernel workspace bytes" => memory.max_kernel_workspace_bytes -= 1,
                        "result bytes" => memory.max_result_bytes -= 1,
                        _ => unreachable!(),
                    }
                    assert!(
                        matches!(
                            procedure.execute_sealed(&query, &graph, limits, memory),
                            Err(FnxSealedExecutionError::Execution(
                                FnxExecutionError::LimitExceeded { resource: observed, .. }
                            )) if observed == resource
                        ),
                        "{resource}"
                    );
                }
                // The native stored-source entrypoint admits this procedure too.
                let native = db
                    .call_fnx_sealed(
                        &query,
                        DEGREE,
                        &FnxParameters::new(),
                        opt,
                        exact_memory,
                        SealedLimits::default(),
                    )
                    .await
                    .unwrap();
                assert_eq!(native.analytics.rows, actual.rows);
                assert_eq!(
                    native.analytics.certificate.result_digest,
                    actual.certificate.result_digest
                );
                retained.push((graph, exact_limits, exact_memory, actual));
            }
        }
        drop(view);
        db.compact(&commit).await.unwrap();
        drop(db);
        for (graph, limits, memory, expected) in retained {
            assert_eq!(
                procedure
                    .execute_sealed(&query, &graph, limits, memory)
                    .unwrap(),
                expected
            );
        }
    });
}

#[test]
fn sealed_degree_empty_singleton_and_retained_loops_match_the_catalog_contract() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let procedure = call(DEGREE);
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let mut opt = options(Directedness::Undirected);
        opt.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        let graph = project(&db, &query, opt).await;
        let empty = procedure
            .execute_sealed(&query, &graph, opt.execution_limits, memory())
            .unwrap();
        assert!(empty.rows.is_empty());
        assert_eq!(empty.certificate.kernel_workspace_bytes, 0);
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(u128::MAX), vec![LabelId(1)], vec![]);
        db.write(&commit, batch).await.unwrap();
        let graph = project(&db, &query, opt).await;
        let singleton = procedure
            .execute_sealed(&query, &graph, opt.execution_limits, memory())
            .unwrap();
        assert_eq!(
            singleton.rows,
            vec![vec![FnxValue::Score(1.0), FnxValue::Vertex(VId(u128::MAX))]]
        );
        // Parallel self-loops reduce to one retained loop, not an even pair
        // of non-loop incidences that could evade the degree metadata check.
        let mut loops = WriteBatch::new(RelationId(1));
        for eid in 1..=2 {
            loops.add_edge(EId(eid), VId(u128::MAX), VId(u128::MAX), vec![]);
        }
        db.write(&commit, loops).await.unwrap();
        let graph = project(&db, &query, opt).await;
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(graph.adjacency_entry_count(), 1);
        assert!(matches!(
            procedure.execute_sealed(&query, &graph, opt.execution_limits, memory()),
            Err(FnxSealedExecutionError::Execution(
                FnxExecutionError::SelfLoopRefused
            ))
        ));
        opt.projection.self_loops = SelfLoopPolicy::Drop;
        let dropped = project(&db, &query, opt).await;
        assert_eq!(
            procedure
                .execute_sealed(&query, &dropped, opt.execution_limits, memory())
                .unwrap()
                .rows,
            singleton.rows
        );
    });
}

#[test]
fn retained_endpoint_directory_matches_tight_workspace_fallback_in_every_direction() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let mut batch = fixture();
        let extra: Vec<_> = (1..=513u128).map(|id| VId((id << 118) | id)).collect();
        for &vertex in &extra {
            batch.create_vertex(vertex, vec![LabelId(1)], vec![]);
        }
        for (index, (source, target)) in [
            (VId(1), extra[0]),
            (extra[0], extra[256]),
            (extra[256], extra[512]),
            (extra[512], VId(u128::MAX)),
        ]
        .into_iter()
        .enumerate()
        {
            batch.add_edge(
                EId(2000 + index as u128),
                source,
                target,
                vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let mut opt = options(direction);
            opt.projection_limits.max_vertices = 1024;
            opt.execution_limits.max_result_rows = 1024;
            let cached = project(&db, &query, opt).await;
            assert_eq!(cached.node_count(), 518);
            // The fixture's excluded endpoints have invalid Bool weights.
            // Successful construction must exclude them before weight binding.
            assert_eq!(cached.input_edge_count(), 22);
            let mandatory_bytes = cached.node_count() * (size_of::<VId>() + size_of::<usize>())
                + cached
                    .incoming_index_stats()
                    .map_or(0, |stats| stats.charged_workspace_bytes);
            assert!(cached.charged_workspace_bytes() > mandatory_bytes);
            opt.projection_limits.max_workspace_bytes = mandatory_bytes;
            let fallback = project(&db, &query, opt).await;
            assert_eq!(fallback.charged_workspace_bytes(), mandatory_bytes);
            assert_eq!(cached.binding(), fallback.binding());
            assert_eq!(cached.digest(), fallback.digest());
            assert_eq!(cached.vertex_ids(), fallback.vertex_ids());
            assert_eq!(cached.edge_count(), fallback.edge_count());
            assert_eq!(cached.input_edge_count(), fallback.input_edge_count());
            assert_eq!(
                cached.adjacency_entry_count(),
                fallback.adjacency_entry_count()
            );
            assert_eq!(
                cached.scan_incidence_bound(),
                fallback.scan_incidence_bound()
            );
            for (ordinal, &vertex) in cached.vertex_ids().iter().enumerate() {
                assert_eq!(cached.vertex_ordinal(vertex), Some(ordinal));
                assert_eq!(fallback.vertex_ordinal(vertex), Some(ordinal));
                assert_eq!(cached.degree(ordinal), fallback.degree(ordinal));
                for lower in [None, Some(extra[256])] {
                    assert_eq!(
                        row(&cached, &query, ordinal, lower),
                        row(&fallback, &query, ordinal, lower),
                        "{direction:?} source={ordinal} lower={lower:?}"
                    );
                }
            }
            for absent in [VId(0), VId(99), VId(extra[256].0 ^ (1u128 << 127))] {
                assert_eq!(cached.vertex_ordinal(absent), None);
                assert_eq!(fallback.vertex_ordinal(absent), None);
            }
            // Exercise source lookup and repeated compressed passes, including
            // all isolates, with identical rows and complete certificates.
            for text in [BFS, RANK] {
                let call = call(text);
                assert_eq!(
                    call.execute_sealed(&query, &cached, opt.execution_limits, memory())
                        .unwrap(),
                    call.execute_sealed(&query, &fallback, opt.execution_limits, memory())
                        .unwrap(),
                    "{direction:?} {text}"
                );
            }
        }
    });
}
