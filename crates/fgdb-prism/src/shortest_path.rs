//! Weighted SSSP over the existing decoded projection, without copying edges.
//!
//! The indexed min-heap contains at most one entry per unsettled vertex. This
//! avoids the O(m) duplicate queue of lazy-deletion Dijkstra implementations.
//! Equal costs use FIFO discovery from canonical VId-ordered rows. Only finite,
//! nonnegative projected weights are accepted, including zero-weight cycles.

use crate::execute::{admit, reserve};
use crate::{
    ComplexityWitness, FNX_METER_RUN, FnxBindError, FnxBindErrorKind, FnxExecutionError,
    FnxExecutionLimits, FnxMeter, GraphView, SnapshotGraphView,
};
use fgdb_types::VId;

/// The pinned foundation's relaxation threshold (not a convergence tolerance).
pub const FNX_DIJKSTRA_EPSILON: f64 = 1e-12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DijkstraComparison {
    /// Accept every representable improvement. This is the primitive default.
    Strict = 0,
    /// Match fnx's `candidate < previous - 1e-12` relaxation and FIFO ties.
    FnxEpsilon = 1,
}

/// Validated weighted-distance parameters. The cutoff is inclusive and is a
/// cost, not a hop count. Negative zero is normalized for canonical binding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DijkstraOptions {
    source: VId,
    cutoff: Option<f64>,
    comparison: DijkstraComparison,
}

impl DijkstraOptions {
    pub fn new(source: VId, cutoff: Option<f64>) -> Result<Self, FnxBindError> {
        if cutoff.is_some_and(|value| !value.is_finite() || value < 0.0) {
            return Err(FnxBindError {
                at: 0,
                kind: FnxBindErrorKind::InvalidArgument(
                    "cutoff must be a finite nonnegative cost or NULL",
                ),
            });
        }
        Ok(Self {
            source,
            cutoff: cutoff.map(|value| if value == 0.0 { 0.0 } else { value }),
            comparison: DijkstraComparison::Strict,
        })
    }

    pub const fn source(self) -> VId {
        self.source
    }

    pub const fn cutoff(self) -> Option<f64> {
        self.cutoff
    }

    pub const fn with_comparison(mut self, comparison: DijkstraComparison) -> Self {
        self.comparison = comparison;
        self
    }

    pub const fn comparison(self) -> DijkstraComparison {
        self.comparison
    }
}

/// Kernel output indexed by the input projection's snapshot-local ordinals.
/// None means unreachable or beyond the cutoff, never an infinite distance.
/// This primitive output is not a query certificate or an authorization grant.
#[derive(Clone, Debug, PartialEq)]
pub struct DijkstraOutput {
    pub distances: Vec<Option<f64>>,
    pub row_count: usize,
    /// Finalized vertices, relaxation edges, and maximum active heap entries.
    /// Preflight weight validation and output conversion are separate work.
    pub witness: ComplexityWitness,
}

/// Execute the in-core primitive with work/row admission and cooperative
/// cancellation. The iteration allowance is irrelevant to this finite kernel.
/// Allocations and the caller's checkpoint callback are not preemptible.
pub fn dijkstra<C>(
    graph: &SnapshotGraphView,
    options: DijkstraOptions,
    limits: FnxExecutionLimits,
    mut checkpoint: impl FnxMeter<C>,
) -> Result<DijkstraOutput, FnxExecutionError<C>> {
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    let n = graph.node_count();
    let mut arcs = 0usize;
    for node in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let row = graph
            .neighbors_indices(node)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        arcs = arcs
            .checked_add(row.len())
            .ok_or(FnxExecutionError::SizeOverflow)?;
    }
    admit(
        "estimated work",
        estimated_work(n, arcs)?,
        limits.max_estimated_work,
    )?;
    workspace_bytes::<C>(n)?;
    run(graph, options, limits.max_result_rows, &mut checkpoint)
}

/// Conservative admission model including validation and both heap sift
/// directions. It is a work model, not an instruction count or wall-clock bound.
pub(crate) fn estimated_work<C>(n: usize, arcs: usize) -> Result<usize, FnxExecutionError<C>> {
    let height = (usize::BITS - n.leading_zeros()) as usize;
    let factor = height
        .checked_mul(2)
        .and_then(|value| value.checked_add(3))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    n.checked_add(arcs)
        .and_then(|value| value.checked_mul(factor))
        .ok_or(FnxExecutionError::SizeOverflow)
}

pub(crate) fn workspace_bytes<C>(n: usize) -> Result<usize, FnxExecutionError<C>> {
    // Heap entries, inverse positions, final distances, overflow reachability.
    let per_vertex = std::mem::size_of::<Entry>()
        + std::mem::size_of::<usize>()
        + std::mem::size_of::<Option<f64>>()
        + std::mem::size_of::<bool>();
    n.checked_mul(per_vertex)
        .ok_or(FnxExecutionError::SizeOverflow)
}

#[derive(Clone, Copy)]
pub(crate) struct Entry {
    pub(crate) node: usize,
    pub(crate) cost: f64,
    sequence: u64,
}

/// Shared queue mechanics, not a graph adapter. Both execution paths retain
/// one entry per unsettled vertex and the same FIFO tie/relaxation policy.
/// A caller must discard the heap after any error, including cancellation.
pub(crate) struct IndexedHeap {
    entries: Vec<Entry>,
    positions: Vec<usize>,
    sequence: u64,
    comparison: DijkstraComparison,
}

/// Population initialization owned by its driver. Each step writes at most
/// one inverse-position slot; no partially initialized heap can escape.
pub(crate) struct HeapInit {
    heap: IndexedHeap,
    population: usize,
}

impl HeapInit {
    pub(crate) fn step(&mut self) -> bool {
        if self.heap.positions.len() < self.population {
            self.heap.positions.push(usize::MAX);
        }
        self.heap.positions.len() == self.population
    }

    pub(crate) fn finish(self) -> Option<IndexedHeap> {
        (self.heap.positions.len() == self.population).then_some(self.heap)
    }
}

/// Exclusive borrow spanning one heap repair. A step performs at most one
/// sift comparison/swap. The popped entry is withheld until repair completes.
/// As with a cancelled synchronous operation, abandoning a pending repair
/// requires discarding the owning heap; the query future owns both together.
pub(crate) struct HeapMutation<'a> {
    heap: &'a mut IndexedHeap,
    position: usize,
    upwards: bool,
    output: Option<Entry>,
    complete: bool,
}

impl HeapMutation<'_> {
    pub(crate) fn step(&mut self) -> Option<Option<Entry>> {
        if !self.complete {
            let next = if self.upwards {
                self.heap.sift_up_step(self.position)
            } else {
                self.heap.sift_down_step(self.position)
            };
            match next {
                Some(position) => self.position = position,
                None => self.complete = true,
            }
        }
        self.complete.then_some(self.output)
    }

    /// Complete a synchronous repair with its exact step count. The moving
    /// entry sees only untouched ancestors/descendants during the lookahead,
    /// so it chooses the same path as `step`. That path has at most one step
    /// per binary-heap level; no graph-sized work or scratch is prepaid.
    fn finish_metered<C>(
        mut self,
        checkpoint: &mut impl FnxMeter<C>,
    ) -> Result<Option<Entry>, HeapError<C>> {
        if self.complete {
            // Empty/singleton pops and rejected offers still have one step.
            checkpoint.unit().map_err(HeapError::Cancelled)?;
            return Ok(self.output);
        }
        let entry = self.heap.entries[self.position];
        let mut end = self.position;
        let mut swaps = 0usize;
        if self.upwards {
            while end > 0 {
                let parent = (end - 1) / 2;
                if !IndexedHeap::before(entry, self.heap.entries[parent]) {
                    break;
                }
                end = parent;
                swaps += 1;
            }
        } else {
            while end < self.heap.entries.len() / 2 {
                let left = 2 * end + 1;
                let right = left + 1;
                let child = if right < self.heap.entries.len()
                    && IndexedHeap::before(self.heap.entries[right], self.heap.entries[left])
                {
                    right
                } else {
                    left
                };
                if !IndexedHeap::before(self.heap.entries[child], entry) {
                    break;
                }
                end = child;
                swaps += 1;
            }
        }
        // The scalar driver charges once for every swap, then once more for
        // the terminal comparison. Keep the refusing unit before any replay.
        let mut unpaid = swaps + 1;
        while unpaid != 0 {
            let run = unpaid.min(FNX_METER_RUN);
            checkpoint.units(run).map_err(HeapError::Cancelled)?;
            unpaid -= run;
        }
        if self.upwards {
            while self.position != end {
                let parent = (self.position - 1) / 2;
                self.heap.swap(self.position, parent);
                self.position = parent;
            }
        } else {
            // A one-based binary-heap index encodes the complete root path.
            // Its prefixes recover each descendant without another compare.
            let path = end + 1;
            for remaining in (0..swaps).rev() {
                let child = (path >> remaining) - 1;
                self.heap.swap(self.position, child);
                self.position = child;
            }
        }
        Ok(self.output)
    }
}

#[derive(Debug)]
pub(crate) enum HeapError<C> {
    Cancelled(C),
    SizeOverflow,
    AllocationFailed,
    InvalidOrdinal,
}

impl<C> From<HeapError<C>> for FnxExecutionError<C> {
    fn from(error: HeapError<C>) -> Self {
        match error {
            HeapError::Cancelled(error) => Self::Cancelled(error),
            HeapError::SizeOverflow => Self::SizeOverflow,
            HeapError::AllocationFailed => Self::AllocationFailed,
            HeapError::InvalidOrdinal => Self::InvalidUpstreamResult,
        }
    }
}

impl IndexedHeap {
    pub(crate) fn new<C>(
        n: usize,
        comparison: DijkstraComparison,
        checkpoint: &mut impl FnMut() -> Result<(), C>,
    ) -> Result<Self, HeapError<C>> {
        checkpoint().map_err(HeapError::Cancelled)?;
        let mut initialization = Self::initialize::<C>(n, comparison)?;
        loop {
            checkpoint().map_err(HeapError::Cancelled)?;
            if initialization.step() {
                return initialization.finish().ok_or(HeapError::InvalidOrdinal);
            }
        }
    }

    /// The synchronous driver's same initialization transcript in bounded
    /// runs. Both reservations still follow the leading checkpoint; filling
    /// their already reserved storage cannot fail partway through a run.
    /// An empty heap retains the scalar driver's one terminal step.
    fn new_metered<C>(
        n: usize,
        comparison: DijkstraComparison,
        checkpoint: &mut impl FnxMeter<C>,
    ) -> Result<Self, HeapError<C>> {
        #[cfg(test)]
        if tests::scalar_passes() {
            return Self::new(n, comparison, &mut || checkpoint.unit());
        }
        checkpoint.unit().map_err(HeapError::Cancelled)?;
        let mut initialization = Self::initialize::<C>(n, comparison)?;
        if n == 0 {
            checkpoint.unit().map_err(HeapError::Cancelled)?;
        }
        while initialization.heap.positions.len() < n {
            let start = initialization.heap.positions.len();
            let count = (n - start).min(FNX_METER_RUN);
            checkpoint.units(count).map_err(HeapError::Cancelled)?;
            initialization
                .heap
                .positions
                .resize(start + count, usize::MAX);
        }
        initialization.finish().ok_or(HeapError::InvalidOrdinal)
    }

    /// Reserve the unchanged two backing stores. Allocation is indivisible;
    /// filling the inverse directory is driven separately, one slot at a time.
    pub(crate) fn initialize<C>(
        n: usize,
        comparison: DijkstraComparison,
    ) -> Result<HeapInit, HeapError<C>> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(n)
            .map_err(|_| HeapError::AllocationFailed)?;
        let mut positions = Vec::new();
        positions
            .try_reserve_exact(n)
            .map_err(|_| HeapError::AllocationFailed)?;
        Ok(HeapInit {
            heap: Self {
                entries,
                positions,
                sequence: 0,
                comparison,
            },
            population: n,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn contains(&self, node: usize) -> Option<bool> {
        self.positions
            .get(node)
            .map(|&position| position != usize::MAX)
    }

    fn before(left: Entry, right: Entry) -> bool {
        left.cost
            .total_cmp(&right.cost)
            .then(left.sequence.cmp(&right.sequence))
            .is_lt()
    }

    fn swap(&mut self, left: usize, right: usize) {
        self.entries.swap(left, right);
        self.positions[self.entries[left].node] = left;
        self.positions[self.entries[right].node] = right;
    }

    pub(crate) fn offer<C>(
        &mut self,
        node: usize,
        cost: f64,
        checkpoint: &mut impl FnMut() -> Result<(), C>,
    ) -> Result<(), HeapError<C>> {
        checkpoint().map_err(HeapError::Cancelled)?;
        let mut mutation = self.offer_steps::<C>(node, cost)?;
        loop {
            checkpoint().map_err(HeapError::Cancelled)?;
            if mutation.step().is_some() {
                return Ok(());
            }
        }
    }

    fn offer_metered<C>(
        &mut self,
        node: usize,
        cost: f64,
        checkpoint: &mut impl FnxMeter<C>,
    ) -> Result<(), HeapError<C>> {
        #[cfg(test)]
        if tests::scalar_heap() {
            return self.offer(node, cost, &mut || checkpoint.unit());
        }
        checkpoint.unit().map_err(HeapError::Cancelled)?;
        self.offer_steps::<C>(node, cost)?
            .finish_metered(checkpoint)?;
        Ok(())
    }

    /// Begin the same strict/epsilon relaxation used by both scheduling modes.
    /// The driver must checkpoint before this mutation and before EACH step.
    pub(crate) fn offer_steps<C>(
        &mut self,
        node: usize,
        cost: f64,
    ) -> Result<HeapMutation<'_>, HeapError<C>> {
        let mut position = *self.positions.get(node).ok_or(HeapError::InvalidOrdinal)?;
        if position != usize::MAX {
            let previous = self.entries[position].cost;
            let threshold = match self.comparison {
                DijkstraComparison::Strict => previous,
                DijkstraComparison::FnxEpsilon => previous - FNX_DIJKSTRA_EPSILON,
            };
            if cost >= threshold {
                return Ok(HeapMutation {
                    heap: self,
                    position: 0,
                    upwards: true,
                    output: None,
                    complete: true,
                });
            }
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(HeapError::SizeOverflow)?;
        let entry = Entry {
            node,
            cost,
            sequence: self.sequence,
        };
        if position == usize::MAX {
            position = self.entries.len();
            self.positions[node] = position;
            self.entries.push(entry);
        } else {
            self.entries[position] = entry;
        }
        Ok(HeapMutation {
            heap: self,
            position,
            upwards: true,
            output: None,
            complete: false,
        })
    }

    fn sift_up_step(&mut self, position: usize) -> Option<usize> {
        if position > 0 {
            let parent = (position - 1) / 2;
            if Self::before(self.entries[position], self.entries[parent]) {
                self.swap(position, parent);
                return Some(parent);
            }
        }
        None
    }

    pub(crate) fn pop<C>(
        &mut self,
        checkpoint: &mut impl FnMut() -> Result<(), C>,
    ) -> Result<Option<Entry>, HeapError<C>> {
        checkpoint().map_err(HeapError::Cancelled)?;
        let mut mutation = self.pop_steps();
        loop {
            checkpoint().map_err(HeapError::Cancelled)?;
            if let Some(output) = mutation.step() {
                return Ok(output);
            }
        }
    }

    fn pop_metered<C>(
        &mut self,
        checkpoint: &mut impl FnxMeter<C>,
    ) -> Result<Option<Entry>, HeapError<C>> {
        #[cfg(test)]
        if tests::scalar_heap() {
            return self.pop(&mut || checkpoint.unit());
        }
        checkpoint.unit().map_err(HeapError::Cancelled)?;
        self.pop_steps().finish_metered(checkpoint)
    }

    pub(crate) fn pop_steps(&mut self) -> HeapMutation<'_> {
        let Some(last) = self.entries.pop() else {
            return HeapMutation {
                heap: self,
                position: 0,
                upwards: false,
                output: None,
                complete: true,
            };
        };
        if self.entries.is_empty() {
            self.positions[last.node] = usize::MAX;
            return HeapMutation {
                heap: self,
                position: 0,
                upwards: false,
                output: Some(last),
                complete: true,
            };
        }
        let first = std::mem::replace(&mut self.entries[0], last);
        self.positions[first.node] = usize::MAX;
        self.positions[last.node] = 0;
        HeapMutation {
            heap: self,
            position: 0,
            upwards: false,
            output: Some(first),
            complete: false,
        }
    }

    fn sift_down_step(&mut self, position: usize) -> Option<usize> {
        // A non-leaf has position < len / 2, so 2*position+1 cannot overflow.
        if position < self.entries.len() / 2 {
            let left = 2 * position + 1;
            let right = left + 1;
            let child = if right < self.entries.len()
                && Self::before(self.entries[right], self.entries[left])
            {
                right
            } else {
                left
            };
            if Self::before(self.entries[child], self.entries[position]) {
                self.swap(position, child);
                return Some(child);
            }
        }
        None
    }
}

#[cfg(test)]
fn validate_weights_incumbent<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(), FnxExecutionError<C>> {
    let n = graph.node_count();
    for node in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let (_, weights) = graph
            .projected_row(node)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for &weight in weights {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            if !weight.is_finite() {
                return Err(FnxExecutionError::InvalidNumericResult);
            }
            if weight < 0.0 {
                return Err(FnxExecutionError::NegativeWeight);
            }
        }
    }
    Ok(())
}

fn validate_weights<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(), FnxExecutionError<C>> {
    // This test-only incumbent lets paired measurements exercise the complete
    // call, including admission and typed results, in the same invocation.
    #[cfg(test)]
    if tests::scan_incumbent_weights() {
        return validate_weights_incumbent(graph, checkpoint);
    }

    // Construction validates finiteness after reduction and records the first
    // negative in final CSR order. Reject the entire incompatible projection,
    // including an unreachable component or a weight beyond the cutoff.
    let negative = graph.first_negative_weight_visit();
    let mut remaining = match negative {
        Some(visit) => visit,
        None => graph
            .node_count()
            .checked_add(
                graph
                    .outgoing_offsets()
                    .last()
                    .copied()
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?,
            )
            .ok_or(FnxExecutionError::SizeOverflow)?,
    };
    // Retain every logical unit and the first refusal point. Meters may batch
    // this immutable validation; ordinary callbacks still receive each unit.
    // Bound runs so bulk cancellation checks cannot be deferred indefinitely.
    while remaining != 0 {
        let run = remaining.min(FNX_METER_RUN);
        checkpoint
            .units(run)
            .map_err(FnxExecutionError::Cancelled)?;
        remaining -= run;
    }
    if negative.is_some() {
        return Err(FnxExecutionError::NegativeWeight);
    }
    Ok(())
}

/// Initialize the two already reserved output buffers without a host call
/// for every vertex. The scalar transcript is one unit for both writes.
fn initialize_output<C>(
    n: usize,
    distances: &mut Vec<Option<f64>>,
    overflowed: &mut Vec<bool>,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(), FnxExecutionError<C>> {
    #[cfg(test)]
    if tests::scalar_passes() {
        for _ in 0..n {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            distances.push(None);
            overflowed.push(false);
        }
        return Ok(());
    }
    while distances.len() < n {
        let start = distances.len();
        let count = (n - start).min(FNX_METER_RUN);
        checkpoint
            .units(count)
            .map_err(FnxExecutionError::Cancelled)?;
        distances.resize(start + count, None);
        overflowed.resize(start + count, false);
    }
    Ok(())
}

/// An overflow-only path is a numeric refusal, not an unreachable vertex.
/// Inspect at most one run ahead, then charge only through the first such
/// vertex. A cancellation at that unit wins; a later one cannot hide the
/// numeric refusal. The lookahead reads immutable, query-local vectors.
fn validate_overflow<C>(
    distances: &[Option<f64>],
    overflowed: &[bool],
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(), FnxExecutionError<C>> {
    #[cfg(test)]
    if tests::scalar_passes() {
        for node in 0..distances.len() {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            if overflowed[node] && distances[node].is_none() {
                return Err(FnxExecutionError::InvalidNumericResult);
            }
        }
        return Ok(());
    }
    for first in (0..distances.len()).step_by(FNX_METER_RUN) {
        let count = (distances.len() - first).min(FNX_METER_RUN);
        let refused =
            (first..first + count).find(|&node| overflowed[node] && distances[node].is_none());
        let charged = refused.map_or(count, |node| node - first + 1);
        checkpoint
            .units(charged)
            .map_err(FnxExecutionError::Cancelled)?;
        if refused.is_some() {
            return Err(FnxExecutionError::InvalidNumericResult);
        }
    }
    Ok(())
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    options: DijkstraOptions,
    row_limit: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<DijkstraOutput, FnxExecutionError<C>> {
    let source = graph
        .vertex_ordinal(options.source)
        .ok_or(FnxExecutionError::UnknownSource(options.source))?;
    admit("result rows", 1, row_limit)?;
    let n = graph.node_count();
    validate_weights(graph, checkpoint)?;
    let mut distances = reserve(n)?;
    let mut overflowed = reserve(n)?;
    let mut heap = IndexedHeap::new_metered(n, options.comparison, checkpoint)?;
    initialize_output(n, &mut distances, &mut overflowed, checkpoint)?;
    heap.offer_metered(source, 0.0, checkpoint)?;
    let mut discovered = 1usize;
    let mut witness = ComplexityWitness {
        algorithm: "single_source_dijkstra_indexed_heap".to_owned(),
        complexity_claim: "O((|V| + |E|) * log(1 + |V|))".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 1,
    };
    loop {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let Some(Entry { node, cost, .. }) = heap.pop_metered(checkpoint)? else {
            break;
        };
        distances[node] = Some(cost);
        witness.nodes_touched = witness
            .nodes_touched
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let (targets, weights) = graph
            .projected_row(node)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for (&target, &weight) in targets.iter().zip(weights) {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            witness.edges_scanned = witness
                .edges_scanned
                .checked_add(1)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            if distances[target].is_some() {
                continue;
            }
            let candidate = cost + weight;
            if !candidate.is_finite() {
                // An overflowing alternative must not reject a later finite
                // shortest path. With a finite cutoff it is simply out of range.
                if options.cutoff.is_none() {
                    overflowed[target] = true;
                }
                continue;
            }
            if options.cutoff.is_some_and(|cutoff| candidate > cutoff) {
                continue;
            }
            if !heap
                .contains(target)
                .ok_or(FnxExecutionError::InvalidUpstreamResult)?
            {
                let requested = discovered
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                admit("result rows", requested, row_limit)?;
                discovered = requested;
            }
            heap.offer_metered(target, candidate, checkpoint)?;
            witness.queue_peak = witness.queue_peak.max(heap.len());
        }
    }
    validate_overflow(&distances, &overflowed, checkpoint)?;
    Ok(DijkstraOutput {
        distances,
        row_count: discovered,
        witness,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Directedness, FnxCallSpec, FnxResult, FnxValue, ParallelEdgePolicy, ProjectionEdge,
        ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding,
    };
    use fgdb_types::{CommitSeq, EId, ids::ObjectId};
    use fnx_classes::{Graph, digraph::DiGraph};
    use std::cell::Cell;
    use std::convert::Infallible;

    thread_local! {
        static INCUMBENT_WEIGHT_SCAN: Cell<bool> = const { Cell::new(false) };
        static INCUMBENT_HEAP: Cell<bool> = const { Cell::new(false) };
        static INCUMBENT_PASSES: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn scan_incumbent_weights() -> bool {
        INCUMBENT_WEIGHT_SCAN.with(Cell::get)
    }

    pub(super) fn scalar_heap() -> bool {
        INCUMBENT_HEAP.with(Cell::get)
    }

    pub(super) fn scalar_passes() -> bool {
        INCUMBENT_PASSES.with(Cell::get)
    }

    fn with_scalar_passes<R>(incumbent: bool, work: impl FnOnce() -> R) -> R {
        struct Reset(bool);
        impl Drop for Reset {
            fn drop(&mut self) {
                INCUMBENT_PASSES.with(|mode| mode.set(self.0));
            }
        }
        let _reset = Reset(INCUMBENT_PASSES.with(|mode| mode.replace(incumbent)));
        work()
    }

    fn with_scalar_heap<R>(incumbent: bool, work: impl FnOnce() -> R) -> R {
        struct Reset(bool);
        impl Drop for Reset {
            fn drop(&mut self) {
                INCUMBENT_HEAP.with(|mode| mode.set(self.0));
            }
        }
        let _reset = Reset(INCUMBENT_HEAP.with(|mode| mode.replace(incumbent)));
        work()
    }

    fn with_weight_scan<R>(incumbent: bool, work: impl FnOnce() -> R) -> R {
        struct Reset(bool);
        impl Drop for Reset {
            fn drop(&mut self) {
                INCUMBENT_WEIGHT_SCAN.with(|mode| mode.set(self.0));
            }
        }
        let _reset = Reset(INCUMBENT_WEIGHT_SCAN.with(|mode| mode.replace(incumbent)));
        work()
    }

    #[derive(Default, Debug)]
    struct Trace {
        seen: usize,
        polls: usize,
        largest_run: usize,
        runs: usize,
    }

    struct ExactMeter<'a> {
        trace: &'a mut Trace,
        stop: Option<usize>,
    }

    impl FnxMeter<&'static str> for ExactMeter<'_> {
        fn unit(&mut self) -> Result<(), &'static str> {
            self.units(1)
        }

        fn units(&mut self, n: usize) -> Result<(), &'static str> {
            assert!((1..=FNX_METER_RUN).contains(&n));
            self.trace.runs += 1;
            self.trace.largest_run = self.trace.largest_run.max(n);
            if let Some(stop) = self.stop
                && self.trace.seen + n >= stop
            {
                self.trace.seen = stop;
                return Err("stop");
            }
            self.trace.seen += n;
            Ok(())
        }

        fn poll(&mut self) -> Result<(), &'static str> {
            self.trace.polls += 1;
            Ok(())
        }
    }

    fn full_call(
        view: &SnapshotGraphView,
        call: &FnxCallSpec,
        cap: FnxExecutionLimits,
        incumbent: bool,
        stop: Option<usize>,
    ) -> (Result<FnxResult, String>, Trace) {
        let mut trace = Trace::default();
        let result = with_weight_scan(incumbent, || {
            call.execute(
                view,
                cap,
                ExactMeter {
                    trace: &mut trace,
                    stop,
                },
            )
        })
        .map_err(|error| format!("{error:?}"));
        (result, trace)
    }

    fn graph(
        n: usize,
        edges: &[(usize, usize, f64)],
        direction: Directedness,
    ) -> SnapshotGraphView {
        let vertices: Vec<_> = (0..n)
            .map(|i| VId(u128::MAX - n as u128 + i as u128))
            .collect();
        let edges: Vec<_> = edges
            .iter()
            .enumerate()
            .map(|(id, &(s, t, weight))| ProjectionEdge {
                eid: EId(id as u128),
                source: vertices[s],
                target: vertices[t],
                weight,
            })
            .collect();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([7; 32]),
                as_of: CommitSeq(9),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: direction,
                parallel_edges: ParallelEdgePolicy::Minimum,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: n,
                max_input_edges: edges.len(),
                max_adjacency_entries: edges.len() * 2,
                max_workspace_bytes: 1 << 24,
            },
        )
        .unwrap()
    }
    fn limits() -> FnxExecutionLimits {
        FnxExecutionLimits {
            max_iterations: 0,
            max_result_rows: 100,
            max_estimated_work: 1 << 24,
        }
    }
    fn options(graph: &SnapshotGraphView, source: usize, cutoff: Option<f64>) -> DijkstraOptions {
        DijkstraOptions::new(graph.vertex_id(source).unwrap(), cutoff).unwrap()
    }
    fn execute(graph: &SnapshotGraphView, source: usize, cutoff: Option<f64>) -> DijkstraOutput {
        dijkstra(graph, options(graph, source, cutoff), limits(), || {
            Ok::<(), Infallible>(())
        })
        .unwrap()
    }

    #[test]
    fn all_small_weighted_topologies_match_independent_dense_floyd_warshall() {
        for mask in 0u16..512 {
            let edges: Vec<_> = (0..9)
                .filter(|bit| mask & (1 << bit) != 0)
                .map(|bit| (bit / 3, bit % 3, [0.0, 0.5, 3.0, 7.0][bit % 4]))
                .collect();
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let view = graph(3, &edges, direction);
                let mut dense = [[f64::INFINITY; 3]; 3];
                for i in 0..3 {
                    dense[i][i] = 0.0;
                    let (targets, weights) = view.projected_row(i).unwrap();
                    for (&j, &weight) in targets.iter().zip(weights) {
                        dense[i][j] = dense[i][j].min(weight);
                    }
                }
                for k in 0..3 {
                    for i in 0..3 {
                        for j in 0..3 {
                            dense[i][j] = dense[i][j].min(dense[i][k] + dense[k][j]);
                        }
                    }
                }
                for source in 0..3 {
                    for cutoff in [None, Some(0.0), Some(0.5), Some(3.0)] {
                        let actual = execute(&view, source, cutoff);
                        let expected: Vec<_> = dense[source]
                            .iter()
                            .copied()
                            .map(|distance| {
                                (distance.is_finite()
                                    && cutoff.is_none_or(|limit| distance <= limit))
                                .then_some(distance)
                            })
                            .collect();
                        assert_eq!(
                            actual.distances, expected,
                            "mask={mask} {direction:?} source={source} cutoff={cutoff:?}"
                        );
                        assert_eq!(
                            actual.row_count,
                            expected.iter().filter(|value| value.is_some()).count()
                        );
                        assert_eq!(actual.witness.nodes_touched, actual.row_count);
                        assert!(actual.witness.queue_peak <= 3);
                    }
                }
            }
        }
    }

    #[test]
    fn all_small_unit_topologies_match_pinned_standalone_fnx() {
        for mask in 0u16..512 {
            let edges: Vec<_> = (0..9)
                .filter(|bit| mask & (1 << bit) != 0)
                .map(|bit| (bit / 3, bit % 3, 1.0))
                .collect();
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let view = graph(3, &edges, direction);
                let mut directed = DiGraph::strict();
                let mut undirected = Graph::strict();
                for name in view.nodes_ordered() {
                    let _ = directed.add_node(name);
                    let _ = undirected.add_node(name);
                }
                for source in 0..3 {
                    for &target in view.neighbors_indices(source).unwrap() {
                        let s = view.get_node_name(source).unwrap();
                        let t = view.get_node_name(target).unwrap();
                        directed.add_edge(s, t).unwrap();
                        if source <= target {
                            undirected.add_edge(s, t).unwrap();
                        }
                    }
                }
                for source in 0..3 {
                    let name = view.get_node_name(source).unwrap();
                    let oracle = if direction == Directedness::Undirected {
                        fnx_algorithms::single_source_dijkstra_path_length(
                            &undirected,
                            name,
                            "weight",
                        )
                    } else {
                        fnx_algorithms::single_source_dijkstra_path_length_directed(
                            &directed, name, "weight",
                        )
                    };
                    let mut expected = vec![None; 3];
                    for (node, distance) in oracle {
                        expected[view.get_node_index(&node).unwrap()] = Some(distance);
                    }
                    assert_eq!(execute(&view, source, None).distances, expected);
                }
            }
        }
    }

    #[test]
    fn cutoff_keeps_zero_cost_closure_and_minimum_parallel_edge_law() {
        let view = graph(
            5,
            &[
                (0, 1, 20.0),
                (0, 1, 2.0),
                (1, 2, 0.0),
                (2, 1, 0.0),
                (2, 3, 0.5),
            ],
            Directedness::Directed,
        );
        assert_eq!(
            execute(&view, 0, Some(2.0)).distances,
            vec![Some(0.0), Some(2.0), Some(2.0), None, None]
        );
        assert_eq!(
            execute(&view, 1, Some(0.0)).distances,
            vec![None, Some(0.0), Some(0.0), None, None]
        );
    }

    #[test]
    fn overflow_alternatives_do_not_poison_finite_paths_or_cutoffs() {
        let overflow = [(0, 1, f64::MAX * 0.75), (1, 3, f64::MAX * 0.75)];
        let view = graph(4, &overflow, Directedness::Directed);
        assert!(matches!(
            dijkstra(&view, options(&view, 0, None), limits(), || Ok::<
                (),
                Infallible,
            >(
                ()
            )),
            Err(FnxExecutionError::InvalidNumericResult)
        ));
        assert_eq!(execute(&view, 0, Some(f64::MAX)).row_count, 2);
        let mut finite = overflow.to_vec();
        finite.extend([(0, 2, f64::MAX * 0.875), (2, 3, 0.0)]);
        let view = graph(4, &finite, Directedness::Directed);
        assert_eq!(execute(&view, 0, None).distances[3], Some(f64::MAX * 0.875));
    }

    #[test]
    fn source_numeric_and_admission_refusals_are_explicit() {
        for cutoff in [f64::NAN, f64::INFINITY, -1.0] {
            assert!(DijkstraOptions::new(VId(1), Some(cutoff)).is_err());
        }
        assert_eq!(
            DijkstraOptions::new(VId(1), Some(-0.0))
                .unwrap()
                .cutoff()
                .unwrap()
                .to_bits(),
            0
        );
        let view = graph(4, &[(2, 3, -1.0)], Directedness::Directed);
        assert!(matches!(
            dijkstra(&view, options(&view, 0, Some(0.0)), limits(), || Ok::<
                (),
                Infallible,
            >(
                ()
            )),
            Err(FnxExecutionError::NegativeWeight)
        ));
        let view = graph(4, &[], Directedness::Directed);
        assert!(matches!(
            dijkstra(
                &view,
                DijkstraOptions::new(VId(0), None).unwrap(),
                limits(),
                || Ok::<(), Infallible>(())
            ),
            Err(FnxExecutionError::UnknownSource(VId(0)))
        ));
        let tight = FnxExecutionLimits {
            max_result_rows: 1,
            ..limits()
        };
        assert_eq!(
            dijkstra(
                &view,
                options(&view, 0, None),
                tight,
                || Ok::<(), Infallible>(())
            )
            .unwrap()
            .row_count,
            1
        );
        for cap in [
            FnxExecutionLimits {
                max_result_rows: 0,
                ..limits()
            },
            FnxExecutionLimits {
                max_estimated_work: 0,
                ..limits()
            },
        ] {
            assert!(matches!(
                dijkstra(
                    &view,
                    options(&view, 0, None),
                    cap,
                    || Ok::<(), Infallible>(())
                ),
                Err(FnxExecutionError::LimitExceeded { .. })
            ));
        }
        assert!(estimated_work::<Infallible>(usize::MAX, 1).is_err());
        assert!(workspace_bytes::<Infallible>(usize::MAX).is_err());
    }

    #[test]
    fn every_checkpoint_including_heap_sifts_cancels_without_partial_results() {
        let view = graph(
            5,
            &[
                (0, 1, 8.0),
                (0, 2, 4.0),
                (0, 3, 2.0),
                (3, 1, 1.0),
                (1, 2, 0.0),
                (2, 4, 1.0),
            ],
            Directedness::Directed,
        );
        let mut total = 0;
        dijkstra(&view, options(&view, 0, None), limits(), || {
            total += 1;
            Ok::<(), &'static str>(())
        })
        .unwrap();
        for stop in 1..=total {
            let mut count = 0;
            let result = dijkstra(&view, options(&view, 0, None), limits(), || {
                count += 1;
                if count == stop { Err("cancel") } else { Ok(()) }
            });
            assert!(matches!(
                result,
                Err(FnxExecutionError::Cancelled("cancel"))
            ));
            assert_eq!(count, stop);
        }
    }

    #[test]
    fn cached_validation_preserves_full_call_results_and_every_refusal_prefix() {
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            for last_weight in [1.0, -1.0] {
                // The last edge is unreachable from source zero. Its rejection
                // occurs beyond the first run even with a zero-cost cutoff.
                let view = graph(
                    80,
                    &[
                        (0, 1, 3.0),
                        (0, 2, 1.0),
                        (2, 1, 1.0),
                        (2, 2, 0.0),
                        (70, 79, last_weight),
                    ],
                    direction,
                );
                for cutoff in [None, Some(0.0)] {
                    let call =
                        FnxCallSpec::single_source_dijkstra_path_length(options(&view, 0, cutoff));
                    for cap in [
                        limits(),
                        FnxExecutionLimits {
                            max_result_rows: 0,
                            ..limits()
                        },
                        FnxExecutionLimits {
                            max_result_rows: 1,
                            ..limits()
                        },
                        FnxExecutionLimits {
                            max_estimated_work: 0,
                            ..limits()
                        },
                    ] {
                        let (expected, before) = full_call(&view, &call, cap, true, None);
                        let (actual, after) = full_call(&view, &call, cap, false, None);
                        assert_eq!(actual, expected);
                        assert_eq!((after.seen, after.polls), (before.seen, before.polls));
                        for stop in 1..=before.seen + 1 {
                            let (expected, before) = full_call(&view, &call, cap, true, Some(stop));
                            let (actual, after) = full_call(&view, &call, cap, false, Some(stop));
                            assert_eq!(actual, expected, "{direction:?} {cutoff:?} stop={stop}");
                            assert_eq!((after.seen, after.polls), (before.seen, before.polls));
                        }
                    }
                    // The blanket callback meter retains every original call,
                    // including all units inside an optimized validation run.
                    let mut totals = [0usize; 2];
                    for (index, incumbent) in [true, false].into_iter().enumerate() {
                        let result = with_weight_scan(incumbent, || {
                            call.execute(&view, limits(), || {
                                totals[index] += 1;
                                Ok::<(), &'static str>(())
                            })
                        })
                        .map_err(|error| format!("{error:?}"));
                        assert_eq!(result, full_call(&view, &call, limits(), true, None).0);
                    }
                    assert_eq!(totals[0], totals[1]);
                    let (_, trace) = full_call(&view, &call, limits(), false, None);
                    assert_eq!(trace.largest_run, FNX_METER_RUN);
                }
            }
        }
    }

    #[test]
    fn metered_heap_initialization_keeps_empty_terminal_and_all_refusal_prefixes() {
        for n in [
            0,
            1,
            FNX_METER_RUN - 1,
            FNX_METER_RUN,
            FNX_METER_RUN + 1,
            129,
            usize::MAX,
        ] {
            let attempt = |incumbent, stop| {
                let mut trace = Trace::default();
                let result = with_scalar_passes(incumbent, || {
                    IndexedHeap::new_metered(
                        n,
                        DijkstraComparison::Strict,
                        &mut ExactMeter {
                            trace: &mut trace,
                            stop,
                        },
                    )
                })
                .map(|heap| {
                    (
                        heap.entries.len(),
                        heap.positions,
                        heap.sequence,
                        heap.comparison,
                    )
                })
                .map_err(|error| format!("{error:?}"));
                (result, trace)
            };
            let (expected, before) = attempt(true, None);
            let (actual, after) = attempt(false, None);
            assert_eq!(actual, expected, "n={n}");
            assert_eq!((after.seen, after.polls), (before.seen, before.polls));
            if n == usize::MAX {
                assert!(actual.is_err());
                assert_eq!(after.seen, 1, "allocation follows the leading unit");
            } else {
                assert_eq!(after.seen, 1 + n.max(1));
            }
            for stop in 1..=before.seen + 1 {
                let (expected, before) = attempt(true, Some(stop));
                let (actual, after) = attempt(false, Some(stop));
                assert_eq!(actual, expected, "n={n} stop={stop}");
                assert_eq!((after.seen, after.polls), (before.seen, before.polls));
            }
        }
    }

    #[test]
    fn overflow_runs_charge_only_through_first_unresolved_numeric_refusal() {
        for n in [0, 2 * FNX_METER_RUN + 3] {
            for refused in [
                None,
                Some(0),
                Some(FNX_METER_RUN - 1),
                Some(FNX_METER_RUN),
                Some(FNX_METER_RUN + 1),
                Some(2 * FNX_METER_RUN + 2),
            ] {
                if refused.is_some_and(|index| index >= n) {
                    continue;
                }
                let mut distances = vec![None; n];
                let mut overflowed = vec![false; n];
                if n != 0 {
                    // A finite alternative resolves this earlier overflow.
                    distances[0] = Some(f64::MAX);
                    overflowed[0] = true;
                }
                if let Some(index) = refused {
                    distances[index] = None;
                    overflowed[index] = true;
                    // A later overflow must not extend the charged prefix.
                    overflowed[n - 1] = true;
                }
                let attempt = |incumbent, stop| {
                    let mut trace = Trace::default();
                    let result = with_scalar_passes(incumbent, || {
                        validate_overflow(
                            &distances,
                            &overflowed,
                            &mut ExactMeter {
                                trace: &mut trace,
                                stop,
                            },
                        )
                    })
                    .map_err(|error| format!("{error:?}"));
                    (result, trace)
                };
                let (expected, before) = attempt(true, None);
                let (actual, after) = attempt(false, None);
                assert_eq!(actual, expected);
                assert_eq!(after.seen, refused.map_or(n, |index| index + 1));
                assert_eq!((after.seen, after.polls), (before.seen, before.polls));
                for stop in 1..=before.seen + 1 {
                    let (expected, before) = attempt(true, Some(stop));
                    let (actual, after) = attempt(false, Some(stop));
                    assert_eq!(actual, expected, "refused={refused:?} stop={stop}");
                    assert_eq!((after.seen, after.polls), (before.seen, before.polls));
                }
            }
        }
    }

    fn compare_linear_passes(
        view: &SnapshotGraphView,
        call: &FnxCallSpec,
        cap: FnxExecutionLimits,
    ) -> bool {
        let compare = |stop| {
            let (expected, before) =
                with_scalar_passes(true, || full_call(view, call, cap, false, stop));
            let (actual, after) =
                with_scalar_passes(false, || full_call(view, call, cap, false, stop));
            assert_eq!(actual, expected, "stop={stop:?}");
            assert_eq!((after.seen, after.polls), (before.seen, before.polls));
            (before, after)
        };
        let (before, after) = compare(None);
        for stop in 1..=before.seen + 1 {
            compare(Some(stop));
        }
        after.runs < before.runs
    }

    #[test]
    fn metered_linear_passes_preserve_complete_calls_and_every_refusal_prefix() {
        let n = 2 * FNX_METER_RUN + 3;
        let edges = [
            (0, 1, 8.0),
            (0, 2, 1.0),
            (2, 1, 1.0),
            (1, 3, 0.0),
            (3, 0, 0.0),
            (0, 0, 0.0),
            (n - 2, n - 1, 0.25),
        ];
        let mut observed_batch = false;
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let view = graph(n, &edges, direction);
            for comparison in [DijkstraComparison::Strict, DijkstraComparison::FnxEpsilon] {
                for source in [0, n - 1] {
                    for cutoff in [None, Some(0.0)] {
                        let call = FnxCallSpec::single_source_dijkstra_path_length(
                            options(&view, source, cutoff).with_comparison(comparison),
                        );
                        for max_result_rows in [0, 1, n] {
                            observed_batch |= compare_linear_passes(
                                &view,
                                &call,
                                FnxExecutionLimits {
                                    max_result_rows,
                                    ..limits()
                                },
                            );
                        }
                    }
                }
            }
        }
        assert!(
            observed_batch,
            "the complete call must reduce host invocations"
        );
    }

    #[test]
    fn metered_linear_passes_preserve_overflow_alternatives_and_cutoffs() {
        let n = 2 * FNX_METER_RUN + 3;
        for target in [FNX_METER_RUN - 1, FNX_METER_RUN, FNX_METER_RUN + 1] {
            for finite_alternative in [false, true] {
                let mut edges = vec![(0, 1, f64::MAX * 0.75), (1, target, f64::MAX * 0.75)];
                if finite_alternative {
                    // Node 1 first overflows the target's distance; a later
                    // settled node then supplies a representable alternative.
                    edges.extend([(0, 2, f64::MAX * 0.9), (2, target, 0.0)]);
                }
                let view = graph(n, &edges, Directedness::Directed);
                for cutoff in [None, Some(f64::MAX)] {
                    let call =
                        FnxCallSpec::single_source_dijkstra_path_length(options(&view, 0, cutoff));
                    compare_linear_passes(
                        &view,
                        &call,
                        FnxExecutionLimits {
                            max_result_rows: n,
                            ..limits()
                        },
                    );
                }
            }
        }
    }

    #[test]
    fn metered_heap_preserves_complete_calls_and_every_refusal_prefix() {
        let mut edges: Vec<_> = (1..13)
            .map(|target| (0, target, 2.0 + (target % 5) as f64))
            .collect();
        for source in 1..13 {
            for (offset, weight) in [(1, 0.25), (2, 1.0), (3, 5e-13)] {
                edges.push((source, 1 + (source - 1 + offset) % 12, weight));
            }
        }
        edges.extend([(0, 0, 0.0), (13, 14, 1.0)]);
        let mut observed_batch = false;
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let view = graph(15, &edges, direction);
            for comparison in [DijkstraComparison::Strict, DijkstraComparison::FnxEpsilon] {
                for source in [0, 14] {
                    for cutoff in [None, Some(0.0), Some(4.0)] {
                        let call = FnxCallSpec::single_source_dijkstra_path_length(
                            options(&view, source, cutoff).with_comparison(comparison),
                        );
                        for max_result_rows in [0, 1, 5, 15] {
                            let cap = FnxExecutionLimits {
                                max_result_rows,
                                ..limits()
                            };
                            let compare = |stop| {
                                let (expected, before) = with_scalar_heap(true, || {
                                    full_call(&view, &call, cap, false, stop)
                                });
                                let (actual, after) = with_scalar_heap(false, || {
                                    full_call(&view, &call, cap, false, stop)
                                });
                                assert_eq!(
                                    actual, expected,
                                    "{direction:?} {comparison:?} {source} {cutoff:?} rows={max_result_rows} stop={stop:?}"
                                );
                                assert_eq!((after.seen, after.polls), (before.seen, before.polls));
                                (before, after)
                            };
                            let (before, after) = compare(None);
                            observed_batch |= after.runs < before.runs;
                            for stop in 1..=before.seen + 1 {
                                compare(Some(stop));
                            }
                        }
                    }
                }
            }
        }
        assert!(
            observed_batch,
            "the complete call must exercise a bulk repair"
        );
    }

    #[test]
    fn metered_heap_keeps_terminal_steps_and_leading_error_precedence() {
        for case in 0..5 {
            for stop in [None, Some(1), Some(2), Some(3), Some(4)] {
                let execute = |incumbent| {
                    let mut heap =
                        IndexedHeap::new(2, DijkstraComparison::Strict, &mut || Ok::<_, ()>(()))
                            .unwrap();
                    if case != 4 {
                        heap.offer(0, 1.0, &mut || Ok::<_, ()>(())).unwrap();
                    }
                    heap.sequence = u64::MAX;
                    let mut trace = Trace::default();
                    let result = with_scalar_heap(incumbent, || {
                        let mut meter = ExactMeter {
                            trace: &mut trace,
                            stop,
                        };
                        match case {
                            // Invalid ordinal and accepted-offer sequence
                            // overflow must follow only the leading unit.
                            0 => heap.offer_metered(2, 1.0, &mut meter).map(|_| None),
                            1 => heap.offer_metered(1, 1.0, &mut meter).map(|_| None),
                            // A rejected offer must not inspect the exhausted
                            // sequence. It still charges its terminal step.
                            2 => heap.offer_metered(0, 2.0, &mut meter).map(|_| None),
                            // Singleton and empty pop also retain that step.
                            _ => heap.pop_metered(&mut meter),
                        }
                    })
                    .map(|entry| {
                        entry.map(|entry| (entry.node, entry.cost.to_bits(), entry.sequence))
                    })
                    .map_err(|error| format!("{error:?}"));
                    (result, trace.seen)
                };
                assert_eq!(execute(false), execute(true), "case={case} stop={stop:?}");
                if stop.is_none() {
                    assert_eq!(execute(false).1, if case < 2 { 1 } else { 2 });
                }
            }
        }
    }

    #[test]
    #[ignore = "manual same-invocation complete Prism-call performance comparison"]
    fn cached_validation_paired_full_call_performance() {
        paired_full_call_performance(false);
    }

    #[test]
    #[ignore = "manual same-invocation complete Prism-call performance comparison"]
    fn linear_passes_paired_full_call_performance() {
        paired_full_call_performance(true);
    }

    fn paired_full_call_performance(linear_passes: bool) {
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        struct Counting<'a>(&'a mut usize);
        impl FnxMeter<Infallible> for Counting<'_> {
            fn unit(&mut self) -> Result<(), Infallible> {
                *self.0 += 1;
                Ok(())
            }
            fn units(&mut self, n: usize) -> Result<(), Infallible> {
                *self.0 += n;
                Ok(())
            }
            fn poll(&mut self) -> Result<(), Infallible> {
                Ok(())
            }
        }

        fn measure(
            view: &SnapshotGraphView,
            calls: &[FnxCallSpec],
            cap: FnxExecutionLimits,
            incumbent: bool,
            linear_passes: bool,
        ) -> (Duration, usize, u64) {
            with_weight_scan(!linear_passes && incumbent, || {
                with_scalar_passes(linear_passes && incumbent, || {
                    let mut units = 0;
                    let mut consumed = 0u64;
                    let start = Instant::now();
                    for _ in 0..8 {
                        for call in calls {
                            let result = call
                                .execute(black_box(view), cap, Counting(&mut units))
                                .unwrap();
                            for row in &result.rows {
                                for value in row {
                                    consumed = consumed.rotate_left(7)
                                        ^ match value {
                                            FnxValue::Vertex(vertex) => {
                                                vertex.0 as u64 ^ (vertex.0 >> 64) as u64
                                            }
                                            FnxValue::Float(value) | FnxValue::Score(value) => {
                                                value.to_bits()
                                            }
                                            FnxValue::Integer(value) => *value,
                                        };
                                }
                            }
                            black_box(&result.certificate);
                            black_box(consumed);
                            // Result destruction is included in the measured call.
                        }
                    }
                    (start.elapsed(), units, consumed)
                })
            })
        }

        let n = 4096;
        let cap = FnxExecutionLimits {
            max_result_rows: n,
            ..limits()
        };
        let mut disconnected: Vec<_> = (0..31).map(|v| (v, v + 1, 1.0)).collect();
        for source in 32..n {
            for offset in 1..=32 {
                let target = 32 + (source - 32 + offset) % (n - 32);
                disconnected.push((source, target, (offset % 7 + 1) as f64));
            }
        }
        let chain: Vec<_> = (0..n - 1).map(|v| (v, v + 1, 1.0)).collect();
        let mut hub: Vec<_> = (1..n).map(|v| (0, v, 1.0)).collect();
        hub.extend((1..n - 1).map(|v| (v, v + 1, 0.5)));
        for (name, edges, sources) in [
            ("small_source_component", disconnected, [0, 8, 16, 31]),
            ("road_chain", chain, [0, n / 3, 2 * n / 3, n - 1]),
            ("hub", hub, [0, 1, n / 2, n - 1]),
            ("isolates", Vec::new(), [0, n / 3, 2 * n / 3, n - 1]),
        ] {
            let view = graph(n, &edges, Directedness::Directed);
            let calls: Vec<_> = sources
                .into_iter()
                .map(|source| {
                    FnxCallSpec::single_source_dijkstra_path_length(options(&view, source, None))
                })
                .collect();
            for call in &calls {
                let (old, old_trace) = with_scalar_passes(linear_passes, || {
                    full_call(&view, call, cap, !linear_passes, None)
                });
                let (new, new_trace) = full_call(&view, call, cap, false, None);
                assert_eq!(new, old);
                assert_eq!(new_trace.seen, old_trace.seen);
            }
            // Warm both complete paths, then alternate order in each pair.
            assert_eq!(
                measure(&view, &calls, cap, true, linear_passes).1,
                measure(&view, &calls, cap, false, linear_passes).1
            );
            let mut incumbent = Vec::new();
            let mut cached = Vec::new();
            for pair in 0..15 {
                let mut results = [(Duration::ZERO, 0, 0); 2];
                for mode in if pair % 2 == 0 { [0, 1] } else { [1, 0] } {
                    results[mode] = measure(&view, &calls, cap, mode == 0, linear_passes);
                }
                assert_eq!((results[0].1, results[0].2), (results[1].1, results[1].2));
                eprintln!(
                    "sssp_pair control={} fixture={name} pair={pair} incumbent_ns={} optimized_ns={} logical_units={} consumed={}",
                    if linear_passes {
                        "linear_passes"
                    } else {
                        "weight_validation"
                    },
                    results[0].0.as_nanos(),
                    results[1].0.as_nanos(),
                    results[0].1,
                    results[0].2,
                );
                incumbent.push(results[0].0.as_nanos());
                cached.push(results[1].0.as_nanos());
            }
            incumbent.sort_unstable();
            cached.sort_unstable();
            eprintln!(
                "{name}: vertices={n} arcs={} calls_per_sample={} incumbent_ns[min/median/max]={}/{}/{} optimized_ns[min/median/max]={}/{}/{} median_speedup={:.3}x",
                edges.len(),
                calls.len() * 8,
                incumbent[0],
                incumbent[7],
                incumbent[14],
                cached[0],
                cached[7],
                cached[14],
                incumbent[7] as f64 / cached[7] as f64,
            );
        }
    }

    fn check_inverse(heap: &IndexedHeap) {
        let mut present = vec![false; heap.positions.len()];
        for (position, entry) in heap.entries.iter().enumerate() {
            assert!(
                !present[entry.node],
                "queue must not retain duplicate vertices"
            );
            present[entry.node] = true;
            assert_eq!(heap.positions[entry.node], position);
        }
        for (node, &present) in present.iter().enumerate() {
            assert_eq!(heap.contains(node), Some(present));
        }
    }

    #[test]
    fn incremental_heap_initialization_never_exposes_a_partial_directory() {
        for n in [0, 1, 2, 17, 256] {
            for stop in 0..=n {
                let mut init =
                    IndexedHeap::initialize::<()>(n, DijkstraComparison::Strict).unwrap();
                for completed in 1..=stop {
                    assert_eq!(init.step(), completed == n);
                    assert_eq!(init.heap.positions.len(), completed);
                }
                let heap = init.finish();
                assert_eq!(heap.is_some(), stop == n);
                if let Some(heap) = heap {
                    assert_eq!(heap.positions, vec![usize::MAX; n]);
                    assert_eq!(heap.len(), 0);
                }
            }
        }
        assert!(matches!(
            IndexedHeap::initialize::<()>(usize::MAX, DijkstraComparison::Strict),
            Err(HeapError::AllocationFailed)
        ));
    }

    #[test]
    fn every_heap_step_preserves_inverse_positions_and_matches_a_dense_queue() {
        for comparison in [DijkstraComparison::Strict, DijkstraComparison::FnxEpsilon] {
            for n in [1, 2, 3, 17, 65] {
                let mut init = IndexedHeap::initialize::<()>(n, comparison).unwrap();
                while !init.step() {}
                let mut stepped = init.finish().unwrap();
                let mut synchronous =
                    IndexedHeap::new(n, comparison, &mut || Ok::<_, ()>(())).unwrap();
                let mut metered = IndexedHeap::new(n, comparison, &mut || Ok::<_, ()>(())).unwrap();
                // Independent O(n) minimum queue: no sift code or inverse map.
                let mut dense: Vec<Option<(f64, u64)>> = vec![None; n];
                let mut sequence = 0u64;
                let mut state = 17u64;
                for turn in 0..512 + n {
                    let mut scalar_units = 0usize;
                    let mut trace = Trace::default();
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    if turn % 4 == 3 || turn >= 512 {
                        let expected = dense
                            .iter()
                            .enumerate()
                            .filter_map(|(node, entry)| entry.map(|(cost, seq)| (node, cost, seq)))
                            .min_by(|a, b| a.1.total_cmp(&b.1).then(a.2.cmp(&b.2)));
                        let actual = {
                            let mut mutation = stepped.pop_steps();
                            let mut steps = 0;
                            loop {
                                steps += 1;
                                let result = mutation.step();
                                check_inverse(mutation.heap);
                                assert!(steps <= usize::BITS as usize + 1);
                                if let Some(result) = result {
                                    break result;
                                }
                            }
                        };
                        let sync = synchronous
                            .pop(&mut || {
                                scalar_units += 1;
                                Ok::<_, ()>(())
                            })
                            .unwrap();
                        let bulk = metered
                            .pop_metered(&mut ExactMeter {
                                trace: &mut trace,
                                stop: None,
                            })
                            .unwrap();
                        let tuple =
                            |entry: Entry| (entry.node, entry.cost.to_bits(), entry.sequence);
                        assert_eq!(
                            actual.map(tuple),
                            expected.map(|(v, c, s)| (v, c.to_bits(), s))
                        );
                        assert_eq!(actual.map(tuple), sync.map(tuple));
                        assert_eq!(actual.map(tuple), bulk.map(tuple));
                        if let Some((node, _, _)) = expected {
                            dense[node] = None;
                        }
                    } else {
                        let node = (state as usize) % n;
                        let cost = if turn % 7 == 0 {
                            dense[node].map_or(1.0, |(cost, _)| (cost - 5e-13).max(0.0))
                        } else {
                            ((state >> 32) % 40) as f64 / 4.0
                        };
                        let accept = dense[node].is_none_or(|(previous, _)| {
                            cost < match comparison {
                                DijkstraComparison::Strict => previous,
                                DijkstraComparison::FnxEpsilon => previous - FNX_DIJKSTRA_EPSILON,
                            }
                        });
                        if accept {
                            sequence += 1;
                            dense[node] = Some((cost, sequence));
                        }
                        {
                            let mut mutation = stepped.offer_steps::<()>(node, cost).unwrap();
                            let mut steps = 0;
                            loop {
                                steps += 1;
                                let result = mutation.step();
                                check_inverse(mutation.heap);
                                assert!(steps <= usize::BITS as usize + 1);
                                if let Some(result) = result {
                                    assert!(result.is_none());
                                    break;
                                }
                            }
                        }
                        synchronous
                            .offer(node, cost, &mut || {
                                scalar_units += 1;
                                Ok::<_, ()>(())
                            })
                            .unwrap();
                        metered
                            .offer_metered(
                                node,
                                cost,
                                &mut ExactMeter {
                                    trace: &mut trace,
                                    stop: None,
                                },
                            )
                            .unwrap();
                    }
                    assert_eq!(trace.seen, scalar_units, "{comparison:?} {n} {turn}");
                    assert_eq!(metered.positions, stepped.positions);
                    assert_eq!(metered.sequence, stepped.sequence);
                    assert_eq!(metered.len(), stepped.len());
                    for (bulk, scalar) in metered.entries.iter().zip(&stepped.entries) {
                        assert_eq!(
                            (bulk.node, bulk.cost.to_bits(), bulk.sequence),
                            (scalar.node, scalar.cost.to_bits(), scalar.sequence)
                        );
                    }
                    check_inverse(&metered);
                    assert_eq!(
                        stepped.len(),
                        dense.iter().filter(|entry| entry.is_some()).count()
                    );
                    assert!(stepped.len() <= n);
                    for child in 1..stepped.len() {
                        assert!(!IndexedHeap::before(
                            stepped.entries[child],
                            stepped.entries[(child - 1) / 2]
                        ));
                    }
                }
                assert_eq!(stepped.len(), 0);
            }
        }
    }

    #[test]
    fn stepped_queue_rejects_bad_ordinals_and_sequence_overflow_before_mutation() {
        let mut heap =
            IndexedHeap::new(2, DijkstraComparison::Strict, &mut || Ok::<_, ()>(())).unwrap();
        assert!(matches!(
            heap.offer_steps::<()>(2, 1.0),
            Err(HeapError::InvalidOrdinal)
        ));
        assert_eq!(heap.len(), 0);
        heap.sequence = u64::MAX;
        assert!(matches!(
            heap.offer_steps::<()>(0, 1.0),
            Err(HeapError::SizeOverflow)
        ));
        assert_eq!(heap.len(), 0);
        check_inverse(&heap);
    }
}
