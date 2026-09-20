//! Numeric aggregate pulls over the SAME pinned edge/incidence history source.
//! No second source representation or eager materialization is introduced.

use super::*;
use fgdb_gql::GraphAggregateError;
use fgdb_gql::edge_stream::aggregate::{
    EdgeAggregateCursor, EdgeAggregateError, EdgeAggregatePlan,
};

type AggregateError = EdgeAggregateError<ReadError, Cancel>;

fn admission_error(error: ReadError) -> AggregateError {
    source_error(error).map_source(GraphAggregateError::Source)
}
fn open_aggregate<'q>(
    view: EmbeddedReadView,
    cx: &'q QueryCx,
    plan: &EdgeAggregatePlan,
    as_of: CommitSeq,
    policy: GqlQueryPolicy,
) -> Result<
    EdgeAggregateCursor<SnapshotEdgeSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q>>,
    AggregateError,
> {
    view.snapshot
        .check_frontier(as_of)
        .map_err(admission_error)?;
    cx.with_restriction(|| cx.checkpoint())
        .map_err(GqlQueryError::Interrupted)?;
    Ok(EdgeAggregateCursor::new(
        SnapshotEdgeSource {
            view,
            cx,
            as_of,
            after: None,
        },
        plan.clone(),
        policy,
        move || cx.with_restriction(|| cx.checkpoint()),
    ))
}

impl<V: Vfs + Clone> Database<V> {
    /// Stream exact global COUNT/SUM over a checked fixed-edge pattern. Compile
    /// with EdgeAggregatePlan::compile; unsupported shapes never fall back to
    /// an eager query. Opening checks handle health and context but examines no
    /// candidate. Each source lookup uses the same immutable generation.
    ///
    /// The first pull consumes the indexed match stream, retaining one input
    /// row plus numeric cells, then emits one complete summary. Only that row
    /// counts against the result allowance. Every root, join and probe candidate
    /// shares cumulative work/scratch/record limits; no partial summary escapes.
    /// The source still pins decoded storage. This is not spill or a byte cap.
    pub fn stream_global_edge_aggregate_governed<'q>(
        &self,
        cx: &'q QueryCx,
        plan: &EdgeAggregatePlan,
        policy: GqlQueryPolicy,
    ) -> Result<
        EdgeAggregateCursor<
            SnapshotEdgeSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
        >,
        AggregateError,
    > {
        let view = self.read_session().map_err(admission_error)?;
        let as_of = view.frontier();
        open_aggregate(view, cx, plan, as_of, policy)
    }

    /// Health and exact-history fences precede cancellation or quota refusal.
    /// The returned cursor borrows only QueryCx, not this handle or definition.
    /// Later writes, compaction, drop or reopen cannot change its admitted cut.
    pub fn stream_global_edge_aggregate_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        plan: &EdgeAggregatePlan,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        EdgeAggregateCursor<
            SnapshotEdgeSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
        >,
        AggregateError,
    > {
        open_aggregate(
            self.read_session().map_err(admission_error)?,
            cx,
            plan,
            as_of,
            policy,
        )
    }
}
impl EmbeddedReadView {
    /// Use this pinned generation, never the database's newer frontier.
    /// Closing without polling drops the cursor's pin without scanning it.
    pub fn stream_global_edge_aggregate_governed<'q>(
        &self,
        cx: &'q QueryCx,
        plan: &EdgeAggregatePlan,
        policy: GqlQueryPolicy,
    ) -> Result<
        EdgeAggregateCursor<
            SnapshotEdgeSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q>,
        >,
        AggregateError,
    > {
        open_aggregate(self.clone(), cx, plan, self.frontier(), policy)
    }
    pub fn stream_global_edge_aggregate_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        plan: &EdgeAggregatePlan,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        EdgeAggregateCursor<
            SnapshotEdgeSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q>,
        >,
        AggregateError,
    > {
        open_aggregate(self.clone(), cx, plan, as_of, policy)
    }
}
