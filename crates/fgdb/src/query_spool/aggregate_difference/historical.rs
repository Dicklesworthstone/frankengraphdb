//! One bound aggregate, two cold historical cuts, and the existing exact merge.
//! Endpoint queries remain private until both complete; no resident result bag.

use super::*;

type DifferenceResult<T> =
    core::result::Result<T, GqlQueryError<NativeAggregateSpoolError, Cancel>>;

fn source<T>(result: Result<T>) -> DifferenceResult<T> {
    result.map_err(GqlQueryError::Source)
}

fn remaining(limit: u64, spent: u64) -> Result<u64> {
    limit
        .checked_sub(spent)
        .ok_or_else(|| NativeSpoolError::IncompleteCursor.into())
}

fn total_work(prior: u64, spent: u64, limit: u64) -> Result<u64> {
    let total = prior.checked_add(spent).ok_or(SpillError::SizeOverflow)?;
    remaining(limit, total)?;
    Ok(total)
}

fn private_policy(mut policy: GqlQueryPolicy) -> GqlQueryPolicy {
    policy.rows = match policy.rows.max_snapshot_records() {
        Some(records) => GqlExecutionBudget::snapshot_records(records),
        None => GqlExecutionBudget::UNLIMITED,
    };
    policy
}

// EOF does not refund the source/reducer's cumulative logical charges. Only
// delivery rows are excluded: endpoint rows are PRIVATE inputs to subtraction.
fn after_policy(policy: GqlQueryPolicy, first: &NativeAggregateSpool) -> Result<GqlQueryPolicy> {
    let mut policy = private_policy(policy);
    if let Some(limit) = policy.rows.max_snapshot_records() {
        policy.rows = GqlExecutionBudget::snapshot_records(remaining(
            limit,
            first.row_stats().snapshot_records,
        )?);
    }
    let used = first.evaluator_stats();
    policy.evaluator.max_work_units = remaining(policy.evaluator.max_work_units, used.work_units)?;
    policy.evaluator.max_scratch_entries =
        remaining(policy.evaluator.max_scratch_entries, used.scratch_entries)?;
    Ok(policy)
}

fn prepare<V: Vfs + Clone>(
    prepared: &PreparedNativeRead,
    view: &BufferedReadView<V>,
    cx: &QueryCx,
    params: &GqlParameters,
    before: CommitSeq,
    after: CommitSeq,
) -> Result<PreparedBufferedAggregate> {
    // Admit BOTH revisions before binding, including equal cuts, source-free
    // aggregates and LIMIT 0. Do not silently clamp to the pinned publication.
    for requested in [before, after] {
        let publication = view.frontier();
        if requested > publication {
            return Err(NativeSpoolError::BufferedRead(Box::new(
                crate::BufferedReadError::BeyondPublication {
                    requested,
                    publication,
                },
            ))
            .into());
        }
    }
    cx.with_restriction(|| cx.checkpoint())
        .map_err(SpillError::Interrupted)?;
    if !matches!(
        prepared,
        PreparedNativeRead::Aggregate(_) | PreparedNativeRead::PipelineAggregate(_)
    ) {
        // An explicit temporal selector must not override the endpoint pair.
        // Other result domains keep their own physical executors; no fallback.
        return Err(NativeAggregateSpoolError::Unsupported);
    }
    let bound = prepared.prepare_buffered_aggregate(params)?;
    if bound.as_of.is_some() {
        return Err(NativeAggregateSpoolError::Unsupported);
    }
    cx.with_restriction(|| cx.checkpoint())
        .map_err(SpillError::Interrupted)?;
    Ok(bound)
}

impl PreparedNativeRead {
    /// Compare this aggregate at two exact revisions of ONE cold read view.
    /// Bind and compile its complete definition once, synchronously, before
    /// source reads or scratch reservations. The returned future owns that
    /// program; this template and params may drop or change immediately.
    /// The view pins both cuts and is exclusively borrowed through completion.
    /// No resident Snapshot or endpoint/difference result table is constructed.
    ///
    /// Each cut uses PreparedBufferedAggregate's existing source, partitioned
    /// reducer and COMPLETE result clauses. Supported relational inputs keep
    /// every child source/window. HAVING, computed output, RETURN DISTINCT,
    /// ordering and pagination finish before subtraction. u64 counts, i128
    /// sums, exact averages and native value/list domains are never narrowed.
    /// Only visible RETURN tuples determine changes: hidden group keys cannot
    /// distinguish equal public rows. Equal cuts still execute both endpoints.
    /// LIMIT 0 cannot hide late source or aggregate-expression failures.
    /// Explicit temporal selectors and unsupported physical profiles refuse;
    /// this adapter does not expand the buffered aggregate compiler's subset.
    ///
    /// policy's source-record/evaluator-work/scratch allowances cover BOTH
    /// complete endpoint evaluations. The second gets the first's measured
    /// remainder, including reduction and result-clause work. Endpoint output
    /// is private; only final nonzero changed tuples spend ResultRows. The
    /// max_input_rows, group_capacity, max_partitions and max_runs parameters
    /// keep their existing per-execution/reducer/sort meanings. Append-only
    /// file quotas and memory pools remain cumulative and are never refunded
    /// by cancellation of equal tuples or abandoned intermediate runs.
    ///
    /// One additional max_work_units covers both complete spill pipelines,
    /// both difference sorts, decoding, projection and callback checkpoints.
    /// Phase errors report the primitive's residual allowance; callback errors
    /// are preserved, not rewritten as accounting failures. Success returns
    /// (public columns, changed tuples, TOTAL additional physical work), not
    /// source work. Column names move out of the bound program without cloning.
    ///
    /// All three files must be distinct query-private owners. Endpoint runs
    /// append to destination; comparison sorts them into source, reusing the
    /// existing authenticated sorter. Use children of a common MemoryPool for
    /// a combined tracked bound. Cold-open recovery, plans/catalogs and caller
    /// copies retain their existing admission contracts. The same pinned
    /// scalar resolver is used for BOTH evaluations and difference decoding.
    ///
    /// The callback receives a signed multiplicity and a memory-charged row
    /// with no keys; values() is in public RETURN order. Its future completes
    /// before another change is consumed. No callback runs until both endpoint
    /// results have completed. A later comparison, callback or cancellation can
    /// still fail after an earlier callback: stage changes until this operation
    /// succeeds. Dropping it drains/acknowledges nothing and never clears file
    /// poisoning. This owner-level API is not authorized or cross-branch DIFF,
    /// a durable resume token, a retention lease, or automatic query planning.
    #[allow(clippy::too_many_arguments)]
    pub fn visit_buffered_aggregate_difference_in_view<'q, V, A, B, C, F, Fut>(
        &self,
        view: &'q mut BufferedReadView<V>,
        cx: &'q QueryCx,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
        source_file: &'q mut SpillFile<A>,
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
        visitor: F,
    ) -> crate::SendFuture<'q, DifferenceResult<(Vec<String>, u64, u64)>>
    where
        V: Vfs + Clone + 'q,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        F: FnMut(i128, NativeAggregateSpoolRow) -> Fut + Send + 'q,
        Fut: Future<Output = Result<()>> + Send + 'q,
    {
        let opened = prepare(self, view, cx, params, before, after);
        Box::pin(async move {
            let mut bound = source(opened)?;
            source(validate_limits(
                group_capacity,
                max_partitions,
                run_rows,
                max_runs,
                page_bytes,
                max_row_bytes,
            ))?;
            // Refuse an impossible work allowance BEFORE opening a source or
            // writer, including a completely source-free/empty query.
            if max_work_units == 0 {
                return source(Err(NativeSpoolError::SortWorkLimit {
                    attempted: 1,
                    limit: 0,
                }
                .into()));
            }
            let input_policy = private_policy(policy);
            bound.as_of = Some(before);
            let (first, used) = source(
                bound
                    .spool_in_view(
                        view,
                        cx,
                        input_policy,
                        &mut *source_file,
                        &mut *partition,
                        &mut *destination,
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
                    .await,
            )?;
            if first.snapshot_seq() != before {
                return source(Err(NativeSpoolError::IncompleteCursor.into()));
            }
            let input_policy = source(after_policy(input_policy, &first))?;
            let allowance = source(remaining(max_work_units, used))?;
            bound.as_of = Some(after);
            let (second, spent) = source(
                bound
                    .spool_in_view(
                        view,
                        cx,
                        input_policy,
                        &mut *source_file,
                        &mut *partition,
                        &mut *destination,
                        group_capacity,
                        max_partitions,
                        run_rows,
                        max_runs,
                        page_bytes,
                        max_row_bytes,
                        max_input_rows,
                        allowance,
                        resolver,
                    )
                    .await,
            )?;
            if second.snapshot_seq() != after {
                return source(Err(NativeSpoolError::IncompleteCursor.into()));
            }
            let columns = std::mem::take(&mut bound.columns);
            drop(bound);
            let used = source(total_work(used, spent, max_work_units))?;
            let allowance = source(remaining(max_work_units, used))?;
            let (changed, spent) = first
                .visit_difference(
                    &second,
                    cx,
                    destination,
                    source_file,
                    run_rows,
                    max_runs,
                    page_bytes,
                    policy.rows.max_result_rows().unwrap_or(u64::MAX),
                    allowance,
                    resolver,
                    visitor,
                )
                .await?;
            Ok((
                columns,
                changed,
                source(total_work(used, spent, max_work_units))?,
            ))
        })
    }
}

#[cfg(test)]
#[path = "historical/tests.rs"]
mod tests;
