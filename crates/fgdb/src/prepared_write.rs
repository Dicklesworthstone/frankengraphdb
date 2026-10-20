//! Prepared writes retain their observations, not a resettable validator epoch.

mod at_basis;
mod atomic;
mod ordered;

pub(super) use at_basis::PreparationAnchor;

use crate::{
    CommitCx, CommitSeq, CrashPoint, Database, ElementId, FirstCommitterWinsValidator, PendingRow,
    PreparedWrite, VId, WriteBatch, WriteError,
};
use asupersync::fs::Vfs;
use fgdb_strata::writer::BlockWriter;
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Clone, Default)]
pub(super) struct PreparedDependencies {
    elements: BTreeSet<ElementId>,
    adjacency: BTreeSet<VId>,
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
        self.elements.extend(other.elements);
        self.adjacency.extend(other.adjacency);
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
        observations.extend(self.elements);
        // Transaction validation checks adjacency-changing writes against
        // endpoint vertex observations, including previously absent edges.
        observations.extend(self.adjacency.into_iter().map(ElementId::Vertex));
    }
}

impl<V: Vfs + Clone> Database<V> {
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
        self.build_create_template_with_edge_prefix(batch, Some(earlier_edges))
    }

    /// One ordinary canonical authority for already-admitted independent
    /// chunks. No row is re-derived from storage, and no earlier prefix is
    /// cloned or sorted while another program statement is being evaluated.
    pub(super) fn compose_independent_edge_chunks(
        &self,
        relation: crate::RelationId,
        chunks: &[crate::LogicalDeltaTemplate],
        dependencies: PreparedDependencies,
    ) -> Result<PreparedWrite, crate::WriteTxnError> {
        let mut rows = Vec::new();
        let mut offset = 0_u64;
        for chunk in chunks {
            let [coordinate] = chunk.coordinate_entries() else {
                return Err(crate::WriteTxnError::UnsupportedAtomicMutation);
            };
            if coordinate.graph != crate::GRAPH
                || coordinate.branch != crate::BRANCH
                || coordinate.relation != relation
                || coordinate.schema_epoch != crate::SchemaEpoch(0)
                || coordinate.schema_transition.is_some()
            {
                return Err(crate::WriteTxnError::UnsupportedAtomicMutation);
            }
            for row in &coordinate.rows {
                let mut row = row.clone();
                let crate::DeltaRow::CreateEdge { birth_ordinal, .. } = &mut row else {
                    return Err(crate::WriteTxnError::UnsupportedAtomicMutation);
                };
                *birth_ordinal = birth_ordinal
                    .checked_add(offset)
                    .ok_or(crate::WriteTxnError::AtomicOrdinalOverflow)?;
                rows.push(row);
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
            vec![crate::CoordinateEntry {
                graph: crate::GRAPH,
                branch: crate::BRANCH,
                relation,
                schema_epoch: crate::SchemaEpoch(0),
                schema_transition: None,
                rows,
            }],
        )
        .map_err(WriteError::Canonical)?;
        Ok(PreparedWrite {
            template,
            basis: self.snapshot.frontier,
            handle_owner: Arc::clone(&self.handle_owner),
            dependencies,
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
                    prepared.dependencies.elements,
                    prepared.dependencies.adjacency,
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
