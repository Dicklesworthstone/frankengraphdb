include!("mixed_rebase.rs");

enum RebasePreparation {
    Automatic,
    Append(u64),
    DisjointFields(u64),
    Mixed(u64),
}

enum ConflictScope {
    Reads,
    ReadsAndWrites,
}

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
        Self {
            transaction,
            database,
            starting_frontier,
            entered_commit: false,
        }
    }
}

impl<V: Vfs + Clone> Drop for TxnCompletionGuard<'_, '_, V> {
    fn drop(&mut self) {
        if !self.transaction.state.is_terminal() {
            self.transaction.state = if self.entered_commit {
                match self.database.state() {
                    crate::DatabaseState::Healthy { published_frontier }
                        if published_frontier == self.starting_frontier =>
                    {
                        EmbeddedTxnState::Aborted
                    }
                    crate::DatabaseState::Healthy { published_frontier }
                        if published_frontier.0 > self.starting_frontier.0 =>
                    {
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

/// The sole variable-width touched-element arm is the engine-derived cascade.
/// Check it element by element instead of hiding an uninterruptible loop inside
/// the shared infallible collector. Other current arms touch at most one ID.
fn validation_touches(
    row: &fgdb_delta_types::DeltaRow,
    touched: &mut std::collections::BTreeSet<ElementId>,
    checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
) -> Result<(), WriteTxnError> {
    checkpoint()?;
    if let fgdb_delta_types::DeltaRow::DeleteVertex {
        vid,
        sorted_retired_incident_edges,
        ..
    } = row
    {
        touched.insert(ElementId::Vertex(*vid));
        for eid in sorted_retired_incident_edges {
            checkpoint()?;
            touched.insert(ElementId::Edge(*eid));
        }
    } else {
        crate::touched_elements(row, touched);
    }
    Ok(())
}

impl WriteTxn {
    /// Validate recorded dependencies and complete either kind of workspace.
    /// A prepared batch returns WriteCommitted, including a normalized no-op.
    /// With no prepared batch, return ReadClosed without writing a capsule,
    /// marker, root or sequence. ReadClosed retains the original snapshot and
    /// separately reports the healthy frontier through which reads validated.
    ///
    /// A write conflict may trigger one deterministic replay of the retained
    /// raw mutation program: independent appends, disjoint field edits and the
    /// supported mixed creation/edit/retirement program use their existing
    /// independence laws. Every original read witness must validate first.
    /// Replay runs under MergeEvalCx, is bounded by the exact evaluator-input
    /// rows admitted at preparation, and must reproduce the original canonical
    /// effects. No application callback or query is repeated. Ineligible
    /// conflicts keep their original FCW error. Live savepoints, active program
    /// scopes and authorization-bound writes retain strict behavior.
    /// Use finish_strict to retain element-level conflict rejection throughout.
    ///
    /// The validation contract is the existing conservative embedded read and
    /// scan witness contract, not full SSI or the durable session protocol.
    /// A dropped UNPOLLED future has no effect. Once polled and admitted by the
    /// owner, every terminal return, unwind or dropped future releases the pin
    /// and workspace; state() distinguishes abort from unknown or committed.
    /// Wrong-owner calls preserve all state for the correct database.
    /// Validation checkpoints every scalable traversal, including cascades;
    /// no new cancellation checkpoint follows publication or read acceptance.
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
        self.complete(
            database,
            cx,
            crash_at,
            false,
            Some(RebasePreparation::Automatic),
        )
        .await
    }

    /// Complete using the original element-level conflict policy, without
    /// semantic replay. Read-only completion and terminal cleanup are the same
    /// as finish; a conflicting prepared write is refused at its original basis.
    pub async fn finish_strict<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
    ) -> Result<EmbeddedTxnCompletion, WriteTxnError> {
        self.complete(database, cx, None, false, None).await
    }

    /// Require a prepared write, validate it, commit and release its pin.
    /// For a workspace that may be read-only, use finish's typed outcome.
    /// Uses the same bounded automatic semantic replay as finish.
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
        match self
            .complete(
                database,
                cx,
                crash_at,
                true,
                Some(RebasePreparation::Automatic),
            )
            .await?
        {
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Ok(commit_seq),
            EmbeddedTxnCompletion::ReadClosed { .. } => {
                unreachable!("write-only completion refuses before read close")
            }
        }
    }

    /// Require a prepared write and commit with strict element-level conflict
    /// rejection. This opts out of commit's automatic semantic replay while
    /// retaining the same owner guard, pin cleanup and publication path.
    pub async fn commit_strict<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
    ) -> Result<CommitSeq, WriteTxnError> {
        match self.complete(database, cx, None, true, None).await? {
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Ok(commit_seq),
            EmbeddedTxnCompletion::ReadClosed { .. } => {
                unreachable!("write-only completion refuses before read close")
            }
        }
    }

    // Type-erased because every finish and commit crosses this one boundary:
    // a caller's `Send` proof stops at `dyn Future + Send` instead of
    // descending through validation and the commit tail (fgdb-a5y6m).
    fn complete<'a, V: Vfs + Clone>(
        &'a mut self,
        database: &'a mut Database<V>,
        cx: &'a CommitCx,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
        require_write: bool,
        rebase: Option<RebasePreparation>,
    ) -> crate::SendFuture<'a, Result<EmbeddedTxnCompletion, WriteTxnError>> {
        Box::pin(cx.with_restriction_async(self.complete_rebased_controlled(
            database,
            cx,
            crash_at,
            require_write,
            rebase,
            || cx.checkpoint().map_err(WriteTxnError::Interrupted),
        )))
    }

    // The private checkpoint seam is shared verbatim by production and the
    // interruption/unwind tests. Authorized callers also use this STRICT seam:
    // their original-basis authorization never implicitly grants replay authority.
    async fn complete_controlled<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
        require_write: bool,
        checkpoint: impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<EmbeddedTxnCompletion, WriteTxnError> {
        self.complete_with_basis_controlled(database, cx, crash_at, require_write, None, checkpoint)
            .await
    }

    // Explicit re-evaluation stays INSIDE the same terminal guard and BEFORE
    // ordinary validation/publication. Every existing caller retains its basis.
    async fn complete_with_basis_controlled<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
        require_write: bool,
        append_rebase: Option<u64>,
        checkpoint: impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<EmbeddedTxnCompletion, WriteTxnError> {
        self.complete_rebased_controlled(
            database,
            cx,
            crash_at,
            require_write,
            append_rebase.map(RebasePreparation::Append),
            checkpoint,
        )
        .await
    }

    // Every finalization policy shares this one owner guard, acceptance point
    // and commit future. Policy selection never creates another publication lane.
    async fn complete_rebased_controlled<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
        require_write: bool,
        rebase: Option<RebasePreparation>,
        mut checkpoint: impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<EmbeddedTxnCompletion, WriteTxnError> {
        self.ensure_database(database)?;
        let mut attempt = TxnCompletionGuard::new(self, database);
        // Health must be checked even for an empty read set. No snapshot
        // observation can bless a handle fenced by an earlier ambiguous write.
        let validated_through = attempt.database.frontier()?;
        if require_write && attempt.transaction.prepared.is_none() {
            return Err(WriteTxnError::NoPreparedWrite);
        }
        checkpoint()?;
        attempt
            .database
            .ensure_delta_window(cx, attempt.transaction.basis)
            .await
            .map_err(|error| match error {
                crate::RebuildError::Interrupted(error) => WriteTxnError::Interrupted(error),
                error => WriteTxnError::Write(WriteError::from(error)),
            })?;
        // A new apply basis must never erase the interval over which earlier
        // point, negative and query observations need validation. Reuse the
        // ordinary witness checker BEFORE any policy can replace the basis.
        // Only write conflicts receive policy-specific independence rules.
        if rebase.is_some()
            && let Some((law, element, committed_at)) = attempt
                .transaction
                .transaction_conflict_in(attempt.database, ConflictScope::Reads, &mut checkpoint)?
        {
            return Err(WriteError::FirstCommitterWins {
                law,
                detail: format!(
                    "rebase read {element:?} changed at {committed_at:?} after pinned basis {:?}",
                    attempt.transaction.basis
                ),
            }
            .into());
        }
        // Intent replay runs under the capability-empty merge context only
        // (FG-INV-17); the commit context stays with validation and publication.
        let merge = cx.merge_eval();
        match rebase {
            Some(RebasePreparation::Automatic) => attempt.transaction.prepare_automatic_rebase(
                attempt.database,
                &merge,
                &mut checkpoint,
            )?,
            Some(RebasePreparation::Append(limit)) => attempt.transaction.prepare_append_rebase(
                attempt.database,
                limit,
                &merge,
                &mut checkpoint,
            )?,
            Some(RebasePreparation::DisjointFields(limit)) => attempt
                .transaction
                .prepare_field_rebase(attempt.database, limit, &merge, &mut checkpoint)?,
            Some(RebasePreparation::Mixed(limit)) => attempt.transaction.prepare_mixed_rebase(
                attempt.database,
                limit,
                &merge,
                &mut checkpoint,
            )?,
            None => {}
        }
        if let Some((law, element, committed_at)) = attempt
            .transaction
            .transaction_conflict(attempt.database, &mut checkpoint)?
        {
            return Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law,
                detail: format!(
                    "transaction dependency {element:?} changed at {committed_at:?} after pinned basis {:?}",
                    attempt.transaction.basis
                ),
            }));
        }
        // The final validation checkpoint precedes BOTH kinds of acceptance.
        // Never return Interrupted after a marker or a successful read close.
        checkpoint()?;
        let completion = if let Some(prepared) = attempt.transaction.prepared.take() {
            attempt.transaction.staged.clear();
            // Completion consumes every rollback point. Release their shared
            // prefixes before taking ownership for the ordinary commit path.
            attempt.transaction.savepoints.clear();
            let mut prepared = std::sync::Arc::unwrap_or_clone(prepared);
            if prepared.authorization_decision_digest.is_none() {
                prepared.authorization_decision_digest =
                    attempt.transaction.authorization_decision_digest;
            }
            // Set before polling the cancellable publication future. Drop
            // consults its retained database fence, never guesses rollback.
            attempt.entered_commit = true;
            let commit_seq = attempt
                .database
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

    // Automatic replay is an escape from an actual conservative write conflict,
    // never an eager refresh of every stale workspace. The caller has already
    // checked every original read witness under this same terminal guard.
    fn prepare_automatic_rebase<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        merge: &fgdb_types::MergeEvalCx,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<(), WriteTxnError> {
        // The ordinary final check below still validates this workspace. No
        // drift can require replay, so do not build its mutation footprint twice.
        if self.prepared.is_none() || self.basis == database.frontier()? {
            return Ok(());
        }
        let Some((law, element, committed_at)) = self.transaction_conflict(database, checkpoint)?
        else {
            return Ok(());
        };
        let original_conflict = WriteTxnError::Write(WriteError::FirstCommitterWins {
            law,
            detail: format!(
                "transaction dependency {element:?} changed at {committed_at:?} after pinned basis {:?}",
                self.basis
            ),
        });
        if law != "FG-LAW-FCW-01"
            || !self.savepoints.is_empty()
            || self.program_multi_relation
            || self.authorization_decision_digest.is_some()
        {
            return Err(original_conflict);
        }
        let previous = self
            .prepared
            .as_ref()
            .expect("prepared write checked above");
        let Some(limit) = previous.replay_row_allowance else {
            return Err(original_conflict);
        };
        if previous.authorization_decision_digest.is_some() {
            return Err(original_conflict);
        }
        // This embedded evaluator is defined only for its fixed graph, branch
        // and schema epoch. Inspect coordinate metadata even when it has no
        // rows: a row-free schema transition must not authorize replay.
        let supported_coordinate = |coordinate: &fgdb_delta_types::CoordinateEntry| {
            coordinate.graph == crate::GRAPH
                && coordinate.branch == crate::BRANCH
                && coordinate.schema_epoch == fgdb_delta_types::SchemaEpoch(0)
                && coordinate.schema_transition.is_none()
        };
        for coordinate in previous.template.coordinate_entries() {
            checkpoint()?;
            if !supported_coordinate(coordinate) {
                return Err(original_conflict);
            }
        }
        for batch in database.delta_since(self.basis)? {
            checkpoint()?;
            for coordinate in batch.coordinate_entries() {
                checkpoint()?;
                if !supported_coordinate(coordinate) {
                    return Err(original_conflict);
                }
            }
        }
        let mut appends = false;
        let mut fields = false;
        let mut retirements = false;
        for batch in &self.staged {
            checkpoint()?;
            for row in &batch.rows {
                checkpoint()?;
                match row {
                    PendingRow::Vertex { ensure: false, .. }
                    | PendingRow::Edge { ensure: false, .. } => appends = true,
                    PendingRow::SetProperty { .. }
                    | PendingRow::SetEdgeProperty { .. }
                    | PendingRow::SetLabel { .. }
                    | PendingRow::CompareAndSet { .. } => fields = true,
                    PendingRow::DeleteVertex { .. } | PendingRow::DeleteEdge { .. } => {
                        retirements = true;
                    }
                    // Raw ENSURE remains a decision even when normalization
                    // removed its effect. No idempotence is inferred here.
                    _ => return Err(original_conflict),
                }
            }
        }
        // Select exactly one existing replay law; never retry a rejected rung
        // through a more permissive policy or rerun application code.
        let result = if retirements || (appends && fields) {
            self.prepare_mixed_rebase(database, limit, merge, checkpoint)
        } else if appends {
            self.prepare_append_rebase(database, limit, merge, checkpoint)
        } else if fields {
            self.prepare_field_rebase(database, limit, merge, checkpoint)
        } else {
            return Err(original_conflict);
        };
        match result {
            Err(
                WriteTxnError::AppendRebaseIneligible
                | WriteTxnError::FieldRebaseIneligible
                | WriteTxnError::MixedRebaseIneligible
                | WriteTxnError::OrderedWriteBudgetExceeded { .. }
                | WriteTxnError::UnsupportedAtomicMutation
                | WriteTxnError::Write(WriteError::FirstCommitterWins { .. }),
            ) => Err(original_conflict),
            // Cancellation, history/rebuild failure and native evaluator
            // errors retain their meaning; they are not eligibility refusals.
            result => result,
        }
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
        self.authorization_decision_digest = None;
        self.savepoints.clear();
        self.read_set.get_mut().clear();
        self.point_reads.get_mut().clear();
        self.match_expansions.get_mut().clear();
        self.scanned_vertex_labels.get_mut().clear();
        self.scanned_vertices.set(false);
        self.scanned_edges.set(false);
        self.scanned_edge_relations.get_mut().clear();
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
        props: &mut Vec<(fgdb_delta_types::PropertyKeyId, CanonicalPropertyValue)>,
        key: fgdb_delta_types::PropertyKeyId,
        value: Option<&CanonicalPropertyValue>,
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
    fn mutation_footprint(
        &self,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<std::collections::BTreeSet<ElementId>, WriteTxnError> {
        let mut footprint = std::collections::BTreeSet::new();
        for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
            checkpoint()?;
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
                PendingRow::DeleteEdge { eid, .. } | PendingRow::SetEdgeProperty { eid, .. } => {
                    footprint.insert(ElementId::Edge(*eid));
                }
                PendingRow::CompareAndSet { elem, .. } => {
                    footprint.insert(*elem);
                }
            }
        }
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                checkpoint()?;
                for row in &coordinate.rows {
                    validation_touches(row, &mut footprint, checkpoint)?;
                }
            }
        }
        Ok(footprint)
    }

    fn transaction_conflict<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<Option<(&'static str, ElementId, CommitSeq)>, WriteTxnError> {
        self.transaction_conflict_in(database, ConflictScope::ReadsAndWrites, checkpoint)
    }

    // One witness interpretation for ordinary completion, refresh and rebase.
    // Reads are never refined to just output identities or to current values:
    // every original positive/negative observation and intervening write counts.
    fn transaction_conflict_in<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        scope: ConflictScope,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<Option<(&'static str, ElementId, CommitSeq)>, WriteTxnError> {
        let read_set = self.read_set.borrow();
        let point_reads = self.point_reads.borrow();
        let match_expansions = self.match_expansions.borrow();
        let scanned_vertex_labels = self.scanned_vertex_labels.borrow();
        let mutation_footprint = match scope {
            ConflictScope::Reads => std::collections::BTreeSet::new(),
            ConflictScope::ReadsAndWrites => self.mutation_footprint(checkpoint)?,
        };
        let scanned_vertices = self.scanned_vertices.get();
        let scanned_edges = self.scanned_edges.get();
        let scanned_edge_relations = self.scanned_edge_relations.borrow();
        if read_set.is_empty()
            && point_reads.is_empty()
            && match_expansions.is_empty()
            && scanned_vertex_labels.is_empty()
            && scanned_edge_relations.is_empty()
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
            checkpoint()?;
            let seq = batch.commit_seq();
            let mut touched = std::collections::BTreeSet::new();
            let mut endpoints = std::collections::BTreeSet::new();
            for coordinate in batch.coordinate_entries() {
                checkpoint()?;
                for row in &coordinate.rows {
                    checkpoint()?;
                    match row {
                        fgdb_delta_types::DeltaRow::CreateVertex { vid, labels, .. } => {
                            if scanned_vertices {
                                return Ok(Some((
                                    "FG-LAW-FCW-READ-01",
                                    ElementId::Vertex(*vid),
                                    seq,
                                )));
                            }
                            for label in labels {
                                checkpoint()?;
                                if scanned_vertex_labels.contains(label) {
                                    return Ok(Some((
                                        "FG-LAW-FCW-READ-01",
                                        ElementId::Vertex(*vid),
                                        seq,
                                    )));
                                }
                            }
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
                        } if scanned_edges
                            || scanned_edge_relations.contains(relation)
                            || match_expansions.contains(&(*src, *relation)) =>
                        {
                            return Ok(Some(("FG-LAW-FCW-READ-01", ElementId::Edge(*eid), seq)));
                        }
                        _ => {}
                    }
                    if let Some(element) = point_reads.conflict(row, checkpoint)? {
                        return Ok(Some(("FG-LAW-FCW-READ-01", element, seq)));
                    }
                    if let Some(element) =
                        point_reads.vertex_scan_conflict(database, row, seq, checkpoint)?
                    {
                        return Ok(Some(("FG-LAW-FCW-READ-01", element, seq)));
                    }
                    crate::adjacency_endpoints(row, &mut endpoints);
                    validation_touches(row, &mut touched, checkpoint)?;
                }
            }
            // Preserve read-before-write conflict precedence and each set's
            // canonical order. Every inspected element is cancellable.
            for element in endpoints.iter().chain(touched.iter()) {
                checkpoint()?;
                if read_set.contains(element) {
                    return Ok(Some(("FG-LAW-FCW-READ-01", *element, seq)));
                }
            }
            for element in endpoints.iter().chain(touched.iter()) {
                checkpoint()?;
                if mutation_footprint.contains(element) {
                    return Ok(Some(("FG-LAW-FCW-01", *element, seq)));
                }
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

#[cfg(test)]
mod completion_tests {
    include!("completion_tests.rs");
}

#[cfg(test)]
mod automatic_rebase_tests {
    include!("automatic_rebase_tests.rs");
}
