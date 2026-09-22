use fgdb_prism::*;
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use fnx_classes::{Graph, digraph::DiGraph};
use std::collections::BTreeMap;
use std::convert::Infallible;

fn graph(vertices: &[VId], edges: &[ProjectionEdge], direction: Directedness) -> SnapshotGraphView {
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([8; 32]),
            as_of: CommitSeq(13),
        },
        vertices,
        edges,
        ProjectionSpec {
            directedness: direction,
            parallel_edges: ParallelEdgePolicy::Sum,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: vertices.len(),
            max_input_edges: edges.len(),
            max_adjacency_entries: edges.len() * 2,
            max_workspace_bytes: 1 << 28,
        },
    )
    .unwrap()
}
fn limits() -> FnxExecutionLimits {
    // Traversals are not iterative convergence algorithms.
    FnxExecutionLimits {
        max_iterations: 0,
        max_result_rows: 100_000,
        max_estimated_work: 1 << 24,
    }
}
fn execute(call: &FnxCallSpec, graph: &SnapshotGraphView) -> FnxResult {
    call.execute(graph, limits(), || Ok::<(), Infallible>(()))
        .unwrap()
}
fn bind(text: &str) -> FnxCallSpec {
    FnxCallSpec::bind(text, &FnxParameters::new()).unwrap()
}
fn distances(result: FnxResult) -> BTreeMap<VId, u64> {
    result
        .rows
        .into_iter()
        .map(|row| match row.as_slice() {
            [FnxValue::Vertex(vertex), FnxValue::Integer(distance)] => (*vertex, *distance),
            _ => panic!("typed distance row required"),
        })
        .collect()
}
fn labels(result: FnxResult) -> BTreeMap<VId, VId> {
    result
        .rows
        .into_iter()
        .map(|row| match row.as_slice() {
            [FnxValue::Vertex(vertex), FnxValue::Vertex(component)] => (*vertex, *component),
            _ => panic!("typed component row required"),
        })
        .collect()
}
fn oracle_labels(components: Vec<Vec<String>>, graph: &SnapshotGraphView) -> BTreeMap<VId, VId> {
    let mut result = BTreeMap::new();
    for component in components {
        let members: Vec<_> = component
            .iter()
            .map(|name| {
                graph
                    .vertex_id(graph.get_node_index(name).unwrap())
                    .unwrap()
            })
            .collect();
        let minimum = *members.iter().min().unwrap();
        for member in members {
            assert!(result.insert(member, minimum).is_none());
        }
    }
    result
}

#[test]
fn every_three_vertex_projection_matches_pinned_fnx_traversals() {
    let vertices = [VId(0), VId(17), VId(u128::MAX)];
    for mask in 0u16..512 {
        let edges: Vec<_> = (0..9)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| ProjectionEdge {
                eid: EId(bit as u128),
                source: vertices[bit / 3],
                target: vertices[bit % 3],
                // Connectivity ignores projected weights, including negative ones.
                weight: -1.0,
            })
            .collect();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let view = graph(&vertices, &edges, direction);
            let mut directed = DiGraph::strict();
            let mut undirected = Graph::strict();
            for name in view.nodes_ordered() {
                let _ = directed.add_node(name);
                let _ = undirected.add_node(name);
            }
            for source in 0..3 {
                for &target in view.neighbors_indices(source).unwrap() {
                    let from = view.get_node_name(source).unwrap();
                    let to = view.get_node_name(target).unwrap();
                    directed.add_edge(from, to).unwrap();
                    if source <= target {
                        undirected.add_edge(from, to).unwrap();
                    }
                }
            }
            for source in 0..3 {
                for cutoff in [None, Some(0), Some(1), Some(2)] {
                    let name = view.get_node_name(source).unwrap();
                    let oracle = if view.is_directed() {
                        fnx_algorithms::single_source_shortest_path_length_directed(
                            &directed, name, cutoff,
                        )
                    } else {
                        fnx_algorithms::single_source_shortest_path_length(
                            &undirected,
                            name,
                            cutoff,
                        )
                    };
                    let expected: BTreeMap<_, _> = oracle
                        .into_iter()
                        .map(|(name, depth)| {
                            (
                                view.vertex_id(view.get_node_index(&name).unwrap()).unwrap(),
                                depth as u64,
                            )
                        })
                        .collect();
                    let actual = execute(
                        &FnxCallSpec::single_source_shortest_path_length(vertices[source], cutoff),
                        &view,
                    );
                    assert!(
                        actual
                            .rows
                            .windows(2)
                            .all(|pair| match (&pair[0][0], &pair[1][0]) {
                                (FnxValue::Vertex(a), FnxValue::Vertex(b)) => a < b,
                                _ => false,
                            })
                    );
                    assert_eq!(
                        distances(actual),
                        expected,
                        "mask={mask}, {direction:?}, source={source}, cutoff={cutoff:?}"
                    );
                }
            }
            if view.is_directed() {
                assert_eq!(
                    labels(execute(&FnxCallSpec::weakly_connected_components(), &view)),
                    oracle_labels(
                        fnx_algorithms::weakly_connected_components(&directed),
                        &view
                    ),
                    "WCC mask={mask} {direction:?}"
                );
                assert_eq!(
                    labels(execute(
                        &FnxCallSpec::strongly_connected_components(),
                        &view
                    )),
                    oracle_labels(
                        fnx_algorithms::strongly_connected_components(&directed),
                        &view
                    ),
                    "SCC mask={mask} {direction:?}"
                );
            } else {
                assert_eq!(
                    labels(execute(&FnxCallSpec::connected_components(), &view)),
                    oracle_labels(
                        fnx_algorithms::connected_components(&undirected).components,
                        &view
                    ),
                    "CC mask={mask}"
                );
            }
        }
    }
}

#[test]
fn full_width_source_parameters_literals_defaults_and_yield_are_bound() {
    let vertex = VId(u128::MAX);
    let mut parameters = FnxParameters::new();
    parameters.insert("source".to_owned(), FnxArgument::Vertex(vertex));
    let call = FnxCallSpec::bind(
        "CALL fnx.single_source_shortest_path_length($source)",
        &parameters,
    )
    .unwrap();
    let literal = bind(&format!(
        "CALL fnx.single_source_shortest_path_length({},NULL) YIELD *",
        vertex.0
    ));
    assert_eq!(call, literal);
    assert_eq!(
        call,
        FnxCallSpec::single_source_shortest_path_length(vertex, None)
    );
    assert!(call.options().is_none());
    parameters.insert("source".to_owned(), FnxArgument::Vertex(VId(0)));
    assert_eq!(
        call.algorithm(),
        FnxAlgorithm::SingleSourceShortestPathLength {
            source: vertex,
            cutoff: None
        }
    );
    assert_ne!(
        call.digest(),
        FnxCallSpec::single_source_shortest_path_length(VId(0), None).digest()
    );
    assert_ne!(
        call.digest(),
        FnxCallSpec::single_source_shortest_path_length(vertex, Some(0)).digest()
    );
    let alias = bind(&format!(
        "CALL fnx.single_source_shortest_path_length({},0) YIELD distance AS hops,vertex AS id;",
        vertex.0
    ));
    let result = execute(
        &alias,
        &graph(&[VId(0), vertex], &[], Directedness::Directed),
    );
    assert_eq!(result.columns, vec!["hops", "id"]);
    assert_eq!(
        result.rows,
        vec![vec![FnxValue::Integer(0), FnxValue::Vertex(vertex)]]
    );
    let component = bind("CALL fnx.strongly_connected_components() YIELD component AS group_id");
    assert_eq!(
        execute(&component, &graph(&[vertex], &[], Directedness::Directed)).rows,
        vec![vec![FnxValue::Vertex(vertex)]]
    );
}

#[test]
fn registered_signatures_reject_cross_schema_fields_wrong_types_and_missing_arguments() {
    for text in [
        "CALL fnx.single_source_shortest_path_length()",
        "CALL fnx.single_source_shortest_path_length(-1)",
        "CALL fnx.single_source_shortest_path_length(1.0)",
        "CALL fnx.single_source_shortest_path_length(NULL)",
        "CALL fnx.single_source_shortest_path_length(true)",
        "CALL fnx.single_source_shortest_path_length(0,-1)",
        "CALL fnx.single_source_shortest_path_length(0,1.0)",
        "CALL fnx.single_source_shortest_path_length(0,NULL,1)",
        "CALL fnx.single_source_shortest_path_length(0) YIELD score",
        "CALL fnx.single_source_shortest_path_length(0) YIELD vertex AS x,distance AS x",
        "CALL fnx.single_source_shortest_path_length(340282366920938463463374607431768211456)",
        "CALL fnx.connected_components(1)",
        "CALL fnx.connected_components() YIELD distance",
        "CALL fnx.weakly_connected_components() YIELD component,component",
        "CALL fnx.strongly_connected_components() YIELD score",
        "CALL fnx.pagerank() YIELD component",
    ] {
        assert!(
            FnxCallSpec::bind(text, &FnxParameters::new()).is_err(),
            "{text}"
        );
    }
    assert_eq!(
        FnxCallSpec::bind(
            "CALL fnx.single_source_shortest_path_length()",
            &FnxParameters::new()
        )
        .unwrap_err()
        .kind,
        FnxBindErrorKind::MissingArgument("source")
    );
    let signature = FnxSignatureRegistry::lookup("fnx.single_source_shortest_path_length").unwrap();
    assert_eq!(signature.parameters[0].default, None);
    assert_eq!(signature.parameters[1].default, Some(FnxArgument::Null));
    for signature in FnxSignatureRegistry::signatures() {
        assert_eq!(signature.graph_input_arity, 1);
        assert_eq!(
            signature.implementation,
            FnxImplementationClass::InCoreDecodedCache
        );
        assert!(!signature.execution_kernel.is_empty());
    }
}

#[test]
fn incompatible_directions_and_missing_source_fail_without_a_result() {
    let directed = graph(&[VId(0)], &[], Directedness::Directed);
    let undirected = graph(&[VId(0)], &[], Directedness::Undirected);
    for (call, view, required) in [
        (
            FnxCallSpec::connected_components(),
            &directed,
            FnxGraphKind::Undirected,
        ),
        (
            FnxCallSpec::weakly_connected_components(),
            &undirected,
            FnxGraphKind::Directed,
        ),
        (
            FnxCallSpec::strongly_connected_components(),
            &undirected,
            FnxGraphKind::Directed,
        ),
    ] {
        match call.execute(view, limits(), || Ok::<(), Infallible>(())) {
            Err(FnxExecutionError::GraphKind { required: actual }) => assert_eq!(actual, required),
            other => panic!("graph kind refusal required: {other:?}"),
        }
    }
    assert!(matches!(
        FnxCallSpec::single_source_shortest_path_length(VId(u128::MAX), None).execute(
            &directed,
            limits(),
            || Ok::<(), Infallible>(())
        ),
        Err(FnxExecutionError::UnknownSource(VId(u128::MAX)))
    ));
}

#[test]
fn empty_graphs_and_isolates_are_preserved_by_every_component_kernel() {
    for (call, direction) in [
        (
            FnxCallSpec::connected_components(),
            Directedness::Undirected,
        ),
        (
            FnxCallSpec::weakly_connected_components(),
            Directedness::Directed,
        ),
        (
            FnxCallSpec::strongly_connected_components(),
            Directedness::Directed,
        ),
    ] {
        let empty = execute(&call, &graph(&[], &[], direction));
        assert!(empty.rows.is_empty());
        assert_eq!(empty.certificate.estimated_work, 0);
        assert_eq!(empty.certificate.kernel_workspace_bytes, 0);
        let isolated = graph(&[VId(u128::MAX), VId(0)], &[], direction);
        assert_eq!(
            labels(execute(&call, &isolated)),
            BTreeMap::from([(VId(0), VId(0)), (VId(u128::MAX), VId(u128::MAX))])
        );
    }
}

#[test]
fn result_and_work_limits_are_enforced_without_convergence_limits_on_traversals() {
    let view = graph(
        &[VId(0), VId(1), VId(2)],
        &[ProjectionEdge {
            eid: EId(0),
            source: VId(0),
            target: VId(1),
            weight: 1.0,
        }],
        Directedness::Directed,
    );
    let isolated = FnxCallSpec::single_source_shortest_path_length(VId(2), None);
    let one_row = FnxExecutionLimits {
        max_result_rows: 1,
        ..limits()
    };
    assert_eq!(
        isolated
            .execute(&view, one_row, || Ok::<(), Infallible>(()))
            .unwrap()
            .rows
            .len(),
        1
    );
    let reachable = FnxCallSpec::single_source_shortest_path_length(VId(0), None);
    assert!(matches!(
        reachable.execute(&view, one_row, || Ok::<(), Infallible>(())),
        Err(FnxExecutionError::LimitExceeded {
            resource: "result rows",
            requested: 2,
            ..
        })
    ));
    let cutoff = FnxCallSpec::single_source_shortest_path_length(VId(0), Some(0));
    assert_eq!(
        cutoff
            .execute(&view, one_row, || Ok::<(), Infallible>(()))
            .unwrap()
            .rows
            .len(),
        1
    );
    for (call, work) in [
        (reachable, 4),
        (FnxCallSpec::weakly_connected_components(), 5),
        (FnxCallSpec::strongly_connected_components(), 8),
    ] {
        let cap = FnxExecutionLimits {
            max_estimated_work: work - 1,
            ..limits()
        };
        assert!(matches!(
            call.execute(&view, cap, || Ok::<(), Infallible>(())),
            Err(FnxExecutionError::LimitExceeded {
                resource: "estimated work",
                ..
            })
        ));
        let cap = FnxExecutionLimits {
            max_estimated_work: work,
            ..limits()
        };
        let result = call
            .execute(&view, cap, || Ok::<(), Infallible>(()))
            .unwrap();
        assert_eq!(result.certificate.estimated_work, work);
        assert!(
            result.certificate.witness.nodes_touched + result.certificate.witness.edges_scanned
                <= work
        );
    }
}

#[test]
fn all_traversal_checkpoints_abort_without_exposing_partial_rows() {
    let vertices = [VId(0), VId(1), VId(2), VId(3)];
    let edges: Vec<_> = [(0, 1), (1, 0), (1, 2), (2, 2)]
        .into_iter()
        .enumerate()
        .map(|(i, (s, t))| ProjectionEdge {
            eid: EId(i as u128),
            source: VId(s),
            target: VId(t),
            weight: 0.0,
        })
        .collect();
    for (call, direction) in [
        (
            FnxCallSpec::single_source_shortest_path_length(VId(0), None),
            Directedness::Directed,
        ),
        (
            FnxCallSpec::connected_components(),
            Directedness::Undirected,
        ),
        (
            FnxCallSpec::weakly_connected_components(),
            Directedness::Directed,
        ),
        (
            FnxCallSpec::strongly_connected_components(),
            Directedness::Directed,
        ),
    ] {
        let view = graph(&vertices, &edges, direction);
        let mut total = 0;
        let complete = call
            .execute(&view, limits(), || {
                total += 1;
                Ok::<(), &'static str>(())
            })
            .unwrap();
        assert!(total > 20);
        for stop in 1..=total {
            let mut observed = 0;
            let result = call.execute(&view, limits(), || {
                observed += 1;
                if observed == stop {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            });
            assert!(matches!(
                result,
                Err(FnxExecutionError::Cancelled("cancelled"))
            ));
            assert_eq!(observed, stop);
        }
        assert_eq!(complete, execute(&call, &view.clone()));
        assert_eq!(complete.certificate.numeric_profile, FNX_DISCRETE_PROFILE);
        assert_eq!(
            complete.certificate.execution_kernel,
            call.signature().execution_kernel
        );
    }
}

#[test]
fn strong_components_handle_a_deep_chain_and_large_cycle_without_recursion() {
    let n = 25_000;
    let vertices: Vec<_> = (0..n).map(|i| VId(i as u128)).collect();
    let mut edges: Vec<_> = (1..n)
        .map(|i| ProjectionEdge {
            eid: EId(i as u128),
            source: vertices[i - 1],
            target: vertices[i],
            weight: 1.0,
        })
        .collect();
    let call = FnxCallSpec::strongly_connected_components();
    let chain = execute(&call, &graph(&vertices, &edges, Directedness::Directed));
    assert_eq!(chain.certificate.witness.nodes_touched, 2 * n);
    assert_eq!(chain.certificate.witness.edges_scanned, 2 * (n - 1));
    assert!(
        labels(chain)
            .into_iter()
            .all(|(vertex, component)| vertex == component)
    );
    edges.push(ProjectionEdge {
        eid: EId(0),
        source: vertices[n - 1],
        target: vertices[0],
        weight: 1.0,
    });
    let cycle = execute(&call, &graph(&vertices, &edges, Directedness::Directed));
    assert!(labels(cycle).values().all(|&component| component == VId(0)));
}
