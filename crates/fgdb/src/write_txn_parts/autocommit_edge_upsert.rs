// Autocommit relationship upsert is begin -> atomic MERGE+branch actions -> finish.

impl<V: Vfs + Clone> Database<V> {
    pub async fn execute_graph_edge_upsert_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        upsert: &fgdb_gql::PreparedGraphEdgeUpsert,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphEdgeUpsertStats,
            fgdb_gql::GraphEdgeMergeOutcome,
            EmbeddedTxnCompletion,
        ),
        fgdb_gql::GqlQueryError<
            fgdb_gql::GraphEdgeUpsertError<WriteTxnError, A>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{GqlQueryError, GraphEdgeUpsertError};
        let infrastructure = |error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, outcome) = match transaction
            .execute_graph_edge_upsert_governed(self, query_cx, upsert, policy, allocate)
        {
            Ok(value) => value,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((stats, outcome, completion))
    }
}
