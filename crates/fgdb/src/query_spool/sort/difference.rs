//! Bounded external bag subtraction over completed native result spools.
//! Ordering and frame admission belong to the existing sorter. This consumer
//! only counts equal, consecutive complete rows, then emits after minus before.

use super::*;
use fgdb_gql::{GqlBudgetDimension, GqlExecutionBudget, GqlQueryError};

// Native errors (including scratch/callback refusal) retain their typed source.
// The public row limit is separate from the sorter's work and file allowances.
type VisitResult<T> = core::result::Result<T, GqlQueryError<NativeSpoolError, Cancel>>;

fn native<T>(result: Result<T>) -> VisitResult<T> {
    result.map_err(GqlQueryError::Source)
}

impl NativeResultSpool {
    /// Visit the exact BAG difference `after - self` without collecting either
    /// completed input or the difference in a resident table.
    ///
    /// Both spools must belong to `source` and have identical visible schemas.
    /// They can come from different admitted historical cuts. This compares
    /// the already selected results, including each query's ORDER BY/page;
    /// rank-only changes are not changes to a bag. It neither proves that the
    /// query definitions match nor grants authority over a database/history.
    /// Native aggregate spools have a different cell domain and are not accepted.
    ///
    /// First externally sort BOTH complete inputs into `scratch`, using the
    /// existing append-only two-file sorter and canonical typed whole-row order.
    /// Original runs remain readable. Then merge the sorted runs through the
    /// existing authenticated reader, retaining at most two lookahead rows, one
    /// representative row and two input pages. The sorter bounds its own run
    /// buffers/catalog. Use a shared MemoryPool for a combined byte allowance;
    /// all frame payloads and comparison-key capacity are charged to the files'
    /// pools. Source/result files and all intermediate runs keep their normal
    /// SpillLimits; there is no recycling, second codec or durable result log.
    ///
    /// The visitor receives one owned, pool-charged canonical GraphValueRow
    /// frame and its NONZERO signed occurrence change. Counts do not expand
    /// into repeated output rows. Each input has a u64 row count, so their exact
    /// difference fits i128, including changes outside i64. NULL, full-width
    /// identities, scalars, lists, maps and paths keep native typed equality.
    /// The visitor's future completes before any further input is pulled.
    /// It may decode with GraphValueRow::decode_canonical; decoded allocations
    /// and any retained visitor output are the consumer's responsibility.
    ///
    /// One `max_work_units` allowance covers schema checks, BOTH sorts, merge
    /// comparisons, counting and delivery checkpoints. This is additional work
    /// over ALREADY completed queries, not a reset or refund of their execution
    /// budgets. `max_changed_rows` counts final nonzero tuples, not occurrences
    /// or intermediate groups. A zero row allowance can accept full cancellation.
    /// `run_rows`, `max_runs` (per input) and `page_bytes` have sort_into's meaning.
    /// Both run caps and schemas are checked before either sort starts.
    ///
    /// Returns (changed tuples delivered, cumulative comparison work) ONLY on
    /// complete success. The visitor can have received an earlier prefix when
    /// a later read, limit, cancellation or visitor fails: stage changes and
    /// publish them only after this future succeeds. Dropping it drains nothing
    /// and releases its tracked buffers; unfinished scratch I/O stays fenced by
    /// the existing SpillFile law. Retrying may repeat callbacks. This is not an
    /// ACK, exactly-once side effect, retention lease or capability-masked API.
    #[allow(clippy::too_many_arguments)]
    pub async fn visit_difference<A, B, F, Fut>(
        &self,
        after: &Self,
        cx: &QueryCx,
        source: &mut SpillFile<A>,
        scratch: &mut SpillFile<B>,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_changed_rows: u64,
        max_work_units: u64,
        mut visitor: F,
    ) -> VisitResult<(u64, u64)>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
        F: FnMut(i128, TrackedBytes) -> Fut + Send,
        Fut: Future<Output = Result<()>> + Send,
    {
        let mut work = Work {
            cx,
            used: 0,
            limit: max_work_units,
        };
        native(work.charge(1))?;
        if run_rows == 0 || page_bytes == 0 || page_bytes > 64 * 1024 {
            return Err(GqlQueryError::Source(SpillError::InvalidLimits.into()));
        }
        let width = self.columns.len();
        if width == 0
            || width > MAX_PATTERN_VERTICES
            || after.columns.len() != width
            || self.encoded_columns != width
            || after.encoded_columns != width
        {
            return Err(GqlQueryError::Source(SpillError::InvalidRun.into()));
        }
        for (left, right) in self.columns.iter().zip(after.columns.iter()) {
            // Bound long schema comparisons too; no callback sees a partial
            // schema, and an early budget refusal cannot start file I/O.
            native(work.charge(1))?;
            if left.len() != right.len() {
                return Err(GqlQueryError::Source(SpillError::InvalidRun.into()));
            }
            for (a, b) in left
                .as_bytes()
                .chunks(1024)
                .zip(right.as_bytes().chunks(1024))
            {
                native(work.charge(a.len()))?;
                if a != b {
                    return Err(GqlQueryError::Source(SpillError::InvalidRun.into()));
                }
            }
        }
        let run_width = u64::try_from(run_rows)
            .map_err(|_| GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
        for input in [self, after] {
            native(work.charge(1))?;
            let required = input.row_count().div_ceil(run_width).max(1);
            if u128::from(required) > max_runs as u128 {
                return Err(GqlQueryError::Source(NativeSpoolError::SortRunLimit {
                    required,
                    limit: max_runs,
                }));
            }
        }
        let pool = source.memory_pool().clone();
        let mut keys = native(ChargedVec::new(&pool, cx, width))?;
        for column in 0..width {
            native(work.charge(1))?;
            keys.values
                .push(GraphValueOrder::ascending(column).with_nulls_first(true));
        }
        // Each sort appends to these same two owners. Both outputs finish in
        // scratch; source's original input runs survive every successful append.
        let (before, used) = native(
            self.sort_continuing(
                cx,
                source,
                scratch,
                &keys.values,
                run_rows,
                max_runs,
                page_bytes,
                max_work_units,
                work.used,
            )
            .await,
        )?;
        work.used = used;
        let (after, used) = native(
            after.sort_continuing(
                cx,
                source,
                scratch,
                &keys.values,
                run_rows,
                max_runs,
                page_bytes,
                max_work_units,
                work.used,
            )
            .await,
        )?;
        work.used = used;
        merge_difference(
            &before,
            &after,
            scratch,
            &keys.values,
            max_changed_rows,
            &mut work,
            &mut visitor,
        )
        .await
    }
}

// Admit each source advance before it can read or allocate a row, including
// EOF checks. Paged I/O still performs its own context and ownership checks.
async fn pull<B: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
    input: &mut Input,
    scratch: &mut SpillFile<B>,
    work: &mut Work<'_>,
) -> VisitResult<Option<TrackedBytes>> {
    native(work.charge(1))?;
    native(input.next(scratch, work.cx).await)
}

#[allow(clippy::too_many_arguments)]
async fn merge_difference<B, F, Fut>(
    before: &NativeResultSpool,
    after: &NativeResultSpool,
    scratch: &mut SpillFile<B>,
    keys: &[GraphValueOrder],
    max_changed_rows: u64,
    work: &mut Work<'_>,
    visitor: &mut F,
) -> VisitResult<(u64, u64)>
where
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    F: FnMut(i128, TrackedBytes) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let input = |spool: &NativeResultSpool| {
        Input::new(
            Run {
                pages: spool.run.clone(),
                rows: spool.row_count(),
            },
            spool.max_row_bytes,
        )
    };
    let (mut left, mut right) = (input(before), input(after));
    let (mut a, mut b) = (
        pull(&mut left, scratch, work).await?,
        pull(&mut right, scratch, work).await?,
    );
    let mut changed = 0_u64;
    let width = before.encoded_columns;
    while a.is_some() || b.is_some() {
        native(work.charge(1))?;
        let take_left = match (&a, &b) {
            (Some(a), Some(b)) => {
                native(canonical::compare(a.as_ref(), b.as_ref(), keys, width, work))?
                    != Ordering::Greater
            }
            (Some(_), None) => true,
            _ => false,
        };
        let row = if take_left { a.take() } else { b.take() }
            .ok_or(GqlQueryError::Source(NativeSpoolError::IncompleteCursor))?;
        let mut weight = if take_left {
            a = pull(&mut left, scratch, work).await?;
            -1_i128
        } else {
            b = pull(&mut right, scratch, work).await?;
            1_i128
        };
        for (head, input, sign) in [
            (&mut a, &mut left, -1_i128),
            (&mut b, &mut right, 1_i128),
        ] {
            while let Some(next) = head.as_ref() {
                native(work.charge(1))?;
                match native(canonical::compare(next.as_ref(), row.as_ref(), keys, width, work))? {
                    Ordering::Less => {
                        return Err(GqlQueryError::Source(SpillError::InvalidRun.into()));
                    }
                    Ordering::Greater => break,
                    Ordering::Equal => {}
                }
                weight = weight
                    .checked_add(sign)
                    .ok_or(GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
                // Refund each duplicate BEFORE loading its successor.
                *head = None;
                *head = pull(input, scratch, work).await?;
            }
        }
        if weight != 0 {
            let next = changed
                .checked_add(1)
                .ok_or(GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
            GqlExecutionBudget::result_rows(max_changed_rows)
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(GqlQueryError::Rows)?;
            native(work.charge(1))?;
            native(visitor(weight, row).await)?;
            changed = next;
            native(work.charge(1))?;
        }
    }
    native(work.charge(1))?;
    Ok((changed, work.used))
}

#[cfg(test)]
#[path = "difference/tests.rs"]
mod tests;
