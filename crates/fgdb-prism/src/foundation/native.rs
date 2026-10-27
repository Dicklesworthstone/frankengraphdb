//! Deterministic degree, core and Newman-load kernels over the admitted CSR.
//!
//! These kernels borrow the projection. They do not build an fnx Graph, node
//! names, or a second adjacency index. The caller retains the catalog's
//! logical charge sequence; arithmetic checks live control without charging
//! those units again.

use crate::execute::{FnxMeter, KernelOutput, KernelValues, reserve};
use crate::{FnxExecutionError, SnapshotGraphView};
use fnx_algorithms::{ComplexityWitness, GraphView};

/// A work step is one initialization, bucket, vertex or incidence operation.
/// Even a single hub row yields to live authority/cancellation between runs.
const ARITHMETIC_RUN: usize = 4096;

pub(super) struct Arithmetic<'a, M> {
    checkpoint: &'a mut M,
    remaining: usize,
}

impl<'a, M> Arithmetic<'a, M> {
    pub(super) fn new(checkpoint: &'a mut M) -> Self {
        Self {
            checkpoint,
            remaining: 0,
        }
    }

    #[inline]
    pub(super) fn step<C>(&mut self) -> Result<(), FnxExecutionError<C>>
    where
        M: FnxMeter<C>,
    {
        if self.remaining == 0 {
            self.checkpoint
                .poll()
                .map_err(FnxExecutionError::Cancelled)?;
            self.remaining = ARITHMETIC_RUN;
        }
        self.remaining -= 1;
        Ok(())
    }

    pub(super) fn finish<C>(&mut self) -> Result<(), FnxExecutionError<C>>
    where
        M: FnxMeter<C>,
    {
        self.checkpoint.poll().map_err(FnxExecutionError::Cancelled)
    }
}

pub(super) fn vector_bytes<T, C>(values: &Vec<T>) -> Result<usize, FnxExecutionError<C>> {
    values
        .capacity()
        .checked_mul(size_of::<T>())
        .ok_or(FnxExecutionError::SizeOverflow)
}

fn zeroes<C>(
    length: usize,
    arithmetic: &mut Arithmetic<'_, impl FnxMeter<C>>,
) -> Result<Vec<usize>, FnxExecutionError<C>> {
    let mut values = reserve(length)?;
    for _ in 0..length {
        arithmetic.step()?;
        values.push(0);
    }
    Ok(values)
}

pub(super) fn degree<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut arithmetic = Arithmetic::new(checkpoint);
    arithmetic.step()?;
    let mut values = reserve(n)?;
    // Match the pinned foundation's multiplication by a single reciprocal.
    // Replacing this with one division per vertex changes some score bits.
    let reciprocal = if n > 1 { 1.0 / (n - 1) as f64 } else { 1.0 };
    let mut arcs = 0usize;
    for vertex in 0..n {
        arithmetic.step()?;
        let degree = graph
            .neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?
            .len();
        arcs = arcs
            .checked_add(degree)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        values.push(if n == 1 {
            1.0
        } else {
            degree as f64 * reciprocal
        });
    }
    let workspace = vector_bytes(&values)?;
    arithmetic.finish()?;
    Ok((
        KernelOutput {
            values: KernelValues::Scores(values),
            row_count: n,
            witness: ComplexityWitness {
                algorithm: "degree_centrality".to_owned(),
                complexity_claim: "O(|V| + |E|)".to_owned(),
                nodes_touched: n,
                edges_scanned: arcs,
                queue_peak: 0,
            },
        },
        workspace,
    ))
}

/// Directed degree uses division, unlike the undirected foundation's reciprocal
/// multiplication. Both immutable CSR faces already encode their exact degrees
/// in adjacent offsets. Reading those directories avoids scanning every edge
/// and allocating a second incoming-count vector on each call.
pub(crate) fn directed_degree<C>(
    graph: &SnapshotGraphView,
    incoming: bool,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut arithmetic = Arithmetic::new(checkpoint);
    let offsets = if incoming {
        graph.incoming_face().0
    } else {
        graph.outgoing_offsets()
    };
    if offsets.len().checked_sub(1) != Some(n) {
        return Err(FnxExecutionError::InvalidUpstreamResult);
    }
    let mut values = reserve(n)?;
    for row in offsets.windows(2) {
        arithmetic.step()?;
        let degree = row[1]
            .checked_sub(row[0])
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        values.push(if n == 1 {
            1.0
        } else {
            degree as f64 / (n - 1) as f64
        });
    }
    let workspace = vector_bytes(&values)?;
    arithmetic.finish()?;
    Ok((
        KernelOutput {
            values: KernelValues::Scores(values),
            row_count: n,
            witness: ComplexityWitness {
                algorithm: if incoming {
                    "fgdb_in_degree_centrality"
                } else {
                    "fgdb_out_degree_centrality"
                }
                .to_owned(),
                complexity_claim: "O(|V|)".to_owned(),
                nodes_touched: n,
                edges_scanned: 0,
                queue_peak: 0,
            },
        },
        workspace,
    ))
}

pub(super) fn core<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut arithmetic = Arithmetic::new(checkpoint);
    arithmetic.step()?;
    if n == 0 {
        arithmetic.finish()?;
        return Ok((
            KernelOutput {
                values: KernelValues::Counts(Vec::new()),
                row_count: 0,
                witness: ComplexityWitness {
                    algorithm: "core_number".to_owned(),
                    complexity_claim: "O(|V| + |E|)".to_owned(),
                    nodes_touched: 0,
                    edges_scanned: 0,
                    queue_peak: 0,
                },
            },
            0,
        ));
    }

    let mut degree = reserve(n)?;
    let mut max_degree = 0usize;
    for vertex in 0..n {
        arithmetic.step()?;
        let count = graph
            .neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?
            .len();
        degree.push(count);
        max_degree = max_degree.max(count);
    }
    let bins_len = max_degree
        .checked_add(1)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    let mut bins = zeroes(bins_len, &mut arithmetic)?;
    let mut vertices = zeroes(n, &mut arithmetic)?;
    let mut position = zeroes(n, &mut arithmetic)?;
    let degree_bytes = vector_bytes(&degree)?;
    let bins_bytes = vector_bytes(&bins)?;
    let vertices_bytes = vector_bytes(&vertices)?;
    let position_bytes = vector_bytes(&position)?;
    let scratch_peak = degree_bytes
        .checked_add(bins_bytes)
        .and_then(|bytes| bytes.checked_add(vertices_bytes))
        .and_then(|bytes| bytes.checked_add(position_bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;

    // Stable counting sort by initial degree. The simple, loop-free graph
    // has max_degree < n, so this is linear and needs no comparison sort.
    for &count in &degree {
        arithmetic.step()?;
        bins[count] += 1;
    }
    let mut start = 0usize;
    for slot in &mut bins {
        arithmetic.step()?;
        let count = *slot;
        *slot = start;
        start = start
            .checked_add(count)
            .ok_or(FnxExecutionError::SizeOverflow)?;
    }
    for vertex in 0..n {
        arithmetic.step()?;
        let count = degree[vertex];
        let slot = bins[count];
        position[vertex] = slot;
        vertices[slot] = vertex;
        bins[count] += 1;
    }
    // Scatter advanced each bin to its end. Restore the bucket starts.
    for count in (1..bins.len()).rev() {
        arithmetic.step()?;
        bins[count] = bins[count - 1];
    }
    bins[0] = 0;

    let mut arcs = 0usize;
    for slot in 0..n {
        arithmetic.step()?;
        let vertex = vertices[slot];
        let row = graph
            .neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for &neighbor in row {
            arithmetic.step()?;
            if degree[neighbor] > degree[vertex] {
                let count = degree[neighbor];
                let from = position[neighbor];
                let to = bins[count];
                let first = vertices[to];
                if neighbor != first {
                    vertices[from] = first;
                    vertices[to] = neighbor;
                    position[neighbor] = to;
                    position[first] = from;
                }
                bins[count] += 1;
                degree[neighbor] -= 1;
            }
        }
        arcs = arcs
            .checked_add(row.len())
            .ok_or(FnxExecutionError::SizeOverflow)?;
    }

    // A peeled vertex's degree never decreases again: the final degree is
    // its core number. Release the bucket index before allocating results.
    drop(bins);
    drop(vertices);
    drop(position);
    let mut counts = reserve(n)?;
    let conversion_peak = degree_bytes
        .checked_add(vector_bytes(&counts)?)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    for count in degree {
        arithmetic.step()?;
        counts.push(u64::try_from(count).map_err(|_| FnxExecutionError::SizeOverflow)?);
    }
    arithmetic.finish()?;
    Ok((
        KernelOutput {
            values: KernelValues::Counts(counts),
            row_count: n,
            witness: ComplexityWitness {
                algorithm: "core_number".to_owned(),
                complexity_claim: "O(|V| + |E|)".to_owned(),
                nodes_touched: n,
                edges_scanned: arcs,
                queue_peak: 0,
            },
        },
        scratch_peak.max(conversion_peak),
    ))
}

/// Includes reached-only resets/reductions, both incidence passes, scalar
/// transitions and delivery. Each source reaches at most C vertices, where
/// C <= min(V, arcs + 1): a component with C vertices needs C-1 edges, and
/// every such edge contributes at least one admitted arc. The looser arc
/// bound also covers directed input conservatively; the registered Load
/// procedure itself requires an undirected, loop-free projection.
/// Connected inputs retain the former bound; edgeless input is now O(V).
pub(crate) fn load_work(nodes: usize, arcs: usize) -> Option<usize> {
    Load::source_bound(nodes, arcs)
        .checked_mul(nodes.checked_mul(4)?.checked_add(arcs.checked_mul(2)?)?)?
        .checked_add(nodes.checked_mul(11)?)?
        .checked_add(16)
}

#[derive(Clone, Copy)]
enum LoadPhase {
    Initialize(usize),
    Reset(usize),
    BreadthFirst,
    Reverse(usize),
    Reduce(usize),
    Normalize(usize),
    Complete,
}

#[derive(Clone, Copy)]
pub(crate) enum LoadStep {
    Scalar,
    Row(usize),
    Complete,
}

/// One bounded state machine serves decoded and compressed graph readers.
/// The BFS queue is also the reverse dependency stack. Predecessor counts are
/// collected during BFS; parents are recovered from the immutable undirected
/// row, without retaining adjacency or a vector of predecessor lists. The
/// same unique queue identifies precisely the scratch cells to reset and
/// output cells to reduce: other components need no work for this source.
pub(crate) struct Load {
    distances: Vec<usize>,
    predecessors: Vec<usize>,
    queue: Vec<usize>,
    between: Vec<f64>,
    scores: Vec<f64>,
    phase: LoadPhase,
    source: usize,
    head: usize,
    active: Option<(usize, bool)>,
    seen_parents: usize,
    scale: f64,
    nodes: usize,
    peak: usize,
    witness: ComplexityWitness,
    #[cfg(test)]
    dense_scans: bool,
}

impl Load {
    /// A reachable component needs at least one fewer edge than vertices.
    /// Saturation avoids overflow for the conservative, arbitrary-arc caller;
    /// the minimum never exceeds this projection's admitted vertex count.
    pub(crate) fn source_bound(nodes: usize, arcs: usize) -> usize {
        nodes.min(arcs.saturating_add(1))
    }

    pub(crate) fn workspace(nodes: usize) -> Option<usize> {
        nodes.checked_mul(3 * size_of::<usize>() + 2 * size_of::<f64>())
    }

    pub(crate) fn new<C>(nodes: usize, limit: usize) -> Result<Self, FnxExecutionError<C>> {
        let expected = Self::workspace(nodes).ok_or(FnxExecutionError::SizeOverflow)?;
        super::super::execute::admit("kernel workspace bytes", expected, limit)?;
        let scale = if nodes > 2 {
            1.0 / (nodes - 1)
                .checked_mul(nodes - 2)
                .ok_or(FnxExecutionError::SizeOverflow)? as f64
        } else {
            1.0
        };
        let mut peak = 0usize;
        // Admit actual vector capacities before reserving the next buffer.
        fn buffer<T, C>(
            nodes: usize,
            live: &mut usize,
            limit: usize,
        ) -> Result<Vec<T>, FnxExecutionError<C>> {
            let requested = nodes
                .checked_mul(size_of::<T>())
                .and_then(|bytes| live.checked_add(bytes))
                .ok_or(FnxExecutionError::SizeOverflow)?;
            super::super::execute::admit("kernel workspace bytes", requested, limit)?;
            let values = reserve(nodes)?;
            let next = live
                .checked_add(vector_bytes(&values)?)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            super::super::execute::admit("kernel workspace bytes", next, limit)?;
            *live = next;
            Ok(values)
        }
        let distances = buffer(nodes, &mut peak, limit)?;
        let predecessors = buffer(nodes, &mut peak, limit)?;
        let queue = buffer(nodes, &mut peak, limit)?;
        let between = buffer(nodes, &mut peak, limit)?;
        let scores = buffer(nodes, &mut peak, limit)?;
        Ok(Self {
            distances,
            predecessors,
            queue,
            between,
            scores,
            phase: LoadPhase::Initialize(0),
            source: 0,
            head: 0,
            active: None,
            seen_parents: 0,
            scale,
            nodes,
            peak,
            witness: ComplexityWitness {
                algorithm: "fgdb_newman_load_centrality_cursor".to_owned(),
                complexity_claim:
                    "O(|V| + sum_components |C| * (|C| + |E_C|)); O(|V|) kernel workspace"
                        .to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            },
            #[cfg(test)]
            dense_scans: false,
        })
    }

    fn pass_len(&self) -> usize {
        #[cfg(test)]
        if self.dense_scans {
            return self.nodes;
        }
        self.queue.len()
    }

    fn pass_vertex(&self, at: usize) -> usize {
        #[cfg(test)]
        if self.dense_scans {
            return at;
        }
        // A connected source reaches every cell. Preserve the incumbent's
        // sequential memory access instead of following an arbitrary BFS
        // permutation when there is no sparse population to skip.
        if self.queue.len() == self.nodes {
            at
        } else {
            self.queue[at]
        }
    }

    /// One scalar transition, or one row whose pulls the caller controls.
    pub(crate) fn step<C>(&mut self) -> Result<LoadStep, FnxExecutionError<C>> {
        if self.active.is_some() {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        match self.phase {
            LoadPhase::Initialize(at) if at < self.nodes => {
                self.distances.push(usize::MAX);
                self.predecessors.push(0);
                self.between.push(0.0);
                self.scores.push(0.0);
                self.phase = LoadPhase::Initialize(at + 1);
            }
            LoadPhase::Initialize(_) if self.nodes == 0 => {
                self.phase = LoadPhase::Complete;
            }
            LoadPhase::Initialize(_) => self.phase = LoadPhase::Reset(0),
            LoadPhase::Reset(at) if at < self.pass_len() => {
                // Initialization already cleared every cell. Thereafter only
                // the preceding BFS's unique reached set can contain state;
                // resetting it leaves every other component untouched/clear.
                let vertex = self.pass_vertex(at);
                self.distances[vertex] = usize::MAX;
                self.predecessors[vertex] = 0;
                self.between[vertex] = 0.0;
                self.phase = LoadPhase::Reset(at + 1);
            }
            LoadPhase::Reset(_) => {
                self.queue.clear();
                self.queue.push(self.source);
                self.head = 0;
                self.distances[self.source] = 0;
                self.between[self.source] = 1.0;
                self.witness.queue_peak = self.witness.queue_peak.max(1);
                self.phase = LoadPhase::BreadthFirst;
            }
            LoadPhase::BreadthFirst if self.head < self.queue.len() => {
                let vertex = self.queue[self.head];
                self.head += 1;
                self.witness.nodes_touched = self
                    .witness
                    .nodes_touched
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                self.active = Some((vertex, false));
                return Ok(LoadStep::Row(vertex));
            }
            LoadPhase::BreadthFirst => self.phase = LoadPhase::Reverse(self.queue.len()),
            LoadPhase::Reverse(at) if at > 0 => {
                let vertex = self.queue[at - 1];
                self.phase = LoadPhase::Reverse(at - 1);
                // Distance-one vertices have only the source as predecessor;
                // upstream deliberately adds no flow to that source.
                if self.distances[vertex] > 1 {
                    if self.predecessors[vertex] == 0 {
                        return Err(FnxExecutionError::InvalidUpstreamResult);
                    }
                    self.active = Some((vertex, true));
                    self.seen_parents = 0;
                    return Ok(LoadStep::Row(vertex));
                }
            }
            LoadPhase::Reverse(_) => self.phase = LoadPhase::Reduce(0),
            LoadPhase::Reduce(at) if at < self.pass_len() => {
                let vertex = self.pass_vertex(at);
                if self.distances[vertex] != usize::MAX {
                    self.between[vertex] -= 1.0;
                }
                // Cells are independent: BFS order changes no floating-point
                // addition within a score. Sources still reduce in canonical
                // order. Unreached contributions were positive zero, so
                // omitting their additions preserves the exact score bits.
                self.scores[vertex] += self.between[vertex];
                if !self.scores[vertex].is_finite() {
                    return Err(FnxExecutionError::InvalidNumericResult);
                }
                self.phase = LoadPhase::Reduce(at + 1);
            }
            LoadPhase::Reduce(_) => {
                self.source += 1;
                self.phase = if self.source < self.nodes {
                    LoadPhase::Reset(0)
                } else {
                    LoadPhase::Normalize(0)
                };
            }
            LoadPhase::Normalize(at) if at < self.nodes => {
                self.scores[at] *= self.scale;
                self.phase = LoadPhase::Normalize(at + 1);
            }
            LoadPhase::Normalize(_) => self.phase = LoadPhase::Complete,
            LoadPhase::Complete => return Ok(LoadStep::Complete),
        }
        Ok(LoadStep::Scalar)
    }

    pub(crate) fn visit<C>(&mut self, neighbor: usize) -> Result<(), FnxExecutionError<C>> {
        let (vertex, reverse) = self
            .active
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        if neighbor >= self.nodes {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        if vertex == neighbor {
            return Err(FnxExecutionError::SelfLoopRefused);
        }
        self.witness.edges_scanned = self
            .witness
            .edges_scanned
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        if reverse {
            if self.distances[neighbor].checked_add(1) == Some(self.distances[vertex]) {
                self.seen_parents = self
                    .seen_parents
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                self.between[neighbor] += self.between[vertex] / self.predecessors[vertex] as f64;
            }
        } else {
            let next = self.distances[vertex]
                .checked_add(1)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            if self.distances[neighbor] == usize::MAX {
                self.distances[neighbor] = next;
                self.between[neighbor] = 1.0;
                self.queue.push(neighbor);
                self.witness.queue_peak = self.witness.queue_peak.max(self.queue.len() - self.head);
            }
            if self.distances[neighbor] == next {
                self.predecessors[neighbor] = self.predecessors[neighbor]
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
            }
        }
        Ok(())
    }

    pub(crate) fn end_row<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        let (vertex, reverse) = self
            .active
            .take()
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        if reverse && self.seen_parents != self.predecessors[vertex] {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        Ok(())
    }

    pub(crate) fn finish<C>(self) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
        if !matches!(self.phase, LoadPhase::Complete) || self.active.is_some() {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        Ok((
            KernelOutput {
                values: KernelValues::Scores(self.scores),
                row_count: self.nodes,
                witness: self.witness,
            },
            self.peak,
        ))
    }
}

pub(super) fn load<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    let state = Load::new(graph.node_count(), usize::MAX)?;
    drive_load(graph, state, checkpoint)
}

fn drive_load<C>(
    graph: &SnapshotGraphView,
    mut state: Load,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    loop {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        match state.step()? {
            LoadStep::Scalar => {}
            LoadStep::Complete => break,
            LoadStep::Row(vertex) => {
                for &neighbor in graph
                    .neighbors_indices(vertex)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?
                {
                    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
                    state.visit(neighbor)?;
                }
                state.end_row()?;
            }
        }
    }
    checkpoint.poll().map_err(FnxExecutionError::Cancelled)?;
    state.finish()
}

#[cfg(test)]
#[path = "load_tests.rs"]
mod load_tests;
