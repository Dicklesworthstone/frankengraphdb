//! Explicit maintenance of the published commit stream's capsule redundancy.

use asupersync::fs::Vfs;
use fgdb_chronicle::CommitError;
use fgdb_types::CommitCx;

use crate::Database;
pub use fgdb_chronicle::scrub::{
    CapsuleScrubSummary as ScrubSummary, LostCapsule, ScrubCrashPoint,
};

impl<V: Vfs + Clone> Database<V> {
    /// Verify every capsule named by the published history and restore damaged
    /// redundancy without changing object, ciphertext, or encoding identity.
    /// Lost objects are reported individually and are never overwritten.
    pub async fn scrub(&mut self, cx: &CommitCx) -> Result<ScrubSummary, CommitError> {
        self.scrub_with_crash(cx, None).await
    }

    /// Run the ordinary scrub path, optionally stopping at a repair I/O boundary.
    /// A stopped repair requires a cold reopen before further maintenance.
    #[doc(hidden)]
    pub async fn scrub_with_crash(
        &mut self,
        cx: &CommitCx,
        crash_at: Option<ScrubCrashPoint>,
    ) -> Result<ScrubSummary, CommitError> {
        // The retained fold is no longer authority once verification begins.
        // Cancellation or loss requires an authoritative reopen; never serve
        // cached rows after discovering their source cannot be folded.
        if !matches!(self.state, crate::DatabaseState::Healthy { .. }) {
            return Err(CommitError::Poisoned);
        }
        let previous_state = self.state;
        self.state = crate::DatabaseState::NeedsAuthoritativeRecovery(crate::RecoveryRequired {
            durable_frontier: self.snapshot.frontier,
            published_frontier: self.snapshot.frontier,
            failed_stage: crate::DerivedPublicationStage::FoldCommittedTemplate,
        });
        let summary = self
            .coordinator
            .scrub_capsules(
                cx,
                self.snapshot.frontier,
                crash_at,
                &mut self.crypto_verification_events,
            )
            .await?;
        if summary.lost.is_empty() {
            self.state = previous_state;
        }
        Ok(summary)
    }
}
