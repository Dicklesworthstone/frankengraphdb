//! Maintenance of published capsules and admitted Strata block objects.

use asupersync::fs::Vfs;
use fgdb_chronicle::CommitError;
use fgdb_strata::{DeltaBlockVersion, store::StoreError};
use fgdb_types::{CommitCx, ObjectId};
use std::collections::BTreeMap;

use crate::Database;
use fgdb_chronicle::scrub::LostReason;
pub use fgdb_chronicle::scrub::{LostCapsule, ScrubCrashPoint};

/// Capsule repair results and a separate audit of published FGSB/FGSP objects.
/// Blocks have no local redundancy: a lost block is reported, never repaired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubSummary {
    pub objects: usize,
    pub clean: Vec<ObjectId>,
    pub repaired: Vec<ObjectId>,
    pub lost: Vec<LostCapsule>,
    pub block_objects: usize,
    pub block_clean: Vec<ObjectId>,
    pub block_lost: Vec<LostCapsule>,
}

impl<V: Vfs + Clone> Database<V> {
    /// Verify every capsule named by the published history and restore damaged
    /// redundancy without changing object, ciphertext, or encoding identity.
    /// Lost objects are reported individually and are never overwritten.
    /// Also re-read every published FGSB and hosted FGSP object. Their identities
    /// come from the admitted snapshot, so damage to a parent cannot hide a child.
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
        let capsules = self
            .coordinator
            .scrub_capsules(
                cx,
                self.snapshot.frontier,
                crash_at,
                &mut self.crypto_verification_events,
            )
            .await?;
        let mut summary = ScrubSummary {
            objects: capsules.objects,
            clean: capsules.clean,
            repaired: capsules.repaired,
            lost: capsules.lost,
            ..ScrubSummary::default()
        };
        let mut objects: BTreeMap<_, _> = self
            .snapshot
            .refs
            .iter()
            .map(|reference| (reference.block_id, false))
            .collect();
        // The retained writer carries the admitted block-to-patch relationship,
        // including when a block can no longer be decoded from storage.
        for block in self.writer.sealed() {
            if objects.contains_key(&block.block_id)
                && let Some(patch) = &block.property_patch
            {
                objects.insert(patch.patch_id, true);
            }
        }
        summary.block_objects = objects.len();
        for (object_id, property_patch) in objects {
            let result = if property_patch {
                self.store
                    .get_edge_property_patch_bytes(cx, object_id)
                    .await
            } else {
                self.store.get_bytes(cx, DeltaBlockVersion(object_id)).await
            };
            match result {
                Ok(_) => summary.block_clean.push(object_id),
                Err(StoreError::IdentityMismatch { .. }) => summary.block_lost.push(LostCapsule {
                    object_id,
                    reason: LostReason::IdentityMismatch,
                }),
                Err(StoreError::Io(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(CommitError::Io(error));
                }
                Err(_) => summary.block_lost.push(LostCapsule {
                    object_id,
                    reason: LostReason::Unusable,
                }),
            }
        }
        if summary.lost.is_empty() && summary.block_lost.is_empty() {
            self.state = previous_state;
        }
        Ok(summary)
    }
}
