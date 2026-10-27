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
#[cfg(not(target_arch = "wasm32"))]
use fgdb_chronicle::{commit::restore::CapsuleRestoreError, transfer::VerifiedObject};

/// A recovered capsule failed admission/restoration, or ordinary database
/// admission still refused the repaired storage. Neither error returns a
/// database handle that could serve retained, unauthenticated graph state.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub enum CapsuleRecoveryError {
    Open(crate::OpenError),
    Capsule {
        object_id: ObjectId,
        source: CapsuleRestoreError,
    },
}

#[cfg(not(target_arch = "wasm32"))]
impl core::fmt::Display for CapsuleRecoveryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Open(error) => write!(f, "database capsule recovery: {error}"),
            Self::Capsule { object_id, source } => {
                write!(f, "database capsule recovery for {object_id:?}: {source}")
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl core::error::Error for CapsuleRecoveryError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Open(error) => Some(error),
            Self::Capsule { source, .. } => Some(source),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl From<crate::OpenError> for CapsuleRecoveryError {
    fn from(error: crate::OpenError) -> Self {
        Self::Open(error)
    }
}

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
    IndexSegment,
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

#[cfg(not(target_arch = "wasm32"))]
impl Database<asupersync::fs::UnixVfs> {
    /// Reopen a damaged database using authenticated copies of committed
    /// capsules, including capsules that local redundancy cannot recover.
    ///
    /// `recovered` must come through Chronicle's authenticated object transfer.
    /// The recovered marker chain and local identity keys select which objects
    /// may be restored; donor keys, encoding layouts and arbitrary paths never
    /// become local storage authority. Every supplied object's identity,
    /// namespace, capsule format and chain membership are checked before any
    /// capsule is replaced, and the selected root opener authenticates first.
    ///
    /// The exclusive writer lease stays held from chain recovery through
    /// capsule replacement and ordinary root admission/replay. A successful
    /// result is the same healthy database that [`Database::open`] produces.
    /// Repair appends no markers and does not advance the committed frontier.
    /// Missing or damaged derived objects can still make admission fail.
    ///
    /// Storage errors or cancellation can leave an already verified subset
    /// restored. They return no serving handle; retrying with the same objects
    /// is safe. Capsule files must still be inside an existing database
    /// directory, under the same fail-closed rules as [`Database::open`].
    pub async fn open_restoring_capsules(
        cx: &CommitCx,
        path: impl AsRef<std::path::Path>,
        keys: crate::DatabaseKeys,
        recovered: &[VerifiedObject],
    ) -> Result<Self, CapsuleRecoveryError> {
        Self::open_restoring_capsules_with_vfs(
            cx,
            asupersync::fs::UnixVfs::new(),
            path,
            keys,
            recovered,
        )
        .await
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<V: Vfs + Clone> Database<V> {
    /// The caller-selected filesystem form of
    /// [`Database::open_restoring_capsules`], with identical admission and
    /// writer-lease rules. The supplied VFS owns both repairs and normal reopen.
    pub async fn open_restoring_capsules_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<std::path::Path>,
        keys: crate::DatabaseKeys,
        recovered: &[VerifiedObject],
    ) -> Result<Self, CapsuleRecoveryError> {
        let path = path.as_ref();
        crate::require_database_dir(cx, &vfs, path).await?;
        let mut coordinator = fgdb_chronicle::CommitCoordinator::open_with_vfs(
            cx,
            vfs.clone(),
            path,
            keys.capsule_keys(),
        )
        .await
        .map_err(crate::OpenError::from)?;
        // An otherwise valid recovered capsule must not let a wrong DEK or a
        // foreign selected root mutate this database's physical encoding.
        crate::authenticated_root_store(cx, &vfs, path, &keys).await?;
        for object in recovered {
            checkpoint(cx).map_err(crate::OpenError::from)?;
            let object_id = object.object_id();
            coordinator
                .validate_capsule_restore(object_id, object)
                .map_err(|source| CapsuleRecoveryError::Capsule { object_id, source })?;
        }
        let mut verification = Vec::new();
        for object in recovered {
            checkpoint(cx).map_err(crate::OpenError::from)?;
            let object_id = object.object_id();
            coordinator
                .restore_capsule(cx, object_id, object, &mut verification)
                .await
                .map_err(|source| CapsuleRecoveryError::Capsule { object_id, source })?;
        }
        // Move the lease-owning coordinator into the ordinary binder. Dropping
        // it and calling open would admit an intervening writer between repair
        // and replay. Keep the same erased Send boundary as ordinary open.
        let admission: crate::SendFuture<'_, Result<Self, crate::OpenError>> =
            Box::pin(Self::bind_with_coordinator(
                cx,
                vfs,
                path,
                keys,
                false,
                crate::ReplayRootCapacity::DURABLE,
                coordinator,
            ));
        let mut database = admission.await?;
        verification.append(&mut database.crypto_verification_events);
        database.crypto_verification_events = verification;
        Ok(database)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Verify every capsule named by the published history and restore damaged
    /// redundancy without changing object, ciphertext, or encoding identity.
    /// Lost objects are reported individually and are never overwritten.
    /// Also re-read the admitted generation's manifest, partition root, V4
    /// segments, FGSB blocks, hosted FGSP properties and FGVP vertex patches.
    /// Packed objects use freshly admitted location tables and uncached
    /// payload reads, so a prior successful sweep cannot hide later damage.
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
                &self.published_root.blocks,
                &self.published_root.vertex_patches,
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
        metadata.extend(segments.map(|(id, level)| {
            let kind = match level {
                fgdb_strata::root_segment::SegmentLevel::Leaf => MetadataKind::Segment,
                fgdb_strata::root_segment::SegmentLevel::Index => MetadataKind::IndexSegment,
            };
            (id, kind)
        }));
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
        // An ordinary store can retain both a previously admitted pack table
        // and its complete bytes. Neither is evidence of today's physical
        // storage. Keep the authenticated inventory above, but resolve every
        // object through a fresh, uncached verification reader for this sweep.
        let store = self.store.verification_reader(cx).await.map_err(|error| {
            CommitError::Io(match error {
                StoreError::Io(error) => error,
                error => std::io::Error::other(error),
            })
        })?;
        let mut objects: BTreeMap<_, _> = self
            .published_root
            .blocks
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
        for reference in &self.published_root.vertex_patches {
            objects.insert(reference.patch_id, DataKind::VertexPatch);
        }
        summary.block_objects = objects.len();
        for (object_id, kind) in objects {
            checkpoint(cx)?;
            let result = match kind {
                DataKind::Block => store.get_bytes(cx, DeltaBlockVersion(object_id)).await,
                DataKind::EdgeProperties => {
                    store.get_edge_property_patch_bytes(cx, object_id).await
                }
                DataKind::VertexPatch => {
                    store
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
                    store.get_manifest_bytes(cx, self.snapshot.manifest).await
                }
                MetadataKind::Root => store.get_root_bytes(cx, self.snapshot.root).await,
                MetadataKind::Segment => store.get_root_segment_bytes(cx, object_id).await,
                MetadataKind::IndexSegment => {
                    store.get_root_index_segment_bytes(cx, object_id).await
                }
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
