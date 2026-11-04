//! Point-to-point BFS over the two already admitted CSR faces.
//!
//! Each direction advances complete distance layers. Before their first
//! intersection the two visited sets are disjoint. An edge into the opposite
//! set can only reach its unexpanded frontier: an earlier opposite layer
//! would already have discovered this edge's source. Consequently the first
//! intersection has distance `forward_depth + reverse_depth + 1`. The entire
//! discovering row is consumed before returning; no arbitrary interleaving
//! within a layer or floating-point distance is involved.
//!
//! This is deliberately a decoded-only kernel. Compressed providers retain
//! their FIFO's stronger source-error observation order, including fallible
//! rows before the target in the same layer. Neither face is copied here.

use crate::execute::{KernelOutput, KernelValues, admit, reserve};
use crate::{
    ComplexityWitness, FNX_METER_RUN, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView,
};
use fgdb_crypto::Hasher;
use fgdb_types::VId;
use std::collections::VecDeque;
use std::mem::size_of;

/// One direction change, recorded before expanding its selected layer.
/// Costs count vertices plus projected arcs, not time or CPU instructions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetBfsSwitch {
    pub reverse: bool,
    pub forward_depth: usize,
    pub reverse_depth: usize,
    pub forward_vertices: usize,
    pub reverse_vertices: usize,
    pub forward_arcs: usize,
    pub reverse_arcs: usize,
    /// Saving relative to fully expanding the other current frontier.
    pub benefit_lower: usize,
    pub benefit_upper: usize,
}

/// Version 1 starts forward and switches only when the active frontier's
/// `(vertices + arcs)` exceeds twice the opposite frontier's cost. A direction
/// lasts at least one complete layer (or the final discovering row). All
/// non-switching layers follow the pinned current-direction FIFO policy and
/// are reconstructible from the immutable projection and call.
///
/// A frozen frontier costs more than twice the selected one at each switch.
/// Before switching back, the active frontier must grow beyond twice that
/// frozen cost. Frozen costs therefore more than double between switches,
/// bounding this transcript by `usize::BITS + 1` records. Conversion needs no
/// graph rebuild: both bitmaps and CSR faces are already present. The margin
/// prices bookkeeping in this visit model; it is not a timing guarantee.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetBfsDecisionCard {
    pub policy_version: u16,
    pub fallback: &'static str,
    pub dwell_layers: usize,
    pub switch_cost_multiplier: usize,
    pub switches: Vec<TargetBfsSwitch>,
}

pub(crate) const KERNEL: &str = "fgdb-prism/target-bidirectional-csr-v1";
// Same-invocation kernel controls regressed at 16, 64 and 256 vertices;
// 1024 was the first measured branching/hub population with a gain. This
// conservative floor is not a complete-call latency guarantee or a claim
// about every reachable subgraph. Empty-face proofs need no frontier state.
const MIN_ASYMMETRIC_VERTICES: usize = 1024;

pub(crate) fn estimated_work(n: usize, arcs: usize, columns: usize) -> Option<usize> {
    // Include both bitmap initializations, frontier planning, queue/discovery
    // bookkeeping, complete row visits, bounded decision encoding and output.
    // The old FIFO remains available when this allowance does not fit.
    n.checked_mul(5)?
        .checked_add(arcs.checked_mul(2)?)?
        .checked_add((usize::BITS as usize + 1).checked_mul(2)?)?
        .checked_add(columns)?
        .checked_add(16)
}

pub(crate) fn eligible(
    graph: &SnapshotGraphView,
    source: VId,
    target: VId,
    cutoff: Option<usize>,
    columns: usize,
    allowance: usize,
) -> bool {
    #[cfg(test)]
    if tests::forward_only() {
        return false;
    }
    if source == target || cutoff.is_some_and(|limit| limit <= 1) {
        return false;
    }
    let (Some(source), Some(target)) = (graph.vertex_ordinal(source), graph.vertex_ordinal(target))
    else {
        return false;
    };
    let outgoing = graph.outgoing_offsets();
    let (incoming, _) = graph.incoming_face();
    let forward_arcs = outgoing[source + 1] - outgoing[source];
    let reverse_arcs = incoming[target + 1] - incoming[target];
    // An empty seed face proves unreachability. Otherwise opt in only when
    // the immutable degree counts already justify reversing the first layer.
    // Symmetric/narrow seeds keep the existing FIFO: complete-call calibration
    // is still required before broadening this conservative selection policy.
    let favorable = forward_arcs == 0
        || reverse_arcs == 0
        || (graph.node_count() >= MIN_ASYMMETRIC_VERTICES
            && forward_arcs
                .checked_add(1)
                .zip(reverse_arcs.checked_add(1))
                .is_some_and(|(forward, reverse)| more_than_twice(forward, reverse)));
    favorable
        && graph
            .outgoing_offsets()
            .last()
            .and_then(|&arcs| estimated_work(graph.node_count(), arcs, columns))
            .is_some_and(|work| work <= allowance)
}

fn more_than_twice(larger: usize, smaller: usize) -> bool {
    larger > smaller && larger - smaller > smaller
}

struct Frontier {
    visited: Vec<u64>,
    queue: VecDeque<usize>,
    growth_extra: usize,
    depth: usize,
    arcs: usize,
}

impl Frontier {
    fn new<C>(n: usize, meter: &mut impl FnxMeter<C>) -> Result<Self, FnxExecutionError<C>> {
        let words = n.div_ceil(u64::BITS as usize);
        let mut visited = reserve(words)?;
        while visited.len() < words {
            let run = FNX_METER_RUN.min(words - visited.len());
            meter.units(run).map_err(FnxExecutionError::Cancelled)?;
            visited.resize(visited.len() + run, 0);
        }
        Ok(Self {
            visited,
            queue: VecDeque::new(),
            growth_extra: 0,
            depth: 0,
            arcs: 0,
        })
    }

    fn contains(&self, vertex: usize) -> bool {
        self.visited[vertex / 64] & (1u64 << (vertex % 64)) != 0
    }

    fn discover<C>(&mut self, vertex: usize, degree: usize) -> Result<(), FnxExecutionError<C>> {
        // Grow only the live FIFO. Unlike the old append-only queue this does
        // not reserve or retain one usize for every graph vertex ever reached.
        if self.queue.len() == self.queue.capacity() {
            self.growth_extra = self.growth_extra.max(
                self.queue
                    .capacity()
                    .checked_mul(size_of::<usize>())
                    .ok_or(FnxExecutionError::SizeOverflow)?,
            );
        }
        // Allocator calls and their backing moves are synchronous. Live
        // checks bound graph arithmetic, not allocation latency. Charge a
        // conservative old-plus-new backing peak even if growth is in place.
        self.queue
            .try_reserve(1)
            .map_err(|_| FnxExecutionError::AllocationFailed)?;
        self.arcs = self
            .arcs
            .checked_add(degree)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        self.visited[vertex / 64] |= 1u64 << (vertex % 64);
        self.queue.push_back(vertex);
        Ok(())
    }

    fn cost<C>(&self) -> Result<usize, FnxExecutionError<C>> {
        self.queue
            .len()
            .checked_add(self.arcs)
            .ok_or(FnxExecutionError::SizeOverflow)
    }

    fn bytes<C>(&self) -> Result<usize, FnxExecutionError<C>> {
        self.visited
            .capacity()
            .checked_mul(size_of::<u64>())
            .and_then(|bytes| {
                self.queue
                    .capacity()
                    .checked_mul(size_of::<usize>())
                    .and_then(|queue| bytes.checked_add(queue))
            })
            .ok_or(FnxExecutionError::SizeOverflow)
    }
}

pub(crate) struct Run {
    pub(crate) output: KernelOutput,
    pub(crate) workspace: usize,
    pub(crate) decision: TargetBfsDecisionCard,
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    source: VId,
    target: VId,
    cutoff: Option<usize>,
    row_limit: usize,
    meter: &mut impl FnxMeter<C>,
) -> Result<Run, FnxExecutionError<C>> {
    let source = graph
        .vertex_ordinal(source)
        .ok_or(FnxExecutionError::UnknownSource(source))?;
    let target = graph
        .vertex_ordinal(target)
        .ok_or(FnxExecutionError::UnknownTarget(target))?;
    meter.unit().map_err(FnxExecutionError::Cancelled)?;
    let n = graph.node_count();
    let outgoing = graph.outgoing_offsets();
    let (incoming, predecessors) = graph.incoming_face();
    let mut decision = TargetBfsDecisionCard {
        policy_version: 1,
        fallback: "current_direction_fifo",
        dwell_layers: 1,
        switch_cost_multiplier: 2,
        switches: Vec::new(),
    };
    let mut witness = ComplexityWitness {
        algorithm: "shortest_path_length_target_bidirectional_csr".to_owned(),
        complexity_claim: "O(|V| + |E|) decoded traversal, two visited bitmaps and live frontiers; complete layers until first intersection".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    // The immutable degree directories prove these empty answers without
    // touching either face or allocating population-sized search state.
    if source == target
        || cutoff == Some(0)
        || outgoing[source] == outgoing[source + 1]
        || incoming[target] == incoming[target + 1]
    {
        let distance = (source == target).then_some(0);
        let rows = usize::from(distance.is_some());
        admit("result rows", rows, row_limit)?;
        witness.nodes_touched = rows;
        meter.unit().map_err(FnxExecutionError::Cancelled)?;
        return Ok(Run {
            output: KernelOutput {
                values: KernelValues::TargetHopDistance { target, distance },
                row_count: rows,
                witness,
            },
            workspace: 0,
            decision,
        });
    }
    let mut forward = Frontier::new(n, meter)?;
    let mut backward = Frontier::new(n, meter)?;
    meter.unit().map_err(FnxExecutionError::Cancelled)?;
    forward.discover(source, outgoing[source + 1] - outgoing[source])?;
    backward.discover(target, incoming[target + 1] - incoming[target])?;
    witness.queue_peak = 2;
    let mut reverse = false;
    let mut evidence_growth_extra = 0;
    let mut distance = (source == target).then_some(0);
    'search: while distance.is_none() && !forward.queue.is_empty() && !backward.queue.is_empty() {
        meter.unit().map_err(FnxExecutionError::Cancelled)?;
        let depth = forward
            .depth
            .checked_add(backward.depth)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        if cutoff.is_some_and(|limit| depth >= limit) {
            break;
        }
        let forward_cost = forward.cost()?;
        let reverse_cost = backward.cost()?;
        let (active_cost, other_cost) = if reverse {
            (reverse_cost, forward_cost)
        } else {
            (forward_cost, reverse_cost)
        };
        if more_than_twice(active_cost, other_cost) {
            if decision.switches.len() >= usize::BITS as usize + 1 {
                return Err(FnxExecutionError::InvalidUpstreamResult);
            }
            if decision.switches.len() == decision.switches.capacity() {
                evidence_growth_extra = evidence_growth_extra.max(
                    decision
                        .switches
                        .capacity()
                        .checked_mul(size_of::<TargetBfsSwitch>())
                        .ok_or(FnxExecutionError::SizeOverflow)?,
                );
            }
            decision
                .switches
                .try_reserve(1)
                .map_err(|_| FnxExecutionError::AllocationFailed)?;
            reverse = !reverse;
            decision.switches.push(TargetBfsSwitch {
                reverse,
                forward_depth: forward.depth,
                reverse_depth: backward.depth,
                forward_vertices: forward.queue.len(),
                reverse_vertices: backward.queue.len(),
                forward_arcs: forward.arcs,
                reverse_arcs: backward.arcs,
                benefit_lower: active_cost - other_cost,
                benefit_upper: active_cost,
            });
        }
        let (active, other, offsets) = if reverse {
            (&mut backward, &forward, incoming)
        } else {
            (&mut forward, &backward, outgoing)
        };
        let vertices = active.queue.len();
        // `arcs` now accumulates only the next frontier. Its complete current
        // cost was recorded above, before any pop or newly discovered vertex.
        active.arcs = 0;
        for _ in 0..vertices {
            meter.unit().map_err(FnxExecutionError::Cancelled)?;
            let vertex = active
                .queue
                .pop_front()
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
            witness.nodes_touched = witness
                .nodes_touched
                .checked_add(1)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            let row = if reverse {
                &predecessors[incoming[vertex]..incoming[vertex + 1]]
            } else {
                graph
                    .neighbors_indices(vertex)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?
            };
            for batch in row.chunks(FNX_METER_RUN) {
                // All ordinals and row boundaries belong to the validated
                // immutable projection. Each live run is at most 64 probes.
                meter
                    .units(batch.len())
                    .map_err(FnxExecutionError::Cancelled)?;
                witness.edges_scanned = witness
                    .edges_scanned
                    .checked_add(batch.len())
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                for &neighbor in batch {
                    if other.contains(neighbor) {
                        distance = Some(
                            depth
                                .checked_add(1)
                                .ok_or(FnxExecutionError::SizeOverflow)?,
                        );
                    } else if distance.is_none() && !active.contains(neighbor) {
                        active.discover(neighbor, offsets[neighbor + 1] - offsets[neighbor])?;
                        witness.queue_peak = witness.queue_peak.max(
                            active
                                .queue
                                .len()
                                .checked_add(other.queue.len())
                                .ok_or(FnxExecutionError::SizeOverflow)?,
                        );
                    }
                }
            }
            if distance.is_some() {
                break 'search;
            }
        }
        active.depth = active
            .depth
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
    }
    let rows = usize::from(distance.is_some());
    admit("result rows", rows, row_limit)?;
    // Capacities never shrink and only one backing allocation grows at once.
    // The final sum plus the largest replaced backing is a conservative peak
    // bound, including an allocator that must hold old and new blocks at once.
    let workspace = forward
        .bytes::<C>()?
        .checked_add(backward.bytes::<C>()?)
        .and_then(|bytes| {
            decision
                .switches
                .capacity()
                .checked_mul(size_of::<TargetBfsSwitch>())
                .and_then(|evidence| bytes.checked_add(evidence))
        })
        .and_then(|bytes| {
            bytes.checked_add(
                forward
                    .growth_extra
                    .max(backward.growth_extra)
                    .max(evidence_growth_extra),
            )
        })
        .ok_or(FnxExecutionError::SizeOverflow)?;
    meter.unit().map_err(FnxExecutionError::Cancelled)?;
    Ok(Run {
        output: KernelOutput {
            values: KernelValues::TargetHopDistance { target, distance },
            row_count: rows,
            witness,
        },
        workspace,
        decision,
    })
}

pub(crate) fn hash_decision<C>(
    hash: &mut Hasher,
    card: Option<&TargetBfsDecisionCard>,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<(), C> {
    let Some(card) = card else {
        hash.update(&[0]);
        return Ok(());
    };
    hash.update(&[1]);
    hash.update(&card.policy_version.to_le_bytes());
    hash.update(&(card.fallback.len() as u128).to_le_bytes());
    hash.update(card.fallback.as_bytes());
    for number in [
        card.dwell_layers,
        card.switch_cost_multiplier,
        card.switches.len(),
    ] {
        hash.update(&(number as u128).to_le_bytes());
    }
    for change in &card.switches {
        checkpoint()?;
        hash.update(&[u8::from(change.reverse)]);
        for number in [
            change.forward_depth,
            change.reverse_depth,
            change.forward_vertices,
            change.reverse_vertices,
            change.forward_arcs,
            change.reverse_arcs,
            change.benefit_lower,
            change.benefit_upper,
        ] {
            hash.update(&(number as u128).to_le_bytes());
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "target_bidirectional_tests.rs"]
pub(crate) mod tests;
