//! Read-only verification of the authoritative history through one exact commit.

use super::{
    COMMIT_LOG_NAME, CapsuleKeys, CommitCoordinator, CommitError, EffectSource, EntryDefect,
    MarkerPrefix, recovered_marker_error, scan_checkpoint,
};
use asupersync::fs::{Vfs, VfsFile};
use asupersync::io::AsyncReadExt;
use fgdb_crypto::Digest;
use fgdb_types::{CommitCx, CommitSeq, ObjectId};
use std::num::NonZeroU64;
use std::path::Path;

/// The first failure to prove a requested history prefix.
#[derive(Debug)]
pub enum HistoryVerificationError {
    Marker(CommitError),
    /// End of the captured log, including a torn final frame, before the cut.
    Unreached {
        requested: CommitSeq,
        verified: CommitSeq,
    },
    Capsule {
        commit_seq: CommitSeq,
        object_id: ObjectId,
        source: CommitError,
    },
    TemplateDigestMismatch {
        commit_seq: CommitSeq,
        object_id: ObjectId,
    },
}

impl core::fmt::Display for HistoryVerificationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Marker(error) => write!(f, "history verification: {error}"),
            Self::Unreached {
                requested,
                verified,
            } => write!(
                f,
                "requested commit {} is absent; verified through {}",
                requested.0, verified.0
            ),
            Self::Capsule {
                commit_seq,
                object_id,
                source,
            } => write!(
                f,
                "commit {} capsule {object_id:?} cannot be recovered: {source}",
                commit_seq.0
            ),
            Self::TemplateDigestMismatch {
                commit_seq,
                object_id,
            } => write!(
                f,
                "commit {} capsule {object_id:?} has the wrong logical template digest",
                commit_seq.0
            ),
        }
    }
}

impl core::error::Error for HistoryVerificationError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Marker(error) | Self::Capsule { source: error, .. } => Some(error),
            _ => None,
        }
    }
}

impl From<CommitError> for HistoryVerificationError {
    fn from(error: CommitError) -> Self {
        Self::Marker(error)
    }
}

impl<V: Vfs> CommitCoordinator<V> {
    /// Verify the complete marker ancestry, recover each referenced capsule
    /// with the supplied keys, and check each marker's logical template digest
    /// using its payload owner's digest function. Return the chain commitment
    /// at exactly `through` for comparison with a trusted checkpoint.
    ///
    /// A live writer can keep appending. Only the captured log extent is visible
    /// and the frame after the requested cut is never read. No lease, root
    /// admission, truncation or object repair participates. Memory tracks only
    /// branch heads, the current marker, and one capsule's recovery workspace.
    pub async fn verify_history_through(
        cx: &CommitCx,
        vfs: &V,
        database_dir: impl AsRef<Path>,
        keys: &CapsuleKeys,
        through: NonZeroU64,
        template_digest: fn(&[u8]) -> Digest,
    ) -> Result<Digest, HistoryVerificationError> {
        let dir = database_dir.as_ref();
        Self::walk_capsules_through(
            cx,
            vfs,
            dir,
            through,
            async |commit_seq, capsule_ref, logical_delta_template_digest| {
                let bytes = Self::read_capsule_from(vfs, dir, keys, capsule_ref, &mut Vec::new())
                    .await
                    .map_err(|source| HistoryVerificationError::Capsule {
                        commit_seq,
                        object_id: capsule_ref,
                        source,
                    })?;
                if template_digest(&bytes) != logical_delta_template_digest {
                    return Err(HistoryVerificationError::TemplateDigestMismatch {
                        commit_seq,
                        object_id: capsule_ref,
                    });
                }
                Ok(())
            },
        )
        .await
    }

    /// Visit the capsule named by each structurally verified marker through an
    /// exact cut, in commit order, even if the capsule itself is missing. No
    /// capsule, key file, writer lease or root is opened. The returned chain
    /// hash can be compared with an independently retained commitment.
    ///
    /// The visitor receives one record at a time. Earlier records may have
    /// been delivered on error. Only a successful return covers the whole cut.
    pub async fn visit_capsule_refs_through(
        cx: &CommitCx,
        vfs: &V,
        database_dir: impl AsRef<Path>,
        through: NonZeroU64,
        mut visit: impl FnMut(CommitSeq, ObjectId) -> std::io::Result<()>,
    ) -> Result<Digest, HistoryVerificationError> {
        Self::walk_capsules_through(
            cx,
            vfs,
            database_dir.as_ref(),
            through,
            async |seq, id, _| {
                visit(seq, id).map_err(CommitError::Io)?;
                Ok(())
            },
        )
        .await
    }

    async fn walk_capsules_through(
        cx: &CommitCx,
        vfs: &V,
        dir: &Path,
        through: NonZeroU64,
        mut visit: impl AsyncFnMut(CommitSeq, ObjectId, Digest) -> Result<(), HistoryVerificationError>,
    ) -> Result<Digest, HistoryVerificationError> {
        let requested = CommitSeq(through.get());
        cx.with_restriction_async(async {
            scan_checkpoint(cx)?;
            let file = match vfs.open_read(&dir.join(COMMIT_LOG_NAME)).await {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(HistoryVerificationError::Unreached {
                        requested,
                        verified: CommitSeq::ORIGIN,
                    });
                }
                Err(error) => return Err(CommitError::Io(error).into()),
            };
            let extent = file.metadata().await.map_err(CommitError::Io)?.len();
            let mut reader = file.take(extent);
            let mut prefix = MarkerPrefix::default();
            let mut entry = Vec::new();
            loop {
                scan_checkpoint(cx)?;
                let decoded = Self::read_next_entry(&mut reader, &mut entry, prefix.chain_value())
                    .await
                    .map_err(CommitError::Io)?;
                let (marker, hash, _) = match decoded {
                    Some(Ok(frame)) => frame,
                    None | Some(Err(EntryDefect::Truncated)) => {
                        return Err(HistoryVerificationError::Unreached {
                            requested,
                            verified: prefix.frontier(),
                        });
                    }
                    Some(Err(EntryDefect::Corrupt)) => {
                        return Err(CommitError::CorruptLogEntry {
                            commit_seq: prefix.next_commit_seq().map_err(CommitError::from)?.0,
                        }
                        .into());
                    }
                };
                let EffectSource::Local {
                    capsule_ref,
                    logical_delta_template_digest,
                } = marker.effect_source;
                prefix
                    .recover(marker, hash)
                    .map_err(recovered_marker_error)?;
                let commit_seq = prefix.frontier();
                visit(commit_seq, capsule_ref, logical_delta_template_digest).await?;
                scan_checkpoint(cx)?;
                if commit_seq == requested {
                    return Ok(hash);
                }
            }
        })
        .await
    }
}
