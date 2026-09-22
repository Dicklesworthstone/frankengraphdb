//! Cancellation-safe I/O drivers for Aegis's existing transition kernels.
//!
//! Publishers are capabilities supplied by the Chronicle/ReplCx runtime, not
//! acknowledgements received from peers. They must retain the exclusive writer
//! fence and authenticated closure pins throughout their futures. No alternate
//! durable format, transport, executor, or membership authority is introduced.

pub mod bonded;

use std::future::Future;

use fgdb_chronicle::identity::{CryptoVerificationSink, EncodedObject};
use fgdb_chronicle::seed::{ObjectPublication, SeedError, SeedObjectSpec};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_chronicle::symbolize::RecoveryTarget;
use fgdb_chronicle::transfer::{BondedPull, DonorId, PullLimits, VerifiedObject};
use fgdb_order::{Error as RaftError, Event, Output, PersistentState, Raft};
use fgdb_types::DatabaseSecurityNamespaceId;

use crate::{CatchupError, CatchupPhase, SnapshotCatchup, SnapshotPublication};

/// Publishes the exact pending Raft state through the canonical Chronicle root.
///
/// Success means the entire referenced closure is durably owned, the root has
/// been synced and reread, and the exclusive writer fence is still held. Merely
/// enqueueing an fsync or obtaining a peer's PreAck is NOT success. A failed or
/// cancelled future may have published and therefore requires root recovery.
/// Implementations must not detach work from the owning ReplCx region.
pub trait RaftPublisher<C> {
    type Error;

    fn publish(
        &mut self,
        state: &PersistentState<C>,
    ) -> impl Future<Output = Result<(), Self::Error>>;
}

#[derive(Debug)]
pub enum SequenceError<E> {
    Raft(RaftError),
    Publication(E),
    /// Installing only the Raft cut would expose an old application snapshot.
    SnapshotNeedsAtomicPublication,
}

impl<E: core::fmt::Debug> core::fmt::Display for SequenceError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis sequencing: {self:?}")
    }
}

impl<E: core::fmt::Debug> core::error::Error for SequenceError<E> {}

struct PublicationGuard<'a, C: Clone + Eq> {
    raft: &'a mut Raft<C>,
    armed: bool,
}

impl<C: Clone + Eq> Drop for PublicationGuard<'_, C> {
    fn drop(&mut self) {
        if self.armed {
            self.raft.publication_failed();
        }
    }
}

/// Execute one authenticated input and release output only after durability.
///
/// The exclusive borrow prevents another input from overtaking publication.
/// Dropping this future during storage I/O poisons the machine, including when
/// storage panics. Recover from the actual authenticated root before reusing the
/// member. Invalid inputs that do not start a transition do not poison it.
///
/// The returned messages can be dispatched independently to their destinations;
/// an unavailable minority must not block delivery to the surviving majority.
/// Committed entries certify ordering, not application visibility. Replay them
/// with the application's durable applied cursor after a crash or cancellation
/// of downstream delivery. SnapshotReady must use `install_snapshot` instead.
pub async fn sequence<C: Clone + Eq, P: RaftPublisher<C>>(
    raft: &mut Raft<C>,
    publisher: &mut P,
    event: Event<C>,
) -> Result<Output<C>, SequenceError<P::Error>> {
    if matches!(&event, Event::SnapshotReady(_)) {
        return Err(SequenceError::SnapshotNeedsAtomicPublication);
    }
    let mut guard = PublicationGuard { raft, armed: false };
    let pending = guard.raft.step(event).map_err(SequenceError::Raft)?;
    guard.armed = true;
    let id = pending.id();
    if pending.requires_write() {
        publisher
            .publish(pending.state())
            .await
            .map_err(SequenceError::Publication)?;
    }
    let output = guard.raft.persisted(id).map_err(SequenceError::Raft)?;
    guard.armed = false;
    Ok(output)
}

/// Acquires an exact inventory object through authenticated ATP recovery.
/// The implementation binds keys, encoding descriptors, donor authorization and
/// namespace to the pinned seed closure before issuing requests. Implementations
/// may use `bonded::recover`; donor agreement alone never constructs this result.
pub trait SeedObjectSource {
    type Error;

    fn recover(
        &mut self,
        object: SeedObjectSpec,
    ) -> impl Future<Output = Result<VerifiedObject, Self::Error>>;
}

/// Borrowed verifier output for one pinned seed-closure object. This is not a
/// wire descriptor or donor-supplied inventory. The catalog retains the actual
/// key capability, authenticated descriptor chain and donor authorization.
pub struct SeedRecovery<'a> {
    pub encoding: &'a EncodedObject,
    pub target: RecoveryTarget<'a>,
    pub dek: &'a [u8; 32],
    pub donors: &'a [DonorId],
    pub limits: PullLimits,
}

pub trait SeedCatalog {
    type Error;

    fn recovery(&self, object: SeedObjectSpec) -> Result<SeedRecovery<'_>, Self::Error>;
}

#[derive(Debug)]
pub enum SeedAcquireError<C, T> {
    Catalog(C),
    Pull(bonded::PullDriveError<T>),
    WrongNamespace,
    InvalidObject,
    /// A different inventory object cannot replace an unfinished pull. Resume
    /// it or explicitly abandon it before changing the authenticated target.
    RecoveryInProgress,
}

impl<C: core::fmt::Debug, T: core::fmt::Debug> core::fmt::Display for SeedAcquireError<C, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis seed object recovery: {self:?}")
    }
}

impl<C: core::fmt::Debug, T: core::fmt::Debug> core::error::Error for SeedAcquireError<C, T> {}

struct ActiveSeedRecovery<'a> {
    object: SeedObjectSpec,
    pull: BondedPull<'a>,
}

/// Concrete bridge from a verifier's seed catalog to concurrent ATP recovery.
/// No full object is requested until its namespace, logical identity, kind and
/// length agree with the authenticated seed inventory. Publication remains the
/// responsibility of SeedPublisher; decoding is never treated as durability.
///
/// The source, rather than an individual recovery future, owns the active pull.
/// Retrying the same object retains authenticated equations, donor quarantines,
/// ESI streams and all resource budgets. Cancellation retires the borrowed I/O
/// window but cannot silently reset the pull or replace its pinned catalog.
pub struct BondedSeedSource<'a, C, T> {
    namespace: DatabaseSecurityNamespaceId,
    catalog: &'a C,
    transport: &'a mut T,
    verification: &'a mut dyn CryptoVerificationSink,
    maximum_window: usize,
    active: Option<ActiveSeedRecovery<'a>>,
}

impl<'a, C: SeedCatalog, T: bonded::PullTransport> BondedSeedSource<'a, C, T> {
    pub fn new(
        namespace: DatabaseSecurityNamespaceId,
        catalog: &'a C,
        transport: &'a mut T,
        verification: &'a mut dyn CryptoVerificationSink,
        maximum_window: usize,
    ) -> Self {
        Self { namespace, catalog, transport, verification, maximum_window, active: None }
    }

    /// Deliberately discard an unfinished immutable-object recovery. Ordinary
    /// transport retries must not call this: it discards useful symbols and
    /// starts a new budget lifetime. It grants no durability or serving authority.
    pub fn abandon_recovery(&mut self) {
        self.active = None;
    }

    /// Re-enable a donor only after the caller authenticates its recovery and
    /// authorization for the still-pinned object. Neither the donor's ESI stream
    /// nor any per-object budget is reset. There must be an active recovery.
    pub fn donor_available(
        &mut self,
        donor: DonorId,
    ) -> Result<(), SeedAcquireError<C::Error, T::Error>> {
        let active = self.active.as_mut().ok_or(SeedAcquireError::InvalidObject)?;
        active.pull.donor_available(donor)
            .map_err(|error| SeedAcquireError::Pull(bonded::PullDriveError::Pull(error)))
    }

    fn prepare(&mut self, object: SeedObjectSpec) -> Result<(), SeedAcquireError<C::Error, T::Error>> {
        if let Some(active) = &self.active {
            if active.object.object_id != object.object_id
                || active.object.object_kind != object.object_kind
                || active.object.compressed_len != object.compressed_len
            {
                return Err(SeedAcquireError::RecoveryInProgress);
            }
            return Ok(());
        }
        // Borrow the externally pinned catalog, not this short-lived call's
        // borrow of self. The pull may then safely outlive its recovery future.
        let catalog: &'a C = self.catalog;
        let material = catalog.recovery(object).map_err(SeedAcquireError::Catalog)?;
        if material.target.namespace != self.namespace {
            return Err(SeedAcquireError::WrongNamespace);
        }
        if material.encoding.object_id() != object.object_id
            || material.target.object_id != object.object_id
            || material.encoding.cipher_descriptor().object_kind != object.object_kind
            || material.encoding.cipher_descriptor().compressed_len != object.compressed_len
        {
            return Err(SeedAcquireError::InvalidObject);
        }
        let pull = BondedPull::new(
            material.encoding, material.target, material.dek, material.donors, material.limits,
        ).map_err(|error| SeedAcquireError::Pull(bonded::PullDriveError::Pull(error)))?;
        self.active = Some(ActiveSeedRecovery { object, pull });
        Ok(())
    }
}

impl<C: SeedCatalog, T: bonded::PullTransport> SeedObjectSource for BondedSeedSource<'_, C, T> {
    type Error = SeedAcquireError<C::Error, T::Error>;

    fn recover(
        &mut self,
        object: SeedObjectSpec,
    ) -> impl Future<Output = Result<VerifiedObject, Self::Error>> {
        async move {
            if self.maximum_window == 0 {
                return Err(SeedAcquireError::Pull(bonded::PullDriveError::InvalidWindow));
            }
            self.prepare(object)?;
            let active = self.active.as_mut().ok_or(SeedAcquireError::InvalidObject)?;
            let recovered = bonded::recover(
                &mut active.pull, self.transport, self.verification, self.maximum_window,
            ).await.map_err(SeedAcquireError::Pull)?;
            // No await separates successful recovery from ownership transfer.
            // On error or cancellation the active pull remains available to retry.
            self.active = None;
            Ok(recovered)
        }
    }
}

/// Chronicle publication capabilities held under the destination writer fence.
pub trait SeedPublisher<C> {
    type Error;

    /// Publish object/encoding/placement/ownership barriers without activating a
    /// new application root. Returning success permits the object durability ack.
    fn publish_object(
        &mut self,
        object: ObjectPublication<'_>,
    ) -> impl Future<Output = Result<(), Self::Error>>;

    /// Publish application state, retention floor and exact Raft state in ONE
    /// canonical root, then sync and reread that root to obtain the evidence.
    /// The publication's authenticated destination generation and root ObjectId
    /// must match. Separate application and consensus publications are invalid.
    fn publish_snapshot(
        &mut self,
        snapshot: SnapshotPublication<'_, C>,
    ) -> impl Future<Output = Result<RootPublicationEvidence, Self::Error>>;
}

#[derive(Debug)]
pub enum SeedDriveError<S, P> {
    Catchup(CatchupError),
    Source(S),
    Publication(P),
}

impl<S: core::fmt::Debug, P: core::fmt::Debug> core::fmt::Display for SeedDriveError<S, P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis seed driver: {self:?}")
    }
}

impl<S: core::fmt::Debug, P: core::fmt::Debug> core::error::Error for SeedDriveError<S, P> {}

// A borrowed session outlives its future. Fence ambiguous root publication
// here, rather than waiting for the caller to drop the session itself.
struct CatchupGuard<'a, 'r, C: Clone + Eq> {
    catchup: &'a mut SnapshotCatchup<'r, C>,
}

impl<C: Clone + Eq> Drop for CatchupGuard<'_, '_, C> {
    fn drop(&mut self) {
        if matches!(self.catchup.phase(), CatchupPhase::Publishing | CatchupPhase::Failed) {
            self.catchup.publication_failed();
        }
    }
}

/// Transfer missing objects, durably publish them, and atomically install a cut.
///
/// This owns the session for callers that do not need to retain partial staging.
/// Use `resume_snapshot` with a caller-owned session to retry transfer or object
/// publication without discarding staged and already-published inventory objects.
/// Neither entry point promotes learners or opens a target for serving reads.
pub async fn install_snapshot<C, S, P>(
    mut catchup: SnapshotCatchup<'_, C>,
    source: &mut S,
    publisher: &mut P,
) -> Result<Output<C>, SeedDriveError<S::Error, P::Error>>
where
    C: Clone + Eq,
    S: SeedObjectSource,
    P: SeedPublisher<C>,
{
    resume_snapshot(&mut catchup, source, publisher).await
}

/// Resume a caller-owned, pinned snapshot installation.
///
/// Transfer errors and cancellation during immutable object publication leave
/// the session in Transferring. Retry this function on the SAME session: staged
/// objects are republished without another ATP pull, and acknowledged objects
/// are skipped. Keep the same BondedSeedSource to retain partial equations too.
/// The object publisher must tolerate retrying the same publication identity.
///
/// Once atomic root publication starts, an error, panic or dropped future marks
/// the session Failed and fences its Raft member immediately, even though the
/// caller still owns the session. Such a session is never retryable: recover the
/// authenticated destination root. Success releases only the installed-cut
/// notification and consensus acknowledgement, not read or voting authority.
pub async fn resume_snapshot<C, S, P>(
    catchup: &mut SnapshotCatchup<'_, C>,
    source: &mut S,
    publisher: &mut P,
) -> Result<Output<C>, SeedDriveError<S::Error, P::Error>>
where
    C: Clone + Eq,
    S: SeedObjectSource,
    P: SeedPublisher<C>,
{
    if catchup.phase() != CatchupPhase::Transferring {
        return Err(SeedDriveError::Catchup(CatchupError::WrongPhase));
    }
    let guard = CatchupGuard { catchup };
    loop {
        match guard.catchup.pending_object() {
            Ok(publication) => {
                let id = publication.id();
                publisher
                    .publish_object(publication)
                    .await
                    .map_err(SeedDriveError::Publication)?;
                guard.catchup.object_published(id).map_err(SeedDriveError::Catchup)?;
                continue;
            }
            Err(CatchupError::Seed(SeedError::StalePublication)) => {}
            Err(error) => return Err(SeedDriveError::Catchup(error)),
        }
        let missing = guard.catchup.missing_objects().next().copied();
        let Some(spec) = missing else { break };
        let object = source.recover(spec).await.map_err(SeedDriveError::Source)?;
        guard.catchup.stage(object).map_err(SeedDriveError::Catchup)?;
    }
    let publication = guard.catchup.begin_publication().map_err(SeedDriveError::Catchup)?;
    let id = publication.id();
    let evidence = publisher.publish_snapshot(publication).await
        .map_err(SeedDriveError::Publication)?;
    guard.catchup.published(id, &evidence).map_err(SeedDriveError::Catchup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{pending, ready};
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    use fgdb_order::{Configuration, Domain, Limits, MemberId, Role};

    struct NoopWake;
    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    fn immediate<F: Future>(future: F) -> F::Output {
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        match pin!(future).as_mut().poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("test operation unexpectedly suspended"),
        }
    }

    #[derive(Default)]
    struct MemoryPublisher {
        state: Option<PersistentState<u64>>,
        writes: usize,
        fail: bool,
    }

    impl RaftPublisher<u64> for MemoryPublisher {
        type Error = &'static str;
        fn publish(&mut self, state: &PersistentState<u64>) -> impl Future<Output = Result<(), Self::Error>> {
            self.writes += 1;
            // Model an uncertain outcome: the root can change before an error.
            self.state = Some(state.clone());
            ready(if self.fail { Err("sync outcome unknown") } else { Ok(()) })
        }
    }

    struct SuspendedPublisher;
    impl RaftPublisher<u64> for SuspendedPublisher {
        type Error = &'static str;
        fn publish(&mut self, _: &PersistentState<u64>) -> impl Future<Output = Result<(), Self::Error>> {
            pending()
        }
    }

    fn node() -> Raft<u64> {
        let config = Configuration::stable(Domain([1; 32]), [2; 32], [MemberId(1)], []).unwrap();
        Raft::new(MemberId(1), config, Limits::default()).unwrap()
    }

    #[test]
    fn committed_output_is_covered_by_published_root() {
        let mut raft = node();
        let mut store = MemoryPublisher::default();
        let election = immediate(sequence(&mut raft, &mut store, Event::ElectionTimeout)).unwrap();
        assert_eq!(election.role, Role::Leader);
        let output = immediate(sequence(&mut raft, &mut store, Event::Propose(41))).unwrap();
        assert!(output.committed.iter().any(|entry| entry.entry.command == Some(41)));
        let durable = store.state.as_ref().unwrap();
        for entry in &output.committed {
            assert!(entry.index <= durable.commit_index());
        }
    }

    #[test]
    fn volatile_heartbeat_does_not_publish_again() {
        let mut raft = node();
        let mut store = MemoryPublisher::default();
        immediate(sequence(&mut raft, &mut store, Event::ElectionTimeout)).unwrap();
        let writes = store.writes;
        immediate(sequence(&mut raft, &mut store, Event::Heartbeat)).unwrap();
        assert_eq!(store.writes, writes);
    }

    #[test]
    fn publication_error_requires_recovery_even_if_root_was_written() {
        let mut raft = node();
        let mut store = MemoryPublisher { fail: true, ..MemoryPublisher::default() };
        assert!(matches!(immediate(sequence(&mut raft, &mut store, Event::ElectionTimeout)), Err(SequenceError::Publication(_))));
        assert!(store.state.is_some());
        assert_eq!(raft.role(), Err(RaftError::RecoveryRequired));
        let reopened = Raft::recover(MemberId(1), store.state.take().unwrap(), Limits::default()).unwrap();
        assert_eq!(reopened.durable_state().unwrap().term(), 1);
    }

    #[test]
    fn cancellation_during_publication_fences_member() {
        let mut raft = node();
        let mut store = SuspendedPublisher;
        {
            let mut future = pin!(sequence(&mut raft, &mut store, Event::ElectionTimeout));
            let waker = Waker::from(Arc::new(NoopWake));
            let mut cx = Context::from_waker(&waker);
            assert!(future.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(raft.role(), Err(RaftError::RecoveryRequired));
    }

    #[test]
    fn invalid_proposal_does_not_poison_or_publish() {
        let mut raft = node();
        let mut store = MemoryPublisher::default();
        assert!(matches!(immediate(sequence(&mut raft, &mut store, Event::Propose(1))), Err(SequenceError::Raft(RaftError::NotLeader))));
        assert_eq!(raft.role(), Ok(Role::Follower));
        assert_eq!(store.writes, 0);
    }
}
