//! Download snapshot objects without borrowing the live consensus machine.
//!
//! A stalled ATP request must not hold the voter while its leader probes it or
//! an election occurs. This owner stages one verified object at a time. Only
//! `prepare` borrows Raft exclusively, revalidates the original opaque offer and
//! freezes the CURRENT suffix for the existing atomic publication gate.

use core::convert::Infallible;

use fgdb_chronicle::seed::{
    ObjectPublication, ReplicaSeed, SeedAnchor, SeedAuditCut, SeedError, SeedObjectSpec, SeedPlan,
    SeedPublicationId,
};
use fgdb_chronicle::transfer::VerifiedObject;
use fgdb_order::{Event, Output, Raft, SnapshotTransfer, SnapshotTransferId};
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId};

use crate::driver::{SeedDriveError, SeedObjectSource, SeedPublisher};
use crate::{CatchupError, CatchupPhase, CatchupPublicationId, SnapshotCatchup};

// Shared with the bound catch-up API. This checks the public cut/closure facts;
// only Raft::step(SnapshotReady) checks the machine-local, nonportable capability.
pub(crate) fn validate<C: Clone + Eq>(
    raft: &Raft<C>,
    namespace: DatabaseSecurityNamespaceId,
    transfer: &SnapshotTransfer,
    plan: &SeedPlan,
) -> Result<(), CatchupError> {
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
    Ok(())
}

/// An owned download, not a voter, installed snapshot, or publication permit.
///
/// Keep the authenticated source catalog, keys and retention pins alive. The
/// runtime must cancel this download when its `transfer_id` appears in Raft's
/// cancelled transfers. Failing to cancel can waste bounded staging, but cannot
/// install an obsolete offer: preparation checks its exact capability again.
/// No method silently creates a new transfer or resets source request budgets.
///
/// Object publication still uses the destination's real writer fence and must
/// be serialized with its other storage publications. It must NOT activate an
/// application/Raft cut. Network recovery borrows neither that publisher nor
/// Raft, so both remain available while a source request is suspended.
pub struct SnapshotDownload {
    namespace: DatabaseSecurityNamespaceId,
    transfer: SnapshotTransfer,
    seed: ReplicaSeed,
}

impl SnapshotDownload {
    pub fn begin<C: Clone + Eq>(
        raft: &Raft<C>,
        namespace: DatabaseSecurityNamespaceId,
        transfer: SnapshotTransfer,
        plan: SeedPlan,
    ) -> Result<Self, CatchupError> {
        validate(raft, namespace, &transfer, &plan)?;
        Ok(Self {
            namespace,
            transfer,
            seed: ReplicaSeed::new(plan),
        })
    }

    pub fn transfer_id(&self) -> SnapshotTransferId {
        self.transfer.id()
    }
    pub fn anchor(&self) -> &SeedAnchor {
        self.seed.plan().anchor()
    }
    /// The verifier-bound visible sub-prefix and complete audit-pipeline root.
    /// None declares the ordinary fully-visible applied-cut profile.
    pub fn audit_cut(&self) -> Option<SeedAuditCut> {
        self.seed.plan().audit_cut()
    }
    pub fn missing_objects(&self) -> impl Iterator<Item = &SeedObjectSpec> {
        self.seed.missing_objects()
    }
    pub fn published_count(&self) -> usize {
        self.seed.published_count()
    }

    /// Rebind a completed download to a freshly verified destination closure
    /// after intervening local publications. Preserve the exact source cut and
    /// transfer capability, and reuse shared durable objects without another
    /// pull. New destination objects must be recovered/published before prepare.
    /// This is explicit replanning, not an ordinary retry or authority refresh;
    /// the caller must authenticate and resource-admit the new canonical plan.
    pub fn refresh_plan(&mut self, plan: SeedPlan) -> Result<(), CatchupError> {
        self.seed.refresh_plan(plan).map_err(CatchupError::Seed)
    }

    pub(crate) fn validate_for<C: Clone + Eq>(&self, raft: &Raft<C>) -> Result<(), CatchupError> {
        validate(raft, self.namespace, &self.transfer, self.seed.plan())
    }

    pub fn stage(&mut self, object: VerifiedObject) -> Result<ObjectPublication<'_>, CatchupError> {
        self.seed.stage(object).map_err(CatchupError::Seed)
    }
    pub fn pending_object(&self) -> Result<ObjectPublication<'_>, CatchupError> {
        self.seed.pending_publication().map_err(CatchupError::Seed)
    }
    pub fn object_published(&mut self, id: SeedPublicationId) -> Result<ObjectId, CatchupError> {
        self.seed.object_published(id).map_err(CatchupError::Seed)
    }

    /// Acquire at most one missing object, without borrowing a replica/publisher.
    /// Returns its staged identity, or None when every object is already durable.
    /// An existing staged object is returned unchanged, not fetched a second time.
    /// Cancellation retains the source's partial pull and this owner's completed
    /// publications; keep the SAME source and download when retrying.
    pub async fn recover_next<S: SeedObjectSource>(
        &mut self,
        source: &mut S,
    ) -> Result<Option<ObjectId>, SeedDriveError<S::Error, Infallible>> {
        match self.seed.pending_publication() {
            Ok(publication) => return Ok(Some(publication.object().object_id())),
            Err(SeedError::StalePublication) => {}
            Err(error) => return Err(SeedDriveError::Catchup(CatchupError::Seed(error))),
        }
        let Some(spec) = self.seed.missing_objects().next().copied() else {
            return Ok(None);
        };
        let object = source.recover(spec).await.map_err(SeedDriveError::Source)?;
        if object.object_id() != spec.object_id {
            return Err(SeedDriveError::Catchup(CatchupError::Seed(
                SeedError::UnexpectedObject,
            )));
        }
        let publication = self
            .seed
            .stage(object)
            .map_err(|error| SeedDriveError::Catchup(CatchupError::Seed(error)))?;
        Ok(Some(publication.object().object_id()))
    }

    /// Publish the staged immutable object through the existing ownership gate.
    /// A failed/cancelled operation keeps the same pending publication ID and
    /// bytes for idempotent retry. It cannot change the active application cut.
    pub async fn publish_next<C, P: SeedPublisher<C>>(
        &mut self,
        publisher: &mut P,
    ) -> Result<ObjectId, SeedDriveError<Infallible, P::Error>> {
        let publication = self
            .seed
            .pending_publication()
            .map_err(|error| SeedDriveError::Catchup(CatchupError::Seed(error)))?;
        let id = publication.id();
        publisher
            .publish_object(publication)
            .await
            .map_err(SeedDriveError::Publication)?;
        self.seed
            .object_published(id)
            .map_err(|error| SeedDriveError::Catchup(CatchupError::Seed(error)))
    }

    /// Freeze only the atomic install, after all objects are durably owned.
    ///
    /// Term changes, replaced offers, a recovered machine or log catch-up may
    /// invalidate the transfer during network work. A stale rejection does not
    /// poison the newer live voter. Once SnapshotReady succeeds, dropping the
    /// returned guard fences that voter; this is NOT a rollback operation.
    ///
    /// The canonical publisher must derive/validate the destination root against
    /// the CURRENT consensus view, not an earlier cached suffix. A stale planned
    /// generation/root must be refused, never blindly overwrite a newer root.
    pub fn prepare<C: Clone + Eq>(
        mut self,
        raft: &mut Raft<C>,
    ) -> Result<SnapshotCatchup<'_, C>, CatchupError> {
        validate(raft, self.namespace, &self.transfer, self.seed.plan())?;
        let seed_id = self.seed.begin_install().map_err(CatchupError::Seed)?.id();
        let transfer_id = self.transfer.id();
        // Arm before the transition and before cloning C: a synchronous panic
        // in a command clone must not leave a reusable speculative voter.
        let mut catchup = SnapshotCatchup {
            raft,
            transfer: self.transfer,
            seed: self.seed,
            phase: CatchupPhase::Publishing,
            publication: None,
            consensus: None,
        };
        let pending = match catchup.raft.step(Event::SnapshotReady(transfer_id)) {
            Ok(pending) => pending,
            Err(error) => {
                // Invalid inputs do not start publication. Internal transition
                // errors already poison Raft itself and are never cleared here.
                catchup.phase = CatchupPhase::Transferring;
                return Err(CatchupError::Raft(error));
            }
        };
        catchup.publication = Some(CatchupPublicationId {
            seed: seed_id,
            consensus: pending.id(),
        });
        catchup.consensus = Some(pending.state().clone());
        Ok(catchup)
    }

    /// Install a downloaded closure through the same atomic publication path.
    pub async fn install<C: Clone + Eq, P: SeedPublisher<C>>(
        self,
        raft: &mut Raft<C>,
        publisher: &mut P,
    ) -> Result<Output<C>, SeedDriveError<Infallible, P::Error>> {
        self.prepare(raft)
            .map_err(SeedDriveError::Catchup)?
            .publish_ready(publisher)
            .await
    }
}

impl<C: Clone + Eq> SnapshotCatchup<'_, C> {
    /// Publish an already prepared cut, without performing any network work.
    /// Own the guard so an error, cancellation or publisher panic fences Raft
    /// before the caller can reuse it, including after a completed root write.
    pub async fn publish_ready<P: SeedPublisher<C>>(
        mut self,
        publisher: &mut P,
    ) -> Result<Output<C>, SeedDriveError<Infallible, P::Error>> {
        self.require(CatchupPhase::Publishing)
            .map_err(SeedDriveError::Catchup)?;
        let publication = self.begin_publication().map_err(SeedDriveError::Catchup)?;
        let id = publication.id();
        let evidence = publisher
            .publish_snapshot(publication)
            .await
            .map_err(SeedDriveError::Publication)?;
        self.published(id, &evidence)
            .map_err(SeedDriveError::Catchup)
    }
}
