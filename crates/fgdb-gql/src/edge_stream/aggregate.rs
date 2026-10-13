//! Governed grouped exact statistics over the existing indexed join cursor.
//!
//! The first pull consumes joined bindings once, retaining group keys and the
//! SAME numeric/DISTINCT cells as vertex aggregation. Collections also retain
//! any complete source-order keys needed to correlate their arguments.
//! Subsequent pulls move completed groups out in canonical result order.
//! HAVING/output expressions are validated on all completed groups before a
//! selected page can escape. Page selection never suppresses input failures.
//! Group and DISTINCT support are metered in-memory state, not spill storage.

use super::*;
use crate::algebra::GraphValue;
use crate::stream::VertexScanEvent;
use crate::stream::aggregate::{Input, NumericState};
use crate::{GraphAggregateError, GraphAggregateRow, PreparedGraphAggregate};
use std::collections::{BTreeMap, btree_map};

mod spill;
pub use spill::{EdgeSpillAggregateCursor, EdgeSpillAggregatePlan};
pub(crate) mod pipeline;
use pipeline::InputPipeline;
mod ordered_collection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeAggregateBuildError {
    RequiresPlainGlobalAggregate,
    Scan(EdgeScanBuildError),
}
impl core::fmt::Display for EdgeAggregateBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RequiresPlainGlobalAggregate => {
                f.write_str("edge aggregate stream requires plain exact aggregates")
            }
            Self::Scan(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for EdgeAggregateBuildError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            _ => None,
        }
    }
}

/// A checked global or grouped COUNT/SUM/AVG/MIN/MAX definition over fixed-edge
/// joins and their admitted existence probes. Argument DISTINCT is per group
/// and per aggregate. Group keys and extrema retain scalar, vertex, edge and
/// path domains; there is no coercion or digest substituted for equality.
///
/// Optional row-local computed columns execute once per complete match before
/// grouping and argument DISTINCT, using the shared projection evaluator.
/// Every declared column executes, even when no aggregate uses it. Constants
/// preserve match multiplicity; null and lazy-branch semantics remain native.
/// Numeric input needs only the current source/projected binding.
///
/// HAVING, hidden/repeated output columns, output expressions, exact ORDER BY
/// and SKIP/LIMIT are supported. Finite ordered pages retain at most SKIP+LIMIT
/// completed candidates; full ordering retains at most the completed groups.
/// Output DISTINCT retains only the best SKIP+LIMIT projected classes and
/// their best-ranked complete representatives. Both rank and class support
/// obey this prefix bound; upstream group accumulation is separately resident.
/// COLLECT and COLLECT DISTINCT preserve the ordinary edge row-stream order
/// proof on the complete child: root edge identity, root source identity when
/// orientation requires it, then each joined edge identity. Row-local input
/// expressions run after this ordered child.
///
/// Plain identified inputs also admit collections of one shared argument when
/// every preceding input column is a grouping key. Batch execution sorts the
/// complete child canonically before aggregation. Within each group that
/// prefix is constant, so sorting the retained collection values reproduces
/// the child's order; later columns distinguish only equal collected values.
/// This in-place finalization shares the source's cumulative work allowance
/// and cancellation checks. It retains no extra input or ordering table.
/// Other plain identified collection inputs retain one complete canonical
/// order key per contributing occurrence, shared across their collection
/// arguments. Their lists are populated in that key order, not by sorting
/// each argument independently. This resident support is work/scratch governed
/// and can grow with matched occurrences even under a small result LIMIT.
/// Single-source WITH chains of ALL column/constant/scalar-VM projections,
/// scopes and filters execute row by row before numeric grouping. Ordinal-
/// dependent or additional graph-reading expressions remain outside this path.
/// Intermediate DISTINCT, ORDER BY, pages,
/// expansion, multi-source operators and COLLECT consumers are not admitted:
/// their positional or support semantics cannot be erased by this fusion.
/// Numeric-only and already ordered paths collect no intermediate match
/// relation. Collection payloads and any required source-order support remain
/// metered resident state, not spilled.
/// Every child operator and
/// column is checked before opening the source; a failed plan is never retried
/// as another source or an eager query. The ordinary row stream's identity
/// prefix remains mandatory there, but is not required for private group input.
#[derive(Clone)]
pub struct EdgeAggregatePlan {
    input: EdgeScanPlan,
    aggregate: PreparedGraphAggregate,
    canonical_collections: bool,
    ordered_collections: Option<ordered_collection::Layout>,
    pipeline: Option<InputPipeline>,
}
impl EdgeAggregatePlan {
    pub fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, EdgeAggregateBuildError> {
        if !aggregate.aggregates().iter().all(|spec| {
            NumericState::supports(spec.function()) || NumericState::collects(spec.function())
        }) {
            return Err(EdgeAggregateBuildError::RequiresPlainGlobalAggregate);
        }
        let collects = aggregate
            .aggregates()
            .iter()
            .any(|spec| NumericState::collects(spec.function()));
        let (aggregate, pipeline) = InputPipeline::prepare(aggregate)
            .ok_or(EdgeAggregateBuildError::RequiresPlainGlobalAggregate)?;
        let mut ordered_collections = None;
        let canonical_collections = if collects {
            // Preserve the complete existing source-order proof and its
            // finalization path. A shared-argument proof needs only values;
            // other canonical identified inputs retain complete order keys.
            match EdgeScanPlan::compile(aggregate.input_pattern().plan()) {
                Ok(_) => false,
                Err(_) if canonical_collection_order_proven(&aggregate) => true,
                Err(error) => {
                    ordered_collections = ordered_collection::Layout::prepare(&aggregate);
                    if ordered_collections.is_none() {
                        return Err(EdgeAggregateBuildError::Scan(error));
                    }
                    false
                }
            }
        } else {
            false
        };
        let input = join::compile_aggregate(aggregate.input_pattern().plan())
            .map_err(EdgeAggregateBuildError::Scan)?;
        Ok(Self {
            input,
            aggregate,
            canonical_collections,
            ordered_collections,
            pipeline,
        })
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.aggregate.aggregate_columns()
    }
    #[must_use]
    pub fn key_columns(&self) -> &[String] {
        self.aggregate.key_columns()
    }
}

fn canonical_collection_order_proven(aggregate: &PreparedGraphAggregate) -> bool {
    if aggregate.input_projection().is_some()
        || !aggregate.input_pattern().plan().requires_identified_edges()
    {
        return false;
    }
    let mut collections = aggregate
        .aggregates()
        .iter()
        .filter(|spec| NumericState::collects(spec.function()));
    let Some(column) = collections.next().and_then(|spec| spec.argument_column()) else {
        return false;
    };
    // Group equality fixes every earlier column in the child's canonical
    // lexicographic order. All collections then share this one argument's
    // value order, including DISTINCT's canonical equality representatives.
    // The join compiler still validates every child operator, projection and
    // unpaginated ALL tail; this proof cannot discard an input modifier.
    collections.all(|spec| spec.argument_column() == Some(column))
        && (0..column).all(|prior| aggregate.group_key_columns().contains(&prior))
}

impl core::fmt::Debug for EdgeAggregatePlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("EdgeAggregatePlan([REDACTED])")
    }
}

pub type EdgeAggregateError<E, C> = GqlQueryError<GraphAggregateError<EdgeScanError<E>>, C>;
type Groups = BTreeMap<Vec<GraphValue>, Group>;
type PendingGroups = btree_map::IntoIter<Vec<GraphValue>, Group>;

struct Group {
    states: Vec<NumericState>,
    collections: ordered_collection::State,
}

struct Accumulation {
    groups: Groups,
    largest_key: usize,
}

/// Global empty input yields one zero/null row; grouped empty input yields none.
/// Every match/probe/source examination and group-state operation shares the
/// original cumulative policy. ResultRows bounds complete groups, not matched
/// occurrences. With output clauses, only selected qualified groups count
/// against ResultRows, not groups rejected by HAVING or the result window.
/// Raw group storage remains subject to work/scratch admission. Source/data,
/// HAVING, output-expression and selected-result-count failures precede ALL output.
/// Delivery work/cancellation can fail after earlier complete groups; one error
/// fuses the cursor. Close/drop frees the pin and pending groups without demand.
///
/// Retained state grows with groups and DISTINCT support, plus chosen extrema,
/// ordered collection payloads and the join's bounded traversal state. List
/// elements spend work/scratch, not the completed-group result-row allowance.
/// One projected binding is transient.
/// Logical payload allowances are not allocator-byte or source-residency caps;
/// this does not add spill, factorized counting or durable cursor resumption.
pub struct EdgeAggregateCursor<S, F> {
    input: EdgeScanCursor<S, F>,
    aggregate: PreparedGraphAggregate,
    canonical_collections: bool,
    ordered_collections: Option<ordered_collection::Layout>,
    pipeline: Option<InputPipeline>,
    state: EdgeScanState,
    pending: Option<PendingGroups>,
    completed: Option<std::vec::IntoIter<GraphAggregateRow>>,
    accumulating: Option<Accumulation>,
}
impl<S: EdgeScanSource, F> EdgeAggregateCursor<S, F> {
    pub fn new(source: S, plan: EdgeAggregatePlan, policy: GqlQueryPolicy, checkpoint: F) -> Self {
        Self {
            input: EdgeScanCursor::new(source, plan.input, policy, checkpoint),
            aggregate: plan.aggregate,
            canonical_collections: plan.canonical_collections,
            ordered_collections: plan.ordered_collections,
            pipeline: plan.pipeline,
            state: EdgeScanState::Open,
            pending: None,
            completed: None,
            accumulating: None,
        }
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.aggregate.aggregate_columns()
    }
    #[must_use]
    pub fn key_columns(&self) -> &[String] {
        self.aggregate.key_columns()
    }
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.input.snapshot_seq()
    }
    #[must_use]
    pub fn state(&self) -> EdgeScanState {
        self.state
    }
    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats {
        self.input.row_stats()
    }
    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.input.evaluator_stats()
    }
    pub fn close(&mut self) {
        if self.state == EdgeScanState::Open {
            self.state = EdgeScanState::Closed;
        }
        self.input.close();
        self.pending = None;
        self.completed = None;
        self.accumulating = None;
    }

    fn accumulate<C>(
        &mut self,
        remaining: &mut usize,
    ) -> Result<Option<Groups>, EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        let global = self.aggregate.group_key_columns().is_empty();
        if self.accumulating.is_none() {
            self.input
                .meter
                .event(GlaExecutionEvent::Work)
                .map_err(lift)?;
            let mut groups = Groups::new();
            if global {
                // Preserve global zero-budget refusal before driving any
                // source, once across every cooperative suspension.
                if !self.aggregate.has_streamed_output_stage() {
                    self.input
                        .meter
                        .increment(GqlBudgetDimension::ResultRows, 0)
                        .map_err(lift)?;
                }
                self.input
                    .meter
                    .event(GlaExecutionEvent::ScratchEntry)
                    .map_err(lift)?;
                groups.insert(
                    Vec::new(),
                    Group {
                        states: states(&self.aggregate, &mut self.input.meter)?,
                        collections: ordered_collection::State::default(),
                    },
                );
            }
            self.accumulating = Some(Accumulation {
                groups,
                largest_key: 0,
            });
        }
        let Accumulation {
            groups,
            largest_key,
        } = self
            .accumulating
            .as_mut()
            .expect("initialized accumulation");
        loop {
            let row = match self.input.advance_bounded(remaining).map_err(lift)? {
                core::task::Poll::Ready(Some(row)) => row,
                core::task::Poll::Ready(None) => break,
                core::task::Poll::Pending => return Ok(None),
            };
            let meter = &mut self.input.meter;
            // The graph compiler still owns source matching/projection. This
            // single-row transformation completes before any group is changed;
            // a refusal discards accumulation and releases no partial summary.
            let row = if let Some(pipeline) = &self.pipeline {
                let Some(row) =
                    pipeline.evaluate(row, &mut |event| meter.event(event).map_err(lift))?
                else {
                    // A rejected binding is not source exhaustion. Continue
                    // within the same cooperative quantum and shared meter.
                    continue;
                };
                row
            } else {
                self.aggregate
                    .evaluate_streamed_input(row, &mut |event| meter.event(event).map_err(lift))?
            };
            let state = if global {
                groups
                    .get_mut(&Vec::<GraphValue>::new())
                    .expect("global group installed")
            } else {
                meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
                let mut key = Vec::new();
                let mut units = 0_usize;
                for &column in self.aggregate.group_key_columns() {
                    meter.event(GlaExecutionEvent::Work).map_err(lift)?;
                    let value = &row.values()[column];
                    units = units
                        .saturating_add(value.payload_units())
                        .saturating_add(1);
                    key.push(
                        value.copy_with_control(&mut |event| meter.event(event).map_err(lift))?,
                    );
                }
                *largest_key = (*largest_key).max(units);
                // Logical B-tree lookup/insertion reservation, including every
                // variable-sized key. No exact allocator/comparison-count claim.
                let levels = groups.len().saturating_add(1).ilog2() as usize + 1;
                for _ in 0..levels
                    .saturating_mul(24)
                    .saturating_mul(largest_key.saturating_add(1))
                {
                    meter.event(GlaExecutionEvent::Work).map_err(lift)?;
                }
                let next = groups
                    .len()
                    .checked_add(1)
                    .and_then(|n| u64::try_from(n).ok())
                    .ok_or(GqlQueryError::Source(
                        GraphAggregateError::ResultCountOverflow,
                    ))?;
                match groups.entry(key) {
                    btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    btree_map::Entry::Vacant(entry) => {
                        if !self.aggregate.has_streamed_output_stage() {
                            meter
                                .policy
                                .rows
                                .check(GqlBudgetDimension::ResultRows, next)
                                .map_err(GqlQueryError::Rows)?;
                        }
                        meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
                        entry.insert(Group {
                            states: states(&self.aggregate, meter)?,
                            collections: ordered_collection::State::default(),
                        })
                    }
                }
            };
            for (at, (spec, cell)) in self
                .aggregate
                .aggregates()
                .iter()
                .zip(&mut state.states)
                .enumerate()
            {
                meter.event(GlaExecutionEvent::Work).map_err(lift)?;
                if self.ordered_collections.is_some() && NumericState::collects(spec.function()) {
                    continue;
                }
                let value = spec.argument_column().map_or(Input::Identity, |column| {
                    Input::from_value(&row.values()[column])
                });
                cell.update_governed(value, at, &mut |event| {
                    meter.event(value_event(event)).map_err(lift)
                })?;
            }
            if let Some(layout) = &self.ordered_collections {
                state
                    .collections
                    .push(layout, row, &mut |event| meter.event(event).map_err(lift))?;
            }
        }
        self.input
            .meter
            .event(GlaExecutionEvent::Work)
            .map_err(lift)?;
        Ok(Some(
            self.accumulating
                .take()
                .expect("completed accumulation")
                .groups,
        ))
    }

    /// Drive at most 64 root/incidence examinations or join transitions before
    /// yielding Pending. No partial aggregate or private matched row escapes.
    /// Empty matches and hidden candidates still yield; indexed positions and
    /// native group/DISTINCT/COLLECT state and any required collection-order
    /// keys are retained. Numeric input never becomes an intermediate bag.
    /// Single source callbacks, probes and final group selection are synchronous
    /// governed sections; the quantum is not a wall-clock duration guarantee.
    pub fn poll_next<C>(
        &mut self,
        context: &mut core::task::Context<'_>,
    ) -> crate::stream::aggregate::AggregatePoll<EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        if self.state != EdgeScanState::Open {
            return core::task::Poll::Ready(None);
        }
        if self.pending.is_none() && self.completed.is_none() {
            let mut remaining = 64;
            match self.accumulate(&mut remaining) {
                Ok(None) => {
                    context.waker().wake_by_ref();
                    return core::task::Poll::Pending;
                }
                Ok(Some(groups)) => {
                    self.input.close();
                    if self.aggregate.has_streamed_output_stage() {
                        match self.select_output(groups) {
                            Ok(rows) => self.completed = Some(rows.into_iter()),
                            Err(error) => {
                                self.state = EdgeScanState::Failed;
                                self.close();
                                return core::task::Poll::Ready(Some(Err(error)));
                            }
                        }
                    } else {
                        self.pending = Some(groups.into_iter());
                    }
                }
                Err(error) => {
                    self.state = EdgeScanState::Failed;
                    self.close();
                    return core::task::Poll::Ready(Some(Err(error)));
                }
            }
        }
        core::task::Poll::Ready(self.next())
    }

    fn finalize<C>(
        &mut self,
        keys: Vec<GraphValue>,
        group: Group,
    ) -> Result<GraphAggregateRow, EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        let meter = &mut self.input.meter;
        let Group {
            mut states,
            collections,
        } = group;
        if let Some(layout) = &self.ordered_collections {
            collections.finish(layout, &mut states, &mut |event| {
                meter.event(event).map_err(lift)
            })?;
        }
        let mut values = Vec::new();
        for state in states {
            meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
            // Keep the existing exact COUNT/SUM fast path; other functions
            // share the vertex reducer's governed ownership/normalization.
            let value = match state {
                basic @ (NumericState::Count(_) | NumericState::Sum(_)) => basic.finish(),
                other if self.canonical_collections => {
                    other.finish_canonical_collection_governed(&mut |event| {
                        meter.event(value_event(event)).map_err(lift)
                    })?
                }
                other => other
                    .finish_governed(&mut |event| meter.event(value_event(event)).map_err(lift))?,
            };
            values.push(value);
        }
        Ok(GraphAggregateRow::from_group_values(keys, values))
    }

    // Move the raw group map; finalize each state once and release it. Retain
    // at most the requested number of selected rows, not skipped/off-page rows
    // or a second matched-input bag. Even a full page cannot hide an error in
    // a later qualified output expression or HAVING comparison.
    fn select_output<C>(
        &mut self,
        groups: Groups,
    ) -> Result<Vec<GraphAggregateRow>, EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        if self.aggregate.incremental_output_is_distinct() || self.aggregate.has_streamed_ordering()
        {
            return self.select_ordered_output(groups);
        }
        let (offset, count) = self.aggregate.incremental_result_window();
        let mut skipped = 0_u64;
        let mut selected = Vec::new();
        for (keys, states) in groups {
            let row = self.finalize(keys, states)?;
            let meter = &mut self.input.meter;
            let Some(row) = self
                .aggregate
                .evaluate_streamed_output(row, &mut |event| meter.event(event).map_err(lift))?
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
            let next = meter
                .increment(GqlBudgetDimension::ResultRows, selected.len() as u64)
                .map_err(lift)?;
            meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
            selected.push(row);
            debug_assert_eq!(selected.len() as u64, next);
        }
        self.input
            .meter
            .event(GlaExecutionEvent::Work)
            .map_err(lift)?;
        Ok(selected)
    }

    fn select_ordered_output<C>(
        &mut self,
        groups: Groups,
    ) -> Result<Vec<GraphAggregateRow>, EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        let mut ranking = self.aggregate.streamed_group_ranking(groups.len());
        for (keys, states) in groups {
            let row = self.finalize(keys, states)?;
            let meter = &mut self.input.meter;
            ranking.push(&self.aggregate, row, &mut |event| {
                meter.event(event).map_err(lift)
            })?;
        }
        let meter = &mut self.input.meter;
        let selected = ranking.finish(&self.aggregate, &mut |event| {
            meter.event(event).map_err(lift)
        })?;
        // The rank prefix includes skipped groups; those must not consume the
        // output allowance. Validate the whole selected page before delivery.
        let count = u64::try_from(selected.len())
            .map_err(|_| GqlQueryError::Source(GraphAggregateError::ResultCountOverflow))?;
        meter
            .policy
            .rows
            .check(GqlBudgetDimension::ResultRows, count)
            .map_err(GqlQueryError::Rows)?;
        Ok(selected)
    }

    fn deliver<C>(
        &mut self,
        row: GraphAggregateRow,
    ) -> Result<GraphAggregateRow, EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        let meter = &mut self.input.meter;
        let next = meter
            .increment(GqlBudgetDimension::ResultRows, meter.rows.result_rows)
            .map_err(lift)?;
        meter.event(GlaExecutionEvent::ResultRow).map_err(lift)?;
        meter.rows.result_rows = next;
        Ok(row)
    }
}
fn states<F, E, C>(
    definition: &PreparedGraphAggregate,
    meter: &mut Meter<F>,
) -> Result<Vec<NumericState>, EdgeAggregateError<E, C>>
where
    F: FnMut() -> Result<(), C>,
{
    meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
    let mut states = Vec::new();
    for spec in definition.aggregates() {
        meter.event(GlaExecutionEvent::ScratchEntry).map_err(lift)?;
        states.push(NumericState::new_governed(spec.function(), &mut |event| {
            meter.event(value_event(event)).map_err(lift)
        })?);
    }
    Ok(states)
}
pub(super) fn value_event(event: VertexScanEvent) -> GlaExecutionEvent {
    match event {
        VertexScanEvent::Work => GlaExecutionEvent::Work,
        VertexScanEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
    }
}
pub(super) fn lift<E, C>(error: GqlQueryError<EdgeScanError<E>, C>) -> EdgeAggregateError<E, C> {
    error.map_source(GraphAggregateError::Source)
}
impl<S: EdgeScanSource, F: FnMut() -> Result<(), C>, C> Iterator for EdgeAggregateCursor<S, F> {
    type Item = Result<GraphAggregateRow, EdgeAggregateError<S::Error, C>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.state != EdgeScanState::Open {
            return None;
        }
        if self.pending.is_none() && self.completed.is_none() {
            let result = loop {
                let mut remaining = usize::MAX;
                match self.accumulate(&mut remaining) {
                    Ok(Some(groups)) => break Ok(groups),
                    Ok(None) => {}
                    Err(error) => break Err(error),
                }
            };
            // All retained keys/witnesses/extrema are owned; source release is
            // independent of whether any output has yet been requested.
            self.input.close();
            match result {
                Ok(groups) if self.aggregate.has_streamed_output_stage() => {
                    match self.select_output(groups) {
                        Ok(rows) => self.completed = Some(rows.into_iter()),
                        Err(error) => {
                            self.state = EdgeScanState::Failed;
                            return Some(Err(error));
                        }
                    }
                }
                Ok(groups) => self.pending = Some(groups.into_iter()),
                Err(error) => {
                    self.state = EdgeScanState::Failed;
                    self.accumulating = None;
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
            self.state = EdgeScanState::Exhausted;
            self.pending = None;
            self.completed = None;
            return None;
        };
        let result = row.and_then(|row| self.deliver(row));
        if result.is_err() {
            self.state = EdgeScanState::Failed;
            self.pending = None;
            self.completed = None;
        } else if self
            .pending
            .as_ref()
            .is_some_and(|groups| groups.len() == 0)
            || self.completed.as_ref().is_some_and(|rows| rows.len() == 0)
        {
            self.state = EdgeScanState::Exhausted;
            self.pending = None;
            self.completed = None;
        }
        Some(result)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.state != EdgeScanState::Open {
            return (0, Some(0));
        }
        if let Some(rows) = &self.completed {
            return (0, Some(rows.len()));
        }
        match &self.pending {
            Some(groups) => (0, Some(groups.len())),
            None if self.aggregate.group_key_columns().is_empty() => (0, Some(1)),
            None => (0, None), // Even empty input may produce one source error.
        }
    }
}
impl<S: EdgeScanSource, F: FnMut() -> Result<(), C>, C> std::iter::FusedIterator
    for EdgeAggregateCursor<S, F>
{
}
impl<S, F> core::fmt::Debug for EdgeAggregateCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdgeAggregateCursor")
            .field("input", &self.input)
            .field("state", &self.state)
            .field("definition_and_groups", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod collection_tests;
