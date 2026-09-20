//! Global scalar aggregate cursors over the existing pinned MVCC source.

use super::*;
use fgdb_gql::GraphAggregateError;
use fgdb_gql::stream::aggregate::{
    VertexAggregateCursor, VertexAggregateError, VertexAggregatePlan,
};

type AggregateError = VertexAggregateError<ReadError, Cancel>;

fn admission_error(error: ReadError) -> AggregateError {
    source_error(error).map_source(GraphAggregateError::Source)
}

fn open_global<'q>(
    view: EmbeddedReadView,
    cx: &'q QueryCx,
    plan: &VertexAggregatePlan,
    as_of: CommitSeq,
    policy: GqlQueryPolicy,
) -> Result<
    VertexAggregateCursor<
        SnapshotVertexSource<'q>,
        impl FnMut() -> Result<(), Cancel> + 'q + use<'q>,
    >,
    AggregateError,
> {
    // The host still admits health, retained history and QueryCx. A checked
    // physical definition is not authority to manufacture a data generation.
    view.snapshot
        .check_frontier(as_of)
        .map_err(admission_error)?;
    cx.with_restriction(|| cx.checkpoint())
        .map_err(GqlQueryError::Interrupted)?;
    Ok(VertexAggregateCursor::new(
        SnapshotVertexSource {
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
    /// Open exact global COUNT/SUM/AVG/MIN/MAX without collecting a projected
    /// snapshot or result bag. Build the immutable physical definition
    /// with `VertexAggregatePlan::compile` from a `PreparedGraphAggregate`.
    /// Unsupported aggregate/GLA shapes fail at compilation, never by retrying
    /// an eager executor. The returned cursor owns the generation and definition
    /// and borrows only `cx`, so the writer and definition may change or drop.
    ///
    /// The first pull consumes the supported source and releases one exact
    /// `GraphAggregateRow`; no partial aggregate escapes. Empty input returns
    /// zero counts and null non-count aggregates. AVG retains an exact reduced
    /// fraction; MIN/MAX retain their scalar or 128-bit vertex domains. Live
    /// state is one numeric cell or selected extremum per aggregate, not an
    /// input bag. Extremum comparison/copy costs and cumulative replacements
    /// remain governed; the shared pinned generation is still in memory.
    pub fn stream_global_aggregate_governed<'q>(
        &self,
        cx: &'q QueryCx,
        plan: &VertexAggregatePlan,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexAggregateCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
        >,
        AggregateError,
    > {
        let view = self.read_session().map_err(admission_error)?;
        let as_of = view.frontier();
        open_global(view, cx, plan, as_of, policy)
    }

    /// Admit the exact retained historical sequence before opening. A future
    /// or pruned cut is an error, including when no rows would match or the
    /// result allowance is zero. No selector is silently clamped to the writer.
    pub fn stream_global_aggregate_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        plan: &VertexAggregatePlan,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexAggregateCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
        >,
        AggregateError,
    > {
        open_global(
            self.read_session().map_err(admission_error)?,
            cx,
            plan,
            as_of,
            policy,
        )
    }
}

impl EmbeddedReadView {
    /// Reuse this already-admitted immutable generation, not the live writer.
    /// Opening examines no candidate history. Closing/dropping does not drain
    /// the source, and any completed/error pull releases the cursor's own pin.
    pub fn stream_global_aggregate_governed<'q>(
        &self,
        cx: &'q QueryCx,
        plan: &VertexAggregatePlan,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexAggregateCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q>,
        >,
        AggregateError,
    > {
        open_global(self.clone(), cx, plan, self.frontier(), policy)
    }

    /// Stream a retained cut no later than this view's captured frontier. All
    /// root and EXISTS/NOT EXISTS probe reads share this exact cut and meter.
    pub fn stream_global_aggregate_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        plan: &VertexAggregatePlan,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexAggregateCursor<
            SnapshotVertexSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q>,
        >,
        AggregateError,
    > {
        open_global(self.clone(), cx, plan, as_of, policy)
    }
}

#[cfg(test)]
mod tests;
