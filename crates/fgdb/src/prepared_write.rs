//! Prepared writes retain their observations, not a resettable validator epoch.

mod at_basis;
mod atomic;
mod incremental;
mod ordered;

pub(super) use at_basis::PreparationAnchor;

use crate::gql_exec::source::{IndexIter, IndexMap};
use crate::{
    CommitCx, CommitSeq, CrashPoint, Database, ElementId, FirstCommitterWinsValidator, PendingRow,
    PreparedWrite, VId, WriteBatch, WriteError,
};
use asupersync::fs::Vfs;
use fgdb_strata::writer::BlockWriter;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Facts about the exact raw input accepted by ordinary preparation. This is
/// neither a replay allowance nor a description inferred from net effects:
/// even a cancelled create/delete or an unused edge ENSURE makes the incremental
/// family proof unavailable. Composed templates without that input keep None.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RawWriteSummary {
    pub(super) relation: crate::RelationId,
    pub(super) visits: u64,
    pub(super) incremental: bool,
}

impl RawWriteSummary {
    fn for_batch(batch: &WriteBatch) -> Option<Self> {
        Some(Self {
            relation: batch.relation,
            visits: u64::try_from(batch.rows.len()).ok()?,
            incremental: batch.rows.iter().all(incremental::supported),
        })
    }
}

/// Captures keep the ordinary mutable set until a prepared prefix is shared.
/// Later observations copy only AVL search paths, preserving pinned prefixes.
#[derive(Clone)]
enum DependencySet<K> {
    Flat(Arc<BTreeSet<K>>),
    Shared(IndexMap<K, ()>),
}

impl<K> Default for DependencySet<K> {
    fn default() -> Self {
        Self::Flat(Arc::new(BTreeSet::new()))
    }
}

impl<K: Ord + Clone> DependencySet<K> {
    fn len(&self) -> usize {
        match self {
            Self::Flat(keys) => keys.len(),
            Self::Shared(keys) => keys.len(),
        }
    }

    fn contains(&self, key: &K) -> bool {
        match self {
            Self::Flat(keys) => keys.contains(key),
            Self::Shared(keys) => keys.get(key).is_some(),
        }
    }

    fn insert(&mut self, key: K) -> bool {
        if let Self::Flat(keys) = self
            && let Some(keys) = Arc::get_mut(keys)
        {
            return keys.insert(key);
        }
        if self.contains(&key) {
            return false;
        }
        // The AVL's compact length is a representation bound, not a limit
        // on dependency observations. Larger sets retain the ordinary tree.
        const AVL_CAPACITY: usize = u32::MAX as usize;
        match self {
            Self::Flat(keys) if keys.len() < AVL_CAPACITY => {
                let mut work = 0;
                let root = IndexMap::from_sorted(
                    keys.iter().cloned().map(|key| (key, ())).collect(),
                    &mut work,
                );
                *self = Self::Shared(root.insert(key, (), &mut work));
            }
            Self::Flat(keys) => {
                Arc::make_mut(keys).insert(key);
            }
            Self::Shared(root) if root.len() < AVL_CAPACITY => {
                *root = root.insert(key, (), &mut 0);
            }
            Self::Shared(root) => {
                let mut keys: BTreeSet<_> = root.iter().map(|(key, _)| key.clone()).collect();
                keys.insert(key);
                *self = Self::Flat(Arc::new(keys));
            }
        }
        true
    }

    fn iter(&self) -> DependencyIter<'_, K> {
        match self {
            Self::Flat(keys) => DependencyIter::Flat(keys.iter()),
            Self::Shared(keys) => DependencyIter::Shared(keys.iter()),
        }
    }
}

impl<K: Ord + Clone> Extend<K> for DependencySet<K> {
    fn extend<T: IntoIterator<Item = K>>(&mut self, keys: T) {
        for key in keys {
            self.insert(key);
        }
    }
}

impl<K: Ord + Clone> PartialEq for DependencySet<K> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl<K: Ord + Clone> Eq for DependencySet<K> {}

impl<K: Ord + Clone> core::fmt::Debug for DependencySet<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DependencySet")
            .field("count", &self.len())
            .field("identities", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone)]
enum DependencyIter<'a, K> {
    Flat(std::collections::btree_set::Iter<'a, K>),
    Shared(IndexIter<'a, K, ()>),
}

impl<'a, K> Iterator for DependencyIter<'a, K> {
    type Item = &'a K;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Flat(keys) => keys.next(),
            Self::Shared(keys) => keys.next().map(|(key, _)| key),
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct PreparedDependencies {
    elements: DependencySet<ElementId>,
    adjacency: DependencySet<VId>,
}

impl core::fmt::Debug for PreparedDependencies {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedDependencies")
            .field("element_count", &self.elements.len())
            .field("adjacency_count", &self.adjacency.len())
            .field("identities", &"[REDACTED]")
            .finish()
    }
}

impl PreparedDependencies {
    /// Borrow the same conservative observations used by rollback, allowing
    /// callers to checkpoint each identity without cloning a dependency set.
    pub(super) fn observations(&self) -> impl Iterator<Item = ElementId> + '_ {
        self.elements
            .iter()
            .copied()
            .chain(self.adjacency.iter().copied().map(ElementId::Vertex))
    }

    pub(super) fn extend(&mut self, other: Self) {
        self.elements.extend(other.elements.iter().copied());
        self.adjacency.extend(other.adjacency.iter().copied());
    }

    pub(super) fn capture(writer: &BlockWriter, batch: &WriteBatch) -> Self {
        let mut result = Self::default();
        for pending in &batch.rows {
            match pending {
                PendingRow::Vertex { vid, .. }
                | PendingRow::SetLabel { vid, .. }
                | PendingRow::SetProperty { vid, .. } => {
                    result.elements.insert(ElementId::Vertex(*vid));
                }
                PendingRow::DeleteVertex { vid, .. } => {
                    result.elements.insert(ElementId::Vertex(*vid));
                    result.adjacency.insert(*vid);
                    result.elements.extend(
                        writer
                            .live_incident_edges(*vid)
                            .into_iter()
                            .map(ElementId::Edge),
                    );
                }
                PendingRow::Edge {
                    eid,
                    src,
                    dst,
                    ensure,
                    ..
                } => {
                    result.elements.insert(ElementId::Edge(*eid));
                    result.elements.insert(ElementId::Vertex(*src));
                    result.elements.insert(ElementId::Vertex(*dst));
                    if *ensure {
                        // Preserve both the absent-triple insertion witness and
                        // every currently satisfying alias. Prefix-created aliases
                        // are already named by earlier PendingRow::Edge records.
                        result.adjacency.insert(*src);
                        for existing in writer.live_incident_edges(*src) {
                            if let Some((s, relation, d, _)) = writer.live_edge(existing)
                                && s == *src
                                && d == *dst
                                && relation == batch.relation
                            {
                                result.elements.insert(ElementId::Edge(existing));
                            }
                        }
                    }
                }
                PendingRow::DeleteEdge { eid, .. } | PendingRow::SetEdgeProperty { eid, .. } => {
                    result.elements.insert(ElementId::Edge(*eid));
                }
                PendingRow::CompareAndSet { elem, .. } => {
                    result.elements.insert(*elem);
                }
            }
        }
        result
    }

    /// A refused preparation still observed its basis. Preserve its
    /// conservative footprint when the caller rolls back the attempted effects.
    pub(super) fn retain_observations(self, observations: &mut BTreeSet<ElementId>) {
        // Transaction validation checks adjacency-changing writes against
        // endpoint vertex observations, including previously absent edges.
        observations.extend(self.observations());
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Independent creations read no earlier chunk. Endpoints are either
    /// created in this chunk or unchanged basis vertices. Retain both identity
    /// sets for ordinary duplicate refusals while admitting values and storage
    /// content once. Earlier vertices are never candidate endpoints.
    pub(super) fn prepare_independent_create_chunk(
        &mut self,
        batch: WriteBatch,
        earlier_vertices: &fgdb_gql::id_hash::IdSet<VId>,
        earlier_edges: &fgdb_gql::id_hash::IdSet<crate::EId>,
    ) -> Result<crate::LogicalDeltaTemplate, WriteError> {
        self.ensure_writable()?;
        if batch.is_empty() {
            return Err(WriteError::EmptyBatch);
        }
        debug_assert!(batch.rows.iter().all(|row| matches!(
            row,
            PendingRow::Vertex { ensure: false, .. } | PendingRow::Edge { ensure: false, .. }
        )));
        // Ordinary preparation admits every supplied value before checking
        // identities, including collisions with earlier successful statements.
        self.admit_write_values(&batch)?;
        self.build_create_template_with_prefix(batch, Some(earlier_vertices), Some(earlier_edges))
    }

    /// Prepare only the next edge-creation chunk of a program whose vertex
    /// selections cannot observe any earlier chunk. The program owner keeps
    /// both successful and refused preparation dependencies separately.
    pub(super) fn prepare_independent_edge_chunk(
        &mut self,
        batch: WriteBatch,
        earlier_edges: &fgdb_gql::id_hash::IdSet<crate::EId>,
    ) -> Result<crate::LogicalDeltaTemplate, WriteError> {
        self.ensure_writable()?;
        if batch.is_empty() {
            return Err(WriteError::EmptyBatch);
        }
        debug_assert!(
            batch
                .rows
                .iter()
                .all(|row| matches!(row, PendingRow::Edge { ensure: false, .. }))
        );
        // This complete input pass must still precede every identity/liveness
        // check, including a collision with an earlier program chunk.
        self.admit_write_values(&batch)?;
        self.build_create_template_with_prefix(batch, None, Some(earlier_edges))
    }

    /// One ordinary canonical authority for already-admitted independent
    /// chunks. No row is re-derived from storage, and no earlier prefix is
    /// cloned or sorted while another program statement is being evaluated.
    /// A fixed relation allows independent vertex and edge creations. Without one,
    /// every chunk must contain only fresh edges over unchanged basis vertices.
    pub(super) fn compose_independent_create_chunks(
        &self,
        relation: Option<crate::RelationId>,
        chunks: &[crate::LogicalDeltaTemplate],
        dependencies: PreparedDependencies,
        replay_row_allowance: Option<u64>,
    ) -> Result<PreparedWrite, crate::WriteTxnError> {
        let mut coordinates = BTreeMap::new();
        if let Some(relation) = relation {
            // Preserve the fixed-coordinate seam even for an empty chunk set.
            // Real independent programs compose only nonempty create chunks.
            coordinates.insert(
                relation,
                crate::CoordinateEntry {
                    graph: crate::GRAPH,
                    branch: crate::BRANCH,
                    relation,
                    schema_epoch: crate::SchemaEpoch(0),
                    schema_transition: None,
                    rows: Vec::new(),
                },
            );
        }
        let mut offset = 0_u64;
        for chunk in chunks {
            let [coordinate] = chunk.coordinate_entries() else {
                return Err(crate::WriteTxnError::UnsupportedAtomicMutation);
            };
            if coordinate.graph != crate::GRAPH
                || coordinate.branch != crate::BRANCH
                || relation.is_some_and(|relation| coordinate.relation != relation)
                || coordinate.schema_epoch != crate::SchemaEpoch(0)
                || coordinate.schema_transition.is_some()
            {
                return Err(crate::WriteTxnError::UnsupportedAtomicMutation);
            }
            for row in &coordinate.rows {
                let mut row = row.clone();
                let birth_ordinal = match &mut row {
                    crate::DeltaRow::CreateVertex { birth_ordinal, .. } if relation.is_some() => {
                        birth_ordinal
                    }
                    crate::DeltaRow::CreateEdge { birth_ordinal, .. } => birth_ordinal,
                    _ => return Err(crate::WriteTxnError::UnsupportedAtomicMutation),
                };
                *birth_ordinal = birth_ordinal
                    .checked_add(offset)
                    .ok_or(crate::WriteTxnError::AtomicOrdinalOverflow)?;
                coordinates
                    .entry(coordinate.relation)
                    .or_insert_with(|| crate::CoordinateEntry {
                        graph: coordinate.graph,
                        branch: coordinate.branch,
                        relation: coordinate.relation,
                        schema_epoch: coordinate.schema_epoch,
                        schema_transition: None,
                        rows: Vec::new(),
                    })
                    .rows
                    .push(row);
            }
            offset = offset
                .checked_add(
                    u64::try_from(coordinate.rows.len())
                        .map_err(|_| crate::WriteTxnError::AtomicOrdinalOverflow)?,
                )
                .ok_or(crate::WriteTxnError::AtomicOrdinalOverflow)?;
        }
        let template = crate::LogicalDeltaTemplate::build(
            crate::intent_semantics_oid(),
            [0_u8; 32],
            coordinates.into_values().collect(),
        )
        .map_err(WriteError::Canonical)?;
        Ok(PreparedWrite {
            template,
            basis: self.snapshot.frontier,
            handle_owner: Arc::clone(&self.handle_owner),
            dependencies,
            replay_row_allowance,
            raw_input: None,
            authorization_decision_digest: None,
        })
    }

    pub(super) fn prepare_write_checked(
        &mut self,
        batch: WriteBatch,
    ) -> Result<PreparedWrite, WriteError> {
        self.ensure_writable()?;
        // Capture before canonicalization erases conditional no-ops. All data
        // comes from the same admitted live writer used to derive the template.
        let mut dependencies = PreparedDependencies::capture(&self.writer, &batch);
        let replay_row_allowance = u64::try_from(batch.rows.len()).ok();
        let raw_input = RawWriteSummary::for_batch(&batch);
        let template = self.build_write_template(batch)?;
        for coordinate in template.coordinate_entries() {
            for row in &coordinate.rows {
                crate::touched_elements(row, &mut dependencies.elements);
            }
        }
        Ok(PreparedWrite {
            template,
            basis: self.snapshot.frontier,
            handle_owner: Arc::clone(&self.handle_owner),
            dependencies,
            replay_row_allowance,
            raw_input,
            authorization_decision_digest: None,
        })
    }

    pub(super) async fn commit_prepared_checked(
        &mut self,
        cx: &CommitCx,
        prepared: PreparedWrite,
        crash_at: Option<CrashPoint>,
    ) -> Result<CommitSeq, WriteError> {
        if !Arc::ptr_eq(&self.handle_owner, &prepared.handle_owner) {
            return Err(WriteError::ForeignPreparedWrite);
        }
        self.ensure_writable()?;
        self.ensure_delta_window(cx, prepared.basis)
            .await
            .map_err(WriteError::from)?;
        let validator =
            FirstCommitterWinsValidator::from_history(prepared.basis, &self.snapshot.delta_index)
                .map_err(WriteError::PreparedHistory)?
                .with_scalar_resolver(self.keys.scalar_resolver.clone())
                .with_dependencies(
                    prepared.dependencies.elements.iter().copied(),
                    prepared.dependencies.adjacency.iter().copied(),
                );
        // Always install the validator for THIS basis, even if another prepared
        // write just committed or failed. Prior attempted drafts are not history.
        self.coordinator.set_validator(Box::new(validator));
        self.commit_template_with_authz(
            cx,
            prepared.template,
            prepared
                .authorization_decision_digest
                .unwrap_or(fgdb_crypto::Digest([0u8; 32])),
            crash_at,
            None,
            None,
        )
        .await
    }
}

#[cfg(test)]
mod dependency_tests {
    use super::*;

    #[test]
    fn dependency_clones_share_capture_and_isolate_later_observations() {
        let mut captured = DependencySet::default();
        captured.extend([5_u64, 1, 3, 1]);
        let mut next = captured.clone();
        let shared_flat = |left: &DependencySet<u64>, right: &DependencySet<u64>| matches!((left, right), (DependencySet::Flat(a), DependencySet::Flat(b)) if Arc::ptr_eq(a, b));
        assert!(shared_flat(&captured, &next));
        assert!(!next.insert(3));
        assert!(shared_flat(&captured, &next));
        assert!(next.insert(2));
        assert!(matches!(&next, DependencySet::Shared(_)));
        let pinned = next.clone();
        next.extend([9, 2, 7, 9]);
        assert_eq!(captured.iter().copied().collect::<Vec<_>>(), vec![1, 3, 5]);
        assert_eq!(pinned.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3, 5]);
        assert_eq!(
            next.iter().copied().collect::<Vec<_>>(),
            vec![1, 2, 3, 5, 7, 9]
        );
        assert_eq!(next.len(), 6);
        let mut flat_equivalent = DependencySet::default();
        flat_equivalent.extend([9, 7, 5, 3, 2, 1]);
        assert_eq!(next, flat_equivalent);
        for set in [&captured, &next] {
            let mut iter = set.iter();
            assert_eq!(iter.next(), Some(&1));
            assert_eq!(iter.clone().collect::<Vec<_>>(), iter.collect::<Vec<_>>());
        }
    }

    #[test]
    fn shared_dependency_prefixes_match_an_independent_set() {
        let mut actual = DependencySet::default();
        let mut expected = BTreeSet::new();
        let mut pinned = Vec::new();
        for step in 0..128_u64 {
            pinned.push((actual.clone(), expected.clone()));
            for key in [(step * 73) % 257, (step * 19) % 257, step % 11] {
                assert_eq!(actual.insert(key), expected.insert(key));
            }
            assert_eq!(
                actual.iter().copied().collect::<BTreeSet<_>>(),
                expected,
                "prefix {step}"
            );
            assert_eq!(actual.len(), expected.len());
        }
        for (actual, expected) in pinned {
            assert_eq!(actual.iter().copied().collect::<BTreeSet<_>>(), expected);
            assert_eq!(actual.len(), expected.len());
        }
    }

    #[test]
    fn observation_order_and_rollback_footprint_preserve_both_witness_domains() {
        use fgdb_types::EId;
        let v = |id| ElementId::Vertex(VId(id));
        let e = |id| ElementId::Edge(EId(id));
        let mut prefix = PreparedDependencies::default();
        prefix.elements.extend([e(3), v(2), v(1), v(2)]);
        prefix.adjacency.extend([VId(9), VId(2), VId(2)]);
        let before = vec![v(1), v(2), e(3), v(2), v(9)];
        assert_eq!(prefix.observations().collect::<Vec<_>>(), before);
        let mut next = prefix.clone();
        let mut suffix = PreparedDependencies::default();
        suffix.elements.extend([v(4), e(3)]);
        suffix.adjacency.extend([VId(4), VId(2)]);
        next.extend(suffix);
        assert_eq!(prefix.observations().collect::<Vec<_>>(), before);
        let after = vec![v(1), v(2), v(4), e(3), v(2), v(4), v(9)];
        assert_eq!(next.observations().collect::<Vec<_>>(), after);
        let mut retained = BTreeSet::from([e(99)]);
        next.retain_observations(&mut retained);
        assert_eq!(retained, after.into_iter().chain([e(99)]).collect());
    }
}
