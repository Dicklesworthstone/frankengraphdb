//! Bounded net catch-up over a pinned, contiguous final-result replay range.
//!
//! Borrow native keys while adding the exact signed deltas. Only surviving
//! tuples acquire payload ownership. No endpoint query or snapshot difference
//! is evaluated, and no intermediate per-commit event is promised to survive.

use super::*;
use fgdb_delta_types::{LimbLimit, ZWeight};
use std::num::NonZeroUsize;

const LIMBS: LimbLimit = LimbLimit::new(4);
type Coalesced = Option<(CommitSeq, ZSet<Vec<QueryValue>>)>;

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
    /// No payload copy is made for a cancelled tuple. Private signed support
    /// is still memory-resident and metered; the tick limit is not a byte bound.
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
        // Reuse the same O(1) indexed successor and gap validation as atomic
        // replay. In particular, even an all-empty range must prove every cut.
        let mut next = self.next(after)?;
        let mut frontier = after;
        let mut remaining = max_ticks.get();
        let mut net = ZSet::new();
        while let Some(frame) = next {
            meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Delivery)?;
            for (row, weight) in frame.rows.iter() {
                add_borrowed(&mut net, row, weight, meter)
                    .map_err(StandingQueryError::Delivery)?;
            }
            frontier = frame.frontier;
            remaining -= 1;
            if remaining == 0 {
                break;
            }
            next = self.next(frontier)?;
        }
        if frontier == after {
            (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
            return Ok(None);
        }
        if meter.policy.rows.max_result_rows()
            .is_some_and(|limit| net.len() as u128 > u128::from(limit))
        {
            return Err(StandingQueryError::Delivery(StandingQueryFailure::ResultBudget));
        }
        let rows = own_survivors(net, meter).map_err(StandingQueryError::Delivery)?;
        (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
        Ok(Some((frontier, rows)))
    }
}

fn add_borrowed<'a>(
    net: &mut ZSet<&'a Vec<QueryValue>>,
    row: &'a Vec<QueryValue>,
    weight: &ZWeight,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    if weight.is_zero() {
        return Err(StandingQueryFailure::InvalidDelta);
    }
    // The owning immutable history pins every referenced key for this fold.
    // Neither a promoted count nor repeated support expands to occurrences.
    let units = weight.magnitude_limb_count().checked_add(1)
        .ok_or(StandingQueryFailure::ScratchBudget)?;
    meter.units(ZSetEvent::ScratchEntry, units)?;
    let weight = weight.checked_clone(LIMBS).map_err(|_| StandingQueryFailure::Arithmetic)?;
    net.accumulate(row, weight, LIMBS, &mut |event| meter.charge(event))
        .map_err(zset_error)
}

fn own_survivors(
    net: ZSet<&Vec<QueryValue>>,
    meter: &mut Meter<'_>,
) -> Result<ZSet<Vec<QueryValue>>, StandingQueryFailure> {
    let mut rows = ZSet::new();
    for (row, weight) in net.into_updates() {
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
        rows.accumulate(cells, weight, LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;
