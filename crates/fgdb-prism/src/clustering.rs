//! Exact triangles and unweighted local clustering over a decoded projection.
//!
//! Build a query-local forward CSR for edges whose endpoints can participate
//! in a triangle, directed from the higher `(degree, VId)` endpoint to the lower
//! one. Mark a source's forward row and scan its targets' forward rows. A
//! triangle is then discovered once and adds one to all three exact counts.
//! The extra ordinal array is admitted scratch; it is never a new storage or
//! authorization root.

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
    /// Forward-CSR construction, counting and self-loop lookup probes,
    /// excluding admission and count-vector initialization.
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
    workspace_bytes::<C>(graph.node_count(), graph.edge_count())?;
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

/// Four n-sized passes, forward construction plus two forward-row visits,
/// and a full lower-degree row as an upper bound on each inner forward scan.
/// Includes the binary lookup bound needed to exclude self-loops from degree.
/// A high-degree star therefore admits linear, not quadratic, work. The bound
/// includes building the scratch CSR, even when the graph has no triangles.
pub(crate) fn estimated_work<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<usize, FnxExecutionError<C>> {
    let mut work = graph
        .node_count()
        .checked_mul(4)
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

/// Conservative bound on peak requested vector storage. `edges` is the
/// projection's simple edge count, including loops: an upper bound on the
/// one-way non-loop CSR. Counts, marks, offsets and forward targets coexist
/// while counting. Marks and targets are allocated only when needed; the
/// latter three vectors are dropped before coefficients are allocated. This
/// is requested-byte accounting, not an allocator-overhead or spill guarantee.
pub(crate) fn workspace_bytes<C>(n: usize, edges: usize) -> Result<usize, FnxExecutionError<C>> {
    let word = std::mem::size_of::<usize>();
    let counting = n
        .checked_mul(std::mem::size_of::<u64>() + word)
        .and_then(|bytes| {
            n.checked_add(1)
                .and_then(|offsets| offsets.checked_mul(word))
                .and_then(|offsets| bytes.checked_add(offsets))
        })
        .and_then(|bytes| {
            edges
                .checked_mul(word)
                .and_then(|targets| bytes.checked_add(targets))
        })
        .ok_or(FnxExecutionError::SizeOverflow)?;
    let conversion = n
        .checked_mul(std::mem::size_of::<u64>() + std::mem::size_of::<f64>())
        .ok_or(FnxExecutionError::SizeOverflow)?;
    Ok(counting.max(conversion))
}

struct ForwardRows {
    offsets: Vec<usize>,
    targets: Vec<usize>,
    has_wedges: bool,
}

impl ForwardRows {
    fn row(&self, source: usize) -> &[usize] {
        &self.targets[self.offsets[source]..self.offsets[source + 1]]
    }
}

/// A triangle needs two distinct other neighbors. The projection is simple,
/// so a row contains at most one self-loop; only length two needs inspection.
fn can_participate(row: &[usize], source: usize) -> bool {
    row.len() > 2 || (row.len() == 2 && row[0] != source && row[1] != source)
}

fn forward_rows<C>(
    graph: &SnapshotGraphView,
    witness: &mut ComplexityWitness,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<ForwardRows, FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut offsets = reserve(n.checked_add(1).ok_or(FnxExecutionError::SizeOverflow)?)?;
    let capacity = graph.edge_count();
    let mut targets = Vec::new();
    let mut has_wedges = false;
    offsets.push(0);
    for source in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        add(&mut witness.nodes_touched, 1)?;
        let row = graph
            .neighbors_indices(source)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        if !can_participate(row, source) {
            offsets.push(targets.len());
            continue;
        }
        for &target in row {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            add(&mut witness.edges_scanned, 1)?;
            let other = graph
                .neighbors_indices(target)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            // A self-loop has the same key and never enters the forward CSR.
            // Rows stay sorted by ordinal; degree only chooses their members.
            if can_participate(other, target) && (other.len(), target) < (row.len(), source) {
                if targets.len() == capacity {
                    return Err(FnxExecutionError::InvalidUpstreamResult);
                }
                if targets.is_empty() {
                    targets = reserve(capacity)?;
                }
                targets.push(target);
            }
        }
        has_wedges |= targets.len() - offsets[source] >= 2;
        offsets.push(targets.len());
    }
    Ok(ForwardRows {
        offsets,
        targets,
        has_wedges,
    })
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<TriangleStatistics, FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut witness = ComplexityWitness {
        algorithm: "triangles_degree_oriented_forward_csr".to_owned(),
        complexity_claim: "O(|V| + |E| + sum_edges min(deg(u), deg(v)))".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    let forward = forward_rows(graph, &mut witness, checkpoint)?;
    let mut triangles = reserve(n)?;
    let mut marked = if forward.has_wedges {
        reserve(n)?
    } else {
        Vec::new()
    };
    for _ in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        triangles.push(0u64);
        if forward.has_wedges {
            marked.push(usize::MAX);
        }
    }
    // Every triangle has one highest endpoint with two forward neighbors.
    // Without such a row, all counts are already known and no marks are needed.
    if forward.has_wedges {
        for source in 0..n {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            add(&mut witness.nodes_touched, 1)?;
            let row = forward.row(source);
            if row.len() < 2 {
                continue;
            }
            for &target in row {
                checkpoint().map_err(FnxExecutionError::Cancelled)?;
                add(&mut witness.edges_scanned, 1)?;
                marked[target] = source;
            }
            for &target in row {
                checkpoint().map_err(FnxExecutionError::Cancelled)?;
                add(&mut witness.edges_scanned, 1)?;
                for &third in forward.row(target) {
                    checkpoint().map_err(FnxExecutionError::Cancelled)?;
                    add(&mut witness.edges_scanned, 1)?;
                    if marked[third] == source {
                        for vertex in [source, target, third] {
                            triangles[vertex] = triangles[vertex]
                                .checked_add(1)
                                .ok_or(FnxExecutionError::SizeOverflow)?;
                        }
                    }
                }
            }
        }
    }
    drop(forward);
    drop(marked);
    let mut clustering = reserve(n)?;
    for node in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        let row = graph
            .neighbors_indices(node)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        // Zero triangles imply an exact +0.0 coefficient for every degree.
        // Retain the row invariant check, but no self-loop lookup is needed.
        if triangles[node] == 0 {
            clustering.push(0.0);
            continue;
        }
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

/// Exact pre-forward-CSR kernel, retained only for paired measurement and
/// differential control. It is never selected by a production call.
#[cfg(test)]
fn incumbent<C>(
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
            let prior = incumbent(&view, &mut || Ok::<(), Infallible>(())).unwrap();
            assert_eq!(actual.triangles, prior.triangles, "incumbent mask={mask}");
            assert_eq!(
                actual
                    .clustering
                    .iter()
                    .map(|x| x.to_bits())
                    .collect::<Vec<_>>(),
                prior
                    .clustering
                    .iter()
                    .map(|x| x.to_bits())
                    .collect::<Vec<_>>(),
                "incumbent mask={mask}"
            );
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
    fn forward_rows_respect_degree_before_vid_and_remove_every_loop() {
        let view = graph(
            8,
            &[
                (0, 1),
                (0, 2),
                (0, 3),
                (0, 4),
                (0, 5),
                (0, 6),
                (1, 2),
                (2, 3),
                (3, 4),
                (4, 1),
                (0, 0),
                (1, 1),
                (2, 2),
                (5, 5),
                (6, 6),
            ],
        );
        let before = (view.vertex_ids().to_vec(), view.digest());
        let mut witness = ComplexityWitness {
            algorithm: String::new(),
            complexity_claim: String::new(),
            nodes_touched: 0,
            edges_scanned: 0,
            queue_peak: 0,
        };
        let forward = forward_rows(&view, &mut witness, &mut || Ok::<(), Infallible>(())).unwrap();
        assert_eq!(forward.offsets.len(), view.node_count() + 1);
        assert_eq!(forward.targets.len(), 8);
        // The minimum VId is the highest-degree vertex: orienting by ordinal
        // alone would incorrectly give this row no forward neighbors.
        assert_eq!(forward.row(0), &[1, 2, 3, 4]);
        for source in 0..view.node_count() {
            let row = view.neighbors_indices(source).unwrap();
            assert!(forward.row(source).windows(2).all(|pair| pair[0] < pair[1]));
            for &target in row {
                let other = view.neighbors_indices(target).unwrap();
                let kept = forward.row(source).contains(&target);
                let eligible = row.iter().filter(|&&v| v != source).count() >= 2
                    && other.iter().filter(|&&v| v != target).count() >= 2;
                assert_eq!(
                    kept,
                    eligible && (other.len(), target) < (row.len(), source)
                );
                if source != target && eligible {
                    assert_ne!(kept, forward.row(target).contains(&source));
                } else {
                    assert!(!kept);
                }
            }
        }
        let actual = triangle_statistics(&view, limits(), || Ok::<(), Infallible>(())).unwrap();
        assert_eq!(actual.triangles, vec![4, 2, 2, 2, 2, 0, 0, 0]);
        let prior = incumbent(&view, &mut || Ok::<(), Infallible>(())).unwrap();
        assert_eq!(actual.triangles, prior.triangles);
        assert_eq!(actual.clustering, prior.clustering);
        assert_eq!(before, (view.vertex_ids().to_vec(), view.digest()));
    }

    #[test]
    fn triangle_free_structure_skips_counting_and_leaf_edges_allocate_no_targets() {
        let n = 64;
        for (edges, empty_targets) in [
            (Vec::new(), true),
            (
                (1..n)
                    .map(|target| (0, target))
                    .chain((0..n).map(|node| (node, node)))
                    .collect(),
                true,
            ),
            ((1..n).map(|node| (node - 1, node)).collect(), false),
        ] {
            let view = graph(n, &edges);
            let mut witness = ComplexityWitness {
                algorithm: String::new(),
                complexity_claim: String::new(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            };
            let forward =
                forward_rows(&view, &mut witness, &mut || Ok::<(), Infallible>(())).unwrap();
            assert!(!forward.has_wedges);
            assert_eq!(forward.targets.is_empty(), empty_targets);
            if empty_targets {
                assert_eq!(forward.targets.capacity(), 0);
            }
            let actual = run(&view, &mut || Ok::<(), Infallible>(())).unwrap();
            assert_eq!(actual.witness.nodes_touched, n);
            assert_eq!(actual.witness.edges_scanned, witness.edges_scanned);
            assert!(actual.triangles.iter().all(|&count| count == 0));
            assert!(actual.clustering.iter().all(|value| value.to_bits() == 0));
        }
    }

    #[test]
    fn work_bound_covers_construction_and_scratch_covers_all_live_vectors() {
        for (n, edges) in [
            (0, vec![]),
            (1, vec![(0, 0)]),
            (4, vec![(0, 0), (0, 1), (0, 2), (1, 2), (1, 3), (2, 3)]),
            (5000, (1..5000).map(|target| (0, target)).collect()),
        ] {
            let view = graph(n, &edges);
            let mut units = 0;
            let result = run(&view, &mut || {
                units += 1;
                Ok::<(), Infallible>(())
            })
            .unwrap();
            let bound = estimated_work(&view, &mut || Ok::<(), Infallible>(())).unwrap();
            assert!(units <= bound, "n={n} units={units} bound={bound}");
            assert!(result.witness.nodes_touched + result.witness.edges_scanned <= units);
            let word = std::mem::size_of::<usize>();
            let count_and_mark = n * (std::mem::size_of::<u64>() + word);
            let forward_offsets = (n + 1) * word;
            let forward_capacity = view.edge_count() * word;
            let conversion = n * (std::mem::size_of::<u64>() + std::mem::size_of::<f64>());
            assert_eq!(
                workspace_bytes::<Infallible>(n, view.edge_count()).unwrap(),
                (count_and_mark + forward_offsets + forward_capacity).max(conversion)
            );
        }
        for (n, edges) in [(usize::MAX, 0), (0, usize::MAX)] {
            assert!(matches!(
                workspace_bytes::<Infallible>(n, edges),
                Err(FnxExecutionError::SizeOverflow)
            ));
        }
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
        for view in [
            view,
            graph(4, &[(0, 1), (0, 2), (0, 3), (1, 1)]),
            graph(3, &[]),
        ] {
            let mut total = 0;
            triangle_statistics(&view, limits(), || {
                total += 1;
                Ok::<(), &'static str>(())
            })
            .unwrap();
            // This includes admission, vector initialization, every forward-CSR
            // construction edge, marking, intersections and coefficient lookups,
            // including graphs whose structure proves all counts are zero.
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

    /// A paired measurement, not an empirical production gate. Both kernels
    /// execute in one process over exactly the same decoded snapshot; the
    /// candidate's forward-CSR construction and cleanup are inside its timer.
    /// Results are fully produced and consumed, and comparisons happen after
    /// each pair. Raw samples expose noise and losses instead of asserting an
    /// arbitrary speed threshold. Run with `--release --ignored --nocapture`.
    #[test]
    #[ignore = "paired current/incumbent triangle measurement; includes scratch construction"]
    fn forward_csr_paired_incumbent_measurement() {
        use std::hint::black_box;
        use std::time::Instant;

        const REPETITIONS: usize = 32;

        fn fixture(shape: &str) -> SnapshotGraphView {
            let (n, edges) = match shape {
                "isolates" => (32_768, Vec::new()),
                "road" => {
                    let n = 32_768;
                    (n, (1..n).map(|v| (v - 1, v)).collect())
                }
                "lattice" => {
                    let width = 128;
                    let mut edges = Vec::new();
                    for row in 0..width {
                        for column in 0..width {
                            let vertex = row * width + column;
                            if column + 1 < width {
                                edges.push((vertex, vertex + 1));
                            }
                            if row + 1 < width {
                                edges.push((vertex, vertex + width));
                            }
                        }
                    }
                    (width * width, edges)
                }
                "star" => {
                    let n = 32_768;
                    (n, (1..n).map(|v| (0, v)).collect())
                }
                "clique" => {
                    let n = 384;
                    (
                        n,
                        (0..n)
                            .flat_map(|s| (s + 1..n).map(move |t| (s, t)))
                            .collect(),
                    )
                }
                "clustered" => {
                    let clusters = 64;
                    let width = 64;
                    let mut edges = Vec::new();
                    for cluster in 0..clusters {
                        let start = cluster * width;
                        for source in start..start + width {
                            for target in source + 1..start + width {
                                edges.push((source, target));
                            }
                        }
                        if cluster > 0 {
                            edges.push((start - 1, start));
                        }
                    }
                    (clusters * width, edges)
                }
                _ => panic!("unknown fixed measurement shape"),
            };
            graph(n, &edges)
        }

        fn measure(
            view: &SnapshotGraphView,
            current: bool,
            metered: bool,
        ) -> (u128, usize, u64, TriangleStatistics) {
            let mut units = 0usize;
            let mut checksum = 0u64;
            let mut last = None;
            let start = Instant::now();
            for repetition in 0..REPETITIONS {
                let statistics = if metered {
                    let mut checkpoint = || {
                        units = black_box(units + 1);
                        Ok::<(), Infallible>(())
                    };
                    if current {
                        run(black_box(view), &mut checkpoint)
                    } else {
                        incumbent(black_box(view), &mut checkpoint)
                    }
                } else if current {
                    run(black_box(view), &mut || Ok::<(), Infallible>(()))
                } else {
                    incumbent(black_box(view), &mut || Ok::<(), Infallible>(()))
                }
                .unwrap();
                let counts = statistics
                    .triangles
                    .iter()
                    .fold(0u64, |sum, &count| sum.wrapping_add(count));
                let coefficients = statistics
                    .clustering
                    .iter()
                    .fold(0u64, |sum, value| sum.wrapping_add(value.to_bits()));
                checksum = checksum
                    .wrapping_mul(31)
                    .wrapping_add(black_box(counts))
                    .wrapping_mul(31)
                    .wrapping_add(black_box(coefficients));
                if repetition + 1 == REPETITIONS {
                    last = Some(statistics);
                }
            }
            (
                start.elapsed().as_nanos(),
                units,
                black_box(checksum),
                last.unwrap(),
            )
        }

        println!(
            "{{\"scope\":\"decoded_triangle_kernel_including_scratch_build_and_result_consumption\",\"empirical_gate_activated\":false,\"samples_per_kernel\":9,\"repetitions_per_sample\":{REPETITIONS},\"nanoseconds\":\"total_per_sample\"}}"
        );
        for shape in ["isolates", "road", "lattice", "star", "clique", "clustered"] {
            let view = fixture(shape);
            for metered in [false, true] {
                let (_, _, prior_checksum, warm_prior) = measure(&view, false, metered);
                let (_, _, current_checksum, warm_current) = measure(&view, true, metered);
                assert_eq!(prior_checksum, current_checksum);
                assert_eq!(warm_prior.triangles, warm_current.triangles);
                assert_eq!(warm_prior.clustering, warm_current.clustering);
                let (mut prior_ns, mut current_ns) = (Vec::new(), Vec::new());
                let (mut prior_units, mut current_units) = (0, 0);
                let (mut prior_edges, mut current_edges) = (0, 0);
                for sample in 0..9 {
                    let (prior, current) = if sample % 2 == 0 {
                        (
                            measure(&view, false, metered),
                            measure(&view, true, metered),
                        )
                    } else {
                        let current = measure(&view, true, metered);
                        (measure(&view, false, metered), current)
                    };
                    assert_eq!(prior.2, current.2, "{shape} {sample} consumption checksum");
                    assert_eq!(prior.3.triangles, current.3.triangles, "{shape} {sample}");
                    assert!(
                        prior
                            .3
                            .clustering
                            .iter()
                            .zip(&current.3.clustering)
                            .all(|(prior, current)| prior.to_bits() == current.to_bits()),
                        "{shape} {sample} coefficient bits"
                    );
                    prior_ns.push(prior.0);
                    current_ns.push(current.0);
                    (prior_units, current_units) = (prior.1, current.1);
                    (prior_edges, current_edges) = (
                        prior.3.witness.edges_scanned,
                        current.3.witness.edges_scanned,
                    );
                }
                let (mut prior_sorted, mut current_sorted) = (prior_ns.clone(), current_ns.clone());
                prior_sorted.sort_unstable();
                current_sorted.sort_unstable();
                let speedup = prior_sorted[4] as f64 / current_sorted[4] as f64;
                let verdict = if speedup >= 1.0 { "win" } else { "loss" };
                let prior_bytes = view.node_count()
                    * (std::mem::size_of::<u64>()
                        + std::mem::size_of::<usize>()
                        + std::mem::size_of::<f64>());
                let current_bytes =
                    workspace_bytes::<Infallible>(view.node_count(), view.edge_count()).unwrap();
                println!(
                    "{{\"shape\":\"{shape}\",\"counted_checkpoints\":{metered},\"vertices\":{},\"edges\":{},\"incumbent_ns\":{prior_ns:?},\"current_ns\":{current_ns:?},\"incumbent_median_ns\":{},\"current_median_ns\":{},\"speedup\":{speedup:.4},\"verdict\":\"{verdict}\",\"incumbent_units_per_sample\":{prior_units},\"current_units_per_sample\":{current_units},\"incumbent_edge_scans_per_run\":{prior_edges},\"current_edge_scans_per_run\":{current_edges},\"incumbent_scratch_bytes\":{prior_bytes},\"current_scratch_bound_bytes\":{current_bytes}}}",
                    view.node_count(),
                    view.edge_count(),
                    prior_sorted[4],
                    current_sorted[4]
                );
            }
        }
    }
}
