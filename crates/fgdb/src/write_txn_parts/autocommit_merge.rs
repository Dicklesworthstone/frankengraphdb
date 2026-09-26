// Autocommit MERGE composes begin -> unique match/create staging -> finish.
// Match semantics, allocator rules and conflict witnesses remain in vertex_merge.rs.

impl<V: Vfs + Clone> Database<V> {
    /// Execute one unique-vertex MERGE as an embedded autocommit operation.
    /// Existing match => ReadClosed; created vertex => WriteCommitted. Ambiguity,
    /// allocation/storage refusal or cancellation aborts the private transaction
    /// and releases its snapshot obligation before returning the error.
    pub async fn execute_graph_vertex_merge_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        merge: &fgdb_gql::PreparedGraphVertexMerge,
        policy: fgdb_gql::GraphVertexMergePolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphVertexMergeStats,
            fgdb_gql::GraphVertexMergeOutcome,
            EmbeddedTxnCompletion,
        ),
        fgdb_gql::GqlQueryError<
            fgdb_gql::GraphVertexMergeError<WriteTxnError, A>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{GqlQueryError, GraphVertexMergeError};
        let infrastructure = |error| GqlQueryError::Source(GraphVertexMergeError::Source(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, outcome) = match transaction
            .execute_graph_vertex_merge_governed(self, query_cx, merge, policy, allocate)
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
