// Plain DELETE autocommit is only begin -> checked overlay deletion -> finish.
// The incident-edge law stays in graph_deletions.rs; this file owns lifecycle.

impl<V: Vfs + Clone> Database<V> {
    /// Execute one prepared non-detaching DELETE as an embedded autocommit.
    /// Attached targets refuse and abort the private transaction; empty target
    /// sets finish read-only without inventing a commit marker.
    pub async fn execute_graph_delete_autocommit_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        deletion: &fgdb_gql::PreparedGraphDelete,
        policy: fgdb_gql::GraphDeletePolicy,
    ) -> Result<
        (fgdb_gql::GraphDeleteStats, EmbeddedTxnCompletion),
        fgdb_gql::GqlQueryError<fgdb_gql::GraphDeleteError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphDeleteError};
        let infrastructure = |error| GqlQueryError::Source(GraphDeleteError::Source(error));
        let mut transaction = self.begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let stats = match transaction.execute_graph_delete_governed(
            self, query_cx, deletion, policy,
        ) {
            Ok(stats) => stats,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction.finish(self, commit_cx).await.map_err(infrastructure)?;
        Ok((stats, completion))
    }

    /// Autocommit DELETE plus its exact distinct vertex target identities.
    /// Edge targets are counted in stats; this receipt remains vertex-only.
    /// The target receipt is withheld until transaction completion succeeds.
    pub async fn execute_graph_delete_returning_autocommit_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        deletion: &fgdb_gql::PreparedGraphDelete,
        policy: fgdb_gql::GraphDeletePolicy,
    ) -> Result<
        (fgdb_gql::GraphDeleteStats, Vec<VId>, EmbeddedTxnCompletion),
        fgdb_gql::GqlQueryError<fgdb_gql::GraphDeleteError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphDeleteError};
        let infrastructure = |error| GqlQueryError::Source(GraphDeleteError::Source(error));
        let mut transaction = self.begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, targets) = match transaction.execute_graph_delete_returning_governed(
            self, query_cx, deletion, policy,
        ) {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction.finish(self, commit_cx).await.map_err(infrastructure)?;
        Ok((stats, targets, completion))
    }
}
