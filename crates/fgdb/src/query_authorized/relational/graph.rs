//! Ordinary/computed native aggregate definitions use the same scoped source
//! as relational aggregates, without rebuilding or weakening their definitions.

use super::*;
use fgdb_gql::PreparedGraphAggregate;
use fgdb_gql::algebra::ValueProjection;

pub(super) fn graph_at<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    query: &PreparedGraphAggregate,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<Vec<GraphAggregateRow>, QueryError> {
    graph_execution_at(snapshot, at, query, scope, execution, policy)
        .map(|result| result.value)
        .map_err(aggregate_error)
}

pub(super) fn graph_execution_at<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    query: &PreparedGraphAggregate,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<
    fgdb_gql::GqlQueryExecution<GraphAggregateRow>,
    GqlQueryError<GraphAggregateError<ReadError>, QueryError>,
> {
    // Plain, uncaptured aggregate inputs consume binding VISIT order, not the
    // sorted projected bag of a standalone pattern. COLLECT (including its
    // first-occurrence DISTINCT form) exposes that distinction. Computed and
    // relational inputs deliberately retain their own completed-row semantics.
    // Identified-edge plans materialize unless the visitor answers them exactly
    // from topology, as the owner path routes them (fgdb-xtsw3).
    if query.input_relation().is_none()
        && query.input_projection().is_none()
        && (!query.input_pattern().plan().requires_identified_edges()
            || query.answers_identified_edges_from_topology())
        && query.input_pattern().value_columns().iter().all(|column| {
            matches!(
                column,
                ValueProjection::Vertex { .. } | ValueProjection::Property { .. }
            )
        })
    {
        return visit_at(snapshot, at, query, scope, execution, policy);
    }
    query.execute_with_source_governed(
        policy,
        |pattern, remaining| pattern_at(snapshot, at, pattern, scope, execution, remaining),
        || execution.borrow_mut().checkpoint(),
    )
}

// Reuse the SAME source admission and masking as pattern_at, then the ordinary
// native aggregate visitor. No projected input rows, second matcher or custom
// collection ordering is introduced. Source and group phases share one policy.
fn visit_at<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    query: &PreparedGraphAggregate,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<
    fgdb_gql::GqlQueryExecution<GraphAggregateRow>,
    GqlQueryError<GraphAggregateError<ReadError>, QueryError>,
> {
    // Admission and evaluation are one phase: their signed work is charged
    // in runs.
    work_runs(execution, GqlQueryError::Interrupted, || {
        let mut usage = AdmissionUsage::default();
        let tables = Tables::admit(
            snapshot,
            query.input_pattern().plan(),
            at,
            scope,
            || {
                execution
                    .borrow_mut()
                    .node()
                    .map_err(GqlQueryError::Interrupted)
            },
            &mut || {
                execution
                    .borrow_mut()
                    .poll()
                    .map_err(GqlQueryError::Interrupted)
            },
            &mut |event| {
                execution
                    .borrow_mut()
                    .checkpoint()
                    .map_err(GqlQueryError::Interrupted)?;
                usage.observe::<ReadError, QueryError>(policy, event)
            },
        )
        .map_err(|error| error.map_source(GraphAggregateError::Source))?;
        let edges = tables
            .edges
            .iter()
            .map(|&((_, from, relation, to), _)| (from, relation, to));
        let remaining = usage.remaining(policy);
        let checkpoint = || execution.borrow_mut().checkpoint();
        // A vertex-only aggregate tests and reads the vertex its scan has just
        // yielded: `scan` remembers it, so neither finds it again (count_vertices
        // 1.9-2.3x). An edge-reading one keeps the plain lookups. Its endpoint
        // tests run in the evaluator's hottest loop, and the remembering
        // closures measured 5-13% slower there.
        let result = if query.input_pattern().plan().reads_edges() {
            query.execute_governed(
                tables.records,
                tables.vertices.iter().map(|&(vid, _)| vid),
                edges,
                |vid, required| Ok(tables.matches(vid, required, scope)),
                |vid, key| Ok(tables.property(vid, key, scope)),
                remaining,
                checkpoint,
            )
        } else {
            let last = std::cell::Cell::new(None);
            query.execute_governed(
                tables.records,
                tables.scan(&last),
                edges,
                |vid, required| Ok(tables.matches_after(&last, vid, required, scope)),
                |vid, key| Ok(tables.property_after(&last, vid, key, scope)),
                remaining,
                checkpoint,
            )
        };
        usage.finish(policy, result)
    })
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
    /// establish physical side-channel isolation. Plain uncaptured definitions
    /// feed the native binding visitor without a projected input bag. Computed,
    /// relational and captured inputs retain their existing materialized path.
    /// Admitted source tables and aggregate state still reside in memory.
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

#[cfg(test)]
#[path = "graph/visitor_tests.rs"]
mod visitor_tests;
