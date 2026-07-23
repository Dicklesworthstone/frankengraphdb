//! Elias-Fano coding for nondecreasing `u64` sequences.
//!
//! This module owns the safe scalar mechanics only. It deliberately exposes no
//! durable byte format: registered format rows and generated readers will own
//! framing, versioning, and per-kind limits. Construction requires an explicit
//! [`EntryLimit`] and uses checked arithmetic before every internal allocation.
//! Sparse samples avoid rescanning earlier one-bits, but this first scalar
//! kernel does not yet carry the word/superblock directory needed to bound
//! zero-word traversal across adversarial gaps; it therefore makes no O(1)
//! rank/select claim.

#![forbid(unsafe_code)]

use core::fmt;

const SELECT_SAMPLE_RATE: usize = 256;

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
    /// Sparse positions used to avoid rescanning earlier one-bits.
    SelectSamples,
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
    /// Computing the sparse select-directory length.
    SelectSampleCount,
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
    select_samples: Vec<usize>,
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
                select_samples: Vec::new(),
            });
        }

        let len_u64 = u64::try_from(values.len()).map_err(|_| EliasFanoError::SizeOverflow {
            calculation: SizeCalculation::EntryCount,
        })?;
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
        let sample_count = values.len().checked_add(SELECT_SAMPLE_RATE - 1).ok_or(
            EliasFanoError::SizeOverflow {
                calculation: SizeCalculation::SelectSampleCount,
            },
        )? / SELECT_SAMPLE_RATE;

        let mut low_words = allocate_zeroed(low_word_count, AllocationTarget::LowBits)?;
        let mut high_words = allocate_zeroed(high_word_count, AllocationTarget::HighBits)?;
        let mut select_samples = Vec::new();
        select_samples
            .try_reserve_exact(sample_count)
            .map_err(|_| EliasFanoError::AllocationFailed {
                target: AllocationTarget::SelectSamples,
                requested: sample_count,
            })?;

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
            if index % SELECT_SAMPLE_RATE == 0 {
                select_samples.push(high_position);
            }
        }

        debug_assert_eq!(select_samples.len(), sample_count);
        Ok(Self {
            len: values.len(),
            max_value,
            low_bits,
            low_words,
            high_words,
            high_bit_len,
            select_samples,
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

    /// Total packed words, including the sparse select directory.
    ///
    /// This is an accounting aid for tests and resource estimators, not a
    /// durable encoding size.
    #[must_use]
    pub const fn storage_words(&self) -> usize {
        self.low_words.len() + self.high_words.len() + self.select_samples.len()
    }

    /// Logical number of bits in the unary high vector, excluding word padding.
    #[must_use]
    pub const fn high_bit_len(&self) -> usize {
        self.high_bit_len
    }

    /// Returns the value at `index`, or `None` when out of bounds.
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
    #[must_use]
    pub fn rank_le(&self, value: u64) -> usize {
        self.partition_point(|candidate| candidate <= value)
    }

    /// Counts represented values strictly less than `value`.
    #[must_use]
    pub fn rank_lt(&self, value: u64) -> usize {
        self.partition_point(|candidate| candidate < value)
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
        let sample_index = index / SELECT_SAMPLE_RATE;
        let sample_value_index = sample_index * SELECT_SAMPLE_RATE;
        let sample_position = *self.select_samples.get(sample_index)?;
        let mut remaining = index - sample_value_index;
        if remaining == 0 {
            return Some(sample_position);
        }

        let mut word_index = sample_position / 64;
        let sample_bit = sample_position % 64;
        let mut bits = if sample_bit == 63 {
            0
        } else {
            self.high_words[word_index] & (!0_u64 << (sample_bit + 1))
        };

        loop {
            let ones = bits.count_ones() as usize;
            if remaining <= ones {
                let within_word = select_one(bits, remaining - 1)?;
                return word_index
                    .checked_mul(64)
                    .and_then(|base| base.checked_add(within_word));
            }
            remaining -= ones;
            word_index = word_index.checked_add(1)?;
            bits = *self.high_words.get(word_index)?;
        }
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

fn words_for_bits(bits: usize) -> Option<usize> {
    let complete = bits / 64;
    complete.checked_add(usize::from(!bits.is_multiple_of(64)))
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

    #[test]
    fn canonical_low_width_and_sparse_universe_stay_linear() {
        let singleton = EliasFano::try_new(&[u64::MAX], EntryLimit::new(1)).unwrap();
        assert_eq!(singleton.low_bits(), 63);
        assert_eq!(singleton.high_bit_len(), 2);
        assert!(singleton.storage_words() <= 3);

        let extremes = EliasFano::try_new(&[0, u64::MAX], EntryLimit::new(2)).unwrap();
        assert_eq!(extremes.low_bits(), 62);
        assert_eq!(extremes.high_bit_len(), 5);
        assert!(extremes.storage_words() <= 5);
        assert_eq!(extremes.select(1), Some(u64::MAX));
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
    fn select_directory_crosses_word_and_sample_boundaries() {
        let values: Vec<u64> = (0..1_025_u64)
            .map(|index| index.saturating_mul(index / 3 + 1))
            .collect();
        assert_matches_naive(&values);
        let encoded = EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();
        for index in [0, 1, 63, 64, 127, 255, 256, 257, 511, 512, 1_024] {
            assert_eq!(encoded.select(index), values.get(index).copied());
        }
    }
}
