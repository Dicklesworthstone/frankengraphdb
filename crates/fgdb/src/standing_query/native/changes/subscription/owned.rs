//! Explicit reclamation of an unshared, session-local subscription circuit.
//!
//! The registration callback can see only names and an immutable result batch,
//! never a source handle. Issuing either handle or successfully forking changes
//! ownership permanently to shared. Only a still-private owner may free slots.

use super::*;
use crate::{NativeSubscriptionSetup, PreparedNativeRead};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
pub(super) struct Ownership {
    range: Range<usize>,
    shared: AtomicBool,
    released: bool,
}

/// An unwind is as much a refused private registration as a returned error.
/// No handle is exposed by subscribe_replaying's admission callback, so its
/// entire unpublished suffix can be discarded without invalidating any alias.
struct Registration<'a, V: Vfs + Clone> {
    database: &'a mut Database<V>,
    first: usize,
    accepted: bool,
}
impl<V: Vfs + Clone> Drop for Registration<'_, V> {
    fn drop(&mut self) {
        if !self.accepted {
            self.database.standing_queries.truncate(self.first);
        }
    }
}

impl PreparedNativeRead {
    /// Atomically register a PRIVATE replaying subscription and admit its first
    /// baseline through the ordinary subscription executor. Policies, native
    /// result semantics, replay retention and ACK rules are unchanged.
    ///
    /// Unlike subscribe_replaying, the returned consumer can explicitly reclaim
    /// its source circuit and replay state with close_and_release. Poll, ACK,
    /// coalesced catch-up and restart keep that ownership private. Requesting
    /// handle()/replay_handle() or successfully calling fork() permanently makes
    /// it shared and disables reclamation, even if the reference is not cloned.
    /// This conservative escape rule protects aliases and dependent queries
    /// without guessing Arc counts or reusing an externally visible index.
    /// Obtain initial column names in `admit`, rather than exposing a handle.
    ///
    /// Admission errors AND unwinds discard only this call's private suffix.
    /// An accepted consumer's Drop or ordinary close never unregisters anything;
    /// resource release remains explicit. This is in-memory ownership, not a
    /// durable subscription, output-release receipt, or base-data deletion.
    pub fn subscribe_scoped<V: Vfs + Clone, T, E>(
        &self,
        database: &mut Database<V>,
        cx: &QueryCx,
        params: &GqlParameters,
        setup: NativeSubscriptionSetup,
        admit: impl FnOnce(&[String], &Arc<SubscriptionBatch>) -> Result<T, E>,
    ) -> Result<(NativeSubscription, T), E>
    where
        E: From<SubscriptionError>,
    {
        let first = database.standing_queries.len();
        let mut registration = Registration {
            database,
            first,
            accepted: false,
        };
        let (mut consumer, output) =
            self.subscribe_replaying(&mut *registration.database, cx, params, setup, admit)?;
        let end = registration.database.standing_queries.len();
        if consumer.handle.index < first
            || consumer.handle.index.checked_add(2) != Some(end)
            || consumer
                .replay
                .as_ref()
                .is_none_or(|replay| replay.index.checked_add(1) != Some(end))
        {
            return Err(SubscriptionError::from(StandingQueryError::Unsupported).into());
        }
        cx.checkpoint()
            .map_err(StandingQueryError::Interrupted)
            .map_err(SubscriptionError::from)?;
        consumer.ownership = Some(Ownership {
            range: first..end,
            shared: AtomicBool::new(false),
            released: false,
        });
        registration.accepted = true;
        Ok((consumer, output))
    }
}

impl NativeSubscription {
    pub(super) fn share_registration(&self) {
        if let Some(ownership) = &self.ownership {
            ownership.shared.store(true, Ordering::Release);
        }
    }

    /// Whether this consumer still exclusively owns a scoped registration.
    /// Reading this flag does not expose a handle or change ownership.
    pub fn can_release_registration(&self) -> bool {
        self.ownership.as_ref().is_some_and(|ownership| {
            !ownership.released && !ownership.shared.load(Ordering::Acquire)
        })
    }

    /// Host-owned terminal cleanup after a request has ended or lost authority.
    /// This performs the SAME private-owner release as close_and_release, but
    /// bounds its metadata-only work from the registry already owned by the
    /// host. It does not need to renew an expired client's query allowance.
    ///
    /// At most N slots are validated, N retirement slots reserved and N suffix
    /// slots inspected. The finite 3*N+1 work and N scratch ceilings cover that
    /// existing release transaction, not graph reads, maintenance or delivery.
    /// Foreign/shared ownership still refuses. Cancellation still preserves
    /// the registration: supervisors must use their own live cleanup context
    /// and retain the consumer until release succeeds or its database retires.
    pub fn close_and_release_for_shutdown<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &QueryCx,
    ) -> Result<usize, SubscriptionError> {
        let overflow = || {
            SubscriptionError::from(StandingQueryError::Delivery(
                StandingQueryFailure::ScratchBudget,
            ))
        };
        let slots = u64::try_from(database.standing_queries.len()).map_err(|_| overflow())?;
        let work = slots
            .checked_mul(3)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(overflow)?;
        self.close_and_release(database, cx, GqlQueryPolicy::new(0, 0, work, slots))
    }

    /// Explicitly close this delivery position and reclaim its PRIVATE source
    /// circuit and replay cache. Returns the number of registry nodes released;
    /// repeating a successful call on the same database returns zero.
    ///
    /// Only subscribe_scoped mints this ownership. A handle escape or successful
    /// fork refuses with SharedRegistration and changes nothing. Ordinary shared
    /// registrations refuse with NotPrivate. No live external handle can name a
    /// freed slot; this consumer's hidden handles are invalidated before reuse.
    /// Later registrations keep their indices, and only a vacant suffix is
    /// trimmed. Interior vacancies retain small slot metadata, not query state.
    ///
    /// Work and scratch meter validation, retirement staging and suffix checks,
    /// not graph rows. Every refusal/unwind before publication leaves both the
    /// circuit and this consumer unchanged and retryable. Failed maintained
    /// sources may be released without rebuilding them; database ownership and
    /// cancellation are still checked. No graph I/O or durable write occurs.
    ///
    /// This explicitly abandons a pending in-process batch, just as close does;
    /// it never acknowledges its receipt or commits consumer side effects.
    /// Caller-held batch Arcs remain valid and outside reclaimed-cache ownership.
    pub fn close_and_release<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &QueryCx,
        policy: GqlQueryPolicy,
    ) -> Result<usize, SubscriptionError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        if !Arc::ptr_eq(&database.handle_owner, &self.handle.owner) {
            return Err(StandingQueryError::ForeignHandle.into());
        }
        let ownership = self
            .ownership
            .as_ref()
            .ok_or(SubscriptionError::NotPrivate)?;
        if ownership.released {
            return Ok(0);
        }
        if ownership.shared.load(Ordering::Acquire) {
            return Err(SubscriptionError::SharedRegistration);
        }
        let range = ownership.range.clone();
        if self.handle.index < range.start
            || self.handle.index.checked_add(2) != Some(range.end)
            || self.replay.as_ref().is_none_or(|replay| {
                !Arc::ptr_eq(&database.handle_owner, &replay.owner)
                    || replay.index.checked_add(1) != Some(range.end)
            })
        {
            return Err(StandingQueryError::UnknownHandle.into());
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
            let count = range.end - range.start;
            let prepared = Release::prepare(&mut database.standing_queries, range, &mut meter)
                .map_err(StandingQueryError::Delivery)?;
            // No recoverable operation follows. Keep removed payloads alive
            // until the registry and consumer are coherently closed together.
            let pending = self.pending.take();
            let replay = self.replay.take();
            let layout = self.handle.native.take();
            self.handle.index = usize::MAX;
            self.closed = true;
            self.acknowledged = None;
            self.last_ack = None;
            if let Some(ownership) = &mut self.ownership {
                ownership.released = true;
            }
            prepared.commit();
            drop((pending, replay, layout));
            Ok(count)
        })
    }
}

#[must_use = "dropping a prepared release preserves every registered node"]
struct Release<'a> {
    queries: &'a mut Vec<StandingQuery>,
    range: Range<usize>,
    keep: usize,
    retired: Vec<StandingQuery>,
}
impl<'a> Release<'a> {
    fn prepare(
        queries: &'a mut Vec<StandingQuery>,
        range: Range<usize>,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        if range.is_empty() || range.end > queries.len() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        for query in &queries[range.clone()] {
            meter.charge(ZSetEvent::Work)?;
            if matches!(query, StandingQuery::Released) {
                return Err(StandingQueryFailure::InvalidDelta);
            }
        }
        let count = range.end - range.start;
        meter.units(ZSetEvent::ScratchEntry, count)?;
        let mut retired = Vec::new();
        retired
            .try_reserve_exact(count)
            .map_err(|_| StandingQueryFailure::ScratchBudget)?;
        let mut keep = queries.len();
        while keep > 0 {
            meter.charge(ZSetEvent::Work)?;
            if range.contains(&(keep - 1)) || matches!(queries[keep - 1], StandingQuery::Released) {
                keep -= 1;
            } else {
                break;
            }
        }
        (meter.checkpoint)()?;
        Ok(Self {
            queries,
            range,
            keep,
            retired,
        })
    }

    fn commit(mut self) {
        for index in self.range {
            self.retired.push(std::mem::replace(
                &mut self.queries[index],
                StandingQuery::Released,
            ));
        }
        self.queries.truncate(self.keep);
        drop(self.retired);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::context::SimulationCheckpointProbe;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    #[test]
    fn host_cleanup_retires_real_nodes_and_cancelled_cleanup_keeps_them_retryable() {
        let ((), report) = run_async_under_lab(0x79a0_04f1, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let mut database = Database::open_memory(
                &contexts.commit(),
                crate::DatabaseKeys::new(
                    [0xa1; 32],
                    DatabaseSecurityNamespaceId([0xa2; 32]),
                    [0xa3; 32],
                ),
            )
            .await
            .unwrap();
            let params = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare_subscription(
                &cx,
                "SUBSCRIBE TO RETURN 7 AS value",
                &params,
                fgdb_gql::RelationBind::new(),
            )
            .unwrap();
            let setup = || NativeSubscriptionSetup {
                maintenance: GqlQueryPolicy::new(1000, 1000, 100_000, 100_000),
                delivery: GqlQueryPolicy::new(1000, 1000, 100_000, 100_000),
                retention: [4, 1000, 100_000],
            };
            let (mut consumer, batch) = prepared
                .subscribe_scoped(&mut database, &cx, &params, setup(), |_, batch| {
                    Ok::<_, SubscriptionError>(Arc::clone(batch))
                })
                .unwrap();
            let count = database.standing_queries.len();
            assert!(count >= 2);
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
            assert_eq!(
                consumer
                    .close_and_release_for_shutdown(&mut database, &observed)
                    .unwrap(),
                count
            );
            assert!(database.standing_queries.is_empty());
            assert!(consumer.is_closed());
            assert_eq!(
                consumer
                    .close_and_release_for_shutdown(&mut database, &cx)
                    .unwrap(),
                0
            );
            assert_eq!(batch.rows().len(), 1); // caller's accepted frame survives
            assert!(probe.calls() > 1);
            for stop in 1..=probe.calls() {
                let (mut consumer, _) = prepared
                    .subscribe_scoped(&mut database, &cx, &params, setup(), |_, _| {
                        Ok::<_, SubscriptionError>(())
                    })
                    .unwrap();
                let control = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let interrupted = cx.with_checkpoint_probe(Arc::clone(&control));
                assert!(
                    consumer
                        .close_and_release_for_shutdown(&mut database, &interrupted)
                        .is_err()
                );
                assert_eq!(control.calls(), stop);
                assert_eq!(database.standing_queries.len(), count);
                assert!(!consumer.is_closed());
                assert!(consumer.can_release_registration());
                assert_eq!(
                    consumer
                        .close_and_release_for_shutdown(&mut database, &cx)
                        .unwrap(),
                    count
                );
                assert!(database.standing_queries.is_empty());
            }
            let (mut shared, _) = prepared
                .subscribe_scoped(&mut database, &cx, &params, setup(), |_, _| {
                    Ok::<_, SubscriptionError>(())
                })
                .unwrap();
            let _ = shared.handle(); // even an un-cloned escape prohibits reuse
            assert!(matches!(
                shared.close_and_release_for_shutdown(&mut database, &cx),
                Err(SubscriptionError::SharedRegistration)
            ));
            assert_eq!(database.standing_queries.len(), count);
            assert!(!shared.is_closed());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
