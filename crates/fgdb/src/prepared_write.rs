//! Prepared writes retain their observations, not a resettable validator epoch.

use crate::{
    CommitCx, CommitSeq, CrashPoint, Database, ElementId, FirstCommitterWinsValidator,
    PendingRow, PreparedWrite, VId, WriteBatch, WriteError,
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
    fn capture(writer: &BlockWriter, batch: &WriteBatch) -> Self {
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
                        writer.live_incident_edges(*vid).into_iter().map(ElementId::Edge),
                    );
                }
                PendingRow::Edge { eid, src, dst, ensure, .. } => {
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
                                && s == *src && d == *dst && relation == batch.relation
                            {
                                result.elements.insert(ElementId::Edge(existing));
                            }
                        }
                    }
                }
                PendingRow::DeleteEdge { eid, .. }
                | PendingRow::SetEdgeProperty { eid, .. } => {
                    result.elements.insert(ElementId::Edge(*eid));
                }
                PendingRow::CompareAndSet { elem, .. } => {
                    result.elements.insert(*elem);
                }
            }
        }
        result
    }
}

impl<V: Vfs + Clone> Database<V> {
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
        let validator = FirstCommitterWinsValidator::from_history(
            prepared.basis,
            &self.snapshot.delta_index,
        )
        .map_err(WriteError::PreparedHistory)?
        .with_dependencies(prepared.dependencies.elements, prepared.dependencies.adjacency);
        // Always install the validator for THIS basis, even if another prepared
        // write just committed or failed. Prior attempted drafts are not history.
        self.coordinator.set_validator(Box::new(validator));
        self.commit_template(cx, prepared.template, crash_at, None, None).await
    }
}
