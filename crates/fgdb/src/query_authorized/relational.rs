//! Complete relational queries share one authenticated source and live permit.
//! Intermediate rows never spend the signed delivery allowance; every graph
//! leaf does spend the same signed node/work allowance. No source can grant
//! itself a fresh permit by being nested below a join, set or aggregate.

#[path = "relational/graph.rs"]
mod graph;
#[path = "relational/native.rs"]
mod native;

use super::*;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GraphAggregateError, GraphAggregateRow, GraphSetExecutionError, PreparedGraphSet,
    PreparedGraphSetAggregate,
};

fn set_error(error: GqlQueryError<GraphSetExecutionError<ReadError>, QueryError>) -> QueryError {
    match error {
        GqlQueryError::Interrupted(error) => error,
        GqlQueryError::Source(error) => {
            QueryError::Set(GqlQueryError::Source(error.map_source(GqlError::Read)))
        }
        GqlQueryError::Rows(error) => QueryError::Set(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => QueryError::Set(GqlQueryError::Evaluator(error)),
        GqlQueryError::IdentifiedEdgesRequired => {
            QueryError::Set(GqlQueryError::IdentifiedEdgesRequired)
        }
    }
}

fn aggregate_error(error: GqlQueryError<GraphAggregateError<ReadError>, QueryError>) -> QueryError {
    match error {
        GqlQueryError::Interrupted(error) => error,
        GqlQueryError::Source(error) => {
            QueryError::Aggregate(GqlQueryError::Source(error.map_source(GqlError::Read)))
        }
        GqlQueryError::Rows(error) => QueryError::Aggregate(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => QueryError::Aggregate(GqlQueryError::Evaluator(error)),
        GqlQueryError::IdentifiedEdgesRequired => {
            QueryError::Aggregate(GqlQueryError::IdentifiedEdgesRequired)
        }
    }
}

fn set_at<Clock: FnMut() -> u64>(
    snapshot: &Snapshot,
    at: CommitSeq,
    query: &PreparedGraphSet,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<Vec<GraphValueRow>, QueryError> {
    query
        .execute_governed(
            policy,
            |pattern, remaining| pattern_at(snapshot, at, pattern, scope, execution, remaining),
            || execution.borrow_mut().checkpoint(),
        )
        .map(|result| result.value)
        .map_err(set_error)
}

fn aggregate_at<Clock: FnMut() -> u64>(
    snapshot: &Snapshot,
    at: CommitSeq,
    query: &PreparedGraphSetAggregate,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<Vec<GraphAggregateRow>, QueryError> {
    query
        .execute_governed(
            policy,
            |pattern, remaining| pattern_at(snapshot, at, pattern, scope, execution, remaining),
            || execution.borrow_mut().checkpoint(),
        )
        .map(|result| result.value)
        .map_err(aggregate_error)
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute an entire relational definition on one capability-visible graph.
    /// Every graph input uses the same immutable generation, branch, verified
    /// predicate program, trusted clock and live permit. Joins, all six set
    /// operations, computed rows, UNWIND, filters, DISTINCT, ordering and pages
    /// retain the existing executor's exact semantics and cumulative policy.
    ///
    /// The signed node bound counts admitted source visits, including repeated
    /// visits by distinct leaves, not distinct graph identities. Work is never
    /// reset per operand. Only the final selected result consumes signed rows.
    /// A late input/relational error releases no prefix, even with LIMIT 0.
    /// Source-free relations require authorization but invent no graph source.
    ///
    /// Authority/clock/branch routing must be trusted host inputs, as documented
    /// by execute_graph_pattern_authorized. Ordinary Database APIs remain
    /// privileged; resident history work is not a physical noninterference
    /// contract. No implicit token refresh, persistent session or spill is added.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_graph_set_authorized(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphSet,
        policy: GqlQueryPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Vec<GraphValueRow>, QueryError> {
        authorized(
            self,
            cx,
            authority,
            token,
            branch,
            None,
            clock,
            |snapshot, at, scope, execution| set_at(snapshot, at, query, scope, execution, policy),
        )
    }

    /// The same complete relational query at one exact retained historical cut.
    /// All leaves use that cut and current authorization; none reacquires the
    /// live frontier or an independent allowance. Future history is refused.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_graph_set_authorized_at(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphSet,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Vec<GraphValueRow>, QueryError> {
        authorized(
            self,
            cx,
            authority,
            token,
            branch,
            Some(as_of),
            clock,
            |snapshot, at, scope, execution| set_at(snapshot, at, query, scope, execution, policy),
        )
    }

    /// Group an entire capability-scoped relation with the existing exact
    /// aggregate implementation. Masking occurs before child predicates,
    /// matching, counting, grouping, HAVING and output expressions, never by
    /// filtering already-computed groups. Hidden topology cannot contribute
    /// to an aggregate and forbidden properties reach it as absent/NULL.
    ///
    /// Child pages, local DISTINCT and joins finish before aggregation. The
    /// signed delivery bound applies to final group rows, not their input
    /// occurrences; native limits and one live signed node/work permit span
    /// the complete operation. Empty global groups still require authentication
    /// and final delivery admission. Counts, wide sums and fractions keep their
    /// original types; value-dependent arithmetic errors are not hidden.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_graph_set_aggregate_authorized(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphSetAggregate,
        policy: GqlQueryPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Vec<GraphAggregateRow>, QueryError> {
        authorized(
            self,
            cx,
            authority,
            token,
            branch,
            None,
            clock,
            |snapshot, at, scope, execution| {
                aggregate_at(snapshot, at, query, scope, execution, policy)
            },
        )
    }

    /// Group the capability-visible graph at the supplied retained cut. Current
    /// capability validity and historical statement visibility are independent:
    /// an old cut cannot revive an expired or retired credential.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_graph_set_aggregate_authorized_at(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphSetAggregate,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Vec<GraphAggregateRow>, QueryError> {
        authorized(
            self,
            cx,
            authority,
            token,
            branch,
            Some(as_of),
            clock,
            |snapshot, at, scope, execution| {
                aggregate_at(snapshot, at, query, scope, execution, policy)
            },
        )
    }
}
