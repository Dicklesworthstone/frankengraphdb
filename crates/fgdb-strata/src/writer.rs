//! The tier-D writer: delta rows in, sealed blocks and a root out.
//!
//! Until now `fgdb-strata` was a format with no producer — blocks existed only
//! because a test built one. This is the piece that makes the tier part of the
//! database: it consumes the same [`DeltaRow`]s the commit stream carries and
//! emits the blocks a partition root names.
//!
//! **IT IS A FOLD, NOT A QUERY.** The writer is fed rows in commit order and
//! never reads back what it wrote. That is what keeps ingest append-only, and it
//! is the whole reason slice 4 chose tombstone supersede over version chains: a
//! writer that had to look up a key's prior versions before sealing would be doing
//! a read on the write path.
//!
//! **IT MUST TRACK LIVE EDGES, AND THAT IS NOT AN INDEX.** A `DeleteEdge` row
//! names an `EId`; an adjacency entry needs `(src, relation, dst)` and the sequence
//! the version began at. Only the creation carries those, so the writer remembers
//! them for edges it has seen live. This is bounded by the live edge count, it is
//! rebuildable by replaying the stream, and it is exactly the state a memtable
//! holds — not a derived structure that could become authoritative. Doctrine 5
//! stands: recovery discards and rebuilds it.
//!
//! **A KEY'S SECOND VERSION FORCES A SEAL.** A block requires strictly ascending
//! unique keys, so a writer that retires a key and re-creates it cannot hold both
//! versions in one pending run. That constraint was discovered by the differential
//! in slice 4 rather than declared, and this is where it is honoured: the writer
//! seals early rather than producing a block the encoder would refuse.

use crate::root::{BlockRef, PartitionRoot, RootError, span_of, validate_root};
use crate::{
    AdjacencyEntry, BlockError, MAX_BLOCK_ENTRIES, block_id, encode_block, validate_entry,
};
use fgdb_delta_types::{DeltaRow, RelationId};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};
use std::collections::BTreeMap;

/// A sealed block: its identity, its bytes, and the range it covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedBlock {
    pub block_id: ObjectId,
    pub bytes: Vec<u8>,
    pub first_seq: CommitSeq,
    pub last_seq: CommitSeq,
}

impl SealedBlock {
    /// The reference a root carries for this block.
    pub fn reference(&self) -> BlockRef {
        BlockRef {
            block_id: self.block_id,
            first_seq: self.first_seq,
            last_seq: self.last_seq,
        }
    }
}

/// Why the writer could not fold a row, seal a block, or publish a root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// A `DeleteEdge` or cascade named an edge this writer never saw created.
    ///
    /// Refused rather than skipped, and that matters: the writer's live-edge map
    /// is rebuilt by replaying the stream from the beginning, so a delete it
    /// cannot resolve means the stream is not being replayed from the beginning —
    /// or is missing a row. Skipping would silently produce a partition whose
    /// adjacency disagrees with the history that built it.
    UnknownEdge { eid: EId },
    /// A cascade's incident-edge list violated its strict ascending contract.
    ///
    /// The durable row carries the list in canonical order; anything else is
    /// not a cascade the stream produced. Refused at preflight, before any
    /// member retires — a mid-loop failure would leave the writer half-applied.
    CascadeOrderViolation { previous: EId, found: EId },
    /// A `CreateEdge` named an edge this writer already holds live.
    ///
    /// Refused rather than overwritten, and the asymmetry with a legal second
    /// version is exact: a re-CREATE of a live edge is not a new version of it
    /// (a retirement followed by a create is that), it is the stream failing to
    /// be a stream. Overwriting the live map would strand the first version —
    /// retired by nothing, answering every future snapshot (fgdb-3usp). Retire
    /// already refuses what it cannot resolve; create refuses what it cannot
    /// add.
    EdgeAlreadyLive { eid: EId },
    /// A row arrived at a sequence before the previous one.
    ///
    /// The writer is a fold over an ordered stream; out-of-order input would put
    /// entries in a block whose declared range no longer bounds them.
    SequenceNotAdvancing {
        previous: CommitSeq,
        offered: CommitSeq,
    },
    /// Sealing produced bytes the block encoder refused.
    Block(BlockError),
    /// The finished root violated a publication or structural law.
    Root(RootError),
}

impl core::fmt::Display for WriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownEdge { eid } => {
                write!(f, "no live version of {eid:?} to retire")
            }
            Self::CascadeOrderViolation { previous, found } => {
                write!(
                    f,
                    "cascade edges must be strictly ascending: {found:?} follows {previous:?}"
                )
            }
            Self::EdgeAlreadyLive { eid } => {
                write!(f, "{eid:?} is already live; a re-create is not a version")
            }
            Self::SequenceNotAdvancing { previous, offered } => write!(
                f,
                "rows must arrive in commit order; {offered:?} follows {previous:?}"
            ),
            Self::Block(error) => write!(f, "sealing: {error}"),
            Self::Root(error) => write!(f, "publishing: {error}"),
        }
    }
}

impl core::error::Error for WriteError {}

/// Folds delta rows into sealed blocks for one partition.
#[derive(Debug)]
pub struct BlockWriter {
    graph: GraphId,
    branch: BranchId,
    partition: u64,
    /// Entries not yet sealed, keyed so a second version of one key is detectable.
    pending: BTreeMap<(VId, RelationId, VId), AdjacencyEntry>,
    /// `(src, relation, dst, created_at)` for every edge currently live.
    live: BTreeMap<EId, (VId, RelationId, VId, CommitSeq)>,
    sealed: Vec<SealedBlock>,
    last_seq: Option<CommitSeq>,
}

impl BlockWriter {
    pub fn new(graph: GraphId, branch: BranchId, partition: u64) -> Self {
        Self {
            graph,
            branch,
            partition,
            pending: BTreeMap::new(),
            live: BTreeMap::new(),
            sealed: Vec::new(),
            last_seq: None,
        }
    }

    /// How many entries are pending — the caller's signal for when to seal.
    ///
    /// Sealing POLICY is deliberately not here. When to cut a block is a tier
    /// migration decision, which the plan requires to emit a decision card under a
    /// policy epoch; inventing a size threshold in the writer would make that
    /// decision silently and in the wrong place.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn sealed(&self) -> &[SealedBlock] {
        &self.sealed
    }

    /// Fold one row at `seq`, sealing early if it would collide with a pending key.
    /// Every typed refusal leaves the writer exactly as it was before the call.
    pub fn apply(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        seq: CommitSeq,
        row: &DeltaRow,
    ) -> Result<(), WriteError> {
        if let Some(previous) = self.last_seq
            && seq.0 < previous.0
        {
            return Err(WriteError::SequenceNotAdvancing {
                previous,
                offered: seq,
            });
        }
        match row {
            DeltaRow::CreateEdge {
                eid,
                src,
                relation,
                dst,
                ..
            } => {
                if self.live.contains_key(eid) {
                    return Err(WriteError::EdgeAlreadyLive { eid: *eid });
                }
                self.push(
                    keys,
                    AdjacencyEntry {
                        src: *src,
                        relation: *relation,
                        dst: *dst,
                        created_at: seq,
                        retired_at: None,
                    },
                )?;
                self.live.insert(*eid, (*src, *relation, *dst, seq));
            }
            DeltaRow::DeleteEdge { eid, .. } => {
                self.retire(keys, *eid, seq)?;
            }
            DeltaRow::DeleteVertex {
                sorted_retired_incident_edges,
                ..
            } => {
                // THE CASCADE IS TAKEN FROM THE ROW, not recomputed from the live
                // map. The row's image is checked against materialized state by the
                // materializer that produced it; recomputing here would be a second
                // opinion about which edges a deletion retires, and two opinions
                // about one fact is how they drift.
                self.preflight_retirements(sorted_retired_incident_edges, seq)?;
                for eid in sorted_retired_incident_edges {
                    self.retire(keys, *eid, seq)?;
                }
            }
            // Every other family is real and none of it is ADJACENCY. Vertex
            // creation, labels, properties, valid time, counters, escrow, sketches,
            // schema and constraints all belong to structures this tier does not
            // hold, and silently folding them into an adjacency block would be
            // worse than not storing them.
            _ => {}
        }
        self.last_seq = Some(seq);
        Ok(())
    }

    /// Prove every cascade member can retire before changing the first one.
    ///
    /// The commit stream carries this list in strict ascending-unique order
    /// (canonical decode enforces it), and this preflight enforces the WHOLE
    /// contract, not only adjacent equality: a non-adjacent duplicate
    /// (`[10, 20, 10]`) or an unsorted list must fail HERE, not mid-loop
    /// after earlier members have already retired — exactly the half-applied
    /// state the preflight exists to prevent (atomicity, 8e299ea). `push`
    /// admits only validated entries and seals at the hard format ceiling, so
    /// after this pass the retirement loop has no reachable typed refusal
    /// left halfway through.
    fn preflight_retirements(&self, eids: &[EId], seq: CommitSeq) -> Result<(), WriteError> {
        let mut previous = None;
        for &eid in eids {
            if let Some(p) = previous {
                if p == eid {
                    return Err(WriteError::UnknownEdge { eid });
                }
                if p > eid {
                    return Err(WriteError::CascadeOrderViolation {
                        previous: p,
                        found: eid,
                    });
                }
            }
            self.retirement_entry(eid, seq)?;
            previous = Some(eid);
        }
        Ok(())
    }

    /// What retiring one edge stages: a tombstone, or nothing at all.
    ///
    /// The NOTHING case is exact: an edge created and deleted in the SAME
    /// commit has an empty visibility interval, which the durable format
    /// rightly refuses (`RetiredBeforeCreated`). Its fold is no entry — the
    /// edge is visible on no snapshot, so the pending creation and the live
    /// record both simply go away (fgdb-zeay). The fold applies only while
    /// the creation is still pending: once it has sealed, the interval can no
    /// longer be folded away, and the format's refusal stands as the honest
    /// answer to a pathological stream (16M+ rows between create and delete
    /// in one commit — a typed refusal, never wrong bytes).
    fn retirement_entry(
        &self,
        eid: EId,
        seq: CommitSeq,
    ) -> Result<Option<AdjacencyEntry>, WriteError> {
        let &(src, relation, dst, created_at) =
            self.live.get(&eid).ok_or(WriteError::UnknownEdge { eid })?;
        if created_at == seq {
            let is_same_run = self
                .pending
                .get(&(src, relation, dst))
                .is_some_and(|pending| pending.created_at == seq && pending.retired_at.is_none());
            if is_same_run {
                return Ok(None);
            }
        }
        let entry = AdjacencyEntry {
            src,
            relation,
            dst,
            created_at,
            retired_at: Some(seq),
        };
        validate_entry(0, &entry).map_err(WriteError::Block)?;
        Ok(Some(entry))
    }

    fn retire(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        eid: EId,
        seq: CommitSeq,
    ) -> Result<(), WriteError> {
        let Some(entry) = self.retirement_entry(eid, seq)? else {
            // The same-commit fold: created and deleted in one commit, so the
            // exact durable image is no entry at all.
            let &(src, relation, dst, _) =
                self.live.get(&eid).ok_or(WriteError::UnknownEdge { eid })?;
            let removed_pending = self.pending.remove(&(src, relation, dst));
            debug_assert!(
                removed_pending.is_some(),
                "the fold requires the pending creation it was proven on"
            );
            let removed_live = self.live.remove(&eid);
            debug_assert!(removed_live.is_some(), "preflighted live edge disappeared");
            return Ok(());
        };
        self.push(keys, entry)?;
        let removed = self.live.remove(&eid);
        debug_assert!(removed.is_some(), "preflighted live edge disappeared");
        Ok(())
    }

    /// Stage one entry, sealing first if its key is already pending.
    ///
    /// A pending entry for the same key that describes the SAME version is an
    /// update — the retirement of something created in this very run — and simply
    /// replaces it, because a block may carry the finished interval directly. A
    /// DIFFERENT version cannot share the block, so it forces a seal.
    fn push(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        entry: AdjacencyEntry,
    ) -> Result<(), WriteError> {
        validate_entry(0, &entry).map_err(WriteError::Block)?;
        let key = (entry.src, entry.relation, entry.dst);
        match self.pending.get(&key) {
            Some(existing) if existing.created_at == entry.created_at => {
                self.pending.insert(key, entry);
            }
            Some(_) => {
                self.seal(keys)?;
                self.pending.insert(key, entry);
            }
            None if self.pending.len()
                >= usize::try_from(MAX_BLOCK_ENTRIES).unwrap_or(usize::MAX) =>
            {
                // This is a format ceiling, not an adaptive seal policy: allowing
                // one more row would create a block no conforming reader accepts.
                self.seal(keys)?;
                self.pending.insert(key, entry);
            }
            None => {
                self.pending.insert(key, entry);
            }
        }
        Ok(())
    }

    /// Seal the pending entries into a block. A no-op when nothing is pending.
    pub fn seal(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
    ) -> Result<Option<SealedBlock>, WriteError> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        let entries: Vec<AdjacencyEntry> = self.pending.values().copied().collect();
        let bytes = encode_block(&entries).map_err(WriteError::Block)?;
        let (first_seq, last_seq) = span_of(&entries).expect("non-empty");
        let sealed = SealedBlock {
            block_id: block_id(keys.0, keys.1, &bytes),
            bytes,
            first_seq,
            last_seq,
        };
        self.pending.clear();
        self.sealed.push(sealed.clone());
        Ok(Some(sealed))
    }

    /// Seal whatever remains and publish a root over every block.
    ///
    /// `published_at` must be at or above the last sequence any block reaches; the
    /// root refuses otherwise, which is what stops a partition from claiming state
    /// it has not caught up to.
    pub fn publish(
        mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        published_at: CommitSeq,
    ) -> Result<(PartitionRoot, Vec<SealedBlock>), WriteError> {
        self.seal(keys)?;
        let root = PartitionRoot {
            graph: self.graph,
            branch: self.branch,
            partition: self.partition,
            published_at,
            blocks: self.sealed.iter().map(SealedBlock::reference).collect(),
        };
        validate_root(&root).map_err(WriteError::Root)?;
        Ok((root, self.sealed))
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockWriter, WriteError};
    use crate::{AdjacencyEntry, BlockError};
    use fgdb_delta_types::RelationId;
    use fgdb_types::ids::DatabaseSecurityNamespaceId;
    use fgdb_types::{BranchId, CommitSeq, GraphId, VId};

    const K_OID: [u8; 32] = [0x5a; 32];

    fn keys() -> (&'static [u8; 32], DatabaseSecurityNamespaceId) {
        (&K_OID, DatabaseSecurityNamespaceId([0x77; 32]))
    }

    #[test]
    fn a_failed_seal_preserves_its_pending_entries() {
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        writer.pending.insert(
            (VId(1), RelationId(1), VId(2)),
            AdjacencyEntry {
                src: VId(1),
                relation: RelationId(1),
                dst: VId(2),
                created_at: CommitSeq(0),
                retired_at: None,
            },
        );

        let expected = Err(WriteError::Block(BlockError::CreatedAtZero { at: 0 }));
        assert_eq!(writer.seal(keys()), expected);
        assert_eq!(writer.pending_len(), 1, "a failed seal retains its input");
        assert_eq!(
            writer.seal(keys()),
            expected,
            "retry observes the same deterministic refusal"
        );
    }
}
