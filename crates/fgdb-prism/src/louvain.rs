//! Deterministic multilevel Louvain over a bounded working edge list.
//!
//! Canonical input edges are read once. Every later level reuses those two
//! admitted edge buffers: stable merge sorting folds parallel coarse edges in
//! encounter order, then restores that order before building incidence links.
//! No string graph, hash-table iteration, recursive traversal, or opaque FNX
//! kernel hides work from the caller. Each scalar step or edge visit can stop.

use crate::execute::{KernelOutput, KernelValues, admit, reserve};
use crate::{
    ComplexityWitness, FnxExecutionError, FnxMeter, GraphView, LouvainOptions, SnapshotGraphView,
};

const NONE: usize = usize::MAX;
pub(crate) const KERNEL: &str = "fgdb-prism/louvain-multilevel-stable-edges-v1";

#[derive(Clone, Copy, Default)]
struct Edge {
    left: usize,
    right: usize,
    weight: f64,
    order: usize,
    next_left: usize,
    next_right: usize,
}

pub(crate) fn workspace<C>(n: usize, arcs: usize) -> Result<usize, FnxExecutionError<C>> {
    n.checked_mul(6 * size_of::<usize>() + 4 * size_of::<f64>())
        .and_then(|bytes| {
            arcs.checked_mul(2 * size_of::<Edge>())
                .and_then(|edges| bytes.checked_add(edges))
        })
        .ok_or(FnxExecutionError::SizeOverflow)
}

pub(crate) fn estimated_work<C>(
    n: usize,
    arcs: usize,
    pass: usize,
    iterations: usize,
    columns: usize,
) -> Result<usize, FnxExecutionError<C>> {
    let log = (usize::BITS - arcs.leading_zeros()) as usize;
    let overflow = || FnxExecutionError::SizeOverflow;
    let sweep = arcs
        .checked_mul(
            32usize
                .checked_add(log.checked_mul(4).ok_or_else(overflow)?)
                .ok_or_else(overflow)?,
        )
        .and_then(|value| n.checked_mul(32).and_then(|nodes| value.checked_add(nodes)))
        .and_then(|value| value.checked_add(64))
        .ok_or_else(overflow)?;
    let output = n
        .checked_mul(columns.checked_add(4).ok_or_else(overflow)?)
        .ok_or_else(overflow)?;
    sweep
        .checked_mul(iterations.checked_add(1).ok_or_else(overflow)?)
        .and_then(|value| value.checked_add(pass))
        .and_then(|value| value.checked_add(output))
        .ok_or_else(overflow)
}

#[derive(Clone, Copy)]
enum SortKey {
    Pair,
    Order,
}

#[derive(Clone, Copy)]
struct Merge {
    key: SortKey,
    width: usize,
    left: usize,
    right: usize,
    middle: usize,
    end: usize,
    out: usize,
}

impl Merge {
    fn new(key: SortKey, width: usize, start: usize, len: usize) -> Self {
        let middle = start.saturating_add(width).min(len);
        let end = middle.saturating_add(width).min(len);
        Self {
            key,
            width,
            left: start,
            right: middle,
            middle,
            end,
            out: start,
        }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Initialize(usize),
    Load(usize),
    Reset(usize),
    Stats(usize),
    Links(usize),
    InitialModularity(usize, f64),
    Sweep,
    Node(usize),
    Neighbors {
        node: usize,
        cursor: usize,
    },
    Candidates {
        node: usize,
        at: usize,
        best: usize,
        gain: f64,
        remove: f64,
    },
    ClearGroups(usize),
    MarkGroups(usize),
    NumberGroups {
        at: usize,
        count: usize,
    },
    ClearSums(usize),
    DegreeSums(usize),
    InternalWeights(usize),
    Modularity(usize, f64),
    Relabel {
        at: usize,
        stop: bool,
    },
    Transform(usize),
    Sort(Merge),
    Reduce {
        read: usize,
        written: usize,
    },
    FinalReset(usize),
    FinalFind(usize),
    FinalAssign(usize),
    Complete,
}

pub(crate) struct Iteration {
    options: LouvainOptions,
    original_n: usize,
    n: usize,
    edge_limit: usize,
    max_iterations: usize,
    iterations: usize,
    levels: usize,
    groups: usize,
    epoch: usize,
    moved: bool,
    improved: bool,
    total: f64,
    baseline: f64,
    labels: Vec<usize>,
    original: Vec<usize>,
    remap: Vec<usize>,
    marks: Vec<usize>,
    touched: Vec<usize>,
    heads: Vec<usize>,
    degrees: Vec<f64>,
    totals: Vec<f64>,
    weights: Vec<f64>,
    intra: Vec<f64>,
    edges: Vec<Edge>,
    scratch: Vec<Edge>,
    phase: Phase,
    peak: usize,
    witness: ComplexityWitness,
}

impl Iteration {
    pub(crate) fn new<C>(
        n: usize,
        arcs: usize,
        options: LouvainOptions,
        max_iterations: usize,
        limit: usize,
    ) -> Result<Self, FnxExecutionError<C>> {
        admit("kernel workspace bytes", workspace::<C>(n, arcs)?, limit)?;
        // Incidence links encode the side in the low bit.
        arcs.checked_mul(2).ok_or(FnxExecutionError::SizeOverflow)?;
        let labels = reserve::<usize, C>(n)?;
        let original = reserve::<usize, C>(n)?;
        let remap = reserve::<usize, C>(n)?;
        let marks = reserve::<usize, C>(n)?;
        let touched = reserve::<usize, C>(n)?;
        let heads = reserve::<usize, C>(n)?;
        let degrees = reserve::<f64, C>(n)?;
        let totals = reserve::<f64, C>(n)?;
        let weights = reserve::<f64, C>(n)?;
        let intra = reserve::<f64, C>(n)?;
        let edges = reserve::<Edge, C>(arcs)?;
        let scratch = reserve::<Edge, C>(arcs)?;
        let mut peak = 0usize;
        for (capacity, size) in [
            (labels.capacity(), size_of::<usize>()),
            (original.capacity(), size_of::<usize>()),
            (remap.capacity(), size_of::<usize>()),
            (marks.capacity(), size_of::<usize>()),
            (touched.capacity(), size_of::<usize>()),
            (heads.capacity(), size_of::<usize>()),
            (degrees.capacity(), size_of::<f64>()),
            (totals.capacity(), size_of::<f64>()),
            (weights.capacity(), size_of::<f64>()),
            (intra.capacity(), size_of::<f64>()),
            (edges.capacity(), size_of::<Edge>()),
            (scratch.capacity(), size_of::<Edge>()),
        ] {
            peak = capacity
                .checked_mul(size)
                .and_then(|bytes| peak.checked_add(bytes))
                .ok_or(FnxExecutionError::SizeOverflow)?;
        }
        admit("kernel workspace bytes", peak, limit)?;
        Ok(Self {
            options,
            original_n: n,
            n,
            edge_limit: arcs,
            max_iterations,
            iterations: 0,
            levels: 0,
            groups: 0,
            epoch: 0,
            moved: false,
            improved: false,
            total: 0.0,
            baseline: 0.0,
            labels,
            original,
            remap,
            marks,
            touched,
            heads,
            degrees,
            totals,
            weights,
            intra,
            edges,
            scratch,
            phase: Phase::Initialize(0),
            peak,
            witness: ComplexityWitness {
                algorithm: "louvain_multilevel".to_owned(),
                complexity_claim:
                    "O(s * (|V| + |E|) + l * |E| log(1+|E|)); O(|V| + |E|) working graph".to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            },
        })
    }

    pub(crate) fn complete(&self) -> bool {
        matches!(self.phase, Phase::Complete)
    }
    pub(crate) fn row(&self) -> Option<usize> {
        match self.phase {
            Phase::Load(vertex) if vertex < self.original_n => Some(vertex),
            _ => None,
        }
    }

    pub(crate) fn visit<C>(
        &mut self,
        neighbor: usize,
        weight: f64,
    ) -> Result<(), FnxExecutionError<C>> {
        let Some(source) = self.row() else {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        };
        if neighbor >= self.original_n {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        self.witness.edges_scanned = self
            .witness
            .edges_scanned
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let weight = if self.options.weighted() { weight } else { 1.0 };
        if !weight.is_finite() {
            return Err(FnxExecutionError::InvalidNumericResult);
        }
        if weight < 0.0 {
            return Err(FnxExecutionError::NegativeWeight);
        }
        if source <= neighbor {
            if self.edges.len() == self.edge_limit {
                return Err(FnxExecutionError::InvalidUpstreamResult);
            }
            self.edges.push(Edge {
                left: source,
                right: neighbor,
                weight,
                order: self.edges.len(),
                next_left: NONE,
                next_right: NONE,
            });
            self.scratch.push(Edge::default());
        }
        Ok(())
    }

    pub(crate) fn end_row<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        let Some(source) = self.row() else {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        };
        self.phase = Phase::Load(source + 1);
        Ok(())
    }

    fn finite<C>(value: f64) -> Result<f64, FnxExecutionError<C>> {
        if value.is_finite() {
            Ok(value)
        } else {
            Err(FnxExecutionError::InvalidNumericResult)
        }
    }

    fn modularity_term<C>(&self, intra: f64, degree: f64) -> Result<f64, FnxExecutionError<C>> {
        let share = degree / (2.0 * self.total);
        Self::finite(intra / self.total - self.options.resolution() * (share * share))
    }

    pub(crate) fn scalar_step<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        self.phase = match self.phase {
            Phase::Initialize(at) if at < self.original_n => {
                self.labels.push(at);
                self.original.push(at);
                self.remap.push(NONE);
                self.marks.push(0);
                self.heads.push(NONE);
                self.degrees.push(0.0);
                self.totals.push(0.0);
                self.weights.push(0.0);
                self.intra.push(0.0);
                Phase::Initialize(at + 1)
            }
            Phase::Initialize(_) => Phase::Load(0),
            Phase::Load(at) if at == self.original_n => Phase::Reset(0),
            Phase::Reset(at) if at < self.n => {
                self.labels[at] = at;
                self.degrees[at] = 0.0;
                self.totals[at] = 0.0;
                self.intra[at] = 0.0;
                self.heads[at] = NONE;
                Phase::Reset(at + 1)
            }
            Phase::Reset(_) => {
                self.total = 0.0;
                self.improved = false;
                Phase::Stats(0)
            }
            Phase::Stats(at) if at < self.edges.len() => {
                let edge = self.edges[at];
                self.total = Self::finite(self.total + edge.weight)?;
                if edge.left == edge.right {
                    self.degrees[edge.left] =
                        Self::finite(self.degrees[edge.left] + 2.0 * edge.weight)?;
                    self.intra[edge.left] = Self::finite(self.intra[edge.left] + edge.weight)?;
                } else {
                    self.degrees[edge.left] = Self::finite(self.degrees[edge.left] + edge.weight)?;
                    self.degrees[edge.right] =
                        Self::finite(self.degrees[edge.right] + edge.weight)?;
                }
                Phase::Stats(at + 1)
            }
            Phase::Stats(_) => {
                if self.total == 0.0 {
                    Phase::FinalReset(0)
                } else {
                    Self::finite(2.0 * self.total * self.total)?;
                    Phase::Links(self.edges.len())
                }
            }
            Phase::Links(remaining) if remaining > 0 => {
                let at = remaining - 1;
                let edge = &mut self.edges[at];
                if edge.left != edge.right {
                    edge.next_left = self.heads[edge.left];
                    self.heads[edge.left] = at * 2;
                    edge.next_right = self.heads[edge.right];
                    self.heads[edge.right] = at * 2 + 1;
                }
                Phase::Links(at)
            }
            Phase::Links(_) => Phase::InitialModularity(0, 0.0),
            Phase::InitialModularity(at, sum) if at < self.n => {
                self.totals[at] = self.degrees[at];
                let sum = if self.levels == 0 {
                    Self::finite(
                        sum + self.modularity_term::<C>(self.intra[at], self.degrees[at])?,
                    )?
                } else {
                    sum
                };
                Phase::InitialModularity(at + 1, sum)
            }
            Phase::InitialModularity(_, sum) => {
                if self.levels == 0 {
                    self.baseline = sum;
                }
                Phase::Sweep
            }
            Phase::Sweep => {
                if self.iterations == self.max_iterations {
                    return Err(FnxExecutionError::NotConverged {
                        max_iterations: self.max_iterations,
                        witness: self.witness.clone(),
                    });
                }
                self.iterations += 1;
                self.moved = false;
                Phase::Node(0)
            }
            Phase::Node(node) if node < self.n => {
                self.epoch = self
                    .epoch
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                self.witness.nodes_touched = self
                    .witness
                    .nodes_touched
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                self.touched.clear();
                Phase::Neighbors {
                    node,
                    cursor: self.heads[node],
                }
            }
            Phase::Node(_) => {
                if self.moved {
                    Phase::Sweep
                } else {
                    self.levels += 1;
                    Phase::ClearGroups(0)
                }
            }
            Phase::Neighbors { node, cursor } if cursor != NONE => {
                let edge = self.edges[cursor / 2];
                let (neighbor, next) = if cursor & 1 == 0 {
                    (edge.right, edge.next_left)
                } else {
                    (edge.left, edge.next_right)
                };
                let community = self.labels[neighbor];
                if self.marks[community] != self.epoch {
                    self.marks[community] = self.epoch;
                    self.weights[community] = edge.weight;
                    self.touched.push(community);
                    self.witness.queue_peak = self.witness.queue_peak.max(self.touched.len());
                } else {
                    self.weights[community] = Self::finite(self.weights[community] + edge.weight)?;
                }
                self.witness.edges_scanned = self
                    .witness
                    .edges_scanned
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                Phase::Neighbors { node, cursor: next }
            }
            Phase::Neighbors { node, .. } => {
                let current = self.labels[node];
                self.totals[current] = Self::finite(self.totals[current] - self.degrees[node])?;
                let own = if self.marks[current] == self.epoch {
                    self.weights[current]
                } else {
                    0.0
                };
                let remove = Self::finite(
                    -own / self.total
                        + self.options.resolution() * self.totals[current] * self.degrees[node]
                            / (2.0 * self.total * self.total),
                )?;
                Phase::Candidates {
                    node,
                    at: 0,
                    best: current,
                    gain: 0.0,
                    remove,
                }
            }
            Phase::Candidates {
                node,
                at,
                mut best,
                mut gain,
                remove,
            } if at < self.touched.len() => {
                let candidate = self.touched[at];
                let next = Self::finite(
                    remove + self.weights[candidate] / self.total
                        - self.options.resolution() * self.totals[candidate] * self.degrees[node]
                            / (2.0 * self.total * self.total),
                )?;
                if next > gain {
                    gain = next;
                    best = candidate;
                }
                Phase::Candidates {
                    node,
                    at: at + 1,
                    best,
                    gain,
                    remove,
                }
            }
            Phase::Candidates { node, best, .. } => {
                self.totals[best] = Self::finite(self.totals[best] + self.degrees[node])?;
                if best != self.labels[node] {
                    self.labels[node] = best;
                    self.moved = true;
                    self.improved = true;
                }
                Phase::Node(node + 1)
            }
            Phase::ClearGroups(at) if at < self.n => {
                self.remap[at] = NONE;
                Phase::ClearGroups(at + 1)
            }
            Phase::ClearGroups(_) => Phase::MarkGroups(0),
            Phase::MarkGroups(at) if at < self.n => {
                self.remap[self.labels[at]] = 0;
                Phase::MarkGroups(at + 1)
            }
            Phase::MarkGroups(_) => Phase::NumberGroups { at: 0, count: 0 },
            Phase::NumberGroups { at, mut count } if at < self.n => {
                if self.remap[at] != NONE {
                    self.remap[at] = count;
                    count += 1;
                }
                Phase::NumberGroups { at: at + 1, count }
            }
            Phase::NumberGroups { count, .. } => {
                self.groups = count;
                Phase::ClearSums(0)
            }
            Phase::ClearSums(at) if at < self.groups => {
                self.totals[at] = 0.0;
                self.intra[at] = 0.0;
                Phase::ClearSums(at + 1)
            }
            Phase::ClearSums(_) => Phase::DegreeSums(0),
            Phase::DegreeSums(at) if at < self.n => {
                let group = self.remap[self.labels[at]];
                self.totals[group] = Self::finite(self.totals[group] + self.degrees[at])?;
                Phase::DegreeSums(at + 1)
            }
            Phase::DegreeSums(_) => Phase::InternalWeights(0),
            Phase::InternalWeights(at) if at < self.edges.len() => {
                let edge = self.edges[at];
                if self.labels[edge.left] == self.labels[edge.right] {
                    let group = self.remap[self.labels[edge.left]];
                    self.intra[group] = Self::finite(self.intra[group] + edge.weight)?;
                }
                Phase::InternalWeights(at + 1)
            }
            Phase::InternalWeights(_) => Phase::Modularity(0, 0.0),
            Phase::Modularity(at, sum) if at < self.groups => Phase::Modularity(
                at + 1,
                Self::finite(sum + self.modularity_term::<C>(self.intra[at], self.totals[at])?)?,
            ),
            Phase::Modularity(_, value) => {
                let stop = self
                    .options
                    .max_levels()
                    .is_some_and(|limit| self.levels >= limit)
                    || !self.improved
                    || value - self.baseline <= self.options.threshold()
                    || self.groups == self.n;
                self.baseline = value;
                Phase::Relabel { at: 0, stop }
            }
            Phase::Relabel { at, stop } if at < self.original_n => {
                self.original[at] = self.remap[self.labels[self.original[at]]];
                Phase::Relabel { at: at + 1, stop }
            }
            Phase::Relabel { stop: true, .. } => Phase::FinalReset(0),
            Phase::Relabel { .. } => Phase::Transform(0),
            Phase::Transform(at) if at < self.edges.len() => {
                let edge = &mut self.edges[at];
                let left = self.remap[self.labels[edge.left]];
                let right = self.remap[self.labels[edge.right]];
                edge.left = left.min(right);
                edge.right = left.max(right);
                edge.order = at;
                Phase::Transform(at + 1)
            }
            Phase::Transform(_) => Phase::Sort(Merge::new(SortKey::Pair, 1, 0, self.edges.len())),
            Phase::Sort(mut merge) => {
                let len = self.edges.len();
                if merge.width >= len {
                    match merge.key {
                        SortKey::Pair => Phase::Reduce {
                            read: 0,
                            written: 0,
                        },
                        SortKey::Order => {
                            self.n = self.groups;
                            Phase::Reset(0)
                        }
                    }
                } else if merge.out < merge.end {
                    let take_left = if merge.left == merge.middle {
                        false
                    } else if merge.right == merge.end {
                        true
                    } else {
                        let left = self.edges[merge.left];
                        let right = self.edges[merge.right];
                        match merge.key {
                            SortKey::Pair => (left.left, left.right) <= (right.left, right.right),
                            SortKey::Order => left.order <= right.order,
                        }
                    };
                    let at = if take_left {
                        let at = merge.left;
                        merge.left += 1;
                        at
                    } else {
                        let at = merge.right;
                        merge.right += 1;
                        at
                    };
                    self.scratch[merge.out] = self.edges[at];
                    merge.out += 1;
                    Phase::Sort(merge)
                } else if merge.end < len {
                    Phase::Sort(Merge::new(merge.key, merge.width, merge.end, len))
                } else {
                    std::mem::swap(&mut self.edges, &mut self.scratch);
                    Phase::Sort(Merge::new(merge.key, merge.width.saturating_mul(2), 0, len))
                }
            }
            Phase::Reduce { read, mut written } if read < self.edges.len() => {
                let edge = self.edges[read];
                if written > 0
                    && (self.edges[written - 1].left, self.edges[written - 1].right)
                        == (edge.left, edge.right)
                {
                    self.edges[written - 1].weight =
                        Self::finite(self.edges[written - 1].weight + edge.weight)?;
                } else {
                    self.edges[written] = edge;
                    written += 1;
                }
                Phase::Reduce {
                    read: read + 1,
                    written,
                }
            }
            Phase::Reduce { written, .. } => {
                self.edges.truncate(written);
                self.scratch.truncate(written);
                Phase::Sort(Merge::new(SortKey::Order, 1, 0, written))
            }
            Phase::FinalReset(at) if at < self.original_n => {
                self.remap[at] = NONE;
                Phase::FinalReset(at + 1)
            }
            Phase::FinalReset(_) => Phase::FinalFind(0),
            Phase::FinalFind(at) if at < self.original_n => {
                let group = self.original[at];
                self.remap[group] = self.remap[group].min(at);
                Phase::FinalFind(at + 1)
            }
            Phase::FinalFind(_) => Phase::FinalAssign(0),
            Phase::FinalAssign(at) if at < self.original_n => {
                self.original[at] = self.remap[self.original[at]];
                Phase::FinalAssign(at + 1)
            }
            Phase::FinalAssign(_) => Phase::Complete,
            Phase::Complete => Phase::Complete,
            Phase::Load(_) => return Err(FnxExecutionError::InvalidUpstreamResult),
        };
        Ok(())
    }

    pub(crate) fn finish(self) -> (KernelOutput, usize) {
        debug_assert!(self.complete());
        (
            KernelOutput {
                values: KernelValues::Components(self.original),
                row_count: self.original_n,
                witness: self.witness,
            },
            self.peak,
        )
    }
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    options: LouvainOptions,
    max_iterations: usize,
    meter: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    meter.unit().map_err(FnxExecutionError::Cancelled)?;
    let arcs = graph
        .outgoing_offsets()
        .last()
        .copied()
        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
    let mut state = Iteration::new(
        graph.node_count(),
        arcs,
        options,
        max_iterations,
        usize::MAX,
    )?;
    while !state.complete() {
        meter.unit().map_err(FnxExecutionError::Cancelled)?;
        if let Some(vertex) = state.row() {
            let (row, weights) = graph
                .projected_row(vertex)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            for (at, &neighbor) in row.iter().enumerate() {
                meter.unit().map_err(FnxExecutionError::Cancelled)?;
                state.visit(neighbor, weights[at])?;
            }
            state.end_row()?;
        } else {
            state.scalar_step()?;
        }
    }
    meter.unit().map_err(FnxExecutionError::Cancelled)?;
    Ok(state.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Directedness, FnxArgument, FnxCallSpec, FnxExecutionLimits, FnxValue, ParallelEdgePolicy,
        ProjectionEdge, ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding,
    };
    use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
    use std::collections::BTreeMap;
    use std::convert::Infallible;

    fn fixture(n: usize, pairs: &[(usize, usize, f64)]) -> SnapshotGraphView {
        let vertices: Vec<_> = (0..n)
            .map(|index| VId((1u128 << 100) | index as u128))
            .collect();
        let edges: Vec<_> = pairs
            .iter()
            .enumerate()
            .map(|(at, &(left, right, weight))| ProjectionEdge {
                eid: EId(at as u128),
                source: vertices[left],
                target: vertices[right],
                weight,
            })
            .collect();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([0x31; 32]),
                as_of: CommitSeq(7),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: Directedness::Undirected,
                parallel_edges: ParallelEdgePolicy::Sum,
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

    fn components(graph: &SnapshotGraphView, options: LouvainOptions) -> Vec<usize> {
        let (output, _) = run(graph, options, 1000, &mut || Ok::<(), Infallible>(())).unwrap();
        let KernelValues::Components(values) = output.values else {
            panic!("community values");
        };
        values
    }

    fn oracle(graph: &SnapshotGraphView, options: LouvainOptions) -> Vec<usize> {
        let mut copy = fnx_classes::Graph::strict();
        let names: Vec<_> = (0..graph.node_count())
            .map(|index| format!("{index:08x}"))
            .collect();
        for name in &names {
            copy.add_node(name);
        }
        for source in 0..graph.node_count() {
            let (neighbors, weights) = graph.projected_row(source).unwrap();
            for (at, &target) in neighbors.iter().enumerate() {
                if source <= target {
                    let weight = if options.weighted() { weights[at] } else { 1.0 };
                    copy.add_edge_with_attrs(
                        &names[source],
                        &names[target],
                        fnx_classes::AttrMap::from([("weight".to_owned(), weight.into())]),
                    )
                    .unwrap();
                }
            }
        }
        let groups = fnx_algorithms::louvain_communities(
            &copy,
            options.resolution(),
            "weight",
            options.threshold(),
            options.max_levels(),
            None,
        );
        let mut output = vec![usize::MAX; names.len()];
        for group in groups {
            let indices: Vec<_> = group
                .iter()
                .map(|name| usize::from_str_radix(name, 16).unwrap())
                .collect();
            let representative = *indices.iter().min().unwrap();
            for index in indices {
                output[index] = representative;
            }
        }
        output
    }

    #[test]
    fn every_small_weighted_topology_matches_the_pinned_multilevel_oracle() {
        for n in 0..=5 {
            let pairs: Vec<_> = (0..n)
                .flat_map(|left| (left + 1..n).map(move |right| (left, right)))
                .collect();
            for mask in 0usize..1usize << pairs.len() {
                let edges: Vec<_> = pairs
                    .iter()
                    .enumerate()
                    .filter(|(at, _)| mask & (1 << at) != 0)
                    .map(|(at, &(left, right))| {
                        (left, right, [0.0, 0.25, 1.0, 4.0][(mask + at) % 4])
                    })
                    .collect();
                let graph = fixture(n, &edges);
                for (resolution, levels) in [(1.0, None), (0.5, Some(1)), (2.0, None)] {
                    let options = LouvainOptions::new(resolution, 1e-7, levels, true).unwrap();
                    assert_eq!(
                        components(&graph, options),
                        oracle(&graph, options),
                        "n={n} mask={mask} resolution={resolution} levels={levels:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn coarsening_and_self_loops_preserve_the_pinned_weighted_partition() {
        let mut edges = Vec::new();
        for group in 0..12 {
            let a = 3 * group;
            edges.extend([
                (a, a + 1, 1.0),
                (a, a + 2, 1.0),
                (a + 1, a + 2, 1.0),
                (a + 2, (a + 3) % 36, 1.0),
            ]);
        }
        let graph = fixture(37, &edges);
        let full = LouvainOptions::default();
        let one = LouvainOptions::new(1.0, 1e-7, Some(1), true).unwrap();
        assert_eq!(components(&graph, full), oracle(&graph, full));
        assert_eq!(components(&graph, one), oracle(&graph, one));
        assert_ne!(
            components(&graph, full),
            components(&graph, one),
            "the fixture must execute graph coarsening"
        );
        assert_eq!(components(&graph, full)[36], 36, "isolate identity");
        edges.extend([(0, 0, 7.0), (4, 4, 2.0), (36, 36, 0.0)]);
        let graph = fixture(37, &edges);
        for options in [
            full,
            one,
            LouvainOptions::new(0.75, 0.0, None, false).unwrap(),
        ] {
            assert_eq!(components(&graph, options), oracle(&graph, options));
        }
    }

    #[test]
    fn algorithm_sweep_limit_is_admitted_separately_from_the_host_ceiling() {
        let graph = fixture(4, &[(0, 1, 1.0), (0, 2, 1.0), (1, 2, 1.0)]);
        let call = FnxCallSpec::louvain_communities(LouvainOptions::default());
        let limits = FnxExecutionLimits {
            max_iterations: 100_000,
            max_result_rows: 4,
            max_estimated_work: 100_000,
        };
        let result = call
            .execute(&graph, limits, || Ok::<(), Infallible>(()))
            .unwrap();
        let vertices = graph.vertex_ids();
        let expected: Vec<_> = [0, 0, 0, 3]
            .into_iter()
            .enumerate()
            .map(|(index, representative)| {
                vec![
                    FnxValue::Vertex(vertices[index]),
                    FnxValue::Vertex(vertices[representative]),
                ]
            })
            .collect();
        assert_eq!(result.rows, expected);
        for graph in [&graph, &fixture(0, &[])] {
            assert!(matches!(
                call.execute(
                    graph,
                    FnxExecutionLimits {
                        max_iterations: 99,
                        ..limits
                    },
                    || Ok::<(), Infallible>(()),
                ),
                Err(FnxExecutionError::LimitExceeded {
                    resource: "iterations",
                    requested: 100,
                    limit: 99,
                })
            ));
        }
        let one_sweep =
            FnxCallSpec::louvain_communities(LouvainOptions::default().with_max_iter(1).unwrap());
        assert!(matches!(
            one_sweep.execute(&graph, limits, || Ok::<(), Infallible>(())),
            Err(FnxExecutionError::NotConverged {
                max_iterations: 1,
                ..
            })
        ));
    }

    #[test]
    fn every_control_boundary_refuses_without_a_partial_partition() {
        let graph = fixture(5, &[(0, 1, 3.0), (1, 2, 1.0), (2, 3, 3.0), (3, 0, 0.5)]);
        let mut calls = 0;
        let (_, peak) = run(&graph, LouvainOptions::default(), 1000, &mut || {
            calls += 1;
            Ok::<(), usize>(())
        })
        .unwrap();
        let arcs = graph.outgoing_offsets().last().copied().unwrap();
        assert!(peak <= workspace::<Infallible>(5, arcs).unwrap());
        assert!(calls <= estimated_work::<Infallible>(5, arcs, 5, 1000, 2).unwrap());
        for stop in 1..=calls {
            let mut calls = 0;
            let result = run(&graph, LouvainOptions::default(), 1000, &mut || {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(FnxExecutionError::Cancelled(at)) if at == stop));
        }
        assert!(matches!(
            run(&graph, LouvainOptions::default(), 1, &mut || Ok::<
                (),
                Infallible,
            >(
                ()
            )),
            Err(FnxExecutionError::NotConverged {
                max_iterations: 1,
                ..
            })
        ));
        let bytes = workspace::<Infallible>(5, arcs).unwrap();
        assert!(matches!(
            Iteration::new::<Infallible>(5, arcs, LouvainOptions::default(), 1000, bytes - 1),
            Err(FnxExecutionError::LimitExceeded {
                resource: "kernel workspace bytes",
                ..
            })
        ));
        let negative = fixture(2, &[(0, 1, -1.0)]);
        assert!(matches!(
            run(&negative, LouvainOptions::default(), 1000, &mut || Ok::<
                (),
                Infallible,
            >(
                ()
            )),
            Err(FnxExecutionError::NegativeWeight)
        ));
        let unweighted = LouvainOptions::new(1.0, 1e-7, None, false).unwrap();
        assert_eq!(components(&negative, unweighted), vec![0, 0]);
    }

    #[test]
    fn signature_options_are_validated_and_bound_into_identity() {
        let defaults = FnxCallSpec::louvain_communities(LouvainOptions::default());
        let bound = FnxCallSpec::bind(
            "CALL fnx.louvain_communities() YIELD vertex, component",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(defaults, bound);
        for arguments in [
            vec![FnxArgument::Float(0.0)],
            vec![FnxArgument::Float(f64::NAN)],
            vec![FnxArgument::Float(1.0), FnxArgument::Float(-1.0)],
            vec![
                FnxArgument::Float(1.0),
                FnxArgument::Float(0.0),
                FnxArgument::Integer(0),
            ],
        ] {
            assert!(
                FnxCallSpec::from_arguments("fnx", "louvain_communities", &arguments, &[]).is_err()
            );
        }
        for options in [
            LouvainOptions::new(2.0, 1e-7, None, true).unwrap(),
            LouvainOptions::new(1.0, 0.0, None, true).unwrap(),
            LouvainOptions::new(1.0, 1e-7, Some(1), true).unwrap(),
            LouvainOptions::new(1.0, 1e-7, None, false).unwrap(),
        ] {
            assert_ne!(defaults, FnxCallSpec::louvain_communities(options));
        }
    }
}
