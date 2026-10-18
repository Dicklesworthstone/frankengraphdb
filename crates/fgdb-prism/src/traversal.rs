//! Exact, checkpointed traversal kernels. They borrow decoded adjacency, never
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
        checkpoint
            .units(run)
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
        let neighbors = graph
            .neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        let mut remaining = neighbors;
        while !remaining.is_empty() {
            // Each arc can discover at most one vertex. A run no longer than
            // the remaining row allowance cannot encounter a row refusal, so
            // charging it before the walk preserves the scalar refusal point.
            // At a full allowance, examine one arc at a time: it may revisit a
            // known vertex, or be the exact first disallowed discovery.
            let allowance = if row_limit >= n {
                FNX_METER_RUN
            } else {
                row_limit.saturating_sub(queue.len()).max(1)
            };
            let run = FNX_METER_RUN.min(allowance).min(remaining.len());
            checkpoint
                .units(run)
                .map_err(FnxExecutionError::Cancelled)?;
            for &neighbor in &remaining[..run] {
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
            remaining = &remaining[run..];
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
