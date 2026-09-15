// Autocommit relationship MERGE is begin -> overlay-aware MERGE -> finish.
// Match/create semantics and conflict witnesses remain in edge_merge.rs.

impl<V: Vfs + Clone> Database<V> {
    /// Execute one directed relationship MERGE in a private transaction.
    /// NoInput and Matched finish read-only; Created publishes through the
    /// ordinary transaction completion protocol. Any refusal aborts the private
    /// transaction and releases its snapshot obligation before returning.
    pub async fn execute_graph_edge_merge_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        merge: &fgdb_gql::PreparedGraphEdgeMerge,
        policy: fgdb_gql::GraphEdgeMergePolicy,
        allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphEdgeMergeStats,
            fgdb_gql::GraphEdgeMergeOutcome,
            EmbeddedTxnCompletion,
        ),
        fgdb_gql::GqlQueryError<
            fgdb_gql::GraphEdgeMergeError<WriteTxnError, A>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{GqlQueryError, GraphEdgeMergeError};
        let infrastructure = |error| GqlQueryError::Source(GraphEdgeMergeError::Source(error));
        let mut transaction = self.begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, outcome) = match transaction.execute_graph_edge_merge_governed(
            self, query_cx, merge, policy, allocate,
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
