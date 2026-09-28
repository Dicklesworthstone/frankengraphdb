//! Foundation procedures (fgdb-lq1v6): the pinned franken_networkx catalog runs
//! as-is over a decoded copy of the admitted projection. The kernel IS fnx, so
//! these laws do not compare it with fnx. They check independently computed
//! values, the VId mapping (vertex identities deliberately out of name order),
//! admission, refusals and the certificate's kernel identity.

use fgdb_prism::*;
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use std::collections::BTreeMap;
use std::convert::Infallible;

/// Vertex identities are not in path order, so a mapping that followed
/// insertion or name order instead of the projection's ordinals would move
/// every score to the wrong vertex.
const PATH: [u128; 4] = [40, 10, 30, 20];

fn view(vertices: &[u128], edges: &[(u128, u128)], loops: SelfLoopPolicy) -> SnapshotGraphView {
    let vertices: Vec<VId> = vertices.iter().map(|&id| VId(id)).collect();
    let edges: Vec<ProjectionEdge> = edges
        .iter()
        .enumerate()
        .map(|(eid, &(source, target))| ProjectionEdge {
            eid: EId(eid as u128 + 1),
            source: VId(source),
            target: VId(target),
            weight: 1.0,
        })
        .collect();
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([7; 32]),
            as_of: CommitSeq(3),
        },
        &vertices,
        &edges,
        ProjectionSpec {
            directedness: Directedness::Undirected,
            parallel_edges: ParallelEdgePolicy::Reject,
            self_loops: loops,
        },
        ProjectionLimits {
            max_vertices: vertices.len(),
            max_input_edges: edges.len(),
            max_adjacency_entries: edges.len() * 2,
            max_workspace_bytes: 1 << 24,
        },
    )
    .unwrap()
}
/// The path PATH[0] - PATH[1] - PATH[2] - PATH[3].
fn path() -> SnapshotGraphView {
    view(
        &PATH,
        &[(PATH[0], PATH[1]), (PATH[1], PATH[2]), (PATH[2], PATH[3])],
        SelfLoopPolicy::Keep,
    )
}
fn limits(max_estimated_work: usize) -> FnxExecutionLimits {
    FnxExecutionLimits {
        max_iterations: 1_000,
        max_result_rows: 1_000,
        max_estimated_work,
    }
}
fn call(name: &str) -> FnxCallSpec {
    FnxCallSpec::bind(&format!("CALL fnx.{name}()"), &FnxParameters::new()).unwrap()
}
fn run(name: &str, graph: &SnapshotGraphView) -> FnxResult {
    call(name)
        .execute(graph, limits(1 << 30), || Ok::<(), Infallible>(()))
        .unwrap()
}
/// The rows as vertex -> value, with each row's value as f64. A row of any
/// other shape is dropped, which the callers' length checks then report.
fn by_vertex(result: &FnxResult) -> BTreeMap<u128, f64> {
    result
        .rows
        .iter()
        .filter_map(|row| match row.as_slice() {
            [FnxValue::Vertex(vertex), FnxValue::Score(value)] => Some((vertex.0, *value)),
            [FnxValue::Vertex(vertex), FnxValue::Integer(value)] => Some((vertex.0, *value as f64)),
            _ => None,
        })
        .collect()
}
fn assert_values(name: &str, graph: &SnapshotGraphView, expected: &[(u128, f64)]) {
    let result = run(name, graph);
    let actual = by_vertex(&result);
    assert_eq!(actual.len(), expected.len(), "{name}: {actual:?}");
    for &(vertex, value) in expected {
        let got = actual.get(&vertex).copied().unwrap_or(f64::NAN);
        assert!(
            (got - value).abs() < 1e-12,
            "{name} at {vertex}: {got} != {value}"
        );
    }
    // Rows come in VId order, whatever order the foundation answered in.
    let order: Vec<u128> = result
        .rows
        .iter()
        .filter_map(|row| match row.first() {
            Some(FnxValue::Vertex(vertex)) => Some(vertex.0),
            _ => None,
        })
        .collect();
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(order, sorted, "{name} rows must be in VId order");
}

#[test]
fn centralities_and_cores_match_independently_computed_values() {
    let path = path();
    let [a, b, c, d] = PATH;
    // Degree / (n - 1).
    assert_values(
        "degree_centrality",
        &path,
        &[
            (a, 1.0 / 3.0),
            (b, 2.0 / 3.0),
            (c, 2.0 / 3.0),
            (d, 1.0 / 3.0),
        ],
    );
    // Each inner vertex lies on the shortest paths of 2 of the 3 pairs that
    // exclude it; undirected normalization 2 / ((n-1)(n-2)) = 1/3.
    assert_values(
        "betweenness_centrality",
        &path,
        &[(a, 0.0), (b, 2.0 / 3.0), (c, 2.0 / 3.0), (d, 0.0)],
    );
    // (n - 1) / sum of distances on a connected graph.
    assert_values(
        "closeness_centrality",
        &path,
        &[
            (a, 3.0 / 6.0),
            (b, 3.0 / 4.0),
            (c, 3.0 / 4.0),
            (d, 3.0 / 6.0),
        ],
    );
    // Sum of reciprocal distances.
    assert_values(
        "harmonic_centrality",
        &path,
        &[
            (a, 1.0 + 1.0 / 2.0 + 1.0 / 3.0),
            (b, 1.0 + 1.0 + 1.0 / 2.0),
            (c, 1.0 + 1.0 + 1.0 / 2.0),
            (d, 1.0 + 1.0 / 2.0 + 1.0 / 3.0),
        ],
    );
    assert_values(
        "core_number",
        &path,
        &[(a, 1.0), (b, 1.0), (c, 1.0), (d, 1.0)],
    );
    // A triangle 5-6-7 with a tail 7-8 and an isolated 9: the triangle is the
    // 2-core, the tail vertex has core 1, the isolate 0.
    let cored = view(
        &[5, 6, 7, 8, 9],
        &[(5, 6), (6, 7), (5, 7), (7, 8)],
        SelfLoopPolicy::Keep,
    );
    assert_values(
        "core_number",
        &cored,
        &[(5, 2.0), (6, 2.0), (7, 2.0), (8, 1.0), (9, 0.0)],
    );
    // A triangle's principal eigenvector is uniform: 1 / sqrt(3) each.
    let triangle = view(&[1, 2, 3], &[(1, 2), (2, 3), (1, 3)], SelfLoopPolicy::Keep);
    let third = 1.0 / 3.0_f64.sqrt();
    let result = run("eigenvector_centrality", &triangle);
    let actual = by_vertex(&result);
    assert_eq!(actual.len(), 3);
    for value in actual.values() {
        assert!((value - third).abs() < 1e-6, "eigenvector {value}");
    }
}

#[test]
fn a_foundation_call_is_certified_with_the_foundation_kernel_and_witness() {
    let result = run("betweenness_centrality", &path());
    let certificate = &result.certificate;
    assert_eq!(
        certificate.execution_kernel,
        "fnx-algorithms/betweenness_centrality"
    );
    assert_eq!(certificate.registry_version, FNX_SIGNATURE_REGISTRY_VERSION);
    assert_eq!(certificate.vertices, 4);
    // The witness is the foundation's own, not one Prism invented.
    assert!(!certificate.witness.algorithm.is_empty());
    assert!(certificate.witness.nodes_touched > 0);
    // Same projection and call, same certificate: the call is deterministic.
    assert_eq!(
        run("betweenness_centrality", &path()).certificate,
        result.certificate
    );
}

#[test]
fn admission_refuses_before_the_foundation_runs() {
    // Betweenness admits n * (n + arcs) = 4 * (4 + 6) = 40 units on the path.
    let refused =
        call("betweenness_centrality").execute(&path(), limits(39), || Ok::<(), Infallible>(()));
    assert!(
        matches!(
            refused,
            Err(FnxExecutionError::LimitExceeded {
                resource: "estimated work",
                limit: 39,
                requested: 40,
            })
        ),
        "{refused:?}"
    );
    assert!(
        call("betweenness_centrality")
            .execute(&path(), limits(40), || Ok::<(), Infallible>(()))
            .is_ok()
    );
    // A directed projection is refused for these undirected-law procedures.
    let directed = SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([7; 32]),
            as_of: CommitSeq(3),
        },
        &[VId(1), VId(2)],
        &[ProjectionEdge {
            eid: EId(1),
            source: VId(1),
            target: VId(2),
            weight: 1.0,
        }],
        ProjectionSpec {
            directedness: Directedness::Directed,
            parallel_edges: ParallelEdgePolicy::Reject,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: 2,
            max_input_edges: 1,
            max_adjacency_entries: 2,
            max_workspace_bytes: 1 << 20,
        },
    )
    .unwrap();
    assert!(matches!(
        call("degree_centrality").execute(&directed, limits(1 << 20), || Ok::<(), Infallible>(())),
        Err(FnxExecutionError::GraphKind {
            required: FnxGraphKind::Undirected
        })
    ));
}

#[test]
fn a_kept_self_loop_is_refused_and_a_dropped_one_is_not() {
    let edges = [(1, 2), (2, 2)];
    let kept = view(&[1, 2], &edges, SelfLoopPolicy::Keep);
    let refused = call("core_number").execute(&kept, limits(1 << 20), || Ok::<(), Infallible>(()));
    assert!(
        matches!(refused, Err(FnxExecutionError::SelfLoopRefused)),
        "{refused:?}"
    );
    let dropped = view(&[1, 2], &edges, SelfLoopPolicy::Drop);
    assert_values("core_number", &dropped, &[(1, 1.0), (2, 1.0)]);
}

#[test]
fn cancellation_is_observed_before_and_after_the_foundation_call() {
    // Count the checkpoints of one successful call, then stop at each one.
    let mut seen = 0usize;
    call("closeness_centrality")
        .execute(&path(), limits(1 << 20), || {
            seen += 1;
            Ok::<(), usize>(())
        })
        .unwrap();
    assert!(seen > 2);
    for stop in 1..=seen {
        let mut at = 0usize;
        let result = call("closeness_centrality").execute(&path(), limits(1 << 20), || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(
            matches!(result, Err(FnxExecutionError::Cancelled(observed)) if observed == stop),
            "stop {stop}"
        );
    }
}
