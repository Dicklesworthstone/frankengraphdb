//! Exact standalone-FNX differentials for the registered minimum spanning
//! forest, including weighted ties, disconnected graphs and full-width IDs.

use fgdb_prism::*;
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use fnx_classes::{AttrMap, Graph};
use std::convert::Infallible;

fn limits() -> FnxExecutionLimits {
    FnxExecutionLimits {
        max_iterations: 0,
        max_result_rows: 1000,
        max_estimated_work: 1 << 24,
    }
}

fn view(
    vertices: &[VId],
    edges: &[ProjectionEdge],
    parallel_edges: ParallelEdgePolicy,
) -> SnapshotGraphView {
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([0x53; 32]),
            as_of: CommitSeq(7),
        },
        vertices,
        edges,
        ProjectionSpec {
            directedness: Directedness::Undirected,
            parallel_edges,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: 1000,
            max_input_edges: 10000,
            max_adjacency_entries: 20000,
            max_workspace_bytes: 1 << 24,
        },
    )
    .unwrap()
}

fn edge(id: usize, a: VId, b: VId, weight: f64) -> ProjectionEdge {
    ProjectionEdge {
        eid: EId((1u128 << 109) + id as u128),
        source: a,
        target: b,
        weight,
    }
}

fn execute(graph: &SnapshotGraphView) -> FnxResult {
    FnxCallSpec::minimum_spanning_tree()
        .execute(graph, limits(), || Ok::<(), Infallible>(()))
        .unwrap()
}

fn rows(result: &FnxResult) -> Vec<(VId, VId, u64)> {
    result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [
                FnxValue::Vertex(a),
                FnxValue::Vertex(b),
                FnxValue::Float(weight),
            ] => (*a, *b, weight.to_bits()),
            _ => panic!("weighted endpoint row: {row:?}"),
        })
        .collect()
}

fn oracle(graph: &SnapshotGraphView) -> Vec<(VId, VId, u64)> {
    // The public projection contract pins vertex AND adjacency order. FNX's
    // stable weight sort deliberately preserves that order for equal weights.
    let mut copy = Graph::strict();
    let vertices = graph.vertex_ids();
    let names: Vec<_> = vertices.iter().map(|vertex| vertex.0.to_string()).collect();
    for name in &names {
        copy.add_node(name.clone());
    }
    for source in 0..vertices.len() {
        let (neighbors, weights) = graph.projected_row(source).unwrap();
        for (&target, &weight) in neighbors.iter().zip(weights) {
            if source <= target {
                copy.add_edge_with_attrs(
                    names[source].clone(),
                    names[target].clone(),
                    AttrMap::from([("weight".to_owned(), weight.to_string().into())]),
                )
                .unwrap();
            }
        }
    }
    assert_eq!(
        copy.nodes_ordered(),
        names.iter().map(String::as_str).collect::<Vec<_>>()
    );
    fnx_algorithms::minimum_spanning_tree(&copy, "weight")
        .edges
        .into_iter()
        .map(|item| {
            (
                VId(item.left.parse().unwrap()),
                VId(item.right.parse().unwrap()),
                item.weight.to_bits(),
            )
        })
        .collect()
}

#[test]
fn every_small_graph_and_seeded_weighted_graph_matches_canonical_fnx_kruskal() {
    let vertices = [VId(u128::MAX), VId(0), VId(1 << 100), VId(29), VId(101)];
    for n in 0..=5 {
        let pairs: Vec<_> = (0..n)
            .flat_map(|a| (a + 1..n).map(move |b| (a, b)))
            .collect();
        for mask in 0usize..1 << pairs.len() {
            for mode in 0..3 {
                let mut edges: Vec<_> = pairs
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(bit, &(a, b))| {
                        edge(
                            bit,
                            vertices[a],
                            vertices[b],
                            match mode {
                                0 => 1.0,
                                1 => [-4.0, -0.0, 0.0, 2.5][bit % 4],
                                _ => ((bit * 7 + mask) % 11) as f64 - 5.0,
                            },
                        )
                    })
                    .collect();
                let graph = view(&vertices[..n], &edges, ParallelEdgePolicy::Reject);
                let expected = oracle(&graph);
                let result = execute(&graph);
                assert_eq!(rows(&result), expected, "n={n},mask={mask},mode={mode}");
                assert_eq!(result.certificate.witness.nodes_touched, n);
                assert_eq!(result.certificate.witness.edges_scanned, edges.len() * 2);
                edges.reverse();
                let reversed: Vec<_> = vertices[..n].iter().rev().copied().collect();
                assert_eq!(
                    rows(&execute(&view(
                        &reversed,
                        &edges,
                        ParallelEdgePolicy::Reject
                    ))),
                    expected
                );
            }
        }
    }
    for seed in 0..24u64 {
        let vertices: Vec<_> = (0..23)
            .rev()
            .map(|i| VId((1u128 << 113) + i * 7919))
            .collect();
        let mut random = seed + 1;
        let mut edges = Vec::new();
        for a in 0..vertices.len() {
            for b in a + 1..vertices.len() {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                if random >> 61 <= 2 {
                    edges.push(edge(
                        edges.len(),
                        vertices[a],
                        vertices[b],
                        ((random >> 32) % 19) as f64 - 9.0,
                    ));
                }
            }
        }
        let graph = view(&vertices, &edges, ParallelEdgePolicy::Reject);
        assert_eq!(rows(&execute(&graph)), oracle(&graph), "seed={seed}");
    }
}

#[test]
fn signed_zero_ties_loops_parallel_reduction_and_isolates_have_literal_results() {
    let v = [VId(0), VId(1 << 100), VId(u128::MAX - 1), VId(u128::MAX)];
    let edges = [
        edge(0, v[0], v[1], 0.0),
        edge(1, v[0], v[2], -0.0),
        edge(2, v[1], v[2], -0.0),
        edge(3, v[3], v[3], -99.0),
    ];
    let graph = view(&v, &edges, ParallelEdgePolicy::Reject);
    assert_eq!(
        rows(&execute(&graph)),
        vec![
            (v[0], v[1], 0.0f64.to_bits()),
            // Projection admission canonicalizes signed zero to +0. The
            // kernel's private row law separately preserves numeric zero ties.
            (v[0], v[2], 0.0f64.to_bits())
        ]
    );
    assert_eq!(rows(&execute(&graph)), oracle(&graph));
    let parallel = [
        edge(0, v[0], v[1], 7.0),
        edge(1, v[1], v[0], -2.0),
        edge(2, v[1], v[2], 3.0),
        edge(3, v[0], v[2], 4.0),
    ];
    for policy in [
        ParallelEdgePolicy::Minimum,
        ParallelEdgePolicy::Maximum,
        ParallelEdgePolicy::Sum,
    ] {
        let graph = view(&v, &parallel, policy);
        assert_eq!(rows(&execute(&graph)), oracle(&graph));
    }
}

#[test]
fn registered_schema_retained_output_and_every_cancellation_preserve_the_forest() {
    let vertices = [VId(0), VId(7), VId(1 << 100), VId(u128::MAX)];
    let graph = view(
        &vertices,
        &[
            edge(0, vertices[0], vertices[1], 3.0),
            edge(1, vertices[0], vertices[2], -2.0),
            edge(2, vertices[1], vertices[2], 0.0),
        ],
        ParallelEdgePolicy::Reject,
    );
    let call = FnxCallSpec::bind(
        "CALL fnx.minimum_spanning_tree() YIELD neighbor AS target,weight,node AS source",
        &FnxParameters::new(),
    )
    .unwrap();
    assert_eq!(call.outputs()[1].field, FnxOutput::Weight);
    assert!(!FnxOutput::Weight.is_vertex_reference());
    assert!(call.supports_sealed_execution());
    assert!(call.supports_cooperative_sealed_execution());
    for text in [
        "CALL fnx.minimum_spanning_tree(1)",
        "CALL fnx.minimum_spanning_tree() YIELD score",
        "CALL fnx.minimum_spanning_tree() YIELD weight,weight",
    ] {
        assert!(
            FnxCallSpec::bind(text, &FnxParameters::new()).is_err(),
            "{text}"
        );
    }
    let mut checkpoints = 0;
    let expected = call
        .execute(&graph, limits(), || {
            checkpoints += 1;
            Ok::<(), &'static str>(())
        })
        .unwrap();
    assert!(checkpoints <= expected.certificate.estimated_work);
    for stop in 1..=checkpoints {
        let mut count = 0;
        assert!(matches!(
            call.execute(&graph, limits(), || {
                count += 1;
                if count == stop { Err("stop") } else { Ok(()) }
            }),
            Err(FnxExecutionError::Cancelled("stop"))
        ));
    }
    let mut mapped = Vec::new();
    assert_eq!(
        call.execute_into(&graph, limits(), || Ok::<(), Infallible>(()), &mut mapped)
            .unwrap(),
        expected.certificate
    );
    assert_eq!(mapped, expected.rows);
    let mut cursor = call
        .execute_rows(&graph, limits(), || Ok::<(), Infallible>(()))
        .unwrap();
    let mut retained = Vec::new();
    let mut row = Vec::new();
    while cursor
        .next_row(&mut row, || Ok::<(), Infallible>(()))
        .unwrap()
    {
        retained.push(row.clone());
    }
    assert_eq!(retained, expected.rows);
    let exact = FnxExecutionLimits {
        max_result_rows: expected.rows.len(),
        max_estimated_work: expected.certificate.estimated_work,
        ..limits()
    };
    assert_eq!(
        call.execute(&graph, exact, || Ok::<(), Infallible>(()))
            .unwrap(),
        expected
    );
    for bad in [
        FnxExecutionLimits {
            max_result_rows: 1,
            ..exact
        },
        FnxExecutionLimits {
            max_estimated_work: exact.max_estimated_work - 1,
            ..exact
        },
    ] {
        assert!(matches!(
            call.execute(&graph, bad, || Ok::<(), Infallible>(())),
            Err(FnxExecutionError::LimitExceeded { .. })
        ));
    }
    assert!(matches!(
        call.validate_sealed_projection(Directedness::Directed),
        Err(FnxSealedExecutionError::Execution(
            FnxExecutionError::GraphKind {
                required: FnxGraphKind::Undirected
            }
        ))
    ));
}
