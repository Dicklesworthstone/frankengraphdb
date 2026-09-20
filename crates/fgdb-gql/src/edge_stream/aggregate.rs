//! Exact global statistics over the existing pull-driven identified joins.
//!
//! This is not eager execution followed by a one-row cursor. Indexed source
//! candidates and joined bindings are consumed on demand through the SAME
//! join stages, predicates, probes and value projection as ordinary streaming.
//! One transient binding, numeric cells, selected extrema and (when requested)
//! canonical DISTINCT support are retained alongside bounded join/probe state.
//! No input bag, global sort or full edge-table admission is constructed.

use super::*;
use crate::stream::aggregate::{Input, NumericState};
use crate::stream::VertexScanEvent;
use crate::{GraphAggregateError, GraphAggregateRow, PreparedGraphAggregate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeAggregateBuildError {
    RequiresPlainGlobalAggregate,
    Scan(EdgeScanBuildError),
}
impl core::fmt::Display for EdgeAggregateBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RequiresPlainGlobalAggregate => {
                f.write_str("edge aggregate stream requires plain exact global statistics")
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

/// A checked global COUNT/SUM/AVG/MIN/MAX definition over the
/// fixed-edge streaming profile, including its admitted EXISTS/NOT EXISTS
/// probes. Aggregate input order is not observable, so the leading edge/vertex
/// output prefix is unnecessary. All column operands are still validated.
///
/// Argument DISTINCT is supported for COUNT, SUM and AVG. Grouping, output
/// DISTINCT, input/output pages, HAVING, computed/relational stages
/// and unsupported graph operators refuse before source access. No row-stream
/// order rule is relaxed publicly. COUNT preserves occurrence multiplicity;
/// missing/null values do not contribute, and noninteger SUM/AVG operands refuse.
/// COUNT DISTINCT and extrema retain native identities, paths and collections
/// without narrowing or digest equality. AVG returns the existing exact fraction.
#[derive(Clone)]
pub struct EdgeAggregatePlan {
    input: EdgeScanPlan,
    aggregate: PreparedGraphAggregate,
}
impl EdgeAggregatePlan {
    pub fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, EdgeAggregateBuildError> {
        if !aggregate.supports_incremental_maintenance()
            || !aggregate.group_key_columns().is_empty()
            || !aggregate
                .aggregates()
                .iter()
                .all(|spec| NumericState::supports(spec.function()))
        {
            return Err(EdgeAggregateBuildError::RequiresPlainGlobalAggregate);
        }
        let input = join::compile_aggregate(aggregate.input_pattern().plan())
            .map_err(EdgeAggregateBuildError::Scan)?;
        Ok(Self {
            input,
            aggregate: aggregate.clone(),
        })
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.aggregate.aggregate_columns()
    }
}
impl core::fmt::Debug for EdgeAggregatePlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("EdgeAggregatePlan([REDACTED])")
    }
}

pub type EdgeAggregateError<E, C> = GqlQueryError<GraphAggregateError<EdgeScanError<E>>, C>;

/// One pull consumes the complete admitted match stream and returns one exact
/// global summary, including zero counts/null statistics for empty input. Joined
/// matches and probe witnesses never debit ResultRows: only the final summary
/// does. Every candidate examination, projection, arithmetic step and final
/// delivery shares the original work/scratch/record allowance.
///
/// A late source, data, budget or cancellation error produces no partial
/// summary, is returned once, and releases the source. Close before polling
/// drives nothing. Input projection can copy one variable-sized payload; this
/// bounds live *row count*, not allocator bytes or decoded source residency.
/// DISTINCT retains unique argument payloads and extrema retain selected values;
/// their copies/comparisons debit the same cumulative work/scratch allowance.
/// It does not add factorized counting, spill, a durable cursor, or transactions.
pub struct EdgeAggregateCursor<S, F> {
    input: EdgeScanCursor<S, F>,
    aggregate: PreparedGraphAggregate,
}
impl<S: EdgeScanSource, F> EdgeAggregateCursor<S, F> {
    pub fn new(source: S, plan: EdgeAggregatePlan, policy: GqlQueryPolicy, checkpoint: F) -> Self {
        Self {
            input: EdgeScanCursor::new(source, plan.input, policy, checkpoint),
            aggregate: plan.aggregate,
        }
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.aggregate.aggregate_columns()
    }
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.input.snapshot_seq()
    }
    #[must_use]
    pub fn state(&self) -> EdgeScanState {
        self.input.state()
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
        self.input.close();
    }

    fn evaluate<C>(&mut self) -> Result<GraphAggregateRow, EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        self.input
            .meter
            .event(GlaExecutionEvent::Work)
            .map_err(lift)?;
        // This profile always has exactly one output, including on empty input.
        // Refuse an impossible output allowance before driving any candidate.
        let result_count = self
            .input
            .meter
            .increment(GqlBudgetDimension::ResultRows, 0)
            .map_err(lift)?;
        self.input
            .meter
            .event(GlaExecutionEvent::ScratchEntry)
            .map_err(lift)?;
        let mut states = Vec::new();
        for spec in self.aggregate.aggregates() {
            self.input
                .meter
                .event(GlaExecutionEvent::ScratchEntry)
                .map_err(lift)?;
            states.push(NumericState::new_governed(spec.function(), &mut |event| {
                self.input.meter.event(value_event(event)).map_err(lift)
            })?);
        }
        while let Some(row) = self.input.advance().map_err(lift)? {
            for (at, (spec, state)) in self
                .aggregate
                .aggregates()
                .iter()
                .zip(&mut states)
                .enumerate()
            {
                self.input
                    .meter
                    .event(GlaExecutionEvent::Work)
                    .map_err(lift)?;
                let value = match spec.argument_column() {
                    None => Input::Identity,
                    Some(column) => Input::from_value(&row.values()[column]),
                };
                state.update_governed(value, at, &mut |event| {
                    self.input.meter.event(value_event(event)).map_err(lift)
                })?;
            }
        }
        let mut values = Vec::new();
        for state in states {
            self.input
                .meter
                .event(GlaExecutionEvent::ScratchEntry)
                .map_err(lift)?;
            // Preserve the established COUNT/SUM finalizer; other cells share
            // the governed exact finalization used by vertex aggregates.
            values.push(match state {
                state @ (NumericState::Count(_) | NumericState::Sum(_)) => state.finish(),
                state => state.finish_governed(&mut |event| {
                    self.input.meter.event(value_event(event)).map_err(lift)
                })?,
            });
        }
        // The shared cells fix the exact domains. Empty numeric aggregates
        // remain NULL even if the declared argument is a nonnumeric identity.
        let row = GraphAggregateRow::from_global_values(values);
        self.input
            .meter
            .event(GlaExecutionEvent::ResultRow)
            .map_err(lift)?;
        self.input.meter.rows.result_rows = result_count;
        Ok(row)
    }
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
        if self.input.state != EdgeScanState::Open {
            return None;
        }
        let result = self.evaluate();
        self.input.state = if result.is_ok() {
            EdgeScanState::Exhausted
        } else {
            EdgeScanState::Failed
        };
        self.input.close();
        Some(result)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (
            0,
            Some(usize::from(self.input.state == EdgeScanState::Open)),
        )
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
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
