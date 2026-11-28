//! Read-only transfer of a named native capsule from a locally mounted donor.

use super::{
    CapsuleError, CapsuleKeys, CommitCoordinator, CryptoVerificationSink,
    MAX_CAPSULE_CONTAINER_BYTES_V1, ObjectId, UnixVfs, Vfs, VfsFile,
    read_bounded_capsule_container_with_limit,
};
use crate::transfer::VerifiedObject;
use fgdb_types::ReplCx;
use std::num::NonZeroUsize;
use std::path::Path;

/// Errors retain the affected local object when a marker selected one.
#[derive(Debug)]
pub enum DonorRepairError {
    Coordinator(super::CommitError),
    Capsule {
        object_id: ObjectId,
        source: super::restore::CapsuleRestoreError,
    },
}

impl core::fmt::Display for DonorRepairError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Coordinator(error) => write!(f, "donor recovery: {error}"),
            Self::Capsule { object_id, source } => {
                write!(f, "donor recovery for {object_id:?}: {source}")
            }
        }
    }
}

impl core::error::Error for DonorRepairError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Coordinator(error) => Some(error),
            Self::Capsule { source, .. } => Some(source),
        }
    }
}

impl<V: Vfs> CommitCoordinator<V> {
    /// Scrub each local marker's capsule and restore losses from a mounted
    /// native donor. The caller authenticates the local published root before
    /// enabling repair. The lease and local chain select every destination.
    /// Only one object's scan evidence and donor container are staged at once.
    /// Forward detailed evidence for damaged capsules and every donor transfer
    /// to the caller. Healthy objects are verified again by authoritative replay.
    /// Returns the number of objects that required donor bytes. After any
    /// interrupted repair reopen the coordinator and resume from actual storage.
    pub async fn restore_lost_from_donor(
        &mut self,
        cx: &fgdb_types::CommitCx,
        repl: &ReplCx,
        directory: &Path,
        maximum_container_bytes: NonZeroUsize,
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<usize, DonorRepairError> {
        use super::{EffectSource, ScrubDisposition};
        if self.is_poisoned() {
            return Err(DonorRepairError::Coordinator(super::CommitError::Poisoned));
        }
        let mut inspected = Vec::new();
        let mut restored = 0;
        for index in 0..self.chain().len() {
            let EffectSource::Local {
                capsule_ref: object_id,
                ..
            } = self.chain().entries()[index].marker.effect_source;
            let failure = |source| DonorRepairError::Capsule { object_id, source };
            cx.checkpoint().map_err(|error| {
                failure(std::io::Error::new(std::io::ErrorKind::Interrupted, error).into())
            })?;
            inspected.clear();
            let status = self.scrub_one(cx, object_id, None, &mut inspected).await;
            if matches!(status, Ok(ScrubDisposition::Intact)) {
                continue;
            }
            for event in inspected.drain(..) {
                verification.record(event);
            }
            if matches!(
                status.map_err(|error| failure(error.into()))?,
                ScrubDisposition::Repaired
            ) {
                continue;
            }
            let object = read_capsule(
                repl,
                directory,
                self.keys(),
                object_id,
                maximum_container_bytes,
                verification,
            )
            .await
            .map_err(|error| failure(error.into()))?;
            self.restore_capsule(cx, object_id, &object, verification)
                .await
                .map_err(failure)?;
            restored += 1;
        }
        // A previous process may have stopped after renaming a verified,
        // file-synced capsule but before syncing its directory. This pass
        // sees it as intact and does not replace it. Close that window even
        // when this particular pass needed no donor bytes.
        super::sync_directory(cx, &self.vfs, &self.dir.join(super::CAPSULE_DIR))
            .await
            .map_err(|error| DonorRepairError::Coordinator(error.into()))?;
        if self.capsule_directory_parent_sync_pending {
            super::sync_directory(cx, &self.vfs, &self.dir)
                .await
                .map_err(|error| DonorRepairError::Coordinator(error.into()))?;
            self.capsule_directory_parent_sync_pending = false;
        }
        Ok(restored)
    }
}

fn checkpoint(cx: &ReplCx) -> Result<(), CapsuleError> {
    cx.checkpoint()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::Interrupted, error).into())
}

/// Recover this exact ObjectId from a host-selected donor database directory.
/// Reads never take the donor's writer lock or modify its journal, capsule or
/// root. The caller independently authorizes the requested identity, pins its
/// source as needed, and admits the verified object against its destination
/// inventory. A successful read makes no claim about a quorum or publication.
pub async fn read_capsule(
    cx: &ReplCx,
    directory: &Path,
    keys: &CapsuleKeys,
    object: ObjectId,
    maximum_container_bytes: NonZeroUsize,
    verification: &mut dyn CryptoVerificationSink,
) -> Result<VerifiedObject, CapsuleError> {
    let ceiling = maximum_container_bytes
        .get()
        .min(MAX_CAPSULE_CONTAINER_BYTES_V1);
    let path = CommitCoordinator::<UnixVfs>::capsule_path(directory, object);
    let vfs = UnixVfs::new();
    cx.with_restriction_async(async {
        checkpoint(cx)?;
        let named = vfs.symlink_metadata(&path).await?;
        if !named.file_type().is_file() {
            return Err(CapsuleError::MalformedContainer);
        }
        let file = vfs.open_read(&path).await?;
        let opened = file.metadata().await?;
        if !opened.file_type().is_file() {
            return Err(CapsuleError::MalformedContainer);
        }
        if opened.len() > ceiling as u64 {
            return Err(CapsuleError::ContainerTooLarge {
                observed_at_least: usize::try_from(opened.len()).unwrap_or(usize::MAX),
                max: ceiling,
            });
        }
        let bytes = read_bounded_capsule_container_with_limit(file, ceiling).await?;
        checkpoint(cx)?;
        let recovered = VerifiedObject::from_capsule_container(keys, object, &bytes, verification)?;
        checkpoint(cx)?;
        Ok(recovered)
    })
    .await
}
