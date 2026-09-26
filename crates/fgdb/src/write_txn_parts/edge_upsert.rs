// Branch-action relationship MERGE reuses the directed edge MERGE adapter and
// ordinary edge-property writes inside one private rollback boundary.

impl WriteTxn {
    /// Execute directed relationship MERGE and the selected branch's edge-
    /// property SETs atomically inside this transaction. NoInput runs no branch.
    /// Any branch quota/storage/cancellation refusal restores the exact staged
    /// prefix while retaining the MERGE's read and absence conflict witnesses.
    /// Issued external identities cannot be reclaimed by workspace rollback.
    pub fn execute_graph_edge_upsert_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        upsert: &fgdb_gql::PreparedGraphEdgeUpsert,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::GraphEdgeUpsertStats, fgdb_gql::GraphEdgeMergeOutcome),
        TxnGqlError<fgdb_gql::GraphEdgeUpsertError<WriteTxnError, A>>,
    > {
        use fgdb_gql::{
            GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlQueryError,
            GraphEdgeMergeOutcome, GraphEdgeUpsertBranch, GraphEdgeUpsertError,
            GraphEdgeUpsertStats,
        };
        // Reject a foreign or terminal transaction before cloning its workspace.
        self.ensure_database(database)
            .map_err(|error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error)))?;
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            let (merge_stats, outcome) = workspace.txn.execute_graph_edge_merge_governed(
                database, cx, upsert.merge(), policy.merge, allocate,
            ).map_err(|error| error.map_source(GraphEdgeUpsertError::Merge))?;
            let (branch, actions, edge) = match outcome {
                GraphEdgeMergeOutcome::NoInput => (GraphEdgeUpsertBranch::NoInput, &[][..], None),
                GraphEdgeMergeOutcome::Matched(edge) =>
                    (GraphEdgeUpsertBranch::Match, upsert.on_match(), Some(edge)),
                GraphEdgeMergeOutcome::Created(edge) =>
                    (GraphEdgeUpsertBranch::Create, upsert.on_create(), Some(edge)),
            };
            let observed = actions.len() as u128;
            if observed > u128::from(policy.max_actions) {
                return Err(GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit {
                    limit: policy.max_actions, observed,
                }));
            }

            let mut evaluator = merge_stats.evaluator;
            let mut event = |kind: GlaExecutionEvent| -> Result<
                (), GqlQueryError<GraphEdgeUpsertError<WriteTxnError, A>, Box<asupersync::error::Error>>,
            > {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                let work = u128::from(evaluator.work_units) + 1;
                let scratch = u128::from(evaluator.scratch_entries)
                    + u128::from(kind == GlaExecutionEvent::ScratchEntry);
                for (observed, limit, dimension) in [
                    (work, policy.merge.query.evaluator.max_work_units, GlaLimitDimension::WorkUnits),
                    (scratch, policy.merge.query.evaluator.max_scratch_entries, GlaLimitDimension::ScratchEntries),
                ] {
                    if observed > u128::from(limit) {
                        return Err(GqlQueryError::Evaluator(GlaLimitExceeded { dimension, limit, observed }));
                    }
                }
                evaluator.work_units = work as u64;
                evaluator.scratch_entries = scratch as u64;
                Ok(())
            };

            let mut batch = WriteBatch::new(upsert.merge().relation());
            if let Some(edge) = edge {
                for action in actions {
                    // Admission precedes each owned property copy, not merely
                    // the final storage call. Branch work shares MERGE's quota.
                    event(GlaExecutionEvent::ScratchEntry)?;
                    batch.set_edge_property(edge, action.key, Some(action.value.value().clone()));
                }
            }
            // Last fallible control point for every branch, including NoInput.
            // No cancellation boundary follows staging or workspace acceptance.
            event(GlaExecutionEvent::Work)?;
            if !actions.is_empty() {
                workspace.txn.write(database, batch)
                    .map_err(|error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error)))?;
            }
            let stats = GraphEdgeUpsertStats {
                merge: merge_stats, branch, action_effects: actions.len() as u64, evaluator,
            };
            workspace.accept();
            Ok((stats, outcome))
        })
    }
}
