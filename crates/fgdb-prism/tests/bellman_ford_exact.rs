//! Integer-domain FNX differentials and independent exact-distance fixtures.
//! The registered exact-dyadic profile deliberately differs from FNX's
//! epsilon/rounded-relaxation policy outside the shared exact integer domain.

use fgdb_prism::*;
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use fnx_classes::{AttrMap, Graph, digraph::DiGraph};
use std::convert::Infallible;

fn limits() -> FnxExecutionLimits {
    FnxExecutionLimits {
        max_iterations: 0,
        max_result_rows: 100,
        max_estimated_work: 1 << 24,
    }
}

fn view(
    vertices: &[VId],
    edges: &[ProjectionEdge],
    directedness: Directedness,
    parallel_edges: ParallelEdgePolicy,
) -> SnapshotGraphView {
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([0xb3; 32]),
            as_of: CommitSeq(19),
        },
        vertices,
        edges,
        ProjectionSpec {
            directedness,
            parallel_edges,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: 100,
            max_input_edges: 1000,
            max_adjacency_entries: 2000,
            max_workspace_bytes: 1 << 22,
        },
    )
    .unwrap()
}

fn edge(id: usize, source: VId, target: VId, weight: f64) -> ProjectionEdge {
    ProjectionEdge {
        eid: EId((1 << 109) + id as u128),
        source,
        target,
        weight,
    }
}

fn run(graph: &SnapshotGraphView, source: VId) -> Result<FnxResult, FnxExecutionError<Infallible>> {
    FnxCallSpec::single_source_bellman_ford_path_length(source).execute(graph, limits(), || Ok(()))
}

fn rows(result: &FnxResult) -> Vec<(VId, u64)> {
    result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [FnxValue::Vertex(vertex), FnxValue::Float(distance)] => (*vertex, distance.to_bits()),
            other => panic!("expected a weighted-distance row, got {other:?}"),
        })
        .collect()
}

fn oracle(graph: &SnapshotGraphView, source: VId) -> Option<Vec<(VId, u64)>> {
    // Build from the admitted projection, including its explicit reduction
    // and direction, so no raw or unscoped input enters the comparison.
    let names: Vec<_> = graph
        .vertex_ids()
        .iter()
        .map(|vertex| vertex.0.to_string())
        .collect();
    let source = source.0.to_string();
    let distances = if graph.spec().directedness == Directedness::Undirected {
        let mut copy = Graph::strict();
        for name in &names {
            copy.add_node(name.clone());
        }
        for u in 0..names.len() {
            let (neighbors, weights) = graph.projected_row(u).unwrap();
            for (&v, &weight) in neighbors.iter().zip(weights) {
                if u <= v {
                    copy.add_edge_with_attrs(
                        names[u].clone(),
                        names[v].clone(),
                        AttrMap::from([("weight".into(), weight.to_string().into())]),
                    )
                    .unwrap();
                }
            }
        }
        fnx_algorithms::single_source_bellman_ford_path_length(&copy, &source, "weight")
    } else {
        let mut copy = DiGraph::strict();
        for name in &names {
            copy.add_node(name.clone());
        }
        for u in 0..names.len() {
            let (neighbors, weights) = graph.projected_row(u).unwrap();
            for (&v, &weight) in neighbors.iter().zip(weights) {
                copy.add_edge_with_attrs(
                    names[u].clone(),
                    names[v].clone(),
                    AttrMap::from([("weight".into(), weight.to_string().into())]),
                )
                .unwrap();
            }
        }
        fnx_algorithms::single_source_bellman_ford_path_length_directed(&copy, &source, "weight")
    }?;
    let mut result: Vec<_> = distances
        .into_iter()
        .map(|(vertex, distance)| (VId(vertex.parse().unwrap()), distance.to_bits()))
        .collect();
    result.sort();
    Some(result)
}

#[test]
fn signed_integer_dags_and_reweighted_cyclic_graphs_match_the_pinned_fnx_oracle() {
    let vertices = [VId(u128::MAX), VId(0), VId(1 << 100), VId(19)];
    let pairs: Vec<_> = (0..4)
        .flat_map(|a| (a + 1..4).map(move |b| (a, b)))
        .collect();
    for mask in 0_usize..1 << pairs.len() {
        let edges: Vec<_> = pairs
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask & (1 << bit) != 0)
            .map(|(id, &(a, b))| edge(id, vertices[a], vertices[b], (id % 5) as f64 - 2.0))
            .collect();
        for direction in [Directedness::Directed, Directedness::Reversed] {
            let graph = view(&vertices, &edges, direction, ParallelEdgePolicy::Reject);
            for source in vertices {
                assert_eq!(
                    rows(&run(&graph, source).unwrap()),
                    oracle(&graph, source).unwrap(),
                    "mask={mask},direction={direction:?},source={source:?}"
                );
            }
        }
    }

    for seed in 1..=24_u64 {
        let vertices: Vec<_> = (0..9).rev().map(|i| VId((1 << 120) + i * 7919)).collect();
        let potential: Vec<_> = (0..9).map(|i| ((i * 7 + seed) % 19) as i64 - 9).collect();
        let mut random = seed;
        let mut edges = Vec::new();
        for a in 0..vertices.len() {
            for b in 0..vertices.len() {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                if a != b && (b == a + 1 || random >> 61 < 2) {
                    // Every cycle sums nonnegative base weights: potentials
                    // cancel, while individual edges may be negative.
                    let base = ((random >> 32) % 4) as i64;
                    let weight = (base + potential[b] - potential[a]) as f64;
                    edges.push(edge(edges.len(), vertices[a], vertices[b], weight));
                }
            }
        }
        let graph = view(
            &vertices,
            &edges,
            Directedness::Directed,
            ParallelEdgePolicy::Reject,
        );
        for source in &vertices {
            assert_eq!(
                rows(&run(&graph, *source).unwrap()),
                oracle(&graph, *source).unwrap(),
                "seed={seed},source={source:?}"
            );
        }
    }
}

#[test]
fn exact_profile_preserves_low_bits_and_allows_wide_tentative_distances() {
    let vertices = [VId(0), VId(1), VId(2), VId(3), VId(4)];
    let graph = view(
        &vertices[..4],
        &[
            edge(0, vertices[0], vertices[1], 1e20),
            edge(1, vertices[1], vertices[2], 1.0),
            edge(2, vertices[2], vertices[3], -1e20),
            edge(3, vertices[0], vertices[3], 0.5),
        ],
        Directedness::Directed,
        ParallelEdgePolicy::Reject,
    );
    let result = run(&graph, vertices[0]).unwrap();
    assert_eq!(
        rows(&result),
        vec![
            (vertices[0], 0.0_f64.to_bits()),
            (vertices[1], 1e20_f64.to_bits()),
            (vertices[2], 1e20_f64.to_bits()),
            // The alternate path is exactly 1, so the direct 0.5 wins.
            (vertices[3], 0.5_f64.to_bits()),
        ]
    );
    // FNX rounds away the +1 before cancellation and instead reports zero.
    // The registry must identify our stronger numerical contract explicitly.
    assert_eq!(oracle(&graph, vertices[0]).unwrap()[3].1, 0.0_f64.to_bits());
    assert!(
        FnxSignatureRegistry::lookup("fnx.single_source_bellman_ford_path_length")
            .unwrap()
            .numeric_profile
            .contains("exact-dyadic")
    );

    let graph = view(
        &vertices,
        &[
            edge(0, vertices[0], vertices[1], f64::MAX),
            edge(1, vertices[0], vertices[4], 0.0),
            edge(2, vertices[1], vertices[2], f64::MAX),
            edge(3, vertices[2], vertices[3], -f64::MAX),
            edge(4, vertices[4], vertices[2], f64::MAX),
        ],
        Directedness::Directed,
        ParallelEdgePolicy::Reject,
    );
    assert_eq!(
        rows(&run(&graph, vertices[0]).unwrap()),
        vec![
            (vertices[0], 0.0_f64.to_bits()),
            (vertices[1], f64::MAX.to_bits()),
            (vertices[2], f64::MAX.to_bits()),
            (vertices[3], 0.0_f64.to_bits()),
            (vertices[4], 0.0_f64.to_bits()),
        ]
    );
}

#[test]
fn negative_cycles_cannot_hide_below_an_epsilon_or_a_large_prefix() {
    let vertices = [VId(0), VId(1 << 100), VId(u128::MAX)];
    for edges in [
        vec![edge(0, vertices[0], vertices[0], -f64::from_bits(1))],
        vec![
            edge(0, vertices[0], vertices[1], 1e20),
            edge(1, vertices[1], vertices[2], -1.0),
            edge(2, vertices[2], vertices[1], 0.0),
        ],
        vec![
            edge(0, vertices[0], vertices[1], f64::MAX),
            edge(1, vertices[1], vertices[2], f64::MAX),
            edge(2, vertices[2], vertices[2], -1.0),
        ],
    ] {
        let graph = view(
            &vertices,
            &edges,
            Directedness::Directed,
            ParallelEdgePolicy::Reject,
        );
        assert!(matches!(
            run(&graph, vertices[0]),
            Err(FnxExecutionError::ReachableNegativeCycle)
        ));
    }
    // An undirected negative edge is a two-step negative walk. An isolated
    // source cannot reach it, so the same projection is valid for that source.
    let graph = view(
        &vertices,
        &[edge(0, vertices[0], vertices[1], -1.0)],
        Directedness::Undirected,
        ParallelEdgePolicy::Reject,
    );
    assert!(matches!(
        run(&graph, vertices[0]),
        Err(FnxExecutionError::ReachableNegativeCycle)
    ));
    assert_eq!(oracle(&graph, vertices[0]), None);
    assert_eq!(
        rows(&run(&graph, vertices[2]).unwrap()),
        vec![(vertices[2], 0.0_f64.to_bits())]
    );
    assert_eq!(
        rows(&run(&graph, vertices[2]).unwrap()),
        oracle(&graph, vertices[2]).unwrap()
    );
}

#[test]
fn final_non_finite_distances_refuse_without_confusing_them_with_negative_cycles() {
    let vertices = [VId(0), VId(1), VId(2)];
    for weight in [f64::MAX, -f64::MAX] {
        let graph = view(
            &vertices,
            &[
                edge(0, vertices[0], vertices[1], weight),
                edge(1, vertices[1], vertices[2], weight),
            ],
            Directedness::Directed,
            ParallelEdgePolicy::Reject,
        );
        assert!(matches!(
            run(&graph, vertices[0]),
            Err(FnxExecutionError::NonFiniteDistanceSum)
        ));
    }
}

#[test]
fn explicit_parallel_edge_reduction_precedes_the_signed_distance_problem() {
    let vertices = [VId(0), VId(1 << 100), VId(u128::MAX)];
    let edges = [
        edge(0, vertices[0], vertices[1], -3.0),
        edge(1, vertices[0], vertices[1], 5.0),
        edge(2, vertices[1], vertices[2], 2.0),
        edge(3, vertices[0], vertices[2], 7.0),
    ];
    for (reducer, expected) in [
        (ParallelEdgePolicy::Minimum, [0.0_f64, -3.0, -1.0]),
        (ParallelEdgePolicy::Maximum, [0.0_f64, 5.0, 7.0]),
        (ParallelEdgePolicy::Sum, [0.0_f64, 2.0, 4.0]),
    ] {
        let graph = view(&vertices, &edges, Directedness::Directed, reducer);
        let result = rows(&run(&graph, vertices[0]).unwrap());
        assert_eq!(
            result,
            vertices
                .iter()
                .copied()
                .zip(expected.map(f64::to_bits))
                .collect::<Vec<_>>()
        );
        assert_eq!(result, oracle(&graph, vertices[0]).unwrap());
    }
}
