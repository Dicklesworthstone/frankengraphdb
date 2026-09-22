//! Weighted SSSP over the existing decoded projection, without copying edges.
//!
//! The indexed min-heap contains at most one entry per unsettled vertex. This
//! avoids the O(m) duplicate queue of lazy-deletion Dijkstra implementations.
//! Equal costs use FIFO discovery from canonical VId-ordered rows. Only finite,
//! nonnegative projected weights are accepted, including zero-weight cycles.

use crate::execute::{admit, reserve};
use crate::{
    ComplexityWitness, FnxBindError, FnxBindErrorKind, FnxExecutionError, FnxExecutionLimits,
    GraphView, SnapshotGraphView,
};
use fgdb_types::VId;

/// The pinned foundation's relaxation threshold (not a convergence tolerance).
pub const FNX_DIJKSTRA_EPSILON: f64 = 1e-12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DijkstraComparison {
    /// Accept every representable improvement. This is the primitive default.
    Strict = 0,
    /// Match fnx's `candidate < previous - 1e-12` relaxation and FIFO ties.
    FnxEpsilon = 1,
}

/// Validated weighted-distance parameters. The cutoff is inclusive and is a
/// cost, not a hop count. Negative zero is normalized for canonical binding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DijkstraOptions {
    source: VId,
    cutoff: Option<f64>,
    comparison: DijkstraComparison,
}

impl DijkstraOptions {
    pub fn new(source: VId, cutoff: Option<f64>) -> Result<Self, FnxBindError> {
        if cutoff.is_some_and(|value| !value.is_finite() || value < 0.0) {
            return Err(FnxBindError {
                at: 0,
                kind: FnxBindErrorKind::InvalidArgument(
                    "cutoff must be a finite nonnegative cost or NULL",
                ),
            });
        }
        Ok(Self {
            source,
            cutoff: cutoff.map(|value| if value == 0.0 { 0.0 } else { value }),
            comparison: DijkstraComparison::Strict,
        })
    }

    pub const fn source(self) -> VId {
        self.source
    }

    pub const fn cutoff(self) -> Option<f64> {
        self.cutoff
    }

    pub const fn with_comparison(mut self, comparison: DijkstraComparison) -> Self {
        self.comparison = comparison;
        self
    }

    pub const fn comparison(self) -> DijkstraComparison {
        self.comparison
    }
}

/// Kernel output indexed by the input projection's snapshot-local ordinals.
/// None means unreachable or beyond the cutoff, never an infinite distance.
/// This primitive output is not a query certificate or an authorization grant.
#[derive(Clone, Debug, PartialEq)]
pub struct DijkstraOutput {
    pub distances: Vec<Option<f64>>,
    pub row_count: usize,
    /// Finalized vertices, relaxation edges, and maximum active heap entries.
    /// Preflight weight validation and output conversion are separate work.
    pub witness: ComplexityWitness,
}

/// Execute the in-core primitive with work/row admission and cooperative
/// cancellation. The iteration allowance is irrelevant to this finite kernel.
/// Allocations and the caller's checkpoint callback are not preemptible.
pub fn dijkstra<C>(
    graph: &SnapshotGraphView,
    options: DijkstraOptions,
    limits: FnxExecutionLimits,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<DijkstraOutput, FnxExecutionError<C>> {
    checkpoint().map_err(FnxExecutionError::Cancelled)?;
    let n = graph.node_count();
    let mut arcs = 0usize;
    for node in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        let row = graph
            .neighbors_indices(node)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        arcs = arcs
            .checked_add(row.len())
            .ok_or(FnxExecutionError::SizeOverflow)?;
    }
    admit(
        "estimated work",
        estimated_work(n, arcs)?,
        limits.max_estimated_work,
    )?;
    workspace_bytes::<C>(n)?;
    run(graph, options, limits.max_result_rows, &mut checkpoint)
}

/// Conservative admission model including validation and both heap sift
/// directions. It is a work model, not an instruction count or wall-clock bound.
pub(crate) fn estimated_work<C>(n: usize, arcs: usize) -> Result<usize, FnxExecutionError<C>> {
    let height = (usize::BITS - n.leading_zeros()) as usize;
    let factor = height
        .checked_mul(2)
        .and_then(|value| value.checked_add(3))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    n.checked_add(arcs)
        .and_then(|value| value.checked_mul(factor))
        .ok_or(FnxExecutionError::SizeOverflow)
}

pub(crate) fn workspace_bytes<C>(n: usize) -> Result<usize, FnxExecutionError<C>> {
    // Heap entries, inverse positions, final distances, overflow reachability.
    let per_vertex = std::mem::size_of::<Entry>()
        + std::mem::size_of::<usize>()
        + std::mem::size_of::<Option<f64>>()
        + std::mem::size_of::<bool>();
    n.checked_mul(per_vertex)
        .ok_or(FnxExecutionError::SizeOverflow)
}

#[derive(Clone, Copy)]
struct Entry {
    node: usize,
    cost: f64,
    sequence: u64,
}

struct IndexedHeap {
    entries: Vec<Entry>,
    positions: Vec<usize>,
    sequence: u64,
    comparison: DijkstraComparison,
}

impl IndexedHeap {
    fn before(left: Entry, right: Entry) -> bool {
        left.cost
            .total_cmp(&right.cost)
            .then(left.sequence.cmp(&right.sequence))
            .is_lt()
    }

    fn swap(&mut self, left: usize, right: usize) {
        self.entries.swap(left, right);
        self.positions[self.entries[left].node] = left;
        self.positions[self.entries[right].node] = right;
    }

    fn offer<C>(
        &mut self,
        node: usize,
        cost: f64,
        checkpoint: &mut impl FnMut() -> Result<(), C>,
    ) -> Result<(), FnxExecutionError<C>> {
        let mut position = self.positions[node];
        if position != usize::MAX {
            let previous = self.entries[position].cost;
            let threshold = match self.comparison {
                DijkstraComparison::Strict => previous,
                DijkstraComparison::FnxEpsilon => previous - FNX_DIJKSTRA_EPSILON,
            };
            if cost >= threshold {
                return Ok(());
            }
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let entry = Entry {
            node,
            cost,
            sequence: self.sequence,
        };
        if position == usize::MAX {
            position = self.entries.len();
            self.positions[node] = position;
            self.entries.push(entry);
        } else {
            self.entries[position] = entry;
        }
        while position > 0 {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            let parent = (position - 1) / 2;
            if !Self::before(self.entries[position], self.entries[parent]) {
                break;
            }
            self.swap(position, parent);
            position = parent;
        }
        Ok(())
    }

    fn pop<C>(
        &mut self,
        checkpoint: &mut impl FnMut() -> Result<(), C>,
    ) -> Result<Option<Entry>, FnxExecutionError<C>> {
        let Some(last) = self.entries.pop() else {
            return Ok(None);
        };
        if self.entries.is_empty() {
            self.positions[last.node] = usize::MAX;
            return Ok(Some(last));
        }
        let first = std::mem::replace(&mut self.entries[0], last);
        self.positions[first.node] = usize::MAX;
        self.positions[last.node] = 0;
        let mut position = 0;
        // A non-leaf has position < len / 2, so 2*position+1 cannot overflow.
        while position < self.entries.len() / 2 {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            let left = 2 * position + 1;
            let right = left + 1;
            let child = if right < self.entries.len()
                && Self::before(self.entries[right], self.entries[left])
            {
                right
            } else {
                left
            };
            if !Self::before(self.entries[child], self.entries[position]) {
                break;
            }
            self.swap(position, child);
            position = child;
        }
        Ok(Some(first))
    }
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    options: DijkstraOptions,
    row_limit: usize,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<DijkstraOutput, FnxExecutionError<C>> {
    let source = graph
        .vertex_ordinal(options.source)
        .ok_or(FnxExecutionError::UnknownSource(options.source))?;
    admit("result rows", 1, row_limit)?;
    let n = graph.node_count();
    // Reject the entire incompatible projection, not just the prefix reached
    // before a cutoff. Otherwise an early exit could hide an invalid weight.
    for node in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        let (_, weights) = graph
            .projected_row(node)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for &weight in weights {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            if !weight.is_finite() {
                return Err(FnxExecutionError::InvalidNumericResult);
            }
            if weight < 0.0 {
                return Err(FnxExecutionError::NegativeWeight);
            }
        }
    }
    let mut distances = reserve(n)?;
    let mut overflowed = reserve(n)?;
    let mut heap = IndexedHeap {
        entries: reserve(n)?,
        positions: reserve(n)?,
        sequence: 0,
        comparison: options.comparison,
    };
    for _ in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        distances.push(None);
        overflowed.push(false);
        heap.positions.push(usize::MAX);
    }
    heap.offer(source, 0.0, checkpoint)?;
    let mut discovered = 1usize;
    let mut witness = ComplexityWitness {
        algorithm: "single_source_dijkstra_indexed_heap".to_owned(),
        complexity_claim: "O((|V| + |E|) * log(1 + |V|))".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 1,
    };
    loop {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        let Some(Entry { node, cost, .. }) = heap.pop(checkpoint)? else {
            break;
        };
        distances[node] = Some(cost);
        witness.nodes_touched = witness
            .nodes_touched
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let (targets, weights) = graph
            .projected_row(node)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for (&target, &weight) in targets.iter().zip(weights) {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            witness.edges_scanned = witness
                .edges_scanned
                .checked_add(1)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            if distances[target].is_some() {
                continue;
            }
            let candidate = cost + weight;
            if !candidate.is_finite() {
                // An overflowing alternative must not reject a later finite
                // shortest path. With a finite cutoff it is simply out of range.
                if options.cutoff.is_none() {
                    overflowed[target] = true;
                }
                continue;
            }
            if options.cutoff.is_some_and(|cutoff| candidate > cutoff) {
                continue;
            }
            if heap.positions[target] == usize::MAX {
                let requested = discovered
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                admit("result rows", requested, row_limit)?;
                discovered = requested;
            }
            heap.offer(target, candidate, checkpoint)?;
            witness.queue_peak = witness.queue_peak.max(heap.entries.len());
        }
    }
    for node in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        if overflowed[node] && distances[node].is_none() {
            // Reachable but unrepresentable is not the same as unreachable.
            return Err(FnxExecutionError::InvalidNumericResult);
        }
    }
    Ok(DijkstraOutput {
        distances,
        row_count: discovered,
        witness,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Directedness, ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
        SelfLoopPolicy, SnapshotBinding,
    };
    use fgdb_types::{CommitSeq, EId, ids::ObjectId};
    use fnx_classes::{Graph, digraph::DiGraph};
    use std::convert::Infallible;

    fn graph(
        n: usize,
        edges: &[(usize, usize, f64)],
        direction: Directedness,
    ) -> SnapshotGraphView {
        let vertices: Vec<_> = (0..n)
            .map(|i| VId(u128::MAX - n as u128 + i as u128))
            .collect();
        let edges: Vec<_> = edges
            .iter()
            .enumerate()
            .map(|(id, &(s, t, weight))| ProjectionEdge {
                eid: EId(id as u128),
                source: vertices[s],
                target: vertices[t],
                weight,
            })
            .collect();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([7; 32]),
                as_of: CommitSeq(9),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: direction,
                parallel_edges: ParallelEdgePolicy::Minimum,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: n,
                max_input_edges: edges.len(),
                max_adjacency_entries: edges.len() * 2,
                max_workspace_bytes: 1 << 24,
            },
        )
        .unwrap()
    }
    fn limits() -> FnxExecutionLimits {
        FnxExecutionLimits {
            max_iterations: 0,
            max_result_rows: 100,
            max_estimated_work: 1 << 24,
        }
    }
    fn options(graph: &SnapshotGraphView, source: usize, cutoff: Option<f64>) -> DijkstraOptions {
        DijkstraOptions::new(graph.vertex_id(source).unwrap(), cutoff).unwrap()
    }
    fn execute(graph: &SnapshotGraphView, source: usize, cutoff: Option<f64>) -> DijkstraOutput {
        dijkstra(graph, options(graph, source, cutoff), limits(), || {
            Ok::<(), Infallible>(())
        })
        .unwrap()
    }

    #[test]
    fn all_small_weighted_topologies_match_independent_dense_floyd_warshall() {
        for mask in 0u16..512 {
            let edges: Vec<_> = (0..9)
                .filter(|bit| mask & (1 << bit) != 0)
                .map(|bit| (bit / 3, bit % 3, [0.0, 0.5, 3.0, 7.0][bit % 4]))
                .collect();
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let view = graph(3, &edges, direction);
                let mut dense = [[f64::INFINITY; 3]; 3];
                for i in 0..3 {
                    dense[i][i] = 0.0;
                    let (targets, weights) = view.projected_row(i).unwrap();
                    for (&j, &weight) in targets.iter().zip(weights) {
                        dense[i][j] = dense[i][j].min(weight);
                    }
                }
                for k in 0..3 {
                    for i in 0..3 {
                        for j in 0..3 {
                            dense[i][j] = dense[i][j].min(dense[i][k] + dense[k][j]);
                        }
                    }
                }
                for source in 0..3 {
                    for cutoff in [None, Some(0.0), Some(0.5), Some(3.0)] {
                        let actual = execute(&view, source, cutoff);
                        let expected: Vec<_> = dense[source]
                            .iter()
                            .copied()
                            .map(|distance| {
                                (distance.is_finite()
                                    && cutoff.is_none_or(|limit| distance <= limit))
                                .then_some(distance)
                            })
                            .collect();
                        assert_eq!(
                            actual.distances, expected,
                            "mask={mask} {direction:?} source={source} cutoff={cutoff:?}"
                        );
                        assert_eq!(
                            actual.row_count,
                            expected.iter().filter(|value| value.is_some()).count()
                        );
                        assert_eq!(actual.witness.nodes_touched, actual.row_count);
                        assert!(actual.witness.queue_peak <= 3);
                    }
                }
            }
        }
    }

    #[test]
    fn all_small_unit_topologies_match_pinned_standalone_fnx() {
        for mask in 0u16..512 {
            let edges: Vec<_> = (0..9)
                .filter(|bit| mask & (1 << bit) != 0)
                .map(|bit| (bit / 3, bit % 3, 1.0))
                .collect();
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let view = graph(3, &edges, direction);
                let mut directed = DiGraph::strict();
                let mut undirected = Graph::strict();
                for name in view.nodes_ordered() {
                    let _ = directed.add_node(name);
                    let _ = undirected.add_node(name);
                }
                for source in 0..3 {
                    for &target in view.neighbors_indices(source).unwrap() {
                        let s = view.get_node_name(source).unwrap();
                        let t = view.get_node_name(target).unwrap();
                        directed.add_edge(s, t).unwrap();
                        if source <= target {
                            undirected.add_edge(s, t).unwrap();
                        }
                    }
                }
                for source in 0..3 {
                    let name = view.get_node_name(source).unwrap();
                    let oracle = if direction == Directedness::Undirected {
                        fnx_algorithms::single_source_dijkstra_path_length(
                            &undirected,
                            name,
                            "weight",
                        )
                    } else {
                        fnx_algorithms::single_source_dijkstra_path_length_directed(
                            &directed, name, "weight",
                        )
                    };
                    let mut expected = vec![None; 3];
                    for (node, distance) in oracle {
                        expected[view.get_node_index(&node).unwrap()] = Some(distance);
                    }
                    assert_eq!(execute(&view, source, None).distances, expected);
                }
            }
        }
    }

    #[test]
    fn cutoff_keeps_zero_cost_closure_and_minimum_parallel_edge_law() {
        let view = graph(
            5,
            &[
                (0, 1, 20.0),
                (0, 1, 2.0),
                (1, 2, 0.0),
                (2, 1, 0.0),
                (2, 3, 0.5),
            ],
            Directedness::Directed,
        );
        assert_eq!(
            execute(&view, 0, Some(2.0)).distances,
            vec![Some(0.0), Some(2.0), Some(2.0), None, None]
        );
        assert_eq!(
            execute(&view, 1, Some(0.0)).distances,
            vec![None, Some(0.0), Some(0.0), None, None]
        );
    }

    #[test]
    fn overflow_alternatives_do_not_poison_finite_paths_or_cutoffs() {
        let overflow = [(0, 1, f64::MAX * 0.75), (1, 3, f64::MAX * 0.75)];
        let view = graph(4, &overflow, Directedness::Directed);
        assert!(matches!(
            dijkstra(&view, options(&view, 0, None), limits(), || Ok::<
                (),
                Infallible,
            >(
                ()
            )),
            Err(FnxExecutionError::InvalidNumericResult)
        ));
        assert_eq!(execute(&view, 0, Some(f64::MAX)).row_count, 2);
        let mut finite = overflow.to_vec();
        finite.extend([(0, 2, f64::MAX * 0.875), (2, 3, 0.0)]);
        let view = graph(4, &finite, Directedness::Directed);
        assert_eq!(execute(&view, 0, None).distances[3], Some(f64::MAX * 0.875));
    }

    #[test]
    fn source_numeric_and_admission_refusals_are_explicit() {
        for cutoff in [f64::NAN, f64::INFINITY, -1.0] {
            assert!(DijkstraOptions::new(VId(1), Some(cutoff)).is_err());
        }
        assert_eq!(
            DijkstraOptions::new(VId(1), Some(-0.0))
                .unwrap()
                .cutoff()
                .unwrap()
                .to_bits(),
            0
        );
        let view = graph(4, &[(2, 3, -1.0)], Directedness::Directed);
        assert!(matches!(
            dijkstra(&view, options(&view, 0, Some(0.0)), limits(), || Ok::<
                (),
                Infallible,
            >(
                ()
            )),
            Err(FnxExecutionError::NegativeWeight)
        ));
        let view = graph(4, &[], Directedness::Directed);
        assert!(matches!(
            dijkstra(
                &view,
                DijkstraOptions::new(VId(0), None).unwrap(),
                limits(),
                || Ok::<(), Infallible>(())
            ),
            Err(FnxExecutionError::UnknownSource(VId(0)))
        ));
        let tight = FnxExecutionLimits {
            max_result_rows: 1,
            ..limits()
        };
        assert_eq!(
            dijkstra(
                &view,
                options(&view, 0, None),
                tight,
                || Ok::<(), Infallible>(())
            )
            .unwrap()
            .row_count,
            1
        );
        for cap in [
            FnxExecutionLimits {
                max_result_rows: 0,
                ..limits()
            },
            FnxExecutionLimits {
                max_estimated_work: 0,
                ..limits()
            },
        ] {
            assert!(matches!(
                dijkstra(
                    &view,
                    options(&view, 0, None),
                    cap,
                    || Ok::<(), Infallible>(())
                ),
                Err(FnxExecutionError::LimitExceeded { .. })
            ));
        }
        assert!(estimated_work::<Infallible>(usize::MAX, 1).is_err());
        assert!(workspace_bytes::<Infallible>(usize::MAX).is_err());
    }

    #[test]
    fn every_checkpoint_including_heap_sifts_cancels_without_partial_results() {
        let view = graph(
            5,
            &[
                (0, 1, 8.0),
                (0, 2, 4.0),
                (0, 3, 2.0),
                (3, 1, 1.0),
                (1, 2, 0.0),
                (2, 4, 1.0),
            ],
            Directedness::Directed,
        );
        let mut total = 0;
        dijkstra(&view, options(&view, 0, None), limits(), || {
            total += 1;
            Ok::<(), &'static str>(())
        })
        .unwrap();
        for stop in 1..=total {
            let mut count = 0;
            let result = dijkstra(&view, options(&view, 0, None), limits(), || {
                count += 1;
                if count == stop { Err("cancel") } else { Ok(()) }
            });
            assert!(matches!(
                result,
                Err(FnxExecutionError::Cancelled("cancel"))
            ));
            assert_eq!(count, stop);
        }
    }
}
