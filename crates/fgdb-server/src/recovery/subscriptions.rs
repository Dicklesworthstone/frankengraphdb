//! Private changefeed registrations follow the connection's owned consumer.
//! Drop only links an already allocated retirement node and wakes the EXISTING
//! host worker. No client task, detached task, I/O or query work is needed to
//! transfer cleanup. Database-exclusive reclamation precedes slot refunds.

use super::*;
use fgdb::{
    NativeSubscription, StandingQueryError, StandingQueryFailure, SubscriptionBatch,
    SubscriptionError, SubscriptionReceipt,
};
use fgdb_gql::GqlQueryPolicy;
use fgdb_types::{CommitSeq, QueryCx};
use std::sync::Weak;

pub(super) struct Retirement {
    consumer: NativeSubscription,
    generation: u64,
    next: Option<Box<Retirement>>,
}

impl Drop for Retirement {
    fn drop(&mut self) {
        // A large operator-configured cap must not turn recovery's discard
        // into a recursively dropped list and exhaust the worker's stack.
        let mut next = self.next.take();
        while let Some(mut item) = next {
            next = item.next.take();
        }
    }
}

/// The server never exposes a producer/replay handle or forks this private
/// registration. Keep the native consumer behind only the delivery operations
/// that preserve its ownership; do not implement Deref to its wider API.
pub(crate) struct OwnedSubscription {
    gate: Weak<Gate>,
    retirement: Option<Box<Retirement>>,
}

impl Generation {
    pub(crate) fn own_subscription(&self, consumer: NativeSubscription) -> OwnedSubscription {
        debug_assert!(consumer.can_release_registration());
        OwnedSubscription {
            gate: Arc::downgrade(&self.gate),
            retirement: Some(Box::new(Retirement {
                consumer,
                generation: self.value,
                next: None,
            })),
        }
    }
}

impl OwnedSubscription {
    fn consumer(&mut self) -> Result<&mut NativeSubscription, SubscriptionError> {
        self.retirement
            .as_mut()
            .map(|item| &mut item.consumer)
            .ok_or(SubscriptionError::Closed)
    }

    pub(crate) fn poll(
        &mut self,
        database: &Database,
        cx: &QueryCx,
        policy: GqlQueryPolicy,
    ) -> Result<Option<Arc<SubscriptionBatch>>, SubscriptionError> {
        self.consumer()?.poll(database, cx, policy)
    }

    pub(crate) fn acknowledge(
        &mut self,
        receipt: &SubscriptionReceipt,
    ) -> Result<CommitSeq, SubscriptionError> {
        self.consumer()?.acknowledge(receipt)
    }

    pub(crate) fn restart_from_current(&mut self) -> Result<(), SubscriptionError> {
        self.consumer()?.restart_from_current()
    }

    #[cfg(test)]
    pub(crate) fn acknowledged_frontier(&self) -> Option<CommitSeq> {
        self.retirement
            .as_ref()
            .and_then(|item| item.consumer.acknowledged_frontier())
    }

    /// Idempotent, allocation-free transfer. The count remains reserved until
    /// the native circuit really retires. Old generations never refund a new
    /// generation's slots, including an old consumer dropped after recovery.
    pub(crate) fn close(&mut self) {
        let Some(mut item) = self.retirement.take() else {
            return;
        };
        item.consumer.close();
        let Some(gate) = self.gate.upgrade() else {
            return;
        };
        {
            let mut state = gate.state.write().unwrap_or_else(PoisonError::into_inner);
            if state.phase != Phase::Ready || state.generation != item.generation {
                // Recovery/retirement owns the entire old registry. No handle
                // from that registry may be applied to its replacement.
                return;
            }
            item.next = state.retired.take();
            state.retired = Some(item);
        }
        gate.signal.committed();
    }
}

impl Drop for OwnedSubscription {
    fn drop(&mut self) {
        self.close();
    }
}

impl Shared {
    /// Called under the database write lock, never the lifecycle gate. Taking
    /// one item into an owned guard makes every refusal/unwind requeue it; no
    /// half-released registration can lose its cleanup obligation.
    pub(super) fn release_subscriptions(
        &self,
        cx: &Cx,
        database: &mut Database,
        generation: u64,
    ) -> Result<(), Unavailable> {
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query = crate::query_cx(&contexts);
        loop {
            let item = {
                let mut state = self
                    .gate
                    .state
                    .write()
                    .unwrap_or_else(PoisonError::into_inner);
                let Some(mut item) = state.retired.take() else {
                    return Ok(());
                };
                state.retired = item.next.take();
                item
            };
            if item.generation != generation {
                // Only recovery can replace the database, under this same
                // exclusive lock. That replacement already retired these nodes.
                drop(item);
                continue;
            }
            let mut owned = OwnedSubscription {
                gate: Arc::downgrade(&self.gate),
                retirement: Some(item),
            };
            let result = owned
                .retirement
                .as_mut()
                .expect("owned retirement")
                .consumer
                .close_and_release_for_shutdown(database, &query);
            if let Err(error) = result {
                if matches!(
                    &error,
                    SubscriptionError::Query(StandingQueryError::Interrupted(_))
                        | SubscriptionError::Query(StandingQueryError::Delivery(
                            StandingQueryFailure::Interrupted
                        ))
                ) {
                    // A request-side drain can be cancelled. The host worker
                    // retries using its own context, not the client's token.
                    return Err(Unavailable::Stopping);
                }
                self.failed(format!("private subscription cleanup refused: {error}"));
                return Err(Unavailable::Fenced);
            }
            // Invalidate the queued obligation before refunding. Recovery
            // cannot reset the counter while this database lock is held.
            drop(owned.retirement.take());
            let refunded = {
                let mut state = self
                    .gate
                    .state
                    .write()
                    .unwrap_or_else(PoisonError::into_inner);
                let refunded = self
                    .subscriptions
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                        count.checked_sub(1)
                    })
                    .is_ok();
                if refunded {
                    state.released_subscriptions = state.released_subscriptions.saturating_add(1);
                }
                refunded
            };
            if !refunded {
                self.failed("subscription registration accounting underflow".to_owned());
                return Err(Unavailable::Fenced);
            }
            self.gate.signal.committed();
        }
    }
}

impl crate::Server {
    /// Trusted-host subscription diagnostics: (allocated registrations,
    /// cumulative released registrations). Allocated includes pending cleanup
    /// until its circuit is actually removed; recovery releases the old
    /// generation as a whole. The released diagnostic saturates at u64::MAX.
    /// This is not exposed by the unauthenticated network health endpoint.
    pub fn subscription_counts(&self, cx: &Cx, name: &str) -> Option<(usize, u64)> {
        cx.checkpoint().ok()?;
        let database = self.databases.get(name)?;
        let state = database
            .db
            .shared
            .gate
            .state
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        Some((
            database.subscriptions.load(Ordering::Acquire),
            state.released_subscriptions,
        ))
    }
}

#[cfg(test)]
mod tests;
