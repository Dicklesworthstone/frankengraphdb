//! Verified seed objects from a mounted Chronicle donor or native backup.

use super::{SeedObjectSource, SeedObjectSpec, VerifiedObject};
use fgdb_chronicle::capsule::{CapsuleError, CapsuleKeys};
use fgdb_chronicle::commit::donor;
use fgdb_chronicle::identity::CryptoVerificationSink;
use fgdb_types::ReplCx;
use std::{num::NonZeroUsize, path::Path};

#[derive(Debug)]
pub enum CapsuleSourceError {
    InventoryMismatch,
    Capsule(CapsuleError),
}

impl core::fmt::Display for CapsuleSourceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InventoryMismatch => {
                f.write_str("donor capsule disagrees with the seed inventory")
            }
            Self::Capsule(error) => write!(f, "reading donor capsule: {error}"),
        }
    }
}

impl core::error::Error for CapsuleSourceError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::InventoryMismatch => None,
            Self::Capsule(error) => Some(error),
        }
    }
}

/// Reads one immutable capsule per recovery call, under the host's replication
/// context and per-container byte limit. The directory is selected locally;
/// each filename is derived from the authenticated inventory's full ObjectId.
/// Keep the donor's closure pinned across the download. Missing, damaged or
/// foreign files are retryable source failures, never destination receipts.
pub struct CapsuleDirectorySource<'a> {
    cx: &'a ReplCx,
    directory: &'a Path,
    keys: &'a CapsuleKeys,
    maximum_container_bytes: NonZeroUsize,
    verification: &'a mut dyn CryptoVerificationSink,
}

impl<'a> CapsuleDirectorySource<'a> {
    pub fn new(
        cx: &'a ReplCx,
        directory: &'a Path,
        keys: &'a CapsuleKeys,
        maximum_container_bytes: NonZeroUsize,
        verification: &'a mut dyn CryptoVerificationSink,
    ) -> Self {
        Self {
            cx,
            directory,
            keys,
            maximum_container_bytes,
            verification,
        }
    }
}

impl SeedObjectSource for CapsuleDirectorySource<'_> {
    type Error = CapsuleSourceError;

    async fn recover(&mut self, object: SeedObjectSpec) -> Result<VerifiedObject, Self::Error> {
        if object.object_kind != self.keys.object_kind() {
            return Err(CapsuleSourceError::InventoryMismatch);
        }
        let recovered = donor::read_capsule(
            self.cx,
            self.directory,
            self.keys,
            object.object_id,
            self.maximum_container_bytes,
            self.verification,
        )
        .await
        .map_err(CapsuleSourceError::Capsule)?;
        if recovered.encoding().cipher_descriptor().compressed_len != object.compressed_len {
            return Err(CapsuleSourceError::InventoryMismatch);
        }
        Ok(recovered)
    }
}
