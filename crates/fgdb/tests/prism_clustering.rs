//! Public Chronicle -> Strata -> Prism coverage for exact local topology CALLs.
//! The oracle is the independent decoded execution of the same admitted view.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{
    AdapterPath, Directedness, FnxAlgorithm, FnxCallSpec, FnxExecutionError, FnxExecutionLimits,
    FnxGraphKind, FnxMemoryLimits, FnxParameters, FnxReadOptions, FnxSealedExecutionError,
    FnxSealedReadError, FnxSelection, FnxSourceLimits, FnxValue, FnxWeightError, FnxWeightSpec,
    MissingWeightPolicy, ParallelEdgePolicy, ProjectionError, ProjectionLimits, ProjectionSpec,
    SealedGraphView, SealedProjectionError, SelfLoopPolicy,
};
use fgdb_strata::tiered::sealed::SealedLimits;
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::mem::size_of;
use std::sync::Arc;

const TEXTS: [&str; 2] = [
    "CALL fnx.triangles() YIELD triangles AS total,vertex AS id",
    "CALL fnx.clustering_coefficient() YIELD score AS coefficient,vertex AS id",
];
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn options() -> FnxReadOptions {
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
            directedness: Directedness::Undirected,
            parallel_edges: ParallelEdgePolicy::Minimum,
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
            max_adjacency_entries: 1000,
            max_workspace_bytes: 1 << 20,
        },
        execution_limits: FnxExecutionLimits {
            max_iterations: 0,
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
fn edge(batch: &mut WriteBatch, eid: u128, s: u128, t: u128, weight: i64) {
    batch.add_edge(
        EId(eid),
        VId(s),
        VId(t),
        vec![(PropertyKeyId(1), CanonicalScalar::Int(weight))],
    );
}
fn fixture() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for v in 0..=12 {
        batch.create_vertex(VId(v), vec![LabelId(1)], vec![]);
    }
    batch.create_vertex(VId(u128::MAX), vec![LabelId(1)], vec![]);
    batch.create_vertex(VId(99), vec![LabelId(2)], vec![]);
    for (id, s, t, w) in [
        (1, 0, 1, -7),
        (2, 1, 2, 0),
        (3, 2, 0, 9),
        (4, 0, 3, 2),
        (5, 3, 1, -4),
        (6, 1, 0, -3),
        (7, 0, 1, 5),
        (8, 0, 0, -1),
    ] {
        edge(&mut batch, id, s, t, w);
    }
    edge(&mut batch, 9, u128::MAX, u128::MAX, 2);
    for v in 4..=12 {
        edge(&mut batch, 100 + v, 0, v, 3);
    }
    // An excluded incoming/outgoing pair would add a third triangle at 1/2.
    for (id, s, t) in [(900, 99, 1), (901, 2, 99)] {
        batch.add_edge(
            EId(id),
            VId(s),
            VId(t),
            vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
        );
    }
    batch
}
fn small() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for v in [0, 1, 2, u128::MAX] {
        batch.create_vertex(VId(v), vec![LabelId(1)], vec![]);
    }
    for (id, s, t) in [(1, 0, 1), (2, 1, 2), (3, 2, 0)] {
        edge(&mut batch, id, s, t, 1);
    }
    batch
}
async fn projection(db: &Database<MemVfs>, cx: &QueryCx, opt: FnxReadOptions) -> SealedGraphView {
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
fn limit(error: FnxSealedExecutionError, resource: &'static str) {
    assert!(matches!(error, FnxSealedExecutionError::Execution(
        FnxExecutionError::LimitExceeded { resource: actual, .. }) if actual == resource));
}

#[test]
fn public_calls_agree_on_reciprocal_parallel_loop_and_hub_shapes() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        db.write(&contexts.commit(), fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        for parallel_edges in [
            ParallelEdgePolicy::Minimum,
            ParallelEdgePolicy::Maximum,
            ParallelEdgePolicy::Sum,
            ParallelEdgePolicy::CollapseUnit,
        ] {
            for self_loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                let mut opt = options();
                opt.projection.parallel_edges = parallel_edges;
                opt.projection.self_loops = self_loops;
                let graph = projection(&db, &cx, opt).await;
                assert!(graph.scan_incidence_bound() > 8);
                for text in TEXTS {
                    let call = FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
                    let expected = view.execute_fnx(&cx, &call, opt).unwrap().analytics;
                    let actual = call
                        .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                        .unwrap();
                    assert_eq!(actual.rows, expected.rows);
                    assert_eq!(actual.columns, expected.columns);
                    assert_eq!(
                        actual.certificate.result_digest,
                        expected.certificate.result_digest
                    );
                    assert_eq!(actual.certificate.adapter, AdapterPath::CompressedCursor);
                    assert_eq!(actual.certificate.vertices, 14);
                    assert_eq!(
                        actual,
                        db.call_fnx_sealed(
                            &cx,
                            text,
                            &FnxParameters::new(),
                            opt,
                            memory(),
                            SealedLimits::default()
                        )
                        .await
                        .unwrap()
                        .analytics
                    );
                    for row in &actual.rows {
                        let FnxValue::Vertex(VId(id)) = row[1] else {
                            panic!("native identity");
                        };
                        let count = match id {
                            0 | 1 => 2,
                            2 | 3 => 1,
                            _ => 0,
                        };
                        if matches!(call.algorithm(), FnxAlgorithm::Triangles) {
                            assert_eq!(row[0], FnxValue::Integer(count));
                        } else {
                            let expected: f64 = match id {
                                0 => 4.0 / 132.0,
                                1 => 4.0 / 6.0,
                                2 | 3 => 1.0,
                                _ => 0.0,
                            };
                            let FnxValue::Score(actual) = row[0] else {
                                panic!("coefficient");
                            };
                            assert_eq!(actual.to_bits(), expected.to_bits());
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn invisible_property_versions_are_charged_without_changing_historical_triangles() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        db.write(&contexts.commit(), small()).await.unwrap();
        let old = db.read_session().unwrap();
        let graph = projection(&db, &cx, options()).await;
        for value in 10..14 {
            let mut batch = WriteBatch::new(RelationId(1));
            batch.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(value)));
            db.write(&contexts.commit(), batch).await.unwrap();
        }
        let mut remove = WriteBatch::new(RelationId(1));
        remove.delete_edge(EId(3));
        db.write(&contexts.commit(), remove).await.unwrap();
        let historical = FnxReadOptions {
            as_of: Some(old.frontier()),
            ..options()
        };
        let history_graph = projection(&db, &cx, historical).await;
        assert!(history_graph.scan_incidence_bound() > graph.scan_incidence_bound());
        let live = projection(&db, &cx, options()).await;
        for text in TEXTS {
            let call = FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
            let original = call
                .execute_sealed(&cx, &graph, options().execution_limits, memory())
                .unwrap();
            let from_history = call
                .execute_sealed(&cx, &history_graph, options().execution_limits, memory())
                .unwrap();
            assert_eq!(original.rows, from_history.rows);
            assert_eq!(
                original.certificate.result_digest,
                from_history.certificate.result_digest
            );
            assert!(from_history.certificate.estimated_work > original.certificate.estimated_work);
            limit(
                call.execute_sealed(
                    &cx,
                    &history_graph,
                    FnxExecutionLimits {
                        max_estimated_work: original.certificate.estimated_work,
                        ..options().execution_limits
                    },
                    memory(),
                )
                .unwrap_err(),
                "estimated work",
            );
            let current = call
                .execute_sealed(&cx, &live, options().execution_limits, memory())
                .unwrap();
            assert!(
                current
                    .rows
                    .iter()
                    .all(|r| r[0] == FnxValue::Integer(0) || r[0] == FnxValue::Score(0.0))
            );
            assert_eq!(
                from_history.rows,
                db.read_session()
                    .unwrap()
                    .execute_fnx(&cx, &call, historical)
                    .unwrap()
                    .analytics
                    .rows
            );
        }
        drop(db);
        for text in TEXTS {
            let call = FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
            assert_eq!(
                call.execute_sealed(&cx, &graph.clone(), options().execution_limits, memory())
                    .unwrap()
                    .rows,
                old.execute_fnx(&cx, &call, options())
                    .unwrap()
                    .analytics
                    .rows
            );
        }
    });
}

#[test]
fn exact_kernel_work_rows_and_result_bytes_are_independent_admissions() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        db.write(&contexts.commit(), small()).await.unwrap();
        let graph = projection(&db, &cx, options()).await;
        for text in TEXTS {
            let call = FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
            let expected = call
                .execute_sealed(&cx, &graph, options().execution_limits, memory())
                .unwrap();
            let columns = call.outputs().len() * size_of::<String>()
                + call.outputs().iter().map(|c| c.name.len()).sum::<usize>();
            let result_bytes = columns
                + graph.node_count()
                    * (size_of::<Vec<FnxValue>>() + call.outputs().len() * size_of::<FnxValue>());
            let exact = FnxExecutionLimits {
                max_iterations: 0,
                max_result_rows: graph.node_count(),
                max_estimated_work: expected.certificate.estimated_work,
            };
            let mem = FnxMemoryLimits {
                max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
                max_result_bytes: result_bytes,
            };
            assert_eq!(
                call.execute_sealed(&cx, &graph, exact, mem).unwrap(),
                expected
            );
            limit(
                call.execute_sealed(
                    &cx,
                    &graph,
                    FnxExecutionLimits {
                        max_result_rows: exact.max_result_rows - 1,
                        ..exact
                    },
                    mem,
                )
                .unwrap_err(),
                "result rows",
            );
            limit(
                call.execute_sealed(
                    &cx,
                    &graph,
                    FnxExecutionLimits {
                        max_estimated_work: exact.max_estimated_work - 1,
                        ..exact
                    },
                    mem,
                )
                .unwrap_err(),
                "estimated work",
            );
            limit(
                call.execute_sealed(
                    &cx,
                    &graph,
                    exact,
                    FnxMemoryLimits {
                        max_kernel_workspace_bytes: mem.max_kernel_workspace_bytes - 1,
                        ..mem
                    },
                )
                .unwrap_err(),
                "kernel workspace bytes",
            );
            limit(
                call.execute_sealed(
                    &cx,
                    &graph,
                    exact,
                    FnxMemoryLimits {
                        max_result_bytes: result_bytes - 1,
                        ..mem
                    },
                )
                .unwrap_err(),
                "result bytes",
            );
        }
    });
}

#[test]
fn graph_kind_and_induced_selection_remain_authoritative_for_topology_calls() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        db.write(&contexts.commit(), fixture()).await.unwrap();
        for text in TEXTS {
            for direction in [Directedness::Directed, Directedness::Reversed] {
                let mut opt = options();
                opt.projection.directedness = direction;
                opt.source_limits.max_work_units = 0;
                assert!(matches!(
                    db.call_fnx_sealed(
                        &cx,
                        text,
                        &FnxParameters::new(),
                        opt,
                        memory(),
                        SealedLimits {
                            max_image_bytes: 0,
                            ..SealedLimits::default()
                        }
                    )
                    .await,
                    Err(FnxSealedReadError::Execution(
                        FnxSealedExecutionError::Execution(FnxExecutionError::GraphKind {
                            required: FnxGraphKind::Undirected
                        })
                    ))
                ));
            }
            let mut opt = options();
            opt.selection.vertex_label = None;
            assert!(matches!(
                db.call_fnx_sealed(
                    &cx,
                    text,
                    &FnxParameters::new(),
                    opt,
                    memory(),
                    SealedLimits::default()
                )
                .await,
                Err(FnxSealedReadError::Projection(
                    SealedProjectionError::Weight {
                        reason: FnxWeightError::NotNumeric,
                        ..
                    }
                ))
            ));
            opt.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
            let unmasked = db
                .call_fnx_sealed(
                    &cx,
                    text,
                    &FnxParameters::new(),
                    opt,
                    memory(),
                    SealedLimits::default(),
                )
                .await
                .unwrap();
            let extra = unmasked
                .analytics
                .rows
                .iter()
                .find(|r| r[1] == FnxValue::Vertex(VId(99)))
                .unwrap();
            assert!(extra[0] == FnxValue::Integer(1) || extra[0] == FnxValue::Score(1.0));
            let affected = unmasked
                .analytics
                .rows
                .iter()
                .find(|r| r[1] == FnxValue::Vertex(VId(1)))
                .unwrap();
            assert!(affected[0] == FnxValue::Integer(3) || affected[0] == FnxValue::Score(0.5));
        }
        let mut opt = options();
        opt.projection.parallel_edges = ParallelEdgePolicy::Reject;
        assert!(matches!(
            db.call_fnx_sealed(
                &cx,
                TEXTS[0],
                &FnxParameters::new(),
                opt,
                memory(),
                SealedLimits::default()
            )
            .await,
            Err(FnxSealedReadError::Projection(
                SealedProjectionError::Projection(ProjectionError::ParallelEdge { .. })
            ))
        ));
        opt.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        opt.projection.self_loops = SelfLoopPolicy::Reject;
        assert!(matches!(
            db.call_fnx_sealed(
                &cx,
                TEXTS[0],
                &FnxParameters::new(),
                opt,
                memory(),
                SealedLimits::default()
            )
            .await,
            Err(FnxSealedReadError::Projection(
                SealedProjectionError::Projection(ProjectionError::SelfLoop(_))
            ))
        ));
    });
}

#[test]
fn cancellation_at_every_measured_kernel_and_database_stage_returns_no_prefix() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        db.write(&contexts.commit(), small()).await.unwrap();
        let graph = projection(&db, &cx, options()).await;
        let root_id = db.read_session().unwrap().partition_root();
        for text in TEXTS {
            let call = FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
            let expected = call
                .execute_sealed(&observed, &graph, options().execution_limits, memory())
                .unwrap();
            for stop in 1..=probe.calls() {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                assert!(matches!(
                    call.execute_sealed(&controlled, &graph, options().execution_limits, memory()),
                    Err(FnxSealedExecutionError::Cancelled(_))
                ));
                assert_eq!(probe.calls(), stop);
            }
            assert_eq!(
                expected,
                call.execute_sealed(&cx, &graph, options().execution_limits, memory())
                    .unwrap()
            );
        }
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
        db.call_fnx_sealed(
            &observed,
            TEXTS[0],
            &FnxParameters::new(),
            options(),
            memory(),
            SealedLimits::default(),
        )
        .await
        .unwrap();
        for stop in 1..=probe.calls() {
            let controlled =
                cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(stop))));
            assert!(
                db.call_fnx_sealed(
                    &controlled,
                    TEXTS[0],
                    &FnxParameters::new(),
                    options(),
                    memory(),
                    SealedLimits::default()
                )
                .await
                .is_err()
            );
            assert_eq!(db.read_session().unwrap().partition_root(), root_id);
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn empty_and_loop_only_sources_return_zero_counts_without_losing_isolates() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut opt = options();
        opt.execution_limits.max_result_rows = 0;
        opt.execution_limits.max_estimated_work = 0;
        let mem = FnxMemoryLimits {
            max_kernel_workspace_bytes: 0,
            ..memory()
        };
        for text in TEXTS {
            assert!(
                db.call_fnx_sealed(
                    &cx,
                    text,
                    &FnxParameters::new(),
                    opt,
                    mem,
                    SealedLimits::default()
                )
                .await
                .unwrap()
                .analytics
                .rows
                .is_empty()
            );
        }
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(u128::MAX), vec![LabelId(1)], vec![]);
        edge(&mut batch, 1, u128::MAX, u128::MAX, 2);
        db.write(&contexts.commit(), batch).await.unwrap();
        for text in TEXTS {
            let result = db
                .call_fnx_sealed(
                    &cx,
                    text,
                    &FnxParameters::new(),
                    options(),
                    memory(),
                    SealedLimits::default(),
                )
                .await
                .unwrap()
                .analytics;
            assert_eq!(result.rows.len(), 1);
            assert_eq!(result.rows[0][1], FnxValue::Vertex(VId(u128::MAX)));
            assert!(
                result.rows[0][0] == FnxValue::Integer(0)
                    || result.rows[0][0] == FnxValue::Score(0.0)
            );
        }
    });
}
