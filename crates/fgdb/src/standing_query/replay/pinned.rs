//! Owned, bounded delivery of a fixed retained prefix of complete result ticks.
//!
//! The cursor pins immutable frames, not the database or its mutable registry.
//! No row payload is read or copied when opening, and pulling moves one already
//! admitted Arc. The pin allowance is separate from cumulative delivery work.

mod merged;

use super::*;
use std::num::NonZeroUsize;

impl<V: Vfs + Clone> Database<V> {
    /// Pin up to `max_ticks` complete successors of `after` for independent
    /// delivery. The selected end and cursor borrow only `cx`, not this database,
    /// the source handle or the replay handle. Writers may continue, retention
    /// may evict, and the sink may rebuild or the database may drop while this
    /// cursor drains exactly the immutable prefix accepted at opening.
    ///
    /// This delivers original atomic ticks, NOT coalesced net differences.
    /// Empty ticks survive and advance their from/frontier chain. None means
    /// caught up at opening; gaps and unhealthy/foreign handles refuse. The
    /// returned end is a selected cut, not evidence of completed delivery. A
    /// failure can follow earlier complete frames; advance consumer state only
    /// for complete frames actually applied once, never straight to that end.
    /// No poll, acknowledgement, subscription receipt or producer state changes.
    ///
    /// `max_pinned_payload_units` bounds the SUM of the selected frames' existing
    /// logical payload units, including empty-frame slots and exact weight limbs.
    /// An over-budget selected prefix refuses as ScratchBudget, never silently
    /// shortens itself. These pins may extend lifetime beyond replay retention;
    /// they are not free memory merely because their bytes are shared. This is
    /// a per-cursor logical bound, not an allocator-byte or global host pin cap.
    /// References retained by the caller after delivery have their own lifetime.
    ///
    /// Opening and every pull share one work/scratch allowance. ResultRows
    /// counts the cumulative compressed support across delivered ticks (not net
    /// support or expanded multiplicity). A whole frame passes admission before
    /// release. Error, unwind, EOF and drop release all remaining pins without
    /// draining; subsequent pulls on a finished cursor do not invoke controls.
    /// Every pull, including EOF, checks QueryCx cancellation. No graph reads,
    /// query execution, payload cloning or weight narrowing occur.
    ///
    /// This privileged embedded API releases already accepted historical data.
    /// Later source failure or rebuild cannot rewrite its pinned frames. It is
    /// not live capability reauthorization, durable resume or a retention lease.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub fn standing_replay_owned_cursor<'cx>(
        &self,
        cx: &'cx QueryCx,
        replay: &StandingQueryHandle,
        after: CommitSeq,
        max_ticks: NonZeroUsize,
        max_pinned_payload_units: usize,
        policy: GqlQueryPolicy,
    ) -> Result<
        Option<(
            CommitSeq,
            impl core::iter::FusedIterator<Item = Result<Arc<StandingReplayBatch>, StandingQueryError>>
            + 'cx
            + use<'cx, V>,
        )>,
        StandingQueryError,
    > {
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
            let Some((end, mut pull)) = PinnedReplay::open(
                &state.history,
                after,
                max_ticks,
                max_pinned_payload_units,
                &mut meter,
            )?
            else {
                return Ok(None);
            };
            let cursor = std::iter::from_fn(move || {
                cx.with_restriction(|| {
                    pull.advance(&mut || {
                        cx.checkpoint()
                            .map_err(|_| StandingQueryFailure::Interrupted)
                    })
                })
            })
            .fuse();
            Ok(Some((end, cursor)))
        })
    }
}

struct PinnedReplay {
    frames: Option<VecDeque<Arc<StandingReplayBatch>>>,
    policy: GqlQueryPolicy,
    stats: StandingQueryStats,
    delivered: u64,
}

impl PinnedReplay {
    fn open(
        history: &History,
        after: CommitSeq,
        max_ticks: NonZeroUsize,
        max_pinned_payload_units: usize,
        meter: &mut Meter<'_>,
    ) -> Result<Option<(CommitSeq, Self)>, StandingQueryError> {
        let mut frames = VecDeque::new();
        let mut end = after;
        let mut units = 0_usize;
        loop {
            meter
                .charge(ZSetEvent::Work)
                .map_err(StandingQueryError::Delivery)?;
            // The shared successor lookup checks retained coverage and exact
            // continuity. Privately pinned earlier frames escape no failed open.
            let Some(frame) = history.next(end)? else {
                break;
            };
            units = units
                .checked_add(frame.units)
                .filter(|units| *units <= max_pinned_payload_units)
                .ok_or(StandingQueryError::Delivery(
                    StandingQueryFailure::ScratchBudget,
                ))?;
            meter
                .charge(ZSetEvent::ScratchEntry)
                .map_err(StandingQueryError::Delivery)?;
            frames
                .try_reserve(1)
                .map_err(|_| StandingQueryError::Delivery(StandingQueryFailure::ScratchBudget))?;
            frames.push_back(Arc::clone(frame));
            end = frame.frontier;
            if frames.len() == max_ticks.get() {
                break;
            }
        }
        (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
        if frames.is_empty() {
            return Ok(None);
        }
        Ok(Some((
            end,
            Self {
                frames: Some(frames),
                policy: meter.policy,
                stats: meter.stats,
                delivered: 0,
            },
        )))
    }

    fn advance(
        &mut self,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Option<Result<Arc<StandingReplayBatch>, StandingQueryError>> {
        // No callback runs while the original state still owns the pins. Any
        // refusal or unwind drops this local queue and permanently fuses us.
        let mut frames = self.frames.take()?;
        let mut meter = Meter {
            policy: self.policy,
            stats: self.stats,
            checkpoint,
        };
        let result = (|| {
            meter.charge(ZSetEvent::Work)?;
            let Some(frame) = frames.front() else {
                (meter.checkpoint)()?;
                return Ok(None);
            };
            let support =
                u64::try_from(frame.rows.len()).map_err(|_| StandingQueryFailure::ResultBudget)?;
            let delivered = self
                .delivered
                .checked_add(support)
                .ok_or(StandingQueryFailure::ResultBudget)?;
            if self
                .policy
                .rows
                .max_result_rows()
                .is_some_and(|limit| delivered > limit)
            {
                return Err(StandingQueryFailure::ResultBudget);
            }
            (meter.checkpoint)()?;
            Ok(Some(delivered))
        })();
        self.stats = meter.stats;
        match result {
            Ok(Some(delivered)) => {
                let frame = frames.pop_front().expect("admitted complete front frame");
                self.delivered = delivered;
                self.frames = Some(frames);
                Some(Ok(frame))
            }
            Ok(None) => None,
            Err(error) => Some(Err(StandingQueryError::Delivery(error))),
        }
    }
}

#[cfg(test)]
mod tests;
