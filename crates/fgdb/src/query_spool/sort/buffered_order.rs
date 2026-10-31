//! Bind and admit the buffered source before storage opening, then feed the
//! existing native external order engine without an intermediate resident view.

use super::*;
use crate::BufferedReadView;
use fgdb_gql::edge_stream::{
    AsyncEdgeJoinCursor, AsyncEdgeJoinPlan, AsyncEdgeJoinSource, AsyncEdgeScanPlan,
    AsyncGraphWalkCursor, AsyncGraphWalkPlan, AsyncGraphWalkSource,
};
use fgdb_gql::spill_set::{AsyncSpillSetPlan, AsyncSpillSetSourcePlan};
use fgdb_gql::stream::AsyncVertexScanPlan;
use fgdb_strata::tiered::memory::MemoryCharge;
use fgdb_types::CanonicalScalarResolver;

#[path = "buffered_sealed.rs"]
mod sealed;
pub use sealed::PreparedBufferedSealedOrder;

#[derive(Clone)]
enum Input {
    Vertex(AsyncVertexScanPlan<GraphValueRow>, ScanSortTail),
    Edge(AsyncEdgeScanPlan, ScanSortTail),
    Join(AsyncEdgeJoinPlan, ScanSortTail),
    Walk(AsyncGraphWalkPlan, ScanSortTail),
    Set(AsyncSpillSetPlan),
}

/// A parameter-bound local query whose complete ORDER BY/DISTINCT/window is
/// executed over a buffered source. Preparation reads no graph or scratch.
/// The sealed physical plan owns its clauses and exact historical selector;
/// an unsupported shape refuses before a database needs to be opened.
#[derive(Clone)]
pub struct PreparedBufferedOrder {
    columns: Vec<String>,
    input: Input,
    as_of: Option<CommitSeq>,
}
impl core::fmt::Debug for PreparedBufferedOrder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PreparedBufferedOrder([REDACTED])")
    }
}

impl PreparedNativeRead {
    /// Bind once and admit a native singleton, vertex, single-edge, fixed-hop
    /// or bounded-path source for external ordering. Source-free expressions/UNWIND open no
    /// graph cursor. Property-only output, local predicates, hidden ordering keys,
    /// edge orientation and row DISTINCT use their existing GLA semantics.
    /// Native unary Project/Filter/Scope/UNWIND relations preserve every
    /// expansion, canonicalization, DISTINCT, order and window through external
    /// stages. UNWIND retains one input/list and one expanded row at a time.
    /// UNION/INTERSECT/EXCEPT, ALL and DISTINCT, complete both operands before
    /// canonical external merging; every child window and the shared native
    /// occurrence/equality laws survive composition. All child sources share
    /// the same snapshot and cumulative record/evaluator/physical allowances.
    /// Native Cartesian relations admit leading UNWIND followed by MATCH,
    /// including correlation through the native following filter. Both inputs
    /// finish once, then bounded row pairs preserve the left-major sequence.
    /// Intermediate COUNT/SUM/AVG/MIN/MAX grouping completes through the native
    /// partitioned reducer, then converts every group to ordinary row values
    /// before downstream clauses. Exact rational averages and wide integer
    /// overflow retain the native row-domain refusal. Admitted COLLECT inputs
    /// preserve their native source or completed relational occurrence order.
    /// Correlated fixed-hop or copy-only EXISTS/NOT EXISTS use the async join
    /// source, including probes without an expansion. Keyed/theta relational
    /// joins finish both child plans, then use bounded-row external matching
    /// and canonical bag ordering for inner/outer/semi/anti results. Pair work
    /// may be quadratic. Correlated Inner/Optional Apply executes each complete
    /// body under its own typed argument frame, including nested CALL, EXISTS,
    /// COUNT and lowered OPTIONAL MATCH relations. Duplicate outer occurrences
    /// run independently; grouping, order and windows remain local to a body.
    /// Collected-list Apply preserves each selected body's order, duplicates
    /// and mapped NULL values. A NULL input list produces NULL without running
    /// its body; an empty input produces an empty list. The completed list is
    /// a resident value with cumulative evaluator and memory admission even
    /// when its body spills. Empty outer input skips the body, while a final
    /// LIMIT zero cannot hide a reached body error. One bounded variable-length
    /// graph atom streams every WALK/TRAIL/ACYCLIC/SIMPLE route, including
    /// zero-hop isolates and captured paths, into the same complete external
    /// ordering. Shortest/unbounded paths, unsupported combinations, procedures
    /// and unlowered optional pattern instructions still refuse.
    /// Terminal exact aggregate results have their own preparation.
    pub fn prepare_buffered_order(&self, params: &GqlParameters) -> Result<PreparedBufferedOrder> {
        let prepare = |error| NativeSpoolError::Prepare(Box::new(error));
        let set = match self {
            Self::Set(template) => Some((
                template
                    .bind_parameters(params)
                    .map_err(|error| prepare(QueryError::SetText(error)))?,
                None,
            )),
            Self::TemporalSet(template) => {
                let bound = template
                    .bind_parameters(params)
                    .map_err(|error| prepare(QueryError::TemporalSetText(error)))?;
                Some((bound.query().clone(), Some(bound.as_of())))
            }
            _ => None,
        };
        if let Some((query, as_of)) = set {
            let plan = AsyncSpillSetPlan::compile_with_bounded_walks(&query).map_err(|_| {
                prepare(QueryError::StreamingUnsupported {
                    facade: self.facade_class(),
                })
            })?;
            return Ok(PreparedBufferedOrder {
                columns: plan.columns().to_vec(),
                input: Input::Set(plan),
                as_of,
            });
        }
        let (query, as_of) = match self {
            Self::Pattern(template) => (
                template
                    .bind_parameters(params)
                    .map_err(|error| prepare(QueryError::PatternText(error)))?,
                None,
            ),
            Self::TemporalPattern(template) => {
                let bound = template
                    .bind_parameters(params)
                    .map_err(|error| prepare(QueryError::TemporalText(error)))?;
                (bound.pattern().clone(), Some(bound.as_of()))
            }
            _ => {
                return Err(prepare(QueryError::StreamingUnsupported {
                    facade: self.facade_class(),
                }));
            }
        };
        let input = if matches!(
            query.plan().operators().first(),
            Some(GlaOperator::ScanVertices)
        ) && query
            .plan()
            .operators()
            .iter()
            .any(|op| matches!(op, GlaOperator::VarLengthExpand { .. }))
        {
            let (plan, tail) =
                AsyncGraphWalkPlan::compile_sort_input(query.plan()).map_err(|error| {
                    prepare(QueryError::EdgeStream(GqlQueryError::Source(
                        EdgeScanError::Plan(error),
                    )))
                })?;
            Input::Walk(plan, tail)
        } else if matches!(
            query.plan().operators().first(),
            Some(GlaOperator::ScanEdges { .. })
        ) {
            let error = |error| {
                prepare(QueryError::EdgeStream(GqlQueryError::Source(
                    EdgeScanError::Plan(error),
                )))
            };
            if query
                .plan()
                .operators()
                .iter()
                .any(|op| matches!(op, GlaOperator::Expand { .. } | GlaOperator::Probe { .. }))
            {
                let (plan, tail) =
                    AsyncEdgeJoinPlan::compile_sort_input(query.plan()).map_err(error)?;
                Input::Join(plan, tail)
            } else {
                let (plan, tail) =
                    AsyncEdgeScanPlan::compile_sort_input(query.plan()).map_err(error)?;
                Input::Edge(plan, tail)
            }
        } else {
            let (plan, tail) =
                AsyncVertexScanPlan::compile_sort_input(query.plan()).map_err(|error| {
                    prepare(QueryError::Stream(GqlQueryError::Source(
                        VertexScanError::Plan(error),
                    )))
                })?;
            Input::Vertex(plan, tail)
        };
        Ok(PreparedBufferedOrder {
            columns: query.columns().to_vec(),
            input,
            as_of,
        })
    }
}

impl PreparedBufferedOrder {
    pub fn from_prepared_relation(
        query: &fgdb_gql::PreparedGraphSet,
        as_of: Option<CommitSeq>,
    ) -> core::result::Result<Self, fgdb_gql::spill_set::SpillSetBuildError> {
        Ok(Self::from_relation(
            AsyncSpillSetPlan::compile_with_bounded_walks(query)?,
            as_of,
        ))
    }

    /// Reuse a fully admitted relational program as private aggregate input.
    /// Its complete stage sequence and source tail remain unchanged.
    pub(crate) fn from_relation(input: AsyncSpillSetPlan, as_of: Option<CommitSeq>) -> Self {
        Self {
            columns: input.columns().to_vec(),
            input: Input::Set(input),
            as_of,
        }
    }

    /// Number of native relational and binary canonicalization/merge barriers.
    /// Every barrier has finite external sort/window/copy work. Correlated
    /// bodies may invoke a barrier repeatedly: this static shape is not a
    /// bound on scratch run attempts, which retain cumulative file quotas.
    pub fn stage_count(&self) -> usize {
        match &self.input {
            Input::Set(plan) => plan.stage_count(),
            _ => 0,
        }
    }

    /// Exact number of reducer definitions in this relational tree. Correlated
    /// invocations repeat their partition and DISTINCT-argument passes within
    /// the same cumulative scratch limits.
    pub fn aggregate_count(&self) -> usize {
        match &self.input {
            Input::Set(plan) => plan.aggregate_count(),
            _ => 0,
        }
    }

    /// Evaluate the complete buffered source, externally order all occurrences,
    /// apply DISTINCT, then SKIP/LIMIT and the visible projection. LIMIT zero
    /// still evaluates the full source and preserves late failures. A handle
    /// escapes only after every phase succeeds; it always belongs to destination.
    ///
    /// Source rows and expressions use the view's MemoryPool. Intermediate
    /// native-stage decode and expression allocations use the destination spill
    /// pool. Row reservations survive canonical encoding and all awaited
    /// appends. The native encoder's complete row/cell/scalar overlap is admitted
    /// to the spill pool before allocation, with max_row_bytes checked against
    /// an allocation-free shape first. Scratch pages, sort runs and merge metadata
    /// use the same scratch-file pools. Returned bytes must match that preflight.
    /// Hosts may use children of a common pool for an aggregate resident bound,
    /// or separately account source and spill allowances. Initial recovery,
    /// bound plans/catalogs and caller-owned copies have their existing bounds.
    ///
    /// Source record/evaluator quotas are cumulative; max_input_rows governs
    /// private input occurrences, and the original result quota governs only
    /// final selected rows. max_sort_work cumulatively covers input encoding and
    /// the existing sort/window work; no phase restarts the allowance.
    /// All three scratch files must be distinct and query-private. Grouping
    /// reuses the auxiliary file for complete aggregate output while the other
    /// two files own input partitions. No allowance is reset or refunded for
    /// appended intermediates, errors or cancellation.
    /// The exact temporal cut is checked against this view's publication.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_in_view<'q, V, A, B, C>(
        &self,
        view: &'q mut BufferedReadView<V>,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        auxiliary: &'q mut SpillFile<C>,
        group_capacity: usize,
        max_partitions: usize,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_sort_work: u64,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        V: Vfs + Clone + 'q,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        self.spool_in_view_with_resolver(
            view,
            cx,
            policy,
            scratch,
            destination,
            auxiliary,
            group_capacity,
            max_partitions,
            run_rows,
            max_runs,
            page_bytes,
            max_row_bytes,
            max_input_rows,
            max_sort_work,
            None,
        )
    }

    /// Execute the same checked buffered program with the native resolver used
    /// to decode artifact-bound text and timestamp values between stages.
    /// The supplied resolver is carried through every scratch barrier; no host
    /// locale, timezone, or replacement artifact can be chosen by this executor.
    /// All quotas and memory guarantees of [`Self::spool_in_view`] apply.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_in_view_with_resolver<'q, V, A, B, C>(
        &self,
        view: &'q mut BufferedReadView<V>,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        auxiliary: &'q mut SpillFile<C>,
        group_capacity: usize,
        max_partitions: usize,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_sort_work: u64,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        V: Vfs + Clone + 'q,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        let as_of = self.as_of.unwrap_or_else(|| view.frontier());
        let columns = self.columns.clone();
        let input_policy = input_policy(policy, max_input_rows);
        match &self.input {
            Input::Set(plan) => {
                let plan = plan.clone();
                Box::pin(async move {
                    let mut host = BufferedHost { view };
                    execute_relation(
                        &plan,
                        None,
                        as_of,
                        &mut host,
                        cx,
                        policy,
                        scratch,
                        destination,
                        auxiliary,
                        ExternalOrderLimits {
                            group_capacity,
                            max_partitions,
                            run_rows,
                            max_runs,
                            page_bytes,
                            max_row_bytes,
                            max_input_rows,
                            max_sort_work,
                        },
                        resolver,
                    )
                    .await
                })
            }
            Input::Vertex(plan, tail) => {
                let tail = tail.clone();
                let opened = view
                    .open_vertex_input(cx, plan.clone(), as_of, input_policy)
                    .map_err(|error| {
                        NativeSpoolError::BufferedExecute(Box::new(
                            error.map_source(ScanError::Vertex),
                        ))
                    });
                Box::pin(async move {
                    ordered_input(
                        cx,
                        columns,
                        opened?,
                        tail,
                        policy,
                        scratch,
                        destination,
                        run_rows,
                        max_runs,
                        page_bytes,
                        max_row_bytes,
                        max_sort_work,
                    )
                    .await
                })
            }
            Input::Walk(plan, tail) => {
                let tail = tail.clone();
                let opened = view
                    .open_graph_walk_input(cx, plan.clone(), as_of, input_policy)
                    .map_err(|error| {
                        NativeSpoolError::BufferedExecute(Box::new(
                            error.map_source(ScanError::Edge),
                        ))
                    });
                Box::pin(async move {
                    ordered_input(
                        cx,
                        columns,
                        opened?,
                        tail,
                        policy,
                        scratch,
                        destination,
                        run_rows,
                        max_runs,
                        page_bytes,
                        max_row_bytes,
                        max_sort_work,
                    )
                    .await
                })
            }
            Input::Join(plan, tail) => {
                let tail = tail.clone();
                let opened = view
                    .open_edge_join_input(cx, plan.clone(), as_of, input_policy)
                    .map_err(|error| {
                        NativeSpoolError::BufferedExecute(Box::new(
                            error.map_source(ScanError::Edge),
                        ))
                    });
                Box::pin(async move {
                    ordered_input(
                        cx,
                        columns,
                        opened?,
                        tail,
                        policy,
                        scratch,
                        destination,
                        run_rows,
                        max_runs,
                        page_bytes,
                        max_row_bytes,
                        max_sort_work,
                    )
                    .await
                })
            }
            Input::Edge(plan, tail) => {
                let tail = tail.clone();
                let opened = view
                    .open_edge_input(cx, plan.clone(), as_of, input_policy)
                    .map_err(|error| {
                        NativeSpoolError::BufferedExecute(Box::new(
                            error.map_source(ScanError::Edge),
                        ))
                    });
                Box::pin(async move {
                    ordered_input(
                        cx,
                        columns,
                        opened?,
                        tail,
                        policy,
                        scratch,
                        destination,
                        run_rows,
                        max_runs,
                        page_bytes,
                        max_row_bytes,
                        max_sort_work,
                    )
                    .await
                })
            }
        }
    }
}

/// Cold physical leaves and reached fields for the shared relational owner.
pub(super) struct BufferedHost<'v, V: Vfs> {
    pub(super) view: &'v mut BufferedReadView<V>,
}

impl<V: Vfs + Clone> ExternalOrderHost for BufferedHost<'_, V> {
    fn check_snapshot(&mut self, snapshot: CommitSeq) -> Result<()> {
        let publication = self.view.frontier();
        if snapshot > publication {
            return Err(NativeSpoolError::BufferedRead(Box::new(
                crate::BufferedReadError::BeyondPublication {
                    requested: snapshot,
                    publication,
                },
            )));
        }
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<()> {
        Ok(())
    }

    fn native_event(&mut self, _event: GlaExecutionEvent) -> Result<()> {
        Ok(())
    }

    fn leaf<'q, A, B>(
        &'q mut self,
        plan: &'q AsyncSpillSetPlan,
        snapshot: CommitSeq,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        limits: ExternalOrderLimits,
        _resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        Box::pin(async move {
            self.check_snapshot(snapshot)?;
            let tail = plan.source_tail().cloned().ok_or(SpillError::InvalidRun)?;
            let columns = plan.source_columns().to_vec();
            match plan.source() {
                AsyncSpillSetSourcePlan::Vertex(source) => {
                    let cursor = self
                        .view
                        .open_vertex_input(cx, source.clone(), snapshot, policy)
                        .map_err(|error| {
                            NativeSpoolError::BufferedExecute(Box::new(
                                error.map_source(ScanError::Vertex),
                            ))
                        })?;
                    ordered_input(
                        cx,
                        columns,
                        cursor,
                        tail,
                        policy,
                        scratch,
                        destination,
                        limits.run_rows,
                        limits.max_runs,
                        limits.page_bytes,
                        limits.max_row_bytes,
                        limits.max_sort_work,
                    )
                    .await
                }
                AsyncSpillSetSourcePlan::Edge(source) => {
                    let cursor = self
                        .view
                        .open_edge_input(cx, source.clone(), snapshot, policy)
                        .map_err(|error| {
                            NativeSpoolError::BufferedExecute(Box::new(
                                error.map_source(ScanError::Edge),
                            ))
                        })?;
                    ordered_input(
                        cx,
                        columns,
                        cursor,
                        tail,
                        policy,
                        scratch,
                        destination,
                        limits.run_rows,
                        limits.max_runs,
                        limits.page_bytes,
                        limits.max_row_bytes,
                        limits.max_sort_work,
                    )
                    .await
                }
                AsyncSpillSetSourcePlan::Walk(source) => {
                    let cursor = self
                        .view
                        .open_graph_walk_input(cx, source.clone(), snapshot, policy)
                        .map_err(|error| {
                            NativeSpoolError::BufferedExecute(Box::new(
                                error.map_source(ScanError::Edge),
                            ))
                        })?;
                    ordered_input(
                        cx,
                        columns,
                        cursor,
                        tail,
                        policy,
                        scratch,
                        destination,
                        limits.run_rows,
                        limits.max_runs,
                        limits.page_bytes,
                        limits.max_row_bytes,
                        limits.max_sort_work,
                    )
                    .await
                }
                AsyncSpillSetSourcePlan::Join(source) => {
                    let cursor = self
                        .view
                        .open_edge_join_input(cx, source.clone(), snapshot, policy)
                        .map_err(|error| {
                            NativeSpoolError::BufferedExecute(Box::new(
                                error.map_source(ScanError::Edge),
                            ))
                        })?;
                    ordered_input(
                        cx,
                        columns,
                        cursor,
                        tail,
                        policy,
                        scratch,
                        destination,
                        limits.run_rows,
                        limits.max_runs,
                        limits.page_bytes,
                        limits.max_row_bytes,
                        limits.max_sort_work,
                    )
                    .await
                }
                _ => Err(SpillError::InvalidRun.into()),
            }
        })
    }

    fn stages<'q, A, B>(
        &'q mut self,
        plan: &'q AsyncSpillSetPlan,
        input: NativeResultSpool,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        limits: ExternalOrderLimits,
        prior_work: u64,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        Box::pin(staged::execute(
            plan,
            input,
            self.view,
            cx,
            policy,
            scratch,
            destination,
            limits.run_rows,
            limits.max_runs,
            limits.page_bytes,
            limits.max_row_bytes,
            limits.max_input_rows,
            limits.max_sort_work,
            prior_work,
            resolver,
        ))
    }
}

// Type adaptation only. Keep the guarded native row intact through canonical
// encoding and awaited appends in the existing drain() loop. No second meter,
// source collection, projection or row-count interpretation is added here.
impl<S, C> SpoolInput for AsyncEdgeJoinCursor<S, C>
where
    S: AsyncEdgeJoinSource<Error = crate::BufferedReadError, OutputGuard = MemoryCharge>,
    // `Result` here is the spool's one-parameter alias, so name the
    // checkpoint's own result type.
    C: FnMut() -> core::result::Result<(), Cancel> + Send,
{
    // Type-erased like the buffered vertex cursor's pull: the buffered sort's
    // Send proof stops at `dyn Future + Send` instead of descending into the
    // join driver.
    fn pull(&mut self) -> impl Future<Output = Option<Result<SpoolRow>>> + Send {
        let pulled: crate::SendFuture<'_, _> = Box::pin(async move {
            self.next().await.map(|row| {
                row.map(|row| SpoolRow::buffered(row.into_parts()))
                    .map_err(|error| {
                        NativeSpoolError::BufferedExecute(Box::new(
                            error.map_source(ScanError::Edge),
                        ))
                    })
            })
        });
        pulled
    }
    fn spool_state(&self) -> ScanState {
        use fgdb_gql::edge_stream::EdgeScanState;
        match self.state() {
            EdgeScanState::Open => ScanState::Open,
            EdgeScanState::Exhausted => ScanState::Exhausted,
            EdgeScanState::Closed => ScanState::Closed,
            EdgeScanState::Failed => ScanState::Failed,
        }
    }
    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        (
            self.snapshot_seq(),
            ScanKind::Edge,
            self.row_stats(),
            self.evaluator_stats(),
        )
    }
}

impl<S, C> SpoolInput for AsyncGraphWalkCursor<S, C>
where
    S: AsyncGraphWalkSource<Error = crate::BufferedReadError, OutputGuard = MemoryCharge>,
    C: FnMut() -> core::result::Result<(), Cancel> + Send,
{
    fn pull(&mut self) -> impl Future<Output = Option<Result<SpoolRow>>> + Send {
        let pulled: crate::SendFuture<'_, _> = Box::pin(async move {
            self.next().await.map(|row| {
                row.map(|row| SpoolRow::buffered(row.into_parts()))
                    .map_err(|error| {
                        NativeSpoolError::BufferedExecute(Box::new(
                            error.map_source(ScanError::Edge),
                        ))
                    })
            })
        });
        pulled
    }

    fn spool_state(&self) -> ScanState {
        use fgdb_gql::edge_stream::EdgeScanState;
        match self.state() {
            EdgeScanState::Open => ScanState::Open,
            EdgeScanState::Exhausted => ScanState::Exhausted,
            EdgeScanState::Closed => ScanState::Closed,
            EdgeScanState::Failed => ScanState::Failed,
        }
    }

    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        (
            self.snapshot_seq(),
            ScanKind::Edge,
            self.row_stats(),
            self.evaluator_stats(),
        )
    }
}

#[cfg(test)]
#[path = "buffered_join_tests.rs"]
mod join_tests;
