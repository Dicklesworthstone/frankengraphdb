//! Governed grouped exact statistics over the existing indexed join cursor.
//!
//! The first pull consumes joined bindings once, retaining group keys and the
//! SAME numeric/DISTINCT cells as vertex aggregation, not an input or result
//! table. Subsequent pulls move completed groups out in canonical key order.
//! Group and DISTINCT support are metered in-memory state, not spill storage.

use super::*;
use crate::algebra::GraphValue;
use crate::stream::aggregate::{Input, NumericState};
use crate::stream::VertexScanEvent;
use crate::{GraphAggregateError, GraphAggregateRow, PreparedGraphAggregate};
use std::collections::{BTreeMap, btree_map};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeAggregateBuildError {
    RequiresPlainGlobalAggregate,
    Scan(EdgeScanBuildError),
}
impl core::fmt::Display for EdgeAggregateBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RequiresPlainGlobalAggregate => f.write_str("edge aggregate stream requires plain exact aggregates"),
            Self::Scan(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for EdgeAggregateBuildError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self { Self::Scan(error) => Some(error), _ => None }
    }
}

/// A checked global or grouped COUNT/SUM/AVG/MIN/MAX definition over fixed-edge
/// joins and their admitted existence probes. Argument DISTINCT is per group
/// and per aggregate. Group keys and extrema retain scalar, vertex, edge and
/// path domains; there is no coercion or digest substituted for equality.
///
/// Computed/relational input, output DISTINCT, HAVING, result ordering/pages
/// and COLLECT remain outside this physical profile. Every child operator and
/// column is checked before opening the source; a failed plan is never retried
/// as another source or an eager query. The ordinary row stream's identity
/// prefix remains mandatory there, but is not required for private group input.
#[derive(Clone)]
pub struct EdgeAggregatePlan {
    input: EdgeScanPlan,
    aggregate: PreparedGraphAggregate,
}
impl EdgeAggregatePlan {
    pub fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, EdgeAggregateBuildError> {
        if !aggregate.supports_incremental_maintenance()
            || !aggregate.aggregates().iter().all(|spec| NumericState::supports(spec.function())) {
            return Err(EdgeAggregateBuildError::RequiresPlainGlobalAggregate);
        }
        let input = join::compile_aggregate(aggregate.input_pattern().plan())
            .map_err(EdgeAggregateBuildError::Scan)?;
        Ok(Self { input, aggregate: aggregate.clone() })
    }
    #[must_use]
    pub fn columns(&self) -> &[String] { self.aggregate.aggregate_columns() }
    #[must_use]
    pub fn key_columns(&self) -> &[String] { self.aggregate.key_columns() }
}
impl core::fmt::Debug for EdgeAggregatePlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("EdgeAggregatePlan([REDACTED])")
    }
}

pub type EdgeAggregateError<E, C> = GqlQueryError<GraphAggregateError<EdgeScanError<E>>, C>;
type Groups = BTreeMap<Vec<GraphValue>, Vec<NumericState>>;
type PendingGroups = btree_map::IntoIter<Vec<GraphValue>, Vec<NumericState>>;

/// Global empty input yields one zero/null row; grouped empty input yields none.
/// Every match/probe/source examination and group-state operation shares the
/// original cumulative policy. ResultRows bounds complete groups, not matched
/// occurrences. Group-count and source/data failures precede ALL output.
/// Delivery work/cancellation can fail after earlier complete groups; one error
/// fuses the cursor. Close/drop frees the pin and pending groups without demand.
///
/// Retained state grows with groups and DISTINCT support, plus chosen extrema
/// and the join's bounded traversal state. One projected binding is transient.
/// Logical payload allowances are not allocator-byte or source-residency caps;
/// this does not add spill, factorized counting or durable cursor resumption.
pub struct EdgeAggregateCursor<S, F> {
    input: EdgeScanCursor<S, F>,
    aggregate: PreparedGraphAggregate,
    state: EdgeScanState,
    pending: Option<PendingGroups>,
}
impl<S: EdgeScanSource, F> EdgeAggregateCursor<S, F> {
    pub fn new(source: S, plan: EdgeAggregatePlan, policy: GqlQueryPolicy, checkpoint: F) -> Self {
        Self { input: EdgeScanCursor::new(source, plan.input, policy, checkpoint),
            aggregate: plan.aggregate, state: EdgeScanState::Open, pending: None }
    }
    #[must_use]
    pub fn columns(&self) -> &[String] { self.aggregate.aggregate_columns() }
    #[must_use]
    pub fn key_columns(&self) -> &[String] { self.aggregate.key_columns() }
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq { self.input.snapshot_seq() }
    #[must_use]
    pub fn state(&self) -> EdgeScanState { self.state }
    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats { self.input.row_stats() }
    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats { self.input.evaluator_stats() }
    pub fn close(&mut self) {
        if self.state == EdgeScanState::Open { self.state = EdgeScanState::Closed; }
        self.input.close();
        self.pending = None;
    }

    fn accumulate<C>(&mut self) -> Result<Groups, EdgeAggregateError<S::Error, C>>
    where F: FnMut() -> Result<(), C> {
        self.input.meter.event(GlaExecutionEvent::Work).map_err(lift)?;
        let mut groups = Groups::new();
        let global = self.aggregate.group_key_columns().is_empty();
        if global {
            // Preserve global zero-budget refusal before driving any source.
            self.input.meter.increment(GqlBudgetDimension::ResultRows, 0).map_err(lift)?;
            self.input.meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
            groups.insert(Vec::new(), states(&self.aggregate, &mut self.input.meter)?);
        }
        let mut largest_key = 0_usize;
        while let Some(row) = self.input.advance().map_err(lift)? {
            let meter = &mut self.input.meter;
            let state = if global {
                groups.get_mut(&Vec::<GraphValue>::new()).expect("global group installed")
            } else {
                meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
                let mut key = Vec::new();
                let mut units = 0_usize;
                for &column in self.aggregate.group_key_columns() {
                    meter.event(GlaExecutionEvent::Work).map_err(lift)?;
                    let value = &row.values()[column];
                    units = units.saturating_add(value.payload_units()).saturating_add(1);
                    key.push(value.copy_with_control(&mut |event| meter.event(event).map_err(lift))?);
                }
                largest_key = largest_key.max(units);
                // Logical B-tree lookup/insertion reservation, including every
                // variable-sized key. No exact allocator/comparison-count claim.
                let levels = groups.len().saturating_add(1).ilog2() as usize + 1;
                for _ in 0..levels.saturating_mul(24).saturating_mul(largest_key.saturating_add(1)) {
                    meter.event(GlaExecutionEvent::Work).map_err(lift)?;
                }
                let next = groups.len().checked_add(1).and_then(|n| u64::try_from(n).ok())
                    .ok_or(GqlQueryError::Source(GraphAggregateError::ResultCountOverflow))?;
                match groups.entry(key) {
                    btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    btree_map::Entry::Vacant(entry) => {
                        meter.policy.rows.check(GqlBudgetDimension::ResultRows, next)
                            .map_err(GqlQueryError::Rows)?;
                        meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
                        entry.insert(states(&self.aggregate, meter)?)
                    }
                }
            };
            for (at, (spec, state)) in self.aggregate.aggregates().iter().zip(state).enumerate() {
                meter.event(GlaExecutionEvent::Work).map_err(lift)?;
                let value = spec.argument_column()
                    .map_or(Input::Identity, |column| Input::from_value(&row.values()[column]));
                state.update_governed(value, at, &mut |event| meter.event(value_event(event)).map_err(lift))?;
            }
        }
        self.input.meter.event(GlaExecutionEvent::Work).map_err(lift)?;
        Ok(groups)
    }

    fn deliver<C>(&mut self, keys: Vec<GraphValue>, states: Vec<NumericState>)
        -> Result<GraphAggregateRow, EdgeAggregateError<S::Error, C>>
    where F: FnMut() -> Result<(), C> {
        let meter = &mut self.input.meter;
        let mut values = Vec::new();
        for state in states {
            meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
            // Keep the existing exact COUNT/SUM fast path; other functions
            // share the vertex reducer's governed ownership/normalization.
            let value = match state {
                basic @ (NumericState::Count(_) | NumericState::Sum(_)) => basic.finish(),
                other => other.finish_governed(&mut |event| meter.event(value_event(event)).map_err(lift))?,
            };
            values.push(value);
        }
        let row = GraphAggregateRow::from_group_values(keys, values);
        let next = meter.increment(GqlBudgetDimension::ResultRows, meter.rows.result_rows).map_err(lift)?;
        meter.event(GlaExecutionEvent::ResultRow).map_err(lift)?;
        meter.rows.result_rows = next;
        Ok(row)
    }
}
fn states<F, E, C>(definition: &PreparedGraphAggregate, meter: &mut Meter<F>)
    -> Result<Vec<NumericState>, EdgeAggregateError<E, C>>
where F: FnMut() -> Result<(), C> {
    meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
    let mut states = Vec::new();
    for spec in definition.aggregates() {
        meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
        states.push(NumericState::new_governed(spec.function(), &mut |event| meter.event(value_event(event)).map_err(lift))?);
    }
    Ok(states)
}
fn value_event(event: VertexScanEvent) -> GlaExecutionEvent {
    match event {
        VertexScanEvent::Work => GlaExecutionEvent::Work,
        VertexScanEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
    }
}
fn lift<E, C>(error: GqlQueryError<EdgeScanError<E>, C>) -> EdgeAggregateError<E, C> {
    error.map_source(GraphAggregateError::Source)
}
impl<S: EdgeScanSource, F: FnMut() -> Result<(), C>, C> Iterator for EdgeAggregateCursor<S, F> {
    type Item = Result<GraphAggregateRow, EdgeAggregateError<S::Error, C>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.state != EdgeScanState::Open { return None; }
        if self.pending.is_none() {
            let result = self.accumulate();
            // All retained keys/witnesses/extrema are owned; source release is
            // independent of whether any output has yet been requested.
            self.input.close();
            match result {
                Ok(groups) => self.pending = Some(groups.into_iter()),
                Err(error) => { self.state = EdgeScanState::Failed; return Some(Err(error)); }
            }
        }
        let Some((keys, states)) = self.pending.as_mut().and_then(Iterator::next) else {
            self.state = EdgeScanState::Exhausted;
            self.pending = None;
            return None;
        };
        let result = self.deliver(keys, states);
        if result.is_err() {
            self.state = EdgeScanState::Failed;
            self.pending = None;
        } else if self.pending.as_ref().is_some_and(|groups| groups.len() == 0) {
            self.state = EdgeScanState::Exhausted;
            self.pending = None;
        }
        Some(result)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.state != EdgeScanState::Open { return (0, Some(0)); }
        match &self.pending {
            Some(groups) => (0, Some(groups.len())),
            None if self.aggregate.group_key_columns().is_empty() => (0, Some(1)),
            None => (0, None), // Even empty input may produce one source error.
        }
    }
}
impl<S: EdgeScanSource, F: FnMut() -> Result<(), C>, C> std::iter::FusedIterator for EdgeAggregateCursor<S, F> {}
impl<S, F> core::fmt::Debug for EdgeAggregateCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdgeAggregateCursor").field("input", &self.input)
            .field("state", &self.state).field("definition_and_groups", &"[REDACTED]").finish()
    }
}

#[cfg(test)]
mod tests;
