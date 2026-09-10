//! Aggregate execution over the existing admitted borrowed snapshot source.

use crate::gql_exec::{AdmissionUsage, AdmittedGqlSnapshot, GqlSnapshotReader};
use crate::{Database, EmbeddedReadView, GqlError, ReadError};
use asupersync::fs::Vfs;
use fgdb_gql::{
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
    PreparedGraphAggregate,
};
use fgdb_types::{CommitSeq, QueryCx};

type AggregateResult<E> = Result<
    GqlQueryExecution<GraphAggregateRow>,
    GqlQueryError<GraphAggregateError<E>, Box<asupersync::error::Error>>,
>;

impl<V: Vfs + Clone> Database<V> {
    /// Summarize one live snapshot without materializing the child match bag.
    /// Source admission, grouping, arithmetic and final output share one policy.
    pub fn execute_graph_aggregate_governed(
        &self,
        cx: &QueryCx,
        aggregate: &PreparedGraphAggregate,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        let as_of = self.frontier().map_err(|error| {
            GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
        })?;
        self.execute_graph_aggregate_governed_at(cx, aggregate, as_of, policy)
    }

    /// Health and exact-sequence fences precede cancellation/resource refusal.
    /// A count, including zero, is never obtained by skipping source admission.
    pub fn execute_graph_aggregate_governed_at(
        &self,
        cx: &QueryCx,
        aggregate: &PreparedGraphAggregate,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        self.ensure_readable()
            .and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(|error| {
                GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
            })?;
        cx.with_restriction(|| execute_at(self, aggregate, as_of, policy, || cx.checkpoint()))
    }
}

impl EmbeddedReadView {
    /// Grouping observes only this immutable generation and its retained data.
    pub fn execute_graph_aggregate_governed(
        &self,
        cx: &QueryCx,
        aggregate: &PreparedGraphAggregate,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        self.execute_graph_aggregate_governed_at(cx, aggregate, self.frontier(), policy)
    }

    pub fn execute_graph_aggregate_governed_at(
        &self,
        cx: &QueryCx,
        aggregate: &PreparedGraphAggregate,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        self.snapshot.check_frontier(as_of).map_err(|error| {
            GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
        })?;
        cx.with_restriction(|| execute_at(self, aggregate, as_of, policy, || cx.checkpoint()))
    }
}

fn execute_at<R: GqlSnapshotReader + ?Sized, C>(
    reader: &R,
    aggregate: &PreparedGraphAggregate,
    as_of: CommitSeq,
    policy: GqlQueryPolicy,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<GqlError>, C>> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    let mut admitted =
        AdmittedGqlSnapshot::admit_logical(aggregate.input_pattern().plan().clone(), reader, as_of)
            .map_err(|error| {
                GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
            })?;
    let mut usage = AdmissionUsage::default();
    admitted
        .materialize(&mut |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            usage.observe::<GraphAggregateError<ReadError>, C>(policy, event)
        })
        .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?;
    let result = aggregate.execute_governed(
        admitted.snapshot_records,
        admitted.vertex_ids(),
        admitted.edge_triples(),
        |vid, predicates| admitted.matches(vid, predicates),
        |vid, key| Ok(admitted.property(vid, key)),
        usage.remaining(policy),
        checkpoint,
    );
    usage
        .finish(policy, result)
        .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))
}
