// Relational composition pins one real source coordinate for every GLA leaf.
// PreparedGraphSet owns projection/filter/set/page semantics and the one meter;
// these adapters only supply the existing durable or canonical-overlay reader.

impl<V: Vfs + Clone> Database<V> {
    /// Execute a prepared relational query at the current durable frontier.
    /// Includes native WITH pipelines, computed RETURN and UNION/INTERSECT/EXCEPT.
    /// Preparation/binding is separate: no query text or catalog callback enters
    /// execution. The immutable database borrow and one exact sequence prevent
    /// different operands from observing different generations.
    pub fn execute_graph_set_governed(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphSetExecutionError<GqlError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphSetExecutionError};
        let as_of = self.frontier().map_err(|error|
            GqlQueryError::Source(GraphSetExecutionError::Source(GqlError::Read(error))))?;
        self.execute_graph_set_governed_at(cx, query, as_of, policy)
    }

    /// All leaves read this same retained sequence, even beneath empty sets or
    /// LIMIT 0. Source visits, traversal and relational work share one allowance.
    /// Only the final page consumes the external result-row allowance. No
    /// partial rows, success certificate or durable marker is issued on error.
    pub fn execute_graph_set_governed_at(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        as_of: CommitSeq,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphSetExecutionError<GqlError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphSetExecutionError};
        // Health/history refusal precedes outer evaluator admission, including
        // a zero allowance. Each leaf retains the ordinary read preflight too.
        self.ensure_readable().and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(|error| GqlQueryError::Source(GraphSetExecutionError::Source(GqlError::Read(error))))?;
        cx.with_restriction(|| query.execute_governed(policy,
            |pattern, remaining| self.execute_graph_pattern_governed_at(cx, pattern, as_of, remaining),
            || cx.checkpoint()))
    }
}

impl crate::EmbeddedReadView {
    /// Execute against this view's immutable generation, not a live writer.
    pub fn execute_graph_set_governed(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphSetExecutionError<GqlError>, Box<asupersync::error::Error>>,
    > {
        self.execute_graph_set_governed_at(cx, query, self.frontier(), policy)
    }

    /// Historical relational execution is confined to this view's retained
    /// history and frontier. A newer writer sequence cannot enter a later arm.
    pub fn execute_graph_set_governed_at(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        as_of: CommitSeq,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphSetExecutionError<GqlError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphSetExecutionError};
        self.snapshot.check_frontier(as_of)
            .map_err(|error| GqlQueryError::Source(GraphSetExecutionError::Source(GqlError::Read(error))))?;
        cx.with_restriction(|| query.execute_governed(policy,
            |pattern, remaining| self.execute_graph_pattern_governed_at(cx, pattern, as_of, remaining),
            || cx.checkpoint()))
    }
}

impl WriteTxn {
    /// Execute all relational leaves over this transaction's original basis and
    /// canonical staged effects. The immutable borrows exclude intervening
    /// mutation. Existing pattern reads retain observations and phantom-scan
    /// witnesses even if a later filter, page, arithmetic error or cancellation
    /// discards their rows. No successful prefix or partial result escapes.
    ///
    /// This neither stages effects nor completes the transaction. Explicit
    /// finish/commit retains its ordinary conflict and completion semantics.
    /// Limits price existing source/evaluator work, not commit I/O or a spill
    /// engine. Set source counts are visits summed across leaves, not unique IDs.
    pub fn execute_graph_set_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphSetExecutionError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphSetExecutionError};
        // Reuse the actual owner/lifecycle/health/history preflight rather than
        // allowing a zero set budget to hide a wrong or finished transaction.
        let _ = self.query_snapshot(database)
            .map_err(|error| GqlQueryError::Source(GraphSetExecutionError::Source(error)))?;
        cx.with_restriction(|| query.execute_governed(policy,
            |pattern, remaining| self.execute_graph_pattern_governed(database, cx, pattern, remaining),
            || cx.checkpoint()))
    }
}
