//! Signed-weight distances with exact dyadic relaxation and cycle detection.
//!
//! Finite binary64 edge weights enter the existing 34-limb exact accumulator.
//! Canonical vertex/neighbor sweeps compare those sums without rounding. Only
//! the completed finite optimum is rounded to binary64, once. Thus a small
//! negative cycle cannot disappear behind an epsilon or a large prefix cost.
//! One state machine serves decoded and fallible compressed row drivers.

use crate::execute::{KernelOutput, KernelValues, admit, reserve};
use crate::{ComplexityWitness, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView};
use fgdb_types::{CanonicalF64, ExactBinary64Sum, VId};
use std::cmp::Ordering;
use std::mem::size_of;

pub(crate) const KERNEL: &str = "fgdb-prism/bellman-ford-exact-dyadic-sweeps-v1";
const LIMBS: usize = 34;

pub(crate) fn workspace<C>(n: usize) -> Result<usize, FnxExecutionError<C>> {
    n.checked_mul(size_of::<Option<ExactBinary64Sum>>() + size_of::<Option<f64>>())
        .ok_or(FnxExecutionError::SizeOverflow)
}

/// At most n complete arc passes, including the final nonmutating cycle
/// check. Every candidate extends a value produced by an earlier relaxation,
/// so this also bounds its number of finite summands to the accumulator's
/// documented 2^64-input domain, independently of negative cycles.
fn relaxation_bound<C>(n: usize, arcs: usize) -> Result<usize, FnxExecutionError<C>> {
    let bound = n.checked_mul(arcs).ok_or(FnxExecutionError::SizeOverflow)?;
    u64::try_from(bound).map_err(|_| FnxExecutionError::SizeOverflow)?;
    Ok(bound)
}

pub(crate) fn estimated_work<C>(
    n: usize,
    arcs: usize,
    pass: usize,
    columns: usize,
) -> Result<usize, FnxExecutionError<C>> {
    relaxation_bound::<C>(n, arcs)?;
    // Copy, exact addition and signed comparison each visit at most 34 limbs.
    // Row base copies, population steps, result rounding and encoding are
    // priced separately. `pass` includes raw compressed history and lookups.
    let sweep = arcs
        .checked_mul(3 * LIMBS + 8)
        .and_then(|edges| {
            n.checked_mul(LIMBS + 8)
                .and_then(|nodes| edges.checked_add(nodes))
        })
        .and_then(|work| work.checked_add(pass))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    let output_factor = columns
        .checked_add(2 * LIMBS + 12)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    n.checked_mul(sweep)
        .and_then(|work| {
            n.checked_mul(output_factor)
                .and_then(|output| work.checked_add(output))
        })
        .and_then(|work| work.checked_add(64))
        .ok_or(FnxExecutionError::SizeOverflow)
}

fn reserved<T, C>(
    n: usize,
    live: &mut usize,
    limit: usize,
) -> Result<Vec<T>, FnxExecutionError<C>> {
    let requested = n
        .checked_mul(size_of::<T>())
        .and_then(|bytes| live.checked_add(bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    admit("kernel workspace bytes", requested, limit)?;
    let values = reserve::<T, C>(n)?;
    let actual = values
        .capacity()
        .checked_mul(size_of::<T>())
        .and_then(|bytes| live.checked_add(bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Scalar,
    Open(usize),
    Row(usize),
    Complete,
}

#[derive(Clone, Copy)]
enum Phase {
    Initialize,
    Select(usize),
    Row(usize),
    Convert,
    Complete,
}

pub(crate) struct Distances {
    distances: Vec<Option<ExactBinary64Sum>>,
    rounded: Vec<Option<f64>>,
    base: Option<ExactBinary64Sum>,
    nodes: usize,
    source: usize,
    round: usize,
    changed: bool,
    discovered: usize,
    row_limit: usize,
    relaxations: usize,
    relaxation_limit: usize,
    peak: usize,
    phase: Phase,
    witness: ComplexityWitness,
}

impl Distances {
    pub(crate) fn new<C>(
        nodes: usize,
        source: usize,
        arcs: usize,
        workspace_limit: usize,
        row_limit: usize,
    ) -> Result<Self, FnxExecutionError<C>> {
        if source >= nodes {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        let relaxation_limit = relaxation_bound(nodes, arcs)?;
        admit("result rows", 1, row_limit)?;
        admit(
            "kernel workspace bytes",
            workspace::<C>(nodes)?,
            workspace_limit,
        )?;
        let mut peak = 0;
        let distances = reserved(nodes, &mut peak, workspace_limit)?;
        let rounded = reserved(nodes, &mut peak, workspace_limit)?;
        Ok(Self {
            distances,
            rounded,
            base: None,
            nodes,
            source,
            round: 0,
            changed: false,
            discovered: 1,
            row_limit,
            relaxations: 0,
            relaxation_limit,
            peak,
            phase: Phase::Initialize,
            witness: ComplexityWitness {
                algorithm: "single_source_bellman_ford_exact_dyadic_sweeps".to_owned(),
                complexity_claim: "O(|V|*(|V|+|E|)) fixed-width exact arithmetic and row visits; O(|V|) workspace; final rounding once".to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            },
        })
    }

    pub(crate) fn workspace_bytes(&self) -> usize {
        self.peak
    }

    pub(crate) fn row_count(&self) -> usize {
        self.discovered
    }

    pub(crate) fn step<C>(&mut self) -> Result<Step, FnxExecutionError<C>> {
        match self.phase {
            Phase::Initialize => {
                if self.distances.len() == self.nodes {
                    self.distances[self.source] = Some(ExactBinary64Sum::new());
                    self.phase = Phase::Select(0);
                } else {
                    self.distances.push(None);
                }
            }
            Phase::Select(vertex) => {
                if vertex == self.nodes {
                    if self.round == self.nodes - 1 || !self.changed {
                        self.phase = Phase::Convert;
                    } else {
                        self.round += 1;
                        self.changed = false;
                        self.phase = Phase::Select(0);
                    }
                } else if let Some(base) = &self.distances[vertex] {
                    self.base = Some(base.clone());
                    self.witness.nodes_touched = self
                        .witness
                        .nodes_touched
                        .checked_add(1)
                        .ok_or(FnxExecutionError::SizeOverflow)?;
                    self.phase = Phase::Row(vertex);
                    return Ok(Step::Open(vertex));
                } else {
                    self.phase = Phase::Select(vertex + 1);
                }
            }
            Phase::Row(vertex) => return Ok(Step::Row(vertex)),
            Phase::Convert => {
                let at = self.rounded.len();
                if at == self.nodes {
                    self.phase = Phase::Complete;
                } else {
                    let value = self.distances[at]
                        .as_ref()
                        .map(|distance| distance.sum().get());
                    if value.is_some_and(|value| !value.is_finite()) {
                        return Err(FnxExecutionError::NonFiniteDistanceSum);
                    }
                    self.rounded.push(value);
                }
            }
            Phase::Complete => return Ok(Step::Complete),
        }
        Ok(Step::Scalar)
    }

    pub(crate) fn visit<C>(
        &mut self,
        target: usize,
        weight: f64,
    ) -> Result<(), FnxExecutionError<C>> {
        if !matches!(self.phase, Phase::Row(_)) || target >= self.nodes {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        if !weight.is_finite() {
            return Err(FnxExecutionError::InvalidNumericResult);
        }
        self.relaxations = self
            .relaxations
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        if self.relaxations > self.relaxation_limit {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        self.witness.edges_scanned = self.relaxations;
        let mut candidate = self
            .base
            .as_ref()
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?
            .clone();
        candidate.add(CanonicalF64::new(weight));
        let improves = match &self.distances[target] {
            None => true,
            Some(existing) => {
                candidate
                    .cmp_finite(existing)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?
                    == Ordering::Less
            }
        };
        if improves {
            if self.round == self.nodes - 1 {
                return Err(FnxExecutionError::ReachableNegativeCycle);
            }
            if self.distances[target].is_none() {
                let discovered = self
                    .discovered
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                admit("result rows", discovered, self.row_limit)?;
                self.discovered = discovered;
            }
            self.distances[target] = Some(candidate);
            self.changed = true;
        }
        Ok(())
    }

    pub(crate) fn end_row<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        let Phase::Row(vertex) = self.phase else {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        };
        self.base = None;
        self.phase = Phase::Select(vertex + 1);
        Ok(())
    }

    pub(crate) fn finish<C>(self) -> Result<KernelOutput, FnxExecutionError<C>> {
        if !matches!(self.phase, Phase::Complete) || self.rounded.len() != self.nodes {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        Ok(KernelOutput {
            values: KernelValues::WeightedDistances(self.rounded),
            row_count: self.discovered,
            witness: self.witness,
        })
    }
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    source: VId,
    row_limit: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    let source = graph
        .vertex_ordinal(source)
        .ok_or(FnxExecutionError::UnknownSource(source))?;
    let arcs = *graph
        .outgoing_offsets()
        .last()
        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
    let mut state = Distances::new(graph.node_count(), source, arcs, usize::MAX, row_limit)?;
    let mut position = 0;
    loop {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        match state.step()? {
            Step::Scalar => {}
            Step::Open(_) => position = 0,
            Step::Row(vertex) => {
                let (neighbors, weights) = graph
                    .projected_row(vertex)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                if let Some(&neighbor) = neighbors.get(position) {
                    let weight = *weights
                        .get(position)
                        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                    position += 1;
                    state.visit(neighbor, weight)?;
                } else {
                    state.end_row()?;
                }
            }
            Step::Complete => break,
        }
    }
    let peak = state.workspace_bytes();
    Ok((state.finish()?, peak))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    #[test]
    fn exact_state_work_and_workspace_bounds_refuse_arithmetic_overflow() {
        assert_eq!(workspace::<Infallible>(0).unwrap(), 0);
        assert_eq!(
            workspace::<Infallible>(7).unwrap(),
            7 * (size_of::<Option<ExactBinary64Sum>>() + size_of::<Option<f64>>())
        );
        assert!(matches!(
            workspace::<Infallible>(usize::MAX),
            Err(FnxExecutionError::SizeOverflow)
        ));
        for (nodes, arcs, pass, columns) in [
            (usize::MAX, 2, 0, 2),
            (2, usize::MAX, 0, 2),
            (2, 1, usize::MAX, 2),
            (2, 1, 0, usize::MAX),
        ] {
            assert!(matches!(
                estimated_work::<Infallible>(nodes, arcs, pass, columns),
                Err(FnxExecutionError::SizeOverflow)
            ));
        }
        assert!(matches!(
            Distances::new::<Infallible>(usize::MAX, 0, 2, usize::MAX, usize::MAX),
            Err(FnxExecutionError::SizeOverflow)
        ));
    }
}
