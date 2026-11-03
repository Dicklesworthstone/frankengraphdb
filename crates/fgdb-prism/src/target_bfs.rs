//! One-target unweighted distance with a visited bitmap and level FIFO.
//!
//! Decoded and compressed drivers share every traversal transition. A target
//! is settled only when popped, between complete rows: finding it in a row
//! never skips that row's remaining neighbors or fallible EOF. No distance
//! vector or adjacency copy survives into the single-row result.
//! A one-hop cutoff needs only the complete source row, so it retains no
//! bitmap or FIFO. It still validates every neighbor and drains through EOF;
//! discovering the target does not hide a later source failure.

use crate::execute::{KernelOutput, KernelValues, admit, reserve};
use crate::{ComplexityWitness, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView};
use fgdb_types::VId;
use std::mem::size_of;

fn trivial(source: usize, target: usize, cutoff: Option<usize>) -> bool {
    source == target || cutoff == Some(0)
}

pub(crate) fn one_hop<T: PartialEq>(source: T, target: T, cutoff: Option<usize>) -> bool {
    let eligible = source != target && cutoff == Some(1);
    #[cfg(test)]
    let eligible = eligible && !tests::force_fifo();
    eligible
}

pub(crate) fn workspace<C>(
    nodes: usize,
    source: usize,
    target: usize,
    cutoff: Option<usize>,
) -> Result<usize, FnxExecutionError<C>> {
    if one_hop(source, target, cutoff) {
        return Ok(0);
    }
    fifo_workspace(nodes, source, target, cutoff)
}

fn fifo_workspace<C>(
    nodes: usize,
    source: usize,
    target: usize,
    cutoff: Option<usize>,
) -> Result<usize, FnxExecutionError<C>> {
    if trivial(source, target, cutoff) {
        return Ok(0);
    }
    nodes
        .div_ceil(u64::BITS as usize)
        .checked_mul(size_of::<u64>())
        .and_then(|bytes| {
            nodes
                .checked_mul(size_of::<usize>())
                .and_then(|queue| bytes.checked_add(queue))
        })
        .ok_or(FnxExecutionError::SizeOverflow)
}

pub(crate) fn estimated_work<C>(
    nodes: usize,
    arcs: usize,
    pass: usize,
    columns: usize,
) -> Result<usize, FnxExecutionError<C>> {
    // One bitset word initialization, source/queue transitions, each possible
    // projected arc, the provider's complete row/history bound, and one result.
    nodes
        .div_ceil(u64::BITS as usize)
        .checked_add(nodes)
        .and_then(|work| work.checked_add(arcs))
        .and_then(|work| work.checked_add(pass))
        .and_then(|work| work.checked_add(columns))
        .and_then(|work| work.checked_add(16))
        .ok_or(FnxExecutionError::SizeOverflow)
}

#[derive(Clone, Copy)]
enum Phase {
    Initialize,
    Seed,
    Next,
    Row,
    Complete,
}

pub(crate) enum Step {
    Scalar,
    Open(usize),
    Complete,
}

pub(crate) struct Search {
    nodes: usize,
    source: usize,
    target: usize,
    cutoff: Option<usize>,
    visited: Vec<u64>,
    queue: Vec<usize>,
    head: usize,
    frontier_end: usize,
    depth: usize,
    phase: Phase,
    distance: Option<usize>,
    witness: ComplexityWitness,
    workspace: usize,
}

impl Search {
    pub(crate) fn new<C>(
        nodes: usize,
        source: usize,
        target: usize,
        cutoff: Option<usize>,
        limit: usize,
    ) -> Result<Self, FnxExecutionError<C>> {
        if source >= nodes || target >= nodes {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        let minimum = fifo_workspace::<C>(nodes, source, target, cutoff)?;
        admit("kernel workspace bytes", minimum, limit)?;
        let (visited, queue, actual) = if trivial(source, target, cutoff) {
            (Vec::new(), Vec::new(), 0)
        } else {
            let visited = reserve::<u64, C>(nodes.div_ceil(u64::BITS as usize))?;
            let bits = visited
                .capacity()
                .checked_mul(size_of::<u64>())
                .ok_or(FnxExecutionError::SizeOverflow)?;
            let requested = nodes
                .checked_mul(size_of::<usize>())
                .and_then(|bytes| bytes.checked_add(bits))
                .ok_or(FnxExecutionError::SizeOverflow)?;
            admit("kernel workspace bytes", requested, limit)?;
            let queue = reserve::<usize, C>(nodes)?;
            let actual = queue
                .capacity()
                .checked_mul(size_of::<usize>())
                .and_then(|bytes| bytes.checked_add(bits))
                .ok_or(FnxExecutionError::SizeOverflow)?;
            admit("kernel workspace bytes", actual, limit)?;
            (visited, queue, actual)
        };
        Ok(Self {
            nodes,
            source,
            target,
            cutoff,
            visited,
            queue,
            head: 0,
            frontier_end: 1,
            depth: 0,
            phase: if trivial(source, target, cutoff) {
                Phase::Complete
            } else {
                Phase::Initialize
            },
            distance: (source == target).then_some(0),
            witness: ComplexityWitness {
                algorithm: "shortest_path_length_target_bitmap_fifo".to_owned(),
                complexity_claim: "O(|V| + |E|) worst-case traversal, stopping at the target; O(|V|) FIFO plus visited bits; O(1) retained result".to_owned(),
                nodes_touched: usize::from(source == target),
                edges_scanned: 0,
                queue_peak: 0,
            },
            workspace: actual,
        })
    }

    /// One bounded scalar transition. Open means the driver must drain that
    /// row, then call end_row; no second vertex may be popped before its EOF.
    #[inline(always)]
    pub(crate) fn step<C>(&mut self) -> Result<Step, FnxExecutionError<C>> {
        match self.phase {
            Phase::Initialize => {
                if self.visited.len() < self.nodes.div_ceil(u64::BITS as usize) {
                    self.visited.push(0);
                } else {
                    self.phase = Phase::Seed;
                }
                Ok(Step::Scalar)
            }
            Phase::Seed => {
                self.mark::<C>(self.source)?;
                self.queue.push(self.source);
                self.witness.queue_peak = 1;
                self.phase = Phase::Next;
                Ok(Step::Scalar)
            }
            Phase::Next => {
                if self.head == self.queue.len() {
                    self.phase = Phase::Complete;
                    return Ok(Step::Complete);
                }
                if self.head == self.frontier_end {
                    self.depth = self
                        .depth
                        .checked_add(1)
                        .ok_or(FnxExecutionError::SizeOverflow)?;
                    self.frontier_end = self.queue.len();
                }
                let vertex = self.queue[self.head];
                self.head += 1;
                self.witness.nodes_touched = self
                    .witness
                    .nodes_touched
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                if vertex == self.target {
                    self.distance = Some(self.depth);
                    self.phase = Phase::Complete;
                    return Ok(Step::Complete);
                }
                if self.cutoff.is_some_and(|limit| self.depth >= limit) {
                    return Ok(Step::Scalar);
                }
                self.phase = Phase::Row;
                Ok(Step::Open(vertex))
            }
            Phase::Complete => Ok(Step::Complete),
            Phase::Row => Err(FnxExecutionError::InvalidUpstreamResult),
        }
    }

    fn mark<C>(&mut self, vertex: usize) -> Result<bool, FnxExecutionError<C>> {
        if vertex >= self.nodes {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        let word = self
            .visited
            .get_mut(vertex / 64)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        let mask = 1u64 << (vertex % 64);
        let fresh = *word & mask == 0;
        *word |= mask;
        Ok(fresh)
    }

    #[inline(always)]
    pub(crate) fn visit<C>(&mut self, neighbor: usize) -> Result<(), FnxExecutionError<C>> {
        if !matches!(self.phase, Phase::Row) {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        self.witness.edges_scanned = self
            .witness
            .edges_scanned
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        if self.mark::<C>(neighbor)? {
            self.queue.push(neighbor);
            self.witness.queue_peak = self.witness.queue_peak.max(self.queue.len() - self.head);
        }
        Ok(())
    }

    pub(crate) fn end_row<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        if !matches!(self.phase, Phase::Row) {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        self.phase = Phase::Next;
        Ok(())
    }

    pub(crate) fn row_count(&self) -> usize {
        usize::from(self.distance.is_some())
    }

    pub(crate) fn finish(self) -> (KernelOutput, usize) {
        debug_assert!(matches!(self.phase, Phase::Complete));
        let row_count = self.row_count();
        (
            KernelOutput {
                values: KernelValues::TargetHopDistance {
                    target: self.target,
                    distance: self.distance,
                },
                row_count,
                witness: self.witness,
            },
            self.workspace,
        )
    }
}

/// Constant-state scan shared by decoded and compressed one-hop drivers. The
/// caller must drain its source row through fallible EOF before `finish`.
pub(crate) struct OneHop {
    nodes: usize,
    target: usize,
    distance: Option<usize>,
    witness: ComplexityWitness,
}

impl OneHop {
    pub(crate) fn new<C>(
        nodes: usize,
        source: usize,
        target: usize,
    ) -> Result<Self, FnxExecutionError<C>> {
        if source >= nodes || target >= nodes || source == target {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        Ok(Self {
            nodes,
            target,
            distance: None,
            witness: ComplexityWitness {
                algorithm: "shortest_path_length_target_one_hop_scan".to_owned(),
                complexity_claim: "O(1 + out_degree(source)) traversal; O(1) state and retained result; complete source row drained".to_owned(),
                nodes_touched: 1,
                edges_scanned: 0,
                queue_peak: 0,
            },
        })
    }

    pub(crate) fn visit<C>(&mut self, neighbor: usize) -> Result<(), FnxExecutionError<C>> {
        if neighbor >= self.nodes {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        self.witness.edges_scanned = self
            .witness
            .edges_scanned
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        if neighbor == self.target {
            self.distance = Some(1);
        }
        Ok(())
    }

    pub(crate) fn row_count(&self) -> usize {
        usize::from(self.distance.is_some())
    }

    pub(crate) fn finish(self) -> KernelOutput {
        KernelOutput {
            row_count: self.row_count(),
            values: KernelValues::TargetHopDistance {
                target: self.target,
                distance: self.distance,
            },
            witness: self.witness,
        }
    }
}

#[inline]
pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    source: VId,
    target: VId,
    cutoff: Option<usize>,
    row_limit: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    if one_hop(source, target, cutoff) {
        run_one_hop(graph, source, target, row_limit, checkpoint)
    } else {
        run_fifo(graph, source, target, cutoff, row_limit, checkpoint)
    }
}

fn run_one_hop<C>(
    graph: &SnapshotGraphView,
    source: VId,
    target: VId,
    row_limit: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let source = graph
        .vertex_ordinal(source)
        .ok_or(FnxExecutionError::UnknownSource(source))?;
    let target = graph
        .vertex_ordinal(target)
        .ok_or(FnxExecutionError::UnknownTarget(target))?;
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    let mut state = OneHop::new(graph.node_count(), source, target)?;
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    let row = graph
        .neighbors_indices(source)
        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
    for &neighbor in row {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        state.visit(neighbor)?;
    }
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    admit("result rows", state.row_count(), row_limit)?;
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    Ok((state.finish(), 0))
}

fn run_fifo<C>(
    graph: &SnapshotGraphView,
    source: VId,
    target: VId,
    cutoff: Option<usize>,
    row_limit: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let source = graph
        .vertex_ordinal(source)
        .ok_or(FnxExecutionError::UnknownSource(source))?;
    let target = graph
        .vertex_ordinal(target)
        .ok_or(FnxExecutionError::UnknownTarget(target))?;
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    let mut state = Search::new(graph.node_count(), source, target, cutoff, usize::MAX)?;
    loop {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        match state.step()? {
            Step::Scalar => {}
            Step::Complete => break,
            Step::Open(vertex) => {
                let row = graph
                    .neighbors_indices(vertex)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                for &neighbor in row {
                    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
                    state.visit(neighbor)?;
                }
                state.end_row()?;
            }
        }
    }
    admit("result rows", state.row_count(), row_limit)?;
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    Ok(state.finish())
}

#[cfg(test)]
#[path = "target_bfs_tests.rs"]
mod tests;
