//! Two exact cuts, one bound definition and one live session permit.
//!
//! This adapter supplies only the existing masked sources to the ordinary
//! result-difference kernel. No privileged Database/query_diff path is reachable.

use super::*;
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::result_diff::{
    DiffEndpoint, GraphDiffError, GraphDiffInput, GraphResultChanges, GraphResultDiff,
};
use fgdb_gql::{
    GraphAggregateTextSlot, PreparedGraphAggregate, PreparedGraphSet, PreparedGraphSetAggregate,
};

type Error = GqlQueryError<GraphDiffError<QueryError>, QueryError>;

fn definition(error: QueryError) -> Error {
    GqlQueryError::Source(GraphDiffError::Definition(error))
}
fn control(error: QueryError) -> Error {
    GqlQueryError::Interrupted(error)
}
fn terminal_diff<T>(result: &Result<T, Error>) -> bool {
    match result {
        Err(GqlQueryError::Interrupted(error)) => terminal_error(error),
        Err(GqlQueryError::Source(
            GraphDiffError::Definition(error) | GraphDiffError::Endpoint { source: error, .. },
        )) => terminal_error(error),
        _ => false,
    }
}

fn admit(view: &EmbeddedReadView, before: CommitSeq, after: CommitSeq) -> Result<(), Error> {
    for endpoint in [DiffEndpoint::Before, DiffEndpoint::After] {
        view.snapshot
            .check_frontier(endpoint.sequence(before, after))
            .map_err(|source| {
                GqlQueryError::Source(GraphDiffError::Endpoint {
                    endpoint,
                    source: QueryError::Read(source),
                })
            })?;
    }
    Ok(())
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Compare one query at two exact revisions of this session's pinned history.
    /// Current capability masking applies BEFORE either query observes vertices,
    /// edges, properties, groups or algorithm projections. Both endpoint queries
    /// and delta consolidation share ONE live signed permit and native allowance.
    /// Only final changed tuples spend the signed result-row limit; equal cuts
    /// and zero changed rows do not bypass source admission or live validity.
    ///
    /// The result is after-minus-before, with complete native rows and exact
    /// signed multiplicities. Reverse cuts are legal. Each query's DISTINCT,
    /// ordering, pagination, OPTIONAL, collection order and relational input are
    /// evaluated before differencing. Pure order changes are not bag changes.
    /// Returned data omits source/evaluator counters, including from Debug/Eq.
    /// No raw endpoint rows, partial delta or diagnostic statistics are released.
    ///
    /// Pattern, aggregate, WITH aggregate and set/procedure definitions use their
    /// existing authorized engines. Explicit historical selectors refuse rather
    /// than overriding a requested endpoint. AT BRANCH must select the host's
    /// fixed branch. Identity-reading functions retain the session's refusal.
    /// Query text and parameters cannot change the catalog, policy or authority.
    ///
    /// This is bounded, materialized same-history result comparison, not raw
    /// graph DIFF, cross-branch authorization, event replay, spill or a durable
    /// cursor. Existing resource-refusal/timing side channels remain; no physical
    /// noninterference claim is made. Expiry, retirement, backwards time and
    /// callback unwind fence this session just as for ordinary authorized reads.
    pub fn query_diff(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
    ) -> Result<GraphResultChanges, Error> {
        self.run_scoped(
            cx,
            |view, branch, scope, resolver, policy, execution| {
                let selector = PreparedGraphBranchText::prepare(text)
                    .map_err(|error| definition(selector_error(error)))?;
                let selected = selector
                    .bind_parameters(params)
                    .map_err(|error| definition(selector_error(error)))?;
                check_branch(&selected, branch).map_err(control)?;
                refuse_element_identity(selected.statement()).map_err(control)?;
                admit(view, before, after)?;
                execution.borrow_mut().checkpoint().map_err(control)?;
                let prepared = PreparedNativeRead::prepare(
                    selected.statement(),
                    selected.parameters(),
                    BorrowedResolver(resolver),
                )
                .map_err(definition)?;
                BoundDiff::bind(&prepared, selected.parameters())?
                    .execute(view, before, after, scope, policy, execution)
            },
            control,
            terminal_diff,
        )
    }

    /// Compare an exact-session prepared read without parsing or catalog calls.
    /// Bind its arguments once for both cuts; a foreign template refuses before
    /// binding or history inspection, even if its text and namespace are equal.
    /// The shared session owner performs the final validity and signed delivery
    /// checks before returning the complete result, including after a failed bind.
    pub fn execute_diff(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
    ) -> Result<GraphResultChanges, Error> {
        let owner = Arc::clone(&self.owner);
        self.run_scoped(
            cx,
            |view, branch, scope, _, policy, execution| {
                if !Arc::ptr_eq(&owner, &prepared.owner) {
                    return Err(control(QueryError::Authorization(
                        AuthorizationError::WrongAuthority,
                    )));
                }
                let selected = prepared
                    .selector
                    .bind_parameters(params)
                    .map_err(|error| definition(selector_error(error)))?;
                check_branch(&selected, branch).map_err(control)?;
                refuse_element_identity(prepared.native.statement()).map_err(control)?;
                admit(view, before, after)?;
                BoundDiff::bind(&prepared.native, selected.parameters())?
                    .execute(view, before, after, scope, policy, execution)
            },
            control,
            terminal_diff,
        )
    }
}

// Bound once, before either endpoint. This is a source adapter, not a parser,
// evaluator, fallback planner or authority carrier. Aggregate slots are borrowed
// from the same immutable template; complete relational children stay intact.
struct BoundDiff<'a> {
    columns: Vec<String>,
    input: Input<'a>,
}
enum Input<'a> {
    Pattern(Box<PreparedGraphPattern<GraphValueRow>>),
    Aggregate(Box<PreparedGraphAggregate>, &'a [GraphAggregateTextSlot]),
    Relational(Box<PreparedGraphSetAggregate>, &'a [GraphAggregateTextSlot]),
    Set(Box<PreparedGraphSet>),
}
impl<'a> BoundDiff<'a> {
    fn bind(prepared: &'a PreparedNativeRead, params: &GqlParameters) -> Result<Self, Error> {
        Ok(match prepared {
            PreparedNativeRead::Pattern(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(|error| definition(QueryError::PatternText(error)))?;
                Self {
                    columns: query.columns().to_vec(),
                    input: Input::Pattern(Box::new(query)),
                }
            }
            PreparedNativeRead::Aggregate(prepared) => Self {
                columns: prepared.columns().to_vec(),
                input: Input::Aggregate(
                    Box::new(
                        prepared
                            .bind_parameters(params)
                            .map_err(|error| definition(QueryError::PatternText(error)))?,
                    ),
                    prepared.output_slots(),
                ),
            },
            PreparedNativeRead::PipelineAggregate(prepared) => Self {
                columns: prepared.columns().to_vec(),
                input: if prepared.requires_relational_input() {
                    Input::Relational(
                        Box::new(
                            prepared
                                .bind_relation_parameters(params)
                                .map_err(|error| definition(QueryError::PipelineText(error)))?,
                        ),
                        prepared.output_slots(),
                    )
                } else {
                    Input::Aggregate(
                        Box::new(
                            prepared
                                .bind_parameters(params)
                                .map_err(|error| definition(QueryError::PipelineText(error)))?,
                        ),
                        prepared.output_slots(),
                    )
                },
            },
            PreparedNativeRead::Set(prepared) => Self {
                columns: prepared.columns().to_vec(),
                input: Input::Set(Box::new(
                    prepared
                        .bind_parameters(params)
                        .map_err(|error| definition(QueryError::SetText(error)))?,
                )),
            },
            PreparedNativeRead::TemporalPattern(_)
            | PreparedNativeRead::TemporalAggregate(_)
            | PreparedNativeRead::TemporalSet(_) => {
                return Err(GqlQueryError::Source(GraphDiffError::TemporalSelector));
            }
        })
    }

    fn execute(
        self,
        view: &EmbeddedReadView,
        before: CommitSeq,
        after: CommitSeq,
        scope: &PlannerPredicates,
        policy: GqlQueryPolicy,
        execution: &Live<'_, '_, '_>,
    ) -> Result<(GraphResultChanges, usize), Error> {
        let Self { columns, input } = self;
        let result = GraphResultDiff::execute(
            before,
            after,
            columns,
            policy,
            |endpoint, remaining| {
                let at = endpoint.sequence(before, after);
                match &input {
                    Input::Pattern(query) => {
                        pattern_at(&view.snapshot, at, query, scope, execution, remaining)
                            .map(GraphDiffInput::values)
                            .map_err(|error| {
                                error.map_source(|source| {
                                    QueryError::Pattern(GqlQueryError::Source(GqlError::Read(
                                        source,
                                    )))
                                })
                            })
                    }
                    Input::Aggregate(query, slots) => graph::graph_execution_at(
                        &view.snapshot,
                        at,
                        query,
                        scope,
                        execution,
                        remaining,
                    )
                    .map(|result| {
                        GraphDiffInput::aggregates(
                            result,
                            slots,
                            query.key_columns().len(),
                            query.aggregate_columns().len(),
                        )
                    })
                    .map_err(|error| {
                        error.map_source(|source| {
                            QueryError::Aggregate(GqlQueryError::Source(source))
                        })
                    }),
                    Input::Relational(query, slots) => aggregate_execution_at(
                        &view.snapshot,
                        at,
                        query,
                        scope,
                        execution,
                        remaining,
                    )
                    .map(|result| {
                        GraphDiffInput::aggregates(
                            result,
                            slots,
                            query.key_columns().len(),
                            query.aggregate_columns().len(),
                        )
                    })
                    .map_err(|error| {
                        error.map_source(|source| {
                            QueryError::Aggregate(GqlQueryError::Source(source))
                        })
                    }),
                    Input::Set(query) => {
                        set_execution_at(&view.snapshot, at, query, scope, execution, remaining)
                            .map(GraphDiffInput::values)
                            .map_err(|error| {
                                error.map_source(|source| {
                                    QueryError::Set(GqlQueryError::Source(source))
                                })
                            })
                    }
                }
            },
            || execution.borrow_mut().checkpoint(),
        )?;
        let count = result.changes().len();
        // The same session run_scoped owner still owes its final live check and
        // signed result-row admission. Only logical changes can leave that owner.
        Ok((result.into_changes(), count))
    }
}

// This file is itself loaded through `#[path]`, so an undecorated child would
// resolve beside it (session/tests.rs), not under diff/.
#[cfg(test)]
#[path = "diff/tests.rs"]
mod tests;
