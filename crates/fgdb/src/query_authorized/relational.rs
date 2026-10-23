//! Complete relational queries share one authenticated source and live permit.
//! Intermediate rows never spend the signed delivery allowance; every graph
//! leaf does spend the same signed node/work allowance. No source can grant
//! itself a fresh permit by being nested below a join, set or aggregate.

#[path = "relational/graph.rs"]
mod graph;
#[path = "relational/native.rs"]
mod native;
#[path = "relational/seeded.rs"]
mod seeded;
pub use native::{
    AuthorizedAggregateCursor, AuthorizedAsyncAggregateCursor, AuthorizedBeaconIndex,
    AuthorizedDifferenceCursor, AuthorizedDifferenceExecution, AuthorizedOrderExecution,
    AuthorizedOrderedReadSession, AuthorizedPreparedFnxCall, AuthorizedPreparedRead,
    AuthorizedReadSession, AuthorizedRowCursor, AuthorizedSendAggregateCursor,
    AuthorizedSendRowCursor, AuthorizedSortLimits, AuthorizedSpoolCursor,
};

use super::*;
use fgdb_delta_types::ElementId;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GraphAggregateError, GraphAggregateRow, GraphSetExecutionError, PreparedGraphSet,
    PreparedGraphSetAggregate,
};

fn set_error(error: GqlQueryError<GraphSetExecutionError<GqlError>, QueryError>) -> QueryError {
    match error {
        GqlQueryError::Interrupted(error) => error,
        GqlQueryError::Source(error) => QueryError::Set(GqlQueryError::Source(error)),
        GqlQueryError::Rows(error) => QueryError::Set(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => QueryError::Set(GqlQueryError::Evaluator(error)),
        GqlQueryError::IdentifiedEdgesRequired => {
            QueryError::Set(GqlQueryError::IdentifiedEdgesRequired)
        }
        GqlQueryError::Data(error) => QueryError::Set(GqlQueryError::Data(error)),
    }
}

fn aggregate_gql_error(
    error: GqlQueryError<GraphAggregateError<GqlError>, QueryError>,
) -> QueryError {
    match error {
        GqlQueryError::Interrupted(error) => error,
        GqlQueryError::Source(error) => QueryError::Aggregate(GqlQueryError::Source(error)),
        GqlQueryError::Rows(error) => QueryError::Aggregate(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => QueryError::Aggregate(GqlQueryError::Evaluator(error)),
        GqlQueryError::IdentifiedEdgesRequired => {
            QueryError::Aggregate(GqlQueryError::IdentifiedEdgesRequired)
        }
        GqlQueryError::Data(error) => QueryError::Aggregate(GqlQueryError::Data(error)),
    }
}

// Parameters can contain vertex/edge values, so an element identity alone is
// not evidence of a preceding capability-scoped pattern or CALL. Resolve its
// winner at this statement's admitted cut, mask the vertex or edge relation
// AND both endpoints, and only then clone the requested property value.
// This ordinary reader retains the existing derived-index lookup path. The
// prepared Beacon reader separately guarantees bounded cold patch searches.
fn scoped_element_property<Clock: FnMut() -> u64>(
    snapshot: &Snapshot,
    at: CommitSeq,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    failure: &RefCell<Option<QueryError>>,
    element: ElementId,
    key: PropertyKeyId,
) -> Result<Option<fgdb_types::CanonicalPropertyValue>, GqlError> {
    // This source-error carrier is private to the governed call below. Its
    // checkpoint or final boundary restores the retained QueryError, including
    // cancellation, before work settlement or any result can escape.
    let interrupted_source = || {
        GqlError::Procedure(crate::query::ProcedureError::Search(
            crate::query::HybridCallError::Combination(
                "authorized element property read interrupted",
            ),
        ))
    };
    if failure.borrow().is_some() {
        return Err(interrupted_source());
    }
    let result: Result<Option<fgdb_types::CanonicalPropertyValue>, QueryError> = (|| {
        if !scope.allows_property(key) {
            return Ok(None);
        }
        // Physical index/history probes may inspect hidden records. They
        // remain cancellable without spending visible source/work allowance.
        let mut control = |_: source::SourceEvent| execution.borrow_mut().poll();
        control(source::SourceEvent::Work)?;
        let props = match element {
            ElementId::Vertex(id) => {
                let Some(vertex) = source::find_vertex(snapshot, id, at, &mut control)? else {
                    return Ok(None);
                };
                if !scope.allows_vertex(&vertex.labels) {
                    return Ok(None);
                }
                vertex.props.as_slice()
            }
            ElementId::Edge(id) => {
                let coordinate = snapshot
                    .adjacency_index()
                    .statement_at(&snapshot.blocks, id, at);
                control(source::SourceEvent::Work)?;
                let Some((block, row)) = coordinate else {
                    return Ok(None);
                };
                let edge = &snapshot.blocks[block][row];
                if !scope.allows_relation(edge.relation) {
                    return Ok(None);
                }
                for endpoint in [edge.src, edge.dst] {
                    let vertex = source::find_vertex(snapshot, endpoint, at, &mut control)?;
                    if vertex.is_none_or(|vertex| !scope.allows_vertex(&vertex.labels)) {
                        return Ok(None);
                    }
                }
                source::edge_properties_at(&snapshot.block_props, block, row)
            }
        };
        control(source::SourceEvent::Work)?;
        Ok(props
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .map(|position| props[position].1.clone()))
    })();
    result.map_err(|error| {
        failure.borrow_mut().get_or_insert(error);
        interrupted_source()
    })
}

fn set_at<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    query: &PreparedGraphSet,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<Vec<GraphValueRow>, QueryError> {
    set_execution_at(snapshot, at, query, scope, execution, policy)
        .map(|result| result.value)
        .map_err(set_error)
}

// Keep measured usage inside the authorized host for multi-endpoint execution.
// Ordinary reads discard it at their existing delivery boundary, as before.
fn set_execution_at<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    query: &PreparedGraphSet,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<
    fgdb_gql::GqlQueryExecution<GraphValueRow>,
    GqlQueryError<GraphSetExecutionError<GqlError>, QueryError>,
> {
    // The whole statement is one phase: its row stages' signed work (ORDER BY
    // and LIMIT over every row a CALL yields) is charged in runs, as its
    // patterns' and kernels' already is.
    work_runs(execution, GqlQueryError::Interrupted, || {
        set_execution_in_runs(snapshot, at, query, scope, execution, policy)
    })
}

fn set_execution_in_runs<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    query: &PreparedGraphSet,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<
    fgdb_gql::GqlQueryExecution<GraphValueRow>,
    GqlQueryError<GraphSetExecutionError<GqlError>, QueryError>,
> {
    let element_failure = RefCell::new(None);
    let result = query.execute_governed_with_seeded_elements(
        policy,
        |pattern, remaining| {
            pattern_at(snapshot, at, pattern, scope, execution, remaining)
                .map_err(|error| error.map_source(GqlError::Read))
        },
        |pattern, seeds, remaining| {
            seeded::pattern_at(snapshot, at, pattern, seeds, scope, execution, remaining)
                .map(|result| result.map_err(|error| error.map_source(GqlError::Read)))
        },
        |call, arguments, remaining| {
            procedure_at(snapshot, at, call, arguments, scope, execution, remaining)
        },
        |element, key| {
            scoped_element_property(
                snapshot,
                at,
                scope,
                execution,
                &element_failure,
                element,
                key,
            )
        },
        || {
            if let Some(error) = element_failure.borrow_mut().take() {
                return Err(error);
            }
            execution.borrow_mut().checkpoint()
        },
    );
    let failure = element_failure.borrow_mut().take();
    match failure {
        Some(error) => Err(GqlQueryError::Interrupted(error)),
        None => result,
    }
}

/// A CALL stage under the capability (fgdb-7qznp). It binds exactly as every
/// host does, then runs Prism through the authorized analytics path over the
/// token's projection, never the privileged `Database::execute_fnx`. Masked
/// vertices, labels, relations and weights are removed before the graph is
/// built, so a hidden edge changes no degree or component (FG-INV-20:
/// security applies before expansion). The call spends this read's own live
/// permit and allowance. No Prism certificate escapes, as on every
/// authorized analytics path.
fn procedure_at<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    call: &fgdb_gql::PreparedProcedureCall,
    arguments: &[fgdb_gql::algebra::GraphValue],
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    remaining: GqlQueryPolicy,
) -> Result<fgdb_gql::GqlQueryExecution<GraphValueRow>, GqlQueryError<GqlError, QueryError>> {
    if crate::query::is_hybrid(call) {
        return super::beacon::hybrid_at(
            snapshot, at, call, arguments, scope, execution, remaining,
        );
    }
    let spec = crate::query::bind_procedure(call, arguments)?;
    let options = crate::query::procedure_options(&spec, at, remaining);
    super::analytics::preflight(&spec, options.projection.directedness)
        .map_err(crate::query::procedure_failure)?;
    let mut usage = AdmissionUsage::default();
    let rows = super::analytics::execute(
        snapshot,
        fgdb_prism::SnapshotBinding {
            root: snapshot.root.0,
            as_of: at,
        },
        &spec,
        options,
        scope,
        execution,
        super::analytics::UsageObserver {
            usage: &mut usage,
            policy: remaining,
        },
    )
    .map_err(crate::query::procedure_failure)?;
    let value = crate::query::procedure_rows(rows)?;
    usage.finish(
        remaining,
        Ok(fgdb_gql::GqlQueryExecution {
            rows: fgdb_gql::GqlExecutionStats {
                snapshot_records: usage.snapshot_records(),
                result_rows: value.len() as u64,
            },
            evaluator: fgdb_gql::GlaExecutionStats::default(),
            value,
        }),
    )
}

fn aggregate_at<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    query: &PreparedGraphSetAggregate,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<Vec<GraphAggregateRow>, QueryError> {
    aggregate_execution_at(snapshot, at, query, scope, execution, policy)
        .map(|result| result.value)
        .map_err(aggregate_gql_error)
}

fn aggregate_execution_at<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    query: &PreparedGraphSetAggregate,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<
    fgdb_gql::GqlQueryExecution<GraphAggregateRow>,
    GqlQueryError<GraphAggregateError<GqlError>, QueryError>,
> {
    let element_failure = RefCell::new(None);
    let result = query.execute_governed_with_seeded_elements(
        policy,
        |pattern, remaining| {
            pattern_at(snapshot, at, pattern, scope, execution, remaining)
                .map_err(|error| error.map_source(GqlError::Read))
        },
        |pattern, seeds, remaining| {
            seeded::pattern_at(snapshot, at, pattern, seeds, scope, execution, remaining)
                .map(|result| result.map_err(|error| error.map_source(GqlError::Read)))
        },
        |call, arguments, remaining| {
            procedure_at(snapshot, at, call, arguments, scope, execution, remaining)
        },
        |element, key| {
            scoped_element_property(
                snapshot,
                at,
                scope,
                execution,
                &element_failure,
                element,
                key,
            )
        },
        || {
            if let Some(error) = element_failure.borrow_mut().take() {
                return Err(error);
            }
            execution.borrow_mut().checkpoint()
        },
    );
    let failure = element_failure.borrow_mut().take();
    match failure {
        Some(error) => Err(GqlQueryError::Interrupted(error)),
        None => result,
    }
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
