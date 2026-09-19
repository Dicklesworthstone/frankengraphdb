//! Native reads over an already-authenticated immutable embedded generation.
//!
//! Only the view supplies data and the default sequence. The live writer is
//! not borrowed or consulted, even if it advances, compacts, is fenced or drops.
//! This is not a durable snapshot lease or a new authorization boundary.

use super::{PreparedNativeRead, QueryError, QueryResult, aggregates, values};
use crate::EmbeddedReadView;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbolResolver};
use fgdb_types::{CommitSeq, QueryCx};

impl EmbeddedReadView {
    /// Execute a native read at this view's immutable frontier. The same native
    /// classification, parameter binding, GLA engines, lossless result columns
    /// and governed policy used by the database remain authoritative.
    ///
    /// Obtain the view with `Database::read_session` or `pinned_read_view` and
    /// reuse it across statements while the writer publishes later generations.
    /// Temporal statements select history no later than this view's frontier;
    /// a future selector is refused, never clamped or served from the writer.
    /// Write statements refuse through native read preparation. This method
    /// cannot stage, commit, finish a transaction or acquire another generation.
    pub fn query(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        policy: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        PreparedNativeRead::prepare(text, params, resolver)?
            .execute_in_view(self, cx, params, policy)
    }
}

impl PreparedNativeRead {
    /// Bind a reusable native template against one retained embedded view.
    /// The template pins no data: the supplied view owns the exact generation.
    /// Rebinding changes argument values, not classification or symbol mapping.
    /// Every operand of compound/pipeline reads uses the same view and policy;
    /// budget, cancellation and source errors keep their native typed variants.
    pub fn execute_in_view(
        &self,
        view: &EmbeddedReadView,
        cx: &QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        self.execute_in_view_at_seq(view, cx, params, policy, view.frontier())
    }

    // Shared by native snapshot entrypoints. Only non-temporal plans use the
    // supplied default; temporal plans bind and validate their own exact cut.
    pub(super) fn execute_in_view_at_seq(
        &self,
        view: &EmbeddedReadView,
        cx: &QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        as_of: CommitSeq,
    ) -> Result<QueryResult, QueryError> {
        match self {
            Self::Pattern(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PatternText)?;
                let result = view
                    .execute_graph_pattern_governed_at(cx, &query, as_of, policy)
                    .map_err(QueryError::Pattern)?;
                Ok(values(query.columns().to_vec(), result.value))
            }
            Self::Aggregate(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PatternText)?;
                let result = view
                    .execute_graph_aggregate_governed_at(cx, &query, as_of, policy)
                    .map_err(QueryError::Aggregate)?;
                Ok(aggregates(
                    prepared.columns().to_vec(),
                    prepared.output_slots(),
                    result.value,
                ))
            }
            Self::PipelineAggregate(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PipelineText)?;
                let result = view
                    .execute_graph_aggregate_governed_at(cx, &query, as_of, policy)
                    .map_err(QueryError::Aggregate)?;
                Ok(aggregates(
                    prepared.columns().to_vec(),
                    prepared.output_slots(),
                    result.value,
                ))
            }
            Self::Set(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::SetText)?;
                let result = view
                    .execute_graph_set_governed_at(cx, &query, as_of, policy)
                    .map_err(QueryError::Set)?;
                Ok(values(prepared.columns().to_vec(), result.value))
            }
            Self::TemporalPattern(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::TemporalText)?;
                let result = view
                    .execute_graph_pattern_governed_at(cx, query.pattern(), query.as_of(), policy)
                    .map_err(QueryError::Pattern)?;
                Ok(values(query.pattern().columns().to_vec(), result.value))
            }
            Self::TemporalAggregate(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::TemporalText)?;
                let result = view
                    .execute_temporal_graph_aggregate_text_governed(cx, &query, policy)
                    .map_err(QueryError::Aggregate)?;
                Ok(aggregates(
                    prepared.columns().to_vec(),
                    prepared.output_slots(),
                    result.value,
                ))
            }
            Self::TemporalSet(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::TemporalSetText)?;
                let result = view
                    .execute_graph_set_governed_at(cx, query.query(), query.as_of(), policy)
                    .map_err(QueryError::Set)?;
                Ok(values(prepared.columns().to_vec(), result.value))
            }
        }
    }
}
