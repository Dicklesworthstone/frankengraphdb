//! Elias-Fano coding for nondecreasing `u64` sequences.
//!
//! This module owns the safe scalar mechanics only. It deliberately exposes no
//! durable byte format: registered format rows and generated readers will own
//! framing, versioning, and per-kind limits. Construction requires an explicit
//! [`EntryLimit`] and uses checked arithmetic before every internal allocation.
//! A cumulative one-count per unary-high word locates `select` targets by
//! binary search, so adversarial zero-word gaps are never traversed linearly.
//! Scalar `select` is `O(log high_words)` with at most one word inspected after
//! the directory search. Rank locates the probe's unary-high bucket with the
//! same directory, then searches only that bucket's packed low fields. Its
//! cost is `O(log high_words + log bucket_entries)` without another index or
//! allocation; this module makes no O(1) rank/select claim.

#![forbid(unsafe_code)]

use core::fmt;

/// Maximum logical entries a caller permits one Elias-Fano value to own.
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

/// Internal allocation named by a construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocationTarget {
    /// Packed low-bit words.
    LowBits,
    /// Unary high-bit words.
    HighBits,
    /// Cumulative one-counts for unary high-bit words.
    HighWordRanks,
}

/// Checked Elias-Fano construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EliasFanoError {
    /// The input contains more entries than the caller authorized.
    EntryLimitExceeded {
        /// Number of input entries.
        entries: usize,
        /// Caller-provided ceiling.
        limit: usize,
    },
    /// Input ceased to be nondecreasing at `index`.
    NotMonotone {
        /// Index of the first smaller value.
        index: usize,
        /// Value immediately before `index`.
        previous: u64,
        /// Value at `index`.
        current: u64,
    },
    /// Representation-size arithmetic or a platform conversion overflowed.
    SizeOverflow {
        /// Stable calculation name.
        calculation: SizeCalculation,
    },
    /// Reserving one representation component failed before publication.
    AllocationFailed {
        /// Component being allocated.
        target: AllocationTarget,
        /// Requested words or entries, according to `target`.
        requested: usize,
    },
}

/// Stable representation-size calculation names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SizeCalculation {
    /// Converting the platform input length into the `u64` value domain.
    EntryCount,
    /// Multiplying entry count by the canonical low-bit width.
    LowBitCount,
    /// Computing the unary high-bit length.
    HighBitCount,
    /// Converting a unary high-bit position to the platform index domain.
    HighBitPosition,
    /// Converting the entry count into the high-word directory's `u32` rank
    /// domain.
    HighWordRank,
}

impl fmt::Display for EliasFanoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EntryLimitExceeded { entries, limit } => write!(
                formatter,
                "Elias-Fano input has {entries} entries, limit is {limit}"
            ),
            Self::NotMonotone {
                index,
                previous,
                current,
            } => write!(
                formatter,
                "Elias-Fano input decreases at index {index}: {previous} then {current}"
            ),
            Self::SizeOverflow { calculation } => {
                write!(
                    formatter,
                    "Elias-Fano {calculation:?} arithmetic overflowed"
                )
            }
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "could not reserve {requested} units for Elias-Fano {target:?}"
            ),
        }
    }
}

impl std::error::Error for EliasFanoError {}

/// Immutable Elias-Fano representation of a nondecreasing sequence.
///
/// Duplicates are retained as distinct positions. [`Self::rank_le`] counts all
/// duplicates at the probe; [`Self::predecessor`] returns the last value not
/// greater than the probe, and [`Self::successor`] returns the first value not
/// less than it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EliasFano {
    len: usize,
    max_value: u64,
    low_bits: u8,
    low_words: Vec<u64>,
    high_words: Vec<u64>,
    high_bit_len: usize,
    high_word_ranks: Vec<u32>,
}

/// Allocation-free forward cursor over one scalar Elias-Fano sequence.
///
/// This physical-access seam is deliberately below durable framing. It scans
/// each unary-high word at most once and reads each low field once, so consuming
/// the cursor is linear in the encoded words plus yielded values. Callers can
/// use [`Self::fill`] to cap the amount of work and caller-owned storage used by
/// one step. Sparse gaps use the rank directory with logarithmic probes in the
/// gap length, so a single value never linearly walks a long run of zero words.
pub struct EliasFanoSequentialCursor<'a> {
    encoded: &'a EliasFano,
    logical_index: usize,
    high_word_index: usize,
    remaining_high_word: u64,
    #[cfg(test)]
    high_words_examined: usize,
    #[cfg(test)]
    high_gap_probes: usize,
}

impl EliasFano {
    /// Constructs the unique scalar Elias-Fano representation selected by
    /// `low_bits = floor(log2(max_value / entry_count))`, clamped to zero.
    pub fn try_new(values: &[u64], limit: EntryLimit) -> Result<Self, EliasFanoError> {
        if values.len() > limit.max_entries() {
            return Err(EliasFanoError::EntryLimitExceeded {
                entries: values.len(),
                limit: limit.max_entries(),
            });
        }

        let (len_u64, len_rank) = preflight_entry_count(values.len())?;

        for (offset, pair) in values.windows(2).enumerate() {
            if pair[0] > pair[1] {
                return Err(EliasFanoError::NotMonotone {
                    index: offset + 1,
                    previous: pair[0],
                    current: pair[1],
                });
            }
        }

        if values.is_empty() {
            return Ok(Self {
                len: 0,
                max_value: 0,
                low_bits: 0,
                low_words: Vec::new(),
                high_words: Vec::new(),
                high_bit_len: 0,
                high_word_ranks: Vec::new(),
            });
        }

        let max_value = values[values.len() - 1];
        let ratio = max_value / len_u64;
        let low_bits = if ratio == 0 {
            0
        } else {
            (u64::BITS - 1 - ratio.leading_zeros()) as u8
        };

        let low_bit_count = values.len().checked_mul(usize::from(low_bits)).ok_or(
            EliasFanoError::SizeOverflow {
                calculation: SizeCalculation::LowBitCount,
            },
        )?;
        let low_word_count = words_for_bits(low_bit_count).ok_or(EliasFanoError::SizeOverflow {
            calculation: SizeCalculation::LowBitCount,
        })?;

        let maximum_high = max_value >> low_bits;
        let high_bit_len_u64 =
            maximum_high
                .checked_add(len_u64)
                .ok_or(EliasFanoError::SizeOverflow {
                    calculation: SizeCalculation::HighBitCount,
                })?;
        let high_bit_len =
            usize::try_from(high_bit_len_u64).map_err(|_| EliasFanoError::SizeOverflow {
                calculation: SizeCalculation::HighBitCount,
            })?;
        let high_word_count = words_for_bits(high_bit_len).ok_or(EliasFanoError::SizeOverflow {
            calculation: SizeCalculation::HighBitCount,
        })?;
        let mut low_words = allocate_zeroed(low_word_count, AllocationTarget::LowBits)?;
        let mut high_words = allocate_zeroed(high_word_count, AllocationTarget::HighBits)?;

        let low_mask = low_mask(low_bits);
        for (index, &value) in values.iter().enumerate() {
            if low_bits != 0 {
                let bit_offset = index * usize::from(low_bits);
                write_low_bits(&mut low_words, bit_offset, low_bits, value & low_mask);
            }

            let index_u64 = u64::try_from(index).map_err(|_| EliasFanoError::SizeOverflow {
                calculation: SizeCalculation::EntryCount,
            })?;
            let high_position_u64 =
                (value >> low_bits)
                    .checked_add(index_u64)
                    .ok_or(EliasFanoError::SizeOverflow {
                        calculation: SizeCalculation::HighBitPosition,
                    })?;
            let high_position =
                usize::try_from(high_position_u64).map_err(|_| EliasFanoError::SizeOverflow {
                    calculation: SizeCalculation::HighBitPosition,
                })?;
            debug_assert!(high_position < high_bit_len);
            high_words[high_position / 64] |= 1_u64 << (high_position % 64);
        }

        let mut high_word_ranks =
            allocate_zeroed_u32(high_word_count, AllocationTarget::HighWordRanks)?;
        let mut cumulative_rank = 0_u32;
        for (rank, &word) in high_word_ranks.iter_mut().zip(&high_words) {
            cumulative_rank = cumulative_rank.checked_add(word.count_ones()).ok_or(
                EliasFanoError::SizeOverflow {
                    calculation: SizeCalculation::HighWordRank,
                },
            )?;
            *rank = cumulative_rank;
        }
        debug_assert_eq!(cumulative_rank, len_rank);

        Ok(Self {
            len: values.len(),
            max_value,
            low_bits,
            low_words,
            high_words,
            high_bit_len,
            high_word_ranks,
        })
    }

    /// Number of represented values, including duplicates.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// True when the represented sequence is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Largest represented value, or `None` for the empty sequence.
    #[must_use]
    pub const fn max_value(&self) -> Option<u64> {
        if self.is_empty() {
            None
        } else {
            Some(self.max_value)
        }
    }

    /// Canonically selected number of low bits per value.
    #[must_use]
    pub const fn low_bits(&self) -> u8 {
        self.low_bits
    }

    /// Canonical packed words, excluding the derived select directory. The
    /// payload codec emits these as little-endian words; constructors keep
    /// unused tail bits zero, so serialization need not decode every value.
    pub(crate) fn payload_words(&self) -> (&[u64], &[u64]) {
        (&self.low_words, &self.high_words)
    }

    /// Total logical payload in equivalent 64-bit words, including the
    /// high-word rank directory rounded up from its `u32` entries.
    ///
    /// This deliberately excludes vector capacities, struct metadata, and
    /// allocator rounding. It is an encoding-density accounting aid, not a
    /// retained-memory measurement or durable encoding size.
    #[must_use]
    pub const fn logical_storage_words(&self) -> usize {
        self.low_words.len() + self.high_words.len() + self.high_word_ranks.len().div_ceil(2)
    }

    /// Capacity of all owned word and rank-directory allocations. This omits
    /// the inline value and allocator metadata; unlike logical_storage_words,
    /// it includes spare capacity and is suitable for retained-cache admission.
    pub fn owned_heap_bytes(&self) -> Option<usize> {
        self.low_words
            .capacity()
            .checked_add(self.high_words.capacity())?
            .checked_mul(size_of::<u64>())?
            .checked_add(
                self.high_word_ranks
                    .capacity()
                    .checked_mul(size_of::<u32>())?,
            )
    }

    /// Logical number of bits in the unary high vector, excluding word padding.
    #[must_use]
    pub const fn high_bit_len(&self) -> usize {
        self.high_bit_len
    }

    /// Creates an allocation-free cursor at the first represented value.
    ///
    /// This is a physical-access seam, not a durable iterator
    /// format or rank/select capability claim.
    pub fn sequential_cursor(&self) -> EliasFanoSequentialCursor<'_> {
        EliasFanoSequentialCursor {
            encoded: self,
            logical_index: 0,
            high_word_index: 0,
            remaining_high_word: self.high_words.first().copied().unwrap_or(0),
            #[cfg(test)]
            high_words_examined: usize::from(!self.high_words.is_empty()),
            #[cfg(test)]
            high_gap_probes: 0,
        }
    }

    /// Creates an allocation-free forward cursor at a logical position.
    ///
    /// A nonzero start uses one positional directory lookup, then scans each
    /// remaining unary-high word at most once. Seeking never decodes the
    /// preceding values. A position at or beyond the end returns an exhausted
    /// cursor; duplicates remain distinct positions.
    pub fn sequential_cursor_from(&self, index: usize) -> EliasFanoSequentialCursor<'_> {
        if index == 0 {
            return self.sequential_cursor();
        }
        if index >= self.len {
            return EliasFanoSequentialCursor {
                encoded: self,
                logical_index: self.len,
                high_word_index: self.high_words.len(),
                remaining_high_word: 0,
                #[cfg(test)]
                high_words_examined: 0,
                #[cfg(test)]
                high_gap_probes: 0,
            };
        }
        let position = self
            .select_high_position(index)
            .expect("validated Elias-Fano position");
        let high_word_index = position / u64::BITS as usize;
        EliasFanoSequentialCursor {
            encoded: self,
            logical_index: index,
            high_word_index,
            remaining_high_word: self.high_words[high_word_index]
                & (u64::MAX << (position % u64::BITS as usize)),
            #[cfg(test)]
            high_words_examined: 1,
            #[cfg(test)]
            high_gap_probes: 0,
        }
    }

    /// Returns the value at `index`, or `None` when out of bounds.
    ///
    /// Locating the unary-high word takes `O(log high_words)` directory
    /// probes; selection inside that word examines at most 64 bits.
    #[must_use]
    pub fn select(&self, index: usize) -> Option<u64> {
        if index >= self.len {
            return None;
        }
        let high_position = self.select_high_position(index)?;
        let index_u64 = u64::try_from(index).ok()?;
        let high = u64::try_from(high_position).ok()?.checked_sub(index_u64)?;
        let low = self.read_low_bits(index);
        Some((high << self.low_bits) | low)
    }

    /// Alias for positional access.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<u64> {
        self.select(index)
    }

    /// Counts represented values less than or equal to `value`.
    ///
    /// Locating the unary-high bucket takes `O(log high_words)` directory
    /// probes, followed by `O(log bucket_entries)` packed-low comparisons.
    #[must_use]
    pub fn rank_le(&self, value: u64) -> usize {
        self.search_rank(value, true).0
    }

    /// Counts represented values strictly less than `value`.
    ///
    /// Locating the unary-high bucket takes `O(log high_words)` directory
    /// probes, followed by `O(log bucket_entries)` packed-low comparisons.
    #[must_use]
    pub fn rank_lt(&self, value: u64) -> usize {
        self.search_rank(value, false).0
    }

    /// Returns the last represented value not greater than `value`.
    #[must_use]
    pub fn predecessor(&self, value: u64) -> Option<u64> {
        self.rank_le(value)
            .checked_sub(1)
            .and_then(|index| self.select(index))
    }

    /// Returns the first represented value not less than `value`.
    #[must_use]
    pub fn successor(&self, value: u64) -> Option<u64> {
        self.select(self.rank_lt(value))
    }

    fn search_rank(&self, value: u64, inclusive: bool) -> (usize, usize) {
        if self.len == 0 || (!inclusive && value == 0) {
            return (0, 0);
        }
        if value > self.max_value || (inclusive && value == self.max_value) {
            return (self.len, 0);
        }
        // The admitted maximum high field equals high_bit_len - len, so this
        // conversion fits even on a narrower platform. Check the probe against
        // max_value BEFORE converting: an arbitrary absent probe need not fit.
        let high = (value >> self.low_bits) as usize;
        let mask = low_mask(self.low_bits);
        let low = value & mask;
        if inclusive && low == mask {
            return self.high_bucket_start(high + 1);
        }
        let (mut left, mut probes) = self.high_bucket_start(high);
        if !inclusive && low == 0 {
            return (left, probes);
        }
        let (mut right, end_probes) = self.high_bucket_start(high + 1);
        probes += end_probes;
        while left < right {
            let middle = left + (right - left) / 2;
            let candidate = self.read_low_bits(middle);
            if candidate < low || (inclusive && candidate == low) {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        (left, probes)
    }

    // The one for entry i occupies high(i) + i. Thus the high-th zero
    // separates all entries with high(i) < high from this bucket. The existing
    // cumulative ONE directory also supplies cumulative ZERO counts, with the
    // final word capped at the actual bit length rather than its zero padding.
    // Return the directory-probe count for the same bounded-work tests as select.
    fn high_bucket_start(&self, high: usize) -> (usize, usize) {
        if high == 0 {
            return (0, 0);
        }
        if high > self.high_bit_len - self.len {
            return (self.len, 0);
        }
        let zero_ordinal = high - 1;
        let mut left = 0;
        let mut right = self.high_word_ranks.len();
        let mut probes = 0;
        while left < right {
            let middle = left + (right - left) / 2;
            probes += 1;
            // Cap before adding, so even a final partial word near usize::MAX
            // never rounds its end beyond the platform index domain.
            let start = middle * 64;
            let end = start + (self.high_bit_len - start).min(64);
            let zeros = end - self.high_word_ranks[middle] as usize;
            if zeros <= zero_ordinal {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        let first_bit = left * 64;
        let preceding_ones = if left == 0 {
            0
        } else {
            self.high_word_ranks[left - 1] as usize
        };
        let preceding_zeros = first_bit - preceding_ones;
        let within_word = zero_ordinal - preceding_zeros;
        let valid_bits = (self.high_bit_len - first_bit).min(64);
        let zeros = !self.high_words[left] & (u64::MAX >> (64 - valid_bits));
        let Some(position) = select_one(zeros, within_word) else {
            debug_assert!(
                false,
                "private Elias-Fano representation lost a high bucket"
            );
            return (self.len, probes);
        };
        (preceding_ones + position - within_word, probes)
    }

    // The former implementation remains an independent, select-based oracle
    // and a paired measurement control. It does not use unary zero separators.
    #[cfg(test)]
    fn partition_point(&self, mut predicate: impl FnMut(u64) -> bool) -> usize {
        let mut left = 0_usize;
        let mut right = self.len;
        while left < right {
            let middle = left + (right - left) / 2;
            let Some(candidate) = self.select(middle) else {
                debug_assert!(false, "private Elias-Fano representation lost a value");
                return left;
            };
            if predicate(candidate) {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        left
    }

    fn select_high_position(&self, index: usize) -> Option<usize> {
        self.search_high_position(index).0
    }

    fn search_high_position(&self, index: usize) -> (Option<usize>, usize) {
        if index >= self.len {
            return (None, 0);
        }
        let Some(target_rank) = u32::try_from(index)
            .ok()
            .and_then(|rank| rank.checked_add(1))
        else {
            return (None, 0);
        };

        let mut left = 0_usize;
        let mut right = self.high_word_ranks.len();
        let mut directory_probes = 0_usize;
        while left < right {
            let middle = left + (right - left) / 2;
            directory_probes += 1;
            if self.high_word_ranks[middle] < target_rank {
                left = middle + 1;
            } else {
                right = middle;
            }
        }

        let Some(&word) = self.high_words.get(left) else {
            return (None, directory_probes);
        };
        let preceding_rank = if left == 0 {
            0
        } else {
            self.high_word_ranks[left - 1]
        };
        let within_word_ordinal = preceding_rank
            .checked_add(1)
            .and_then(|first_rank| target_rank.checked_sub(first_rank))
            .and_then(|ordinal| usize::try_from(ordinal).ok());
        let position = within_word_ordinal
            .and_then(|ordinal| select_one(word, ordinal))
            .and_then(|within_word| {
                left.checked_mul(64)
                    .and_then(|base| base.checked_add(within_word))
            });
        (position, directory_probes)
    }

    #[cfg(test)]
    fn select_directory_probes(&self, index: usize) -> Option<usize> {
        let (position, probes) = self.search_high_position(index);
        position.map(|_| probes)
    }

    fn read_low_bits(&self, index: usize) -> u64 {
        if self.low_bits == 0 {
            return 0;
        }
        let width = usize::from(self.low_bits);
        let bit_offset = index * width;
        let word_index = bit_offset / 64;
        let shift = bit_offset % 64;
        let mut value = self.low_words[word_index] >> shift;
        if shift + width > 64 {
            value |= self.low_words[word_index + 1] << (64 - shift);
        }
        value & low_mask(self.low_bits)
    }
}

impl EliasFanoSequentialCursor<'_> {
    /// Gallop over a zero-word gap, then locate its end in the bounded interval.
    /// Searching only the gap keeps total traversal linear in the encoded size;
    /// a global directory search at every word boundary would not have that law.
    fn skip_empty_high_words(&mut self) -> Option<()> {
        let target_rank = u32::try_from(self.logical_index).ok()?.checked_add(1)?;
        let last = self.encoded.high_word_ranks.len().checked_sub(1)?;
        let origin = self.high_word_index;
        let mut left = origin.checked_add(1)?;
        let mut stride = 1_usize;
        let mut right;
        loop {
            right = origin.saturating_add(stride).min(last);
            #[cfg(test)]
            {
                self.high_gap_probes += 1;
            }
            if self.encoded.high_word_ranks[right] >= target_rank {
                break;
            }
            if right == last {
                return None;
            }
            left = right + 1;
            stride = stride.saturating_mul(2);
        }
        while left < right {
            let middle = left + (right - left) / 2;
            #[cfg(test)]
            {
                self.high_gap_probes += 1;
            }
            if self.encoded.high_word_ranks[middle] < target_rank {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        self.high_word_index = left;
        self.remaining_high_word = *self.encoded.high_words.get(left)?;
        #[cfg(test)]
        {
            self.high_words_examined += 1;
        }
        Some(())
    }

    /// Fills at most `output.len()` values and returns the initialized prefix.
    ///
    /// No allocation or repeated positional search per value is performed.
    /// Sparse high-word gaps use bounded directory probes.
    pub fn fill(&mut self, output: &mut [u64]) -> usize {
        let mut written = 0_usize;
        while written < output.len() {
            let Some(value) = self.next() else {
                break;
            };
            output[written] = value;
            written += 1;
        }
        written
    }

    #[cfg(test)]
    const fn high_words_examined(&self) -> usize {
        self.high_words_examined
    }
}

impl Iterator for EliasFanoSequentialCursor<'_> {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        if self.logical_index >= self.encoded.len {
            return None;
        }

        while self.remaining_high_word == 0 {
            self.high_word_index = self.high_word_index.checked_add(1)?;
            self.remaining_high_word = *self.encoded.high_words.get(self.high_word_index)?;
            #[cfg(test)]
            {
                self.high_words_examined += 1;
            }
            if self.remaining_high_word == 0 {
                self.skip_empty_high_words()?;
            }
        }

        let within_word = self.remaining_high_word.trailing_zeros() as usize;
        self.remaining_high_word &= self.remaining_high_word - 1;
        let high_position = self
            .high_word_index
            .checked_mul(u64::BITS as usize)?
            .checked_add(within_word)?;
        let index_u64 = u64::try_from(self.logical_index).ok()?;
        let high = u64::try_from(high_position).ok()?.checked_sub(index_u64)?;
        let low = self.encoded.read_low_bits(self.logical_index);
        let value = (high << self.encoded.low_bits) | low;
        self.logical_index += 1;
        Some(value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.encoded.len - self.logical_index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for EliasFanoSequentialCursor<'_> {}
impl core::iter::FusedIterator for EliasFanoSequentialCursor<'_> {}

fn words_for_bits(bits: usize) -> Option<usize> {
    let complete = bits / 64;
    complete.checked_add(usize::from(!bits.is_multiple_of(64)))
}

fn preflight_entry_count(entries: usize) -> Result<(u64, u32), EliasFanoError> {
    let entries_u64 = u64::try_from(entries).map_err(|_| EliasFanoError::SizeOverflow {
        calculation: SizeCalculation::EntryCount,
    })?;
    let high_word_rank = checked_high_word_rank(entries)?;
    Ok((entries_u64, high_word_rank))
}

fn checked_high_word_rank(entries: usize) -> Result<u32, EliasFanoError> {
    u32::try_from(entries).map_err(|_| EliasFanoError::SizeOverflow {
        calculation: SizeCalculation::HighWordRank,
    })
}

fn allocate_zeroed(count: usize, target: AllocationTarget) -> Result<Vec<u64>, EliasFanoError> {
    let mut words = Vec::new();
    words
        .try_reserve_exact(count)
        .map_err(|_| EliasFanoError::AllocationFailed {
            target,
            requested: count,
        })?;
    words.resize(count, 0);
    Ok(words)
}

fn allocate_zeroed_u32(count: usize, target: AllocationTarget) -> Result<Vec<u32>, EliasFanoError> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count)
        .map_err(|_| EliasFanoError::AllocationFailed {
            target,
            requested: count,
        })?;
    entries.resize(count, 0);
    Ok(entries)
}

fn low_mask(bits: u8) -> u64 {
    if bits == 0 { 0 } else { (1_u64 << bits) - 1 }
}

fn write_low_bits(words: &mut [u64], bit_offset: usize, width: u8, value: u64) {
    debug_assert!((1..64).contains(&width));
    let width = usize::from(width);
    let word_index = bit_offset / 64;
    let shift = bit_offset % 64;
    words[word_index] |= value << shift;
    if shift + width > 64 {
        words[word_index + 1] |= value >> (64 - shift);
    }
}

fn select_one(mut bits: u64, ordinal: usize) -> Option<usize> {
    if ordinal >= bits.count_ones() as usize {
        return None;
    }
    for _ in 0..ordinal {
        bits &= bits - 1;
    }
    Some(bits.trailing_zeros() as usize)
}

#[cfg(test)]
mod tests {
    #[test]
    fn retained_heap_counts_spare_words_and_rank_directory_capacity() {
        let mut encoded =
            super::EliasFano::try_new(&[1, 9, 1000], super::EntryLimit::new(3)).unwrap();
        encoded.low_words.reserve_exact(101);
        encoded.high_words.reserve_exact(73);
        encoded.high_word_ranks.reserve_exact(53);
        let expected = (encoded.low_words.capacity() + encoded.high_words.capacity())
            * size_of::<u64>()
            + encoded.high_word_ranks.capacity() * size_of::<u32>();
        assert_eq!(encoded.owned_heap_bytes(), Some(expected));
        assert!(expected > encoded.logical_storage_words() * size_of::<u64>());
        assert_eq!(encoded.select(2), Some(1000));
    }

    use super::*;

    fn assert_matches_naive(values: &[u64]) {
        let encoded = EliasFano::try_new(values, EntryLimit::new(values.len()))
            .expect("bounded nondecreasing fixture");
        assert_eq!(encoded.len(), values.len());
        assert_eq!(encoded.is_empty(), values.is_empty());
        assert_eq!(encoded.max_value(), values.last().copied());
        for (index, &expected) in values.iter().enumerate() {
            assert_eq!(encoded.select(index), Some(expected));
            assert_eq!(encoded.get(index), Some(expected));
        }
        assert_eq!(encoded.select(values.len()), None);

        let mut probes = vec![0, 1, 2, 3, 7, 15, 31, u64::MAX - 1, u64::MAX];
        for &value in values {
            probes.push(value);
            if let Some(before) = value.checked_sub(1) {
                probes.push(before);
            }
            if let Some(after) = value.checked_add(1) {
                probes.push(after);
            }
        }
        probes.sort_unstable();
        probes.dedup();

        for probe in probes {
            let rank_le = values.partition_point(|&value| value <= probe);
            let rank_lt = values.partition_point(|&value| value < probe);
            assert_eq!(encoded.rank_le(probe), rank_le, "rank_le({probe})");
            assert_eq!(encoded.rank_lt(probe), rank_lt, "rank_lt({probe})");
            assert_eq!(
                encoded.rank_le(probe),
                encoded.partition_point(|value| value <= probe),
                "select-based rank_le({probe})"
            );
            assert_eq!(
                encoded.rank_lt(probe),
                encoded.partition_point(|value| value < probe),
                "select-based rank_lt({probe})"
            );
            assert_eq!(
                encoded.predecessor(probe),
                values[..rank_le].last().copied(),
                "predecessor({probe})"
            );
            assert_eq!(
                encoded.successor(probe),
                values.get(rank_lt).copied(),
                "successor({probe})"
            );
        }
    }

    #[test]
    fn empty_duplicates_zero_and_max_are_exact() {
        for values in [
            Vec::new(),
            vec![0],
            vec![0, 0, 0, 1, 1],
            vec![u64::MAX],
            vec![0, u64::MAX],
            vec![u64::MAX, u64::MAX, u64::MAX],
        ] {
            assert_matches_naive(&values);
        }
    }

    #[test]
    fn construction_rejects_limit_and_order_before_allocation() {
        assert_eq!(
            EliasFano::try_new(&[1, 2], EntryLimit::new(1)),
            Err(EliasFanoError::EntryLimitExceeded {
                entries: 2,
                limit: 1,
            })
        );
        assert_eq!(
            EliasFano::try_new(&[1, 3, 2, 4], EntryLimit::new(4)),
            Err(EliasFanoError::NotMonotone {
                index: 2,
                previous: 3,
                current: 2,
            })
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn entry_count_preflight_rejects_rank_overflow_without_a_value_slice() {
        let entries = u32::MAX as usize + 1;
        assert_eq!(
            preflight_entry_count(entries),
            Err(EliasFanoError::SizeOverflow {
                calculation: SizeCalculation::HighWordRank,
            })
        );
    }

    #[test]
    fn canonical_low_width_and_sparse_universe_stay_linear() {
        let singleton = EliasFano::try_new(&[u64::MAX], EntryLimit::new(1)).unwrap();
        assert_eq!(singleton.low_bits(), 63);
        assert_eq!(singleton.high_bit_len(), 2);
        assert!(singleton.logical_storage_words() <= 3);

        let extremes = EliasFano::try_new(&[0, u64::MAX], EntryLimit::new(2)).unwrap();
        assert_eq!(extremes.low_bits(), 62);
        assert_eq!(extremes.high_bit_len(), 5);
        assert!(extremes.logical_storage_words() <= 5);
        assert_eq!(extremes.select(1), Some(u64::MAX));
    }

    #[test]
    fn sequential_cursor_is_bounded_exact_and_scans_each_high_word_once() {
        let values = (0_u64..4_097)
            .map(|index| index * 1_000_003 + (index % 17))
            .collect::<Vec<_>>();
        let encoded =
            EliasFano::try_new(&values, EntryLimit::new(values.len())).expect("valid fixture");
        let mut cursor = encoded.sequential_cursor();
        let mut empty = [];
        assert_eq!(cursor.fill(&mut empty), 0);
        assert_eq!(cursor.len(), values.len());

        let mut offset = 0_usize;
        let mut scratch = [u64::MAX; 37];
        while offset < values.len() {
            let count = cursor.fill(&mut scratch);
            assert!(count > 0);
            assert!(count <= scratch.len());
            assert_eq!(&scratch[..count], &values[offset..offset + count]);
            offset += count;
            assert_eq!(cursor.len(), values.len() - offset);
        }

        assert_eq!(cursor.next(), None);
        assert_eq!(cursor.fill(&mut scratch), 0);
        assert_eq!(cursor.high_words_examined(), encoded.high_words.len());
    }

    #[test]
    fn sequential_cursor_preserves_empty_duplicate_and_full_width_values() {
        for values in [
            Vec::new(),
            vec![0],
            vec![0, 0, 0, 1, 1],
            vec![0, 1_u64 << 63, u64::MAX],
            vec![u64::MAX, u64::MAX, u64::MAX],
        ] {
            let encoded =
                EliasFano::try_new(&values, EntryLimit::new(values.len())).expect("valid fixture");
            assert_eq!(encoded.sequential_cursor().collect::<Vec<_>>(), values);
        }
    }

    #[test]
    fn sequential_seek_matches_every_suffix_across_words_and_duplicate_runs() {
        let fixtures = [
            Vec::new(),
            vec![0; 129],
            vec![u64::MAX; 129],
            (0..513).map(|i| (i / 3) * 1_000_003).collect(),
            (0..257).map(|i| if i < 2 { 0 } else { u64::MAX }).collect(),
            (0..193).map(|i| u64::MAX - 192 + i).collect(),
        ];
        for values in fixtures {
            let encoded = EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();
            for start in (0..=values.len() + 1).chain([usize::MAX]) {
                let mut cursor = encoded.sequential_cursor_from(start);
                let expected = &values[start.min(values.len())..];
                assert_eq!(cursor.len(), expected.len());
                let mut actual = Vec::new();
                let mut scratch = [0; 17];
                assert_eq!(cursor.fill(&mut []), 0);
                loop {
                    let written = cursor.fill(&mut scratch);
                    if written == 0 {
                        break;
                    }
                    actual.extend_from_slice(&scratch[..written]);
                    assert_eq!(cursor.len(), expected.len() - actual.len());
                }
                assert_eq!(actual, expected, "start={start}");
                assert_eq!(cursor.next(), None);
                assert_eq!(cursor.next(), None);
                if start < values.len() {
                    let first_word = if start == 0 {
                        0
                    } else {
                        encoded.select_high_position(start).unwrap() / 64
                    };
                    assert!(
                        cursor.high_words_examined() <= encoded.high_words.len() - first_word,
                        "a seek must not rescan the preceding high words"
                    );
                }
            }
        }
    }

    #[test]
    fn sequential_sparse_gaps_use_logarithmic_directory_work() {
        for zeros in [0_usize, 17] {
            let mut values = vec![0; zeros];
            values.resize(65_537, u64::MAX);
            let encoded = EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();
            let mut cursor = encoded.sequential_cursor();
            let prefix = cursor.by_ref().take(zeros + 1).collect::<Vec<_>>();
            assert_eq!(prefix, values[..zeros + 1]);
            assert!(cursor.high_words_examined <= 3);
            assert!(cursor.high_gap_probes > 0);
            assert!(
                cursor.high_gap_probes <= 2 * encoded.high_words.len().ilog2() as usize + 3,
                "one sparse incidence must not scan every intervening word"
            );
            assert_eq!(cursor.collect::<Vec<_>>(), values[zeros + 1..]);
        }
    }

    fn enumerate_sequences(prefix: &mut Vec<u64>, minimum: u64, remaining: usize) {
        assert_matches_naive(prefix);
        if remaining == 0 {
            return;
        }
        for next in minimum..=6 {
            prefix.push(next);
            enumerate_sequences(prefix, next, remaining - 1);
            let popped = prefix.pop();
            assert_eq!(popped, Some(next));
        }
    }

    #[test]
    fn exhaustive_small_universe_matches_naive_rank_and_select() {
        enumerate_sequences(&mut Vec::new(), 0, 5);
    }

    #[test]
    fn deterministic_random_and_adversarial_gaps_match_naive() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for round in 0..128_usize {
            let count = 1 + (round * 37 % 513);
            let mut values = Vec::with_capacity(count);
            let mut current = if round % 7 == 0 { u64::MAX - 4_096 } else { 0 };
            for index in 0..count {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let gap = match index % 11 {
                    0 => 0,
                    1 => 1,
                    2 => state & 0xff,
                    3 => state & 0xffff,
                    4 if round % 5 == 0 => 1_u64 << 40,
                    _ => state & 0x0fff,
                };
                current = current.saturating_add(gap);
                values.push(current);
            }
            assert_matches_naive(&values);
        }
    }

    #[test]
    fn select_directory_crosses_word_and_rank_boundaries() {
        let values: Vec<u64> = (0..1_025_u64)
            .map(|index| index.saturating_mul(index / 3 + 1))
            .collect();
        assert_matches_naive(&values);
        let encoded = EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();
        for index in [0, 1, 63, 64, 127, 255, 256, 257, 511, 512, 1_024] {
            assert_eq!(encoded.select(index), values.get(index).copied());
        }
    }

    #[test]
    fn select_directory_binary_searches_adversarial_zero_word_gap() {
        let mut values = vec![u64::MAX; 4_097];
        values[0] = 0;
        let encoded = EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();

        let zero_word_gap = encoded
            .high_words
            .iter()
            .skip(1)
            .take_while(|&&word| word == 0)
            .count();
        assert!(
            zero_word_gap >= 100,
            "fixture must retain an adversarial gap"
        );

        let probes = encoded
            .select_directory_probes(1)
            .expect("second value has a select position");
        assert_eq!(encoded.select(0), Some(0));
        assert_eq!(encoded.select(1), Some(u64::MAX));
        assert_eq!(encoded.select(values.len() - 1), Some(u64::MAX));
        assert!(
            probes <= 9,
            "binary search over {} high words used {probes} probes",
            encoded.high_words.len()
        );
        assert!(
            probes < zero_word_gap,
            "select must not inspect each of the {zero_word_gap} zero words"
        );
    }

    #[test]
    fn rank_zero_separators_cross_words_without_counting_final_padding() {
        for count in [0, 1, 58, 59, 60, 62, 63, 64, 65, 127, 128, 129] {
            for tail in [1, 2, 65] {
                let mut values = vec![0; count];
                values.extend(std::iter::repeat_n(2, tail));
                values.extend([5, 5]);
                assert_matches_naive(&values);
                let encoded = EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();
                let maximum_high = encoded.high_bit_len - encoded.len;
                for high in (0..=maximum_high + 1).chain([usize::MAX]) {
                    let expected =
                        values.partition_point(|value| (value >> encoded.low_bits) < high as u64);
                    assert_eq!(encoded.high_bucket_start(high).0, expected);
                }
            }
        }
        // A separator immediately after a complete unary word and a final
        // word with exactly 64 valid bits exercise both sides of the cap.
        for count in [58, 59, 60, 63, 64, 65] {
            let mut values = vec![0; count];
            values.push(5);
            assert_matches_naive(&values);
        }
    }

    #[test]
    fn rank_searches_all_low_widths_and_packed_field_boundaries() {
        for width in 0..=63 {
            let count = (u64::MAX >> width).min(257) as usize;
            let maximum = (count as u64) << width;
            let mut values = (0..count)
                .map(|index| {
                    ((index as u128 * maximum as u128) / count.saturating_sub(1).max(1) as u128)
                        as u64
                })
                .collect::<Vec<_>>();
            *values.last_mut().unwrap() = maximum;
            let encoded = EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();
            assert_eq!(encoded.low_bits, width as u8);
            assert_matches_naive(&values);
        }
    }

    #[test]
    fn rank_large_duplicates_and_sparse_extremes_match_both_oracles() {
        let count = 65_537;
        let mut gap = vec![u64::MAX; count];
        gap[..65].fill(0);
        for values in [
            (0..count).map(|index| index as u64 * 17).collect(),
            (0..count)
                .map(|index| (index / 257) as u64 * 1_000_000_007)
                .collect(),
            (0..count)
                .map(|index| u64::MAX - (count - 1 - index) as u64)
                .collect(),
            gap,
            vec![0; count],
        ] {
            let encoded = EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();
            for index in (0..count).step_by(17).chain([count - 1]) {
                let at = values[index];
                for probe in [0, at.saturating_sub(1), at, at.saturating_add(1), u64::MAX] {
                    for inclusive in [false, true] {
                        let predicate = |value: u64| value < probe || (inclusive && value == probe);
                        let expected = values.partition_point(|&value| predicate(value));
                        assert_eq!(encoded.search_rank(probe, inclusive).0, expected);
                        assert_eq!(encoded.partition_point(predicate), expected);
                    }
                }
            }
        }
    }

    fn scalar_rank_directory_probes(encoded: &EliasFano, value: u64) -> (usize, usize) {
        let mut left = 0;
        let mut right = encoded.len;
        let mut probes = 0;
        while left < right {
            let middle = left + (right - left) / 2;
            probes += encoded.select_directory_probes(middle).unwrap();
            if encoded.select(middle).unwrap() <= value {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        (left, probes)
    }

    #[test]
    fn rank_directory_work_has_no_nested_select_search() {
        let values = (0..=65_536).map(|index| index * 8).collect::<Vec<_>>();
        let encoded = EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();
        let search_bound = usize::BITS - encoded.high_word_ranks.len().leading_zeros();
        let mut old_probes = 0;
        let mut new_probes = 0;
        for index in (1..values.len() - 1).step_by(131) {
            let probe = values[index] + 1;
            let (actual, probes) = encoded.search_rank(probe, true);
            let (expected, scalar_probes) = scalar_rank_directory_probes(&encoded, probe);
            assert_eq!(actual, expected);
            assert!(probes <= 2 * search_bound as usize);
            old_probes += scalar_probes;
            new_probes += probes;
        }
        assert!(old_probes > 5 * new_probes);

        let mut sparse = vec![u64::MAX; 65_537];
        sparse[..17].fill(0);
        let encoded = EliasFano::try_new(&sparse, EntryLimit::new(sparse.len())).unwrap();
        let search_bound = usize::BITS - encoded.high_word_ranks.len().leading_zeros();
        for probe in [1, 1 << 32, 1 << 63, u64::MAX - 1] {
            let (actual, probes) = encoded.search_rank(probe, true);
            assert_eq!(actual, 17);
            assert!(probes <= 2 * search_bound as usize);
        }
    }

    // Incoming rows are a permutation of outgoing CSR positions. Retain only
    // positions that miss the real cursor's same/adjacent-source shortcut, so
    // this workload measures the actual offset-rank fallback, not imaginary
    // repeated work on every incidence of a hub.
    fn csr_rank_fixture(hub: bool) -> (Vec<u64>, Vec<u64>) {
        let nodes = 8_192usize;
        let hubs = [3, 1_031, 2_053, 4_099, 6_143, 7_169, 8_191];
        let mut offsets = vec![0];
        let mut incoming = Vec::new();
        for source in 0..nodes {
            let mut targets = if hub && hubs.contains(&source) {
                (0..nodes)
                    .filter(|&target| target != source)
                    .collect::<Vec<_>>()
            } else if hub {
                vec![(source + 1) % nodes]
            } else {
                [1, 2, 3, 7, 17, 65, 257, 1_025]
                    .map(|offset| (source + offset) % nodes)
                    .to_vec()
            };
            targets.sort_unstable();
            for target in targets {
                incoming.push((target, source, incoming.len() as u64));
            }
            offsets.push(incoming.len() as u64);
        }
        incoming.sort_unstable();
        let mut probes = Vec::new();
        let mut previous = None;
        for (target, source, position) in incoming {
            if previous.is_none_or(|(prior_target, prior_source)| {
                target != prior_target || (source != prior_source && source != prior_source + 1)
            }) {
                probes.push(position);
            }
            previous = Some((target, source));
        }
        (offsets, probes)
    }

    fn resolve_csr_positions(encoded: &EliasFano, probes: &[u64], scalar: bool) -> u64 {
        probes.iter().fold(0u64, |checksum, &position| {
            let position = std::hint::black_box(position);
            let rank = if scalar {
                encoded.partition_point(|candidate| candidate <= position)
            } else {
                encoded.rank_le(position)
            };
            let source = rank - 1;
            let local = position - encoded.select(source).unwrap();
            checksum
                .wrapping_mul(1_000_003)
                .wrapping_add(source as u64)
                .wrapping_add(local)
        })
    }

    #[test]
    fn rank_csr_sparse_and_hub_positions_resolve_identical_sources_and_offsets() {
        for hub in [false, true] {
            let (offsets, probes) = csr_rank_fixture(hub);
            assert!((65_000..=66_000).contains(offsets.last().unwrap()));
            assert!(probes.len() > 32_000);
            let encoded = EliasFano::try_new(&offsets, EntryLimit::new(offsets.len())).unwrap();
            for &position in &probes {
                let rank = offsets.partition_point(|&offset| offset <= position);
                assert_eq!(encoded.rank_le(position), rank);
                assert_eq!(encoded.select(rank - 1), Some(offsets[rank - 1]));
            }
            assert_eq!(
                resolve_csr_positions(&encoded, &probes, false),
                resolve_csr_positions(&encoded, &probes, true)
            );
        }
    }

    #[test]
    #[ignore = "manual paired compressed CSR offset-rank measurement"]
    fn rank_bucket_lookup_paired_csr_offsets_measurement() {
        for hub in [false, true] {
            let (offsets, probes) = csr_rank_fixture(hub);
            let encoded = EliasFano::try_new(&offsets, EntryLimit::new(offsets.len())).unwrap();
            let expected = resolve_csr_positions(&encoded, &probes, true);
            assert_eq!(resolve_csr_positions(&encoded, &probes, false), expected);
            let mut scalar_ns = Vec::new();
            let mut bucket_ns = Vec::new();
            for trial in 0..5 {
                for scalar in [trial % 2 == 0, trial % 2 != 0] {
                    let started = std::time::Instant::now();
                    let checksum = resolve_csr_positions(
                        std::hint::black_box(&encoded),
                        std::hint::black_box(&probes),
                        scalar,
                    );
                    let elapsed = started.elapsed().as_nanos();
                    std::hint::black_box(checksum);
                    assert_eq!(checksum, expected);
                    if scalar {
                        scalar_ns.push(elapsed);
                    } else {
                        bucket_ns.push(elapsed);
                    }
                }
            }
            println!(
                "ef_csr_rank hub={hub} rows={} incidences={} rank_fallbacks={} \
                 scalar_ns={scalar_ns:?} bucket_ns={bucket_ns:?} checksum={expected}",
                offsets.len() - 1,
                offsets.last().unwrap(),
                probes.len(),
            );
            scalar_ns.sort_unstable();
            bucket_ns.sort_unstable();
            println!(
                "ef_csr_rank hub={hub} scalar_median_ns={} bucket_median_ns={} \
                 scope=offset_rank_and_select_only",
                scalar_ns[scalar_ns.len() / 2],
                bucket_ns[bucket_ns.len() / 2],
            );
        }
    }
}
