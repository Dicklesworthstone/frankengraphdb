//! Ordinary/computed native aggregate definitions use the same scoped source
//! as relational aggregates, without rebuilding or weakening their definitions.

use super::*;
use fgdb_gql::PreparedGraphAggregate;

pub(super) fn graph_at<Clock: FnMut() -> u64>(
    snapshot: &Snapshot,
    at: CommitSeq,
    query: &PreparedGraphAggregate,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<Vec<GraphAggregateRow>, QueryError> {
    query
        .execute_with_source_governed(
            policy,
            |pattern, remaining| pattern_at(snapshot, at, pattern, scope, execution, remaining),
            || execution.borrow_mut().checkpoint(),
        )
        .map(|result| result.value)
        .map_err(aggregate_error)
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute a complete native aggregate on the capability-visible graph.
    /// Plain, computed and relational definitions retain all source expressions,
    /// grouping, argument DISTINCT, HAVING, hidden outputs, ordering and pages.
    /// Labels, properties and every transit vertex are scoped BEFORE matching
    /// and aggregation. No raw source is substituted for a complete row pipeline.
    ///
    /// One current permit covers all graph inputs, source/aggregate work and
    /// final delivery. Only selected group rows spend signed delivery rows.
    /// Returned counts, integer sums, fractions, paths and lists are unchanged
    /// native values; masked properties are absent/NULL, not post-filtered data.
    ///
    /// Like execute_graph_pattern_authorized, this requires host-owned issuer,
    /// branch mapping and clock. It does not secure privileged Database APIs or
    /// establish physical side-channel isolation. Plain/computed source rows
    /// are materialized under native work/scratch limits; this is not streaming.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_graph_aggregate_authorized(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphAggregate,
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
                graph_at(snapshot, at, query, scope, execution, policy)
            },
        )
    }

    /// Apply current authorization to this aggregate at one exact historical
    /// cut. Authentication precedes the frontier check. Resolve historical
    /// winners before scoping, never revive an earlier permitted version, and
    /// recheck credential validity even for empty input or an empty group page.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_graph_aggregate_authorized_at(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphAggregate,
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
                graph_at(snapshot, at, query, scope, execution, policy)
            },
        )
    }
}
