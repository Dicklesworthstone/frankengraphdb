// Plain DELETE is intentionally separate from DETACH DELETE. The GQL kernel
// freezes/deduplicates targets; this adapter alone can prove that each target
// has no live incident relationship in the canonical transaction overlay.

impl WriteTxn {
    /// Stage a non-detaching query-selected element deletion. Vertex targets
    /// must have no incident relationships remaining after explicitly selected
    /// edge targets are removed. Relationship deletion keeps both endpoints.
    /// A refusal appends no delete batch.
    ///
    /// The incident-edge scan is also a conflict witness: a concurrent edge
    /// insertion after this check invalidates completion instead of allowing a
    /// plain DELETE to turn into an implicit cascade. Success still only stages;
    /// the caller explicitly finishes or commits the transaction.
    pub fn execute_graph_delete_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        deletion: &fgdb_gql::PreparedGraphDelete,
        policy: fgdb_gql::GraphDeletePolicy,
    ) -> Result<
        fgdb_gql::GraphDeleteStats,
        TxnGqlError<fgdb_gql::GraphDeleteError<WriteTxnError>>,
    > {
        self.execute_graph_delete_returning_governed(database, cx, deletion, policy)
            .map(|(stats, _)| stats)
    }

    /// The same DELETE with its distinct staged vertex targets only; edge
    /// targets are included in statistics, not this vertex-identity receipt.
    /// This is a transaction-local receipt, not a durability acknowledgement.
    /// Returned source/work/scratch totals include the incident-edge validation
    /// and delete proposals, under the SAME allowance as MATCH. Storage overlay
    /// materialization and preparation are not bounded-memory query execution.
    pub fn execute_graph_delete_returning_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        deletion: &fgdb_gql::PreparedGraphDelete,
        policy: fgdb_gql::GraphDeletePolicy,
    ) -> Result<
        (fgdb_gql::GraphDeleteStats, Vec<VId>),
        TxnGqlError<fgdb_gql::GraphDeleteError<WriteTxnError>>,
    > {
        self.execute_graph_delete_elements_returning_governed(database, cx, deletion, policy)
            .map(|(stats, vertices, _)| (stats, vertices))
    }

    /// Return disjoint, sorted vertex and relationship target identities.
    /// Receipts describe staging, never successful durability.
    pub fn execute_graph_delete_elements_returning_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        deletion: &fgdb_gql::PreparedGraphDelete,
        policy: fgdb_gql::GraphDeletePolicy,
    ) -> Result<
        WithAffectedIds<fgdb_gql::GraphDeleteStats>,
        TxnGqlError<fgdb_gql::GraphDeleteError<WriteTxnError>>,
    > {
        use fgdb_gql::{GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded,
            GqlBudgetDimension, GraphDeleteError, GqlQueryError};
        let source = |error| GqlQueryError::Source(GraphDeleteError::Source(error));

        self.ensure_database(database).map_err(source)?;
        let live = database.frontier().map_err(WriteTxnError::from).map_err(source)?;
        if live != self.basis {
            return Err(source(WriteTxnError::SnapshotAdvanced { pinned: self.basis, live }));
        }
        if let Some(first) = self.staged.first()
            && !self.program_multi_relation
            && self.staged.iter().all(|batch| batch.relation == first.relation)
            && deletion.relation() != first.relation
        {
            return Err(source(WriteTxnError::RelationMismatch {
                expected: first.relation, found: deletion.relation(),
            }));
        }

        cx.with_restriction(|| {
            let proposal = deletion.execute_governed(
                policy,
                |pattern, budget| self.execute_graph_pattern_governed(database, cx, pattern, budget),
                || cx.checkpoint(),
            )?;
            let mut stats = proposal.stats();
            let (targets, edge_targets) = proposal.into_target_parts();
            if targets.is_empty() && edge_targets.is_empty() {
                return Ok((stats, targets, edge_targets));
            }

            // Read the exact staged overlay once. Edge deletions already staged
            // by the transaction disappear here; staged edge creations appear.
            // edges() retains both point and scan dependencies for completion.
            let edges = if targets.is_empty() { Vec::new() } else { self.edges(database).map_err(source)? };
            let records = u64::try_from(edges.len()).ok()
                .and_then(|count| stats.selection.snapshot_records.checked_add(count))
                .ok_or(GqlQueryError::Source(GraphDeleteError::InvalidSourceStatistics))?;
            policy.query.rows.check(GqlBudgetDimension::SnapshotRecords, records)
                .map_err(GqlQueryError::Rows)?;
            stats.selection.snapshot_records = records;
            let mut event = |kind: GlaExecutionEvent| -> Result<(),
                GqlQueryError<GraphDeleteError<WriteTxnError>, Box<asupersync::error::Error>>>
            {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                let work = u128::from(stats.evaluator.work_units) + 1;
                let scratch = u128::from(stats.evaluator.scratch_entries)
                    + u128::from(kind == GlaExecutionEvent::ScratchEntry);
                for (observed, limit, dimension) in [
                    (work, policy.query.evaluator.max_work_units, GlaLimitDimension::WorkUnits),
                    (scratch, policy.query.evaluator.max_scratch_entries, GlaLimitDimension::ScratchEntries),
                ] {
                    if observed > u128::from(limit) {
                        return Err(GqlQueryError::Evaluator(GlaLimitExceeded { dimension, limit, observed }));
                    }
                }
                stats.evaluator.work_units = work as u64;
                stats.evaluator.scratch_entries = scratch as u64;
                Ok(())
            };

            // The proposal is sorted and unique. Scan edges once rather than
            // rescanning every edge for every target; do not allocate an index.
            for edge in &edges {
                event(GlaExecutionEvent::Work)?;
                if (targets.binary_search(&edge.entry.src).is_ok()
                    || targets.binary_search(&edge.entry.dst).is_ok())
                    && edge_targets.binary_search(&edge.entry.eid).is_err()
                {
                    return Err(GqlQueryError::Source(GraphDeleteError::IncidentRelationships));
                }
            }

            let mut batch = WriteBatch::new(deletion.relation());
            for target in &edge_targets {
                event(GlaExecutionEvent::ScratchEntry)?;
                batch.delete_edge(*target);
            }
            for target in &targets {
                event(GlaExecutionEvent::ScratchEntry)?;
                // Storage still uses its ordinary vertex-delete representation.
                // The proof above guarantees its cascade set is empty in this
                // workspace; concurrent topology changes are caught at finish.
                batch.delete_vertex(*target);
            }
            event(GlaExecutionEvent::Work)?;
            self.write(database, batch).map_err(source)?;
            Ok((stats, targets, edge_targets))
        })
    }
}
