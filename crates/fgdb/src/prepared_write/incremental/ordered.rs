//! Extend ordered relation slices from their admitted net effects.
//!
//! The original raw program still determines edge ownership, relation slices,
//! source ordinals and replay work. Only native mutation evaluation is reduced:
//! unrelated admitted identities never enter another evaluator input.

use super::{seed_row, supported, target};
use crate::prepared_write::RawOrderedSummary;
use crate::prepared_write::ordered::{edge_owners, restore_ordinal, route_resolved, vertex_effect};
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
    /// Unchanged coordinates retain their checked canonical rows and merge only
    /// new effects. Stable raw routing is retained with the admitted prefix;
    /// unresolved identity targets keep the complete routing pass. Owned row
    /// copying and canonical ownership checks remain prefix-sized.
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
        let cached = previous
            .raw_ordered
            .as_ref()
            .filter(|summary| summary.batches == staged.len());
        if previous.basis != self.snapshot.frontier
            || staged.is_empty()
            || suffix.is_empty()
            || suffix.iter().any(WriteBatch::is_empty)
            || (cached.is_none() && staged.iter().any(WriteBatch::is_empty))
        {
            return Ok(None);
        }
        let first = staged[0].relation;
        if cached.is_none()
            && staged
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
            || suffix
                .iter()
                .flat_map(|batch| &batch.rows)
                .any(|row| !supported(row))
            || (cached.is_none()
                && staged
                    .iter()
                    .flat_map(|batch| &batch.rows)
                    .any(|row| !supported(row)))
        {
            return Ok(None);
        }

        // Ownership admission must precede value/identity evaluation, including
        // when a repeated EId's two declarations have different relation types.
        let declarations = match cached {
            Some(summary) => {
                let Some(declarations) = summary.extend_declarations(&self.writer, suffix)? else {
                    return Ok(None);
                };
                declarations
            }
            None => edge_owners(&self.writer, staged.iter().chain(suffix))?,
        };
        let mut batch_relations = cached
            .map(|summary| summary.batch_relations.clone())
            .unwrap_or_default();
        let mut edge_relations = cached
            .map(|summary| summary.edge_relations.clone())
            .unwrap_or_default();
        let mut routed_suffix = Vec::new();
        let mut visits = cached.map_or(0, |summary| summary.visits);
        let mut vertices = cached.map_or(0, |summary| summary.vertices);
        let mut prefix_visits = visits;
        let mut prefix_vertices = vertices;
        let mut stable_routing = true;
        let raw_prefix = if cached.is_some() { &[][..] } else { staged };
        for (is_suffix, batch) in raw_prefix
            .iter()
            .map(|batch| (false, batch))
            .chain(suffix.iter().map(|batch| (true, batch)))
        {
            batch_relations.insert(batch.relation);
            for pending in &batch.rows {
                visits = visits
                    .checked_add(1)
                    .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
                let (owner, resolved) =
                    route_resolved(&self.writer, &declarations, batch.relation, pending);
                stable_routing &= resolved;
                if let Some(relation) = owner {
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
        // With no vertex instructions, a batch containing only foreign edge
        // mutations still has no slice in its enclosing relation.
        let mut relations = edge_relations.clone();
        if vertices != 0 {
            relations.extend(&batch_relations);
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
        let vertex_owner = relations.first().copied();
        let mut retain_canonical = prior_coordinates
            .iter()
            .map(|coordinate| coordinate.relation)
            .eq(relations.iter().copied());
        let edge_owner = |eid, relation| {
            declarations
                .get(&eid)
                .copied()
                .or_else(|| {
                    self.writer
                        .live_edge(eid)
                        .map(|(_, relation, _, _)| relation)
                })
                .unwrap_or(relation)
        };
        for coordinate in prior_coordinates {
            for row in &coordinate.rows {
                let Some(element) = target(row) else {
                    return Ok(None);
                };
                // Reusing a checked coordinate must never keep a foreign
                // edge or the shared vertex backbone in its old owner. The
                // existing complete composition handles such moves unchanged.
                if retain_canonical {
                    retain_canonical = match element {
                        ElementId::Vertex(_) => Some(coordinate.relation) == vertex_owner,
                        ElementId::Edge(eid) => {
                            edge_owner(eid, coordinate.relation) == coordinate.relation
                        }
                    };
                }
                if let DeltaRow::CreateEdge { eid, src, dst, .. } = row
                    && needed.contains(&ElementId::Edge(*eid))
                {
                    needed.insert(ElementId::Vertex(*src));
                    needed.insert(ElementId::Vertex(*dst));
                }
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
                    ElementId::Edge(eid) => Some(edge_owner(eid, coordinate.relation)),
                };
                if owner.is_some_and(|owner| !coordinates.contains_key(&owner)) {
                    return Ok(None);
                }
                if !needed.contains(&element) {
                    if retain_canonical {
                        // The final canonical merge borrows this checked row
                        // directly; do not copy it into an intermediate slice.
                        continue;
                    }
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
        let template = if retain_canonical {
            let additional = coordinates
                .into_values()
                .map(|coordinate| (coordinate.coordinate(), coordinate.rows))
                .collect();
            previous
                .template
                .replace_coordinate_rows_many(
                    |_, row| {
                        !needed.contains(&target(row).expect("supported prefix checked above"))
                    },
                    additional,
                )
                .map_err(WriteError::Canonical)?
                .ok_or(WriteTxnError::UnsupportedAtomicMutation)?
        } else {
            LogicalDeltaTemplate::build(
                previous.template.intent_semantics_oid(),
                *previous.template.source_intent_root_digest(),
                coordinates.into_values().collect(),
            )
            .map_err(WriteError::Canonical)?
        };
        let raw_ordered = if stable_routing {
            staged.len().checked_add(suffix.len()).and_then(|batches| {
                declarations
                    .into_shared()
                    .map(|declarations| RawOrderedSummary {
                        batches,
                        visits,
                        vertices,
                        batch_relations,
                        edge_relations,
                        declarations,
                    })
            })
        } else {
            None
        };
        Ok(Some(PreparedWrite {
            template,
            basis: previous.basis,
            handle_owner: Arc::clone(&self.handle_owner),
            dependencies,
            replay_row_allowance,
            raw_input: None,
            raw_ordered,
            authorization_decision_digest: previous.authorization_decision_digest,
        }))
    }
}

#[cfg(test)]
mod tests;
