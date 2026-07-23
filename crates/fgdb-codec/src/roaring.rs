//! Canonical scalar roaring-style bitmaps over sorted, unique `u32` values.
//!
//! Values are partitioned by their high 16 bits. Each non-empty chunk is
//! represented as exactly one closed container kind:
//!
//! - array: one `u16` per value (`2 * cardinality` logical payload bytes),
//! - bitmap: 65,536 bits (`8,192` logical payload bytes), or
//! - run: inclusive `(start, end)` pairs (`4 * run_count` logical payload
//!   bytes).
//!
//! Construction chooses the smallest exact logical payload. Equal-cost
//! representations use the stable tie-break `Array < Run < Bitmap`. Container
//! headers and chunk-directory metadata are deliberately excluded because
//! they are common representation metadata rather than container payload.
//! This module defines no durable framing, version, or codec identifier.
//!
//! The implementation is entirely scalar and safe. Point lookup is logarithmic
//! for array and run containers and constant-word lookup for bitmap containers.
//! Rank and select have representation-dependent costs; no universal `O(1)`
//! claim is made. Intersection walks only represented chunks and values (or a
//! bitmap chunk's fixed 1,024 words), never the full `u32` universe.

#![forbid(unsafe_code)]

use core::fmt;

const LOW_BITS: u32 = 16;
const BITMAP_WORDS: usize = (1_usize << LOW_BITS) / u64::BITS as usize;
const BITMAP_PAYLOAD_BYTES: usize = BITMAP_WORDS * core::mem::size_of::<u64>();
const ARRAY_VALUE_BYTES: usize = core::mem::size_of::<u16>();
const RUN_BYTES: usize = 2 * core::mem::size_of::<u16>();

/// Maximum number of logical values one operation may materialize.
///
/// The bound is checked before construction allocates. Intersection first
/// counts its result without allocation, applies the bound, and only then
/// reserves result storage.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EntryLimit(usize);

impl EntryLimit {
    /// Creates an exact entry ceiling.
    #[must_use]
    pub const fn new(max_entries: usize) -> Self {
        Self(max_entries)
    }

    /// Returns the configured ceiling.
    #[must_use]
    pub const fn max_entries(self) -> usize {
        self.0
    }
}

/// Closed set of scalar container representations.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ContainerKind {
    /// Sorted low-16-bit values.
    Array,
    /// A fixed 65,536-bit chunk bitmap.
    Bitmap,
    /// Sorted, disjoint inclusive low-16-bit runs.
    Run,
}

/// Internal allocation named by a fallible construction error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AllocationTarget {
    /// Top-level chunk directory entries.
    ChunkDirectory,
    /// Low values in an array container.
    ArrayValues,
    /// Words in a bitmap container.
    BitmapWords,
    /// Inclusive pairs in a run container.
    Runs,
    /// Temporary low values for one intersected chunk.
    IntersectionValues,
}

/// Stable names for checked representation-size calculations.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SizeCalculation {
    /// Counting non-empty high-16-bit chunks.
    ChunkCount,
    /// Multiplying array cardinality by the element width.
    ArrayPayload,
    /// Multiplying run count by the pair width.
    RunPayload,
    /// Adding per-chunk intersection cardinalities.
    IntersectionCardinality,
    /// Adding a chunk cardinality to the bitmap cardinality.
    TotalCardinality,
}

/// Typed failure from bitmap construction or intersection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoaringError {
    /// The logical input or result is larger than the caller permits.
    EntryLimitExceeded {
        /// Number of logical values.
        entries: usize,
        /// Caller-provided ceiling.
        limit: usize,
    },
    /// A sorted input contains the same value twice.
    Duplicate {
        /// Index of the second occurrence.
        index: usize,
        /// Repeated value.
        value: u32,
    },
    /// Input ceased to be increasing at `index`.
    NotSorted {
        /// Index of the first smaller value.
        index: usize,
        /// Value immediately before `index`.
        previous: u32,
        /// Rejected value.
        current: u32,
    },
    /// Representation-size arithmetic overflowed.
    SizeOverflow {
        /// Stable name of the failed calculation.
        calculation: SizeCalculation,
    },
    /// Reserving one representation component failed before publication.
    AllocationFailed {
        /// Component that could not be reserved.
        target: AllocationTarget,
        /// Requested units (entries, words, or runs according to `target`).
        requested: usize,
        /// High-16-bit chunk key for a container allocation.
        high_key: Option<u16>,
    },
}

impl fmt::Display for RoaringError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EntryLimitExceeded { entries, limit } => {
                write!(
                    formatter,
                    "roaring input has {entries} entries, limit is {limit}"
                )
            }
            Self::Duplicate { index, value } => {
                write!(
                    formatter,
                    "roaring input duplicates {value} at index {index}"
                )
            }
            Self::NotSorted {
                index,
                previous,
                current,
            } => write!(
                formatter,
                "roaring input decreases at index {index}: {previous} then {current}"
            ),
            Self::SizeOverflow { calculation } => {
                write!(formatter, "roaring {calculation:?} arithmetic overflowed")
            }
            Self::AllocationFailed {
                target,
                requested,
                high_key,
            } => {
                if let Some(high_key) = high_key {
                    write!(
                        formatter,
                        "could not reserve {requested} units for roaring {target:?} in chunk {high_key}"
                    )
                } else {
                    write!(
                        formatter,
                        "could not reserve {requested} units for roaring {target:?}"
                    )
                }
            }
        }
    }
}

impl std::error::Error for RoaringError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Run {
    start: u16,
    end: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Container {
    Array(Vec<u16>),
    Bitmap(Vec<u64>),
    Run(Vec<Run>),
}

impl Container {
    fn kind(&self) -> ContainerKind {
        match self {
            Self::Array(_) => ContainerKind::Array,
            Self::Bitmap(_) => ContainerKind::Bitmap,
            Self::Run(_) => ContainerKind::Run,
        }
    }

    fn contains(&self, low: u16) -> bool {
        match self {
            Self::Array(values) => values.binary_search(&low).is_ok(),
            Self::Bitmap(words) => {
                let index = usize::from(low);
                words[index / u64::BITS as usize] & (1_u64 << (index % u64::BITS as usize)) != 0
            }
            Self::Run(runs) => runs
                .binary_search_by(|run| {
                    if run.end < low {
                        core::cmp::Ordering::Less
                    } else if run.start > low {
                        core::cmp::Ordering::Greater
                    } else {
                        core::cmp::Ordering::Equal
                    }
                })
                .is_ok(),
        }
    }

    fn rank_le(&self, low: u16) -> usize {
        match self {
            Self::Array(values) => values.partition_point(|&value| value <= low),
            Self::Bitmap(words) => {
                let low_index = usize::from(low);
                let word_index = low_index / u64::BITS as usize;
                let preceding = words[..word_index]
                    .iter()
                    .map(|word| word.count_ones() as usize)
                    .sum::<usize>();
                let inclusive_bit = low_index % u64::BITS as usize;
                let mask = if inclusive_bit == u64::BITS as usize - 1 {
                    u64::MAX
                } else {
                    (1_u64 << (inclusive_bit + 1)) - 1
                };
                preceding + (words[word_index] & mask).count_ones() as usize
            }
            Self::Run(runs) => {
                let mut rank = 0_usize;
                for run in runs {
                    if low < run.start {
                        break;
                    }
                    let upper = low.min(run.end);
                    rank += usize::from(upper) - usize::from(run.start) + 1;
                    if low <= run.end {
                        break;
                    }
                }
                rank
            }
        }
    }

    fn select(&self, mut index: usize) -> Option<u16> {
        match self {
            Self::Array(values) => values.get(index).copied(),
            Self::Bitmap(words) => {
                for (word_index, &word) in words.iter().enumerate() {
                    let count = word.count_ones() as usize;
                    if index >= count {
                        index -= count;
                        continue;
                    }
                    let mut remaining_word = word;
                    for _ in 0..index {
                        remaining_word &= remaining_word - 1;
                    }
                    let bit = remaining_word.trailing_zeros() as usize;
                    return u16::try_from(word_index * u64::BITS as usize + bit).ok();
                }
                None
            }
            Self::Run(runs) => {
                for run in runs {
                    let run_len = usize::from(run.end) - usize::from(run.start) + 1;
                    if index < run_len {
                        return u16::try_from(usize::from(run.start) + index).ok();
                    }
                    index -= run_len;
                }
                None
            }
        }
    }

    fn iter(&self) -> ContainerIter<'_> {
        match self {
            Self::Array(values) => ContainerIter::Array(values.iter()),
            Self::Bitmap(words) => ContainerIter::Bitmap(BitmapIter::new(words)),
            Self::Run(runs) => ContainerIter::Run(RunIter::new(runs)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Chunk {
    high_key: u16,
    prefix_len: usize,
    cardinality: usize,
    container: Container,
}

/// Immutable canonical roaring-style set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoaringBitmap {
    chunks: Vec<Chunk>,
    len: usize,
}

impl RoaringBitmap {
    /// Constructs a canonical bitmap from strictly increasing values.
    ///
    /// Entry limit and ordering are validated before the first allocation.
    /// The exact selected payload sizes are checked before each fallible
    /// reserve, and no partially built bitmap is published on error.
    pub fn try_from_sorted(values: &[u32], limit: EntryLimit) -> Result<Self, RoaringError> {
        let mut reservation = SystemReservation;
        Self::try_from_sorted_with(values, limit, &mut reservation)
    }

    fn try_from_sorted_with<R: Reservation>(
        values: &[u32],
        limit: EntryLimit,
        reservation: &mut R,
    ) -> Result<Self, RoaringError> {
        validate_input(values, limit)?;
        if values.is_empty() {
            return Ok(Self {
                chunks: Vec::new(),
                len: 0,
            });
        }

        let chunk_count = count_chunks(values)?;
        let mut chunks = Vec::new();
        reserve_exact(
            &mut chunks,
            chunk_count,
            AllocationTarget::ChunkDirectory,
            None,
            reservation,
        )?;

        let mut start = 0_usize;
        let mut prefix_len = 0_usize;
        while start < values.len() {
            let high_key = high(values[start]);
            let mut end = start + 1;
            while end < values.len() && high(values[end]) == high_key {
                end += 1;
            }
            let cardinality = end - start;
            let container =
                build_container_from_values(&values[start..end], high_key, reservation)?;
            chunks.push(Chunk {
                high_key,
                prefix_len,
                cardinality,
                container,
            });
            prefix_len = prefix_len
                .checked_add(cardinality)
                .ok_or(RoaringError::SizeOverflow {
                    calculation: SizeCalculation::TotalCardinality,
                })?;
            start = end;
        }

        debug_assert_eq!(prefix_len, values.len());
        Ok(Self {
            chunks,
            len: values.len(),
        })
    }

    /// Number of represented values.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// True when the set is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of represented high-16-bit chunks.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Canonical container kind selected for `high_key`.
    #[must_use]
    pub fn container_kind(&self, high_key: u16) -> Option<ContainerKind> {
        self.chunks
            .binary_search_by_key(&high_key, |chunk| chunk.high_key)
            .ok()
            .map(|index| self.chunks[index].container.kind())
    }

    /// Returns whether `value` is in the set.
    #[must_use]
    pub fn contains(&self, value: u32) -> bool {
        let high_key = high(value);
        self.chunks
            .binary_search_by_key(&high_key, |chunk| chunk.high_key)
            .is_ok_and(|index| self.chunks[index].container.contains(low(value)))
    }

    /// Counts represented values less than or equal to `value`.
    #[must_use]
    pub fn rank_le(&self, value: u32) -> usize {
        let high_key = high(value);
        match self
            .chunks
            .binary_search_by_key(&high_key, |chunk| chunk.high_key)
        {
            Ok(index) => {
                let chunk = &self.chunks[index];
                chunk.prefix_len + chunk.container.rank_le(low(value))
            }
            Err(insertion_index) => self
                .chunks
                .get(insertion_index)
                .map_or(self.len, |chunk| chunk.prefix_len),
        }
    }

    /// Returns the zero-based `index`th value, or `None` when out of range.
    #[must_use]
    pub fn select(&self, index: usize) -> Option<u32> {
        if index >= self.len {
            return None;
        }

        let mut left = 0_usize;
        let mut right = self.chunks.len();
        while left < right {
            let middle = left + (right - left) / 2;
            let chunk = &self.chunks[middle];
            if index < chunk.prefix_len + chunk.cardinality {
                right = middle;
            } else {
                left = middle + 1;
            }
        }
        let chunk = &self.chunks[left];
        let low = chunk.container.select(index - chunk.prefix_len)?;
        Some(join(chunk.high_key, low))
    }

    /// Iterates values in strictly increasing order without allocation.
    #[must_use]
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            chunks: &self.chunks,
            next_chunk: 0,
            current_high: 0,
            current: None,
            remaining: self.len,
        }
    }

    /// Computes a canonical set intersection under an explicit result bound.
    ///
    /// A first allocation-free pass computes the exact result cardinality and
    /// non-empty chunk count. The second pass allocates only output chunks and
    /// at most one matching chunk's low values at a time.
    pub fn intersection(&self, other: &Self, limit: EntryLimit) -> Result<Self, RoaringError> {
        let (result_len, result_chunks) = intersection_shape(self, other)?;
        if result_len > limit.max_entries() {
            return Err(RoaringError::EntryLimitExceeded {
                entries: result_len,
                limit: limit.max_entries(),
            });
        }
        if result_len == 0 {
            return Ok(Self {
                chunks: Vec::new(),
                len: 0,
            });
        }

        let mut reservation = SystemReservation;
        let mut chunks = Vec::new();
        reserve_exact(
            &mut chunks,
            result_chunks,
            AllocationTarget::ChunkDirectory,
            None,
            &mut reservation,
        )?;

        let mut left_index = 0_usize;
        let mut right_index = 0_usize;
        let mut prefix_len = 0_usize;
        while left_index < self.chunks.len() && right_index < other.chunks.len() {
            let left = &self.chunks[left_index];
            let right = &other.chunks[right_index];
            match left.high_key.cmp(&right.high_key) {
                core::cmp::Ordering::Less => left_index += 1,
                core::cmp::Ordering::Greater => right_index += 1,
                core::cmp::Ordering::Equal => {
                    let cardinality = intersection_cardinality(&left.container, &right.container)?;
                    if cardinality != 0 {
                        let mut lows = Vec::new();
                        reserve_exact(
                            &mut lows,
                            cardinality,
                            AllocationTarget::IntersectionValues,
                            Some(left.high_key),
                            &mut reservation,
                        )?;
                        write_intersection(&left.container, &right.container, &mut lows);
                        debug_assert_eq!(lows.len(), cardinality);
                        let container =
                            build_container_from_lows(&lows, left.high_key, &mut reservation)?;
                        chunks.push(Chunk {
                            high_key: left.high_key,
                            prefix_len,
                            cardinality,
                            container,
                        });
                        prefix_len = prefix_len.checked_add(cardinality).ok_or(
                            RoaringError::SizeOverflow {
                                calculation: SizeCalculation::TotalCardinality,
                            },
                        )?;
                    }
                    left_index += 1;
                    right_index += 1;
                }
            }
        }
        debug_assert_eq!(prefix_len, result_len);
        debug_assert_eq!(chunks.len(), result_chunks);

        Ok(Self {
            chunks,
            len: result_len,
        })
    }
}

impl<'a> IntoIterator for &'a RoaringBitmap {
    type Item = u32;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Allocation-free ascending iterator over a [`RoaringBitmap`].
pub struct Iter<'a> {
    chunks: &'a [Chunk],
    next_chunk: usize,
    current_high: u16,
    current: Option<ContainerIter<'a>>,
    remaining: usize,
}

impl Iterator for Iter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(current) = &mut self.current
                && let Some(low) = current.next()
            {
                self.remaining -= 1;
                return Some(join(self.current_high, low));
            }

            let chunk = self.chunks.get(self.next_chunk)?;
            self.next_chunk += 1;
            self.current_high = chunk.high_key;
            self.current = Some(chunk.container.iter());
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for Iter<'_> {}

enum ContainerIter<'a> {
    Array(core::slice::Iter<'a, u16>),
    Bitmap(BitmapIter<'a>),
    Run(RunIter<'a>),
}

impl Iterator for ContainerIter<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Array(values) => values.next().copied(),
            Self::Bitmap(values) => values.next(),
            Self::Run(values) => values.next(),
        }
    }
}

struct BitmapIter<'a> {
    words: &'a [u64],
    next_word_index: usize,
    current_word_index: usize,
    remaining_word: u64,
}

impl<'a> BitmapIter<'a> {
    fn new(words: &'a [u64]) -> Self {
        Self {
            words,
            next_word_index: 0,
            current_word_index: 0,
            remaining_word: 0,
        }
    }
}

impl Iterator for BitmapIter<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.remaining_word != 0 {
                let bit = self.remaining_word.trailing_zeros() as usize;
                self.remaining_word &= self.remaining_word - 1;
                return u16::try_from(self.current_word_index * u64::BITS as usize + bit).ok();
            }

            let &word = self.words.get(self.next_word_index)?;
            self.current_word_index = self.next_word_index;
            self.next_word_index += 1;
            self.remaining_word = word;
        }
    }
}

struct RunIter<'a> {
    runs: &'a [Run],
    run_index: usize,
    current: Option<u16>,
}

impl<'a> RunIter<'a> {
    fn new(runs: &'a [Run]) -> Self {
        Self {
            runs,
            run_index: 0,
            current: None,
        }
    }
}

impl Iterator for RunIter<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<Self::Item> {
        let run = self.runs.get(self.run_index)?;
        let value = self.current.unwrap_or(run.start);
        if value == run.end {
            self.run_index += 1;
            self.current = None;
        } else {
            self.current = value.checked_add(1);
        }
        Some(value)
    }
}

trait Reservation {
    fn before_reserve(
        &mut self,
        target: AllocationTarget,
        requested: usize,
        high_key: Option<u16>,
    ) -> Result<(), RoaringError>;
}

struct SystemReservation;

impl Reservation for SystemReservation {
    fn before_reserve(
        &mut self,
        _target: AllocationTarget,
        _requested: usize,
        _high_key: Option<u16>,
    ) -> Result<(), RoaringError> {
        Ok(())
    }
}

fn reserve_exact<T, R: Reservation>(
    values: &mut Vec<T>,
    requested: usize,
    target: AllocationTarget,
    high_key: Option<u16>,
    reservation: &mut R,
) -> Result<(), RoaringError> {
    reservation.before_reserve(target, requested, high_key)?;
    values
        .try_reserve_exact(requested)
        .map_err(|_| RoaringError::AllocationFailed {
            target,
            requested,
            high_key,
        })
}

fn validate_input(values: &[u32], limit: EntryLimit) -> Result<(), RoaringError> {
    if values.len() > limit.max_entries() {
        return Err(RoaringError::EntryLimitExceeded {
            entries: values.len(),
            limit: limit.max_entries(),
        });
    }

    for (offset, pair) in values.windows(2).enumerate() {
        if pair[0] == pair[1] {
            return Err(RoaringError::Duplicate {
                index: offset + 1,
                value: pair[1],
            });
        }
        if pair[0] > pair[1] {
            return Err(RoaringError::NotSorted {
                index: offset + 1,
                previous: pair[0],
                current: pair[1],
            });
        }
    }
    Ok(())
}

fn count_chunks(values: &[u32]) -> Result<usize, RoaringError> {
    if values.is_empty() {
        return Ok(0);
    }
    let mut chunks = 1_usize;
    for pair in values.windows(2) {
        if high(pair[0]) != high(pair[1]) {
            chunks = chunks.checked_add(1).ok_or(RoaringError::SizeOverflow {
                calculation: SizeCalculation::ChunkCount,
            })?;
        }
    }
    Ok(chunks)
}

fn build_container_from_values<R: Reservation>(
    values: &[u32],
    high_key: u16,
    reservation: &mut R,
) -> Result<Container, RoaringError> {
    debug_assert!(!values.is_empty());
    debug_assert!(values.iter().all(|&value| high(value) == high_key));
    let count = values.len();
    let runs = count_runs(values.iter().map(|&value| low(value)));
    let kind = choose_kind(count, runs)?;

    match kind {
        ContainerKind::Array => {
            let mut lows = Vec::new();
            reserve_exact(
                &mut lows,
                count,
                AllocationTarget::ArrayValues,
                Some(high_key),
                reservation,
            )?;
            lows.extend(values.iter().map(|&value| low(value)));
            Ok(Container::Array(lows))
        }
        ContainerKind::Bitmap => {
            let mut words = Vec::new();
            reserve_exact(
                &mut words,
                BITMAP_WORDS,
                AllocationTarget::BitmapWords,
                Some(high_key),
                reservation,
            )?;
            words.resize(BITMAP_WORDS, 0_u64);
            for &value in values {
                set_bit(&mut words, low(value));
            }
            Ok(Container::Bitmap(words))
        }
        ContainerKind::Run => {
            let mut encoded_runs = Vec::new();
            reserve_exact(
                &mut encoded_runs,
                runs,
                AllocationTarget::Runs,
                Some(high_key),
                reservation,
            )?;
            write_runs(values.iter().map(|&value| low(value)), &mut encoded_runs);
            Ok(Container::Run(encoded_runs))
        }
    }
}

fn build_container_from_lows<R: Reservation>(
    values: &[u16],
    high_key: u16,
    reservation: &mut R,
) -> Result<Container, RoaringError> {
    debug_assert!(!values.is_empty());
    let runs = count_runs(values.iter().copied());
    let kind = choose_kind(values.len(), runs)?;

    match kind {
        ContainerKind::Array => {
            let mut lows = Vec::new();
            reserve_exact(
                &mut lows,
                values.len(),
                AllocationTarget::ArrayValues,
                Some(high_key),
                reservation,
            )?;
            lows.extend_from_slice(values);
            Ok(Container::Array(lows))
        }
        ContainerKind::Bitmap => {
            let mut words = Vec::new();
            reserve_exact(
                &mut words,
                BITMAP_WORDS,
                AllocationTarget::BitmapWords,
                Some(high_key),
                reservation,
            )?;
            words.resize(BITMAP_WORDS, 0_u64);
            for &value in values {
                set_bit(&mut words, value);
            }
            Ok(Container::Bitmap(words))
        }
        ContainerKind::Run => {
            let mut encoded_runs = Vec::new();
            reserve_exact(
                &mut encoded_runs,
                runs,
                AllocationTarget::Runs,
                Some(high_key),
                reservation,
            )?;
            write_runs(values.iter().copied(), &mut encoded_runs);
            Ok(Container::Run(encoded_runs))
        }
    }
}

fn choose_kind(cardinality: usize, run_count: usize) -> Result<ContainerKind, RoaringError> {
    let array_cost =
        cardinality
            .checked_mul(ARRAY_VALUE_BYTES)
            .ok_or(RoaringError::SizeOverflow {
                calculation: SizeCalculation::ArrayPayload,
            })?;
    let run_cost = run_count
        .checked_mul(RUN_BYTES)
        .ok_or(RoaringError::SizeOverflow {
            calculation: SizeCalculation::RunPayload,
        })?;

    let mut best_cost = array_cost;
    let mut best_kind = ContainerKind::Array;
    if run_cost < best_cost {
        best_cost = run_cost;
        best_kind = ContainerKind::Run;
    }
    if BITMAP_PAYLOAD_BYTES < best_cost {
        best_kind = ContainerKind::Bitmap;
    }
    Ok(best_kind)
}

fn count_runs(values: impl IntoIterator<Item = u16>) -> usize {
    let mut values = values.into_iter();
    let Some(mut previous) = values.next() else {
        return 0;
    };
    let mut count = 1_usize;
    for current in values {
        if previous.checked_add(1) != Some(current) {
            count += 1;
        }
        previous = current;
    }
    count
}

fn write_runs(values: impl IntoIterator<Item = u16>, output: &mut Vec<Run>) {
    let mut values = values.into_iter();
    let Some(mut start) = values.next() else {
        return;
    };
    let mut end = start;
    for current in values {
        if end.checked_add(1) == Some(current) {
            end = current;
        } else {
            output.push(Run { start, end });
            start = current;
            end = current;
        }
    }
    output.push(Run { start, end });
}

fn set_bit(words: &mut [u64], low: u16) {
    let index = usize::from(low);
    words[index / u64::BITS as usize] |= 1_u64 << (index % u64::BITS as usize);
}

fn intersection_shape(
    left: &RoaringBitmap,
    right: &RoaringBitmap,
) -> Result<(usize, usize), RoaringError> {
    let mut left_index = 0_usize;
    let mut right_index = 0_usize;
    let mut cardinality = 0_usize;
    let mut chunks = 0_usize;
    while left_index < left.chunks.len() && right_index < right.chunks.len() {
        let left_chunk = &left.chunks[left_index];
        let right_chunk = &right.chunks[right_index];
        match left_chunk.high_key.cmp(&right_chunk.high_key) {
            core::cmp::Ordering::Less => left_index += 1,
            core::cmp::Ordering::Greater => right_index += 1,
            core::cmp::Ordering::Equal => {
                let chunk_cardinality =
                    intersection_cardinality(&left_chunk.container, &right_chunk.container)?;
                if chunk_cardinality != 0 {
                    cardinality = cardinality.checked_add(chunk_cardinality).ok_or(
                        RoaringError::SizeOverflow {
                            calculation: SizeCalculation::IntersectionCardinality,
                        },
                    )?;
                    chunks = chunks.checked_add(1).ok_or(RoaringError::SizeOverflow {
                        calculation: SizeCalculation::ChunkCount,
                    })?;
                }
                left_index += 1;
                right_index += 1;
            }
        }
    }
    Ok((cardinality, chunks))
}

fn intersection_cardinality(left: &Container, right: &Container) -> Result<usize, RoaringError> {
    let mut left_values = left.iter().peekable();
    let mut right_values = right.iter().peekable();
    let mut count = 0_usize;
    while let (Some(&left_value), Some(&right_value)) = (left_values.peek(), right_values.peek()) {
        match left_value.cmp(&right_value) {
            core::cmp::Ordering::Less => {
                left_values.next();
            }
            core::cmp::Ordering::Greater => {
                right_values.next();
            }
            core::cmp::Ordering::Equal => {
                count = count.checked_add(1).ok_or(RoaringError::SizeOverflow {
                    calculation: SizeCalculation::IntersectionCardinality,
                })?;
                left_values.next();
                right_values.next();
            }
        }
    }
    Ok(count)
}

fn write_intersection(left: &Container, right: &Container, output: &mut Vec<u16>) {
    let mut left_values = left.iter().peekable();
    let mut right_values = right.iter().peekable();
    while let (Some(&left_value), Some(&right_value)) = (left_values.peek(), right_values.peek()) {
        match left_value.cmp(&right_value) {
            core::cmp::Ordering::Less => {
                left_values.next();
            }
            core::cmp::Ordering::Greater => {
                right_values.next();
            }
            core::cmp::Ordering::Equal => {
                output.push(left_value);
                left_values.next();
                right_values.next();
            }
        }
    }
}

fn high(value: u32) -> u16 {
    (value >> LOW_BITS) as u16
}

fn low(value: u32) -> u16 {
    value as u16
}

fn join(high: u16, low: u16) -> u32 {
    (u32::from(high) << LOW_BITS) | u32::from(low)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(values: &[u32]) -> RoaringBitmap {
        RoaringBitmap::try_from_sorted(values, EntryLimit::new(values.len()))
            .expect("valid test bitmap")
    }

    fn assert_matches_naive(values: &[u32]) {
        let bitmap = build(values);
        assert_eq!(bitmap.len(), values.len());
        assert_eq!(bitmap.is_empty(), values.is_empty());
        assert_eq!(bitmap.iter().collect::<Vec<_>>(), values);
        assert_eq!((&bitmap).into_iter().collect::<Vec<_>>(), values);
        assert_eq!(
            bitmap.iter().size_hint(),
            (values.len(), Some(values.len()))
        );

        for (index, &value) in values.iter().enumerate() {
            assert!(bitmap.contains(value));
            assert_eq!(bitmap.select(index), Some(value));
        }
        assert_eq!(bitmap.select(values.len()), None);

        let probes = [0, 1, 2, 65_535, 65_536, 65_537, u32::MAX - 1, u32::MAX];
        for probe in probes {
            assert_eq!(
                bitmap.contains(probe),
                values.binary_search(&probe).is_ok(),
                "contains({probe})"
            );
            assert_eq!(
                bitmap.rank_le(probe),
                values.partition_point(|&value| value <= probe),
                "rank_le({probe})"
            );
        }
    }

    #[test]
    fn empty_and_cross_chunk_boundaries() {
        assert_matches_naive(&[]);
        assert_matches_naive(&[0, 1, 65_535, 65_536, 65_537, u32::MAX]);
        let bitmap = build(&[0, 65_536, u32::MAX]);
        assert_eq!(bitmap.chunk_count(), 3);
    }

    #[test]
    fn exhaustive_small_sets_match_sorted_vec() {
        const UNIVERSE: u32 = 12;
        for mask in 0_u32..(1_u32 << UNIVERSE) {
            let values = (0..UNIVERSE)
                .filter(|bit| mask & (1_u32 << bit) != 0)
                .collect::<Vec<_>>();
            assert_matches_naive(&values);
            let bitmap = build(&values);
            for probe in 0..=UNIVERSE {
                assert_eq!(
                    bitmap.rank_le(probe),
                    values.partition_point(|&value| value <= probe)
                );
            }
        }
    }

    #[test]
    fn deterministic_randomized_differential() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _case in 0..512 {
            let mut values = Vec::new();
            for candidate in 0_u32..4096 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                if state >> 60 == 0 {
                    values.push(candidate);
                }
                if state & 0x1f == 0 {
                    values.push((3_u32 << 16) | candidate);
                }
            }
            values.sort_unstable();
            values.dedup();
            assert_matches_naive(&values);

            let bitmap = build(&values);
            for _ in 0..64 {
                state = state
                    .wrapping_mul(2_862_933_555_777_941_757)
                    .wrapping_add(3_037_000_493);
                let probe = state as u32;
                assert_eq!(bitmap.contains(probe), values.binary_search(&probe).is_ok());
                assert_eq!(
                    bitmap.rank_le(probe),
                    values.partition_point(|&value| value <= probe)
                );
            }
        }
    }

    #[test]
    fn deterministic_container_cost_selection_and_ties() {
        let two_consecutive = build(&[10, 11]);
        assert_eq!(
            two_consecutive.container_kind(0),
            Some(ContainerKind::Array),
            "array/run cost tie prefers array"
        );

        let array_bitmap_tie = (0_u32..4096).map(|value| value * 2).collect::<Vec<_>>();
        assert_eq!(
            build(&array_bitmap_tie).container_kind(0),
            Some(ContainerKind::Array),
            "array/bitmap cost tie prefers array"
        );

        let bitmap_wins = (0_u32..4097).map(|value| value * 2).collect::<Vec<_>>();
        assert_eq!(
            build(&bitmap_wins).container_kind(0),
            Some(ContainerKind::Bitmap)
        );

        let one_long_run = (100_u32..10_000).collect::<Vec<_>>();
        assert_eq!(
            build(&one_long_run).container_kind(0),
            Some(ContainerKind::Run)
        );

        let all_three_tie = (0_u32..2048)
            .flat_map(|run| [run * 4, run * 4 + 1])
            .collect::<Vec<_>>();
        assert_eq!(all_three_tie.len(), 4096);
        assert_eq!(
            build(&all_three_tie).container_kind(0),
            Some(ContainerKind::Array),
            "three-way tie prefers array"
        );

        let run_bitmap_tie = (0_u32..2048)
            .flat_map(|run| {
                if run == 0 {
                    vec![0, 1, 2]
                } else {
                    vec![run * 4, run * 4 + 1]
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(run_bitmap_tie.len(), 4097);
        assert_eq!(
            build(&run_bitmap_tie).container_kind(0),
            Some(ContainerKind::Run),
            "run/bitmap cost tie prefers run"
        );
    }

    #[test]
    fn adversarial_sparse_bitmap_and_run_chunks_round_trip() {
        let sparse = [0, 65_535, 65_536, 17_u32 << 16, u32::MAX];
        assert_matches_naive(&sparse);

        let bitmap_values = (0_u32..32_768).map(|value| value * 2).collect::<Vec<_>>();
        let bitmap = build(&bitmap_values);
        assert_eq!(bitmap.container_kind(0), Some(ContainerKind::Bitmap));
        assert_matches_naive(&bitmap_values);

        let full_run = (0_u32..=u16::MAX.into()).collect::<Vec<_>>();
        let run = build(&full_run);
        assert_eq!(run.container_kind(0), Some(ContainerKind::Run));
        assert_matches_naive(&full_run);
    }

    #[test]
    fn malformed_input_and_limit_are_typed() {
        assert_eq!(
            RoaringBitmap::try_from_sorted(&[1, 1], EntryLimit::new(2)),
            Err(RoaringError::Duplicate { index: 1, value: 1 })
        );
        assert_eq!(
            RoaringBitmap::try_from_sorted(&[2, 1], EntryLimit::new(2)),
            Err(RoaringError::NotSorted {
                index: 1,
                previous: 2,
                current: 1,
            })
        );
        assert_eq!(
            RoaringBitmap::try_from_sorted(&[1, 2], EntryLimit::new(1)),
            Err(RoaringError::EntryLimitExceeded {
                entries: 2,
                limit: 1,
            })
        );
    }

    struct RecordingReservation {
        calls: usize,
        fail_target: Option<AllocationTarget>,
    }

    impl Reservation for RecordingReservation {
        fn before_reserve(
            &mut self,
            target: AllocationTarget,
            requested: usize,
            high_key: Option<u16>,
        ) -> Result<(), RoaringError> {
            self.calls += 1;
            if self.fail_target == Some(target) {
                return Err(RoaringError::AllocationFailed {
                    target,
                    requested,
                    high_key,
                });
            }
            Ok(())
        }
    }

    #[test]
    fn invalid_input_is_rejected_before_any_allocation() {
        for (values, limit) in [(&[2_u32, 1][..], 2_usize), (&[1_u32, 2][..], 1_usize)] {
            let mut reservation = RecordingReservation {
                calls: 0,
                fail_target: None,
            };
            assert!(
                RoaringBitmap::try_from_sorted_with(
                    values,
                    EntryLimit::new(limit),
                    &mut reservation
                )
                .is_err()
            );
            assert_eq!(reservation.calls, 0);
        }
    }

    #[test]
    fn allocation_failures_name_each_selected_component() {
        let cases = [
            (vec![7_u32], AllocationTarget::ChunkDirectory, None, 1_usize),
            (vec![7_u32], AllocationTarget::ArrayValues, Some(0), 1),
            (
                (0_u32..4097).map(|value| value * 2).collect(),
                AllocationTarget::BitmapWords,
                Some(0),
                BITMAP_WORDS,
            ),
            (
                (100_u32..10_000).collect(),
                AllocationTarget::Runs,
                Some(0),
                1,
            ),
        ];

        for (values, target, high_key, requested) in cases {
            let mut reservation = RecordingReservation {
                calls: 0,
                fail_target: Some(target),
            };
            assert_eq!(
                RoaringBitmap::try_from_sorted_with(
                    &values,
                    EntryLimit::new(values.len()),
                    &mut reservation
                ),
                Err(RoaringError::AllocationFailed {
                    target,
                    requested,
                    high_key,
                })
            );
        }
    }

    fn naive_intersection(left: &[u32], right: &[u32]) -> Vec<u32> {
        let mut output = Vec::new();
        let mut left_index = 0_usize;
        let mut right_index = 0_usize;
        while left_index < left.len() && right_index < right.len() {
            match left[left_index].cmp(&right[right_index]) {
                core::cmp::Ordering::Less => left_index += 1,
                core::cmp::Ordering::Greater => right_index += 1,
                core::cmp::Ordering::Equal => {
                    output.push(left[left_index]);
                    left_index += 1;
                    right_index += 1;
                }
            }
        }
        output
    }

    #[test]
    fn exhaustive_small_intersections_match_naive_and_are_symmetric() {
        const UNIVERSE: u32 = 8;
        let all_sets = (0_u32..(1_u32 << UNIVERSE))
            .map(|mask| {
                (0..UNIVERSE)
                    .filter(|bit| mask & (1_u32 << bit) != 0)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        for left in &all_sets {
            let left_bitmap = build(left);
            let idempotent = left_bitmap
                .intersection(&left_bitmap, EntryLimit::new(left.len()))
                .expect("idempotent intersection");
            assert_eq!(idempotent, left_bitmap);

            for right in &all_sets {
                let right_bitmap = build(right);
                let expected = naive_intersection(left, right);
                let left_right = left_bitmap
                    .intersection(&right_bitmap, EntryLimit::new(expected.len()))
                    .expect("left/right intersection");
                let right_left = right_bitmap
                    .intersection(&left_bitmap, EntryLimit::new(expected.len()))
                    .expect("right/left intersection");
                assert_eq!(left_right.iter().collect::<Vec<_>>(), expected);
                assert_eq!(left_right, right_left);
            }
        }
    }

    #[test]
    fn mixed_container_intersections_are_canonical() {
        let array_values = vec![0, 2, 4, 6, 8, 10];
        let bitmap_values = (0_u32..4097).map(|value| value * 2).collect::<Vec<_>>();
        let run_values = (4_u32..20_000).collect::<Vec<_>>();
        let high_values = (0_u32..5000)
            .map(|value| (9_u32 << 16) | value)
            .collect::<Vec<_>>();

        let mut right_values = run_values.clone();
        right_values.extend_from_slice(&high_values);

        let array = build(&array_values);
        let bitmap = build(&bitmap_values);
        let right = build(&right_values);
        assert_eq!(array.container_kind(0), Some(ContainerKind::Array));
        assert_eq!(bitmap.container_kind(0), Some(ContainerKind::Bitmap));
        assert_eq!(right.container_kind(0), Some(ContainerKind::Run));

        for left in [&array, &bitmap] {
            let expected = naive_intersection(&left.iter().collect::<Vec<_>>(), &right_values);
            let intersection = left
                .intersection(&right, EntryLimit::new(expected.len()))
                .expect("mixed intersection");
            assert_eq!(intersection.iter().collect::<Vec<_>>(), expected);
            assert_eq!(
                intersection,
                build(&expected),
                "intersection is reconstructed canonically"
            );
        }
    }

    #[test]
    fn intersection_limit_is_checked_before_result_allocation() {
        let left = build(&(0_u32..100).collect::<Vec<_>>());
        let right = left.clone();
        assert_eq!(
            left.intersection(&right, EntryLimit::new(99)),
            Err(RoaringError::EntryLimitExceeded {
                entries: 100,
                limit: 99,
            })
        );
    }
}
