//! A full scan's admitted records, kept for the statements that follow it on
//! the same snapshot (fgdb-61p1n).
//!
//! Under a capability that hides nothing, every whole-graph statement admits
//! the same records in the same order with the same live calls: they depend
//! only on the snapshot and the cut. The first such statement records where
//! each admitted record sits and every live call it made. A later one replays
//! those calls exactly (`tape.rs`), so its charges, refusal points and
//! cancellation ordinals are a fresh scan's, then refills its tables from the
//! coordinates instead of merging both histories again. A scoped capability,
//! a bound root, the transaction overlay and every metadata projection run as
//! before.
//!
//! The sidecar follows `query_prism/cache.rs`: a weak owner on the live
//! snapshot allocation, no eviction of live entries on admission, and live
//! cancellation checks while contended. Its database owns the allowance. It is
//! trusted-host state and is never handed to a token holder.

use super::tape::{Step, Tape};
use super::{Fault, SourceEvent, charge_edge, charge_vertex};
use crate::Snapshot;
use crate::query::prism::LiveCache;
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, EId, VId};
use std::mem::size_of;
use std::sync::Arc;

/// Retained coordinate bytes over all live snapshots. A graph too large for
/// this is admitted afresh by every statement, as before. roadnet-ca (2.0M
/// vertices, 5.5M edges) needs more than 128 MiB, and readmitting it made a
/// resident count of its edges 3.6x slower. 1 GiB keeps a pokec core (1.6M
/// vertices, 30.6M edges: ~300 MB of coordinates and ~370 MB of positioned
/// edges) beside its vertex-only core, on a resident server that holds
/// 15-50 GiB for such graphs.
pub(crate) const ADMITTED_BYTES: usize = 1024 * 1024 * 1024;

pub(crate) type AdmittedCache = LiveCache<Snapshot, AdmissionKey, Arc<AdmittedCore>>;

/// What a full scan's admitted set depends on under a scope that hides nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdmissionKey {
    pub(super) at: CommitSeq,
    /// Whether the plan reads edges: a vertex-only plan admits none.
    pub(super) edges: bool,
}

/// A core keeps its edges' identities inline beside their coordinates while
/// all of them fit this many bytes; a larger core reads each back from its
/// statement on refill. Read-back costs a lookup per edge (epinions edge
/// count +10%, the unlabelled one +25%), so it is only paid where inline
/// identities would not fit the budget at all (web-google's ~290 MB).
const INLINE_IDENTITY_BYTES: usize = 256 * 1024 * 1024;

/// Lets the admission laws (which run serialized) cover both refill forms.
#[cfg(test)]
pub(super) static INLINE_IDENTITIES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

fn inline_identity_bytes() -> usize {
    #[cfg(test)]
    if !INLINE_IDENTITIES.load(std::sync::atomic::Ordering::Relaxed) {
        return 0;
    }
    INLINE_IDENTITY_BYTES
}

/// An admitted edge's identity: (EId, src, relation, dst).
pub(super) type EdgeIdentity = (EId, VId, RelationId, VId);

/// A core keeps its edges positioned (each endpoint's position in the sealed
/// vertex table, its relation's in `relations`) while they fit this many
/// bytes, 12 per edge: a triples-only statement then reads them in place,
/// with per-vertex state in arrays, instead of copying or reading back every
/// edge.
const POSITIONED_EDGE_BYTES: usize = 512 * 1024 * 1024;

/// A fixed counter-table capacity, not a timing crossover. Counts reuse a
/// positioned core's relation ordinals and retain at most 32 KiB. A wider
/// catalog keeps the ordinary exact edge visitor without a summary.
const COUNT_SUMMARY_RELATIONS: usize = 4096;

/// Lets the admission laws cover cores with and without positioned edges.
#[cfg(test)]
pub(super) static POSITIONED_EDGES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

fn positioned_edge_bytes() -> usize {
    #[cfg(test)]
    if !POSITIONED_EDGES.load(std::sync::atomic::Ordering::Relaxed) {
        return 0;
    }
    POSITIONED_EDGE_BYTES
}

/// Every admitted edge of a core, in its sealed (identity) order, by
/// position: endpoints in the sealed vertex table every refill of the core
/// builds, relations in `relations`, which is ascending.
#[derive(Debug, Default)]
pub(super) struct PositionedEdges {
    pub(super) relations: Vec<RelationId>,
    pub(super) edges: Vec<fgdb_gql::PositionedEdge>,
    counts: Option<Vec<u64>>,
}

impl PositionedEdges {
    fn bytes(&self) -> usize {
        self.relations.capacity() * size_of::<RelationId>()
            + self.edges.capacity() * size_of::<fgdb_gql::PositionedEdge>()
            + self
                .counts
                .as_ref()
                .map_or(0, |counts| counts.capacity() * size_of::<u64>())
    }
}

/// Where one admitted edge's winning statement sits. Its identity is that
/// statement's, read back on refill from the snapshot allocation the core is
/// kept beside: 8 bytes per edge instead of 64, which is what lets a
/// web-google core (5.1M edges) fit the retention budget at all.
#[derive(Clone, Copy, Debug)]
pub(super) struct CoreEdge {
    pub(super) block: u32,
    pub(super) row: u32,
}

/// A full scan's admitted records, in admission order (ascending identity),
/// as coordinates into the snapshot allocation that produced them.
#[derive(Debug, Default)]
pub(crate) struct AdmittedCore {
    /// Identity, patch and row of each admitted vertex. The identity stays
    /// inline: a vertex-only statement refills from it without touching the
    /// rows, and vertices are few beside edges.
    pub(super) vertices: Vec<(VId, u32, u32)>,
    pub(super) edges: Vec<CoreEdge>,
    /// Each edge's identity in `edges` order, while they fit
    /// [`INLINE_IDENTITY_BYTES`]; otherwise refill reads them back.
    pub(super) identities: Option<Vec<EdgeIdentity>>,
    /// The edges by position, when the scan admitted them in strictly
    /// ascending identity (so admission order is the sealed order) and they
    /// fit [`POSITIONED_EDGE_BYTES`].
    pub(super) positioned: Option<PositionedEdges>,
    pub(super) records: u64,
    pub(super) tape: Tape,
    /// Where each vertex sits in the sealed table this core refills, built by
    /// the first statement that refills it and shared by every later one.
    /// Its complete prospective allocation is reserved by `bytes` before
    /// admission, even when a later statement is the first to build it.
    pub(super) positions: std::sync::OnceLock<Option<Arc<super::positions::VertexPositions>>>,
}

impl AdmittedCore {
    /// Spend exactly the live calls of the original admission. A table refill
    /// and a count summary share this replay; neither may skip source polls,
    /// signed nodes, native records or scratch on a cache hit.
    pub(super) fn replay(
        &self,
        node: &mut impl FnMut() -> Result<(), Fault>,
        poll: &mut impl FnMut() -> Result<(), Fault>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<(), Fault> {
        self.tape.replay(poll, |step| match step {
            Step::Vertex(labels) => charge_vertex(labels as usize, node, control),
            Step::Edge => charge_edge(control),
            Step::Checkpoint => Ok(()),
        })
    }

    /// Counts of the exact live rows this core admitted, never counts of
    /// historical identities. Vertex coordinates are the canonical winner
    /// visitor's unique rows; positioned edges contain only admitted edges
    /// whose two endpoints passed that same admission.
    pub(super) fn count_source(&self, source: fgdb_gql::GraphCountSource) -> Option<(u64, u64)> {
        match source {
            fgdb_gql::GraphCountSource::Vertices => {
                let count = u64::try_from(self.vertices.len()).ok()?;
                Some((count, count))
            }
            // Labels and wildcard edges use exact metadata, never this
            // scan-summary transcript. Scoped admission keeps its predicates.
            fgdb_gql::GraphCountSource::Label(_) | fgdb_gql::GraphCountSource::Edges => None,
            fgdb_gql::GraphCountSource::Relation(relation) => {
                if self.edges.is_empty() {
                    return Some((0, 0));
                }
                let positioned = self.positioned.as_ref()?;
                let counts = positioned.counts.as_ref()?;
                let matching = positioned
                    .relations
                    .binary_search(&relation)
                    .map_or(0, |at| counts[at]);
                Some((u64::try_from(self.edges.len()).ok()?, matching))
            }
        }
    }

    pub(super) fn bytes(&self) -> usize {
        size_of::<Self>()
            + self.vertices.capacity() * size_of::<(VId, u32, u32)>()
            + self.edges.capacity() * size_of::<CoreEdge>()
            + self.identities.as_ref().map_or(0, |identities| {
                identities.capacity() * size_of::<EdgeIdentity>()
            })
            + self.positioned.as_ref().map_or(0, PositionedEdges::bytes)
            + self.tape.bytes()
            + super::positions::VertexPositions::retained_bytes(self.vertices.len()).unwrap_or(0)
    }
}

/// The core a full scan is recording. A coordinate or label count that does
/// not fit the compact form abandons the recording; the scan itself goes on.
#[derive(Default)]
pub(super) struct Recording {
    core: AdmittedCore,
    abandoned: bool,
    /// The identities outgrew [`INLINE_IDENTITY_BYTES`] and were dropped.
    compact: bool,
    /// The positioned edges were dropped: an edge arrived out of strictly
    /// ascending identity, a position did not fit, or they outgrew
    /// [`POSITIONED_EDGE_BYTES`].
    unpositioned: bool,
    /// The newest admitted edge's identity, and its relation's position.
    last_edge: Option<EId>,
    last_relation: Option<(RelationId, u32)>,
}

impl Recording {
    /// A charge-free source poll.
    pub(super) fn poll(&mut self) {
        self.core.tape.poll();
    }

    /// The admitted vertex `vid` at `row` of `patch`, and the number of its
    /// labels that were charged.
    pub(super) fn vertex(&mut self, vid: VId, patch: usize, row: usize, labels: usize) {
        match (
            u32::try_from(patch),
            u32::try_from(row),
            u32::try_from(labels),
        ) {
            (Ok(patch), Ok(row), Ok(labels)) => {
                self.core.tape.emit(Some(Step::Vertex(labels)));
                self.core.vertices.push((vid, patch, row));
            }
            _ => self.abandoned = true,
        }
    }

    /// The admitted edge `identity`, whose winning statement is `row` of
    /// `block`, with its endpoints at `positions` of the sealed vertex table.
    pub(super) fn edge(
        &mut self,
        identity: EdgeIdentity,
        block: usize,
        row: usize,
        positions: [usize; 2],
    ) {
        match (u32::try_from(block), u32::try_from(row)) {
            (Ok(block), Ok(row)) => {
                self.core.tape.emit(Some(Step::Edge));
                self.core.edges.push(CoreEdge { block, row });
                self.position(identity, positions);
                if !self.compact {
                    let identities = self.core.identities.get_or_insert_with(Vec::new);
                    if (identities.len() + 1) * size_of::<EdgeIdentity>() <= inline_identity_bytes()
                    {
                        identities.push(identity);
                    } else {
                        self.core.identities = None;
                        self.compact = true;
                    }
                }
            }
            _ => self.abandoned = true,
        }
    }

    /// Keep `identity`'s edge by position while the positioned form holds.
    fn position(&mut self, (eid, _, relation, _): EdgeIdentity, [source, destination]: [usize; 2]) {
        if self.unpositioned {
            return;
        }
        let ascending = self.last_edge.is_none_or(|last| last < eid);
        self.last_edge = Some(eid);
        let positioned = self.core.positioned.get_or_insert_with(Default::default);
        let relation_at = match self.last_relation {
            Some((known, at)) if known == relation => Some(at),
            _ => match positioned
                .relations
                .iter()
                .position(|&known| known == relation)
            {
                Some(at) => u32::try_from(at).ok(),
                None => u32::try_from(positioned.relations.len()).ok().inspect(|_| {
                    positioned.relations.push(relation);
                }),
            },
        };
        let fits = (positioned.edges.len() + 1) * size_of::<fgdb_gql::PositionedEdge>()
            <= positioned_edge_bytes();
        match (
            u32::try_from(source),
            u32::try_from(destination),
            relation_at,
        ) {
            (Ok(source), Ok(destination), Some(relation_at)) if ascending && fits => {
                self.last_relation = Some((relation, relation_at));
                positioned.edges.push(fgdb_gql::PositionedEdge {
                    source,
                    destination,
                    relation: relation_at,
                });
            }
            _ => {
                self.core.positioned = None;
                self.unpositioned = true;
            }
        }
    }

    /// The finished core of a scan that admitted `records`, unless abandoned.
    pub(super) fn finish(self, records: u64) -> Option<AdmittedCore> {
        if self.abandoned {
            return None;
        }
        let mut core = self.core;
        core.records = records;
        core.tape = core.tape.finish();
        core.vertices.shrink_to_fit();
        // Inline identities are sealed here once, as every refill's table
        // would seal them (by identity, a repeat keeping its last
        // admission), so a refill is already in order and a triples-only
        // read can take them in place. The tape keeps admission order.
        if let Some(identities) = core.identities.take() {
            let mut paired: Vec<(EdgeIdentity, CoreEdge)> =
                identities.into_iter().zip(core.edges.drain(..)).collect();
            super::seal_by_key(&mut paired, |&((eid, ..), _)| eid);
            let (identities, edges): (Vec<_>, Vec<_>) = paired.into_iter().unzip();
            core.identities = Some(identities);
            core.edges = edges;
        }
        core.edges.shrink_to_fit();
        if let Some(identities) = &mut core.identities {
            identities.shrink_to_fit();
        }
        // Positioned edges arrived in strictly ascending identity, so their
        // admission order is already the sealed order. Relations are
        // renumbered in ascending identity, so a relation's position, like a
        // vertex's, orders exactly as its identity.
        if let Some(positioned) = &mut core.positioned {
            let mut order: Vec<u32> = (0..positioned.relations.len() as u32).collect();
            order.sort_unstable_by_key(|&at| positioned.relations[at as usize]);
            let mut renumbered = vec![0_u32; order.len()];
            for (new, &old) in order.iter().enumerate() {
                renumbered[old as usize] = new as u32;
            }
            positioned.relations.sort_unstable();
            let mut counts = (positioned.relations.len() <= COUNT_SUMMARY_RELATIONS)
                .then(|| vec![0_u64; positioned.relations.len()]);
            for edge in &mut positioned.edges {
                edge.relation = renumbered[edge.relation as usize];
                if let Some(counts) = &mut counts {
                    counts[edge.relation as usize] += 1;
                }
            }
            positioned.counts = counts;
            positioned.edges.shrink_to_fit();
            positioned.relations.shrink_to_fit();
        }
        Some(core)
    }
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
