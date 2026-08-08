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
//! names an `EId`; an adjacency entry needs `(src, relation, dst, eid)` and the sequence
//! the version began at. Only the creation carries those, so the writer remembers
//! them for edges it has seen live. This is bounded by the live edge count, it is
//! rebuildable by replaying the stream, and it is exactly the state a memtable
//! holds — not a derived structure that could become authoritative. Doctrine 5
//! stands: recovery discards and rebuilds it.
//!
//! **EID IS PART OF THE KEY.** Parallel edges may share `(src, relation, dst)`;
//! their stable EIds are the unconditional discriminator. A block therefore
//! orders by the full four-field key, and a retirement replaces only the pending
//! statement for that exact EId. The writer separately remembers every admitted
//! EId seen in this replay lane so retirement never makes an allocation slot
//! reusable here. Graph-wide allocator authority remains upstream of a
//! partition-local writer.

use crate::edge_props::{
    EdgePropertyPatchError, EdgePropertyRow, MAX_PROPERTY_PATCH_ROWS, encode_property_patch,
    property_patch_id,
};
use crate::root::{BlockRef, PartitionRoot, PatchRef, RootError, span_of, validate_root};
use crate::vertex::{VertexPatchError, VertexRow, encode_patch, span_of_rows, vertex_patch_id};
use crate::{
    AdjacencyEntry, BlockError, MAX_BLOCK_ENTRIES, block_id, encode_block,
    encode_block_with_properties, validate_entry,
};
use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_types::CanonicalScalar;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};
use std::collections::{BTreeMap, BTreeSet};

/// A sealed block: its identity, its bytes, the range it covers, and — when
/// any of its entries carry properties — the hosted edge property patch the
/// block's locator column references (fgdb-yqor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedBlock {
    pub block_id: ObjectId,
    pub bytes: Vec<u8>,
    pub first_seq: CommitSeq,
    pub last_seq: CommitSeq,
    /// The FGSP object this block's locators reference, if any. The caller
    /// must make it durable BEFORE the root that names the block, or root
    /// admission fails closed on the unreachable patch.
    pub property_patch: Option<SealedPropertyPatch>,
}

/// A sealed edge property patch riding beside its hosting block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedPropertyPatch {
    pub patch_id: ObjectId,
    pub bytes: Vec<u8>,
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

/// One staged adjacency statement: the entry and its properties.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingStatement {
    entry: AdjacencyEntry,
    props: EdgePropertyRow,
}

/// The live-map record of one edge (see the `live` field's doc).
#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveEdge {
    src: VId,
    relation: RelationId,
    dst: VId,
    created_at: CommitSeq,
    props: EdgePropertyRow,
}

/// One vertex content transition the fold applies (fgdb-stb6).
enum VertexContentUpdate {
    Label {
        label: LabelId,
        member: bool,
    },
    Property {
        key: PropertyKeyId,
        value: Option<CanonicalScalar>,
    },
}

/// A sealed vertex patch: its identity, its bytes, and the range it covers —
/// the vertex counterpart of [`SealedBlock`] (fgdb-3xoi).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedPatch {
    pub patch_id: ObjectId,
    pub bytes: Vec<u8>,
    pub first_seq: CommitSeq,
    pub last_seq: CommitSeq,
}

impl SealedPatch {
    /// The reference a root carries for this patch.
    pub fn reference(&self) -> PatchRef {
        PatchRef {
            patch_id: self.patch_id,
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
    /// Refused rather than overwritten. A re-CREATE of a live edge is the stream
    /// failing to be a stream; overwriting the live map would strand the first
    /// interval, retired by nothing and answering every future snapshot
    /// (fgdb-3usp). A re-create after retirement is separately refused as
    /// `EdgeIdentitySpent`.
    EdgeAlreadyLive { eid: EId },
    /// A create tried to reuse an EId admitted earlier in this stream.
    ///
    /// Retirement removes an edge from `live`, never from the graph allocator's
    /// spent set. Keeping this distinct from `EdgeAlreadyLive` tells the caller
    /// whether the conflicting identity is visible or historical.
    EdgeIdentitySpent { eid: EId },
    /// A row arrived at a sequence before the previous one.
    ///
    /// The writer is a fold over an ordered stream; out-of-order input would put
    /// entries in a block whose declared range no longer bounds them.
    SequenceNotAdvancing {
        previous: CommitSeq,
        offered: CommitSeq,
    },
    /// A `DeleteVertex` named a vertex this writer never saw created —
    /// the vertex counterpart of [`WriteError::UnknownEdge`], refused for the
    /// same replay-from-the-beginning reason.
    UnknownVertex { vid: VId },
    /// A `CreateVertex` named a vertex this writer already holds live.
    ///
    /// Refused for the reason `EdgeAlreadyLive` is: a re-create of a live
    /// identity is the stream failing to be a stream, and overwriting the
    /// staged row would strand the first version.
    VertexAlreadyLive { vid: VId },
    /// A create tried to reuse a VId admitted earlier in this stream.
    VertexIdentitySpent { vid: VId },
    /// Sealing produced bytes the vertex patch encoder refused, or a row's
    /// shape violated the patch format's canonical laws at fold time.
    Patch(VertexPatchError),
    /// Sealing produced bytes the edge property patch encoder refused.
    EdgeProps(EdgePropertyPatchError),
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
            Self::EdgeIdentitySpent { eid } => {
                write!(f, "edge identity {eid:?} is permanently spent")
            }
            Self::SequenceNotAdvancing { previous, offered } => write!(
                f,
                "rows must arrive in commit order; {offered:?} follows {previous:?}"
            ),
            Self::UnknownVertex { vid } => {
                write!(f, "no live version of {vid:?} to retire")
            }
            Self::VertexAlreadyLive { vid } => {
                write!(f, "{vid:?} is already live; a re-create is not a version")
            }
            Self::VertexIdentitySpent { vid } => {
                write!(f, "vertex identity {vid:?} is permanently spent")
            }
            Self::Patch(error) => write!(f, "vertex sealing: {error}"),
            Self::EdgeProps(error) => write!(f, "edge property sealing: {error}"),
            Self::Block(error) => write!(f, "sealing: {error}"),
            Self::Root(error) => write!(f, "publishing: {error}"),
        }
    }
}

impl core::error::Error for WriteError {}

/// Folds delta rows into sealed blocks for one partition.
///
/// `Clone` is load-bearing, not convenience: `publish` consumes the writer,
/// so a caller that keeps folding across publications — the incremental
/// snapshot path that removes the O(history) per-commit rebuild
/// (`fgdb-fujt`) — publishes from a clone and retains the original. The fold
/// is a deterministic function of the row sequence, so a clone-publish at
/// sequence k is byte-identical to a fresh writer replaying rows 1..k and
/// publishing; `tests/incremental_publish_equals_rebuild.rs` pins exactly
/// that equality, per shape, with a control that can fail.
#[derive(Debug, Clone)]
pub struct BlockWriter {
    graph: GraphId,
    branch: BranchId,
    partition: u64,
    /// Entries not yet sealed, keyed by the complete stable adjacency
    /// identity, each with the properties its statement carries.
    pending: BTreeMap<(VId, RelationId, VId, EId), PendingStatement>,
    /// The full statement of every edge currently live — endpoints, creation,
    /// AND properties, because a tombstone must RESTATE the properties so a
    /// pre-retirement snapshot keeps answering them (fgdb-yqor), exactly as
    /// the vertex live map remembers rows for the same reason.
    live: BTreeMap<EId, LiveEdge>,
    /// Every EId admitted while replaying this partition-local writer.
    ///
    /// The graph-wide allocator is enforced before partition routing; this is a
    /// defense-in-depth check over the history visible to this fold.
    spent: BTreeSet<EId>,
    sealed: Vec<SealedBlock>,
    /// Vertex rows not yet sealed into a patch, keyed by STATEMENT identity
    /// `(vid, created_at)` — the vertex half of the fold (fgdb-3xoi), keyed
    /// per version since FGSV V2 chains statements (fgdb-stb6). The same
    /// memtable argument as `live`: bounded, rebuildable by replay, never
    /// authoritative. BTreeMap order IS the patch's canonical row order.
    pending_vertices: BTreeMap<(VId, CommitSeq), VertexRow>,
    /// The full row of every VId currently live in this fold.
    ///
    /// A map and not a set for the reason `live` carries edge births: a
    /// `DeleteVertex` names only the VId, while the tombstone that retires a
    /// SEALED row must restate the exact birth — ordinal, labels, properties —
    /// and only the creation carried those.
    live_vertices: BTreeMap<VId, VertexRow>,
    /// Every VId admitted while replaying this partition-local writer.
    spent_vertices: BTreeSet<VId>,
    sealed_patches: Vec<SealedPatch>,
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
            spent: BTreeSet::new(),
            sealed: Vec::new(),
            pending_vertices: BTreeMap::new(),
            live_vertices: BTreeMap::new(),
            spent_vertices: BTreeSet::new(),
            sealed_patches: Vec::new(),
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

    pub fn sealed_patches(&self) -> &[SealedPatch] {
        &self.sealed_patches
    }

    /// The endpoints and creation of one live edge, or `None` when the fold
    /// holds no live version of it.
    ///
    /// The read face of the writer's live map (fgdb-p3ok): the spine derives
    /// delete before-images from CURRENT state, and the fold's live map IS
    /// that state — rebuilt by replay, never authoritative, and already
    /// maintained for retirement. A second fold of the same stream beside
    /// this one would be two opinions about one fact.
    pub fn live_edge(&self, eid: EId) -> Option<(VId, RelationId, VId, CommitSeq)> {
        self.live
            .get(&eid)
            .map(|edge| (edge.src, edge.relation, edge.dst, edge.created_at))
    }

    /// Is `vid` live in this fold?
    pub fn is_vertex_live(&self, vid: VId) -> bool {
        self.live_vertices.contains_key(&vid)
    }

    /// Every live edge touching `vid`, in canonical ascending-EId order —
    /// the exact set a vertex deletion's cascade before-image must equal
    /// (both directions, the reference semantics).
    pub fn live_incident_edges(&self, vid: VId) -> Vec<EId> {
        self.live
            .iter()
            .filter(|(_, edge)| edge.src == vid || edge.dst == vid)
            .map(|(eid, _)| *eid)
            .collect()
    }

    /// How many vertex rows are pending — the vertex half of the seal signal.
    pub fn pending_vertex_len(&self) -> usize {
        self.pending_vertices.len()
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
                props,
                ..
            } => {
                if self.live.contains_key(eid) {
                    return Err(WriteError::EdgeAlreadyLive { eid: *eid });
                }
                if self.spent.contains(eid) {
                    return Err(WriteError::EdgeIdentitySpent { eid: *eid });
                }
                self.push(
                    keys,
                    AdjacencyEntry {
                        src: *src,
                        relation: *relation,
                        dst: *dst,
                        eid: *eid,
                        created_at: seq,
                        retired_at: None,
                    },
                    props.clone(),
                )?;
                self.live.insert(
                    *eid,
                    LiveEdge {
                        src: *src,
                        relation: *relation,
                        dst: *dst,
                        created_at: seq,
                        props: props.clone(),
                    },
                );
                let was_fresh = self.spent.insert(*eid);
                debug_assert!(was_fresh, "spent-set admission was checked above");
            }
            DeltaRow::DeleteEdge { eid, .. } => {
                self.retire(keys, *eid, seq)?;
            }
            DeltaRow::DeleteVertex {
                vid,
                sorted_retired_incident_edges,
                ..
            } => {
                // THE VERTEX MUST BE LIVE, proven before any cascade member
                // retires — the same atomicity argument as
                // `preflight_retirements`: a refusal after the first edge
                // retired would leave the writer half-applied. A delete this
                // fold cannot resolve means the stream is not being replayed
                // from the beginning, exactly as for `UnknownEdge`.
                let row = self
                    .live_vertices
                    .get(vid)
                    .cloned()
                    .ok_or(WriteError::UnknownVertex { vid: *vid })?;
                // THE CASCADE IS TAKEN FROM THE ROW, not recomputed from the live
                // map. The row's image is checked against materialized state by the
                // materializer that produced it; recomputing here would be a second
                // opinion about which edges a deletion retires, and two opinions
                // about one fact is how they drift.
                self.preflight_retirements(sorted_retired_incident_edges, seq)?;
                // The vertex's own retirement stages exactly like an edge's:
                // created-and-deleted in one commit folds to NO row while the
                // creation is still pending; anything else restates the exact
                // birth with the interval closed. Proven lawful here, before
                // the first edge retires.
                let same_commit_fold = row.created_at == seq
                    && self
                        .pending_vertices
                        .get(&(*vid, seq))
                        .is_some_and(|pending| pending.retired_at.is_none());
                let tombstone = if same_commit_fold {
                    None
                } else {
                    let mut retired = row;
                    retired.retired_at = Some(seq);
                    crate::vertex::validate_patch_row(0, &retired).map_err(WriteError::Patch)?;
                    Some(retired)
                };
                for eid in sorted_retired_incident_edges {
                    self.retire(keys, *eid, seq)?;
                }
                match tombstone {
                    None => {
                        let removed = self.pending_vertices.remove(&(*vid, seq));
                        debug_assert!(
                            removed.is_some(),
                            "the fold requires the pending creation it was proven on"
                        );
                    }
                    Some(retired) => {
                        let key = (*vid, retired.created_at);
                        if !self.pending_vertices.contains_key(&key)
                            && self.pending_vertices.len()
                                >= usize::try_from(crate::vertex::MAX_PATCH_ROWS)
                                    .unwrap_or(usize::MAX)
                        {
                            self.seal_vertices(keys)?;
                        }
                        self.pending_vertices.insert(key, retired);
                    }
                }
                let removed = self.live_vertices.remove(vid);
                debug_assert!(removed.is_some(), "preflighted live vertex disappeared");
            }
            DeltaRow::CreateVertex {
                vid,
                birth_ordinal,
                labels,
                props,
                ..
            } => {
                if self.live_vertices.contains_key(vid) {
                    return Err(WriteError::VertexAlreadyLive { vid: *vid });
                }
                if self.spent_vertices.contains(vid) {
                    return Err(WriteError::VertexIdentitySpent { vid: *vid });
                }
                let row = VertexRow {
                    vid: *vid,
                    birth_ordinal: *birth_ordinal,
                    created_at: seq,
                    retired_at: None,
                    labels: labels.clone(),
                    props: props.clone(),
                };
                // The capsule's canonical template encoding already refuses
                // unsorted labels (NonCanonicalLabelOrder), so a violation here
                // means the rows did not come from a canonical stream — a typed
                // refusal, before any state changes.
                crate::vertex::validate_patch_row(0, &row).map_err(WriteError::Patch)?;
                if self.pending_vertices.len()
                    >= usize::try_from(crate::vertex::MAX_PATCH_ROWS).unwrap_or(usize::MAX)
                {
                    // A format ceiling, exactly like the block path: one more
                    // row would create a patch no conforming reader accepts.
                    self.seal_vertices(keys)?;
                }
                self.pending_vertices.insert((*vid, seq), row.clone());
                self.live_vertices.insert(*vid, row);
                let was_fresh = self.spent_vertices.insert(*vid);
                debug_assert!(was_fresh, "spent-set admission was checked above");
            }
            DeltaRow::LabelMembership {
                vid, label, after, ..
            } => {
                self.fold_vertex_update(
                    keys,
                    *vid,
                    seq,
                    VertexContentUpdate::Label {
                        label: *label,
                        member: *after,
                    },
                )?;
            }
            DeltaRow::Property {
                elem,
                property,
                after,
                ..
            } => match elem {
                ElementId::Vertex(vid) => {
                    self.fold_vertex_update(
                        keys,
                        *vid,
                        seq,
                        VertexContentUpdate::Property {
                            key: *property,
                            value: after.clone(),
                        },
                    )?;
                }
                ElementId::Edge(eid) => {
                    // Edge properties have no tier-D storage yet — the
                    // block-hosted patch refs are `fgdb-w3-properties-gou`'s.
                    // The row stays durable in the stream, and LIVENESS is
                    // still enforced here so an unlawful stream refuses
                    // instead of silently versioning a ghost.
                    if !self.live.contains_key(eid) {
                        return Err(WriteError::UnknownEdge { eid: *eid });
                    }
                }
            },
            // Every other family is real and none of it belongs to the two
            // structures this tier holds (adjacency blocks and vertex row
            // patches). Valid time, counters, escrow, sketches, schema and
            // constraints belong to structures that do not exist yet, and
            // silently folding them would be worse than not storing them.
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
    ) -> Result<Option<(AdjacencyEntry, EdgePropertyRow)>, WriteError> {
        let edge = self.live.get(&eid).ok_or(WriteError::UnknownEdge { eid })?;
        if edge.created_at == seq {
            let is_same_run = self
                .pending
                .get(&(edge.src, edge.relation, edge.dst, eid))
                .is_some_and(|pending| {
                    pending.entry.created_at == seq && pending.entry.retired_at.is_none()
                });
            if is_same_run {
                return Ok(None);
            }
        }
        let entry = AdjacencyEntry {
            src: edge.src,
            relation: edge.relation,
            dst: edge.dst,
            eid,
            created_at: edge.created_at,
            retired_at: Some(seq),
        };
        validate_entry(0, &entry).map_err(WriteError::Block)?;
        // THE TOMBSTONE RESTATES THE PROPERTIES: supersede replaces the whole
        // statement, so a tombstone without them would erase the edge's
        // properties from every pre-retirement snapshot (fgdb-yqor).
        Ok(Some((entry, edge.props.clone())))
    }

    fn retire(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        eid: EId,
        seq: CommitSeq,
    ) -> Result<(), WriteError> {
        let Some((entry, props)) = self.retirement_entry(eid, seq)? else {
            // The same-commit fold: created and deleted in one commit, so the
            // exact durable image is no entry at all.
            let edge = self.live.get(&eid).ok_or(WriteError::UnknownEdge { eid })?;
            let key = (edge.src, edge.relation, edge.dst, eid);
            let removed_pending = self.pending.remove(&key);
            debug_assert!(
                removed_pending.is_some(),
                "the fold requires the pending creation it was proven on"
            );
            let removed_live = self.live.remove(&eid);
            debug_assert!(removed_live.is_some(), "preflighted live edge disappeared");
            return Ok(());
        };
        self.push(keys, entry, props)?;
        let removed = self.live.remove(&eid);
        debug_assert!(removed.is_some(), "preflighted live edge disappeared");
        Ok(())
    }

    /// Stage one entry under its full stable edge key.
    ///
    /// A pending entry for the same key that describes the SAME version is an
    /// update — the retirement of something created in this very run — and simply
    /// replaces it, because a block may carry the finished interval directly. A
    /// DIFFERENT statement for that EId cannot share the block, so it forces a seal.
    fn push(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        entry: AdjacencyEntry,
        props: EdgePropertyRow,
    ) -> Result<(), WriteError> {
        validate_entry(0, &entry).map_err(WriteError::Block)?;
        let key = (entry.src, entry.relation, entry.dst, entry.eid);
        let statement = PendingStatement { entry, props };
        match self.pending.get(&key) {
            Some(existing) if existing.entry.created_at == entry.created_at => {
                self.pending.insert(key, statement);
            }
            Some(_) => {
                self.seal(keys)?;
                self.pending.insert(key, statement);
            }
            None if self.pending.len()
                >= usize::try_from(MAX_BLOCK_ENTRIES).unwrap_or(usize::MAX) =>
            {
                // This is a format ceiling, not an adaptive seal policy: allowing
                // one more row would create a block no conforming reader accepts.
                self.seal(keys)?;
                self.pending.insert(key, statement);
            }
            None => {
                self.pending.insert(key, statement);
            }
        }
        Ok(())
    }

    /// Seal pending rows into their descriptor-local V3 blocks.  The writer is
    /// partition-local, while §6.2 blocks are descriptor-local, so this is the
    /// boundary where one replay fold fans into immutable blocks; no descriptor
    /// field may be smuggled back into an entry to avoid that split.
    pub fn seal(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
    ) -> Result<Option<SealedBlock>, WriteError> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        let mut by_descriptor: BTreeMap<(VId, RelationId), Vec<PendingStatement>> = BTreeMap::new();
        for statement in self.pending.values().cloned() {
            by_descriptor
                .entry((statement.entry.src, statement.entry.relation))
                .or_default()
                .push(statement);
        }
        let mut sealed = Vec::with_capacity(by_descriptor.len());
        for statements in by_descriptor.into_values() {
            // The locator addresses at most MAX_PROPERTY_PATCH_ROWS propertied
            // entries per block — a FORMAT ceiling, so a descriptor run whose
            // propertied count exceeds it splits into consecutive blocks,
            // exactly as the entry-count ceiling splits a pending run.
            let ceiling = usize::try_from(MAX_PROPERTY_PATCH_ROWS).unwrap_or(usize::MAX);
            let mut chunk: Vec<PendingStatement> = Vec::new();
            let mut chunk_propertied = 0usize;
            let mut chunks: Vec<Vec<PendingStatement>> = Vec::new();
            for statement in statements {
                let propertied = usize::from(!statement.props.is_empty());
                if chunk_propertied + propertied > ceiling {
                    chunks.push(std::mem::take(&mut chunk));
                    chunk_propertied = 0;
                }
                chunk_propertied += propertied;
                chunk.push(statement);
            }
            if !chunk.is_empty() {
                chunks.push(chunk);
            }
            for statements in chunks {
                let entries: Vec<AdjacencyEntry> =
                    statements.iter().map(|statement| statement.entry).collect();
                let mut locators = Vec::with_capacity(statements.len());
                let mut rows: Vec<EdgePropertyRow> = Vec::new();
                for statement in &statements {
                    if statement.props.is_empty() {
                        locators.push(0u8);
                    } else {
                        rows.push(statement.props.clone());
                        locators.push(u8::try_from(rows.len()).expect("chunked to the ceiling"));
                    }
                }
                let (bytes, property_patch) = if rows.is_empty() {
                    (encode_block(&entries).map_err(WriteError::Block)?, None)
                } else {
                    let patch_bytes =
                        encode_property_patch(&rows).map_err(WriteError::EdgeProps)?;
                    let patch_id = property_patch_id(keys.0, keys.1, &patch_bytes);
                    let bytes = encode_block_with_properties(&entries, patch_id, &locators)
                        .map_err(WriteError::Block)?;
                    (
                        bytes,
                        Some(SealedPropertyPatch {
                            patch_id,
                            bytes: patch_bytes,
                        }),
                    )
                };
                let (first_seq, last_seq) = span_of(&entries).expect("non-empty");
                sealed.push(SealedBlock {
                    block_id: block_id(keys.0, keys.1, &bytes),
                    bytes,
                    first_seq,
                    last_seq,
                    property_patch,
                });
            }
        }
        // Roots publish in nondecreasing frontier order even when descriptor
        // keys sort differently from the commit stream that populated them.
        sealed.sort_by_key(|block| (block.last_seq, block.first_seq, block.block_id));
        let first = sealed.first().cloned();
        self.pending.clear();
        self.sealed.extend(sealed);
        Ok(first)
    }

    /// Fold one vertex content transition (fgdb-stb6): the live statement
    /// retires at `seq` and its successor — identical birth, updated content
    /// — begins there, the FGSV V2 chain shape. A transition inside the
    /// statement's own creation commit folds IN PLACE: the intermediate
    /// content never existed on any snapshot, exactly like the same-commit
    /// delete fold. The writer APPLIES the after-image and does not verify
    /// the before-image, for the cascade's reason: the row's image is checked
    /// against materialized state by the materializer that produced it.
    fn fold_vertex_update(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        vid: VId,
        seq: CommitSeq,
        update: VertexContentUpdate,
    ) -> Result<(), WriteError> {
        let current = self
            .live_vertices
            .get(&vid)
            .cloned()
            .ok_or(WriteError::UnknownVertex { vid })?;
        let mut successor = current.clone();
        match update {
            VertexContentUpdate::Label { label, member } => {
                match successor.labels.binary_search(&label) {
                    Ok(at) => {
                        if !member {
                            successor.labels.remove(at);
                        }
                    }
                    Err(at) => {
                        if member {
                            successor.labels.insert(at, label);
                        }
                    }
                }
            }
            VertexContentUpdate::Property { key, value } => {
                match successor.props.binary_search_by_key(&key, |(k, _)| *k) {
                    Ok(at) => match value {
                        Some(value) => successor.props[at].1 = value,
                        None => {
                            successor.props.remove(at);
                        }
                    },
                    Err(at) => {
                        if let Some(value) = value {
                            successor.props.insert(at, (key, value));
                        }
                    }
                }
            }
        }
        if current.created_at == seq
            && self
                .pending_vertices
                .get(&(vid, seq))
                .is_some_and(|pending| pending.retired_at.is_none())
        {
            self.pending_vertices.insert((vid, seq), successor.clone());
            self.live_vertices.insert(vid, successor);
            return Ok(());
        }
        let mut tombstone = current;
        tombstone.retired_at = Some(seq);
        // Sealed-in-this-commit creations reach here and refuse as
        // RetiredBeforeCreated (an empty interval), the same honest format
        // answer the edge path gives a pathological stream.
        crate::vertex::validate_patch_row(0, &tombstone).map_err(WriteError::Patch)?;
        successor.created_at = seq;
        successor.retired_at = None;
        crate::vertex::validate_patch_row(0, &successor).map_err(WriteError::Patch)?;
        let tombstone_key = (vid, tombstone.created_at);
        let successor_key = (vid, seq);
        let incoming = usize::from(!self.pending_vertices.contains_key(&tombstone_key))
            + usize::from(!self.pending_vertices.contains_key(&successor_key));
        if self.pending_vertices.len() + incoming
            > usize::try_from(crate::vertex::MAX_PATCH_ROWS).unwrap_or(usize::MAX)
        {
            self.seal_vertices(keys)?;
        }
        self.pending_vertices.insert(tombstone_key, tombstone);
        self.pending_vertices
            .insert(successor_key, successor.clone());
        self.live_vertices.insert(vid, successor);
        Ok(())
    }

    /// Seal pending vertex rows into one canonical patch.
    ///
    /// The vertex counterpart of [`BlockWriter::seal`]: partition-local rows
    /// leave the mutable staging map as one immutable, content-addressed
    /// object. A failed seal leaves the staged rows exactly as they were.
    pub fn seal_vertices(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
    ) -> Result<Option<SealedPatch>, WriteError> {
        if self.pending_vertices.is_empty() {
            return Ok(None);
        }
        let rows: Vec<VertexRow> = self.pending_vertices.values().cloned().collect();
        let bytes = encode_patch(&rows).map_err(WriteError::Patch)?;
        let (first_seq, last_seq) = span_of_rows(&rows).expect("non-empty");
        let sealed = SealedPatch {
            patch_id: vertex_patch_id(keys.0, keys.1, &bytes),
            bytes,
            first_seq,
            last_seq,
        };
        self.pending_vertices.clear();
        self.sealed_patches.push(sealed.clone());
        Ok(Some(sealed))
    }

    /// Seal whatever remains and publish a root over every block and patch.
    ///
    /// `published_at` must be at or above the last sequence any block or patch
    /// reaches; the root refuses otherwise, which is what stops a partition
    /// from claiming state it has not caught up to.
    pub fn publish(
        mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        published_at: CommitSeq,
    ) -> Result<(PartitionRoot, Vec<SealedBlock>, Vec<SealedPatch>), WriteError> {
        self.seal(keys)?;
        self.seal_vertices(keys)?;
        let root = PartitionRoot {
            graph: self.graph,
            branch: self.branch,
            partition: self.partition,
            published_at,
            blocks: self.sealed.iter().map(SealedBlock::reference).collect(),
            vertex_patches: self
                .sealed_patches
                .iter()
                .map(SealedPatch::reference)
                .collect(),
        };
        validate_root(&root).map_err(WriteError::Root)?;
        Ok((root, self.sealed, self.sealed_patches))
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockWriter, PendingStatement, WriteError};
    use crate::{AdjacencyEntry, BlockError};
    use fgdb_delta_types::RelationId;
    use fgdb_types::ids::DatabaseSecurityNamespaceId;
    use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};

    const K_OID: [u8; 32] = [0x5a; 32];

    fn keys() -> (&'static [u8; 32], DatabaseSecurityNamespaceId) {
        (&K_OID, DatabaseSecurityNamespaceId([0x77; 32]))
    }

    #[test]
    fn a_failed_seal_preserves_its_pending_entries() {
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        writer.pending.insert(
            (VId(1), RelationId(1), VId(2), EId(10)),
            PendingStatement {
                entry: AdjacencyEntry {
                    src: VId(1),
                    relation: RelationId(1),
                    dst: VId(2),
                    eid: EId(10),
                    created_at: CommitSeq(0),
                    retired_at: None,
                },
                props: vec![],
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
