//! Restore an already committed capsule from authenticated bonded recovery.
//!
//! The recovered marker chain, not a donor or a caller-supplied pathname, selects
//! the object. The existing coordinator owns the writer lease, local keys, VFS,
//! capsule pathname and atomic scrub publisher. No log entry, root, availability
//! receipt, membership permission or client outcome is created by restoration.

use super::{
    CapsuleError, CommitCoordinator, CommitError, CryptoVerificationSink,
    MAX_CAPSULE_CONTAINER_BYTES_V1, ObjectId, ScrubCrashPoint, Vfs, encode_container,
    sync_directory, sync_file,
};
use crate::transfer::VerifiedObject;
use fgdb_types::CommitCx;

#[derive(Debug)]
pub enum CapsuleRestoreError {
    RecoveryRequired,
    UnreferencedObject(ObjectId),
    WrongObject,
    WrongNamespace,
    UnsupportedPayload,
    LocalIdentityMismatch,
    ObjectTooLarge,
    NonRegularDestination,
    Storage(CommitError),
}

impl core::fmt::Display for CapsuleRestoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "capsule restoration: {error}"),
            other => write!(f, "capsule restoration: {other:?}"),
        }
    }
}
impl core::error::Error for CapsuleRestoreError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            _ => None,
        }
    }
}
impl From<CommitError> for CapsuleRestoreError {
    fn from(error: CommitError) -> Self {
        Self::Storage(error)
    }
}
impl From<CapsuleError> for CapsuleRestoreError {
    fn from(error: CapsuleError) -> Self {
        Self::Storage(CommitError::Capsule(error))
    }
}
impl From<std::io::Error> for CapsuleRestoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Storage(CommitError::Io(error))
    }
}

impl<V: Vfs> CommitCoordinator<V> {
    /// Admit a recovered object against this coordinator's actual marker chain
    /// and local capsule identity authority, before storage or encoder work.
    ///
    /// This is not a reusable permission: restore repeats admission under its
    /// exclusive borrow. The input must have passed BondedPull's full symbol,
    /// ciphertext, AEAD and logical-identity verification. Local identity is
    /// recomputed too: another K_oid or a nonempty logical header is not a valid
    /// realization of this handle's header-free capsule format.
    pub fn validate_capsule_restore(
        &self,
        expected: ObjectId,
        recovered: &VerifiedObject,
    ) -> Result<(), CapsuleRestoreError> {
        if self.poisoned {
            return Err(CapsuleRestoreError::RecoveryRequired);
        }
        if recovered.object_id() != expected {
            return Err(CapsuleRestoreError::WrongObject);
        }
        if recovered.namespace() != self.keys.namespace() {
            return Err(CapsuleRestoreError::WrongNamespace);
        }
        let descriptor = recovered.encoding().cipher_descriptor();
        let len = u64::try_from(recovered.plaintext().len())
            .map_err(|_| CapsuleRestoreError::ObjectTooLarge)?;
        if descriptor.object_kind != self.keys.object_kind()
            || descriptor.codec_profile != 0
            || descriptor.compressed_len != len
            || descriptor.canonical_plaintext_len != len
        {
            return Err(CapsuleRestoreError::UnsupportedPayload);
        }
        // A generous fixed V1 framing ceiling bounds local re-sealing input.
        // The native encoder additionally enforces its stricter source-block
        // limit. This is not a hard allocator/CPU guarantee.
        if recovered.plaintext().len() > MAX_CAPSULE_CONTAINER_BYTES_V1 {
            return Err(CapsuleRestoreError::ObjectTooLarge);
        }
        if !self.capsule_is_referenced(expected) {
            return Err(CapsuleRestoreError::UnreferencedObject(expected));
        }
        if self.keys.identify(recovered.plaintext()) != expected {
            return Err(CapsuleRestoreError::LocalIdentityMismatch);
        }
        Ok(())
    }

    /// Restore bytes required by an ALREADY committed marker. Recode into the
    /// existing local capsule profile; a donor's nonce, DEK and coding layout
    /// are not installed as local storage authority. ObjectId and logical bytes
    /// stay exact. The source cannot append a marker or advance any frontier.
    ///
    /// The replacement follows the ordinary scrub temp-file sync -> rename ->
    /// directory-sync path. An exact existing container is re-synced, never
    /// accepted as durable merely because its pathname exists. The final reader
    /// is the same bounded, authenticated capsule reader used by recovery.
    ///
    /// From publication until that read succeeds, error, panic or cancellation
    /// fences the coordinator. Reopen the actual chain before retrying; an
    /// interrupted replacement may already be complete. Success establishes
    /// only capsule storage, not application recovery or permission to serve.
    pub async fn restore_capsule(
        &mut self,
        cx: &CommitCx,
        expected: ObjectId,
        recovered: &VerifiedObject,
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<(), CapsuleRestoreError> {
        self.restore_capsule_with_crash(cx, expected, recovered, verification, None)
            .await
    }

    /// Inject the existing scrub crash points into the SAME restoration path.
    #[doc(hidden)]
    pub async fn restore_capsule_with_crash(
        &mut self,
        cx: &CommitCx,
        expected: ObjectId,
        recovered: &VerifiedObject,
        verification: &mut dyn CryptoVerificationSink,
        crash_at: Option<ScrubCrashPoint>,
    ) -> Result<(), CapsuleRestoreError> {
        self.validate_capsule_restore(expected, recovered)?;
        let sealed = self.keys.seal(recovered.plaintext())?;
        if sealed.object_id != expected {
            return Err(CapsuleRestoreError::LocalIdentityMismatch);
        }
        let bytes = encode_container(&sealed);
        drop(sealed);
        let path = Self::capsule_path(&self.dir, expected);
        let identical = cx
            .with_restriction_async(async {
                match self.vfs.symlink_metadata(&path).await {
                    Ok(metadata) if !metadata.file_type().is_file() => {
                        Err(CapsuleRestoreError::NonRegularDestination)
                    }
                    Ok(_) => {
                        let mut file = self.vfs.open_read(&path).await?;
                        if Self::existing_capsule_prefix_len(cx, &mut file, &bytes).await?
                            == Some(bytes.len())
                        {
                            Ok(Some(file))
                        } else {
                            Ok(None)
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error.into()),
                }
            })
            .await?;

        // Arm before invoking any publication future. There is no await between
        // successful verification and disarming; unwind cannot reopen this gate.
        self.poisoned = true;
        if let Some(file) = identical {
            sync_file(cx, &file).await?;
            sync_directory(cx, &self.vfs, &self.dir.join(super::CAPSULE_DIR)).await?;
        } else {
            self.replace_scrubbed_capsule(cx, &path, &bytes, crash_at)
                .await?;
        }
        // Restore also covers a capsule directory recreated during coordinator
        // open. Its own parent entry must be durable before success can escape.
        if self.capsule_directory_parent_sync_pending {
            sync_directory(cx, &self.vfs, &self.dir).await?;
            self.capsule_directory_parent_sync_pending = false;
        }
        let reread = self.read_capsule(cx, expected, verification).await?;
        if reread.as_slice() != recovered.plaintext() {
            return Err(CapsuleRestoreError::LocalIdentityMismatch);
        }
        self.poisoned = false;
        Ok(())
    }
}
