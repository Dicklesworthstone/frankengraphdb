//! Bounded native Cartesian products, including the input relation for
//! leading UNWIND followed by MATCH. A compiler-proved native selection can
//! test borrowed pairs before copying/spilling them. No graph is rerun per row.

use super::*;
use crate::query::view::spool::sort::{Input as RunInput, Run};
use fgdb_gql::GlaExecutionEvent;
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue};
use fgdb_gql::spill_set::{SpillSetCrossPlan, SpillSetRowError};
use fgdb_strata::tiered::memory::MemoryPool;
use fgdb_types::CanonicalScalarResolver;

#[path = "cross_keys.rs"]
mod keyed;
#[path = "cross_seeds.rs"]
pub(super) mod seeds;

pub(super) fn position(spool: &NativeResultSpool) -> RunInput {
    RunInput::new(
        Run {
            pages: spool.run.clone(),
            rows: spool.row_count(),
        },
        spool.max_row_bytes,
    )
}

pub(super) fn decode(
    bytes: TrackedBytes,
    pool: &MemoryPool,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<SpoolRow> {
    decode_borrowed(bytes.as_ref(), pool, work, resolver)
}

fn decode_borrowed(
    bytes: &[u8],
    pool: &MemoryPool,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<SpoolRow> {
    work.charge(bytes.len())?;
    let reserved = codec::decoded(bytes, work.cx)?;
    let charge = pool
        .reserve(work.cx, reserved)
        .map_err(SpillError::Memory)?;
    let row = match resolver {
        Some(resolver) => GraphValueRow::decode_canonical_with_resolver(bytes, resolver),
        None => GraphValueRow::decode_stored(bytes),
    }
    .map_err(NativeSpoolError::Decode)?;
    Ok(SpoolRow::buffered((row, charge)))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<H, A, B, C>(
    plan: &SpillSetCrossPlan,
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
        mut evaluator,
        work: prior_work,
    } = binary::complete_children(
        plan.left(),
        plan.right(),
        Some(plan),
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
    // Structural eligibility is fixed before examining values. Complete both
    // original children first; a keyed-path refusal never retries the product.
    if plan.supports_keyed_selection() {
        let mut work = Work {
            cx,
            used: prior_work,
            limit: max_sort_work,
        };
        let mut control = |event| host.native_event(event);
        let evaluator = host::ControlledStats {
            stats: evaluator,
            control: &mut control,
        };
        let result = keyed::execute(
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
    // Both children have completed, even if the product is empty or an
    // enclosing clause asks for no output. A selected product never retains
    // L*R rows: admit accepted occurrences below instead. Plain products keep
    // their original pre-admission, including behind an ORDER/LIMIT barrier.
    let expected = if plan.has_selection() {
        None
    } else {
        let product = left
            .row_count()
            .checked_mul(right.row_count())
            .ok_or(SpillError::SizeOverflow)?;
        GqlExecutionBudget::result_rows(max_input_rows)
            .check(GqlBudgetDimension::ResultRows, product)
            .map_err(|error| {
                NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
            })?;
        Some(product)
    };
    let pool = scratch.memory_pool().clone();
    let mut work = Work {
        cx,
        used: prior_work,
        limit: max_sort_work,
    };
    let mut left_input = position(&left);
    let mut writer = scratch.paged_writer(cx, page_bytes)?;
    let mut rows = 0_u64;
    let mut largest = 0;
    while let Some(bytes) = left_input.next(destination, cx).await? {
        let left_row = decode(bytes, &pool, &mut work, resolver)?;
        let mut right_input = position(&right);
        while let Some(bytes) = right_input.next(destination, cx).await? {
            let right_row = decode(bytes, &pool, &mut work, resolver)?;
            let mut charge = pool.reserve(cx, 0).map_err(SpillError::Memory)?;
            let row = plan
                .select_pair_with_control(&left_row.row, &right_row.row, &mut |event| {
                    host::event(host, cx, policy, &mut evaluator, event)?;
                    if event == GlaExecutionEvent::ScratchEntry {
                        charge
                            .grow(
                                cx,
                                8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES),
                            )
                            .map_err(SpillError::Memory)?;
                    }
                    Ok(())
                })
                .map_err(|error| match error {
                    SpillSetRowError::Control(error) => error,
                    SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
                })?;
            let Some(row) = row else {
                continue;
            };
            let next = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            GqlExecutionBudget::result_rows(max_input_rows)
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(|error| {
                    NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
                })?;
            let row = SpoolRow::buffered((row, charge));
            let encoded = codec::encode(&pool, &mut work, &row.row, max_row_bytes)?;
            write_row(&mut writer, &encoded.0, &mut work).await?;
            largest = largest.max(encoded.0.len());
            rows = next;
            // Each decoded/copy/encoder reservation survives the awaited
            // append, then refunds before advancing to the next right row.
        }
    }
    drop(left_input);
    if expected.is_some_and(|expected| rows != expected) {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    work.charge(1)?;
    let run = writer.finish(cx).await?;
    let product = NativeResultSpool {
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
    let result = staged::copy(&product, scratch, destination, page_bytes, &mut work).await?;
    Ok((result, work.used))
}
