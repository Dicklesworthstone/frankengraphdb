// Branch-action relationship MERGE reuses the directed edge MERGE adapter and
// ordinary edge-property writes inside one private rollback boundary.

impl WriteTxn {
    /// Execute directed relationship MERGE and the selected branch's edge-
    /// property SETs atomically inside this transaction. NoInput runs no branch.
    /// Any branch quota/storage/cancellation refusal restores the exact staged
    /// prefix while retaining the MERGE's read and absence conflict witnesses.
    pub fn execute_graph_edge_upsert_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        upsert: &fgdb_gql::PreparedGraphEdgeUpsert,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::GraphEdgeUpsertStats, fgdb_gql::GraphEdgeMergeOutcome),
        fgdb_gql::GqlQueryError<
            fgdb_gql::GraphEdgeUpsertError<WriteTxnError, A>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{
            GqlQueryError, GraphEdgeMergeOutcome, GraphEdgeUpsertBranch, GraphEdgeUpsertError,
            GraphEdgeUpsertStats,
        };
        let workspace = MutationProgramWorkspace::new(self);
        let merged = workspace.txn.execute_graph_edge_merge_governed(
            database, cx, upsert.merge(), policy.merge, allocate,
        );
        let (merge_stats, outcome) = match merged {
            Ok(value) => value,
            Err(GqlQueryError::Source(error)) => {
                return Err(GqlQueryError::Source(GraphEdgeUpsertError::Merge(error)));
            }
            Err(GqlQueryError::Rows(error)) => return Err(GqlQueryError::Rows(error)),
            Err(GqlQueryError::Evaluator(error)) => return Err(GqlQueryError::Evaluator(error)),
            Err(GqlQueryError::Interrupted(error)) => return Err(GqlQueryError::Interrupted(error)),
        };
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
        if let Some(edge) = edge
            && !actions.is_empty()
        {
            let mut batch = WriteBatch::new(upsert.merge().relation());
            for action in actions {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                batch.set_edge_property(edge, action.key, Some(action.value.value().clone()));
            }
            workspace.txn.write(database, batch)
                .map_err(|error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error)))?;
        }
        let stats = GraphEdgeUpsertStats {
            merge: merge_stats,
            branch,
            action_effects: actions.len() as u64,
        };
        workspace.accept();
        Ok((stats, outcome))
    }
}
