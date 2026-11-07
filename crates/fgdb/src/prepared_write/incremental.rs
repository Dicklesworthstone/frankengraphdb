//! Reuse an admitted prefix without interpreting it again.
//!
//! Only the prior net effects needed by the new instructions are replayed into
//! the ordinary native evaluator. Deletions and edge ENSURE stay on the full
//! preparation path: their spent identities and adjacency/alias observations
//! cannot be reconstructed from an arbitrary normalized prefix alone.

use super::{PreparedDependencies, RawWriteSummary};
use crate::write_txn::point_effect_selector::{EffectKey, select, select_range};
use crate::{
    Database, DeltaRow, ElementId, PendingRow, PreparedWrite, Vfs, WriteBatch, WriteError,
    WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{EId, VId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

mod ordered;

pub(super) fn supported(row: &PendingRow) -> bool {
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

/// Select the affected canonical ranges without scanning unrelated net rows.
/// The selector's prefix order is already checked against the durable encoder
/// by the point-projection laws. Keep the selected rows in the SAME canonical
/// order as a complete filtered scan: births, memberships, then properties.
/// In particular, all endpoint births precede their edges in the native seed.
fn prefix_seed_rows<'a, E>(
    rows: &'a [DeltaRow],
    needed: &mut BTreeSet<ElementId>,
    poll: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<Vec<&'a DeltaRow>>, E> {
    poll()?;
    // Later families sort after every supported family. Retirements form one
    // contiguous interval. Test both before accepting any affected subset;
    // an unsupported unrelated row must still request full preparation.
    if rows
        .last()
        .is_some_and(|row| EffectKey::of(row) == EffectKey::Other)
        || !select_range(
            rows,
            EffectKey::DeleteVertex(VId(0)),
            EffectKey::DeleteEdge(EId(u128::MAX)),
            poll,
        )?
        .is_empty()
    {
        return Ok(None);
    }

    // An identity-addressed SET/CAS may name a prefix-created edge without
    // naming either endpoint. Recreate those endpoints in the same small seed.
    let mut endpoints = BTreeSet::new();
    for &element in needed.iter() {
        if let ElementId::Edge(eid) = element {
            for row in select(rows, EffectKey::CreateEdge(eid), poll)? {
                poll()?;
                if let DeltaRow::CreateEdge { src, dst, .. } = row {
                    endpoints.insert(*src);
                    endpoints.insert(*dst);
                }
            }
        }
    }
    needed.extend(endpoints.into_iter().map(ElementId::Vertex));

    let mut selected = Vec::new();
    let mut append = |first, last| -> Result<(), E> {
        for row in select_range(rows, first, last, poll)? {
            poll()?;
            selected.push(row);
        }
        Ok(())
    };
    for &element in needed.iter() {
        if let ElementId::Vertex(vid) = element {
            let key = EffectKey::CreateVertex(vid);
            append(key, key)?;
        }
    }
    for &element in needed.iter() {
        if let ElementId::Edge(eid) = element {
            let key = EffectKey::CreateEdge(eid);
            append(key, key)?;
        }
    }
    for &element in needed.iter() {
        if let ElementId::Vertex(vid) = element {
            append(
                EffectKey::Label(vid, LabelId(0)),
                EffectKey::Label(vid, LabelId(u64::MAX)),
            )?;
        }
    }
    for &element in needed.iter() {
        append(
            EffectKey::Property(element, PropertyKeyId(0)),
            EffectKey::Property(element, PropertyKeyId(u64::MAX)),
        )?;
    }
    Ok(Some(selected))
}

/// Reconstruct a supported net effect as an ordinary evaluator instruction.
/// The optional ordinal belongs to an already exposed birth, not this replay.
fn seed_row(
    row: &DeltaRow,
    relation: crate::RelationId,
    prefix_visits: u64,
) -> Option<(PendingRow, Option<u64>)> {
    let (pending, birth) = match row {
        DeltaRow::CreateVertex {
            vid,
            birth_ordinal,
            labels,
            props,
            valid_time: None,
        } if *birth_ordinal <= prefix_visits => (
            PendingRow::Vertex {
                vid: *vid,
                labels: labels.clone(),
                props: props.clone(),
                ensure: false,
            },
            Some(*birth_ordinal),
        ),
        DeltaRow::CreateEdge {
            eid,
            birth_ordinal,
            src,
            relation: edge_relation,
            dst,
            canonical_key: None,
            props,
            valid_time: None,
        } if *edge_relation == relation && *birth_ordinal <= prefix_visits => (
            PendingRow::Edge {
                eid: *eid,
                src: *src,
                dst: *dst,
                props: props.clone(),
                ensure: false,
            },
            Some(*birth_ordinal),
        ),
        DeltaRow::LabelMembership {
            vid, label, after, ..
        } => (
            PendingRow::SetLabel {
                vid: *vid,
                label: *label,
                member: *after,
            },
            None,
        ),
        DeltaRow::Property {
            elem: ElementId::Vertex(vid),
            property,
            after,
            ..
        } => (
            PendingRow::SetProperty {
                vid: *vid,
                key: *property,
                value: after.clone(),
            },
            None,
        ),
        DeltaRow::Property {
            elem: ElementId::Edge(eid),
            property,
            after,
            ..
        } => (
            PendingRow::SetEdgeProperty {
                eid: *eid,
                key: *property,
                value: after.clone(),
            },
            None,
        ),
        _ => return None,
    };
    Some((pending, birth))
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

        let prefix_visits = match previous.raw_input {
            // These facts were recorded from the original raw input, not its
            // smaller canonical template. A retained prefix need not visit
            // every old instruction again just to recover the same facts.
            Some(summary) if summary.relation == suffix.relation && summary.incremental => {
                summary.visits
            }
            Some(_) => return Ok(None),
            None => {
                // Composed preparations have no single raw-input proof. Keep
                // the original validation, including erased retirements and
                // ENSURE aliases; never infer eligibility from net rows alone.
                let mut visits = 0_u64;
                for batch in staged {
                    if batch.relation != suffix.relation
                        || batch.rows.iter().any(|row| !supported(row))
                    {
                        return Ok(None);
                    }
                    visits = visits
                        .checked_add(
                            u64::try_from(batch.rows.len())
                                .map_err(|_| WriteTxnError::AtomicOrdinalOverflow)?,
                        )
                        .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
                }
                visits
            }
        };

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
        let selected = match prefix_seed_rows(&coordinate.rows, &mut needed, &mut || {
            Ok::<(), core::convert::Infallible>(())
        }) {
            Ok(Some(selected)) => selected,
            Ok(None) => return Ok(None),
            Err(never) => match never {},
        };

        let mut seed = WriteBatch::new(suffix.relation);
        let mut retained_births = BTreeMap::new();
        for row in selected {
            let element = target(row).expect("supported prefix selected above");
            let Some((pending, birth)) = seed_row(row, suffix.relation, prefix_visits) else {
                return Ok(None);
            };
            if let Some(birth) = birth {
                retained_births.insert(element, birth);
            }
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
        let mut next_rows = Vec::with_capacity(partial_coordinate.rows.len());
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
            next_rows.push(row);
        }
        // The replaced and retained sets have disjoint element identities.
        // No delete/cascade can cross that boundary, so only the small input
        // needs NENF folding. Retained canonical rows need no full re-sort or
        // payload encoding pass; their owned copies are still composed here.
        let Some(template) = previous
            .template
            .replace_coordinate_rows(
                coordinate.coordinate(),
                |row| !needed.contains(&target(row).expect("supported prefix checked above")),
                next_rows,
            )
            .map_err(WriteError::Canonical)?
        else {
            return Ok(None);
        };
        partial.template = template;
        let mut dependencies: PreparedDependencies = previous.dependencies.clone();
        dependencies.extend(partial.dependencies);
        partial.dependencies = dependencies;
        // Seed rows are an evaluation aid, not newly admitted source intents.
        // In particular do not upgrade a prefix whose owner withheld replay.
        partial.replay_row_allowance = previous
            .replay_row_allowance
            .and_then(|visits| visits.checked_add(suffix_visits));
        // The seed is only an evaluator input. Its row count must not become
        // the provenance for the next suffix or its original birth ordinals.
        partial.raw_input =
            prefix_visits
                .checked_add(suffix_visits)
                .map(|visits| RawWriteSummary {
                    relation: suffix.relation,
                    visits,
                    incremental: true,
                });
        partial.authorization_decision_digest = previous.authorization_decision_digest;
        Ok(Some(partial))
    }
}

#[cfg(test)]
mod tests;
