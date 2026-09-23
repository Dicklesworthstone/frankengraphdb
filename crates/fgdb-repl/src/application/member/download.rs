//! Live download composition over the owning member's one publication backend.

use core::convert::Infallible;

use fgdb_chronicle::seed::SeedPlan;
use fgdb_order::SnapshotTransfer;
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId};

use super::{AppliedReplica, AppliedReplicaOutput, MemberSeedError};
use crate::application::{Application, ApplicationError, ApplicationStateError, validate_progress};
use crate::download::SnapshotDownload;
use crate::driver::{RaftPublisher, SeedDriveError, SeedPublisher};
use crate::replica::ReplicaOutput;

impl<C: Clone + Eq, A: Application<C> + RaftPublisher<C> + SeedPublisher<C>> AppliedReplica<C, A> {
    /// Admit a download while retaining ownership of the voter/application.
    /// Poll download.recover_next independently and continue step/apply_next.
    /// No new service, membership or query authority is granted by admission.
    pub fn begin_snapshot_download(
        &self,
        namespace: DatabaseSecurityNamespaceId,
        transfer: SnapshotTransfer,
        plan: SeedPlan,
    ) -> Result<SnapshotDownload, MemberSeedError<Infallible, <A as SeedPublisher<C>>::Error, <A as Application<C>>::Error>> {
        self.available().map_err(|error| MemberSeedError::Application(error.into()))?;
        if plan.anchor().publication_generation <= self.application.progress.publication_generation {
            return Err(MemberSeedError::Application(ApplicationStateError::InvalidPublication.into()));
        }
        self.replica.begin_snapshot_download(namespace, transfer, plan)
            .map_err(|error| MemberSeedError::Seed(SeedDriveError::Catchup(error)))
    }

    /// Publish one recovered immutable object with this member's SAME backend.
    /// No mutable backend escapes and no object acknowledgement activates state.
    /// Cancellation retains the pending object ID for an idempotent retry.
    pub async fn publish_download_object(
        &mut self,
        download: &mut SnapshotDownload,
    ) -> Result<ObjectId, MemberSeedError<Infallible, <A as SeedPublisher<C>>::Error, <A as Application<C>>::Error>> {
        self.available().map_err(|error| MemberSeedError::Application(error.into()))?;
        self.replica.validate_download(download)
            .map_err(|error| MemberSeedError::Seed(SeedDriveError::Catchup(error)))?;
        download.publish_next::<C, A>(&mut self.application.application).await
            .map_err(MemberSeedError::Seed)
    }

    /// Atomically publish the downloaded cut and reload its application before
    /// any install response can escape. Unlike install_snapshot, no network I/O
    /// occurs while this method borrows the member. Root publication and reload
    /// remain exclusive, with the original before-call cancellation/panic fence.
    ///
    /// The supplied plan must still name the exact CURRENT destination root and
    /// generation. Live protocol/application publications may stale an earlier
    /// plan; the canonical publisher must refuse it rather than overwrite them.
    pub async fn install_download(
        &mut self,
        download: SnapshotDownload,
    ) -> Result<AppliedReplicaOutput<C>, MemberSeedError<Infallible, <A as SeedPublisher<C>>::Error, <A as Application<C>>::Error>> {
        self.available().map_err(|error| MemberSeedError::Application(error.into()))?;
        let expected_root = download.anchor().publication_root;
        let expected_generation = download.anchor().publication_generation;
        if expected_generation <= self.application.progress.publication_generation {
            return Err(MemberSeedError::Application(ApplicationStateError::InvalidPublication.into()));
        }
        let output = self.replica.install_download(download, &mut self.application.application)
            .await.map_err(MemberSeedError::Seed)?;
        self.complete_snapshot(output, expected_root, expected_generation)
            .await.map_err(MemberSeedError::Application)
    }
}

impl<C: Clone + Eq, A: Application<C> + RaftPublisher<C>> AppliedReplica<C, A> {
    // Shared by bound/offline and live-download installs. There is exactly one
    // validation/activation path, so a new entry point cannot forget a gate.
    pub(super) async fn complete_snapshot(
        &mut self,
        output: ReplicaOutput<C>,
        expected_root: ObjectId,
        expected_generation: u64,
    ) -> Result<AppliedReplicaOutput<C>, ApplicationError<<A as Application<C>>::Error>> {
        // The atomic root may already be current. Fence BEFORE invoking load,
        // including synchronous panic and cancellation of the reload future.
        self.application.poisoned = true;
        let progress = self.application.application.load().await.map_err(ApplicationError::Backend)?;
        let state = self.replica.durable_state().map_err(ApplicationStateError::Raft)?;
        validate_progress(state, &progress)?;
        if progress.applied.index != state.commit_index()
            || progress.visible_index != progress.applied.index
            || progress.publication_root != expected_root
            || progress.publication_generation != expected_generation
        {
            return Err(ApplicationStateError::InvalidPublication.into());
        }
        self.application.progress = progress;
        let output = self.absorb(output)?;
        self.application.poisoned = false;
        Ok(output)
    }
}
