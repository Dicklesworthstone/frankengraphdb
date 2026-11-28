//! Read-only transfer of a named native capsule from a locally mounted donor.

use super::{
    CapsuleError, CapsuleKeys, CommitCoordinator, CryptoVerificationSink,
    MAX_CAPSULE_CONTAINER_BYTES_V1, ObjectId, UnixVfs, Vfs, VfsFile,
    read_bounded_capsule_container_with_limit,
};
use crate::transfer::VerifiedObject;
use fgdb_types::ReplCx;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

/// At most one container per source is examined for each required ObjectId.
pub const MAX_CAPSULE_DONORS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DonorListError {
    MissingPrimary,
    TooMany,
    Duplicate,
}

impl core::fmt::Display for DonorListError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::MissingPrimary => "capsule donor fallbacks require a primary donor",
            Self::TooMany => "at most eight capsule donor directories may be supplied",
            Self::Duplicate => "duplicate capsule donor directory",
        })
    }
}

impl core::error::Error for DonorListError {}

/// Ordered, operator-selected native databases or history archives. Every
/// candidate must independently authenticate the same object under the same
/// keys. A donor's journal, membership or configuration is never imported.
#[derive(Clone, Copy)]
pub struct CapsuleDonors<'a> {
    primary: &'a Path,
    fallbacks: &'a [PathBuf],
}

impl<'a> CapsuleDonors<'a> {
    pub fn new(primary: &'a Path) -> Self {
        Self {
            primary,
            fallbacks: &[],
        }
    }

    pub fn with_fallbacks(
        primary: &'a Path,
        fallbacks: &'a [PathBuf],
    ) -> Result<Self, DonorListError> {
        if fallbacks.len() >= MAX_CAPSULE_DONORS {
            return Err(DonorListError::TooMany);
        }
        for (at, path) in fallbacks.iter().enumerate() {
            if path == primary || fallbacks[..at].contains(path) {
                return Err(DonorListError::Duplicate);
            }
        }
        Ok(Self { primary, fallbacks })
    }

    /// Return the first authenticated realization of the requested identity.
    /// Missing, unavailable, foreign or damaged source bytes permit another
    /// configured source. Cancellation, denied access and allocation failure
    /// stop the attempt. When all donors fail on content or availability,
    /// retain the primary source's error for the operator.
    pub async fn read(
        self,
        cx: &ReplCx,
        keys: &CapsuleKeys,
        object: ObjectId,
        ceiling: NonZeroUsize,
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<VerifiedObject, CapsuleError> {
        let first = match read_capsule(cx, self.primary, keys, object, ceiling, verification).await
        {
            Err(error) if source_failure(&error) && !self.fallbacks.is_empty() => error,
            result => return result,
        };
        for path in self.fallbacks {
            match read_capsule(cx, path, keys, object, ceiling, verification).await {
                Err(error) if source_failure(&error) => continue,
                result => return result,
            }
        }
        checkpoint(cx)?;
        Err(first)
    }
}

fn source_failure(error: &CapsuleError) -> bool {
    use std::io::ErrorKind;
    match error {
        // EIO from a failing mounted disk need not have a portable ErrorKind.
        // Only source reads reach this classifier. Never retry a local
        // permission, cancellation, allocation or caller contract failure.
        CapsuleError::Io(error) => !matches!(
            error.kind(),
            ErrorKind::PermissionDenied
                | ErrorKind::Interrupted
                | ErrorKind::OutOfMemory
                | ErrorKind::InvalidInput
                | ErrorKind::Unsupported
        ),
        CapsuleError::AllocationFailed
        | CapsuleError::Recovery(crate::symbolize::SymbolizeError::AllocationFailed) => false,
        _ => true,
    }
}

#[cfg(test)]
mod donor_policy_tests {
    use super::*;

    #[test]
    fn authorization_cancellation_and_memory_pressure_cannot_select_another_donor() {
        use std::io::ErrorKind;
        for kind in [
            ErrorKind::Interrupted,
            ErrorKind::PermissionDenied,
            ErrorKind::OutOfMemory,
        ] {
            assert!(!source_failure(&CapsuleError::Io(kind.into())));
        }
        assert!(!source_failure(&CapsuleError::AllocationFailed));
        assert!(!source_failure(&CapsuleError::Recovery(
            crate::symbolize::SymbolizeError::AllocationFailed
        )));
        assert!(source_failure(&CapsuleError::Io(
            ErrorKind::NotFound.into()
        )));
        assert!(source_failure(&CapsuleError::MalformedContainer));
        let names: Vec<_> = (0..MAX_CAPSULE_DONORS)
            .map(|i| PathBuf::from(i.to_string()))
            .collect();
        let primary = Path::new("primary");
        assert!(CapsuleDonors::with_fallbacks(primary, &names[..MAX_CAPSULE_DONORS - 1]).is_ok());
        assert!(matches!(
            CapsuleDonors::with_fallbacks(primary, &names),
            Err(DonorListError::TooMany)
        ));
        assert!(matches!(
            CapsuleDonors::with_fallbacks(&names[0], &names[..1]),
            Err(DonorListError::Duplicate)
        ));
        assert!(matches!(
            CapsuleDonors::with_fallbacks(primary, &[names[0].clone(), names[0].clone()]),
            Err(DonorListError::Duplicate)
        ));
    }
}

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
        self.restore_lost_from_donors(
            cx,
            repl,
            CapsuleDonors::new(directory),
            maximum_container_bytes,
            verification,
        )
        .await
    }

    /// Recover across multiple incomplete native donors without multiplying
    /// destination publications. Each locally selected capsule can come from
    /// a different source. Destination validation and storage errors terminate
    /// the repair and are never retried against another donor.
    pub async fn restore_lost_from_donors(
        &mut self,
        cx: &fgdb_types::CommitCx,
        repl: &ReplCx,
        donors: CapsuleDonors<'_>,
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
            let object = donors
                .read(
                    repl,
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
