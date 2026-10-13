//! Opt-in net replay catch-up with the existing one-pending-receipt lifecycle.

use super::*;
use std::num::NonZeroUsize;

impl NativeSubscription {
    /// Poll a baseline or one exact NET transition spanning up to `max_ticks`
    /// consecutive retained commits. This is for synchronizing result state,
    /// not for observing every per-commit intermediate result or side effect.
    /// Ordinary poll keeps its atomic-tick selection policy.
    ///
    /// Enable replay before falling behind. Without replay this preserves the
    /// ordinary one-tick poll/gap behavior; it never reexecutes the query to
    /// synthesize a missing range. A baseline still uses the ordinary native
    /// bag path. Empty coalesced changes require ACK because their frontier
    /// advanced, while None means caught up without another frame.
    ///
    /// One new frame uses a single cumulative work/scratch allowance and a
    /// final NET-tuple quota. A refusal leaves the ACK, receipt serial and
    /// pending frame untouched; it never returns a smaller successful prefix
    /// merely to fit a budget. Retry with another bound or allowance explicitly.
    /// Replay retention and its per-commit maintenance policy are unchanged.
    ///
    /// An already pending frame is shared unchanged, even when a later poll
    /// requests a smaller range, uses ordinary poll, or its history is evicted.
    /// Apply the whole batch once, then acknowledge its one receipt. That ACK
    /// advances to the batch frontier, not to a caller-selected intermediate cut.
    /// Fork receipts, idempotent ACKs, restart/close and health fences retain
    /// the ordinary subscription rules. No durable resume or wire mode is added.
    pub fn poll_coalesced<V: Vfs + Clone>(
        &mut self,
        database: &Database<V>,
        cx: &QueryCx,
        max_ticks: NonZeroUsize,
        policy: GqlQueryPolicy,
    ) -> Result<Option<Arc<SubscriptionBatch>>, SubscriptionError> {
        if self.closed {
            return Err(SubscriptionError::Closed);
        }
        // Baselines, redelivery and non-replay consumers keep exactly the
        // existing source admission, allocation and publication path.
        if self.pending.is_some() {
            return self.poll(database, cx, policy);
        }
        let (Some(from), Some(replay)) = (self.acknowledged, self.replay.as_ref()) else {
            return self.poll(database, cx, policy);
        };
        let current = database
            .admitted_standing_query(cx, &self.handle)?
            .status()
            .1;
        database.standing_replay_window(cx, replay)?;
        if from == current {
            return Ok(None);
        }
        self.serial
            .checked_add(1)
            .ok_or(SubscriptionError::ReceiptExhausted)?;
        let (frontier, rows) = database
            .standing_replay_coalesced(cx, replay, from, max_ticks, policy)?
            .ok_or(StandingQueryError::Delivery(
                StandingQueryFailure::InvalidDelta,
            ))?;
        if frontier <= from
            || frontier > current
            || u128::from(frontier.0 - from.0) > max_ticks.get() as u128
        {
            return Err(StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta).into());
        }
        // No consumer mutation precedes this final checkpoint. The existing
        // publication owns the complete bag and mints one monotone receipt.
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.publish(frontier, rows).map(Some)
    }
}

#[cfg(test)]
mod tests;
