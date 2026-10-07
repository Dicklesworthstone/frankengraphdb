//! Private aggregate occurrences from the existing indexed edge join cursor.

use super::*;
use crate::spill_aggregate::SpillAggregateDefinition;

#[derive(Clone)]
pub struct EdgeSpillAggregatePlan {
    input: EdgeScanPlan,
    definition: SpillAggregateDefinition,
}
impl EdgeSpillAggregatePlan {
    pub(crate) fn compile(
        definition: SpillAggregateDefinition,
    ) -> Result<Self, EdgeScanBuildError> {
        let input = join::compile_aggregate(definition.aggregate.input_pattern().plan())?;
        Ok(Self { input, definition })
    }
    pub fn definition(&self) -> &SpillAggregateDefinition {
        &self.definition
    }
}
impl core::fmt::Debug for EdgeSpillAggregatePlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdgeSpillAggregatePlan")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}

/// The checked join input keeps one cumulative source/reduction/output meter.
/// Source ownership is released at EOF, independently of retained partitions.
pub struct EdgeSpillAggregateCursor<S, F> {
    input: EdgeScanCursor<S, F>,
    definition: SpillAggregateDefinition,
}
impl<S: EdgeScanSource, F> EdgeSpillAggregateCursor<S, F> {
    pub fn new(
        source: S,
        plan: EdgeSpillAggregatePlan,
        policy: GqlQueryPolicy,
        checkpoint: F,
    ) -> Self {
        Self {
            input: EdgeScanCursor::new(source, plan.input, policy, checkpoint),
            definition: plan.definition,
        }
    }
    pub fn definition(&self) -> &SpillAggregateDefinition {
        &self.definition
    }
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.input.snapshot_seq()
    }
    pub fn state(&self) -> EdgeScanState {
        self.input.state()
    }
    pub fn row_stats(&self) -> GqlExecutionStats {
        self.input.row_stats()
    }
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.input.evaluator_stats()
    }
    pub fn close(&mut self) {
        self.input.close();
    }

    fn fail(&mut self) {
        self.input.state = EdgeScanState::Failed;
        self.input.close();
    }

    pub fn next_input<C>(
        &mut self,
    ) -> Result<Option<GraphValueRow>, EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        if self.input.state != EdgeScanState::Open {
            return Ok(None);
        }
        let result = (|| {
            let Some(row) = self.input.advance().map_err(lift)? else {
                return Ok(None);
            };
            let meter = &mut self.input.meter;
            self.definition.validate_input(&row, &mut |event| {
                meter.event(value_event(event)).map_err(lift)
            })?;
            Ok(Some(row))
        })();
        match &result {
            Ok(None) => {
                self.input.state = EdgeScanState::Exhausted;
                self.input.close();
            }
            Err(_) => self.fail(),
            Ok(Some(_)) => {}
        }
        result
    }

    pub fn charge<C>(
        &mut self,
        event: VertexScanEvent,
    ) -> Result<(), EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        let result = self.input.meter.event(value_event(event)).map_err(lift);
        if result.is_err() {
            self.fail();
        }
        result
    }

    pub fn finish_result<C>(&mut self) -> Result<(), EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        if self.input.state != EdgeScanState::Exhausted {
            self.fail();
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        let result = (|| {
            let meter = &mut self.input.meter;
            let next = meter
                .increment(GqlBudgetDimension::ResultRows, meter.rows.result_rows)
                .map_err(lift)?;
            meter.event(GlaExecutionEvent::ResultRow).map_err(lift)?;
            meter.rows.result_rows = next;
            Ok(())
        })();
        if result.is_err() {
            self.fail();
        }
        result
    }
}

impl<S: EdgeScanSource, F: FnMut() -> Result<(), C>, C> Iterator
    for EdgeSpillAggregateCursor<S, F>
{
    type Item = Result<GraphValueRow, EdgeAggregateError<S::Error, C>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_input().transpose()
    }
}
impl<S: EdgeScanSource, F: FnMut() -> Result<(), C>, C> core::iter::FusedIterator
    for EdgeSpillAggregateCursor<S, F>
{
}
