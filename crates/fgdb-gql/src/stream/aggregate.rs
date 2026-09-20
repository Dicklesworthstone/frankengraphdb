//! Constant-state global COUNT/SUM over an admitted vertex source.
//!
//! The checked input reuses the ordinary vertex GLA compiler and predicate /
//! probe executor. Its order is not exposed: only order-independent, exact
//! global COUNT(*) / COUNT(value) / SUM(Int64) definitions are admitted. It
//! retains one numeric cell per aggregate, never a projected input bag or a
//! property payload. This bounds execution state, not source residency.

use super::*;
use crate::algebra::{GraphValue, GraphValueRow, ValueProjection};
use crate::{
    GraphAggregateError, GraphAggregateFunction, GraphAggregateRow, GraphAggregateValue,
    PreparedGraphAggregate,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VertexAggregateBuildError {
    RequiresPlainGlobalCountOrSum,
    Scan(VertexScanBuildError),
}
impl core::fmt::Display for VertexAggregateBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RequiresPlainGlobalCountOrSum => {
                f.write_str("vertex aggregate stream requires plain global COUNT or SUM")
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

/// An immutable physical specialization, not a second query language. Grouped,
/// DISTINCT, computed, relational, HAVING, ordering and paginated definitions
/// refuse before a source is opened. Ordinary row streams retain their stricter
/// leading-identity order requirement; it is relaxed only inside this plan.
#[derive(Clone)]
pub struct VertexAggregatePlan {
    input: VertexScanPlan<GraphValueRow>,
    aggregate: PreparedGraphAggregate,
}
impl VertexAggregatePlan {
    pub fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, VertexAggregateBuildError> {
        if !aggregate.supports_incremental_maintenance()
            || !aggregate.group_key_columns().is_empty()
            || !aggregate.aggregates().iter().all(|spec| {
                matches!(
                    spec.function(),
                    GraphAggregateFunction::CountRows
                        | GraphAggregateFunction::Count
                        | GraphAggregateFunction::SumInt
                )
            })
        {
            return Err(VertexAggregateBuildError::RequiresPlainGlobalCountOrSum);
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
impl core::fmt::Debug for VertexAggregatePlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("VertexAggregatePlan([REDACTED])")
    }
}

pub type VertexAggregateError<E, C> = GqlQueryError<GraphAggregateError<VertexScanError<E>>, C>;

/// One demand produces one completed global aggregate row, including on empty
/// input. No partial aggregate is released. A late source/data/budget/cancel
/// error is emitted once; every terminal path releases the source pin. Close
/// before the first pull does no source work. Input histories, predicates,
/// probes, arithmetic and output all debit the same cumulative meter.
pub struct VertexAggregateCursor<S, F> {
    source: Option<S>,
    plan: VertexAggregatePlan,
    meter: Meter<F>,
    snapshot_seq: CommitSeq,
    state: VertexScanState,
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
                rows: GqlExecutionStats {
                    snapshot_records: 0,
                    result_rows: 0,
                },
                evaluator: GlaExecutionStats::default(),
            },
            state: VertexScanState::Open,
        }
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.plan.columns()
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
    }

    fn evaluate<C>(&mut self) -> Result<GraphAggregateRow, VertexAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        let meter = &mut self.meter;
        meter.event(VertexScanEvent::Work).map_err(lift)?;
        // A plain global aggregate ALWAYS has one output, including empty
        // input. Refuse its allowance before driving or retaining anything.
        let _ = meter.next_result_count().map_err(lift)?;
        meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
        let mut states = Vec::new();
        for spec in self.plan.aggregate.aggregates() {
            meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
            states.push(match spec.function() {
                GraphAggregateFunction::CountRows | GraphAggregateFunction::Count => {
                    NumericState::Count(0)
                }
                GraphAggregateFunction::SumInt => NumericState::Sum(None),
                _ => unreachable!("the immutable physical plan admitted only COUNT/SUM"),
            });
        }
        let GlaOperator::ProjectValues { columns } = self.plan.input.projection.as_ref() else {
            unreachable!("the immutable physical plan admitted value projections");
        };
        let source = self.source.as_mut().expect("open cursor owns its source");
        let mut last = None;
        while !self.plan.input.empty {
            let next = flatten(source.next_vertex(&mut |event| meter.event(event))).map_err(lift)?;
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
            let row = flatten(source.vertex(vid, &mut |event| meter.event(event))).map_err(lift)?;
            let Some(row) = row else {
                continue;
            };
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
            for (aggregate, (spec, state)) in self
                .plan
                .aggregate
                .aggregates()
                .iter()
                .zip(&mut states)
                .enumerate()
            {
                meter.event(VertexScanEvent::Work).map_err(lift)?;
                // Identity is nonnull but never an integer SUM operand. A
                // missing property and canonical null have the same aggregate
                // null behavior; incompatible nonnull SUM inputs must refuse.
                let value = match spec.argument_column() {
                    None => Input::Identity,
                    Some(column) => match &columns[column] {
                        ValueProjection::Vertex { .. } => Input::Identity,
                        ValueProjection::Property { key, .. } => {
                            let found = seek(row.properties, key, |entry| entry.0, &mut |event| {
                                meter.event(event)
                            })
                            .map_err(lift)?;
                            Input::Scalar(found.map(|(_, value)| value))
                        }
                        _ => unreachable!("projection profile was checked before source access"),
                    },
                };
                state.update(value, aggregate)?;
            }
        }
        // Own only the final numeric cells, never source payloads. The plan's
        // existing exact-domain constructor supplies the same public row shape
        // as ordinary and incrementally maintained aggregates.
        let mut values = Vec::new();
        for state in states {
            meter.event(VertexScanEvent::ScratchEntry).map_err(lift)?;
            values.push(match state {
                NumericState::Count(value) => GraphAggregateValue::Count(value),
                NumericState::Sum(Some(value)) => GraphAggregateValue::Integer(value),
                NumericState::Sum(None) => {
                    GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null))
                }
            });
        }
        let row = self
            .plan
            .aggregate
            .incremental_global_row(values)
            .expect("the checked global numeric definition fixes the exact output domains");
        meter.emit().map_err(lift)?;
        Ok(row)
    }
}

fn lift<E, C>(error: GqlQueryError<VertexScanError<E>, C>) -> VertexAggregateError<E, C> {
    error.map_source(GraphAggregateError::Source)
}

enum Input<'a> {
    Identity,
    Scalar(Option<&'a CanonicalScalar>),
}
enum NumericState {
    Count(u64),
    Sum(Option<i128>),
}
impl NumericState {
    fn update<E, C>(
        &mut self,
        input: Input<'_>,
        aggregate: usize,
    ) -> Result<(), VertexAggregateError<E, C>> {
        if matches!(input, Input::Scalar(None | Some(CanonicalScalar::Null))) {
            return Ok(());
        }
        let overflow = || GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate });
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
        let result = self.evaluate();
        self.state = if result.is_ok() {
            VertexScanState::Exhausted
        } else {
            VertexScanState::Failed
        };
        self.source = None;
        Some(result)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(usize::from(self.state == VertexScanState::Open)))
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
