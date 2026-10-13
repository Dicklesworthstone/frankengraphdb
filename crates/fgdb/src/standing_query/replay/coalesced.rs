//! Bounded net catch-up over a pinned, contiguous final-result replay range.
//!
//! Merge the already sorted tick bags with one borrowed head per nonempty tick.
//! Complete each key's chronological signed sum before owning its payload. No
//! endpoint query, snapshot difference or intermediate-support table is built.

use super::*;
use fgdb_delta_types::{LimbLimit, ZWeight};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::num::NonZeroUsize;

const LIMBS: LimbLimit = LimbLimit::new(4);
type Coalesced = Option<(CommitSeq, ZSet<Vec<QueryValue>>)>;
type Entry<'a> = (&'a Vec<QueryValue>, &'a ZWeight);

impl<V: Vfs + Clone> Database<V> {
    /// Read the exact NET change for up to `max_ticks` retained commits after
    /// `after`. The returned frontier is the end of that complete prefix, not
    /// necessarily the live frontier. Apply the bag once to a result from the
    /// SAME source at `after`, then continue from the returned frontier.
    ///
    /// None means already caught up. Some with an empty bag still advances the
    /// frontier: empty ticks and changes that cancel have not disappeared from
    /// history. Expired, future and pre-registration cuts return ReplayGap;
    /// coalescing never substitutes an available suffix for a missing prefix.
    /// Repeated reads acknowledge nothing and do not change retention.
    ///
    /// One cumulative work/scratch allowance covers all selected ticks, exact
    /// arithmetic and surviving payload copies. ResultRows counts final net
    /// tuples AFTER cancellation, not intermediate support or occurrences.
    /// No payload copy is made for a cancelled tuple. The merge retains one
    /// iterator and borrowed head per nonempty selected tick, plus one exact
    /// accumulator; it does not retain all intermediate changed keys. Retained
    /// history and the final result are still resident. Logical scratch events
    /// are cumulative work/growth reservations, not a live allocator-byte cap.
    /// Canonical key comparisons retain the existing Z-set cost contract.
    /// Any refusal discards the whole tentative result, leaving history intact.
    ///
    /// This is state catch-up, not a per-commit event feed: transient results,
    /// intermediate ranks and per-tick side effects are intentionally absent.
    /// Use standing_replay_next for atomic ticks. No durable resume, commit
    /// proof, authority declassification, source scan or new log is introduced.
    pub fn standing_replay_coalesced(
        &self,
        cx: &QueryCx,
        replay: &StandingQueryHandle,
        after: CommitSeq,
        max_ticks: NonZeroUsize,
        policy: GqlQueryPolicy,
    ) -> Result<Option<(CommitSeq, ZSet<Vec<QueryValue>>)>, StandingQueryError> {
        let StandingQuery::Replay(state) = self.admitted_standing_query(cx, replay)? else {
            return Err(StandingQueryError::Unsupported);
        };
        if state.history.frontier() != state.frontier {
            return Err(StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta));
        }
        cx.with_restriction(|| {
            let mut checkpoint = || {
                cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted)
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            state.history.coalesce(after, max_ticks, &mut meter)
        })
    }
}

impl History {
    fn coalesce(
        &self,
        after: CommitSeq,
        max_ticks: NonZeroUsize,
        meter: &mut Meter<'_>,
    ) -> Result<Coalesced, StandingQueryError> {
        // Validate the complete selected prefix before reading its row bags.
        // The same immutable history borrow pins these cuts during the merge.
        // Empty frames still participate in successor validation and metering.
        let mut next = self.next(after)?;
        let mut frontier = after;
        let mut selected = 0_usize;
        while let Some(frame) = next {
            meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Delivery)?;
            frontier = frame.frontier;
            selected += 1;
            if selected == max_ticks.get() {
                break;
            }
            next = self.next(frontier)?;
        }
        if selected == 0 {
            (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
            return Ok(None);
        }
        // History::next already proved after lies inside this contiguous range.
        let start = usize::try_from(after.0 - self.retained_after().0)
            .map_err(|_| StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta))?;
        let end = start.checked_add(selected)
            .filter(|end| *end <= self.frames.len())
            .ok_or(StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta))?;
        let sources = self.frames.range(start..end).map(|frame| frame.rows.iter());
        let mut merge = SortedMerge::new(sources, meter).map_err(StandingQueryError::Delivery)?;
        let mut rows = ZSet::new();
        while let Some((row, weight)) = merge.next(meter).map_err(StandingQueryError::Delivery)? {
            // This key is complete across ALL selected ticks, so no later row
            // can cancel it. Admit the final support before copying its payload.
            let count = rows.len().checked_add(1)
                .ok_or(StandingQueryError::Delivery(StandingQueryFailure::ResultBudget))?;
            if meter.policy.rows.max_result_rows()
                .is_some_and(|limit| count as u128 > u128::from(limit))
            {
                return Err(StandingQueryError::Delivery(StandingQueryFailure::ResultBudget));
            }
            let cells = own_row(row, meter).map_err(StandingQueryError::Delivery)?;
            rows.accumulate(cells, weight, LIMBS, &mut |event| meter.charge(event))
                .map_err(zset_error).map_err(StandingQueryError::Delivery)?;
        }
        (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
        Ok(Some((frontier, rows)))
    }
}

struct Head<'a> {
    row: &'a Vec<QueryValue>,
    weight: &'a ZWeight,
    source: usize,
}
impl PartialEq for Head<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.row.cmp(other.row) == Ordering::Equal
    }
}
impl Eq for Head<'_> {}
impl PartialOrd for Head<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for Head<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max heap. Reverse BOTH keys: equal rows must add
        // oldest tick first, preserving checked intermediate-overflow behavior.
        other.row.cmp(self.row).then_with(|| other.source.cmp(&self.source))
    }
}

/// Private source-bounded merge; only immutable canonical Z-set iterators enter
/// from History. Each source has at most one live heap head, and replacing a
/// popped head reuses its admitted slot. Keys and payloads remain borrowed.
struct SortedMerge<'a, I> {
    sources: Vec<I>,
    heads: BinaryHeap<Head<'a>>,
    done: bool,
}
impl<'a, I: Iterator<Item = Entry<'a>>> SortedMerge<'a, I> {
    fn new(
        sources: impl IntoIterator<Item = I>,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        let mut merged = Self { sources: Vec::new(), heads: BinaryHeap::new(), done: false };
        for mut source in sources {
            meter.charge(ZSetEvent::Work)?;
            if let Some((row, weight)) = source.next() {
                meter.units(ZSetEvent::ScratchEntry, 2)?;
                merged.sources.try_reserve(1).map_err(|_| StandingQueryFailure::ScratchBudget)?;
                merged.heads.try_reserve(1).map_err(|_| StandingQueryFailure::ScratchBudget)?;
                let at = merged.sources.len();
                merged.sources.push(source);
                merged.heads.push(Head { row, weight, source: at });
            }
        }
        Ok(merged)
    }

    fn next(
        &mut self,
        meter: &mut Meter<'_>,
    ) -> Result<Option<(&'a Vec<QueryValue>, ZWeight)>, StandingQueryFailure> {
        if self.done { return Ok(None); }
        // A failed or unwinding partial key can never be resumed as a complete
        // sum. The caller discards its tentative output on any such refusal.
        self.done = true;
        loop {
            meter.charge(ZSetEvent::Work)?;
            let Some(mut head) = self.heads.pop() else { return Ok(None); };
            let row = head.row;
            let mut total = ZWeight::ZERO;
            loop {
                total = add_weight(&total, head.weight, meter)?;
                meter.charge(ZSetEvent::Work)?;
                if let Some((next_row, next_weight)) = self.sources[head.source].next() {
                    if next_row <= head.row { return Err(StandingQueryFailure::InvalidDelta); }
                    self.heads.push(Head { row: next_row, weight: next_weight, source: head.source });
                }
                meter.charge(ZSetEvent::Work)?;
                if self.heads.peek().is_some_and(|next| next.row == row) {
                    head = self.heads.pop().ok_or(StandingQueryFailure::InvalidDelta)?;
                } else {
                    break;
                }
            }
            if !total.is_zero() {
                self.done = false;
                return Ok(Some((row, total)));
            }
        }
    }
}

fn add_weight(
    total: &ZWeight,
    weight: &ZWeight,
    meter: &mut Meter<'_>,
) -> Result<ZWeight, StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    if weight.is_zero() { return Err(StandingQueryFailure::InvalidDelta); }
    let units = weight.magnitude_limb_count().checked_add(1)
        .ok_or(StandingQueryFailure::ScratchBudget)?;
    meter.units(ZSetEvent::ScratchEntry, units)?;
    // Admit every input even if a later contribution cancels it. A checked
    // result alone must not launder an oversized promoted source weight.
    let weight = weight.checked_clone(LIMBS).map_err(|_| StandingQueryFailure::Arithmetic)?;
    meter.charge(ZSetEvent::Work)?;
    let units = total.magnitude_limb_count().max(weight.magnitude_limb_count())
        .checked_add(1).ok_or(StandingQueryFailure::ScratchBudget)?;
    meter.units(ZSetEvent::ScratchEntry, units)?;
    total.checked_add(&weight, LIMBS).map_err(|_| StandingQueryFailure::Arithmetic)
}

fn own_row(
    row: &[QueryValue],
    meter: &mut Meter<'_>,
) -> Result<Vec<QueryValue>, StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    meter.charge(ZSetEvent::ScratchEntry)?;
    let mut cells = Vec::new();
    for value in row {
        meter.charge(ZSetEvent::Work)?;
        let units = match value {
            QueryValue::Value(value) => value.payload_units(),
            _ => 0,
        }.checked_add(1).ok_or(StandingQueryFailure::ScratchBudget)?;
        meter.units(ZSetEvent::ScratchEntry, units)?;
        cells.try_reserve(1).map_err(|_| StandingQueryFailure::ScratchBudget)?;
        cells.push(value.clone());
    }
    Ok(cells)
}

#[cfg(test)]
mod tests;
