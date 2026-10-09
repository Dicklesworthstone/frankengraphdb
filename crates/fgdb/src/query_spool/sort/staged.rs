//! External execution of the checked native unary relation program. Every
//! completed child is a real scratch barrier: source order, intermediate
//! DISTINCT/window and expression-failure precedence survive composition.

use super::*;
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue};
use fgdb_gql::spill_set::{AsyncSpillSetPlan, SpillSetRowError, SpillSetStage};
use fgdb_gql::{GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded};
use fgdb_types::CanonicalScalarResolver;

#[derive(Clone, Copy)]
struct Limits {
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
}

// Continue the original source's native allowance. Physical decode/encoding,
// copying and sorting have their separate cumulative Work counter below.
fn native_event(
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    stats: &mut GlaExecutionStats,
    event: GlaExecutionEvent,
) -> Result<()> {
    cx.with_restriction(|| cx.checkpoint()).map_err(|error| {
        NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Interrupted(error)))
    })?;
    if event == GlaExecutionEvent::ResultRow {
        // The checked unary evaluator never admits final rows. A stage must
        // finish in full before the following stage or final quota can run.
        return Err(NativeSpoolError::IncompleteCursor);
    }
    let next = |used: u64, added: u64, limit, dimension| {
        let observed = u128::from(used) + u128::from(added);
        if observed > u128::from(limit) {
            Err(NativeSpoolError::BufferedExecute(Box::new(
                GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                }),
            )))
        } else {
            Ok(observed as u64)
        }
    };
    let work_units = next(
        stats.work_units,
        1,
        policy.evaluator.max_work_units,
        GlaLimitDimension::WorkUnits,
    )?;
    let scratch_entries = next(
        stats.scratch_entries,
        u64::from(event == GlaExecutionEvent::ScratchEntry),
        policy.evaluator.max_scratch_entries,
        GlaLimitDimension::ScratchEntries,
    )?;
    *stats = GlaExecutionStats {
        work_units,
        scratch_entries,
    };
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<A, B>(
    plan: AsyncSpillSetPlan,
    mut input: NativeResultSpool,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    max_sort_work: u64,
    prior_work: u64,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<(NativeResultSpool, u64)>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    if plan.stages().is_empty() || input.columns() != plan.source_columns() {
        return Err(SpillError::InvalidRun.into());
    }
    let limits = Limits {
        run_rows,
        max_runs,
        page_bytes,
        max_row_bytes,
        max_input_rows,
    };
    let mut work = Work {
        cx,
        used: prior_work,
        limit: max_sort_work,
    };
    let mut evaluator = input.evaluator_stats();
    let mut in_destination = true;
    for (ordinal, stage) in plan.stages().iter().enumerate() {
        let budget = if ordinal + 1 == plan.stages().len() {
            policy.rows
        } else {
            GqlExecutionBudget::result_rows(max_input_rows)
        };
        input = if in_destination {
            run_stage(
                input,
                stage,
                destination,
                scratch,
                limits,
                budget,
                policy,
                &mut evaluator,
                &mut work,
                resolver,
            )
            .await?
        } else {
            run_stage(
                input,
                stage,
                scratch,
                destination,
                limits,
                budget,
                policy,
                &mut evaluator,
                &mut work,
                resolver,
            )
            .await?
        };
        in_destination = !in_destination;
    }
    if !in_destination {
        input = copy(&input, scratch, destination, page_bytes, &mut work).await?;
    }
    if input.columns() != plan.columns() {
        return Err(SpillError::InvalidRun.into());
    }
    input.evaluator = evaluator;
    Ok((input, work.used))
}

// This function always leaves its result in destination. Both files append;
// no handle is overwritten, recycled or selected by retrying a failed phase.
#[allow(clippy::too_many_arguments)]
async fn run_stage<A, B>(
    input: NativeResultSpool,
    stage: &SpillSetStage,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    limits: Limits,
    budget: GqlExecutionBudget,
    policy: GqlQueryPolicy,
    evaluator: &mut GlaExecutionStats,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let transformed = transform(
        &input,
        stage,
        source,
        destination,
        limits,
        policy,
        evaluator,
        work,
        resolver,
    )
    .await?;
    if let Some(requested) = stage.order()
        && !stage.columns().is_empty()
    {
        // Empty requested order is native canonical tuple order, whose NULL
        // position differs from an explicit default ASC key. Reserve this
        // bounded physical key vector before constructing it.
        let count = if requested.is_empty() {
            stage.columns().len()
        } else {
            requested.len()
        };
        work.charge(count)?;
        let charge = destination
            .memory_pool()
            .reserve(
                work.cx,
                count
                    .checked_mul(size_of::<GraphValueOrder>())
                    .ok_or(SpillError::SizeOverflow)?,
            )
            .map_err(SpillError::Memory)?;
        let order = (
            if requested.is_empty() {
                (0..count)
                    .map(|column| GraphValueOrder::ascending(column).with_nulls_first(true))
                    .collect::<Vec<_>>()
            } else {
                requested.to_vec()
            },
            charge,
        );
        let (sorted, used) = transformed
            .sort_continuing(
                work.cx,
                destination,
                source,
                &order.0,
                limits.run_rows,
                limits.max_runs,
                limits.page_bytes,
                work.limit,
                work.used,
            )
            .await?;
        work.used = used;
        drop(order);
        window(
            &sorted,
            source,
            destination,
            stage,
            budget,
            limits.page_bytes,
            work,
        )
        .await
    } else {
        // Filter/Scope keep the child's sequence unless the native plan orders
        // them explicitly. Materialize first even for a zero output count, so
        // a later predicate failure cannot disappear behind pagination.
        if stage.distinct() && !stage.columns().is_empty() {
            return Err(SpillError::InvalidRun.into());
        }
        let selected = window(
            &transformed,
            destination,
            source,
            stage,
            budget,
            limits.page_bytes,
            work,
        )
        .await?;
        copy(&selected, source, destination, limits.page_bytes, work).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn transform<A, B>(
    input: &NativeResultSpool,
    stage: &SpillSetStage,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    limits: Limits,
    policy: GqlQueryPolicy,
    evaluator: &mut GlaExecutionStats,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    if input.encoded_columns != stage.input_types().len() {
        return Err(SpillError::InvalidRun.into());
    }
    let pool = destination.memory_pool().clone();
    let mut reader = input.reader(source);
    let mut writer = destination.paged_writer(work.cx, limits.page_bytes)?;
    let mut rows = 0_u64;
    let mut row_index = 0_usize;
    let mut largest = 0_usize;
    while let Some(bytes) = reader.next_row(work.cx).await? {
        work.charge(bytes.len())?;
        let reserved = codec::decoded(bytes.as_ref(), work.cx)?;
        let mut charge = pool
            .reserve(work.cx, reserved)
            .map_err(SpillError::Memory)?;
        let row = match resolver {
            Some(resolver) => {
                GraphValueRow::decode_canonical_with_resolver(bytes.as_ref(), resolver)
            }
            None => GraphValueRow::decode_canonical(bytes.as_ref()),
        }
        .map_err(NativeSpoolError::Decode)?;
        drop(bytes);
        let output = stage
            .evaluate(row, row_index, &mut |event| {
                native_event(work.cx, policy, evaluator, event)?;
                if event == GlaExecutionEvent::ScratchEntry {
                    let bytes = 8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
                    charge.grow(work.cx, bytes).map_err(SpillError::Memory)?;
                }
                Ok(())
            })
            .map_err(|error| match error {
                SpillSetRowError::Control(error) => error,
                SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
            })?;
        row_index = row_index.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        if let Some(row) = output {
            if row.len() != stage.columns().len() {
                return Err(SpillError::InvalidRun.into());
            }
            let next = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            GqlExecutionBudget::result_rows(limits.max_input_rows)
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(|error| {
                    NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
                })?;
            let row = SpoolRow::buffered((row, charge));
            // Data precedes each affine charge in both aggregates. They stay
            // intact across every await, including cancellation and unwinding.
            let encoded = codec::encode(&pool, work, &row.row, limits.max_row_bytes)?;
            write_row(&mut writer, &encoded.0, work).await?;
            largest = largest.max(encoded.0.len());
            rows = next;
            drop(encoded);
            drop(row);
        }
    }
    if reader.state() != ScanState::Exhausted
        || u64::try_from(row_index).ok() != Some(input.row_count())
    {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    drop(reader);
    work.charge(1)?;
    let run = writer.finish(work.cx).await?;
    Ok(NativeResultSpool {
        columns: stage.columns().to_vec().into(),
        encoded_columns: stage.columns().len(),
        snapshot: input.snapshot,
        kind: input.kind,
        rows: GqlExecutionStats {
            snapshot_records: input.rows.snapshot_records,
            result_rows: rows,
        },
        evaluator: *evaluator,
        max_row_bytes: largest,
        run,
    })
}

async fn copy<A, B>(
    input: &NativeResultSpool,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    page_bytes: usize,
    work: &mut Work<'_>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    let mut reader = input.reader(source);
    let mut writer = destination.paged_writer(work.cx, page_bytes)?;
    while let Some(row) = reader.next_row(work.cx).await? {
        write_row(&mut writer, row.as_ref(), work).await?;
    }
    if reader.state() != ScanState::Exhausted {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    drop(reader);
    work.charge(1)?;
    let run = writer.finish(work.cx).await?;
    Ok(NativeResultSpool {
        run,
        ..input.clone()
    })
}
