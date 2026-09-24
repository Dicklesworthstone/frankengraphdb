//! Incoming access is a compressed permutation of an admitted outgoing image.
//!
//! For a fixed (destination, relation), original incidence positions increase
//! in (source, EId, content-version) order. Encode those positions, NOT another
//! copy of neighbors, edge identities, visibility intervals or property rows.
//! A cursor dereferences the same immutable source image on both faces. No
//! caller-supplied permutation or serialized index can acquire source authority.

use super::image::reserved;
use super::{
    Image, SealedAnchor, SealedEdge, SealedError, SealedPartition, SealedScanBudget,
    SealedScanStep, check_limit,
};
use fgdb_codec::elias_fano::{EliasFano, EntryLimit};
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::mem::size_of;
use std::sync::Arc;

// Bound each existing, non-preemptible EF construction call. Sorting, directory
// walks and individual source incidences checkpoint separately.
const CHUNK_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncomingIndexLimits {
    pub max_rows: usize,
    pub max_incidences: usize,
    /// Conservative peak of requested vector backing stores during construction,
    /// including the retained index and its sort scratch. Excludes the original
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

struct Index {
    rows: Vec<IncomingRow>,
    chunks: Vec<EliasFano>,
    stats: IncomingIndexStats,
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
    /// security-filtered graph statistic.
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
    /// The quantum bounds checkpointed steps (collection, comparisons, sift
    /// levels, group walks and chunks), not CPU time or allocator latency.
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
        self.incoming_index_cooperative_with_checkpoint(
            cx, limits, quantum, yield_now, || Ok::<(), SealedError>(()),
        ).await
    }

    /// Keep QueryCx-first typed live checks across every build stage and both
    /// sides of each suspension. The future exclusively owns temporary refs,
    /// heap-sort progress and unexposed output; cancel/refusal/drop releases it
    /// without changing the original image or publishing a partial index.
    pub async fn incoming_index_cooperative_with_checkpoint<E, Y, F>(
        &self,
        cx: &QueryCx,
        limits: IncomingIndexLimits,
        quantum: std::num::NonZeroUsize,
        yield_now: Y,
        checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<SealedIncomingIndex, E>
    where
        E: From<SealedError>,
        Y: FnMut() -> F,
        F: std::future::Future<Output = ()>,
    {
        cx.with_restriction_async(async {
            let mut control = construction::Cooperative {
                cx, fuel: SealedScanBudget::new(quantum.get()), quantum,
                yield_now, guard: checkpoint,
            };
            construction::build(self, limits, &mut control).await
        }).await
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
    pub fn shares_index_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.index, &other.index)
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
            at,
            as_of,
            finished: false,
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
    at: usize,
    as_of: CommitSeq,
    finished: bool,
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

    fn next_budgeted_inner<E: From<SealedError>>(
        &mut self,
        budget: &mut SealedScanBudget,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, E> {
        if self.finished {
            return Ok(SealedScanStep::End);
        }
        let result = self.pull(budget, checkpoint);
        if result.is_err() || matches!(&result, Ok(SealedScanStep::End)) {
            self.finished = true;
        }
        result
    }

    fn pull<E: From<SealedError>>(
        &mut self,
        budget: &mut SealedScanBudget,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, E> {
        checkpoint()?;
        while let Some(chunk) = self.chunks.get(self.chunk) {
            if !budget.spend() {
                return Ok(SealedScanStep::Yield);
            }
            checkpoint()?;
            let Some(position) = chunk.select(self.at) else {
                self.chunk += 1;
                self.at = 0;
                continue;
            };
            self.at += 1;
            let row_index = self
                .image
                .offsets
                .rank_le(position)
                .checked_sub(1)
                .ok_or(SealedError::NonCanonical)?;
            let row = self
                .image
                .rows
                .get(row_index)
                .ok_or(SealedError::NonCanonical)?;
            let offset = position
                .checked_sub(
                    self.image
                        .offsets
                        .select(row_index)
                        .ok_or(SealedError::NonCanonical)?,
                )
                .ok_or(SealedError::NonCanonical)?;
            let (entry, locator) = row
                .incidence(
                    self.image,
                    usize::try_from(offset).map_err(|_| SealedError::SizeOverflow)?,
                )
                .ok_or(SealedError::NonCanonical)?;
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
                return Ok(SealedScanStep::Item(SealedEdge { entry, properties }));
            }
        }
        Ok(SealedScanStep::End)
    }
}

#[cfg(test)]
#[path = "incoming_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod checkpoint_tests;

// One construction algorithm, two scheduling policies. Rust's future retains
// the exact loop/sift/group/chunk positions; no handwritten restart or rescans.
// Keep this executing source inside incoming.rs so the existing Prism source
// certificate includes BOTH drivers, not just an include/module declaration.
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
        async fn check(&mut self) -> Result<(), E> { (self.0)() }
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
        G: FnMut() -> Result<(), E>,
    {
        type Error = E;
        async fn check(&mut self) -> Result<(), E> {
            self.cx.checkpoint().map_err(SealedError::Interrupted)?;
            (self.guard)()?;
            if !self.fuel.spend() {
                (self.yield_now)().await;
                // Reauthorize BEFORE refuelling or touching any source/output.
                self.cx.checkpoint().map_err(SealedError::Interrupted)?;
                (self.guard)()?;
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
        match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
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

    pub(super) async fn build<C: Checkpoint>(
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
        let mut refs = reserved(count)?;
        let mut position = 0u64;
        for row in &source.image.rows {
            control.check().await?;
            for at in 0..row.len() {
                control.check().await?;
                let (entry, _) = row
                    .incidence(&source.image, at)
                    .ok_or(SealedError::NonCanonical)?;
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
        sort(&mut refs, control).await?;

        // Preflight every requested output allocation before retaining any
        // directory or compressed chunk. Scratch remains live through encoding.
        let mut row_count = 0usize;
        let mut chunk_count = 0usize;
        let mut payload_bytes = 0usize;
        let mut start = 0;
        while start < refs.len() {
            control.check().await?;
            let end = group_end(&refs, start, control).await?;
            row_count = add(row_count, 1)?;
            check_limit("incoming rows", row_count, limits.max_rows)?;
            for values in refs[start..end].chunks(CHUNK_ENTRIES) {
                control.check().await?;
                chunk_count = add(chunk_count, 1)?;
                payload_bytes = add(
                    payload_bytes,
                    ef_bytes(
                        values.len(),
                        values.last().ok_or(SealedError::NonCanonical)?.position,
                    )?,
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
            let end = group_end(&refs, start, control).await?;
            let first_chunk = chunks.len();
            for entries in refs[start..end].chunks(CHUNK_ENTRIES) {
                control.check().await?;
                for (at, entry) in entries.iter().enumerate() {
                    values[at] = entry.position;
                }
                chunks.push(
                    EliasFano::try_new(&values[..entries.len()], EntryLimit::new(CHUNK_ENTRIES))
                        .map_err(SealedError::EliasFano)?,
                );
                control.check().await?;
            }
            rows.push(IncomingRow {
                destination: refs[start].destination,
                relation: refs[start].relation,
                first_chunk,
                end_chunk: chunks.len(),
                incidences: end - start,
            });
            start = end;
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

    // Constant-scratch heapsort. No allocation or uninterruptible whole-population
    // sort hides between the surrounding checkpoints. Input keys are total/unique.
    async fn sort<T: Ord, C: Checkpoint>(
        values: &mut [T],
        control: &mut C,
    ) -> Result<(), C::Error> {
        let mut ordered = true;
        for pair in values.windows(2) {
            control.check().await?;
            if pair[0] > pair[1] {
                ordered = false;
            }
        }
        if ordered {
            return Ok(());
        }
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

#[cfg(test)]
#[path = "incoming_cooperative_tests.rs"]
mod cooperative_tests;
