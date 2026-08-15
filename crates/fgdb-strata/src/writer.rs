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
    AdjacencyEntry, BlockError, DeltaBlockVersion, MAX_BLOCK_ENTRIES, block_id, encode_block,
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

type EncodedEdgeSeal = (
    Vec<SealedBlock>,
    BTreeMap<(VId, RelationId), DeltaBlockVersion>,
);

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
    /// A `DeleteVertex` cascade is not exactly the live incident set.
    ///
    /// Too few would leave a dangling edge after the vertex is gone; too
    /// many claims a retirement that never happened. The fold verifies
    /// equality, then retires the declared list — it does not invent a
    /// different cascade (fgdb-17ht). The oracle's same law is
    /// `ApplyError::CascadeImageMismatch`.
    CascadeImageMismatch {
        vid: VId,
        declared: Vec<EId>,
        actual: Vec<EId>,
    },
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
    /// A `CreateEdge` named an endpoint this fold does not hold live.
    ///
    /// Refused rather than staged: a partition with an edge to a vertex that
    /// does not exist is not a graph, and the oracle apply of the same row
    /// is `ApplyError::DanglingEndpoint`. Recovery folds the stream through
    /// this writer (fgdb-7g91).
    DanglingEndpoint { eid: EId, endpoint: VId },
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
            Self::CascadeImageMismatch { vid, .. } => write!(
                f,
                "deletion of {vid:?} declares a cascade image that is not its incident edge set"
            ),
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
            Self::DanglingEndpoint { eid, endpoint } => {
                write!(f, "{eid:?} names endpoint {endpoint:?}, which is not live")
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
    pending: BTreeMap<(VId, RelationId, VId, EId, CommitSeq), PendingStatement>,
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
    /// The newest sealed block per descriptor family — the predecessor each
    /// family's NEXT block links (V6, fgdb-4391). Rebuilt deterministically by
    /// the same replay fold that rebuilds everything else here; never
    /// authoritative — the durable chain is in the block headers.
    chain_heads: BTreeMap<(VId, RelationId), DeltaBlockVersion>,
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
    /// Live identities whose same-seq creation has already been sealed in
    /// this run. A later same-seq delete cannot fold those away — the
    /// durable image already exists, and an empty interval is illegal.
    sealed_live_edges: BTreeSet<(EId, CommitSeq)>,
    sealed_live_vertices: BTreeSet<(VId, CommitSeq)>,
    last_seq: Option<CommitSeq>,
}

impl BlockWriter {
    /// Rebuild a writer's fold state from an ADMITTED published partition
    /// (fgdb-ge6a fast open). Every input is the store's own admission
    /// output — decoded blocks, hosted columns, vertex patches — so this
    /// DERIVES the maps a from-scratch stream fold would have built rather
    /// than trusting a caller's claim about them, and the fast-open
    /// equivalence law pins the derived writer's publish byte-identical to
    /// the rebuilt one's.
    #[allow(clippy::too_many_arguments)]
    pub fn from_published_partition(
        graph: GraphId,
        branch: BranchId,
        partition: u64,
        sealed: Vec<SealedBlock>,
        sealed_patches: Vec<SealedPatch>,
        blocks: &[Vec<AdjacencyEntry>],
        block_props: &[Option<crate::edge_props::BlockProps>],
        patches: &[Vec<VertexRow>],
        frontier: CommitSeq,
    ) -> Result<Self, RootError> {
        let mut live = BTreeMap::new();
        for (entry, props) in
            crate::root::merge_all_edges_with_props(blocks, block_props, frontier)?
        {
            live.insert(
                entry.eid,
                LiveEdge {
                    src: entry.src,
                    relation: entry.relation,
                    dst: entry.dst,
                    created_at: entry.created_at,
                    props,
                },
            );
        }
        let mut spent = BTreeSet::new();
        for block in blocks {
            for entry in block {
                spent.insert(entry.eid);
            }
        }
        let mut live_vertices = BTreeMap::new();
        for row in crate::vertex::merge_all_vertices(patches, frontier) {
            live_vertices.insert(row.vid, row);
        }
        let mut spent_vertices = BTreeSet::new();
        for rows in patches {
            for row in rows {
                spent_vertices.insert(row.vid);
            }
        }
        // The chain head per family is the LAST published block of that
        // family — publication order IS the chain (fgdb-4391).
        let mut chain_heads = BTreeMap::new();
        for (sealed_block, entries) in sealed.iter().zip(blocks) {
            if let Some(first) = entries.first() {
                chain_heads.insert(
                    (first.src, first.relation),
                    DeltaBlockVersion(sealed_block.block_id),
                );
            }
        }
        Ok(Self {
            graph,
            branch,
            partition,
            pending: BTreeMap::new(),
            live,
            spent,
            sealed,
            chain_heads,
            pending_vertices: BTreeMap::new(),
            live_vertices,
            spent_vertices,
            sealed_patches,
            sealed_live_edges: BTreeSet::new(),
            sealed_live_vertices: BTreeSet::new(),
            last_seq: (frontier.0 > 0).then_some(frontier),
        })
    }

    pub fn new(graph: GraphId, branch: BranchId, partition: u64) -> Self {
        Self {
            graph,
            branch,
            partition,
            pending: BTreeMap::new(),
            live: BTreeMap::new(),
            spent: BTreeSet::new(),
            sealed: Vec::new(),
            chain_heads: BTreeMap::new(),
            pending_vertices: BTreeMap::new(),
            live_vertices: BTreeMap::new(),
            spent_vertices: BTreeSet::new(),
            sealed_patches: Vec::new(),
            sealed_live_edges: BTreeSet::new(),
            sealed_live_vertices: BTreeSet::new(),
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

    /// The property row of the LIVE statement of `eid`, when one exists —
    /// the before-image source for an update's derivation (fgdb-ls5b).
    pub fn live_edge_row(&self, eid: EId) -> Option<EdgePropertyRow> {
        self.live.get(&eid).map(|edge| edge.props.clone())
    }

    /// Was `eid` ever admitted in this fold's history? Identities are
    /// permanently spent, and a caller composing a batch must be able to ask
    /// BEFORE committing bytes whose fold would refuse (fgdb-kokz).
    pub fn is_edge_spent(&self, eid: EId) -> bool {
        self.spent.contains(&eid)
    }

    /// The vertex counterpart of [`BlockWriter::is_edge_spent`].
    pub fn is_vertex_spent(&self, vid: VId) -> bool {
        self.spent_vertices.contains(&vid)
    }

    /// Is `vid` live in this fold?
    pub fn is_vertex_live(&self, vid: VId) -> bool {
        self.live_vertices.contains_key(&vid)
    }

    /// The full LIVE statement of `vid` — the row the statement-chain
    /// version transcript encodes (fgdb-ge6a v3).
    pub fn live_vertex_row(&self, vid: VId) -> Option<VertexRow> {
        self.live_vertices.get(&vid).cloned()
    }

    /// The full LIVE statement of `eid` — topology, birth of the current
    /// content version, and its row (fgdb-ge6a v3).
    pub fn live_edge_statement(
        &self,
        eid: EId,
    ) -> Option<(VId, RelationId, VId, CommitSeq, EdgePropertyRow)> {
        self.live.get(&eid).map(|edge| {
            (
                edge.src,
                edge.relation,
                edge.dst,
                edge.created_at,
                edge.props.clone(),
            )
        })
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

    fn pending_has_live_at(&self, seq: CommitSeq) -> bool {
        self.pending
            .values()
            .any(|pending| pending.entry.created_at == seq && pending.entry.retired_at.is_none())
    }

    fn pending_vertices_have_live_at(&self, seq: CommitSeq) -> bool {
        self.pending_vertices
            .values()
            .any(|row| row.created_at == seq && row.retired_at.is_none())
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
                let entry = AdjacencyEntry {
                    src: *src,
                    relation: *relation,
                    dst: *dst,
                    eid: *eid,
                    created_at: seq,
                    retired_at: None,
                };
                // Format first so a seq-0 row stays CreatedAtZero even when
                // the endpoints are also missing.
                validate_entry(0, &entry).map_err(WriteError::Block)?;
                for endpoint in [*src, *dst] {
                    if !self.live_vertices.contains_key(&endpoint) {
                        return Err(WriteError::DanglingEndpoint {
                            eid: *eid,
                            endpoint,
                        });
                    }
                }
                self.push(keys, entry, props.clone())?;
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
                // Verify the declared image against the live incident set,
                // then retire THE DECLARED LIST. An undercount would leave
                // a dangling edge; an overcount claims a retirement that
                // never happened. Equality is the oracle's law
                // (`CascadeImageMismatch`); we do not invent a different
                // cascade to apply.
                let actual = self.live_incident_edges(*vid);
                if sorted_retired_incident_edges != &actual {
                    return Err(WriteError::CascadeImageMismatch {
                        vid: *vid,
                        declared: sorted_retired_incident_edges.clone(),
                        actual,
                    });
                }
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
                        .is_some_and(|pending| pending.retired_at.is_none())
                    && !self.sealed_live_vertices.contains(&(*vid, seq));
                let tombstone = if same_commit_fold {
                    None
                } else {
                    let mut retired = row;
                    retired.retired_at = Some(seq);
                    crate::vertex::validate_patch_row(0, &retired).map_err(WriteError::Patch)?;
                    Some(retired)
                };
                // Seal an oversized vertex pending set BEFORE the cascade
                // retires anything. Doing it after — the old order — left
                // edges gone and the vertex live when `seal_vertices`
                // refused (the same half-applied shape preflight exists
                // to prevent on the edge list).
                let need_vertex_seal = tombstone.as_ref().is_some_and(|retired| {
                    let key = (*vid, retired.created_at);
                    !self.pending_vertices.contains_key(&key)
                        && self.pending_vertices.len()
                            >= usize::try_from(crate::vertex::MAX_PATCH_ROWS).unwrap_or(usize::MAX)
                        && !self.pending_vertices_have_live_at(seq)
                });
                // Same law on the edge half: if the cascade's tombstones
                // would trip the entry ceiling on a *later* `push`, seal
                // first. Otherwise the first `retire` stages, the next
                // early-seal refuses, and apply returns with a partial
                // cascade — the shape preflight is written to forbid.
                let mut incoming = 0usize;
                for eid in sorted_retired_incident_edges {
                    if let Some((entry, _)) = self.retirement_entry(*eid, seq)? {
                        let key = (entry.src, entry.relation, entry.dst, *eid, entry.created_at);
                        incoming += usize::from(!self.pending.contains_key(&key));
                    }
                }
                let need_edge_seal = self.pending.len() + incoming
                    > usize::try_from(MAX_BLOCK_ENTRIES).unwrap_or(usize::MAX)
                    && !self.pending_has_live_at(seq);
                // DeleteVertex is the only apply arm that may pre-seal BOTH
                // maps. Committing the vertex seal and then refusing the
                // edge seal used to leave pending vertices durable while
                // apply returned a no-op-shaped error.
                if need_vertex_seal && need_edge_seal {
                    let vertex_sealed = self.encode_pending_vertices(keys)?;
                    let (edge_sealed, next_heads) = self.encode_pending_edges(keys)?;
                    self.commit_vertex_seal(vertex_sealed);
                    self.commit_edge_seal(edge_sealed, next_heads);
                } else if need_vertex_seal {
                    self.seal_vertices(keys)?;
                } else if need_edge_seal {
                    self.seal(keys)?;
                }
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
                        self.pending_vertices
                            .insert((*vid, retired.created_at), retired);
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
                    && !self.pending_vertices_have_live_at(seq)
                {
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
                    self.fold_edge_update(keys, *eid, seq, *property, after.clone())?;
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
    /// record both simply go away (fgdb-zeay). Early-seal must not freeze a
    /// live same-seq creation: the format ceiling is 256, and publish already
    /// splits oversized pending into conforming blocks (fgdb-wlxe).
    fn retirement_entry(
        &self,
        eid: EId,
        seq: CommitSeq,
    ) -> Result<Option<(AdjacencyEntry, EdgePropertyRow)>, WriteError> {
        let edge = self.live.get(&eid).ok_or(WriteError::UnknownEdge { eid })?;
        if edge.created_at == seq {
            let is_same_run = self
                .pending
                .get(&(edge.src, edge.relation, edge.dst, eid, seq))
                .is_some_and(|pending| pending.entry.retired_at.is_none());
            // A restatement after an explicit seal is still pending, but the
            // original live creation is already durable. Folding would leave
            // that sealed row as a ghost (fgdb-6j7t).
            if is_same_run && !self.sealed_live_edges.contains(&(eid, seq)) {
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
            let key = (edge.src, edge.relation, edge.dst, eid, edge.created_at);
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
        // Statement-keyed since V7 (fgdb-ls5b): a retire and its content
        // successor are DISTINCT keys that lawfully share one block under the
        // widened (dst, eid, created_at) canonical order. A same-key insert
        // remains the same-version restatement (a retirement of something
        // created in this very run) and replaces in place.
        let key = (
            entry.src,
            entry.relation,
            entry.dst,
            entry.eid,
            entry.created_at,
        );
        // publish/seal already splits oversized pending into conforming
        // blocks. Early-seal must not freeze live rows of the commit still
        // being applied — a later same-seq delete can only fold away while
        // the creation is pending (fgdb-wlxe).
        let apply_seq = entry.retired_at.unwrap_or(entry.created_at);
        if !self.pending.contains_key(&key)
            && self.pending.len() >= usize::try_from(MAX_BLOCK_ENTRIES).unwrap_or(usize::MAX)
            && !self.pending_has_live_at(apply_seq)
        {
            self.seal(keys)?;
        }
        self.pending.insert(key, PendingStatement { entry, props });
        Ok(())
    }

    /// Encode pending edge statements without committing them.
    fn encode_pending_edges(
        &self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
    ) -> Result<EncodedEdgeSeal, WriteError> {
        let mut by_descriptor: BTreeMap<(VId, RelationId), Vec<PendingStatement>> = BTreeMap::new();
        for statement in self.pending.values().cloned() {
            by_descriptor
                .entry((statement.entry.src, statement.entry.relation))
                .or_default()
                .push(statement);
        }
        struct StagedChunk {
            first_seq: CommitSeq,
            last_seq: CommitSeq,
            family: (VId, RelationId),
            family_ordinal: usize,
            statements: Vec<PendingStatement>,
        }
        let mut staged: Vec<StagedChunk> = Vec::with_capacity(by_descriptor.len());
        for statements in by_descriptor.into_values() {
            // Cut a family at the entry-count ceiling OR the hosted-patch
            // row ceiling, whichever binds first — the same pair of format
            // limits `compact::pack_retained` uses. Early-seal may leave
            // more than MAX_BLOCK_ENTRIES pending so a later same-seq
            // delete can still fold away (fgdb-wlxe / fgdb-otcw).
            let entry_ceiling = usize::try_from(MAX_BLOCK_ENTRIES).unwrap_or(usize::MAX);
            let property_ceiling = usize::try_from(MAX_PROPERTY_PATCH_ROWS).unwrap_or(usize::MAX);
            let mut chunk: Vec<PendingStatement> = Vec::new();
            let mut chunk_propertied = 0usize;
            let mut chunks: Vec<Vec<PendingStatement>> = Vec::new();
            for statement in statements {
                let propertied = usize::from(!statement.props.is_empty());
                if !chunk.is_empty()
                    && (chunk.len() == entry_ceiling
                        || chunk_propertied + propertied > property_ceiling)
                {
                    chunks.push(std::mem::take(&mut chunk));
                    chunk_propertied = 0;
                }
                chunk_propertied += propertied;
                chunk.push(statement);
            }
            if !chunk.is_empty() {
                chunks.push(chunk);
            }
            for (family_ordinal, statements) in chunks.into_iter().enumerate() {
                let entries: Vec<AdjacencyEntry> =
                    statements.iter().map(|statement| statement.entry).collect();
                let (first_seq, last_seq) = span_of(&entries).expect("non-empty");
                let family = (
                    entries.first().expect("chunks are non-empty").src,
                    entries.first().expect("chunks are non-empty").relation,
                );
                staged.push(StagedChunk {
                    first_seq,
                    last_seq,
                    family,
                    family_ordinal,
                    statements,
                });
            }
        }
        // Roots publish in nondecreasing frontier order even when descriptor
        // keys sort differently from the commit stream that populated them.
        // The predecessor chain (V6, fgdb-4391) must follow the PUBLISHED
        // order, so ordering is decided BEFORE encoding — the sort key is
        // fully derived from content, never from the block identity the
        // predecessor link is about to change.
        staged.sort_by_key(|chunk| {
            (
                chunk.last_seq,
                chunk.first_seq,
                chunk.family,
                chunk.family_ordinal,
            )
        });
        // Encode every chunk before committing. Intra-seal predecessor
        // links still advance — but only on a local copy — so a later
        // chunk's refusal leaves `chain_heads` and `pending` exactly as
        // they were (`seal_vertices` already does this; the public apply
        // contract is that every typed refusal is a no-op).
        let mut next_heads = self.chain_heads.clone();
        let mut sealed = Vec::with_capacity(staged.len());
        for chunk in staged {
            let StagedChunk {
                first_seq,
                last_seq,
                family,
                statements,
                ..
            } = chunk;
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
            let predecessor = next_heads.get(&family).copied();
            let (bytes, property_patch) = if rows.is_empty() {
                (
                    encode_block(self.partition, predecessor, &entries)
                        .map_err(WriteError::Block)?,
                    None,
                )
            } else {
                let patch_bytes = encode_property_patch(&rows).map_err(WriteError::EdgeProps)?;
                let patch_id = property_patch_id(keys.0, keys.1, &patch_bytes);
                let bytes = encode_block_with_properties(
                    self.partition,
                    predecessor,
                    &entries,
                    patch_id,
                    &locators,
                    &rows,
                )
                .map_err(WriteError::Block)?;
                (
                    bytes,
                    Some(SealedPropertyPatch {
                        patch_id,
                        bytes: patch_bytes,
                    }),
                )
            };
            let sealed_id = block_id(keys.0, keys.1, &bytes);
            // The family's chain advances to this block; the next chunk of
            // the same family — even within THIS seal — links to it.
            next_heads.insert(family, DeltaBlockVersion(sealed_id));
            sealed.push(SealedBlock {
                block_id: sealed_id,
                bytes,
                first_seq,
                last_seq,
                property_patch,
            });
        }
        Ok((sealed, next_heads))
    }

    fn commit_edge_seal(
        &mut self,
        sealed: Vec<SealedBlock>,
        next_heads: BTreeMap<(VId, RelationId), DeltaBlockVersion>,
    ) {
        for statement in self.pending.values() {
            if statement.entry.retired_at.is_none() {
                self.sealed_live_edges
                    .insert((statement.entry.eid, statement.entry.created_at));
            }
        }
        self.chain_heads = next_heads;
        self.pending.clear();
        self.sealed.extend(sealed);
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
        let (sealed, next_heads) = self.encode_pending_edges(keys)?;
        let first = sealed.first().cloned();
        self.commit_edge_seal(sealed, next_heads);
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
    /// Fold one edge property transition (fgdb-ls5b): the live statement
    /// retires at `seq` and its successor — identical topology, updated row —
    /// begins there, the FGSV V2 chain shape applied to edges. A transition
    /// inside the statement's own creation commit folds IN PLACE: the
    /// intermediate row never existed on any snapshot, exactly like the
    /// same-commit vertex fold. The writer APPLIES the after-image and does
    /// not verify the before-image, for the cascade's reason: images are
    /// checked against materialized state by the materializer that produced
    /// them.
    fn fold_edge_update(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        eid: EId,
        seq: CommitSeq,
        key: PropertyKeyId,
        value: Option<CanonicalScalar>,
    ) -> Result<(), WriteError> {
        let current = self
            .live
            .get(&eid)
            .cloned()
            .ok_or(WriteError::UnknownEdge { eid })?;
        let mut successor_row = current.props.clone();
        match successor_row.binary_search_by_key(&key, |(k, _)| *k) {
            Ok(at) => match value {
                Some(value) => successor_row[at].1 = value,
                None => {
                    successor_row.remove(at);
                }
            },
            Err(at) => {
                if let Some(value) = value {
                    successor_row.insert(at, (key, value));
                }
            }
        }
        let statement_key = (current.src, current.relation, current.dst, eid, seq);
        if current.created_at == seq {
            if self
                .pending
                .get(&statement_key)
                .is_some_and(|pending| pending.entry.retired_at.is_none())
            {
                let entry = self
                    .pending
                    .get(&statement_key)
                    .expect("checked above")
                    .entry;
                self.pending.insert(
                    statement_key,
                    PendingStatement {
                        entry,
                        props: successor_row.clone(),
                    },
                );
                self.live
                    .entry(eid)
                    .and_modify(|live| live.props = successor_row);
                return Ok(());
            }
            // The same-commit creation already sealed (format ceiling).
            // Restate the live statement; a tombstone at created_at == seq
            // would be an empty interval (fgdb-aubf).
            let restatement = AdjacencyEntry {
                src: current.src,
                relation: current.relation,
                dst: current.dst,
                eid,
                created_at: seq,
                retired_at: None,
            };
            self.push(keys, restatement, successor_row.clone())?;
            self.live.insert(
                eid,
                LiveEdge {
                    src: current.src,
                    relation: current.relation,
                    dst: current.dst,
                    created_at: seq,
                    props: successor_row,
                },
            );
            return Ok(());
        }
        // The retiring statement keeps ITS OWN row — that row is the content
        // of `[created_at, seq)` and pre-update snapshots keep answering it.
        let tombstone = AdjacencyEntry {
            src: current.src,
            relation: current.relation,
            dst: current.dst,
            eid,
            created_at: current.created_at,
            retired_at: Some(seq),
        };
        let successor = AdjacencyEntry {
            src: current.src,
            relation: current.relation,
            dst: current.dst,
            eid,
            created_at: seq,
            retired_at: None,
        };
        // Same law as `fold_vertex_update`: seal first when the pair would
        // trip the ceiling on the *second* push. Otherwise the tombstone
        // stages, the successor's early-seal refuses, and apply returns
        // with a retirement that `live` does not know about.
        let tombstone_key = (
            current.src,
            current.relation,
            current.dst,
            eid,
            current.created_at,
        );
        let successor_key = (current.src, current.relation, current.dst, eid, seq);
        let incoming = usize::from(!self.pending.contains_key(&tombstone_key))
            + usize::from(!self.pending.contains_key(&successor_key));
        if self.pending.len() + incoming > usize::try_from(MAX_BLOCK_ENTRIES).unwrap_or(usize::MAX)
            && !self.pending_has_live_at(seq)
        {
            self.seal(keys)?;
        }
        self.push(keys, tombstone, current.props.clone())?;
        self.push(keys, successor, successor_row.clone())?;
        self.live.insert(
            eid,
            LiveEdge {
                src: current.src,
                relation: current.relation,
                dst: current.dst,
                created_at: seq,
                props: successor_row,
            },
        );
        Ok(())
    }

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
        if current.created_at == seq {
            if self
                .pending_vertices
                .get(&(vid, seq))
                .is_some_and(|pending| pending.retired_at.is_none())
            {
                self.pending_vertices.insert((vid, seq), successor.clone());
                self.live_vertices.insert(vid, successor);
                return Ok(());
            }
            // Same-commit creation already sealed. Restate the live row
            // rather than closing an empty interval (fgdb-aubf).
            successor.created_at = seq;
            successor.retired_at = None;
            crate::vertex::validate_patch_row(0, &successor).map_err(WriteError::Patch)?;
            if !self.pending_vertices.contains_key(&(vid, seq))
                && self.pending_vertices.len()
                    >= usize::try_from(crate::vertex::MAX_PATCH_ROWS).unwrap_or(usize::MAX)
                && !self.pending_vertices_have_live_at(seq)
            {
                self.seal_vertices(keys)?;
            }
            self.pending_vertices.insert((vid, seq), successor.clone());
            self.live_vertices.insert(vid, successor);
            return Ok(());
        }
        let mut tombstone = current;
        tombstone.retired_at = Some(seq);
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
            && !self.pending_vertices_have_live_at(seq)
        {
            self.seal_vertices(keys)?;
        }
        self.pending_vertices.insert(tombstone_key, tombstone);
        self.pending_vertices
            .insert(successor_key, successor.clone());
        self.live_vertices.insert(vid, successor);
        Ok(())
    }

    /// Encode pending vertex rows without committing them.
    fn encode_pending_vertices(
        &self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
    ) -> Result<Vec<SealedPatch>, WriteError> {
        // Early-seal may leave more than MAX_PATCH_ROWS pending so a later
        // same-seq delete can still fold away. Encode every conforming
        // chunk before committing so a mid-seal refusal leaves the map
        // untouched — the same atomicity the single-patch path had.
        let rows: Vec<VertexRow> = self.pending_vertices.values().cloned().collect();
        let ceiling = usize::try_from(crate::vertex::MAX_PATCH_ROWS).unwrap_or(usize::MAX);
        let mut staged: Vec<Vec<VertexRow>> =
            rows.chunks(ceiling).map(<[VertexRow]>::to_vec).collect();
        staged.sort_by_key(|chunk| {
            span_of_rows(chunk)
                .map(|(first_seq, last_seq)| (last_seq, first_seq))
                .unwrap_or((CommitSeq(0), CommitSeq(0)))
        });
        let mut sealed = Vec::with_capacity(staged.len());
        for chunk in staged {
            let bytes = encode_patch(&chunk).map_err(WriteError::Patch)?;
            let (first_seq, last_seq) = span_of_rows(&chunk).expect("non-empty");
            sealed.push(SealedPatch {
                patch_id: vertex_patch_id(keys.0, keys.1, &bytes),
                bytes,
                first_seq,
                last_seq,
            });
        }
        Ok(sealed)
    }

    fn commit_vertex_seal(&mut self, sealed: Vec<SealedPatch>) {
        for row in self.pending_vertices.values() {
            if row.retired_at.is_none() {
                self.sealed_live_vertices.insert((row.vid, row.created_at));
            }
        }
        self.pending_vertices.clear();
        self.sealed_patches.extend(sealed);
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
        let sealed = self.encode_pending_vertices(keys)?;
        let first = sealed.first().cloned();
        self.commit_vertex_seal(sealed);
        Ok(first)
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
    use crate::vertex::{MAX_PATCH_ROWS, VertexPatchError, VertexRow};
    use crate::{AdjacencyEntry, BlockError, MAX_BLOCK_ENTRIES};
    use fgdb_delta_types::{DeltaRow, ElementId, PropertyKeyId, RelationId};
    use fgdb_types::ids::DatabaseSecurityNamespaceId;
    use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, VId};

    const K_OID: [u8; 32] = [0x5a; 32];

    fn keys() -> (&'static [u8; 32], DatabaseSecurityNamespaceId) {
        (&K_OID, DatabaseSecurityNamespaceId([0x77; 32]))
    }

    #[test]
    fn a_failed_seal_preserves_its_pending_entries() {
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        writer.pending.insert(
            (VId(1), RelationId(1), VId(2), EId(10), CommitSeq(0)),
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

    /// A later chunk's refusal must not leave the earlier chunk's predecessor
    /// installed. `seal_vertices` encodes every chunk before committing;
    /// edge `seal` used to advance `chain_heads` inside the encode loop, so a
    /// two-family pending set whose second family is illegal would poison
    /// retry: the next encode linked a predecessor that was never published.
    #[test]
    fn a_failed_seal_does_not_advance_chain_heads_of_an_earlier_chunk() {
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        writer.pending.insert(
            (VId(1), RelationId(1), VId(2), EId(10), CommitSeq(1)),
            PendingStatement {
                entry: AdjacencyEntry {
                    src: VId(1),
                    relation: RelationId(1),
                    dst: VId(2),
                    eid: EId(10),
                    created_at: CommitSeq(1),
                    retired_at: None,
                },
                props: vec![],
            },
        );
        writer.pending.insert(
            (VId(2), RelationId(1), VId(3), EId(11), CommitSeq(1)),
            PendingStatement {
                entry: AdjacencyEntry {
                    src: VId(2),
                    relation: RelationId(1),
                    dst: VId(3),
                    eid: EId(11),
                    created_at: CommitSeq(1),
                    retired_at: Some(CommitSeq(1)),
                },
                props: vec![],
            },
        );

        let expected = Err(WriteError::Block(BlockError::RetiredBeforeCreated {
            at: 0,
            created_at: CommitSeq(1),
            retired_at: CommitSeq(1),
        }));
        assert_eq!(writer.seal(keys()), expected);
        assert_eq!(writer.pending_len(), 2, "both families stay pending");
        assert!(
            writer.chain_heads.is_empty(),
            "the lawful first family must not install a predecessor that was \
             never published"
        );
        assert!(writer.sealed.is_empty());
        assert_eq!(
            writer.seal(keys()),
            expected,
            "retry observes the same refusal against the same heads"
        );
        assert!(writer.chain_heads.is_empty());
    }

    /// Cross-commit edge restatement `push`es a tombstone then a successor.
    /// If the successor `push` is the one that hits the entry ceiling, a
    /// failing seal used to leave the tombstone staged while `live` still
    /// held the pre-update edge — apply's typed-refusal contract is a no-op.
    #[test]
    fn a_failed_successor_push_does_not_leave_a_restatement_tombstone() {
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        let rel = RelationId(1);
        for vid in [1_u128, 2] {
            writer
                .apply(
                    keys(),
                    CommitSeq(1),
                    &DeltaRow::CreateVertex {
                        vid: VId(vid),
                        birth_ordinal: u64::try_from(vid).expect("test vid fits"),
                        labels: vec![],
                        props: vec![],
                        valid_time: None,
                    },
                )
                .expect("vertices");
        }
        writer
            .apply(
                keys(),
                CommitSeq(1),
                &DeltaRow::CreateEdge {
                    eid: EId(10),
                    birth_ordinal: 3,
                    src: VId(1),
                    relation: rel,
                    dst: VId(2),
                    canonical_key: None,
                    props: vec![],
                    valid_time: None,
                },
            )
            .expect("edge");
        writer.seal(keys()).expect("birth is durable");
        writer.seal_vertices(keys()).expect("vertex birth durable");

        let ceiling = usize::try_from(MAX_BLOCK_ENTRIES).unwrap();
        for i in 0..(ceiling - 1) {
            let src = VId(1000 + i as u128);
            writer.pending.insert(
                (src, rel, VId(3), EId(1000 + i as u128), CommitSeq(0)),
                PendingStatement {
                    entry: AdjacencyEntry {
                        src,
                        relation: rel,
                        dst: VId(3),
                        eid: EId(1000 + i as u128),
                        created_at: CommitSeq(0),
                        retired_at: None,
                    },
                    props: vec![],
                },
            );
        }
        assert_eq!(writer.pending_len(), ceiling - 1);

        let refusal = writer.apply(
            keys(),
            CommitSeq(2),
            &DeltaRow::Property {
                elem: ElementId::Edge(EId(10)),
                property: PropertyKeyId(7),
                before: None,
                after: Some(CanonicalScalar::Int(4)),
            },
        );
        assert_eq!(
            refusal,
            Err(WriteError::Block(BlockError::CreatedAtZero { at: 0 }))
        );
        assert!(
            writer.live_edge(EId(10)).is_some(),
            "the refused restatement must leave the pre-update edge live"
        );
        assert!(
            !writer
                .pending
                .contains_key(&(VId(1), rel, VId(2), EId(10), CommitSeq(1))),
            "the tombstone of a refused restatement must not stay staged"
        );
        assert_eq!(
            writer.pending_len(),
            ceiling - 1,
            "the planted illegal families stay; the restatement added nothing"
        );
    }

    /// `DeleteVertex` used to retire the cascade and only then seal an
    /// oversized vertex pending set. A refusing `seal_vertices` left the
    /// edges gone and the vertex live — the half-applied state
    /// `preflight_retirements` exists to prevent.
    #[test]
    fn a_failed_vertex_seal_does_not_retire_the_cascade() {
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        let rel = RelationId(1);
        for vid in [1_u128, 2] {
            writer
                .apply(
                    keys(),
                    CommitSeq(1),
                    &DeltaRow::CreateVertex {
                        vid: VId(vid),
                        birth_ordinal: u64::try_from(vid).expect("test vid fits"),
                        labels: vec![],
                        props: vec![],
                        valid_time: None,
                    },
                )
                .expect("vertices");
        }
        writer
            .apply(
                keys(),
                CommitSeq(1),
                &DeltaRow::CreateEdge {
                    eid: EId(10),
                    birth_ordinal: 3,
                    src: VId(1),
                    relation: rel,
                    dst: VId(2),
                    canonical_key: None,
                    props: vec![],
                    valid_time: None,
                },
            )
            .expect("edge");
        writer.seal(keys()).expect("edge birth durable");
        writer.seal_vertices(keys()).expect("vertex birth durable");

        let ceiling = usize::try_from(MAX_PATCH_ROWS).unwrap();
        for i in 0..ceiling {
            let vid = VId(2000 + i as u128);
            writer.pending_vertices.insert(
                (vid, CommitSeq(0)),
                VertexRow {
                    vid,
                    birth_ordinal: 1,
                    created_at: CommitSeq(0),
                    retired_at: None,
                    labels: vec![],
                    props: vec![],
                },
            );
        }
        assert_eq!(writer.pending_vertex_len(), ceiling);

        let refusal = writer.apply(
            keys(),
            CommitSeq(2),
            &DeltaRow::DeleteVertex {
                vid: VId(2),
                before_version: fgdb_types::ids::ObjectId([0u8; 32]),
                sorted_retired_incident_edges: vec![EId(10)],
            },
        );
        assert_eq!(
            refusal,
            Err(WriteError::Patch(VertexPatchError::CreatedAtZero { at: 0 }))
        );
        assert!(
            writer.is_vertex_live(VId(2)),
            "the refused delete must leave the vertex live"
        );
        assert!(
            writer.live_edge(EId(10)).is_some(),
            "the refused delete must not retire the cascade"
        );
        assert_eq!(
            writer.pending_vertex_len(),
            ceiling,
            "the planted illegal rows stay; the delete added nothing"
        );
    }

    /// A two-edge cascade used to `push` the first tombstone and only then
    /// early-seal on the second. A refusing seal left E10 retired and E11
    /// live — half a cascade, which preflight is supposed to make
    /// unreachable.
    #[test]
    fn a_failed_cascade_seal_does_not_retire_the_first_edge() {
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        let rel = RelationId(1);
        for vid in [1_u128, 2] {
            writer
                .apply(
                    keys(),
                    CommitSeq(1),
                    &DeltaRow::CreateVertex {
                        vid: VId(vid),
                        birth_ordinal: u64::try_from(vid).expect("test vid fits"),
                        labels: vec![],
                        props: vec![],
                        valid_time: None,
                    },
                )
                .expect("vertices");
        }
        for (eid, src, dst) in [(10_u128, 1_u128, 2_u128), (11, 2, 1)] {
            writer
                .apply(
                    keys(),
                    CommitSeq(1),
                    &DeltaRow::CreateEdge {
                        eid: EId(eid),
                        birth_ordinal: u64::try_from(eid).expect("test eid fits"),
                        src: VId(src),
                        relation: rel,
                        dst: VId(dst),
                        canonical_key: None,
                        props: vec![],
                        valid_time: None,
                    },
                )
                .expect("edge");
        }
        writer.seal(keys()).expect("edge births durable");
        writer.seal_vertices(keys()).expect("vertex births durable");

        let ceiling = usize::try_from(MAX_BLOCK_ENTRIES).unwrap();
        for i in 0..(ceiling - 1) {
            let src = VId(1000 + i as u128);
            writer.pending.insert(
                (src, rel, VId(3), EId(1000 + i as u128), CommitSeq(0)),
                PendingStatement {
                    entry: AdjacencyEntry {
                        src,
                        relation: rel,
                        dst: VId(3),
                        eid: EId(1000 + i as u128),
                        created_at: CommitSeq(0),
                        retired_at: None,
                    },
                    props: vec![],
                },
            );
        }
        assert_eq!(writer.pending_len(), ceiling - 1);

        let refusal = writer.apply(
            keys(),
            CommitSeq(2),
            &DeltaRow::DeleteVertex {
                vid: VId(2),
                before_version: fgdb_types::ids::ObjectId([0u8; 32]),
                sorted_retired_incident_edges: vec![EId(10), EId(11)],
            },
        );
        assert_eq!(
            refusal,
            Err(WriteError::Block(BlockError::CreatedAtZero { at: 0 }))
        );
        assert!(
            writer.is_vertex_live(VId(2)),
            "the refused delete must leave the vertex live"
        );
        assert!(
            writer.live_edge(EId(10)).is_some() && writer.live_edge(EId(11)).is_some(),
            "neither cascade member may retire if a later member's seal refuses"
        );
        assert_eq!(
            writer.pending_len(),
            ceiling - 1,
            "the planted illegal families stay; the cascade added nothing"
        );
    }

    /// DeleteVertex used to `seal_vertices` first and only then `seal` the
    /// cascade. A refusing edge seal left the vertex pending map already
    /// published — apply returned Err while the writer was not as it was.
    #[test]
    fn a_failed_cascade_seal_does_not_commit_a_prior_vertex_seal() {
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        let rel = RelationId(1);
        for vid in [1_u128, 2] {
            writer
                .apply(
                    keys(),
                    CommitSeq(1),
                    &DeltaRow::CreateVertex {
                        vid: VId(vid),
                        birth_ordinal: u64::try_from(vid).expect("test vid fits"),
                        labels: vec![],
                        props: vec![],
                        valid_time: None,
                    },
                )
                .expect("vertices");
        }
        for (eid, src, dst) in [(10_u128, 1_u128, 2_u128), (11, 2, 1)] {
            writer
                .apply(
                    keys(),
                    CommitSeq(1),
                    &DeltaRow::CreateEdge {
                        eid: EId(eid),
                        birth_ordinal: u64::try_from(eid).expect("test eid fits"),
                        src: VId(src),
                        relation: rel,
                        dst: VId(dst),
                        canonical_key: None,
                        props: vec![],
                        valid_time: None,
                    },
                )
                .expect("edge");
        }
        writer.seal(keys()).expect("edge births durable");
        writer.seal_vertices(keys()).expect("vertex births durable");
        let patches_before = writer.sealed_patches().len();

        let vertex_ceiling = usize::try_from(MAX_PATCH_ROWS).unwrap();
        for i in 0..vertex_ceiling {
            let vid = VId(2000 + i as u128);
            writer.pending_vertices.insert(
                (vid, CommitSeq(1)),
                VertexRow {
                    vid,
                    birth_ordinal: 1,
                    created_at: CommitSeq(1),
                    retired_at: None,
                    labels: vec![],
                    props: vec![],
                },
            );
        }
        assert_eq!(writer.pending_vertex_len(), vertex_ceiling);

        let edge_ceiling = usize::try_from(MAX_BLOCK_ENTRIES).unwrap();
        for i in 0..(edge_ceiling - 1) {
            let src = VId(1000 + i as u128);
            writer.pending.insert(
                (src, rel, VId(3), EId(1000 + i as u128), CommitSeq(0)),
                PendingStatement {
                    entry: AdjacencyEntry {
                        src,
                        relation: rel,
                        dst: VId(3),
                        eid: EId(1000 + i as u128),
                        created_at: CommitSeq(0),
                        retired_at: None,
                    },
                    props: vec![],
                },
            );
        }
        assert_eq!(writer.pending_len(), edge_ceiling - 1);

        let refusal = writer.apply(
            keys(),
            CommitSeq(2),
            &DeltaRow::DeleteVertex {
                vid: VId(2),
                before_version: fgdb_types::ids::ObjectId([0u8; 32]),
                sorted_retired_incident_edges: vec![EId(10), EId(11)],
            },
        );
        assert_eq!(
            refusal,
            Err(WriteError::Block(BlockError::CreatedAtZero { at: 0 }))
        );
        assert!(
            writer.is_vertex_live(VId(2)),
            "the refused delete must leave the vertex live"
        );
        assert!(
            writer.live_edge(EId(10)).is_some() && writer.live_edge(EId(11)).is_some(),
            "neither cascade member may retire if the paired edge seal refuses"
        );
        assert_eq!(
            writer.pending_vertex_len(),
            vertex_ceiling,
            "a refused dual pre-seal must not publish the vertex pending map"
        );
        assert_eq!(
            writer.sealed_patches().len(),
            patches_before,
            "a refused dual pre-seal must not grow the sealed patch list"
        );
        assert_eq!(
            writer.pending_len(),
            edge_ceiling - 1,
            "the planted illegal families stay; the cascade added nothing"
        );
    }
}
