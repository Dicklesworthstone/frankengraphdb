// Autocommit vertex upsert is begin -> atomic MERGE+branch actions -> finish.

impl<V: Vfs + Clone> Database<V> {
    /// Execute unique vertex MERGE plus its selected ON MATCH/ON CREATE action
    /// branch in one private transaction. A branch refusal aborts the whole
    /// private workspace; a matched branch with real actions commits those
    /// actions, while a branch with no staged effects may close read-only.
    pub async fn execute_graph_vertex_upsert_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        upsert: &fgdb_gql::PreparedGraphVertexUpsert,
        policy: fgdb_gql::GraphVertexUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphVertexUpsertStats,
            fgdb_gql::GraphVertexMergeOutcome,
            EmbeddedTxnCompletion,
        ),
        fgdb_gql::GqlQueryError<
            fgdb_gql::GraphVertexUpsertError<WriteTxnError, A>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{GqlQueryError, GraphVertexUpsertError};
        let infrastructure = |error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error));
        let mut transaction = self.begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, outcome) = match transaction.execute_graph_vertex_upsert_governed(
            self, query_cx, upsert, policy, allocate,
        ) {
            Ok(value) => value,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction.finish(self, commit_cx).await.map_err(infrastructure)?;
        Ok((stats, outcome, completion))
    }
}
