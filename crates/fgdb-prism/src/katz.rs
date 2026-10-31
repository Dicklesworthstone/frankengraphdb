//! Checked Katz centrality over the admitted projection.
//!
//! The pinned FNX kernel scatters each source's previous score in canonical
//! source/neighbor order, applies `alpha * value + beta`, tests the absolute L1
//! change, and L2-normalizes only the converged result. Fixed-width VId names
//! and projection ordinals have the same order. This scalar state machine
//! preserves every addition across decoded rows, compressed cursors and yields
//! while retaining only two score vectors. No copied graph or incoming CSR is
//! needed, including for a directed projection.

use crate::execute::{KernelOutput, KernelValues, admit, reserve};
use crate::{
    ComplexityWitness, FNX_METER_RUN, FnxExecutionError, FnxMeter, GraphView, KatzOptions,
    SnapshotGraphView,
};
use std::mem::size_of;

fn shortcut(n: usize, options: KatzOptions) -> bool {
    n == 0 || (n == 1 && options == KatzOptions::default())
}

pub(crate) fn workspace<C>(n: usize, options: KatzOptions) -> Result<usize, FnxExecutionError<C>> {
    n.checked_mul(if shortcut(n, options) { 1 } else { 2 } * size_of::<f64>())
        .ok_or(FnxExecutionError::SizeOverflow)
}

/// Conservative admission includes every possible iteration, provider row
/// lookup/history work, scalar phases and result conversion. It is separate
/// from the witness, which reports the iterations actually completed.
pub(crate) fn estimated_work<C>(
    n: usize,
    arcs: usize,
    pass: usize,
    columns: usize,
    options: KatzOptions,
) -> Result<usize, FnxExecutionError<C>> {
    let overflow = || FnxExecutionError::SizeOverflow;
    let output = n
        .checked_mul(columns.checked_add(2).ok_or_else(overflow)?)
        .and_then(|value| value.checked_add(columns))
        .ok_or_else(overflow)?;
    let scalar = n
        .checked_mul(8)
        .and_then(|value| value.checked_add(output))
        .and_then(|value| value.checked_add(32))
        .ok_or_else(overflow)?;
    if shortcut(n, options) {
        return Ok(scalar);
    }
    pass.checked_add(arcs)
        .and_then(|value| n.checked_mul(5).and_then(|n| value.checked_add(n)))
        .and_then(|value| value.checked_add(12))
        .and_then(|value| value.checked_mul(options.max_iter()))
        .and_then(|value| value.checked_add(scalar))
        .ok_or_else(overflow)
}

fn reserved<C>(n: usize, live: &mut usize, limit: usize) -> Result<Vec<f64>, FnxExecutionError<C>> {
    let requested = n
        .checked_mul(size_of::<f64>())
        .and_then(|bytes| live.checked_add(bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    admit("kernel workspace bytes", requested, limit)?;
    let values = reserve::<f64, C>(n)?;
    let actual = values
        .capacity()
        .checked_mul(size_of::<f64>())
        .and_then(|bytes| live.checked_add(bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    // Charge the actual backing before initializing it or allocating again.
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

#[derive(Clone, Copy)]
enum Phase {
    Initialize(usize),
    Reset(usize),
    Scatter(usize),
    Affine(usize),
    Delta { at: usize, sum: f64 },
    Norm { at: usize, sum: f64 },
    Normalize { at: usize, normalizer: f64 },
    Complete,
}

pub(crate) struct Iteration {
    scores: Vec<f64>,
    next_scores: Vec<f64>,
    nodes: usize,
    iterations: usize,
    options: KatzOptions,
    phase: Phase,
    witness: ComplexityWitness,
    peak: usize,
}

impl Iteration {
    pub(crate) fn new<C>(
        nodes: usize,
        options: KatzOptions,
        limit: usize,
    ) -> Result<Self, FnxExecutionError<C>> {
        admit(
            "kernel workspace bytes",
            workspace::<C>(nodes, options)?,
            limit,
        )?;
        let mut live = 0;
        let scores = reserved(nodes, &mut live, limit)?;
        let next_scores = if shortcut(nodes, options) {
            Vec::new()
        } else {
            reserved(nodes, &mut live, limit)?
        };
        Ok(Self {
            scores,
            next_scores,
            nodes,
            iterations: 0,
            options,
            phase: Phase::Initialize(0),
            witness: ComplexityWitness {
                algorithm: "katz_centrality_power_iteration".to_owned(),
                complexity_claim: "O(k * (|V| + |E|))".to_owned(),
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
            Phase::Scatter(vertex) => Some(vertex),
            _ => None,
        }
    }

    pub(crate) fn visit<C>(&mut self, neighbor: usize) -> Result<(), FnxExecutionError<C>> {
        let Phase::Scatter(source) = self.phase else {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        };
        let target = self
            .next_scores
            .get_mut(neighbor)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        *target += self.scores[source];
        self.witness.edges_scanned = self
            .witness
            .edges_scanned
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        Ok(())
    }

    /// Drivers must reach the fallible row's EOF before advancing this phase.
    pub(crate) fn end_row<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        self.phase = match self.phase {
            Phase::Scatter(vertex) if vertex + 1 < self.nodes => Phase::Scatter(vertex + 1),
            Phase::Scatter(_) => {
                self.iterations = self
                    .iterations
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                self.witness.nodes_touched = self
                    .witness
                    .nodes_touched
                    .checked_add(self.nodes)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                Phase::Affine(0)
            }
            _ => return Err(FnxExecutionError::InvalidUpstreamResult),
        };
        Ok(())
    }

    /// Exactly one vertex operation or fixed-size phase transition. The scalar
    /// accumulator survives suspension, so no reduction reassociates additions.
    pub(crate) fn scalar_step<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        self.phase = match self.phase {
            Phase::Initialize(at) if at < self.nodes => {
                self.scores.push(if shortcut(self.nodes, self.options) {
                    1.0
                } else {
                    0.0
                });
                if !shortcut(self.nodes, self.options) {
                    self.next_scores.push(0.0);
                }
                Phase::Initialize(at + 1)
            }
            Phase::Initialize(_) if shortcut(self.nodes, self.options) => {
                // FNX returns empty/singleton results without any iteration.
                self.witness.nodes_touched = self.nodes;
                Phase::Complete
            }
            Phase::Initialize(_) => Phase::Reset(0),
            Phase::Reset(at) if at < self.nodes => {
                self.next_scores[at] = 0.0;
                Phase::Reset(at + 1)
            }
            Phase::Reset(_) => Phase::Scatter(0),
            Phase::Affine(at) if at < self.nodes => {
                self.next_scores[at] =
                    self.options.alpha() * self.next_scores[at] + self.options.beta();
                Phase::Affine(at + 1)
            }
            Phase::Affine(_) => Phase::Delta { at: 0, sum: 0.0 },
            Phase::Delta { at, sum } if at < self.nodes => Phase::Delta {
                at: at + 1,
                sum: sum + (self.next_scores[at] - self.scores[at]).abs(),
            },
            Phase::Delta { sum, .. } => {
                std::mem::swap(&mut self.scores, &mut self.next_scores);
                if sum < self.nodes as f64 * self.options.tolerance() {
                    Phase::Norm { at: 0, sum: 0.0 }
                } else if self.iterations == self.options.max_iter() {
                    // This is the pinned checked FNX entry's failure contract,
                    // including divergent/nonfinite iterates. No last iterate
                    // is published as a converged score.
                    return Err(FnxExecutionError::NotConverged {
                        max_iterations: self.options.max_iter(),
                        witness: self.witness.clone(),
                    });
                } else {
                    Phase::Reset(0)
                }
            }
            Phase::Norm { at, sum } if at < self.nodes => {
                let value = self.scores[at];
                Phase::Norm {
                    at: at + 1,
                    sum: sum + value * value,
                }
            }
            Phase::Norm { sum, .. } => {
                let norm = sum.sqrt();
                if !norm.is_finite() {
                    return Err(FnxExecutionError::InvalidNumericResult);
                }
                Phase::Normalize {
                    at: 0,
                    normalizer: norm,
                }
            }
            Phase::Normalize { at, normalizer } if at < self.nodes => {
                if normalizer > 0.0 {
                    self.scores[at] /= normalizer;
                } else if self.scores[at] != 0.0 {
                    // A tiny custom beta can underflow the sum of squares.
                    // Preserve a genuinely zero vector, but do not advertise
                    // nonzero unnormalized values as unit-length scores.
                    return Err(FnxExecutionError::InvalidNumericResult);
                }
                if !self.scores[at].is_finite() {
                    return Err(FnxExecutionError::InvalidNumericResult);
                }
                Phase::Normalize {
                    at: at + 1,
                    normalizer,
                }
            }
            Phase::Normalize { .. } => Phase::Complete,
            Phase::Scatter(_) => return Err(FnxExecutionError::InvalidUpstreamResult),
            Phase::Complete => Phase::Complete,
        };
        Ok(())
    }

    pub(crate) fn finish(self) -> (KernelOutput, usize) {
        debug_assert!(self.complete());
        (
            KernelOutput {
                values: KernelValues::Scores(self.scores),
                row_count: self.nodes,
                witness: self.witness,
            },
            self.peak,
        )
    }
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    options: KatzOptions,
    meter: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    meter.unit().map_err(FnxExecutionError::Cancelled)?;
    let mut state = Iteration::new(graph.node_count(), options, usize::MAX)?;
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

#[cfg(test)]
#[path = "katz_tests.rs"]
mod tests;
