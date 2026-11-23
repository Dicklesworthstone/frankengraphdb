//! Pinned FNX differentials and public delivery laws for directed DAG calls.

use fgdb_prism::*;
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use fnx_classes::digraph::DiGraph;
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
    edges: &[(VId, VId, f64)],
    direction: Directedness,
    parallel_edges: ParallelEdgePolicy,
) -> SnapshotGraphView {
    let edges: Vec<_> = edges
        .iter()
        .enumerate()
        .map(|(index, &(source, target, weight))| ProjectionEdge {
            eid: EId((1 << 110) + index as u128),
            source,
            target,
            weight,
        })
        .collect();
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([0x44; 32]),
            as_of: CommitSeq(12),
        },
        vertices,
        &edges,
        ProjectionSpec {
            directedness: direction,
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

fn oracle(graph: &SnapshotGraphView) -> DiGraph {
    let mut reference = DiGraph::strict();
    for vertex in graph.vertex_ids() {
        reference.add_node(vertex.0.to_string());
    }
    for source in 0..graph.node_count() {
        for &target in graph.neighbors_indices(source).unwrap() {
            reference
                .add_edge(
                    graph.vertex_ids()[source].0.to_string(),
                    graph.vertex_ids()[target].0.to_string(),
                )
                .unwrap();
        }
    }
    reference
}

fn check(graph: &SnapshotGraphView) {
    let reference = oracle(graph);
    let expected = fnx_algorithms::topological_sort(&reference);
    let acyclic = fnx_algorithms::is_directed_acyclic_graph(&reference);
    assert_eq!(expected.is_some(), acyclic);
    let predicate = FnxCallSpec::is_directed_acyclic_graph()
        .execute(
            graph,
            FnxExecutionLimits {
                max_result_rows: 1,
                ..limits()
            },
            || Ok::<(), Infallible>(()),
        )
        .unwrap();
    assert_eq!(predicate.rows, vec![vec![FnxValue::Boolean(acyclic)]]);
    let actual =
        FnxCallSpec::topological_sort().execute(graph, limits(), || Ok::<(), Infallible>(()));
    match expected {
        None => assert!(matches!(actual, Err(FnxExecutionError::DirectedCycle))),
        Some(expected) => {
            let expected: Vec<_> = expected
                .order
                .iter()
                .enumerate()
                .map(|(rank, name)| {
                    vec![
                        FnxValue::Vertex(VId(name.parse().unwrap())),
                        FnxValue::Integer(rank as u64),
                    ]
                })
                .collect();
            assert_eq!(actual.unwrap().rows, expected);
        }
    }
}

#[test]
fn every_three_vertex_digraph_including_self_loops_matches_pinned_fnx() {
    for n in 0..=3 {
        let vertices: Vec<_> = (0..n).map(|i| VId((1 << 100) + i as u128)).collect();
        let arcs: Vec<_> = (0..n)
            .flat_map(|source| (0..n).map(move |target| (source, target)))
            .collect();
        for mask in 0usize..1 << arcs.len() {
            let edges: Vec<_> = arcs
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, &(source, target))| (vertices[source], vertices[target], -3.0))
                .collect();
            check(&view(
                &vertices,
                &edges,
                Directedness::Directed,
                ParallelEdgePolicy::Reject,
            ));
        }
    }
}

#[test]
fn disconnected_ties_full_width_ids_parallel_reduction_and_reversal_are_explicit() {
    let vertices = [VId(u128::MAX), VId(0), VId(17), VId(1 << 100), VId(99)];
    let edges = [
        (VId(u128::MAX), VId(0), -4.0),
        (VId(u128::MAX), VId(0), 4.0),
        (VId(u128::MAX), VId(17), 0.0),
        (VId(17), VId(1 << 100), 2.0),
        (VId(0), VId(1 << 100), 1.0),
    ];
    for direction in [Directedness::Directed, Directedness::Reversed] {
        for policy in [ParallelEdgePolicy::Sum, ParallelEdgePolicy::CollapseUnit] {
            let graph = view(&vertices, &edges, direction, policy);
            check(&graph);
            let mut shuffled_vertices = vertices;
            let mut shuffled_edges = edges;
            shuffled_vertices.reverse();
            shuffled_edges.reverse();
            let shuffled = view(&shuffled_vertices, &shuffled_edges, direction, policy);
            for call in [
                FnxCallSpec::topological_sort(),
                FnxCallSpec::is_directed_acyclic_graph(),
            ] {
                assert_eq!(
                    call.execute(&graph, limits(), || Ok::<(), Infallible>(()))
                        .unwrap()
                        .rows,
                    call.execute(&shuffled, limits(), || Ok::<(), Infallible>(()))
                        .unwrap()
                        .rows,
                );
            }
        }
    }
}

#[test]
fn binding_reordered_yields_retained_delivery_and_every_cancellation_share_one_result() {
    let parameters = FnxParameters::new();
    assert_eq!(
        FnxCallSpec::bind("CALL fnx.topological_sort()", &parameters).unwrap(),
        FnxCallSpec::topological_sort(),
    );
    assert_eq!(
        FnxCallSpec::bind("CALL fnx.is_directed_acyclic_graph()", &parameters).unwrap(),
        FnxCallSpec::is_directed_acyclic_graph(),
    );
    for invalid in [
        "CALL fnx.topological_sort(1)",
        "CALL fnx.topological_sort() YIELD acyclic",
        "CALL fnx.is_directed_acyclic_graph(true)",
        "CALL fnx.is_directed_acyclic_graph() YIELD node",
    ] {
        assert!(
            FnxCallSpec::bind(invalid, &parameters).is_err(),
            "{invalid}"
        );
    }
    let graph = view(
        &[VId(0), VId(7), VId(u128::MAX)],
        &[(VId(u128::MAX), VId(0), 1.0)],
        Directedness::Directed,
        ParallelEdgePolicy::Reject,
    );
    for text in [
        "CALL fnx.topological_sort() YIELD rank AS position,node AS dependency",
        "CALL fnx.is_directed_acyclic_graph() YIELD acyclic AS valid",
    ] {
        let call = FnxCallSpec::bind(text, &parameters).unwrap();
        assert!(call.supports_sealed_execution());
        assert!(call.supports_cooperative_sealed_execution());
        let mut checks = 0;
        let expected = call
            .execute(&graph, limits(), || {
                checks += 1;
                Ok::<(), &'static str>(())
            })
            .unwrap();
        assert!(checks <= expected.certificate.estimated_work);
        for stop in 1..=checks {
            let mut at = 0;
            assert!(matches!(
                call.execute(&graph, limits(), || {
                    at += 1;
                    if at == stop { Err("cancelled") } else { Ok(()) }
                }),
                Err(FnxExecutionError::Cancelled("cancelled"))
            ));
            assert_eq!(at, stop);
        }
        let mut mapped = Vec::new();
        assert_eq!(
            call.execute_into(&graph, limits(), || Ok::<(), Infallible>(()), &mut mapped)
                .unwrap(),
            expected.certificate,
        );
        assert_eq!(mapped, expected.rows);
        let mut cursor = call
            .execute_rows(&graph, limits(), || Ok::<(), Infallible>(()))
            .unwrap();
        assert_eq!(cursor.certificate(), &expected.certificate);
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
                max_result_rows: exact.max_result_rows - 1,
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
    }
}

#[test]
fn empty_predicate_is_one_boolean_and_cycles_never_publish_an_ordering_prefix() {
    let empty = view(&[], &[], Directedness::Directed, ParallelEdgePolicy::Reject);
    assert!(matches!(
        FnxCallSpec::is_directed_acyclic_graph().execute(
            &empty,
            FnxExecutionLimits {
                max_result_rows: 0,
                ..limits()
            },
            || Ok::<(), Infallible>(()),
        ),
        Err(FnxExecutionError::LimitExceeded {
            resource: "result rows",
            requested: 1,
            limit: 0
        })
    ));
    assert_eq!(
        FnxCallSpec::is_directed_acyclic_graph()
            .execute(&empty, limits(), || Ok::<(), Infallible>(()))
            .unwrap()
            .rows,
        vec![vec![FnxValue::Boolean(true)]],
    );
    assert!(
        FnxCallSpec::topological_sort()
            .execute(
                &empty,
                FnxExecutionLimits {
                    max_result_rows: 0,
                    ..limits()
                },
                || Ok::<(), Infallible>(()),
            )
            .unwrap()
            .rows
            .is_empty()
    );
    for edges in [
        vec![(VId(7), VId(7), 0.0)],
        vec![(VId(7), VId(9), 1.0), (VId(9), VId(7), -1.0)],
    ] {
        let graph = view(
            &[VId(0), VId(7), VId(9)],
            &edges,
            Directedness::Directed,
            ParallelEdgePolicy::Reject,
        );
        assert_eq!(
            FnxCallSpec::is_directed_acyclic_graph()
                .execute(&graph, limits(), || Ok::<(), Infallible>(()))
                .unwrap()
                .rows,
            vec![vec![FnxValue::Boolean(false)]],
        );
        let mut sink = Vec::new();
        assert!(matches!(
            FnxCallSpec::topological_sort().execute_into(
                &graph,
                limits(),
                || Ok::<(), Infallible>(()),
                &mut sink,
            ),
            Err(FnxExecutionError::DirectedCycle)
        ));
        assert!(sink.is_empty());
        check(&graph);
    }
    let undirected = view(
        &[VId(0)],
        &[],
        Directedness::Undirected,
        ParallelEdgePolicy::Reject,
    );
    for call in [
        FnxCallSpec::topological_sort(),
        FnxCallSpec::is_directed_acyclic_graph(),
    ] {
        assert!(matches!(
            call.execute(&undirected, limits(), || Ok::<(), Infallible>(())),
            Err(FnxExecutionError::GraphKind {
                required: FnxGraphKind::Directed
            })
        ));
    }
}
