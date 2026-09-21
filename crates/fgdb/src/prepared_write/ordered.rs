//! Source-ordered relation composition using the existing mutation evaluator.
//!
//! Vertex instructions form a shared ordered backbone. Each immutable edge
//! identity belongs to one relation slice; that slice retains every instruction
//! for the edge and every vertex instruction, at their original positions.
//! The ordinary evaluator therefore sees the same endpoint lifetimes, guards,
//! property prefixes and cascades as the full program. Nothing is hoisted.
//!
//! This is a decomposition law, not a second interpreter or a shadow database.
//! Every slice runs build_write_template via prepare_write_checked. Normalized
//! vertex effects must agree exactly across slices and are published once in
//! the least coordinate. Birth ordinals are restored to original raw visits.

use super::PreparedDependencies;
use crate::{Database, PendingRow, PreparedWrite, WriteBatch, WriteError, WriteTxnError};
use asupersync::fs::Vfs;
use fgdb_delta_types::{CoordinateEntry, DeltaRow, ElementId, LogicalDeltaTemplate, RelationId};
use fgdb_strata::writer::BlockWriter;
use fgdb_types::{CommitCx, CommitSeq, EId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Fixed edge ownership, including declarations whose effects later cancel.
/// A request identity cannot be declared under two relation types in one
/// ordered program. Reject even ambiguous ENSURE declarations: their requested
/// ID may be unused, but must never authorize a cross-type identity collision.
fn edge_owners(
    writer: &BlockWriter,
    batches: &[WriteBatch],
) -> Result<BTreeMap<EId, RelationId>, WriteTxnError> {
    let mut owners = BTreeMap::new();
    for batch in batches {
        for pending in &batch.rows {
            if let PendingRow::Edge { eid, .. } = pending {
                let known = writer.live_edge(*eid).map(|(_, relation, _, _)| relation);
                let owner = owners.entry(*eid).or_insert(known.unwrap_or(batch.relation));
                if *owner != batch.relation {
                    return Err(WriteTxnError::AtomicRelationConflict {
                        first: *owner,
                        second: batch.relation,
                        element: ElementId::Edge(*eid),
                    });
                }
            }
        }
    }
    Ok(owners)
}

/// Only edge creation takes its type from the enclosing batch. Existing edge
/// mutations are identity-addressed; putting one in another batch must not
/// move it to that batch's relation or lose its earlier same-program writes.
fn route(
    writer: &BlockWriter,
    declarations: &BTreeMap<EId, RelationId>,
    relation: RelationId,
    pending: &PendingRow,
) -> Option<RelationId> {
    let eid = match pending {
        PendingRow::Edge { .. } => return Some(relation),
        PendingRow::DeleteEdge { eid, .. } | PendingRow::SetEdgeProperty { eid, .. } => *eid,
        PendingRow::CompareAndSet { elem: ElementId::Edge(eid), .. } => *eid,
        PendingRow::Vertex { .. }
        | PendingRow::DeleteVertex { .. }
        | PendingRow::SetLabel { .. }
        | PendingRow::SetProperty { .. }
        | PendingRow::CompareAndSet { elem: ElementId::Vertex(_), .. } => return None,
    };
    declarations.get(&eid).copied()
        .or_else(|| writer.live_edge(eid).map(|(_, relation, _, _)| relation))
        // Missing targets stay in a real slice, where the existing evaluator
        // distinguishes an optional no-op from UnknownEdge or a failed guard.
        .or(Some(relation))
}

fn vertex_effect(row: &DeltaRow) -> Result<bool, WriteTxnError> {
    match row {
        DeltaRow::CreateVertex { .. }
        | DeltaRow::DeleteVertex { .. }
        | DeltaRow::LabelMembership { .. }
        | DeltaRow::Property { elem: ElementId::Vertex(_), .. } => Ok(true),
        DeltaRow::CreateEdge { .. }
        | DeltaRow::DeleteEdge { .. }
        | DeltaRow::Property { elem: ElementId::Edge(_), .. } => Ok(false),
        // A new mutation family needs a decomposition law before admission.
        _ => Err(WriteTxnError::UnsupportedAtomicMutation),
    }
}

fn restore_ordinal(row: &mut DeltaRow, visits: &[u64]) -> Result<(), WriteTxnError> {
    if let DeltaRow::CreateVertex { birth_ordinal, .. }
    | DeltaRow::CreateEdge { birth_ordinal, .. } = row {
        let index = birth_ordinal.checked_sub(1)
            .and_then(|ordinal| usize::try_from(ordinal).ok())
            .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
        *birth_ordinal = *visits.get(index).ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
    }
    Ok(())
}

impl<V: Vfs + Clone> Database<V> {
    /// Prepare dependent, source-ordered writes spanning edge relations.
    ///
    /// Unlike prepare_atomic_writes' independent groups, later instructions
    /// observe earlier vertex and edge mutations across batch boundaries.
    /// Create/update/CAS/delete/ensure use the same evaluator as a single batch.
    /// No intermediate capsule, sequence, snapshot or durable object is made.
    /// The returned PreparedWrite retains all original-basis dependencies and
    /// commits through the ordinary FCW and Chronicle publication path.
    ///
    /// Successful effects obey input order, not relation-ID order. Error
    /// selection across independent slices is deterministic but does not claim
    /// to identify the earliest failing source instruction. An EId requested by
    /// a creation/ensure must have one declared relation throughout the program,
    /// including cancelled declarations and otherwise-unused ensure IDs.
    /// Existing edge updates/deletes/CAS route by the edge's real identity.
    ///
    /// This bounded decomposition copies the vertex instruction backbone per
    /// touched relation. It is not spill, arbitrary future intent semantics,
    /// cross-graph transactions or SSI. The independent-group API is unchanged.
    pub fn prepare_ordered_writes(
        &mut self,
        batches: Vec<WriteBatch>,
    ) -> Result<PreparedWrite, WriteTxnError> {
        self.ensure_writable()?;
        if batches.is_empty() || batches.iter().any(WriteBatch::is_empty) {
            return Err(WriteError::EmptyBatch.into());
        }
        let first = batches[0].relation;
        if batches.iter().all(|batch| batch.relation == first) {
            // Preserve the original one-relation template byte-for-byte,
            // including existing identity-addressed foreign-edge mutations.
            let mut combined = WriteBatch::new(first);
            for batch in batches {
                combined.extend(batch)?;
            }
            return self.prepare_write_checked(combined).map_err(Into::into);
        }
        let declarations = edge_owners(&self.writer, &batches)?;
        let mut relations = BTreeSet::new();
        let mut routed = Vec::new();
        let mut ordinal = 0_u64;
        for batch in &batches {
            relations.insert(batch.relation);
            for pending in &batch.rows {
                ordinal = ordinal.checked_add(1).ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
                let target = route(&self.writer, &declarations, batch.relation, pending);
                if let Some(relation) = target {
                    relations.insert(relation);
                }
                routed.push((ordinal, target, pending));
            }
        }
        let mut coordinates: BTreeMap<RelationId, CoordinateEntry> = BTreeMap::new();
        let mut shared_vertices: Option<Vec<DeltaRow>> = None;
        let mut dependencies = PreparedDependencies::default();
        for relation in relations {
            let mut slice = WriteBatch::new(relation);
            let mut visits = Vec::new();
            for &(ordinal, target, pending) in &routed {
                if target.is_none_or(|target| target == relation) {
                    slice.rows.push(pending.clone());
                    visits.push(ordinal);
                }
            }
            if slice.is_empty() {
                continue;
            }
            let prepared = self.prepare_write_checked(slice)?;
            let [coordinate] = prepared.template.coordinate_entries() else {
                return Err(WriteTxnError::UnsupportedAtomicMutation);
            };
            if coordinate.relation != relation {
                return Err(WriteTxnError::UnsupportedAtomicMutation);
            }
            if let Some(previous) = coordinates.values().next() {
                if previous.graph != coordinate.graph
                    || previous.branch != coordinate.branch
                    || previous.schema_epoch != coordinate.schema_epoch
                    || previous.schema_transition != coordinate.schema_transition
                {
                    return Err(WriteTxnError::UnsupportedAtomicMutation);
                }
            }
            let mut coordinate = coordinate.clone();
            let mut vertices = Vec::new();
            let mut edges = Vec::new();
            for mut row in coordinate.rows {
                restore_ordinal(&mut row, &visits)?;
                if vertex_effect(&row)? {
                    vertices.push(row);
                } else {
                    edges.push(row);
                }
            }
            match &shared_vertices {
                Some(expected) if *expected != vertices => {
                    // In particular the same normalized cascade must name the
                    // same BASIS edges in every slice. Same-program incident
                    // creates are cancelled by the existing NENF before here.
                    return Err(WriteTxnError::UnsupportedAtomicMutation);
                }
                Some(_) => {}
                None => shared_vertices = Some(vertices),
            }
            coordinate.rows = edges;
            coordinates.insert(relation, coordinate);
            dependencies.elements.extend(prepared.dependencies.elements);
            dependencies.adjacency.extend(prepared.dependencies.adjacency);
        }
        // Every new endpoint is born before any later coordinate's edges.
        // Vertex deletes also precede later coordinates; each slice's NENF has
        // already absorbed ALL effects on cascaded edges, so no update can
        // resurrect an edge retired by this single shared delete payload.
        let Some((_, owner)) = coordinates.first_key_value() else {
            return Err(WriteTxnError::UnsupportedAtomicMutation);
        };
        let owner_relation = owner.relation;
        coordinates.get_mut(&owner_relation).expect("nonempty coordinate map")
            .rows.extend(shared_vertices.unwrap_or_default());
        let template = LogicalDeltaTemplate::build(
            crate::intent_semantics_oid(),
            [0_u8; 32],
            coordinates.into_values().collect(),
        ).map_err(WriteError::Canonical)?;
        Ok(PreparedWrite {
            template,
            basis: self.snapshot.frontier,
            handle_owner: Arc::clone(&self.handle_owner),
            dependencies,
        })
    }

    /// Publish a dependent ordered program as ONE commit. Preparation never
    /// publishes a prefix, and commit retains the normal unknown-outcome fence.
    pub async fn write_ordered(
        &mut self,
        cx: &CommitCx,
        batches: Vec<WriteBatch>,
    ) -> Result<CommitSeq, WriteTxnError> {
        let prepared = self.prepare_ordered_writes(batches)?;
        self.commit_prepared(cx, prepared).await.map_err(Into::into)
    }
}

impl PreparedWrite {
    /// Re-entering ordered staging must not renumber births already visible
    /// through the transaction's canonical overlay. This also permits a
    /// previously independent prefix to enter ordered composition: its effects
    /// commute, but its original relation-group birth ordering may differ.
    /// New ordered births occur after all previous raw visits, so retaining old
    /// ordinals cannot collide with a newly appended instruction's ordinal.
    pub(crate) fn retain_birth_ordinals(
        mut self,
        previous: Option<&Self>,
    ) -> Result<Self, WriteTxnError> {
        let Some(previous) = previous else { return Ok(self); };
        if !Arc::ptr_eq(&self.handle_owner, &previous.handle_owner) {
            return Err(WriteTxnError::WrongDatabase);
        }
        if self.basis != previous.basis {
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: previous.basis,
                live: self.basis,
            });
        }
        let births: BTreeMap<_, _> = previous.template.coordinate_entries().iter()
            .flat_map(|coordinate| &coordinate.rows)
            .filter_map(|row| match row {
                DeltaRow::CreateVertex { vid, birth_ordinal, .. } =>
                    Some((ElementId::Vertex(*vid), *birth_ordinal)),
                DeltaRow::CreateEdge { eid, birth_ordinal, .. } =>
                    Some((ElementId::Edge(*eid), *birth_ordinal)),
                _ => None,
            }).collect();
        if births.is_empty() { return Ok(self); }
        let mut coordinates = self.template.coordinate_entries().to_vec();
        let mut changed = false;
        for coordinate in &mut coordinates {
            for row in &mut coordinate.rows {
                let (identity, ordinal) = match row {
                    DeltaRow::CreateVertex { vid, birth_ordinal, .. } =>
                        (ElementId::Vertex(*vid), birth_ordinal),
                    DeltaRow::CreateEdge { eid, birth_ordinal, .. } =>
                        (ElementId::Edge(*eid), birth_ordinal),
                    _ => continue,
                };
                if let Some(retained) = births.get(&identity) {
                    changed |= *ordinal != *retained;
                    *ordinal = *retained;
                }
            }
        }
        if changed {
            // This method is private to staging ordinary engine-built writes;
            // both inputs use the same existing intent semantics and source.
            self.template = LogicalDeltaTemplate::build(
                crate::intent_semantics_oid(), [0_u8; 32], coordinates,
            ).map_err(WriteError::Canonical)?;
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests;
