// Branch-action vertex MERGE reuses the unique-MERGE adapter and ordinary
// WriteBatch staging inside one private rollback boundary. No nested commit.

impl WriteTxn {
    /// Execute unique vertex MERGE and then the selected branch's simultaneous
    /// literal property/label actions as one all-or-nothing staged operation.
    ///
    /// A create branch may have already asked the external allocator for an ID
    /// before a later action/storage refusal; rollback cannot reclaim that ID.
    /// The transaction workspace, however, is restored exactly to its prefix and
    /// all MATCH/absence observations remain conflict witnesses. Success still
    /// requires the caller to finish/commit the outer transaction.
    pub fn execute_graph_vertex_upsert_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        upsert: &fgdb_gql::PreparedGraphVertexUpsert,
        policy: fgdb_gql::GraphVertexUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphVertexUpsertStats,
            fgdb_gql::GraphVertexMergeOutcome,
        ),
        fgdb_gql::GqlQueryError<
            fgdb_gql::GraphVertexUpsertError<WriteTxnError, A>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{
            GqlQueryError, GraphVertexMergeOutcome, GraphVertexUpsertAction,
            GraphVertexUpsertBranch, GraphVertexUpsertError, GraphVertexUpsertStats,
        };

        let workspace = MutationProgramWorkspace::new(self);
        let merged = workspace.txn.execute_graph_vertex_merge_governed(
            database,
            cx,
            upsert.merge(),
            policy.merge,
            allocate,
        );
        let (merge_stats, outcome) = match merged {
            Ok(value) => value,
            Err(GqlQueryError::Source(error)) => {
                return Err(GqlQueryError::Source(GraphVertexUpsertError::Merge(error)));
            }
            Err(GqlQueryError::Rows(error)) => return Err(GqlQueryError::Rows(error)),
            Err(GqlQueryError::Evaluator(error)) => return Err(GqlQueryError::Evaluator(error)),
            Err(GqlQueryError::Interrupted(error)) => return Err(GqlQueryError::Interrupted(error)),
        };

        let (branch, actions) = match outcome {
            GraphVertexMergeOutcome::Matched(_) => (GraphVertexUpsertBranch::Match, upsert.on_match()),
            GraphVertexMergeOutcome::Created(_) => (GraphVertexUpsertBranch::Create, upsert.on_create()),
        };
        let observed = actions.len() as u128;
        if observed > u128::from(policy.max_actions) {
            return Err(GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
                limit: policy.max_actions,
                observed,
            }));
        }

        let vertex = outcome.vertex();
        if !actions.is_empty() {
            let mut batch = WriteBatch::new(upsert.merge().relation());
            for action in actions {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                match action {
                    GraphVertexUpsertAction::SetProperty { key, value } => {
                        batch.set_vertex_property(vertex, *key, Some(value.value().clone()));
                    }
                    GraphVertexUpsertAction::SetLabel { label, present } => {
                        batch.set_vertex_label(vertex, *label, *present);
                    }
                }
            }
            // No cancellation boundary after this synchronous staging call.
            workspace.txn.write(database, batch)
                .map_err(|error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error)))?;
        }

        let stats = GraphVertexUpsertStats {
            merge: merge_stats,
            branch,
            action_effects: actions.len() as u64,
        };
        workspace.accept();
        Ok((stats, outcome))
    }
}
