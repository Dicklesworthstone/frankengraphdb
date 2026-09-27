// Branch-action relationship MERGE shares one lowering and resource contract
// between native transactions and capability-scoped execution.

fn edge_upsert_actions<E, A, C>(
    upsert: &fgdb_gql::PreparedGraphEdgeUpsert,
    policy: fgdb_gql::GraphEdgeUpsertPolicy,
    merge_stats: fgdb_gql::GraphEdgeMergeStats,
    outcome: fgdb_gql::GraphEdgeMergeOutcome,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<
    (fgdb_gql::GraphEdgeUpsertStats, WriteBatch),
    fgdb_gql::GqlQueryError<fgdb_gql::GraphEdgeUpsertError<E, A>, C>,
> {
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlQueryError,
        GraphEdgeMergeOutcome, GraphEdgeUpsertBranch, GraphEdgeUpsertError, GraphEdgeUpsertStats,
    };
    let (branch, actions, edge) = match outcome {
        GraphEdgeMergeOutcome::NoInput => (GraphEdgeUpsertBranch::NoInput, &[][..], None),
        GraphEdgeMergeOutcome::Matched(edge) => {
            (GraphEdgeUpsertBranch::Match, upsert.on_match(), Some(edge))
        }
        GraphEdgeMergeOutcome::Created(edge) => (
            GraphEdgeUpsertBranch::Create,
            upsert.on_create(),
            Some(edge),
        ),
    };
    let observed = actions.len() as u128;
    if observed > u128::from(policy.max_actions) {
        return Err(GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit {
            limit: policy.max_actions,
            observed,
        }));
    }
    let mut evaluator = merge_stats.evaluator;
    let mut event = |kind: GlaExecutionEvent| -> Result<
        (), fgdb_gql::GqlQueryError<GraphEdgeUpsertError<E, A>, C>,
    > {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
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
            // Preserve the exact native accounting: one entry/work event per
            // selected action, before copying its already-admitted scalar.
            event(GlaExecutionEvent::ScratchEntry)?;
            batch.set_edge_property(edge, action.key, Some(action.value.value().clone()));
        }
    }
    // NoInput still crosses the final acceptance event, with zero actions.
    event(GlaExecutionEvent::Work)?;
    Ok((
        GraphEdgeUpsertStats {
            merge: merge_stats,
            branch,
            action_effects: actions.len() as u64,
            evaluator,
        },
        batch,
    ))
}

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
        (
            fgdb_gql::GraphEdgeUpsertStats,
            fgdb_gql::GraphEdgeMergeOutcome,
        ),
        TxnGqlError<fgdb_gql::GraphEdgeUpsertError<WriteTxnError, A>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphEdgeUpsertError};
        // Reject a foreign or terminal transaction before cloning its workspace.
        self.ensure_database(database)
            .map_err(|error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error)))?;
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            let (merge_stats, outcome) = workspace
                .txn
                .execute_graph_edge_merge_governed(
                    database,
                    cx,
                    upsert.merge(),
                    policy.merge,
                    allocate,
                )
                .map_err(|error| error.map_source(GraphEdgeUpsertError::Merge))?;
            let (stats, batch) = edge_upsert_actions::<WriteTxnError, A, _>(
                upsert,
                policy,
                merge_stats,
                outcome,
                || cx.checkpoint(),
            )?;
            if !batch.is_empty() {
                workspace
                    .txn
                    .write(database, batch)
                    .map_err(|error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error)))?;
            }
            // No fallible operation follows the atomic stage or acceptance.
            workspace.accept();
            Ok((stats, outcome))
        })
    }
}
