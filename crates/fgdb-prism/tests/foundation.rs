//! Foundation procedures and their declared native fast paths. Degree, core
//! and eigenvector run over the admitted CSR and are checked against pinned fnx
//! and independent expectations. Other procedures retain the foundation copy.
//! All paths keep canonical VId mapping, admission, refusals and provenance.

use fgdb_prism::*;
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use fnx_classes::Graph;
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

fn degree_bits(result: &FnxResult) -> BTreeMap<VId, u64> {
    result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [FnxValue::Vertex(vertex), FnxValue::Score(score)] => (*vertex, score.to_bits()),
            _ => panic!("typed degree row required: {row:?}"),
        })
        .collect()
}

fn core_values(result: &FnxResult) -> BTreeMap<VId, u64> {
    result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [FnxValue::Vertex(vertex), FnxValue::Integer(core)] => (*vertex, *core),
            _ => panic!("typed core row required: {row:?}"),
        })
        .collect()
}

/// Only the oracle needs the string-keyed graph removed from the native path.
fn copied_graph(graph: &SnapshotGraphView) -> Graph {
    let mut copy = Graph::strict();
    let names = graph.nodes_ordered();
    for &name in &names {
        copy.add_node(name);
    }
    for source in 0..graph.node_count() {
        for &target in graph.neighbors_indices(source).unwrap() {
            if source < target {
                copy.add_edge(names[source], names[target]).unwrap();
            }
        }
    }
    copy
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeStop {
    Work(usize),
    Poll(usize),
}

#[derive(Debug, Default)]
struct NativeControl {
    units: usize,
    // Logical charge at each live poll, including polls during the arithmetic.
    polls: Vec<usize>,
    stop: Option<NativeStop>,
    stopped: bool,
}

impl FnxMeter<NativeStop> for &mut NativeControl {
    fn unit(&mut self) -> Result<(), NativeStop> {
        assert!(!self.stopped, "no control call may follow a refusal");
        self.units += 1;
        let stop = NativeStop::Work(self.units);
        if self.stop == Some(stop) {
            self.stopped = true;
            Err(stop)
        } else {
            Ok(())
        }
    }

    fn poll(&mut self) -> Result<(), NativeStop> {
        assert!(!self.stopped, "no control call may follow a refusal");
        self.polls.push(self.units);
        let stop = NativeStop::Poll(self.polls.len());
        if self.stop == Some(stop) {
            self.stopped = true;
            Err(stop)
        } else {
            Ok(())
        }
    }
}

#[test]
fn native_degree_and_core_match_pinned_fnx_on_every_five_vertex_topology() {
    // Enumeration includes isolates, disconnected cores, dense ties and hubs.
    // These identities share low halves and disagree with insertion order.
    let vertices = [u128::MAX, 17, 1 << 127, 0, (1 << 64) | 17];
    let pairs: Vec<_> = (0..vertices.len())
        .flat_map(|source| ((source + 1)..vertices.len()).map(move |target| (source, target)))
        .collect();
    for mask in 0..(1usize << pairs.len()) {
        let edges: Vec<_> = pairs
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask & (1 << bit) != 0)
            .map(|(_, &(source, target))| (vertices[source], vertices[target]))
            .collect();
        let graph = view(&vertices, &edges, SelfLoopPolicy::Keep);
        let copy = copied_graph(&graph);
        let degree = fnx_algorithms::degree_centrality(&copy);
        let core = fnx_algorithms::core_number(&copy);
        let vertex = |name: &str| {
            graph
                .vertex_id(graph.get_node_index(name).unwrap())
                .unwrap()
        };
        let expected_degree: BTreeMap<_, _> = degree
            .scores
            .into_iter()
            .map(|entry| (vertex(&entry.node), entry.score.to_bits()))
            .collect();
        let expected_core: BTreeMap<_, _> = core
            .core_numbers
            .into_iter()
            .map(|entry| (vertex(&entry.node), u64::try_from(entry.core).unwrap()))
            .collect();
        let actual_degree = run("degree_centrality", &graph);
        let actual_core = run("core_number", &graph);
        assert_eq!(degree_bits(&actual_degree), expected_degree, "mask {mask}");
        assert_eq!(core_values(&actual_core), expected_core, "mask {mask}");
        for (actual, expected) in [
            (&actual_degree.certificate.witness, &degree.witness),
            (&actual_core.certificate.witness, &core.witness),
        ] {
            assert_eq!(
                (
                    actual.nodes_touched,
                    actual.edges_scanned,
                    actual.queue_peak
                ),
                (
                    expected.nodes_touched,
                    expected.edges_scanned,
                    expected.queue_peak
                ),
                "mask {mask}"
            );
        }
    }
}

#[test]
fn native_core_matches_independent_k_core_elimination_on_all_small_topologies() {
    let vertices = [u128::MAX, 17, 1 << 127, 0, (1 << 64) | 17];
    let n = vertices.len();
    let pairs: Vec<_> = (0..n)
        .flat_map(|source| ((source + 1)..n).map(move |target| (source, target)))
        .collect();
    for mask in 0..(1usize << pairs.len()) {
        let mut adjacent = [[false; 5]; 5];
        let mut edges = Vec::new();
        for (bit, &(source, target)) in pairs.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                adjacent[source][target] = true;
                adjacent[target][source] = true;
                edges.push((vertices[source], vertices[target]));
            }
        }
        // Independently construct every k-core by repeated simultaneous
        // elimination. This deliberately uses no degree ordering or buckets.
        let mut cores = [0u64; 5];
        for k in 1..n {
            let mut present = [true; 5];
            loop {
                let removed: Vec<_> = (0..n)
                    .filter(|&source| {
                        present[source]
                            && (0..n)
                                .filter(|&target| present[target] && adjacent[source][target])
                                .count()
                                < k
                    })
                    .collect();
                if removed.is_empty() {
                    break;
                }
                for vertex in removed {
                    present[vertex] = false;
                }
            }
            for vertex in 0..n {
                if present[vertex] {
                    cores[vertex] = k as u64;
                }
            }
        }
        let graph = view(&vertices, &edges, SelfLoopPolicy::Keep);
        let expected: BTreeMap<_, _> = vertices
            .iter()
            .copied()
            .zip(cores)
            .map(|(vertex, core)| (VId(vertex), core))
            .collect();
        assert_eq!(
            core_values(&run("core_number", &graph)),
            expected,
            "mask {mask}"
        );
    }
}

#[test]
fn native_degree_keeps_reciprocal_multiplication_and_empty_singleton_semantics() {
    let vertices = [u128::MAX, 0, 17, 1 << 64, 1 << 127, (1 << 64) | 17];
    let graph = view(
        &vertices,
        &[
            (vertices[0], vertices[1]),
            (vertices[0], vertices[2]),
            (vertices[0], vertices[3]),
        ],
        SelfLoopPolicy::Keep,
    );
    let expected = 3.0_f64 * (1.0 / 5.0);
    assert_ne!(expected.to_bits(), (3.0_f64 / 5.0).to_bits());
    assert_eq!(
        degree_bits(&run("degree_centrality", &graph))[&VId(u128::MAX)],
        expected.to_bits()
    );

    let empty = view(&[], &[], SelfLoopPolicy::Keep);
    let singleton = view(&[u128::MAX], &[], SelfLoopPolicy::Keep);
    for name in ["degree_centrality", "core_number"] {
        let result = run(name, &empty);
        assert!(result.rows.is_empty());
        assert_eq!(result.certificate.kernel_workspace_bytes, 0);
        assert_eq!(result.certificate.witness.nodes_touched, 0);
        assert_eq!(result.certificate.witness.edges_scanned, 0);
        let single = run(name, &singleton);
        assert_eq!(single.certificate.witness.nodes_touched, 1);
        assert_eq!(single.certificate.witness.edges_scanned, 0);
        assert_eq!(single.certificate.witness.queue_peak, 0);
        let looped = view(
            &[u128::MAX],
            &[(u128::MAX, u128::MAX)],
            SelfLoopPolicy::Keep,
        );
        assert!(matches!(
            call(name).execute(&looped, limits(100), &mut NativeControl::default()),
            Err(FnxExecutionError::SelfLoopRefused)
        ));
    }
    assert_eq!(
        degree_bits(&run("degree_centrality", &singleton)),
        BTreeMap::from([(VId(u128::MAX), 1.0_f64.to_bits())])
    );
    assert_eq!(
        core_values(&run("core_number", &singleton)),
        BTreeMap::from([(VId(u128::MAX), 0)])
    );
}

#[test]
fn native_foundation_preserves_every_logical_refusal_and_loop_precedence() {
    for graph in [
        view(&[], &[], SelfLoopPolicy::Keep),
        view(&[u128::MAX], &[], SelfLoopPolicy::Keep),
        path(),
    ] {
        for name in ["degree_centrality", "core_number", "eigenvector_centrality"] {
            let mut complete = NativeControl::default();
            let actual = call(name)
                .execute(&graph, limits(1 << 20), &mut complete)
                .unwrap();
            assert_eq!(actual, run(name, &graph), "{name}");
            // Old copy: admission + arcs + nodes + rows + kernel boundaries +
            // conversion + publication. Polls must not charge these again.
            assert_eq!(complete.units, 4 * graph.node_count() + 5, "{name}");
            for ordinal in 1..=complete.units {
                let stop = NativeStop::Work(ordinal);
                let mut control = NativeControl {
                    stop: Some(stop),
                    ..Default::default()
                };
                let result = call(name).execute(&graph, limits(1 << 20), &mut control);
                assert!(
                    matches!(result, Err(FnxExecutionError::Cancelled(observed)) if observed == stop),
                    "{name} at {ordinal}"
                );
                assert_eq!(control.units, ordinal, "{name} at {ordinal}");
                assert!(control.stopped);
            }
        }
    }

    let edges = [(0, 17), (17, 17)];
    let kept = view(&[0, 17, u128::MAX], &edges, SelfLoopPolicy::Keep);
    // The loop is in canonical row 1. Its row checkpoint precedes validation;
    // the next row's work refusal must never overtake the self-loop refusal.
    let loop_unit = 2 * kept.node_count() + 1 + 2;
    for name in ["degree_centrality", "core_number", "eigenvector_centrality"] {
        for ordinal in [loop_unit, loop_unit + 1] {
            let stop = NativeStop::Work(ordinal);
            let mut control = NativeControl {
                stop: Some(stop),
                ..Default::default()
            };
            let result = call(name).execute(&kept, limits(1 << 20), &mut control);
            if ordinal == loop_unit {
                assert!(matches!(
                    result,
                    Err(FnxExecutionError::Cancelled(observed)) if observed == stop
                ));
            } else {
                assert!(matches!(result, Err(FnxExecutionError::SelfLoopRefused)));
            }
            assert_eq!(control.units, loop_unit);
        }
        let dropped = view(&[0, 17, u128::MAX], &edges, SelfLoopPolicy::Drop);
        assert!(
            call(name)
                .execute(&dropped, limits(1 << 20), &mut NativeControl::default())
                .is_ok()
        );
    }
}

#[test]
fn native_eigenvector_certifies_workspace_and_keeps_live_nonconvergence_refusals() {
    let name = "eigenvector_centrality";
    for (graph, bytes) in [
        (view(&[], &[], SelfLoopPolicy::Keep), 0),
        (view(&[u128::MAX], &[], SelfLoopPolicy::Keep), 8),
        (path(), 4 * 16),
        (
            view(
                &[u128::MAX, 0, 1 << 127],
                &[(u128::MAX, 0), (0, 1 << 127), (u128::MAX, 1 << 127)],
                SelfLoopPolicy::Keep,
            ),
            3 * 16,
        ),
    ] {
        let mut control = NativeControl::default();
        let actual = call(name)
            .execute(&graph, limits(1 << 20), &mut control)
            .unwrap();
        let (oracle, converged) =
            fnx_algorithms::eigenvector_centrality_with_params(&copied_graph(&graph), 100, 1e-6);
        assert!(converged);
        let expected: BTreeMap<_, _> = oracle
            .scores
            .into_iter()
            .map(|entry| {
                (
                    graph
                        .vertex_id(graph.get_node_index(&entry.node).unwrap())
                        .unwrap(),
                    entry.score.to_bits(),
                )
            })
            .collect();
        assert_eq!(degree_bits(&actual), expected);
        let certificate = &actual.certificate;
        assert_eq!(
            certificate.execution_kernel,
            "fgdb-prism/eigenvector-csr-gather-v1"
        );
        assert_eq!(certificate.registry_version, FNX_SIGNATURE_REGISTRY_VERSION);
        assert_eq!(certificate.kernel_workspace_bytes, bytes);
        assert_eq!(certificate.witness, oracle.witness);
        assert_eq!(control.units, 4 * graph.node_count() + 5);
    }

    // This path does not converge within the registered 100 iterations. Its
    // bounded refusal must remain distinct from cancellation at any live poll.
    let n = 129;
    let vertices: Vec<_> = (0..n).map(|i| ((i as u128) << 64) | 17).collect();
    let edges: Vec<_> = vertices.windows(2).map(|pair| (pair[0], pair[1])).collect();
    let graph = view(&vertices, &edges, SelfLoopPolicy::Keep);
    let (oracle, converged) =
        fnx_algorithms::eigenvector_centrality_with_params(&copied_graph(&graph), 100, 1e-6);
    assert!(!converged);
    let mut complete = NativeControl::default();
    let witness = match call(name).execute(&graph, limits(1 << 20), &mut complete) {
        Err(FnxExecutionError::NotConverged {
            max_iterations,
            witness,
        }) => {
            assert_eq!(max_iterations, 100);
            witness
        }
        other => panic!("bounded eigenvector refusal required: {other:?}"),
    };
    assert_eq!(witness, oracle.witness);
    assert_eq!(witness.nodes_touched, 100 * n);
    assert_eq!(witness.edges_scanned, 100 * 2 * (n - 1));
    assert_eq!(witness.queue_peak, 0);
    assert_eq!(complete.units, 3 * n + 2);
    assert!(complete.polls.len() >= (100 * n).div_ceil(4_096));
    assert!(complete.polls.iter().all(|&units| units == 3 * n + 2));
    for ordinal in 1..=complete.polls.len() {
        let stop = NativeStop::Poll(ordinal);
        let mut control = NativeControl {
            stop: Some(stop),
            ..Default::default()
        };
        let result = call(name).execute(&graph, limits(1 << 20), &mut control);
        assert!(
            matches!(result, Err(FnxExecutionError::Cancelled(observed)) if observed == stop),
            "eigenvector poll {ordinal}"
        );
        assert_eq!(control.polls.len(), ordinal);
        assert_eq!(control.units, complete.polls[ordinal - 1]);
        assert!(control.stopped);
    }
}

#[test]
fn native_foundation_polls_during_precharged_arithmetic_and_stops_immediately() {
    const ARITHMETIC_RUN: usize = 4_096;
    let n = 2 * ARITHMETIC_RUN + 7;
    let vertices: Vec<_> = (0..n).map(|i| ((i as u128) << 64) | 17).collect();
    let edges: Vec<_> = vertices[1..]
        .iter()
        .map(|&target| (vertices[0], target))
        .collect();
    let graph = view(&vertices, &edges, SelfLoopPolicy::Keep);
    let execution_limits = FnxExecutionLimits {
        max_result_rows: n,
        ..limits(1 << 30)
    };

    for name in ["degree_centrality", "core_number"] {
        let mut complete = NativeControl::default();
        let result = call(name)
            .execute(&graph, execution_limits, &mut complete)
            .unwrap();
        assert_eq!(result.rows.len(), n);
        assert_eq!(complete.units, 4 * n + 5, "{name}");
        // Arithmetic follows the admitted old copy phases, without additional
        // work charges. Even one hub row must allow control to be observed.
        let during_arithmetic = complete
            .polls
            .iter()
            .filter(|&&units| units == 3 * n + 2)
            .count();
        assert!(
            during_arithmetic >= n.div_ceil(ARITHMETIC_RUN),
            "{name}: {complete:?}"
        );
        for ordinal in 1..=complete.polls.len() {
            let stop = NativeStop::Poll(ordinal);
            let mut control = NativeControl {
                stop: Some(stop),
                ..Default::default()
            };
            let result = call(name).execute(&graph, execution_limits, &mut control);
            assert!(
                matches!(result, Err(FnxExecutionError::Cancelled(observed)) if observed == stop),
                "{name} poll {ordinal}"
            );
            assert_eq!(control.polls.len(), ordinal, "{name} poll {ordinal}");
            assert_eq!(
                control.units,
                complete.polls[ordinal - 1],
                "{name} poll {ordinal}"
            );
            assert!(control.stopped);
        }
    }
}

#[test]
fn native_foundation_certifies_vertex_workspace_and_keeps_preflight_limits() {
    let n = 97;
    let vertices: Vec<_> = (0..n).map(|i| ((i as u128) << 64) | 17).collect();
    let star: Vec<_> = vertices[1..]
        .iter()
        .map(|&target| (vertices[0], target))
        .collect();
    let clique: Vec<_> = (0..n)
        .flat_map(|source| ((source + 1)..n).map(move |target| (source, target)))
        .map(|(source, target)| (vertices[source], vertices[target]))
        .collect();
    for edges in [Vec::new(), star, clique] {
        let graph = view(&vertices, &edges, SelfLoopPolicy::Keep);
        let expected_work = n + 2 * edges.len();
        let bins = if edges.is_empty() { 1 } else { n };
        for (name, kernel, bytes) in [
            (
                "degree_centrality",
                "fgdb-prism/degree-centrality-csr-v1",
                n * size_of::<f64>(),
            ),
            (
                "core_number",
                "fgdb-prism/core-number-bucket-csr-v1",
                (3 * n + bins) * size_of::<usize>(),
            ),
        ] {
            let result = run(name, &graph);
            let certificate = &result.certificate;
            assert_eq!(certificate.execution_kernel, kernel);
            assert_eq!(certificate.registry_version, FNX_SIGNATURE_REGISTRY_VERSION);
            // Star and clique have max degree n-1, yet radically different
            // edge counts. Isolates need one bin. Native workspace must not
            // contain the old copied arcs or a padded n-entry bucket table.
            assert_eq!(certificate.kernel_workspace_bytes, bytes, "{name}");
            assert_eq!(certificate.estimated_work, expected_work);
            assert_eq!(certificate.witness.nodes_touched, n);
            assert_eq!(certificate.witness.edges_scanned, 2 * edges.len());
            assert_eq!(certificate.witness.queue_peak, 0);

            let is_star = edges.len() == n - 1;
            if name == "core_number" {
                let expected = if edges.is_empty() {
                    0
                } else if is_star {
                    1
                } else {
                    (n - 1) as u64
                };
                assert!(core_values(&result).values().all(|&core| core == expected));
            } else {
                for (vertex, bits) in degree_bits(&result) {
                    let expected = if edges.is_empty() {
                        0.0
                    } else if is_star && vertex != VId(vertices[0]) {
                        1.0 / (n - 1) as f64
                    } else {
                        1.0
                    };
                    assert_eq!(bits, expected.to_bits());
                }
            }

            let mut control = NativeControl::default();
            let refused = call(name).execute(&graph, limits(expected_work - 1), &mut control);
            assert!(matches!(
                refused,
                Err(FnxExecutionError::LimitExceeded {
                    resource: "estimated work", limit, requested
                }) if limit == expected_work - 1 && requested == expected_work
            ));
            assert_eq!(control.units, n + 1);
            assert!(control.polls.is_empty());

            let mut control = NativeControl::default();
            let refused = call(name).execute(
                &graph,
                FnxExecutionLimits {
                    max_result_rows: n - 1,
                    ..limits(1 << 30)
                },
                &mut control,
            );
            assert!(matches!(
                refused,
                Err(FnxExecutionError::LimitExceeded {
                    resource: "result rows", limit, requested
                }) if limit == n - 1 && requested == n
            ));
            assert_eq!(control.units, 1);
            assert!(control.polls.is_empty());
        }
    }
}
