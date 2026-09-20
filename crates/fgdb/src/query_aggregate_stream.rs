//! Native text entrypoints for governed global and grouped aggregate pulls.
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
    fn key_columns(&self) -> &[String];
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
    fn key_columns(&self) -> &[String] {
        VertexAggregateCursor::key_columns(self)
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
    fn key_columns(&self) -> &[String] {
        // EdgeAggregatePlan currently admits global definitions only.
        &[]
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

/// A native aggregate query whose first pull consumes the source. Subsequent
/// pulls move completed groups out in the operator's canonical key order.
/// Global empty input emits one zero/null row; grouped empty input emits none.
/// `columns()` and `output_slots()` describe textual RETURN order, which may
/// interleave or repeat keys among aggregates. The returned GraphAggregateRow
/// keeps separate keys()/values() storage, addressed by key_columns() and
/// aggregate_columns(). No scalar, identity or exact numeric domain is coerced.
///
/// Opening reads no candidates. Close/drop does not drain the source. Failure
/// emits one typed error and permanently fuses the cursor. Source/accumulation
/// failures precede all output; delivery failure may follow complete groups.
/// Closing releases the pin and any undelivered group state without draining.
/// No projected input table, per-row query, data copy or second meter is added.
/// The one ownership box is metadata; the pinned database is still in memory.
/// Group state is governed but not spill-backed. This is not a session lease
/// or a durable resumption token.
/// Vertex and fixed-edge/join profiles share this surface. Source failures keep
/// their original typed cause in ScanError; budgets and interruption stay outside
/// that sum. The selected operator never changes after opening.
pub struct NativeAggregateCursor<'q> {
    inner: Box<dyn AggregatePull + 'q>,
    layout: OutputLayout,
}
impl<'q> NativeAggregateCursor<'q> {
    fn new(cursor: impl AggregatePull + 'q, layout: OutputLayout) -> Self {
        Self {
            inner: Box::new(cursor),
            layout,
        }
    }
    /// The physical source chosen structurally before any candidate is read.
    #[must_use]
    pub fn kind(&self) -> ScanKind {
        self.inner.kind()
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.layout.columns
    }
    /// Each RETURN column selects a borrowed key or aggregate by its native
    /// ordinal. Repeated expressions with distinct aliases need no payload copy.
    #[must_use]
    pub fn output_slots(&self) -> &[GraphAggregateTextSlot] {
        &self.layout.slots
    }
    #[must_use]
    pub fn key_columns(&self) -> &[String] {
        self.inner.key_columns()
    }
    #[must_use]
    pub fn aggregate_columns(&self) -> &[String] {
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
    /// Open a checked aggregate specialization from this native
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
        let (compiled, as_of) = match self {
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
        let CompiledAggregate { plan, layout } = compiled;
        match plan {
            AggregatePlan::Vertex(plan) => Ok(NativeAggregateCursor::new(
                view.stream_global_aggregate_governed_at(cx, &plan, as_of, policy)
                    .map_err(QueryError::AggregateStream)?,
                layout,
            )),
            AggregatePlan::Edge(plan) => Ok(NativeAggregateCursor::new(
                view.stream_global_edge_aggregate_governed_at(cx, &plan, as_of, policy)
                    .map_err(QueryError::EdgeAggregateStream)?,
                layout,
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
    fn key_columns(&self) -> &[String] {
        match self {
            Self::Vertex(plan) => plan.key_columns(),
            Self::Edge(_) => &[],
        }
    }
}

struct OutputLayout {
    columns: Vec<String>,
    slots: Vec<GraphAggregateTextSlot>,
}
struct CompiledAggregate {
    plan: AggregatePlan,
    layout: OutputLayout,
}

fn compile(
    query: &PreparedGraphAggregate,
    columns: &[String],
    slots: &[GraphAggregateTextSlot],
    facade: crate::NativeReadClass,
) -> Result<CompiledAggregate, QueryError> {
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
    // Keep the physical row unmodified. Only bounded, owned schema metadata
    // maps the native RETURN order onto its keys and aggregate values. Reject
    // inconsistent future compiler metadata BEFORE opening a source, including
    // an out-of-range, missing, or misnamed first occurrence of a column.
    if !valid_layout(columns, slots, plan.key_columns(), plan.columns()) {
        return Err(QueryError::StreamingUnsupported { facade });
    }
    Ok(CompiledAggregate {
        plan,
        layout: OutputLayout { columns: columns.to_vec(), slots: slots.to_vec() },
    })
}

fn valid_layout(
    columns: &[String],
    slots: &[GraphAggregateTextSlot],
    keys: &[String],
    aggregates: &[String],
) -> bool {
    if columns.len() != slots.len()
        || columns.len() > fgdb_gql::algebra::MAX_PATTERN_VERTICES
        || slots.iter().any(|slot| match slot {
            GraphAggregateTextSlot::GroupKey(at) => *at >= keys.len(),
            GraphAggregateTextSlot::Aggregate(at) => *at >= aggregates.len(),
        })
    {
        return false;
    }
    // The physical profiles retain all keys and summaries. The first alias
    // names each retained cell; repeating a key later with a different alias is
    // valid native projection, not another group or another owned key payload.
    keys.iter().enumerate().all(|(index, name)| {
        slots.iter().position(|slot| matches!(slot,
            GraphAggregateTextSlot::GroupKey(at) if *at == index))
            .is_some_and(|at| &columns[at] == name)
    }) && aggregates.iter().enumerate().all(|(index, name)| {
        slots.iter().position(|slot| matches!(slot,
            GraphAggregateTextSlot::Aggregate(at) if *at == index))
            .is_some_and(|at| &columns[at] == name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use GraphAggregateTextSlot::{Aggregate, GroupKey};

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn layout_admits_interleaved_reordered_and_repeated_keys_without_copying_cells() {
        let keys = names(&["first", "second"]);
        let aggregates = names(&["count", "sum"]);
        for order in [
            [0, 1, 2, 3], [2, 0, 3, 1], [1, 3, 0, 2], [3, 2, 1, 0],
        ] {
            let schema = names(&["first", "second", "count", "sum"]);
            let all = [GroupKey(0), GroupKey(1), Aggregate(0), Aggregate(1)];
            let columns: Vec<_> = order.iter().map(|&at| schema[at].clone()).collect();
            let slots: Vec<_> = order.iter().map(|&at| all[at]).collect();
            assert!(valid_layout(&columns, &slots, &keys, &aggregates));
        }
        assert!(valid_layout(
            &names(&["count", "first", "alias", "sum", "second"]),
            &[Aggregate(0), GroupKey(0), GroupKey(0), Aggregate(1), GroupKey(1)],
            &keys, &aggregates,
        ));
    }

    #[test]
    fn layout_rejects_wrong_names_missing_cells_bad_ordinals_and_unbounded_metadata() {
        let keys = names(&["key"]);
        let aggregates = names(&["total"]);
        for (columns, slots) in [
            (names(&["key", "total"]), vec![Aggregate(0), GroupKey(0)]),
            (names(&["key", "total"]), vec![GroupKey(1), Aggregate(0)]),
            (names(&["key", "total"]), vec![GroupKey(0), Aggregate(1)]),
            (names(&["key"]), vec![GroupKey(0)]),
            (names(&["total"]), vec![Aggregate(0)]),
            (names(&["key", "total"]), vec![GroupKey(0)]),
        ] {
            assert!(!valid_layout(&columns, &slots, &keys, &aggregates));
        }
        let len = fgdb_gql::algebra::MAX_PATTERN_VERTICES + 1;
        assert!(!valid_layout(&vec!["total".to_owned(); len],
            &vec![Aggregate(0); len], &[], &aggregates));
    }
}
