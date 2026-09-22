//! Acknowledged, session-local pull delivery over the maintained native bag.
//!
//! A consumer retains one immutable unacknowledged frame. Optional bounded
//! replay is a shared dependent sink in the SAME maintained circuit: commits
//! are captured automatically, without polling a query or adding another log
//! authority. Without replay the source retains one tick. Expired histories
//! refuse explicitly; no consumer silently skips a missing prefix. Durable
//! registration, retention leases and transport ACK persistence are not claimed.

use super::*;

/// An opaque, subscription-specific receipt. It acknowledges a complete frame,
/// not an arbitrary caller-selected commit frontier. Cloning a receipt permits
/// retrying an ACK; it does not create another delivery position.
#[derive(Clone, Debug)]
pub struct SubscriptionReceipt {
    owner: Arc<()>,
    serial: u64,
}

/// A complete native BAG delivery. A snapshot REPLACES the consumer's bag;
/// a delta is integrated exactly once into the acknowledged preceding bag.
/// Rows use the same columns and exact signed weights as standing_native_bag.
/// ORDER BY rank moves are deliberately not represented as bag differences.
pub struct SubscriptionBatch {
    receipt: SubscriptionReceipt,
    from: Option<CommitSeq>,
    frontier: CommitSeq,
    rows: Arc<ZSet<Vec<QueryValue>>>,
}
impl core::fmt::Debug for SubscriptionBatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SubscriptionBatch")
            .field("from", &self.from)
            .field("frontier", &self.frontier)
            .field("support", &self.rows.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}
impl SubscriptionBatch {
    pub fn receipt(&self) -> &SubscriptionReceipt {
        &self.receipt
    }
    /// None marks a replacement baseline, never an empty predecessor delta.
    pub fn from(&self) -> Option<CommitSeq> {
        self.from
    }
    pub fn frontier(&self) -> CommitSeq {
        self.frontier
    }
    pub fn is_snapshot(&self) -> bool {
        self.from.is_none()
    }
    pub fn rows(&self) -> &ZSet<Vec<QueryValue>> {
        &self.rows
    }
}

#[derive(Debug)]
pub enum SubscriptionError {
    Query(StandingQueryError),
    Closed,
    InvalidReceipt,
    Unacknowledged,
    ReceiptExhausted,
    ReplayAlreadyEnabled,
}
impl core::fmt::Display for SubscriptionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Query(error) => error.fmt(f),
            Self::Closed => f.write_str("subscription delivery is closed"),
            Self::InvalidReceipt => f.write_str("receipt does not acknowledge this delivery"),
            Self::Unacknowledged => f.write_str("subscription already has an unacknowledged frame"),
            Self::ReceiptExhausted => f.write_str("subscription receipt sequence exhausted"),
            Self::ReplayAlreadyEnabled => f.write_str("subscription already has a replay sink"),
        }
    }
}
impl core::error::Error for SubscriptionError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Query(error) => Some(error),
            _ => None,
        }
    }
}
impl From<StandingQueryError> for SubscriptionError {
    fn from(error: StandingQueryError) -> Self {
        Self::Query(error)
    }
}

/// One independent delivery position over a database-owned native circuit.
/// The first successful poll selects the then-current baseline, not the time
/// this object was created. Commits before that poll are included in its bag.
/// An ACK advances only this consumer, never the producer's commit frontier.
///
/// Keep the complete frame until the consumer transaction has applied it, then
/// acknowledge its receipt. Retried polls share the same immutable allocation.
/// This is at-least-once in-process delivery, not crash-safe exactly-once side
/// effects. Receipts cannot be serialized into durable resume tokens.
#[derive(Debug)]
pub struct NativeSubscription {
    handle: StandingQueryHandle,
    replay: Option<StandingQueryHandle>,
    owner: Arc<()>,
    acknowledged: Option<CommitSeq>,
    last_ack: Option<u64>,
    serial: u64,
    pending: Option<Arc<SubscriptionBatch>>,
    closed: bool,
}

impl<V: Vfs + Clone> Database<V> {
    /// Open an independent consumer for an existing native maintained query.
    /// Owner, health, freshness and native layout are admitted before a handle
    /// is copied. The first poll obtains a metered, compressed bag baseline.
    /// Opening a consumer neither scans graph records nor copies result rows.
    pub fn open_standing_subscription(
        &self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<NativeSubscription, SubscriptionError> {
        self.standing_native_columns(cx, handle)?;
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        Ok(NativeSubscription {
            handle: handle.clone(),
            replay: None,
            owner: Arc::new(()),
            acknowledged: None,
            last_ack: None,
            serial: 0,
            pending: None,
            closed: false,
        })
    }
}

impl NativeSubscription {
    /// The underlying registration remains available for explicit repair and
    /// ordinary maintained reads. Consumers over the same handle are isolated.
    pub fn handle(&self) -> &StandingQueryHandle {
        &self.handle
    }
    pub fn acknowledged_frontier(&self) -> Option<CommitSeq> {
        self.acknowledged
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }
    /// The shared replay sink for window inspection or explicit rebuild.
    /// Rebuilding this sink alone never repairs an unavailable source view.
    pub fn replay_handle(&self) -> Option<&StandingQueryHandle> {
        self.replay.as_ref()
    }

    /// Enable automatic bounded replay before the first poll, or while caught
    /// up with no pending frame. An old acknowledged cut cannot be backfilled:
    /// consume its available delta or explicitly restart before enabling replay.
    /// All refusal paths leave the consumer and registry unchanged.
    ///
    /// The sink retains final native BAG deltas with independent tick, support
    /// and logical payload limits. Its per-commit maintenance policy is fixed
    /// here and independent of future poll allowances. `fork` shares this one
    /// sink instead of copying or re-evaluating a circuit for each consumer.
    pub fn enable_replay<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &QueryCx,
        max_ticks: usize,
        max_rows: usize,
        max_payload_units: usize,
        policy: GqlQueryPolicy,
    ) -> Result<CommitSeq, SubscriptionError> {
        if self.closed {
            return Err(SubscriptionError::Closed);
        }
        let current = database.admitted_standing_query(cx, &self.handle)?.status().1;
        if self.replay.is_some() {
            return Err(SubscriptionError::ReplayAlreadyEnabled);
        }
        if self.pending.is_some() {
            return Err(SubscriptionError::Unacknowledged);
        }
        if let Some(from) = self.acknowledged {
            if from != current {
                return Err(StandingQueryError::DeltaUnavailable { from, frontier: current }.into());
            }
        }
        let replay = database.register_standing_replay(
            cx, &self.handle, max_ticks, max_rows, max_payload_units, policy,
        )?;
        // Exclusive database access spans admission and linking. Nothing
        // fallible follows registry publication, so no orphan sink can escape.
        self.replay = Some(replay);
        Ok(current)
    }

    /// Create an independent consumer sharing this circuit and replay sink.
    /// Its first poll is a current baseline, NOT a copy of this consumer's ACK
    /// or pending transaction. Receipts, ACKs, restart and close are isolated.
    /// Creating a fork neither appends registry nodes nor copies result rows.
    pub fn fork<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &QueryCx,
    ) -> Result<Self, SubscriptionError> {
        if self.closed {
            return Err(SubscriptionError::Closed);
        }
        if let Some(replay) = &self.replay {
            database.standing_replay_window(cx, replay)?;
        }
        let mut consumer = database.open_standing_subscription(cx, &self.handle)?;
        consumer.replay = self.replay.clone();
        Ok(consumer)
    }

    /// Return a complete replacement baseline, the FIRST unacknowledged
    /// successor, or None when caught up. With replay, later commits may be
    /// buffered behind this frame. Empty ticks still require acknowledgement.
    ///
    /// Redelivery checks source ownership/health/cancellation, then shares the
    /// previously admitted frame, even if that frame has since left retention.
    /// New frames also require a healthy replay sink when enabled. A snapshot
    /// or legacy one-tick delta uses native bag copy allowances; retained replay
    /// shares its payload and charges only the handle and compressed row quota.
    /// A refusal never changes ACK/pending state. Gaps never become empty deltas.
    pub fn poll<V: Vfs + Clone>(
        &mut self,
        database: &Database<V>,
        cx: &QueryCx,
        policy: GqlQueryPolicy,
    ) -> Result<Option<Arc<SubscriptionBatch>>, SubscriptionError> {
        if self.closed {
            return Err(SubscriptionError::Closed);
        }
        let current = database.admitted_standing_query(cx, &self.handle)?.status().1;
        if let Some(pending) = &self.pending {
            return Ok(Some(Arc::clone(pending)));
        }
        if let Some(replay) = &self.replay {
            database.standing_replay_window(cx, replay)?;
        }
        if self.acknowledged == Some(current) {
            return Ok(None);
        }
        self.serial.checked_add(1).ok_or(SubscriptionError::ReceiptExhausted)?;
        if let (Some(from), Some(replay)) = (self.acknowledged, self.replay.as_ref()) {
            let frame = database.standing_replay_next(cx, replay, from, policy)?
                .ok_or(StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta))?;
            if frame.from() != from || from.checked_successor().ok() != Some(frame.frontier()) {
                return Err(StandingQueryError::Delivery(StandingQueryFailure::InvalidDelta).into());
            }
            cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
            return self.publish_shared(frame.frontier(), frame.shared_rows()).map(Some);
        }
        let (frontier, rows) = match self.acknowledged {
            Some(from) => database.standing_native_delta(cx, &self.handle, from, policy)?,
            None => database.standing_native_bag(cx, &self.handle, policy)?,
        };
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.publish(frontier, rows).map(Some)
    }

    fn publish(
        &mut self,
        frontier: CommitSeq,
        rows: ZSet<Vec<QueryValue>>,
    ) -> Result<Arc<SubscriptionBatch>, SubscriptionError> {
        self.publish_shared(frontier, Arc::new(rows))
    }

    fn publish_shared(
        &mut self,
        frontier: CommitSeq,
        rows: Arc<ZSet<Vec<QueryValue>>>,
    ) -> Result<Arc<SubscriptionBatch>, SubscriptionError> {
        if self.closed {
            return Err(SubscriptionError::Closed);
        }
        if self.pending.is_some() {
            return Err(SubscriptionError::Unacknowledged);
        }
        let serial = self.serial.checked_add(1).ok_or(SubscriptionError::ReceiptExhausted)?;
        let frame = Arc::new(SubscriptionBatch {
            receipt: SubscriptionReceipt { owner: Arc::clone(&self.owner), serial },
            from: self.acknowledged,
            frontier,
            rows,
        });
        self.pending = Some(Arc::clone(&frame));
        self.serial = serial;
        Ok(frame)
    }

    /// Acknowledge only a frame issued to THIS subscription. Retrying the last
    /// successful ACK is idempotent, even while a newer frame is pending. A
    /// foreign, superseded or invalidated receipt never changes either cursor.
    /// No fallible row arithmetic or caller callback occurs during publication.
    pub fn acknowledge(
        &mut self,
        receipt: &SubscriptionReceipt,
    ) -> Result<CommitSeq, SubscriptionError> {
        if self.closed {
            return Err(SubscriptionError::Closed);
        }
        if !Arc::ptr_eq(&self.owner, &receipt.owner) {
            return Err(SubscriptionError::InvalidReceipt);
        }
        if self.last_ack == Some(receipt.serial) {
            return self.acknowledged.ok_or(SubscriptionError::InvalidReceipt);
        }
        let pending = self.pending.as_ref()
            .filter(|frame| frame.receipt.serial == receipt.serial)
            .ok_or(SubscriptionError::InvalidReceipt)?;
        let frontier = pending.frontier;
        self.acknowledged = Some(frontier);
        self.last_ack = Some(receipt.serial);
        self.pending = None;
        Ok(frontier)
    }

    /// Explicitly abandon old delivery state after a gap or consumer reset.
    /// All old receipts become invalid. The next successful poll is a full
    /// replacement baseline. The configured replay sink is kept. Repair an
    /// unavailable source/sink with its rebuild API before taking this baseline.
    pub fn restart_from_current(&mut self) -> Result<(), SubscriptionError> {
        if self.closed {
            return Err(SubscriptionError::Closed);
        }
        self.pending = None;
        self.acknowledged = None;
        self.last_ack = None;
        // Never reuse receipt serials, including when restarting at the same cut.
        Ok(())
    }

    /// Close this delivery position and release its retained frame. Idempotent.
    /// Database-owned view/replay nodes live until the Database is dropped;
    /// closing does not unregister shared nodes or invalidate another consumer.
    pub fn close(&mut self) {
        self.closed = true;
        self.pending = None;
        self.acknowledged = None;
        self.last_ack = None;
    }
}

#[cfg(test)]
mod replay_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn consumer() -> NativeSubscription {
        NativeSubscription {
            handle: StandingQueryHandle {
                owner: Arc::new(()),
                index: 0,
                native: None,
            },
            replay: None,
            owner: Arc::new(()),
            acknowledged: None,
            last_ack: None,
            serial: 0,
            pending: None,
            closed: false,
        }
    }

    #[test]
    fn acknowledgment_isolated_idempotent_and_does_not_consume_next_frame() {
        let mut first = consumer();
        let mut second = consumer();
        let at = CommitSeq::ORIGIN.checked_successor().unwrap();
        let frame = first.publish(at, ZSet::new()).unwrap();
        let foreign = second.publish(at, ZSet::new()).unwrap();
        assert!(frame.is_snapshot());
        assert_eq!(first.acknowledged_frontier(), None);
        assert!(matches!(
            first.acknowledge(foreign.receipt()),
            Err(SubscriptionError::InvalidReceipt)
        ));
        assert_eq!(first.acknowledge(frame.receipt()).unwrap(), at);
        assert_eq!(first.acknowledge(frame.receipt()).unwrap(), at);
        let next = at.checked_successor().unwrap();
        let delta = first.publish(next, ZSet::new()).unwrap();
        assert_eq!(delta.from(), Some(at));
        assert!(!delta.is_snapshot());
        assert!(delta.rows().is_empty());
        assert_eq!(first.acknowledge(frame.receipt()).unwrap(), at);
        assert!(Arc::ptr_eq(first.pending.as_ref().unwrap(), &delta));
        assert_eq!(first.acknowledge(delta.receipt()).unwrap(), next);
        assert!(matches!(
            first.acknowledge(frame.receipt()),
            Err(SubscriptionError::InvalidReceipt)
        ));
        assert_eq!(second.acknowledged_frontier(), None);
    }

    #[test]
    fn no_overwrite_and_rebaseline_invalidates_every_prior_receipt() {
        let mut sub = consumer();
        let at = CommitSeq::ORIGIN;
        let old = sub.publish(at, ZSet::new()).unwrap();
        assert!(matches!(
            sub.publish(at, ZSet::new()),
            Err(SubscriptionError::Unacknowledged)
        ));
        assert!(Arc::ptr_eq(sub.pending.as_ref().unwrap(), &old));
        sub.restart_from_current().unwrap();
        let new = sub.publish(at, ZSet::new()).unwrap();
        assert!(new.is_snapshot());
        assert!(matches!(
            sub.acknowledge(old.receipt()),
            Err(SubscriptionError::InvalidReceipt)
        ));
        assert_eq!(sub.acknowledge(new.receipt()).unwrap(), at);
        sub.restart_from_current().unwrap();
        assert!(matches!(
            sub.acknowledge(new.receipt()),
            Err(SubscriptionError::InvalidReceipt)
        ));
    }

    #[test]
    fn receipt_exhaustion_and_close_cannot_wrap_or_resurrect_delivery() {
        let mut sub = consumer();
        sub.serial = u64::MAX - 1;
        let frame = sub.publish(CommitSeq::ORIGIN, ZSet::new()).unwrap();
        sub.acknowledge(frame.receipt()).unwrap();
        assert!(matches!(
            sub.publish(CommitSeq::ORIGIN, ZSet::new()),
            Err(SubscriptionError::ReceiptExhausted)
        ));
        assert_eq!(sub.acknowledged_frontier(), Some(CommitSeq::ORIGIN));
        sub.close();
        sub.close();
        assert!(sub.is_closed());
        assert!(matches!(sub.restart_from_current(), Err(SubscriptionError::Closed)));
        assert!(matches!(
            sub.acknowledge(frame.receipt()),
            Err(SubscriptionError::Closed)
        ));
        assert!(matches!(
            sub.publish(CommitSeq::ORIGIN, ZSet::new()),
            Err(SubscriptionError::Closed)
        ));
    }
}
