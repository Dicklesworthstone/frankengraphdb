//! The pinned franken_networkx catalog and its differential native kernels.
//!
//! Degree centrality, core decomposition, eigenvector power iteration,
//! Brandes betweenness, closeness and harmonic centrality
//! borrow the admitted CSR directly. They preserve the catalog's logical
//! charges and check live control during arithmetic. Certificates identify
//! these kernels and their actual vector allocation peak; the projection
//! remains an in-core DECODED_CACHE.
//!
//! No registered foundation entry constructs a second graph. Distance
//! centralities retain bit-parallel BFS for favorable graphs and use
//! independent source traversals for high-diameter or disconnected inputs.
//! Native eigenvector iteration matches
//! the checked foundation entry's convergence and refusal semantics. Katz
//! remains unregistered because its checked entry drops the result and
//! witness on non-convergence.

mod brandes;
mod distance;
mod eigenvector;
mod native;

#[cfg(test)]
use crate::FNX_METER_RUN;
use crate::execute::{FnxMeter, KernelOutput};
use crate::{FnxExecutionError, FoundationAlgorithm, SnapshotGraphView};
use fnx_algorithms::GraphView;
#[cfg(test)]
use fnx_classes::Graph;

pub(crate) const EIGENVECTOR_MAX_ITERATIONS: usize = 100;
pub(crate) const EIGENVECTOR_TOLERANCE: f64 = 1.0e-6;

/// Admission-time work bound over `n` vertices and `arcs` adjacency entries
/// (an undirected edge counts in both rows).
pub(crate) fn estimated_work(
    algorithm: FoundationAlgorithm,
    n: usize,
    arcs: usize,
) -> Option<usize> {
    let step = n.checked_add(arcs)?;
    match algorithm {
        FoundationAlgorithm::DegreeCentrality | FoundationAlgorithm::CoreNumber => Some(step),
        FoundationAlgorithm::ClosenessCentrality
        | FoundationAlgorithm::HarmonicCentrality
        | FoundationAlgorithm::BetweennessCentrality => n.checked_mul(step),
        FoundationAlgorithm::EigenvectorCentrality => {
            step.checked_mul(EIGENVECTOR_MAX_ITERATIONS + 1)
        }
    }
}

/// Keep the original charged order: one unit per node, then one per source
/// row. Live polls bound each edge-copy batch without inventing charged edge
/// units. The copy stays private on refusal, including a failure after only
/// part of a node batch was charged. Allocator calls inside fnx remain
/// synchronous; this does not claim to preempt its allocations or kernels.
#[cfg(test)]
fn copy_graph<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<Graph, FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut copy = Graph::strict();
    // Fixed scratch avoids collecting a second n-entry name-reference vector.
    // The foundation still owns one copied name per admitted vertex.
    for start in (0..n).step_by(FNX_METER_RUN) {
        let len = (n - start).min(FNX_METER_RUN);
        let mut names = [""; FNX_METER_RUN];
        for (offset, name) in names[..len].iter_mut().enumerate() {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            *name = graph
                .get_node_name(start + offset)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        }
        if copy.extend_nodes_unrecorded(names[..len].iter().copied()) != len {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
    }
    for source in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let row = graph
            .neighbors_indices(source)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for batch in row.chunks(FNX_METER_RUN) {
            checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
            if batch.contains(&source) {
                return Err(FnxExecutionError::SelfLoopRefused);
            }
            // Undirected edges are canonical (source < target); each is added
            // once, in exactly the former add_edge order. Names were inserted
            // in the projection's dense order, so these are existing indices.
            // SnapshotGraphView's builder has already validated the endpoints
            // and applied the explicit parallel-edge policy.
            let selected = batch.iter().filter(|&&target| source < target).count();
            let inserted = copy.extend_existing_index_edges_unrecorded(
                batch
                    .iter()
                    .copied()
                    .filter(|&target| source < target)
                    .map(|target| (source, target)),
            );
            if inserted != selected {
                return Err(FnxExecutionError::InvalidUpstreamResult);
            }
        }
    }
    Ok(copy)
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    algorithm: FoundationAlgorithm,
    _arcs: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    // Retain the copy path's node and source-row charge phases, including
    // the exact self-loop refusal checkpoint. No node names are created.
    for _ in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    }
    for source in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let row = graph
            .neighbors_indices(source)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        // Projection rows are sorted, simple and immutable. Self-loop
        // validation needs no extra traversal of all admitted arcs.
        if row.binary_search(&source).is_ok() {
            return Err(FnxExecutionError::SelfLoopRefused);
        }
    }
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    let result = match algorithm {
        FoundationAlgorithm::DegreeCentrality => native::degree(graph, checkpoint)?,
        FoundationAlgorithm::CoreNumber => native::core(graph, checkpoint)?,
        FoundationAlgorithm::EigenvectorCentrality => eigenvector::run(graph, checkpoint)?,
        FoundationAlgorithm::BetweennessCentrality => brandes::run(graph, checkpoint)?,
        FoundationAlgorithm::ClosenessCentrality => distance::run::<false, C>(graph, checkpoint)?,
        FoundationAlgorithm::HarmonicCentrality => distance::run::<true, C>(graph, checkpoint)?,
    };
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Directedness, ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
        SelfLoopPolicy, SnapshotBinding,
    };
    use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Stop {
        Unit(usize),
        Poll(usize),
    }

    #[derive(Default)]
    struct Meter {
        units: usize,
        polls: usize,
        stop: Option<Stop>,
    }

    impl FnxMeter<Stop> for Meter {
        fn unit(&mut self) -> Result<(), Stop> {
            self.units += 1;
            let here = Stop::Unit(self.units);
            if self.stop == Some(here) {
                Err(here)
            } else {
                Ok(())
            }
        }

        fn poll(&mut self) -> Result<(), Stop> {
            self.polls += 1;
            let here = Stop::Poll(self.polls);
            if self.stop == Some(here) {
                Err(here)
            } else {
                Ok(())
            }
        }
    }

    fn hub(n: usize, with_loop: bool) -> SnapshotGraphView {
        let mut vertices: Vec<_> = (0..n)
            .map(|i| VId((1u128 << 100) + (i as u128) * 7919))
            .collect();
        if let Some(last) = vertices.last_mut() {
            *last = VId(u128::MAX);
        }
        let mut edges: Vec<_> = (1..n.saturating_sub(1))
            .map(|i| ProjectionEdge {
                eid: EId((1u128 << 110) + i as u128),
                source: vertices[0],
                target: vertices[i],
                weight: 1.0,
            })
            .collect();
        if with_loop {
            edges.push(ProjectionEdge {
                eid: EId(u128::MAX),
                source: vertices[n / 2],
                target: vertices[n / 2],
                weight: 1.0,
            });
        }
        vertices.reverse();
        edges.reverse();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([6; 32]),
                as_of: CommitSeq(9),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: Directedness::Undirected,
                parallel_edges: ParallelEdgePolicy::Reject,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: n,
                max_input_edges: edges.len(),
                max_adjacency_entries: edges.len() * 2,
                max_workspace_bytes: 1 << 28,
            },
        )
        .unwrap()
    }

    // The former constructor is an independent fnx API oracle; it preserves
    // its original edge insertion order and charged sequence.
    fn old_copy(
        graph: &SnapshotGraphView,
        meter: &mut Meter,
    ) -> Result<Graph, FnxExecutionError<Stop>> {
        let names = graph.nodes_ordered();
        let mut copy = Graph::strict();
        for name in &names {
            meter.unit().map_err(FnxExecutionError::Cancelled)?;
            copy.add_node(*name);
        }
        for source in 0..graph.node_count() {
            meter.unit().map_err(FnxExecutionError::Cancelled)?;
            for &target in graph.neighbors_indices(source).unwrap() {
                if target == source {
                    return Err(FnxExecutionError::SelfLoopRefused);
                }
                if source < target {
                    copy.add_edge(names[source], names[target]).unwrap();
                }
            }
        }
        Ok(copy)
    }

    fn same_graph(actual: &Graph, expected: &Graph) {
        assert_eq!(actual.nodes_ordered(), expected.nodes_ordered());
        assert_eq!(
            actual.edges_ordered_indices(),
            expected.edges_ordered_indices(),
        );
        assert_eq!(
            actual
                .edges_storage_order_index_iter()
                .map(|(source, target, _)| (source, target))
                .collect::<Vec<_>>(),
            expected
                .edges_storage_order_index_iter()
                .map(|(source, target, _)| (source, target))
                .collect::<Vec<_>>(),
        );
        assert_eq!(actual.edge_count(), expected.edge_count());
        for (index, name) in expected.nodes_ordered().into_iter().enumerate() {
            assert_eq!(actual.get_node_index(name), Some(index));
            assert_eq!(
                actual.neighbors_indices(index),
                expected.neighbors_indices(index),
            );
        }
    }

    #[test]
    fn indexed_copy_preserves_the_graph_and_charged_sequence() {
        for n in [0, 1, 2, 3, 65, 130, 1025] {
            let graph = hub(n, false);
            let (mut old_meter, mut new_meter) = (Meter::default(), Meter::default());
            let expected = old_copy(&graph, &mut old_meter).unwrap();
            let actual = copy_graph(&graph, &mut new_meter).unwrap();
            same_graph(&actual, &expected);
            assert_eq!(new_meter.units, 2 * n);
            assert_eq!(new_meter.units, old_meter.units);
            let batches: usize = (0..n)
                .map(|source| {
                    graph
                        .neighbors_indices(source)
                        .unwrap()
                        .len()
                        .div_ceil(FNX_METER_RUN)
                })
                .sum();
            assert_eq!(new_meter.polls, batches);
            assert!(
                actual.evidence_ledger().records().len()
                    <= expected.evidence_ledger().records().len()
            );
        }
    }

    #[test]
    fn indexed_copy_keeps_every_charged_refusal_and_loop_precedence() {
        for with_loop in [false, true] {
            let graph = hub(130, with_loop);
            for stop in 1..=2 * graph.node_count() + 1 {
                let mut old = Meter {
                    stop: Some(Stop::Unit(stop)),
                    ..Meter::default()
                };
                let mut new = Meter {
                    stop: Some(Stop::Unit(stop)),
                    ..Meter::default()
                };
                let expected = old_copy(&graph, &mut old);
                let actual = copy_graph(&graph, &mut new);
                match (actual, expected) {
                    (Ok(actual), Ok(expected)) => same_graph(&actual, &expected),
                    (
                        Err(FnxExecutionError::Cancelled(a)),
                        Err(FnxExecutionError::Cancelled(b)),
                    ) => assert_eq!(a, b),
                    (
                        Err(FnxExecutionError::SelfLoopRefused),
                        Err(FnxExecutionError::SelfLoopRefused),
                    ) => {}
                    (actual, expected) => {
                        panic!("stop {stop}, loop {with_loop}: {actual:?} != {expected:?}")
                    }
                }
                assert_eq!(new.units, old.units);
            }
        }
    }

    #[test]
    fn a_hub_copy_checks_live_cancellation_between_edge_batches_without_recharging() {
        let graph = hub(1025, false);
        for stop in [1, 2, 3, 8] {
            let mut meter = Meter {
                stop: Some(Stop::Poll(stop)),
                ..Meter::default()
            };
            let result = copy_graph(&graph, &mut meter);
            assert!(
                matches!(result, Err(FnxExecutionError::Cancelled(Stop::Poll(at))) if at == stop)
            );
            // All stops occur inside the first hub row, after the same charged
            // prefix. No complete graph or kernel result is published.
            assert_eq!(meter.units, graph.node_count() + 1);
            assert_eq!(meter.polls, stop);
        }
    }

    #[test]
    #[ignore = "paired adapter plus degree-kernel timing; run with --ignored --nocapture"]
    fn paired_hub_foundation_construction() {
        use std::hint::black_box;
        use std::time::Instant;

        for n in [2049, 8193, 16385] {
            let graph = hub(n, false);
            let mut samples = [Vec::new(), Vec::new()];
            let mut expected = None;
            for round in 0..6 {
                for arm in [round % 2, 1 - round % 2] {
                    let start = Instant::now();
                    let mut meter = Meter::default();
                    let copy = if arm == 0 {
                        old_copy(&graph, &mut meter)
                    } else {
                        copy_graph(&graph, &mut meter)
                    }
                    .unwrap();
                    let result = fnx_algorithms::degree_centrality(black_box(&copy));
                    samples[arm].push(start.elapsed().as_nanos());
                    assert_eq!(meter.units, 2 * n);
                    if let Some(expected) = &expected {
                        assert_eq!(&result, expected);
                    } else {
                        expected = Some(result);
                    }
                }
            }
            for sample in &mut samples {
                sample.sort_unstable();
            }
            eprintln!(
                "foundation_copy_plus_degree n={n} m={} old_median_ns={} indexed_median_ns={} ordered_results_equal=true",
                graph.edge_count(),
                samples[0][3],
                samples[1][3],
            );
        }
    }
}
