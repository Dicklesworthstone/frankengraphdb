//! Compile the native terminal order/window into the existing spill pipeline.
//! No text rewriting, identity-prefix injection, eager fallback or new evaluator.

use super::*;
use fgdb_gql::scan_stream::ScanSortTail;
use fgdb_gql::stream::{VertexScanCursor, VertexScanPlan};
use fgdb_gql::{GqlBudgetDimension, GqlExecutionBudget};

fn input_policy(policy: GqlQueryPolicy, max_input_rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy {
        // These are PRIVATE input occurrences. The original output allowance
        // is enforced only after ordering and pagination, never spent twice.
        rows: match policy.rows.max_snapshot_records() {
            Some(records) => GqlExecutionBudget::new(records, max_input_rows),
            None => GqlExecutionBudget::result_rows(max_input_rows),
        },
        evaluator: policy.evaluator,
    }
}

fn open<'q>(
    prepared: &PreparedNativeRead,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    params: &GqlParameters,
    policy: GqlQueryPolicy,
    max_input_rows: u64,
) -> core::result::Result<
    (Vec<String>, impl SpoolInput + 'q + use<'q>, ScanSortTail),
    QueryError,
> {
    let (query, as_of) = match prepared {
        PreparedNativeRead::Pattern(template) => (
            template.bind_parameters(params).map_err(QueryError::PatternText)?,
            view.frontier(),
        ),
        PreparedNativeRead::TemporalPattern(template) => {
            let bound = template.bind_parameters(params).map_err(QueryError::TemporalText)?;
            (bound.pattern().clone(), bound.as_of())
        }
        _ => return Err(QueryError::StreamingUnsupported { facade: prepared.facade_class() }),
    };
    // Exact-cut admission precedes physical compilation, as for ordinary scans.
    // This clones only the existing immutable source pin, never candidate rows.
    let source = view.vertex_scan_source(cx, as_of)
        .map_err(|e| QueryError::Stream(GqlQueryError::Source(VertexScanError::Source(e))))?;
    let (plan, tail) = VertexScanPlan::compile_sort_input(query.plan())
        .map_err(|e| QueryError::Stream(GqlQueryError::Source(VertexScanError::Plan(e))))?;
    cx.with_restriction(|| cx.checkpoint())
        .map_err(|e| QueryError::Stream(GqlQueryError::Interrupted(e)))?;
    let cursor = VertexScanCursor::new(source, plan, input_policy(policy, max_input_rows), move || {
        cx.with_restriction(|| cx.checkpoint())
    });
    Ok((query.columns().to_vec(), cursor, tail))
}

impl PreparedNativeRead {
    /// Execute a single-vertex native query with external ORDER BY, THEN SKIP/LIMIT.
    ///
    /// Unlike spool_sorted's caller-supplied post-selection order, this compiles
    /// the query's OWN terminal clauses from its bound GLA. Property-only and
    /// property-first projections, explicit direction/null placement, implicit
    /// canonical whole-row ordering, predicates and supported EXISTS probes use
    /// the ordinary vertex collector and source. No synthetic identity column
    /// changes DISTINCT, ties or the public schema. The current profile refuses
    /// DISTINCT, edge-rooted/relational/aggregate plans, hidden sort columns and
    /// every operator the vertex compiler cannot execute. There is no fallback.
    ///
    /// Opening binds parameters and pins the exact source synchronously. The
    /// future borrows ONLY cx and the two scratch files; writer/template/params
    /// may change or drop. Unpolled futures reserve no scratch. Unsupported
    /// shapes refuse even under LIMIT 0. Complete source evaluation and sorting
    /// precede pagination, so a small page cannot hide a later data exception.
    ///
    /// policy's source-record/work/scratch limits govern one source traversal.
    /// max_input_rows is a separate intermediate-occurrence ceiling (reported
    /// by the input cursor's ResultRows error). policy.rows.max_result_rows is
    /// enforced on the FINAL selected rows, including zero, before acceptance.
    /// The returned spool reports source snapshot records, final result count
    /// and the original source evaluator statistics. The second returned value
    /// is ADDITIONAL sort/window work under max_sort_work: neither the sort nor
    /// the final window resets that allowance. Spill buffers/catalogs are byte-
    /// charged to their existing pools; source logical scratch remains separate.
    ///
    /// Both files must be distinct query-private files; use children of a common
    /// pool for a combined resident bound. Runs append without quota refunds or
    /// recycling. Intermediates initially use destination, sort into scratch,
    /// then the final page is appended to destination. Only its complete result
    /// handle escapes. Failures/drop keep the existing poisoning/cleanup laws.
    /// sort_into documents run_rows/max_runs/page_bytes and minimum headroom.
    /// One native row plus its canonical encoding and the decoded source remain
    /// outside the spill pool. This is not a Warden grant, durable result, or a
    /// claim that every GQL operator or the underlying database is out-of-core.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_ordered<'q, V, A, B>(
        &self,
        database: &Database<V>,
        cx: &'q QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_sort_work: u64,
    ) -> impl Future<Output = Result<(NativeResultSpool, u64)>> + 'q + use<'q, V, A, B>
    where
        V: Vfs + Clone,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
    {
        let opened = database.read_session()
            .map_err(|e| QueryError::Stream(GqlQueryError::Source(VertexScanError::Source(e))))
            .and_then(|view| open(self, &view, cx, params, policy, max_input_rows));
        async move {
            let (columns, cursor, tail) = opened.map_err(|e| NativeSpoolError::Prepare(Box::new(e)))?;
            evaluate(cx, columns, cursor, tail, policy, scratch, destination, run_rows,
                max_runs, page_bytes, max_row_bytes, max_sort_work).await
        }
    }

    /// The same query-owned external order/window at this already-pinned view.
    /// A temporal selector uses its bound historical cut, not a newer live head.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_ordered_in_view<'q, A, B>(
        &self,
        view: &EmbeddedReadView,
        cx: &'q QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_sort_work: u64,
    ) -> impl Future<Output = Result<(NativeResultSpool, u64)>> + 'q + use<'q, A, B>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
    {
        let opened = open(self, view, cx, params, policy, max_input_rows);
        async move {
            let (columns, cursor, tail) = opened.map_err(|e| NativeSpoolError::Prepare(Box::new(e)))?;
            evaluate(cx, columns, cursor, tail, policy, scratch, destination, run_rows,
                max_runs, page_bytes, max_row_bytes, max_sort_work).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn evaluate<I, A, B>(
    cx: &QueryCx,
    columns: Vec<String>,
    cursor: I,
    tail: ScanSortTail,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_sort_work: u64,
) -> Result<(NativeResultSpool, u64)>
where
    I: SpoolInput,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    cx.with_restriction(|| cx.checkpoint()).map_err(SpillError::Interrupted)?;
    validate_order(tail.order(), columns.len())?;
    if run_rows == 0 || page_bytes == 0 || page_bytes > 64 * 1024 {
        return Err(SpillError::InvalidLimits.into());
    }
    if max_runs == 0 {
        return Err(NativeSpoolError::SortRunLimit { required: 1, limit: 0 });
    }
    if max_sort_work == 0 {
        return Err(NativeSpoolError::SortWorkLimit { attempted: 1, limit: 0 });
    }
    let input = drain(cx, columns, cursor, destination, page_bytes, max_row_bytes).await?;
    let (sorted, used) = input.sort_into(cx, destination, scratch, tail.order(), run_rows,
        max_runs, page_bytes, max_sort_work).await?;
    let mut work = Work { cx, used, limit: max_sort_work };
    let result = window(&sorted, scratch, destination, &tail, policy.rows, page_bytes, &mut work).await?;
    Ok((result, work.used))
}

async fn window<A, B>(
    sorted: &NativeResultSpool,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    tail: &ScanSortTail,
    budget: GqlExecutionBudget,
    page_bytes: usize,
    work: &mut Work<'_>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    debug_assert!(!tail.distinct());
    let mut reader = sorted.reader(scratch);
    let mut writer = destination.paged_writer(work.cx, page_bytes)?;
    let mut skip = tail.offset();
    let mut selected = 0_u64;
    let mut largest = 0;
    while let Some(row) = reader.next_row(work.cx).await? {
        work.charge(1)?;
        if skip != 0 {
            skip -= 1;
            continue;
        }
        if tail.count().is_some_and(|count| selected >= count) {
            continue;
        }
        let next = selected.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        budget.check(GqlBudgetDimension::ResultRows, next)
            .map_err(|e| NativeSpoolError::Execute(Box::new(GqlQueryError::Rows(e))))?;
        write_row(&mut writer, row.as_ref(), work).await?;
        largest = largest.max(row.len());
        selected = next;
    }
    // Read and authenticate the complete sorted population even after LIMIT.
    // No source/transfer failure is converted into a successful page prefix.
    if reader.state() != ScanState::Exhausted {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    drop(reader);
    work.charge(1)?;
    let run = writer.finish(work.cx).await?;
    Ok(NativeResultSpool {
        columns: Arc::clone(&sorted.columns),
        snapshot: sorted.snapshot,
        kind: sorted.kind,
        rows: GqlExecutionStats { snapshot_records: sorted.rows.snapshot_records, result_rows: selected },
        evaluator: sorted.evaluator,
        max_row_bytes: largest,
        run,
    })
}

#[cfg(test)]
#[path = "native_order_tests.rs"]
mod tests;
