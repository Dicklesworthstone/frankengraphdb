use fgdb_prism::{
    AdapterPath, Directedness, GraphView, ParallelEdgePolicy, ProjectionEdge, ProjectionError,
    ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
    PROJECTED_WEIGHT_ATTRIBUTE,
};
use fgdb_types::ids::ObjectId;
use fgdb_types::{CommitSeq, EId, VId};

fn binding() -> SnapshotBinding {
    SnapshotBinding { root: ObjectId([7; 32]), as_of: CommitSeq(9) }
}
fn limits() -> ProjectionLimits {
    ProjectionLimits {
        max_vertices: 128,
        max_input_edges: 4096,
        max_adjacency_entries: 8192,
        max_workspace_bytes: 1 << 22,
    }
}
fn spec(direction: Directedness, parallel: ParallelEdgePolicy) -> ProjectionSpec {
    ProjectionSpec { directedness: direction, parallel_edges: parallel, self_loops: SelfLoopPolicy::Keep }
}
fn edge(eid: u128, source: u128, target: u128, weight: f64) -> ProjectionEdge {
    ProjectionEdge { eid: EId(eid), source: VId(source), target: VId(target), weight }
}
fn graph(vertices: &[VId], edges: &[ProjectionEdge], spec: ProjectionSpec) -> SnapshotGraphView {
    SnapshotGraphView::build(binding(), vertices, edges, spec, limits()).unwrap()
}

#[test]
fn sparse_128_bit_ids_are_not_storage_ordinals() {
    let large = VId(u128::MAX);
    let view = graph(
        &[large, VId(0), VId(17)],
        &[edge(1, u128::MAX, 0, 2.0)],
        spec(Directedness::Directed, ParallelEdgePolicy::Reject),
    );
    assert_eq!(view.vertex_ids(), &[VId(0), VId(17), large]);
    assert_eq!(view.vertex_ordinal(large), Some(2));
    assert_eq!(view.vertex_id(2), Some(large));
    assert_eq!(view.vertex_id(usize::MAX), None);
    assert_eq!(view.neighbors_indices(2), Some(&[0][..]));
    assert_eq!(view.neighbors_indices(1), Some(&[][..]));
    assert_eq!(view.neighbors_indices(usize::MAX), None);
    assert_eq!(view.in_neighbors_indices(0), Some(&[2][..]));
    assert_eq!(view.in_neighbors_indices(2), Some(&[][..]));
    assert_eq!(view.in_neighbors_indices(usize::MAX), None);
    assert_eq!(view.get_node_name(2), Some("ffffffffffffffffffffffffffffffff"));
    for (i, node) in view.nodes_ordered().iter().copied().enumerate() {
        assert_eq!(view.get_node_index(node), Some(i));
    }
    assert_eq!(view.get_node_index("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"), None);
    assert_eq!(view.get_node_index("0"), None);
    assert_eq!(view.get_node_index("+0000000000000000000000000000000"), None);
    assert_eq!(view.node_count(), 3);
    assert_eq!(view.edge_count(), 1);
    assert_eq!(view.adapter_path(), AdapterPath::DecodedCache);
    assert_eq!(view.adapter_path().as_str(), "DECODED_CACHE");
}

#[test]
fn both_faces_iterators_weights_and_reversal_agree() {
    let edges = [edge(5, 3, 1, 4.0), edge(2, 1, 2, 8.0), edge(9, 2, 2, 3.0)];
    for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
        let view = graph(&[VId(3), VId(2), VId(1)], &edges, spec(direction, ParallelEdgePolicy::Reject));
        for i in 0..view.node_count() {
            let node = view.get_node_name(i).unwrap();
            let out: Vec<_> = view.neighbors_iter(node).unwrap().collect();
            let incoming: Vec<_> = view.in_neighbors_iter(node).unwrap().collect();
            let out_indices = view.neighbors_indices(i).unwrap();
            let in_indices = view.in_neighbors_indices(i).unwrap();
            assert!(out_indices.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(in_indices.windows(2).all(|pair| pair[0] < pair[1]));
            assert_eq!(out, out_indices.iter().map(|&j| view.get_node_name(j).unwrap()).collect::<Vec<_>>());
            assert_eq!(incoming, in_indices.iter().map(|&j| view.get_node_name(j).unwrap()).collect::<Vec<_>>());
            assert_eq!(view.neighbor_count(node), out.len());
            for &j in out_indices {
                assert!(view.in_neighbors_indices(j).unwrap().contains(&i));
                let target = view.get_node_name(j).unwrap();
                assert!(view.has_edge(node, target));
                assert_eq!(view.edge_weight(node, target, None), 1.0);
                assert_eq!(view.edge_weight(node, target, Some(PROJECTED_WEIGHT_ATTRIBUTE)), view.projected_weight(i, j).unwrap());
                assert_eq!(view.edge_weight_by_indices(i, j, Some(PROJECTED_WEIGHT_ATTRIBUTE)), view.projected_weight(i, j).unwrap());
            }
        }
        assert_eq!(view.edge_count(), 3);
        assert_eq!(view.is_directed(), direction != Directedness::Undirected);
        let s = view.vertex_ordinal(VId(1)).unwrap();
        let t = view.vertex_ordinal(VId(2)).unwrap();
        if direction == Directedness::Reversed {
            assert_eq!(view.projected_weight(t, s), Some(8.0));
            assert_eq!(view.projected_weight(s, t), None);
        } else {
            assert_eq!(view.projected_weight(s, t), Some(8.0));
        }
    }
}

#[test]
fn parallel_edges_never_collapse_implicitly() {
    let vertices = [VId(1), VId(2)];
    let edges = [edge(7, 1, 2, 9.0), edge(4, 1, 2, 2.0), edge(8, 1, 2, -1.0)];
    assert!(matches!(
        SnapshotGraphView::build(binding(), &vertices, &edges, spec(Directedness::Directed, ParallelEdgePolicy::Reject), limits()),
        Err(ProjectionError::ParallelEdge { .. })
    ));
    for (policy, expected) in [
        (ParallelEdgePolicy::CollapseUnit, 1.0),
        (ParallelEdgePolicy::Minimum, -1.0),
        (ParallelEdgePolicy::Maximum, 9.0),
        (ParallelEdgePolicy::Sum, 10.0),
    ] {
        let view = graph(&vertices, &edges, spec(Directedness::Directed, policy));
        assert_eq!(view.edge_count(), 1);
        assert_eq!(view.input_edge_count(), 3);
        assert_eq!(view.projected_weight(0, 1), Some(expected));
        assert_eq!(view.neighbors_indices(0), Some(&[1][..]));
        assert_eq!(view.in_neighbors_indices(1), Some(&[0][..]));
    }
    let antiparallel = [edge(1, 1, 2, 3.0), edge(2, 2, 1, 5.0)];
    let directed = graph(&vertices, &antiparallel, spec(Directedness::Directed, ParallelEdgePolicy::Reject));
    assert_eq!(directed.edge_count(), 2);
    assert!(matches!(
        SnapshotGraphView::build(binding(), &vertices, &antiparallel, spec(Directedness::Undirected, ParallelEdgePolicy::Reject), limits()),
        Err(ProjectionError::ParallelEdge { .. })
    ));
    let undirected = graph(&vertices, &antiparallel, spec(Directedness::Undirected, ParallelEdgePolicy::Sum));
    assert_eq!(undirected.edge_count(), 1);
    assert_eq!(undirected.projected_weight(0, 1), Some(8.0));
    assert_eq!(undirected.projected_weight(1, 0), Some(8.0));
}

#[test]
fn self_loop_laws_preserve_single_adjacency_entry() {
    let vertices = [VId(1)];
    let edges = [edge(1, 1, 1, 4.0)];
    let mut policy = spec(Directedness::Undirected, ParallelEdgePolicy::Reject);
    let mut budget = limits();
    budget.max_adjacency_entries = 1;
    let view = SnapshotGraphView::build(binding(), &vertices, &edges, policy, budget).unwrap();
    assert_eq!(view.neighbors_indices(0), Some(&[0][..]));
    assert_eq!(view.in_neighbors_indices(0), Some(&[0][..]));
    assert_eq!(view.edge_count(), 1);
    assert_eq!(view.projected_weight(0, 0), Some(4.0));
    policy.self_loops = SelfLoopPolicy::Drop;
    let dropped = graph(&vertices, &edges, policy);
    assert_eq!(dropped.node_count(), 1);
    assert_eq!(dropped.edge_count(), 0);
    policy.self_loops = SelfLoopPolicy::Reject;
    assert!(matches!(SnapshotGraphView::build(binding(), &vertices, &edges, policy, limits()), Err(ProjectionError::SelfLoop(EId(1)))));
}

#[test]
fn reductions_are_eid_ordered_and_digest_is_permutation_invariant() {
    let policy = spec(Directedness::Directed, ParallelEdgePolicy::Sum);
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [edge(1, 1, 2, 1e16), edge(2, 1, 2, -1e16), edge(3, 1, 2, 1.0)];
    let first = graph(&vertices, &edges, policy);
    let second = graph(&[VId(3), VId(2), VId(1)], &[edges[2], edges[0], edges[1]], policy);
    assert_eq!(first.projected_weight(0, 1), Some(1.0));
    assert_eq!(second.projected_weight(0, 1), Some(1.0));
    assert_eq!(first.digest(), second.digest());
    let mut other = binding();
    other.as_of = CommitSeq(8);
    let historical = SnapshotGraphView::build(other, &vertices, &edges, policy, limits()).unwrap();
    assert_ne!(first.digest(), historical.digest());
    other = binding();
    other.root = ObjectId([8; 32]);
    assert_ne!(first.digest(), SnapshotGraphView::build(other, &vertices, &edges, policy, limits()).unwrap().digest());
    let alternate_population = [edge(4, 1, 2, 1.0)];
    assert_ne!(first.digest(), graph(&vertices, &alternate_population, policy).digest());
    assert_ne!(first.digest(), graph(&vertices, &edges, spec(Directedness::Reversed, ParallelEdgePolicy::Sum)).digest());
}

#[test]
fn malformed_inputs_refuse_without_partial_graphs() {
    let policy = spec(Directedness::Directed, ParallelEdgePolicy::Reject);
    let vertices = [VId(1), VId(2)];
    assert!(matches!(SnapshotGraphView::build(binding(), &[VId(1), VId(1)], &[], policy, limits()), Err(ProjectionError::DuplicateVertex(VId(1)))));
    assert!(matches!(SnapshotGraphView::build(binding(), &vertices, &[edge(1, 1, 2, 1.0), edge(1, 2, 1, 1.0)], policy, limits()), Err(ProjectionError::DuplicateEdge(EId(1)))));
    assert!(matches!(SnapshotGraphView::build(binding(), &vertices, &[edge(1, 1, 3, 1.0)], policy, limits()), Err(ProjectionError::MissingEndpoint { vertex: VId(3), .. })));
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(matches!(SnapshotGraphView::build(binding(), &vertices, &[edge(1, 1, 2, bad)], policy, limits()), Err(ProjectionError::NonFiniteWeight(EId(1)))));
    }
    assert!(matches!(SnapshotGraphView::build(binding(), &vertices, &[edge(1, 1, 2, f64::MAX), edge(2, 1, 2, f64::MAX)], spec(Directedness::Directed, ParallelEdgePolicy::Sum), limits()), Err(ProjectionError::WeightOverflow { .. })));
    let zero = graph(&vertices, &[edge(1, 1, 2, -0.0)], policy);
    assert_eq!(zero.projected_weight(0, 1).unwrap().to_bits(), 0.0f64.to_bits());
}

#[test]
fn explicit_discard_policies_do_not_inspect_discarded_weights() {
    let mut policy = spec(Directedness::Directed, ParallelEdgePolicy::Reject);
    policy.self_loops = SelfLoopPolicy::Drop;
    assert_eq!(graph(&[VId(1)], &[edge(1, 1, 1, f64::NAN)], policy).edge_count(), 0);
    let unit = graph(&[VId(1), VId(2)], &[edge(1, 1, 2, f64::NAN)], spec(Directedness::Directed, ParallelEdgePolicy::CollapseUnit));
    assert_eq!(unit.projected_weight(0, 1), Some(1.0));
}

#[test]
fn cache_is_immutable_and_clones_share_one_generation() {
    let policy = spec(Directedness::Directed, ParallelEdgePolicy::Reject);
    let mut edges = [edge(1, 1, 2, 3.0)];
    let view = graph(&[VId(1), VId(2)], &edges, policy);
    let cloned = view.clone();
    assert!(view.shares_cache_with(&cloned));
    assert_eq!(view.neighbors_indices(0).unwrap().as_ptr(), cloned.neighbors_indices(0).unwrap().as_ptr());
    edges[0].weight = 9.0;
    assert_eq!(view.projected_weight(0, 1), Some(3.0));
    let rebuilt = graph(&[VId(1), VId(2)], &edges, policy);
    assert!(!view.shares_cache_with(&rebuilt));
    assert_ne!(view.digest(), rebuilt.digest());
}

#[test]
fn empty_graph_invalid_names_and_all_admission_boundaries() {
    let policy = spec(Directedness::Directed, ParallelEdgePolicy::Reject);
    let empty = graph(&[], &[], policy);
    assert_eq!(empty.node_count(), 0);
    assert_eq!(empty.edge_count(), 0);
    assert_eq!(empty.neighbors_indices(0), None);
    assert_eq!(empty.in_neighbors_indices(0), None);
    assert!(empty.nodes_ordered().is_empty());
    assert!(!empty.has_node(""));
    assert!(!empty.has_edge("x", "y"));
    assert_eq!(empty.neighbor_count("x"), 0);
    assert!(empty.neighbors_iter("x").is_none());
    assert!(empty.in_neighbors_iter("x").is_none());
    let vertices = [VId(1), VId(2)];
    let edges = [edge(1, 1, 2, 1.0), edge(2, 1, 2, 2.0)];
    let reduce = spec(Directedness::Directed, ParallelEdgePolicy::Sum);
    let mut budget = limits();
    budget.max_adjacency_entries = 1;
    let view = SnapshotGraphView::build(binding(), &vertices, &edges, reduce, budget).unwrap();
    assert_eq!(view.edge_count(), 1); // admission uses the reduced graph
    budget.max_workspace_bytes = view.charged_workspace_bytes();
    SnapshotGraphView::build(binding(), &vertices, &edges, reduce, budget).unwrap();
    budget.max_workspace_bytes -= 1;
    assert!(matches!(SnapshotGraphView::build(binding(), &vertices, &edges, reduce, budget), Err(ProjectionError::LimitExceeded { resource: "workspace bytes", .. })));
    for resource in 0..3 {
        let mut budget = limits();
        match resource {
            0 => budget.max_vertices = 1,
            1 => budget.max_input_edges = 1,
            _ => budget.max_adjacency_entries = 0,
        }
        assert!(matches!(SnapshotGraphView::build(binding(), &vertices, &edges, reduce, budget), Err(ProjectionError::LimitExceeded { .. })));
    }
}

#[test]
fn every_three_node_topology_has_consistent_sorted_reverse_rows() {
    // Independent bit-matrix oracle includes isolates, self-loops, cycles,
    // antiparallel edges and all degree patterns; input enumeration is reversed.
    let vertices = [VId(101), VId(503), VId(u128::MAX)];
    for mask in 0u16..512 {
        let mut edges = Vec::new();
        for bit in (0..9).rev() {
            if mask & (1 << bit) != 0 {
                edges.push(ProjectionEdge {
                    eid: EId(bit as u128),
                    source: vertices[bit / 3],
                    target: vertices[bit % 3],
                    weight: (bit + 1) as f64,
                });
            }
        }
        for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
            let view = graph(&vertices, &edges, spec(direction, ParallelEdgePolicy::CollapseUnit));
            for s in 0..3 {
                for t in 0..3 {
                    let forward = mask & (1 << (s * 3 + t)) != 0;
                    let reverse = mask & (1 << (t * 3 + s)) != 0;
                    let expected = match direction {
                        Directedness::Directed => forward,
                        Directedness::Reversed => reverse,
                        Directedness::Undirected => forward || reverse,
                    };
                    assert_eq!(view.neighbors_indices(s).unwrap().contains(&t), expected);
                    assert_eq!(view.in_neighbors_indices(t).unwrap().contains(&s), expected);
                    assert_eq!(view.projected_weight(s, t), expected.then_some(1.0));
                }
            }
        }
    }
}
