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
        cx.with_restriction_async(async {
            let mut frames = HistoryFrames::open(cx, vfs, dir, through).await?;
            loop {
                let frame = frames.next(cx).await?;
                let bytes = Self::read_capsule_from(vfs, dir, keys, frame.capsule, &mut Vec::new())
                    .await
                    .map_err(|source| HistoryVerificationError::Capsule {
                        commit_seq: frame.seq,
                        object_id: frame.capsule,
                        source,
                    })?;
                if template_digest(&bytes) != frame.template_digest {
                    return Err(HistoryVerificationError::TemplateDigestMismatch {
                        commit_seq: frame.seq,
                        object_id: frame.capsule,
                    });
                }
                scan_checkpoint(cx)?;
                if frame.seq.0 == through.get() {
                    return Ok(frame.chain_hash);
                }
            }
        })
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
        let dir = database_dir.as_ref();
        cx.with_restriction_async(async {
            let mut frames = HistoryFrames::open(cx, vfs, dir, through).await?;
            loop {
                let frame = frames.next(cx).await?;
                visit(frame.seq, frame.capsule).map_err(CommitError::Io)?;
                scan_checkpoint(cx)?;
                if frame.seq.0 == through.get() {
                    return Ok(frame.chain_hash);
                }
            }
        })
        .await
    }
}

/// One acquired log extent and its verified prefix. All consumers read the
/// same frames, including the archive writer that must preserve the exact bytes.
pub(super) struct HistoryFrames<V: Vfs> {
    reader: asupersync::io::Take<V::File>,
    prefix: MarkerPrefix,
    bytes: Vec<u8>,
    requested: CommitSeq,
}

pub(super) struct HistoryFrame<'a> {
    pub(super) bytes: &'a [u8],
    pub(super) seq: CommitSeq,
    pub(super) capsule: ObjectId,
    pub(super) template_digest: Digest,
    pub(super) chain_hash: Digest,
}

impl<V: Vfs> HistoryFrames<V> {
    pub(super) async fn open(
        cx: &CommitCx,
        vfs: &V,
        dir: &Path,
        through: NonZeroU64,
    ) -> Result<Self, HistoryVerificationError> {
        scan_checkpoint(cx)?;
        let requested = CommitSeq(through.get());
        let missing_or_io = |error: std::io::Error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                HistoryVerificationError::Unreached {
                    requested,
                    verified: CommitSeq::ORIGIN,
                }
            } else {
                CommitError::Io(error).into()
            }
        };
        let invalid = || {
            CommitError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "history journal is not a regular file",
            ))
        };
        let path = dir.join(COMMIT_LOG_NAME);
        if !vfs
            .symlink_metadata(&path)
            .await
            .map_err(missing_or_io)?
            .file_type()
            .is_file()
        {
            return Err(invalid().into());
        }
        let file = vfs.open_read(&path).await.map_err(missing_or_io)?;
        let meta = file.metadata().await.map_err(CommitError::Io)?;
        if !meta.file_type().is_file() {
            return Err(invalid().into());
        }
        Ok(Self {
            reader: file.take(meta.len()),
            prefix: MarkerPrefix::default(),
            bytes: Vec::new(),
            requested,
        })
    }

    pub(super) async fn next(
        &mut self,
        cx: &CommitCx,
    ) -> Result<HistoryFrame<'_>, HistoryVerificationError> {
        scan_checkpoint(cx)?;
        let decoded = CommitCoordinator::<V>::read_next_entry(
            &mut self.reader,
            &mut self.bytes,
            self.prefix.chain_value(),
        )
        .await
        .map_err(CommitError::Io)?;
        let (marker, hash, _) = match decoded {
            Some(Ok(frame)) => frame,
            None | Some(Err(EntryDefect::Truncated)) => {
                return Err(HistoryVerificationError::Unreached {
                    requested: self.requested,
                    verified: self.prefix.frontier(),
                });
            }
            Some(Err(EntryDefect::Corrupt)) => {
                return Err(CommitError::CorruptLogEntry {
                    commit_seq: self.prefix.next_commit_seq().map_err(CommitError::from)?.0,
                }
                .into());
            }
        };
        let EffectSource::Local {
            capsule_ref,
            logical_delta_template_digest,
        } = marker.effect_source;
        self.prefix
            .recover(marker, hash)
            .map_err(recovered_marker_error)?;
        Ok(HistoryFrame {
            bytes: &self.bytes,
            seq: self.prefix.frontier(),
            capsule: capsule_ref,
            template_digest: logical_delta_template_digest,
            chain_hash: hash,
        })
    }
}
