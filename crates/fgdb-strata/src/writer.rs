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
    EdgePropertyPatchError, EdgePropertyRow, MAX_PROPERTY_PATCH_ROWS, PROPERTY_PATCH_HEADER_BYTES,
    admitted_row_bytes, encode_property_patch, property_patch_id,
};
use crate::id_map::{IdMap, IdSet};
use crate::root::{BlockRef, PartitionRoot, PatchRef, RootError, span_of, validate_root};
use crate::vertex::{
    VertexPatchError, VertexPatchRows, VertexRow, encode_patch, span_of_rows, vertex_patch_id,
};
use crate::{
    AdjacencyEntry, BlockError, DeltaBlockVersion, MAX_BLOCK_ENTRIES, block_id, encode_block,
    encode_block_with_properties, validate_entry,
};
use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_types::CanonicalPropertyValue;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// A sealed block: its identity, its bytes, the range it covers, and — when
/// any of its entries carry properties — the hosted edge property patch the
/// block's locator column references (fgdb-yqor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedBlock {
    pub block_id: ObjectId,
    /// Immutable encoded payload. Retained writers and historical preparation
    /// anchors share these bytes; cloning a writer never copies sealed history.
    pub bytes: Arc<[u8]>,
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
    pub bytes: Arc<[u8]>,
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

/// An encoded edge seal: its blocks, and the chain heads of only the families
/// it advanced, which committing the seal merges into the writer's heads.
type EncodedEdgeSeal = (
    Vec<SealedBlock>,
    BTreeMap<(VId, RelationId), DeltaBlockVersion>,
);
type EdgeStatementKey = (VId, RelationId, VId, EId, CommitSeq);

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
    /// Intrusive incidence links, source first. A self-loop uses only the
    /// source links. Dense slots are local to this live map, never IDs.
    incident: [IncidentLinks; 2],
}

/// IdMap admits fewer than u32::MAX - 1 entries, so this sentinel cannot
/// alias a live slot. Full-width VId/EId values remain the map's keys.
const NO_INCIDENT_EDGE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IncidentLinks {
    previous: u32,
    next: u32,
}

impl IncidentLinks {
    const EMPTY: Self = Self {
        previous: NO_INCIDENT_EDGE,
        next: NO_INCIDENT_EDGE,
    };
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveVertex {
    row: VertexRow,
    incident_head: u32,
}

impl LiveVertex {
    fn new(row: VertexRow) -> Self {
        Self {
            row,
            incident_head: NO_INCIDENT_EDGE,
        }
    }
}

/// One vertex content transition the fold applies (fgdb-stb6).
enum VertexContentUpdate {
    Label {
        label: LabelId,
        member: bool,
    },
    Property {
        key: PropertyKeyId,
        value: Option<CanonicalPropertyValue>,
    },
}

/// A sealed vertex patch: its identity, its bytes, and the range it covers —
/// the vertex counterpart of [`SealedBlock`] (fgdb-3xoi).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedPatch {
    pub patch_id: ObjectId,
    pub bytes: Arc<[u8]>,
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
/// `Clone` preserves an exact independent fold, including live rows and spent
/// identities, for historical preparation. Sealed payloads are immutable and
/// shared individually, so the clone copies maps and object metadata without
/// copying historical encoded bytes. Continued folding changes only the
/// clone's private staging/live state and appends new sealed objects.
/// `publish_in_place` serves the retained live writer without a clone. A
/// clone-publish at sequence k remains byte-identical to a fresh writer
/// replaying rows 1..k; `tests/incremental_publish_equals_rebuild.rs` pins that
/// equality, independent subsequent mutation, and payload sharing.
#[derive(Debug, Clone)]
pub struct BlockWriter {
    graph: GraphId,
    branch: BranchId,
    partition: u64,
    /// Entries not yet sealed, keyed by the complete stable adjacency
    /// identity, each with the properties its statement carries.
    pending: BTreeMap<EdgeStatementKey, PendingStatement>,
    /// The full statement of every edge currently live — endpoints, creation,
    /// AND properties, because a tombstone must RESTATE the properties so a
    /// pre-retirement snapshot keeps answering them (fgdb-yqor), exactly as
    /// the vertex live map remembers rows for the same reason.
    live: IdMap<EId, LiveEdge>,
    /// Every EId admitted while replaying this partition-local writer.
    ///
    /// The graph-wide allocator is enforced before partition routing; this is a
    /// defense-in-depth check over the history visible to this fold. Shared
    /// copy-on-write so the engine allocator can probe it without a copy
    /// ([`BlockWriter::spent_edges`]).
    spent: Arc<IdSet<EId>>,
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
    live_vertices: IdMap<VId, LiveVertex>,
    /// Published-constructor inputs historically may omit vertex patches.
    /// Such incomplete endpoint coverage keeps the exact full-scan path;
    /// ordinary folds and complete recovered partitions use local links.
    incident_indexed: bool,
    /// Every VId admitted while replaying this partition-local writer, shared
    /// copy-on-write like `spent`.
    spent_vertices: Arc<IdSet<VId>>,
    sealed_patches: Vec<SealedPatch>,
    /// Live identities whose same-seq creation has already been sealed in
    /// this run. A later same-seq delete cannot fold those away — the
    /// durable image already exists, and an empty interval is illegal.
    /// Only `last_seq` can still use these guards: an older sequence cannot
    /// be applied again, and a newer sequence retires a nonempty interval.
    /// A successful sequence advance clears both sets; refused advances
    /// leave them intact with the rest of the fold.
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
        patches: &[VertexPatchRows],
        frontier: CommitSeq,
    ) -> Result<Self, RootError> {
        let live_edges = crate::root::merge_all_edges_with_props(blocks, block_props, frontier)?;
        Ok(Self::assemble_published(
            graph,
            branch,
            partition,
            sealed,
            sealed_patches,
            live_edges,
            blocks,
            patches,
            frontier,
        ))
    }

    /// [`Self::from_published_partition`] over the edge history a root walk
    /// already admitted ([`crate::store::RootAdmission::edge_statements`]),
    /// so a writable open validates published history ONCE, in the walk,
    /// instead of collapsing every block a second time here (fgdb-e8gr4).
    /// `admitted` must be the collapse of exactly `blocks`, in order.
    #[allow(clippy::too_many_arguments)]
    pub fn from_admitted_partition(
        graph: GraphId,
        branch: BranchId,
        partition: u64,
        sealed: Vec<SealedBlock>,
        sealed_patches: Vec<SealedPatch>,
        admitted: &BTreeMap<(EId, CommitSeq), AdjacencyEntry>,
        blocks: &[Vec<AdjacencyEntry>],
        block_props: &[Option<crate::edge_props::BlockProps>],
        patches: &[VertexPatchRows],
        frontier: CommitSeq,
    ) -> Result<Self, RootError> {
        let live_edges =
            crate::root::merge_admitted_edges_with_props(admitted, blocks, block_props, frontier)?;
        Ok(Self::assemble_published(
            graph,
            branch,
            partition,
            sealed,
            sealed_patches,
            live_edges,
            blocks,
            patches,
            frontier,
        ))
    }

    /// The rest of a published writer, once its live edges are known.
    #[allow(clippy::too_many_arguments)]
    fn assemble_published(
        graph: GraphId,
        branch: BranchId,
        partition: u64,
        sealed: Vec<SealedBlock>,
        sealed_patches: Vec<SealedPatch>,
        live_edges: Vec<(AdjacencyEntry, crate::edge_props::EdgePropertyRow)>,
        blocks: &[Vec<AdjacencyEntry>],
        patches: &[VertexPatchRows],
        frontier: CommitSeq,
    ) -> Self {
        // History merging already selected one live row per identity. Reserve
        // those exact counts so recovery does not repeatedly allocate and
        // rehash a growing live map. Keep the same insertion order and probe
        // layout as the ordinary fold.
        let mut sealed_live_edges = BTreeSet::new();
        let mut live = IdMap::with_capacity(live_edges.len());
        for (entry, props) in live_edges {
            // These births already have durable images. A same-frontier
            // restatement may become pending again after reconstruction,
            // but a following delete still cannot fold that image away.
            if entry.created_at == frontier {
                sealed_live_edges.insert((entry.eid, entry.created_at));
            }
            live.insert(
                entry.eid,
                LiveEdge {
                    src: entry.src,
                    relation: entry.relation,
                    dst: entry.dst,
                    created_at: entry.created_at,
                    props,
                    incident: [IncidentLinks::EMPTY; 2],
                },
            );
        }
        let spent: IdSet<EId> = blocks.iter().flatten().map(|entry| entry.eid).collect();
        let mut sealed_live_vertices = BTreeSet::new();
        let live_rows = crate::vertex::merge_all_vertices(patches, frontier);
        let mut live_vertices = IdMap::with_capacity(live_rows.len());
        for row in live_rows {
            if row.created_at == frontier {
                sealed_live_vertices.insert((row.vid, row.created_at));
            }
            live_vertices.insert(row.vid, LiveVertex::new(row));
        }
        let spent_vertices: IdSet<VId> = patches.iter().flatten().map(|row| row.vid).collect();
        // The chain head per family is the LAST published block of that
        // family — publication order IS the chain (fgdb-4391).
        let chain_heads = last_wins(
            sealed
                .iter()
                .zip(blocks)
                .filter_map(|(sealed_block, entries)| {
                    entries.first().map(|first| {
                        (
                            (first.src, first.relation),
                            DeltaBlockVersion(sealed_block.block_id),
                        )
                    })
                })
                .collect(),
        );
        let mut writer = Self {
            graph,
            branch,
            partition,
            pending: BTreeMap::new(),
            live,
            spent: Arc::new(spent),
            sealed,
            chain_heads,
            pending_vertices: BTreeMap::new(),
            live_vertices,
            incident_indexed: true,
            spent_vertices: Arc::new(spent_vertices),
            sealed_patches,
            sealed_live_edges,
            sealed_live_vertices,
            last_seq: (frontier.0 > 0).then_some(frontier),
        };
        for slot in 0..writer.live.len() {
            let edge = writer.live.at_slot(slot as u32).1;
            if let Ok(endpoint_slots) = writer.incident_endpoint_slots(edge.src, edge.dst) {
                writer.link_incident_edge(slot as u32, endpoint_slots);
            } else {
                // Coverage and linking share the same endpoint probes. An
                // incomplete input must leave no partly-built topology for
                // a later clone or fold to observe: discard the private
                // links before selecting the existing scan fallback.
                writer.incident_indexed = false;
                if slot != 0 {
                    writer.clear_incident_links();
                }
                break;
            }
        }
        writer
    }

    pub fn new(graph: GraphId, branch: BranchId, partition: u64) -> Self {
        Self {
            graph,
            branch,
            partition,
            pending: BTreeMap::new(),
            live: IdMap::new(),
            spent: Arc::default(),
            sealed: Vec::new(),
            chain_heads: BTreeMap::new(),
            pending_vertices: BTreeMap::new(),
            live_vertices: IdMap::new(),
            incident_indexed: true,
            spent_vertices: Arc::default(),
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

    /// A shared, immutable view of every edge identity this fold admitted:
    /// the engine allocator's existence check (fgdb-hxgm1 channel 2). The
    /// handle is an O(1) clone. While a strong handle is outstanding, the
    /// fold's next admission copies the set instead of mutating it under the
    /// reader. A caller that may outlive that admission keeps an
    /// `Arc::downgrade` instead: the fold then mutates in place, and the weak
    /// handle stops upgrading.
    pub fn spent_edges(&self) -> Arc<IdSet<EId>> {
        Arc::clone(&self.spent)
    }

    /// The vertex counterpart of [`BlockWriter::spent_edges`].
    pub fn spent_vertices(&self) -> Arc<IdSet<VId>> {
        Arc::clone(&self.spent_vertices)
    }

    /// Is `vid` live in this fold?
    pub fn is_vertex_live(&self, vid: VId) -> bool {
        self.live_vertices.contains_key(&vid)
    }

    /// The full LIVE statement of `vid` — the row the statement-chain
    /// version transcript encodes (fgdb-ge6a v3).
    pub fn live_vertex_row(&self, vid: VId) -> Option<VertexRow> {
        self.live_vertices.get(&vid).map(|live| live.row.clone())
    }

    /// Borrow one live vertex's labels without copying its property payload.
    /// Derived cardinalities use this before the corresponding row is folded.
    pub fn live_vertex_labels(&self, vid: VId) -> Option<&[LabelId]> {
        self.live_vertices
            .get(&vid)
            .map(|live| live.row.labels.as_slice())
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
    /// Complete folds visit only this vertex's degree before sorting those
    /// EIds; unrelated live edges do not contribute lookup work. Published
    /// inputs with incomplete endpoint coverage retain the full-scan path.
    pub fn live_incident_edges(&self, vid: VId) -> Vec<EId> {
        let mut incident = Vec::new();
        if self.incident_indexed {
            let mut slot = self
                .live_vertices
                .get(&vid)
                .map_or(NO_INCIDENT_EDGE, |live| live.incident_head);
            while slot != NO_INCIDENT_EDGE {
                #[cfg(test)]
                tests::record_incident_visit();
                let (eid, edge) = self.live.at_slot(slot);
                incident.push(*eid);
                slot = edge.incident[usize::from(edge.src != vid)].next;
            }
        } else {
            // Preserve constructor semantics for incomplete vertex patches:
            // even an absent vertex may have edges in that admitted input.
            incident.extend(
                self.live
                    .iter()
                    .filter(|(_, edge)| {
                        #[cfg(test)]
                        tests::record_incident_visit();
                        edge.src == vid || edge.dst == vid
                    })
                    .map(|(eid, _)| *eid),
            );
        }
        // Incidence order is private; the public cascade remains canonical.
        incident.sort_unstable();
        incident
    }

    fn incident_links_mut(&mut self, slot: u32, vid: VId) -> &mut IncidentLinks {
        let edge = self.live.at_slot_mut(slot);
        debug_assert!(edge.src == vid || edge.dst == vid);
        &mut edge.incident[usize::from(edge.src != vid)]
    }

    /// Resolve source before destination, preserving CreateEdge's refusal
    /// order. Dense vertex slots survive edge staging, which never inserts
    /// or removes a live vertex, and can be reused when linking succeeds.
    fn incident_endpoint_slots(&self, src: VId, dst: VId) -> Result<[u32; 2], VId> {
        Ok([
            self.live_vertices.dense_slot(&src).ok_or(src)?,
            self.live_vertices.dense_slot(&dst).ok_or(dst)?,
        ])
    }

    fn clear_incident_links(&mut self) {
        for slot in 0..self.live.len() {
            self.live.at_slot_mut(slot as u32).incident = [IncidentLinks::EMPTY; 2];
        }
        for slot in 0..self.live_vertices.len() {
            self.live_vertices.at_slot_mut(slot as u32).incident_head = NO_INCIDENT_EDGE;
        }
    }

    /// Insert an already-admitted edge at its endpoint heads. This adds no
    /// allocation: links live in the existing edge/vertex records, bounded
    /// by their live map capacities, and clone with the same dense order.
    fn link_incident_edge(&mut self, slot: u32, endpoint_slots: [u32; 2]) {
        if !self.incident_indexed {
            return;
        }
        let edge = self.live.at_slot(slot).1;
        let endpoints = [edge.src, edge.dst];
        for (side, vid) in endpoints.into_iter().enumerate() {
            if side == 1 && vid == endpoints[0] {
                continue;
            }
            debug_assert_eq!(*self.live_vertices.at_slot(endpoint_slots[side]).0, vid);
            let vertex = self.live_vertices.at_slot_mut(endpoint_slots[side]);
            let next = vertex.incident_head;
            vertex.incident_head = slot;
            self.live.at_slot_mut(slot).incident[side] = IncidentLinks {
                previous: NO_INCIDENT_EDGE,
                next,
            };
            if next != NO_INCIDENT_EDGE {
                self.incident_links_mut(next, vid).previous = slot;
            }
        }
    }

    /// Unlink only after retirement's fallible staging has succeeded, then
    /// repair IdMap's single swap-removal. No other edge's slot moves.
    fn remove_live_edge(&mut self, eid: EId) {
        let slot = self.live.dense_slot(&eid).expect("preflighted live edge");
        if self.incident_indexed {
            let edge = self.live.at_slot(slot).1;
            let endpoints = [edge.src, edge.dst];
            let incident = edge.incident;
            for (side, vid) in endpoints.into_iter().enumerate() {
                if side == 1 && vid == endpoints[0] {
                    continue;
                }
                let links = incident[side];
                if links.previous == NO_INCIDENT_EDGE {
                    self.live_vertices
                        .get_mut(&vid)
                        .expect("indexed endpoint")
                        .incident_head = links.next;
                } else {
                    self.incident_links_mut(links.previous, vid).next = links.next;
                }
                if links.next != NO_INCIDENT_EDGE {
                    self.incident_links_mut(links.next, vid).previous = links.previous;
                }
            }
        }
        let removed = self.live.remove(&eid);
        debug_assert!(removed.is_some(), "preflighted live edge disappeared");
        if self.incident_indexed && (slot as usize) < self.live.len() {
            let moved = self.live.at_slot(slot).1;
            let endpoints = [moved.src, moved.dst];
            let incident = moved.incident;
            for (side, vid) in endpoints.into_iter().enumerate() {
                if side == 1 && vid == endpoints[0] {
                    continue;
                }
                let links = incident[side];
                if links.previous == NO_INCIDENT_EDGE {
                    self.live_vertices
                        .get_mut(&vid)
                        .expect("indexed endpoint")
                        .incident_head = slot;
                } else {
                    self.incident_links_mut(links.previous, vid).next = slot;
                }
                if links.next != NO_INCIDENT_EDGE {
                    self.incident_links_mut(links.next, vid).previous = slot;
                }
            }
        }
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
                let endpoint_slots =
                    self.incident_endpoint_slots(*src, *dst)
                        .map_err(|endpoint| WriteError::DanglingEndpoint {
                            eid: *eid,
                            endpoint,
                        })?;
                self.push(keys, entry, props.clone())?;
                self.live.insert(
                    *eid,
                    LiveEdge {
                        src: *src,
                        relation: *relation,
                        dst: *dst,
                        created_at: seq,
                        props: props.clone(),
                        incident: [IncidentLinks::EMPTY; 2],
                    },
                );
                // Fresh IdMap insertion appends, and its own checked cap
                // proves this slot fits u32 without truncating an identity.
                self.link_incident_edge((self.live.len() - 1) as u32, endpoint_slots);
                let was_fresh = Arc::make_mut(&mut self.spent).insert(*eid);
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
                    .map(|live| live.row.clone())
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
                let mut deferred_same_commit = BTreeSet::new();
                for eid in sorted_retired_incident_edges {
                    match self.retirement_entry(*eid, seq)? {
                        Some((entry, _)) => {
                            let key =
                                (entry.src, entry.relation, entry.dst, *eid, entry.created_at);
                            incoming += usize::from(!self.pending.contains_key(&key));
                        }
                        None => {
                            let edge = self
                                .live
                                .get(eid)
                                .expect("retirement preflight proved the edge live");
                            deferred_same_commit.insert((
                                edge.src,
                                edge.relation,
                                edge.dst,
                                *eid,
                                edge.created_at,
                            ));
                        }
                    }
                }
                // Model the set that remains AFTER same-commit incident
                // creations fold away. They cannot be sealed (that would
                // freeze a ghost live row), but they also must not suppress
                // a pre-seal needed by the old-edge tombstones that follow.
                let remaining_pending = self
                    .pending
                    .keys()
                    .filter(|key| !deferred_same_commit.contains(key))
                    .count();
                let remaining_has_live_at = self.pending.iter().any(|(key, pending)| {
                    !deferred_same_commit.contains(key)
                        && pending.entry.created_at == seq
                        && pending.entry.retired_at.is_none()
                });
                let need_edge_seal = remaining_pending + incoming
                    > usize::try_from(MAX_BLOCK_ENTRIES).unwrap_or(usize::MAX)
                    && !remaining_has_live_at;
                // DeleteVertex is the only apply arm that may pre-seal BOTH
                // maps. Committing the vertex seal and then refusing the
                // edge seal used to leave pending vertices durable while
                // apply returned a no-op-shaped error.
                if need_vertex_seal && need_edge_seal {
                    let vertex_sealed = self.encode_pending_vertices(keys)?;
                    let (edge_sealed, advanced) =
                        self.encode_pending_edges_except(keys, &deferred_same_commit)?;
                    self.commit_vertex_seal(vertex_sealed);
                    self.commit_edge_seal_except(edge_sealed, advanced, &deferred_same_commit);
                } else if need_vertex_seal {
                    self.seal_vertices(keys)?;
                } else if need_edge_seal {
                    let (edge_sealed, advanced) =
                        self.encode_pending_edges_except(keys, &deferred_same_commit)?;
                    self.commit_edge_seal_except(edge_sealed, advanced, &deferred_same_commit);
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
                self.live_vertices.insert(*vid, LiveVertex::new(row));
                let was_fresh = Arc::make_mut(&mut self.spent_vertices).insert(*vid);
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
        if self.last_seq != Some(seq) {
            // Do this only after the entire row succeeds. In particular, a
            // refused next-sequence row must not erase the guards protecting
            // a subsequent same-sequence delete of an already sealed birth.
            // Any early seal inside this first advancing row only encountered
            // older births, so none of its guards can be needed at `seq`.
            self.sealed_live_edges.clear();
            self.sealed_live_vertices.clear();
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
            self.remove_live_edge(eid);
            return Ok(());
        };
        self.push(keys, entry, props)?;
        self.remove_live_edge(eid);
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
        self.encode_pending_edges_except(keys, &BTreeSet::new())
    }

    /// Encode every pending edge except same-commit creations that the
    /// current `DeleteVertex` cascade will fold away. Those rows must remain
    /// mutable until retirement; sealing them would make an empty interval
    /// durable as a live ghost.
    fn encode_pending_edges_except(
        &self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        deferred: &BTreeSet<EdgeStatementKey>,
    ) -> Result<EncodedEdgeSeal, WriteError> {
        let mut by_descriptor: BTreeMap<(VId, RelationId), Vec<PendingStatement>> = BTreeMap::new();
        for (key, statement) in &self.pending {
            if deferred.contains(key) {
                continue;
            }
            by_descriptor
                .entry((statement.entry.src, statement.entry.relation))
                .or_default()
                .push(statement.clone());
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
            // Cut a family at the entry-count, hosted-patch row, or stored
            // byte ceiling, whichever binds first — the same limits
            // `compact::pack_retained` uses. Early-seal may leave
            // more than MAX_BLOCK_ENTRIES pending so a later same-seq
            // delete can still fold away (fgdb-wlxe / fgdb-otcw).
            let entry_ceiling = usize::try_from(MAX_BLOCK_ENTRIES).unwrap_or(usize::MAX);
            let property_ceiling = usize::try_from(MAX_PROPERTY_PATCH_ROWS).unwrap_or(usize::MAX);
            let mut chunk: Vec<PendingStatement> = Vec::new();
            let mut chunk_propertied = 0usize;
            let mut patch_bytes = PROPERTY_PATCH_HEADER_BYTES;
            let mut chunks: Vec<Vec<PendingStatement>> = Vec::new();
            for statement in statements {
                let propertied = usize::from(!statement.props.is_empty());
                let row_bytes =
                    admitted_row_bytes(&statement.props).map_err(WriteError::EdgeProps)?;
                if !chunk.is_empty()
                    && (chunk.len() == entry_ceiling
                        || chunk_propertied + propertied > property_ceiling
                        || patch_bytes + row_bytes > crate::store::MAX_STORED_OBJECT_BYTES)
                {
                    chunks.push(std::mem::take(&mut chunk));
                    chunk_propertied = 0;
                    patch_bytes = PROPERTY_PATCH_HEADER_BYTES;
                }
                chunk_propertied += propertied;
                patch_bytes += row_bytes;
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
        // links still advance, but only in a local overlay of the families
        // this seal touches, so a later chunk's refusal leaves `chain_heads`
        // and `pending` exactly as they were (`seal_vertices` already does
        // this; the public apply contract is that every typed refusal is a
        // no-op). The overlay, not a copy of every head ever sealed, keeps a
        // commit's seal from growing with the partition's history.
        let mut advanced: BTreeMap<(VId, RelationId), DeltaBlockVersion> = BTreeMap::new();
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
            let predecessor = advanced
                .get(&family)
                .or_else(|| self.chain_heads.get(&family))
                .copied();
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
                        bytes: patch_bytes.into(),
                    }),
                )
            };
            let sealed_id = block_id(keys.0, keys.1, &bytes);
            // The family's chain advances to this block; the next chunk of
            // the same family — even within THIS seal — links to it.
            advanced.insert(family, DeltaBlockVersion(sealed_id));
            sealed.push(SealedBlock {
                block_id: sealed_id,
                bytes: bytes.into(),
                first_seq,
                last_seq,
                property_patch,
            });
        }
        Ok((sealed, advanced))
    }

    fn commit_edge_seal(
        &mut self,
        sealed: Vec<SealedBlock>,
        advanced: BTreeMap<(VId, RelationId), DeltaBlockVersion>,
    ) {
        self.commit_edge_seal_except(sealed, advanced, &BTreeSet::new());
    }

    /// Commit an edge seal while retaining same-commit creations that the
    /// active vertex-delete cascade is about to fold away.
    fn commit_edge_seal_except(
        &mut self,
        sealed: Vec<SealedBlock>,
        advanced: BTreeMap<(VId, RelationId), DeltaBlockVersion>,
        deferred: &BTreeSet<EdgeStatementKey>,
    ) {
        for (key, statement) in &self.pending {
            if !deferred.contains(key)
                && statement.entry.retired_at.is_none()
                && Some(statement.entry.created_at) == self.last_seq
            {
                self.sealed_live_edges
                    .insert((statement.entry.eid, statement.entry.created_at));
            }
        }
        self.chain_heads.extend(advanced);
        self.pending.retain(|key, _| deferred.contains(key));
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
        let (sealed, advanced) = self.encode_pending_edges(keys)?;
        let first = sealed.first().cloned();
        self.commit_edge_seal(sealed, advanced);
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
        value: Option<CanonicalPropertyValue>,
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
                if let Some(live) = self.live.get_mut(&eid) {
                    live.props = successor_row;
                }
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
                    incident: current.incident,
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
                incident: current.incident,
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
            .map(|live| live.row.clone())
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
                self.live_vertices.get_mut(&vid).expect("live vertex").row = successor;
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
            self.live_vertices.get_mut(&vid).expect("live vertex").row = successor;
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
        self.live_vertices.get_mut(&vid).expect("live vertex").row = successor;
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
        let mut staged = crate::vertex::pack_rows(rows).map_err(WriteError::Patch)?;
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
                bytes: bytes.into(),
                first_seq,
                last_seq,
            });
        }
        Ok(sealed)
    }

    fn commit_vertex_seal(&mut self, sealed: Vec<SealedPatch>) {
        for row in self.pending_vertices.values() {
            if row.retired_at.is_none() && Some(row.created_at) == self.last_seq {
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
        let root = self.publish_in_place(keys, published_at)?;
        Ok((root, self.sealed, self.sealed_patches))
    }

    /// [`Self::publish`] for a retained writer: seal whatever remains and
    /// return the root, leaving the blocks and patches readable through
    /// [`Self::sealed`] and [`Self::sealed_patches`]. Both only ever append,
    /// so a caller can tell this publication's new objects from their
    /// positions. A failed seal leaves the staged rows exactly as they were.
    pub fn publish_in_place(
        &mut self,
        keys: (&[u8; 32], DatabaseSecurityNamespaceId),
        published_at: CommitSeq,
    ) -> Result<PartitionRoot, WriteError> {
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
        Ok(root)
    }
}

/// The map that inserting `pairs` in order builds, a later pair replacing an
/// earlier one with an equal key, built in one bulk pass. A writable open
/// assembles its writer's maps from millions of pairs; one insert each, in the
/// random key order a keyed EId permutation gives, was most of that assembly
/// on a packed web-google (fgdb-e8gr4).
fn last_wins<K: Ord, V>(mut pairs: Vec<(K, V)>) -> BTreeMap<K, V> {
    // Stable: pairs with equal keys keep their order, so the last one wins.
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut unique: Vec<(K, V)> = Vec::with_capacity(pairs.len());
    for pair in pairs {
        match unique.last_mut() {
            Some(last) if last.0 == pair.0 => *last = pair,
            _ => unique.push(pair),
        }
    }
    unique.into_iter().collect()
}

#[cfg(test)]
mod last_wins_tests {
    use super::last_wins;
    use std::collections::BTreeMap;

    #[test]
    fn last_wins_builds_the_map_that_inserting_in_order_builds() {
        let mut state = 0x001a_570f_u64;
        let mut next = move |bound: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % bound
        };
        for trial in 0..2_000 {
            let len = next(64) as usize;
            let keys = 1 + next(40);
            let pairs: Vec<(u64, u64)> = (0..len).map(|at| (next(keys), at as u64)).collect();
            let mut inserted = BTreeMap::new();
            for &(key, value) in &pairs {
                inserted.insert(key, value);
            }
            assert_eq!(last_wins(pairs), inserted, "trial {trial}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockWriter, NO_INCIDENT_EDGE, PendingStatement, WriteError};
    use crate::vertex::{MAX_PATCH_ROWS, VertexPatchError, VertexRow};
    use crate::{AdjacencyEntry, BlockError, MAX_BLOCK_ENTRIES};
    use fgdb_delta_types::{DeltaRow, ElementId, PropertyKeyId, RelationId};
    use fgdb_types::ids::DatabaseSecurityNamespaceId;
    use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, VId};

    const K_OID: [u8; 32] = [0x5a; 32];

    fn keys() -> (&'static [u8; 32], DatabaseSecurityNamespaceId) {
        (&K_OID, DatabaseSecurityNamespaceId([0x77; 32]))
    }

    thread_local! {
        static INCIDENT_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    pub(super) fn record_incident_visit() {
        INCIDENT_VISITS.with(|count| count.set(count.get() + 1));
    }

    fn incident_vertex(vid: VId, ordinal: u64) -> DeltaRow {
        DeltaRow::CreateVertex {
            vid,
            birth_ordinal: ordinal,
            labels: vec![],
            props: vec![],
            valid_time: None,
        }
    }

    fn incident_edge(eid: EId, src: VId, dst: VId) -> DeltaRow {
        DeltaRow::CreateEdge {
            eid,
            birth_ordinal: eid.0 as u64,
            src,
            relation: RelationId((eid.0 % 3) as u64),
            dst,
            canonical_key: None,
            props: vec![],
            valid_time: None,
        }
    }

    #[test]
    fn sealed_fold_guards_are_bounded_by_current_sequence_not_statement_history() {
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        for vid in [VId(1), VId(2)] {
            writer
                .apply(keys(), CommitSeq(1), &incident_vertex(vid, vid.0 as u64))
                .expect("vertex");
        }
        writer
            .apply(
                keys(),
                CommitSeq(1),
                &incident_edge(EId(10), VId(1), VId(2)),
            )
            .expect("edge");
        writer.seal(keys()).expect("initial edge seal");
        writer.seal_vertices(keys()).expect("initial vertex seal");
        let original = writer.clone();

        for seq in 2..=128 {
            for elem in [ElementId::Vertex(VId(1)), ElementId::Edge(EId(10))] {
                writer
                    .apply(
                        keys(),
                        CommitSeq(seq),
                        &DeltaRow::Property {
                            elem,
                            property: PropertyKeyId(7),
                            before: None,
                            after: Some(CanonicalScalar::Int(seq as i64).into()),
                        },
                    )
                    .expect("content successor");
            }
            assert!(writer.sealed_live_edges.is_empty());
            assert!(writer.sealed_live_vertices.is_empty());
            writer.seal(keys()).expect("edge seal");
            writer.seal_vertices(keys()).expect("vertex seal");
            assert_eq!(writer.sealed_live_edges.len(), 1);
            assert_eq!(writer.sealed_live_vertices.len(), 1);
            assert!(
                writer
                    .sealed_live_edges
                    .contains(&(EId(10), CommitSeq(seq)))
            );
            assert!(
                writer
                    .sealed_live_vertices
                    .contains(&(VId(1), CommitSeq(seq)))
            );
        }
        // Advancing the clone cannot clear the old generation's guards.
        assert_eq!(original.sealed_live_edges.len(), 1);
        assert_eq!(original.sealed_live_vertices.len(), 2);
        assert_eq!(original.last_seq, Some(CommitSeq(1)));

        let mut blocks = Vec::new();
        let mut props = Vec::new();
        for block in writer.sealed() {
            let (entries, hosted) = crate::decode_block_with_properties(&block.bytes).unwrap();
            blocks.push(entries);
            props.push(hosted.map(|(_, locators)| {
                crate::edge_props::BlockProps {
                    locators,
                    rows: crate::edge_props::decode_property_patch(
                        &block.property_patch.as_ref().unwrap().bytes,
                    )
                    .unwrap(),
                }
            }));
        }
        let patches: Vec<_> = writer
            .sealed_patches()
            .iter()
            .map(|patch| crate::vertex::decode_patch(&patch.bytes).unwrap())
            .collect();
        // The retired guard history is not durable history: every old cut
        // must still recover the content visible when that commit sealed.
        for seq in 1..=128 {
            let expected = if seq == 1 {
                vec![]
            } else {
                vec![(PropertyKeyId(7), CanonicalScalar::Int(seq as i64).into())]
            };
            let vertex = crate::vertex::merge_vertex(&patches, VId(1), CommitSeq(seq)).unwrap();
            let (_, edge_props) =
                crate::root::merge_edge_with_props(&blocks, &props, EId(10), CommitSeq(seq))
                    .unwrap()
                    .unwrap();
            assert_eq!(vertex.props, expected);
            assert_eq!(edge_props, expected);
        }
    }

    #[test]
    fn refused_sequence_advances_preserve_sealed_same_sequence_delete_guards() {
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        for vid in [VId(1), VId(2)] {
            writer
                .apply(keys(), CommitSeq(7), &incident_vertex(vid, vid.0 as u64))
                .unwrap();
        }
        writer
            .apply(
                keys(),
                CommitSeq(7),
                &incident_edge(EId(10), VId(2), VId(2)),
            )
            .unwrap();
        writer.seal(keys()).unwrap();
        writer.seal_vertices(keys()).unwrap();
        for seq in [8, 100, 6] {
            let before = format!("{writer:?}");
            assert!(
                writer
                    .apply(keys(), CommitSeq(seq), &incident_vertex(VId(1), 1))
                    .is_err()
            );
            assert_eq!(format!("{writer:?}"), before, "refused sequence {seq}");
        }
        for elem in [ElementId::Vertex(VId(1)), ElementId::Edge(EId(10))] {
            writer
                .apply(
                    keys(),
                    CommitSeq(7),
                    &DeltaRow::Property {
                        elem,
                        property: PropertyKeyId(7),
                        before: None,
                        after: Some(CanonicalScalar::Int(9).into()),
                    },
                )
                .expect("same-sequence restatement");
        }
        let before = format!("{writer:?}");
        let seq = CommitSeq(7);
        assert_eq!(
            writer.apply(
                keys(),
                seq,
                &DeltaRow::DeleteEdge {
                    eid: EId(10),
                    before_version: fgdb_types::ObjectId([0; 32]),
                },
            ),
            Err(WriteError::Block(BlockError::RetiredBeforeCreated {
                at: 0,
                created_at: seq,
                retired_at: seq,
            }))
        );
        assert_eq!(format!("{writer:?}"), before);
        assert_eq!(
            writer.apply(
                keys(),
                seq,
                &DeltaRow::DeleteVertex {
                    vid: VId(1),
                    before_version: fgdb_types::ObjectId([0; 32]),
                    sorted_retired_incident_edges: vec![],
                },
            ),
            Err(WriteError::Patch(VertexPatchError::RetiredBeforeCreated {
                at: 0,
                created_at: seq,
                retired_at: seq,
            }))
        );
        assert_eq!(format!("{writer:?}"), before);
    }

    #[test]
    fn reconstructed_writers_preserve_current_frontier_sealed_birth_guards() {
        let mut original = BlockWriter::new(GraphId(1), BranchId(1), 0);
        for vid in [VId(1), VId(2)] {
            original
                .apply(keys(), CommitSeq(1), &incident_vertex(vid, vid.0 as u64))
                .unwrap();
        }
        original
            .apply(
                keys(),
                CommitSeq(1),
                &incident_edge(EId(10), VId(2), VId(2)),
            )
            .unwrap();
        original.seal(keys()).unwrap();
        original.seal_vertices(keys()).unwrap();
        let update = |elem| DeltaRow::Property {
            elem,
            property: PropertyKeyId(7),
            before: None,
            after: Some(CanonicalScalar::Int(9).into()),
        };
        let seq = CommitSeq(7);
        for elem in [ElementId::Vertex(VId(1)), ElementId::Edge(EId(10))] {
            original.apply(keys(), seq, &update(elem)).unwrap();
        }
        original.seal(keys()).unwrap();
        original.seal_vertices(keys()).unwrap();
        let mut blocks = Vec::new();
        let mut props = Vec::new();
        for block in original.sealed() {
            let (entries, hosted) = crate::decode_block_with_properties(&block.bytes).unwrap();
            blocks.push(entries);
            props.push(hosted.map(|(_, locators)| {
                crate::edge_props::BlockProps {
                    locators,
                    rows: crate::edge_props::decode_property_patch(
                        &block.property_patch.as_ref().unwrap().bytes,
                    )
                    .unwrap(),
                }
            }));
        }
        let patches: Vec<_> = original
            .sealed_patches()
            .iter()
            .map(|patch| crate::vertex::decode_patch(&patch.bytes).unwrap())
            .collect();
        let mut admission = crate::root::EdgeHistoryValidator::default();
        for (index, block) in blocks.iter().enumerate() {
            admission.observe_block(index, block).unwrap();
        }
        let revalidated = BlockWriter::from_published_partition(
            GraphId(1),
            BranchId(1),
            0,
            original.sealed().to_vec(),
            original.sealed_patches().to_vec(),
            &blocks,
            &props,
            &patches,
            seq,
        )
        .unwrap();
        let admitted = BlockWriter::from_admitted_partition(
            GraphId(1),
            BranchId(1),
            0,
            original.sealed().to_vec(),
            original.sealed_patches().to_vec(),
            &admission.statements(),
            &blocks,
            &props,
            &patches,
            seq,
        )
        .unwrap();
        for mut writer in [revalidated, admitted] {
            for elem in [ElementId::Vertex(VId(1)), ElementId::Edge(EId(10))] {
                writer.apply(keys(), seq, &update(elem)).unwrap();
            }
            let before = format!("{writer:?}");
            let edge_delete = DeltaRow::DeleteEdge {
                eid: EId(10),
                before_version: fgdb_types::ObjectId([0; 32]),
            };
            let vertex_delete = DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: fgdb_types::ObjectId([0; 32]),
                sorted_retired_incident_edges: vec![],
            };
            assert_eq!(
                writer.apply(keys(), seq, &edge_delete),
                Err(WriteError::Block(BlockError::RetiredBeforeCreated {
                    at: 0,
                    created_at: seq,
                    retired_at: seq,
                }))
            );
            assert_eq!(format!("{writer:?}"), before);
            assert_eq!(
                writer.apply(keys(), seq, &vertex_delete),
                Err(WriteError::Patch(VertexPatchError::RetiredBeforeCreated {
                    at: 0,
                    created_at: seq,
                    retired_at: seq,
                }))
            );
            assert_eq!(format!("{writer:?}"), before);
            assert_eq!(writer.sealed_live_edges.len(), 1);
            assert_eq!(writer.sealed_live_vertices.len(), 1);
            assert!(
                !writer
                    .sealed_live_vertices
                    .contains(&(VId(2), CommitSeq(1)))
            );
            writer.apply(keys(), CommitSeq(8), &edge_delete).unwrap();
            writer.apply(keys(), CommitSeq(8), &vertex_delete).unwrap();
            assert!(writer.sealed_live_edges.is_empty());
            assert!(writer.sealed_live_vertices.is_empty());
            writer.publish(keys(), CommitSeq(8)).unwrap();
        }
    }

    #[test]
    fn delayed_and_early_seals_do_not_retain_previous_sequence_guards() {
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        for vid in [VId(1), VId(2)] {
            writer
                .apply(keys(), CommitSeq(1), &incident_vertex(vid, vid.0 as u64))
                .unwrap();
        }
        for eid in 1..=u128::from(MAX_BLOCK_ENTRIES) {
            writer
                .apply(
                    keys(),
                    CommitSeq(1),
                    &incident_edge(EId(eid), VId(1), VId(2)),
                )
                .unwrap();
        }
        assert!(
            writer.sealed().is_empty(),
            "same-sequence births stay pending"
        );
        let next = EId(u128::from(MAX_BLOCK_ENTRIES) + 1);
        writer
            .apply(keys(), CommitSeq(2), &incident_edge(next, VId(1), VId(2)))
            .unwrap();
        assert!(
            !writer.sealed().is_empty(),
            "advancing row early-seals old births"
        );
        assert!(writer.sealed_live_edges.is_empty());
        writer
            .apply(keys(), CommitSeq(2), &incident_vertex(VId(3), 3))
            .unwrap();
        writer.seal(keys()).unwrap();
        writer.seal_vertices(keys()).unwrap();
        assert_eq!(
            writer.sealed_live_edges.iter().copied().collect::<Vec<_>>(),
            vec![(next, CommitSeq(2))]
        );
        assert_eq!(
            writer
                .sealed_live_vertices
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![(VId(3), CommitSeq(2))]
        );
        // A later retirement of an old sealed birth closes a nonempty interval.
        writer
            .apply(
                keys(),
                CommitSeq(2),
                &DeltaRow::DeleteEdge {
                    eid: EId(1),
                    before_version: fgdb_types::ObjectId([0; 32]),
                },
            )
            .unwrap();
        assert!(writer.live_edge(EId(1)).is_none());
        writer.publish(keys(), CommitSeq(2)).unwrap();
    }

    // The previous production implementation, independent of the links.
    fn incident_scan(writer: &BlockWriter, vid: VId) -> Vec<EId> {
        let mut incident: Vec<EId> = writer
            .live
            .iter()
            .filter(|(_, edge)| edge.src == vid || edge.dst == vid)
            .map(|(eid, _)| *eid)
            .collect();
        incident.sort_unstable();
        incident
    }

    fn assert_incident_index(writer: &BlockWriter) {
        assert!(writer.incident_indexed);
        let mut memberships = 0;
        for (&vid, vertex) in writer.live_vertices.iter() {
            let mut slot = vertex.incident_head;
            let mut previous = NO_INCIDENT_EDGE;
            let mut held = std::collections::BTreeSet::new();
            while slot != NO_INCIDENT_EDGE {
                assert!((slot as usize) < writer.live.len(), "dangling slot");
                let (eid, edge) = writer.live.at_slot(slot);
                assert!(edge.src == vid || edge.dst == vid, "wrong endpoint");
                assert!(held.insert(*eid), "cycle or duplicate self-loop");
                let links = edge.incident[usize::from(edge.src != vid)];
                assert_eq!(links.previous, previous);
                previous = slot;
                slot = links.next;
            }
            memberships += held.len();
            let expected = incident_scan(writer, vid);
            assert_eq!(held.into_iter().collect::<Vec<_>>(), expected);
            assert_eq!(writer.live_incident_edges(vid), expected);
        }
        assert_eq!(
            memberships,
            writer
                .live
                .iter()
                .map(|(_, edge)| 1 + usize::from(edge.src != edge.dst))
                .sum::<usize>()
        );
    }

    #[test]
    fn incident_index_matches_scan_through_versions_clones_and_dense_moves() {
        let mut indexed = BlockWriter::new(GraphId(1), BranchId(1), 0);
        let mut vertices = vec![VId(0), VId(u128::MAX)];
        // Equal low halves deliberately cannot identify a vertex.
        vertices.extend((1..=67u128).map(|n| VId((n << 64) | 7)));
        for (ordinal, &vid) in vertices.iter().enumerate() {
            indexed
                .apply(keys(), CommitSeq(1), &incident_vertex(vid, ordinal as u64))
                .expect("vertex");
        }
        let mut scanned = indexed.clone();
        scanned.incident_indexed = false;
        let mut rng = 0x83d4_f137_a62e_19cbu64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for step in 0..660u64 {
            let seq = CommitSeq(2 + step / 11);
            let vid = vertices[next() as usize % vertices.len()];
            let choice = next() % 10;
            let row = if choice < 6 || indexed.live.is_empty() {
                let dst = if step % 5 == 0 {
                    vid
                } else {
                    vertices[next() as usize % vertices.len()]
                };
                // Distinct full-width edges may also share their low half.
                incident_edge(EId((u128::from(step + 1) << 64) | 9), vid, dst)
            } else if choice < 8 {
                let slot = (next() as usize % indexed.live.len()) as u32;
                DeltaRow::DeleteEdge {
                    eid: *indexed.live.at_slot(slot).0,
                    before_version: fgdb_types::ObjectId([0; 32]),
                }
            } else {
                let elem = if choice == 8 {
                    let slot = (next() as usize % indexed.live.len()) as u32;
                    ElementId::Edge(*indexed.live.at_slot(slot).0)
                } else {
                    ElementId::Vertex(vid)
                };
                DeltaRow::Property {
                    elem,
                    property: PropertyKeyId(step % 3),
                    before: None,
                    after: (step % 4 != 0).then(|| CanonicalScalar::Int(step as i64).into()),
                }
            };
            assert_eq!(
                indexed.apply(keys(), seq, &row),
                scanned.apply(keys(), seq, &row)
            );
            assert_incident_index(&indexed);
            for &vertex in &vertices {
                assert_eq!(
                    indexed.live_incident_edges(vertex),
                    scanned.live_incident_edges(vertex)
                );
                assert_eq!(
                    indexed.live_vertex_row(vertex),
                    scanned.live_vertex_row(vertex)
                );
            }
            for (&eid, _) in indexed.live.iter() {
                assert_eq!(
                    indexed.live_edge_statement(eid),
                    scanned.live_edge_statement(eid)
                );
            }
            if step % 29 == 0 && !indexed.live.is_empty() {
                let before = format!("{indexed:?}");
                let mut cloned = indexed.clone();
                let eid = *cloned.live.at_slot(0).0;
                cloned
                    .apply(
                        keys(),
                        seq,
                        &DeltaRow::DeleteEdge {
                            eid,
                            before_version: fgdb_types::ObjectId([0; 32]),
                        },
                    )
                    .expect("clone retires independently");
                assert_incident_index(&cloned);
                assert_eq!(format!("{indexed:?}"), before);
            }
            if step % 31 == 0 && !indexed.live.is_empty() {
                let edge = indexed.live.at_slot(0).1;
                let row = DeltaRow::DeleteVertex {
                    vid: edge.src,
                    before_version: fgdb_types::ObjectId([0; 32]),
                    sorted_retired_incident_edges: vec![],
                };
                let before = format!("{indexed:?}");
                let refused = indexed.apply(keys(), seq, &row);
                assert!(matches!(
                    refused,
                    Err(WriteError::CascadeImageMismatch { .. })
                ));
                assert_eq!(refused, scanned.apply(keys(), seq, &row));
                assert_eq!(
                    format!("{indexed:?}"),
                    before,
                    "typed refusal changed links"
                );
            }
            if step % 11 == 10 {
                indexed.seal(keys()).expect("edge seal");
                indexed.seal_vertices(keys()).expect("vertex seal");
                scanned.seal(keys()).expect("scan edge seal");
                scanned.seal_vertices(keys()).expect("scan vertex seal");
                assert_eq!(
                    indexed.clone().publish(keys(), seq),
                    scanned.clone().publish(keys(), seq),
                    "canonical bytes at step {step}"
                );
            }
        }
        // Ascending cascades remove arbitrary dense slots, including moved
        // heads, parallel incidences and both sides of a self-loop.
        for vid in vertices {
            let row = DeltaRow::DeleteVertex {
                vid,
                before_version: fgdb_types::ObjectId([0; 32]),
                sorted_retired_incident_edges: incident_scan(&indexed, vid),
            };
            assert_eq!(
                indexed.apply(keys(), CommitSeq(100), &row),
                scanned.apply(keys(), CommitSeq(100), &row)
            );
            assert_incident_index(&indexed);
        }
        assert!(indexed.live.is_empty());
        assert_eq!(
            indexed.publish(keys(), CommitSeq(100)),
            scanned.publish(keys(), CommitSeq(100))
        );
    }

    #[test]
    fn incident_index_rebuilds_from_both_constructors_and_keeps_incomplete_fallback() {
        let mut original = BlockWriter::new(GraphId(1), BranchId(1), 0);
        let vertices = [VId(0), VId(1 << 96), VId(u128::MAX)];
        for (ordinal, vid) in vertices.into_iter().enumerate() {
            original
                .apply(keys(), CommitSeq(1), &incident_vertex(vid, ordinal as u64))
                .expect("vertex");
        }
        for (eid, src, dst) in [
            (EId(0), vertices[0], vertices[1]),
            (EId(1 << 96), vertices[1], vertices[1]),
            (EId(u128::MAX), vertices[2], vertices[0]),
        ] {
            original
                .apply(keys(), CommitSeq(1), &incident_edge(eid, src, dst))
                .expect("edge");
        }
        original
            .publish_in_place(keys(), CommitSeq(1))
            .expect("publish");
        let blocks: Vec<_> = original
            .sealed()
            .iter()
            .map(|block| crate::decode_block(&block.bytes).expect("block"))
            .collect();
        let patches: Vec<_> = original
            .sealed_patches()
            .iter()
            .map(|patch| crate::vertex::decode_patch(&patch.bytes).expect("patch"))
            .collect();
        let props = vec![None; blocks.len()];
        let mut admission = crate::root::EdgeHistoryValidator::default();
        for (index, block) in blocks.iter().enumerate() {
            admission
                .observe_block(index, block)
                .expect("admitted block");
        }
        let recovered = BlockWriter::from_published_partition(
            GraphId(1),
            BranchId(1),
            0,
            original.sealed().to_vec(),
            original.sealed_patches().to_vec(),
            &blocks,
            &props,
            &patches,
            CommitSeq(1),
        )
        .expect("published writer");
        let admitted = BlockWriter::from_admitted_partition(
            GraphId(1),
            BranchId(1),
            0,
            original.sealed().to_vec(),
            original.sealed_patches().to_vec(),
            &admission.statements(),
            &blocks,
            &props,
            &patches,
            CommitSeq(1),
        )
        .expect("admitted writer");
        let mut incomplete = BlockWriter::from_published_partition(
            GraphId(1),
            BranchId(1),
            0,
            original.sealed().to_vec(),
            original.sealed_patches().to_vec(),
            &blocks,
            &props,
            &[],
            CommitSeq(1),
        )
        .expect("existing incomplete constructor semantics");
        assert!(!incomplete.incident_indexed);
        for vid in vertices {
            assert_eq!(
                incomplete.live_incident_edges(vid),
                incident_scan(&original, vid)
            );
        }
        incomplete
            .apply(
                keys(),
                CommitSeq(2),
                &DeltaRow::DeleteEdge {
                    eid: EId(0),
                    before_version: fgdb_types::ObjectId([0; 32]),
                },
            )
            .expect("incomplete fold still retires by EId");
        for vid in vertices {
            assert_eq!(
                incomplete.live_incident_edges(vid),
                incident_scan(&incomplete, vid)
            );
        }
        // The first two EIds have covered endpoints, but the last edge's
        // source is absent. Exercise abandoning a nonempty linked prefix,
        // not only the empty-patch refusal at the first edge above.
        let partial_rows: Vec<_> = patches
            .iter()
            .flatten()
            .filter(|row| row.vid != vertices[2])
            .cloned()
            .collect();
        let partial_patch = crate::vertex::decode_patch(
            &crate::vertex::encode_patch(&partial_rows).expect("partial patch"),
        )
        .expect("decoded partial patch");
        let mut partial = BlockWriter::from_published_partition(
            GraphId(1),
            BranchId(1),
            0,
            original.sealed().to_vec(),
            original.sealed_patches().to_vec(),
            &blocks,
            &props,
            &[partial_patch],
            CommitSeq(1),
        )
        .expect("partly covered constructor input");
        assert_eq!(*partial.live.at_slot(0).0, EId(0));
        assert_eq!(*partial.live.at_slot(2).0, EId(u128::MAX));
        assert!(
            partial
                .incident_endpoint_slots(vertices[0], vertices[1])
                .is_ok()
        );
        assert!(
            partial
                .incident_endpoint_slots(vertices[2], vertices[0])
                .is_err()
        );
        assert!(!partial.incident_indexed);
        assert!(
            partial
                .live
                .iter()
                .all(|(_, edge)| edge.incident == [super::IncidentLinks::EMPTY; 2])
        );
        assert!(
            partial
                .live_vertices
                .iter()
                .all(|(_, vertex)| vertex.incident_head == NO_INCIDENT_EDGE)
        );
        partial
            .apply(keys(), CommitSeq(2), &incident_vertex(vertices[2], 9))
            .expect("previously missing vertex can arrive in the fold");
        assert!(
            !partial.incident_indexed,
            "fallback never activates stale links"
        );
        for writer in [&partial, &partial.clone()] {
            for vid in vertices {
                assert_eq!(
                    writer.live_incident_edges(vid),
                    incident_scan(&original, vid)
                );
            }
        }
        let mut published = Vec::new();
        for mut writer in [original, recovered, admitted] {
            assert_incident_index(&writer);
            writer
                .apply(
                    keys(),
                    CommitSeq(2),
                    &DeltaRow::Property {
                        elem: ElementId::Edge(EId(u128::MAX)),
                        property: PropertyKeyId(7),
                        before: None,
                        after: Some(CanonicalScalar::Int(4).into()),
                    },
                )
                .expect("property successor");
            writer
                .apply(
                    keys(),
                    CommitSeq(2),
                    &DeltaRow::DeleteEdge {
                        eid: EId(0),
                        before_version: fgdb_types::ObjectId([0; 32]),
                    },
                )
                .expect("middle slot retirement");
            writer
                .apply(
                    keys(),
                    CommitSeq(2),
                    &DeltaRow::DeleteVertex {
                        vid: vertices[1],
                        before_version: fgdb_types::ObjectId([0; 32]),
                        sorted_retired_incident_edges: vec![EId(1 << 96)],
                    },
                )
                .expect("loop cascade");
            assert_incident_index(&writer);
            published.push(
                writer
                    .publish(keys(), CommitSeq(2))
                    .expect("published continuation"),
            );
        }
        assert_eq!(published[0], published[1]);
        assert_eq!(published[0], published[2]);
    }

    #[test]
    fn incident_lookup_visits_only_the_requested_vertex_degree() {
        for unrelated in [0..0, 2..2050] {
            let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
            for vid in [VId(0), VId(1)] {
                writer
                    .apply(keys(), CommitSeq(1), &incident_vertex(vid, vid.0 as u64))
                    .expect("vertex");
            }
            for eid in 0..3u128 {
                writer
                    .apply(
                        keys(),
                        CommitSeq(1),
                        &incident_edge(EId(eid), VId(0), VId(eid % 2)),
                    )
                    .expect("target incidence");
            }
            for n in unrelated {
                writer
                    .apply(keys(), CommitSeq(1), &incident_vertex(VId(n), n as u64))
                    .expect("unrelated vertex");
                writer
                    .apply(
                        keys(),
                        CommitSeq(1),
                        &incident_edge(EId(n + 10), VId(n), VId(n)),
                    )
                    .expect("unrelated loop");
            }
            INCIDENT_VISITS.with(|count| count.set(0));
            assert_eq!(
                writer.live_incident_edges(VId(0)),
                vec![EId(0), EId(1), EId(2)]
            );
            assert_eq!(INCIDENT_VISITS.with(std::cell::Cell::get), 3);
            INCIDENT_VISITS.with(|count| count.set(0));
            assert!(writer.live_incident_edges(VId(u128::MAX)).is_empty());
            assert_eq!(INCIDENT_VISITS.with(std::cell::Cell::get), 0);
        }
        let old_edge = std::mem::size_of::<(
            VId,
            RelationId,
            VId,
            CommitSeq,
            crate::edge_props::EdgePropertyRow,
        )>();
        let edge_growth = std::mem::size_of::<super::LiveEdge>() - old_edge;
        let vertex_growth =
            std::mem::size_of::<super::LiveVertex>() - std::mem::size_of::<VertexRow>();
        assert_eq!(edge_growth, 16, "two fixed pairs of u32 slots");
        assert!(vertex_growth <= std::mem::align_of::<VertexRow>());
        eprintln!(
            "incident_index_metadata edge_before={old_edge} edge_after={} vertex_before={} vertex_after={} extra_allocated_record_bytes=16*edge_capacity+{vertex_growth}*vertex_capacity no_secondary_allocations=true",
            std::mem::size_of::<super::LiveEdge>(),
            std::mem::size_of::<VertexRow>(),
            std::mem::size_of::<super::LiveVertex>()
        );
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
                after: Some(CanonicalScalar::Int(4).into()),
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

    /// A same-commit incident edge suppresses the ordinary pre-seal because
    /// sealing it would freeze a live row that must fold away. If that edge
    /// sorts before older incident edges, however, retiring it removes the
    /// suppression before the rest of the cascade is staged. A later old-edge
    /// tombstone can then reach the ceiling and refuse a seal after earlier
    /// members have already retired. The mixed cascade must be just as atomic
    /// as an all-old cascade.
    #[test]
    fn a_failed_mixed_cascade_seal_does_not_partially_retire() {
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
        for eid in [10_u128, 11, 12] {
            writer
                .apply(
                    keys(),
                    CommitSeq(1),
                    &DeltaRow::CreateEdge {
                        eid: EId(eid),
                        birth_ordinal: u64::try_from(eid).expect("test eid fits"),
                        src: VId(1),
                        relation: rel,
                        dst: VId(2),
                        canonical_key: None,
                        props: vec![],
                        valid_time: None,
                    },
                )
                .expect("old edge");
        }
        writer.seal(keys()).expect("old edge births durable");
        writer.seal_vertices(keys()).expect("vertex births durable");

        writer
            .apply(
                keys(),
                CommitSeq(2),
                &DeltaRow::CreateEdge {
                    eid: EId(1),
                    birth_ordinal: 4,
                    src: VId(1),
                    relation: rel,
                    dst: VId(2),
                    canonical_key: None,
                    props: vec![],
                    valid_time: None,
                },
            )
            .expect("same-commit edge");

        let ceiling = usize::try_from(MAX_BLOCK_ENTRIES).unwrap();
        for i in 0..(ceiling - 2) {
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

        let seq = CommitSeq(2);
        assert!(
            writer.pending_has_live_at(seq),
            "the same-commit edge must exercise the old pre-seal suppression predicate"
        );
        let old_incoming = [EId(10), EId(11), EId(12)]
            .into_iter()
            .map(|eid| {
                let (entry, _) = writer
                    .retirement_entry(eid, seq)
                    .expect("old edge retirement preflights")
                    .expect("old edge requires a tombstone");
                let key = (entry.src, entry.relation, entry.dst, eid, entry.created_at);
                usize::from(!writer.pending.contains_key(&key))
            })
            .sum::<usize>();
        assert_eq!(old_incoming, 3, "all old edges add pending tombstones");
        assert!(
            writer.pending_len() + old_incoming > ceiling,
            "the old edge tombstones must require a pre-seal once the same-commit row folds"
        );
        assert!(
            writer
                .retirement_entry(EId(1), seq)
                .expect("same-commit retirement preflights")
                .is_none(),
            "the same-commit edge must fold instead of adding a tombstone"
        );
        let before = writer.clone();

        let refusal = writer.apply(
            keys(),
            seq,
            &DeltaRow::DeleteVertex {
                vid: VId(2),
                before_version: fgdb_types::ids::ObjectId([0u8; 32]),
                sorted_retired_incident_edges: vec![EId(1), EId(10), EId(11), EId(12)],
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
        for eid in [1_u128, 10, 11, 12] {
            assert!(
                writer.live_edge(EId(eid)).is_some(),
                "the refused mixed cascade must leave E{eid} live"
            );
        }
        assert_eq!(
            writer.pending_len(),
            ceiling - 1,
            "the refused mixed cascade must restore the same-commit creation"
        );
        assert_eq!(writer.graph, before.graph, "graph identity changed");
        assert_eq!(writer.branch, before.branch, "branch identity changed");
        assert_eq!(writer.partition, before.partition, "partition changed");
        assert_eq!(writer.pending, before.pending, "pending edge bytes changed");
        assert_eq!(writer.live, before.live, "live edge state changed");
        assert_eq!(writer.spent, before.spent, "spent edge state changed");
        assert_eq!(writer.sealed, before.sealed, "sealed edge bytes changed");
        assert_eq!(
            writer.chain_heads, before.chain_heads,
            "edge chain heads changed"
        );
        assert_eq!(
            writer.pending_vertices, before.pending_vertices,
            "pending vertex bytes changed"
        );
        assert_eq!(
            writer.live_vertices, before.live_vertices,
            "live vertex state changed"
        );
        assert_eq!(
            writer.spent_vertices, before.spent_vertices,
            "spent vertex state changed"
        );
        assert_eq!(
            writer.sealed_patches, before.sealed_patches,
            "sealed vertex bytes changed"
        );
        assert_eq!(
            writer.sealed_live_edges, before.sealed_live_edges,
            "sealed-live edge state changed"
        );
        assert_eq!(
            writer.sealed_live_vertices, before.sealed_live_vertices,
            "sealed-live vertex state changed"
        );
        assert_eq!(writer.last_seq, before.last_seq, "sequence state changed");
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

    /// fgdb-e8gr4: a writer built from a root walk's ADMITTED edge history
    /// equals one that re-collapses every published block. The walk is an
    /// EdgeHistoryValidator observing each block in publication order, as
    /// `reopen_sealed` does. The history mixes two families, a retirement, a
    /// content successor, a vertex-delete cascade and a later creation. Both
    /// paths share property rows and vertex patches verbatim, so the law
    /// isolates the one input that differs: where the collapse comes from.
    #[test]
    fn a_writer_from_the_admitted_history_equals_a_revalidated_one() {
        let (graph, branch) = (GraphId(1), BranchId(1));
        let vertex = |vid: u128, ordinal| DeltaRow::CreateVertex {
            vid: VId(vid),
            birth_ordinal: ordinal,
            labels: vec![],
            props: vec![],
            valid_time: None,
        };
        let edge = |eid: u128, src: u128, relation: u64, dst: u128, ordinal| DeltaRow::CreateEdge {
            eid: EId(eid),
            birth_ordinal: ordinal,
            src: VId(src),
            relation: RelationId(relation),
            dst: VId(dst),
            canonical_key: None,
            props: vec![],
            valid_time: None,
        };
        let unversioned = fgdb_types::ids::ObjectId([0; 32]);
        let commits = [
            vec![vertex(1, 0), vertex(2, 1), vertex(3, 2)],
            vec![
                edge(10, 1, 1, 2, 3),
                edge(11, 2, 1, 3, 4),
                edge(12, 1, 2, 3, 5),
            ],
            vec![DeltaRow::DeleteEdge {
                eid: EId(11),
                before_version: unversioned,
            }],
            vec![DeltaRow::Property {
                elem: ElementId::Edge(EId(10)),
                property: PropertyKeyId(7),
                before: None,
                after: Some(CanonicalScalar::Int(4).into()),
            }],
            vec![DeltaRow::DeleteVertex {
                vid: VId(3),
                before_version: unversioned,
                sorted_retired_incident_edges: vec![EId(12)],
            }],
            vec![edge(13, 2, 1, 1, 6)],
        ];
        let mut writer = BlockWriter::new(graph, branch, 0);
        for (index, rows) in commits.iter().enumerate() {
            let at = CommitSeq(index as u64 + 1);
            for row in rows {
                writer.apply(keys(), at, row).expect("legal history folds");
            }
            writer.seal(keys()).expect("per-commit edge seal");
            writer
                .seal_vertices(keys())
                .expect("per-commit vertex seal");
        }
        let frontier = CommitSeq(commits.len() as u64);
        let (_, sealed, sealed_patches) = writer.publish(keys(), frontier).expect("publishes");
        let blocks: Vec<Vec<AdjacencyEntry>> = sealed
            .iter()
            .map(|block| crate::decode_block(&block.bytes).expect("decodes"))
            .collect();
        let block_props: Vec<Option<crate::edge_props::BlockProps>> =
            blocks.iter().map(|_| None).collect();
        let walk = |count: usize| {
            let mut validator = crate::root::EdgeHistoryValidator::default();
            for (at, block) in blocks.iter().enumerate().take(count) {
                validator.observe_block(at, block).expect("lawful history");
            }
            validator
        };
        let admitted = walk(blocks.len());
        // Retirements and the content successor restate earlier statements:
        // the collapse is not a copy of the entries.
        let entries: usize = blocks.iter().map(Vec::len).sum();
        assert!(admitted.statements().len() < entries, "{entries} entries");

        let revalidated = BlockWriter::from_published_partition(
            graph,
            branch,
            0,
            sealed.clone(),
            sealed_patches.clone(),
            &blocks,
            &block_props,
            &[],
            frontier,
        )
        .expect("revalidated writer");
        let from_walk = BlockWriter::from_admitted_partition(
            graph,
            branch,
            0,
            sealed.clone(),
            sealed_patches.clone(),
            &admitted.statements(),
            &blocks,
            &block_props,
            &[],
            frontier,
        )
        .expect("admitted writer");
        assert_eq!(format!("{from_walk:?}"), format!("{revalidated:?}"));

        // The comparison can fail: an admission that missed the last block
        // yields a different writer.
        let stale = BlockWriter::from_admitted_partition(
            graph,
            branch,
            0,
            sealed,
            sealed_patches,
            &walk(blocks.len() - 1).statements(),
            &blocks,
            &block_props,
            &[],
            frontier,
        )
        .expect("stale writer");
        assert_ne!(format!("{stale:?}"), format!("{revalidated:?}"));
    }
}
