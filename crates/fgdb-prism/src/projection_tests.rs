//! Admission, cancellation and semantic laws for BOTH projection entrypoints.
use super::*;
use std::collections::BTreeMap;
use std::convert::Infallible;

fn binding() -> SnapshotBinding {
    SnapshotBinding { root: ObjectId([7; 32]), as_of: CommitSeq(31) }
}
fn limits() -> ProjectionLimits {
    ProjectionLimits {
        max_vertices: 100, max_input_edges: 100,
        max_adjacency_entries: 200, max_workspace_bytes: 1 << 20,
    }
}
fn spec() -> ProjectionSpec {
    ProjectionSpec { directedness: Directedness::Directed,
        parallel_edges: ParallelEdgePolicy::Sum, self_loops: SelfLoopPolicy::Keep }
}
fn edge(eid: u128, source: u128, target: u128, weight: f64) -> ProjectionEdge {
    ProjectionEdge { eid: EId(eid), source: VId(source), target: VId(target), weight }
}
fn fixture() -> (Vec<VId>, Vec<ProjectionEdge>) {
    (vec![VId(u128::MAX), VId(17), VId(0), VId(91)], vec![
        edge(9, 17, 17, 0.0), edge(8, 17, u128::MAX, 2.0),
        edge(3, 0, 17, 1.0), edge(1, 0, 17, 1e16),
        edge(7, u128::MAX, 0, -0.0), edge(2, 0, 17, -1e16),
    ])
}

#[test]
fn checkpointed_sort_matches_std_on_every_small_permutation_and_duplicate_keys() {
    fn permutations(values: &mut [usize], start: usize) {
        if start == values.len() {
            let mut actual = values.to_vec();
            let mut expected = actual.clone();
            expected.sort_unstable();
            sort_by_key(&mut actual, |value| *value, &mut || Ok::<(), Infallible>(())).unwrap();
            assert_eq!(actual, expected);
            // Duplicate keys, an important admission-error input shape.
            let mut actual: Vec<_> = values.iter().map(|value| value / 2).collect();
            let mut expected = actual.clone();
            expected.sort_unstable();
            sort_by_key(&mut actual, |value| *value, &mut || Ok::<(), Infallible>(())).unwrap();
            assert_eq!(actual, expected);
            return;
        }
        for i in start..values.len() {
            values.swap(start, i);
            permutations(values, start + 1);
            values.swap(start, i);
        }
    }
    for n in 0..=8 {
        permutations(&mut (0..n).collect::<Vec<_>>(), 0);
    }
    let mut ordered: Vec<_> = (0..4096).collect();
    let mut checks = 0;
    sort_by_key(&mut ordered, |value| *value, &mut || {
        checks += 1;
        Ok::<(), Infallible>(())
    }).unwrap();
    assert_eq!(checks, ordered.len(), "ordered input must stay linear");
}

#[test]
fn cancellation_interrupts_every_heap_sift_and_never_resumes() {
    let input = [9, 1, 8, 2, 7, 3, 6, 4, 5, 0];
    let mut checks = 0;
    sort_by_key(&mut input.to_vec(), |value| *value, &mut || {
        checks += 1;
        Ok::<(), &'static str>(())
    }).unwrap();
    assert!(checks > input.len() * 2);
    for stop in 1..=checks {
        let mut observed = 0;
        let error = sort_by_key(&mut input.to_vec(), |value| *value, &mut || {
            observed += 1;
            if observed == stop { Err("cancel") } else { Ok(()) }
        }).unwrap_err();
        assert_eq!(error, ProjectionBuildError::Cancelled("cancel"));
        assert_eq!(observed, stop);
    }
}

#[test]
fn every_projection_checkpoint_discards_partial_state_for_borrowed_and_owned_inputs() {
    let (vertices, edges) = fixture();
    for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
        for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
            let spec = ProjectionSpec { directedness: direction, self_loops: loops, ..spec() };
            for owned in [false, true] {
                let mut checks = 0;
                let checkpoint = || { checks += 1; Ok::<(), &'static str>(()) };
                if owned {
                    SnapshotGraphView::build_owned_with_checkpoint(binding(), vertices.clone(), edges.clone(), spec, limits(), checkpoint).unwrap();
                } else {
                    SnapshotGraphView::build_with_checkpoint(binding(), &vertices, &edges, spec, limits(), checkpoint).unwrap();
                }
                assert!(checks > 100, "validation, hashing and assembly must all checkpoint");
                for stop in 1..=checks {
                    let mut observed = 0;
                    let checkpoint = || {
                        observed += 1;
                        if observed == stop { Err("cancel") } else { Ok(()) }
                    };
                    let result = if owned {
                        SnapshotGraphView::build_owned_with_checkpoint(binding(), vertices.clone(), edges.clone(), spec, limits(), checkpoint)
                    } else {
                        SnapshotGraphView::build_with_checkpoint(binding(), &vertices, &edges, spec, limits(), checkpoint)
                    };
                    assert_eq!(result.unwrap_err(), ProjectionBuildError::Cancelled("cancel"));
                    assert_eq!(observed, stop);
                }
            }
        }
    }
    assert_eq!((vertices, edges), fixture(), "borrowed inputs must never mutate");
}

#[test]
fn owned_projection_retains_the_vertex_allocation_and_charges_spare_capacity() {
    let mut vertices = Vec::with_capacity(32);
    vertices.extend([VId(17), VId(0)]);
    let mut edges = Vec::with_capacity(64);
    edges.push(edge(9, 0, 17, 2.0));
    let pointer = vertices.as_ptr();
    let input_bytes = vertices.capacity() * size_of::<VId>() + edges.capacity() * size_of::<ProjectionEdge>();
    let expected = input_bytes + 2 * 32 + 2 * (3 * 2 + 1) * size_of::<usize>()
        + 2 * size_of::<usize>() + size_of::<f64>();
    let graph = SnapshotGraphView::build_owned_with_checkpoint(binding(), vertices, edges, spec(),
        ProjectionLimits { max_workspace_bytes: expected, ..limits() }, || Ok::<(), Infallible>(())).unwrap();
    assert_eq!(graph.vertex_ids().as_ptr(), pointer, "do not clone transferred vertices");
    assert_eq!(graph.vertex_ids(), &[VId(0), VId(17)]);
    assert_eq!(graph.charged_workspace_bytes(), expected);
    let shared = graph.clone();
    assert!(graph.shares_cache_with(&shared));
    assert!(std::ptr::eq(graph.projected_row(0).unwrap().0, shared.projected_row(0).unwrap().0));

    // Even EMPTY vectors can carry substantial backing stores. Length-only
    // admission would hide this memory and perform later allocations anyway.
    let vertices = Vec::<VId>::with_capacity(32);
    let mut checks = 0;
    let error = SnapshotGraphView::build_owned_with_checkpoint(binding(), vertices, vec![], spec(),
        ProjectionLimits { max_workspace_bytes: 0, ..limits() }, || {
            checks += 1;
            Ok::<(), Infallible>(())
        }).unwrap_err();
    assert!(matches!(error, ProjectionBuildError::Projection(ProjectionError::LimitExceeded {
        resource: "workspace bytes", ..
    })));
    assert_eq!(checks, 1, "refuse transferred capacity before sorting/assembly");
}

#[test]
fn all_three_node_topologies_match_an_independent_multigraph_reducer() {
    let vertices = [VId(0), VId(17), VId(u128::MAX - 1), VId(u128::MAX)];
    for mask in 0..512 {
        let mut edges = Vec::new();
        for bit in 0..9 {
            if mask & (1 << bit) != 0 {
                for copy in 0..2 {
                    edges.push(ProjectionEdge { eid: EId((bit * 2 + copy) as u128),
                        source: vertices[bit / 3], target: vertices[bit % 3], weight: (copy + 1) as f64 });
                }
            }
        }
        for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
            for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                for parallel in [ParallelEdgePolicy::CollapseUnit, ParallelEdgePolicy::Minimum,
                    ParallelEdgePolicy::Maximum, ParallelEdgePolicy::Sum] {
                    let spec = ProjectionSpec { directedness: direction, self_loops: loops, parallel_edges: parallel };
                    let mut reference: BTreeMap<(VId, VId), f64> = BTreeMap::new();
                    for edge in &edges {
                        let (mut source, mut target) = (edge.source, edge.target);
                        if source == target && loops == SelfLoopPolicy::Drop { continue; }
                        if direction == Directedness::Reversed
                            || (direction == Directedness::Undirected && target < source) {
                            std::mem::swap(&mut source, &mut target);
                        }
                        let weight = if parallel == ParallelEdgePolicy::CollapseUnit { 1.0 } else { edge.weight };
                        reference.entry((source, target)).and_modify(|old| {
                            *old = match parallel {
                                ParallelEdgePolicy::Minimum => old.min(weight),
                                ParallelEdgePolicy::Maximum => old.max(weight),
                                ParallelEdgePolicy::Sum => *old + weight,
                                _ => 1.0,
                            };
                        }).or_insert(weight);
                    }
                    let graph = SnapshotGraphView::build_with_checkpoint(binding(), &vertices, &edges, spec, limits(),
                        || Ok::<(), Infallible>(())).unwrap();
                    let mut reordered = edges.clone();
                    reordered.reverse();
                    let owned = SnapshotGraphView::build_owned_with_checkpoint(binding(), vertices.iter().copied().rev().collect(), reordered,
                        spec, limits(), || Ok::<(), Infallible>(())).unwrap();
                    assert_eq!(graph.digest(), owned.digest());
                    assert_eq!(graph.edge_count(), reference.len());
                    assert_eq!(graph.input_edge_count(), edges.len());
                    assert_eq!(graph.node_count(), 4); // includes an isolated vertex
                    for (s, &source) in vertices.iter().enumerate() {
                        let mut out = Vec::new();
                        let mut incoming = Vec::new();
                        for (t, &target) in vertices.iter().enumerate() {
                            let get = |a, b| reference.get(&(a, b)).copied().or_else(|| {
                                (direction == Directedness::Undirected).then(|| reference.get(&(b, a)).copied()).flatten()
                            });
                            let expected = get(source, target);
                            assert_eq!(graph.projected_weight(s, t).map(f64::to_bits), expected.map(f64::to_bits));
                            assert_eq!(owned.projected_weight(s, t).map(f64::to_bits), expected.map(f64::to_bits));
                            if expected.is_some() { out.push(t); }
                            if get(target, source).is_some() { incoming.push(t); }
                        }
                        assert_eq!(graph.neighbors_indices(s).unwrap(), out.as_slice());
                        assert_eq!(graph.in_neighbors_indices(s).unwrap(), incoming.as_slice());
                    }
                }
            }
        }
    }
}

#[test]
fn eid_order_fixes_floating_reduction_and_source_identity_stays_bound() {
    let (vertices, mut edges) = fixture();
    let graph = SnapshotGraphView::build(binding(), &vertices, &edges, spec(), limits()).unwrap();
    // ((1e16 + -1e16) + 1) differs from (1e16 + (-1e16 + 1)).
    assert_eq!(graph.projected_weight(0, 1).unwrap().to_bits(), 1.0f64.to_bits());
    let original = graph.digest();
    for _ in 0..edges.len() {
        edges.rotate_left(1);
        assert_eq!(SnapshotGraphView::build(binding(), &vertices, &edges, spec(), limits()).unwrap().digest(), original);
    }
    let mut changed = binding();
    changed.as_of = CommitSeq(changed.as_of.0 + 1);
    assert_ne!(SnapshotGraphView::build(changed, &vertices, &edges, spec(), limits()).unwrap().digest(), original);
    edges[0].eid = EId(100);
    assert_ne!(SnapshotGraphView::build(binding(), &vertices, &edges, spec(), limits()).unwrap().digest(), original);
}

#[test]
fn cancellation_and_validation_errors_are_not_interchangeable() {
    let vertices = [VId(0), VId(0)];
    assert_eq!(SnapshotGraphView::build(binding(), &vertices, &[], spec(), limits()).unwrap_err(), ProjectionError::DuplicateVertex(VId(0)));
    assert_eq!(SnapshotGraphView::build_with_checkpoint(binding(), &vertices, &[], spec(), limits(),
        || Err("stop")).unwrap_err(), ProjectionBuildError::Cancelled("stop"));
    let vertices = [VId(0), VId(17)];
    let invalid = [
        (vec![edge(1, 0, 91, 1.0)], ProjectionError::MissingEndpoint { edge: EId(1), vertex: VId(91) }),
        (vec![edge(1, 0, 17, 1.0), edge(1, 17, 0, 1.0)], ProjectionError::DuplicateEdge(EId(1))),
        (vec![edge(1, 0, 17, f64::NAN)], ProjectionError::NonFiniteWeight(EId(1))),
        (vec![edge(1, 0, 17, f64::MAX), edge(2, 0, 17, f64::MAX)],
            ProjectionError::WeightOverflow { source: VId(0), target: VId(17) }),
    ];
    for (edges, expected) in invalid {
        assert_eq!(SnapshotGraphView::build(binding(), &vertices, &edges, spec(), limits()).unwrap_err(), expected);
        assert_eq!(SnapshotGraphView::build_owned_with_checkpoint(binding(), vertices.to_vec(), edges, spec(), limits(),
            || Ok::<(), Infallible>(())).unwrap_err(), ProjectionBuildError::Projection(expected));
    }
}

#[test]
fn discarded_weights_and_self_loops_do_not_become_observable() {
    let vertices = [VId(0), VId(17)];
    let edges = [edge(1, 0, 0, f64::NAN), edge(2, 0, 17, f64::NEG_INFINITY)];
    let spec = ProjectionSpec { self_loops: SelfLoopPolicy::Drop, parallel_edges: ParallelEdgePolicy::CollapseUnit, ..spec() };
    let graph = SnapshotGraphView::build_with_checkpoint(binding(), &vertices, &edges, spec, limits(),
        || Ok::<(), Infallible>(())).unwrap();
    assert_eq!(graph.edge_count(), 1);
    assert_eq!(graph.projected_weight(0, 1), Some(1.0));
    assert_eq!(graph.projected_weight(0, 0), None);
}
