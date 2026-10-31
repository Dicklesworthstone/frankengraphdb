//! External keyed/theta joins over completed authenticated child runs.
use super::host::ControlledStats;
use super::*;
use crate::query::view::spool::sort::{ChargedVec, Input as MergeInput};
use fgdb_gql::GlaExecutionEvent;
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue, GraphValueOrder};
use fgdb_gql::row_join::RowJoinKind;
use fgdb_gql::spill_set::{SpillSetJoinPlan, SpillSetRowError};
use fgdb_strata::tiered::memory::{MemoryCharge, MemoryPool};
use fgdb_types::CanonicalScalarResolver;
use std::cmp::Ordering;

#[path = "relational_join_filtered.rs"]
mod filtered;
#[path = "relational_join_pairs.rs"]
mod pairs;

fn failure(error: SpillSetRowError<NativeSpoolError>) -> NativeSpoolError {
    match error {
        SpillSetRowError::Control(error) => error,
        SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
    }
}
fn event(
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    evaluator: &mut ControlledStats<'_>,
    charge: &mut MemoryCharge,
    event: GlaExecutionEvent,
) -> Result<()> {
    staged::native_event(cx, policy, evaluator, event)?;
    if event == GlaExecutionEvent::ScratchEntry {
        charge
            .grow(
                cx,
                8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES),
            )
            .map_err(SpillError::Memory)?;
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn matches_pair(
    plan: &SpillSetJoinPlan,
    left: &GraphValueRow,
    right: &GraphValueRow,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    evaluator: &mut ControlledStats<'_>,
    pool: &MemoryPool,
) -> Result<bool> {
    let mut temporary = pool.reserve(cx, 0).map_err(SpillError::Memory)?;
    plan.matches_pair_with_control(left, right, &mut |value| {
        event(cx, policy, evaluator, &mut temporary, value)
    })
    .map_err(failure)
}

// Sort only completed children: their expressions, windows and source errors
// retain their original execution order. The ordinary sorter owns all runs,
// frame authentication, memory reservations and the cumulative physical meter.
#[allow(clippy::too_many_arguments)]
async fn order_keys<A, B>(
    plan: &SpillSetJoinPlan,
    side: usize,
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
    let mut order =
        ChargedVec::<GraphValueOrder>::new(destination.memory_pool(), work.cx, plan.keys().len())?;
    for &(left, right) in plan.keys() {
        work.charge(1)?;
        let column = if side == 0 { left } else { right };
        // A repeated component does not change this side's lexicographic
        // tuple order, and the ordinary sorter rejects duplicate columns.
        if !order.values.iter().any(|key| key.column == column) {
            order
                .values
                .push(GraphValueOrder::ascending(column).with_nulls_first(true));
        }
    }
    let (sorted, used) = input
        .sort_continuing(
            work.cx,
            source,
            destination,
            &order.values,
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

#[allow(clippy::too_many_arguments)]
async fn pull_sorted<F>(
    input: &mut MergeInput,
    source: &mut SpillFile<F>,
    pool: &MemoryPool,
    policy: GqlQueryPolicy,
    evaluator: &mut ControlledStats<'_>,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<Option<SpoolRow>>
where
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    staged::native_event(work.cx, policy, evaluator, GlaExecutionEvent::Work)?;
    match input.next(source, work.cx).await? {
        Some(bytes) => cross::decode(bytes, pool, work, resolver).map(Some),
        None => Ok(None),
    }
}

// Equality-only SEMI/ANTI needs one witness per left occurrence, not a group
// product or a resident key table. Sort both sides, then advance monotonically
// with one decoded head per side. Right duplicates never multiply left rows.
// The enclosing native Join stage restores its canonical/explicit output order
// BEFORE applying pagination or allowing a parent expression to observe rows.
#[allow(clippy::too_many_arguments)]
async fn merge_presence<A, B, C>(
    plan: &SpillSetJoinPlan,
    left: &NativeResultSpool,
    right: &NativeResultSpool,
    snapshot: CommitSeq,
    snapshot_records: u64,
    mut evaluator: ControlledStats<'_>,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    auxiliary: &mut SpillFile<C>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let left = order_keys(
        plan,
        0,
        left,
        destination,
        scratch,
        run_rows,
        max_runs,
        page_bytes,
        work,
    )
    .await?;
    let right = order_keys(
        plan,
        1,
        right,
        destination,
        auxiliary,
        run_rows,
        max_runs,
        page_bytes,
        work,
    )
    .await?;
    let cx = work.cx;
    let pool = scratch.memory_pool().clone();
    let mut left_input = cross::position(&left);
    let mut right_input = cross::position(&right);
    let mut right_row = pull_sorted(
        &mut right_input,
        auxiliary,
        &pool,
        policy,
        &mut evaluator,
        work,
        resolver,
    )
    .await?;
    let mut writer = destination.paged_writer(cx, page_bytes)?;
    let mut rows = 0_u64;
    let mut largest = 0;
    while let Some(left_row) = pull_sorted(
        &mut left_input,
        scratch,
        &pool,
        policy,
        &mut evaluator,
        work,
        resolver,
    )
    .await?
    {
        let found = loop {
            let Some(right_row_ref) = right_row.as_ref() else {
                break false;
            };
            let ordering = plan
                .compare_keys_with_control(&left_row.row, &right_row_ref.row, &mut |event| {
                    staged::native_event(cx, policy, &mut evaluator, event)
                })
                .map_err(failure)?;
            match ordering {
                Ordering::Less => break false,
                Ordering::Equal => {
                    // Total key order puts NULL with NULL; native equality
                    // still refuses it. No residual predicate is admitted here.
                    break matches_pair(
                        plan,
                        &left_row.row,
                        &right_row_ref.row,
                        cx,
                        policy,
                        &mut evaluator,
                        &pool,
                    )?;
                }
                Ordering::Greater => {
                    drop(right_row.take());
                    right_row = pull_sorted(
                        &mut right_input,
                        auxiliary,
                        &pool,
                        policy,
                        &mut evaluator,
                        work,
                        resolver,
                    )
                    .await?;
                }
            }
        };
        if found == (plan.kind() == RowJoinKind::Semi) {
            let next = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            GqlExecutionBudget::result_rows(max_input_rows)
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(|error| {
                    NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
                })?;
            let mut charge = pool.reserve(cx, 0).map_err(SpillError::Memory)?;
            let row = plan
                .project_pair_with_control(Some(&left_row.row), None, &mut |value| {
                    event(cx, policy, &mut evaluator, &mut charge, value)
                })
                .map_err(failure)?;
            let row = SpoolRow::buffered((row, charge));
            let encoded = codec::encode(&pool, work, &row.row, max_row_bytes)?;
            write_row(&mut writer, &encoded.0, work).await?;
            largest = largest.max(encoded.0.len());
            rows = next;
        }
    }
    // An empty left side or an early witness must not hide a corrupt suffix,
    // quota refusal or cancellation in the sorted right run.
    drop(right_row);
    while pull_sorted(
        &mut right_input,
        auxiliary,
        &pool,
        policy,
        &mut evaluator,
        work,
        resolver,
    )
    .await?
    .is_some()
    {}
    drop(left_input);
    drop(right_input);
    staged::native_event(cx, policy, &mut evaluator, GlaExecutionEvent::Work)?;
    work.charge(1)?;
    let run = writer.finish(cx).await?;
    Ok(NativeResultSpool {
        columns: plan.columns().to_vec().into(),
        encoded_columns: plan.columns().len(),
        snapshot,
        kind: ScanKind::Set,
        rows: GqlExecutionStats {
            snapshot_records,
            result_rows: rows,
        },
        evaluator: evaluator.stats,
        max_row_bytes: largest,
        run,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<H, A, B, C>(
    plan: &SpillSetJoinPlan,
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
    let binary::CompletedChildren {
        left,
        right,
        snapshot_records,
        evaluator,
        work: prior_work,
    } = binary::complete_children(
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
    let mut control = |event| host.native_event(event);
    let mut evaluator = ControlledStats {
        stats: evaluator,
        control: &mut control,
    };
    let pool = scratch.memory_pool().clone();
    let mut work = Work {
        cx,
        used: prior_work,
        limit: max_sort_work,
    };
    for (side, child) in [(0, &left), (1, &right)] {
        let mut input = cross::position(child);
        let mut seen = 0_u64;
        while let Some(bytes) = input.next(destination, cx).await? {
            let row = cross::decode(bytes, &pool, &mut work, resolver)?;
            plan.validate_row_with_control(&row.row, side, &mut |event| {
                staged::native_event(cx, policy, &mut evaluator, event)
            })
            .map_err(failure)?;
            seen = seen.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        }
        if seen != child.row_count() {
            return Err(NativeSpoolError::IncompleteCursor);
        }
    }
    if plan.supports_merge_filtered() {
        let result = filtered::execute(
            plan,
            &left,
            &right,
            snapshot,
            snapshot_records,
            evaluator,
            policy,
            scratch,
            destination,
            auxiliary,
            run_rows,
            max_runs,
            page_bytes,
            max_row_bytes,
            max_input_rows,
            &mut work,
            resolver,
        )
        .await?;
        return Ok((result, work.used));
    }
    if plan.supports_merge_presence() {
        let result = merge_presence(
            plan,
            &left,
            &right,
            snapshot,
            snapshot_records,
            evaluator,
            policy,
            scratch,
            destination,
            auxiliary,
            run_rows,
            max_runs,
            page_bytes,
            max_row_bytes,
            max_input_rows,
            &mut work,
            resolver,
        )
        .await?;
        return Ok((result, work.used));
    }
    if plan.supports_merge_pairs() {
        let result = pairs::execute(
            plan,
            &left,
            &right,
            snapshot,
            snapshot_records,
            evaluator,
            policy,
            scratch,
            destination,
            auxiliary,
            run_rows,
            max_runs,
            page_bytes,
            max_row_bytes,
            max_input_rows,
            &mut work,
            resolver,
        )
        .await?;
        return Ok((result, work.used));
    }
    let mut writer = scratch.paged_writer(cx, page_bytes)?;
    let mut rows = 0_u64;
    let mut largest = 0;
    macro_rules! emit {
        ($left:expr,$right:expr) => {{
            let next = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            GqlExecutionBudget::result_rows(max_input_rows)
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(|error| {
                    NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
                })?;
            let mut charge = pool.reserve(cx, 0).map_err(SpillError::Memory)?;
            let row = plan
                .project_pair_with_control($left, $right, &mut |value| {
                    event(cx, policy, &mut evaluator, &mut charge, value)
                })
                .map_err(failure)?;
            let row = SpoolRow::buffered((row, charge));
            let encoded = codec::encode(&pool, &mut work, &row.row, max_row_bytes)?;
            write_row(&mut writer, &encoded.0, &mut work).await?;
            largest = largest.max(encoded.0.len());
            rows = next;
        }};
    }
    let kind = plan.kind();
    let presence = matches!(kind, RowJoinKind::Semi | RowJoinKind::Anti);
    let mut left_input = cross::position(&left);
    while let Some(bytes) = left_input.next(destination, cx).await? {
        let left_row = cross::decode(bytes, &pool, &mut work, resolver)?;
        let mut right_input = cross::position(&right);
        let mut found = false;
        while let Some(bytes) = right_input.next(destination, cx).await? {
            let right_row = cross::decode(bytes, &pool, &mut work, resolver)?;
            if matches_pair(
                plan,
                &left_row.row,
                &right_row.row,
                cx,
                policy,
                &mut evaluator,
                &pool,
            )? {
                found = true;
                if presence {
                    break;
                }
                emit!(Some(&left_row.row), Some(&right_row.row));
            }
        }
        match kind {
            RowJoinKind::Semi if found => emit!(Some(&left_row.row), None),
            RowJoinKind::Anti if !found => emit!(Some(&left_row.row), None),
            RowJoinKind::Left | RowJoinKind::Full if !found => emit!(Some(&left_row.row), None),
            _ => {}
        }
    }
    drop(left_input);
    if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
        let mut right_input = cross::position(&right);
        while let Some(bytes) = right_input.next(destination, cx).await? {
            let right_row = cross::decode(bytes, &pool, &mut work, resolver)?;
            let mut left_input = cross::position(&left);
            let mut found = false;
            while let Some(bytes) = left_input.next(destination, cx).await? {
                let left_row = cross::decode(bytes, &pool, &mut work, resolver)?;
                if matches_pair(
                    plan,
                    &left_row.row,
                    &right_row.row,
                    cx,
                    policy,
                    &mut evaluator,
                    &pool,
                )? {
                    found = true;
                    break;
                }
            }
            if !found {
                emit!(None, Some(&right_row.row));
            }
        }
    }
    staged::native_event(cx, policy, &mut evaluator, GlaExecutionEvent::Work)?;
    work.charge(1)?;
    let run = writer.finish(cx).await?;
    let joined = NativeResultSpool {
        columns: plan.columns().to_vec().into(),
        encoded_columns: plan.columns().len(),
        snapshot,
        kind: ScanKind::Set,
        rows: GqlExecutionStats {
            snapshot_records,
            result_rows: rows,
        },
        evaluator: evaluator.stats,
        max_row_bytes: largest,
        run,
    };
    let result = staged::copy(&joined, scratch, destination, page_bytes, &mut work).await?;
    Ok((result, work.used))
}
