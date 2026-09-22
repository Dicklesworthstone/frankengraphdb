//! Real Chronicle -> admitted read view -> authenticated Strata image -> CALL.
//! Private store access below only obtains an image through the actual sealer;
//! no test fabricates an anchor, root receipt, vertex directory or graph source.

#[path = "query_prism_source_tests.rs"]
mod source_admission;
#[path = "query_prism_direction_tests.rs"]
mod direction;

use super::*;
use crate::{DatabaseKeys, DatabaseState, MemVfs, WriteBatch};
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{
    AdapterPath, FnxArgument, FnxExecutionError, FnxExecutionLimits, FnxValue, FnxWeightError,
    FnxWeightSpec, MissingWeightPolicy,
};
use fgdb_strata::tiered::sealed::RowStorageKind;
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::sync::Arc;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
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
            directedness: Directedness::Directed,
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
    for vertex in 1..=12 {
        batch.create_vertex(VId(vertex), vec![LabelId(1)], vec![]);
    }
    batch.create_vertex(VId(u128::MAX), vec![LabelId(1)], vec![]); // full-width isolated identity
    batch.create_vertex(VId(99), vec![LabelId(2)], vec![]);
    for (eid, weight) in [(100, 8), (101, 2), (102, 5)] {
        batch.add_edge(
            EId(eid),
            VId(1),
            VId(2),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(weight))],
        );
    }
    for target in 3..=10 {
        batch.add_edge(
            EId(200 + target),
            VId(1),
            VId(target),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(20 + target as i64))],
        );
    }
    for (eid, source, target, weight) in [
        (300, 2, 3, 0),
        (301, 3, 2, 0),
        (302, 3, 4, 1),
        (303, 4, 5, 1),
    ] {
        batch.add_edge(
            EId(eid),
            VId(source),
            VId(target),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(weight))],
        );
    }
    // Must be excluded by the induced vertex selection before numeric binding.
    batch.add_edge(
        EId(999),
        VId(10),
        VId(99),
        vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
    );
    batch
}
fn small_fixture() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for vertex in 1..=3 {
        batch.create_vertex(VId(vertex), vec![LabelId(1)], vec![]);
    }
    batch.add_edge(
        EId(1),
        VId(1),
        VId(2),
        vec![(PropertyKeyId(1), CanonicalScalar::Int(2))],
    );
    batch.add_edge(
        EId(2),
        VId(2),
        VId(3),
        vec![(PropertyKeyId(1), CanonicalScalar::Int(0))],
    );
    batch
}
fn parameters() -> FnxParameters {
    [("source".to_owned(), FnxArgument::Vertex(VId(1)))]
        .into_iter()
        .collect()
}
const DIJKSTRA: &str =
    "CALL fnx.single_source_dijkstra_path_length($source,NULL,true) YIELD vertex,distance";
fn dijkstra_call() -> FnxCallSpec {
    FnxCallSpec::bind(DIJKSTRA, &parameters()).unwrap()
}

#[test]
fn native_database_and_pinned_images_agree_for_all_five_compressed_procedures() {
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
        let image = db
            .store
            .seal_partition(
                &query,
                view.partition_root(),
                view.frontier(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            image.storage_kind(VId(1), RelationId(1)),
            Some(RowStorageKind::SealedCsr)
        );
        assert_eq!(
            image.storage_kind(VId(2), RelationId(1)),
            Some(RowStorageKind::Inline)
        );
        for policy in [
            ParallelEdgePolicy::Minimum,
            ParallelEdgePolicy::Maximum,
            ParallelEdgePolicy::Sum,
            ParallelEdgePolicy::CollapseUnit,
        ] {
            let mut options = options();
            options.projection.parallel_edges = policy;
            let prepared = db
                .prism_sealed_projection_at(
                    &query,
                    None,
                    options.selection,
                    options.projection,
                    options.projection_limits,
                    options.source_limits,
                    SealedLimits::default(),
                )
                .await
                .unwrap();
            for text in [
                DIJKSTRA,
                "CALL fnx.single_source_shortest_path_length($source) YIELD distance,vertex",
                "CALL fnx.pagerank(0.85,1000,1e-9,true) YIELD score AS rank,vertex",
                "CALL fnx.weakly_connected_components() YIELD vertex,component",
                "CALL fnx.strongly_connected_components() YIELD component AS group_id,vertex",
            ] {
                let call = FnxCallSpec::bind(text, &parameters()).unwrap();
                let decoded = view.execute_fnx(&query, &call, options).unwrap();
                let compressed = view
                    .call_fnx_sealed(&query, &image, text, &parameters(), options, memory())
                    .unwrap();
                assert_eq!(compressed.analytics.columns, decoded.analytics.columns);
                assert_eq!(
                    compressed.analytics.rows, decoded.analytics.rows,
                    "{policy:?}: {text}"
                );
                assert_eq!(
                    compressed.analytics.certificate.result_digest,
                    decoded.analytics.certificate.result_digest
                );
                assert_eq!(
                    compressed.analytics.certificate.adapter,
                    AdapterPath::CompressedCursor
                );
                assert_eq!(
                    compressed.analytics.certificate.snapshot.root,
                    view.partition_root().0
                );
                assert_eq!(
                    compressed.analytics.certificate.snapshot.as_of,
                    view.frontier()
                );
                assert_eq!(compressed.analytics.certificate.vertices, 13);
                assert_eq!(compressed.selection, options.selection);
                assert_eq!(
                    compressed.analytics,
                    call.execute_sealed(&query, &prepared, options.execution_limits, memory())
                        .unwrap()
                );
                assert_eq!(
                    compressed,
                    db.call_fnx_sealed(
                        &query,
                        text,
                        &parameters(),
                        options,
                        memory(),
                        SealedLimits::default()
                    )
                    .await
                    .unwrap()
                );
            }
        }
        let result = view
            .execute_fnx_sealed(&query, &image, &dijkstra_call(), options(), memory())
            .unwrap();
        assert_eq!(
            &result.analytics.rows[..5],
            &[
                vec![FnxValue::Vertex(VId(1)), FnxValue::Float(0.0)],
                vec![FnxValue::Vertex(VId(2)), FnxValue::Float(2.0)],
                vec![FnxValue::Vertex(VId(3)), FnxValue::Float(2.0)],
                vec![FnxValue::Vertex(VId(4)), FnxValue::Float(3.0)],
                vec![FnxValue::Vertex(VId(5)), FnxValue::Float(4.0)],
            ]
        );
        let params = [("source".to_owned(), FnxArgument::Vertex(VId(u128::MAX)))]
            .into_iter()
            .collect();
        let isolated = db
            .call_fnx_sealed(
                &query,
                DIJKSTRA,
                &params,
                options(),
                memory(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            isolated.analytics.rows,
            vec![vec![FnxValue::Vertex(VId(u128::MAX)), FnxValue::Float(0.0)]]
        );
    });
}

#[test]
fn history_reloads_and_writer_drop_preserve_the_exact_admitted_source() {
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
        let old_image = db
            .store
            .seal_partition(
                &query,
                old.partition_root(),
                old.frontier(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let original = old
            .execute_fnx_sealed(&query, &old_image, &dijkstra_call(), options(), memory())
            .unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.set_edge_property(EId(101), PropertyKeyId(1), Some(CanonicalScalar::Int(6)));
        update.delete_edge(EId(102));
        update.delete_vertex(VId(3));
        update.delete_vertex(VId(12));
        db.write(&commit, update).await.unwrap();
        let latest = db.read_session().unwrap();
        let image = db
            .store
            .seal_partition(
                &query,
                latest.partition_root(),
                old.frontier(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let historical_options = FnxReadOptions {
            as_of: Some(old.frontier()),
            ..options()
        };
        let historical = latest
            .execute_fnx_sealed(
                &query,
                &image,
                &dijkstra_call(),
                historical_options,
                memory(),
            )
            .unwrap();
        assert_eq!(historical.analytics.rows, original.analytics.rows);
        assert_ne!(
            historical.analytics.certificate.snapshot.root,
            original.analytics.certificate.snapshot.root
        );
        assert_ne!(historical.digest, original.digest);
        let current = latest
            .execute_fnx_sealed(&query, &image, &dijkstra_call(), options(), memory())
            .unwrap();
        assert_eq!(current.analytics.certificate.vertices, 11);
        assert_eq!(
            current.analytics.rows[1],
            vec![FnxValue::Vertex(VId(2)), FnxValue::Float(6.0)]
        );
        assert!(
            current
                .analytics
                .rows
                .iter()
                .all(|row| row[0] != FnxValue::Vertex(VId(3)))
        );
        assert_eq!(
            current.analytics.rows,
            latest
                .execute_fnx(&query, &dijkstra_call(), options())
                .unwrap()
                .analytics
                .rows
        );
        assert_eq!(
            historical.analytics.rows,
            db.execute_fnx_sealed(
                &query,
                &dijkstra_call(),
                historical_options,
                memory(),
                SealedLimits::default()
            )
            .await
            .unwrap()
            .analytics
            .rows
        );

        let bytes = image.encode(&query, SealedLimits::default()).unwrap();
        let anchor = image.anchor();
        let reloaded =
            SealedPartition::reload(&query, anchor, &bytes, SealedLimits::default(), None).unwrap();
        assert!(!image.shares_image_with(&reloaded));
        assert_eq!(
            current,
            latest
                .execute_fnx_sealed(&query, &reloaded, &dijkstra_call(), options(), memory())
                .unwrap()
        );
        let mut damaged = bytes;
        damaged[0] ^= 1;
        assert!(matches!(
            SealedPartition::reload(&query, anchor, &damaged, SealedLimits::default(), None),
            Err(SealedError::ImageMismatch)
        ));
        let projection = latest
            .prism_sealed_projection_at(
                &query,
                &reloaded,
                latest.frontier(),
                options().selection,
                options().projection,
                options().projection_limits,
                options().source_limits,
            )
            .unwrap();
        let expected = projection
            .call_fnx(
                &query,
                DIJKSTRA,
                &parameters(),
                options().execution_limits,
                memory(),
            )
            .unwrap();
        let prepared = db
            .prism_sealed_projection_at(
                &query,
                None,
                options().selection,
                options().projection,
                options().projection_limits,
                options().source_limits,
                SealedLimits::default(),
            )
            .await
            .unwrap();
        drop(db);
        drop(image);
        drop(reloaded);
        assert_eq!(
            original,
            old.execute_fnx_sealed(&query, &old_image, &dijkstra_call(), options(), memory())
                .unwrap()
        );
        drop(old);
        drop(latest);
        assert_eq!(
            expected,
            projection
                .clone()
                .call_fnx(
                    &query,
                    DIJKSTRA,
                    &parameters(),
                    options().execution_limits,
                    memory()
                )
                .unwrap()
        );
        assert_eq!(
            current.analytics.rows,
            prepared
                .call_fnx(
                    &query,
                    DIJKSTRA,
                    &parameters(),
                    options().execution_limits,
                    memory()
                )
                .unwrap()
                .rows
        );
    });
}

#[test]
fn root_substitution_future_cuts_and_retired_image_history_fail_before_empty_selection() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let old = db.read_session().unwrap();
        let old_image = db
            .store
            .seal_partition(
                &query,
                old.partition_root(),
                old.frontier(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(5)));
        db.write(&commit, update).await.unwrap();
        let latest = db.read_session().unwrap();
        let image = db
            .store
            .seal_partition(
                &query,
                latest.partition_root(),
                latest.frontier(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let mut empty = options();
        empty.selection.vertex_label = Some(LabelId(999));
        empty.source_limits.max_work_units = 0;
        assert!(matches!(
            old.execute_fnx_sealed(&query, &image, &dijkstra_call(), empty, memory()),
            Err(SealedReadError::SourceMismatch)
        ));
        assert!(matches!(
            latest.execute_fnx_sealed(&query, &old_image, &dijkstra_call(), empty, memory()),
            Err(SealedReadError::SourceMismatch)
        ));
        empty.as_of = Some(old.frontier());
        assert!(matches!(
            latest.execute_fnx_sealed(&query, &image, &dijkstra_call(), empty, memory()),
            Err(SealedReadError::Projection(SealedProjectionError::Read(
                SealedError::SnapshotOutsideAnchor { .. }
            )))
        ));
        let future = FnxReadOptions {
            as_of: Some(CommitSeq(latest.frontier().0 + 1)),
            ..options()
        };
        assert!(matches!(
            db.execute_fnx_sealed(
                &query,
                &dijkstra_call(),
                future,
                memory(),
                SealedLimits::default()
            )
            .await,
            Err(SealedReadError::Input(Error::Read(
                ReadError::BeyondFrontier { .. }
            )))
        ));
        assert!(matches!(
            old.call_fnx_sealed(
                &query,
                &image,
                "CALL fnx.unknown()",
                &parameters(),
                empty,
                memory()
            ),
            Err(SealedReadError::Input(Error::Bind(_)))
        ));

        // Same namespace and sequence do not authorize a different graph root.
        let mut foreign = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let mut different = small_fixture();
        different.add_edge(
            EId(3),
            VId(3),
            VId(1),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
        );
        foreign.write(&commit, different).await.unwrap();
        let other = foreign.read_session().unwrap();
        assert_eq!(old.frontier(), other.frontier());
        let other_image = foreign
            .store
            .seal_partition(
                &query,
                other.partition_root(),
                other.frontier(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            old.execute_fnx_sealed(&query, &other_image, &dijkstra_call(), options(), memory()),
            Err(SealedReadError::SourceMismatch)
        ));
        // Inject the ordinary writer truthfulness state, not forged graph data.
        db.state = DatabaseState::CommitOutcomeUnknown {
            published_frontier: latest.frontier(),
        };
        assert!(matches!(
            db.execute_fnx_sealed(
                &query,
                &dijkstra_call(),
                options(),
                memory(),
                SealedLimits::default()
            )
            .await,
            Err(SealedReadError::Input(Error::Read(_)))
        ));
        latest
            .execute_fnx_sealed(&query, &image, &dijkstra_call(), options(), memory())
            .unwrap();
    });
}

#[test]
fn host_sealing_projection_and_kernel_admissions_do_not_fall_back_to_decoded() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        for limit in 0..3 {
            let mut opt = options();
            match limit {
                0 => opt.source_limits.max_work_units = 0,
                1 => opt.source_limits.max_staging_bytes = 0,
                _ => opt.projection_limits.max_vertices = 2,
            }
            let result = db
                .execute_fnx_sealed(
                    &query,
                    &dijkstra_call(),
                    opt,
                    memory(),
                    SealedLimits {
                        max_image_bytes: 0,
                        ..SealedLimits::default()
                    },
                )
                .await;
            assert!(matches!(
                result,
                Err(SealedReadError::Input(Error::SourceLimit { .. }))
                    | Err(SealedReadError::Input(Error::Projection(
                        ProjectionError::LimitExceeded { .. }
                    )))
            ));
        }
        for sealing in [
            SealedLimits {
                max_incidences: 0,
                ..SealedLimits::default()
            },
            SealedLimits {
                max_image_bytes: 0,
                ..SealedLimits::default()
            },
        ] {
            assert!(matches!(
                db.execute_fnx_sealed(&query, &dijkstra_call(), options(), memory(), sealing)
                    .await,
                Err(SealedReadError::Seal(SealedError::Limit { .. }))
                    | Err(SealedReadError::Seal(SealedError::Identity(_)))
            ));
        }
        let mut opt = options();
        opt.projection_limits.max_input_edges = 1;
        opt.selection.vertex_label = Some(LabelId(999));
        assert!(matches!(
            db.execute_fnx_sealed(
                &query,
                &dijkstra_call(),
                opt,
                memory(),
                SealedLimits::default()
            )
            .await,
            Err(SealedReadError::Projection(
                SealedProjectionError::Projection(ProjectionError::LimitExceeded {
                    resource: "source incidences",
                    ..
                })
            ))
        ));
        for cap in 0..4 {
            let mut opt = options();
            let mut mem = memory();
            match cap {
                0 => mem.max_kernel_workspace_bytes = 0,
                1 => mem.max_result_bytes = 0,
                2 => opt.execution_limits.max_estimated_work = 0,
                _ => opt.execution_limits.max_result_rows = 1,
            }
            assert!(matches!(
                db.execute_fnx_sealed(&query, &dijkstra_call(), opt, mem, SealedLimits::default())
                    .await,
                Err(SealedReadError::Execution(
                    FnxSealedExecutionError::Execution(FnxExecutionError::LimitExceeded { .. })
                ))
            ));
        }
        let mut opt = options();
        opt.execution_limits.max_result_rows = 1;
        let text = "CALL fnx.single_source_dijkstra_path_length($source,0,true) YIELD distance";
        assert_eq!(
            db.call_fnx_sealed(
                &query,
                text,
                &parameters(),
                opt,
                memory(),
                SealedLimits::default()
            )
            .await
            .unwrap()
            .analytics
            .rows,
            vec![vec![FnxValue::Float(0.0)]]
        );
        opt.projection.directedness = Directedness::Undirected;
        assert!(matches!(
            db.execute_fnx_sealed(
                &query,
                &dijkstra_call(),
                opt,
                memory(),
                SealedLimits::default()
            )
            .await,
            Err(SealedReadError::Execution(
                FnxSealedExecutionError::Execution(FnxExecutionError::LimitExceeded {
                    resource: "result rows", ..
                })
            ))
        ));
        opt.execution_limits.max_result_rows = 3;
        assert_eq!(db.execute_fnx_sealed(&query, &dijkstra_call(), opt, memory(),
            SealedLimits::default()).await.unwrap().analytics.rows, vec![
                vec![FnxValue::Vertex(VId(1)), FnxValue::Float(0.0)],
                vec![FnxValue::Vertex(VId(2)), FnxValue::Float(2.0)],
                vec![FnxValue::Vertex(VId(3)), FnxValue::Float(2.0)],
            ]);
        opt = options();
        opt.selection.relation = None;
        assert!(matches!(
            db.execute_fnx_sealed(
                &query,
                &dijkstra_call(),
                opt,
                memory(),
                SealedLimits::default()
            )
            .await,
            Err(SealedReadError::Projection(
                SealedProjectionError::RelationRequired
            ))
        ));
        assert!(matches!(
            db.call_fnx_sealed(
                &query,
                "CALL fnx.triangles()",
                &parameters(),
                options(),
                memory(),
                SealedLimits {
                    max_image_bytes: 0,
                    ..SealedLimits::default()
                }
            )
            .await,
            Err(SealedReadError::Execution(
                FnxSealedExecutionError::UnsupportedAlgorithm(_)
            ))
        ));
        db.execute_fnx_sealed(
            &query,
            &dijkstra_call(),
            options(),
            memory(),
            SealedLimits::default(),
        )
        .await
        .unwrap();
    });
}

#[test]
fn induced_endpoints_and_explicit_loop_and_weight_discard_precede_numeric_binding() {
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
        db.execute_fnx_sealed(
            &query,
            &dijkstra_call(),
            options(),
            memory(),
            SealedLimits::default(),
        )
        .await
        .unwrap();
        let mut all = options();
        all.selection.vertex_label = None;
        assert!(matches!(
            db.execute_fnx_sealed(
                &query,
                &dijkstra_call(),
                all,
                memory(),
                SealedLimits::default()
            )
            .await,
            Err(SealedReadError::Projection(SealedProjectionError::Weight {
                edge: EId(999),
                reason: FnxWeightError::NotNumeric
            }))
        ));
        all.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        let ignored = db
            .execute_fnx_sealed(
                &query,
                &dijkstra_call(),
                all,
                memory(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        assert!(
            ignored
                .analytics
                .rows
                .iter()
                .any(|row| row[0] == FnxValue::Vertex(VId(99)))
        );
        let mut loop_batch = WriteBatch::new(RelationId(1));
        loop_batch.add_edge(EId(1000), VId(u128::MAX), VId(u128::MAX), vec![]);
        db.write(&commit, loop_batch).await.unwrap();
        assert!(matches!(
            db.execute_fnx_sealed(
                &query,
                &dijkstra_call(),
                options(),
                memory(),
                SealedLimits::default()
            )
            .await,
            Err(SealedReadError::Projection(SealedProjectionError::Weight {
                edge: EId(1000),
                reason: FnxWeightError::Missing
            }))
        ));
        let mut opt = options();
        opt.projection.self_loops = SelfLoopPolicy::Drop;
        db.execute_fnx_sealed(
            &query,
            &dijkstra_call(),
            opt,
            memory(),
            SealedLimits::default(),
        )
        .await
        .unwrap();
        opt.projection.self_loops = SelfLoopPolicy::Reject;
        assert!(matches!(
            db.execute_fnx_sealed(
                &query,
                &dijkstra_call(),
                opt,
                memory(),
                SealedLimits::default()
            )
            .await,
            Err(SealedReadError::Projection(
                SealedProjectionError::Projection(ProjectionError::SelfLoop(EId(1000)))
            ))
        ));
    });
}

#[test]
fn cancellation_through_source_sealing_projection_kernel_and_result_never_publishes_a_prefix() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        let image = db
            .store
            .seal_partition(
                &query,
                view.partition_root(),
                view.frontier(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = query.with_checkpoint_probe(Arc::clone(&probe));
        let expected = view
            .execute_fnx_sealed(&observed, &image, &dijkstra_call(), options(), memory())
            .unwrap();
        let count = probe.calls();
        assert!(count > 20);
        for stop in 1..=count {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
            assert!(matches!(
                view.execute_fnx_sealed(&controlled, &image, &dijkstra_call(), options(), memory()),
                Err(SealedReadError::Input(Error::Cancelled(_)))
                    | Err(SealedReadError::Projection(SealedProjectionError::Read(
                        SealedError::Interrupted(_)
                    )))
                    | Err(SealedReadError::Execution(
                        FnxSealedExecutionError::Cancelled(_)
                    ))
            ));
            assert_eq!(probe.calls(), stop);
        }
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = query.with_checkpoint_probe(Arc::clone(&probe));
        let auto = db
            .execute_fnx_sealed(
                &observed,
                &dijkstra_call(),
                options(),
                memory(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(auto, expected);
        let count = probe.calls();
        for stop in 1..=count {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = query.with_checkpoint_probe(probe);
            // Storage I/O keeps its native nested error classification; no
            // stage is allowed to convert interruption into a successful EOF.
            assert!(
                db.execute_fnx_sealed(
                    &controlled,
                    &dijkstra_call(),
                    options(),
                    memory(),
                    SealedLimits::default()
                )
                .await
                .is_err()
            );
            assert_eq!(
                db.read_session().unwrap().partition_root(),
                view.partition_root()
            );
        }
        assert_eq!(
            expected,
            view.execute_fnx_sealed(&query, &image, &dijkstra_call(), options(), memory())
                .unwrap()
        );
        assert_eq!(
            auto,
            db.execute_fnx_sealed(
                &query,
                &dijkstra_call(),
                options(),
                memory(),
                SealedLimits::default()
            )
            .await
            .unwrap()
        );
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}