//! Bounded delivery history as a dependent sink in the standing-query circuit.
//!
//! This cache is derived from accepted FINAL native output deltas, not a second
//! commit authority. No graph reads, query re-execution, thread or lock is used.
//! Whole ticks (including empty ticks) enter in commit order. Retention evicts
//! only a prefix; an oversized tick fences this sink without damaging its input.
//! Registration, retained frames and cursors are session-local, not durable.

use super::*;
use crate::QueryValue;
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Limits {
    ticks: usize,
    rows: usize,
    units: usize,
}
impl Limits {
    fn new(ticks: usize, rows: usize, units: usize) -> Option<Self> {
        (ticks != 0 && units != 0).then_some(Self { ticks, rows, units })
    }
}

/// One complete final-output bag derivative. Sharing a frame never expands
/// multiplicities or copies payloads. Frames held by applications can outlive
/// cache eviction; application-owned references are outside cache quotas.
pub struct StandingReplayBatch {
    from: CommitSeq,
    frontier: CommitSeq,
    rows: Arc<ZSet<Vec<QueryValue>>>,
    units: usize,
}
impl core::fmt::Debug for StandingReplayBatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StandingReplayBatch")
            .field("from", &self.from)
            .field("frontier", &self.frontier)
            .field("support", &self.rows.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}
impl StandingReplayBatch {
    pub fn from(&self) -> CommitSeq {
        self.from
    }
    pub fn frontier(&self) -> CommitSeq {
        self.frontier
    }
    pub fn rows(&self) -> &ZSet<Vec<QueryValue>> {
        &self.rows
    }
    /// Share the compressed bag without copying any cells or exact weights.
    pub fn shared_rows(&self) -> Arc<ZSet<Vec<QueryValue>>> {
        Arc::clone(&self.rows)
    }
}

/// Retention counts for one healthy sink. `retained_after` is the oldest
/// consumer cut from which every subsequent tick is still available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StandingReplayWindow {
    pub retained_after: CommitSeq,
    pub frontier: CommitSeq,
    pub ticks: usize,
    pub rows: usize,
    pub payload_units: usize,
}

struct History {
    baseline: CommitSeq,
    frames: VecDeque<Arc<StandingReplayBatch>>,
    rows: usize,
    units: usize,
}
impl History {
    fn new(at: CommitSeq) -> Self {
        Self { baseline: at, frames: VecDeque::new(), rows: 0, units: 0 }
    }
    fn frontier(&self) -> CommitSeq {
        self.frames.back().map_or(self.baseline, |frame| frame.frontier)
    }
    fn retained_after(&self) -> CommitSeq {
        self.frames.front().map_or(self.baseline, |frame| frame.from)
    }
    fn window(&self) -> StandingReplayWindow {
        StandingReplayWindow {
            retained_after: self.retained_after(),
            frontier: self.frontier(),
            ticks: self.frames.len(),
            rows: self.rows,
            payload_units: self.units,
        }
    }
    fn next(&self, after: CommitSeq) -> Result<Option<&Arc<StandingReplayBatch>>, StandingQueryError> {
        let frontier = self.frontier();
        let retained_after = self.retained_after();
        if after < retained_after || after > frontier {
            return Err(StandingQueryError::ReplayGap { after, retained_after, frontier });
        }
        if after == frontier {
            return Ok(None);
        }
        // Whole global ticks are contiguous. Index by sequence, not by a scan
        // of the retained prefix: each consumer step is O(1) in backlog length.
        let offset = usize::try_from(after.0 - retained_after.0)
            .map_err(|_| StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta))?;
        let frame = self.frames.get(offset)
            .filter(|frame| frame.from == after && after.checked_successor().ok() == Some(frame.frontier))
            .ok_or(StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta))?;
        Ok(Some(frame))
    }

    fn prepare(
        &mut self,
        at: CommitSeq,
        rows: ZSet<Vec<QueryValue>>,
        limits: Limits,
        meter: &mut Meter<'_>,
    ) -> Result<Update<'_>, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let from = self.frontier();
        if from.checked_successor().ok() != Some(at) {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        if rows.len() > limits.rows {
            return Err(StandingQueryFailure::ResultBudget);
        }
        // Logical accounting, not allocator bytes: a frame slot, each tuple
        // slot, each exact weight limb, and the existing native payload units.
        // Count compressed support, NEVER the absolute/expanded multiplicity.
        let mut units = 1_usize;
        for (row, weight) in rows.iter() {
            meter.charge(ZSetEvent::Work)?;
            units = units.checked_add(1)
                .and_then(|n| n.checked_add(weight.magnitude_limb_count()))
                .ok_or(StandingQueryFailure::ScratchBudget)?;
            for cell in row {
                meter.charge(ZSetEvent::Work)?;
                let payload = match cell {
                    QueryValue::Value(value) => value.payload_units(),
                    _ => 0,
                };
                units = units.checked_add(1).and_then(|n| n.checked_add(payload))
                    .ok_or(StandingQueryFailure::ScratchBudget)?;
            }
            if units > limits.units {
                return Err(StandingQueryFailure::ScratchBudget);
            }
        }
        // The entire prospective tick must fit by itself. Never evict history
        // and then discover that the new frame cannot be retained.
        if units > limits.units {
            return Err(StandingQueryFailure::ScratchBudget);
        }
        let mut retained_rows = self.rows;
        let mut retained_units = self.units;
        let mut evict = 0;
        while self.frames.len() - evict >= limits.ticks
            || retained_rows > limits.rows - rows.len()
            || retained_units > limits.units - units
        {
            meter.charge(ZSetEvent::Work)?;
            let frame = self.frames.get(evict).ok_or(StandingQueryFailure::InvalidDelta)?;
            retained_rows = retained_rows.checked_sub(frame.rows.len())
                .ok_or(StandingQueryFailure::InvalidDelta)?;
            retained_units = retained_units.checked_sub(frame.units)
                .ok_or(StandingQueryFailure::InvalidDelta)?;
            evict += 1;
        }
        let next_rows = retained_rows + rows.len(); // admitted above, cannot overflow
        let next_units = retained_units + units;
        meter.units(ZSetEvent::ScratchEntry, evict)?;
        let mut retired = Vec::new();
        retired.try_reserve_exact(evict).map_err(|_| StandingQueryFailure::ScratchBudget)?;
        // If a prefix will be removed, its slots already admit the new frame.
        // Reserve before publication; no allocation can fail during the swap.
        if evict == 0 {
            meter.charge(ZSetEvent::ScratchEntry)?;
            self.frames.try_reserve(1).map_err(|_| StandingQueryFailure::ScratchBudget)?;
        }
        meter.units(ZSetEvent::ScratchEntry, 2)?;
        let frame = Arc::new(StandingReplayBatch { from, frontier: at, rows: Arc::new(rows), units });
        (meter.checkpoint)()?;
        Ok(Update { owner: self, frame, evict, next_rows, next_units, retired })
    }
}

/// Preparation pins the old history. Every arithmetic/admission/checkpoint
/// refusal and dropping this guard preserve all frames, cuts and counters.
/// Capacity reservation may grow the allocation but cannot change logical state.
#[must_use = "dropping a replay update leaves the retained history unchanged"]
struct Update<'a> {
    owner: &'a mut History,
    frame: Arc<StandingReplayBatch>,
    evict: usize,
    next_rows: usize,
    next_units: usize,
    retired: Vec<Arc<StandingReplayBatch>>,
}
impl Update<'_> {
    fn commit(mut self) {
        // The exclusive borrow pins the exact prefix measured by prepare.
        // Hold retired payloads until AFTER coherent publication, even when
        // the last reference would run a payload destructor on release.
        for _ in 0..self.evict {
            if let Some(frame) = self.owner.frames.pop_front() {
                self.retired.push(frame);
            }
        }
        self.owner.frames.push_back(self.frame);
        self.owner.rows = self.next_rows;
        self.owner.units = self.next_units;
        drop(self.retired);
    }
}

pub(crate) struct State {
    pub(super) source: StandingQueryHandle,
    pub(super) limits: Limits,
    history: History,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}
impl State {
    pub(super) fn maintain(
        &mut self,
        batch: &LogicalDeltaBatch,
        sources: &[StandingQuery],
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let at = batch.commit_seq();
        if self.frontier.checked_successor().ok() != Some(at)
            || batch.frontier() != at || batch.commit_marker_identity().commit_seq != at
        {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let source = sources.get(self.source.index).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        let (_, frontier, failure) = source.status();
        if frontier != at || failure.is_some() {
            return Err(StandingQueryFailure::DependencyUnavailable);
        }
        let layout = self.source.native.as_deref().ok_or(StandingQueryFailure::InvalidDelta)?;
        let rows = source.native_delta_for_replay(layout, meter)?;
        meter.stats.delta_rows = u64::try_from(rows.len()).map_err(|_| StandingQueryFailure::WorkBudget)?;
        self.history.prepare(at, rows, self.limits, meter)?.commit();
        Ok(())
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Retain complete native result deltas in the SAME dependency-ordered
    /// circuit that maintains their source. No polling/capture call is needed
    /// after writes, and producer work is never rerun to reconstruct old ticks.
    /// Registration starts at the current source frontier with an empty history.
    ///
    /// Limits bound retained ticks, sum of changed support rows, and logical
    /// payload units. Ticks/units must be nonzero; zero rows admits empty ticks.
    /// Per-tick copying has the supplied work/scratch/result policy. Eviction
    /// removes the shortest prefix needed to fit a whole new frame. A frame
    /// too large alone fences ONLY this derived sink; a durable write and its
    /// source/siblings remain accepted. Rebuild establishes a new empty history
    /// at the current healthy source, not a fabricated missing prefix.
    ///
    /// Use the original source handle for a snapshot baseline. This returned
    /// handle identifies delivery history, not another query result. Retention
    /// is bounded in logical units, not process bytes; shared frames held by
    /// callers and temporary preparation allocations have separate lifetimes.
    pub fn register_standing_replay(
        &mut self,
        cx: &QueryCx,
        source: &StandingQueryHandle,
        max_ticks: usize,
        max_rows: usize,
        max_payload_units: usize,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.standing_native_columns(cx, source)?;
        let limits = Limits::new(max_ticks, max_rows, max_payload_units)
            .ok_or(StandingQueryError::InvalidReplayLimits)?;
        let state = self.prepare_standing_replay(cx, source, limits, policy)?;
        Ok(self.store_standing_query(StandingQuery::Replay(Box::new(state))))
    }

    pub(super) fn prepare_standing_replay(
        &self,
        cx: &QueryCx,
        source: &StandingQueryHandle,
        limits: Limits,
        policy: GqlQueryPolicy,
    ) -> Result<State, StandingQueryError> {
        self.standing_native_columns(cx, source)?;
        let frontier = self.admitted_standing_query(cx, source)?.status().1;
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        Ok(State {
            source: source.clone(), limits, history: History::new(frontier),
            policy, frontier, stats: StandingQueryStats::default(), failure: None,
        })
    }

    pub fn standing_replay_window(
        &self,
        cx: &QueryCx,
        replay: &StandingQueryHandle,
    ) -> Result<StandingReplayWindow, StandingQueryError> {
        let StandingQuery::Replay(state) = self.admitted_standing_query(cx, replay)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(state.history.window())
    }

    /// Pull the FIRST retained successor of `after`, not the newest tick.
    /// None means caught up; expired, future and pre-registration cuts refuse.
    /// Repeated calls share the same immutable frame and acknowledge nothing.
    /// Lookup is O(1) in backlog length and expands no row multiplicities.
    /// ResultRows limits the frame's compressed support. Work/scratch admission
    /// charges the shared handle, not nonexistent payload copies or graph scans.
    pub fn standing_replay_next(
        &self,
        cx: &QueryCx,
        replay: &StandingQueryHandle,
        after: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<Option<Arc<StandingReplayBatch>>, StandingQueryError> {
        let StandingQuery::Replay(state) = self.admitted_standing_query(cx, replay)? else {
            return Err(StandingQueryError::Unsupported);
        };
        let Some(frame) = state.history.next(after)? else { return Ok(None); };
        cx.with_restriction(|| {
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Delivery)?;
            if policy.rows.max_result_rows().is_some_and(|limit| frame.rows.len() as u128 > u128::from(limit)) {
                return Err(StandingQueryError::Delivery(StandingQueryFailure::ResultBudget));
            }
            meter.charge(ZSetEvent::ScratchEntry).map_err(StandingQueryError::Delivery)?;
            (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
            Ok(Some(Arc::clone(frame)))
        })
    }
}

#[cfg(test)]
mod tests;
