//! An owned replica/application boundary: quorum -> apply -> visible -> pin.
//!
//! Consensus replies are released without waiting for application work. Drive
//! one bounded apply slice between network turns; a hidden write never stalls
//! the ordered audit controls needed to release it. Read requests retain their
//! own quorum proof until the required visible cut can be pinned exactly once.
//! The backend owns all three publication capabilities over the SAME Chronicle
//! store/fence. No mutable replica/application escape hatch can bypass a fence.

use std::future::Future;

mod download;
pub mod proposal;
pub mod write;

use fgdb_chronicle::seed::SeedPlan;
use fgdb_order::{Event, MemberId, Output, PersistentState, Role, SnapshotTransfer};
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId};

use super::{
    Application, ApplicationDriver, ApplicationError, ApplicationProgress, ApplicationStateError,
    AppliedPosition, position_at,
};
use crate::driver::{RaftPublisher, SeedDriveError, SeedObjectSource, SeedPublisher};
use crate::replica::{
    ReadIndexId, ReadIndexReady, ReadResolution, Replica, ReplicaError, ReplicaOutput,
};

/// A backend-owned immutable view and its verified cut. Its lifetime must retain
/// all objects/keys needed by the view, independently of later applies or member
/// destruction. The backend authenticates the exact historical state-at-cut;
/// these coordinates are not a substitute for that proof or for a retention pin.
pub struct PinnedApplication<V> {
    pub basis: ApplicationProgress,
    pub at: AppliedPosition,
    pub state_root: ObjectId,
    pub view: V,
}

/// The view/pin capability for a canonical application. It runs under narrowed
/// Cx, performs resource admission and is cancellation-correct. Failure/drop
/// releases any unreturned pin without changing semantic/application progress.
///
/// Pin EXACTLY `at`, which can precede `basis.applied` when its suffix is hidden.
/// Returning the latest applied view in that case would leak hidden effects.
/// Resolve retained historical state through the canonical root/log verifier;
/// unavailable history fails closed. `basis` binds the audit-safe observation,
/// not an instruction to roll the store back to an older physical RootSlot.
/// Fresh Warden authority and query audit admission remain required separately.
pub trait ReadApplication<C>: Application<C> {
    type View;

    fn pin_visible(
        &mut self,
        basis: &ApplicationProgress,
        at: AppliedPosition,
    ) -> impl Future<Output = Result<PinnedApplication<Self::View>, Self::Error>>;
}

/// One admitted read, one fresh-quorum proof, one retained visible view. This is
/// not Clone and its request is consumed when returned. The data gate does not
/// itself authorize a query, export internal coordinates, or establish a lease.
pub struct ApplicationRead<V> {
    read: ReadIndexReady,
    pinned: PinnedApplication<V>,
}

impl<V> ApplicationRead<V> {
    pub fn id(&self) -> &ReadIndexId {
        self.read.id()
    }
    pub fn required_index(&self) -> u64 {
        self.read.index()
    }
    pub fn at(&self) -> AppliedPosition {
        self.pinned.at
    }
    pub fn state_root(&self) -> ObjectId {
        self.pinned.state_root
    }
    pub fn view(&self) -> &V {
        &self.pinned.view
    }
}

pub enum ReadState<V> {
    PendingQuorum,
    PendingApplication { required: u64, applied: u64 },
    PendingAudit { required: u64, visible: u64 },
    Ready(ApplicationRead<V>),
}

#[derive(Debug)]
pub struct AppliedReplicaOutput<C> {
    /// Dispatch promptly: append replies must not wait for apply/audit release.
    pub consensus: Output<C>,
    /// Includes reads already quorum-ready but not yet pinned before stepdown.
    pub leadership_lost: Vec<ReadIndexId>,
}

#[derive(Debug)]
pub enum MemberError<E> {
    State(ApplicationStateError),
    Replica(ReplicaError<E>),
}
impl<E: core::fmt::Debug> core::fmt::Display for MemberError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis applied member: {self:?}")
    }
}
impl<E: core::fmt::Debug> core::error::Error for MemberError<E> {}
impl<E> From<ApplicationStateError> for MemberError<E> {
    fn from(error: ApplicationStateError) -> Self {
        Self::State(error)
    }
}

#[derive(Debug)]
pub enum MemberSeedError<S, P, A> {
    Seed(SeedDriveError<S, P>),
    Application(ApplicationError<A>),
}
impl<S: core::fmt::Debug, P: core::fmt::Debug, A: core::fmt::Debug> core::fmt::Display
    for MemberSeedError<S, P, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis applied member seeding: {self:?}")
    }
}
impl<S: core::fmt::Debug, P: core::fmt::Debug, A: core::fmt::Debug> core::error::Error
    for MemberSeedError<S, P, A>
{
}

struct WaitingRead {
    id: ReadIndexId,
    quorum: Option<ReadIndexReady>,
}

/// Owns consensus and application state through their shared publication backend.
/// Pending admission is bounded across BOTH quorum wait and audit/apply wait.
/// Caller-held returned views have their own backend resource/retention budgets.
/// Recovery always starts with an empty read set; old request IDs cannot resume.
pub struct AppliedReplica<C, A> {
    replica: Replica<C>,
    application: ApplicationDriver<C, A>,
    waiting: Vec<WaitingRead>,
    maximum_reads: usize,
    writes: write::WriteTracker,
}

impl<C: Clone + Eq, A: Application<C> + RaftPublisher<C>> AppliedReplica<C, A> {
    /// Transfer ownership of an authenticated replica with no outstanding reads.
    /// The backend must hold the same store and exclusive writer fence used to
    /// recover it. An existing serving instance must not remain usable elsewhere.
    pub async fn recover(
        replica: Replica<C>,
        backend: A,
        maximum_batch_entries: usize,
        maximum_reads: usize,
    ) -> Result<Self, ApplicationError<<A as Application<C>>::Error>> {
        if maximum_reads == 0 || maximum_reads > 1024 {
            return Err(ApplicationStateError::InvalidLimits.into());
        }
        if replica.pending_reads() != 0 {
            return Err(ApplicationStateError::PendingReadsAtAttach.into());
        }
        let mut waiting = Vec::new();
        waiting
            .try_reserve_exact(maximum_reads)
            .map_err(|_| ApplicationStateError::AllocationFailed)?;
        let application =
            ApplicationDriver::recover(backend, &replica, maximum_batch_entries).await?;
        Ok(Self {
            replica,
            application,
            waiting,
            maximum_reads,
            writes: write::WriteTracker::new(),
        })
    }

    pub fn id(&self) -> MemberId {
        self.replica.id()
    }
    pub fn pending_reads(&self) -> usize {
        self.waiting.len()
    }

    /// Configure the consensus append bound without exposing mutable Raft or
    /// application state. Only a healthy follower can change this volatile
    /// setting; applying or installing still uses the same publication backend.
    pub fn configure_append_pipeline(&mut self, maximum: usize) -> Result<(), ApplicationStateError> {
        self.available()?;
        self.replica
            .configure_append_pipeline(maximum)
            .map_err(ApplicationStateError::Raft)
    }
    pub fn progress(&self) -> Result<ApplicationProgress, ApplicationStateError> {
        self.available()?;
        self.application.progress()
    }
    pub fn durable_state(&self) -> Result<&PersistentState<C>, ApplicationStateError> {
        self.available()?;
        self.replica
            .durable_state()
            .map_err(ApplicationStateError::Raft)
    }

    pub async fn step(
        &mut self,
        event: Event<C>,
    ) -> Result<AppliedReplicaOutput<C>, MemberError<<A as RaftPublisher<C>>::Error>> {
        self.available()?;
        if let Event::Compact(cut) = &event {
            let progress = self.application.progress;
            if cut.index() > progress.visible_index {
                return Err(ApplicationStateError::CompactionNotVisible.into());
            }
            if cut.index() == progress.applied.index && cut.state_root() != progress.state_root.0 {
                return Err(ApplicationStateError::SnapshotStateMismatch.into());
            }
            // The caller still owes complete retention-floor/log-to-state proofs;
            // this additional applied/visible bound is not GC authority.
        }
        let output = self
            .replica
            .step(&mut self.application.application, event)
            .await
            .map_err(MemberError::Replica)?;
        self.absorb(output).map_err(MemberError::State)
    }

    pub async fn apply_next(
        &mut self,
    ) -> Result<Option<ApplicationProgress>, ApplicationError<<A as Application<C>>::Error>> {
        self.available()?;
        self.application.apply_next(&self.replica).await
    }

    /// Reads may wait for application/audit progress without blocking consensus.
    /// Runtime deadlines/cancellation retire requests through cancel_read.
    pub async fn read_index(
        &mut self,
    ) -> Result<(ReadIndexId, AppliedReplicaOutput<C>), MemberError<<A as RaftPublisher<C>>::Error>>
    {
        self.available()?;
        if self.waiting.len() >= self.maximum_reads {
            return Err(ApplicationStateError::ReadBackpressure.into());
        }
        let (id, output) = self
            .replica
            .read_index(&mut self.application.application)
            .await
            .map_err(MemberError::Replica)?;
        // Capacity was reserved before admission. No await separates the issued
        // ID from recording it and absorbing a possible immediate quorum result.
        self.waiting.push(WaitingRead {
            id: id.clone(),
            quorum: None,
        });
        let output = self.absorb(output).map_err(MemberError::State)?;
        Ok((id, output))
    }

    pub fn cancel_read(&mut self, id: &ReadIndexId) -> bool {
        let Some(position) = self.waiting.iter().position(|waiting| &waiting.id == id) else {
            return false;
        };
        self.replica.cancel_read(id);
        self.waiting.remove(position);
        true
    }

    /// Transfer through the existing bonded/atomic install path using the SAME
    /// backend as ordinary application and Raft publication. Reload/verify the
    /// newly installed application cut before releasing the install response.
    /// Cancelling or failing that reload cannot resurrect the old application.
    pub async fn install_snapshot<S: SeedObjectSource>(
        &mut self,
        namespace: DatabaseSecurityNamespaceId,
        transfer: SnapshotTransfer,
        plan: SeedPlan,
        source: &mut S,
    ) -> Result<
        AppliedReplicaOutput<C>,
        MemberSeedError<S::Error, <A as SeedPublisher<C>>::Error, <A as Application<C>>::Error>,
    >
    where
        A: SeedPublisher<C>,
    {
        self.available()
            .map_err(|error| MemberSeedError::Application(error.into()))?;
        let anchor = plan.anchor();
        let expected_root = anchor.publication_root;
        let expected_generation = anchor.publication_generation;
        if expected_generation <= self.application.progress.publication_generation {
            return Err(MemberSeedError::Application(
                ApplicationStateError::InvalidPublication.into(),
            ));
        }
        let output = self
            .replica
            .install_snapshot(
                namespace,
                transfer,
                plan,
                source,
                &mut self.application.application,
            )
            .await
            .map_err(MemberSeedError::Seed)?;
        self.complete_snapshot(output, expected_root, expected_generation)
            .await
            .map_err(MemberSeedError::Application)
    }

    fn available(&self) -> Result<(), ApplicationStateError> {
        self.application.available()?;
        self.replica
            .durable_state()
            .map_err(ApplicationStateError::Raft)?;
        Ok(())
    }

    fn absorb(
        &mut self,
        output: ReplicaOutput<C>,
    ) -> Result<AppliedReplicaOutput<C>, ApplicationStateError> {
        self.writes.observe(
            self.replica.durable_state().map_err(ApplicationStateError::Raft)?,
            output.consensus.role,
        );
        let mut leadership_lost = Vec::new();
        if output.consensus.role != Role::Leader {
            // Include already-confirmed reads, which no longer live in the
            // kernel's pending map but must not survive an observed stepdown.
            leadership_lost.extend(self.waiting.drain(..).map(|read| read.id));
        } else {
            for resolution in output.reads {
                let id = match &resolution {
                    ReadResolution::Ready(ready) => ready.id(),
                    ReadResolution::LeadershipLost(id) => id,
                };
                let Some(position) = self.waiting.iter().position(|waiting| &waiting.id == id)
                else {
                    self.application.poisoned = true;
                    return Err(ApplicationStateError::ReadHistoryMismatch);
                };
                match resolution {
                    ReadResolution::Ready(ready) if self.waiting[position].quorum.is_none() => {
                        self.waiting[position].quorum = Some(ready);
                    }
                    ReadResolution::LeadershipLost(_) => {
                        leadership_lost.push(self.waiting.remove(position).id);
                    }
                    _ => {
                        self.application.poisoned = true;
                        return Err(ApplicationStateError::ReadHistoryMismatch);
                    }
                }
            }
        }
        Ok(AppliedReplicaOutput {
            consensus: output.consensus,
            leadership_lost,
        })
    }
}

impl<C: Clone + Eq, A: ReadApplication<C> + RaftPublisher<C>> AppliedReplica<C, A> {
    /// Resolve ONE original read invocation. Waiting and pin failures retain the
    /// request; successful pinning consumes it. A cancelled pin future retains
    /// the quorum proof but must release its unreturned backend pin. Pinning is
    /// read-only and cannot change application progress or release hidden state.
    pub async fn try_read(
        &mut self,
        id: &ReadIndexId,
    ) -> Result<ReadState<A::View>, ApplicationError<<A as Application<C>>::Error>> {
        self.available()?;
        let position = self
            .waiting
            .iter()
            .position(|waiting| &waiting.id == id)
            .ok_or(ApplicationStateError::UnknownRead)?;
        let Some(ready) = &self.waiting[position].quorum else {
            return Ok(ReadState::PendingQuorum);
        };
        let state = self
            .replica
            .durable_state()
            .map_err(ApplicationStateError::Raft)?;
        if self.replica.role().map_err(ApplicationStateError::Raft)? != Role::Leader
            || ready.term() != state.term()
            || ready.leader() != self.replica.id()
            || ready.domain() != state.configuration().domain()
            || ready.configuration() != state.configuration().identity()
        {
            return Err(ApplicationStateError::LeadershipLost.into());
        }
        let basis = self.application.progress;
        let required = ready.index();
        if basis.applied.index < required {
            return Ok(ReadState::PendingApplication {
                required,
                applied: basis.applied.index,
            });
        }
        if basis.visible_index < required {
            return Ok(ReadState::PendingAudit {
                required,
                visible: basis.visible_index,
            });
        }
        let at = position_at(state, basis.visible_index)
            .ok_or(ApplicationStateError::InvalidPosition)?;
        let pinned = self
            .application
            .application
            .pin_visible(&basis, at)
            .await
            .map_err(ApplicationError::Backend)?;
        if pinned.basis != basis
            || pinned.at != at
            || (at == basis.applied && pinned.state_root != basis.state_root)
        {
            self.application.poisoned = true;
            return Err(ApplicationStateError::InvalidReadSnapshot.into());
        }
        let Some(read) = self.waiting.remove(position).quorum else {
            self.application.poisoned = true;
            return Err(ApplicationStateError::ReadHistoryMismatch.into());
        };
        Ok(ReadState::Ready(ApplicationRead { read, pinned }))
    }
}

#[cfg(test)]
#[path = "member_tests.rs"]
mod tests;
