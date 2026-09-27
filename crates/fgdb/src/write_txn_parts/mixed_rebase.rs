// Compose the existing append and field independence laws without splitting
// the original ordered program, its observed reads or its publication.

#[derive(Default)]
struct MixedRebaseFootprint {
    append: AppendRebaseFootprint,
    fields: FieldRebaseFootprint,
    retired_edges: std::collections::BTreeSet<EId>,
}

fn mixed_rebase_error(error: WriteTxnError) -> WriteTxnError {
    match error {
        WriteTxnError::AppendRebaseIneligible | WriteTxnError::FieldRebaseIneligible => {
            WriteTxnError::MixedRebaseIneligible
        }
        // In particular, never replace an interruption with an eligibility error.
        error => error,
    }
}

impl MixedRebaseFootprint {
    fn admits(row: &PendingRow) -> bool {
        matches!(
            row,
            PendingRow::Vertex { ensure: false, .. }
                | PendingRow::Edge { ensure: false, .. }
                | PendingRow::SetProperty { .. }
                | PendingRow::SetEdgeProperty { .. }
                | PendingRow::SetLabel { .. }
                | PendingRow::CompareAndSet { .. }
                | PendingRow::DeleteEdge { .. }
        )
    }

    fn record(&mut self, row: &PendingRow) -> Result<(), WriteTxnError> {
        if !Self::admits(row) {
            return Err(WriteTxnError::MixedRebaseIneligible);
        }
        match row {
            PendingRow::Vertex { .. } | PendingRow::Edge { .. } => self.append.record(row),
            PendingRow::DeleteEdge { eid, .. } => {
                // Even delete-if-present on an absent identity is a decision:
                // preserve it when the net template contains no retirement.
                self.retired_edges.insert(*eid);
                Ok(())
            }
            _ => self.fields.record(row),
        }
        .map_err(mixed_rebase_error)
    }

    fn conflicts(
        &self,
        row: &fgdb_delta_types::DeltaRow,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<bool, WriteTxnError> {
        if self
            .append
            .conflicts(row, checkpoint)
            .map_err(mixed_rebase_error)?
        {
            return Ok(true);
        }
        if self
            .fields
            .conflicts(row, checkpoint)
            .map_err(mixed_rebase_error)?
        {
            return Ok(true);
        }
        if self.retired_edges.is_empty() {
            return Ok(false);
        }
        // A retirement protects the entire edge, not just fields named by
        // other updates. Deleting an endpoint can retire it without emitting
        // a DeleteEdge row, so the engine-derived cascade is authoritative.
        use fgdb_delta_types::DeltaRow;
        match row {
            DeltaRow::CreateEdge { eid, .. }
            | DeltaRow::DeleteEdge { eid, .. }
            | DeltaRow::Property {
                elem: ElementId::Edge(eid),
                ..
            } => {
                Ok(self.retired_edges.contains(eid))
            }
            DeltaRow::DeleteVertex {
                sorted_retired_incident_edges,
                ..
            } => {
                for eid in sorted_retired_incident_edges {
                    checkpoint()?;
                    if self.retired_edges.contains(eid) {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            _ => Ok(false), // Unknown families already refused above.
        }
    }
}

impl WriteTxn {
    /// Rebase an ordered program of appends, field edits and edge retirements.
    ///
    /// Unconditional vertex/edge creation may be interleaved with property
    /// SET/unset, label updates and property CAS, including edits of elements
    /// created earlier in this program. Identity-addressed edge deletion may
    /// replace existing relationships atomically with new edges and fields.
    /// All raw slots remain protected even
    /// when normalization removes an update or a CAS selects its no-op branch.
    /// Edge retirement protects the complete edge, including a delete-if-present
    /// decision on an absent identity. Concurrent updates, identity ABA and
    /// endpoint cascades refuse even if the final edge looks unchanged/absent.
    /// ENSURE and vertex deletion remain ineligible even if their effect vanished:
    /// vertex cascades need a separate incident-set independence law.
    /// Savepoints and active mixed-program rollback scopes remain ineligible.
    ///
    /// The shared completion guard validates every recorded point, negative,
    /// scan and expansion observation over the ORIGINAL retained-history
    /// interval before this policy can change the basis. This does not narrow
    /// whole-row reads, replay queries or let new writes erase old observations.
    /// Both existing independence laws apply to the same complete suffix:
    /// created IDs cannot have been touched, endpoints cannot have retired,
    /// field targets must retain their lifetime, and protected property/label
    /// slots must have no intervening write, including change-and-restoration.
    /// Schema/constraint and unknown delta families fail closed.
    ///
    /// A creation identity already recorded as read is refused when advancing
    /// the basis: a staged record may have exposed its basis-placeholder birth
    /// timestamp. The coarse witness cannot distinguish that observation from
    /// an earlier negative read. Identity reservation alone is not a row read.
    ///
    /// The COMPLETE original program is re-evaluated ONCE by the native ordered
    /// mutation evaluator and must reproduce its exact canonical template.
    /// Neither creation and update halves nor relation groups are independently
    /// committed. Ordinary FCW finalizes one Chronicle write with unchanged
    /// unknown/recovery outcome handling. No callback or application code reruns.
    ///
    /// max_expanded_rows bounds all relation-expanded evaluator input before
    /// cloning staged payloads. History/footprint walks checkpoint; existing
    /// synchronous preparation and template comparison are not internally
    /// preemptible. This is not byte-memory, spill, or storage-work governance.
    /// Once polled and owner-admitted the attempt is terminal, including on
    /// refusal, cancellation or unwind. Wrong-owner/unpolled calls preserve the
    /// workspace. This raw embedded API is not token-authorized or full SSI.
    pub async fn commit_mixed_rebased<V: Vfs + Clone>(
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
                Some(RebasePreparation::Mixed(max_expanded_rows)),
                || cx.checkpoint().map_err(WriteTxnError::Interrupted),
            ))
            .await?;
        match completion {
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Ok(commit_seq),
            EmbeddedTxnCompletion::ReadClosed { .. } => {
                unreachable!("mixed finalization requires a prepared write")
            }
        }
    }

    // Only called under the shared terminal guard, after original-basis read
    // validation. No partially advanced workspace is ever returned to a caller.
    fn prepare_mixed_rebase<V: Vfs + Clone>(
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
        if !self.savepoints.is_empty() || self.program_multi_relation {
            return Err(WriteTxnError::MixedRebaseIneligible);
        }
        for batch in &self.staged {
            checkpoint()?;
            for row in &batch.rows {
                checkpoint()?;
                if !MixedRebaseFootprint::admits(row) {
                    return Err(WriteTxnError::MixedRebaseIneligible);
                }
            }
        }
        database.admit_ordered_write_rows(self.staged.iter(), max_expanded_rows)?;
        let mut footprint = MixedRebaseFootprint::default();
        for row in self.staged.iter().flat_map(|batch| &batch.rows) {
            checkpoint()?;
            footprint.record(row)?;
        }
        if frontier != self.basis {
            self.validate_unobserved_creations(&footprint.append.creations, checkpoint)
                .map_err(mixed_rebase_error)?;
        }
        // Checking final values is insufficient: this tail catches identity,
        // field and lifetime ABA, including cascades without DeleteEdge rows.
        for batch in database.delta_since(self.basis)? {
            checkpoint()?;
            for coordinate in batch.coordinate_entries() {
                checkpoint()?;
                for row in &coordinate.rows {
                    if footprint.conflicts(row, checkpoint)? {
                        return Err(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-01",
                            detail: "mixed rebase crossed a protected identity, field or lifetime"
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
            return Err(WriteTxnError::MixedRebaseIneligible);
        }
        debug_assert_eq!(prepared.basis, frontier);
        self.prepared = Some(prepared);
        self.basis = frontier;
        Ok(())
    }
}

#[cfg(test)]
mod mixed_rebase_tests {
    include!("mixed_rebase_tests.rs");
}