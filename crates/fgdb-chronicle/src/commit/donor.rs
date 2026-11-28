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
