//! Maintenance of published capsules and the admitted Strata generation.

use asupersync::fs::Vfs;
use fgdb_chronicle::CommitError;
use fgdb_strata::vertex::VertexPatchVersion;
use fgdb_strata::{DeltaBlockVersion, store::StoreError};
use fgdb_types::{CommitCx, ObjectId};
use std::collections::BTreeMap;

use crate::Database;
use fgdb_chronicle::scrub::LostReason;
pub use fgdb_chronicle::scrub::{LostCapsule, ScrubCrashPoint};

/// Capsule repair results and separate audits of the current generation's data
/// (FGSB/FGSP/FGVP) and metadata (manifest, root, root segments). Strata objects
/// have no local redundancy: a lost object is reported, never repaired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubSummary {
    pub objects: usize,
    pub clean: Vec<ObjectId>,
    pub repaired: Vec<ObjectId>,
    pub lost: Vec<LostCapsule>,
    pub block_objects: usize,
    pub block_clean: Vec<ObjectId>,
    pub block_lost: Vec<LostCapsule>,
    pub metadata_objects: usize,
    pub metadata_clean: Vec<ObjectId>,
    pub metadata_lost: Vec<LostCapsule>,
}

#[derive(Clone, Copy)]
enum DataKind {
    Block,
    EdgeProperties,
    VertexPatch,
}

#[derive(Clone, Copy)]
enum MetadataKind {
    Manifest,
    Root,
    Segment,
}

fn record_verification(
    object_id: ObjectId,
    result: Result<Vec<u8>, StoreError>,
    clean: &mut Vec<ObjectId>,
    lost: &mut Vec<LostCapsule>,
) -> Result<(), CommitError> {
    match result {
        Ok(_) => clean.push(object_id),
        Err(StoreError::IdentityMismatch { .. }) => lost.push(LostCapsule {
            object_id,
            reason: LostReason::IdentityMismatch,
        }),
        Err(StoreError::Io(error)) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(CommitError::Io(error));
        }
        Err(_) => lost.push(LostCapsule {
            object_id,
            reason: LostReason::Unusable,
        }),
    }
    Ok(())
}

fn checkpoint(cx: &CommitCx) -> Result<(), CommitError> {
    cx.checkpoint().map_err(|error| {
        CommitError::Io(std::io::Error::new(std::io::ErrorKind::Interrupted, error))
    })
}

impl<V: Vfs + Clone> Database<V> {
    /// Verify every capsule named by the published history and restore damaged
    /// redundancy without changing object, ciphertext, or encoding identity.
    /// Lost objects are reported individually and are never overwritten.
    /// Also re-read the admitted generation's manifest, partition root, V4
    /// segments, FGSB blocks, hosted FGSP properties and FGVP vertex patches.
    /// The complete inventory comes from admission metadata retained before
    /// this sweep, so damage to a parent cannot hide a child. Unreachable
    /// staging objects and superseded fallback-slot generations are not swept.
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
        checkpoint(cx)?;
        let segments = self
            .receipts
            .root_segment_ids(
                crate::PARTITION,
                &self.snapshot.refs,
                &self.snapshot.patch_refs,
            )
            .ok_or_else(|| {
                CommitError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "scrub has no complete admission inventory for the current root",
                ))
            })?;
        let mut metadata = BTreeMap::from([
            (self.snapshot.manifest.0, MetadataKind::Manifest),
            (self.snapshot.root.0, MetadataKind::Root),
        ]);
        metadata.extend(segments.map(|id| (id, MetadataKind::Segment)));
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
            .map(|reference| (reference.block_id, DataKind::Block))
            .collect();
        // The retained writer carries the admitted block-to-patch relationship,
        // including when a block can no longer be decoded from storage.
        for block in self.writer.sealed() {
            if objects.contains_key(&block.block_id)
                && let Some(patch) = &block.property_patch
            {
                objects.insert(patch.patch_id, DataKind::EdgeProperties);
            }
        }
        for reference in &self.snapshot.patch_refs {
            objects.insert(reference.patch_id, DataKind::VertexPatch);
        }
        summary.block_objects = objects.len();
        for (object_id, kind) in objects {
            checkpoint(cx)?;
            let result = match kind {
                DataKind::Block => self.store.get_bytes(cx, DeltaBlockVersion(object_id)).await,
                DataKind::EdgeProperties => {
                    self.store
                        .get_edge_property_patch_bytes(cx, object_id)
                        .await
                }
                DataKind::VertexPatch => {
                    self.store
                        .get_patch_bytes(cx, VertexPatchVersion(object_id))
                        .await
                }
            };
            record_verification(
                object_id,
                result,
                &mut summary.block_clean,
                &mut summary.block_lost,
            )?;
        }
        summary.metadata_objects = metadata.len();
        for (object_id, kind) in metadata {
            checkpoint(cx)?;
            let result = match kind {
                MetadataKind::Manifest => {
                    self.store
                        .get_manifest_bytes(cx, self.snapshot.manifest)
                        .await
                }
                MetadataKind::Root => self.store.get_root_bytes(cx, self.snapshot.root).await,
                MetadataKind::Segment => self.store.get_root_segment_bytes(cx, object_id).await,
            };
            record_verification(
                object_id,
                result,
                &mut summary.metadata_clean,
                &mut summary.metadata_lost,
            )?;
        }
        if summary.lost.is_empty()
            && summary.block_lost.is_empty()
            && summary.metadata_lost.is_empty()
        {
            self.state = previous_state;
        }
        Ok(summary)
    }
}
