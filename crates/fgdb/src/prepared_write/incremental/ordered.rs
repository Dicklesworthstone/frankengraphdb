//! Extend ordered relation slices from their admitted net effects.
//!
//! The original raw program still determines edge ownership, relation slices,
//! source ordinals and replay work. Only native mutation evaluation is reduced:
//! unrelated admitted identities never enter another evaluator input.

use super::{seed_row, supported, target};
use crate::prepared_write::ordered::{edge_owners, restore_ordinal, route, vertex_effect};
use crate::{
    CoordinateEntry, Database, DeltaRow, ElementId, LogicalDeltaTemplate, PendingRow,
    PreparedWrite, RelationId, Vfs, WriteBatch, WriteError, WriteTxnError,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

fn expanded_rows(visits: u64, vertices: u64, relations: usize) -> Option<u64> {
    visits
        .checked_sub(vertices)?
        .checked_add(vertices.checked_mul(u64::try_from(relations).ok()?)?)
}

impl<V: Vfs + Clone> Database<V> {
    /// Incrementally prepare an ordered suffix without cloning or evaluating
    /// every earlier input row. The same supported-family law as the ordinary
    /// incremental lane applies to BOTH raw prefix and raw suffix instructions.
    ///
    /// Original relation slices are retained even when their only remaining
    /// evaluator input is a shared vertex instruction. This preserves native
    /// error precedence: a lower slice's CAS may refuse before a higher slice
    /// admits an invalid edge value. No slice is invented from normalized rows.
    /// Raw-prefix routing and final canonical composition remain prefix-sized.
    pub(crate) fn try_extend_prepared_ordered_writes(
        &mut self,
        previous: &PreparedWrite,
        staged: &[WriteBatch],
        suffix: &[WriteBatch],
    ) -> Result<Option<PreparedWrite>, WriteTxnError> {
        self.ensure_writable()?;
        if !Arc::ptr_eq(&self.handle_owner, &previous.handle_owner) {
            return Err(WriteError::ForeignPreparedWrite.into());
        }
        if previous.basis != self.snapshot.frontier
            || staged.is_empty()
            || suffix.is_empty()
            || staged.iter().chain(suffix).any(WriteBatch::is_empty)
        {
            return Ok(None);
        }
        let first = staged[0].relation;
        if staged
            .iter()
            .chain(suffix)
            .all(|batch| batch.relation == first)
        {
            // Ordered preparation deliberately preserves the one-relation
            // evaluator's identity-addressed foreign-edge behavior verbatim.
            let mut combined = WriteBatch::new(first);
            for batch in suffix {
                combined.rows.extend(batch.rows.iter().cloned());
            }
            return self.try_extend_prepared_write(previous, staged, &combined);
        }
        let prior_coordinates = previous.template.coordinate_entries();
        if prior_coordinates.is_empty()
            || prior_coordinates.iter().any(|coordinate| {
                coordinate.graph != crate::GRAPH
                    || coordinate.branch != crate::BRANCH
                    || coordinate.schema_epoch != crate::SchemaEpoch(0)
                    || coordinate.schema_transition.is_some()
            })
            || staged
                .iter()
                .chain(suffix)
                .flat_map(|batch| &batch.rows)
                .any(|row| !supported(row))
        {
            return Ok(None);
        }

        // Ownership admission must precede value/identity evaluation, including
        // when a repeated EId's two declarations have different relation types.
        let declarations = edge_owners(&self.writer, staged.iter().chain(suffix))?;
        let mut relations = BTreeSet::new();
        let mut edge_relations = BTreeSet::new();
        let mut routed_suffix = Vec::new();
        let mut visits = 0_u64;
        let mut vertices = 0_u64;
        let mut prefix_visits = 0_u64;
        let mut prefix_vertices = 0_u64;
        for (is_suffix, batch) in staged
            .iter()
            .map(|batch| (false, batch))
            .chain(suffix.iter().map(|batch| (true, batch)))
        {
            relations.insert(batch.relation);
            for pending in &batch.rows {
                visits = visits
                    .checked_add(1)
                    .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
                let owner = route(&self.writer, &declarations, batch.relation, pending);
                if let Some(relation) = owner {
                    relations.insert(relation);
                    edge_relations.insert(relation);
                } else {
                    vertices += 1;
                }
                if is_suffix {
                    routed_suffix.push((visits, owner, pending));
                }
            }
            if !is_suffix {
                prefix_visits = visits;
                prefix_vertices = vertices;
            }
        }
        if vertices == 0 {
            // As in full ordered preparation, an enclosing batch relation with
            // only foreign identity-addressed edge instructions has no slice.
            relations = edge_relations;
        }

        let mut needed = BTreeSet::new();
        for (_, _, row) in &routed_suffix {
            match row {
                PendingRow::Vertex { vid, .. }
                | PendingRow::SetLabel { vid, .. }
                | PendingRow::SetProperty { vid, .. } => {
                    needed.insert(ElementId::Vertex(*vid));
                }
                PendingRow::Edge { eid, src, dst, .. } => {
                    needed.insert(ElementId::Edge(*eid));
                    needed.insert(ElementId::Vertex(*src));
                    needed.insert(ElementId::Vertex(*dst));
                }
                PendingRow::SetEdgeProperty { eid, .. } => {
                    needed.insert(ElementId::Edge(*eid));
                }
                PendingRow::CompareAndSet { elem, .. } => {
                    needed.insert(*elem);
                }
                PendingRow::DeleteVertex { .. } | PendingRow::DeleteEdge { .. } => {
                    return Ok(None);
                }
            }
        }
        for row in previous.template.rows() {
            if target(row).is_none() {
                return Ok(None);
            }
            if let DeltaRow::CreateEdge { eid, src, dst, .. } = row
                && needed.contains(&ElementId::Edge(*eid))
            {
                needed.insert(ElementId::Vertex(*src));
                needed.insert(ElementId::Vertex(*dst));
            }
        }

        let mut coordinates: BTreeMap<_, _> = relations
            .iter()
            .map(|&relation| {
                (
                    relation,
                    CoordinateEntry {
                        graph: crate::GRAPH,
                        branch: crate::BRANCH,
                        relation,
                        schema_epoch: crate::SchemaEpoch(0),
                        schema_transition: None,
                        rows: Vec::new(),
                    },
                )
            })
            .collect();
        let mut seed_vertices = Vec::new();
        let mut seed_edges: BTreeMap<RelationId, Vec<(PendingRow, u64)>> = BTreeMap::new();
        let mut untouched_vertices = Vec::new();
        for coordinate in prior_coordinates {
            for row in &coordinate.rows {
                let element = target(row).expect("supported prefix checked above");
                let owner = match element {
                    ElementId::Vertex(_) => None,
                    ElementId::Edge(eid) => Some(
                        declarations
                            .get(&eid)
                            .copied()
                            .or_else(|| {
                                self.writer
                                    .live_edge(eid)
                                    .map(|(_, relation, _, _)| relation)
                            })
                            .unwrap_or(coordinate.relation),
                    ),
                };
                if owner.is_some_and(|owner| !coordinates.contains_key(&owner)) {
                    return Ok(None);
                }
                if !needed.contains(&element) {
                    if let Some(owner) = owner {
                        let Some(coordinate) = coordinates.get_mut(&owner) else {
                            return Ok(None);
                        };
                        coordinate.rows.push(row.clone());
                    } else {
                        untouched_vertices.push(row.clone());
                    }
                    continue;
                }
                let Some((pending, birth)) =
                    seed_row(row, owner.unwrap_or(coordinate.relation), prefix_visits)
                else {
                    return Ok(None);
                };
                // Only create instructions use a visit to restore an ordinal.
                // Field seed rows have no birth and never consume source visits.
                let seed = (pending, birth.unwrap_or(0));
                if let Some(owner) = owner {
                    seed_edges.entry(owner).or_default().push(seed);
                } else {
                    seed_vertices.push(seed);
                }
            }
        }

        let mut dependencies = previous.dependencies.clone();
        let mut shared_vertices = None;
        for (&relation, coordinate) in &mut coordinates {
            let mut slice = WriteBatch::new(relation);
            let mut slice_visits = Vec::new();
            for (pending, visit) in seed_vertices
                .iter()
                .cloned()
                .chain(seed_edges.remove(&relation).into_iter().flatten())
            {
                slice.rows.push(pending);
                slice_visits.push(visit);
            }
            for &(visit, owner, pending) in &routed_suffix {
                if owner.is_none_or(|owner| owner == relation) {
                    slice.rows.push(pending.clone());
                    slice_visits.push(visit);
                }
            }
            if slice.is_empty() {
                continue;
            }
            let partial = self.prepare_write_checked(slice)?;
            let [partial_coordinate] = partial.template.coordinate_entries() else {
                return Err(WriteTxnError::UnsupportedAtomicMutation);
            };
            if partial_coordinate.relation != relation {
                return Err(WriteTxnError::UnsupportedAtomicMutation);
            }
            let mut partial_vertices = Vec::new();
            for mut row in partial_coordinate.rows.iter().cloned() {
                restore_ordinal(&mut row, &slice_visits)?;
                if vertex_effect(&row)? {
                    partial_vertices.push(row);
                } else {
                    coordinate.rows.push(row);
                }
            }
            match &shared_vertices {
                Some(expected) if *expected != partial_vertices => {
                    return Err(WriteTxnError::UnsupportedAtomicMutation);
                }
                Some(_) => {}
                None => shared_vertices = Some(partial_vertices),
            }
            dependencies.extend(partial.dependencies);
        }
        let Some(mut owner) = coordinates.first_entry() else {
            return Err(WriteTxnError::UnsupportedAtomicMutation);
        };
        owner.get_mut().rows.extend(untouched_vertices);
        owner
            .get_mut()
            .rows
            .extend(shared_vertices.unwrap_or_default());

        // Preserve a withheld/reduced allowance. New relation slices also add
        // replay work for the ORIGINAL prefix vertex rows, including no-ops.
        // Seed instructions are never counted as newly admitted source work.
        let replay_row_allowance = previous.replay_row_allowance.and_then(|previous_rows| {
            let before = expanded_rows(prefix_visits, prefix_vertices, prior_coordinates.len())?;
            let after = expanded_rows(visits, vertices, coordinates.len())?;
            previous_rows.checked_add(after.checked_sub(before)?)
        });
        let template = LogicalDeltaTemplate::build(
            previous.template.intent_semantics_oid(),
            *previous.template.source_intent_root_digest(),
            coordinates.into_values().collect(),
        )
        .map_err(WriteError::Canonical)?;
        Ok(Some(PreparedWrite {
            template,
            basis: previous.basis,
            handle_owner: Arc::clone(&self.handle_owner),
            dependencies,
            replay_row_allowance,
            raw_input: None,
            authorization_decision_digest: previous.authorization_decision_digest,
        }))
    }
}

#[cfg(test)]
mod tests;
