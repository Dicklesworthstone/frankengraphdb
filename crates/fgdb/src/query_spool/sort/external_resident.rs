//! Exact-cut resident graph leaves for the shared external relational engine.

use super::*;
use fgdb_types::CanonicalScalarResolver;

pub(super) enum ResidentOrder<I> {
    Native(OpenedOrder<I>),
    Relation {
        plan: AsyncSpillSetPlan,
        view: EmbeddedReadView,
        snapshot: CommitSeq,
    },
}

/// Parameter binding, physical admission and source pinning happen before the
/// future is returned. Neither a later writer nor parameter rebinding can move
/// a correlated body's graph or element reads to another generation/cut.
pub(super) fn open<'q>(
    prepared: &PreparedNativeRead,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    params: &GqlParameters,
    policy: GqlQueryPolicy,
    max_input_rows: u64,
) -> core::result::Result<ResidentOrder<impl SpoolInput + 'q + use<'q>>, QueryError> {
    let (set, snapshot) = match prepared {
        PreparedNativeRead::Set(template) => (
            template
                .bind_parameters(params)
                .map_err(QueryError::SetText)?,
            view.frontier(),
        ),
        PreparedNativeRead::TemporalSet(template) => {
            let bound = template
                .bind_parameters(params)
                .map_err(QueryError::TemporalSetText)?;
            (bound.query().clone(), bound.as_of())
        }
        _ => {
            return super::open(prepared, view, cx, params, policy, max_input_rows, None)
                .map(ResidentOrder::Native);
        }
    };
    view.snapshot.check_frontier(snapshot).map_err(|error| {
        QueryError::Stream(GqlQueryError::Source(VertexScanError::Source(error)))
    })?;
    if set.ordered_row_pull().is_some() {
        // Retain the native compiler's proven projection fusion and its exact
        // source counters for already admitted linear plans. Binding remains
        // a single synchronous operation; only nonfusible relations recurse.
        return super::open(
            prepared,
            view,
            cx,
            params,
            policy,
            max_input_rows,
            Some((set, snapshot)),
        )
        .map(ResidentOrder::Native);
    }
    let plan = AsyncSpillSetPlan::compile(&set).map_err(|_| QueryError::StreamingUnsupported {
        facade: prepared.facade_class(),
    })?;
    Ok(ResidentOrder::Relation {
        plan,
        view: view.clone(),
        snapshot,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<I, A, B, C>(
    opened: ResidentOrder<I>,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    auxiliary: &mut SpillFile<C>,
    limits: ExternalOrderLimits,
) -> Result<(NativeResultSpool, u64)>
where
    I: SpoolInput,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    match opened {
        ResidentOrder::Native(opened) => {
            execute_opened(
                opened,
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
            )
            .await
        }
        ResidentOrder::Relation {
            plan,
            view,
            snapshot,
        } => {
            let mut host = ResidentHost { view };
            execute_relation(
                &plan,
                None,
                snapshot,
                &mut host,
                cx,
                policy,
                scratch,
                destination,
                auxiliary,
                limits,
                None,
            )
            .await
            .map_err(resident_stage_error)
        }
    }
}

struct ResidentHost {
    view: EmbeddedReadView,
}

impl ExternalOrderHost for ResidentHost {
    fn check_snapshot(&mut self, snapshot: CommitSeq) -> Result<()> {
        self.view
            .snapshot
            .check_frontier(snapshot)
            .map_err(|error| {
                NativeSpoolError::Execute(Box::new(GqlQueryError::Source(ScanError::Vertex(
                    VertexScanError::Source(error),
                ))))
            })
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
            let query = plan.source_pattern().ok_or(SpillError::InvalidRun)?;
            let tail = plan.source_tail().cloned().ok_or(SpillError::InvalidRun)?;
            let cursor = if matches!(
                query.plan().operators().first(),
                Some(GlaOperator::ScanEdges { .. })
            ) {
                let source = self.view.edge_scan_source(cx, snapshot).map_err(|error| {
                    NativeSpoolError::Execute(Box::new(GqlQueryError::Source(ScanError::Edge(
                        EdgeScanError::Source(error),
                    ))))
                })?;
                let (source_plan, _) =
                    EdgeScanPlan::compile_sort_input(query.plan()).map_err(|error| {
                        NativeSpoolError::Execute(Box::new(GqlQueryError::Source(ScanError::Edge(
                            EdgeScanError::Plan(error),
                        ))))
                    })?;
                ScanCursor::Edge(Box::new(EdgeScanCursor::new(
                    source,
                    source_plan,
                    policy,
                    move || cx.with_restriction(|| cx.checkpoint()),
                )))
            } else {
                let source = self
                    .view
                    .vertex_scan_source(cx, snapshot)
                    .map_err(|error| {
                        NativeSpoolError::Execute(Box::new(GqlQueryError::Source(
                            ScanError::Vertex(VertexScanError::Source(error)),
                        )))
                    })?;
                let (source_plan, _) =
                    VertexScanPlan::compile_sort_input(query.plan()).map_err(|error| {
                        NativeSpoolError::Execute(Box::new(GqlQueryError::Source(
                            ScanError::Vertex(VertexScanError::Plan(error)),
                        )))
                    })?;
                ScanCursor::Vertex(VertexScanCursor::new(
                    source,
                    source_plan,
                    policy,
                    move || cx.with_restriction(|| cx.checkpoint()),
                ))
            };
            ordered_input(
                cx,
                plan.source_columns().to_vec(),
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
        Box::pin(async move {
            let mut elements = ResidentElements {
                view: &self.view,
                as_of: input.snapshot_seq(),
                budget: policy.rows,
                records: input.row_stats().snapshot_records,
            };
            let (mut output, work) = staged::execute_with_elements(
                plan,
                input,
                &mut elements,
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
            )
            .await
            .map_err(resident_stage_error)?;
            output.rows.snapshot_records = elements.records;
            Ok((output, work))
        })
    }
}
