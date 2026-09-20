//! Native text entrypoints for the bounded global aggregate pull operator.
//!
//! Preparation and binding remain the native grammar's job. Physical admission
//! happens once, before the source is driven, and never retries an eager query.

use super::{Cancel, PreparedNativeRead, QueryError};
use crate::{Database, EmbeddedReadView, ReadError};
use asupersync::fs::Vfs;
use fgdb_gql::algebra::GlaOperator;
use fgdb_gql::edge_stream::aggregate::{EdgeAggregateCursor, EdgeAggregatePlan};
use fgdb_gql::edge_stream::{EdgeScanSource, EdgeScanState};
use fgdb_gql::scan_stream::{ScanError, ScanKind};
use fgdb_gql::stream::aggregate::{
    VertexAggregateCursor, VertexAggregatePlan,
};
use fgdb_gql::stream::{VertexScanSource, VertexScanState};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphAggregateError, GraphAggregateRow, GraphAggregateTextSlot, GraphSymbolResolver,
    PreparedGraphAggregate,
};
use fgdb_types::{CommitSeq, QueryCx};

type NativeError = GqlQueryError<GraphAggregateError<ScanError<ReadError>>, Cancel>;

// Erase only the host/source implementation, not result domains or errors.
// One fixed-size ownership box makes the public lifetime depend ONLY on QueryCx,
// never on a temporary resolver, parameter map, template or database handle.
trait AggregatePull: Send {
    fn next_row(&mut self) -> Option<Result<GraphAggregateRow, NativeError>>;
    fn size_hint(&self) -> (usize, Option<usize>);
    fn kind(&self) -> ScanKind;
    fn columns(&self) -> &[String];
    fn snapshot_seq(&self) -> CommitSeq;
    fn state(&self) -> VertexScanState;
    fn row_stats(&self) -> GqlExecutionStats;
    fn evaluator_stats(&self) -> GlaExecutionStats;
    fn close(&mut self);
}

impl<S, F> AggregatePull for VertexAggregateCursor<S, F>
where
    S: VertexScanSource<Error = ReadError> + Send,
    F: FnMut() -> Result<(), Cancel> + Send,
{
    fn next_row(&mut self) -> Option<Result<GraphAggregateRow, NativeError>> {
        self.next().map(|row| {
            row.map_err(|error| error.map_source(|error| error.map_source(ScanError::Vertex)))
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        Iterator::size_hint(self)
    }
    fn kind(&self) -> ScanKind {
        ScanKind::Vertex
    }
    fn columns(&self) -> &[String] {
        VertexAggregateCursor::columns(self)
    }
    fn snapshot_seq(&self) -> CommitSeq {
        VertexAggregateCursor::snapshot_seq(self)
    }
    fn state(&self) -> VertexScanState {
        VertexAggregateCursor::state(self)
    }
    fn row_stats(&self) -> GqlExecutionStats {
        VertexAggregateCursor::row_stats(self)
    }
    fn evaluator_stats(&self) -> GlaExecutionStats {
        VertexAggregateCursor::evaluator_stats(self)
    }
    fn close(&mut self) {
        VertexAggregateCursor::close(self);
    }
}

impl<S, F> AggregatePull for EdgeAggregateCursor<S, F>
where
    S: EdgeScanSource<Error = ReadError> + Send,
    F: FnMut() -> Result<(), Cancel> + Send,
{
    fn next_row(&mut self) -> Option<Result<GraphAggregateRow, NativeError>> {
        self.next().map(|row| {
            row.map_err(|error| error.map_source(|error| error.map_source(ScanError::Edge)))
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        Iterator::size_hint(self)
    }
    fn kind(&self) -> ScanKind {
        ScanKind::Edge
    }
    fn columns(&self) -> &[String] {
        EdgeAggregateCursor::columns(self)
    }
    fn snapshot_seq(&self) -> CommitSeq {
        EdgeAggregateCursor::snapshot_seq(self)
    }
    fn state(&self) -> VertexScanState {
        match EdgeAggregateCursor::state(self) {
            EdgeScanState::Open => VertexScanState::Open,
            EdgeScanState::Exhausted => VertexScanState::Exhausted,
            EdgeScanState::Closed => VertexScanState::Closed,
            EdgeScanState::Failed => VertexScanState::Failed,
        }
    }
    fn row_stats(&self) -> GqlExecutionStats {
        EdgeAggregateCursor::row_stats(self)
    }
    fn evaluator_stats(&self) -> GlaExecutionStats {
        EdgeAggregateCursor::evaluator_stats(self)
    }
    fn close(&mut self) {
        EdgeAggregateCursor::close(self);
    }
}

/// A native aggregate query whose first pull consumes the source and emits one
/// complete row, including on empty input. `columns()` addresses `row.values()`
/// in textual RETURN order; counts and wide sums keep their exact domains.
///
/// Opening reads no candidates. Close/drop does not drain the source. Failure
/// emits one typed error, releases the pin and permanently fuses the cursor.
/// No projected input table, per-row query, data copy or second meter is added.
/// The one ownership box is metadata; the pinned database is still in memory.
/// This is not a grouped cursor, session lease or a durable resumption token.
/// Vertex and fixed-edge/join profiles share this surface. Source failures keep
/// their original typed cause in ScanError; budgets and interruption stay outside
/// that sum. The selected operator never changes after opening.
pub struct NativeAggregateCursor<'q> {
    inner: Box<dyn AggregatePull + 'q>,
}
impl<'q> NativeAggregateCursor<'q> {
    fn new(cursor: impl AggregatePull + 'q) -> Self {
        Self {
            inner: Box::new(cursor),
        }
    }
    /// The physical source chosen structurally before any candidate is read.
    #[must_use]
    pub fn kind(&self) -> ScanKind {
        self.inner.kind()
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.inner.columns()
    }
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.inner.snapshot_seq()
    }
    #[must_use]
    pub fn state(&self) -> VertexScanState {
        self.inner.state()
    }
    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats {
        self.inner.row_stats()
    }
    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.inner.evaluator_stats()
    }
    pub fn close(&mut self) {
        self.inner.close();
    }
}
impl Iterator for NativeAggregateCursor<'_> {
    type Item = Result<GraphAggregateRow, NativeError>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next_row()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}
impl std::iter::FusedIterator for NativeAggregateCursor<'_> {}
impl core::fmt::Debug for NativeAggregateCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeAggregateCursor")
            .field("kind", &self.kind())
            .field("snapshot_seq", &self.snapshot_seq())
            .field("state", &self.state())
            .field("rows", &self.row_stats())
            .field("evaluator", &self.evaluator_stats())
            .field("definition_and_source", &"[REDACTED]")
            .finish()
    }
}

impl EmbeddedReadView {
    /// All statement binding and source reads use this retained generation.
    /// A historical selector cannot escape the view's frontier to the writer.
    pub fn query_aggregate_stream<'q>(
        &self,
        cx: &'q QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        policy: GqlQueryPolicy,
    ) -> Result<NativeAggregateCursor<'q>, QueryError> {
        PreparedNativeRead::prepare(text, params, resolver)?
            .stream_aggregate_in_view(self, cx, params, policy)
    }
}

impl PreparedNativeRead {
    /// Open the checked global aggregate specialization from this native
    /// definition. Rebinding neither reparses text nor re-resolves graph names.
    /// The returned cursor borrows only cx, not this template or its arguments.
    pub fn stream_aggregate<'q, V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &'q QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
    ) -> Result<NativeAggregateCursor<'q>, QueryError> {
        let view = database.read_session().map_err(|error| {
            QueryError::AggregateStream(
                fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphAggregateError::Source(
                    fgdb_gql::stream::VertexScanError::Source(error),
                )),
            )
        })?;
        self.stream_aggregate_in_view(&view, cx, params, policy)
    }

    /// Normal, temporal and pipeline aggregate grammars retain their own typed
    /// binding errors. The physical compiler, not the facade name, decides
    /// whether the complete bound shape is supported. An unsupported clause is
    /// never dropped, including under a zero output allowance or empty input.
    pub fn stream_aggregate_in_view<'q>(
        &self,
        view: &EmbeddedReadView,
        cx: &'q QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
    ) -> Result<NativeAggregateCursor<'q>, QueryError> {
        let facade = self.facade_class();
        let (plan, as_of) = match self {
            Self::Aggregate(prepared) => {
                let query = prepared.bind_parameters(params).map_err(QueryError::PatternText)?;
                (
                    compile(&query, prepared.columns(), prepared.output_slots(), facade)?,
                    view.frontier(),
                )
            }
            Self::TemporalAggregate(prepared) => {
                let query = prepared.bind_parameters(params).map_err(QueryError::TemporalText)?;
                (
                    compile(query.aggregate(), prepared.columns(), prepared.output_slots(), facade)?,
                    query.as_of(),
                )
            }
            Self::PipelineAggregate(prepared) => {
                let query = prepared.bind_parameters(params).map_err(QueryError::PipelineText)?;
                (
                    compile(&query, prepared.columns(), prepared.output_slots(), facade)?,
                    view.frontier(),
                )
            }
            _ => return Err(QueryError::StreamingUnsupported { facade }),
        };
        match plan {
            AggregatePlan::Vertex(plan) => Ok(NativeAggregateCursor::new(
                view.stream_global_aggregate_governed_at(cx, &plan, as_of, policy)
                    .map_err(QueryError::AggregateStream)?,
            )),
            AggregatePlan::Edge(plan) => Ok(NativeAggregateCursor::new(
                view.stream_global_edge_aggregate_governed_at(cx, &plan, as_of, policy)
                    .map_err(QueryError::EdgeAggregateStream)?,
            )),
        }
    }
}

// Select by the bound GLA root, never by source text, a trial execution, or a
// failed compilation. A rejected edge shape cannot silently become a vertex
// scan (nor an eagerly materialized aggregate) even on empty input.
enum AggregatePlan {
    Vertex(VertexAggregatePlan),
    Edge(EdgeAggregatePlan),
}
impl AggregatePlan {
    fn columns(&self) -> &[String] {
        match self {
            Self::Vertex(plan) => plan.columns(),
            Self::Edge(plan) => plan.columns(),
        }
    }
}

fn compile(
    query: &PreparedGraphAggregate,
    columns: &[String],
    slots: &[GraphAggregateTextSlot],
    facade: crate::NativeReadClass,
) -> Result<AggregatePlan, QueryError> {
    let plan = if matches!(
        query.input_pattern().plan().operators().first(),
        Some(GlaOperator::ScanEdges { .. })
    ) {
        AggregatePlan::Edge(
            EdgeAggregatePlan::compile(query).map_err(QueryError::EdgeAggregateStreamPlan)?,
        )
    } else {
        AggregatePlan::Vertex(
            VertexAggregatePlan::compile(query).map_err(QueryError::AggregateStreamPlan)?,
        )
    };
    // Expose row.values() directly, without coercing/cloning numeric or scalar
    // payloads. The checked plain global shape has no keys/hidden summaries;
    // refuse a future facade remapping rather than silently mislabeling cells.
    if !query.group_key_columns().is_empty()
        || columns != plan.columns()
        || slots.len() != columns.len()
        || !slots.iter().enumerate().all(|(index, slot)| {
            matches!(slot, GraphAggregateTextSlot::Aggregate(at) if *at == index)
        })
    {
        return Err(QueryError::StreamingUnsupported { facade });
    }
    Ok(plan)
}
