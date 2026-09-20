//! Governed global and grouped exact aggregates over a vertex source.
//!
//! The ordinary vertex GLA compiler owns predicates and existence probes. The
//! source is driven once, without a projected input bag. Global aggregation
//! retains a fixed number of numeric cells. Grouped aggregation retains one
//! owned canonical key, numeric state and selected extrema per group, then yields
//! completed groups
//! in key order. Group storage is governed but not spill-backed; source residency
//! is independent of this operator's live-state bound.
//! Argument DISTINCT retains canonical support per aggregate and group under
//! the same meter. Its state grows with unique values, not row occurrences.

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
/// keys may be scalar properties or native vertex identities. Output DISTINCT,
/// computed/relational input, HAVING, ordering and pagination still refuse
/// before opening a source. The underlying row stream's leading-identity order
/// requirement is relaxed only because this operator owns result grouping.
/// Argument DISTINCT is independent of the unsupported output-DISTINCT stage.
#[derive(Clone)]
pub struct VertexAggregatePlan {
    input: VertexScanPlan<GraphValueRow>,
    aggregate: PreparedGraphAggregate,
}
impl VertexAggregatePlan {
    pub fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, VertexAggregateBuildError> {
        if !aggregate.supports_incremental_maintenance()
            || !aggregate.aggregates().iter().all(|spec| {
                matches!(
                    spec.function(),
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
            })
        {
            return Err(VertexAggregateBuildError::RequiresPlainGlobalAggregate);
        }
        let input = VertexScanPlan::compile_with_projection(
            aggregate.input_pattern().plan(),
            |projection, ordering| {
                let GlaOperator::ProjectValues { columns } = projection else {
                    return false;
                };
                matches!(ordering, GlaOperator::OrderByValues)
                    && columns.iter().all(|column| match column {
                        ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. } => {
                            slot.ordinal() == 0
                        }
                        _ => false,
                    })
            },
        )
        .map_err(VertexAggregateBuildError::Scan)?;
        Ok(Self { input, aggregate: aggregate.clone() })
    }

    /// Names addressing GraphAggregateRow::values(), not grouping keys.
    #[must_use]
    pub fn columns(&self) -> &[String] { self.aggregate.aggregate_columns() }
    /// Names addressing GraphAggregateRow::keys(), in canonical grouping order.
    #[must_use]
    pub fn key_columns(&self) -> &[String] { self.aggregate.key_columns() }
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
/// groups out in canonical key order without constructing a result-row vector.
/// Global empty input has one zero/null row. Grouped empty input has no rows.
/// No partial group escapes. Source/data and group-count refusals precede ANY
/// output; cancellation/work/scratch refusal during delivery may follow earlier
/// complete rows, as for other pull cursors. One typed error permanently fuses
/// the cursor. Close releases both the source and undelivered groups, without
/// draining. Every phase uses the same cumulative meter.
pub struct VertexAggregateCursor<S, F> {
    source: Option<S>,
    plan: VertexAggregatePlan,
    meter: Meter<F>,
    snapshot_seq: CommitSeq,
    state: VertexScanState,
    pending: Option<PendingGroups>,
}
impl<S: VertexScanSource, F> VertexAggregateCursor<S, F> {
    pub fn new(source: S, plan: VertexAggregatePlan, policy: GqlQueryPolicy, checkpoint: F) -> Self {
        Self {
            snapshot_seq: source.snapshot_seq(),
            source: Some(source),
            plan,
            meter: Meter {
                checkpoint,
                policy,
                rows: GqlExecutionStats { snapshot_records: 0, result_rows: 0 },
                evaluator: GlaExecutionStats::default(),
            },
            state: VertexScanState::Open,
            pending: None,
        }
    }
    #[must_use]
    pub fn columns(&self) -> &[String] { self.plan.columns() }
    #[must_use]
    pub fn key_columns(&self) -> &[String] { self.plan.key_columns() }
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq { self.snapshot_seq }
    #[must_use]
    pub fn state(&self) -> VertexScanState { self.state }
    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats { self.meter.rows }
    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats { self.meter.evaluator }
    pub fn close(&mut self) {
        if self.state == VertexScanState::Open { self.state = VertexScanState::Closed; }
        self.source = None;
        self.pending = None;
    }

    fn accumulate<C>(&mut self) -> Result<Groups, VertexAggregateError<S::Error, C>>
    where F: FnMut() -> Result<(), C>,
    {
        let meter = &mut self.meter;
        meter.event(VertexScanEvent::Work).map_err(lift)?;
        let global = self.plan.aggregate.group_key_columns().is_empty();
        let mut groups = Groups::new();
        if global {
            // Preserve constant-state global aggregation and its early output
            // admission, including a zero output budget on empty input.
            let _ = meter.next_result_count().map_err(lift)?;
            meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
            groups.insert(Vec::new(), states(&self.plan.aggregate, meter)?);
        }
        let GlaOperator::ProjectValues { columns } = self.plan.input.projection.as_ref() else {
            unreachable!("the immutable physical plan admitted value projections");
        };
        let source = self.source.as_mut().expect("uninitialized cursor owns a source");
        let mut last = None;
        while !self.plan.input.empty {
            let next = flatten(source.next_vertex(&mut |event| meter.event(event))).map_err(lift)?;
            let Some(vid) = next else { break; };
            meter.event(VertexScanEvent::Work).map_err(lift)?;
            if last.is_some_and(|previous| vid <= previous) {
                return Err(lift(GqlQueryError::Source(VertexScanError::NonIncreasingIdentity)));
            }
            last = Some(vid);
            meter.record().map_err(lift)?;
            let row = flatten(source.vertex(vid, &mut |event| meter.event(event))).map_err(lift)?;
            let Some(row) = row else { continue; };
            let accepted = {
                let metered = std::cell::RefCell::new(&mut *meter);
                self.plan.input.accepts(vid, row, &*source,
                    &mut |event| metered.borrow_mut().event(event),
                    &mut || metered.borrow_mut().record()).map_err(lift)?
            };
            if !accepted { continue; }
            let state = if global {
                groups.get_mut(&Vec::<GraphValue>::new()).expect("global group was installed")
            } else {
                meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
                let mut key = Vec::new();
                for &column in self.plan.aggregate.group_key_columns() {
                    meter.event(VertexScanEvent::Work).map_err(lift)?;
                    let value = match argument(&columns[column], vid, row, meter)? {
                        Input::Vertex(vid) => GraphValue::Vertex(vid),
                        Input::Identity => unreachable!("group keys have a declared column"),
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
                // The number of groups is monotone during this read. Admit a
                // new group's final output BEFORE allocating its numeric state.
                // This does not increment delivered result_rows.
                let next = groups.len().checked_add(1).and_then(|n| u64::try_from(n).ok())
                    .ok_or(GqlQueryError::Source(GraphAggregateError::ResultCountOverflow))?;
                meter.event(VertexScanEvent::Work).map_err(lift)?;
                match groups.entry(key) {
                    btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    btree_map::Entry::Vacant(entry) => {
                        meter.policy.rows.check(GqlBudgetDimension::ResultRows, next)
                            .map_err(GqlQueryError::Rows)?;
                        meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
                        entry.insert(states(&self.plan.aggregate, meter)?)
                    }
                }
            };
            for (aggregate, (spec, state)) in self.plan.aggregate.aggregates().iter()
                .zip(state.iter_mut()).enumerate()
            {
                meter.event(VertexScanEvent::Work).map_err(lift)?;
                let input = match spec.argument_column() {
                    None => Input::Identity,
                    Some(column) => argument(&columns[column], vid, row, meter)?,
                };
                state.update_governed(input, aggregate, &mut |event| {
                    meter.event(event).map_err(lift)
                })?;
            }
        }
        // Empty grouped results have no emit() call to provide this checkpoint.
        meter.event(VertexScanEvent::Work).map_err(lift)?;
        Ok(groups)
    }

    fn deliver<C>(&mut self, keys: Vec<GraphValue>, states: Vec<NumericState>)
        -> Result<GraphAggregateRow, VertexAggregateError<S::Error, C>>
    where F: FnMut() -> Result<(), C>,
    {
        let mut values = Vec::new();
        for state in states {
            // Membership changes admission, never the numeric result domain.
            let state = match state {
                NumericState::Distinct(state) => (*state).into_numeric(),
                state => state,
            };
            self.meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
            values.push(match state {
                NumericState::Count(value) => GraphAggregateValue::Count(value),
                NumericState::Sum(Some(value)) => GraphAggregateValue::Integer(value),
                NumericState::Average { sum, count } if count != 0 => {
                    // Reuse the ordinary exact rational normalization, never float.
                    for _ in 0..128 {
                        self.meter.event(VertexScanEvent::Work).map_err(lift)?;
                    }
                    GraphAggregateValue::Average(GraphExactAverage::new(sum, count)
                        .expect("nonnull average has a positive denominator"))
                }
                NumericState::Sum(None) | NumericState::Average { .. } => {
                    GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null))
                }
                NumericState::Extreme { value, .. } => GraphAggregateValue::Value(
                    value.unwrap_or(GraphValue::Scalar(CanonicalScalar::Null)),
                ),
                NumericState::Distinct(_) => unreachable!("DISTINCT cannot contain DISTINCT"),
            });
        }
        let row = if keys.is_empty() {
            // Preserve empty global SUM/AVG over a nonnumeric static domain:
            // no nonnull operand was encountered, so its answer is NULL.
            GraphAggregateRow::from_global_values(values)
        } else {
            self.plan.aggregate.materialize_incremental_row(keys, values)
                .expect("checked aggregate fixes key and result domains")
        };
        self.meter.emit().map_err(lift)?;
        Ok(row)
    }
}

fn scalar_payload_units(value: &CanonicalScalar) -> usize {
    let bytes = match value {
        CanonicalScalar::Bytes(value) => value.as_slice().len(),
        CanonicalScalar::Text(value) => value.len()
            .saturating_add(value.canonical_sort_key().map_or(0, <[u8]>::len)),
        CanonicalScalar::Timestamp(value) => value.zone().map_or(0, |zone| zone.identifier().len()),
        _ => 0,
    };
    bytes.div_ceil(crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
}
fn argument<'a, F, E, C>(column: &ValueProjection, vid: VId, row: VertexScanRow<'a>, meter: &mut Meter<F>)
    -> Result<Input<'a>, VertexAggregateError<E, C>>
where F: FnMut() -> Result<(), C>,
{
    match column {
        ValueProjection::Vertex { .. } => Ok(Input::Vertex(vid)),
        ValueProjection::Property { key, .. } => {
            let found = seek(row.properties, key, |entry| entry.0, &mut |event| meter.event(event))
                .map_err(lift)?;
            Ok(Input::Scalar(found.map(|(_, value)| value)))
        }
        _ => unreachable!("projection profile checked before source access"),
    }
}
fn states<F, E, C>(definition: &PreparedGraphAggregate, meter: &mut Meter<F>)
    -> Result<Vec<NumericState>, VertexAggregateError<E, C>>
where F: FnMut() -> Result<(), C>,
{
    meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
    let mut states = Vec::new();
    for spec in definition.aggregates() {
        meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
        states.push(match spec.function() {
            GraphAggregateFunction::CountRows | GraphAggregateFunction::Count => NumericState::Count(0),
            GraphAggregateFunction::SumInt => NumericState::Sum(None),
            GraphAggregateFunction::AverageInt => NumericState::Average { sum: 0, count: 0 },
            GraphAggregateFunction::Min | GraphAggregateFunction::Max => NumericState::Extreme {
                value: None,
                maximum: spec.function() == GraphAggregateFunction::Max,
                payload_units: 0,
            },
            GraphAggregateFunction::CountDistinct
            | GraphAggregateFunction::SumIntDistinct
            | GraphAggregateFunction::AverageIntDistinct => {
                meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
                NumericState::Distinct(Box::new(DistinctState::new(spec.function())))
            }
            _ => unreachable!("only scalar aggregates admitted"),
        });
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
}
impl Input<'_> {
    fn payload_units(&self) -> usize {
        match self {
            Self::Scalar(Some(value)) => scalar_payload_units(value),
            _ => 0,
        }
    }
}
pub(crate) enum NumericState {
    Distinct(Box<DistinctState>),
    Count(u64),
    Sum(Option<i128>),
    Average { sum: i128, count: u64 },
    Extreme { value: Option<GraphValue>, maximum: bool, payload_units: usize },
}
impl NumericState {
    // The edge reducer admits only COUNT/SUM. Share their exact output domains
    // without changing the vertex AVG/extremum path or its event sequence.
    pub(crate) fn finish(self) -> GraphAggregateValue {
        match self {
            Self::Count(value) => GraphAggregateValue::Count(value),
            Self::Sum(Some(value)) => GraphAggregateValue::Integer(value),
            Self::Sum(None) => GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
            _ => unreachable!("checked COUNT/SUM reducer"),
        }
    }

    fn update_governed<E, C>(
        &mut self,
        input: Input<'_>,
        aggregate: usize,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), VertexAggregateError<E, C>>,
    ) -> Result<(), VertexAggregateError<E, C>> {
        if let Self::Distinct(state) = self {
            return state.update(input, aggregate, control);
        }
        let Self::Extreme { value, maximum, payload_units } = self else {
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
                if *maximum { next > old } else { next < old }
            }
            (Some(GraphValue::Scalar(old)), Input::Scalar(Some(next))) => {
                if *maximum { *next > old } else { *next < old }
            }
            _ => unreachable!("a checked aggregate has one immutable argument domain"),
        };
        if replace {
            control(VertexScanEvent::ScratchEntry)?;
            for _ in 0..units { control(VertexScanEvent::ScratchEntry)?; }
            let owned = match input {
                Input::Vertex(vid) => GraphValue::Vertex(vid),
                Input::Scalar(Some(scalar)) => GraphValue::Scalar(scalar.clone()),
                _ => unreachable!("MIN/MAX has a checked nonnull argument"),
            };
            *value = Some(owned);
            *payload_units = units;
        }
        Ok(())
    }
    pub(crate) fn update<E, C>(&mut self, input: Input<'_>, aggregate: usize)
        -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>
    {
        if matches!(input, Input::Scalar(None | Some(CanonicalScalar::Null))) { return Ok(()); }
        let overflow = || GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate });
        match self {
            Self::Count(value) => *value = value.checked_add(1).ok_or_else(overflow)?,
            Self::Sum(total) => {
                let Input::Scalar(Some(CanonicalScalar::Int(value))) = input else {
                    return Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate }));
                };
                *total = Some(total.unwrap_or(0).checked_add(i128::from(*value)).ok_or_else(overflow)?);
            }
            Self::Average { sum, count } => {
                let Input::Scalar(Some(CanonicalScalar::Int(value))) = input else {
                    return Err(GqlQueryError::Source(GraphAggregateError::NonIntegerAverage { aggregate }));
                };
                let next_count = count.checked_add(1).ok_or_else(overflow)?;
                let next_sum = sum.checked_add(i128::from(*value)).ok_or_else(overflow)?;
                *sum = next_sum;
                *count = next_count;
            }
            Self::Extreme { .. } | Self::Distinct(_) => {
                unreachable!("value support and ownership require governed updates")
            }
        }
        Ok(())
    }
}

impl<S, F, C> Iterator for VertexAggregateCursor<S, F>
where S: VertexScanSource, F: FnMut() -> Result<(), C>,
{
    type Item = Result<GraphAggregateRow, VertexAggregateError<S::Error, C>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.state != VertexScanState::Open { return None; }
        if self.pending.is_none() {
            let result = self.accumulate();
            // Every retained key is owned. Release the snapshot before delivery.
            self.source = None;
            match result {
                Ok(groups) => self.pending = Some(groups.into_iter()),
                Err(error) => {
                    self.state = VertexScanState::Failed;
                    return Some(Err(error));
                }
            }
        }
        let Some((keys, states)) = self.pending.as_mut().and_then(Iterator::next) else {
            self.state = VertexScanState::Exhausted;
            self.pending = None;
            return None;
        };
        let result = self.deliver(keys, states);
        if result.is_err() {
            self.state = VertexScanState::Failed;
            self.pending = None;
        } else if self.pending.as_ref().is_some_and(|groups| groups.len() == 0) {
            self.state = VertexScanState::Exhausted;
            self.pending = None;
        }
        Some(result)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.state != VertexScanState::Open { return (0, Some(0)); }
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
where S: VertexScanSource, F: FnMut() -> Result<(), C>, {}
impl<S, F> core::fmt::Debug for VertexAggregateCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VertexAggregateCursor").field("state", &self.state)
            .field("rows", &self.meter.rows).field("evaluator", &self.meter.evaluator)
            .field("definition_and_source", &"[REDACTED]").finish()
    }
}

#[cfg(test)]
mod tests;
