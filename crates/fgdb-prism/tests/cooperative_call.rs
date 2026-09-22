//! Projection-to-kernel integration. This exercises the real registered call,
//! not a mock GraphView or a second analytics implementation.
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxExecutionError, FnxExecutionLimits, PageRankOptions,
    ParallelEdgePolicy, ProjectionBuildError, ProjectionEdge, ProjectionLimits,
    ProjectionSpec, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
};
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};

fn input() -> (Vec<VId>, Vec<ProjectionEdge>) {
    let vertices = vec![VId(u128::MAX), VId(17), VId(0), VId(99)];
    let edges = vec![
        ProjectionEdge { eid: EId(7), source: VId(17), target: VId(u128::MAX), weight: 1.0 },
        ProjectionEdge { eid: EId(3), source: VId(u128::MAX), target: VId(0), weight: 1.0 },
        ProjectionEdge { eid: EId(1), source: VId(0), target: VId(17), weight: 1.0 },
        ProjectionEdge { eid: EId(2), source: VId(0), target: VId(17), weight: 2.0 },
    ];
    (vertices, edges)
}
fn binding() -> SnapshotBinding {
    // Provenance fixture only, not an authorization grant.
    SnapshotBinding { root: ObjectId([19; 32]), as_of: CommitSeq(11) }
}
fn spec() -> ProjectionSpec {
    ProjectionSpec { directedness: Directedness::Directed,
        parallel_edges: ParallelEdgePolicy::Sum, self_loops: SelfLoopPolicy::Keep }
}
fn projection_limits() -> ProjectionLimits {
    ProjectionLimits { max_vertices: 4, max_input_edges: 4,
        max_adjacency_entries: 4, max_workspace_bytes: 1 << 20 }
}
fn execution_limits() -> FnxExecutionLimits {
    FnxExecutionLimits { max_iterations: 100, max_result_rows: 4, max_estimated_work: 10000 }
}

#[test]
fn owned_projection_executes_registered_call_with_identical_results_and_certificate() {
    let (vertices, edges) = input();
    let borrowed = SnapshotGraphView::build(binding(), &vertices, &edges, spec(), projection_limits()).unwrap();
    let owned = SnapshotGraphView::build_owned_with_checkpoint(binding(), vertices, edges,
        spec(), projection_limits(), || Ok::<(), &'static str>(())).unwrap();
    let call = FnxCallSpec::pagerank(PageRankOptions::default());
    let baseline = call.execute(&borrowed, execution_limits(), || Ok::<(), &'static str>(())).unwrap();
    let actual = call.execute(&owned, execution_limits(), || Ok::<(), &'static str>(())).unwrap();
    assert_eq!(actual, baseline);
    assert_eq!(actual.certificate.snapshot, binding());
    assert_eq!(actual.certificate.input_edges, 4);
    assert_eq!(actual.certificate.edges, 3);
    assert_eq!(actual.rows.len(), 4); // the isolated VId remains in the result
}

#[test]
fn cancellation_propagates_across_the_complete_projection_and_call_pipeline() {
    let call = FnxCallSpec::pagerank(PageRankOptions::default());
    let (vertices, edges) = input();
    let mut all_checks = 0;
    let mut control = || { all_checks += 1; Ok::<(), &'static str>(()) };
    let graph = SnapshotGraphView::build_owned_with_checkpoint(binding(), vertices, edges,
        spec(), projection_limits(), &mut control).unwrap();
    call.execute(&graph, execution_limits(), &mut control).unwrap();
    let baseline = call.execute(&graph, execution_limits(), || Ok::<(), &'static str>(())).unwrap();
    let mut projection_cuts = 0;
    let mut execution_cuts = 0;
    for stop in 1..=all_checks {
        let (vertices, edges) = input();
        let mut observed = 0;
        let mut control = || {
            observed += 1;
            if observed == stop { Err("stop") } else { Ok(()) }
        };
        match SnapshotGraphView::build_owned_with_checkpoint(binding(), vertices, edges,
            spec(), projection_limits(), &mut control) {
            Err(ProjectionBuildError::Cancelled("stop")) => { projection_cuts += 1; }
            Ok(projection) => {
                assert!(matches!(call.execute(&projection, execution_limits(), &mut control),
                    Err(FnxExecutionError::Cancelled("stop"))));
                execution_cuts += 1;
            }
            other => panic!("unexpected projection outcome: {other:?}"),
        }
        assert_eq!(observed, stop, "no callback may run after cancellation");
    }
    assert!(projection_cuts > 0);
    assert!(execution_cuts > 0);
    // Failed work must neither mutate nor invalidate a separately pinned cache.
    assert_eq!(call.execute(&graph, execution_limits(), || Ok::<(), &'static str>(())).unwrap(), baseline);
}
