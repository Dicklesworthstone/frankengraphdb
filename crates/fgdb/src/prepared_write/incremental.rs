//! Reuse an admitted single-relation prefix without interpreting it again.
//!
//! Only the prior net effects needed by the new instructions are replayed into
//! the ordinary native evaluator. Deletions and edge ENSURE stay on the full
//! preparation path: their spent identities and adjacency/alias observations
//! cannot be reconstructed from an arbitrary normalized prefix alone.

use super::PreparedDependencies;
use crate::{
    Database, DeltaRow, ElementId, PendingRow, PreparedWrite, Vfs, WriteBatch, WriteError,
    WriteTxnError,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

fn supported(row: &PendingRow) -> bool {
    match row {
        PendingRow::Vertex { .. }
        | PendingRow::Edge { ensure: false, .. }
        | PendingRow::SetLabel { .. }
        | PendingRow::SetProperty { .. }
        | PendingRow::SetEdgeProperty { .. }
        | PendingRow::CompareAndSet { .. } => true,
        PendingRow::DeleteVertex { .. }
        | PendingRow::DeleteEdge { .. }
        | PendingRow::Edge { ensure: true, .. } => false,
    }
}

fn target(row: &DeltaRow) -> Option<ElementId> {
    match row {
        DeltaRow::CreateVertex { vid, .. } | DeltaRow::LabelMembership { vid, .. } => {
            Some(ElementId::Vertex(*vid))
        }
        DeltaRow::CreateEdge { eid, .. } => Some(ElementId::Edge(*eid)),
        DeltaRow::Property { elem, .. } => Some(*elem),
        _ => None,
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Extend an already admitted ordinary prefix with the same native
    /// evaluator used by full preparation. `None` requests the unchanged full
    /// path; it never accepts a partially interpreted or unsupported prefix.
    ///
    /// This limits repeated prefix evaluation, before-image lookup, scalar
    /// admission and storage-content admission to affected identities. The final
    /// template and dependency sets are still composed per call, so this is not
    /// a claim that arbitrary transaction staging is linear in statement count.
    pub(crate) fn try_extend_prepared_write(
        &mut self,
        previous: &PreparedWrite,
        staged: &[WriteBatch],
        suffix: &WriteBatch,
    ) -> Result<Option<PreparedWrite>, WriteTxnError> {
        self.ensure_writable()?;
        if !Arc::ptr_eq(&self.handle_owner, &previous.handle_owner) {
            return Err(WriteError::ForeignPreparedWrite.into());
        }
        if previous.basis != self.snapshot.frontier || staged.is_empty() || suffix.is_empty() {
            return Ok(None);
        }
        let [coordinate] = previous.template.coordinate_entries() else {
            return Ok(None);
        };
        if coordinate.graph != crate::GRAPH
            || coordinate.branch != crate::BRANCH
            || coordinate.relation != suffix.relation
            || coordinate.schema_epoch != crate::SchemaEpoch(0)
            || coordinate.schema_transition.is_some()
            || suffix.rows.iter().any(|row| !supported(row))
        {
            return Ok(None);
        }

        let mut prefix_visits = 0_u64;
        for batch in staged {
            // Inspect RAW instructions, not just net rows. A create/delete can
            // disappear from the template while permanently spending its ID
            // inside the transaction; a later create must still refuse.
            if batch.relation != suffix.relation || batch.rows.iter().any(|row| !supported(row)) {
                return Ok(None);
            }
            prefix_visits = prefix_visits
                .checked_add(
                    u64::try_from(batch.rows.len())
                        .map_err(|_| WriteTxnError::AtomicOrdinalOverflow)?,
                )
                .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
        }

        let mut needed = BTreeSet::new();
        for row in &suffix.rows {
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
        // A SET/CAS on a prefix-created edge must recreate its endpoints in
        // the small evaluator input, even if the suffix never names them.
        for row in &coordinate.rows {
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

        let mut seed = WriteBatch::new(suffix.relation);
        let mut retained_births = BTreeMap::new();
        let mut untouched = Vec::with_capacity(coordinate.rows.len());
        for row in &coordinate.rows {
            let element = target(row).expect("supported prefix checked above");
            if !needed.contains(&element) {
                untouched.push(row.clone());
                continue;
            }
            let pending = match row {
                DeltaRow::CreateVertex {
                    vid,
                    birth_ordinal,
                    labels,
                    props,
                    valid_time: None,
                } => {
                    if *birth_ordinal > prefix_visits {
                        return Ok(None);
                    }
                    retained_births.insert(element, *birth_ordinal);
                    PendingRow::Vertex {
                        vid: *vid,
                        labels: labels.clone(),
                        props: props.clone(),
                        ensure: false,
                    }
                }
                DeltaRow::CreateEdge {
                    eid,
                    birth_ordinal,
                    src,
                    relation,
                    dst,
                    canonical_key: None,
                    props,
                    valid_time: None,
                } if *relation == suffix.relation => {
                    if *birth_ordinal > prefix_visits {
                        return Ok(None);
                    }
                    retained_births.insert(element, *birth_ordinal);
                    PendingRow::Edge {
                        eid: *eid,
                        src: *src,
                        dst: *dst,
                        props: props.clone(),
                        ensure: false,
                    }
                }
                DeltaRow::LabelMembership {
                    vid, label, after, ..
                } => PendingRow::SetLabel {
                    vid: *vid,
                    label: *label,
                    member: *after,
                },
                DeltaRow::Property {
                    elem: ElementId::Vertex(vid),
                    property,
                    after,
                    ..
                } => PendingRow::SetProperty {
                    vid: *vid,
                    key: *property,
                    value: after.clone(),
                },
                DeltaRow::Property {
                    elem: ElementId::Edge(eid),
                    property,
                    after,
                    ..
                } => PendingRow::SetEdgeProperty {
                    eid: *eid,
                    key: *property,
                    value: after.clone(),
                },
                _ => return Ok(None),
            };
            seed.rows.push(pending);
        }
        let seed_visits =
            u64::try_from(seed.rows.len()).map_err(|_| WriteTxnError::AtomicOrdinalOverflow)?;
        let suffix_visits =
            u64::try_from(suffix.rows.len()).map_err(|_| WriteTxnError::AtomicOrdinalOverflow)?;
        seed.rows.extend(suffix.rows.iter().cloned());

        // The native evaluator admits all new values before liveness/identity
        // checks and validates complete final after-images. All-create seeds
        // continue to use its existing specialized creation path.
        let mut partial = self.prepare_write_checked(seed)?;
        let [partial_coordinate] = partial.template.coordinate_entries() else {
            return Err(WriteTxnError::UnsupportedAtomicMutation);
        };
        for row in &partial_coordinate.rows {
            let mut row = row.clone();
            let birth = match &mut row {
                DeltaRow::CreateVertex {
                    vid, birth_ordinal, ..
                } => Some((ElementId::Vertex(*vid), birth_ordinal)),
                DeltaRow::CreateEdge {
                    eid, birth_ordinal, ..
                } => Some((ElementId::Edge(*eid), birth_ordinal)),
                _ => None,
            };
            if let Some((element, ordinal)) = birth {
                *ordinal = match retained_births.get(&element) {
                    Some(retained) => *retained,
                    None => ordinal
                        .checked_sub(seed_visits)
                        .and_then(|visit| prefix_visits.checked_add(visit))
                        .ok_or(WriteTxnError::AtomicOrdinalOverflow)?,
                };
            }
            untouched.push(row);
        }
        // The replaced and retained sets have disjoint element identities.
        // No delete/cascade can cross that boundary, so only the small input
        // needs NENF folding; the full prefix is merely canonically encoded.
        let next_coordinate = crate::CoordinateEntry {
            graph: coordinate.graph,
            branch: coordinate.branch,
            relation: coordinate.relation,
            schema_epoch: coordinate.schema_epoch,
            schema_transition: coordinate.schema_transition,
            rows: untouched,
        };
        partial.template = crate::LogicalDeltaTemplate::build(
            previous.template.intent_semantics_oid(),
            *previous.template.source_intent_root_digest(),
            vec![next_coordinate],
        )
        .map_err(WriteError::Canonical)?;
        let mut dependencies: PreparedDependencies = previous.dependencies.clone();
        dependencies.extend(partial.dependencies);
        partial.dependencies = dependencies;
        // Seed rows are an evaluation aid, not newly admitted source intents.
        // In particular do not upgrade a prefix whose owner withheld replay.
        partial.replay_row_allowance = previous
            .replay_row_allowance
            .and_then(|visits| visits.checked_add(suffix_visits));
        partial.authorization_decision_digest = previous.authorization_decision_digest;
        Ok(Some(partial))
    }
}

#[cfg(test)]
mod tests;
