//! Bounded net catch-up over a pinned, contiguous final-result replay range.
//!
//! Merge the already sorted tick bags with one borrowed head per nonempty tick.
//! Complete each key's chronological signed sum before owning its payload. No
//! endpoint query, snapshot difference or intermediate-support table is built.

mod cursor;

use super::*;
use fgdb_delta_types::{LimbLimit, ZWeight};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::num::NonZeroUsize;

/// The last coalesced tick and the consolidated change through it.
type CoalescedTicks = (CommitSeq, ZSet<Vec<QueryValue>>);

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
    ) -> Result<Option<CoalescedTicks>, StandingQueryError> {
        let StandingQuery::Replay(state) = self.admitted_standing_query(cx, replay)? else {
            return Err(StandingQueryError::Unsupported);
        };
        if state.history.frontier() != state.frontier {
            return Err(StandingQueryError::Delivery(
                StandingQueryFailure::InvalidDelta,
            ));
        }
        cx.with_restriction(|| {
            let mut checkpoint = || {
                cx.checkpoint()
                    .map_err(|_| StandingQueryFailure::Interrupted)
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            state.history.coalesce(after, max_ticks, &mut meter)
        })
    }

    /// Compare two exact retained revisions of one maintained native result.
    /// Returns the compressed signed bag `result(after) - result(before)` by
    /// integrating ONLY the complete ticks in `(min(before, after), max(..)]`.
    /// Neither endpoint need be the live frontier. Reverse cuts invert each
    /// final exact weight and can be used to move an existing result backwards;
    /// the matching baseline and source definition remain the caller's duty.
    ///
    /// Both endpoints must belong to this replay sink's retained window, even
    /// when equal. Pre-registration, evicted and future cuts return ReplayGap
    /// before row bags are read. Every selected successor is checked, including
    /// empty ticks. There is no shorter-prefix, available-suffix or endpoint
    /// query fallback. Equal admitted cuts return an empty bag, not missing
    /// history. A failed or stale sink cannot be used even for equal cuts.
    ///
    /// One cumulative policy covers interval validation, merge, exact weight
    /// arithmetic and surviving payload copies. ResultRows limits final net
    /// support after cancellation. Reverse arithmetic uses the same bounded
    /// exact integer domain; it never narrows promoted multiplicities to i128.
    /// Any refusal or interruption returns no partial bag and leaves retention,
    /// source state and acknowledgements unchanged. Pure order changes are not
    /// bag changes, but the source's selected window is already in each delta.
    ///
    /// This is session-local derived result history, not arbitrary Chronicle
    /// history, raw graph DIFF syntax, cross-branch comparison, a durable resume
    /// token or capability-authorized transport. Registration/rebuild establishes
    /// the earliest possible baseline; reopening does not reconstruct it.
    pub fn standing_replay_difference(
        &self,
        cx: &QueryCx,
        replay: &StandingQueryHandle,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<ZSet<Vec<QueryValue>>, StandingQueryError> {
        let StandingQuery::Replay(state) = self.admitted_standing_query(cx, replay)? else {
            return Err(StandingQueryError::Unsupported);
        };
        if state.history.frontier() != state.frontier {
            return Err(StandingQueryError::Delivery(
                StandingQueryFailure::InvalidDelta,
            ));
        }
        cx.with_restriction(|| {
            let mut checkpoint = || {
                cx.checkpoint()
                    .map_err(|_| StandingQueryFailure::Interrupted)
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            state.history.difference(before, after, &mut meter)
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
        let Some((frontier, range)) = self.select_range(after, max_ticks, meter)? else {
            return Ok(None);
        };
        let rows = self.collect_range(range, false, meter)?;
        Ok(Some((frontier, rows)))
    }

    fn difference(
        &self,
        before: CommitSeq,
        after: CommitSeq,
        meter: &mut Meter<'_>,
    ) -> Result<ZSet<Vec<QueryValue>>, StandingQueryError> {
        let range = self.select_difference(before, after, meter)?;
        self.collect_range(range, before > after, meter)
    }

    fn collect_range(
        &self,
        range: std::ops::Range<usize>,
        reverse: bool,
        meter: &mut Meter<'_>,
    ) -> Result<ZSet<Vec<QueryValue>>, StandingQueryError> {
        let sources = self.frames.range(range).map(|frame| frame.rows.iter());
        let mut merge = SortedMerge::new(sources, meter).map_err(StandingQueryError::Delivery)?;
        let mut rows = ZSet::new();
        while let Some((row, weight)) = merge.next(meter).map_err(StandingQueryError::Delivery)? {
            // This key is complete across ALL selected ticks, so no later row
            // can cancel it. Admit the final support before copying its payload.
            let count = rows
                .len()
                .checked_add(1)
                .ok_or(StandingQueryError::Delivery(
                    StandingQueryFailure::ResultBudget,
                ))?;
            if meter
                .policy
                .rows
                .max_result_rows()
                .is_some_and(|limit| count as u128 > u128::from(limit))
            {
                return Err(StandingQueryError::Delivery(
                    StandingQueryFailure::ResultBudget,
                ));
            }
            let weight =
                orient_weight(weight, reverse, meter).map_err(StandingQueryError::Delivery)?;
            let cells = own_row(row, meter).map_err(StandingQueryError::Delivery)?;
            rows.accumulate(cells, weight, LIMBS, &mut |event| meter.charge(event))
                .map_err(zset_error)
                .map_err(StandingQueryError::Delivery)?;
        }
        (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
        Ok(rows)
    }

    fn select_difference(
        &self,
        before: CommitSeq,
        after: CommitSeq,
        meter: &mut Meter<'_>,
    ) -> Result<std::ops::Range<usize>, StandingQueryError> {
        let retained_after = self.retained_after();
        let frontier = self.frontier();
        // Validate BOTH requested cuts before inspecting any selected payload.
        // Do not turn equal unavailable cuts into an apparently exact empty bag.
        for cut in [before, after] {
            meter
                .charge(ZSetEvent::Work)
                .map_err(StandingQueryError::Delivery)?;
            if cut < retained_after || cut > frontier {
                return Err(StandingQueryError::ReplayGap {
                    after: cut,
                    retained_after,
                    frontier,
                });
            }
        }
        let low = before.min(after);
        let high = before.max(after);
        let invalid = || StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta);
        let start = usize::try_from(low.0 - retained_after.0).map_err(|_| invalid())?;
        let count = usize::try_from(high.0 - low.0).map_err(|_| invalid())?;
        let end = start
            .checked_add(count)
            .filter(|end| *end <= self.frames.len())
            .ok_or_else(invalid)?;
        let range = start..end;
        let mut next = low;
        for frame in self.frames.range(range.clone()) {
            meter
                .charge(ZSetEvent::Work)
                .map_err(StandingQueryError::Delivery)?;
            if frame.from != next || next.checked_successor().ok() != Some(frame.frontier) {
                return Err(invalid());
            }
            next = frame.frontier;
        }
        if next != high {
            return Err(invalid());
        }
        (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
        Ok(range)
    }

    fn select_range(
        &self,
        after: CommitSeq,
        max_ticks: NonZeroUsize,
        meter: &mut Meter<'_>,
    ) -> Result<Option<(CommitSeq, std::ops::Range<usize>)>, StandingQueryError> {
        // Validate the complete selected prefix before reading its row bags.
        // The same immutable history borrow pins these cuts during the merge.
        // Empty frames still participate in successor validation and metering.
        let mut next = self.next(after)?;
        let mut frontier = after;
        let mut selected = 0_usize;
        while let Some(frame) = next {
            meter
                .charge(ZSetEvent::Work)
                .map_err(StandingQueryError::Delivery)?;
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
        let end = start
            .checked_add(selected)
            .filter(|end| *end <= self.frames.len())
            .ok_or(StandingQueryError::Delivery(
                StandingQueryFailure::InvalidDelta,
            ))?;
        Ok(Some((frontier, start..end)))
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
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Head<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max heap. Reverse BOTH keys: equal rows must add
        // oldest tick first, preserving checked intermediate-overflow behavior.
        other
            .row
            .cmp(self.row)
            .then_with(|| other.source.cmp(&self.source))
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
        let mut merged = Self {
            sources: Vec::new(),
            heads: BinaryHeap::new(),
            done: false,
        };
        for mut source in sources {
            meter.charge(ZSetEvent::Work)?;
            if let Some((row, weight)) = source.next() {
                meter.units(ZSetEvent::ScratchEntry, 2)?;
                merged
                    .sources
                    .try_reserve(1)
                    .map_err(|_| StandingQueryFailure::ScratchBudget)?;
                merged
                    .heads
                    .try_reserve(1)
                    .map_err(|_| StandingQueryFailure::ScratchBudget)?;
                let at = merged.sources.len();
                merged.sources.push(source);
                merged.heads.push(Head {
                    row,
                    weight,
                    source: at,
                });
            }
        }
        Ok(merged)
    }

    fn next(
        &mut self,
        meter: &mut Meter<'_>,
    ) -> Result<Option<(&'a Vec<QueryValue>, ZWeight)>, StandingQueryFailure> {
        if self.done {
            return Ok(None);
        }
        // A failed or unwinding partial key can never be resumed as a complete
        // sum. The caller discards its tentative output on any such refusal.
        self.done = true;
        loop {
            meter.charge(ZSetEvent::Work)?;
            let Some(mut head) = self.heads.pop() else {
                return Ok(None);
            };
            let row = head.row;
            let mut total = ZWeight::ZERO;
            loop {
                total = add_weight(&total, head.weight, meter)?;
                meter.charge(ZSetEvent::Work)?;
                if let Some((next_row, next_weight)) = self.sources[head.source].next() {
                    if next_row <= head.row {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    self.heads.push(Head {
                        row: next_row,
                        weight: next_weight,
                        source: head.source,
                    });
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
    if weight.is_zero() {
        return Err(StandingQueryFailure::InvalidDelta);
    }
    let units = weight
        .magnitude_limb_count()
        .checked_add(1)
        .ok_or(StandingQueryFailure::ScratchBudget)?;
    meter.units(ZSetEvent::ScratchEntry, units)?;
    // Admit every input even if a later contribution cancels it. A checked
    // result alone must not launder an oversized promoted source weight.
    let weight = weight
        .checked_clone(LIMBS)
        .map_err(|_| StandingQueryFailure::Arithmetic)?;
    meter.charge(ZSetEvent::Work)?;
    let units = total
        .magnitude_limb_count()
        .max(weight.magnitude_limb_count())
        .checked_add(1)
        .ok_or(StandingQueryFailure::ScratchBudget)?;
    meter.units(ZSetEvent::ScratchEntry, units)?;
    total
        .checked_add(&weight, LIMBS)
        .map_err(|_| StandingQueryFailure::Arithmetic)
}

fn orient_weight(
    weight: ZWeight,
    reverse: bool,
    meter: &mut Meter<'_>,
) -> Result<ZWeight, StandingQueryFailure> {
    if !reverse {
        return Ok(weight);
    }
    // Negate only AFTER chronological consolidation. Reversing tick traversal
    // would change the accepted intermediate sums under the exact limb bound.
    meter.charge(ZSetEvent::Work)?;
    let units = weight
        .magnitude_limb_count()
        .checked_add(1)
        .ok_or(StandingQueryFailure::ScratchBudget)?;
    meter.units(ZSetEvent::ScratchEntry, units)?;
    weight
        .checked_neg(LIMBS)
        .map_err(|_| StandingQueryFailure::Arithmetic)
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
        }
        .checked_add(1)
        .ok_or(StandingQueryFailure::ScratchBudget)?;
        meter.units(ZSetEvent::ScratchEntry, units)?;
        cells
            .try_reserve(1)
            .map_err(|_| StandingQueryFailure::ScratchBudget)?;
        cells.push(value.clone());
    }
    Ok(cells)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod difference_tests {
    use super::*;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::CanonicalScalar;

    fn budget(rows: u64) -> GqlQueryPolicy {
        GqlQueryPolicy::new(0, rows, 1_000_000, 1_000_000)
    }
    fn key(value: i64) -> Vec<QueryValue> {
        vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
            value,
        )))]
    }
    fn bag(counts: [i128; 2]) -> ZSet<Vec<QueryValue>> {
        ZSet::from_updates(
            counts
                .into_iter()
                .enumerate()
                .map(|(i, count)| (key(i as i64), ZWeight::from_i128(count))),
            LIMBS,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap()
    }
    fn append(history: &mut History, counts: [i128; 2]) {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: budget(2),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        history
            .prepare(
                history.frontier().checked_successor().unwrap(),
                bag(counts),
                Limits::new(16, 100, 100_000).unwrap(),
                &mut meter,
            )
            .unwrap()
            .commit();
    }
    fn run(
        history: &History,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> (
        Result<ZSet<Vec<QueryValue>>, StandingQueryError>,
        StandingQueryStats,
    ) {
        let mut meter = Meter {
            policy,
            stats: StandingQueryStats::default(),
            checkpoint,
        };
        let result = history.difference(before, after, &mut meter);
        (result, meter.stats)
    }

    #[test]
    fn every_endpoint_pair_matches_independent_snapshot_count_arithmetic() {
        for mut code in 0..729_u32 {
            let mut counts = [[0_i128; 2]; 3];
            for snapshot in &mut counts {
                for count in snapshot {
                    *count = i128::from(code % 3);
                    code /= 3;
                }
            }
            let mut history = History::new(CommitSeq(17));
            for pair in counts.windows(2) {
                append(
                    &mut history,
                    [pair[1][0] - pair[0][0], pair[1][1] - pair[0][1]],
                );
            }
            let window = history.window();
            for before in 0..3 {
                for after in 0..3 {
                    let actual = run(
                        &history,
                        CommitSeq(17 + before as u64),
                        CommitSeq(17 + after as u64),
                        budget(2),
                        &mut || Ok(()),
                    )
                    .0
                    .unwrap();
                    assert_eq!(
                        actual,
                        bag([
                            counts[after][0] - counts[before][0],
                            counts[after][1] - counts[before][1],
                        ])
                    );
                    assert_eq!(history.window(), window);
                }
            }
        }
    }

    #[test]
    fn exact_interval_excludes_later_ticks_and_preserves_promoted_reverse_weights() {
        let mut history = History::new(CommitSeq(u64::MAX - 3));
        append(&mut history, [23, 0]);
        append(&mut history, [i128::MIN, 0]);
        append(&mut history, [0, 99]);
        let low = CommitSeq(u64::MAX - 2);
        let high = CommitSeq(u64::MAX - 1);
        let forward = run(&history, low, high, budget(1), &mut || Ok(()))
            .0
            .unwrap();
        assert_eq!(forward, bag([i128::MIN, 0]));
        let reverse = run(&history, high, low, budget(1), &mut || Ok(()))
            .0
            .unwrap();
        let (row, weight) = reverse.iter().next().unwrap();
        let positive = ZWeight::from_i128(i128::MAX)
            .checked_add(&ZWeight::ONE, LIMBS)
            .unwrap();
        assert_eq!(reverse.len(), 1);
        assert_eq!(row, &key(0));
        assert_eq!(weight, &positive);
        assert!(weight.is_promoted());
    }

    #[test]
    fn equal_cuts_and_cancelled_intervals_need_no_final_result_allowance() {
        let mut history = History::new(CommitSeq(4));
        append(&mut history, [7, 0]);
        append(&mut history, [0, 0]);
        append(&mut history, [-7, 0]);
        for (before, after) in [(4, 4), (5, 5), (7, 7), (4, 7), (7, 4), (5, 6)] {
            assert!(
                run(
                    &history,
                    CommitSeq(before),
                    CommitSeq(after),
                    budget(0),
                    &mut || Ok(())
                )
                .0
                .unwrap()
                .is_empty()
            );
        }
        assert!(matches!(
            run(&history, CommitSeq(4), CommitSeq(5), budget(0), &mut || Ok(
                ()
            ))
            .0,
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            ))
        ));
    }

    #[test]
    fn unavailable_endpoints_and_interior_gaps_never_yield_partial_differences() {
        let mut history = History::new(CommitSeq(5));
        append(&mut history, [1, 0]);
        append(&mut history, [0, 1]);
        append(&mut history, [-1, 0]);
        let retired = history.frames.pop_front().unwrap();
        history.rows -= retired.rows.len();
        history.units -= retired.units;
        for (before, after, rejected) in [
            (5, 8, 5),
            (8, 5, 5),
            (5, 5, 5),
            (6, 9, 9),
            (9, 6, 9),
            (9, 9, 9),
        ] {
            assert!(matches!(
                run(&history, CommitSeq(before), CommitSeq(after), budget(2), &mut || Ok(())).0,
                Err(StandingQueryError::ReplayGap { after, retained_after: CommitSeq(6), frontier: CommitSeq(8) })
                    if after == CommitSeq(rejected)
            ));
        }
        assert_eq!(
            run(&history, CommitSeq(6), CommitSeq(7), budget(2), &mut || Ok(
                ()
            ))
            .0
            .unwrap(),
            bag([0, 1])
        );
        Arc::get_mut(history.frames.back_mut().unwrap())
            .unwrap()
            .from = CommitSeq(6);
        // Corruption outside a selected interval is not accidentally consumed.
        assert_eq!(
            run(&history, CommitSeq(6), CommitSeq(7), budget(2), &mut || Ok(
                ()
            ))
            .0
            .unwrap(),
            bag([0, 1])
        );
        for (before, after) in [(6, 8), (8, 6)] {
            assert!(matches!(
                run(
                    &history,
                    CommitSeq(before),
                    CommitSeq(after),
                    budget(2),
                    &mut || Ok(())
                )
                .0,
                Err(StandingQueryError::Delivery(
                    StandingQueryFailure::InvalidDelta
                ))
            ));
        }
    }

    #[test]
    fn every_direction_checkpoint_and_budget_refusal_preserves_history_for_retry() {
        let mut history = History::new(CommitSeq(2));
        append(&mut history, [4, -2]);
        append(&mut history, [-3, 0]);
        append(&mut history, [0, 5]);
        let window = history.window();
        let frames = history.frames.iter().cloned().collect::<Vec<_>>();
        for (before, after, expected) in [(2, 5, [1, 3]), (5, 2, [-1, -3])] {
            let mut calls = 0;
            let (result, stats) = run(
                &history,
                CommitSeq(before),
                CommitSeq(after),
                budget(2),
                &mut || {
                    calls += 1;
                    Ok(())
                },
            );
            assert_eq!(result.unwrap(), bag(expected));
            for stop in 1..=calls {
                let mut seen = 0;
                assert!(matches!(
                    run(
                        &history,
                        CommitSeq(before),
                        CommitSeq(after),
                        budget(2),
                        &mut || {
                            seen += 1;
                            if seen == stop {
                                Err(StandingQueryFailure::Interrupted)
                            } else {
                                Ok(())
                            }
                        }
                    )
                    .0,
                    Err(StandingQueryError::Delivery(
                        StandingQueryFailure::Interrupted
                    ))
                ));
                assert_eq!(seen, stop);
                assert_eq!(history.window(), window);
                for (actual, original) in history.frames.iter().zip(&frames) {
                    assert!(Arc::ptr_eq(actual, original));
                }
                assert_eq!(
                    run(
                        &history,
                        CommitSeq(before),
                        CommitSeq(after),
                        budget(2),
                        &mut || Ok(())
                    )
                    .0
                    .unwrap(),
                    bag(expected)
                );
            }
            for (work, scratch, rows, passes) in [
                (stats.work_units, stats.scratch_entries, 2, true),
                (stats.work_units - 1, stats.scratch_entries, 2, false),
                (stats.work_units, stats.scratch_entries - 1, 2, false),
                (stats.work_units, stats.scratch_entries, 1, false),
            ] {
                let result = run(
                    &history,
                    CommitSeq(before),
                    CommitSeq(after),
                    GqlQueryPolicy::new(0, rows, work, scratch),
                    &mut || Ok(()),
                )
                .0;
                assert_eq!(result.is_ok(), passes);
                assert_eq!(history.window(), window);
            }
        }
    }
}
