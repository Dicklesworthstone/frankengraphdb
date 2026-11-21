//! One-batch authoritative replay over the already recovered Chronicle chain.
//! The cursor retains only weak source/cut identities, never another log,
//! subscription owner, retention lease, or acknowledgement authority.

use crate::{Database, LogicalDeltaBatch, ReadError, RebuildError, SendFuture};
use asupersync::fs::Vfs;
use fgdb_chronicle::marker::{CHAIN_ORIGIN, ChainedMarker, MarkerChain};
use fgdb_chronicle::root::IdentityTuple;
use fgdb_crypto::Digest;
use fgdb_types::{CommitCx, CommitSeq, MarkerRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cut {
    sequence: CommitSeq,
    marker: Option<MarkerRef>,
    chain_hash: Digest,
}

impl Cut {
    fn capture(chain: &MarkerChain, sequence: CommitSeq) -> Result<Self, DeltaReplayError> {
        if sequence == CommitSeq::ORIGIN {
            return Ok(Self {
                sequence,
                marker: None,
                chain_hash: CHAIN_ORIGIN,
            });
        }
        Ok(Self::from_entry(entry_at(chain, sequence)?))
    }

    fn from_entry(entry: &ChainedMarker) -> Self {
        let sequence = CommitSeq(entry.marker.commit_seq);
        Self {
            sequence,
            marker: Some(MarkerRef {
                marker_oid: entry.marker_oid,
                commit_seq: sequence,
            }),
            chain_hash: entry.chain_hash,
        }
    }

    fn verify(self, chain: &MarkerChain) -> Result<(), DeltaReplayError> {
        if Self::capture(chain, self.sequence)? != self {
            return Err(DeltaReplayError::HistoryChanged { at: self.sequence });
        }
        Ok(())
    }
}

fn entry_at(chain: &MarkerChain, at: CommitSeq) -> Result<&ChainedMarker, DeltaReplayError> {
    let entries = chain.entries();
    let position = entries.partition_point(|entry| entry.marker.commit_seq < at.0);
    entries
        .get(position)
        .filter(|entry| entry.marker.commit_seq == at.0)
        .ok_or(DeltaReplayError::Gap { expected: at })
}

/// A privileged embedded replay position within one fixed committed interval.
///
/// Create it with [`Database::delta_replay`], then pull with
/// [`Database::next_delta_replay`]. Each successful pull returns one complete
/// canonical batch and advances `position`; errors and dropped futures leave
/// it unchanged. Empty logical ticks are real batches and advance normally.
/// Later database commits never extend `through`.
///
/// The root identity tuple and both boundary marker/chain commitments are
/// rechecked on every pull, including after reopening the database. Cloning a
/// cursor permits repeat delivery of the same canonical batch; it grants no
/// authorization or exactly-once effects. A process can also start a new reader
/// from its explicitly retained sequence pair. That is a fresh source selection,
/// not authentication of a serialized cursor or proof of a prior delivery.
/// Under the current embedded DirectoryBound identity, identical histories in
/// the same security namespace have the same source identity; this does not
/// establish physical-directory uniqueness or external rollback resistance.
///
/// No graph snapshot, capsule, or replay window is retained by this value.
/// Newly retired history can therefore refuse a later pull. This is a raw
/// Local committed-delta source, not a query-result subscription, public resume
/// capability, CDC wire protocol, or protected/masked feed.
#[derive(Clone, PartialEq, Eq)]
#[must_use = "a replay cursor has no effect until its committed batches are pulled"]
pub struct DeltaReplayCursor {
    source: IdentityTuple,
    from: CommitSeq,
    position: Cut,
    through: Cut,
}

impl core::fmt::Debug for DeltaReplayCursor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeltaReplayCursor")
            .field("from", &self.from)
            .field("position", &self.position.sequence)
            .field("through", &self.through.sequence)
            .field("source_binding", &"[REDACTED]")
            .finish()
    }
}

impl DeltaReplayCursor {
    /// The exclusive beginning of the originally admitted interval.
    #[must_use]
    pub fn from(&self) -> CommitSeq {
        self.from
    }

    /// Last successfully returned batch, or `from` before the first pull.
    #[must_use]
    pub fn position(&self) -> CommitSeq {
        self.position.sequence
    }

    /// Inclusive end selected at construction; later commits cannot move it.
    #[must_use]
    pub fn through(&self) -> CommitSeq {
        self.through.sequence
    }

    /// Weak marker identity at `position`, absent only at the origin.
    #[must_use]
    pub fn position_marker(&self) -> Option<MarkerRef> {
        self.position.marker
    }

    /// Whether every batch in this cursor's admitted interval was returned.
    /// This does not bypass health, identity, or retention checks on later pulls.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.position.sequence == self.through.sequence
    }

    fn next_sequence(&self) -> Result<Option<CommitSeq>, DeltaReplayError> {
        if self.position.sequence > self.through.sequence {
            return Err(DeltaReplayError::InvalidRange {
                after: self.position.sequence,
                through: self.through.sequence,
            });
        }
        if self.is_exhausted() {
            return Ok(None);
        }
        self.position
            .sequence
            .checked_successor()
            .map(Some)
            .map_err(|_| DeltaReplayError::SequenceExhausted {
                after: self.position.sequence,
            })
    }
}

/// A complete-batch replay refusal. Existing read and reconstruction errors
/// retain their typed future, retired, unhealthy, missing-capsule, corruption,
/// canonical-decoding, and I/O details; no failure becomes an empty suffix.
#[derive(Debug)]
pub enum DeltaReplayError {
    Read(ReadError),
    InvalidRange {
        after: CommitSeq,
        through: CommitSeq,
    },
    ForeignSource,
    HistoryChanged {
        at: CommitSeq,
    },
    Gap {
        expected: CommitSeq,
    },
    SequenceExhausted {
        after: CommitSeq,
    },
    Interrupted(Box<asupersync::error::Error>),
    Reconstruction(Box<RebuildError>),
}

impl From<ReadError> for DeltaReplayError {
    fn from(error: ReadError) -> Self {
        Self::Read(error)
    }
}

impl From<RebuildError> for DeltaReplayError {
    fn from(error: RebuildError) -> Self {
        match error {
            RebuildError::Interrupted(error) => Self::Interrupted(error),
            error => Self::Reconstruction(Box::new(error)),
        }
    }
}

impl core::fmt::Display for DeltaReplayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Read(error) => error.fmt(f),
            Self::InvalidRange { after, through } => {
                write!(
                    f,
                    "delta replay starts at {after:?} after its end {through:?}"
                )
            }
            Self::ForeignSource => f.write_str("delta replay belongs to another database identity"),
            Self::HistoryChanged { at } => {
                write!(f, "delta replay's committed history changed at {at:?}")
            }
            Self::Gap { expected } => {
                write!(f, "delta replay lacks the committed marker at {expected:?}")
            }
            Self::SequenceExhausted { after } => {
                write!(f, "delta replay sequence is exhausted after {after:?}")
            }
            Self::Interrupted(error) => error.fmt(f),
            Self::Reconstruction(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for DeltaReplayError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Interrupted(error) => Some(error.as_ref()),
            Self::Reconstruction(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Select exactly `(after, through]` from this healthy Chronicle history.
    ///
    /// Both cuts must be committed and at or below the published frontier.
    /// An equal pair is a valid empty interval, including the origin. This
    /// checks source/cut identities but performs no capsule I/O or registration.
    /// Every pull repeats admission before exposing a complete batch.
    ///
    /// Unlike [`Self::ensure_delta_window`], replay does not hydrate the missing
    /// historical prefix of a checkpoint-opened `LocalDeltaBatchIndex`. It may
    /// read that unloaded prefix from Chronicle, but it never overrides an
    /// explicit retirement. The cursor holds no retention lease.
    pub fn delta_replay(
        &self,
        after: CommitSeq,
        through: CommitSeq,
    ) -> Result<DeltaReplayCursor, DeltaReplayError> {
        self.admit_delta_replay_range(after, through)?;
        let chain = self.coordinator.chain();
        Ok(DeltaReplayCursor {
            source: self.delta_replay_identity(),
            from: after,
            position: Cut::capture(chain, after)?,
            through: Cut::capture(chain, through)?,
        })
    }

    /// Authenticate and return the next complete batch in a fixed replay range.
    ///
    /// Uses the same capsule/template/marker verifier as recovery and lazy
    /// delta-window admission, even if this batch is already cached. At most
    /// one capsule is read per pull, and no history-sized batch collection or
    /// graph rebuild is created. The capsule decoder's existing object bounds
    /// apply; one large batch is indivisible and the returned batch is owned by
    /// the caller. This is not an allocator-byte or whole-handle memory ceiling.
    ///
    /// The existing `crypto_verification_events` evidence sink is preserved:
    /// successful verification and verification failures may append diagnostic
    /// events, including before a cancelled pull. Those records grow with reads
    /// and are not silently discarded to make a memory-bound claim. Graph state,
    /// published roots, loaded delta history, and cursor position are unchanged
    /// on failure. Only a fully verified, finally admitted batch advances the
    /// cursor, with no await between advancement and return.
    pub fn next_delta_replay<'a>(
        &'a mut self,
        cx: &'a CommitCx,
        cursor: &'a mut DeltaReplayCursor,
    ) -> SendFuture<'a, Result<Option<LogicalDeltaBatch>, DeltaReplayError>> {
        Box::pin(self.next_delta_replay_inner(cx, cursor))
    }

    async fn next_delta_replay_inner(
        &mut self,
        cx: &CommitCx,
        cursor: &mut DeltaReplayCursor,
    ) -> Result<Option<LogicalDeltaBatch>, DeltaReplayError> {
        self.ensure_readable()?;
        cx.checkpoint().map_err(DeltaReplayError::Interrupted)?;
        if cursor.source != self.delta_replay_identity() {
            return Err(DeltaReplayError::ForeignSource);
        }
        self.admit_delta_replay_range(cursor.position.sequence, cursor.through.sequence)?;
        let chain = self.coordinator.chain();
        cursor.position.verify(chain)?;
        cursor.through.verify(chain)?;
        let Some(next) = cursor.next_sequence()? else {
            return Ok(None);
        };
        let entry = entry_at(chain, next)?;
        if entry.marker.prev_global != cursor.position.marker {
            return Err(DeltaReplayError::HistoryChanged { at: next });
        }
        let completed = Cut::from_entry(entry);
        let result = crate::read_delta_batch(
            cx,
            &self.coordinator,
            &self.keys,
            entry,
            &mut self.crypto_verification_events,
        )
        .await;
        cx.checkpoint().map_err(DeltaReplayError::Interrupted)?;
        let batch = result.map_err(DeltaReplayError::from)?;
        // The exclusive database borrow prevents a publication or retention
        // change during I/O; cancellation still receives its final checkpoint.
        self.ensure_readable()?;
        if batch.commit_seq() != next
            || batch.frontier() != next
            || Some(batch.commit_marker_identity()) != completed.marker
        {
            return Err(DeltaReplayError::HistoryChanged { at: next });
        }
        cursor.position = completed;
        Ok(Some(batch))
    }

    fn delta_replay_identity(&self) -> IdentityTuple {
        // Reuse the root publication's sole identity construction, including
        // the current embedded DirectoryBound incarnation/visibility posture.
        // Generation, manifest and bootstrap bytes are not cursor identity.
        crate::spine_slot(&self.keys, 0, self.snapshot.manifest, 0).identity_tuple()
    }

    fn admit_delta_replay_range(
        &self,
        after: CommitSeq,
        through: CommitSeq,
    ) -> Result<(), DeltaReplayError> {
        self.ensure_readable()?;
        if after > through {
            return Err(DeltaReplayError::InvalidRange { after, through });
        }
        self.snapshot.check_frontier(through)?;
        let index = &self.snapshot.delta_index;
        if index.retained_after_commit_seq() != self.delta_materialized_after {
            // Same distinction as ensure_delta_window: an omitted checkpoint
            // prefix may be authenticated lazily; an explicit cut may not.
            index
                .since(after)
                .map(drop)
                .map_err(crate::read_error_from_index)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
