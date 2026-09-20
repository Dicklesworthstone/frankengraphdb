//! Native text entrypoints for the bounded global aggregate pull operator.
//!
//! Preparation and binding remain the native grammar's job. Physical admission
//! happens once, before the source is driven, and never retries an eager query.

use super::{Cancel, PreparedNativeRead, QueryError};
use crate::{Database, EmbeddedReadView, ReadError};
use asupersync::fs::Vfs;
use fgdb_gql::stream::aggregate::{
    VertexAggregateCursor, VertexAggregateError, VertexAggregatePlan,
};
use fgdb_gql::stream::{VertexScanSource, VertexScanState};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryPolicy, GraphAggregateRow,
    GraphAggregateTextSlot, GraphSymbolResolver, PreparedGraphAggregate,
};
use fgdb_types::{CommitSeq, QueryCx};

// Erase only the host/source implementation, not result domains or errors.
// One fixed-size ownership box makes the public lifetime depend ONLY on QueryCx,
// never on a temporary resolver, parameter map, template or database handle.
trait AggregatePull:
    Iterator<Item = Result<GraphAggregateRow, VertexAggregateError<ReadError, Cancel>>> + Send
{
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

/// A native aggregate query whose first pull consumes the source and emits one
/// complete row, including on empty input. `columns()` addresses `row.values()`
/// in textual RETURN order; counts and wide sums keep their exact domains.
///
/// Opening reads no candidates. Close/drop does not drain the source. Failure
/// emits one typed error, releases the pin and permanently fuses the cursor.
/// No projected input table, per-row query, data copy or second meter is added.
/// The one ownership box is metadata; the pinned database is still in memory.
/// This is not a grouped cursor, session lease or a durable resumption token.
pub struct NativeAggregateCursor<'q> {
    inner: Box<dyn AggregatePull + 'q>,
}
impl<'q> NativeAggregateCursor<'q> {
    fn new<S, F>(cursor: VertexAggregateCursor<S, F>) -> Self
    where
        S: VertexScanSource<Error = ReadError> + Send + 'q,
        F: FnMut() -> Result<(), Cancel> + Send + 'q,
    {
        Self {
            inner: Box::new(cursor),
        }
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
    type Item = Result<GraphAggregateRow, VertexAggregateError<ReadError, Cancel>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}
impl std::iter::FusedIterator for NativeAggregateCursor<'_> {}
impl core::fmt::Debug for NativeAggregateCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeAggregateCursor")
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
        let cursor = view
            .stream_global_aggregate_governed_at(cx, &plan, as_of, policy)
            .map_err(QueryError::AggregateStream)?;
        Ok(NativeAggregateCursor::new(cursor))
    }
}

fn compile(
    query: &PreparedGraphAggregate,
    columns: &[String],
    slots: &[GraphAggregateTextSlot],
    facade: crate::NativeReadClass,
) -> Result<VertexAggregatePlan, QueryError> {
    let plan = VertexAggregatePlan::compile(query).map_err(QueryError::AggregateStreamPlan)?;
    // Expose row.values() directly, without coercing/cloning numeric or scalar
    // payloads. The checked plain global shape has no keys/hidden summaries;
    // refuse a future facade remapping rather than silently mislabeling cells.
    if columns != plan.columns()
        || slots.len() != columns.len()
        || !slots.iter().enumerate().all(|(index, slot)| {
            matches!(slot, GraphAggregateTextSlot::Aggregate(at) if *at == index)
        })
    {
        return Err(QueryError::StreamingUnsupported { facade });
    }
    Ok(plan)
}
