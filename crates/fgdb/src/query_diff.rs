//! Native result differences over one already-authenticated embedded history.
//!
//! Bind once, evaluate the complete query at both exact cuts through existing
//! engines, then consolidate signed native rows. No first-leaf substitution,
//! repeated parsing, event-log replay, caller-supplied coverage assertion, or
//! separately mutable graph image participates in this operation.

use super::{Cancel, PreparedNativeRead, QueryError};
use crate::{Database, EmbeddedReadView};
use asupersync::fs::Vfs;
use fgdb_gql::result_diff::{DiffEndpoint, GraphDiffError, GraphDiffInput, GraphResultDiff};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbolResolver};
use fgdb_types::{CommitSeq, QueryCx};

type Error = GqlQueryError<GraphDiffError<QueryError>, Cancel>;

fn definition(error: QueryError) -> Error {
    GqlQueryError::Source(GraphDiffError::Definition(error))
}
fn admit(
    view: &EmbeddedReadView, cx: &QueryCx, before: CommitSeq, after: CommitSeq,
) -> Result<(), Error> {
    // Both endpoint fences precede resolver/binding callbacks or source work,
    // including equal cuts and LIMIT 0. Existing engines also enforce every
    // history/source precondition; none is bypassed to produce an empty bag.
    for endpoint in [DiffEndpoint::Before, DiffEndpoint::After] {
        view.snapshot.check_frontier(endpoint.sequence(before, after)).map_err(|error| {
            GqlQueryError::Source(GraphDiffError::Endpoint {
                endpoint, source: QueryError::Read(error),
            })
        })?;
    }
    cx.with_restriction(|| cx.checkpoint()).map_err(GqlQueryError::Interrupted)
}

impl<V: Vfs + Clone> Database<V> {
    /// Exact NET result changes between two revisions of this opened history.
    /// Prepare and bind one native query, evaluate its complete semantics at
    /// `before` and `after`, and return after-minus-before as compressed signed
    /// rows. A row present twice before and once after has weight -1, not zero.
    /// A changed property/aggregate retracts the old complete row and inserts
    /// the new row. Pure order changes are not bag changes; query DISTINCT,
    /// ORDER BY, OFFSET/LIMIT and all child scopes still run at each endpoint.
    ///
    /// Both evaluations use one immutable read generation, so a resolver
    /// cannot cause mixed cuts. Explicit temporal statements refuse instead
    /// of overriding either endpoint. Reverse endpoints are legal. Equal cuts
    /// still validate and execute the query; they cannot hide a query error.
    /// Counts, wide sums, exact averages, NULLs and graph identities retain
    /// their original domains, including textual RETURN slot order.
    ///
    /// One policy covers the SUM of source records, work and scratch across
    /// both queries and exact consolidation. ResultRows bounds consolidated
    /// changed tuples, not endpoint rows or absolute signed occurrences. A
    /// refused endpoint or exhausted budget returns no partial difference.
    /// Use PreparedNativeRead::diff to reuse frozen parsing/name resolution.
    ///
    /// This is the same-history, complete-endpoint result-difference subset:
    /// not reserved DIFF syntax, cross-branch comparison, intermediate events,
    /// a CDC cursor, partial-coverage proof, durable lease or spill execution.
    /// It needs retained endpoint state, not an intervening retained delta log.
    #[allow(clippy::too_many_arguments)]
    pub fn query_diff(
        &self, cx: &QueryCx, text: &str, params: &GqlParameters,
        resolver: impl GraphSymbolResolver, before: CommitSeq, after: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<GraphResultDiff, Error> {
        // Fenced live handles cannot mint a new admitted generation.
        let view = self.read_session().map_err(|error| definition(QueryError::Read(error)))?;
        view.query_diff(cx, text, params, resolver, before, after, policy)
    }
}

impl EmbeddedReadView {
    /// Compare two exact revisions retained by THIS immutable generation.
    /// A later writer or reopened handle is never consulted. Neither endpoint
    /// may exceed this view's frontier, even if the live writer has advanced.
    /// The same cumulative budget and all-or-nothing difference apply as for
    /// Database::query_diff. Returned values do not borrow the view or query.
    #[allow(clippy::too_many_arguments)]
    pub fn query_diff(
        &self, cx: &QueryCx, text: &str, params: &GqlParameters,
        resolver: impl GraphSymbolResolver, before: CommitSeq, after: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<GraphResultDiff, Error> {
        admit(self, cx, before, after)?;
        cx.with_restriction(|| {
            let prepared = PreparedNativeRead::prepare(text, params, resolver).map_err(definition)?;
            prepared.diff_admitted(self, cx, params, before, after, policy)
        })
    }
}

impl PreparedNativeRead {
    /// Compare one frozen template at two committed revisions. Parameter
    /// binding happens once per operation, not once per endpoint. The entire
    /// bound pattern, aggregate/WITH input or set tree executes at each cut.
    /// No temporal-selector stripping or physical fallback is performed.
    #[allow(clippy::too_many_arguments)]
    pub fn diff<V: Vfs + Clone>(
        &self, database: &Database<V>, cx: &QueryCx, params: &GqlParameters,
        before: CommitSeq, after: CommitSeq, policy: GqlQueryPolicy,
    ) -> Result<GraphResultDiff, Error> {
        let view = database.read_session().map_err(|error| definition(QueryError::Read(error)))?;
        self.diff_in_view(&view, cx, params, before, after, policy)
    }

    /// Reuse a pinned history without acquiring the writer's latest generation.
    /// Source errors identify Before/After; budget and interruption errors
    /// remain the ordinary typed GQL errors rather than becoming empty input.
    #[allow(clippy::too_many_arguments)]
    pub fn diff_in_view(
        &self, view: &EmbeddedReadView, cx: &QueryCx, params: &GqlParameters,
        before: CommitSeq, after: CommitSeq, policy: GqlQueryPolicy,
    ) -> Result<GraphResultDiff, Error> {
        admit(view, cx, before, after)?;
        cx.with_restriction(|| self.diff_admitted(view, cx, params, before, after, policy))
    }

    fn diff_admitted(
        &self, view: &EmbeddedReadView, cx: &QueryCx, params: &GqlParameters,
        before: CommitSeq, after: CommitSeq, policy: GqlQueryPolicy,
    ) -> Result<GraphResultDiff, Error> {
        // Keep each native source class and bind error. A complete aggregate's
        // private input is NOT replaced by its primary pattern; compound and
        // computed stages are owned by the existing aggregate/set executor.
        match self {
            Self::Pattern(prepared) => {
                let query = prepared.bind_parameters(params)
                    .map_err(|error| definition(QueryError::PatternText(error)))?;
                GraphResultDiff::execute(before, after, query.columns().to_vec(), policy,
                    |endpoint, remaining| view.execute_graph_pattern_governed_at(
                        cx, &query, endpoint.sequence(before, after), remaining,
                    ).map(GraphDiffInput::values).map_err(|error| error.map_source(|source|
                        QueryError::Pattern(GqlQueryError::Source(source)))),
                    || cx.checkpoint())
            }
            Self::Aggregate(prepared) => {
                let query = prepared.bind_parameters(params)
                    .map_err(|error| definition(QueryError::PatternText(error)))?;
                GraphResultDiff::execute(before, after, prepared.columns().to_vec(), policy,
                    |endpoint, remaining| view.execute_graph_aggregate_governed_at(
                        cx, &query, endpoint.sequence(before, after), remaining,
                    ).map(|result| GraphDiffInput::aggregates(result, prepared.output_slots(),
                        query.key_columns().len(), query.aggregate_columns().len()))
                        .map_err(|error| error.map_source(|source|
                            QueryError::Aggregate(GqlQueryError::Source(source)))),
                    || cx.checkpoint())
            }
            Self::PipelineAggregate(prepared) => {
                let query = prepared.bind_parameters(params)
                    .map_err(|error| definition(QueryError::PipelineText(error)))?;
                GraphResultDiff::execute(before, after, prepared.columns().to_vec(), policy,
                    |endpoint, remaining| view.execute_graph_aggregate_governed_at(
                        cx, &query, endpoint.sequence(before, after), remaining,
                    ).map(|result| GraphDiffInput::aggregates(result, prepared.output_slots(),
                        query.key_columns().len(), query.aggregate_columns().len()))
                        .map_err(|error| error.map_source(|source|
                            QueryError::Aggregate(GqlQueryError::Source(source)))),
                    || cx.checkpoint())
            }
            Self::Set(prepared) => {
                let query = prepared.bind_parameters(params)
                    .map_err(|error| definition(QueryError::SetText(error)))?;
                GraphResultDiff::execute(before, after, prepared.columns().to_vec(), policy,
                    |endpoint, remaining| view.execute_graph_set_governed_at(
                        cx, &query, endpoint.sequence(before, after), remaining,
                    ).map(GraphDiffInput::values).map_err(|error| error.map_source(|source|
                        QueryError::Set(GqlQueryError::Source(source)))),
                    || cx.checkpoint())
            }
            Self::TemporalPattern(_) | Self::TemporalAggregate(_) | Self::TemporalSet(_) => {
                Err(GqlQueryError::Source(GraphDiffError::TemporalSelector))
            }
        }
    }
}
