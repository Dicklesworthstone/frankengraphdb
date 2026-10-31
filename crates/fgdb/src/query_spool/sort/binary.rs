//! Bounded binary set execution over complete authenticated child relations.
//! Both children finish in source order. Their selected rows are then sorted
//! canonically and merged with the same decision kernel as resident SetOp.

use super::*;
use crate::query::view::spool::sort::{Input as MergeInput, Run};
use fgdb_gql::spill_set::{AsyncSpillSetPlan, SpillSetBinaryPlan, SpillSetMergeStep};
use fgdb_gql::{
    GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlBudgetExceeded, GraphSetQuantifier,
};
use fgdb_types::CanonicalScalarResolver;

// A child owns its ordinary cursor and counters. Pass only the remaining
// allowance to the next child, then translate its refusal back into the one
// enclosing query's counters. Result rows remain private per-operator limits.
pub(super) fn remaining_policy(
    policy: GqlQueryPolicy,
    left: &NativeResultSpool,
) -> Result<GqlQueryPolicy> {
    let remaining = |limit: u64, used: u64| {
        limit
            .checked_sub(used)
            .ok_or(NativeSpoolError::IncompleteCursor)
    };
    Ok(GqlQueryPolicy {
        rows: match policy.rows.max_snapshot_records() {
            Some(limit) => GqlExecutionBudget::new(
                remaining(limit, left.rows.snapshot_records)?,
                policy.rows.max_result_rows().unwrap_or(u64::MAX),
            ),
            None => {
                GqlExecutionBudget::result_rows(policy.rows.max_result_rows().unwrap_or(u64::MAX))
            }
        },
        evaluator: fgdb_gql::GlaExecutionLimits::new(
            remaining(policy.evaluator.max_work_units, left.evaluator.work_units)?,
            remaining(
                policy.evaluator.max_scratch_entries,
                left.evaluator.scratch_entries,
            )?,
        ),
    })
}

fn rebase_query_error<E, C>(
    error: GqlQueryError<E, C>,
    policy: GqlQueryPolicy,
    left: &NativeResultSpool,
) -> Result<GqlQueryError<E, C>> {
    Ok(match error {
        GqlQueryError::Evaluator(error) => {
            let (used, limit) = match error.dimension {
                GlaLimitDimension::WorkUnits => {
                    (left.evaluator.work_units, policy.evaluator.max_work_units)
                }
                GlaLimitDimension::ScratchEntries => (
                    left.evaluator.scratch_entries,
                    policy.evaluator.max_scratch_entries,
                ),
            };
            GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension: error.dimension,
                limit,
                observed: error.observed.saturating_add(u128::from(used)),
            })
        }
        GqlQueryError::Rows(error) if error.dimension == GqlBudgetDimension::SnapshotRecords => {
            GqlQueryError::Rows(GqlBudgetExceeded {
                dimension: error.dimension,
                limit: policy.rows.max_snapshot_records().unwrap_or(u64::MAX),
                observed: left
                    .rows
                    .snapshot_records
                    .checked_add(error.observed)
                    .ok_or(SpillError::SizeOverflow)?,
            })
        }
        other => other,
    })
}

pub(super) fn rebase_error(
    error: NativeSpoolError,
    policy: GqlQueryPolicy,
    left: &NativeResultSpool,
    prior_work: u64,
    work_limit: u64,
) -> NativeSpoolError {
    let rebased = match error {
        NativeSpoolError::BufferedExecute(error) => rebase_query_error(*error, policy, left)
            .map(|error| NativeSpoolError::BufferedExecute(Box::new(error))),
        NativeSpoolError::Execute(error) => rebase_query_error(*error, policy, left)
            .map(|error| NativeSpoolError::Execute(Box::new(error))),
        NativeSpoolError::Procedure(error) => rebase_query_error(*error, policy, left)
            .map(|error| NativeSpoolError::Procedure(Box::new(error))),
        NativeSpoolError::Authorized(error) => {
            let mapped = match *error {
                QueryError::Stream(error) => {
                    rebase_query_error(error, policy, left).map(QueryError::Stream)
                }
                QueryError::EdgeStream(error) => {
                    rebase_query_error(error, policy, left).map(QueryError::EdgeStream)
                }
                QueryError::Set(error) => {
                    rebase_query_error(error, policy, left).map(QueryError::Set)
                }
                QueryError::Pattern(error) => {
                    rebase_query_error(error, policy, left).map(QueryError::Pattern)
                }
                QueryError::Aggregate(error) => {
                    rebase_query_error(error, policy, left).map(QueryError::Aggregate)
                }
                other => Ok(other),
            };
            mapped.map(|error| NativeSpoolError::Authorized(Box::new(error)))
        }
        NativeSpoolError::Aggregate(error) => match *error {
            NativeAggregateSpoolError::Execute(error) => rebase_query_error(*error, policy, left)
                .map(|error| NativeAggregateSpoolError::Execute(Box::new(error)).into()),
            NativeAggregateSpoolError::BufferedExecute(error) => {
                rebase_query_error(*error, policy, left)
                    .map(|error| NativeAggregateSpoolError::BufferedExecute(Box::new(error)).into())
            }
            NativeAggregateSpoolError::Spool(error) => {
                Ok(rebase_error(error, policy, left, prior_work, work_limit))
            }
            other => Ok(other.into()),
        },
        NativeSpoolError::SortWorkLimit { attempted, .. } => prior_work
            .checked_add(attempted)
            .map(|attempted| NativeSpoolError::SortWorkLimit {
                attempted,
                limit: work_limit,
            })
            .ok_or_else(|| SpillError::SizeOverflow.into()),
        other => Ok(other),
    };
    match rebased {
        Ok(error) | Err(error) => error,
    }
}

pub(super) struct CompletedChildren {
    pub(super) left: NativeResultSpool,
    pub(super) right: NativeResultSpool,
    pub(super) snapshot_records: u64,
    pub(super) evaluator: GlaExecutionStats,
    pub(super) work: u64,
}

/// Binary operators share complete-child sequencing, original snapshots and
/// one cumulative allowance. Reading an already completed child again never
/// spends source records or reexecutes its graph or expressions.
#[allow(clippy::too_many_arguments)]
pub(super) async fn complete_children<H, A, B, C>(
    left_plan: &AsyncSpillSetPlan,
    right_plan: &AsyncSpillSetPlan,
    seed_plan: Option<&fgdb_gql::spill_set::SpillSetCrossPlan>,
    arguments: Option<&GraphValueRow>,
    snapshot: CommitSeq,
    host: &mut H,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    auxiliary: &mut SpillFile<C>,
    group_capacity: usize,
    max_partitions: usize,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    max_sort_work: u64,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<CompletedChildren>
where
    H: ExternalOrderHost,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    cx.with_restriction(|| cx.checkpoint())
        .map_err(SpillError::Interrupted)?;
    validate_spill_limits(run_rows, max_runs, page_bytes, max_sort_work)?;
    let left_width = left_plan.columns().len();
    let right_width = right_plan.columns().len();
    let limits = ExternalOrderLimits {
        group_capacity,
        max_partitions,
        run_rows,
        max_runs,
        page_bytes,
        max_row_bytes,
        max_input_rows,
        max_sort_work,
    };
    let (mut left, mut left_work) = execute_relation(
        left_plan,
        arguments,
        snapshot,
        host,
        cx,
        policy,
        scratch,
        destination,
        auxiliary,
        limits,
        resolver,
    )
    .await?;
    // Only a selected product's single-root proof permits source narrowing.
    // The complete leading relation (including late errors) precedes every
    // seed decision. Large/unaffordable bags preserve the ordinary path.
    let seeds = if let Some(plan) = seed_plan {
        cross::seeds::prepare(
            plan,
            &mut left,
            host,
            cx,
            policy,
            destination,
            run_rows,
            &mut left_work,
            max_sort_work,
            resolver,
        )
        .await?
    } else {
        None
    };
    let right_policy = remaining_policy(policy, &left)?;
    let right_work_limit = max_sort_work
        .checked_sub(left_work)
        .ok_or(NativeSpoolError::IncompleteCursor)?;
    // Append-only files keep the complete left result alive while the right
    // child runs through the same files. Empty left bags do not skip it.
    let right_limits = ExternalOrderLimits {
        max_sort_work: right_work_limit,
        ..limits
    };
    let right_result = if let Some(seeds) = seeds {
        async {
            host.checkpoint()?;
            host.check_snapshot(snapshot)?;
            validate_spill_limits(
                right_limits.run_rows,
                right_limits.max_runs,
                right_limits.page_bytes,
                right_limits.max_sort_work,
            )?;
            let (input, work) = host
                .seeded_vertex_leaf(
                    right_plan,
                    seeds,
                    snapshot,
                    cx,
                    input_policy(right_policy, max_input_rows),
                    scratch,
                    destination,
                    right_limits,
                )
                .await?;
            host.checkpoint()?;
            host.stages(
                right_plan,
                input,
                cx,
                right_policy,
                scratch,
                destination,
                right_limits,
                work,
                resolver,
            )
            .await
        }
        .await
    } else {
        execute_relation(
            right_plan,
            arguments,
            snapshot,
            host,
            cx,
            right_policy,
            scratch,
            destination,
            auxiliary,
            right_limits,
            resolver,
        )
        .await
    };
    let (right, right_work) = right_result
        .map_err(|error| rebase_error(error, policy, &left, left_work, max_sort_work))?;
    if left.snapshot != snapshot
        || right.snapshot != snapshot
        || left.encoded_columns != left_width
        || right.encoded_columns != right_width
        || left.columns().len() != left_width
        || right.columns().len() != right_width
    {
        return Err(SpillError::InvalidRun.into());
    }
    let input_rows = left
        .row_count()
        .checked_add(right.row_count())
        .ok_or(SpillError::SizeOverflow)?;
    GqlExecutionBudget::result_rows(max_input_rows)
        .check(GqlBudgetDimension::ResultRows, input_rows)
        .map_err(|error| NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error))))?;
    let records = left
        .rows
        .snapshot_records
        .checked_add(right.rows.snapshot_records)
        .ok_or(SpillError::SizeOverflow)?;
    policy
        .rows
        .check(GqlBudgetDimension::SnapshotRecords, records)
        .map_err(|error| NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error))))?;
    let mut evaluator = GlaExecutionStats {
        work_units: left
            .evaluator
            .work_units
            .checked_add(right.evaluator.work_units)
            .ok_or(SpillError::SizeOverflow)?,
        scratch_entries: left
            .evaluator
            .scratch_entries
            .checked_add(right.evaluator.scratch_entries)
            .ok_or(SpillError::SizeOverflow)?,
    };
    host::event(host, cx, policy, &mut evaluator, GlaExecutionEvent::Work)?;
    let work = left_work
        .checked_add(right_work)
        .ok_or(SpillError::SizeOverflow)?;
    Ok(CompletedChildren {
        left,
        right,
        snapshot_records: records,
        evaluator,
        work,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<H, A, B, C>(
    plan: &SpillSetBinaryPlan,
    arguments: Option<&GraphValueRow>,
    snapshot: CommitSeq,
    host: &mut H,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    auxiliary: &mut SpillFile<C>,
    group_capacity: usize,
    max_partitions: usize,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    max_sort_work: u64,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<(NativeResultSpool, u64)>
where
    H: ExternalOrderHost,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let CompletedChildren {
        left,
        right,
        snapshot_records: records,
        mut evaluator,
        work: prior_work,
    } = complete_children(
        plan.left(),
        plan.right(),
        None,
        arguments,
        snapshot,
        host,
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
        resolver,
    )
    .await?;
    let width = plan.columns().len();
    let mut work = Work {
        cx,
        used: prior_work,
        limit: max_sort_work,
    };
    // Canonicalize only AFTER both original child windows and expressions
    // finish. A different child ORDER BY cannot change set multiplicities.
    let left = canonical_input(
        &left,
        destination,
        scratch,
        run_rows,
        max_runs,
        page_bytes,
        &mut work,
    )
    .await?;
    let right = canonical_input(
        &right,
        destination,
        scratch,
        run_rows,
        max_runs,
        page_bytes,
        &mut work,
    )
    .await?;
    let distinct = plan.quantifier() == GraphSetQuantifier::Distinct;
    let mut left = Head::new(left, distinct);
    let mut right = Head::new(right, distinct);
    left.advance(scratch, host, policy, &mut evaluator, &mut work)
        .await?;
    right
        .advance(scratch, host, policy, &mut evaluator, &mut work)
        .await?;
    let mut writer = destination.paged_writer(cx, page_bytes)?;
    let mut rows = 0_u64;
    let mut largest = 0;
    loop {
        let step = plan.next_with_control(
            left.row.as_ref(),
            right.row.as_ref(),
            &mut |event| host::event(host, cx, policy, &mut evaluator, event),
            &mut |a, b, control| {
                control(GlaExecutionEvent::Work)?;
                canonical::compare(a.as_ref(), b.as_ref(), &[], width, &mut work)
            },
        )?;
        let (emit_left, emit_right, advance_left, advance_right) = match step {
            SpillSetMergeStep::Done => break,
            SpillSetMergeStep::EmitLeft => (true, false, true, false),
            SpillSetMergeStep::EmitRight => (false, true, false, true),
            SpillSetMergeStep::EmitLeftAdvanceRight => (true, false, true, true),
            SpillSetMergeStep::AdvanceLeft => (false, false, true, false),
            SpillSetMergeStep::AdvanceRight => (false, false, false, true),
            SpillSetMergeStep::AdvanceBoth => (false, false, true, true),
        };
        if emit_left || emit_right {
            let row = if emit_left {
                left.row.as_ref()
            } else {
                right.row.as_ref()
            }
            .ok_or(NativeSpoolError::IncompleteCursor)?;
            let next = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            GqlExecutionBudget::result_rows(max_input_rows)
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(|error| {
                    NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
                })?;
            host::event(
                host,
                cx,
                policy,
                &mut evaluator,
                GlaExecutionEvent::ScratchEntry,
            )?;
            write_row(&mut writer, row.as_ref(), &mut work).await?;
            rows = next;
            largest = largest.max(row.len());
        }
        if advance_left {
            left.advance(scratch, host, policy, &mut evaluator, &mut work)
                .await?;
        }
        if advance_right {
            right
                .advance(scratch, host, policy, &mut evaluator, &mut work)
                .await?;
        }
    }
    // A native decision may know that the rest cannot contribute. Finish
    // authentication and quota/cancellation checks before accepting a result.
    while left.row.is_some() {
        left.advance(scratch, host, policy, &mut evaluator, &mut work)
            .await?;
    }
    while right.row.is_some() {
        right
            .advance(scratch, host, policy, &mut evaluator, &mut work)
            .await?;
    }
    if left.input.remaining != 0 || right.input.remaining != 0 {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    drop(left);
    drop(right);
    work.charge(1)?;
    let run = writer.finish(cx).await?;
    Ok((
        NativeResultSpool {
            columns: plan.columns().to_vec().into(),
            encoded_columns: width,
            snapshot,
            kind: ScanKind::Set,
            rows: GqlExecutionStats {
                snapshot_records: records,
                result_rows: rows,
            },
            evaluator,
            max_row_bytes: largest,
            run,
        },
        work.used,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn canonical_input<A, B>(
    input: &NativeResultSpool,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    work: &mut Work<'_>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let width = input.encoded_columns;
    if width == 0 {
        // Unit tuples all have equal native keys; an empty sort-key list is
        // intentionally unavailable to the public sorter, so copy this domain.
        return staged::copy(input, source, destination, page_bytes, work).await;
    }
    let charge = destination
        .memory_pool()
        .reserve(
            work.cx,
            width
                .checked_mul(size_of::<GraphValueOrder>())
                .ok_or(SpillError::SizeOverflow)?,
        )
        .map_err(SpillError::Memory)?;
    let order = (
        (0..width)
            .map(|column| GraphValueOrder::ascending(column).with_nulls_first(true))
            .collect::<Vec<_>>(),
        charge,
    );
    let (sorted, used) = input
        .sort_continuing(
            work.cx,
            source,
            destination,
            &order.0,
            run_rows,
            max_runs,
            page_bytes,
            work.limit,
            work.used,
        )
        .await?;
    work.used = used;
    Ok(sorted)
}

// Each head owns an authenticated page position and one tracked frame. When
// DISTINCT advances, its old frame survives only until a different tuple is
// found; multiplicity never becomes a resident hash table or count vector.
struct Head {
    input: MergeInput,
    row: Option<TrackedBytes>,
    columns: usize,
    distinct: bool,
}
impl Head {
    fn new(spool: NativeResultSpool, distinct: bool) -> Self {
        Self {
            input: MergeInput::new(
                Run {
                    pages: spool.run,
                    rows: spool.rows.result_rows,
                },
                spool.max_row_bytes,
            ),
            row: None,
            columns: spool.encoded_columns,
            distinct,
        }
    }

    async fn advance<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin, H: ExternalOrderHost>(
        &mut self,
        source: &mut SpillFile<F>,
        host: &mut H,
        policy: GqlQueryPolicy,
        evaluator: &mut GlaExecutionStats,
        work: &mut Work<'_>,
    ) -> Result<()> {
        let previous = self.row.take().filter(|_| self.distinct);
        loop {
            host::event(host, work.cx, policy, evaluator, GlaExecutionEvent::Work)?;
            let Some(row) = self.input.next(source, work.cx).await? else {
                return Ok(());
            };
            work.charge(row.len())?;
            canonical::validate(row.as_ref(), self.columns, work)?;
            if self.distinct
                && let Some(previous) = &previous
            {
                host::event(host, work.cx, policy, evaluator, GlaExecutionEvent::Work)?;
                if canonical::compare(previous.as_ref(), row.as_ref(), &[], self.columns, work)?
                    == Ordering::Equal
                {
                    continue;
                }
            }
            host::event(
                host,
                work.cx,
                policy,
                evaluator,
                GlaExecutionEvent::ScratchEntry,
            )?;
            self.row = Some(row);
            return Ok(());
        }
    }
}
