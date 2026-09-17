// Compound summaries reuse the canonical pattern readers. This module owns
// only the one-coordinate lifetime, not matching, set or aggregate semantics.
mod set_aggregate_queries {
    use super::*;
    use fgdb_gql::{
        GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphAggregateError,
        GraphAggregateRow, PreparedGraphSetAggregate,
    };
    use fgdb_types::QueryCx;

    type ResultRows<E> = Result<
        GqlQueryExecution<GraphAggregateRow>,
        GqlQueryError<GraphAggregateError<E>, Box<asupersync::error::Error>>,
    >;

    impl<V: Vfs + Clone> Database<V> {
        /// Summarize a completed compound relation at one durable frontier.
        /// Native set/WITH inputs may be bound separately and reused. Counts,
        /// i128 sums and rational averages retain their exact result domains.
        pub fn execute_graph_set_aggregate_governed(
            &self, cx: &QueryCx, query: &PreparedGraphSetAggregate, policy: GqlQueryPolicy,
        ) -> ResultRows<GqlError> {
            let as_of = self.frontier().map_err(|error|
                GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error))))?;
            self.execute_graph_set_aggregate_governed_at(cx, query, as_of, policy)
        }

        /// Every operand reads the SAME retained sequence under this immutable
        /// database borrow. Health/history preflight precedes outer work limits,
        /// even for empty differences, zero output pages or a zero allowance.
        /// All operand visits, relational stages and grouping share one budget.
        pub fn execute_graph_set_aggregate_governed_at(
            &self, cx: &QueryCx, query: &PreparedGraphSetAggregate,
            as_of: CommitSeq, policy: GqlQueryPolicy,
        ) -> ResultRows<GqlError> {
            self.ensure_readable().and_then(|()| self.snapshot.check_frontier(as_of))
                .map_err(|error|
                    GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error))))?;
            cx.with_restriction(|| query.execute_governed(policy,
                |pattern, remaining| self.execute_graph_pattern_governed_at(cx, pattern, as_of, remaining),
                || cx.checkpoint()))
        }
    }

    impl crate::EmbeddedReadView {
        /// All operands remain within this pinned immutable generation even
        /// while the live database advances or compacts. No live writer lookup
        /// supplies a later operand, nor can a filtered row release the pin.
        pub fn execute_graph_set_aggregate_governed(
            &self, cx: &QueryCx, query: &PreparedGraphSetAggregate, policy: GqlQueryPolicy,
        ) -> ResultRows<GqlError> {
            self.execute_graph_set_aggregate_governed_at(cx, query, self.frontier(), policy)
        }

        /// Historical execution is confined to this view's retained frontier.
        /// Only fully accepted final groups escape; input rows stay private.
        pub fn execute_graph_set_aggregate_governed_at(
            &self, cx: &QueryCx, query: &PreparedGraphSetAggregate,
            as_of: CommitSeq, policy: GqlQueryPolicy,
        ) -> ResultRows<GqlError> {
            self.snapshot.check_frontier(as_of).map_err(|error|
                GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error))))?;
            cx.with_restriction(|| query.execute_governed(policy,
                |pattern, remaining| self.execute_graph_pattern_governed_at(cx, pattern, as_of, remaining),
                || cx.checkpoint()))
        }
    }

    impl WriteTxn {
        /// Summarize the completed set over one canonical staged overlay.
        /// Immutable transaction/database borrows exclude mutation between
        /// operands. Each operand uses the existing positive/negative read and
        /// scan witnesses, including rows canceled by INTERSECT/EXCEPT or pages.
        /// Those observations survive late arithmetic, budget or source errors.
        ///
        /// This is read-only with respect to staged effects and durability:
        /// it does not finish, commit, retry or replace the caller's workspace.
        /// Ordinary finish/commit still validates the retained observations.
        pub fn execute_graph_set_aggregate_governed<V: Vfs + Clone>(
            &self, database: &Database<V>, cx: &QueryCx,
            query: &PreparedGraphSetAggregate, policy: GqlQueryPolicy,
        ) -> ResultRows<WriteTxnError> {
            let _ = self.query_snapshot(database)
                .map_err(|error| GqlQueryError::Source(GraphAggregateError::Source(error)))?;
            cx.with_restriction(|| query.execute_governed(policy,
                |pattern, remaining| self.execute_graph_pattern_governed(database, cx, pattern, remaining),
                || cx.checkpoint()))
        }
    }
}
