//! Aggregate, set, and bounded shortest-walk execution over one admitted snapshot source.

use crate::gql_exec::{AdmissionUsage, AdmittedGqlSnapshot, GqlSnapshotReader};
use crate::{Database, EmbeddedReadView, GqlError, ReadError, Snapshot};
use asupersync::fs::Vfs;
use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::{
    GlaExecutionEvent, GlaExecutionLimits, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded,
    GqlBudgetDimension, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregateError, GraphAggregateRow, GraphShortestWalkCursor, GraphWalkBounds,
    PreparedGraphAggregate,
};
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::collections::BTreeMap;

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

    pub fn execute_temporal_graph_aggregate_text_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::BoundTemporalGraphAggregateQuery,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        self.execute_graph_aggregate_governed_at(cx, query.aggregate(), query.as_of(), policy)
    }
}

impl EmbeddedReadView {
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

    pub fn execute_temporal_graph_aggregate_text_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::BoundTemporalGraphAggregateQuery,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        self.execute_graph_aggregate_governed_at(cx, query.aggregate(), query.as_of(), policy)
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
    let result = aggregate.execute_governed_with_element_properties(
        admitted.snapshot_records,
        admitted.vertex_ids(),
        admitted.identified_edges(),
        |vid, predicates| admitted.matches(vid, predicates),
        |vid, key| Ok(admitted.property(vid, key)),
        |eid, key| Ok(admitted.edge_property(eid, key)),
        usage.remaining(policy),
        checkpoint,
    );
    usage
        .finish(policy, result)
        .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))
}

type SetResult<E> = Result<
    GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    GqlQueryError<fgdb_gql::GraphSetExecutionError<E>, Box<asupersync::error::Error>>,
>;

impl<V: Vfs + Clone> Database<V> {
    pub fn execute_graph_set_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        let as_of = self.frontier().map_err(|error| {
            GqlQueryError::Source(fgdb_gql::GraphSetExecutionError::Source(GqlError::Read(
                error,
            )))
        })?;
        self.execute_graph_set_governed_at(cx, query, as_of, policy)
    }

    pub fn execute_graph_set_governed_at(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        self.ensure_readable()
            .and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(|error| {
                GqlQueryError::Source(fgdb_gql::GraphSetExecutionError::Source(GqlError::Read(
                    error,
                )))
            })?;
        cx.with_restriction(|| {
            query.execute_governed(
                policy,
                |pattern, allowance| {
                    crate::gql_exec::execute_pattern_at(self, pattern, as_of, allowance, || {
                        cx.checkpoint()
                    })
                },
                || cx.checkpoint(),
            )
        })
    }

    pub fn execute_temporal_graph_set_text_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::BoundTemporalGraphSetQuery,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        self.execute_graph_set_governed_at(cx, query.query(), query.as_of(), policy)
    }
}

impl EmbeddedReadView {
    pub fn execute_graph_set_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        self.execute_graph_set_governed_at(cx, query, self.frontier(), policy)
    }

    pub fn execute_graph_set_governed_at(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        self.snapshot.check_frontier(as_of).map_err(|error| {
            GqlQueryError::Source(fgdb_gql::GraphSetExecutionError::Source(GqlError::Read(
                error,
            )))
        })?;
        cx.with_restriction(|| {
            query.execute_governed(
                policy,
                |pattern, allowance| {
                    crate::gql_exec::execute_pattern_at(self, pattern, as_of, allowance, || {
                        cx.checkpoint()
                    })
                },
                || cx.checkpoint(),
            )
        })
    }

    pub fn execute_temporal_graph_set_text_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::BoundTemporalGraphSetQuery,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        self.execute_graph_set_governed_at(cx, query.query(), query.as_of(), policy)
    }
}

type ShortestResult =
    Result<GqlQueryExecution<VId>, GqlQueryError<GqlError, Box<asupersync::error::Error>>>;

impl<V: Vfs + Clone> Database<V> {
    /// Return every shortest WALK occurrence from one existing source to each
    /// reachable endpoint inside the finite hop interval. Equal-length routes
    /// and parallel edges retain multiplicity; longer alternatives to the same
    /// source/end partition are pruned. Results are sorted by endpoint identity.
    /// This endpoint API is the execution primitive for ALL SHORTEST; it does
    /// not claim path-value capture or intermediate/path-expression predicates.
    pub fn execute_all_shortest_walk_governed(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        let as_of = self
            .frontier()
            .map_err(GqlError::Read)
            .map_err(GqlQueryError::Source)?;
        self.execute_all_shortest_walk_governed_at(
            cx, source, relation, direction, bounds, as_of, policy,
        )
    }

    pub fn execute_all_shortest_walk_governed_at(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        self.ensure_readable()
            .and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(GqlError::Read)
            .map_err(GqlQueryError::Source)?;
        cx.with_restriction(|| {
            execute_shortest_at(
                &self.snapshot,
                source,
                relation,
                direction,
                bounds,
                as_of,
                policy,
                || cx.checkpoint(),
            )
        })
    }
}

impl EmbeddedReadView {
    pub fn execute_all_shortest_walk_governed(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        self.execute_all_shortest_walk_governed_at(
            cx,
            source,
            relation,
            direction,
            bounds,
            self.frontier(),
            policy,
        )
    }

    pub fn execute_all_shortest_walk_governed_at(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        self.snapshot
            .check_frontier(as_of)
            .map_err(GqlError::Read)
            .map_err(GqlQueryError::Source)?;
        cx.with_restriction(|| {
            execute_shortest_at(
                &self.snapshot,
                source,
                relation,
                direction,
                bounds,
                as_of,
                policy,
                || cx.checkpoint(),
            )
        })
    }
}

fn charge_shortest<C>(
    stats: &mut GlaExecutionStats,
    limits: GlaExecutionLimits,
    event: GlaExecutionEvent,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<(), GqlQueryError<GqlError, C>> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    let mut next = *stats;
    let work = u128::from(next.work_units) + 1;
    if work > u128::from(limits.max_work_units) {
        return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
            dimension: GlaLimitDimension::WorkUnits,
            limit: limits.max_work_units,
            observed: work,
        }));
    }
    next.work_units = work as u64;
    if event == GlaExecutionEvent::ScratchEntry {
        let scratch = u128::from(next.scratch_entries) + 1;
        if scratch > u128::from(limits.max_scratch_entries) {
            return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension: GlaLimitDimension::ScratchEntries,
                limit: limits.max_scratch_entries,
                observed: scratch,
            }));
        }
        next.scratch_entries = scratch as u64;
    }
    *stats = next;
    Ok(())
}

fn execute_shortest_at<C>(
    snapshot: &Snapshot,
    source: VId,
    relation: RelationId,
    direction: GlaDirection,
    bounds: GraphWalkBounds,
    as_of: CommitSeq,
    policy: GqlQueryPolicy,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<GqlQueryExecution<VId>, GqlQueryError<GqlError, C>> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    let mut usage = AdmissionUsage::default();
    let exists = super::find_vertex(&snapshot.patches, source, as_of, &mut |event| {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        usage.observe::<GqlError, C>(policy, event)
    })?
    .is_some();
    if !exists {
        return usage.finish(
            policy,
            Ok(GqlQueryExecution {
                value: Vec::new(),
                rows: GqlExecutionStats {
                    snapshot_records: usage.records,
                    result_rows: 0,
                },
                evaluator: GlaExecutionStats::default(),
            }),
        );
    }

    let edges = super::scan_edges(
        &snapshot.blocks,
        &snapshot.block_props,
        as_of,
        &mut |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            usage.observe::<GqlError, C>(policy, event)
        },
    )?;
    let mut pairs = BTreeMap::<(VId, VId), u64>::new();
    for ((_, left, actual_relation, right), _) in edges {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        usage.observe::<GqlError, C>(policy, super::SourceEvent::Work)?;
        if actual_relation != relation {
            continue;
        }
        let directions = match direction {
            GlaDirection::Forward => [(left, right), (left, right)],
            GlaDirection::Reverse => [(right, left), (right, left)],
            GlaDirection::Undirected => [(left, right), (right, left)],
        };
        let count = if direction == GlaDirection::Undirected && left != right {
            2
        } else {
            1
        };
        for &(from, to) in directions.iter().take(count) {
            usage.observe::<GqlError, C>(policy, super::SourceEvent::Work)?;
            if !pairs.contains_key(&(from, to)) {
                usage.observe::<GqlError, C>(policy, super::SourceEvent::ScratchEntry)?;
            }
            let multiplicity = pairs.entry((from, to)).or_default();
            *multiplicity = multiplicity
                .checked_add(1)
                .expect("visible edge count fits u64");
        }
    }
    let mut adjacency = BTreeMap::<VId, Vec<VId>>::new();
    for ((from, to), multiplicity) in pairs {
        if !adjacency.contains_key(&from) {
            usage.observe::<GqlError, C>(policy, super::SourceEvent::ScratchEntry)?;
        }
        let neighbors = adjacency.entry(from).or_default();
        for _ in 0..multiplicity {
            usage.observe::<GqlError, C>(policy, super::SourceEvent::ScratchEntry)?;
            neighbors.push(to);
        }
    }

    let remaining = usage.remaining(policy);
    let result = (|| {
        let mut evaluator = GlaExecutionStats::default();
        let mut control =
            |event| charge_shortest(&mut evaluator, remaining.evaluator, event, &mut checkpoint);
        let mut cursor =
            GraphShortestWalkCursor::new(source, bounds, Some(&adjacency), &mut control)?;
        let mut counts = BTreeMap::<VId, u64>::new();
        let mut occurrences = 0_u64;
        while let Some(endpoint) = cursor.next_with_control(&mut control)? {
            let next = occurrences
                .checked_add(1)
                .expect("in-memory result count fits u64");
            policy
                .rows
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(GqlQueryError::Rows)?;
            occurrences = next;
            // Price the logical retained occurrence even though equal endpoints
            // are compressed into one counter until deterministic release.
            control(GlaExecutionEvent::ScratchEntry)?;
            control(GlaExecutionEvent::Work)?;
            if !counts.contains_key(&endpoint) {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
            let count = counts.entry(endpoint).or_default();
            *count = count
                .checked_add(1)
                .expect("bounded result multiplicity fits u64");
        }
        let mut value = Vec::new();
        for (endpoint, multiplicity) in counts {
            for _ in 0..multiplicity {
                control(GlaExecutionEvent::ResultRow)?;
                value.push(endpoint);
            }
        }
        Ok(GqlQueryExecution {
            value,
            rows: GqlExecutionStats {
                snapshot_records: usage.records,
                result_rows: occurrences,
            },
            evaluator,
        })
    })();
    usage.finish(policy, result)
}
