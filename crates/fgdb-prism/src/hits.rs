//! HITS over the admitted projection, with no copied graph or neighbor rows.
//!
//! The pinned fnx entry orders sources and neighbors by their names. Prism's
//! fixed-width names have exactly its VId ordinal order, so an outgoing-row
//! scatter for authorities followed by a row gather for hubs preserves every
//! addition. One state machine carries these accumulators across decoded,
//! compressed and cooperative drivers. Only three score vectors are retained.

use crate::execute::{KernelOutput, KernelValues, admit, reserve};
use crate::{
    ComplexityWitness, FNX_METER_RUN, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView,
};
use std::mem::size_of;

pub(crate) const MAX_ITERATIONS: usize = 100;
const TOLERANCE: f64 = 1e-8;

pub(crate) fn workspace<C>(n: usize) -> Result<usize, FnxExecutionError<C>> {
    n.checked_mul(if n <= 1 { 2 } else { 3 } * size_of::<f64>())
        .ok_or(FnxExecutionError::SizeOverflow)
}

/// A conservative physical admission bound. `pass` includes the provider's
/// row lookup/history work; both outgoing passes and all scalar phases count.
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
    let scalar = n
        .checked_mul(10)
        .and_then(|value| value.checked_add(output))
        .and_then(|value| value.checked_add(32))
        .ok_or_else(overflow)?;
    if n <= 1 {
        return Ok(scalar);
    }
    pass.checked_add(arcs)
        .and_then(|value| value.checked_mul(2))
        .and_then(|value| n.checked_mul(12).and_then(|n| value.checked_add(n)))
        .and_then(|value| value.checked_add(16))
        .and_then(|value| value.checked_mul(MAX_ITERATIONS))
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
    // Capacity, not the requested length, is the retained allocation charge.
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

#[derive(Clone, Copy)]
enum Phase {
    Initialize(usize),
    Reset(usize),
    Authority(usize),
    AuthoritySum { at: usize, sum: f64 },
    AuthorityNormalize { at: usize, sum: f64 },
    Hub { vertex: usize, sum: f64 },
    HubSum { at: usize, sum: f64 },
    HubNormalize { at: usize, sum: f64 },
    Delta { at: usize, sum: f64 },
    FinalHubSum { at: usize, sum: f64 },
    FinalHubNormalize { at: usize, sum: f64 },
    FinalAuthoritySum { at: usize, sum: f64 },
    FinalAuthorityNormalize { at: usize, sum: f64 },
    Validate(usize),
    Complete,
}

pub(crate) struct Iteration {
    hubs: Vec<f64>,
    authorities: Vec<f64>,
    next_hubs: Vec<f64>,
    nodes: usize,
    iterations: usize,
    phase: Phase,
    witness: ComplexityWitness,
    peak: usize,
}

impl Iteration {
    pub(crate) fn new<C>(nodes: usize, limit: usize) -> Result<Self, FnxExecutionError<C>> {
        admit("kernel workspace bytes", workspace::<C>(nodes)?, limit)?;
        let mut live = 0;
        let hubs = reserved(nodes, &mut live, limit)?;
        let authorities = reserved(nodes, &mut live, limit)?;
        let next_hubs = if nodes <= 1 {
            Vec::new()
        } else {
            reserved(nodes, &mut live, limit)?
        };
        Ok(Self {
            hubs,
            authorities,
            next_hubs,
            nodes,
            iterations: 0,
            phase: Phase::Initialize(0),
            witness: ComplexityWitness {
                algorithm: "hits_centrality_power_iteration".to_owned(),
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
            Phase::Authority(vertex) | Phase::Hub { vertex, .. } => Some(vertex),
            _ => None,
        }
    }

    pub(crate) fn visit<C>(&mut self, neighbor: usize) -> Result<(), FnxExecutionError<C>> {
        let authority = self
            .authorities
            .get_mut(neighbor)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        match self.phase {
            Phase::Authority(source) => *authority += self.hubs[source],
            Phase::Hub { vertex, sum } => {
                self.phase = Phase::Hub {
                    vertex,
                    sum: sum + *authority,
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

    pub(crate) fn end_row<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        self.phase = match self.phase {
            Phase::Authority(vertex) if vertex + 1 < self.nodes => Phase::Authority(vertex + 1),
            Phase::Authority(_) => Phase::AuthoritySum { at: 0, sum: 0.0 },
            Phase::Hub { vertex, sum } => {
                self.next_hubs[vertex] = if sum.is_finite() { sum } else { 0.0 };
                if vertex + 1 < self.nodes {
                    Phase::Hub {
                        vertex: vertex + 1,
                        sum: 0.0,
                    }
                } else {
                    Phase::HubSum { at: 0, sum: 0.0 }
                }
            }
            _ => return Err(FnxExecutionError::InvalidUpstreamResult),
        };
        Ok(())
    }

    /// One vertex or fixed-size phase transition. No fill, reduction, copy or
    /// final validation can run an unbounded pass between live checkpoints.
    pub(crate) fn scalar_step<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        self.phase = match self.phase {
            Phase::Initialize(at) if at < self.nodes => {
                self.hubs.push(if self.nodes == 1 {
                    1.0
                } else {
                    1.0 / self.nodes as f64
                });
                self.authorities
                    .push(if self.nodes == 1 { 1.0 } else { 0.0 });
                if self.nodes > 1 {
                    self.next_hubs.push(0.0);
                }
                Phase::Initialize(at + 1)
            }
            Phase::Initialize(_) if self.nodes <= 1 => {
                self.witness.nodes_touched = self.nodes;
                Phase::Complete
            }
            Phase::Initialize(_) => {
                self.iterations = 1;
                Phase::Reset(0)
            }
            Phase::Reset(at) if at < self.nodes => {
                self.authorities[at] = 0.0;
                // Every next_hubs entry is overwritten by its complete row.
                Phase::Reset(at + 1)
            }
            Phase::Reset(_) => Phase::Authority(0),
            Phase::AuthoritySum { at, sum } if at < self.nodes => {
                let value = &mut self.authorities[at];
                if !value.is_finite() {
                    *value = 0.0;
                }
                Phase::AuthoritySum {
                    at: at + 1,
                    sum: sum + *value,
                }
            }
            Phase::AuthoritySum { sum, .. } => Phase::AuthorityNormalize { at: 0, sum },
            Phase::AuthorityNormalize { at, sum } if at < self.nodes => {
                if sum > 0.0 {
                    self.authorities[at] /= sum;
                }
                Phase::AuthorityNormalize { at: at + 1, sum }
            }
            Phase::AuthorityNormalize { .. } => Phase::Hub {
                vertex: 0,
                sum: 0.0,
            },
            Phase::HubSum { at, sum } if at < self.nodes => Phase::HubSum {
                at: at + 1,
                sum: sum + self.next_hubs[at],
            },
            Phase::HubSum { sum, .. } => Phase::HubNormalize { at: 0, sum },
            Phase::HubNormalize { at, sum } if at < self.nodes => {
                if sum > 0.0 {
                    self.next_hubs[at] /= sum;
                }
                Phase::HubNormalize { at: at + 1, sum }
            }
            Phase::HubNormalize { .. } => Phase::Delta { at: 0, sum: 0.0 },
            Phase::Delta { at, sum } if at < self.nodes => Phase::Delta {
                at: at + 1,
                sum: sum + (self.next_hubs[at] - self.hubs[at]).abs(),
            },
            Phase::Delta { sum, .. } => {
                // Copy-free, with the exact bits of fnx's copy_from_slice.
                std::mem::swap(&mut self.hubs, &mut self.next_hubs);
                self.witness.nodes_touched =
                    self.nodes.saturating_mul(self.iterations).saturating_mul(2);
                if sum < TOLERANCE || self.iterations == MAX_ITERATIONS {
                    // fnx returns the last iterate at exhaustion; it does not
                    // have the checked non-convergence refusal of eigenvector.
                    Phase::FinalHubSum { at: 0, sum: 0.0 }
                } else {
                    self.iterations += 1;
                    Phase::Reset(0)
                }
            }
            Phase::FinalHubSum { at, sum } if at < self.nodes => Phase::FinalHubSum {
                at: at + 1,
                sum: sum + self.hubs[at],
            },
            Phase::FinalHubSum { sum, .. } => Phase::FinalHubNormalize { at: 0, sum },
            Phase::FinalHubNormalize { at, sum } if at < self.nodes => {
                self.hubs[at] = if sum > 0.0 {
                    self.hubs[at] / sum
                } else {
                    1.0 / self.nodes as f64
                };
                Phase::FinalHubNormalize { at: at + 1, sum }
            }
            Phase::FinalHubNormalize { .. } => Phase::FinalAuthoritySum { at: 0, sum: 0.0 },
            Phase::FinalAuthoritySum { at, sum } if at < self.nodes => Phase::FinalAuthoritySum {
                at: at + 1,
                sum: sum + self.authorities[at],
            },
            Phase::FinalAuthoritySum { sum, .. } => Phase::FinalAuthorityNormalize { at: 0, sum },
            Phase::FinalAuthorityNormalize { at, sum } if at < self.nodes => {
                self.authorities[at] = if sum > 0.0 {
                    self.authorities[at] / sum
                } else {
                    1.0 / self.nodes as f64
                };
                Phase::FinalAuthorityNormalize { at: at + 1, sum }
            }
            Phase::FinalAuthorityNormalize { .. } => Phase::Validate(0),
            Phase::Validate(at) if at < self.nodes => {
                if !self.hubs[at].is_finite()
                    || !self.authorities[at].is_finite()
                    || self.hubs[at] < 0.0
                    || self.authorities[at] < 0.0
                {
                    return Err(FnxExecutionError::InvalidNumericResult);
                }
                Phase::Validate(at + 1)
            }
            Phase::Validate(_) => Phase::Complete,
            Phase::Authority(_) | Phase::Hub { .. } => {
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
                values: KernelValues::HubsAndAuthorities {
                    hubs: self.hubs,
                    authorities: self.authorities,
                },
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
