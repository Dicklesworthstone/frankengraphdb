//! Exact visible aggregate tuples over the existing external sort/merge.
//! No aggregate is reevaluated; private keys never become delivery identity.

use super::*;
use crate::query::view::spool::sort::FrameOrder;
use fgdb_gql::{GqlBudgetDimension, GqlExecutionBudget};
use fgdb_gql::algebra::MAX_PATTERN_VERTICES;
use std::cmp::Ordering;

type DifferenceResult<T> =
    core::result::Result<T, GqlQueryError<NativeAggregateSpoolError, Cancel>>;

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
        let units = left.len().checked_add(right.len())
            .and_then(|bytes| self.repetitions.checked_add(1).and_then(|n| bytes.checked_mul(n)))
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
        let mut charge = self.pool.reserve(work.cx, requested).map_err(SpillError::Memory)?;
        let mut cells = Vec::new();
        cells.try_reserve_exact(slots).map_err(|_| {
            SpillError::Memory(MemoryError::AllocationFailed { requested })
        })?;
        if cells.capacity() > slots {
            let extra = (cells.capacity() - slots)
                .checked_mul(size_of::<GraphAggregateValue>())
                .and_then(|bytes| bytes.checked_mul(2))
                .ok_or(SpillError::SizeOverflow)?;
            charge.grow(work.cx, extra).map_err(SpillError::Memory)?;
        }
        // Admit the total copied payload before the first clone. Native typed
        // comparisons/copies use their bounded row domain, not a new evaluator.
        work.charge(bytes.len().checked_mul(self.repetitions).ok_or(SpillError::SizeOverflow)?)?;
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

fn continue_error(error: NativeAggregateSpoolError, prior: u64, limit: u64) -> NativeAggregateSpoolError {
    match error {
        NativeAggregateSpoolError::Spool(NativeSpoolError::SortWorkLimit { attempted, .. }) => {
            match prior.checked_add(attempted) {
                Some(attempted) => NativeSpoolError::SortWorkLimit { attempted, limit }.into(),
                None => SpillError::SizeOverflow.into(),
            }
        }
        error => error,
    }
}

impl NativeAggregateSpool {
    /// Visit the exact BAG difference of completed aggregate results, after - self.
    /// Both inputs must belong to source_file and have matching visible columns,
    /// RETURN slots and private widths. This does not prove a common query or
    /// history, grant authority, or reevaluate either aggregate.
    ///
    /// Compare the visible RETURN tuple in native GraphAggregateValue order.
    /// Hidden group keys cannot cause spurious changes; repeated/reordered slots,
    /// u64 counts, i128 sums, exact averages, NULLs and ordered COLLECT values
    /// retain their domains. Rank-only changes do not change a result bag.
    ///
    /// The existing external sorter and shared difference merge own the runs,
    /// authenticated readers and signed counting. Neither endpoint nor the final
    /// difference is collected in memory. The merge retains at most three row
    /// frames and two pages; comparisons additionally decode two bounded rows.
    /// Decode and output-copy reservations are acquired before allocation from
    /// source_file's pool. File pages/sort buffers use their existing pools;
    /// use a shared parent pool for a combined bound. A callback-owned row keeps
    /// its reservation until dropped, independently of this operation.
    ///
    /// The callback receives a nonzero signed multiplicity and a row with no
    /// keys: values() is already in columns() order. It is awaited before another
    /// input is pulled. Counts are never expanded into repeated output rows.
    /// One max_work_units covers BOTH sorts, validation, decoding, comparisons,
    /// projection and callback checkpoints. max_changed_rows counts final
    /// nonzero tuples only; full cancellation can pass a zero row limit.
    /// max_runs is PER INPUT; file quotas stay cumulative and append-only.
    /// These are additional comparison budgets, not endpoint query budgets.
    ///
    /// Returns (delivered tuples, physical work) only on COMPLETE success.
    /// Callbacks may already have received a prefix before a late error: stage
    /// changes until this future succeeds. Error, unwind and dropped in-flight
    /// work release merge buffers without draining or acknowledging. Retrying
    /// can repeat callbacks. Pending scratch I/O retains SpillFile's poison
    /// rule; retire a poisoned file rather than treating drop as rollback.
    /// The resolver must preserve its pinned scalar artifacts throughout.
    /// This is not durable resume, cross-branch coverage or authorized DIFF.
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
        let mut work = Work {
            cx,
            used: 0,
            limit: max_work_units,
        };
        source(work.charge(1).map_err(Into::into))?;
        if run_rows == 0 || page_bytes == 0 || page_bytes > 64 * 1024 {
            return source(Err(SpillError::InvalidLimits.into()));
        }
        let width = self.keys.len().checked_add(self.aggregates.len())
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
            for (a, b) in left.as_bytes().chunks(1024).zip(right.as_bytes().chunks(1024)) {
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
                }.into()));
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
        // Exactly two completed handles; the sorter owns all variable run state.
        let mut before_sorted = None;
        for input in [self, after] {
            let prior = work.used;
            let remaining = work.limit.checked_sub(prior).ok_or_else(|| {
                GqlQueryError::Source(NativeSpoolError::IncompleteCursor.into())
            })?;
            let (sorted, used) = source(input.inner.sort_with(
                cx, source_file, scratch, &mut order, run_rows, max_runs,
                page_bytes, remaining,
            ).await.map_err(|error| continue_error(error, prior, max_work_units)))?;
            work.used = prior.checked_add(used)
                .ok_or_else(|| GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
            if let Some(before_sorted) = before_sorted.take() {
                let mut merge = NativeSpoolDifference::<B>::merge_with_order(
                    &before_sorted, &sorted, order,
                );
                let mut changed = 0_u64;
                while let Some((weight, frame)) = merge.advance(scratch, &mut work).await? {
                    let next = changed.checked_add(1).ok_or_else(|| {
                        GqlQueryError::Source(SpillError::SizeOverflow.into())
                    })?;
                    GqlExecutionBudget::result_rows(max_changed_rows)
                        .check(GqlBudgetDimension::ResultRows, next)
                        .map_err(GqlQueryError::Rows)?;
                    let row = source(merge.order().project(frame.as_ref(), &mut work))?;
                    drop(frame);
                    source(work.charge(1).map_err(Into::into))?;
                    source(visitor(weight, row).await)?;
                    changed = next;
                    // A callback cannot hide interruption by returning
                    // success, including after the final delivered tuple.
                    source(work.charge(1).map_err(Into::into))?;
                }
                return Ok((changed, work.used));
            }
            before_sorted = Some(sorted);
        }
        source(Err(NativeSpoolError::IncompleteCursor.into()))
    }
}

#[cfg(test)]
#[path = "aggregate_difference/tests.rs"]
mod tests;
