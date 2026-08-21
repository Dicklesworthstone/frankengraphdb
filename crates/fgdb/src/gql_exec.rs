//! The product MATCH executors, all riding ONE kernel
//! (fgdb-gql-one-kernel-7y17): live, as-of, and the transaction overlay
//! (`write_txn.rs`) each hand `crate::execute_bound_plan_over` their own
//! source set and expansion, so the row discipline — projected vertex IDs
//! ascending, deduplicated — has exactly one owner and cannot drift between
//! faces. The parser normalizes either arrow spelling into edge source and
//! destination variables, so execution always builds source-to-destinations
//! adjacency in one pass over the admitted edge table.
//!
//! A two-hop pattern (fgdb-gql-two-hop-8pfw) composes TWO per-relation
//! adjacency maps, both filled by the same single scan of the admitted edge
//! table — never `neighbours()`, never a second table read. The expansion
//! handed to the kernel is projection-shaped: hop-2 destinations for the
//! path's far end, the intermediates that continue for the via variable, and
//! either serves the source projection since the kernel only asks "did this
//! source reach anything".

use crate::{Database, EdgeRecord, ReadError};
use asupersync::fs::Vfs;
use fgdb_delta_types::RelationId;
use fgdb_gql::{BoundPlan, EdgeDirection, ReturnProjection};
use fgdb_types::{CommitSeq, VId};
use std::collections::BTreeMap;

/// Both-orientation adjacency for one relation, filled by ONE scan
/// (fgdb-w5-parsers-nje.2): each edge lists its dst under its src AND its
/// src under its dst, so a vertex's expansion is the union of its outgoing
/// dests and incoming srcs — and the key set is every vertex incident to a
/// matching edge, which is exactly the undirected pattern's source set (a
/// dest-only vertex is still a `RETURN a` row).
fn undirected_adjacency(records: &[EdgeRecord], relation: RelationId) -> BTreeMap<VId, Vec<VId>> {
    let mut adjacency = BTreeMap::<VId, Vec<VId>>::new();
    for record in records {
        if record.entry.relation == relation {
            adjacency
                .entry(record.entry.src)
                .or_default()
                .push(record.entry.dst);
            adjacency
                .entry(record.entry.dst)
                .or_default()
                .push(record.entry.src);
        }
    }
    adjacency
}

/// src → dsts adjacency for one relation, and the same for the optional
/// hop-2 relation, filled by ONE scan of the fetched edge table.
fn relation_adjacencies(
    records: Vec<EdgeRecord>,
    hop1: RelationId,
    hop2: Option<RelationId>,
) -> (BTreeMap<VId, Vec<VId>>, BTreeMap<VId, Vec<VId>>) {
    let mut hop1_adjacency = BTreeMap::<VId, Vec<VId>>::new();
    let mut hop2_adjacency = BTreeMap::<VId, Vec<VId>>::new();
    for record in records {
        if record.entry.relation == hop1 {
            hop1_adjacency
                .entry(record.entry.src)
                .or_default()
                .push(record.entry.dst);
        }
        if hop2 == Some(record.entry.relation) {
            hop2_adjacency
                .entry(record.entry.src)
                .or_default()
                .push(record.entry.dst);
        }
    }
    (hop1_adjacency, hop2_adjacency)
}

/// Run the shared kernel over the prepared adjacency maps — the one body
/// behind the live and as-of faces.
fn execute_over_adjacencies(
    plan: &BoundPlan,
    hop1: BTreeMap<VId, Vec<VId>>,
    hop2: BTreeMap<VId, Vec<VId>>,
) -> Result<Vec<VId>, ReadError> {
    let sources: Vec<_> = hop1.keys().copied().collect();
    if plan.hop2_relation.is_none() {
        return crate::execute_bound_plan_over(plan, sources, |src, _| {
            Ok(hop1.get(&src).cloned().unwrap_or_default())
        });
    }
    crate::execute_bound_plan_over(plan, sources, |src, _| {
        let vias = hop1.get(&src).map(Vec::as_slice).unwrap_or_default();
        Ok(match plan.projection {
            // RETURN of the via variable: the intermediates that actually
            // continue — a hop-1 destination with no hop-2 edge is not on
            // any two-hop path.
            ReturnProjection::Destination => vias
                .iter()
                .filter(|via| hop2.contains_key(via))
                .copied()
                .collect(),
            // The path's far end — and the source projection, whose kernel
            // arm only asks whether this source reached anything, which for
            // a two-hop pattern means reaching a composed destination.
            ReturnProjection::Source | ReturnProjection::Hop2Destination => vias
                .iter()
                .flat_map(|via| hop2.get(via).cloned().unwrap_or_default())
                .collect(),
        })
    })
}

/// Execute the pinned bound MATCH expansion over the database's live Strata
/// view.
pub(crate) fn execute<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
) -> Result<Vec<VId>, ReadError> {
    let records = db.edges()?;
    if plan.direction == EdgeDirection::Undirected && plan.hop2_relation.is_none() {
        let adjacency = undirected_adjacency(&records, plan.relation);
        return execute_over_adjacencies(plan, adjacency, BTreeMap::new());
    }
    let (hop1, hop2) = relation_adjacencies(records, plan.relation, plan.hop2_relation);
    execute_over_adjacencies(plan, hop1, hop2)
}

/// Execute the pinned bound MATCH expansion at one historical frontier —
/// the same kernel, with the adjacency pass pinned to `as_of`.
pub(crate) fn execute_at<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
    as_of: CommitSeq,
) -> Result<Vec<VId>, ReadError> {
    let records = db.edges_at(as_of)?;
    if plan.direction == EdgeDirection::Undirected && plan.hop2_relation.is_none() {
        let adjacency = undirected_adjacency(&records, plan.relation);
        return execute_over_adjacencies(plan, adjacency, BTreeMap::new());
    }
    let (hop1, hop2) = relation_adjacencies(records, plan.relation, plan.hop2_relation);
    execute_over_adjacencies(plan, hop1, hop2)
}
