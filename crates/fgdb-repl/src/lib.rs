//! Aegis replica catch-up: bonded object recovery to atomic snapshot installation.
//!
//! The ordinary Raft offer transition first persists the leader term and emits
//! an opaque [`SnapshotTransfer`]. [`SnapshotCatchup`] binds that capability to
//! the canonical verifier's complete Chronicle seed inventory. Only verified,
//! durably owned objects permit SnapshotReady. The resulting application and
//! Raft state are then supplied as ONE publication, never sequentially activated.
//!
//! [`driver::sequence`] drives durability-gated Raft transitions, while
//! [`SnapshotCatchup::install`] owns the complete object-to-root installation.
//! This crate introduces no sockets, file formats, FEC implementation or service
//! authority. The ReplCx/ATP transport, canonical verifier, retention pins,
//! exclusive destination writer fence and real Chronicle publisher remain
//! mandatory. Use an offline/non-serving target, not an unfenced query engine.

#![forbid(unsafe_code)]
#![cfg(not(target_arch = "wasm32"))]

pub mod driver;

use fgdb_chronicle::seed::{
    ObjectPublication, ReplicaSeed, SeedError, SeedInstallation, SeedObjectSpec,
    SeedPlan, SeedPublicationId,
};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_chronicle::transfer::VerifiedObject;
use fgdb_order::{Error as RaftError, Event, Output, PersistenceId, PersistentState,
    Raft, SnapshotTransfer};
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatchupPhase {
    Transferring,
    Publishing,
    Complete,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatchupError {
    WrongNamespace,
    SnapshotBindingMismatch,
    WrongConfiguration,
    StaleSnapshot,
    WrongPhase,
    StalePublication,
    UnexpectedConsensusOutput,
    Seed(SeedError),
    Raft(RaftError),
}

impl core::fmt::Display for CatchupError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis snapshot catch-up: {self:?}")
    }
}

impl core::error::Error for CatchupError {}

/// One in-process acknowledgement bound to BOTH exact publication generations.
/// Neither part can be decoded from a network message or constructed separately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatchupPublicationId {
    seed: SeedPublicationId,
    consensus: PersistenceId,
}

/// The two views describe one atomic destination root publication.
///
/// The publisher MUST encode the exact `consensus` state into the same canonical
/// closure that installs `seed`'s application state/retention floor. That closure
/// must match the plan's destination root ObjectId and generation. Publishing
/// only either view, or publishing them as separate active roots, is invalid.
pub struct SnapshotPublication<'a, C> {
    id: CatchupPublicationId,
    seed: SeedInstallation<'a>,
    consensus: &'a PersistentState<C>,
}

impl<C> SnapshotPublication<'_, C> {
    pub fn id(&self) -> CatchupPublicationId {
        self.id.clone()
    }

    pub fn seed(&self) -> &SeedInstallation<'_> {
        &self.seed
    }

    pub fn consensus(&self) -> &PersistentState<C> {
        self.consensus
    }
}

/// Exclusively borrows one replica so appends/elections cannot overtake the cut.
/// The borrow cannot fence another process or a concurrently serving application;
/// the runtime must hold those real fences and the source closure's retention pin.
pub struct SnapshotCatchup<'r, C: Clone + Eq> {
    raft: &'r mut Raft<C>,
    transfer: SnapshotTransfer,
    seed: ReplicaSeed,
    phase: CatchupPhase,
    publication: Option<CatchupPublicationId>,
    // Immutable bounded suffix copy permits cancellation-safe reacquisition of
    // the publication view without evaluating SnapshotReady a second time.
    consensus: Option<PersistentState<C>>,
}

impl<'r, C: Clone + Eq> SnapshotCatchup<'r, C> {
    /// `transfer` must come from this node's persisted snapshot-offer output.
    /// SnapshotReady revalidates its opaque node/incarnation/request capability
    /// before any root publication can start. A stale/cross-node handle can at
    /// most waste bounded object staging; it cannot install or acknowledge.
    /// `namespace` is the authenticated destination namespace, not a donor claim.
    pub fn begin(
        raft: &'r mut Raft<C>,
        namespace: DatabaseSecurityNamespaceId,
        transfer: SnapshotTransfer,
        plan: SeedPlan,
    ) -> Result<Self, CatchupError> {
        let cut = transfer.snapshot();
        let anchor = plan.anchor();
        if anchor.namespace != namespace {
            return Err(CatchupError::WrongNamespace);
        }
        if anchor.consensus_domain != cut.domain().0
            || anchor.configuration != cut.configuration()
            || anchor.snapshot_manifest.0 != cut.manifest()
            || anchor.state_root.0 != cut.state_root()
            || anchor.retention_floor.0 != cut.retention_floor()
            || anchor.raft_index != cut.index()
            || anchor.raft_term != cut.term()
        {
            return Err(CatchupError::SnapshotBindingMismatch);
        }
        let state = raft.durable_state().map_err(CatchupError::Raft)?;
        let configuration = state.configuration();
        if cut.domain() != configuration.domain()
            || cut.configuration() != configuration.identity()
            || !configuration.voters().contains(&transfer.source())
            || transfer.source() == raft.id()
        {
            return Err(CatchupError::WrongConfiguration);
        }
        if cut.term() > state.term() || cut.index() <= state.commit_index() {
            return Err(CatchupError::StaleSnapshot);
        }
        Ok(Self {
            raft,
            transfer,
            seed: ReplicaSeed::new(plan),
            phase: CatchupPhase::Transferring,
            publication: None,
            consensus: None,
        })
    }

    /// Own the session across cancellable I/O and drive its complete installation.
    /// This consumes the session so cancellation cannot leave an ambiguously
    /// published root attached to a reusable live voter.
    pub async fn install<S, P>(
        self,
        source: &mut S,
        publisher: &mut P,
    ) -> Result<Output<C>, driver::SeedDriveError<S::Error, P::Error>>
    where
        S: driver::SeedObjectSource,
        P: driver::SeedPublisher<C>,
    {
        driver::install_snapshot(self, source, publisher).await
    }

    pub fn phase(&self) -> CatchupPhase {
        self.phase
    }

    pub fn missing_objects(&self) -> impl Iterator<Item = &SeedObjectSpec> {
        self.seed.missing_objects()
    }

    pub fn published_count(&self) -> usize {
        self.seed.published_count()
    }

    /// Only Chronicle's authenticated bonded recovery can construct this object.
    /// Successful decoding is not a durability acknowledgement.
    pub fn stage(&mut self, object: VerifiedObject) -> Result<ObjectPublication<'_>, CatchupError> {
        self.require(CatchupPhase::Transferring)?;
        self.seed.stage(object).map_err(CatchupError::Seed)
    }

    pub fn pending_object(&self) -> Result<ObjectPublication<'_>, CatchupError> {
        self.require(CatchupPhase::Transferring)?;
        self.seed.pending_publication().map_err(CatchupError::Seed)
    }

    /// Call after the actual object/placement/ownership publication barriers.
    /// This integration acknowledgement is not itself a storage operation.
    pub fn object_published(&mut self, id: SeedPublicationId) -> Result<ObjectId, CatchupError> {
        self.require(CatchupPhase::Transferring)?;
        self.seed.object_published(id).map_err(CatchupError::Seed)
    }

    /// Prepare ONE atomic application/Raft snapshot publication after every
    /// object is durably owned. Repeated calls return the same immutable state
    /// and generation; cancellation cannot accidentally evaluate Ready twice.
    pub fn begin_publication(&mut self) -> Result<SnapshotPublication<'_, C>, CatchupError> {
        if self.phase == CatchupPhase::Transferring {
            let seed_id = self.seed.begin_install().map_err(CatchupError::Seed)?.id();
            let pending = match self.raft.step(Event::SnapshotReady(self.transfer.id())) {
                Ok(pending) => pending,
                Err(error) => {
                    self.seed.publication_failed();
                    self.phase = CatchupPhase::Failed;
                    return Err(CatchupError::Raft(error));
                }
            };
            self.consensus = Some(pending.state().clone());
            self.publication = Some(CatchupPublicationId { seed: seed_id, consensus: pending.id() });
            self.phase = CatchupPhase::Publishing;
        }
        self.require(CatchupPhase::Publishing)?;
        let id = self.publication.as_ref().ok_or(CatchupError::StalePublication)?.clone();
        let consensus = self.consensus.as_ref().ok_or(CatchupError::StalePublication)?;
        let seed = self.seed.begin_install().map_err(CatchupError::Seed)?;
        Ok(SnapshotPublication { id, seed, consensus })
    }

    /// Complete BOTH gates using the exact post-sync root reread evidence.
    /// The runtime must obtain this evidence for the root assembled from the
    /// complete SnapshotPublication above. Only this method releases the Raft
    /// acknowledgement and atomically installed application-cut notification.
    pub fn published(
        &mut self,
        id: CatchupPublicationId,
        evidence: &RootPublicationEvidence,
    ) -> Result<Output<C>, CatchupError> {
        self.require(CatchupPhase::Publishing)?;
        if self.publication.as_ref() != Some(&id) {
            return Err(CatchupError::StalePublication);
        }
        self.seed.finish_install(id.seed, evidence).map_err(CatchupError::Seed)?;
        let output = match self.raft.persisted(id.consensus) {
            Ok(output) => output,
            Err(error) => {
                self.publication_failed();
                return Err(CatchupError::Raft(error));
            }
        };
        if output.installed_snapshot.as_ref() != Some(self.transfer.snapshot()) {
            self.publication_failed();
            return Err(CatchupError::UnexpectedConsensusOutput);
        }
        self.phase = CatchupPhase::Complete;
        self.publication = None;
        self.consensus = None;
        Ok(output)
    }

    /// An unknown storage outcome is not rollback. Reopen from the authenticated
    /// destination root before this replica may process another consensus input.
    pub fn publication_failed(&mut self) {
        if self.phase != CatchupPhase::Complete {
            self.seed.publication_failed();
            self.raft.publication_failed();
            self.phase = CatchupPhase::Failed;
        }
    }

    fn require(&self, phase: CatchupPhase) -> Result<(), CatchupError> {
        if self.phase == phase { Ok(()) } else { Err(CatchupError::WrongPhase) }
    }
}

impl<C: Clone + Eq> Drop for SnapshotCatchup<'_, C> {
    fn drop(&mut self) {
        // Immutable object staging alone cannot replace the consensus root.
        // Once the root publication may have escaped, never reactivate the old
        // voter merely because its catch-up future/session was cancelled.
        if matches!(self.phase, CatchupPhase::Publishing | CatchupPhase::Failed) {
            self.raft.publication_failed();
        }
    }
}
