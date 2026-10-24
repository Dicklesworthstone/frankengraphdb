//! Fixed-hop aggregate input through the ordinary awaitable join driver.
//! External storage, group populations and argument DISTINCT remain host-owned.

use super::{SpillAggregateBuildError, SpillAggregateDefinition};
use crate::edge_stream::aggregate::EdgeAggregateError;
use crate::edge_stream::{
    AsyncEdgeJoinCursor, AsyncEdgeJoinOutput, AsyncEdgeJoinPlan, AsyncEdgeJoinSource,
    EdgeScanBuildError, EdgeScanState,
};
use crate::stream::VertexScanEvent;
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    PreparedGraphAggregate,
};
use fgdb_types::CommitSeq;

/// A complete native numeric definition sealed with its fixed-hop input plan.
/// Private input may omit the identity prefix required by directly delivered
/// join rows. The existing compiler still refuses child DISTINCT/pagination,
/// probes, OPTIONAL, variable-length expansion and unsupported projections.
#[derive(Clone, Debug)]
pub struct AsyncEdgeJoinSpillAggregatePlan {
    input: AsyncEdgeJoinPlan,
    definition: SpillAggregateDefinition,
}
impl AsyncEdgeJoinSpillAggregatePlan {
    pub fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, SpillAggregateBuildError> {
        Self::from_definition(SpillAggregateDefinition::compile(aggregate)?)
            .map_err(SpillAggregateBuildError::Edge)
    }

    pub(crate) fn from_definition(
        definition: SpillAggregateDefinition,
    ) -> Result<Self, EdgeScanBuildError> {
        let input =
            AsyncEdgeJoinPlan::compile_aggregate(definition.aggregate.input_pattern().plan())?;
        Ok(Self { input, definition })
    }

    pub fn definition(&self) -> &SpillAggregateDefinition {
        &self.definition
    }
}

/// One private joined occurrence at a time, with its independent output guard.
/// Root/nested candidates, native expressions, reduction and final delivery use
/// one cumulative meter. Input occurrences never spend ResultRows; the host
/// MUST enforce a separate input cap and retain each guard through its last
/// encoding/append. An empty input still permits the native global zero/null
/// summary. Only exhausted input can admit completed result groups.
///
/// This is not a reducer or a spill implementation. Feed definition()'s exact
/// cells and DISTINCT support contract into the existing external host. The
/// driver retains only its hop-bounded traversal; source residency and physical
/// byte admission remain the source's responsibility. Error, early close and
/// dropping a polled pull free the source and every retained traversal record.
pub struct AsyncEdgeJoinSpillAggregateCursor<S: AsyncEdgeJoinSource, F> {
    input: AsyncEdgeJoinCursor<S, F>,
    definition: SpillAggregateDefinition,
}
impl<S: AsyncEdgeJoinSource, F> AsyncEdgeJoinSpillAggregateCursor<S, F> {
    pub fn new(
        source: S,
        plan: AsyncEdgeJoinSpillAggregatePlan,
        policy: GqlQueryPolicy,
        checkpoint: F,
    ) -> Self {
        Self {
            input: AsyncEdgeJoinCursor::new(source, plan.input, policy, checkpoint),
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

    /// Evaluate and validate one complete input before host partitioning.
    /// Each computed allocation is admitted through output_event AFTER native
    /// work/scratch admission; keep the grown guard with the returned row.
    pub async fn next_input<C>(
        &mut self,
        output_event: &mut (
                 impl FnMut(&mut S::OutputGuard, VertexScanEvent) -> Result<(), S::Error> + Send
             ),
    ) -> Result<Option<AsyncEdgeJoinOutput<S::OutputGuard>>, EdgeAggregateError<S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        self.input
            .next_aggregate_input(&self.definition, output_event)
            .await
    }

    /// Source-free external reduction admission through the input's meter.
    pub fn charge<E, C>(
        &mut self,
        event: VertexScanEvent,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        self.input.charge_aggregate(event)
    }

    pub fn finish_result<E, C>(&mut self) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>
    where
        F: FnMut() -> Result<(), C>,
    {
        self.input.finish_aggregate_result()
    }
}
impl<S: AsyncEdgeJoinSource, F> core::fmt::Debug for AsyncEdgeJoinSpillAggregateCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncEdgeJoinSpillAggregateCursor")
            .field("input", &self.input)
            .field("definition", &self.definition)
            .finish()
    }
}
