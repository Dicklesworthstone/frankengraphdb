//! Exact triangles and unweighted local clustering over borrowed simple rows.
//!
//! Each edge is visited from its higher-(degree,VId) endpoint. Mark that
//! endpoint's neighbors, scan the lower-degree row, and count each common
//! neighbor as the third vertex. A triangle contributes exactly once to each
//! vertex (via its opposite edge). No adjacency sets or graph copies are built.

use crate::execute::{admit, reserve};
use crate::{
    ComplexityWitness, FnxExecutionError, FnxExecutionLimits, FnxGraphKind, GraphView,
    SnapshotGraphView,
};

#[derive(Clone, Debug, PartialEq)]
pub struct TriangleStatistics {
    /// One exact count per snapshot-local vertex; self-loops are ignored.
    pub triangles: Vec<u64>,
    /// 2*t/(d*(d-1)), or zero for fewer than two distinct other neighbors.
    /// This is unweighted clustering, not a weighted/directed formula.
    pub clustering: Vec<f64>,
    /// Counting and self-loop lookup probes, excluding admission/initialization.
    pub witness: ComplexityWitness,
}

/// Compute both statistics once under in-core admission and cancellation.
/// Callers must explicitly choose an undirected projection before execution.
/// Limits are admission allowances, not a hard memory quota or spill contract.
pub fn triangle_statistics<C>(
    graph: &SnapshotGraphView,
    limits: FnxExecutionLimits,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<TriangleStatistics, FnxExecutionError<C>> {
    checkpoint().map_err(FnxExecutionError::Cancelled)?;
    if graph.is_directed() {
        return Err(FnxExecutionError::GraphKind {
            required: FnxGraphKind::Undirected,
        });
    }
    admit("result rows", graph.node_count(), limits.max_result_rows)?;
    admit(
        "estimated work",
        estimated_work(graph, &mut checkpoint)?,
        limits.max_estimated_work,
    )?;
    workspace_bytes::<C>(graph.node_count())?;
    run(graph, &mut checkpoint)
}

fn bits(length: usize) -> usize {
    (usize::BITS - length.leading_zeros()) as usize
}
fn add<C>(counter: &mut usize, value: usize) -> Result<(), FnxExecutionError<C>> {
    *counter = counter
        .checked_add(value)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    Ok(())
}

/// n-sized passes + two row visits + one lower-degree scan per simple edge.
/// Includes the binary lookup bound needed to exclude self-loops from degree.
/// A high-degree star therefore admits linear, not quadratic, work.
pub(crate) fn estimated_work<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<usize, FnxExecutionError<C>> {
    let mut work = graph
        .node_count()
        .checked_mul(2)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    for source in 0..graph.node_count() {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        let row = graph
            .neighbors_indices(source)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        add(
            &mut work,
            row.len()
                .checked_mul(2)
                .ok_or(FnxExecutionError::SizeOverflow)?,
        )?;
        add(&mut work, bits(row.len()))?;
        for &target in row {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            if source < target {
                let other = graph
                    .neighbors_indices(target)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                add(&mut work, row.len().min(other.len()))?;
            }
        }
    }
    Ok(work)
}

pub(crate) fn workspace_bytes<C>(n: usize) -> Result<usize, FnxExecutionError<C>> {
    n.checked_mul(
        std::mem::size_of::<u64>() + std::mem::size_of::<usize>() + std::mem::size_of::<f64>(),
    )
    .ok_or(FnxExecutionError::SizeOverflow)
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<TriangleStatistics, FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut triangles = reserve(n)?;
    let mut marked = reserve(n)?;
    let mut clustering = reserve(n)?;
    for _ in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        triangles.push(0u64);
        marked.push(usize::MAX);
    }
    let mut witness = ComplexityWitness {
        algorithm: "triangles_degree_oriented_mark_rows".to_owned(),
        complexity_claim: "O(|V| + |E| + sum_edges min(deg(u), deg(v)))".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    for source in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        add(&mut witness.nodes_touched, 1)?;
        let row = graph
            .neighbors_indices(source)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for &target in row {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            add(&mut witness.edges_scanned, 1)?;
            if source != target {
                marked[target] = source;
            }
        }
        for &target in row {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            add(&mut witness.edges_scanned, 1)?;
            let other = graph
                .neighbors_indices(target)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            if (other.len(), target) >= (row.len(), source) {
                continue;
            }
            for &third in other {
                checkpoint().map_err(FnxExecutionError::Cancelled)?;
                add(&mut witness.edges_scanned, 1)?;
                if third != target && third != source && marked[third] == source {
                    triangles[third] = triangles[third]
                        .checked_add(1)
                        .ok_or(FnxExecutionError::SizeOverflow)?;
                }
            }
        }
    }
    for node in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        let row = graph
            .neighbors_indices(node)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        let mut low = 0;
        let mut high = row.len();
        let mut has_loop = false;
        while low < high {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            add(&mut witness.edges_scanned, 1)?;
            let middle = low + (high - low) / 2;
            match row[middle].cmp(&node) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    has_loop = true;
                    break;
                }
            }
        }
        let degree = (row.len() - usize::from(has_loop)) as u128;
        let coefficient = if degree < 2 {
            0.0
        } else {
            let denominator = degree
                .checked_mul(degree - 1)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            let numerator = u128::from(triangles[node]) * 2;
            if numerator > denominator {
                return Err(FnxExecutionError::InvalidUpstreamResult);
            }
            numerator as f64 / denominator as f64
        };
        clustering.push(coefficient);
    }
    Ok(TriangleStatistics {
        triangles,
        clustering,
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
    use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
    use fnx_classes::Graph;
    use std::convert::Infallible;

    fn graph(n: usize, edges: &[(usize, usize)]) -> SnapshotGraphView {
        let vertices: Vec<_> = (0..n).map(|i| VId(i as u128 * 17)).collect();
        let edges: Vec<_> = edges
            .iter()
            .enumerate()
            .map(|(id, &(s, t))| ProjectionEdge {
                eid: EId(id as u128),
                source: vertices[s],
                target: vertices[t],
                weight: -3.0,
            })
            .collect();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([3; 32]),
                as_of: CommitSeq(4),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: Directedness::Undirected,
                parallel_edges: ParallelEdgePolicy::CollapseUnit,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: n,
                max_input_edges: edges.len(),
                max_adjacency_entries: edges.len() * 2,
                max_workspace_bytes: 1 << 26,
            },
        )
        .unwrap()
    }
    fn limits() -> FnxExecutionLimits {
        FnxExecutionLimits {
            max_iterations: 0,
            max_result_rows: 10000,
            max_estimated_work: 1 << 26,
        }
    }

    #[test]
    fn all_four_vertex_topologies_including_loops_match_fnx_and_brute_force() {
        let pairs: Vec<_> = (0..4).flat_map(|i| (i..4).map(move |j| (i, j))).collect();
        for mask in 0usize..(1 << pairs.len()) {
            let edges: Vec<_> = pairs
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, &pair)| pair)
                .collect();
            let view = graph(4, &edges);
            let actual = triangle_statistics(&view, limits(), || Ok::<(), Infallible>(())).unwrap();
            let mut expected = vec![0u64; 4];
            for a in 0..4 {
                for b in (a + 1)..4 {
                    for c in (b + 1)..4 {
                        if edges.contains(&(a, b))
                            && edges.contains(&(a, c))
                            && edges.contains(&(b, c))
                        {
                            expected[a] += 1;
                            expected[b] += 1;
                            expected[c] += 1;
                        }
                    }
                }
            }
            assert_eq!(actual.triangles, expected, "mask={mask}");
            let mut oracle = Graph::strict();
            for name in view.nodes_ordered() {
                let _ = oracle.add_node(name);
            }
            for &(s, t) in &edges {
                oracle
                    .add_edge(
                        view.get_node_name(s).unwrap(),
                        view.get_node_name(t).unwrap(),
                    )
                    .unwrap();
            }
            for value in fnx_algorithms::triangles(&oracle).triangles {
                assert_eq!(
                    actual.triangles[view.get_node_index(&value.node).unwrap()],
                    value.count as u64
                );
            }
            for value in fnx_algorithms::clustering_coefficient(&oracle).scores {
                assert_eq!(
                    actual.clustering[view.get_node_index(&value.node).unwrap()].to_bits(),
                    value.score.to_bits()
                );
            }
            let bound = estimated_work(&view, &mut || Ok::<(), Infallible>(())).unwrap();
            assert!(actual.witness.nodes_touched + actual.witness.edges_scanned <= bound);
        }
    }

    #[test]
    fn hubs_are_linear_and_parallel_edges_do_not_multiply_triangles() {
        let star: Vec<_> = (1..5000).map(|i| (0, i)).collect();
        let view = graph(5000, &star);
        let actual = triangle_statistics(&view, limits(), || Ok::<(), Infallible>(())).unwrap();
        assert!(actual.triangles.iter().all(|&count| count == 0));
        assert!(actual.witness.edges_scanned < star.len() * 8);
        let view = graph(4, &[(0, 1), (1, 0), (0, 1), (1, 2), (2, 0), (0, 0)]);
        let actual = triangle_statistics(&view, limits(), || Ok::<(), Infallible>(())).unwrap();
        assert_eq!(actual.triangles, vec![1, 1, 1, 0]);
        assert_eq!(actual.clustering, vec![1.0, 1.0, 1.0, 0.0]);
        let empty =
            triangle_statistics(&graph(0, &[]), limits(), || Ok::<(), Infallible>(())).unwrap();
        assert!(empty.triangles.is_empty() && empty.clustering.is_empty());
    }

    #[test]
    fn row_and_work_limits_and_every_checkpoint_refuse_without_partial_output() {
        let view = graph(4, &[(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3), (0, 0)]);
        for cap in [
            FnxExecutionLimits {
                max_result_rows: 3,
                ..limits()
            },
            FnxExecutionLimits {
                max_estimated_work: 0,
                ..limits()
            },
        ] {
            assert!(matches!(
                triangle_statistics(&view, cap, || Ok::<(), Infallible>(())),
                Err(FnxExecutionError::LimitExceeded { .. })
            ));
        }
        let mut total = 0;
        triangle_statistics(&view, limits(), || {
            total += 1;
            Ok::<(), &'static str>(())
        })
        .unwrap();
        for stop in 1..=total {
            let mut count = 0;
            let result = triangle_statistics(&view, limits(), || {
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
