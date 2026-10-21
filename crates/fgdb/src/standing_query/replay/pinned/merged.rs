//! Owned net-change delivery over pinned replay frames, without self-borrows.
//!
//! Reuse atomic-prefix pinning and seek each immutable Z-set after its owned
//! head. The heap holds at most one head per nonempty tick, never all output.

use super::*;
use fgdb_delta_types::{LimbLimit, ZWeight};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

const LIMBS: LimbLimit = LimbLimit::new(4);
type Change = (Vec<QueryValue>, ZWeight);
type Item = Result<Change, StandingQueryError>;

impl<V: Vfs + Clone> Database<V> {
    /// Stream the NET result change across up to `max_ticks` complete retained
    /// successors of `after`, without borrowing the database while draining.
    /// None means caught up at opening; an empty net result may still advance
    /// across cancelled or empty ticks. The returned cut is the SELECTED end,
    /// not successful delivery. Stage changes privately and apply the complete
    /// transition only after successful EOF, including its final checkpoint.
    /// An error may follow earlier rows; dropping early is not completion.
    ///
    /// The immutable frames, not a database/registry borrow, pin the accepted
    /// prefix. Writes, eviction, rebuild and database destruction may proceed
    /// afterward. There is no graph scan, query rerun, output bag or expanded
    /// multiplicity. Canonical rows and checked signed weights retain their
    /// original domains. Equal keys add in commit order before cancellation.
    ///
    /// One copied key/weight per nonempty tick plus the current accumulator
    /// bounds live merge state. Seeks cost O(log n) per source step; heap work
    /// is O(log k) for k nonempty ticks. Head payloads ARE copied and charged,
    /// including keys later cancelled; this is not a zero-copy merge. Successful
    /// output transfers its head allocation without a second payload copy.
    /// Opening and all pulls share one cumulative work/scratch policy, while
    /// ResultRows counts only delivered nonzero net keys. Comparison cost has
    /// the existing Z-set logical-event contract, not a byte/cycle guarantee.
    ///
    /// The separate pin limit counts ALL selected frame payload units, even
    /// empty ticks, before head copies. It may extend retained data lifetimes;
    /// it is per-cursor, not a global memory cap. An excessive requested prefix
    /// refuses instead of silently shortening itself. Error, unwind, EOF and
    /// drop release pins and heads without draining or acknowledging anything.
    /// This is privileged session-local state catch-up, not an event feed,
    /// durable resume, spill execution or live capability reauthorization.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn standing_replay_owned_coalesced_cursor<'cx>(
        &self,
        cx: &'cx QueryCx,
        replay: &StandingQueryHandle,
        after: CommitSeq,
        max_ticks: NonZeroUsize,
        max_pinned_payload_units: usize,
        policy: GqlQueryPolicy,
    ) -> Result<Option<(CommitSeq, impl core::iter::FusedIterator<Item = Item> + 'cx + use<'cx, V>)>, StandingQueryError> {
        let history = self.owned_net_history(cx, replay)?;
        cx.with_restriction(|| {
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            let Some((end, pins)) = PinnedReplay::open(
                history, after, max_ticks, max_pinned_payload_units, &mut meter,
            )? else { return Ok(None); };
            let pull = NetPull::from_pins(pins, false, &mut meter).map_err(StandingQueryError::Delivery)?;
            Ok(Some((end, drive(cx, pull))))
        })
    }

    /// Stream result(after) - result(before) over an EXACT retained interval.
    /// Both cuts are admitted, including equal cuts; evicted, future and
    /// pre-registration cuts refuse. Reverse cuts negate the completed sum,
    /// never reverse chronological checked addition or narrow promoted weights.
    /// Equal admitted cuts yield an empty cursor whose EOF still checks control.
    /// The ownership, pin/cumulative budgets and atomic-consumer completion
    /// contract are those of standing_replay_owned_coalesced_cursor.
    #[allow(clippy::too_many_arguments)]
    pub fn standing_replay_owned_difference_cursor<'cx>(
        &self,
        cx: &'cx QueryCx,
        replay: &StandingQueryHandle,
        before: CommitSeq,
        after: CommitSeq,
        max_pinned_payload_units: usize,
        policy: GqlQueryPolicy,
    ) -> Result<impl core::iter::FusedIterator<Item = Item> + 'cx + use<'cx, V>, StandingQueryError> {
        let history = self.owned_net_history(cx, replay)?;
        cx.with_restriction(|| {
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            let pins = pin_interval(history, before, after, max_pinned_payload_units, &mut meter)?;
            let pull = NetPull::from_pins(pins, before > after, &mut meter).map_err(StandingQueryError::Delivery)?;
            Ok(drive(cx, pull))
        })
    }

    fn owned_net_history(&self, cx: &QueryCx, replay: &StandingQueryHandle) -> Result<&History, StandingQueryError> {
        let StandingQuery::Replay(state) = self.admitted_standing_query(cx, replay)? else {
            return Err(StandingQueryError::Unsupported);
        };
        if state.history.frontier() != state.frontier {
            return Err(StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta));
        }
        Ok(&state.history)
    }
}

fn drive(cx: &QueryCx, mut pull: NetPull) -> impl core::iter::FusedIterator<Item = Item> + '_ {
    std::iter::from_fn(move || {
        cx.with_restriction(|| pull.advance(&mut || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted)))
    }).fuse()
}

fn pin_interval(
    history: &History,
    before: CommitSeq,
    after: CommitSeq,
    max_units: usize,
    meter: &mut Meter<'_>,
) -> Result<PinnedReplay, StandingQueryError> {
    let retained_after = history.retained_after();
    let frontier = history.frontier();
    for cut in [before, after] {
        meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Delivery)?;
        if cut < retained_after || cut > frontier {
            return Err(StandingQueryError::ReplayGap { after: cut, retained_after, frontier });
        }
    }
    let low = before.min(after);
    let high = before.max(after);
    let count = usize::try_from(high.0 - low.0)
        .map_err(|_| StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta))?;
    if let Some(count) = NonZeroUsize::new(count) {
        let (end, pins) = PinnedReplay::open(history, low, count, max_units, meter)?
            .ok_or(StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta))?;
        if end != high {
            return Err(StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta));
        }
        Ok(pins)
    } else {
        (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
        Ok(PinnedReplay {
            frames: Some(VecDeque::new()), policy: meter.policy, stats: meter.stats, delivered: 0,
        })
    }
}

struct OwnedHead {
    row: Vec<QueryValue>,
    weight: ZWeight,
    source: usize,
}
impl PartialEq for OwnedHead {
    fn eq(&self, other: &Self) -> bool { self.source == other.source && self.row == other.row }
}
impl Eq for OwnedHead {}
impl PartialOrd for OwnedHead {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for OwnedHead {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max heap reversed in BOTH fields: canonical keys, then oldest tick.
        other.row.cmp(&self.row).then_with(|| other.source.cmp(&self.source))
    }
}
impl OwnedHead {
    fn copy(row: &[QueryValue], weight: &ZWeight, source: usize, meter: &mut Meter<'_>) -> Result<Self, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        if weight.is_zero() { return Err(StandingQueryFailure::InvalidDelta); }
        meter.units(ZSetEvent::ScratchEntry, weight.magnitude_limb_count().checked_add(1).ok_or(StandingQueryFailure::ScratchBudget)?)?;
        let weight = weight.checked_clone(LIMBS).map_err(|_| StandingQueryFailure::Arithmetic)?;
        meter.charge(ZSetEvent::ScratchEntry)?;
        let mut cells = Vec::new();
        for cell in row {
            meter.charge(ZSetEvent::Work)?;
            let units = match cell { QueryValue::Value(value) => value.payload_units(), _ => 0 };
            meter.units(ZSetEvent::ScratchEntry, units.checked_add(1).ok_or(StandingQueryFailure::ScratchBudget)?)?;
            cells.try_reserve(1).map_err(|_| StandingQueryFailure::ScratchBudget)?;
            cells.push(cell.clone());
        }
        Ok(Self { row: cells, weight, source })
    }
}

struct OwnedMerge {
    frames: VecDeque<Arc<StandingReplayBatch>>,
    heads: BinaryHeap<OwnedHead>,
}
impl OwnedMerge {
    fn new(frames: VecDeque<Arc<StandingReplayBatch>>, meter: &mut Meter<'_>) -> Result<Self, StandingQueryFailure> {
        let mut heads = BinaryHeap::new();
        for (source, frame) in frames.iter().enumerate() {
            meter.charge(ZSetEvent::Work)?;
            if let Some((row, weight)) = frame.rows.first_after(None) {
                meter.charge(ZSetEvent::ScratchEntry)?;
                heads.try_reserve(1).map_err(|_| StandingQueryFailure::ScratchBudget)?;
                heads.push(OwnedHead::copy(row, weight, source, meter)?);
            }
        }
        (meter.checkpoint)()?;
        Ok(Self { frames, heads })
    }

    fn consume(&mut self, source: usize, row: &Vec<QueryValue>, weight: ZWeight, total: ZWeight, meter: &mut Meter<'_>) -> Result<ZWeight, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let units = total.magnitude_limb_count().max(weight.magnitude_limb_count())
            .checked_add(1).ok_or(StandingQueryFailure::ScratchBudget)?;
        meter.units(ZSetEvent::ScratchEntry, units)?;
        let total = total.checked_add(&weight, LIMBS).map_err(|_| StandingQueryFailure::Arithmetic)?;
        meter.charge(ZSetEvent::Work)?;
        if let Some((next, weight)) = self.frames[source].rows.first_after(Some(row)) {
            let head = OwnedHead::copy(next, weight, source, meter)?;
            // One predecessor was popped: the admitted heap slot is reusable.
            self.heads.push(head);
        }
        Ok(total)
    }

    fn next(&mut self, meter: &mut Meter<'_>) -> Result<Option<Change>, StandingQueryFailure> {
        loop {
            meter.charge(ZSetEvent::Work)?;
            let Some(OwnedHead { row, weight, source }) = self.heads.pop() else { return Ok(None); };
            let mut total = self.consume(source, &row, weight, ZWeight::ZERO, meter)?;
            loop {
                meter.charge(ZSetEvent::Work)?;
                if !self.heads.peek().is_some_and(|head| head.row == row) { break; }
                let head = self.heads.pop().ok_or(StandingQueryFailure::InvalidDelta)?;
                total = self.consume(head.source, &head.row, head.weight, total, meter)?;
            }
            if !total.is_zero() { return Ok(Some((row, total))); }
        }
    }
}

struct NetPull {
    merge: Option<OwnedMerge>,
    reverse: bool,
    policy: GqlQueryPolicy,
    stats: StandingQueryStats,
    delivered: u64,
}
impl NetPull {
    fn from_pins(mut pins: PinnedReplay, reverse: bool, meter: &mut Meter<'_>) -> Result<Self, StandingQueryFailure> {
        let frames = pins.frames.take().ok_or(StandingQueryFailure::InvalidDelta)?;
        let merge = OwnedMerge::new(frames, meter)?;
        Ok(Self { merge: Some(merge), reverse, policy: meter.policy, stats: meter.stats, delivered: 0 })
    }

    fn advance(&mut self, checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>) -> Option<Item> {
        // Own all pins/heads locally before fallible or caller-controlled work.
        // Errors and unwinds cannot resume a partial key or retain its history.
        let mut merge = self.merge.take()?;
        let mut meter = Meter { policy: self.policy, stats: self.stats, checkpoint };
        let result = (|| {
            let Some((row, mut weight)) = merge.next(&mut meter)? else {
                (meter.checkpoint)()?;
                return Ok(None);
            };
            let delivered = self.delivered.checked_add(1).ok_or(StandingQueryFailure::ResultBudget)?;
            if self.policy.rows.max_result_rows().is_some_and(|limit| delivered > limit) {
                return Err(StandingQueryFailure::ResultBudget);
            }
            if self.reverse {
                meter.charge(ZSetEvent::Work)?;
                meter.units(ZSetEvent::ScratchEntry, weight.magnitude_limb_count().checked_add(1).ok_or(StandingQueryFailure::ScratchBudget)?)?;
                weight = weight.checked_neg(LIMBS).map_err(|_| StandingQueryFailure::Arithmetic)?;
            }
            (meter.checkpoint)()?;
            Ok(Some((row, weight, delivered)))
        })();
        self.stats = meter.stats;
        match result {
            Ok(Some((row, weight, delivered))) => {
                self.delivered = delivered;
                self.merge = Some(merge);
                Some(Ok((row, weight)))
            }
            Ok(None) => None,
            Err(error) => Some(Err(StandingQueryError::Delivery(error))),
        }
    }
}

#[cfg(test)]
mod tests;
