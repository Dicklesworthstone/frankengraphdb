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

use crate::root::{BlockRef, PartitionRoot, span_of};
use crate::{AdjacencyEntry, BlockError, block_id, encode_block};
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

/// Why the writer could not fold a row or seal a block.
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
    /// A row arrived at a sequence at or before the previous one.
    ///
    /// The writer is a fold over an ordered stream; out-of-order input would put
    /// entries in a block whose declared range no longer bounds them.
    SequenceNotAdvancing {
        previous: CommitSeq,
        offered: CommitSeq,
    },
    /// Sealing produced bytes the block encoder refused.
    Block(BlockError),
}

impl core::fmt::Display for WriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownEdge { eid } => {
                write!(f, "no live version of {eid:?} to retire")
            }
            Self::SequenceNotAdvancing { previous, offered } => write!(
                f,
                "rows must arrive in commit order; {offered:?} follows {previous:?}"
            ),
            Self::Block(error) => write!(f, "sealing: {error}"),
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
        self.last_seq = Some(seq);

        match row {
            DeltaRow::CreateEdge {
                eid,
                src,
                relation,
                dst,
                ..
            } => {
                self.live.insert(*eid, (*src, *relation, *dst, seq));
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
        Ok(())
    }

    fn retire(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        eid: EId,
        seq: CommitSeq,
    ) -> Result<(), WriteError> {
        let (src, relation, dst, created_at) = self
            .live
            .remove(&eid)
            .ok_or(WriteError::UnknownEdge { eid })?;
        self.push(
            keys,
            AdjacencyEntry {
                src,
                relation,
                dst,
                created_at,
                retired_at: Some(seq),
            },
        )
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
        let key = (entry.src, entry.relation, entry.dst);
        match self.pending.get(&key) {
            Some(existing) if existing.created_at == entry.created_at => {
                self.pending.insert(key, entry);
            }
            Some(_) => {
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
        self.pending.clear();
        let bytes = encode_block(&entries).map_err(WriteError::Block)?;
        let (first_seq, last_seq) = span_of(&entries).expect("non-empty");
        let sealed = SealedBlock {
            block_id: block_id(keys.0, keys.1, &bytes),
            bytes,
            first_seq,
            last_seq,
        };
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
        Ok((root, self.sealed))
    }
}
