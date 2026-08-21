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
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::{BoundPlan, EdgeDirection, ReturnProjection};
use fgdb_types::{CommitSeq, VId};
use std::collections::BTreeMap;

/// Both-orientation adjacency for the hop-1 relation AND the optional hop-2
/// relation, filled by ONE loop over the fetched edge table
/// (fgdb-w5-parsers-nje.2 one-hop, fgdb-gql-undir-2hop-7mrc two-hop) — the
/// undirected twin of [`relation_adjacencies`]. Each edge lists its dst
/// under its src AND its src under its dst, so a vertex's expansion is the
/// union of outgoing dests and incoming srcs, and the hop-1 key set is
/// every incident vertex (a dest-only vertex is still a `RETURN a` row).
/// The composition downstream is map-agnostic, so feeding it these maps IS
/// the undirected two-hop: vias are undirected hop-1 neighbours, and their
/// undirected hop-2 neighbours (or the vias that have any) are the rows.
fn undirected_adjacencies(
    records: &[EdgeRecord],
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
            hop1_adjacency
                .entry(record.entry.dst)
                .or_default()
                .push(record.entry.src);
        }
        if hop2 == Some(record.entry.relation) {
            hop2_adjacency
                .entry(record.entry.src)
                .or_default()
                .push(record.entry.dst);
            hop2_adjacency
                .entry(record.entry.dst)
                .or_default()
                .push(record.entry.src);
        }
    }
    (hop1_adjacency, hop2_adjacency)
}

/// dst → srcs adjacency for the hop-1 relation AND the optional hop-2
/// relation, filled by ONE loop over the fetched edge table
/// (fgdb-w5-parsers-nje.4) — the inverted twin of [`relation_adjacencies`]
/// for the incoming TWO-hop chain `(a)<-[:R]-(b)<-[:S]-(c)`: walking from
/// the anchor means walking every edge against its flow, so both hops
/// invert and the existing composition then IS the reverse composition.
/// Incoming ONE-hop never comes here — the parser normalizes its variable
/// roles, so it executes on the uninverted maps.
fn inverted_adjacencies(
    records: &[EdgeRecord],
    hop1: RelationId,
    hop2: Option<RelationId>,
) -> (BTreeMap<VId, Vec<VId>>, BTreeMap<VId, Vec<VId>>) {
    let mut hop1_adjacency = BTreeMap::<VId, Vec<VId>>::new();
    let mut hop2_adjacency = BTreeMap::<VId, Vec<VId>>::new();
    for record in records {
        if record.entry.relation == hop1 {
            hop1_adjacency
                .entry(record.entry.dst)
                .or_default()
                .push(record.entry.src);
        }
        if hop2 == Some(record.entry.relation) {
            hop2_adjacency
                .entry(record.entry.dst)
                .or_default()
                .push(record.entry.src);
        }
    }
    (hop1_adjacency, hop2_adjacency)
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
    // WHERE a <> b (fgdb-gql-where-neq-v476): the predicate binds the two
    // hop-1 pattern variables, so it filters exactly the hop-1 step — a
    // self-loop edge stops matching, in every direction mode, before any
    // projection or hop-2 composition sees it. Filtering the KERNEL's
    // composed expansion instead would express a <> c on a two-hop plan,
    // which is a different (unrequested) predicate.
    let hop1_kept = |src: VId, via: &VId| plan.neq.is_none() || *via != src;
    if plan.hop2_relation.is_none() {
        return crate::execute_bound_plan_over(plan, sources, |src, _| {
            let mut dests = hop1.get(&src).cloned().unwrap_or_default();
            dests.retain(|dst| hop1_kept(src, dst));
            Ok(dests)
        });
    }
    crate::execute_bound_plan_over(plan, sources, |src, _| {
        let vias: Vec<VId> = hop1
            .get(&src)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|via| hop1_kept(src, via))
            .copied()
            .collect();
        let vias = vias.as_slice();
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

/// The label the PROJECTED role must carry, if the plan binds one
/// (fgdb-w5-parsers-nje.5): `RETURN a` rows answer for the source variable
/// and its label, `RETURN b` rows for the destination's. An unlabeled plan
/// binds nothing here and must consult no vertex row at all; the two-hop
/// far end has no label field in this bounded grammar.
fn projected_label(plan: &BoundPlan) -> Option<LabelId> {
    match plan.projection {
        ReturnProjection::Source => plan.src_label,
        ReturnProjection::Destination => plan.dst_label,
        ReturnProjection::Hop2Destination => None,
    }
}

/// Execute the pinned bound MATCH expansion over the database's live Strata
/// view.
pub(crate) fn execute<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
) -> Result<Vec<VId>, ReadError> {
    let records = db.edges()?;
    let rows = if plan.direction == EdgeDirection::Undirected {
        let (hop1, hop2) = undirected_adjacencies(&records, plan.relation, plan.hop2_relation);
        execute_over_adjacencies(plan, hop1, hop2)?
    } else if plan.direction == EdgeDirection::Incoming && plan.hop2_relation.is_some() {
        let (hop1, hop2) = inverted_adjacencies(&records, plan.relation, plan.hop2_relation);
        execute_over_adjacencies(plan, hop1, hop2)?
    } else {
        let (hop1, hop2) = relation_adjacencies(records, plan.relation, plan.hop2_relation);
        execute_over_adjacencies(plan, hop1, hop2)?
    };
    let Some(label) = projected_label(plan) else {
        return Ok(rows);
    };
    let mut kept = Vec::with_capacity(rows.len());
    for vid in rows {
        if db
            .vertex(vid)?
            .is_some_and(|row| row.labels.contains(&label))
        {
            kept.push(vid);
        }
    }
    Ok(kept)
}

/// Execute the pinned bound MATCH expansion at one historical frontier —
/// the same kernel, with the adjacency pass pinned to `as_of`.
pub(crate) fn execute_at<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
    as_of: CommitSeq,
) -> Result<Vec<VId>, ReadError> {
    let records = db.edges_at(as_of)?;
    let rows = if plan.direction == EdgeDirection::Undirected {
        let (hop1, hop2) = undirected_adjacencies(&records, plan.relation, plan.hop2_relation);
        execute_over_adjacencies(plan, hop1, hop2)?
    } else if plan.direction == EdgeDirection::Incoming && plan.hop2_relation.is_some() {
        let (hop1, hop2) = inverted_adjacencies(&records, plan.relation, plan.hop2_relation);
        execute_over_adjacencies(plan, hop1, hop2)?
    } else {
        let (hop1, hop2) = relation_adjacencies(records, plan.relation, plan.hop2_relation);
        execute_over_adjacencies(plan, hop1, hop2)?
    };
    let Some(label) = projected_label(plan) else {
        return Ok(rows);
    };
    let mut kept = Vec::with_capacity(rows.len());
    for vid in rows {
        if db
            .vertex_at(vid, as_of)?
            .is_some_and(|row| row.labels.contains(&label))
        {
            kept.push(vid);
        }
    }
    Ok(kept)
}
