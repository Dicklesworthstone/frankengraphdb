//! `LocalDeltaBatchIndex` — the bounded authenticated window over committed
//! delta batches, and the frontier that says how far it reaches.
//!
//! **A delta index is not an append-only map** (plan:1928). It is a *window*:
//! entries exist for every commit sequence in `(retained_after_commit_seq,
//! frontier]` and for none outside it, and the interval is required to be a
//! **gap-free exact map**. Both halves of that matter — a gap makes "the deltas
//! since N" unanswerable, and an entry outside the window is a batch the
//! retention story says was already dropped.
//!
//! THE LAW THIS TYPE EXISTS TO MAKE UNBREAKABLE (plan:397): apply "inserts
//! `(next_commit_seq -> batch_ref)` into `LocalDeltaBatchIndex`, and advances
//! `local_delta_frontier` to that same commit sequence **in the same state
//! transition**. The batch is therefore reachable from the candidate root at
//! the first crash point at which the commit exists; there is no separate Local
//! delta-publish command or unrooted interval."
//!
//! So [`insert`](LocalDeltaBatchIndex::insert) is the only mutator that adds
//! anything, and it does both jobs or neither. There is deliberately no
//! `advance_frontier` and no `put_entry`: an API that could do one without the
//! other would let a commit exist whose batch is unreachable, which is the
//! interval the plan says must not exist. The invariant is maintained by
//! construction rather than checked afterwards.
//!
//! Plan:397 also enumerates the batch/index disagreements — "a missing,
//! duplicate, gapped, wrong-marker, or wrong-frontier insertion fails apply" —
//! and [`IndexError`] preserves that list, one arm each. The independent §5.2
//! arithmetic law adds the permanent `CommitSeqExhausted` refusal: the list of
//! malformed insertions does not authorize wrapping the global frontier.
//!
//! SUBSET NOTE (doctrine 7). The plan spells entries as
//! `StrongRef<LogicalDeltaBatch<LocalCommitted>>` and carries a
//! `retired_prefix_commitment` plus an `active_cut_ref`. The kinds are
//! `reserved` rather than `active` so the ref is not spellable, and the
//! commitment is a digest this crate cannot compute (`fgdb-crypto` is a higher
//! foundation position). Entries are therefore held by value — which is
//! consistent with the retention story, since the index is what retains a batch
//! — and [`retire_prefix`](LocalDeltaBatchIndex::retire_prefix) *returns* the
//! batches it dropped so the consumer that owns hashing can commit to exactly
//! them. A commitment field stored here would be one nothing in this crate
//! could verify, and so one free to lie.

use crate::LogicalDeltaBatch;
use fgdb_types::{CommitSeq, CommitSeqExhausted as CommitSeqExhaustion};
use std::collections::BTreeMap;

/// Index format version (§16.6: durable formats are versioned from day one).
pub const INDEX_FORMAT_V1: u16 = 1;

/// Why an insertion or a retirement was refused.
///
/// The batch-disagreement arms are plan:397's enumeration verbatim, because a
/// caller that hits one needs to know which condition it hit — they have
/// different causes and different fixes. Sequence exhaustion is the separate
/// §5.2 permanent fail-closed condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexError {
    /// The persisted frontier is the largest representable sequence, so no
    /// further batch can be assigned without wrapping to the reserved origin.
    CommitSeqExhausted(CommitSeqExhaustion),
    /// The batch's sequence is beyond `frontier + 1`: inserting it would leave
    /// a hole, making "the deltas since N" unanswerable for the skipped range.
    Gapped {
        expected: CommitSeq,
        found: CommitSeq,
    },
    /// The batch's sequence is at or below the frontier, so this window already
    /// covers it. Re-inserting would either duplicate an entry or silently
    /// replace one, and replacing is worse: the index would stop agreeing with
    /// the commit stream while still looking gap-free.
    Duplicate {
        frontier: CommitSeq,
        found: CommitSeq,
    },
    /// The batch's marker identity names a different commit sequence than the
    /// batch claims. One of the two is wrong and the index cannot tell which,
    /// so it refuses rather than pick.
    WrongMarker {
        batch_commit_seq: CommitSeq,
        marker_commit_seq: CommitSeq,
    },
    /// A local batch's own frontier must be its own commit sequence
    /// (plan:1926, `frontier: Local{commit_seq}`). Anything else is a batch
    /// built for a different role.
    WrongFrontier {
        commit_seq: CommitSeq,
        frontier: CommitSeq,
    },
    /// The durable index key and the batch stored under it name different
    /// commits. A decoder must preserve both values independently so
    /// verification can bind the map structure to the batch identity.
    WrongEntryKey { stored: CommitSeq, batch: CommitSeq },
    /// The declared index format is not one this build reads. Format is the
    /// first law verification enforces, because every other law is written
    /// against a format it does not know it has (fgdb-dzh4 item 1).
    UnsupportedFormat { format: u16 },
    /// A retirement that would move the retained floor past the frontier, or
    /// backwards. Either would leave the window describing an interval it does
    /// not hold.
    UnretirableInterval {
        retained_after: CommitSeq,
        frontier: CommitSeq,
        requested: CommitSeq,
    },
}

impl core::fmt::Display for IndexError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CommitSeqExhausted(cause) => write!(f, "{cause}"),
            Self::Gapped { expected, found } => write!(
                f,
                "gapped insertion: expected commit_seq {expected:?}, found {found:?}"
            ),
            Self::Duplicate { frontier, found } => write!(
                f,
                "duplicate insertion: frontier is already {frontier:?}, found {found:?}"
            ),
            Self::WrongMarker {
                batch_commit_seq,
                marker_commit_seq,
            } => write!(
                f,
                "wrong marker: batch claims {batch_commit_seq:?}, its marker names {marker_commit_seq:?}"
            ),
            Self::WrongFrontier {
                commit_seq,
                frontier,
            } => write!(
                f,
                "wrong frontier: a local batch at {commit_seq:?} declares frontier {frontier:?}"
            ),
            Self::WrongEntryKey { stored, batch } => write!(
                f,
                "wrong entry key: index key {stored:?} stores a batch for {batch:?}"
            ),
            Self::UnsupportedFormat { format } => {
                write!(f, "index format {format} is not implemented")
            }
            Self::UnretirableInterval {
                retained_after,
                frontier,
                requested,
            } => write!(
                f,
                "cannot retire through {requested:?}: window is ({retained_after:?}, {frontier:?}]"
            ),
        }
    }
}

impl core::error::Error for IndexError {}

impl From<CommitSeqExhaustion> for IndexError {
    fn from(cause: CommitSeqExhaustion) -> Self {
        Self::CommitSeqExhausted(cause)
    }
}

/// The bounded window of retained local delta batches.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalDeltaBatchIndex {
    format: u16,
    retained_after_commit_seq: CommitSeq,
    frontier: CommitSeq,
    entries: BTreeMap<u64, LogicalDeltaBatch>,
}

impl Default for LocalDeltaBatchIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalDeltaBatchIndex {
    /// An empty window at the origin. `retained_after == frontier` means the
    /// interval `(0, 0]` is empty, which is exactly right before the first
    /// commit — not a special "uninitialised" state needing its own handling.
    pub fn new() -> Self {
        Self {
            format: INDEX_FORMAT_V1,
            retained_after_commit_seq: CommitSeq::ORIGIN,
            frontier: CommitSeq::ORIGIN,
            entries: BTreeMap::new(),
        }
    }

    /// Build a window from parts, INCLUDING incoherent ones.
    ///
    /// Test-facing. `insert` and `retire_prefix` maintain the gap-free
    /// invariant by construction, so a broken window is unreachable through the
    /// safe API — which would leave [`verify`](Self::verify) untestable, and an
    /// untested checker is indistinguishable from one that always returns `Ok`.
    /// A decoder reading durable bytes will need exactly this shape.
    #[doc(hidden)]
    pub fn from_parts_for_test(
        retained_after_commit_seq: CommitSeq,
        frontier: CommitSeq,
        keyed_batches: Vec<(CommitSeq, LogicalDeltaBatch)>,
    ) -> Self {
        let mut entries = BTreeMap::new();
        for (stored_seq, batch) in keyed_batches {
            entries.insert(stored_seq.0, batch);
        }
        Self {
            format: INDEX_FORMAT_V1,
            retained_after_commit_seq,
            frontier,
            entries,
        }
    }

    pub fn format(&self) -> u16 {
        self.format
    }

    pub fn frontier(&self) -> CommitSeq {
        self.frontier
    }

    pub fn retained_after_commit_seq(&self) -> CommitSeq {
        self.retained_after_commit_seq
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, commit_seq: CommitSeq) -> Option<&LogicalDeltaBatch> {
        self.entries.get(&commit_seq.0)
    }

    /// Every retained batch in commit order.
    pub fn iter(&self) -> impl Iterator<Item = &LogicalDeltaBatch> {
        self.entries.values()
    }

    /// The sequence the next insertion must carry.
    pub fn next_commit_seq(&self) -> Result<CommitSeq, IndexError> {
        Ok(self.frontier.checked_successor()?)
    }

    /// Validate the laws intrinsic to one local batch, independently of where
    /// an index stores it.
    ///
    /// Both insertion and durable-state verification use this one predicate.
    /// Otherwise `insert` can be fail-closed while a decoder constructs the
    /// same malformed batch through `from_parts` and `verify` blesses it.
    fn validate_batch(batch: &LogicalDeltaBatch) -> Result<(), IndexError> {
        let commit_seq = batch.commit_seq();
        let marker_commit_seq = batch.commit_marker_identity().commit_seq;
        if marker_commit_seq != commit_seq {
            return Err(IndexError::WrongMarker {
                batch_commit_seq: commit_seq,
                marker_commit_seq,
            });
        }
        if batch.frontier() != commit_seq {
            return Err(IndexError::WrongFrontier {
                commit_seq,
                frontier: batch.frontier(),
            });
        }
        Ok(())
    }

    /// Insert a batch AND advance the frontier — one transition, or neither.
    ///
    /// This is the only way to add anything, deliberately. Separate
    /// `put_entry` and `advance_frontier` operations would permit a state where
    /// a commit exists and its batch is not yet reachable, which plan:397 says
    /// must never exist ("no separate Local delta-publish command or unrooted
    /// interval"). Every check runs before anything mutates, so a refusal
    /// leaves the index exactly as it was.
    pub fn insert(&mut self, batch: LogicalDeltaBatch) -> Result<(), IndexError> {
        let commit_seq = batch.commit_seq();

        // Once the persisted frontier is MAX, every attempted mutation is the
        // same permanent exhaustion refusal. Check this before inspecting the
        // offered batch so no diagnostic can make a wrapped/reused sequence
        // look admissible.
        let expected = self.next_commit_seq()?;

        // A local batch must agree with itself before the index will agree with
        // it: its marker must name its own sequence, and its frontier must be
        // that sequence. Both are cheap here and impossible to check later.
        Self::validate_batch(&batch)?;

        // Duplicate is checked before gap so the diagnostic names the right
        // problem: a sequence at or below the frontier is not "a gap of
        // negative size", it is a re-insertion.
        if commit_seq.0 <= self.frontier.0 {
            return Err(IndexError::Duplicate {
                frontier: self.frontier,
                found: commit_seq,
            });
        }
        if commit_seq != expected {
            return Err(IndexError::Gapped {
                expected,
                found: commit_seq,
            });
        }
        // The duplicate law binds the ACTUAL map, not only the derived
        // frontier: a decoder-shaped window can hold keys above its claimed
        // frontier, and `entries.insert` would silently REPLACE one — the
        // outcome the `Duplicate` arm exists to forbid (fgdb-uqkt).
        if self.entries.contains_key(&commit_seq.0) {
            return Err(IndexError::Duplicate {
                frontier: self.frontier,
                found: commit_seq,
            });
        }

        self.entries.insert(commit_seq.0, batch);
        self.frontier = commit_seq;
        Ok(())
    }

    /// Retire everything at or below `through`, returning the dropped batches
    /// in commit order.
    ///
    /// They are RETURNED rather than discarded because the consumer owes a
    /// `retired_prefix_commitment` over exactly them, and it is the only layer
    /// that can hash. Dropping them here would leave that commitment
    /// uncomputable from anything the index still holds.
    pub fn retire_prefix(
        &mut self,
        through: CommitSeq,
    ) -> Result<Vec<LogicalDeltaBatch>, IndexError> {
        if through.0 < self.retained_after_commit_seq.0 || through.0 > self.frontier.0 {
            return Err(IndexError::UnretirableInterval {
                retained_after: self.retained_after_commit_seq,
                frontier: self.frontier,
                requested: through,
            });
        }
        let mut retired = Vec::new();
        let keys: Vec<u64> = self
            .entries
            .range(..=through.0)
            .map(|(seq, _)| *seq)
            .collect();
        for key in keys {
            if let Some(batch) = self.entries.remove(&key) {
                retired.push(batch);
            }
        }
        self.retained_after_commit_seq = through;
        Ok(retired)
    }

    /// Check the window invariant: exactly one entry for every sequence in
    /// `(retained_after_commit_seq, frontier]` and nothing outside it.
    ///
    /// `insert` and `retire_prefix` maintain this by construction, so this is
    /// for a value that arrived some other way — decoded from durable bytes, or
    /// built by a future apply path — where "maintained by construction" is a
    /// claim about code that did not run here.
    ///
    /// The walk is proportional to retained entries, not to the numeric width
    /// of the claimed interval. A corrupt frontier near `u64::MAX` therefore
    /// cannot turn verification of a small decoded value into a vast loop.
    pub fn verify(&self) -> Result<(), IndexError> {
        // The format law is first, the same order the template validate gives
        // its own format arm (canonical.rs): a decoder-shaped index carrying
        // an unknown format must not verify clean because every OTHER law
        // happens to hold (fgdb-dzh4 item 1).
        if self.format != INDEX_FORMAT_V1 {
            return Err(IndexError::UnsupportedFormat {
                format: self.format,
            });
        }
        if self.retained_after_commit_seq.0 > self.frontier.0 {
            return Err(IndexError::UnretirableInterval {
                retained_after: self.retained_after_commit_seq,
                frontier: self.frontier,
                requested: self.retained_after_commit_seq,
            });
        }

        let mut previous = self.retained_after_commit_seq.0;
        for (stored_seq, batch) in &self.entries {
            if *stored_seq <= self.retained_after_commit_seq.0 || *stored_seq > self.frontier.0 {
                return Err(IndexError::UnretirableInterval {
                    retained_after: self.retained_after_commit_seq,
                    frontier: self.frontier,
                    requested: CommitSeq(*stored_seq),
                });
            }
            let batch_commit_seq = batch.commit_seq();
            if batch_commit_seq.0 != *stored_seq {
                return Err(IndexError::WrongEntryKey {
                    stored: CommitSeq(*stored_seq),
                    batch: batch_commit_seq,
                });
            }
            let expected = CommitSeq(previous).checked_successor()?;
            if CommitSeq(*stored_seq) != expected {
                return Err(IndexError::Gapped {
                    expected,
                    found: CommitSeq(*stored_seq),
                });
            }
            Self::validate_batch(batch)?;
            previous = *stored_seq;
        }

        if previous != self.frontier.0 {
            let expected = CommitSeq(previous).checked_successor()?;
            return Err(IndexError::Gapped {
                expected,
                found: self.frontier,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{IndexError, LocalDeltaBatchIndex};

    /// The format law is the FIRST law verify enforces: a window carrying an
    /// unknown format must not verify clean because every other law holds
    /// (fgdb-dzh4 item 1). Private-field access is exactly why this lives in
    /// the crate rather than the integration suite — the durable decoder
    /// path is what the format field exists for.
    #[test]
    fn verify_refuses_an_unknown_format() {
        let mut index = LocalDeltaBatchIndex::new();
        index.format = 99;
        assert_eq!(
            index.verify(),
            Err(IndexError::UnsupportedFormat { format: 99 })
        );
    }
}
