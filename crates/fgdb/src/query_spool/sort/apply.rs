//! Correlated external execution over authenticated, complete child runs.
//! Row extension keeps the current outer row, lexical frame and body row live.
//! Collected Apply additionally retains one bounded, charged result list.

use super::*;
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue};
use fgdb_gql::spill_set::{
    SpillSetApplyKind, SpillSetApplyPlan, SpillSetArgumentsPlan, SpillSetRowError, SpillSetRows,
    SpillSetValuesPlan,
};
use fgdb_strata::tiered::memory::{MemoryCharge, MemoryPool};
use fgdb_types::CanonicalScalarResolver;

fn failure(error: SpillSetRowError<NativeSpoolError>) -> NativeSpoolError {
    match error {
        SpillSetRowError::Control(error) => error,
        SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
    }
}

fn row_charge(pool: &MemoryPool, cx: &QueryCx, width: usize) -> Result<MemoryCharge> {
    let bytes = width
        .checked_mul(size_of::<GraphValue>())
        .and_then(|bytes| bytes.checked_add(size_of::<GraphValueRow>()))
        .ok_or(SpillError::SizeOverflow)?;
    pool.reserve(cx, bytes)
        .map_err(SpillError::Memory)
        .map_err(Into::into)
}

fn copy_event<H: ExternalOrderHost>(
    host: &mut H,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    evaluator: &mut GlaExecutionStats,
    charge: &mut MemoryCharge,
    event: GlaExecutionEvent,
) -> Result<()> {
    host::event(host, cx, policy, evaluator, event)?;
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

fn admit_rows(limit: u64, rows: u64) -> Result<()> {
    GqlExecutionBudget::result_rows(limit)
        .check(GqlBudgetDimension::ResultRows, rows)
        .map_err(|error| NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error))))
}

#[allow(clippy::too_many_arguments)]
async fn seed<H, F>(
    columns: &[String],
    kind: ScanKind,
    snapshot: CommitSeq,
    host: &mut H,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    destination: &mut SpillFile<F>,
    limits: ExternalOrderLimits,
    start: impl FnOnce(
        &mut dyn FnMut(GlaExecutionEvent) -> Result<()>,
    ) -> core::result::Result<SpillSetRows, SpillSetRowError<NativeSpoolError>>,
) -> Result<(NativeResultSpool, u64)>
where
    H: ExternalOrderHost,
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let pool = destination.memory_pool().clone();
    let mut evaluator = GlaExecutionStats::default();
    let mut charge = row_charge(&pool, cx, columns.len())?;
    let mut cursor =
        start(&mut |event| copy_event(host, cx, policy, &mut evaluator, &mut charge, event))
            .map_err(failure)?;
    let mut work = Work {
        cx,
        used: 0,
        limit: limits.max_sort_work,
    };
    let mut writer = destination.paged_writer(cx, limits.page_bytes)?;
    let mut rows = 0_u64;
    let mut largest = 0;
    loop {
        let next = cursor
            .next_with_control(&mut |event| {
                copy_event(host, cx, policy, &mut evaluator, &mut charge, event)
            })
            .map_err(failure)?;
        let Some((row, remaining)) = next else { break };
        cursor = remaining;
        if row.len() != columns.len() || rows != 0 {
            return Err(SpillError::InvalidRun.into());
        }
        admit_rows(limits.max_input_rows, 1)?;
        let encoded = codec::encode(&pool, &mut work, &row, limits.max_row_bytes)?;
        write_row(&mut writer, &encoded.0, &mut work).await?;
        largest = encoded.0.len();
        rows = 1;
    }
    if rows != 1 {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    work.charge(1)?;
    let run = writer.finish(cx).await?;
    Ok((
        NativeResultSpool {
            columns: columns.to_vec().into(),
            encoded_columns: columns.len(),
            snapshot,
            kind,
            rows: GqlExecutionStats {
                snapshot_records: 0,
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
pub(super) async fn seed_values<H, F>(
    plan: &SpillSetValuesPlan,
    snapshot: CommitSeq,
    host: &mut H,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    destination: &mut SpillFile<F>,
    limits: ExternalOrderLimits,
) -> Result<(NativeResultSpool, u64)>
where
    H: ExternalOrderHost,
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    seed(
        &[],
        ScanKind::Values,
        snapshot,
        host,
        cx,
        policy,
        destination,
        limits,
        |mut control| plan.start(&mut control),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn seed_arguments<H, F>(
    plan: &SpillSetArgumentsPlan,
    frame: Option<&GraphValueRow>,
    snapshot: CommitSeq,
    host: &mut H,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    destination: &mut SpillFile<F>,
    limits: ExternalOrderLimits,
) -> Result<(NativeResultSpool, u64)>
where
    H: ExternalOrderHost,
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    seed(
        plan.columns(),
        ScanKind::Values,
        snapshot,
        host,
        cx,
        policy,
        destination,
        limits,
        |mut control| plan.start(frame, &mut control),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<H, A, B, C>(
    plan: &SpillSetApplyPlan,
    arguments: Option<&GraphValueRow>,
    snapshot: CommitSeq,
    host: &mut H,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    auxiliary: &mut SpillFile<C>,
    limits: ExternalOrderLimits,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<(NativeResultSpool, u64)>
where
    H: ExternalOrderHost,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let (mut outer, prior_work) = execute_relation(
        plan.left(),
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
    if outer.snapshot != snapshot || outer.encoded_columns != plan.left().columns().len() {
        return Err(SpillError::InvalidRun.into());
    }
    // Keep this run's coordinates and row count fixed. Its private counters
    // accumulate every reached frame/body so residual policy and diagnostics
    // always refer to the whole invocation, including duplicate outer rows.
    let mut outer_input = cross::position(&outer);
    let pool = scratch.memory_pool().clone();
    let mut work = Work {
        cx,
        used: prior_work,
        limit: limits.max_sort_work,
    };
    let mut output = scratch.paged_writer(cx, limits.page_bytes)?.suspend(cx)?;
    let mut outer_seen = 0_u64;
    let mut input_rows = outer.row_count();
    let mut rows = 0_u64;
    let mut largest = 0;
    while let Some(bytes) = outer_input.next(destination, cx).await? {
        host.checkpoint()?;
        let left = cross::decode(bytes, &pool, &mut work, resolver)?;
        let row_index = usize::try_from(outer_seen).map_err(|_| SpillError::SizeOverflow)?;
        let collect = matches!(plan.kind(), SpillSetApplyKind::Collect { .. });
        // A NULL list skips its correlated body, including argument copying
        // and every graph/property read. Static body admission already ran.
        if collect
            && plan
                .collect_input_is_null(&left.row, &mut |event| {
                    host::event(host, cx, policy, &mut outer.evaluator, event)
                })
                .map_err(failure)?
        {
            let next = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            admit_rows(limits.max_input_rows, next)?;
            let mut charge = row_charge(&pool, cx, plan.columns().len())?;
            let row = plan
                .null_collect(&left.row, &mut |event| {
                    copy_event(host, cx, policy, &mut outer.evaluator, &mut charge, event)
                })
                .map_err(failure)?;
            let encoded = codec::encode(&pool, &mut work, &row, limits.max_row_bytes)?;
            let mut writer = output.resume(cx, scratch)?;
            write_row(&mut writer, &encoded.0, &mut work).await?;
            largest = largest.max(encoded.0.len());
            output = writer.suspend(cx)?;
            rows = next;
            outer_seen = outer_seen.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            continue;
        }
        let mut frame_charge = row_charge(&pool, cx, plan.imports().len())?;
        let frame = plan
            .prepare_arguments(&left.row, row_index, &mut |event| {
                copy_event(
                    host,
                    cx,
                    policy,
                    &mut outer.evaluator,
                    &mut frame_charge,
                    event,
                )
            })
            .map_err(failure)?;
        let body_policy = binary::remaining_policy(policy, &outer)?;
        let body_work_limit = limits
            .max_sort_work
            .checked_sub(work.used)
            .ok_or(NativeSpoolError::IncompleteCursor)?;
        let (body, body_work) = execute_relation(
            plan.right(),
            Some(&frame),
            snapshot,
            host,
            cx,
            body_policy,
            scratch,
            destination,
            auxiliary,
            ExternalOrderLimits {
                max_sort_work: body_work_limit,
                ..limits
            },
            resolver,
        )
        .await
        .map_err(|error| {
            binary::rebase_error(error, policy, &outer, work.used, limits.max_sort_work)
        })?;
        if body.snapshot != snapshot || body.encoded_columns != plan.right().columns().len() {
            return Err(SpillError::InvalidRun.into());
        }
        work.used = work
            .used
            .checked_add(body_work)
            .ok_or(SpillError::SizeOverflow)?;
        outer.rows.snapshot_records = outer
            .rows
            .snapshot_records
            .checked_add(body.rows.snapshot_records)
            .ok_or(SpillError::SizeOverflow)?;
        outer.evaluator.work_units = outer
            .evaluator
            .work_units
            .checked_add(body.evaluator.work_units)
            .ok_or(SpillError::SizeOverflow)?;
        outer.evaluator.scratch_entries = outer
            .evaluator
            .scratch_entries
            .checked_add(body.evaluator.scratch_entries)
            .ok_or(SpillError::SizeOverflow)?;
        input_rows = input_rows
            .checked_add(body.row_count())
            .ok_or(SpillError::SizeOverflow)?;
        admit_rows(limits.max_input_rows, input_rows)?;
        let mut body_input = cross::position(&body);
        let mut writer = output.resume(cx, scratch)?;
        let mut body_seen = 0_u64;
        if collect {
            // The graph and child relations remain external; one list cell
            // must fit the existing value/row and shared-memory bounds. Keep
            // its charge across all body reads and the final awaited append.
            let mut charge = row_charge(&pool, cx, plan.columns().len())?;
            let mut collection = plan
                .start_collection(row_index, &mut |event| {
                    copy_event(host, cx, policy, &mut outer.evaluator, &mut charge, event)
                })
                .map_err(failure)?;
            while let Some(bytes) = body_input.next(destination, cx).await? {
                let right = cross::decode(bytes, &pool, &mut work, resolver)?;
                collection = collection
                    .push(&right.row, &mut |event| {
                        copy_event(host, cx, policy, &mut outer.evaluator, &mut charge, event)
                    })
                    .map_err(failure)?;
                body_seen = body_seen.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            }
            if body_seen != body.row_count() {
                return Err(NativeSpoolError::IncompleteCursor);
            }
            let row = collection
                .finish(&left.row, &mut |event| {
                    copy_event(host, cx, policy, &mut outer.evaluator, &mut charge, event)
                })
                .map_err(failure)?;
            let next = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            admit_rows(limits.max_input_rows, next)?;
            let encoded = codec::encode(&pool, &mut work, &row, limits.max_row_bytes)?;
            write_row(&mut writer, &encoded.0, &mut work).await?;
            largest = largest.max(encoded.0.len());
            rows = next;
        } else {
            while let Some(bytes) = body_input.next(destination, cx).await? {
                let right = cross::decode(bytes, &pool, &mut work, resolver)?;
                let next = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
                admit_rows(limits.max_input_rows, next)?;
                let mut charge = row_charge(&pool, cx, plan.columns().len())?;
                let row = plan
                    .copy_pair(&left.row, &right.row, &mut |event| {
                        copy_event(host, cx, policy, &mut outer.evaluator, &mut charge, event)
                    })
                    .map_err(failure)?;
                let encoded = codec::encode(&pool, &mut work, &row, limits.max_row_bytes)?;
                write_row(&mut writer, &encoded.0, &mut work).await?;
                largest = largest.max(encoded.0.len());
                rows = next;
                body_seen = body_seen.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            }
            if body_seen != body.row_count() {
                return Err(NativeSpoolError::IncompleteCursor);
            }
            if body_seen == 0 && plan.kind() == SpillSetApplyKind::Optional {
                let next = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
                admit_rows(limits.max_input_rows, next)?;
                let mut charge = row_charge(&pool, cx, plan.columns().len())?;
                let row = plan
                    .null_extend(&left.row, &mut |event| {
                        copy_event(host, cx, policy, &mut outer.evaluator, &mut charge, event)
                    })
                    .map_err(failure)?;
                let encoded = codec::encode(&pool, &mut work, &row, limits.max_row_bytes)?;
                write_row(&mut writer, &encoded.0, &mut work).await?;
                largest = largest.max(encoded.0.len());
                rows = next;
            }
        }
        output = writer.suspend(cx)?;
        outer_seen = outer_seen.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        // Decoded rows, copied imports and each body's metadata refund here.
        // Appended scratch bytes and run attempts deliberately do not refund.
    }
    if outer_seen != outer.row_count() {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    host.checkpoint()?;
    work.charge(1)?;
    let run = output.resume(cx, scratch)?.finish(cx).await?;
    let result = NativeResultSpool {
        columns: plan.columns().to_vec().into(),
        encoded_columns: plan.columns().len(),
        snapshot,
        kind: ScanKind::Set,
        rows: GqlExecutionStats {
            snapshot_records: outer.rows.snapshot_records,
            result_rows: rows,
        },
        evaluator: outer.evaluator,
        max_row_bytes: largest,
        run,
    };
    let result = staged::copy(&result, scratch, destination, limits.page_bytes, &mut work).await?;
    Ok((result, work.used))
}
