//! End-to-end registry dispatch through actual Chronicle/Strata historical
//! views. The traversal kernels must never substitute a fresh database head.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_prism::*;
use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

fn options() -> FnxReadOptions {
    FnxReadOptions {
        as_of: None,
        selection: FnxSelection {
            vertex_label: Some(LabelId(1)),
            relation: Some(RelationId(1)),
            weight: FnxWeightSpec::Unit,
        },
        projection: ProjectionSpec {
            directedness: Directedness::Directed,
            parallel_edges: ParallelEdgePolicy::CollapseUnit,
            self_loops: SelfLoopPolicy::Keep,
        },
        source_limits: FnxSourceLimits {
            max_work_units: 100_000,
            max_scratch_entries: 10_000,
            max_staging_bytes: 1 << 22,
        },
        projection_limits: ProjectionLimits {
            max_vertices: 100,
            max_input_edges: 1000,
            max_adjacency_entries: 2000,
            max_workspace_bytes: 1 << 22,
        },
        execution_limits: FnxExecutionLimits {
            max_iterations: 0,
            max_result_rows: 100,
            max_estimated_work: 1 << 24,
        },
    }
}
fn vertex(value: u128) -> FnxValue {
    FnxValue::Vertex(VId(value))
}
fn components(values: &[(u128, u128)]) -> Vec<Vec<FnxValue>> {
    values
        .iter()
        .map(|&(id, component)| vec![vertex(id), vertex(component)])
        .collect()
}

#[test]
fn traversal_calls_follow_selected_historical_generations_across_writes_and_drop() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let keys = DatabaseKeys::new(
            [0x51; 32],
            DatabaseSecurityNamespaceId([0x52; 32]),
            [0x53; 32],
        );
        let mut db = Database::<MemVfs>::open_memory(&commit, keys)
            .await
            .unwrap();
        let mut first = WriteBatch::new(RelationId(1));
        for id in [1, 2, u128::MAX] {
            first.create_vertex(VId(id), vec![LabelId(1)], vec![]);
        }
        first.create_vertex(VId(99), vec![LabelId(2)], vec![]);
        for (id, source, target) in [(1, 1, 2), (2, 2, 1), (3, 2, 99), (4, 99, u128::MAX)] {
            first.add_edge(EId(id), VId(source), VId(target), vec![]);
        }
        db.write(&commit, first).await.unwrap();
        let old = db.read_session().unwrap();
        let parameters = FnxParameters::new();
        let bfs = "CALL fnx.single_source_shortest_path_length(1)";
        let wcc = "CALL fnx.weakly_connected_components()";
        let scc = "CALL fnx.strongly_connected_components()";
        let old_bfs = old.call_fnx(&query, bfs, &parameters, options()).unwrap();
        assert_eq!(
            old_bfs.analytics.rows,
            vec![
                vec![vertex(1), FnxValue::Integer(0)],
                vec![vertex(2), FnxValue::Integer(1)],
            ]
        );
        let expected = components(&[(1, 1), (2, 1), (u128::MAX, u128::MAX)]);
        let old_scc = old.call_fnx(&query, scc, &parameters, options()).unwrap();
        assert_eq!(old_scc.analytics.rows, expected);
        assert_eq!(
            db.call_fnx(&query, wcc, &parameters, options())
                .unwrap()
                .analytics
                .rows,
            expected
        );

        let mut bridge = WriteBatch::new(RelationId(1));
        bridge.add_edge(EId(5), VId(2), VId(u128::MAX), vec![]);
        db.write(&commit, bridge).await.unwrap();
        let reached = db.call_fnx(&query, bfs, &parameters, options()).unwrap();
        assert_eq!(
            reached.analytics.rows[2],
            vec![vertex(u128::MAX), FnxValue::Integer(2)]
        );
        let connected = components(&[(1, 1), (2, 1), (u128::MAX, 1)]);
        assert_eq!(
            db.call_fnx(&query, wcc, &parameters, options())
                .unwrap()
                .analytics
                .rows,
            connected
        );
        assert_eq!(
            db.call_fnx(&query, scc, &parameters, options())
                .unwrap()
                .analytics
                .rows,
            expected
        );

        let mut close_cycle = WriteBatch::new(RelationId(1));
        close_cycle.add_edge(EId(6), VId(u128::MAX), VId(1), vec![]);
        db.write(&commit, close_cycle).await.unwrap();
        let latest = db.read_session().unwrap();
        assert_eq!(
            latest
                .call_fnx(&query, scc, &parameters, options())
                .unwrap()
                .analytics
                .rows,
            connected
        );
        let historical = latest
            .call_fnx(
                &query,
                scc,
                &parameters,
                FnxReadOptions {
                    as_of: Some(old.frontier()),
                    ..options()
                },
            )
            .unwrap();
        assert_eq!(historical.analytics.rows, old_scc.analytics.rows);
        assert_ne!(historical.digest, old_scc.digest);
        let mut undirected = options();
        undirected.projection.directedness = Directedness::Undirected;
        assert_eq!(
            latest
                .call_fnx(
                    &query,
                    "CALL fnx.connected_components()",
                    &parameters,
                    undirected
                )
                .unwrap()
                .analytics
                .rows,
            connected
        );
        assert!(matches!(
            latest.call_fnx(
                &query,
                "CALL fnx.connected_components()",
                &parameters,
                options()
            ),
            Err(FnxReadError::Execution(FnxExecutionError::GraphKind {
                required: FnxGraphKind::Undirected
            }))
        ));

        drop(db);
        assert_eq!(
            old_bfs,
            old.call_fnx(&query, bfs, &parameters, options()).unwrap()
        );
        assert_eq!(
            old_scc,
            old.call_fnx(&query, scc, &parameters, options()).unwrap()
        );
    });
}
