//! Incoming access is a compressed permutation of an admitted outgoing image.
//!
//! For a fixed (destination, relation), original incidence positions increase
//! in (source, EId, content-version) order. Encode those positions, NOT another
//! copy of neighbors, edge identities, visibility intervals or property rows.
//! A cursor dereferences the same immutable source image on both faces. No
//! caller-supplied permutation or serialized index can acquire source authority.

use super::image::{Row, RowScan, RowScanStep, Storage, reserved};
use super::{
    Image, SealedAnchor, SealedBuildEvent, SealedEdge, SealedError, SealedLimits, SealedPartition,
    SealedScanBudget, SealedScanStep, check_limit,
};
use fgdb_codec::ef_payload::{self, EfPayloadLimits};
use fgdb_codec::elias_fano::{EliasFano, EliasFanoSequentialCursor, EntryLimit};
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::mem::size_of;
use std::sync::Arc;

#[path = "incoming_transpose.rs"]
mod transpose;
use transpose::{Bucket, TransposeError};

impl From<TransposeError> for SealedError {
    fn from(error: TransposeError) -> Self {
        match error {
            TransposeError::SizeOverflow => Self::SizeOverflow,
            TransposeError::InvalidPosition => Self::NonCanonical,
        }
    }
}

// Bound each existing, non-preemptible EF construction call. Counting/scatter,
// sorting, directory walks and source incidences checkpoint separately.
const CHUNK_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncomingIndexLimits {
    pub max_rows: usize,
    pub max_incidences: usize,
    /// Conservative peak of requested vector backing stores during construction,
    /// including the retained index and construction scratch. Excludes the original
    /// image, fixed stack scratch, Arc metadata and allocator rounding/overhead.
    pub max_workspace_bytes: usize,
}

impl Default for IncomingIndexLimits {
    fn default() -> Self {
        Self {
            max_rows: 1_000_000,
            max_incidences: 1_000_000,
            max_workspace_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncomingIndexStats {
    pub rows: usize,
    pub incidences: usize,
    pub chunks: usize,
    pub charged_resident_bytes: usize,
    pub charged_workspace_bytes: usize,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct IncidenceRef {
    destination: VId,
    relation: RelationId,
    position: u64,
}

struct IncomingRow {
    destination: VId,
    relation: RelationId,
    first_chunk: usize,
    end_chunk: usize,
    incidences: usize,
}

pub(super) struct Index {
    rows: Vec<IncomingRow>,
    chunks: Vec<EliasFano>,
    stats: IncomingIndexStats,
    retained_bytes: usize,
}

/// Clone-shared incoming index that also pins its ORIGINAL authenticated image.
/// Construction is in-core; this is not an external-memory index or a durable
/// GC lease. Persist the source with its existing protocol and derive this cache
/// again after authenticated reload. The source image's bytes/anchor never change.
#[derive(Clone)]
pub struct SealedIncomingIndex {
    source: SealedPartition,
    index: Arc<Index>,
}

impl core::fmt::Debug for SealedIncomingIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SealedIncomingIndex")
            .field("scope", &self.source.anchor().scope())
            .field("stats", &self.index.stats)
            .finish()
    }
}

fn add(left: usize, right: usize) -> Result<usize, SealedError> {
    left.checked_add(right).ok_or(SealedError::SizeOverflow)
}

fn mul(left: usize, right: usize) -> Result<usize, SealedError> {
    left.checked_mul(right).ok_or(SealedError::SizeOverflow)
}

/// The existing scalar EF layout: packed low words, unary high words and one
/// u32 rank per high word. This is requested backing storage, not an RSS claim.
fn ef_bytes(count: usize, maximum: u64) -> Result<usize, SealedError> {
    if count == 0 {
        return Ok(0);
    }
    let count64 = u64::try_from(count).map_err(|_| SealedError::SizeOverflow)?;
    let ratio = maximum / count64;
    let low = if ratio == 0 {
        0
    } else {
        u64::BITS - 1 - ratio.leading_zeros()
    };
    let low_words = mul(count, low as usize)?.div_ceil(64);
    let high_bits = (maximum >> low)
        .checked_add(count64)
        .ok_or(SealedError::SizeOverflow)?;
    let high_words = usize::try_from(high_bits)
        .map_err(|_| SealedError::SizeOverflow)?
        .div_ceil(64);
    add(
        mul(low_words, size_of::<u64>())?,
        mul(high_words, size_of::<u64>() + size_of::<u32>())?,
    )
}

impl SealedPartition {
    /// Count retained versions in one outgoing descriptor, INCLUDING invisible
    /// history. This is a low-level source-cost bound, not a visible degree or
    /// security-filtered graph statistic. Zero outside a scoped anchor's
    /// relation describes this image only; it does not prove graph absence.
    pub fn retained_row_len(&self, source: VId, relation: RelationId) -> usize {
        self.image
            .find_row(source, relation)
            .map_or(0, |row| row.len())
    }

    /// Derive incoming access exclusively from this already-admitted image.
    /// No edge payload or property row is copied; all versions remain available
    /// over this image's exact [floor, publication] interval.
    pub fn incoming_index(
        &self,
        cx: &QueryCx,
        limits: IncomingIndexLimits,
    ) -> Result<SealedIncomingIndex, SealedError> {
        SealedIncomingIndex::build(self, limits, &mut || {
            cx.checkpoint().map_err(SealedError::Interrupted)
        })
    }

    /// Derive the same source-bound index under an additional live checkpoint.
    /// The query context is always checked first; the caller cannot replace
    /// cancellation or image admission. A typed checkpoint failure abandons
    /// all partial sort/directory/encoding state without returning an index.
    pub fn incoming_index_with_checkpoint<E: From<SealedError>>(
        &self,
        cx: &QueryCx,
        limits: IncomingIndexLimits,
        mut checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<SealedIncomingIndex, E> {
        SealedIncomingIndex::build_controlled(self, limits, &mut || {
            cx.checkpoint().map_err(SealedError::Interrupted)?;
            checkpoint()
        })
    }

    /// Build the same source-bound incoming index with cooperative scheduling.
    /// The quantum bounds checkpointed steps (collection, count/scatter,
    /// comparisons, sift levels, group walks and chunks), not CPU time or
    /// allocator latency.
    /// Each indivisible EF construction contains at most 256 locators. The
    /// caller supplies its runtime yield; an immediately ready future opts out
    /// of scheduling fairness. No detached task or new source authority exists.
    pub async fn incoming_index_cooperative<Y, F>(
        &self,
        cx: &QueryCx,
        limits: IncomingIndexLimits,
        quantum: std::num::NonZeroUsize,
        yield_now: Y,
    ) -> Result<SealedIncomingIndex, SealedError>
    where
        Y: FnMut() -> F,
        F: std::future::Future<Output = ()>,
    {
        self.incoming_index_cooperative_with_checkpoint(cx, limits, quantum, yield_now, || {
            Ok::<(), SealedError>(())
        })
        .await
    }

    /// Keep QueryCx-first typed live checks across every build stage and both
    /// sides of each suspension. The future exclusively owns temporary refs,
    /// sort progress and unexposed output; cancel/refusal/drop releases it
    /// without changing the original image or publishing a partial index.
    pub async fn incoming_index_cooperative_with_checkpoint<E, Y, F>(
        &self,
        cx: &QueryCx,
        limits: IncomingIndexLimits,
        quantum: std::num::NonZeroUsize,
        yield_now: Y,
        mut checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<SealedIncomingIndex, E>
    where
        E: From<SealedError>,
        Y: FnMut() -> F,
        F: std::future::Future<Output = ()>,
    {
        self.incoming_index_cooperative_with_events(cx, limits, quantum, yield_now, |_| {
            checkpoint()
        })
        .await
    }

    /// Distinguish logical build work from extra scheduling checkpoints while
    /// retaining the same QueryCx-first live checks. The Work event sequence
    /// equals `incoming_index_with_checkpoint` for every quantum. Scheduling
    /// events preserve caller liveness after yields without charging the
    /// scheduler's chosen quantum as additional logical source work.
    pub async fn incoming_index_cooperative_with_events<E, Y, F>(
        &self,
        cx: &QueryCx,
        limits: IncomingIndexLimits,
        quantum: std::num::NonZeroUsize,
        yield_now: Y,
        checkpoint: impl FnMut(SealedBuildEvent) -> Result<(), E>,
    ) -> Result<SealedIncomingIndex, E>
    where
        E: From<SealedError>,
        Y: FnMut() -> F,
        F: std::future::Future<Output = ()>,
    {
        cx.with_restriction_async(async {
            let mut control = construction::Cooperative {
                cx,
                fuel: SealedScanBudget::new(quantum.get()),
                quantum,
                yield_now,
                guard: checkpoint,
            };
            construction::build(self, limits, &mut control).await
        })
        .await
    }
}

impl SealedIncomingIndex {
    fn build(
        source: &SealedPartition,
        limits: IncomingIndexLimits,
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<Self, SealedError> {
        Self::build_controlled(source, limits, checkpoint)
    }

    fn build_controlled<E: From<SealedError>>(
        source: &SealedPartition,
        limits: IncomingIndexLimits,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        construction::build_sync(source, limits, checkpoint)
    }

    pub fn source(&self) -> &SealedPartition {
        &self.source
    }
    pub fn source_anchor(&self) -> SealedAnchor {
        self.source.anchor()
    }
    pub fn stats(&self) -> IncomingIndexStats {
        self.index.stats
    }

    /// Actual owned index capacities and inline metadata, excluding its shared
    /// source image, Arc control block, allocator headers and build scratch.
    /// This is measured once during construction, including every EF directory.
    pub fn retained_index_bytes(&self) -> usize {
        self.index.retained_bytes
    }
    pub fn shares_index_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.index, &other.index)
    }
    pub(super) fn index_arc(&self) -> Arc<Index> {
        Arc::clone(&self.index)
    }

    fn find(&self, destination: VId, relation: RelationId) -> Option<&IncomingRow> {
        self.index
            .rows
            .binary_search_by_key(&(destination, relation), |row| {
                (row.destination, row.relation)
            })
            .ok()
            .map(|index| &self.index.rows[index])
    }

    /// Retained incidence count, not a visible or authorized graph degree.
    /// Zero outside the source anchor's relation is physical image metadata,
    /// not proof of an empty graph row.
    pub fn retained_row_len(&self, destination: VId, relation: RelationId) -> usize {
        self.find(destination, relation)
            .map_or(0, |row| row.incidences)
    }

    pub fn row(
        &self,
        cx: &QueryCx,
        destination: VId,
        relation: RelationId,
        as_of: CommitSeq,
    ) -> Result<SealedIncomingCursor<'_>, SealedError> {
        self.row_from(cx, destination, relation, as_of, None)
    }

    /// Seek by the original SOURCE identity. Every returned edge retains its
    /// original src/dst orientation and borrows its original property slice.
    pub fn row_from(
        &self,
        cx: &QueryCx,
        destination: VId,
        relation: RelationId,
        as_of: CommitSeq,
        lower_source: Option<VId>,
    ) -> Result<SealedIncomingCursor<'_>, SealedError> {
        self.open(destination, relation, as_of, lower_source, &mut || {
            cx.checkpoint().map_err(SealedError::Interrupted)
        })
    }

    fn open(
        &self,
        destination: VId,
        relation: RelationId,
        as_of: CommitSeq,
        lower_source: Option<VId>,
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<SealedIncomingCursor<'_>, SealedError> {
        checkpoint()?;
        self.source.anchor.authorize(as_of)?;
        self.source.anchor.check_relation(relation)?;
        let chunks = self.find(destination, relation).map_or(&[][..], |row| {
            &self.index.chunks[row.first_chunk..row.end_chunk]
        });
        let lower = match lower_source {
            None => 0,
            Some(source) => {
                let row = self
                    .source
                    .image
                    .rows
                    .partition_point(|row| row.key.src < source);
                self.source
                    .image
                    .offsets
                    .select(row)
                    .ok_or(SealedError::NonCanonical)?
            }
        };
        let chunk =
            chunks.partition_point(|chunk| chunk.max_value().is_some_and(|max| max < lower));
        let at = chunks.get(chunk).map_or(0, |chunk| chunk.rank_lt(lower));
        Ok(SealedIncomingCursor {
            image: &self.source.image,
            chunks,
            chunk,
            positions: chunks
                .get(chunk)
                .map(|chunk| chunk.sequential_cursor_from(at)),
            source_row: None,
            as_of,
            finished: false,
            suspended: false,
        })
    }
}

#[cfg(test)]
fn sort<T: Ord>(
    values: &mut [T],
    checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
) -> Result<(), SealedError> {
    construction::sort_sync(values, checkpoint)
}

/// Allocation-free incoming cursor. Errors are terminal and cannot be confused
/// with clean EOF; invisible versions still participate in cancellation checks.
pub struct SealedIncomingCursor<'a> {
    image: &'a Image,
    chunks: &'a [EliasFano],
    chunk: usize,
    positions: Option<EliasFanoSequentialCursor<'a>>,
    source_row: Option<SourceRowScan<'a>>,
    as_of: CommitSeq,
    finished: bool,
    suspended: bool,
}

// Incoming locators are strictly increasing source-image positions. Keep the
// current source row and its physical decoder for parallel edges/versions, and
// use the next descriptor's count for an adjacent-row transition. A sparse
// jump still seeks the EF directory directly; it never scans unrelated rows.
struct SourceRowScan<'a> {
    index: usize,
    start: u64,
    end: u64,
    scan: RowScan<'a>,
}

impl<'a> SealedIncomingCursor<'a> {
    pub fn next(&mut self, cx: &QueryCx) -> Result<Option<SealedEdge<'a>>, SealedError> {
        self.next_inner(&mut || cx.checkpoint().map_err(SealedError::Interrupted))
    }

    /// Check live caller policy at the same boundaries as source cancellation,
    /// including invisible versions and transitions between compressed chunks.
    /// A failure is terminal even when a later call supplies another callback.
    pub fn next_with_checkpoint<E: From<SealedError>>(
        &mut self,
        cx: &QueryCx,
        mut checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<SealedEdge<'a>>, E> {
        self.next_controlled(&mut || {
            cx.checkpoint().map_err(SealedError::Interrupted)?;
            checkpoint()
        })
    }

    fn next_inner(
        &mut self,
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<Option<SealedEdge<'a>>, SealedError> {
        self.next_controlled(checkpoint)
    }

    fn next_controlled<E: From<SealedError>>(
        &mut self,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<SealedEdge<'a>>, E> {
        loop {
            match self.next_budgeted_inner(&mut SealedScanBudget::new(usize::MAX), checkpoint)? {
                SealedScanStep::Item(edge) => return Ok(Some(edge)),
                SealedScanStep::End => return Ok(None),
                SealedScanStep::Yield => {}
            }
        }
    }

    /// Bound raw locator work, not merely visible edges. The exact chunk and
    /// within-chunk position survive Yield; original property slices stay
    /// borrowed from the authenticated source across every pause.
    pub fn next_budgeted(
        &mut self,
        cx: &QueryCx,
        budget: &mut SealedScanBudget,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, SealedError> {
        self.next_budgeted_inner(budget, &mut || {
            cx.checkpoint().map_err(SealedError::Interrupted)
        })
    }

    /// Resume under both shared fuel and a typed live guard. Neither a Yield
    /// nor supplying a different callback can revive an earlier failed cursor.
    pub fn next_budgeted_with_checkpoint<E: From<SealedError>>(
        &mut self,
        cx: &QueryCx,
        budget: &mut SealedScanBudget,
        mut checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, E> {
        self.next_budgeted_inner(budget, &mut || {
            cx.checkpoint().map_err(SealedError::Interrupted)?;
            checkpoint()
        })
    }

    /// Distinguish logical pull/locator work from retry checks after Yield.
    /// Work events are independent of scheduling quantum; both event kinds
    /// retain QueryCx-first live authorization, including zero-fuel retries.
    /// Refusal on either kind terminally fuses this cursor.
    pub fn next_budgeted_with_events<E: From<SealedError>>(
        &mut self,
        cx: &QueryCx,
        budget: &mut SealedScanBudget,
        mut checkpoint: impl FnMut(SealedBuildEvent) -> Result<(), E>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, E> {
        self.next_budgeted_events_inner(budget, &mut |event| {
            cx.checkpoint().map_err(SealedError::Interrupted)?;
            checkpoint(event)
        })
    }

    fn next_budgeted_inner<E: From<SealedError>>(
        &mut self,
        budget: &mut SealedScanBudget,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, E> {
        self.next_budgeted_events_inner(budget, &mut |_| checkpoint())
    }

    fn next_budgeted_events_inner<E: From<SealedError>>(
        &mut self,
        budget: &mut SealedScanBudget,
        checkpoint: &mut impl FnMut(SealedBuildEvent) -> Result<(), E>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, E> {
        if self.finished {
            return Ok(SealedScanStep::End);
        }
        let result = self.pull_events::<true, E>(budget, checkpoint);
        if result.is_err() || matches!(&result, Ok(SealedScanStep::End)) {
            self.finished = true;
        }
        result
    }

    #[cfg(test)]
    fn pull<const FILTER: bool, E: From<SealedError>>(
        &mut self,
        budget: &mut SealedScanBudget,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, E> {
        self.pull_events::<FILTER, E>(budget, &mut |_| checkpoint())
    }

    fn pull_events<const FILTER: bool, E: From<SealedError>>(
        &mut self,
        budget: &mut SealedScanBudget,
        checkpoint: &mut impl FnMut(SealedBuildEvent) -> Result<(), E>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, E> {
        checkpoint(if self.suspended {
            SealedBuildEvent::Scheduling
        } else {
            SealedBuildEvent::Work
        })?;
        while self.chunk < self.chunks.len() {
            if !budget.spend() {
                self.suspended = true;
                return Ok(SealedScanStep::Yield);
            }
            checkpoint(SealedBuildEvent::Work)?;
            let Some(position) = self.positions.as_mut().and_then(Iterator::next) else {
                self.chunk += 1;
                self.positions = self
                    .chunks
                    .get(self.chunk)
                    .map(EliasFano::sequential_cursor);
                continue;
            };
            let Some((entry, locator)) = self.incidence::<FILTER>(position)? else {
                continue;
            };
            if entry.visible_at(self.as_of) {
                let properties = if locator == 0 {
                    &[][..]
                } else {
                    self.image
                        .properties
                        .get(locator as usize - 1)
                        .ok_or(SealedError::NonCanonical)?
                        .as_slice()
                };
                self.suspended = false;
                return Ok(SealedScanStep::Item(SealedEdge { entry, properties }));
            }
        }
        Ok(SealedScanStep::End)
    }

    fn incidence<const FILTER: bool>(
        &mut self,
        position: u64,
    ) -> Result<Option<(crate::AdjacencyEntry, u32)>, SealedError> {
        if !self
            .source_row
            .as_ref()
            .is_some_and(|row| row.start <= position && position < row.end)
        {
            let adjacent = self.source_row.as_ref().and_then(|current| {
                let index = current.index.checked_add(1)?;
                let row = self.image.rows.get(index)?;
                let end = current.end.checked_add(u64::try_from(row.len()).ok()?)?;
                (current.end <= position && position < end).then_some((index, current.end))
            });
            let (index, start) = match adjacent {
                Some(next) => next,
                None => {
                    let index = self
                        .image
                        .offsets
                        .rank_le(position)
                        .checked_sub(1)
                        .ok_or(SealedError::NonCanonical)?;
                    let start = self
                        .image
                        .offsets
                        .select(index)
                        .ok_or(SealedError::NonCanonical)?;
                    (index, start)
                }
            };
            let row = self
                .image
                .rows
                .get(index)
                .ok_or(SealedError::NonCanonical)?;
            let end = start
                .checked_add(u64::try_from(row.len()).map_err(|_| SealedError::SizeOverflow)?)
                .ok_or(SealedError::SizeOverflow)?;
            let offset = position
                .checked_sub(start)
                .ok_or(SealedError::NonCanonical)?;
            self.source_row = Some(SourceRowScan {
                index,
                start,
                end,
                scan: RowScan::new(
                    self.image,
                    row,
                    usize::try_from(offset).map_err(|_| SealedError::SizeOverflow)?,
                ),
            });
        }
        let current = self.source_row.as_mut().ok_or(SealedError::NonCanonical)?;
        let offset = usize::try_from(
            position
                .checked_sub(current.start)
                .ok_or(SealedError::NonCanonical)?,
        )
        .map_err(|_| SealedError::SizeOverflow)?;
        if current.scan.position != offset {
            let row = self
                .image
                .rows
                .get(current.index)
                .ok_or(SealedError::NonCanonical)?;
            current.scan = RowScan::new(self.image, row, offset);
        }
        if !FILTER {
            return current
                .scan
                .next()
                .map(Some)
                .ok_or(SealedError::NonCanonical);
        }
        match current
            .scan
            .next_or_hidden_span(self.as_of)
            .ok_or(SealedError::NonCanonical)?
        {
            RowScanStep::Incidence(entry, locator) => Ok(Some((entry, locator))),
            RowScanStep::Hidden(_) => {
                // Only this locator has spent fuel and passed its live guard.
                // Advance one raw incidence, never the whole source span: a
                // yield or refusal must keep its original incoming position.
                // Long hidden spans invalidate the neighbor decoder; short
                // gaps advance only that decoder. Invisible CSR versions never
                // decode edge identities or read property locators.
                current.scan.skip_hidden(1);
                Ok(None)
            }
        }
    }

    // The pre-filter streaming reader remains a same-process physical control.
    // The positional reader in incoming_tests is the independent result oracle.
    #[cfg(test)]
    fn next_decoding_history<E: From<SealedError>>(
        &mut self,
        budget: &mut SealedScanBudget,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, E> {
        if self.finished {
            return Ok(SealedScanStep::End);
        }
        let result = self.pull::<false, E>(budget, checkpoint);
        if result.is_err() || matches!(&result, Ok(SealedScanStep::End)) {
            self.finished = true;
        }
        result
    }
}

#[cfg(test)]
#[path = "incoming_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod checkpoint_tests;

// One construction algorithm, two scheduling policies. Rust's future retains
// the exact loop/scatter/sift/group/chunk positions; no handwritten restarts.
// Prism's source certificate includes this driver and incoming_transpose.rs.
mod construction {
    use super::*;
    use std::future::Future;
    use std::num::NonZeroUsize;
    use std::task::{Context, Poll, Waker};

    pub(super) trait Checkpoint {
        type Error: From<SealedError>;
        async fn check(&mut self) -> Result<(), Self::Error>;
    }

    struct Immediate<G>(G);
    impl<E, G> Checkpoint for Immediate<G>
    where
        E: From<SealedError>,
        G: FnMut() -> Result<(), E>,
    {
        type Error = E;
        async fn check(&mut self) -> Result<(), E> {
            (self.0)()
        }
    }

    pub(super) struct Cooperative<'a, Y, G> {
        pub(super) cx: &'a QueryCx,
        pub(super) fuel: SealedScanBudget,
        pub(super) quantum: NonZeroUsize,
        pub(super) yield_now: Y,
        pub(super) guard: G,
    }
    impl<E, Y, F, G> Checkpoint for Cooperative<'_, Y, G>
    where
        E: From<SealedError>,
        Y: FnMut() -> F,
        F: Future<Output = ()>,
        G: FnMut(SealedBuildEvent) -> Result<(), E>,
    {
        type Error = E;
        async fn check(&mut self) -> Result<(), E> {
            self.cx.checkpoint().map_err(SealedError::Interrupted)?;
            (self.guard)(SealedBuildEvent::Work)?;
            if !self.fuel.spend() {
                (self.yield_now)().await;
                // Reauthorize BEFORE refuelling or touching any source/output.
                self.cx.checkpoint().map_err(SealedError::Interrupted)?;
                (self.guard)(SealedBuildEvent::Scheduling)?;
                self.fuel = SealedScanBudget::new(self.quantum.get());
                self.fuel.spend();
            }
            Ok(())
        }
    }

    // Only called on the closed Immediate-control algorithm. There is no
    // executor, blocking wait, retry loop or external future on this path.
    // An accidental new suspension fails closed and drops all temporary state.
    fn immediate<T, E: From<SealedError>>(
        future: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(result) => result,
            Poll::Pending => Err(SealedError::NonCanonical.into()),
        }
    }

    pub(super) fn build_sync<E: From<SealedError>>(
        source: &SealedPartition,
        limits: IncomingIndexLimits,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<SealedIncomingIndex, E> {
        immediate(build(source, limits, &mut Immediate(checkpoint)))
    }

    #[cfg(test)]
    pub(super) fn sort_sync<T: Ord>(
        values: &mut [T],
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<(), SealedError> {
        immediate(sort(values, &mut Immediate(checkpoint)))
    }

    #[cfg(test)]
    pub(super) fn heap_sort_sync<T: Ord>(
        values: &mut [T],
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<(), SealedError> {
        let mut control = Immediate(checkpoint);
        immediate(async {
            if !ordered(values, &mut control).await? {
                heap_sort(values, &mut control).await?;
            }
            Ok(())
        })
    }

    #[cfg(test)]
    pub(super) fn sort_at_depth_sync<T: Ord>(
        values: &mut [T],
        depth: u32,
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<(), SealedError> {
        immediate(introsort(values, depth, &mut Immediate(checkpoint)))
    }

    #[cfg(test)]
    pub(super) fn build_comparison_sync<E: From<SealedError>>(
        source: &SealedPartition,
        limits: IncomingIndexLimits,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<SealedIncomingIndex, E> {
        immediate(build_selected::<false, _>(
            source,
            limits,
            &mut Immediate(checkpoint),
        ))
    }

    struct Counted {
        buckets: Vec<Bucket>,
        positions: Vec<u64>,
        relation: RelationId,
    }

    enum References {
        Sorted(Vec<IncidenceRef>),
        Counted(Counted),
    }

    struct Group {
        destination: VId,
        relation: RelationId,
        end: usize,
    }

    impl References {
        fn len(&self) -> usize {
            match self {
                Self::Sorted(refs) => refs.len(),
                Self::Counted(counted) => counted.positions.len(),
            }
        }

        fn position(&self, index: usize) -> Result<u64, SealedError> {
            match self {
                Self::Sorted(refs) => refs.get(index).map(|entry| entry.position),
                Self::Counted(counted) => counted.positions.get(index).copied(),
            }
            .ok_or(SealedError::NonCanonical)
        }

        async fn group<C: Checkpoint>(
            &self,
            image: &Image,
            group: usize,
            start: usize,
            control: &mut C,
        ) -> Result<Group, C::Error> {
            match self {
                Self::Sorted(refs) => {
                    let end = group_end(refs, start, control).await?;
                    Ok(Group {
                        destination: refs[start].destination,
                        relation: refs[start].relation,
                        end,
                    })
                }
                Self::Counted(counted) => {
                    let bucket = counted
                        .buckets
                        .get(group)
                        .ok_or(SealedError::NonCanonical)?;
                    let range = bucket.range();
                    if !bucket.is_complete() || range.start != start || range.end <= start {
                        return Err(SealedError::NonCanonical.into());
                    }
                    Ok(Group {
                        destination: image
                            .dictionary
                            .get(group)
                            .ok_or(SealedError::NonCanonical)?,
                        relation: counted.relation,
                        end: range.end,
                    })
                }
            }
        }
    }

    /// Borrow just the destination key. Complete image admission already
    /// checked EIds, spans, locators and dictionary membership. CSR values are
    /// dictionary ranks; inline values retain full-width membership lookup.
    struct DestinationRanks<'a> {
        image: &'a Image,
        row: &'a Row,
        neighbors: Option<EliasFanoSequentialCursor<'a>>,
        at: usize,
    }

    impl<'a> DestinationRanks<'a> {
        fn new(image: &'a Image, row: &'a Row) -> Result<Self, SealedError> {
            let neighbors = match row.storage {
                Storage::Inline(_) => None,
                Storage::Run(index) => Some(
                    image
                        .runs
                        .get(index)
                        .ok_or(SealedError::NonCanonical)?
                        .neighbors
                        .sequential_cursor(),
                ),
            };
            Ok(Self {
                image,
                row,
                neighbors,
                at: 0,
            })
        }

        fn next(&mut self) -> Result<usize, SealedError> {
            if self.at >= self.row.len() {
                return Err(SealedError::NonCanonical);
            }
            let rank = match &mut self.neighbors {
                Some(neighbors) => {
                    usize::try_from(neighbors.next().ok_or(SealedError::NonCanonical)?)
                        .map_err(|_| SealedError::NonCanonical)?
                }
                None => {
                    let destination = self
                        .row
                        .incidence(self.image, self.at)
                        .ok_or(SealedError::NonCanonical)?
                        .0
                        .dst;
                    let rank = self.image.destination_rank(destination);
                    if self.image.dictionary.get(rank) != Some(destination) {
                        return Err(SealedError::NonCanonical);
                    }
                    rank
                }
            };
            self.at += 1;
            Ok(rank)
        }
    }

    async fn counted<C: Checkpoint>(
        source: &SealedPartition,
        relation: RelationId,
        scratch: usize,
        control: &mut C,
    ) -> Result<References, C::Error> {
        let count = source.image.incidences;
        let destinations = source.image.dictionary.len();
        let backing = add(
            mul(count, size_of::<u64>())?,
            mul(destinations, size_of::<Bucket>())?,
        )?;
        // Dictionary admission proves destinations <= count. Preserve the
        // original 32*count allowance even though this path can retain less.
        if destinations > count || backing > scratch {
            return Err(SealedError::NonCanonical.into());
        }
        let mut positions = reserved(count)?;
        let mut buckets = reserved(destinations)?;
        for _ in 0..destinations {
            control.check().await?;
            buckets.push(Bucket::default());
        }
        for row in &source.image.rows {
            control.check().await?;
            let mut ranks = DestinationRanks::new(&source.image, row)?;
            for _ in 0..row.len() {
                control.check().await?;
                let rank = ranks.next()?;
                buckets
                    .get_mut(rank)
                    .ok_or(SealedError::NonCanonical)?
                    .count_one()
                    .map_err(SealedError::from)?;
                // Initialize inside the already guarded count pass. A second
                // source pass will fill disjoint slots in this same buffer.
                positions.push(0);
            }
        }
        if positions.len() != count {
            return Err(SealedError::NonCanonical.into());
        }
        let mut end = 0;
        for bucket in &mut buckets {
            control.check().await?;
            end = bucket.prefix(end).map_err(SealedError::from)?;
        }
        if end != count {
            return Err(SealedError::NonCanonical.into());
        }
        let mut position = 0u64;
        for row in &source.image.rows {
            control.check().await?;
            let mut ranks = DestinationRanks::new(&source.image, row)?;
            for _ in 0..row.len() {
                control.check().await?;
                let rank = ranks.next()?;
                buckets
                    .get_mut(rank)
                    .ok_or(SealedError::NonCanonical)?
                    .scatter(&mut positions, position)
                    .map_err(SealedError::from)?;
                position = position.checked_add(1).ok_or(SealedError::SizeOverflow)?;
            }
        }
        Ok(References::Counted(Counted {
            buckets,
            positions,
            relation,
        }))
    }

    async fn comparison<C: Checkpoint>(
        source: &SealedPartition,
        count: usize,
        already_ordered: bool,
        control: &mut C,
    ) -> Result<References, C::Error> {
        let mut refs = reserved(count)?;
        let mut position = 0u64;
        for row in &source.image.rows {
            control.check().await?;
            let mut scan = RowScan::new(&source.image, row, 0);
            for _ in 0..row.len() {
                control.check().await?;
                let (entry, _) = scan.next().ok_or(SealedError::NonCanonical)?;
                refs.push(IncidenceRef {
                    destination: entry.dst,
                    relation: entry.relation,
                    position,
                });
                position = position.checked_add(1).ok_or(SealedError::SizeOverflow)?;
            }
        }
        if refs.len() != count {
            return Err(SealedError::NonCanonical.into());
        }
        if !already_ordered {
            sort(&mut refs, control).await?;
        }
        Ok(References::Sorted(refs))
    }

    /// Scoped admission proves a single relation and ordered destinations
    /// within each outgoing row. Only adjacent row boundaries remain to prove
    /// global incoming order. This replaces, rather than duplicates, the old
    /// incidence-by-incidence sortedness walk for chains and ordered hubs.
    async fn scoped_rows_ordered<C: Checkpoint>(
        source: &SealedPartition,
        control: &mut C,
    ) -> Result<bool, C::Error> {
        if source.image.dictionary.len() <= 1 {
            return Ok(true);
        }
        for pair in source.image.rows.windows(2) {
            control.check().await?;
            let last = pair[0]
                .len()
                .checked_sub(1)
                .ok_or(SealedError::NonCanonical)?;
            let previous = pair[0]
                .incidence(&source.image, last)
                .ok_or(SealedError::NonCanonical)?
                .0
                .dst;
            let next = pair[1]
                .incidence(&source.image, 0)
                .ok_or(SealedError::NonCanonical)?
                .0
                .dst;
            if previous > next {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) async fn build<C: Checkpoint>(
        source: &SealedPartition,
        limits: IncomingIndexLimits,
        control: &mut C,
    ) -> Result<SealedIncomingIndex, C::Error> {
        if let Some(durable) = source.durable_incoming() {
            control.check().await?;
            check_limit(
                "incoming incidences",
                durable.stats.incidences,
                limits.max_incidences,
            )?;
            check_limit("incoming rows", durable.stats.rows, limits.max_rows)?;
            check_limit(
                "incoming workspace bytes",
                durable.stats.charged_resident_bytes,
                limits.max_workspace_bytes,
            )?;
            return Ok(SealedIncomingIndex {
                source: source.clone(),
                index: Arc::clone(durable),
            });
        }
        build_selected::<true, _>(source, limits, control).await
    }

    async fn build_selected<const COUNTING: bool, C: Checkpoint>(
        source: &SealedPartition,
        limits: IncomingIndexLimits,
        control: &mut C,
    ) -> Result<SealedIncomingIndex, C::Error> {
        control.check().await?;
        let count = source.image.incidences;
        check_limit("incoming incidences", count, limits.max_incidences)?;
        let scratch = mul(count, size_of::<IncidenceRef>())?;
        check_limit(
            "incoming workspace bytes",
            scratch,
            limits.max_workspace_bytes,
        )?;
        // A scoped anchor proves one immutable relation and dictionary order.
        // Stable counting avoids comparison sorting for those images. CSR
        // incidences use stored ranks directly; inline incidences still spend
        // O(log dictionary) full-width membership lookup. Unscoped images keep
        // the complete comparator path, including distinct relation groups.
        let refs = if COUNTING && let Some(relation) = source.anchor.relation() {
            if scoped_rows_ordered(source, control).await? {
                comparison(source, count, true, control).await?
            } else {
                counted(source, relation, scratch, control).await?
            }
        } else {
            comparison(source, count, false, control).await?
        };

        // Preflight every requested output allocation before retaining any
        // directory or compressed chunk. Scratch remains live through encoding.
        let mut row_count = 0usize;
        let mut chunk_count = 0usize;
        let mut payload_bytes = 0usize;
        let mut start = 0;
        while start < refs.len() {
            control.check().await?;
            let group = refs.group(&source.image, row_count, start, control).await?;
            let end = group.end;
            row_count = add(row_count, 1)?;
            check_limit("incoming rows", row_count, limits.max_rows)?;
            for first in (start..end).step_by(CHUNK_ENTRIES) {
                control.check().await?;
                chunk_count = add(chunk_count, 1)?;
                let length = (end - first).min(CHUNK_ENTRIES);
                payload_bytes = add(
                    payload_bytes,
                    ef_bytes(length, refs.position(first + length - 1)?)?,
                )?;
            }
            start = end;
        }
        let resident = add(
            payload_bytes,
            add(
                mul(row_count, size_of::<IncomingRow>())?,
                mul(chunk_count, size_of::<EliasFano>())?,
            )?,
        )?;
        let workspace = add(scratch, resident)?;
        check_limit(
            "incoming workspace bytes",
            workspace,
            limits.max_workspace_bytes,
        )?;
        let mut rows = reserved(row_count)?;
        let mut chunks = reserved(chunk_count)?;
        let mut values = [0u64; CHUNK_ENTRIES];
        start = 0;
        while start < refs.len() {
            control.check().await?;
            let group = refs
                .group(&source.image, rows.len(), start, control)
                .await?;
            let end = group.end;
            let first_chunk = chunks.len();
            for first in (start..end).step_by(CHUNK_ENTRIES) {
                control.check().await?;
                let length = (end - first).min(CHUNK_ENTRIES);
                for (at, value) in values[..length].iter_mut().enumerate() {
                    *value = refs.position(first + at)?;
                }
                chunks.push(
                    EliasFano::try_new(&values[..length], EntryLimit::new(CHUNK_ENTRIES))
                        .map_err(SealedError::EliasFano)?,
                );
                control.check().await?;
            }
            rows.push(IncomingRow {
                destination: group.destination,
                relation: group.relation,
                first_chunk,
                end_chunk: chunks.len(),
                incidences: end - start,
            });
            start = end;
        }
        control.check().await?;
        let mut retained_bytes = add(
            size_of::<Index>(),
            add(
                mul(rows.capacity(), size_of::<IncomingRow>())?,
                mul(chunks.capacity(), size_of::<EliasFano>())?,
            )?,
        )?;
        for chunk in &chunks {
            control.check().await?;
            retained_bytes = add(
                retained_bytes,
                chunk.owned_heap_bytes().ok_or(SealedError::SizeOverflow)?,
            )?;
        }
        control.check().await?;
        Ok(SealedIncomingIndex {
            source: source.clone(),
            index: Arc::new(Index {
                rows,
                chunks,
                stats: IncomingIndexStats {
                    rows: row_count,
                    incidences: count,
                    chunks: chunk_count,
                    charged_resident_bytes: resident,
                    charged_workspace_bytes: workspace,
                },
                retained_bytes,
            }),
        })
    }

    async fn group_end<C: Checkpoint>(
        refs: &[IncidenceRef],
        start: usize,
        control: &mut C,
    ) -> Result<usize, C::Error> {
        let key = (refs[start].destination, refs[start].relation);
        let mut end = start + 1;
        while end < refs.len() && (refs[end].destination, refs[end].relation) == key {
            control.check().await?;
            end += 1;
        }
        Ok(end)
    }

    // Retained source order is often already incoming order, notably for a
    // single hub. Keep that case linear and preserve the complete guarded walk.
    async fn ordered<T: Ord, C: Checkpoint>(
        values: &mut [T],
        control: &mut C,
    ) -> Result<bool, C::Error> {
        let mut ordered = true;
        for pair in values.windows(2) {
            control.check().await?;
            if pair[0] > pair[1] {
                ordered = false;
            }
        }
        Ok(ordered)
    }

    // A sequential partition has much better locality than repeated heap
    // sifts over graph-sized incidence arrays. Share one resumable sorter
    // between immediate and cooperative construction; no comparison sort is
    // hidden between yields and no second incidence array is allocated.
    pub(super) async fn sort<T: Ord, C: Checkpoint>(
        values: &mut [T],
        control: &mut C,
    ) -> Result<(), C::Error> {
        if ordered(values, control).await? {
            return Ok(());
        }
        let depth = 2 * (usize::BITS - values.len().leading_zeros());
        introsort(values, depth, control).await
    }

    #[derive(Clone, Copy)]
    struct SortSpan {
        start: usize,
        end: usize,
        depth: u32,
    }

    async fn introsort<T: Ord, C: Checkpoint>(
        values: &mut [T],
        depth: u32,
        control: &mut C,
    ) -> Result<(), C::Error> {
        let mut current = SortSpan {
            start: 0,
            end: values.len(),
            depth,
        };
        // Visit the smaller partition immediately and retain the larger one.
        // Every additional pending span therefore halves the current length;
        // usize::BITS slots suffice even for the largest addressable slice.
        // This fixed scratch also lives inside the cooperative future. It is
        // independent of the graph size and adds no heap/resident-index charge.
        let mut pending = [current; usize::BITS as usize];
        let mut count = 0;
        loop {
            let length = current.end - current.start;
            if length <= 16 {
                let row = &mut values[current.start..current.end];
                for index in 1..row.len() {
                    let mut at = index;
                    while at != 0 {
                        control.check().await?;
                        if row[at - 1] <= row[at] {
                            break;
                        }
                        row.swap(at - 1, at);
                        at -= 1;
                    }
                }
            } else if current.depth == 0 {
                // O(n log n) worst-case work, including duplicate-heavy or
                // adversarial pivot orders, without an allocation or recursion.
                heap_sort(&mut values[current.start..current.end], control).await?;
            } else {
                let row = &mut values[current.start..current.end];
                let (middle, last) = (length / 2, length - 1);
                if length >= 128 {
                    // A median of three medians resists organ-pipe and
                    // reverse-order runs without randomization or a new
                    // allocation. Every sampled comparison remains guarded.
                    let step = length / 8;
                    order_three(row, 0, step, 2 * step, control).await?;
                    order_three(row, middle - step, middle, middle + step, control).await?;
                    order_three(row, last - 2 * step, last - step, last, control).await?;
                    order_three(row, step, middle, last - step, control).await?;
                } else {
                    order_three(row, 0, middle, last, control).await?;
                }
                row.swap(middle, last);
                let mut below = 0;
                for index in 0..last {
                    control.check().await?;
                    if row[index] < row[last] {
                        row.swap(index, below);
                        below += 1;
                    }
                }
                row.swap(below, last);
                let pivot = current.start + below;
                let left = SortSpan {
                    start: current.start,
                    end: pivot,
                    depth: current.depth - 1,
                };
                let right = SortSpan {
                    start: pivot + 1,
                    end: current.end,
                    depth: current.depth - 1,
                };
                let (smaller, larger) = if left.end - left.start < right.end - right.start {
                    (left, right)
                } else {
                    (right, left)
                };
                if larger.end - larger.start > 1 {
                    *pending.get_mut(count).ok_or(SealedError::SizeOverflow)? = larger;
                    count += 1;
                }
                current = smaller;
                continue;
            }
            if count == 0 {
                return Ok(());
            }
            count -= 1;
            current = pending[count];
        }
    }

    async fn order_three<T: Ord, C: Checkpoint>(
        values: &mut [T],
        first: usize,
        middle: usize,
        last: usize,
        control: &mut C,
    ) -> Result<(), C::Error> {
        control.check().await?;
        if values[middle] < values[first] {
            values.swap(first, middle);
        }
        control.check().await?;
        if values[last] < values[first] {
            values.swap(first, last);
        }
        control.check().await?;
        if values[last] < values[middle] {
            values.swap(middle, last);
        }
        Ok(())
    }

    // The original constant-scratch sorter remains the introspection fallback
    // and a test-only incumbent for paired sort-stage measurements.
    async fn heap_sort<T: Ord, C: Checkpoint>(
        values: &mut [T],
        control: &mut C,
    ) -> Result<(), C::Error> {
        for root in (0..values.len() / 2).rev() {
            sift(values, root, control).await?;
        }
        for end in (1..values.len()).rev() {
            control.check().await?;
            values.swap(0, end);
            sift(&mut values[..end], 0, control).await?;
        }
        Ok(())
    }

    async fn sift<T: Ord, C: Checkpoint>(
        values: &mut [T],
        mut root: usize,
        control: &mut C,
    ) -> Result<(), C::Error> {
        while root < values.len() / 2 {
            control.check().await?;
            let left = 2 * root + 1;
            let right = left + 1;
            let child = if right < values.len() && values[right] > values[left] {
                right
            } else {
                left
            };
            if values[root] >= values[child] {
                break;
            }
            values.swap(root, child);
            root = child;
        }
        Ok(())
    }
}

pub(super) fn encode_durable_index(
    index: &Index,
    relation: RelationId,
    image: &Image,
    max_bytes: usize,
    checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
) -> Result<Vec<u8>, SealedError> {
    checkpoint()?;
    let mut out = Vec::new();
    let push = |out: &mut Vec<u8>, slice: &[u8]| -> Result<(), SealedError> {
        let next_len = out
            .len()
            .checked_add(slice.len())
            .ok_or(SealedError::SizeOverflow)?;
        check_limit("durable run bytes", next_len, max_bytes)?;
        out.try_reserve(slice.len())
            .map_err(|_| SealedError::AllocationFailed)?;
        out.extend_from_slice(slice);
        Ok(())
    };
    push(
        &mut out,
        &u32::try_from(index.rows.len())
            .map_err(|_| SealedError::SizeOverflow)?
            .to_le_bytes(),
    )?;
    push(
        &mut out,
        &u32::try_from(index.chunks.len())
            .map_err(|_| SealedError::SizeOverflow)?
            .to_le_bytes(),
    )?;
    for row in &index.rows {
        checkpoint()?;
        if row.relation != relation {
            return Err(SealedError::NonCanonical);
        }
        let rank = image.destination_rank(row.destination);
        if image.dictionary.get(rank) != Some(row.destination) {
            return Err(SealedError::NonCanonical);
        }
        push(
            &mut out,
            &u32::try_from(rank)
                .map_err(|_| SealedError::SizeOverflow)?
                .to_le_bytes(),
        )?;
        push(
            &mut out,
            &u32::try_from(row.incidences)
                .map_err(|_| SealedError::SizeOverflow)?
                .to_le_bytes(),
        )?;
    }
    for chunk in &index.chunks {
        checkpoint()?;
        let ef_limits = EfPayloadLimits {
            max_entries: chunk.len(),
            max_bytes,
        };
        let maximum = chunk.max_value().unwrap_or(0);
        let length = ef_payload::encoded_len(chunk.len(), maximum, ef_limits)
            .map_err(SealedError::Payload)?;
        push(&mut out, &maximum.to_le_bytes())?;
        push(
            &mut out,
            &u32::try_from(length)
                .map_err(|_| SealedError::SizeOverflow)?
                .to_le_bytes(),
        )?;
        ef_payload::encode_to(chunk, ef_limits, |part| push(&mut out, part))?;
    }
    checkpoint()?;
    Ok(out)
}

fn verify_locator_endpoint(
    image: &Image,
    position: u64,
    destination: VId,
    relation: RelationId,
) -> Result<(), SealedError> {
    let row_index = image
        .offsets
        .rank_le(position)
        .checked_sub(1)
        .ok_or(SealedError::NonCanonical)?;
    let start = image
        .offsets
        .select(row_index)
        .ok_or(SealedError::NonCanonical)?;
    let offset = usize::try_from(
        position
            .checked_sub(start)
            .ok_or(SealedError::NonCanonical)?,
    )
    .map_err(|_| SealedError::SizeOverflow)?;
    let (entry, _) = image
        .rows
        .get(row_index)
        .ok_or(SealedError::NonCanonical)?
        .incidence(image, offset)
        .ok_or(SealedError::NonCanonical)?;
    if entry.dst != destination || entry.relation != relation {
        return Err(SealedError::NonCanonical);
    }
    Ok(())
}

pub(super) fn decode_durable_index(
    bytes: &[u8],
    relation: RelationId,
    image: &Image,
    limits: SealedLimits,
    checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
) -> Result<Arc<Index>, SealedError> {
    checkpoint()?;
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8], SealedError> {
        let end = at.checked_add(n).ok_or(SealedError::SizeOverflow)?;
        let slice = bytes.get(at..end).ok_or(SealedError::Truncated)?;
        at = end;
        Ok(slice)
    };
    let row_count =
        u32::from_le_bytes(take(4)?.try_into().map_err(|_| SealedError::Truncated)?) as usize;
    let chunk_count =
        u32::from_le_bytes(take(4)?.try_into().map_err(|_| SealedError::Truncated)?) as usize;
    check_limit("descriptors", row_count, limits.max_rows)?;
    check_limit("incidences", image.incidences, limits.max_incidences)?;
    if row_count > image.dictionary.len() || row_count > image.incidences {
        return Err(SealedError::NonCanonical);
    }
    if image.incidences == 0 {
        if row_count != 0 || chunk_count != 0 {
            return Err(SealedError::NonCanonical);
        }
        if at != bytes.len() {
            return Err(SealedError::TrailingBytes);
        }
        let retained_bytes = size_of::<Index>();
        return Ok(Arc::new(Index {
            rows: Vec::new(),
            chunks: Vec::new(),
            stats: IncomingIndexStats {
                rows: 0,
                incidences: 0,
                chunks: 0,
                charged_resident_bytes: 0,
                charged_workspace_bytes: 0,
            },
            retained_bytes,
        }));
    }

    let mut rows = reserved(row_count)?;
    let mut prev_rank: Option<usize> = None;
    let mut total_incidences = 0usize;
    let mut expected_chunks = 0usize;
    for _ in 0..row_count {
        checkpoint()?;
        let rank =
            u32::from_le_bytes(take(4)?.try_into().map_err(|_| SealedError::Truncated)?) as usize;
        let incidences =
            u32::from_le_bytes(take(4)?.try_into().map_err(|_| SealedError::Truncated)?) as usize;
        if incidences == 0 || prev_rank.is_some_and(|prev| prev >= rank) {
            return Err(SealedError::NonCanonical);
        }
        let destination = image
            .dictionary
            .get(rank)
            .ok_or(SealedError::NonCanonical)?;
        prev_rank = Some(rank);
        total_incidences = add(total_incidences, incidences)?;
        let row_chunks = incidences.div_ceil(CHUNK_ENTRIES);
        let first_chunk = expected_chunks;
        expected_chunks = add(expected_chunks, row_chunks)?;
        rows.push(IncomingRow {
            destination,
            relation,
            first_chunk,
            end_chunk: expected_chunks,
            incidences,
        });
    }
    if total_incidences != image.incidences || expected_chunks != chunk_count {
        return Err(SealedError::NonCanonical);
    }

    let mut chunks = reserved(chunk_count)?;
    let mut payload_bytes = 0usize;
    let max_locator = (image.incidences - 1) as u64;
    for row in &rows {
        let mut remaining = row.incidences;
        let mut prev_locator: Option<u64> = None;
        for _ in row.first_chunk..row.end_chunk {
            checkpoint()?;
            let expected_len = remaining.min(CHUNK_ENTRIES);
            remaining -= expected_len;
            let maximum =
                u64::from_le_bytes(take(8)?.try_into().map_err(|_| SealedError::Truncated)?);
            if maximum > max_locator {
                return Err(SealedError::NonCanonical);
            }
            let payload_len =
                u32::from_le_bytes(take(4)?.try_into().map_err(|_| SealedError::Truncated)?)
                    as usize;
            let raw_payload = take(payload_len)?;
            let chunk = ef_payload::decode(
                raw_payload,
                expected_len,
                maximum,
                EfPayloadLimits {
                    max_entries: expected_len,
                    max_bytes: limits.max_image_bytes,
                },
            )
            .map_err(SealedError::Payload)?;
            let mut first_in_chunk = None;
            let mut last_in_chunk = None;
            for pos in chunk.sequential_cursor() {
                if prev_locator.is_some_and(|prev| prev >= pos) || pos > max_locator {
                    return Err(SealedError::NonCanonical);
                }
                if first_in_chunk.is_none() {
                    first_in_chunk = Some(pos);
                }
                last_in_chunk = Some(pos);
                prev_locator = Some(pos);
            }
            let first_pos = first_in_chunk.ok_or(SealedError::NonCanonical)?;
            let last_pos = last_in_chunk.ok_or(SealedError::NonCanonical)?;
            if last_pos != maximum {
                return Err(SealedError::NonCanonical);
            }
            verify_locator_endpoint(image, first_pos, row.destination, relation)?;
            if last_pos != first_pos {
                verify_locator_endpoint(image, last_pos, row.destination, relation)?;
            }
            payload_bytes = add(payload_bytes, ef_bytes(expected_len, maximum)?)?;
            chunks.push(chunk);
        }
    }
    if at != bytes.len() {
        return Err(SealedError::TrailingBytes);
    }

    let resident = add(
        payload_bytes,
        add(
            mul(row_count, size_of::<IncomingRow>())?,
            mul(chunk_count, size_of::<EliasFano>())?,
        )?,
    )?;
    let mut retained_bytes = add(
        size_of::<Index>(),
        add(
            mul(rows.capacity(), size_of::<IncomingRow>())?,
            mul(chunks.capacity(), size_of::<EliasFano>())?,
        )?,
    )?;
    for chunk in &chunks {
        checkpoint()?;
        retained_bytes = add(
            retained_bytes,
            chunk.owned_heap_bytes().ok_or(SealedError::SizeOverflow)?,
        )?;
    }
    checkpoint()?;
    Ok(Arc::new(Index {
        rows,
        chunks,
        stats: IncomingIndexStats {
            rows: row_count,
            incidences: image.incidences,
            chunks: chunk_count,
            charged_resident_bytes: resident,
            charged_workspace_bytes: resident,
        },
        retained_bytes,
    }))
}

#[cfg(test)]
#[path = "incoming_cooperative_tests.rs"]
mod cooperative_tests;
