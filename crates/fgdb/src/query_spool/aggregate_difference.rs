//! Exact visible aggregate tuples over the existing external sort/merge.
//! No aggregate is reevaluated; private keys never become delivery identity.

use super::*;
use crate::query::view::spool::sort::{FrameOrder, Merge};
use fgdb_gql::algebra::MAX_PATTERN_VERTICES;
use fgdb_gql::{GqlBudgetDimension, GqlExecutionBudget};
use std::cmp::Ordering;

type DifferenceResult<T> =
    core::result::Result<T, GqlQueryError<NativeAggregateSpoolError, Cancel>>;
type NextChange = DifferenceResult<Option<(i128, NativeAggregateSpoolRow)>>;

fn source<T>(result: Result<T>) -> DifferenceResult<T> {
    result.map_err(GqlQueryError::Source)
}

// The common, checked physical layout is immutable. Different group keys can
// produce equal visible tuples, and repeated slots do not create new identity.
struct VisibleOrder<'a> {
    slots: Arc<[GraphAggregateTextSlot]>,
    keys: usize,
    aggregates: usize,
    repetitions: usize,
    pool: MemoryPool,
    resolver: Option<&'a (dyn CanonicalScalarResolver + Send + Sync)>,
}

#[derive(Clone, Copy)]
enum Cell<'a> {
    Count(u64),
    Integer(i128),
    Value(&'a GraphValue),
    Average(GraphExactAverage),
}
impl<'a> Cell<'a> {
    fn at(row: &'a GraphAggregateRow, slot: GraphAggregateTextSlot) -> Result<Self> {
        match slot {
            GraphAggregateTextSlot::GroupKey(at) => row
                .keys()
                .get(at)
                .map(Self::Value)
                .ok_or_else(|| SpillError::InvalidRun.into()),
            GraphAggregateTextSlot::Aggregate(at) => match row.values().get(at) {
                Some(GraphAggregateValue::Count(value)) => Ok(Self::Count(*value)),
                Some(GraphAggregateValue::Integer(value)) => Ok(Self::Integer(*value)),
                Some(GraphAggregateValue::Value(value)) => Ok(Self::Value(value)),
                Some(GraphAggregateValue::Average(value)) => Ok(Self::Average(*value)),
                None => invalid(),
            },
        }
    }
    fn rank(self) -> u8 {
        // This is GraphAggregateValue's canonical variant order, NOT numeric
        // equivalence. Count(1), Integer(1), Value(Int(1)) and Average(1/1)
        // are four distinct logical cells even though their numbers agree.
        match self {
            Self::Count(_) => 0,
            Self::Integer(_) => 1,
            Self::Value(_) => 2,
            Self::Average(_) => 3,
        }
    }
    fn compare(self, other: Self) -> Ordering {
        match (self, other) {
            (Self::Count(a), Self::Count(b)) => a.cmp(&b),
            (Self::Integer(a), Self::Integer(b)) => a.cmp(&b),
            (Self::Value(a), Self::Value(b)) => a.cmp(b),
            (Self::Average(a), Self::Average(b)) => a.cmp(&b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
    fn to_owned(self) -> GraphAggregateValue {
        match self {
            Self::Count(value) => GraphAggregateValue::Count(value),
            Self::Integer(value) => GraphAggregateValue::Integer(value),
            Self::Value(value) => GraphAggregateValue::Value(value.clone()),
            Self::Average(value) => GraphAggregateValue::Average(value),
        }
    }
}

impl FrameOrder for VisibleOrder<'_> {
    type Error = NativeAggregateSpoolError;

    fn admit(&mut self, columns: usize) -> Result<()> {
        if self.keys.checked_add(self.aggregates) != Some(columns) {
            return invalid();
        }
        Ok(())
    }
    fn validate(&mut self, bytes: &[u8], columns: usize, work: &mut Work<'_>) -> Result<()> {
        self.admit(columns)?;
        work.charge(bytes.len())?;
        let _charge = decoded_reservation(&self.pool, work.cx, bytes, 2)?;
        let frame = decode_row(bytes, self.resolver)?;
        // Validate the COMPLETE envelope, including omitted groups/aggregates.
        // A discarded column must never make malformed input acceptable.
        decode_envelope(&frame, self.keys, self.aggregates)?;
        Ok(())
    }
    fn compare(
        &mut self,
        left: &[u8],
        right: &[u8],
        columns: usize,
        work: &mut Work<'_>,
    ) -> Result<Ordering> {
        self.admit(columns)?;
        // Decode once, then compare each selected cell at most repetitions
        // times. A repeated large list cannot multiply uncharged comparison.
        let units = left
            .len()
            .checked_add(right.len())
            .and_then(|bytes| {
                self.repetitions
                    .checked_add(1)
                    .and_then(|n| bytes.checked_mul(n))
            })
            .ok_or(SpillError::SizeOverflow)?;
        work.charge(units)?;
        let _left_charge = decoded_reservation(&self.pool, work.cx, left, 2)?;
        let left = decode_row(left, self.resolver)?;
        let left = decode_envelope(&left, self.keys, self.aggregates)?;
        let _right_charge = decoded_reservation(&self.pool, work.cx, right, 2)?;
        let right = decode_row(right, self.resolver)?;
        let right = decode_envelope(&right, self.keys, self.aggregates)?;
        for &slot in self.slots.iter() {
            work.charge(1)?;
            let order = Cell::at(&left, slot)?.compare(Cell::at(&right, slot)?);
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        // No private-row tie-break: hidden keys do not distinguish public rows.
        Ok(Ordering::Equal)
    }
}

impl VisibleOrder<'_> {
    fn project(&self, bytes: &[u8], work: &mut Work<'_>) -> Result<NativeAggregateSpoolRow> {
        work.charge(bytes.len())?;
        let _decoded = decoded_reservation(&self.pool, work.cx, bytes, 2)?;
        let frame = decode_row(bytes, self.resolver)?;
        let row = decode_envelope(&frame, self.keys, self.aggregates)?;
        // A selected source cell is copied at most `repetitions` times. Use
        // the existing codec's allocation-free reservation, not encoded length
        // as a proxy for decoded memory. Include output vector/boxing overlap.
        // The estimate intentionally also includes omitted source cells.
        let slots = self.slots.len();
        let fixed = slots
            .checked_mul(size_of::<GraphAggregateValue>())
            .and_then(|bytes| bytes.checked_mul(2))
            .ok_or(SpillError::SizeOverflow)?;
        let requested = super::decoded(bytes, work.cx)?
            .checked_mul(self.repetitions)
            .and_then(|bytes| bytes.checked_add(fixed))
            .ok_or(SpillError::SizeOverflow)?;
        let mut charge = self
            .pool
            .reserve(work.cx, requested)
            .map_err(SpillError::Memory)?;
        let mut cells = Vec::new();
        cells.try_reserve_exact(slots).map_err(|_| {
            SpillError::Memory(MemoryError::AllocationFailed { requested })
        })?;
        if cells.capacity() > slots {
            let extra = (cells.capacity() - slots)
                .checked_mul(size_of::<GraphAggregateValue>())
                .and_then(|bytes| bytes.checked_mul(2))
                .ok_or(SpillError::SizeOverflow)?;
            charge
                .grow(work.cx, extra)
                .map_err(SpillError::Memory)?;
        }
        // Admit the total copied payload before the first clone. Native typed
        // comparisons/copies use their bounded row domain, not a new evaluator.
        work.charge(
            bytes
                .len()
                .checked_mul(self.repetitions)
                .ok_or(SpillError::SizeOverflow)?,
        )?;
        for &slot in self.slots.iter() {
            work.charge(1)?;
            cells.push(Cell::at(&row, slot)?.to_owned());
        }
        Ok(NativeAggregateSpoolRow {
            // Public values are already in RETURN order. No hidden grouping
            // key or scratch envelope is exposed through the output carrier.
            row: GraphAggregateRow::from_group_values(Vec::new(), cells),
            _charge: charge,
        })
    }
}

fn continue_error(
    error: NativeAggregateSpoolError,
    prior: u64,
    limit: u64,
) -> NativeAggregateSpoolError {
    match error {
        NativeAggregateSpoolError::Spool(NativeSpoolError::SortWorkLimit {
            attempted,
            ..
        }) => match prior.checked_add(attempted) {
            Some(attempted) => NativeSpoolError::SortWorkLimit { attempted, limit }.into(),
            None => SpillError::SizeOverflow.into(),
        },
        error => error,
    }
}

impl NativeAggregateSpool {
    /// Visit the exact BAG difference of completed aggregate results, after - self.
    /// This is the callback facade over difference_stream's single resumable
    /// merge state machine. Both inputs are fully sorted before the first
    /// callback, every callback is awaited, and a post-callback checkpoint is
    /// charged even for the final tuple. A delivered prefix remains provisional
    /// until this method returns success; retry may repeat callbacks.
    #[allow(clippy::too_many_arguments)]
    pub async fn visit_difference<A, B, F, Fut>(
        &self,
        after: &Self,
        cx: &QueryCx,
        source_file: &mut SpillFile<A>,
        scratch: &mut SpillFile<B>,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_changed_rows: u64,
        max_work_units: u64,
        resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
        mut visitor: F,
    ) -> DifferenceResult<(u64, u64)>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
        F: FnMut(i128, NativeAggregateSpoolRow) -> Fut + Send,
        Fut: Future<Output = Result<()>> + Send,
    {
        let mut cursor = self
            .difference_stream(
                after,
                cx,
                source_file,
                scratch,
                run_rows,
                max_runs,
                page_bytes,
                max_changed_rows,
                max_work_units,
                resolver,
            )
            .await?;
        while let Some((weight, row)) = cursor.next_inner().await? {
            source(visitor(weight, row).await)?;
            // A callback cannot hide interruption by returning success,
            // including after the final delivered tuple.
            cursor.visit_completed()?;
        }
        Ok((cursor.changed_rows(), cursor.work_units()))
    }

    /// Compare COMPLETE aggregate results as an exact after-minus-before bag.
    /// Both spools must belong to source and have the same public columns,
    /// RETURN slot mapping and private key/aggregate widths. They need not be
    /// current. This API does not prove common query/history or grant authority.
    ///
    /// External sorting and merge compare only the visible native tuple in
    /// canonical GraphAggregateValue order. Omitted group keys cannot create
    /// spurious changes; repeated/reordered RETURN slots remain intact. Counts,
    /// signed i128 sums, reduced exact averages, NULLs and COLLECT lists preserve
    /// their domains. No float cast, scalar narrowing, new codec or reevaluation
    /// participates. Rank-only changes are not differences between bags.
    ///
    /// Both sorts, validation, decoding, comparisons, projection and pulls share
    /// max_work_units. max_runs is PER INPUT; file quotas remain cumulative and
    /// append-only. max_changed_rows counts nonzero final tuples, not occurrences.
    /// A zero result limit can accept complete cancellation. Quotas describe
    /// additional comparison work over already completed queries, not their
    /// execution cost. Native source/evaluator counters are not reset or reused.
    ///
    /// Merge retains at most three encoded rows and two pages. Comparison may
    /// additionally decode two bounded rows. Decoding and output copies reserve
    /// through source's existing memory pool before allocation; repeated slots
    /// are conservatively reserved by their maximum reuse count. File pages and
    /// sort buffers keep their own pools. Use one shared parent for a combined
    /// bound. Metadata/column handles are shared, not recounted as payloads.
    ///
    /// The cursor borrows only scratch, cx and the pinned scalar resolver. Input
    /// spools, source file, database and query may drop after opening. Returned
    /// rows have no keys: values() is the complete RETURN tuple in columns()
    /// order, with a memory reservation lasting as long as the row. Caller-made
    /// copies use the caller's budget. The resolver must retain the same scalar
    /// artifacts as ordinary aggregate reads; host-locale substitution is illegal.
    /// The concrete cursor type is inferred; no private scratch owner is exposed.
    ///
    /// Stage all delivered changes until state() is Exhausted. Errors, unwind
    /// and dropped IN-FLIGHT pulls permanently fuse delivery; unpolled pulls and
    /// idle cursors perform no I/O. close/drop do not drain or acknowledge.
    /// Pending scratch I/O keeps SpillFile's poison rule. This is not a durable
    /// cursor, authorization boundary or cross-branch coverage proof.
    #[allow(clippy::too_many_arguments, private_interfaces, unnameable_types)]
    pub async fn difference_stream<'a, A, B>(
        &self,
        after: &Self,
        cx: &'a QueryCx,
        source_file: &mut SpillFile<A>,
        scratch: &'a mut SpillFile<B>,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_changed_rows: u64,
        max_work_units: u64,
        resolver: Option<&'a (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> DifferenceResult<NativeAggregateSpoolDifference<'a, B>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    {
        let mut work = Work {
            cx,
            used: 0,
            limit: max_work_units,
        };
        source(work.charge(1).map_err(Into::into))?;
        if run_rows == 0 || page_bytes == 0 || page_bytes > 64 * 1024 {
            return source(Err(SpillError::InvalidLimits.into()));
        }
        let width = self
            .keys
            .len()
            .checked_add(self.aggregates.len())
            .ok_or_else(|| GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
        if width == 0
            || width > MAX_PATTERN_VERTICES
            || self.columns.is_empty()
            || self.columns.len() > MAX_PATTERN_VERTICES
            || self.columns.len() != self.slots.len()
            || self.columns.len() != after.columns.len()
            || self.slots.as_ref() != after.slots.as_ref()
            || self.keys.len() != after.keys.len()
            || self.aggregates.len() != after.aggregates.len()
            || self.inner.encoded_columns != width
            || after.inner.encoded_columns != width
        {
            return source(invalid());
        }
        for (left, right) in self.columns.iter().zip(after.columns.iter()) {
            source(work.charge(1).map_err(Into::into))?;
            if left.len() != right.len() {
                return source(invalid());
            }
            for (a, b) in left
                .as_bytes()
                .chunks(1024)
                .zip(right.as_bytes().chunks(1024))
            {
                source(work.charge(a.len()).map_err(Into::into))?;
                if a != b {
                    return source(invalid());
                }
            }
        }
        let mut repetitions = 0;
        for slot in self.slots.iter() {
            source(work.charge(1).map_err(Into::into))?;
            let valid = match *slot {
                GraphAggregateTextSlot::GroupKey(at) => at < self.keys.len(),
                GraphAggregateTextSlot::Aggregate(at) => at < self.aggregates.len(),
            };
            if !valid {
                return source(invalid());
            }
            let mut count = 0;
            for other in self.slots.iter() {
                source(work.charge(1).map_err(Into::into))?;
                count += usize::from(slot == other);
            }
            repetitions = repetitions.max(count);
        }
        let run_width = u64::try_from(run_rows)
            .map_err(|_| GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
        for input in [self, after] {
            source(work.charge(1).map_err(Into::into))?;
            let required = input.row_count().div_ceil(run_width).max(1);
            if u128::from(required) > max_runs as u128 {
                return source(Err(NativeSpoolError::SortRunLimit {
                    required,
                    limit: max_runs,
                }
                .into()));
            }
        }
        let mut order = VisibleOrder {
            slots: Arc::clone(&self.slots),
            keys: self.keys.len(),
            aggregates: self.aggregates.len(),
            repetitions,
            pool: source_file.memory_pool().clone(),
            resolver,
        };
        // Only two fixed result handles, never a variable run/payload catalog.
        // Avoid storing them in a growable container, even though the count is fixed.
        let mut before_sorted = None;
        for input in [self, after] {
            let prior = work.used;
            let remaining = work.limit.checked_sub(prior).ok_or_else(|| {
                GqlQueryError::Source(NativeSpoolError::IncompleteCursor.into())
            })?;
            let (sorted, used) = source(
                input
                    .inner
                    .sort_with(
                        cx,
                        source_file,
                        scratch,
                        &mut order,
                        run_rows,
                        max_runs,
                        page_bytes,
                        remaining,
                    )
                    .await
                    .map_err(|error| continue_error(error, prior, max_work_units)),
            )?;
            work.used = prior
                .checked_add(used)
                .ok_or_else(|| GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
            if let Some(before_sorted) = before_sorted.take() {
                return Ok(NativeAggregateSpoolDifference {
                    scratch,
                    columns: Arc::clone(&self.columns),
                    before: self.snapshot_seq(),
                    after: after.snapshot_seq(),
                    merge: Some(Merge::new(&before_sorted, &sorted, order)),
                    max_changed_rows,
                    changed: 0,
                    work,
                    state: ScanState::Open,
                });
            }
            before_sorted = Some(sorted);
        }
        // The fixed two-input loop always returns above after both sorts.
        source(Err(NativeSpoolError::IncompleteCursor.into()))
    }
}

/// Backpressured exact aggregate changes. Values in each returned row are
/// already in public RETURN order; private keys/envelopes are never exposed.
pub struct NativeAggregateSpoolDifference<'a, B> {
    scratch: &'a mut SpillFile<B>,
    columns: Arc<[String]>,
    before: CommitSeq,
    after: CommitSeq,
    merge: Option<Merge<VisibleOrder<'a>>>,
    max_changed_rows: u64,
    changed: u64,
    work: Work<'a>,
    state: ScanState,
}
impl<B> NativeAggregateSpoolDifference<'_, B> {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn before(&self) -> CommitSeq {
        self.before
    }
    pub fn after(&self) -> CommitSeq {
        self.after
    }
    pub fn state(&self) -> ScanState {
        self.state
    }
    pub fn changed_rows(&self) -> u64 {
        self.changed
    }
    pub fn work_units(&self) -> u64 {
        self.work.used
    }
    pub fn close(&mut self) {
        self.merge = None;
        if self.state == ScanState::Open {
            self.state = ScanState::Closed;
        }
    }
    fn visit_completed(&mut self) -> DifferenceResult<()> {
        source(self.work.charge(1).map_err(Into::into))
    }
}
impl<B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send> NativeAggregateSpoolDifference<'_, B> {
    pub fn next_change(&mut self) -> crate::SendFuture<'_, NextChange> {
        Box::pin(self.next_inner())
    }
    async fn next_inner(&mut self) -> NextChange {
        if self.state != ScanState::Open {
            return Ok(None);
        }
        self.state = ScanState::Failed;
        let mut merge = self.merge.take().ok_or_else(|| {
            GqlQueryError::Source(NativeAggregateSpoolError::from(
                NativeSpoolError::IncompleteCursor,
            ))
        })?;
        let Some((weight, frame)) = merge.advance(self.scratch, &mut self.work).await? else {
            self.state = ScanState::Exhausted;
            return Ok(None);
        };
        let next = self
            .changed
            .checked_add(1)
            .ok_or_else(|| GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
        GqlExecutionBudget::result_rows(self.max_changed_rows)
            .check(GqlBudgetDimension::ResultRows, next)
            .map_err(GqlQueryError::Rows)?;
        let row = source(merge.order().project(frame.as_ref(), &mut self.work))?;
        drop(frame);
        source(self.work.charge(1).map_err(Into::into))?;
        // Projection and final admission are inside the same failure boundary.
        // A decode/clone refusal or unwind cannot restore a partially delivered group.
        self.changed = next;
        self.merge = Some(merge);
        self.state = ScanState::Open;
        Ok(Some((weight, row)))
    }
}
impl<B> core::fmt::Debug for NativeAggregateSpoolDifference<'_, B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeAggregateSpoolDifference")
            .field("before", &self.before)
            .field("after", &self.after)
            .field("changed_rows", &self.changed)
            .field("state", &self.state)
            .field("schema_and_values", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
#[path = "aggregate_difference/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "aggregate_difference/pull_tests.rs"]
mod pull_tests;
