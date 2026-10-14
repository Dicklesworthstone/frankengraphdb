//! External keyed/theta joins over completed authenticated child runs.
use super::*;
use crate::BufferedReadView;
use fgdb_gql::GlaExecutionEvent;
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue};
use fgdb_gql::row_join::RowJoinKind;
use fgdb_gql::spill_set::{SpillSetJoinPlan, SpillSetRowError};
use fgdb_strata::tiered::memory::{MemoryCharge, MemoryPool};
use fgdb_types::CanonicalScalarResolver;
fn failure(error: SpillSetRowError<NativeSpoolError>) -> NativeSpoolError {
    match error {
        SpillSetRowError::Control(error) => error,
        SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
    }
}
fn event(
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    evaluator: &mut GlaExecutionStats,
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
    evaluator: &mut GlaExecutionStats,
    pool: &MemoryPool,
) -> Result<bool> {
    let mut temporary = pool.reserve(cx, 0).map_err(SpillError::Memory)?;
    plan.matches_pair_with_control(left, right, &mut |value| {
        event(cx, policy, evaluator, &mut temporary, value)
    })
    .map_err(failure)
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<V, A, B, C>(
    plan: SpillSetJoinPlan,
    snapshot: CommitSeq,
    view: &mut BufferedReadView<V>,
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
    V: Vfs + Clone,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let binary::CompletedChildren {
        left,
        right,
        snapshot_records,
        mut evaluator,
        work: prior_work,
    } = binary::complete_children(
        plan.left().clone(),
        plan.right().clone(),
        snapshot,
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
        resolver,
    )
    .await?;
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
                &plan,
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
                    &plan,
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
        evaluator,
        max_row_bytes: largest,
        run,
    };
    let result = staged::copy(&joined, scratch, destination, page_bytes, &mut work).await?;
    Ok((result, work.used))
}
