//! Exact, checkpointed traversal and label kernels. They borrow decoded adjacency, never
//! construct a second Graph/DiGraph, recurse on the process stack, or consult
//! weights that their registered semantics ignore.

use crate::execute::{ComplexityWitness, KernelOutput, KernelValues, admit, reserve};
use crate::{
    FNX_METER_RUN, FnxAlgorithm, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView,
};

fn witness(algorithm: &str) -> ComplexityWitness {
    ComplexityWitness {
        algorithm: algorithm.to_owned(),
        complexity_claim: "O(|V| + |E|)".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    }
}
fn increment<C>(counter: &mut usize) -> Result<(), FnxExecutionError<C>> {
    *counter = counter
        .checked_add(1)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    Ok(())
}

/// Charged work for a completed CDLP call: the shared admission/conversion
/// passes, a loop-law scan, vector initialization, and every label update.
/// Directed incidence scans read both CSR directions; undirected scans read
/// the shared symmetric row once. Zero iterations still validates the graph
/// and initializes all output labels. This is not a CPU-instruction estimate.
pub(crate) fn cdlp_estimated_work(
    n: usize,
    arcs: usize,
    max_iter: usize,
    directed: bool,
) -> Option<usize> {
    let incidences = arcs.checked_mul(if directed { 2 } else { 1 })?;
    let iterations = n.checked_add(incidences)?.checked_mul(max_iter)?;
    let vectors = if max_iter == 0 || n == 0 { 1 } else { 4 };
    n.checked_mul(3 + vectors)?
        .checked_add(arcs)?
        .checked_add(iterations)?
        .checked_add(3)
}

/// Graphalytics §2.3.4/A.5, using dense ordinals only because the projection
/// orders them by the complete VId. The two label buffers prevent any update
/// from observing labels written during its own round. Epoch-tagged counters
/// avoid a map allocation or an O(n) histogram clear for every source vertex.
pub(crate) fn cdlp<C>(
    graph: &SnapshotGraphView,
    max_iter: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    // The registered simple-graph law applies even to a zero-iteration call.
    for source in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let row = graph
            .neighbors_indices(source)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for batch in row.chunks(FNX_METER_RUN) {
            checkpoint
                .units(batch.len())
                .map_err(FnxExecutionError::Cancelled)?;
            if batch.contains(&source) {
                return Err(FnxExecutionError::SelfLoopRefused);
            }
        }
    }
    let mut labels = reserve(n)?;
    while labels.len() < n {
        let start = labels.len();
        let len = (n - start).min(FNX_METER_RUN);
        checkpoint
            .units(len)
            .map_err(FnxExecutionError::Cancelled)?;
        labels.extend(start..start + len);
    }
    let mut witness = ComplexityWitness {
        algorithm: "cdlp_synchronous_minimum_vid".to_owned(),
        complexity_claim: "O(|V| + |E| + k * (|V| + |E|))".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    let word = std::mem::size_of::<usize>();
    if n == 0 || max_iter == 0 {
        let workspace = labels
            .capacity()
            .checked_mul(word)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        return Ok((
            KernelOutput {
                values: KernelValues::Components(labels),
                row_count: n,
                witness,
            },
            workspace,
        ));
    }
    let mut next = reserve(n)?;
    let mut counts: Vec<usize> = reserve(n)?;
    let mut generations: Vec<usize> = reserve(n)?;
    for values in [&mut next, &mut counts, &mut generations] {
        while values.len() < n {
            let len = (n - values.len()).min(FNX_METER_RUN);
            checkpoint
                .units(len)
                .map_err(FnxExecutionError::Cancelled)?;
            values.resize(values.len() + len, 0);
        }
    }
    let workspace = [
        labels.capacity(),
        next.capacity(),
        counts.capacity(),
        generations.capacity(),
    ]
    .into_iter()
    .try_fold(0usize, |bytes, capacity| {
        bytes.checked_add(capacity.checked_mul(word)?)
    })
    .ok_or(FnxExecutionError::SizeOverflow)?;
    let mut generation = 0usize;
    for _ in 0..max_iter {
        for source in 0..n {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            generation = generation
                .checked_add(1)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            let mut best = labels[source];
            let mut best_count = 0;
            let outgoing = graph
                .neighbors_indices(source)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            // In an undirected projection every frequency would be doubled
            // by scanning incoming too; that cannot change the winning label.
            let incoming = if graph.is_directed() {
                graph
                    .in_neighbors_indices(source)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?
            } else {
                &[]
            };
            for row in [outgoing, incoming] {
                for batch in row.chunks(FNX_METER_RUN) {
                    checkpoint
                        .units(batch.len())
                        .map_err(FnxExecutionError::Cancelled)?;
                    for &neighbor in batch {
                        let label = labels[neighbor];
                        if generations[label] != generation {
                            generations[label] = generation;
                            counts[label] = 0;
                        }
                        let count = counts[label]
                            .checked_add(1)
                            .ok_or(FnxExecutionError::SizeOverflow)?;
                        counts[label] = count;
                        if count > best_count || (count == best_count && label < best) {
                            best = label;
                            best_count = count;
                        }
                    }
                    witness.edges_scanned = witness
                        .edges_scanned
                        .checked_add(batch.len())
                        .ok_or(FnxExecutionError::SizeOverflow)?;
                }
            }
            next[source] = best;
            increment(&mut witness.nodes_touched)?;
        }
        std::mem::swap(&mut labels, &mut next);
    }
    Ok((
        KernelOutput {
            values: KernelValues::Components(labels),
            row_count: n,
            witness,
        },
        workspace,
    ))
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    algorithm: FnxAlgorithm,
    row_limit: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<KernelOutput, FnxExecutionError<C>> {
    match algorithm {
        FnxAlgorithm::SingleSourceShortestPathLength { source, cutoff } => {
            let source = graph
                .vertex_ordinal(source)
                .ok_or(FnxExecutionError::UnknownSource(source))?;
            bfs(graph, source, cutoff, row_limit, checkpoint)
        }
        FnxAlgorithm::ConnectedComponents => components(graph, false, checkpoint),
        FnxAlgorithm::WeaklyConnectedComponents => components(graph, true, checkpoint),
        FnxAlgorithm::StronglyConnectedComponents => {
            strongly_connected(graph, &mut || checkpoint.unit())
        }
        FnxAlgorithm::PageRank(_)
        | FnxAlgorithm::SingleSourceDijkstraPathLength(_)
        | FnxAlgorithm::Triangles
        | FnxAlgorithm::ClusteringCoefficient
        | FnxAlgorithm::Cdlp { .. }
        | FnxAlgorithm::DirectedClusteringCoefficient
        | FnxAlgorithm::Foundation(_) => Err(FnxExecutionError::InvalidUpstreamResult),
    }
}

fn bfs<C>(
    graph: &SnapshotGraphView,
    source: usize,
    cutoff: Option<usize>,
    row_limit: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<KernelOutput, FnxExecutionError<C>> {
    admit("result rows", 1, row_limit)?;
    let n = graph.node_count();
    let mut distances = reserve(n)?;
    let mut queue = reserve(n)?;
    while distances.len() < n {
        let run = FNX_METER_RUN.min(n - distances.len());
        if run == 1 {
            checkpoint.unit()
        } else {
            checkpoint.units(run)
        }
        .map_err(FnxExecutionError::Cancelled)?;
        distances.resize(distances.len() + run, None);
    }
    distances[source] = Some(0usize);
    queue.push(source);
    let mut head = 0;
    let mut witness = witness("single_source_shortest_path_length_bfs");
    witness.queue_peak = 1;
    while head < queue.len() {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let vertex = queue[head];
        head += 1;
        increment(&mut witness.nodes_touched)?;
        let depth = distances[vertex].ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        if cutoff.is_some_and(|cutoff| depth >= cutoff) {
            continue;
        }
        let next_depth = depth
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let mut neighbors = graph
            .neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        while !neighbors.is_empty() {
            // At most one result is discovered per arc. Precharge only a run
            // that cannot hit the row limit: otherwise a later work refusal
            // could incorrectly overtake the first overflowing discovery.
            // When all n rows fit, every arc is safe even after all vertices
            // have been reached; a tighter exhausted allowance stays scalar.
            let allowed = if row_limit >= n {
                FNX_METER_RUN
            } else {
                row_limit
                    .saturating_sub(queue.len())
                    .clamp(1, FNX_METER_RUN)
            };
            let (run, remaining) = neighbors.split_at(allowed.min(neighbors.len()));
            if run.len() == 1 {
                checkpoint.unit()
            } else {
                checkpoint.units(run.len())
            }
            .map_err(FnxExecutionError::Cancelled)?;
            for &neighbor in run {
                increment(&mut witness.edges_scanned)?;
                if distances[neighbor].is_none() {
                    let requested = queue
                        .len()
                        .checked_add(1)
                        .ok_or(FnxExecutionError::SizeOverflow)?;
                    admit("result rows", requested, row_limit)?;
                    distances[neighbor] = Some(next_depth);
                    queue.push(neighbor);
                    witness.queue_peak = witness.queue_peak.max(queue.len() - head);
                }
            }
            neighbors = remaining;
        }
    }
    Ok(KernelOutput {
        values: KernelValues::Distances(distances),
        row_count: queue.len(),
        witness,
    })
}

fn components<C>(
    graph: &SnapshotGraphView,
    weak: bool,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<KernelOutput, FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut labels = reserve(n)?;
    let mut queue = reserve(n)?;
    while labels.len() < n {
        let run = FNX_METER_RUN.min(n - labels.len());
        if run == 1 {
            checkpoint.unit()
        } else {
            checkpoint.units(run)
        }
        .map_err(FnxExecutionError::Cancelled)?;
        labels.resize(labels.len() + run, usize::MAX);
    }
    let mut witness = witness(if weak {
        "weakly_connected_components_bfs"
    } else {
        "connected_components_bfs"
    });
    // Roots are visited in VId order, so the first unseen member is already
    // the minimum VId in its component. No post-hoc hash-map relabel is needed.
    for root in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        if labels[root] != usize::MAX {
            continue;
        }
        queue.clear();
        labels[root] = root;
        queue.push(root);
        witness.queue_peak = witness.queue_peak.max(1);
        let mut head = 0;
        while head < queue.len() {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            let vertex = queue[head];
            head += 1;
            increment(&mut witness.nodes_touched)?;
            for face in 0..(if weak { 2 } else { 1 }) {
                let neighbors = (if face == 0 {
                    graph.neighbors_indices(vertex)
                } else {
                    graph.in_neighbors_indices(vertex)
                })
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                // Component output rows were admitted before execution, so no
                // row-limit refusal can interrupt these arcs. Keep their order
                // and exact work count, with at most 64 arcs between controls.
                for run in neighbors.chunks(FNX_METER_RUN) {
                    if run.len() == 1 {
                        checkpoint.unit()
                    } else {
                        checkpoint.units(run.len())
                    }
                    .map_err(FnxExecutionError::Cancelled)?;
                    for &neighbor in run {
                        increment(&mut witness.edges_scanned)?;
                        if labels[neighbor] == usize::MAX {
                            labels[neighbor] = root;
                            queue.push(neighbor);
                            witness.queue_peak = witness.queue_peak.max(queue.len() - head);
                        }
                    }
                }
            }
        }
    }
    Ok(KernelOutput {
        values: KernelValues::Components(labels),
        row_count: n,
        witness,
    })
}

/// Iterative Kosaraju, rather than recursive Tarjan. The first-pass stack is
/// reused as the second-pass component queue and member list. Each vector is
/// reserved once for at most n entries; incoming rows are already in the cache.
fn strongly_connected<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<KernelOutput, FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut seen = reserve(n)?;
    let mut labels = reserve(n)?;
    let mut order = reserve(n)?;
    let mut stack = reserve(n)?;
    for _ in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        seen.push(false);
        labels.push(usize::MAX);
    }
    let mut witness = witness("strongly_connected_components_iterative_kosaraju");
    for root in 0..n {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        if seen[root] {
            continue;
        }
        seen[root] = true;
        stack.push((root, 0usize));
        witness.queue_peak = witness.queue_peak.max(stack.len());
        while let Some(&(vertex, offset)) = stack.last() {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            let row = graph
                .neighbors_indices(vertex)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            if let Some(&neighbor) = row.get(offset) {
                increment(&mut witness.edges_scanned)?;
                let last = stack.len() - 1;
                stack[last].1 += 1;
                if !seen[neighbor] {
                    seen[neighbor] = true;
                    stack.push((neighbor, 0));
                    witness.queue_peak = witness.queue_peak.max(stack.len());
                }
            } else {
                stack.pop();
                order.push(vertex);
                increment(&mut witness.nodes_touched)?;
            }
        }
    }
    for &root in order.iter().rev() {
        checkpoint().map_err(FnxExecutionError::Cancelled)?;
        if labels[root] != usize::MAX {
            continue;
        }
        stack.clear();
        stack.push((root, 0));
        labels[root] = root;
        let mut minimum = root;
        let mut head = 0;
        witness.queue_peak = witness.queue_peak.max(1);
        while head < stack.len() {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            let vertex = stack[head].0;
            head += 1;
            increment(&mut witness.nodes_touched)?;
            let row = graph
                .in_neighbors_indices(vertex)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            for &neighbor in row {
                checkpoint().map_err(FnxExecutionError::Cancelled)?;
                increment(&mut witness.edges_scanned)?;
                if labels[neighbor] == usize::MAX {
                    labels[neighbor] = root;
                    minimum = minimum.min(neighbor);
                    stack.push((neighbor, 0));
                    witness.queue_peak = witness.queue_peak.max(stack.len() - head);
                }
            }
        }
        for &(vertex, _) in &stack {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            labels[vertex] = minimum;
        }
    }
    Ok(KernelOutput {
        values: KernelValues::Components(labels),
        row_count: n,
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

    fn graph(n: usize, edges: &[(usize, usize)], directedness: Directedness) -> SnapshotGraphView {
        let vertices: Vec<_> = (0..n)
            .map(|index| VId((index as u128) << 64 | 17))
            .collect();
        let edges: Vec<_> = edges
            .iter()
            .enumerate()
            .map(|(index, &(source, target))| ProjectionEdge {
                eid: EId((index as u128) << 64 | 29),
                source: vertices[source],
                target: vertices[target],
                // The registered BFS ignores weights, including negatives.
                weight: -1.0,
            })
            .collect();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([23; 32]),
                as_of: CommitSeq(7),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness,
                parallel_edges: ParallelEdgePolicy::Sum,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: n,
                max_input_edges: edges.len(),
                max_adjacency_entries: 2 * edges.len(),
                max_workspace_bytes: 1 << 29,
            },
        )
        .unwrap()
    }

    /// Exact scalar predecessor, retained only as a differential and paired
    /// measurement control. Production never selects this implementation.
    fn incumbent<C>(
        graph: &SnapshotGraphView,
        source: usize,
        cutoff: Option<usize>,
        row_limit: usize,
        checkpoint: &mut impl FnMut() -> Result<(), C>,
    ) -> Result<KernelOutput, FnxExecutionError<C>> {
        admit("result rows", 1, row_limit)?;
        let n = graph.node_count();
        let mut distances = reserve(n)?;
        let mut queue = reserve(n)?;
        for _ in 0..n {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            distances.push(None);
        }
        distances[source] = Some(0usize);
        queue.push(source);
        let mut head = 0;
        let mut witness = witness("single_source_shortest_path_length_bfs");
        witness.queue_peak = 1;
        while head < queue.len() {
            checkpoint().map_err(FnxExecutionError::Cancelled)?;
            let vertex = queue[head];
            head += 1;
            increment(&mut witness.nodes_touched)?;
            let depth = distances[vertex].ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            if cutoff.is_some_and(|cutoff| depth >= cutoff) {
                continue;
            }
            let next_depth = depth
                .checked_add(1)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            let neighbors = graph
                .neighbors_indices(vertex)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            for &neighbor in neighbors {
                checkpoint().map_err(FnxExecutionError::Cancelled)?;
                increment(&mut witness.edges_scanned)?;
                if distances[neighbor].is_none() {
                    let requested = queue
                        .len()
                        .checked_add(1)
                        .ok_or(FnxExecutionError::SizeOverflow)?;
                    admit("result rows", requested, row_limit)?;
                    distances[neighbor] = Some(next_depth);
                    queue.push(neighbor);
                    witness.queue_peak = witness.queue_peak.max(queue.len() - head);
                }
            }
        }
        Ok(KernelOutput {
            values: KernelValues::Distances(distances),
            row_count: queue.len(),
            witness,
        })
    }

    #[derive(Default)]
    struct Meter {
        units: usize,
        calls: usize,
        full_runs: usize,
        stop: Option<usize>,
        stopped: bool,
    }

    impl FnxMeter<usize> for Meter {
        fn poll(&mut self) -> Result<(), usize> {
            Ok(())
        }

        fn unit(&mut self) -> Result<(), usize> {
            self.units(1)
        }

        fn units(&mut self, count: usize) -> Result<(), usize> {
            assert!(!self.stopped, "no call after refusal");
            assert!((1..=FNX_METER_RUN).contains(&count));
            self.calls += 1;
            self.full_runs += usize::from(count == FNX_METER_RUN);
            if let Some(stop) = self.stop
                && self.units + count >= stop
            {
                self.units = stop;
                self.stopped = true;
                return Err(stop);
            }
            self.units += count;
            Ok(())
        }
    }

    fn same(
        actual: Result<KernelOutput, FnxExecutionError<usize>>,
        expected: Result<KernelOutput, FnxExecutionError<usize>>,
    ) {
        match (actual, expected) {
            (Ok(actual), Ok(expected)) => {
                let (KernelValues::Distances(actual_rows), KernelValues::Distances(expected_rows)) =
                    (actual.values, expected.values)
                else {
                    panic!("BFS produces distances");
                };
                assert_eq!(actual_rows, expected_rows);
                assert_eq!(actual.row_count, expected.row_count);
                assert_eq!(actual.witness, expected.witness);
            }
            (
                Err(FnxExecutionError::Cancelled(actual)),
                Err(FnxExecutionError::Cancelled(expected)),
            ) => {
                assert_eq!(actual, expected);
            }
            (
                Err(FnxExecutionError::LimitExceeded {
                    resource: a,
                    limit: b,
                    requested: c,
                }),
                Err(FnxExecutionError::LimitExceeded {
                    resource: x,
                    limit: y,
                    requested: z,
                }),
            ) => assert_eq!((a, b, c), (x, y, z)),
            (Err(actual), Err(expected)) => panic!("unexpected refusals: {actual:?}, {expected:?}"),
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => panic!("different outcome: {error:?}"),
        }
    }

    #[test]
    fn bfs_runs_preserve_scalar_refusal_order_at_every_budget_and_row_boundary() {
        let n = 2 * FNX_METER_RUN + 7;
        let mut edges = vec![(0, 0), (0, 1), (0, 1)];
        for target in 1..n - 1 {
            edges.push((0, target));
            edges.push((target, (target + 1) % (n - 1)));
        }
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let view = graph(n, &edges, direction);
            for source in [0, n - 1] {
                for cutoff in [None, Some(0), Some(1)] {
                    for row_limit in [
                        0,
                        1,
                        FNX_METER_RUN - 1,
                        FNX_METER_RUN,
                        FNX_METER_RUN + 1,
                        n - 1,
                        n,
                    ] {
                        let mut scalar = Meter::default();
                        let expected =
                            incumbent(&view, source, cutoff, row_limit, &mut || scalar.unit());
                        let mut batched = Meter::default();
                        let actual = bfs(&view, source, cutoff, row_limit, &mut batched);
                        same(actual, expected);
                        assert_eq!(batched.units, scalar.units);
                        if row_limit != 0 {
                            assert!(batched.calls < scalar.calls);
                            assert!(batched.full_runs >= n / FNX_METER_RUN);
                        }
                        if source == 0
                            && cutoff != Some(0)
                            && row_limit == n
                            && direction != Directedness::Reversed
                        {
                            assert!(
                                batched.full_runs > n / FNX_METER_RUN,
                                "the hub's arcs, not only initialization, use full runs"
                            );
                        }
                        // Includes the first unit *after* a row-limit refusal:
                        // that cancellation must never hide the earlier error.
                        for stop in 1..=scalar.units + 1 {
                            let mut scalar = Meter {
                                stop: Some(stop),
                                ..Meter::default()
                            };
                            let expected =
                                incumbent(&view, source, cutoff, row_limit, &mut || scalar.unit());
                            let mut batched = Meter {
                                stop: Some(stop),
                                ..Meter::default()
                            };
                            let actual = bfs(&view, source, cutoff, row_limit, &mut batched);
                            same(actual, expected);
                            assert_eq!(batched.units, scalar.units);
                            assert_eq!(batched.stopped, scalar.stopped);
                        }
                    }
                }
            }
        }
    }

    /// Includes both BFS allocations, the entire traversal, result consumption
    /// and cleanup. Projection, admission, database I/O and certificate costs
    /// are outside this decoded-kernel measurement. Both implementations run
    /// in the same process, alternating order, with all raw samples reported.
    #[test]
    #[ignore = "paired scalar/batched BFS measurement; run optimized with --nocapture"]
    fn bfs_paired_incumbent_measurement() {
        use std::hint::black_box;
        use std::time::Instant;

        const REPETITIONS: usize = 16;

        #[derive(Default)]
        struct CountingMeter {
            units: usize,
        }

        impl FnxMeter<usize> for CountingMeter {
            fn poll(&mut self) -> Result<(), usize> {
                Ok(())
            }

            fn unit(&mut self) -> Result<(), usize> {
                self.units(1)
            }

            fn units(&mut self, count: usize) -> Result<(), usize> {
                self.units = black_box(self.units + count);
                Ok(())
            }
        }

        fn fixture(shape: &str) -> SnapshotGraphView {
            let (n, edges) = match shape {
                "isolates" => (32_768, Vec::new()),
                "road" => (32_768, (1..32_768).map(|node| (node - 1, node)).collect()),
                "star" => (32_768, (1..32_768).map(|node| (0, node)).collect()),
                "lattice" => {
                    let width = 128;
                    let mut edges = Vec::new();
                    for row in 0..width {
                        for column in 0..width {
                            let node = row * width + column;
                            if column + 1 < width {
                                edges.push((node, node + 1));
                            }
                            if row + 1 < width {
                                edges.push((node, node + width));
                            }
                        }
                    }
                    (width * width, edges)
                }
                "clustered" => {
                    let width = 64;
                    let clusters = 128;
                    let mut edges = Vec::new();
                    for cluster in 0..clusters {
                        let start = cluster * width;
                        for source in start..start + width {
                            for target in source + 1..start + width {
                                edges.push((source, target));
                            }
                        }
                        if cluster != 0 {
                            edges.push((start - 1, start));
                        }
                    }
                    (width * clusters, edges)
                }
                _ => panic!("unknown fixed measurement shape"),
            };
            graph(n, &edges, Directedness::Undirected)
        }

        fn measure(
            view: &SnapshotGraphView,
            current: bool,
            counted: bool,
        ) -> (u128, usize, u64, KernelOutput) {
            let mut meter = CountingMeter::default();
            let mut checksum = 0u64;
            let mut last = None;
            let start = Instant::now();
            for repetition in 0..REPETITIONS {
                let output = if counted {
                    if current {
                        bfs(black_box(view), 0, None, view.node_count(), &mut meter)
                    } else {
                        incumbent(black_box(view), 0, None, view.node_count(), &mut || {
                            meter.unit()
                        })
                    }
                } else {
                    let mut control = || Ok::<(), usize>(());
                    if current {
                        bfs(black_box(view), 0, None, view.node_count(), &mut control)
                    } else {
                        incumbent(black_box(view), 0, None, view.node_count(), &mut control)
                    }
                }
                .unwrap();
                let KernelValues::Distances(distances) = &output.values else {
                    panic!("BFS produces distances");
                };
                for &distance in distances {
                    checksum = checksum
                        .wrapping_mul(31)
                        .wrapping_add(distance.unwrap_or(usize::MAX) as u64);
                }
                black_box(checksum);
                if repetition + 1 == REPETITIONS {
                    last = Some(output);
                }
            }
            (
                start.elapsed().as_nanos(),
                meter.units,
                checksum,
                last.unwrap(),
            )
        }

        println!(
            "{{\"scope\":\"decoded_bfs_kernel_including_scratch_and_result_consumption\",\"empirical_gate_activated\":false,\"samples_per_kernel\":9,\"repetitions_per_sample\":{REPETITIONS},\"nanoseconds\":\"total_per_sample\"}}"
        );
        for shape in ["isolates", "road", "star", "lattice", "clustered"] {
            let view = fixture(shape);
            for counted in [false, true] {
                let prior = measure(&view, false, counted);
                let current = measure(&view, true, counted);
                assert_eq!((prior.1, prior.2), (current.1, current.2));
                same(Ok(current.3), Ok(prior.3));
                let (mut prior_ns, mut current_ns) = (Vec::new(), Vec::new());
                let mut units = 0;
                for sample in 0..9 {
                    let (prior, current) = if sample % 2 == 0 {
                        (
                            measure(&view, false, counted),
                            measure(&view, true, counted),
                        )
                    } else {
                        let current = measure(&view, true, counted);
                        (measure(&view, false, counted), current)
                    };
                    assert_eq!((prior.1, prior.2), (current.1, current.2));
                    same(Ok(current.3), Ok(prior.3));
                    units = current.1;
                    prior_ns.push(prior.0);
                    current_ns.push(current.0);
                }
                let (mut prior_sorted, mut current_sorted) = (prior_ns.clone(), current_ns.clone());
                prior_sorted.sort_unstable();
                current_sorted.sort_unstable();
                let ratio = prior_sorted[4] as f64 / current_sorted[4] as f64;
                println!(
                    "{{\"shape\":\"{shape}\",\"counting_meter\":{counted},\"vertices\":{},\"edges\":{},\"incumbent_ns\":{prior_ns:?},\"current_ns\":{current_ns:?},\"incumbent_median_ns\":{},\"current_median_ns\":{},\"ratio_prior_over_current\":{ratio:.4},\"units_per_sample\":{units}}}",
                    view.node_count(),
                    view.edge_count(),
                    prior_sorted[4],
                    current_sorted[4]
                );
            }
        }
    }
}
