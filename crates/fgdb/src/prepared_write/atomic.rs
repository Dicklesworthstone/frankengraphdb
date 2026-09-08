//! Atomic composition of independent relation groups at one common basis.
//!
//! Each group's ordinary builder remains the sole evaluator of its ordered
//! intents. We prove that no group changes anything another observed before
//! placing their canonical effects in one capsule. This is not sequential
//! cross-coordinate evaluation: references to another group's new vertices and
//! overlapping read/write dependencies are refused, never silently reordered.

use super::PreparedDependencies;
use crate::{Database, PreparedWrite, WriteBatch, WriteError, WriteTxnError};
use asupersync::fs::Vfs;
use fgdb_delta_types::{DeltaRow, ElementId, LogicalDeltaTemplate, RelationId};
use fgdb_types::{CommitCx, CommitSeq};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Check symmetric read/write independence without pairwise group comparisons.
/// Read/read sharing is legal, including ordinary edge insertions sharing live
/// endpoints. The ordinary preparation dependency set includes negative reads,
/// conditional no-ops, endpoint liveness and actual ensure aliases.
#[derive(Default)]
struct Independence {
    readers: BTreeMap<ElementId, RelationId>,
    writers: BTreeMap<ElementId, RelationId>,
}

impl Independence {
    fn admit(
        &mut self,
        relation: RelationId,
        reads: &BTreeSet<ElementId>,
        writes: &BTreeSet<ElementId>,
    ) -> Result<(), WriteTxnError> {
        for (elements, previous) in [
            (reads, &self.writers),
            (writes, &self.readers),
            (writes, &self.writers),
        ] {
            for element in elements {
                if let Some(other) = previous.get(element) {
                    return Err(WriteTxnError::AtomicRelationConflict {
                        first: *other,
                        second: relation,
                        element: *element,
                    });
                }
            }
        }
        // Refusal above cannot leave a partially admitted group's footprint.
        for element in reads {
            self.readers.entry(*element).or_insert(relation);
        }
        for element in writes {
            self.writers.insert(*element, relation);
        }
        Ok(())
    }
}

fn written_elements(template: &LogicalDeltaTemplate) -> Result<BTreeSet<ElementId>, WriteTxnError> {
    let mut writes = BTreeSet::new();
    for coordinate in template.coordinate_entries() {
        for row in &coordinate.rows {
            match row {
                DeltaRow::CreateVertex { .. }
                | DeltaRow::CreateEdge { .. }
                | DeltaRow::DeleteVertex { .. }
                | DeltaRow::DeleteEdge { .. }
                | DeltaRow::LabelMembership { .. }
                | DeltaRow::Property { .. } => crate::touched_elements(row, &mut writes),
                // An added semantic arm needs an explicit independence law;
                // unknown effects cannot become a falsely empty footprint.
                _ => return Err(WriteTxnError::UnsupportedAtomicMutation),
            }
        }
    }
    Ok(writes)
}

impl<V: Vfs + Clone> Database<V> {
    /// Prepare one atomic write spanning multiple edge relations.
    ///
    /// Batches with the same relation are concatenated in input order and use
    /// the ordinary ordered evaluator. Different relation groups are evaluated
    /// at the same live basis and must be read/write independent. Sharing live
    /// endpoints for unconstrained edge creations is legal. Mutating a vertex
    /// another group uses, or relying on a vertex created by another group,
    /// refuses. This is not a general multi-statement cross-relation workspace.
    ///
    /// The returned existing `PreparedWrite` commits through `commit_prepared`:
    /// one capsule, one marker, one sequence, and one snapshot publication.
    /// Preparation performs no publication. All groups retain the ordinary
    /// property-size admission, no-op dependencies and history-based FCW guards.
    /// Empty input or an explicitly empty member is refused.
    pub fn prepare_atomic_writes(
        &mut self,
        batches: Vec<WriteBatch>,
    ) -> Result<PreparedWrite, WriteTxnError> {
        self.ensure_writable()?;
        if batches.is_empty() || batches.iter().any(WriteBatch::is_empty) {
            return Err(WriteError::EmptyBatch.into());
        }
        let mut groups: BTreeMap<RelationId, WriteBatch> = BTreeMap::new();
        for batch in batches {
            groups
                .entry(batch.relation)
                .or_insert_with(|| WriteBatch::new(batch.relation))
                .extend(batch)?;
        }
        let mut independence = Independence::default();
        let mut dependencies = PreparedDependencies::default();
        let mut coordinates = Vec::with_capacity(groups.len());
        let mut ordinal_offset = 0_u64;
        for (relation, batch) in groups {
            let visits =
                u64::try_from(batch.len()).map_err(|_| WriteTxnError::AtomicOrdinalOverflow)?;
            let next_offset = ordinal_offset
                .checked_add(visits)
                .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
            let prepared = self.prepare_write_checked(batch)?;
            let writes = written_elements(&prepared.template)?;
            independence.admit(relation, &prepared.dependencies.elements, &writes)?;

            for mut coordinate in prepared.template.coordinate_entries().iter().cloned() {
                // Birth ordinals describe intent visits in this one command,
                // including no-ops. Canonical relation order provides disjoint
                // intervals while preserving each group's internal visit order.
                // NENF has already absorbed same-group create/update/delete
                // sequences, so no surviving delete names a shifted creation.
                for row in &mut coordinate.rows {
                    if let DeltaRow::CreateVertex { birth_ordinal, .. }
                    | DeltaRow::CreateEdge { birth_ordinal, .. } = row
                    {
                        *birth_ordinal = birth_ordinal
                            .checked_add(ordinal_offset)
                            .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
                    }
                }
                coordinates.push(coordinate);
            }
            dependencies.elements.extend(prepared.dependencies.elements);
            dependencies
                .adjacency
                .extend(prepared.dependencies.adjacency);
            ordinal_offset = next_offset;
        }
        // Coordinate and row canonicalization remain owned by the existing
        // format; no new durable encoding or compatibility profile is invented.
        let template =
            LogicalDeltaTemplate::build(crate::intent_semantics_oid(), [0_u8; 32], coordinates)
                .map_err(WriteError::Canonical)?;
        Ok(PreparedWrite {
            template,
            basis: self.snapshot.frontier,
            handle_owner: Arc::clone(&self.handle_owner),
            dependencies,
        })
    }

    /// Prepare and publish independent relation groups as one atomic commit.
    /// The durable tail, cancellation fencing and ambiguous-outcome recovery
    /// are exactly the existing `commit_prepared` path, not a batch-by-batch loop.
    pub async fn write_atomic(
        &mut self,
        cx: &CommitCx,
        batches: Vec<WriteBatch>,
    ) -> Result<CommitSeq, WriteTxnError> {
        let prepared = self.prepare_atomic_writes(batches)?;
        self.commit_prepared(cx, prepared).await.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::{EId, VId};

    #[test]
    fn symmetric_independence_allows_read_sharing_and_refuses_without_mutation() {
        let endpoint = ElementId::Vertex(VId(1));
        let edge_a = ElementId::Edge(EId(10));
        let edge_b = ElementId::Edge(EId(11));
        let mut check = Independence::default();
        check
            .admit(RelationId(1), &[endpoint, edge_a].into(), &[edge_a].into())
            .unwrap();
        check
            .admit(RelationId(2), &[endpoint, edge_b].into(), &[edge_b].into())
            .unwrap();
        let before_readers = check.readers.clone();
        let before_writers = check.writers.clone();
        let rejected = check.admit(RelationId(3), &[endpoint].into(), &[endpoint].into());
        assert!(matches!(
            rejected,
            Err(WriteTxnError::AtomicRelationConflict { .. })
        ));
        assert_eq!(check.readers, before_readers);
        assert_eq!(check.writers, before_writers);
        let rejected = check.admit(RelationId(4), &[edge_a].into(), &BTreeSet::new());
        assert!(matches!(
            rejected,
            Err(WriteTxnError::AtomicRelationConflict { .. })
        ));
        assert_eq!(check.readers, before_readers);
        assert_eq!(check.writers, before_writers);
    }
}
