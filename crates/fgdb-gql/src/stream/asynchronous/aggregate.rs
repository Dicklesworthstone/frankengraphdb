//! Awaitable aggregate occurrences from the ordinary vertex scan kernel.
//! The source record, projected row and host reservation never become a graph
//! cache. The original query meter continues through external reduction.

use super::*;
use crate::GraphAggregateError;
use crate::algebra::GraphValueRow;
use crate::spill_aggregate::SpillAggregateDefinition;
use crate::stream::aggregate::{VertexAggregateError, VertexSpillAggregatePlan, input_event, lift};

#[derive(Clone)]
pub struct AsyncVertexSpillAggregatePlan {
    input: AsyncVertexScanPlan<GraphValueRow>,
    definition: SpillAggregateDefinition,
}

impl AsyncVertexSpillAggregatePlan {
    pub(crate) fn compile(
        definition: SpillAggregateDefinition,
    ) -> Result<Self, VertexScanBuildError> {
        AsyncVertexScanPlan::check_source(definition.aggregate.input_pattern().plan())?;
        let (input, definition) = VertexSpillAggregatePlan::compile(definition)?.into_parts();
        Ok(Self {
            input: AsyncVertexScanPlan::from_inner(input),
            definition,
        })
    }

    pub fn definition(&self) -> &SpillAggregateDefinition {
        &self.definition
    }
}

impl core::fmt::Debug for AsyncVertexSpillAggregatePlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncVertexSpillAggregatePlan")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}

/// Private, guarded input occurrences for a host-owned external reducer.
///
/// next_input spends no ResultRows allowance. Source decoding, predicates,
/// raw projection, computed input and validation use one cumulative meter.
/// EOF drops the source; charge and finish_result keep that meter afterward.
/// A result row is legal only after complete source exhaustion.
pub struct AsyncVertexSpillAggregateCursor<S: AsyncVertexScanSource, F> {
    input: AsyncVertexScanCursor<S, F, GraphValueRow>,
    definition: SpillAggregateDefinition,
}

impl<S: AsyncVertexScanSource, F> AsyncVertexSpillAggregateCursor<S, F> {
    pub fn new(
        source: S,
        plan: AsyncVertexSpillAggregatePlan,
        policy: GqlQueryPolicy,
        checkpoint: F,
    ) -> Self {
        Self {
            input: AsyncVertexScanCursor::new(source, plan.input, policy, checkpoint),
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

    /// Return one complete native input and its independently owned guard.
    /// Each computed-input allocation invokes output_event BEFORE allocating,
    /// after logical admission. A byte-accounted host grows the supplied guard
    /// on ScratchEntry and keeps it through encoding and asynchronous append.
    /// The callback also observes Work events; it cannot reset native quotas.
    /// Plain inputs move their existing row and guard without another copy.
    /// A callback refusal is a typed source error and permanently fuses input.
    pub async fn next_input<C>(
        &mut self,
        output_event: &mut (
                 impl FnMut(&mut S::OutputGuard, VertexScanEvent) -> Result<(), S::Error> + Send
             ),
    ) -> Result<
        Option<AsyncVertexScanOutput<GraphValueRow, S::OutputGuard>>,
        VertexAggregateError<S::Error, C>,
    >
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        let Some(output) = self.input.next_inner::<false, C>().await else {
            return Ok(None);
        };
        let (row, mut guard) = output.map_err(lift)?.into_parts();
        // Once a source prefix has been consumed, every computation must either
        // complete this occurrence or leave a terminal cursor, including unwind.
        self.input.state = VertexScanState::Failed;
        let source = self.input.source.take();
        let result = (|| {
            let meter = &mut self.input.meter;
            let mut control = |event| {
                meter.event(event).map_err(lift)?;
                output_event(&mut guard, event).map_err(|error| {
                    GqlQueryError::Source(GraphAggregateError::Source(VertexScanError::Source(
                        error,
                    )))
                })
            };
            let row = self
                .definition
                .aggregate
                .evaluate_streamed_input(row, &mut |event| control(input_event(event)))?;
            self.definition.validate_input(&row, &mut control)?;
            Ok(AsyncVertexScanOutput { row, guard })
        })();
        match result {
            Ok(row) => {
                self.input.source = source;
                self.input.state = VertexScanState::Open;
                Ok(Some(row))
            }
            Err(error) => {
                self.fail();
                Err(error)
            }
        }
    }

    /// Meter-only reduction admission. E is chosen by the host; this operation
    /// cannot construct a source error because no source access is performed.
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
            .control(event)
            .map_err(|error| error.map_source(|never| match never {}));
        if result.is_err() {
            self.fail();
        }
        result
    }

    /// Admit one final group only after all source input succeeded.
    /// Result-count overflow is the native aggregate error, independent of the
    /// source's error type; logical budget and interruption remain unchanged.
    pub fn finish_result<E, C>(&mut self) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        if self.input.state != VertexScanState::Exhausted {
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
                .control(VertexScanEvent::Work)
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

impl<S: AsyncVertexScanSource, F> core::fmt::Debug for AsyncVertexSpillAggregateCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncVertexSpillAggregateCursor")
            .field("input", &self.input)
            .field("definition", &self.definition)
            .finish()
    }
}
