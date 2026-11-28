//! Standalone Chronicle history backups at an independently identified marker.

use super::verify::{HistoryFrames, HistoryVerificationError};
use super::{
    CAPSULE_DIR, COMMIT_LOG_NAME, CapsuleKeys, CommitCoordinator, CommitError, OpenOptions,
    read_bounded_capsule_container, scan_checkpoint, sync_directory, sync_file,
};
use crate::transfer::VerifiedObject;
use asupersync::fs::{Vfs, VfsFile};
use asupersync::io::AsyncWriteExt;
use fgdb_crypto::Digest;
use fgdb_types::{CommitCx, CommitSeq, ObjectId};
use std::io;
use std::num::NonZeroU64;
use std::path::Path;

impl<V: Vfs> CommitCoordinator<V> {
    /// Copy the exact marker frames and their authenticated native capsule containers
    /// through a trusted (sequence, chain hash) cut, beside a live source writer.
    /// The host supplies a new destination below an existing, trusted parent.
    ///
    /// No journal becomes visible until all source capsules and template digests,
    /// the terminal hash, and the destination data barriers have succeeded.
    /// If publication is interrupted, inspect the destination with
    /// `verify_history_through` or retry into a fresh directory. This operation
    /// never overwrites an existing directory and never copies private keys.
    /// It archives committed effects, not the host's other state or authority.
    pub async fn backup_history_through(
        cx: &CommitCx,
        vfs: &V,
        source: &Path,
        destination: &Path,
        keys: &CapsuleKeys,
        cut: (NonZeroU64, Digest),
        template_digest: fn(&[u8]) -> Digest,
    ) -> Result<Digest, HistoryVerificationError> {
        cx.with_restriction_async(async {
            scan_checkpoint(cx)?;
            let parent = destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let parent = vfs.canonicalize(parent).await.map_err(CommitError::Io)?;
            let source = vfs.canonicalize(source).await.map_err(CommitError::Io)?;
            if parent.starts_with(&source) {
                return Err(CommitError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "backup destination must be outside the source database",
                ))
                .into());
            }
            let leaf = destination.file_name().ok_or_else(|| {
                CommitError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "backup destination must name a new directory",
                ))
            })?;
            let destination = parent.join(leaf);
            let mut frames = HistoryFrames::open(cx, vfs, &source, cut.0).await?;
            vfs.create_dir(&destination)
                .await
                .map_err(CommitError::Io)?;
            let capsules = destination.join(CAPSULE_DIR);
            vfs.create_dir(&capsules).await.map_err(CommitError::Io)?;
            let pending = destination.join("commits.pending");
            let mut journal = vfs
                .open(&pending, &OpenOptions::new().write(true).create_new(true))
                .await
                .map_err(CommitError::Io)?;
            let archive = CapsuleArchive {
                vfs,
                source: &source,
                destination: &destination,
                keys,
                template_digest,
            };
            let hash = loop {
                let frame = frames.next(cx).await?;
                archive
                    .copy(cx, frame.seq, frame.capsule, frame.template_digest)
                    .await?;
                journal
                    .write_all(frame.bytes)
                    .await
                    .map_err(CommitError::Io)?;
                scan_checkpoint(cx)?;
                if frame.seq.0 == cut.0.get() {
                    break frame.chain_hash;
                }
            };
            if hash != cut.1 {
                return Err(CommitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "backup cut differs from the expected marker chain hash",
                ))
                .into());
            }
            sync_directory(cx, vfs, &capsules)
                .await
                .map_err(CommitError::Io)?;
            sync_file(cx, &mut journal).await.map_err(CommitError::Io)?;
            drop(journal);
            // Persist the capsule directory entry before the journal can survive
            // a power loss. Syncing its contents alone does not persist its name.
            sync_directory(cx, vfs, &destination)
                .await
                .map_err(CommitError::Io)?;
            sync_directory(cx, vfs, &parent)
                .await
                .map_err(CommitError::Io)?;
            scan_checkpoint(cx)?;
            // A new public name, never a replacing rename. A crash from this
            // point on may leave a complete archive even if its caller got no receipt.
            vfs.hard_link(&pending, &destination.join(COMMIT_LOG_NAME))
                .await
                .map_err(CommitError::Io)?;
            vfs.remove_file(&pending).await.map_err(CommitError::Io)?;
            sync_directory(cx, vfs, &destination)
                .await
                .map_err(CommitError::Io)?;
            Ok(hash)
        })
        .await
    }
}

struct CapsuleArchive<'a, V> {
    vfs: &'a V,
    source: &'a Path,
    destination: &'a Path,
    keys: &'a CapsuleKeys,
    template_digest: fn(&[u8]) -> Digest,
}

impl<V: Vfs> CapsuleArchive<'_, V> {
    async fn copy(
        &self,
        cx: &CommitCx,
        commit_seq: CommitSeq,
        object_id: ObjectId,
        expected_digest: Digest,
    ) -> Result<(), HistoryVerificationError> {
        let vfs = self.vfs;
        let path = CommitCoordinator::<V>::capsule_path(self.source, object_id);
        let failure = |source| HistoryVerificationError::Capsule {
            commit_seq,
            object_id,
            source,
        };
        let regular = || {
            failure(CommitError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "backup capsule is not a regular file",
            )))
        };
        if !vfs
            .symlink_metadata(&path)
            .await
            .map_err(|e| failure(e.into()))?
            .file_type()
            .is_file()
        {
            return Err(regular());
        }
        let file = vfs.open_read(&path).await.map_err(|e| failure(e.into()))?;
        if !file
            .metadata()
            .await
            .map_err(|e| failure(e.into()))?
            .file_type()
            .is_file()
        {
            return Err(regular());
        }
        let bytes = read_bounded_capsule_container(file)
            .await
            .map_err(|e| failure(e.into()))?;
        scan_checkpoint(cx)?;
        let object =
            VerifiedObject::from_capsule_container(self.keys, object_id, &bytes, &mut Vec::new())
                .map_err(|e| failure(e.into()))?;
        if (self.template_digest)(object.plaintext()) != expected_digest {
            return Err(HistoryVerificationError::TemplateDigestMismatch {
                commit_seq,
                object_id,
            });
        }
        drop(object);
        scan_checkpoint(cx)?;
        let target = CommitCoordinator::<V>::capsule_path(self.destination, object_id);
        match vfs
            .open(&target, &OpenOptions::new().write(true).create_new(true))
            .await
        {
            Ok(mut copy) => {
                copy.write_all(&bytes).await.map_err(CommitError::Io)?;
                sync_file(cx, &mut copy).await.map_err(CommitError::Io)?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // More than one marker may reference the same object. Check the
                // actual destination, so reusing a file never weakens validation.
                let existing = CommitCoordinator::<V>::read_capsule_from(
                    vfs,
                    self.destination,
                    self.keys,
                    object_id,
                    &mut Vec::new(),
                )
                .await
                .map_err(failure)?;
                if (self.template_digest)(&existing) != expected_digest {
                    return Err(HistoryVerificationError::TemplateDigestMismatch {
                        commit_seq,
                        object_id,
                    });
                }
            }
            Err(error) => return Err(CommitError::Io(error).into()),
        }
        Ok(())
    }
}
