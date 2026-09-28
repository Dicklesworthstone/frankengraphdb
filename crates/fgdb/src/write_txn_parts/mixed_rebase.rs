// Compose the existing append and field independence laws without splitting
// the original ordered program, its observed reads or its publication.

#[derive(Default)]
struct MixedRebaseFootprint {
    append: AppendRebaseFootprint,
    fields: FieldRebaseFootprint,
    retired_edges: std::collections::BTreeSet<EId>,
    retired_vertices: std::collections::BTreeSet<VId>,
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
                | PendingRow::DeleteVertex { .. }
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
            PendingRow::DeleteVertex { vid, .. } => {
                // Retain even an absent delete-if-present and a create/delete
                // pair normalized away by the native evaluator.
                self.retired_vertices.insert(*vid);
                Ok(())
            }
            _ => self.fields.record(row),
        }
        .map_err(mixed_rebase_error)
    }

    fn protect_vertex_cascades(
        &mut self,
        template: &fgdb_delta_types::LogicalDeltaTemplate,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<(), WriteTxnError> {
        if self.retired_vertices.is_empty() {
            return Ok(());
        }
        // The native NENF already owns the complete BASIS incident-edge image,
        // including explicit earlier edge deletions absorbed into a cascade.
        // Borrow it rather than scanning a newer graph or rebuilding an image.
        // Same-program creations that NENF cancels retain raw append guards.
        for coordinate in template.coordinate_entries() {
            checkpoint()?;
            for row in &coordinate.rows {
                checkpoint()?;
                if let fgdb_delta_types::DeltaRow::DeleteVertex {
                    vid,
                    sorted_retired_incident_edges,
                    ..
                } = row
                {
                    if !self.retired_vertices.contains(vid) {
                        return Err(WriteTxnError::MixedRebaseIneligible);
                    }
                    for eid in sorted_retired_incident_edges {
                        checkpoint()?;
                        self.retired_edges.insert(*eid);
                    }
                }
            }
        }
        Ok(())
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
        if self.retired_edges.is_empty() && self.retired_vertices.is_empty() {
            return Ok(false);
        }
        // Retirement protects complete element contents and lifetimes. New
        // incidence is a phantom even when its edge is deleted again later.
        // Existing incidence is guarded by its canonical cascade identities.
        use fgdb_delta_types::DeltaRow;
        match row {
            DeltaRow::CreateVertex { vid, .. }
            | DeltaRow::LabelMembership { vid, .. }
            | DeltaRow::Property {
                elem: ElementId::Vertex(vid),
                ..
            } => Ok(self.retired_vertices.contains(vid)),
            DeltaRow::CreateEdge { eid, src, dst, .. } => Ok(self.retired_edges.contains(eid)
                || self.retired_vertices.contains(src)
                || self.retired_vertices.contains(dst)),
            DeltaRow::DeleteEdge { eid, .. }
            | DeltaRow::Property {
                elem: ElementId::Edge(eid),
                ..
            } => Ok(self.retired_edges.contains(eid)),
            DeltaRow::DeleteVertex {
                vid,
                sorted_retired_incident_edges,
                ..
            } => {
                if self.retired_vertices.contains(vid) {
                    return Ok(true);
                }
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
    /// Rebase an ordered program of appends, field edits and element retirements.
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
    /// Vertex retirement protects its complete contents and lifetime, every
    /// existing incident edge in the original native cascade, and the absence
    /// of new incoming/outgoing edges across ALL relations. Parallel edges and
    /// self-loops are included. Changed-and-restored contents or topology still
    /// conflict. Even an absent delete-if-present retains its vertex identity.
    /// The cascade image comes only from the already prepared canonical effects;
    /// no source graph is rescanned to infer which earlier edges existed.
    /// ENSURE remains ineligible even when its net effect vanished.
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
        merge: &fgdb_types::MergeEvalCx,
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
        footprint.protect_vertex_cascades(&previous.template, checkpoint)?;
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
        let prepared = database.prepare_ordered_writes_replay(
            merge,
            self.staged.clone(),
            max_expanded_rows,
        )?;
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

#[cfg(test)]
mod vertex_rebase_tests {
    include!("vertex_rebase_tests.rs");
}
