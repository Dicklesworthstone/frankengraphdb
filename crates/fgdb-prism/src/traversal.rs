//! Exact, checkpointed traversal and label kernels. They borrow decoded adjacency, never
//! construct a second Graph/DiGraph, recurse on the process stack, or consult
//! weights that their registered semantics ignore.

use crate::execute::{ComplexityWitness, KernelOutput, KernelValues, admit, reserve};
use crate::{
    FNX_METER_RUN, FnxAlgorithm, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView,
};

/// One completed pull level under [`BfsDecisionCard`]'s deterministic policy.
/// Bounds and observations count graph visits, not CPU instructions or time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BfsPullDecision {
    pub depth: usize,
    pub frontier_vertices: usize,
    pub frontier_arcs: usize,
    pub unseen_incoming_arcs: usize,
    pub vertices: usize,
    /// Guaranteed visits avoided compared with scanning this frontier's arcs.
    pub benefit_lower: usize,
    /// Maximum visits avoided: even an empty pull scans all vertex candidates.
    pub benefit_upper: usize,
    pub candidate_visits: usize,
    pub incoming_visits: usize,
}

/// Replayable, answer-preserving physical BFS decisions. Version 1 selects
/// pull only when `frontier_arcs / 2 > vertices + unseen_incoming_arcs` and all
/// projected vertices fit the result allowance. Every other level uses the
/// pinned top-down fallback. Only actual pull levels need records: the source
/// digest, projection, source/cutoff, and this formula reconstruct the omitted
/// push levels. There is no clock, randomness, learned estimate, or mutable
/// policy state. A choice lasts exactly one level; conversion needs no bitmap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BfsDecisionCard {
    pub policy_version: u16,
    pub fallback: &'static str,
    pub dwell_levels: usize,
    pub pull_cost_multiplier: usize,
    pub pulls: Vec<BfsPullDecision>,
}

pub(crate) struct TraversalOutput {
    pub output: KernelOutput,
    pub bfs_decision_card: Option<BfsDecisionCard>,
    pub workspace_bytes: Option<usize>,
}

impl TraversalOutput {
    fn without_bfs(output: KernelOutput) -> Self {
        Self {
            output,
            bfs_decision_card: None,
            workspace_bytes: None,
        }
    }
}

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
) -> Result<TraversalOutput, FnxExecutionError<C>> {
    match algorithm {
        FnxAlgorithm::SingleSourceShortestPathLength { source, cutoff } => {
            let source = graph
                .vertex_ordinal(source)
                .ok_or(FnxExecutionError::UnknownSource(source))?;
            bounded_bfs(graph, source, cutoff, row_limit, checkpoint)
        }
        FnxAlgorithm::ConnectedComponents => {
            components(graph, false, checkpoint).map(TraversalOutput::without_bfs)
        }
        FnxAlgorithm::WeaklyConnectedComponents => {
            components(graph, true, checkpoint).map(TraversalOutput::without_bfs)
        }
        FnxAlgorithm::StronglyConnectedComponents => {
            strongly_connected(graph, checkpoint).map(TraversalOutput::without_bfs)
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

/// A pull is paid for by outgoing arcs that this frontier will never scan:
/// its entire candidate pass and incoming probes cost at most n + unseen_in.
/// Frontiers are disjoint, so replacing a push only below that bound preserves
/// the existing n + m traversal-work admission. The factor-two margin avoids
/// switching for a merely equal bound and pays for degree bookkeeping. This
/// is a visit-count guarantee; locality and control costs still need timing.
fn bounded_bfs<C>(
    graph: &SnapshotGraphView,
    source: usize,
    cutoff: Option<usize>,
    row_limit: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<TraversalOutput, FnxExecutionError<C>> {
    let n = graph.node_count();
    let out_offsets = graph.outgoing_offsets();
    let arcs = *out_offsets
        .last()
        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
    // A pull cannot satisfy the bound in this sparse case. A tight row
    // allowance must also retain the predecessor's discovery/refusal order:
    // batch charging may not hide a row refusal behind a later work refusal.
    if row_limit < n || arcs / 2 <= n {
        let output = bfs(graph, source, cutoff, row_limit, checkpoint)?;
        return Ok(TraversalOutput {
            output,
            bfs_decision_card: None,
            workspace_bytes: None,
        });
    }
    admit("result rows", 1, row_limit)?;
    let (in_offsets, incoming) = graph.incoming_face();
    let mut unseen_in = incoming
        .len()
        .checked_sub(in_offsets[source + 1] - in_offsets[source])
        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
    let mut distances = reserve(n)?;
    let mut queue = reserve(n)?;
    while distances.len() < n {
        let count = FNX_METER_RUN.min(n - distances.len());
        checkpoint
            .units(count)
            .map_err(FnxExecutionError::Cancelled)?;
        distances.resize(distances.len() + count, None);
    }
    distances[source] = Some(0usize);
    queue.push(source);
    let mut pulls = Vec::new();
    let mut head = 0;
    let mut witness = witness("single_source_shortest_path_length_bfs");
    witness.queue_peak = 1;
    while head < queue.len() {
        let frontier_end = queue.len();
        let frontier_vertices = frontier_end - head;
        let depth = distances[queue[head]].ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        let mut frontier_arcs = 0usize;
        // Charge these vertices once, before observing their degrees. Result
        // rows cannot refuse in this kernel, so precharging the frontier does
        // not overtake an earlier per-discovery error. No second degree array
        // or per-arc direction test is needed.
        for run in queue[head..frontier_end].chunks(FNX_METER_RUN) {
            checkpoint
                .units(run.len())
                .map_err(FnxExecutionError::Cancelled)?;
            for &vertex in run {
                frontier_arcs = frontier_arcs
                    .checked_add(out_offsets[vertex + 1] - out_offsets[vertex])
                    .ok_or(FnxExecutionError::SizeOverflow)?;
            }
            witness.nodes_touched = witness
                .nodes_touched
                .checked_add(run.len())
                .ok_or(FnxExecutionError::SizeOverflow)?;
        }
        if cutoff.is_some_and(|cutoff| depth >= cutoff) {
            break;
        }
        let next_depth = depth
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let pull_bound = n
            .checked_add(unseen_in)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        if frontier_arcs / 2 > pull_bound {
            // Record only completed choices; any refusal drops all scratch
            // and evidence with the result. Capacity is included in the peak
            // workspace returned below, alongside both traversal vectors.
            pulls
                .try_reserve(1)
                .map_err(|_| FnxExecutionError::AllocationFailed)?;
            let mut decision = BfsPullDecision {
                depth,
                frontier_vertices,
                frontier_arcs,
                unseen_incoming_arcs: unseen_in,
                vertices: n,
                benefit_lower: frontier_arcs - pull_bound,
                benefit_upper: frontier_arcs - n,
                candidate_visits: 0,
                incoming_visits: 0,
            };
            head = frontier_end;
            for vertex in 0..n {
                checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
                increment(&mut witness.nodes_touched)?;
                increment(&mut decision.candidate_visits)?;
                if distances[vertex].is_some() {
                    continue;
                }
                let row = &incoming[in_offsets[vertex]..in_offsets[vertex + 1]];
                for run in row.chunks(FNX_METER_RUN) {
                    checkpoint
                        .units(run.len())
                        .map_err(FnxExecutionError::Cancelled)?;
                    let mut reached = false;
                    // Examine the complete charged run, even after a match.
                    // Short-circuiting here would claim unvisited arcs. Stop
                    // between runs, with at most 63 extra probes after a hit.
                    for &predecessor in run {
                        reached |= distances[predecessor] == Some(depth);
                    }
                    witness.edges_scanned = witness
                        .edges_scanned
                        .checked_add(run.len())
                        .ok_or(FnxExecutionError::SizeOverflow)?;
                    decision.incoming_visits = decision
                        .incoming_visits
                        .checked_add(run.len())
                        .ok_or(FnxExecutionError::SizeOverflow)?;
                    if reached {
                        distances[vertex] = Some(next_depth);
                        queue.push(vertex);
                        unseen_in = unseen_in
                            .checked_sub(in_offsets[vertex + 1] - in_offsets[vertex])
                            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                        witness.queue_peak = witness.queue_peak.max(queue.len() - head);
                        break;
                    }
                }
            }
            pulls.push(decision);
        } else {
            while head < frontier_end {
                let vertex = queue[head];
                head += 1;
                let neighbors = graph
                    .neighbors_indices(vertex)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                for run in neighbors.chunks(FNX_METER_RUN) {
                    checkpoint
                        .units(run.len())
                        .map_err(FnxExecutionError::Cancelled)?;
                    for &neighbor in run {
                        increment(&mut witness.edges_scanned)?;
                        if distances[neighbor].is_none() {
                            distances[neighbor] = Some(next_depth);
                            queue.push(neighbor);
                            unseen_in = unseen_in
                                .checked_sub(in_offsets[neighbor + 1] - in_offsets[neighbor])
                                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                            witness.queue_peak = witness.queue_peak.max(queue.len() - head);
                        }
                    }
                }
            }
        }
    }
    let workspace_bytes = distances
        .capacity()
        .checked_mul(std::mem::size_of::<Option<usize>>())
        .and_then(|bytes| {
            queue
                .capacity()
                .checked_mul(std::mem::size_of::<usize>())
                .and_then(|queue| bytes.checked_add(queue))
        })
        .and_then(|bytes| {
            pulls
                .capacity()
                .checked_mul(std::mem::size_of::<BfsPullDecision>())
                .and_then(|decisions| bytes.checked_add(decisions))
        })
        .ok_or(FnxExecutionError::SizeOverflow)?;
    let bfs_decision_card = if pulls.is_empty() {
        None
    } else {
        witness.algorithm = "single_source_shortest_path_length_bounded_pull_v1".to_owned();
        Some(BfsDecisionCard {
            policy_version: 1,
            fallback: "top_down",
            dwell_levels: 1,
            pull_cost_multiplier: 2,
            pulls,
        })
    };
    Ok(TraversalOutput {
        output: KernelOutput {
            values: KernelValues::Distances(distances),
            row_count: queue.len(),
            witness,
        },
        bfs_decision_card,
        workspace_bytes: Some(workspace_bytes),
    })
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
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<KernelOutput, FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut seen = reserve(n)?;
    let mut labels = reserve(n)?;
    let mut order = reserve(n)?;
    let mut stack = reserve(n)?;
    while seen.len() < n {
        let count = (n - seen.len()).min(FNX_METER_RUN);
        if count == 1 {
            checkpoint.unit()
        } else {
            checkpoint.units(count)
        }
        .map_err(FnxExecutionError::Cancelled)?;
        seen.resize(seen.len() + count, false);
        labels.resize(labels.len() + count, usize::MAX);
    }
    let mut witness = witness("strongly_connected_components_iterative_kosaraju");
    let mut next_root = 0;
    while next_root < n {
        // Already-seen roots have no intervening work. Include at most the
        // first unseen root, since its DFS must finish before the next charge.
        let proposed = (n - next_root).min(FNX_METER_RUN);
        let count = seen[next_root..next_root + proposed]
            .iter()
            .position(|&visited| !visited)
            .map_or(proposed, |offset| offset + 1);
        if count == 1 {
            checkpoint.unit()
        } else {
            checkpoint.units(count)
        }
        .map_err(FnxExecutionError::Cancelled)?;
        next_root += count;
        let root = next_root - 1;
        if seen[root] {
            continue;
        }
        seen[root] = true;
        stack.push((root, 0usize));
        witness.queue_peak = witness.queue_peak.max(stack.len());
        while let Some(&(vertex, offset)) = stack.last() {
            let row = graph.neighbors_indices(vertex);
            // Look ahead through at most 64 already-seen neighbors, stopping
            // at the first DFS descent. Those edges cannot change `seen`, so
            // this is the exact original DFS and finish order. A row failure
            // or witness overflow still follows its original one-unit charge.
            let proposed = row.map_or(1, |row| {
                let remaining = row.len() - offset;
                // Sparse rows and DFS completion need no lookahead. Retain
                // the one-unit specialization instead of scanning a singleton
                // once here and once again after its checkpoint.
                if remaining <= 1 {
                    return 1;
                }
                let count = remaining.min(FNX_METER_RUN);
                row[offset..offset + count]
                    .iter()
                    .position(|&neighbor| !seen[neighbor])
                    .map_or(count, |offset| offset + 1)
            });
            let count = if witness.edges_scanned.checked_add(proposed).is_some() {
                proposed
            } else {
                1
            };
            if count == 1 {
                checkpoint.unit()
            } else {
                checkpoint.units(count)
            }
            .map_err(FnxExecutionError::Cancelled)?;
            let row = row.ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            if offset < row.len() {
                witness.edges_scanned = witness
                    .edges_scanned
                    .checked_add(count)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                let last = stack.len() - 1;
                stack[last].1 += count;
                let neighbor = row[offset + count - 1];
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
    let mut next_root = n;
    while next_root > 0 {
        // Reverse finish order is unchanged; only consecutive roots already
        // assigned to a component can share the following root's charge run.
        let proposed = next_root.min(FNX_METER_RUN);
        let count = order[next_root - proposed..next_root]
            .iter()
            .rev()
            .position(|&root| labels[root] == usize::MAX)
            .map_or(proposed, |offset| offset + 1);
        if count == 1 {
            checkpoint.unit()
        } else {
            checkpoint.units(count)
        }
        .map_err(FnxExecutionError::Cancelled)?;
        next_root -= count;
        let root = order[next_root];
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
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            let vertex = stack[head].0;
            head += 1;
            increment(&mut witness.nodes_touched)?;
            let mut row = graph
                .in_neighbors_indices(vertex)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            while !row.is_empty() {
                // Every result row was admitted before this kernel. No
                // discovery can refuse inside a run; retain witness overflow
                // precedence by staying scalar if its counter is near MAX.
                let proposed = row.len().min(FNX_METER_RUN);
                let count = if witness.edges_scanned.checked_add(proposed).is_some() {
                    proposed
                } else {
                    1
                };
                if count == 1 {
                    checkpoint.unit()
                } else {
                    checkpoint.units(count)
                }
                .map_err(FnxExecutionError::Cancelled)?;
                witness.edges_scanned = witness
                    .edges_scanned
                    .checked_add(count)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                for &neighbor in &row[..count] {
                    if labels[neighbor] == usize::MAX {
                        labels[neighbor] = root;
                        minimum = minimum.min(neighbor);
                        stack.push((neighbor, 0));
                        witness.queue_peak = witness.queue_peak.max(stack.len() - head);
                    }
                }
                row = &row[count..];
            }
        }
        for members in stack.chunks(FNX_METER_RUN) {
            if members.len() == 1 {
                checkpoint.unit()
            } else {
                checkpoint.units(members.len())
            }
            .map_err(FnxExecutionError::Cancelled)?;
            for &(vertex, _) in members {
                labels[vertex] = minimum;
            }
        }
    }
    Ok(KernelOutput {
        values: KernelValues::Components(labels),
        row_count: n,
        witness,
    })
}

/// Exact scalar predecessor retained for differential and paired controls.
#[cfg(test)]
fn strongly_connected_incumbent<C>(
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

    fn distance_values(output: &KernelOutput) -> &[Option<usize>] {
        let KernelValues::Distances(values) = &output.values else {
            panic!("BFS produces distances");
        };
        values
    }

    /// Independent all-pairs min-plus oracle built from the raw input, not
    /// either projected CSR face or the optimized frontier representation.
    fn distance_oracle(
        n: usize,
        edges: &[(usize, usize)],
        direction: Directedness,
        source: usize,
        cutoff: Option<usize>,
    ) -> Vec<Option<usize>> {
        let mut paths = vec![usize::MAX; n * n];
        for vertex in 0..n {
            paths[vertex * n + vertex] = 0;
        }
        for &(source, target) in edges {
            if source == target {
                continue;
            }
            if direction != Directedness::Reversed {
                paths[source * n + target] = 1;
            }
            if direction != Directedness::Directed {
                paths[target * n + source] = 1;
            }
        }
        for via in 0..n {
            for from in 0..n {
                for to in 0..n {
                    if let Some(distance) = paths[from * n + via].checked_add(paths[via * n + to]) {
                        paths[from * n + to] = paths[from * n + to].min(distance);
                    }
                }
            }
        }
        paths[source * n..(source + 1) * n]
            .iter()
            .map(|&distance| {
                (distance != usize::MAX && cutoff.is_none_or(|cutoff| distance <= cutoff))
                    .then_some(distance)
            })
            .collect()
    }

    fn dense_bfs_fixture(width: usize) -> (usize, Vec<(usize, usize)>) {
        let mut edges = Vec::new();
        for vertex in 1..=width {
            edges.push((0, vertex));
            for target in 1..=width {
                edges.push((vertex, target));
            }
            edges.push((vertex, width + 1));
        }
        // A tail requires another level after the pull; newly found vertices
        // must never act as members of the current frontier. The final two
        // vertices are unreachable, including an isolated self-loop.
        edges.extend([
            (width + 1, width + 2),
            (width + 2, width + 2),
            (width + 4, width + 4),
            (0, 1),
        ]);
        (width + 5, edges)
    }

    fn layered_bfs_fixture(first: usize, second: usize) -> (usize, Vec<(usize, usize)>) {
        let tail = first + second + 1;
        let mut edges = Vec::new();
        for vertex in 1..=first {
            edges.push((0, vertex));
            for target in 1..=first {
                edges.push((vertex, target));
            }
        }
        for vertex in first + 1..tail {
            edges.push((1, vertex));
            for target in first + 1..tail {
                edges.push((vertex, target));
            }
        }
        edges.extend([(first + 1, tail), (tail, tail), (tail + 1, tail + 1)]);
        (tail + 2, edges)
    }

    #[test]
    fn bounded_pull_matches_independent_distances_and_keeps_linear_work() {
        let (n, edges) = dense_bfs_fixture(FNX_METER_RUN + 3);
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let view = graph(n, &edges, direction);
            let source = if direction == Directedness::Reversed {
                n - 3
            } else {
                0
            };
            let arcs = *view.outgoing_offsets().last().unwrap();
            for cutoff in [None, Some(0), Some(1), Some(2), Some(3), Some(usize::MAX)] {
                let expected = distance_oracle(n, &edges, direction, source, cutoff);
                let mut meter = Meter::default();
                let actual = bounded_bfs(&view, source, cutoff, n, &mut meter).unwrap();
                assert_eq!(distance_values(&actual.output), expected);
                assert_eq!(actual.output.row_count, expected.iter().flatten().count());
                assert_eq!(
                    meter.units,
                    n + actual.output.witness.nodes_touched + actual.output.witness.edges_scanned
                );
                assert!(
                    actual.output.witness.nodes_touched + actual.output.witness.edges_scanned
                        <= n + arcs
                );
                assert!(actual.output.witness.queue_peak <= n);
                if cutoff.is_none() {
                    let card = actual.bfs_decision_card.as_ref().expect("dense pull");
                    assert_eq!(card.policy_version, 1);
                    assert_eq!(card.fallback, "top_down");
                    assert_eq!(card.dwell_levels, 1);
                    assert_eq!(card.pull_cost_multiplier, 2);
                    assert!(!card.pulls.is_empty());
                    let mut previous_depth = None;
                    for pull in &card.pulls {
                        assert!(previous_depth.is_none_or(|previous| previous < pull.depth));
                        previous_depth = Some(pull.depth);
                        assert_eq!(pull.vertices, n);
                        assert_eq!(pull.candidate_visits, n);
                        assert!(pull.incoming_visits <= pull.unseen_incoming_arcs);
                        assert!(pull.frontier_arcs / 2 > n + pull.unseen_incoming_arcs);
                        let avoided =
                            pull.frontier_arcs - pull.candidate_visits - pull.incoming_visits;
                        assert!((pull.benefit_lower..=pull.benefit_upper).contains(&avoided));
                    }
                    assert!(actual.output.witness.edges_scanned < arcs / 4);
                    let vector_bytes =
                        n * (std::mem::size_of::<Option<usize>>() + std::mem::size_of::<usize>());
                    assert_eq!(
                        actual.workspace_bytes.unwrap(),
                        vector_bytes
                            + card.pulls.capacity() * std::mem::size_of::<BfsPullDecision>()
                    );
                    assert_eq!(
                        actual.output.witness.algorithm,
                        "single_source_shortest_path_length_bounded_pull_v1"
                    );
                }
            }
            // Full-width VIds still flow through the production CALL output;
            // only temporary traversal positions use usize ordinals.
            let call = crate::FnxCallSpec::single_source_shortest_path_length(
                view.vertex_id(source).unwrap(),
                None,
            );
            let limits = crate::FnxExecutionLimits {
                max_iterations: 0,
                max_result_rows: n,
                max_estimated_work: n + arcs,
            };
            let result = call.execute(&view, limits, || Ok::<(), usize>(())).unwrap();
            assert!(
                result
                    .rows
                    .windows(2)
                    .all(|rows| match (&rows[0][0], &rows[1][0]) {
                        (crate::FnxValue::Vertex(left), crate::FnxValue::Vertex(right)) =>
                            left < right,
                        _ => false,
                    })
            );
            assert!(result.rows.iter().all(|row| match row[0] {
                crate::FnxValue::Vertex(vertex) => vertex.0 & u64::MAX as u128 == 17,
                _ => false,
            }));
            assert_eq!(result.certificate.estimated_work, n + arcs);
            assert!(matches!(
                call.execute(
                    &view,
                    crate::FnxExecutionLimits {
                        max_estimated_work: n + arcs - 1,
                        ..limits
                    },
                    || Ok::<(), usize>(())
                ),
                Err(FnxExecutionError::LimitExceeded {
                    resource: "estimated work",
                    ..
                })
            ));
        }
    }

    #[test]
    fn bounded_pull_preserves_every_cancellation_ordinal() {
        let (n, edges) = dense_bfs_fixture(FNX_METER_RUN + 1);
        for direction in [Directedness::Directed, Directedness::Reversed] {
            let view = graph(n, &edges, direction);
            let source = if direction == Directedness::Reversed {
                n - 3
            } else {
                0
            };
            for cutoff in [None, Some(1), Some(2)] {
                let mut total = Meter::default();
                let complete = bounded_bfs(&view, source, cutoff, n, &mut total).unwrap();
                for stop in 1..=total.units + 1 {
                    let mut scalar = Meter {
                        stop: Some(stop),
                        ..Meter::default()
                    };
                    let expected = bounded_bfs(&view, source, cutoff, n, &mut || scalar.unit());
                    let mut bulk = Meter {
                        stop: Some(stop),
                        ..Meter::default()
                    };
                    let actual = bounded_bfs(&view, source, cutoff, n, &mut bulk);
                    assert_eq!(scalar.units, bulk.units);
                    assert_eq!(scalar.stopped, bulk.stopped);
                    if stop <= total.units {
                        assert!(
                            matches!(expected, Err(FnxExecutionError::Cancelled(at)) if at == stop)
                        );
                        assert!(
                            matches!(actual, Err(FnxExecutionError::Cancelled(at)) if at == stop)
                        );
                    } else {
                        let expected = expected.unwrap();
                        let actual = actual.unwrap();
                        assert_eq!(actual.bfs_decision_card, expected.bfs_decision_card);
                        assert_eq!(actual.bfs_decision_card, complete.bfs_decision_card);
                        same(Ok(actual.output), Ok(expected.output));
                    }
                }
            }
        }
    }

    #[test]
    fn repeated_pull_levels_keep_new_discoveries_out_of_the_current_frontier() {
        // The first clique pays for pulling into the second, whose internal
        // arcs in turn pay for a second pull into the tail. Both decisions
        // must use their own depth and remaining incoming volume. Incoming
        // rows also contain newly discovered vertices from the same level.
        let first = 48;
        let second = 21;
        let tail = first + second + 1;
        let (n, edges) = layered_bfs_fixture(first, second);
        let view = graph(n, &edges, Directedness::Directed);
        let mut meter = Meter::default();
        let actual = bounded_bfs(&view, 0, None, n, &mut meter).unwrap();
        assert_eq!(
            distance_values(&actual.output),
            distance_oracle(n, &edges, Directedness::Directed, 0, None)
        );
        let card = actual.bfs_decision_card.unwrap();
        assert_eq!(card.pulls.len(), 2);
        assert_eq!(card.pulls[0].depth, 1);
        assert_eq!(card.pulls[0].frontier_vertices, first);
        assert_eq!(card.pulls[1].depth, 2);
        assert_eq!(card.pulls[1].frontier_vertices, second);
        assert_eq!(card.pulls[1].unseen_incoming_arcs, 3);
        assert_eq!(distance_values(&actual.output)[tail], Some(3));
        assert_eq!(distance_values(&actual.output)[tail + 1], None);
        assert_eq!(actual.output.witness.nodes_touched, n - 1 + 2 * n);
        assert_eq!(
            meter.units,
            n + actual.output.witness.nodes_touched + actual.output.witness.edges_scanned
        );
        assert!(
            actual.output.witness.nodes_touched + actual.output.witness.edges_scanned
                <= n + view.outgoing_offsets().last().unwrap()
        );
    }

    #[test]
    fn tight_rows_keep_the_top_down_refusal_prefix_on_dense_graphs() {
        let (n, edges) = dense_bfs_fixture(8);
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let view = graph(n, &edges, direction);
            for source in [0, n - 3, n - 2] {
                for cutoff in [None, Some(0), Some(1), Some(2)] {
                    for row_limit in 0..n {
                        let mut scalar = Meter::default();
                        let expected =
                            incumbent(&view, source, cutoff, row_limit, &mut || scalar.unit());
                        let mut actual_meter = Meter::default();
                        let actual =
                            bounded_bfs(&view, source, cutoff, row_limit, &mut actual_meter).map(
                                |result| {
                                    assert!(result.bfs_decision_card.is_none());
                                    result.output
                                },
                            );
                        same(actual, expected);
                        assert_eq!(actual_meter.units, scalar.units);
                        for stop in 1..=scalar.units + 1 {
                            let mut scalar = Meter {
                                stop: Some(stop),
                                ..Meter::default()
                            };
                            let expected =
                                incumbent(&view, source, cutoff, row_limit, &mut || scalar.unit());
                            let mut bulk = Meter {
                                stop: Some(stop),
                                ..Meter::default()
                            };
                            let actual = bounded_bfs(&view, source, cutoff, row_limit, &mut bulk)
                                .map(|result| result.output);
                            same(actual, expected);
                            assert_eq!(bulk.units, scalar.units);
                            assert_eq!(bulk.stopped, scalar.stopped);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn sparse_graphs_retain_the_original_kernel_and_workspace() {
        let n = 2 * FNX_METER_RUN + 7;
        for edges in [
            Vec::new(),
            (1..n).map(|vertex| (vertex - 1, vertex)).collect(),
            (1..n).map(|vertex| (0, vertex)).collect(),
        ] {
            for direction in [Directedness::Directed, Directedness::Undirected] {
                let view = graph(n, &edges, direction);
                let mut old = Meter::default();
                let expected = bfs(&view, 0, None, n, &mut old);
                let mut new = Meter::default();
                let actual = bounded_bfs(&view, 0, None, n, &mut new).unwrap();
                assert!(actual.bfs_decision_card.is_none());
                assert!(actual.workspace_bytes.is_none());
                assert_eq!(new.units, old.units);
                same(Ok(actual.output), expected);
            }
        }
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

    fn same_components(
        actual: Result<KernelOutput, FnxExecutionError<usize>>,
        expected: Result<KernelOutput, FnxExecutionError<usize>>,
    ) {
        match (actual, expected) {
            (Ok(actual), Ok(expected)) => {
                let (
                    KernelValues::Components(actual_rows),
                    KernelValues::Components(expected_rows),
                ) = (actual.values, expected.values)
                else {
                    panic!("SCC produces component labels");
                };
                assert_eq!(actual_rows, expected_rows);
                assert_eq!(actual.row_count, expected.row_count);
                assert_eq!(actual.witness, expected.witness);
            }
            (
                Err(FnxExecutionError::Cancelled(actual)),
                Err(FnxExecutionError::Cancelled(expected)),
            ) => assert_eq!(actual, expected),
            (Err(actual), Err(expected)) => panic!("unexpected refusals: {actual:?}, {expected:?}"),
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => panic!("different outcome: {error:?}"),
        }
    }

    #[test]
    fn scc_runs_match_scalar_and_independent_reachability_on_every_small_digraph() {
        // This oracle uses only the input pairs and transitive closure, not
        // either traversal's CSR, DFS order, or component bookkeeping.
        for n in 0..=4 {
            let pairs: Vec<_> = (0..n)
                .flat_map(|source| {
                    (0..n)
                        .filter(move |&target| source != target)
                        .map(move |target| (source, target))
                })
                .collect();
            for mask in 0usize..1usize << pairs.len() {
                let edges: Vec<_> = pairs
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(_, &pair)| pair)
                    .collect();
                let mut reachable = vec![vec![false; n]; n];
                for (vertex, row) in reachable.iter_mut().enumerate() {
                    row[vertex] = true;
                }
                for &(source, target) in &edges {
                    reachable[source][target] = true;
                }
                for middle in 0..n {
                    for source in 0..n {
                        for target in 0..n {
                            reachable[source][target] |=
                                reachable[source][middle] && reachable[middle][target];
                        }
                    }
                }
                let expected: Vec<_> = (0..n)
                    .map(|vertex| {
                        (0..n)
                            .find(|&other| reachable[vertex][other] && reachable[other][vertex])
                            .unwrap()
                    })
                    .collect();
                let view = graph(n, &edges, Directedness::Directed);
                let mut bulk = Meter::default();
                let actual = strongly_connected(&view, &mut bulk).unwrap();
                let KernelValues::Components(actual_rows) = &actual.values else {
                    panic!("SCC produces component labels");
                };
                assert_eq!(actual_rows, &expected, "n={n} mask={mask}");
                assert_eq!(actual.witness.nodes_touched, 2 * n);
                assert_eq!(actual.witness.edges_scanned, 2 * edges.len());
                assert_eq!(bulk.units, 6 * n + 2 * edges.len());
                let mut scalar = Meter::default();
                let prior = strongly_connected_incumbent(&view, &mut || scalar.unit());
                same_components(Ok(actual), prior);
                assert_eq!(bulk.units, scalar.units);
            }
        }
    }

    #[test]
    fn scc_runs_keep_every_scalar_refusal_and_dfs_boundary() {
        let n = 2 * FNX_METER_RUN + 7;
        let mut hub = vec![(0, 0), (0, 1), (0, 1)];
        for vertex in 1..n - 2 {
            hub.push((0, vertex));
            hub.push((vertex, 0));
            // Deep DFS descents and long already-seen prefixes alternate.
            hub.push((vertex, (vertex + 1) % (n - 2)));
        }
        let cases = [
            (0, Vec::new()),
            (n, Vec::new()),
            (n, hub),
            (n, (1..n).map(|vertex| (vertex - 1, vertex)).collect()),
            (
                8,
                (0..8)
                    .flat_map(|source| (0..8).map(move |target| (source, target)))
                    .collect(),
            ),
            // The first member reached in the second pass is not its minimum
            // ordinal, and later finish-order roots are already assigned.
            (7, vec![(6, 2), (2, 4), (4, 6), (5, 1), (1, 3), (3, 5)]),
        ];
        for (n, edges) in cases {
            for direction in [Directedness::Directed, Directedness::Reversed] {
                let view = graph(n, &edges, direction);
                let mut scalar = Meter::default();
                let prior = strongly_connected_incumbent(&view, &mut || scalar.unit());
                let mut bulk = Meter::default();
                let actual = strongly_connected(&view, &mut bulk);
                same_components(actual, prior);
                assert_eq!(bulk.units, scalar.units);
                if n >= FNX_METER_RUN {
                    assert!(bulk.calls < scalar.calls);
                    assert!(bulk.full_runs >= n / FNX_METER_RUN);
                }
                if n >= FNX_METER_RUN && edges.len() > 2 * n {
                    assert!(
                        bulk.full_runs > n / FNX_METER_RUN,
                        "DFS, transpose, roots, and relabeling must batch beyond initialization"
                    );
                }
                for stop in 1..=scalar.units + 1 {
                    let mut scalar = Meter {
                        stop: Some(stop),
                        ..Meter::default()
                    };
                    let prior = strongly_connected_incumbent(&view, &mut || scalar.unit());
                    let mut bulk = Meter {
                        stop: Some(stop),
                        ..Meter::default()
                    };
                    let actual = strongly_connected(&view, &mut bulk);
                    same_components(actual, prior);
                    assert_eq!(bulk.units, scalar.units, "n={n} {direction:?} stop={stop}");
                    assert_eq!(bulk.stopped, scalar.stopped);
                }
            }
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

    /// Paired direction-switch comparison against the actual batched push
    /// fallback, including allocation, metering, evidence and cleanup. Graph
    /// projection, CALL admission, certificates and database I/O are excluded.
    /// Sparse and cutoff cases expose fallback overhead alongside dense gains.
    #[test]
    #[ignore = "paired bounded-pull BFS measurement; run optimized with --nocapture"]
    fn bounded_bfs_paired_push_measurement() {
        use std::hint::black_box;
        use std::time::Instant;

        #[derive(Default)]
        struct Work(usize);
        impl FnxMeter<usize> for Work {
            fn poll(&mut self) -> Result<(), usize> {
                Ok(())
            }
            fn unit(&mut self) -> Result<(), usize> {
                self.units(1)
            }
            fn units(&mut self, count: usize) -> Result<(), usize> {
                self.0 = black_box(self.0 + count);
                Ok(())
            }
        }
        fn measure(view: &SnapshotGraphView, cutoff: Option<usize>, adaptive: bool) -> u128 {
            let start = Instant::now();
            for _ in 0..64 {
                let mut work = Work::default();
                let output = if adaptive {
                    black_box(
                        bounded_bfs(black_box(view), 0, cutoff, view.node_count(), &mut work)
                            .unwrap(),
                    )
                    .output
                } else {
                    bfs(black_box(view), 0, cutoff, view.node_count(), &mut work).unwrap()
                };
                black_box((output, work.0));
            }
            start.elapsed().as_nanos()
        }
        for shape in ["road", "hub", "dense", "dense_cutoff", "layered"] {
            let (n, edges, cutoff) = match shape {
                "road" => (8193, (1..8193).map(|v| (v - 1, v)).collect(), None),
                "hub" => (
                    8193,
                    (1..8193).flat_map(|v| [(0, v), (v, 0)]).collect(),
                    None,
                ),
                "dense" | "dense_cutoff" => {
                    let (n, edges) = dense_bfs_fixture(512);
                    (n, edges, (shape == "dense_cutoff").then_some(1))
                }
                "layered" => {
                    let (n, edges) = layered_bfs_fixture(256, 96);
                    (n, edges, None)
                }
                _ => unreachable!(),
            };
            let view = graph(n, &edges, Directedness::Directed);
            let mut prior_work = Work::default();
            let prior = bfs(&view, 0, cutoff, n, &mut prior_work).unwrap();
            let mut current_work = Work::default();
            let current = bounded_bfs(&view, 0, cutoff, n, &mut current_work).unwrap();
            assert_eq!(distance_values(&prior), distance_values(&current.output));
            let arcs = *view.outgoing_offsets().last().unwrap();
            assert!(
                current.output.witness.nodes_touched + current.output.witness.edges_scanned
                    <= n + arcs
            );
            let pulls = current
                .bfs_decision_card
                .as_ref()
                .map_or(0, |card| card.pulls.len());
            assert_eq!(pulls > 0, shape == "dense" || shape == "layered");
            let (mut prior_ns, mut current_ns) = (Vec::new(), Vec::new());
            for sample in 0..15 {
                let (prior, current) = if sample % 2 == 0 {
                    (measure(&view, cutoff, false), measure(&view, cutoff, true))
                } else {
                    let current = measure(&view, cutoff, true);
                    (measure(&view, cutoff, false), current)
                };
                prior_ns.push(prior);
                current_ns.push(current);
            }
            println!(
                "{{\"scope\":\"decoded_bfs_kernel_including_allocation_metering_evidence_cleanup\",\"shape\":\"{shape}\",\"vertices\":{n},\"arcs\":{arcs},\"pulls\":{pulls},\"repetitions_per_sample\":64,\"incumbent_ns\":{prior_ns:?},\"bounded_ns\":{current_ns:?},\"incumbent_units\":{},\"bounded_units\":{},\"empirical_gate_activated\":false}}",
                prior_work.0, current_work.0
            );
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
