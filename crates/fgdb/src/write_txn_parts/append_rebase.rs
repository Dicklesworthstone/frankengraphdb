// Re-evaluate a deliberately narrow, decision-free append program. The native
// mutation evaluator remains the only producer of effects and dependencies.

impl WriteTxn {
    /// Commit unobserved vertex/edge creations after explicit append rebase.
    ///
    /// Ordinary commit conservatively conflicts on shared endpoint vertices.
    /// This opt-in finalization can admit independent appends to the same
    /// neighborhood: it re-evaluates the original typed creation instructions
    /// against the current healthy writer and then uses ordinary FCW validation
    /// and ONE Chronicle publication. No source text is parsed or retried.
    ///
    /// Eligibility requires only unconditional creates, no recorded point or
    /// query reads (including negative reads), no scan/expansion witnesses, no
    /// savepoints and no active mixed-program scope. ENSURE, CAS, updates and
    /// deletes are refused, even when their net effect was a no-op. The complete
    /// retained history is required. Schema/constraint or unknown delta families,
    /// writes to proposed identities and retirement of any endpoint all refuse.
    /// Reusing an identity that was concurrently created then deleted is refused
    /// even when the current writer no longer contains it.
    ///
    /// Re-evaluation must produce the EXACT original canonical template. This
    /// preserves IDs, payloads, creation ordering and already-issued identities;
    /// it never converts a create into an ensure or silently renumbers births.
    /// Only the validated basis and freshly captured dependencies are replaced.
    /// A prefix whose atomic-group ordering cannot be reproduced is refused.
    ///
    /// max_expanded_rows bounds the complete relation-expanded evaluator input
    /// before cloning staged payloads. It is not a byte-memory, history-work or
    /// I/O budget. History/eligibility traversals checkpoint; the existing
    /// synchronous evaluator and template comparison remain non-preemptible.
    ///
    /// This is a TERMINAL commit attempt, not refresh_snapshot: once polled and
    /// owner-admitted, any refusal, cancellation or unwind releases the workspace
    /// and its pin. Wrong-owner calls and unpolled futures preserve it. After
    /// publication starts, the ordinary unknown/recovery contract applies. No
    /// extra cancellation check follows publication. Raw embedded authority is
    /// unchanged; this is not an authorized-token API, full SSI, automatic retry
    /// or the general context-derived semantic merge ladder.
    pub async fn commit_append_only_rebased<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        max_expanded_rows: u64,
    ) -> Result<CommitSeq, WriteTxnError> {
        let completion = cx
            .with_restriction_async(self.complete_with_basis_controlled(
                database,
                cx,
                None,
                true,
                Some(max_expanded_rows),
                || cx.checkpoint().map_err(WriteTxnError::Interrupted),
            ))
            .await?;
        match completion {
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Ok(commit_seq),
            EmbeddedTxnCompletion::ReadClosed { .. } => {
                unreachable!("append finalization requires a prepared write")
            }
        }
    }

    // Called only under the terminal completion guard. A failed observation
    // cannot escape into a still-active old-basis workspace and affect a retry.
    fn prepare_append_rebase<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        max_expanded_rows: u64,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<(), WriteTxnError> {
        use fgdb_delta_types::DeltaRow;
        use std::collections::BTreeSet;

        let frontier = database.frontier()?;
        let previous = self.prepared.as_ref().ok_or(WriteTxnError::NoPreparedWrite)?;
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
            return Err(WriteTxnError::AppendRebaseIneligible);
        }
        // Reject conditional/raw decisions BEFORE looking at the new writer.
        // Normalization must not turn an ENSURE or CAS into an eligible append.
        for batch in &self.staged {
            checkpoint()?;
            for row in &batch.rows {
                checkpoint()?;
                if !matches!(
                    row,
                    PendingRow::Vertex { ensure: false, .. }
                        | PendingRow::Edge { ensure: false, .. }
                ) {
                    return Err(WriteTxnError::AppendRebaseIneligible);
                }
            }
        }
        database.admit_ordered_write_rows(self.staged.iter(), max_expanded_rows)?;
        let mut creations = BTreeSet::new();
        let mut endpoints = BTreeSet::new();
        for row in self.staged.iter().flat_map(|batch| &batch.rows) {
            checkpoint()?;
            match row {
                PendingRow::Vertex { vid, .. } => {
                    creations.insert(ElementId::Vertex(*vid));
                }
                PendingRow::Edge { eid, src, dst, .. } => {
                    creations.insert(ElementId::Edge(*eid));
                    endpoints.insert(*src);
                    endpoints.insert(*dst);
                }
                _ => unreachable!("raw append eligibility was checked"),
            }
        }
        // A current-state lookup alone misses create/delete identity races and
        // retired endpoints. Admission needs the COMPLETE original-basis tail.
        for batch in database.delta_since(self.basis)? {
            checkpoint()?;
            for coordinate in batch.coordinate_entries() {
                checkpoint()?;
                for row in &coordinate.rows {
                    checkpoint()?;
                    match row {
                        DeltaRow::CreateVertex { .. }
                        | DeltaRow::CreateEdge { .. }
                        | DeltaRow::DeleteVertex { .. }
                        | DeltaRow::DeleteEdge { .. }
                        | DeltaRow::LabelMembership { .. }
                        | DeltaRow::Property { .. } => {}
                        // Includes schema/constraints. New families need an
                        // explicit independence law before rebase may cross them.
                        _ => return Err(WriteTxnError::AppendRebaseIneligible),
                    }
                    let mut touched = BTreeSet::new();
                    validation_touches(row, &mut touched, checkpoint)?;
                    let mut collision = false;
                    for element in &touched {
                        checkpoint()?;
                        collision |= creations.contains(element);
                    }
                    let retired_endpoint = matches!(
                        row, DeltaRow::DeleteVertex { vid, .. } if endpoints.contains(vid)
                    );
                    if collision || retired_endpoint {
                        return Err(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-01",
                            detail: "append rebase crossed an identity write or endpoint retirement"
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
        // This reconstructs both the canonical effects and CURRENT dependency
        // set with the ordinary evaluator, rather than blessing a stale draft.
        let prepared = database.prepare_ordered_writes_bounded(
            self.staged.clone(), max_expanded_rows,
        )?;
        checkpoint()?;
        if prepared.template != previous.template {
            return Err(WriteTxnError::AppendRebaseIneligible);
        }
        debug_assert_eq!(prepared.basis, frontier);
        self.prepared = Some(prepared);
        self.basis = frontier;
        Ok(())
    }
}

#[cfg(test)]
mod append_rebase_tests {
    include!("append_rebase_tests.rs");
}
