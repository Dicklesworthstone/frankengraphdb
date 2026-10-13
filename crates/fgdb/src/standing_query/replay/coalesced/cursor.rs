//! Borrowed net replay delivery without a collected output bag.

use super::*;

/// One borrowed consolidated replay change and its weight.
type ReplayChange<'a> = Result<(&'a [QueryValue], ZWeight), StandingQueryError>;

impl<V: Vfs + Clone> Database<V> {
    /// Open a pull iterator over the net changes in a complete retained prefix.
    /// The tuple's sequence names the SELECTED end, not successful delivery.
    /// Stage each change privately; apply the transition and advance from
    /// `after` only after exhausting the iterator without an error. Dropping
    /// it early is not completion. This API never acknowledges a subscription.
    ///
    /// None means already caught up. Some may yield no rows when changes
    /// cancel, but its end still advances across all selected empty ticks.
    /// Gaps, foreign handles and unavailable replay sinks refuse at opening.
    /// The database borrow pins the selected history and prevents concurrent
    /// writes/rebuild/eviction through this Database until borrows are released.
    /// Dropping the caller's replay handle does not invalidate the iterator.
    ///
    /// Each item contains borrowed native cells and an owned exact signed
    /// weight. Columns match the replay source. Full-width identities and
    /// numeric representations are not coerced. No payload clone, occurrence
    /// expansion, graph scan, query execution or output bag is performed.
    /// Merge state is one iterator/head per nonempty selected tick plus one
    /// exact accumulator. Retained history and caller-owned results remain
    /// outside this transient bound; it is not a live byte cap or spill.
    ///
    /// Opening and all pulls share one cumulative work/scratch policy.
    /// ResultRows counts successfully delivered nonzero net keys. It is
    /// checked after a key's complete sum, so cancellations cost no rows.
    /// Arithmetic, budget or interruption can fail AFTER earlier items; one
    /// error permanently fuses delivery and releases the merge without drain.
    /// Even the final EOF checkpoint must succeed. Retrying by reopening does
    /// not consume history or change an ACK. Use standing_replay_coalesced for
    /// a materialized, all-or-nothing result instead. Neither is an event feed,
    /// durable cursor, receipt or capability-authorized transport operation.
    pub fn standing_replay_coalesced_cursor<'a>(
        &'a self,
        cx: &'a QueryCx,
        replay: &StandingQueryHandle,
        after: CommitSeq,
        max_ticks: NonZeroUsize,
        policy: GqlQueryPolicy,
    ) -> Result<
        Option<(
            CommitSeq,
            impl core::iter::FusedIterator<Item = ReplayChange<'a>> + 'a + use<'a, V>,
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
            let Some((frontier, range)) =
                state.history.select_range(after, max_ticks, &mut meter)?
            else {
                return Ok(None);
            };
            let sources = state
                .history
                .frames
                .range(range)
                .map(|frame| frame.rows.iter());
            let merge =
                SortedMerge::new(sources, &mut meter).map_err(StandingQueryError::Delivery)?;
            (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
            let mut pull = Pull {
                merge: Some(merge),
                delivered: 0,
                policy,
                stats: meter.stats,
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
            Ok(Some((frontier, cursor)))
        })
    }
}

struct Pull<'a, I> {
    merge: Option<SortedMerge<'a, I>>,
    delivered: u64,
    policy: GqlQueryPolicy,
    stats: StandingQueryStats,
}

impl<'a, I: Iterator<Item = Entry<'a>>> Pull<'a, I> {
    fn advance(
        &mut self,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Option<Result<(&'a [QueryValue], ZWeight), StandingQueryError>> {
        // Take ownership before any fallible/user code. A refusal or unwind
        // drops the merge and cannot resume a partly summed or undelivered key.
        let mut merge = self.merge.take()?;
        let mut meter = Meter {
            policy: self.policy,
            stats: self.stats,
            checkpoint,
        };
        let result = (|| {
            let Some((row, weight)) = merge.next(&mut meter)? else {
                (meter.checkpoint)()?;
                return Ok(None);
            };
            let delivered = self
                .delivered
                .checked_add(1)
                .ok_or(StandingQueryFailure::ResultBudget)?;
            if self
                .policy
                .rows
                .max_result_rows()
                .is_some_and(|limit| delivered > limit)
            {
                return Err(StandingQueryFailure::ResultBudget);
            }
            // The caller gets a reference to immutable retained cells; no
            // payload scratch is charged for a clone that never happens.
            (meter.checkpoint)()?;
            Ok(Some((row.as_slice(), weight, delivered)))
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
