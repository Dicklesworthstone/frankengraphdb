//! Necessary-equality probes over completed selected-product children.
//!
//! The ordinary semantic sorter forms candidate groups; every candidate still
//! runs the native predicate. Private occurrence ordinals restore the original
//! left-major sequence before any parent operator observes the selected rows.

use super::super::host::ControlledStats;
use super::*;
use crate::query::view::spool::sort::ChargedVec;
use fgdb_gql::algebra::GraphValueOrder;
use fgdb_strata::tiered::memory::MemoryCharge;
use fgdb_strata::tiered::memory::spill::PagedSpillWriter;
use fgdb_types::CanonicalScalar;
use std::cmp::Ordering;

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

// A bijection over the COMPLETE u64 occurrence domain. Flipping the sign bit
// makes the native signed-Int order agree with the original unsigned order;
// no floating conversion, truncation or row-count-dependent narrowing occurs.
fn ordinal(value: u64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(i64::from_be_bytes(
        (value ^ (1_u64 << 63)).to_be_bytes(),
    )))
}

/// Append private positions without copying the original graph payload. Both
/// parts use the ordinary native row codec; only the row header is reframed.
/// The original output has already passed its PUBLIC max_row_bytes check.
async fn write_tagged<F>(
    writer: &mut PagedSpillWriter<'_, F>,
    bytes: &[u8],
    width: usize,
    positions: &[u64],
    pool: &MemoryPool,
    work: &mut Work<'_>,
) -> Result<usize>
where
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let fields = canonical::visible_prefix(bytes, width, width, work)?;
    let mut values = ChargedVec::<GraphValue>::new(pool, work.cx, positions.len())?;
    for &position in positions {
        work.charge(1)?;
        values.values.push(ordinal(position));
    }
    // The vector's byte guards remain alive while the native row owns its
    // allocation, including throughout the awaited encoder/append overlap.
    let _boxed_overlap = pool
        .reserve(
            work.cx,
            positions
                .len()
                .checked_mul(size_of::<GraphValue>())
                .ok_or(SpillError::SizeOverflow)?,
        )
        .map_err(SpillError::Memory)?;
    let row = GraphValueRow::from_owned_values(core::mem::take(&mut values.values));
    let encoded = codec::encode(pool, work, &row, usize::MAX)?;
    let extra = canonical::visible_prefix(&encoded.0, positions.len(), positions.len(), work)?;
    let columns = width
        .checked_add(positions.len())
        .ok_or(SpillError::SizeOverflow)?;
    let length = canonical::ROW
        .len()
        .checked_add(8)
        .and_then(|length| length.checked_add(fields.len()))
        .and_then(|length| length.checked_add(extra.len()))
        .ok_or(SpillError::SizeOverflow)?;
    let length_u64 = u64::try_from(length).map_err(|_| SpillError::SizeOverflow)?;
    let columns = u64::try_from(columns).map_err(|_| SpillError::SizeOverflow)?;
    work.charge(length)?;
    writer.write(work.cx, &length_u64.to_be_bytes()).await?;
    writer.write(work.cx, canonical::ROW).await?;
    writer.write(work.cx, &columns.to_be_bytes()).await?;
    writer.write(work.cx, fields).await?;
    writer.write(work.cx, extra).await?;
    Ok(length)
}

struct PositionedRow {
    value: SpoolRow,
    ordinal: u64,
}

// Reuse the canonical framing and decoder for logical rows. The small private
// ordinal is removed BEFORE native schema checks, predicates or projection.
// All simultaneously live encoded/decoded allocations have affine pool guards.
#[allow(clippy::too_many_arguments)]
async fn pull<F>(
    input: &mut RunInput,
    source: &mut SpillFile<F>,
    width: usize,
    pool: &MemoryPool,
    policy: GqlQueryPolicy,
    evaluator: &mut ControlledStats<'_>,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<Option<PositionedRow>>
where
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    staged::native_event(work.cx, policy, evaluator, GlaExecutionEvent::Work)?;
    let Some(bytes) = input.next(source, work.cx).await? else {
        return Ok(None);
    };
    let columns = width.checked_add(1).ok_or(SpillError::SizeOverflow)?;
    let ordinal = canonical::trailing_ordinal(bytes.as_ref(), columns, work)?;
    let fields = canonical::visible_prefix(bytes.as_ref(), columns, width, work)?;
    let length = canonical::ROW
        .len()
        .checked_add(8)
        .and_then(|length| length.checked_add(fields.len()))
        .ok_or(SpillError::SizeOverflow)?;
    let mut plain = ChargedVec::<u8>::new(pool, work.cx, length)?;
    work.charge(length)?;
    plain.values.extend_from_slice(canonical::ROW);
    let count = u64::try_from(width).map_err(|_| SpillError::SizeOverflow)?;
    plain.values.extend_from_slice(&count.to_be_bytes());
    plain.values.extend_from_slice(fields);
    let value = decode_borrowed(&plain.values, pool, work, resolver)?;
    Ok(Some(PositionedRow { value, ordinal }))
}

#[allow(clippy::too_many_arguments)]
async fn tag_input<A, B>(
    plan: &SpillSetCrossPlan,
    side: usize,
    keys: &[(usize, usize)],
    input: &NativeResultSpool,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    pool: &MemoryPool,
    policy: GqlQueryPolicy,
    evaluator: &mut ControlledStats<'_>,
    page_bytes: usize,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let cx = work.cx;
    let width = input.encoded_columns;
    let mut reader = position(input);
    let mut writer = destination.paged_writer(cx, page_bytes)?;
    let mut seen = 0_u64;
    let mut rows = 0_u64;
    let mut largest = 0;
    while let Some(bytes) = reader.next(source, cx).await? {
        let row = decode_borrowed(bytes.as_ref(), pool, work, resolver)?;
        // Validate every complete child row, including unmatched and NULL
        // suffixes, before deciding whether it can witness a TRUE equality.
        let mut temporary = pool.reserve(cx, 0).map_err(SpillError::Memory)?;
        let eligible = plan
            .probe_eligible_with_control(&row.row, side, keys, &mut |value| {
                event(cx, policy, evaluator, &mut temporary, value)
            })
            .map_err(failure)?;
        drop(temporary);
        if eligible {
            largest = largest
                .max(write_tagged(&mut writer, bytes.as_ref(), width, &[seen], pool, work).await?);
            rows = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        }
        seen = seen.checked_add(1).ok_or(SpillError::SizeOverflow)?;
    }
    if seen != input.row_count() {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    work.charge(1)?;
    let run = writer.finish(cx).await?;
    Ok(NativeResultSpool {
        columns: input.columns.clone(),
        encoded_columns: width.checked_add(1).ok_or(SpillError::SizeOverflow)?,
        snapshot: input.snapshot,
        kind: ScanKind::Set,
        rows: GqlExecutionStats {
            snapshot_records: input.rows.snapshot_records,
            result_rows: rows,
        },
        evaluator: evaluator.stats,
        max_row_bytes: largest,
        run,
    })
}

#[allow(clippy::too_many_arguments)]
async fn order_input<A, B>(
    keys: &[(usize, usize)],
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
    let capacity = keys.len().checked_add(1).ok_or(SpillError::SizeOverflow)?;
    let mut order =
        ChargedVec::<GraphValueOrder>::new(destination.memory_pool(), work.cx, capacity)?;
    for &(left, right) in keys {
        let column = if side == 0 { left } else { right };
        work.charge(order.values.len().saturating_add(1))?;
        if !order.values.iter().any(|key| key.column == column) {
            order.values.push(GraphValueOrder::ascending(column));
        }
    }
    // Original position follows the ENTIRE semantic key tuple. The sorter's
    // typed full-row tie-break can therefore never split equal key groups.
    order
        .values
        .push(GraphValueOrder::ascending(input.encoded_columns - 1));
    let (sorted, used) = input
        .sort_query_continuing(
            work.cx,
            source,
            destination,
            &order.values,
            true,
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

fn uncached_position(input: &RunInput) -> RunInput {
    RunInput {
        run: input.run.clone(),
        remaining: input.remaining,
        offset: input.offset,
        page: None,
        max_row_bytes: input.max_row_bytes,
    }
}

#[allow(clippy::too_many_arguments)]
fn compare(
    plan: &SpillSetCrossPlan,
    keys: &[(usize, usize)],
    left: &PositionedRow,
    right: &PositionedRow,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    evaluator: &mut ControlledStats<'_>,
) -> Result<Ordering> {
    plan.compare_probe_keys_with_control(&left.value.row, &right.value.row, keys, &mut |value| {
        staged::native_event(cx, policy, evaluator, value)
    })
    .map_err(failure)
}

#[derive(Default)]
struct Output {
    rows: u64,
    largest: usize,
}
impl Output {
    #[allow(clippy::too_many_arguments)]
    async fn candidate<F>(
        &mut self,
        plan: &SpillSetCrossPlan,
        left: &PositionedRow,
        right: &PositionedRow,
        writer: &mut PagedSpillWriter<'_, F>,
        pool: &MemoryPool,
        policy: GqlQueryPolicy,
        evaluator: &mut ControlledStats<'_>,
        max_row_bytes: usize,
        max_input_rows: u64,
        work: &mut Work<'_>,
    ) -> Result<()>
    where
        F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    {
        let cx = work.cx;
        let mut charge = pool.reserve(cx, 0).map_err(SpillError::Memory)?;
        let selected = plan
            .select_pair_with_control(&left.value.row, &right.value.row, &mut |value| {
                event(cx, policy, evaluator, &mut charge, value)
            })
            .map_err(failure)?;
        let Some(row) = selected else {
            return Ok(());
        };
        let next = self.rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        GqlExecutionBudget::result_rows(max_input_rows)
            .check(GqlBudgetDimension::ResultRows, next)
            .map_err(|error| {
                NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
            })?;
        let row = SpoolRow::buffered((row, charge));
        // Public row admission applies to the exact visible output. Private
        // positions are separately charged to memory, append and sort limits.
        let encoded = codec::encode(pool, work, &row.row, max_row_bytes)?;
        let size = write_tagged(
            writer,
            &encoded.0,
            plan.columns().len(),
            &[left.ordinal, right.ordinal],
            pool,
            work,
        )
        .await?;
        self.rows = next;
        self.largest = self.largest.max(size);
        Ok(())
    }
}

// The right head moves only forward between key groups. Each left occurrence
// replays its equal-key group from an immutable uncached bookmark. Retaining
// one head and one candidate bounds memory independently of duplicate count.
#[allow(clippy::too_many_arguments)]
async fn sweep<A, B, C>(
    plan: &SpillSetCrossPlan,
    keys: &[(usize, usize)],
    left: &NativeResultSpool,
    right: &NativeResultSpool,
    left_file: &mut SpillFile<A>,
    right_file: &mut SpillFile<B>,
    writer: &mut PagedSpillWriter<'_, C>,
    output: &mut Output,
    pool: &MemoryPool,
    policy: GqlQueryPolicy,
    evaluator: &mut ControlledStats<'_>,
    max_row_bytes: usize,
    max_input_rows: u64,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<()>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let cx = work.cx;
    let left_width = left.encoded_columns - 1;
    let right_width = right.encoded_columns - 1;
    let mut outer = position(left);
    let mut peer = position(right);
    let mut head = pull(
        &mut peer,
        right_file,
        right_width,
        pool,
        policy,
        evaluator,
        work,
        resolver,
    )
    .await?;
    while let Some(row) = pull(
        &mut outer, left_file, left_width, pool, policy, evaluator, work, resolver,
    )
    .await?
    {
        while let Some(right_row) = head.as_ref() {
            match compare(plan, keys, &row, right_row, cx, policy, evaluator)? {
                Ordering::Less => break,
                Ordering::Greater => {
                    drop(head.take());
                    head = pull(
                        &mut peer,
                        right_file,
                        right_width,
                        pool,
                        policy,
                        evaluator,
                        work,
                        resolver,
                    )
                    .await?;
                }
                Ordering::Equal => {
                    output
                        .candidate(
                            plan,
                            &row,
                            right_row,
                            writer,
                            pool,
                            policy,
                            evaluator,
                            max_row_bytes,
                            max_input_rows,
                            work,
                        )
                        .await?;
                    let mut probe = uncached_position(&peer);
                    // Never hold both a forward-page cache and a replay cache.
                    drop(peer.page.take());
                    while let Some(candidate) = pull(
                        &mut probe,
                        right_file,
                        right_width,
                        pool,
                        policy,
                        evaluator,
                        work,
                        resolver,
                    )
                    .await?
                    {
                        match compare(plan, keys, &row, &candidate, cx, policy, evaluator)? {
                            Ordering::Less => break,
                            Ordering::Greater => return Err(SpillError::InvalidRun.into()),
                            Ordering::Equal => {}
                        }
                        output
                            .candidate(
                                plan,
                                &row,
                                &candidate,
                                writer,
                                pool,
                                policy,
                                evaluator,
                                max_row_bytes,
                                max_input_rows,
                                work,
                            )
                            .await?;
                    }
                    break;
                }
            }
        }
    }
    drop(head);
    // A last left key cannot conceal a corrupt/unread peer suffix. Complete
    // both immutable runs even when no more candidate pairs can be emitted.
    while pull(
        &mut peer,
        right_file,
        right_width,
        pool,
        policy,
        evaluator,
        work,
        resolver,
    )
    .await?
    .is_some()
    {}
    Ok(())
}

struct StripPositions(usize);
impl Window for StripPositions {
    fn evaluation_width(&self) -> usize {
        self.0 + 2
    }
    fn visible_width(&self) -> usize {
        self.0
    }
    fn distinct(&self) -> bool {
        false
    }
    fn offset(&self) -> u64 {
        0
    }
    fn count(&self) -> Option<u64> {
        None
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<A, B, C>(
    plan: &SpillSetCrossPlan,
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
    let pool = scratch.memory_pool().clone();
    let mut charge = pool.reserve(work.cx, 0).map_err(SpillError::Memory)?;
    let keys = plan
        .probe_keys_with_control(&mut |value| {
            event(work.cx, policy, &mut evaluator, &mut charge, value)
        })
        .map_err(failure)?;
    codec::retain_reservation(
        &mut charge,
        keys.capacity()
            .checked_mul(size_of::<(usize, usize)>())
            .ok_or(SpillError::SizeOverflow)?,
        work.cx,
    )?;
    let owned_keys = (keys, charge);
    let keys = owned_keys.0.as_slice();
    if keys.is_empty() {
        return Err(SpillError::InvalidRun.into());
    }
    let left = tag_input(
        plan,
        0,
        keys,
        left,
        destination,
        scratch,
        &pool,
        policy,
        &mut evaluator,
        page_bytes,
        work,
        resolver,
    )
    .await?;
    let left = order_input(
        keys, 0, &left, scratch, auxiliary, run_rows, max_runs, page_bytes, work,
    )
    .await?;
    let right = tag_input(
        plan,
        1,
        keys,
        right,
        destination,
        scratch,
        &pool,
        policy,
        &mut evaluator,
        page_bytes,
        work,
        resolver,
    )
    .await?;
    let right = order_input(
        keys,
        1,
        &right,
        scratch,
        destination,
        run_rows,
        max_runs,
        page_bytes,
        work,
    )
    .await?;
    let mut output = Output::default();
    let mut writer = scratch.paged_writer(work.cx, page_bytes)?;
    sweep(
        plan,
        keys,
        &left,
        &right,
        auxiliary,
        destination,
        &mut writer,
        &mut output,
        &pool,
        policy,
        &mut evaluator,
        max_row_bytes,
        max_input_rows,
        work,
        resolver,
    )
    .await?;
    staged::native_event(work.cx, policy, &mut evaluator, GlaExecutionEvent::Work)?;
    work.charge(1)?;
    let run = writer.finish(work.cx).await?;
    let width = plan.columns().len();
    let tagged = NativeResultSpool {
        columns: plan.columns().to_vec().into(),
        encoded_columns: width.checked_add(2).ok_or(SpillError::SizeOverflow)?,
        snapshot,
        kind: ScanKind::Set,
        rows: GqlExecutionStats {
            snapshot_records,
            result_rows: output.rows,
        },
        evaluator: evaluator.stats,
        max_row_bytes: output.largest,
        run,
    };
    // Restore exact original occurrence order BEFORE a following COLLECT,
    // projection, LIMIT, or user order can observe the selected sequence.
    let (ordered, used) = tagged
        .sort_continuing(
            work.cx,
            scratch,
            destination,
            &[
                GraphValueOrder::ascending(width),
                GraphValueOrder::ascending(width + 1),
            ],
            run_rows,
            max_runs,
            page_bytes,
            work.limit,
            work.used,
        )
        .await?;
    work.used = used;
    let visible = window(
        &ordered,
        destination,
        scratch,
        &StripPositions(width),
        GqlExecutionBudget::result_rows(max_input_rows),
        page_bytes,
        work,
    )
    .await?;
    staged::copy(&visible, scratch, destination, page_bytes, work).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrence_ordinals_preserve_the_entire_unsigned_order() {
        let original = [0, 1, (1_u64 << 63) - 1, 1_u64 << 63, u64::MAX - 1, u64::MAX];
        let mut previous = None;
        for value in original {
            let GraphValue::Scalar(CanonicalScalar::Int(encoded)) = ordinal(value) else {
                panic!("ordinal must use the native fixed-width Int codec")
            };
            assert_eq!(
                u64::from_be_bytes(encoded.to_be_bytes()) ^ (1_u64 << 63),
                value
            );
            if let Some(previous) = previous {
                assert!(previous < encoded);
            }
            previous = Some(encoded);
        }
    }
}
