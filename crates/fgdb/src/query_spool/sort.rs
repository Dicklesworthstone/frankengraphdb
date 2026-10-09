//! External merge ordering of authenticated native result frames. No graph
//! evaluator, scalar codec, durable result format or alternate scratch owner.

use super::*;
use fgdb_gql::algebra::{GraphOrderError, GraphValueOrder, MAX_PATTERN_VERTICES};
use fgdb_strata::tiered::memory::spill::PagedSpillWriter;
use fgdb_strata::tiered::memory::{MemoryCharge, MemoryError, MemoryPool};
use std::cmp::Ordering;

// This file is loaded through #[path] from query_spool.rs, so its children
// resolve from query_spool/, as a mod.rs's would. Name them explicitly.
#[path = "sort/canonical.rs"]
mod canonical;
#[path = "sort/prepared.rs"]
mod prepared;
pub use prepared::PreparedBufferedOrder;

type Result<T> = core::result::Result<T, NativeSpoolError>;

// External aggregate argument support uses typed equality on a selected tuple,
// not complete-row equality or the result layer's numeric-equivalence domain.
pub(super) fn equal_columns(
    left: &[u8],
    right: &[u8],
    selected: &[GraphValueOrder],
    columns: usize,
    work: &mut Work<'_>,
) -> Result<bool> {
    canonical::equal_columns(left, right, selected, columns, work)
}

pub(super) struct Work<'a> {
    pub(super) cx: &'a QueryCx,
    pub(super) used: u64,
    pub(super) limit: u64,
}
impl Work<'_> {
    pub(super) fn charge(&mut self, units: usize) -> Result<()> {
        self.cx
            .with_restriction(|| self.cx.checkpoint())
            .map_err(SpillError::Interrupted)?;
        let units = u64::try_from(units).map_err(|_| SpillError::SizeOverflow)?;
        let attempted = self
            .used
            .checked_add(units)
            .ok_or(SpillError::SizeOverflow)?;
        if attempted > self.limit {
            return Err(NativeSpoolError::SortWorkLimit {
                attempted,
                limit: self.limit,
            });
        }
        self.used = attempted;
        Ok(())
    }
}

/// Private semantic comparison seam. The physical sorter still owns every
/// run, allocation, merge and byte charge. Aggregate callers retain their exact
/// numeric domains and cumulative evaluator while supplying this comparison.
pub(super) trait FrameOrder {
    type Error: From<NativeSpoolError> + From<SpillError>;

    fn admit(&mut self, columns: usize) -> core::result::Result<(), Self::Error>;
    fn validate(
        &mut self,
        row: &[u8],
        columns: usize,
        work: &mut Work<'_>,
    ) -> core::result::Result<(), Self::Error>;
    fn compare(
        &mut self,
        left: &[u8],
        right: &[u8],
        columns: usize,
        work: &mut Work<'_>,
    ) -> core::result::Result<Ordering, Self::Error>;
}

struct CanonicalOrder<'a>(&'a [GraphValueOrder]);
impl FrameOrder for CanonicalOrder<'_> {
    type Error = NativeSpoolError;

    fn admit(&mut self, columns: usize) -> Result<()> {
        validate_order(self.0, columns)
    }
    fn validate(&mut self, row: &[u8], columns: usize, work: &mut Work<'_>) -> Result<()> {
        canonical::validate(row, columns, work)
    }
    fn compare(
        &mut self,
        left: &[u8],
        right: &[u8],
        columns: usize,
        work: &mut Work<'_>,
    ) -> Result<Ordering> {
        canonical::compare(left, right, self.0, columns, work)
    }
}

// Payloads drop before their capacity charges. reserve_exact may return extra
// capacity; admit that too before any vector is made available to the sorter.
struct ChargedVec<T> {
    values: Vec<T>,
    _charge: MemoryCharge,
    _extra: Option<MemoryCharge>,
}
impl<T> ChargedVec<T> {
    fn new(pool: &MemoryPool, cx: &QueryCx, capacity: usize) -> Result<Self> {
        let requested = capacity
            .checked_mul(size_of::<T>())
            .ok_or(SpillError::SizeOverflow)?;
        let charge = pool.reserve(cx, requested).map_err(SpillError::Memory)?;
        let mut values = Vec::<T>::new();
        values
            .try_reserve_exact(capacity)
            .map_err(|_| SpillError::Memory(MemoryError::AllocationFailed { requested }))?;
        let actual = values
            .capacity()
            .checked_mul(size_of::<T>())
            .ok_or(SpillError::SizeOverflow)?;
        let extra = if actual > requested {
            Some(
                pool.reserve(cx, actual - requested)
                    .map_err(SpillError::Memory)?,
            )
        } else {
            None
        };
        Ok(Self {
            values,
            _charge: charge,
            _extra: extra,
        })
    }
}

#[derive(Clone)]
struct Run {
    pages: PagedSpillRun,
    rows: u64,
}

// Interleave two positions over ONE file without cloning a file handle or
// duplicating the authenticated frame reader. Each next() temporarily lends
// the same exclusive file to the ordinary NativeSpoolCursor. Cached pages
// have already authenticated against this exact immutable run.
struct Input {
    run: Run,
    remaining: u64,
    offset: usize,
    page: Option<(u64, TrackedBytes)>,
    max_row_bytes: usize,
}
impl Input {
    fn new(run: Run, max_row_bytes: usize) -> Self {
        Self {
            remaining: run.rows,
            run,
            offset: 0,
            page: None,
            max_row_bytes,
        }
    }
    async fn next<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
        &mut self,
        scratch: &mut SpillFile<F>,
        cx: &QueryCx,
    ) -> Result<Option<TrackedBytes>> {
        let mut cursor = NativeSpoolCursor {
            scratch,
            run: self.run.pages.clone(),
            remaining: self.remaining,
            max_row_bytes: self.max_row_bytes,
            offset: self.offset,
            page: self.page.take(),
            state: if self.remaining == 0 {
                ScanState::Exhausted
            } else {
                ScanState::Open
            },
        };
        let result = cursor.next_row(cx).await?;
        self.remaining = cursor.remaining;
        self.offset = cursor.offset;
        self.page = cursor.page.take();
        Ok(result)
    }
}

fn validate_order(order: &[GraphValueOrder], columns: usize) -> Result<()> {
    if order.is_empty() {
        return Err(NativeSpoolError::SortOrder(GraphOrderError::EmptyOrder));
    }
    if order.len() > MAX_PATTERN_VERTICES {
        return Err(NativeSpoolError::SortOrder(
            GraphOrderError::TooManyColumns {
                limit: MAX_PATTERN_VERTICES,
                observed: order.len(),
            },
        ));
    }
    for (at, key) in order.iter().enumerate() {
        if key.column >= columns {
            return Err(NativeSpoolError::SortOrder(
                GraphOrderError::UnknownColumn { column: key.column },
            ));
        }
        if order[..at].iter().any(|before| before.column == key.column) {
            return Err(NativeSpoolError::SortOrder(
                GraphOrderError::DuplicateColumn { column: key.column },
            ));
        }
    }
    Ok(())
}

impl NativeResultSpool {
    /// Sort this COMPLETE result into destination, without collecting all rows.
    ///
    /// `order` names output columns and uses GraphValueOrder's typed ordering:
    /// explicit null placement, ascending/descending nonnull values, then the
    /// canonical whole row as the deterministic tie-break. Duplicate rows are
    /// preserved. Comparisons borrow the native canonical frames; length
    /// prefixes are not mistaken for value order and no scalar is decoded.
    ///
    /// This sorts the rows ALREADY selected by the input query, including any
    /// original SKIP/LIMIT. It is not a rewrite of that query's ORDER BY. Both
    /// returned schema/snapshot and original evaluator statistics are unchanged;
    /// the second return value is additional logical sort work, not I/O/latency.
    ///
    /// Exactly `run_rows` rows (except the last run) are retained for fallible
    /// in-place heapsort. Pairwise merge passes retain two row heads, two cached
    /// input pages and one output page, not an entire run. All frame buffers and
    /// vector capacities use their file's MemoryPool. Run metadata is O(initial
    /// runs), explicitly capped by max_runs and charged before source demand.
    /// Hosts should give both files children of a common pool for a total cap.
    /// Minimum headroom must cover row buffers, page buffers, writer metadata
    /// and the two catalogs; choosing oversized run_rows can refuse admission.
    ///
    /// Source and destination must be distinct, exclusively owned scratch
    /// files. Passes alternate between them, always APPENDING. A final copy, if
    /// needed, places the result in destination; the original run is unmodified.
    /// Logical file/run limits include all intermediate runs (no recycling).
    /// Budget/cancellation/I/O/integrity failure or dropped future returns no
    /// sorted-result handle. Finished intermediate runs still consume quota;
    /// unfinished I/O/writers keep the existing poisoned-file cleanup law.
    ///
    /// max_work_units covers output frame bytes, structural visits, comparisons
    /// and merge steps. It does not count page I/O or hashing; the paged I/O
    /// implementation separately checks its QueryCx and file quotas. Each page
    /// authenticates before use; unread corrupt pages cannot enter comparisons.
    /// There is no eager fallback, new storage authority, Warden grant, spill
    /// for the decoded graph, or full larger-than-memory query-engine claim.
    ///
    /// Type-erased like the spool and commit chokepoints (fgdb-a5y6m): a
    /// caller's `Send` proof stops at `dyn Future + Send` instead of
    /// descending the whole run/merge chain, which the aggregate finish and
    /// ordered spill paths otherwise pushed past the recursion limit.
    #[allow(clippy::too_many_arguments)]
    pub fn sort_into<'a, A, B>(
        &'a self,
        cx: &'a QueryCx,
        source: &'a mut SpillFile<A>,
        destination: &'a mut SpillFile<B>,
        order: &'a [GraphValueOrder],
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_work_units: u64,
    ) -> crate::SendFuture<'a, Result<(Self, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'a,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'a,
    {
        Box::pin(async move {
            self.sort_with(
                cx,
                source,
                destination,
                &mut CanonicalOrder(order),
                run_rows,
                max_runs,
                page_bytes,
                max_work_units,
            )
            .await
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn sort_with<A, B, O: FrameOrder + Send>(
        &self,
        cx: &QueryCx,
        source: &mut SpillFile<A>,
        destination: &mut SpillFile<B>,
        order: &mut O,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_work_units: u64,
    ) -> core::result::Result<(Self, u64), O::Error>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
        O::Error: Send,
    {
        cx.with_restriction_async(self.sort_inner(
            cx,
            source,
            destination,
            order,
            run_rows,
            max_runs,
            page_bytes,
            max_work_units,
            0,
        ))
        .await
    }

    // Buffered native intake has already spent encoding work. Continue that
    // SAME allowance, preserving the original limit in every reported refusal.
    // Ordinary sort_into/sort_with begin at zero and keep their public contract.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn sort_continuing<'a, A, B>(
        &'a self,
        cx: &'a QueryCx,
        source: &'a mut SpillFile<A>,
        destination: &'a mut SpillFile<B>,
        order: &'a [GraphValueOrder],
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_work_units: u64,
        prior_work: u64,
    ) -> crate::SendFuture<'a, Result<(Self, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'a,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'a,
    {
        Box::pin(async move {
            cx.with_restriction_async(self.sort_inner(
                cx,
                source,
                destination,
                &mut CanonicalOrder(order),
                run_rows,
                max_runs,
                page_bytes,
                max_work_units,
                prior_work,
            ))
            .await
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn sort_inner<A, B, O: FrameOrder + Send>(
        &self,
        cx: &QueryCx,
        source: &mut SpillFile<A>,
        destination: &mut SpillFile<B>,
        order: &mut O,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_work_units: u64,
        prior_work: u64,
    ) -> core::result::Result<(Self, u64), O::Error>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
        O::Error: Send,
    {
        let mut work = Work {
            cx,
            used: prior_work,
            limit: max_work_units,
        };
        work.charge(1)?;
        order.admit(self.encoded_columns)?;
        if run_rows == 0 || page_bytes == 0 || page_bytes > 64 * 1024 {
            return Err(SpillError::InvalidLimits.into());
        }
        let width = u64::try_from(run_rows).map_err(|_| SpillError::SizeOverflow)?;
        let required = self.row_count().div_ceil(width).max(1);
        if u128::from(required) > max_runs as u128 {
            return Err(NativeSpoolError::SortRunLimit {
                required,
                limit: max_runs,
            }
            .into());
        }
        let count = usize::try_from(required).map_err(|_| SpillError::SizeOverflow)?;
        let pool = source.memory_pool().clone();
        let mut current = ChargedVec::<Run>::new(&pool, cx, count)?;
        let mut next = ChargedVec::<Run>::new(&pool, cx, count.div_ceil(2))?;
        let capacity =
            usize::try_from(self.row_count().min(width)).map_err(|_| SpillError::SizeOverflow)?;
        {
            let mut rows = ChargedVec::<TrackedBytes>::new(&pool, cx, capacity)?;
            let mut input = self.reader(source);
            let mut remaining = self.row_count();
            loop {
                for _ in 0..remaining.min(width) {
                    work.charge(1)?;
                    let row = input
                        .next_row(cx)
                        .await?
                        .ok_or(NativeSpoolError::IncompleteCursor)?;
                    order.validate(row.as_ref(), self.encoded_columns, &mut work)?;
                    rows.values.push(row);
                }
                heap_sort(&mut rows.values, order, self.encoded_columns, &mut work)?;
                let rows_written = rows.values.len() as u64;
                let mut writer = destination.paged_writer(cx, page_bytes)?;
                for row in &rows.values {
                    write_row(&mut writer, row.as_ref(), &mut work).await?;
                }
                work.charge(1)?;
                let pages = writer.finish(cx).await?;
                current.values.push(Run {
                    pages,
                    rows: rows_written,
                });
                remaining -= rows_written;
                rows.values.clear();
                if remaining == 0 {
                    break;
                }
            }
            if input.next_row(cx).await?.is_some() || input.state() != ScanState::Exhausted {
                return Err(NativeSpoolError::IncompleteCursor.into());
            }
        }
        let mut in_destination = true;
        while current.values.len() > 1 {
            work.charge(1)?;
            if in_destination {
                merge_pass(
                    destination,
                    source,
                    &current.values,
                    &mut next.values,
                    self.max_row_bytes,
                    page_bytes,
                    order,
                    self.encoded_columns,
                    &mut work,
                )
                .await?;
            } else {
                merge_pass(
                    source,
                    destination,
                    &current.values,
                    &mut next.values,
                    self.max_row_bytes,
                    page_bytes,
                    order,
                    self.encoded_columns,
                    &mut work,
                )
                .await?;
            }
            current.values.clear();
            std::mem::swap(&mut current, &mut next);
            in_destination = !in_destination;
        }
        let mut final_run = current
            .values
            .pop()
            .ok_or(NativeSpoolError::IncompleteCursor)?;
        if !in_destination {
            final_run = merge_pair(
                source,
                destination,
                &final_run,
                None,
                self.max_row_bytes,
                page_bytes,
                order,
                self.encoded_columns,
                &mut work,
            )
            .await?;
        }
        if final_run.rows != self.row_count() || final_run.pages.len() != self.encoded_len() {
            return Err(NativeSpoolError::IncompleteCursor.into());
        }
        work.charge(1)?;
        Ok((
            Self {
                columns: Arc::clone(&self.columns),
                encoded_columns: self.encoded_columns,
                snapshot: self.snapshot,
                kind: self.kind,
                rows: self.rows,
                evaluator: self.evaluator,
                max_row_bytes: self.max_row_bytes,
                run: final_run.pages,
            },
            work.used,
        ))
    }
}

async fn write_row<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
    writer: &mut PagedSpillWriter<'_, F>,
    row: &[u8],
    work: &mut Work<'_>,
) -> Result<()> {
    work.charge(row.len())?;
    let len = u64::try_from(row.len()).map_err(|_| SpillError::SizeOverflow)?;
    writer.write(work.cx, &len.to_be_bytes()).await?;
    writer.write(work.cx, row).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn merge_pass<A, B, O: FrameOrder + Send>(
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    runs: &[Run],
    output: &mut Vec<Run>,
    max_row_bytes: usize,
    page_bytes: usize,
    order: &mut O,
    columns: usize,
    work: &mut Work<'_>,
) -> core::result::Result<(), O::Error>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    O::Error: Send,
{
    debug_assert!(output.is_empty() && output.capacity() >= runs.len().div_ceil(2));
    for pair in runs.chunks(2) {
        work.charge(1)?;
        output.push(
            merge_pair(
                source,
                destination,
                &pair[0],
                pair.get(1),
                max_row_bytes,
                page_bytes,
                order,
                columns,
                work,
            )
            .await?,
        );
    }
    Ok(())
}

/// Type-erased because both merge callers cross it once per pair: a caller's
/// `Send` proof stops at `dyn Future + Send` instead of descending through
/// the run readers and the paged writer, which the aggregate finish and the
/// ordered spill paths otherwise pushed past the recursion limit.
#[allow(clippy::too_many_arguments)]
fn merge_pair<'a, 'w, A, B, O: FrameOrder + Send>(
    source: &'a mut SpillFile<A>,
    destination: &'a mut SpillFile<B>,
    left: &'a Run,
    right: Option<&'a Run>,
    max_row_bytes: usize,
    page_bytes: usize,
    order: &'a mut O,
    columns: usize,
    work: &'a mut Work<'w>,
) -> crate::SendFuture<'a, core::result::Result<Run, O::Error>>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    O::Error: Send,
    'w: 'a,
{
    Box::pin(merge_pair_inner(
        source,
        destination,
        left,
        right,
        max_row_bytes,
        page_bytes,
        order,
        columns,
        work,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn merge_pair_inner<A, B, O: FrameOrder>(
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    left: &Run,
    right: Option<&Run>,
    max_row_bytes: usize,
    page_bytes: usize,
    order: &mut O,
    columns: usize,
    work: &mut Work<'_>,
) -> core::result::Result<Run, O::Error>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    let expected = left
        .rows
        .checked_add(right.map_or(0, |run| run.rows))
        .ok_or(SpillError::SizeOverflow)?;
    let mut left = Input::new(left.clone(), max_row_bytes);
    let mut right = right.map(|run| Input::new(run.clone(), max_row_bytes));
    let mut a = left.next(source, work.cx).await?;
    let mut b = match &mut right {
        Some(input) => input.next(source, work.cx).await?,
        None => None,
    };
    let mut writer = destination.paged_writer(work.cx, page_bytes)?;
    let mut rows = 0_u64;
    while a.is_some() || b.is_some() {
        work.charge(1)?;
        let take_left = match (&a, &b) {
            (Some(a), Some(b)) => {
                order.compare(a.as_ref(), b.as_ref(), columns, work)? != Ordering::Greater
            }
            (Some(_), None) => true,
            _ => false,
        };
        let row = if take_left { a.take() } else { b.take() }
            .ok_or(NativeSpoolError::IncompleteCursor)?;
        write_row(&mut writer, row.as_ref(), work).await?;
        drop(row); // refund the previous head BEFORE loading the next one
        rows = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        if take_left {
            a = left.next(source, work.cx).await?;
        } else if let Some(input) = &mut right {
            b = input.next(source, work.cx).await?;
        }
    }
    if rows != expected {
        return Err(NativeSpoolError::IncompleteCursor.into());
    }
    work.charge(1)?;
    Ok(Run {
        pages: writer.finish(work.cx).await?,
        rows,
    })
}

// A fallible, allocation-free sort rather than swallowing control failures in
// std's infallible comparator. Refusal stops immediately and publishes no run.
fn heap_sort<O: FrameOrder>(
    rows: &mut [TrackedBytes],
    order: &mut O,
    columns: usize,
    work: &mut Work<'_>,
) -> core::result::Result<(), O::Error> {
    let len = rows.len();
    for root in (0..len / 2).rev() {
        sift(rows, root, len, order, columns, work)?;
    }
    for end in (1..len).rev() {
        work.charge(1)?;
        rows.swap(0, end);
        sift(rows, 0, end, order, columns, work)?;
    }
    Ok(())
}
fn sift<O: FrameOrder>(
    rows: &mut [TrackedBytes],
    mut root: usize,
    end: usize,
    order: &mut O,
    columns: usize,
    work: &mut Work<'_>,
) -> core::result::Result<(), O::Error> {
    while root < end / 2 {
        let mut child = root * 2 + 1;
        if child + 1 < end
            && order.compare(
                rows[child].as_ref(),
                rows[child + 1].as_ref(),
                columns,
                work,
            )? == Ordering::Less
        {
            child += 1;
        }
        if order.compare(rows[root].as_ref(), rows[child].as_ref(), columns, work)?
            != Ordering::Less
        {
            break;
        }
        work.charge(1)?;
        rows.swap(root, child);
        root = child;
    }
    Ok(())
}

#[cfg(test)]
#[path = "sort/tests.rs"]
mod tests;
