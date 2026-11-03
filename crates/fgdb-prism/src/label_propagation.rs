//! Deterministic semi-synchronous label propagation on borrowed graph rows.
//!
//! The pinned FNX procedure colors vertices greedily in stored node order.
//! This projection pins that order to canonical VIds, then visits each color
//! class in that same order. Retaining a current label
//! on maximal ties prevents oscillation; other ties choose the smallest label.
//! One scalar state machine preserves those rules across decoded, compressed,
//! and cooperative execution without retaining adjacency or color-class lists.

use crate::execute::{KernelOutput, KernelValues, admit, reserve};
use crate::{
    ComplexityWitness, FNX_METER_RUN, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView,
};
use std::mem::size_of;

pub(crate) const MAX_ITERATIONS: usize = 100;

pub(crate) fn workspace<C>(n: usize) -> Result<usize, FnxExecutionError<C>> {
    n.checked_mul(5 * size_of::<usize>())
        .ok_or(FnxExecutionError::SizeOverflow)
}

/// Includes the coloring pass, its bounded color search, all 100 possible
/// propagation passes, scalar bookkeeping, and canonical output. `pass`
/// charges physical row lookup/history work separately from emitted arcs.
pub(crate) fn estimated_work<C>(
    n: usize,
    arcs: usize,
    pass: usize,
    columns: usize,
) -> Result<usize, FnxExecutionError<C>> {
    let overflow = || FnxExecutionError::SizeOverflow;
    let output = n
        .checked_mul(columns.checked_add(2).ok_or_else(overflow)?)
        .and_then(|value| value.checked_add(columns))
        .ok_or_else(overflow)?;
    let initialization = n
        .checked_mul(16)
        .and_then(|value| value.checked_add(arcs))
        .and_then(|value| value.checked_add(output))
        .and_then(|value| value.checked_add(32))
        .ok_or_else(overflow)?;
    pass.checked_add(arcs)
        .and_then(|value| n.checked_mul(4).and_then(|n| value.checked_add(n)))
        .and_then(|value| value.checked_add(4))
        .and_then(|value| value.checked_mul(MAX_ITERATIONS + 1))
        .and_then(|value| value.checked_add(initialization))
        .ok_or_else(overflow)
}

fn reserved<C>(
    n: usize,
    live: &mut usize,
    limit: usize,
) -> Result<Vec<usize>, FnxExecutionError<C>> {
    let requested = n
        .checked_mul(size_of::<usize>())
        .and_then(|bytes| live.checked_add(bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    admit("kernel workspace bytes", requested, limit)?;
    let values = reserve::<usize, C>(n)?;
    let actual = values
        .capacity()
        .checked_mul(size_of::<usize>())
        .and_then(|bytes| live.checked_add(bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

#[derive(Clone, Copy)]
enum Phase {
    Initialize(usize),
    ColorRow(usize),
    ColorChoose {
        vertex: usize,
        color: usize,
    },
    ColorPrefix {
        color: usize,
        offset: usize,
    },
    ColorScatter(usize),
    ResetMarks(usize),
    Propagate {
        at: usize,
        best_label: usize,
        best_count: usize,
    },
    EndRound,
    RepresentativeReset(usize),
    RepresentativeFind(usize),
    RepresentativeAssign(usize),
    Complete,
}

pub(crate) struct Iteration {
    labels: Vec<usize>,
    // Reused for canonical community representatives after convergence.
    colors: Vec<usize>,
    // Generation stamps avoid an O(V) frequency reset for every vertex.
    marks: Vec<usize>,
    // Color-class sizes, then stable scatter positions, then label counts.
    counts: Vec<usize>,
    order: Vec<usize>,
    nodes: usize,
    color_count: usize,
    iterations: usize,
    epoch: usize,
    changed: bool,
    phase: Phase,
    witness: ComplexityWitness,
    peak: usize,
}

impl Iteration {
    pub(crate) fn new<C>(nodes: usize, limit: usize) -> Result<Self, FnxExecutionError<C>> {
        admit("kernel workspace bytes", workspace::<C>(nodes)?, limit)?;
        let mut live = 0;
        let labels = reserved(nodes, &mut live, limit)?;
        let colors = reserved(nodes, &mut live, limit)?;
        let marks = reserved(nodes, &mut live, limit)?;
        let counts = reserved(nodes, &mut live, limit)?;
        let order = reserved(nodes, &mut live, limit)?;
        Ok(Self {
            labels,
            colors,
            marks,
            counts,
            order,
            nodes,
            color_count: 0,
            iterations: 0,
            epoch: 0,
            changed: false,
            phase: Phase::Initialize(0),
            witness: ComplexityWitness {
                algorithm: "label_propagation_semisynchronous".to_owned(),
                complexity_claim: "O((k + 1) * (|V| + |E|)); O(|V|) integer workspace".to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            },
            peak: live,
        })
    }

    pub(crate) fn complete(&self) -> bool {
        matches!(self.phase, Phase::Complete)
    }

    pub(crate) fn row(&self) -> Option<usize> {
        match self.phase {
            Phase::ColorRow(vertex) => Some(vertex),
            Phase::Propagate { at, .. } => Some(self.order[at]),
            _ => None,
        }
    }

    pub(crate) fn visit<C>(&mut self, neighbor: usize) -> Result<(), FnxExecutionError<C>> {
        let source = self.row().ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        if source == neighbor {
            return Err(FnxExecutionError::SelfLoopRefused);
        }
        let &label = self
            .labels
            .get(neighbor)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        match self.phase {
            Phase::ColorRow(vertex) => {
                if neighbor < vertex {
                    self.marks[self.colors[neighbor]] = vertex;
                }
            }
            Phase::Propagate {
                at,
                mut best_label,
                mut best_count,
            } => {
                if self.marks[label] != self.epoch {
                    self.marks[label] = self.epoch;
                    self.counts[label] = 0;
                }
                let count = self.counts[label]
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                self.counts[label] = count;
                if count > best_count || (count == best_count && label < best_label) {
                    best_count = count;
                    best_label = label;
                }
                self.phase = Phase::Propagate {
                    at,
                    best_label,
                    best_count,
                };
            }
            _ => return Err(FnxExecutionError::InvalidUpstreamResult),
        }
        self.witness.edges_scanned = self
            .witness
            .edges_scanned
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        Ok(())
    }

    fn next_epoch<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        self.epoch = self
            .epoch
            .checked_add(1)
            .filter(|&value| value != usize::MAX)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        Ok(())
    }

    pub(crate) fn end_row<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        self.phase = match self.phase {
            Phase::ColorRow(vertex) => Phase::ColorChoose { vertex, color: 0 },
            Phase::Propagate {
                at,
                best_label,
                best_count,
            } => {
                let vertex = self.order[at];
                let current = self.labels[vertex];
                if best_count > 0
                    && (self.marks[current] != self.epoch || self.counts[current] != best_count)
                {
                    self.labels[vertex] = best_label;
                    self.changed = true;
                }
                if at + 1 < self.nodes {
                    self.next_epoch()?;
                    Phase::Propagate {
                        at: at + 1,
                        best_label: usize::MAX,
                        best_count: 0,
                    }
                } else {
                    Phase::EndRound
                }
            }
            _ => return Err(FnxExecutionError::InvalidUpstreamResult),
        };
        self.witness.nodes_touched = self
            .witness
            .nodes_touched
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        Ok(())
    }

    /// Every step touches one vertex, one color, or fixed-size metadata. The
    /// color-class ordering uses a stable counting scatter, not a hidden sort.
    pub(crate) fn scalar_step<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        self.phase = match self.phase {
            Phase::Initialize(at) if at < self.nodes => {
                self.labels.push(at);
                self.colors.push(0);
                self.marks.push(usize::MAX);
                self.counts.push(0);
                self.order.push(0);
                Phase::Initialize(at + 1)
            }
            Phase::Initialize(_) if self.nodes == 0 => Phase::Complete,
            Phase::Initialize(_) => Phase::ColorRow(0),
            Phase::ColorChoose { vertex, color } => {
                let &mark = self
                    .marks
                    .get(color)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                if mark == vertex {
                    Phase::ColorChoose {
                        vertex,
                        color: color + 1,
                    }
                } else {
                    self.colors[vertex] = color;
                    self.counts[color] += 1;
                    self.color_count = self.color_count.max(color + 1);
                    if vertex + 1 < self.nodes {
                        Phase::ColorRow(vertex + 1)
                    } else {
                        Phase::ColorPrefix {
                            color: 0,
                            offset: 0,
                        }
                    }
                }
            }
            Phase::ColorPrefix { color, offset } if color < self.color_count => {
                let end = offset
                    .checked_add(self.counts[color])
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                self.counts[color] = end;
                Phase::ColorPrefix {
                    color: color + 1,
                    offset: end,
                }
            }
            Phase::ColorPrefix { offset, .. } => {
                if offset != self.nodes {
                    return Err(FnxExecutionError::InvalidUpstreamResult);
                }
                Phase::ColorScatter(self.nodes)
            }
            Phase::ColorScatter(remaining) if remaining > 0 => {
                let vertex = remaining - 1;
                let color = self.colors[vertex];
                self.counts[color] -= 1;
                self.order[self.counts[color]] = vertex;
                Phase::ColorScatter(vertex)
            }
            Phase::ColorScatter(_) => Phase::ResetMarks(0),
            Phase::ResetMarks(at) if at < self.nodes => {
                self.marks[at] = usize::MAX;
                Phase::ResetMarks(at + 1)
            }
            Phase::ResetMarks(_) => {
                self.iterations = 1;
                Phase::Propagate {
                    at: 0,
                    best_label: usize::MAX,
                    best_count: 0,
                }
            }
            Phase::EndRound => {
                if !self.changed || self.iterations == MAX_ITERATIONS {
                    // FNX returns its last partition at the cap.
                    Phase::RepresentativeReset(0)
                } else {
                    self.iterations += 1;
                    self.changed = false;
                    self.next_epoch()?;
                    Phase::Propagate {
                        at: 0,
                        best_label: usize::MAX,
                        best_count: 0,
                    }
                }
            }
            Phase::RepresentativeReset(at) if at < self.nodes => {
                self.colors[at] = usize::MAX;
                Phase::RepresentativeReset(at + 1)
            }
            Phase::RepresentativeReset(_) => Phase::RepresentativeFind(0),
            Phase::RepresentativeFind(at) if at < self.nodes => {
                let representative = &mut self.colors[self.labels[at]];
                *representative = (*representative).min(at);
                Phase::RepresentativeFind(at + 1)
            }
            Phase::RepresentativeFind(_) => Phase::RepresentativeAssign(0),
            Phase::RepresentativeAssign(at) if at < self.nodes => {
                self.labels[at] = self.colors[self.labels[at]];
                Phase::RepresentativeAssign(at + 1)
            }
            Phase::RepresentativeAssign(_) => Phase::Complete,
            Phase::ColorRow(_) | Phase::Propagate { .. } => {
                return Err(FnxExecutionError::InvalidUpstreamResult);
            }
            Phase::Complete => Phase::Complete,
        };
        Ok(())
    }

    pub(crate) fn finish(self) -> (KernelOutput, usize) {
        debug_assert!(self.complete());
        (
            KernelOutput {
                values: KernelValues::Components(self.labels),
                row_count: self.nodes,
                witness: self.witness,
            },
            self.peak,
        )
    }
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    meter: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    meter.unit().map_err(FnxExecutionError::Cancelled)?;
    let mut state = Iteration::new(graph.node_count(), usize::MAX)?;
    while !state.complete() {
        meter.unit().map_err(FnxExecutionError::Cancelled)?;
        if let Some(vertex) = state.row() {
            let neighbors = graph
                .neighbors_indices(vertex)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            for run in neighbors.chunks(FNX_METER_RUN) {
                meter
                    .units(run.len())
                    .map_err(FnxExecutionError::Cancelled)?;
                for &neighbor in run {
                    state.visit(neighbor)?;
                }
            }
            state.end_row()?;
        } else {
            state.scalar_step()?;
        }
    }
    meter.unit().map_err(FnxExecutionError::Cancelled)?;
    Ok(state.finish())
}
