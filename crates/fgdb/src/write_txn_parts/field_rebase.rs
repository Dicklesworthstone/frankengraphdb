// Rebase field-local intents only after proving their complete raw read/write
// domain unchanged. Effects still come from the ordinary native evaluator.

#[derive(Default)]
struct FieldRebaseFootprint {
    properties: std::collections::BTreeSet<(ElementId, fgdb_delta_types::PropertyKeyId)>,
    labels: std::collections::BTreeSet<(VId, LabelId)>,
    targets: std::collections::BTreeSet<ElementId>,
}

impl FieldRebaseFootprint {
    fn record(&mut self, row: &PendingRow) -> Result<(), WriteTxnError> {
        let (element, property) = match row {
            PendingRow::SetProperty { vid, key, .. } => (ElementId::Vertex(*vid), *key),
            PendingRow::SetEdgeProperty { eid, key, .. } => (ElementId::Edge(*eid), *key),
            // A successful conditional no-op still observes this exact field.
            // Guard outcomes are preserved even when no Property row survives.
            PendingRow::CompareAndSet { elem, key, .. } => (*elem, *key),
            PendingRow::SetLabel { vid, label, .. } => {
                self.targets.insert(ElementId::Vertex(*vid));
                self.labels.insert((*vid, *label));
                return Ok(());
            }
            _ => return Err(WriteTxnError::FieldRebaseIneligible),
        };
        self.targets.insert(element);
        self.properties.insert((element, property));
        Ok(())
    }

    fn conflicts(
        &self,
        row: &fgdb_delta_types::DeltaRow,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<bool, WriteTxnError> {
        use fgdb_delta_types::DeltaRow;
        checkpoint()?;
        match row {
            DeltaRow::Property { elem, property, .. } => {
                Ok(self.properties.contains(&(*elem, *property)))
            }
            DeltaRow::LabelMembership { vid, label, .. } => {
                Ok(self.labels.contains(&(*vid, *label)))
            }
            DeltaRow::CreateVertex { vid, .. } => {
                Ok(self.targets.contains(&ElementId::Vertex(*vid)))
            }
            DeltaRow::CreateEdge { eid, .. } | DeltaRow::DeleteEdge { eid, .. } => {
                Ok(self.targets.contains(&ElementId::Edge(*eid)))
            }
            DeltaRow::DeleteVertex {
                vid,
                sorted_retired_incident_edges,
                ..
            } => {
                if self.targets.contains(&ElementId::Vertex(*vid)) {
                    return Ok(true);
                }
                // A targeted edge can be retired without a DeleteEdge row.
                // Walk the authoritative cascade, never just the final graph.
                for eid in sorted_retired_incident_edges {
                    checkpoint()?;
                    if self.targets.contains(&ElementId::Edge(*eid)) {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            // Includes schema/constraint state and future mutation families.
            // They have no field-independence law on this execution surface.
            _ => Err(WriteTxnError::FieldRebaseIneligible),
        }
    }
}

impl WriteTxn {
    /// Commit independent field edits despite another edit to the same element.
    ///
    /// This explicit finalization admits SET/unset of vertex or edge properties,
    /// label membership changes, and property compare-and-set. It requires no
    /// recorded point, negative or query reads, scans, expansion witnesses,
    /// savepoints or active mixed-program scope. Creation/ensure/deletion intents
    /// are ineligible even when their effects cancelled. Ordinary commit and
    /// refresh_snapshot keep their existing element-level conflict behavior.
    ///
    /// Every raw property or label slot is protected, including cancelled SETs,
    /// absent-property removals and CAS guards whose branch normalized to no-op.
    /// The complete retained original-basis history must contain no write to
    /// those slots and no target creation/retirement, including edge cascades.
    /// An intervening change and restoration of the same value still conflicts.
    /// Edits to other fields, labels or incidence may pass; schema/constraint or
    /// unsupported delta families refuse rather than assume independence.
    ///
    /// Re-evaluation uses the SAME ordered native mutation evaluator against the
    /// current healthy writer and must reproduce the exact canonical template.
    /// This validates current before-images/guards and captures fresh native
    /// dependencies; no stale draft is blessed by changing only its basis.
    /// There is no last-writer-wins merge, lost-update fallback, new interpreter,
    /// identity allocation, implicit read refresh or intermediate publication.
    ///
    /// max_expanded_rows admits the complete relation-expanded evaluator input
    /// before cloning mutation payloads. History/footprint walks checkpoint;
    /// native synchronous preparation and template equality are not internally
    /// preemptible. This is not a byte-memory, storage-work or spill guarantee.
    ///
    /// Once polled and owner-admitted, this is a TERMINAL attempt through the
    /// existing completion guard. Refusal/cancellation/unwind releases the pin;
    /// wrong-owner and unpolled calls preserve the workspace. Native FCW and ONE
    /// Chronicle commit still finalize it, with unchanged unknown/recovery rules
    /// and no new fallible check after publication. This raw embedded API is not
    /// a capability-token boundary, full SSI or the general semantic merge ladder.
    pub async fn commit_disjoint_fields_rebased<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        max_expanded_rows: u64,
    ) -> Result<CommitSeq, WriteTxnError> {
        let completion = cx
            .with_restriction_async(self.complete_rebased_controlled(
                database,
                cx,
                None,
                true,
                Some(RebasePreparation::DisjointFields(max_expanded_rows)),
                || cx.checkpoint().map_err(WriteTxnError::Interrupted),
            ))
            .await?;
        match completion {
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Ok(commit_seq),
            EmbeddedTxnCompletion::ReadClosed { .. } => {
                unreachable!("field finalization requires a prepared write")
            }
        }
    }

    // Runs only under the terminal owner guard; new observations cannot escape
    // a refused rebase into an active old-basis workspace.
    fn prepare_field_rebase<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        max_expanded_rows: u64,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<(), WriteTxnError> {
        let frontier = database.frontier()?;
        let previous = self
            .prepared
            .as_ref()
            .ok_or(WriteTxnError::NoPreparedWrite)?;
        if !std::sync::Arc::ptr_eq(&self.handle_owner, &previous.handle_owner) {
            return Err(WriteTxnError::WrongDatabase);
        }
        if previous.basis != self.basis {
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live: previous.basis,
            });
        }
        if !self.read_set.borrow().is_empty()
            || !self.match_expansions.borrow().is_empty()
            || !self.scanned_vertex_labels.borrow().is_empty()
            || self.scanned_vertices.get()
            || self.scanned_edges.get()
            || !self.savepoints.is_empty()
            || self.program_multi_relation
        {
            return Err(WriteTxnError::FieldRebaseIneligible);
        }
        // Check the original vocabulary before normalization, routing or copies.
        for batch in &self.staged {
            checkpoint()?;
            for row in &batch.rows {
                checkpoint()?;
                if !matches!(
                    row,
                    PendingRow::SetProperty { .. }
                        | PendingRow::SetEdgeProperty { .. }
                        | PendingRow::SetLabel { .. }
                        | PendingRow::CompareAndSet { .. }
                ) {
                    return Err(WriteTxnError::FieldRebaseIneligible);
                }
            }
        }
        database.admit_ordered_write_rows(self.staged.iter(), max_expanded_rows)?;
        let mut footprint = FieldRebaseFootprint::default();
        for row in self.staged.iter().flat_map(|batch| &batch.rows) {
            checkpoint()?;
            footprint.record(row)?;
        }
        // A current field comparison cannot establish an unchanged interval:
        // both field ABA and target retirement must be checked across the tail.
        for batch in database.delta_since(self.basis)? {
            checkpoint()?;
            for coordinate in batch.coordinate_entries() {
                checkpoint()?;
                for row in &coordinate.rows {
                    if footprint.conflicts(row, checkpoint)? {
                        return Err(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-01",
                            detail: "field rebase crossed a protected field or target lifetime"
                                .to_owned(),
                        }
                        .into());
                    }
                }
            }
        }
        checkpoint()?;
        if frontier == self.basis {
            return Ok(());
        }
        let prepared =
            database.prepare_ordered_writes_bounded(self.staged.clone(), max_expanded_rows)?;
        checkpoint()?;
        if prepared.template != previous.template {
            return Err(WriteTxnError::FieldRebaseIneligible);
        }
        debug_assert_eq!(prepared.basis, frontier);
        self.prepared = Some(prepared);
        self.basis = frontier;
        Ok(())
    }
}

#[cfg(test)]
mod field_rebase_tests {
    include!("field_rebase_tests.rs");
}
