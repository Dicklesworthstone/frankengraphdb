//! One native query opening, one source drain, and the existing external sort.
//! The returned future owns its immutable source cursor, never the writer.

use super::*;

#[path = "native_order.rs"]
mod native_order;

impl PreparedNativeRead {
    /// Evaluate a native pull query and externally order its selected result.
    ///
    /// Opening has the SAME synchronous health, snapshot and physical-plan
    /// admission as `spool`. The future retains the native cursor but borrows
    /// neither this template, parameters nor database: they may change or drop
    /// after this call. Only `cx`, the two scratch files and `order` are borrowed
    /// until completion. An unpolled future reserves no scratch or sort memory.
    ///
    /// This is exactly native `spool` followed by `sort_into`, not an eager
    /// fallback or another evaluator. All predicates, multiplicity and the
    /// original query's SKIP/LIMIT run FIRST; `order` sorts that selected result.
    /// In particular, sorting LIMIT 10 does not choose a different ten rows.
    /// Unsupported native streaming shapes still refuse during opening.
    ///
    /// Invalid order, zero run_rows/max_runs, invalid page size and an impossible
    /// zero sort-work allowance refuse before the first source pull or scratch
    /// writer. The exact run count is admitted once the source has completed.
    /// Native execution keeps its original policy; max_work_units is a separate
    /// additional sort allowance, never a reset of the native execution budget.
    ///
    /// The native source pin is released before merge sorting. Only a completely
    /// accepted result in destination escapes. Failed or dropped operations keep
    /// the existing append-only quota and poisoned-file cleanup laws; successful
    /// intermediate runs are not deleted or refunded. Source and destination
    /// must be distinct query-private files and should share a parent memory
    /// pool. The returned counter is sort work; source statistics remain native.
    ///
    /// One native row and its canonical encoding remain outside the spill pool,
    /// as documented by `spool`; decoded graph storage also remains resident.
    /// No automatic ORDER BY planning, Warden grant, durable result or physical
    /// I/O bound is implied. `sort_into` documents minimum memory and run limits.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_sorted<'q, V, A, B>(
        &self,
        database: &Database<V>,
        cx: &'q QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        source: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        order: &'q [GraphValueOrder],
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_work_units: u64,
    ) -> impl Future<Output = Result<(NativeResultSpool, u64)>> + 'q + use<'q, V, A, B>
    where
        V: Vfs + Clone,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
    {
        let opened = self.stream(database, cx, params, policy);
        async move {
            let (columns, cursor) = opened.map_err(|e| NativeSpoolError::Prepare(Box::new(e)))?;
            drain_sorted(
                cx,
                columns,
                cursor,
                source,
                destination,
                order,
                run_rows,
                max_runs,
                page_bytes,
                max_row_bytes,
                max_work_units,
            )
            .await
        }
    }

    /// Sort the native result from an already admitted view. Temporal selectors
    /// still bind their own exact cut within that view. This clones only its
    /// source pin at opening, and never reacquires the writer's newer snapshot.
    /// All ordering, limits and failure contracts are those of `spool_sorted`.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_sorted_in_view<'q, A, B>(
        &self,
        view: &EmbeddedReadView,
        cx: &'q QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        source: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        order: &'q [GraphValueOrder],
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_work_units: u64,
    ) -> impl Future<Output = Result<(NativeResultSpool, u64)>> + 'q + use<'q, A, B>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
    {
        let opened = self.stream_in_view(view, cx, params, policy);
        async move {
            let (columns, cursor) = opened.map_err(|e| NativeSpoolError::Prepare(Box::new(e)))?;
            drain_sorted(
                cx,
                columns,
                cursor,
                source,
                destination,
                order,
                run_rows,
                max_runs,
                page_bytes,
                max_row_bytes,
                max_work_units,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain_sorted<VS, VF, ES, EF, A, B>(
    cx: &QueryCx,
    columns: Vec<String>,
    cursor: ScanCursor<VS, VF, ES, EF>,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    order: &[GraphValueOrder],
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_work_units: u64,
) -> Result<(NativeResultSpool, u64)>
where
    VS: VertexScanSource<Error = ReadError>,
    ES: EdgeScanSource<Error = ReadError>,
    VF: FnMut() -> core::result::Result<(), Cancel>,
    EF: FnMut() -> core::result::Result<(), Cancel>,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    cx.with_restriction(|| cx.checkpoint())
        .map_err(SpillError::Interrupted)?;
    validate_order(order, columns.len())?;
    if run_rows == 0 || page_bytes == 0 || page_bytes > 64 * 1024 {
        return Err(SpillError::InvalidLimits.into());
    }
    if max_runs == 0 {
        return Err(NativeSpoolError::SortRunLimit {
            required: 1,
            limit: 0,
        });
    }
    if max_work_units == 0 {
        return Err(NativeSpoolError::SortWorkLimit {
            attempted: 1,
            limit: 0,
        });
    }
    let encoded_columns = columns.len();
    let spool = drain(
        cx,
        columns,
        encoded_columns,
        cursor,
        source,
        page_bytes,
        max_row_bytes,
    )
    .await?;
    spool
        .sort_into(
            cx,
            source,
            destination,
            order,
            run_rows,
            max_runs,
            page_bytes,
            max_work_units,
        )
        .await
}
