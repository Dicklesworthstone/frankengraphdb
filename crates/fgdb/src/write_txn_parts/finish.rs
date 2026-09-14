// Completion owns cleanup across returns, Rust unwinding and dropped futures.
// It never performs durable work in Drop or turns an unknown marker into abort.
struct TxnCompletionGuard<'txn, 'db, V: Vfs + Clone> {
    transaction: &'txn mut WriteTxn,
    database: &'db mut Database<V>,
    starting_frontier: CommitSeq,
    entered_commit: bool,
}

impl<'txn, 'db, V: Vfs + Clone> TxnCompletionGuard<'txn, 'db, V> {
    fn new(transaction: &'txn mut WriteTxn, database: &'db mut Database<V>) -> Self {
        let starting_frontier = database.snapshot.frontier;
        Self { transaction, database, starting_frontier, entered_commit: false }
    }
}

impl<V: Vfs + Clone> Drop for TxnCompletionGuard<'_, '_, V> {
    fn drop(&mut self) {
        if !self.transaction.state.is_terminal() {
            self.transaction.state = if self.entered_commit {
                match self.database.state() {
                    crate::DatabaseState::Healthy { published_frontier }
                        if published_frontier == self.starting_frontier => EmbeddedTxnState::Aborted,
                    crate::DatabaseState::Healthy { published_frontier }
                        if published_frontier.0 > self.starting_frontier.0 => {
                            // This guard has the exclusive database borrow. An
                            // advanced healthy frontier can only be its write.
                            EmbeddedTxnState::Completed(EmbeddedTxnCompletion::WriteCommitted {
                                commit_seq: published_frontier,
                            })
                        }
                    crate::DatabaseState::Healthy { published_frontier }
                    | crate::DatabaseState::CommitOutcomeUnknown { published_frontier } => {
                        EmbeddedTxnState::CommitOutcomeUnknown { published_frontier }
                    }
                    crate::DatabaseState::NeedsAuthoritativeRecovery(recovery) => {
                        EmbeddedTxnState::CommittedNeedsRecovery {
                            commit_seq: recovery.durable_frontier,
                        }
                    }
                }
            } else {
                EmbeddedTxnState::Aborted
            };
        }
        // Validation's temporary RefCell borrows and the inner commit future
        // are already dropped. Exclusive access allows nonpanicking get_mut.
        self.transaction.discard_terminal_workspace();
    }
}

impl WriteTxn {
    /// Validate recorded dependencies and complete either kind of workspace.
    /// A prepared batch returns WriteCommitted, including a normalized no-op.
    /// With no prepared batch, return ReadClosed without writing a capsule,
    /// marker, root or sequence. ReadClosed retains the original snapshot and
    /// separately reports the healthy frontier through which reads validated.
    ///
    /// The validation contract is the existing conservative embedded read and
    /// scan witness contract, not full SSI or the durable session protocol.
    /// A dropped UNPOLLED future has no effect. Once polled and admitted by the
    /// owner, every terminal return, unwind or dropped future releases the pin
    /// and workspace; state() distinguishes abort from unknown or committed.
    /// Wrong-owner calls preserve all state for the correct database.
    pub async fn finish<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
    ) -> Result<EmbeddedTxnCompletion, WriteTxnError> {
        self.finish_with_crash(database, cx, None).await
    }

    /// The same completion path with the existing production commit fault seam.
    /// A read close performs no write and therefore cannot hit a crash point.
    #[doc(hidden)]
    pub async fn finish_with_crash<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
    ) -> Result<EmbeddedTxnCompletion, WriteTxnError> {
        self.complete(database, cx, crash_at, false).await
    }

    /// Require a prepared write, validate it, commit and release its pin.
    /// For a workspace that may be read-only, use finish's typed outcome.
    pub async fn commit<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
    ) -> Result<CommitSeq, WriteTxnError> {
        self.commit_with_crash(database, cx, None).await
    }

    /// Commit through the production crash-point path. Wrong-owner calls leave
    /// the transaction unchanged. An admitted owner's terminal attempt releases
    /// the pin even when its future is dropped before the await returns.
    pub async fn commit_with_crash<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
    ) -> Result<CommitSeq, WriteTxnError> {
        match self.complete(database, cx, crash_at, true).await? {
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Ok(commit_seq),
            EmbeddedTxnCompletion::ReadClosed { .. } => {
                unreachable!("write-only completion refuses before read close")
            }
        }
    }

    async fn complete<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
        require_write: bool,
    ) -> Result<EmbeddedTxnCompletion, WriteTxnError> {
        self.ensure_database(database)?;
        let mut attempt = TxnCompletionGuard::new(self, database);
        // Health must be checked even for an empty read set. No snapshot
        // observation can bless a handle fenced by an earlier ambiguous write.
        let validated_through = attempt.database.frontier()?;
        if require_write && attempt.transaction.prepared.is_none() {
            return Err(WriteTxnError::NoPreparedWrite);
        }
        if let Some((law, element, committed_at)) =
            attempt.transaction.transaction_conflict(attempt.database)?
        {
            return Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law,
                detail: format!(
                    "transaction dependency {element:?} changed at {committed_at:?} after pinned basis {:?}",
                    attempt.transaction.basis
                ),
            }));
        }
        let completion = if let Some(prepared) = attempt.transaction.prepared.take() {
            attempt.transaction.staged.clear();
            // Set before polling the cancellable publication future. Drop
            // consults its retained database fence, never guesses rollback.
            attempt.entered_commit = true;
            let commit_seq = attempt.database
                .commit_prepared_with_crash(cx, prepared, crash_at)
                .await?;
            EmbeddedTxnCompletion::WriteCommitted { commit_seq }
        } else {
            debug_assert!(attempt.transaction.staged.is_empty());
            EmbeddedTxnCompletion::ReadClosed {
                snapshot_seq: attempt.transaction.basis,
                validated_through,
            }
        };
        attempt.transaction.state = EmbeddedTxnState::Completed(completion);
        Ok(completion)
    }

    /// End the transaction without publishing its prepared batch.
    pub fn abort(mut self) {
        if !self.state.is_terminal() {
            self.state = EmbeddedTxnState::Aborted;
        }
        self.discard_terminal_workspace();
    }

    fn discard_terminal_workspace(&mut self) {
        self.staged.clear();
        self.prepared = None;
        self.read_set.get_mut().clear();
        self.match_expansions.get_mut().clear();
        self.scanned_vertex_labels.get_mut().clear();
        self.scanned_vertices.set(false);
        self.scanned_edges.set(false);
        self.release_pin();
    }

    fn combined_batch(staged: &[WriteBatch]) -> Option<WriteBatch> {
        let mut batches = staged.iter().cloned();
        let mut combined = batches.next()?;
        for mut batch in batches {
            debug_assert_eq!(batch.relation, combined.relation);
            combined.rows.append(&mut batch.rows);
        }
        Some(combined)
    }

    fn overlay_property(
        props: &mut Vec<(fgdb_delta_types::PropertyKeyId, CanonicalScalar)>,
        key: fgdb_delta_types::PropertyKeyId,
        value: Option<&CanonicalScalar>,
    ) {
        match props.binary_search_by_key(&key, |(property, _)| *property) {
            Ok(at) => match value {
                Some(value) => props[at].1 = value.clone(),
                None => {
                    props.remove(at);
                }
            },
            Err(at) => {
                if let Some(value) = value {
                    props.insert(at, (key, value.clone()));
                }
            }
        }
    }

    /// Preparation observes more than its net writes: conditional no-ops and
    /// ensure-existing operations still depend on their targets, and edge
    /// creation depends on its endpoints. Keep those dependencies even when
    /// canonicalization removes the corresponding mutation from the template.
    fn mutation_footprint(&self) -> std::collections::BTreeSet<ElementId> {
        let mut footprint = std::collections::BTreeSet::new();
        for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
            match pending {
                PendingRow::Vertex { vid, .. }
                | PendingRow::DeleteVertex { vid, .. }
                | PendingRow::SetLabel { vid, .. }
                | PendingRow::SetProperty { vid, .. } => {
                    footprint.insert(ElementId::Vertex(*vid));
                }
                PendingRow::Edge { eid, src, dst, .. } => {
                    footprint.insert(ElementId::Edge(*eid));
                    footprint.insert(ElementId::Vertex(*src));
                    footprint.insert(ElementId::Vertex(*dst));
                }
                PendingRow::DeleteEdge { eid, .. }
                | PendingRow::SetEdgeProperty { eid, .. } => {
                    footprint.insert(ElementId::Edge(*eid));
                }
                PendingRow::CompareAndSet { elem, .. } => {
                    footprint.insert(*elem);
                }
            }
        }
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                for row in &coordinate.rows {
                    // Include engine-derived cascade targets, not only the
                    // identifiers the caller happened to name explicitly.
                    crate::touched_elements(row, &mut footprint);
                }
            }
        }
        footprint
    }

    fn transaction_conflict<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<Option<(&'static str, ElementId, CommitSeq)>, ReadError> {
        let read_set = self.read_set.borrow();
        let match_expansions = self.match_expansions.borrow();
        let scanned_vertex_labels = self.scanned_vertex_labels.borrow();
        let mutation_footprint = self.mutation_footprint();
        let scanned_vertices = self.scanned_vertices.get();
        let scanned_edges = self.scanned_edges.get();
        if read_set.is_empty()
            && match_expansions.is_empty()
            && scanned_vertex_labels.is_empty()
            && mutation_footprint.is_empty()
            && !scanned_vertices
            && !scanned_edges
        {
            return Ok(None);
        }
        // One complete suffix serves both read and mutation validation. Do not
        // trust the coordinator's resettable in-memory FCW map for an old basis.
        // delta_since refuses a retired prefix instead of silently validating
        // against only the surviving tail. Validation precedes prepared.take().
        for batch in database.delta_since(self.basis)? {
            let seq = batch.commit_seq();
            let mut touched = std::collections::BTreeSet::new();
            let mut endpoints = std::collections::BTreeSet::new();
            for coordinate in batch.coordinate_entries() {
                for row in &coordinate.rows {
                    match row {
                        fgdb_delta_types::DeltaRow::CreateVertex { vid, labels, .. }
                            if scanned_vertices
                                || labels
                                    .iter()
                                    .any(|label| scanned_vertex_labels.contains(label)) =>
                        {
                            return Ok(Some(("FG-LAW-FCW-READ-01", ElementId::Vertex(*vid), seq)));
                        }
                        fgdb_delta_types::DeltaRow::LabelMembership {
                            vid,
                            label,
                            after: true,
                            ..
                        } if scanned_vertex_labels.contains(label) => {
                            return Ok(Some(("FG-LAW-FCW-READ-01", ElementId::Vertex(*vid), seq)));
                        }
                        fgdb_delta_types::DeltaRow::CreateEdge {
                            eid, src, relation, ..
                        } if scanned_edges || match_expansions.contains(&(*src, *relation)) => {
                            return Ok(Some(("FG-LAW-FCW-READ-01", ElementId::Edge(*eid), seq)));
                        }
                        _ => {}
                    }
                    crate::adjacency_endpoints(row, &mut endpoints);
                    crate::touched_elements(row, &mut touched);
                }
            }
            if let Some(element) = endpoints
                .iter()
                .chain(touched.iter())
                .find(|element| read_set.contains(*element))
            {
                return Ok(Some(("FG-LAW-FCW-READ-01", *element, seq)));
            }
            if let Some(element) = endpoints
                .iter()
                .chain(touched.iter())
                .find(|element| mutation_footprint.contains(*element))
            {
                return Ok(Some(("FG-LAW-FCW-01", *element, seq)));
            }
        }
        Ok(None)
    }

    fn release_pin(&mut self) {
        if let Some(pin) = self.pin.take() {
            let _receipt = pin.abort();
        }
    }
}
