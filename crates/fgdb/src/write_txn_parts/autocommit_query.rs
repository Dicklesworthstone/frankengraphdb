// Returning mutations and vertex upserts use the ordinary stage -> finish
// lifecycle, as CREATE RETURN does. No result escapes before completion.

impl<V: Vfs + Clone> Database<V> {
    /// Execute MATCH ... SET/REMOVE/DETACH DELETE ... RETURN in one private
    /// transaction. The native statement evaluates and admits its complete
    /// result before staging. RETURN pagination does not limit write effects;
    /// LIMIT 0 may therefore complete with WriteCommitted and no rows.
    ///
    /// A prepublication refusal aborts the private transaction and releases its
    /// snapshot obligation. Completion failures retain their original cause,
    /// including unknown/durable-needs-recovery outcomes; they do not imply
    /// rollback and contain no successful result prefix.
    pub async fn execute_graph_mutation_query_autocommit_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        query: &fgdb_gql::PreparedGraphMutationQuery,
        policy: fgdb_gql::GraphMutationPolicy,
    ) -> Result<
        (
            fgdb_gql::GraphMutationStats,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        TxnGqlError<fgdb_gql::GraphMutationQueryError<WriteTxnError>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphMutationError, GraphMutationQueryError};
        let infrastructure = |error| {
            GqlQueryError::Source(GraphMutationQueryError::Mutation(
                GraphMutationError::Source(error),
            ))
        };
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, rows) = match transaction
            .execute_graph_mutation_query_governed(self, query_cx, query, policy)
        {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((stats, rows, completion))
    }

    /// Execute vertex MERGE, its ON CREATE/ON MATCH actions and trailing SET,
    /// then RETURN, and finish one ordinary transaction. The result reads the
    /// selected vertex's post-clause state, not a later live database snapshot.
    /// RETURN expression or quota failures discard all effects of this call.
    ///
    /// Identity allocation remains caller-owned. Matched MERGE does not call
    /// the allocator; a failed result never licenses reuse of an issued ID.
    pub async fn execute_graph_vertex_upsert_query_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        query: &fgdb_gql::PreparedGraphVertexUpsertQuery,
        policy: fgdb_gql::GraphVertexUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphVertexUpsertStats,
            fgdb_gql::GraphVertexMergeOutcome,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        TxnGqlError<fgdb_gql::GraphVertexUpsertError<WriteTxnError, A>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphVertexUpsertError};
        let infrastructure = |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, outcome, rows) = match transaction
            .execute_graph_vertex_upsert_query_governed(self, query_cx, query, policy, allocate)
        {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((stats, outcome, rows, completion))
    }

    /// MERGE RETURN autocommit with IDs reserved by this database. Uses the
    /// same statement engine and completion rules as the caller-allocated API.
    pub async fn execute_graph_vertex_upsert_query_autocommit_engine_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        query: &fgdb_gql::PreparedGraphVertexUpsertQuery,
        policy: fgdb_gql::GraphVertexUpsertPolicy,
    ) -> Result<
        (
            fgdb_gql::GraphVertexUpsertStats,
            fgdb_gql::GraphVertexMergeOutcome,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        TxnGqlError<fgdb_gql::GraphVertexUpsertError<WriteTxnError, WriteTxnError>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphVertexUpsertError};
        let infrastructure = |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, outcome, rows) = match transaction
            .execute_graph_vertex_upsert_query_engine_governed(self, query_cx, query, policy)
        {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((stats, outcome, rows, completion))
    }
}
