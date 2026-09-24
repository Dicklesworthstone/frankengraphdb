//! Governed global and grouped exact aggregates over a vertex source.
//!
//! The ordinary vertex GLA compiler owns predicates and existence probes. The
//! source is driven once, without a projected input bag. Global aggregation
//! retains a fixed number of numeric cells. Grouped aggregation retains one
//! owned canonical key, numeric state and selected extrema per group, then yields
//! completed groups in canonical result order. Group storage is governed but
//! not spill-backed; source residency
//! is independent of this operator's live-state bound.
//! Argument DISTINCT retains canonical support per aggregate and group under
//! the same meter. Its state grows with unique values, not row occurrences.
//! COLLECT retains nonnull occurrences in the admitted visitation order;
//! COLLECT DISTINCT retains the first occurrence, not sorted support.

mod collection;
mod distinct;
use distinct::DistinctState;

use super::*;
use crate::algebra::{GraphValue, GraphValueRow, ValueProjection};
use crate::{
    GraphAggregateError, GraphAggregateFunction, GraphAggregateRow, GraphAggregateValue,
    GraphExactAverage, PreparedGraphAggregate,
};
use std::collections::{BTreeMap, btree_map};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VertexAggregateBuildError {
    RequiresPlainGlobalAggregate,
    Scan(VertexScanBuildError),
}
impl core::fmt::Display for VertexAggregateBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RequiresPlainGlobalAggregate => {
                f.write_str("vertex aggregate stream requires plain exact aggregates")
            }
            Self::Scan(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for VertexAggregateBuildError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            _ => None,
        }
    }
}

/// An immutable physical specialization, not a second query language. Grouping
/// keys may be scalar properties, native vertex identities or checked computed
/// values. Row-local expressions use the shared projection VM before grouping
/// and argument DISTINCT. Plain operands retain their direct borrowed path.
/// HAVING, hidden/repeated columns, output expressions, output DISTINCT,
/// ORDER BY and SKIP/LIMIT use the same completed-group stage as edge streams.
/// COLLECT preserves the ordinary vertex visitor's ascending-identity order.
/// Computed collection inputs must also satisfy the ordinary row-stream order
/// proof: their batch child sorts rows before evaluating expressions. A child
/// with a different order refuses rather than silently reordering a list.
/// Relational inputs still refuse. Argument DISTINCT and output DISTINCT remain
/// separate stages on opposite sides of aggregation. Collections are metered
/// owned results, not constant-state numeric summaries or spill-backed values.
#[derive(Clone)]
pub struct VertexAggregatePlan {
    input: VertexScanPlan<GraphValueRow>,
    aggregate: PreparedGraphAggregate,
    collects: bool,
}
impl VertexAggregatePlan {
    pub fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, VertexAggregateBuildError> {
        if !aggregate.aggregates().iter().all(|spec| {
            NumericState::supports(spec.function()) || NumericState::collects(spec.function())
        }) {
            return Err(VertexAggregateBuildError::RequiresPlainGlobalAggregate);
        }
        let aggregate = aggregate
            .prepare_streamed_output()
            .ok_or(VertexAggregateBuildError::RequiresPlainGlobalAggregate)?;
        let collects = aggregate
            .aggregates()
            .iter()
            .any(|spec| NumericState::collects(spec.function()));
        if collects && aggregate.input_projection().is_some() {
            // Batch computed inputs consume the child's canonical sorted rows.
            // Reuse the sealed identity-leading proof rather than assuming that
            // a commutative numeric reducer's relaxed projection is ordered.
            VertexScanPlan::compile(aggregate.input_pattern().plan())
                .map_err(VertexAggregateBuildError::Scan)?;
        }
        let input = VertexScanPlan::compile_with_projection(
            aggregate.input_pattern().plan(),
            |projection, ordering| {
                let GlaOperator::ProjectValues { columns } = projection else {
                    return false;
                };
                matches!(ordering, GlaOperator::OrderByValues)
                    && columns.iter().all(|column| match column {
                        ValueProjection::Vertex { slot }
                        | ValueProjection::Property { slot, .. } => slot.ordinal() == 0,
                        _ => false,
                    })
            },
        )
        .map_err(VertexAggregateBuildError::Scan)?;
        Ok(Self {
            input,
            aggregate,
            collects,
        })
    }

    /// Names addressing GraphAggregateRow::values(), not grouping keys.
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.aggregate.aggregate_columns()
    }
    /// Names addressing GraphAggregateRow::keys(), in canonical grouping order.
    #[must_use]
    pub fn key_columns(&self) -> &[String] {
        self.aggregate.key_columns()
    }
}
impl core::fmt::Debug for VertexAggregatePlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("VertexAggregatePlan([REDACTED])")
    }
}

pub type VertexAggregateError<E, C> = GqlQueryError<GraphAggregateError<VertexScanError<E>>, C>;
type Groups = BTreeMap<Vec<GraphValue>, Vec<NumericState>>;
type PendingGroups = btree_map::IntoIter<Vec<GraphValue>, Vec<NumericState>>;

/// The first demand consumes the source; subsequent demands move completed
/// groups out in canonical result order. Plain reductions move group state;
/// result clauses finish and validate all groups before exposing a selected page.
/// Global empty input has one zero/null row. Grouped empty input has no rows.
/// No partial group escapes. Source/data and group-count refusals precede ANY
/// output; cancellation/work/scratch refusal during delivery may follow earlier
/// complete rows, as for other pull cursors. One typed error permanently fuses
/// the cursor. Close releases both the source and undelivered groups, without
/// draining. Every phase uses the same cumulative meter.
/// Only selected post-HAVING/DISTINCT/window rows spend the result allowance.
/// Ordered ALL retains at most min(groups, SKIP+LIMIT) ranked candidates;
/// DISTINCT also retains one representative per output class. Neither LIMIT
/// nor a delivery quota bounds upstream group/support memory or source residency.
/// Each COLLECT list entry and owned payload is charged before retention. A
/// DISTINCT collection also retains separately charged equality support until
/// finalization. No result-row budget is spent on individual list elements.
pub struct VertexAggregateCursor<S, F> {
    source: Option<S>,
    plan: VertexAggregatePlan,
    meter: Meter<F>,
    snapshot_seq: CommitSeq,
    state: VertexScanState,
    pending: Option<PendingGroups>,
    completed: Option<std::vec::IntoIter<GraphAggregateRow>>,
}
impl<S: VertexScanSource, F> VertexAggregateCursor<S, F> {
    pub fn new(
        source: S,
        plan: VertexAggregatePlan,
        policy: GqlQueryPolicy,
        checkpoint: F,
    ) -> Self {
        Self {
            snapshot_seq: source.snapshot_seq(),
            source: Some(source),
            plan,
            meter: Meter {
                checkpoint,
                policy,
                rows: GqlExecutionStats {
                    snapshot_records: 0,
                    result_rows: 0,
                },
                evaluator: GlaExecutionStats::default(),
            },
            state: VertexScanState::Open,
            pending: None,
            completed: None,
        }
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.plan.columns()
    }
    #[must_use]
    pub fn key_columns(&self) -> &[String] {
        self.plan.key_columns()
    }
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.snapshot_seq
    }
    #[must_use]
    pub fn state(&self) -> VertexScanState {
        self.state
    }
    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats {
        self.meter.rows
    }
    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.meter.evaluator
    }
    pub fn close(&mut self) {
        if self.state == VertexScanState::Open {
            self.state = VertexScanState::Closed;
        }
        self.source = None;
        self.pending = None;
        self.completed = None;
    }

    fn accumulate<C>(&mut self) -> Result<Groups, VertexAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        let meter = &mut self.meter;
        meter.event(VertexScanEvent::Work).map_err(lift)?;
        let global = self.plan.aggregate.group_key_columns().is_empty();
        let mut groups = Groups::new();
        if global {
            // Preserve constant-state global aggregation and its early output
            // admission, including a zero output budget on empty input.
            if !self.plan.aggregate.has_streamed_output_stage() {
                let _ = meter.next_result_count().map_err(lift)?;
            }
            meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
            groups.insert(Vec::new(), states(&self.plan.aggregate, meter)?);
        }
        let GlaOperator::ProjectValues { columns } = self.plan.input.projection.as_ref() else {
            unreachable!("the immutable physical plan admitted value projections");
        };
        let source = self
            .source
            .as_mut()
            .expect("uninitialized cursor owns a source");
        let mut last = None;
        let mut largest_computed_key = 0_usize;
        if !self.plan.input.empty {
            loop {
                let next =
                    flatten(source.next_vertex(&mut |event| meter.event(event))).map_err(lift)?;
                let Some(vid) = next else {
                    break;
                };
                meter.event(VertexScanEvent::Work).map_err(lift)?;
                if last.is_some_and(|previous| vid <= previous) {
                    return Err(lift(GqlQueryError::Source(
                        VertexScanError::NonIncreasingIdentity,
                    )));
                }
                last = Some(vid);
                meter.record().map_err(lift)?;
                let record = flatten(source.vertex_record(vid, &mut |event| meter.event(event)))
                    .map_err(lift)?;
                let Some(record) = record else {
                    continue;
                };
                // One source-admitted image lives through this occurrence's
                // predicates, computed columns and aggregate updates. A scoped
                // source may own masked fields and refuse raw vertex(); never
                // bypass that boundary for grouping, DISTINCT or collections.
                // The borrowed default adds no events and copies no payload.
                let row = record.as_row();
                let accepted = {
                    let metered = std::cell::RefCell::new(&mut *meter);
                    self.plan
                        .input
                        .accepts(
                            vid,
                            row,
                            &*source,
                            &mut |event| metered.borrow_mut().event(event),
                            &mut || metered.borrow_mut().record(),
                        )
                        .map_err(lift)?
                };
                if !accepted {
                    continue;
                }
                // Only computed definitions own a transient source projection.
                // The sealed collector retains ordinary property/null behavior
                // and charges every copy; the shared VM then completes ALL
                // computed columns before a group can see this occurrence.
                let computed = if self.plan.aggregate.input_projection().is_some() {
                    let input = project_input_row::<GraphValueRow, _>(
                        vid,
                        row,
                        &self.plan.input.projection,
                        &mut |event| meter.event(event).map_err(lift),
                    )?;
                    Some(
                        self.plan
                            .aggregate
                            .evaluate_streamed_input(input, &mut |event| {
                                meter.event(input_event(event)).map_err(lift)
                            })?,
                    )
                } else {
                    None
                };
                let state = if global {
                    groups
                        .get_mut(&Vec::<GraphValue>::new())
                        .expect("global group was installed")
                } else {
                    meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
                    let mut key = Vec::new();
                    for &column in self.plan.aggregate.group_key_columns() {
                        meter.event(VertexScanEvent::Work).map_err(lift)?;
                        let value = match projected_argument(
                            column,
                            computed.as_ref(),
                            columns,
                            vid,
                            row,
                            meter,
                        )? {
                            Input::Vertex(vid) => GraphValue::Vertex(vid),
                            Input::Identity => unreachable!("a key has a checked input column"),
                            Input::Value(value) => value.copy_with_control(&mut |event| {
                                meter.event(input_event(event)).map_err(lift)
                            })?,
                            Input::Scalar(value) => {
                                let value = value.unwrap_or(&CanonicalScalar::Null);
                                // Charge the actual borrowed payload before cloning it.
                                // Copies are temporary on an existing group and retained
                                // only for a new one; no input row is saved.
                                let units = scalar_payload_units(value);
                                for _ in 0..units {
                                    meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
                                }
                                GraphValue::Scalar(value.clone())
                            }
                        };
                        meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
                        key.push(value);
                    }
                    if computed.is_some() {
                        // Computed keys may own recursive native values. Reserve
                        // comparison work for this logical lookup/insertion;
                        // this is not std::BTreeMap allocator accounting.
                        let units = key.iter().fold(0_usize, |total, value| {
                            total
                                .saturating_add(value.payload_units())
                                .saturating_add(1)
                        });
                        largest_computed_key = largest_computed_key.max(units);
                        let levels = groups.len().saturating_add(1).ilog2() as usize + 1;
                        for _ in 0..levels
                            .saturating_mul(24)
                            .saturating_mul(largest_computed_key.saturating_add(1))
                        {
                            meter.event(VertexScanEvent::Work).map_err(lift)?;
                        }
                    }
                    // Only a plain reduction emits every group. Result clauses
                    // may reject, collapse or skip groups, so their quota is
                    // checked after selection, not on private support counts.
                    let next = groups
                        .len()
                        .checked_add(1)
                        .and_then(|n| u64::try_from(n).ok())
                        .ok_or(GqlQueryError::Source(
                            GraphAggregateError::ResultCountOverflow,
                        ))?;
                    meter.event(VertexScanEvent::Work).map_err(lift)?;
                    match groups.entry(key) {
                        btree_map::Entry::Occupied(entry) => entry.into_mut(),
                        btree_map::Entry::Vacant(entry) => {
                            if !self.plan.aggregate.has_streamed_output_stage() {
                                meter
                                    .policy
                                    .rows
                                    .check(GqlBudgetDimension::ResultRows, next)
                                    .map_err(GqlQueryError::Rows)?;
                            }
                            meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
                            entry.insert(states(&self.plan.aggregate, meter)?)
                        }
                    }
                };
                for (aggregate, (spec, state)) in self
                    .plan
                    .aggregate
                    .aggregates()
                    .iter()
                    .zip(state.iter_mut())
                    .enumerate()
                {
                    meter.event(VertexScanEvent::Work).map_err(lift)?;
                    let input = match spec.argument_column() {
                        None => Input::Identity,
                        Some(column) => {
                            projected_argument(column, computed.as_ref(), columns, vid, row, meter)?
                        }
                    };
                    state.update_governed(input, aggregate, &mut |event| {
                        meter.event(event).map_err(lift)
                    })?;
                }
            }
        }
        // Empty grouped results have no emit() call to provide this checkpoint.
        meter.event(VertexScanEvent::Work).map_err(lift)?;
        Ok(groups)
    }

    fn finalize<C>(
        &mut self,
        keys: Vec<GraphValue>,
        states: Vec<NumericState>,
    ) -> Result<GraphAggregateRow, VertexAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        let mut values = Vec::new();
        for state in states {
            self.meter
                .event(VertexScanEvent::ScratchEntry)
                .map_err(lift)?;
            values.push(state.finish_governed(&mut |event| self.meter.event(event).map_err(lift))?);
        }
        let row = if keys.is_empty() {
            // Preserve empty global SUM/AVG over a nonnumeric static domain:
            // no nonnull operand was encountered, so its answer is NULL.
            GraphAggregateRow::from_global_values(values)
        } else if self.plan.aggregate.input_projection().is_some()
            || self.plan.aggregate.has_streamed_output_stage()
            || self.plan.collects
        {
            // The projection compiler checked every column and the shared
            // exact cells checked every consumed value. Preserve native key
            // domains without weakening public maintained-row admission.
            GraphAggregateRow::from_group_values(keys, values)
        } else {
            self.plan
                .aggregate
                .materialize_incremental_row(keys, values)
                .expect("checked aggregate fixes key and result domains")
        };
        Ok(row)
    }

    fn select_output<C>(
        &mut self,
        groups: Groups,
    ) -> Result<Vec<GraphAggregateRow>, VertexAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        if !self.plan.aggregate.ordering().is_empty()
            || self.plan.aggregate.incremental_output_is_distinct()
        {
            let mut ranking = self.plan.aggregate.streamed_group_ranking(groups.len());
            for (keys, states) in groups {
                let row = self.finalize(keys, states)?;
                let meter = &mut self.meter;
                ranking.push(&self.plan.aggregate, row, &mut |event| {
                    meter.event(input_event(event)).map_err(lift)
                })?;
            }
            let meter = &mut self.meter;
            let rows = ranking.finish(&self.plan.aggregate, &mut |event| {
                meter.event(input_event(event)).map_err(lift)
            })?;
            let count = u64::try_from(rows.len())
                .map_err(|_| GqlQueryError::Source(GraphAggregateError::ResultCountOverflow))?;
            meter
                .policy
                .rows
                .check(GqlBudgetDimension::ResultRows, count)
                .map_err(GqlQueryError::Rows)?;
            return Ok(rows);
        }
        // Canonical unordered pages need no second ranked collection. Retire
        // each raw group once, but evaluate even off-page qualified expressions.
        let (offset, count) = self.plan.aggregate.incremental_result_window();
        let mut skipped = 0_u64;
        let mut selected = Vec::new();
        for (keys, states) in groups {
            let row = self.finalize(keys, states)?;
            let meter = &mut self.meter;
            let Some(row) = self
                .plan
                .aggregate
                .evaluate_streamed_output(row, &mut |event| {
                    meter.event(input_event(event)).map_err(lift)
                })?
            else {
                continue;
            };
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if count.is_some_and(|count| selected.len() as u64 >= count) {
                continue;
            }
            let next = selected
                .len()
                .checked_add(1)
                .and_then(|n| u64::try_from(n).ok())
                .ok_or(GqlQueryError::Source(
                    GraphAggregateError::ResultCountOverflow,
                ))?;
            meter
                .policy
                .rows
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(GqlQueryError::Rows)?;
            meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
            selected.push(row);
        }
        self.meter.event(VertexScanEvent::Work).map_err(lift)?;
        Ok(selected)
    }
}

// A bounded transient row, using the same sealed GLA collector as row streams.
// The generic bound exposes its inherited projection without a second encoder.
fn project_input_row<Row: VertexScanOutput, E>(
    vid: VId,
    row: VertexScanRow<'_>,
    projection: &GlaOperator,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
) -> Result<Row, E> {
    Row::project(vid, row, projection, control)
}

fn input_event(event: GlaExecutionEvent) -> VertexScanEvent {
    match event {
        GlaExecutionEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
        // A private projection cannot spend the public result-row allowance.
        GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => VertexScanEvent::Work,
    }
}

fn projected_argument<'a, F, E, C>(
    column: usize,
    computed: Option<&'a GraphValueRow>,
    columns: &[ValueProjection],
    vid: VId,
    row: VertexScanRow<'a>,
    meter: &mut Meter<F>,
) -> Result<Input<'a>, VertexAggregateError<E, C>>
where
    F: FnMut() -> Result<(), C>,
{
    match computed {
        Some(values) => Ok(Input::from_value(&values.values()[column])),
        None => argument(&columns[column], vid, row, meter),
    }
}

fn scalar_payload_units(value: &CanonicalScalar) -> usize {
    let bytes = match value {
        CanonicalScalar::Bytes(value) => value.as_slice().len(),
        CanonicalScalar::Text(value) => value
            .len()
            .saturating_add(value.canonical_sort_key().map_or(0, <[u8]>::len)),
        CanonicalScalar::Timestamp(value) => value.zone().map_or(0, |zone| zone.identifier().len()),
        _ => 0,
    };
    bytes.div_ceil(crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
}
fn argument<'a, F, E, C>(
    column: &ValueProjection,
    vid: VId,
    row: VertexScanRow<'a>,
    meter: &mut Meter<F>,
) -> Result<Input<'a>, VertexAggregateError<E, C>>
where
    F: FnMut() -> Result<(), C>,
{
    match column {
        ValueProjection::Vertex { .. } => Ok(Input::Vertex(vid)),
        ValueProjection::Property { key, .. } => {
            let found = seek(row.properties, key, |entry| entry.0, &mut |event| {
                meter.event(event)
            })
            .map_err(lift)?;
            Ok(Input::Scalar(found.map(|(_, value)| value)))
        }
        _ => unreachable!("projection profile checked before source access"),
    }
}
fn states<F, E, C>(
    definition: &PreparedGraphAggregate,
    meter: &mut Meter<F>,
) -> Result<Vec<NumericState>, VertexAggregateError<E, C>>
where
    F: FnMut() -> Result<(), C>,
{
    meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
    let mut states = Vec::new();
    for spec in definition.aggregates() {
        meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
        states.push(NumericState::new_governed(spec.function(), &mut |event| {
            meter.event(event).map_err(lift)
        })?);
    }
    Ok(states)
}
fn lift<E, C>(error: GqlQueryError<VertexScanError<E>, C>) -> VertexAggregateError<E, C> {
    error.map_source(GraphAggregateError::Source)
}

#[derive(Clone, Copy)]
pub(crate) enum Input<'a> {
    Identity,
    Vertex(VId),
    Scalar(Option<&'a CanonicalScalar>),
    /// Borrowed edge/path/collection values from a checked row projection.
    Value(&'a GraphValue),
}
impl<'a> Input<'a> {
    pub(crate) fn from_value(value: &'a GraphValue) -> Self {
        match value {
            GraphValue::Scalar(scalar) => Self::Scalar(Some(scalar)),
            GraphValue::Vertex(vid) => Self::Vertex(*vid),
            _ => Self::Value(value),
        }
    }

    fn normalized(self) -> Self {
        match self {
            Self::Value(value) => Self::from_value(value),
            input => input,
        }
    }

    fn payload_units(&self) -> usize {
        match self {
            Self::Scalar(Some(value)) => scalar_payload_units(value),
            Self::Value(value) => value.payload_units(),
            _ => 0,
        }
    }
}
pub(crate) enum NumericState {
    Distinct(Box<DistinctState>),
    Collect(Vec<GraphValue>),
    Count(u64),
    Sum(Option<i128>),
    Average {
        sum: i128,
        count: u64,
    },
    Extreme {
        value: Option<GraphValue>,
        maximum: bool,
        payload_units: usize,
    },
}
impl NumericState {
    // Order-sensitive cells need an additional physical source-order proof;
    // keep them out of supports(), the commutative reducer admission contract.
    pub(crate) fn collects(function: GraphAggregateFunction) -> bool {
        matches!(
            function,
            GraphAggregateFunction::Collect | GraphAggregateFunction::CollectDistinct
        )
    }

    pub(crate) fn supports(function: GraphAggregateFunction) -> bool {
        matches!(
            function,
            GraphAggregateFunction::CountRows
                | GraphAggregateFunction::Count
                | GraphAggregateFunction::SumInt
                | GraphAggregateFunction::AverageInt
                | GraphAggregateFunction::Min
                | GraphAggregateFunction::Max
                | GraphAggregateFunction::CountDistinct
                | GraphAggregateFunction::SumIntDistinct
                | GraphAggregateFunction::AverageIntDistinct
        )
    }

    /// Source-independent cells, not a second aggregate evaluator. Callers
    /// reserve their cell vector; this reserves DISTINCT's extra ownership box.
    pub(crate) fn new_governed<E>(
        function: GraphAggregateFunction,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        Ok(match function {
            GraphAggregateFunction::CountRows | GraphAggregateFunction::Count => Self::Count(0),
            GraphAggregateFunction::SumInt => Self::Sum(None),
            GraphAggregateFunction::AverageInt => Self::Average { sum: 0, count: 0 },
            GraphAggregateFunction::Collect => Self::Collect(Vec::new()),
            GraphAggregateFunction::Min | GraphAggregateFunction::Max => Self::Extreme {
                value: None,
                maximum: function == GraphAggregateFunction::Max,
                payload_units: 0,
            },
            GraphAggregateFunction::CountDistinct
            | GraphAggregateFunction::SumIntDistinct
            | GraphAggregateFunction::AverageIntDistinct
            | GraphAggregateFunction::CollectDistinct => {
                control(VertexScanEvent::ScratchEntry)?;
                Self::Distinct(Box::new(DistinctState::new(function)))
            }
        })
    }

    /// Move extrema and retire DISTINCT support. Normalization keeps the exact
    /// rational domain and the existing bounded Euclidean-work reservation.
    pub(crate) fn finish_governed<E>(
        self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
    ) -> Result<GraphAggregateValue, E> {
        let state = match self {
            Self::Distinct(state) => (*state).into_numeric(),
            state => state,
        };
        Ok(match state {
            Self::Count(value) => GraphAggregateValue::Count(value),
            Self::Collect(values) => {
                // Retire membership before moving the list. Account for the
                // shallow Vec-to-box compaction, without recopying payloads.
                control(VertexScanEvent::ScratchEntry)?;
                for _ in &values {
                    control(VertexScanEvent::Work)?;
                }
                GraphAggregateValue::Value(GraphValue::List(values.into_boxed_slice()))
            }
            Self::Sum(Some(value)) => GraphAggregateValue::Integer(value),
            Self::Average { sum, count } if count != 0 => {
                for _ in 0..128 {
                    control(VertexScanEvent::Work)?;
                }
                GraphAggregateValue::Average(
                    GraphExactAverage::new(sum, count)
                        .expect("nonnull average has a positive denominator"),
                )
            }
            Self::Sum(None) | Self::Average { .. } => {
                GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null))
            }
            Self::Extreme { value, .. } => GraphAggregateValue::Value(
                value.unwrap_or(GraphValue::Scalar(CanonicalScalar::Null)),
            ),
            Self::Distinct(_) => unreachable!("DISTINCT cannot contain DISTINCT"),
        })
    }

    // The edge reducer admits only COUNT/SUM. Share their exact output domains
    // without changing the vertex AVG/extremum path or its event sequence.
    pub(crate) fn finish(self) -> GraphAggregateValue {
        match self {
            Self::Count(value) => GraphAggregateValue::Count(value),
            Self::Sum(Some(value)) => GraphAggregateValue::Integer(value),
            Self::Sum(None) => {
                GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null))
            }
            _ => unreachable!("checked COUNT/SUM reducer"),
        }
    }

    pub(crate) fn update_governed<E, C>(
        &mut self,
        input: Input<'_>,
        aggregate: usize,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
        let input = input.normalized();
        if let Self::Distinct(state) = self {
            return state.update(input, aggregate, control);
        }
        if let Self::Collect(values) = self {
            return collection::push(values, input, control);
        }
        let Self::Extreme {
            value,
            maximum,
            payload_units,
        } = self
        else {
            return self.update(input, aggregate);
        };
        if matches!(input, Input::Scalar(None | Some(CanonicalScalar::Null))) {
            return Ok(());
        }
        let units = input.payload_units();
        // Keep the established global MIN/MAX kernel's admission before both
        // canonical comparison and replacement ownership, now per group.
        for _ in 0..units.saturating_add(*payload_units) {
            control(VertexScanEvent::Work)?;
        }
        let replace = match (value.as_ref(), &input) {
            (None, _) => true,
            (Some(GraphValue::Vertex(old)), Input::Vertex(next)) => {
                if *maximum {
                    next > old
                } else {
                    next < old
                }
            }
            (Some(GraphValue::Scalar(old)), Input::Scalar(Some(next))) => {
                if *maximum {
                    *next > old
                } else {
                    *next < old
                }
            }
            (Some(old), Input::Value(next)) => {
                if *maximum {
                    *next > old
                } else {
                    *next < old
                }
            }
            _ => unreachable!("a checked aggregate has one immutable argument domain"),
        };
        if replace {
            control(VertexScanEvent::ScratchEntry)?;
            for _ in 0..units {
                control(VertexScanEvent::ScratchEntry)?;
            }
            let owned = match input {
                Input::Vertex(vid) => GraphValue::Vertex(vid),
                Input::Scalar(Some(scalar)) => GraphValue::Scalar(scalar.clone()),
                Input::Value(value) => value.clone(),
                _ => unreachable!("MIN/MAX has a checked nonnull argument"),
            };
            *value = Some(owned);
            *payload_units = units;
        }
        Ok(())
    }
    pub(crate) fn update<E, C>(
        &mut self,
        input: Input<'_>,
        aggregate: usize,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
        let input = input.normalized();
        if matches!(input, Input::Scalar(None | Some(CanonicalScalar::Null))) {
            return Ok(());
        }
        let overflow =
            || GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate });
        match self {
            Self::Count(value) => *value = value.checked_add(1).ok_or_else(overflow)?,
            Self::Sum(total) => {
                let Input::Scalar(Some(CanonicalScalar::Int(value))) = input else {
                    return Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
                        aggregate,
                    }));
                };
                *total = Some(
                    total
                        .unwrap_or(0)
                        .checked_add(i128::from(*value))
                        .ok_or_else(overflow)?,
                );
            }
            Self::Average { sum, count } => {
                let Input::Scalar(Some(CanonicalScalar::Int(value))) = input else {
                    return Err(GqlQueryError::Source(
                        GraphAggregateError::NonIntegerAverage { aggregate },
                    ));
                };
                let next_count = count.checked_add(1).ok_or_else(overflow)?;
                let next_sum = sum.checked_add(i128::from(*value)).ok_or_else(overflow)?;
                *sum = next_sum;
                *count = next_count;
            }
            Self::Extreme { .. } | Self::Distinct(_) | Self::Collect(_) => {
                unreachable!("value support and ownership require governed updates")
            }
        }
        Ok(())
    }
}

impl<S, F, C> Iterator for VertexAggregateCursor<S, F>
where
    S: VertexScanSource,
    F: FnMut() -> Result<(), C>,
{
    type Item = Result<GraphAggregateRow, VertexAggregateError<S::Error, C>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.state != VertexScanState::Open {
            return None;
        }
        if self.pending.is_none() && self.completed.is_none() {
            let result = self.accumulate();
            // Every retained key is owned. Release the snapshot before delivery.
            self.source = None;
            match result {
                Ok(groups) if self.plan.aggregate.has_streamed_output_stage() => {
                    match self.select_output(groups) {
                        Ok(rows) => self.completed = Some(rows.into_iter()),
                        Err(error) => {
                            self.state = VertexScanState::Failed;
                            return Some(Err(error));
                        }
                    }
                }
                Ok(groups) => self.pending = Some(groups.into_iter()),
                Err(error) => {
                    self.state = VertexScanState::Failed;
                    return Some(Err(error));
                }
            }
        }
        let row = if let Some(rows) = &mut self.completed {
            rows.next().map(Ok)
        } else {
            self.pending
                .as_mut()
                .and_then(Iterator::next)
                .map(|(keys, states)| self.finalize(keys, states))
        };
        let Some(row) = row else {
            self.state = VertexScanState::Exhausted;
            self.pending = None;
            self.completed = None;
            return None;
        };
        let result = row.and_then(|row| {
            self.meter.emit().map_err(lift)?;
            Ok(row)
        });
        if result.is_err() {
            self.state = VertexScanState::Failed;
            self.pending = None;
            self.completed = None;
        } else if self
            .pending
            .as_ref()
            .is_some_and(|groups| groups.len() == 0)
            || self.completed.as_ref().is_some_and(|rows| rows.len() == 0)
        {
            self.state = VertexScanState::Exhausted;
            self.pending = None;
            self.completed = None;
        }
        Some(result)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.state != VertexScanState::Open {
            return (0, Some(0));
        }
        if let Some(rows) = &self.completed {
            return (0, Some(rows.len()));
        }
        match &self.pending {
            Some(groups) => (0, Some(groups.len())),
            None if self.plan.aggregate.group_key_columns().is_empty() => (0, Some(1)),
            // Empty grouped input can still produce ONE error, so unknown is
            // the only valid upper bound before source completion.
            None => (0, None),
        }
    }
}
impl<S, F, C> FusedIterator for VertexAggregateCursor<S, F>
where
    S: VertexScanSource,
    F: FnMut() -> Result<(), C>,
{
}
impl<S, F> core::fmt::Debug for VertexAggregateCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VertexAggregateCursor")
            .field("state", &self.state)
            .field("rows", &self.meter.rows)
            .field("evaluator", &self.meter.evaluator)
            .field("definition_and_source", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod value_tests;

#[cfg(test)]
mod computed_tests;

#[cfg(test)]
mod output_tests;

#[cfg(test)]
mod record_tests;
