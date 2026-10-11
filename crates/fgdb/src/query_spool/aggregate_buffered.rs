//! Buffered native input for the existing external numeric reducer. The same
//! partition, reduction, HAVING, output DISTINCT and ordering engine consumes
//! either a resident cursor or an asynchronous cursor with an owned row charge.

use super::*;
use crate::BufferedReadView;
use fgdb_gql::edge_stream::{AsyncEdgeJoinSource, AsyncEdgeScanSource};
use fgdb_gql::spill_aggregate::{
    AsyncEdgeJoinSpillAggregateCursor, AsyncEdgeSpillAggregateCursor, AsyncSpillAggregatePlan,
    AsyncVertexSpillAggregateCursor,
};
use fgdb_gql::stream::AsyncVertexScanSource;
use fgdb_gql::{GlaExecutionEvent, GqlBudgetDimension};

struct BufferedInput<'q, T> {
    cursor: T,
    cx: &'q QueryCx,
}

macro_rules! buffered_input {
    ($cursor:ident, $source:ident, $variant:ident, $exhausted:path) => {
        impl<S, F> GroupInput for BufferedInput<'_, $cursor<S, F>>
        where
            S: $source<Error = crate::BufferedReadError, OutputGuard = MemoryCharge>,
            F: FnMut() -> core::result::Result<(), Cancel> + Send,
        {
            fn next_input(&mut self) -> crate::SendFuture<'_, Result<Option<SpoolRow>>> {
                Box::pin(async move {
                    let cx = self.cx;
                    // Source-record reservations do not cover computed-input
                    // allocations. Grow the returned row's independent
                    // guard BEFORE every native allocation and retain it until
                    // the shared drain has encoded and appended the occurrence.
                    let mut reserve = |guard: &mut MemoryCharge, event| {
                        if event == VertexScanEvent::ScratchEntry {
                            let bytes = 8 * size_of::<GraphValue>()
                                .max(fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
                            guard.grow(cx, bytes)?;
                        }
                        Ok::<_, crate::BufferedReadError>(())
                    };
                    cx.with_restriction_async(self.cursor.next_input(&mut reserve))
                        .await
                        .map(|row| row.map(|row| SpoolRow::buffered(row.into_parts())))
                        .map_err(|error| {
                            NativeAggregateSpoolError::BufferedExecute(Box::new(
                                error.map_source(|error| error.map_source(ScanError::$variant)),
                            ))
                        })
                })
            }
            fn charge(
                &mut self,
                event: VertexScanEvent,
            ) -> core::result::Result<(), ExecutionError> {
                // This is genuinely source-free admission. The generic error
                // domain is the existing reducer's; no storage error is erased.
                self.cx
                    .with_restriction(|| self.cursor.charge::<ScanError<ReadError>, Cancel>(event))
            }
            fn finish_result(&mut self) -> core::result::Result<(), ExecutionError> {
                self.cx.with_restriction(|| {
                    self.cursor.finish_result::<ScanError<ReadError>, Cancel>()
                })
            }
            fn exhausted(&self) -> bool {
                self.cursor.state() == $exhausted
            }
            fn snapshot_seq(&self) -> CommitSeq {
                self.cursor.snapshot_seq()
            }
            fn kind(&self) -> ScanKind {
                ScanKind::$variant
            }
            fn row_stats(&self) -> GqlExecutionStats {
                self.cursor.row_stats()
            }
            fn evaluator_stats(&self) -> GlaExecutionStats {
                self.cursor.evaluator_stats()
            }
        }
    };
}
buffered_input!(
    AsyncVertexSpillAggregateCursor,
    AsyncVertexScanSource,
    Vertex,
    fgdb_gql::stream::VertexScanState::Exhausted
);
buffered_input!(
    AsyncEdgeSpillAggregateCursor,
    AsyncEdgeScanSource,
    Edge,
    fgdb_gql::edge_stream::EdgeScanState::Exhausted
);
buffered_input!(
    AsyncEdgeJoinSpillAggregateCursor,
    AsyncEdgeJoinSource,
    Edge,
    fgdb_gql::edge_stream::EdgeScanState::Exhausted
);

// The source and every relational barrier have completed in authenticated
// scratch. Keep their exact native counters alive during reduction; their
// private row count is not the number of final aggregate results.
struct CompletedRelation<'q> {
    cx: &'q QueryCx,
    policy: GqlQueryPolicy,
    snapshot: CommitSeq,
    kind: ScanKind,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
}

impl<'q> CompletedRelation<'q> {
    fn new(cx: &'q QueryCx, policy: GqlQueryPolicy, input: &NativeResultSpool) -> Self {
        Self {
            cx,
            policy,
            snapshot: input.snapshot_seq(),
            kind: input.kind(),
            rows: GqlExecutionStats {
                snapshot_records: input.row_stats().snapshot_records,
                result_rows: 0,
            },
            evaluator: input.evaluator_stats(),
        }
    }
}

impl GroupInput for CompletedRelation<'_> {
    fn next_input(&mut self) -> crate::SendFuture<'_, Result<Option<SpoolRow>>> {
        // This meter can only accompany the already completed input run.
        // Accidentally dispatching it as a fresh source must not imply EOF.
        Box::pin(async { Err(NativeSpoolError::IncompleteCursor.into()) })
    }

    fn charge(&mut self, event: VertexScanEvent) -> core::result::Result<(), ExecutionError> {
        self.cx
            .with_restriction(|| self.cx.checkpoint())
            .map_err(GqlQueryError::Interrupted)?;
        self.evaluator
            .charge_event(
                self.policy.evaluator,
                match event {
                    VertexScanEvent::Work => GlaExecutionEvent::Work,
                    VertexScanEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                },
            )
            .map_err(GqlQueryError::Evaluator)
    }

    fn finish_result(&mut self) -> core::result::Result<(), ExecutionError> {
        let count = self.rows.result_rows.checked_add(1).ok_or_else(|| {
            GqlQueryError::Source(fgdb_gql::GraphAggregateError::ResultCountOverflow)
        })?;
        self.policy
            .rows
            .check(GqlBudgetDimension::ResultRows, count)
            .map_err(GqlQueryError::Rows)?;
        self.charge(VertexScanEvent::Work)?;
        self.rows.result_rows = count;
        Ok(())
    }

    fn exhausted(&self) -> bool {
        true
    }
    fn snapshot_seq(&self) -> CommitSeq {
        self.snapshot
    }
    fn kind(&self) -> ScanKind {
        self.kind
    }
    fn row_stats(&self) -> GqlExecutionStats {
        self.rows
    }
    fn evaluator_stats(&self) -> GlaExecutionStats {
        self.evaluator
    }
}

/// A completely bound local aggregate source plus its ordinary numeric/output
/// definition. Construction reads no graph. The only execution strategy is the
/// buffered source feeding the native grace-partition reducer.
#[derive(Clone)]
pub struct PreparedBufferedAggregate {
    plan: AsyncSpillAggregatePlan,
    columns: Vec<String>,
    slots: Vec<GraphAggregateTextSlot>,
    as_of: Option<CommitSeq>,
}
impl core::fmt::Debug for PreparedBufferedAggregate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PreparedBufferedAggregate([REDACTED])")
    }
}

impl PreparedNativeRead {
    /// Bind native COUNT/SUM/AVG/MIN/MAX and admit a local asynchronous vertex
    /// or fixed-hop edge source before database opening. Computed inputs, HAVING,
    /// computed visible output, RETURN DISTINCT and exact numeric ordering use
    /// the ordinary aggregate compiler. COUNT/SUM/AVG DISTINCT arguments use
    /// bounded external support passes. Unary WITH projection/filter pipelines
    /// retain intermediate DISTINCT, ordering and pagination in external stages.
    /// COLLECT, relational joins/UNWIND, OPTIONAL/variable-length expansion and probes refuse
    /// at preparation. Join routing and one record per hop use the view pool;
    /// routing metadata remains resident and may refuse when it does not fit.
    pub fn prepare_buffered_aggregate(
        &self,
        params: &GqlParameters,
    ) -> Result<PreparedBufferedAggregate> {
        let BoundAggregate {
            definition,
            columns,
            slots,
            as_of,
        } = bind(self, params)?;
        let plan = AsyncSpillAggregatePlan::compile(&definition)
            .map_err(|_| NativeAggregateSpoolError::Unsupported)?;
        let definition = plan.definition();
        if !crate::query::aggregate_stream::valid_layout(
            &columns,
            &slots,
            definition.key_columns(),
            definition.aggregate_columns(),
        ) {
            return Err(NativeAggregateSpoolError::Unsupported);
        }
        Ok(PreparedBufferedAggregate {
            plan,
            columns,
            slots,
            as_of,
        })
    }
}

impl PreparedBufferedAggregate {
    /// Number of external relational barriers before aggregate reduction.
    /// Hosts include these append-only passes in scratch metadata admission.
    pub fn stage_count(&self) -> usize {
        match &self.plan {
            AsyncSpillAggregatePlan::Relation { input, .. } => input.stages().len(),
            _ => 0,
        }
    }

    /// Evaluate all buffered input into authenticated partitions, reduce bounded
    /// group populations, then apply the native completed-group clauses. The
    /// returned handle belongs to destination and appears only after complete
    /// source/reduction/order success, including when final LIMIT is zero.
    ///
    /// Source decoding, local evaluation and each projected/computed input row
    /// are charged to the view pool. The row guard survives every awaited
    /// append. The existing spill pool charges canonical input encoding, decoded
    /// partition rows, group states, sort buffers and metadata before retention.
    /// All three scratch files must be distinct and query-private; their pools
    /// may share a parent with the view pool, or hosts can account independent
    /// source and spill allowances. Plans/catalogs, initial recovery and copies
    /// created by the caller remain under their existing contracts.
    ///
    /// The original GQL source/evaluator meter survives EOF and governs all
    /// reduction and final result admission. Private input spends no ResultRows;
    /// max_input_rows bounds occurrences separately. max_work_units bounds the
    /// existing partition/codec/sort/copy work without resets or disk refunds.
    /// Exact historical cuts, numeric domains, artifact resolution and all
    /// partition/run/page/row limits match spool_aggregate_in_view.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_in_view<'q, V, A, B, C>(
        &self,
        view: &'q mut BufferedReadView<V>,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        source: &'q mut SpillFile<A>,
        partition: &'q mut SpillFile<B>,
        destination: &'q mut SpillFile<C>,
        group_capacity: usize,
        max_partitions: usize,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_work_units: u64,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeAggregateSpool, u64)>>
    where
        V: Vfs + Clone + 'q,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        let as_of = self.as_of.unwrap_or_else(|| view.frontier());
        let definition = self.plan.definition().clone();
        let columns = self.columns.clone();
        let slots = self.slots.clone();
        if let AsyncSpillAggregatePlan::Relation { input, .. } = &self.plan {
            let ordered =
                crate::PreparedBufferedOrder::from_relation(input.as_ref().clone(), Some(as_of));
            return Box::pin(async move {
                validate_limits(
                    group_capacity,
                    max_partitions,
                    run_rows,
                    max_runs,
                    page_bytes,
                    max_row_bytes,
                )?;
                let private_policy = GqlQueryPolicy {
                    rows: GqlExecutionBudget::new(
                        policy.rows.max_snapshot_records().unwrap_or(u64::MAX),
                        max_input_rows,
                    ),
                    evaluator: policy.evaluator,
                };
                // The ordered runner always writes its result to its second
                // file. Leave that completed relation in the reducer's source
                // file so no reader aliases a live destination writer.
                let (initial, prior_work) = ordered
                    .spool_in_view_with_resolver(
                        view,
                        cx,
                        private_policy,
                        partition,
                        source,
                        run_rows,
                        max_runs,
                        page_bytes,
                        max_row_bytes,
                        max_input_rows,
                        max_work_units,
                        resolver,
                    )
                    .await?;
                let opened = Opened {
                    input: Box::new(CompletedRelation::new(cx, policy, &initial)),
                    definition,
                    columns,
                    slots,
                };
                execute(
                    opened,
                    Some(initial),
                    prior_work,
                    cx,
                    source,
                    partition,
                    destination,
                    group_capacity,
                    max_partitions,
                    run_rows,
                    max_runs,
                    page_bytes,
                    max_row_bytes,
                    max_input_rows,
                    max_work_units,
                    resolver,
                )
                .await
            });
        }
        let input: Result<Box<dyn GroupInput + 'q>> = match &self.plan {
            AsyncSpillAggregatePlan::Vertex(plan) => view
                .open_vertex_aggregate_input(cx, plan.clone(), as_of, policy)
                .map(|cursor| -> Box<dyn GroupInput + 'q> {
                    Box::new(BufferedInput { cursor, cx })
                })
                .map_err(|error| {
                    NativeAggregateSpoolError::BufferedExecute(Box::new(error.map_source(
                        |error| fgdb_gql::GraphAggregateError::Source(ScanError::Vertex(error)),
                    )))
                }),
            AsyncSpillAggregatePlan::Edge(plan) => view
                .open_edge_aggregate_input(cx, plan.clone(), as_of, policy)
                .map(|cursor| -> Box<dyn GroupInput + 'q> {
                    Box::new(BufferedInput { cursor, cx })
                })
                .map_err(|error| {
                    NativeAggregateSpoolError::BufferedExecute(Box::new(error.map_source(
                        |error| fgdb_gql::GraphAggregateError::Source(ScanError::Edge(error)),
                    )))
                }),
            AsyncSpillAggregatePlan::Join(plan) => view
                .open_edge_join_aggregate_input(cx, plan.clone(), as_of, policy)
                .map(|cursor| -> Box<dyn GroupInput + 'q> {
                    Box::new(BufferedInput { cursor, cx })
                })
                .map_err(|error| {
                    NativeAggregateSpoolError::BufferedExecute(Box::new(error.map_source(
                        |error| fgdb_gql::GraphAggregateError::Source(ScanError::Edge(error)),
                    )))
                }),
            AsyncSpillAggregatePlan::Relation { .. } => {
                return Box::pin(async { Err(NativeAggregateSpoolError::Unsupported) });
            }
        };
        Box::pin(async move {
            let opened = Opened {
                input: input?,
                definition,
                columns,
                slots,
            };
            execute(
                opened,
                None,
                0,
                cx,
                source,
                partition,
                destination,
                group_capacity,
                max_partitions,
                run_rows,
                max_runs,
                page_bytes,
                max_row_bytes,
                max_input_rows,
                max_work_units,
                resolver,
            )
            .await
        })
    }
}
