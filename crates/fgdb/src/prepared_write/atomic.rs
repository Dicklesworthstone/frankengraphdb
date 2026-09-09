//! Atomic composition of relation groups through the ordinary write builder.
//!
//! Independent groups retain their existing common-basis contract. A leading
//! prefix of vertex creations/ensures can additionally seed new endpoints
//! shared by several relations. Every suffix sees that same immutable prefix;
//! groups must leave it unchanged and remain mutually read/write independent.
//! Prefix effects are emitted once, before any dependent edge coordinate.

use super::PreparedDependencies;
use crate::{Database, PendingRow, PreparedWrite, WriteBatch, WriteError, WriteTxnError};
use asupersync::fs::Vfs;
use fgdb_delta_types::{CoordinateEntry, DeltaRow, ElementId, LogicalDeltaTemplate, RelationId};
use fgdb_types::{CommitCx, CommitSeq, VId};
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

/// Detect an explicit vertex-initialization prefix, never hoist a later
/// creation past an edge, condition, update or delete. Independent operations
/// stay on their original path, including their exact ordinal/coordinate law.
/// Only a cross-relation dependency on an absent endpoint activates prefix
/// composition. Already-live ensured endpoints retain the independent path.
struct SharedVertexPrefix {
    rows: Vec<PendingRow>,
    origins: BTreeMap<VId, RelationId>,
}

impl SharedVertexPrefix {
    fn discover(batches: &[WriteBatch], is_live: impl Fn(VId) -> bool) -> Option<Self> {
        let mut count = 0;
        let mut origins = BTreeMap::new();
        'prefix: for batch in batches {
            for row in &batch.rows {
                let PendingRow::Vertex { vid, .. } = row else {
                    break 'prefix;
                };
                count += 1;
                // Keep duplicate raw creations: ordinary preparation, not the
                // discovery pass, must reject them as AlreadyLive.
                origins.entry(*vid).or_insert(batch.relation);
            }
        }
        let crosses = batches.iter().any(|batch| {
            batch.rows.iter().any(|row| match row {
                PendingRow::Edge { src, dst, .. } => [src, dst].into_iter().any(|vid| {
                    origins.get(vid).is_some_and(|origin| *origin != batch.relation)
                        && !is_live(*vid)
                }),
                _ => false,
            })
        });
        crosses.then(|| Self {
            rows: batches.iter().flat_map(|batch| &batch.rows).take(count).cloned().collect(),
            origins,
        })
    }

    fn owns(&self, element: &ElementId) -> bool {
        matches!(element, ElementId::Vertex(vid) if self.origins.contains_key(vid))
    }

    fn conflict(&self, vid: VId, relation: RelationId) -> WriteTxnError {
        WriteTxnError::AtomicRelationConflict {
            first: self.origins[&vid],
            second: relation,
            element: ElementId::Vertex(vid),
        }
    }

    /// Verify the COMPLETE common prefix after each ordinary NENF evaluation.
    /// Checking only surviving creates would miss a suffix that erased one.
    /// Prefix metadata/content must match exactly, not just its identity.
    fn strip_verified_prefix(
        &self,
        template: &LogicalDeltaTemplate,
        expected: &BTreeMap<VId, DeltaRow>,
        relation: RelationId,
        suffix_offset: u64,
    ) -> Result<Vec<CoordinateEntry>, WriteTxnError> {
        let mut seen = BTreeSet::new();
        let mut coordinates = Vec::new();
        for mut coordinate in template.coordinate_entries().iter().cloned() {
            let mut suffix = Vec::new();
            for mut row in coordinate.rows {
                if let DeltaRow::CreateVertex { vid, .. } = &row
                    && let Some(original) = expected.get(vid)
                {
                    if &row != original || !seen.insert(*vid) {
                        return Err(self.conflict(*vid, relation));
                    }
                    continue;
                }
                let mut touched = BTreeSet::new();
                crate::touched_elements(&row, &mut touched);
                if let Some(ElementId::Vertex(vid)) = touched.iter().find(|elem| self.owns(elem)) {
                    return Err(self.conflict(*vid, relation));
                }
                if let DeltaRow::CreateVertex { birth_ordinal, .. }
                | DeltaRow::CreateEdge { birth_ordinal, .. } = &mut row
                {
                    *birth_ordinal = birth_ordinal.checked_add(suffix_offset)
                        .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
                }
                suffix.push(row);
            }
            coordinate.rows = suffix;
            coordinates.push(coordinate);
        }
        if let Some(vid) = expected.keys().find(|vid| !seen.contains(*vid)) {
            return Err(self.conflict(*vid, relation));
        }
        Ok(coordinates)
    }
}

/// The bounded engine has one graph/branch/schema. Still compare the entire
/// coordinate header before joining payloads: future bindings may not silently
/// collapse merely because their relation IDs coincide.
fn merge_coordinate(
    coordinates: &mut BTreeMap<RelationId, CoordinateEntry>,
    coordinate: CoordinateEntry,
) -> Result<(), WriteTxnError> {
    use std::collections::btree_map::Entry;
    match coordinates.entry(coordinate.relation) {
        Entry::Vacant(slot) => { slot.insert(coordinate); }
        Entry::Occupied(mut slot) => {
            let previous = slot.get_mut();
            if previous.graph != coordinate.graph || previous.branch != coordinate.branch
                || previous.schema_epoch != coordinate.schema_epoch
                || previous.schema_transition != coordinate.schema_transition
            {
                return Err(WriteTxnError::UnsupportedAtomicMutation);
            }
            previous.rows.extend(coordinate.rows);
        }
    }
    Ok(())
}

impl<V: Vfs + Clone> Database<V> {
    /// Prepare one atomic write spanning multiple edge relations.
    ///
    /// Independent relation groups keep their original common-basis semantics.
    /// A leading run of vertex creations/ensures can also supply new
    /// endpoints to other relation groups. It must precede every non-creation
    /// intent in the input. Every group evaluates through the ordinary builder
    /// against that same prefix, must leave its canonical vertex content
    /// unchanged, and must remain independent of the other suffix groups.
    /// This admits atomic graph initialization, not arbitrary cross-relation
    /// statement interleaving, cross-group updates or dependency reordering.
    ///
    /// One existing PreparedWrite owns all effects and dependencies; commit
    /// publishes one capsule, marker, sequence and snapshot. Empty input and
    /// empty members refuse before preparation. No part publishes on refusal.
    pub fn prepare_atomic_writes(
        &mut self,
        batches: Vec<WriteBatch>,
    ) -> Result<PreparedWrite, WriteTxnError> {
        self.ensure_writable()?;
        if batches.is_empty() || batches.iter().any(WriteBatch::is_empty) {
            return Err(WriteError::EmptyBatch.into());
        }
        if let Some(prefix) = SharedVertexPrefix::discover(&batches, |vid| self.writer.is_vertex_live(vid)) {
            return self.prepare_vertex_prefixed_groups(batches, prefix);
        }
        let mut groups: BTreeMap<RelationId, WriteBatch> = BTreeMap::new();
        for batch in batches {
            groups.entry(batch.relation).or_insert_with(|| WriteBatch::new(batch.relation))
                .extend(batch)?;
        }
        let mut independence = Independence::default();
        let mut dependencies = PreparedDependencies::default();
        let mut coordinates = Vec::with_capacity(groups.len());
        let mut ordinal_offset = 0_u64;
        for (relation, batch) in groups {
            let visits = u64::try_from(batch.len()).map_err(|_| WriteTxnError::AtomicOrdinalOverflow)?;
            let next_offset = ordinal_offset.checked_add(visits)
                .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
            let prepared = self.prepare_write_checked(batch)?;
            let writes = written_elements(&prepared.template)?;
            independence.admit(relation, &prepared.dependencies.elements, &writes)?;
            for mut coordinate in prepared.template.coordinate_entries().iter().cloned() {
                // Original independent-group ordinal law is unchanged.
                for row in &mut coordinate.rows {
                    if let DeltaRow::CreateVertex { birth_ordinal, .. }
                    | DeltaRow::CreateEdge { birth_ordinal, .. } = row {
                        *birth_ordinal = birth_ordinal.checked_add(ordinal_offset)
                            .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
                    }
                }
                coordinates.push(coordinate);
            }
            dependencies.elements.extend(prepared.dependencies.elements);
            dependencies.adjacency.extend(prepared.dependencies.adjacency);
            ordinal_offset = next_offset;
        }
        self.finish_atomic_preparation(coordinates, dependencies)
    }

    fn prepare_vertex_prefixed_groups(
        &mut self,
        batches: Vec<WriteBatch>,
        prefix: SharedVertexPrefix,
    ) -> Result<PreparedWrite, WriteTxnError> {
        // Vertex identities are graph-wide in this slice, not relation-local.
        // The least coordinate must own the SINGLE prefix payload so canonical
        // replay creates all endpoints before any later relation's edges.
        let owner = batches.iter().map(|batch| batch.relation).min()
            .ok_or(WriteError::EmptyBatch)?;
        let prefix_visits = u64::try_from(prefix.rows.len())
            .map_err(|_| WriteTxnError::AtomicOrdinalOverflow)?;
        let prefix_prepared = self.prepare_write_checked(WriteBatch {
            relation: owner, rows: prefix.rows.clone(),
        })?;
        let mut expected = BTreeMap::new();
        let mut coordinates = BTreeMap::new();
        for coordinate in prefix_prepared.template.coordinate_entries().iter().cloned() {
            for row in &coordinate.rows {
                let DeltaRow::CreateVertex { vid, .. } = row else {
                    return Err(WriteTxnError::UnsupportedAtomicMutation);
                };
                if !prefix.origins.contains_key(vid) {
                    return Err(WriteTxnError::UnsupportedAtomicMutation);
                }
                expected.insert(*vid, row.clone());
            }
            merge_coordinate(&mut coordinates, coordinate)?;
        }
        // Ensures of live or earlier prefix-created vertices may be no-ops.
        // Their raw visits still count, and their observations remain captured.
        let mut dependencies = prefix_prepared.dependencies;
        let mut groups = BTreeMap::new();
        let mut remaining_prefix = prefix.rows.len();
        for batch in batches {
            let suffix = groups.entry(batch.relation)
                .or_insert_with(|| WriteBatch::new(batch.relation));
            for row in batch.rows {
                if remaining_prefix > 0 { remaining_prefix -= 1; }
                else { suffix.rows.push(row); }
            }
        }
        let mut independence = Independence::default();
        let mut suffix_offset = 0_u64;
        for (relation, mut suffix) in groups {
            if suffix.is_empty() { continue; }
            let visits = u64::try_from(suffix.len()).map_err(|_| WriteTxnError::AtomicOrdinalOverflow)?;
            let next_offset = suffix_offset.checked_add(visits)
                .filter(|offset| prefix_visits.checked_add(*offset).is_some())
                .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
            let mut batch = WriteBatch { relation, rows: prefix.rows.clone() };
            batch.rows.append(&mut suffix.rows);
            // No shadow database and no second mutation evaluator: the same
            // builder resolves order, ensures, CAS, admission and net effects.
            let prepared = self.prepare_write_checked(batch)?;
            let mut writes = written_elements(&prepared.template)?;
            let stripped = prefix.strip_verified_prefix(
                &prepared.template, &expected, relation, suffix_offset,
            )?;
            // Every group has just proved that the common prefix is immutable.
            // Its repeated evaluation is not a write/write conflict between
            // suffixes. KEEP these negative reads in the final external FCW
            // dependencies; remove them only for this intra-command check.
            let created_by_prefix = |element: &ElementId| {
                matches!(element, ElementId::Vertex(vid) if expected.contains_key(vid))
            };
            writes.retain(|element| !created_by_prefix(element));
            let reads = prepared.dependencies.elements.iter().copied()
                .filter(|element| !created_by_prefix(element)).collect();
            independence.admit(relation, &reads, &writes)?;
            for coordinate in stripped { merge_coordinate(&mut coordinates, coordinate)?; }
            dependencies.elements.extend(prepared.dependencies.elements);
            dependencies.adjacency.extend(prepared.dependencies.adjacency);
            suffix_offset = next_offset;
        }
        self.finish_atomic_preparation(coordinates.into_values().collect(), dependencies)
    }

    fn finish_atomic_preparation(
        &self,
        coordinates: Vec<CoordinateEntry>,
        dependencies: PreparedDependencies,
    ) -> Result<PreparedWrite, WriteTxnError> {
        let template = LogicalDeltaTemplate::build(
            crate::intent_semantics_oid(), [0_u8; 32], coordinates,
        ).map_err(WriteError::Canonical)?;
        Ok(PreparedWrite {
            template, basis: self.snapshot.frontier,
            handle_owner: Arc::clone(&self.handle_owner), dependencies,
        })
    }

    /// Prepare and publish the admitted relation groups as one atomic commit.
    /// The durable tail, cancellation fencing and ambiguous-outcome recovery
    /// are the existing commit_prepared path, never a batch-by-batch loop.
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
    use fgdb_types::EId;

    #[test]
    fn symmetric_independence_allows_read_sharing_and_refuses_without_mutation() {
        let endpoint = ElementId::Vertex(VId(1));
        let edge_a = ElementId::Edge(EId(10));
        let edge_b = ElementId::Edge(EId(11));
        let mut check = Independence::default();
        check.admit(RelationId(1), &[endpoint, edge_a].into(), &[edge_a].into()).unwrap();
        check.admit(RelationId(2), &[endpoint, edge_b].into(), &[edge_b].into()).unwrap();
        let before_readers = check.readers.clone();
        let before_writers = check.writers.clone();
        let rejected = check.admit(RelationId(3), &[endpoint].into(), &[endpoint].into());
        assert!(matches!(rejected, Err(WriteTxnError::AtomicRelationConflict { .. })));
        assert_eq!(check.readers, before_readers);
        assert_eq!(check.writers, before_writers);
        let rejected = check.admit(RelationId(4), &[edge_a].into(), &BTreeSet::new());
        assert!(matches!(rejected, Err(WriteTxnError::AtomicRelationConflict { .. })));
        assert_eq!(check.readers, before_readers);
        assert_eq!(check.writers, before_writers);
    }

    #[test]
    fn shared_prefix_is_explicit_and_never_hoists_a_later_creation() {
        let mut vertices = WriteBatch::new(RelationId(9));
        vertices.create_vertex(VId(1), vec![], vec![]);
        vertices.create_vertex(VId(2), vec![], vec![]);
        let mut edge = WriteBatch::new(RelationId(1));
        edge.add_edge(EId(10), VId(1), VId(2), vec![]);
        let prefix = SharedVertexPrefix::discover(&[vertices.clone(), edge.clone()], |_| false).unwrap();
        assert_eq!(prefix.rows.len(), 2);
        assert_eq!(prefix.origins[&VId(1)], RelationId(9));
        assert!(SharedVertexPrefix::discover(&[edge, vertices], |_| false).is_none());
    }

    #[test]
    fn independent_and_single_relation_shapes_keep_the_original_path() {
        let mut a = WriteBatch::new(RelationId(1));
        a.create_vertex(VId(1), vec![], vec![]);
        a.create_vertex(VId(2), vec![], vec![]);
        a.add_edge(EId(10), VId(1), VId(2), vec![]);
        let mut b = WriteBatch::new(RelationId(2));
        b.create_vertex(VId(3), vec![], vec![]);
        b.create_vertex(VId(4), vec![], vec![]);
        b.add_edge(EId(11), VId(3), VId(4), vec![]);
        assert!(SharedVertexPrefix::discover(&[a.clone()], |_| false).is_none());
        assert!(SharedVertexPrefix::discover(&[a, b], |_| false).is_none());
    }

    #[test]
    fn ensured_endpoints_select_the_shared_path_only_when_not_already_live() {
        let mut first = WriteBatch::new(RelationId(9));
        first.ensure_vertex(VId(1), vec![], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        let mut next = WriteBatch::new(RelationId(1));
        next.add_edge(EId(10), VId(1), VId(2), vec![]);
        let batches = [first, next];
        assert!(SharedVertexPrefix::discover(&batches, |_| true).is_none());
        let prefix = SharedVertexPrefix::discover(&batches, |_| false).unwrap();
        assert_eq!(prefix.rows.len(), 2);
    }
}
