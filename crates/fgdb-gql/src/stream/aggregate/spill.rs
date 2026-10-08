//! Private aggregate occurrences from the ordinary governed vertex loop.

use super::*;
use crate::spill_aggregate::SpillAggregateDefinition;

#[derive(Clone)]
pub struct VertexSpillAggregatePlan {
    input: VertexScanPlan<GraphValueRow>,
    definition: SpillAggregateDefinition,
}
impl VertexSpillAggregatePlan {
    pub(crate) fn compile(
        definition: SpillAggregateDefinition,
    ) -> Result<Self, VertexScanBuildError> {
        let input = VertexScanPlan::compile_with_projection(
            definition.aggregate.input_pattern().plan(),
            None,
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
        )?;
        Ok(Self { input, definition })
    }
    pub fn definition(&self) -> &SpillAggregateDefinition {
        &self.definition
    }

    pub(in crate::stream) fn into_parts(
        self,
    ) -> (VertexScanPlan<GraphValueRow>, SpillAggregateDefinition) {
        (self.input, self.definition)
    }
}
impl core::fmt::Debug for VertexSpillAggregatePlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VertexSpillAggregatePlan")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}

/// A private input stream with one meter retained through external reduction.
/// Input occurrences never consume ResultRows. EOF drops the source; charge()
/// and finish_result() continue using the original cumulative policy afterward.
pub struct VertexSpillAggregateCursor<S, F> {
    input: VertexScanCursor<S, F, GraphValueRow>,
    definition: SpillAggregateDefinition,
}
impl<S: VertexScanSource, F> VertexSpillAggregateCursor<S, F> {
    pub fn new(
        source: S,
        plan: VertexSpillAggregatePlan,
        policy: GqlQueryPolicy,
        checkpoint: F,
    ) -> Self {
        Self {
            input: VertexScanCursor::new(source, plan.input, policy, checkpoint),
            definition: plan.definition,
        }
    }
    pub fn definition(&self) -> &SpillAggregateDefinition {
        &self.definition
    }
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.input.snapshot_seq()
    }
    pub fn state(&self) -> VertexScanState {
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
        self.input.state = VertexScanState::Failed;
        self.input.close();
    }

    pub fn next_input<C>(
        &mut self,
    ) -> Result<Option<GraphValueRow>, VertexAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        if self.input.state != VertexScanState::Open {
            return Ok(None);
        }
        let result = (|| {
            let Some(row) = self.input.advance_inner::<false, C>().map_err(lift)? else {
                return Ok(None);
            };
            let meter = &mut self.input.meter;
            // Complete every native computed column before validation and
            // partitioning. Aggregate positions name this projected schema,
            // not the graph source columns, and private rows spend no output
            // allowance. Plain definitions still move their row unchanged.
            let row = self
                .definition
                .aggregate
                .evaluate_streamed_input(row, &mut |event| {
                    meter.event(input_event(event)).map_err(lift)
                })?;
            self.definition
                .validate_input(&row, &mut |event| meter.event(event).map_err(lift))?;
            Ok(Some(row))
        })();
        match &result {
            Ok(None) => {
                self.input.state = VertexScanState::Exhausted;
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
    ) -> Result<(), VertexAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        let result = self.input.meter.event(event).map_err(lift);
        if result.is_err() {
            self.fail();
        }
        result
    }

    /// Charge one complete final group, only after all source input is checked.
    pub fn finish_result<C>(&mut self) -> Result<(), VertexAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        if self.input.state != VertexScanState::Exhausted {
            self.fail();
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        let result = self.input.meter.emit().map_err(lift);
        if result.is_err() {
            self.fail();
        }
        result
    }
}

impl<S: VertexScanSource, F: FnMut() -> Result<(), C>, C> Iterator
    for VertexSpillAggregateCursor<S, F>
{
    type Item = Result<GraphValueRow, VertexAggregateError<S::Error, C>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_input().transpose()
    }
}
impl<S: VertexScanSource, F: FnMut() -> Result<(), C>, C> core::iter::FusedIterator
    for VertexSpillAggregateCursor<S, F>
{
}
