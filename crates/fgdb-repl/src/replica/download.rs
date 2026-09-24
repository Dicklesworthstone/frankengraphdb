//! Live snapshot downloads without an escape hatch to the owned Raft machine.

use core::convert::Infallible;

use fgdb_chronicle::seed::SeedPlan;
use fgdb_order::SnapshotTransfer;
use fgdb_types::DatabaseSecurityNamespaceId;

use super::{Replica, ReplicaOutput};
use crate::CatchupError;
use crate::download::SnapshotDownload;
use crate::driver::{SeedDriveError, SeedPublisher};

impl<C: Clone + Eq> Replica<C> {
    /// Start independent object recovery from a persisted offer. The returned
    /// owner does not borrow this member; keep delivering messages/deadlines and
    /// act on cancelled_snapshot_transfers while the download is in flight.
    pub fn begin_snapshot_download(
        &self,
        namespace: DatabaseSecurityNamespaceId,
        transfer: SnapshotTransfer,
        plan: SeedPlan,
    ) -> Result<SnapshotDownload, CatchupError> {
        SnapshotDownload::begin(&self.raft, namespace, transfer, plan)
    }

    pub(crate) fn validate_download(
        &self,
        download: &SnapshotDownload,
    ) -> Result<(), CatchupError> {
        download.validate_for(&self.raft)
    }

    /// Perform only the atomic installation, after independent recovery and
    /// object publication. Observe all released output through the ordinary
    /// driver: callers cannot lose read-history/role transitions by taking Raft.
    /// Same-store writer fencing and current canonical root verification remain
    /// required of the publisher. Stale downloads never bypass SnapshotReady.
    pub async fn install_download<P: SeedPublisher<C>>(
        &mut self,
        download: SnapshotDownload,
        publisher: &mut P,
    ) -> Result<ReplicaOutput<C>, SeedDriveError<Infallible, P::Error>> {
        let output = download.install(&mut self.raft, publisher).await?;
        self.observe(output, None)
            .map_err(|error| SeedDriveError::Catchup(CatchupError::Raft(error)))
    }
}
