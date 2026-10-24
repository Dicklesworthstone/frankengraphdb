//! Awaitable aggregate occurrences through the checked single-edge kernel.

use super::*;
use crate::GraphAggregateError;
use crate::edge_stream::aggregate::{EdgeAggregateError, lift, value_event};
use crate::spill_aggregate::SpillAggregateDefinition;
use crate::stream::VertexScanEvent;

#[derive(Clone)]
pub struct AsyncEdgeSpillAggregatePlan {
    input: AsyncEdgeScanPlan,
    definition: SpillAggregateDefinition,
}

impl AsyncEdgeSpillAggregatePlan {
    pub(crate) fn compile(
        definition: SpillAggregateDefinition,
    ) -> Result<Self, EdgeScanBuildError> {
        let input = EdgeScanPlan::compile_local(
            definition.aggregate.input_pattern().plan(),
            LocalOutput::AggregateInput,
        )?;
        Ok(Self {
            input: AsyncEdgeScanPlan::from_inner(input),
            definition,
        })
    }
    pub fn definition(&self) -> &SpillAggregateDefinition {
        &self.definition
    }
}

impl core::fmt::Debug for AsyncEdgeSpillAggregatePlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncEdgeSpillAggregatePlan")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}

/// Private one-edge occurrences with the same source/reduction/result meter.
/// The two orientations of an undirected edge share one admitted source record
/// and spend no result allowance. No joined or probe plan can enter this cursor.
pub struct AsyncEdgeSpillAggregateCursor<S: AsyncEdgeScanSource, F> {
    input: AsyncEdgeScanCursor<S, F>,
    definition: SpillAggregateDefinition,
}

impl<S: AsyncEdgeScanSource, F> AsyncEdgeSpillAggregateCursor<S, F> {
    pub fn new(
        source: S,
        plan: AsyncEdgeSpillAggregatePlan,
        policy: GqlQueryPolicy,
        checkpoint: F,
    ) -> Self {
        Self {
            input: AsyncEdgeScanCursor::new(source, plan.input, policy, checkpoint),
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

    /// Compute and validate each complete input once before partitioning.
    /// output_event receives every native input event after logical admission
    /// and before allocation; a byte-accounted host grows the guard on each
    /// ScratchEntry, retaining it through encoding and asynchronous append.
    /// Callback/source failures preserve their type, fuse input and drop pending
    /// orientations. LIMIT zero on final output does not suppress input errors.
    pub async fn next_input<C>(
        &mut self,
        output_event: &mut (
                 impl FnMut(&mut S::OutputGuard, VertexScanEvent) -> Result<(), S::Error> + Send
             ),
    ) -> Result<Option<AsyncEdgeScanOutput<S::OutputGuard>>, EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        let Some(output) = self.input.next_inner::<false, C>().await else {
            return Ok(None);
        };
        let (row, mut guard) = output.map_err(lift)?.into_parts();
        self.input.state = EdgeScanState::Failed;
        // Keep source and any second orientation in this future until the
        // computed occurrence succeeds. Refusal or unwind releases both.
        let source = self.input.source.take();
        let pending = self.input.pending.take();
        let result = (|| {
            let meter = &mut self.input.meter;
            let mut control = |event| {
                meter.event(value_event(event)).map_err(lift)?;
                output_event(&mut guard, event).map_err(|error| {
                    GqlQueryError::Source(GraphAggregateError::Source(EdgeScanError::Source(error)))
                })
            };
            let row = self
                .definition
                .aggregate
                .evaluate_streamed_input(row, &mut |event| {
                    control(match event {
                        GlaExecutionEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
                        GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => {
                            VertexScanEvent::Work
                        }
                    })
                })?;
            self.definition.validate_input(&row, &mut control)?;
            Ok(AsyncEdgeScanOutput { row, guard })
        })();
        match result {
            Ok(row) => {
                self.input.source = source;
                self.input.pending = pending;
                self.input.state = EdgeScanState::Open;
                Ok(Some(row))
            }
            Err(error) => {
                self.fail();
                Err(error)
            }
        }
    }

    /// Source-free admission for external reduction under the original meter.
    /// The host chooses E; an impossible source error is eliminated by type.
    pub fn charge<E, C>(
        &mut self,
        event: VertexScanEvent,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        let result = self
            .input
            .meter
            .control(value_event(event))
            .map_err(|error| error.map_source(|never| match never {}));
        if result.is_err() {
            self.fail();
        }
        result
    }

    pub fn finish_result<E, C>(&mut self) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>
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
            let count = meter
                .rows
                .result_rows
                .checked_add(1)
                .ok_or(GqlQueryError::Source(
                    GraphAggregateError::ResultCountOverflow,
                ))?;
            meter
                .policy
                .rows
                .check(GqlBudgetDimension::ResultRows, count)
                .map_err(GqlQueryError::Rows)?;
            meter
                .control(GlaExecutionEvent::ResultRow)
                .map_err(|error| error.map_source(|never| match never {}))?;
            meter.rows.result_rows = count;
            Ok(())
        })();
        if result.is_err() {
            self.fail();
        }
        result
    }
}

impl<S: AsyncEdgeScanSource, F> core::fmt::Debug for AsyncEdgeSpillAggregateCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncEdgeSpillAggregateCursor")
            .field("input", &self.input)
            .field("definition", &self.definition)
            .finish()
    }
}
