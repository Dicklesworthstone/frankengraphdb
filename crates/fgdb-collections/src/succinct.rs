//! Immutable scalar bitvectors with rank and bounded-local select.
//!
//! Bits are numbered from the least-significant bit of word zero. Construction
//! accepts an explicit logical bit length and rejects both a non-exact word
//! count and non-zero padding in the final word. Consequently, equal logical
//! vectors always have equal [`SuccinctBitVector::as_words`] values.
//!
//! Rank uses a two-level directory:
//!
//! - one cumulative `usize` count per eight-word (512-bit) superblock, and
//! - one `u16` count per word relative to its superblock.
//!
//! Thus `rank1` and `rank0` perform a fixed number of directory accesses and
//! one population count: `O(1)`.
//!
//! Select deliberately makes a weaker, honest claim. It binary-searches the
//! superblock directory and then examines at most eight words. Its bound is
//! `O(log ceil(bit_len / 512) + 8 + 63)` scalar primitive steps; the final
//! `63` is the maximum number of low set bits cleared inside the selected
//! word. No query falls back to scanning the complete bitvector.
//!
//! This is an in-memory representation, not a durable encoding.

#![forbid(unsafe_code)]

use core::fmt;
use core::iter::FusedIterator;
use core::mem::size_of;

const WORD_BITS: usize = u64::BITS as usize;
const SUPERBLOCK_WORDS: usize = 8;
const SUPERBLOCK_BITS: usize = SUPERBLOCK_WORDS * WORD_BITS;

/// Internal allocation named by a fallible construction error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AllocationTarget {
    /// Canonical bit words built by [`SuccinctBitVector::try_from_bits`].
    Words,
    /// Cumulative counts at 512-bit boundaries.
    RankSuperblocks,
    /// Counts at 64-bit boundaries relative to a superblock.
    RankSubblocks,
}

/// Typed failure from succinct bitvector construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BitVectorError {
    /// The supplied word count is not exactly `ceil(bit_len / 64)`.
    WordCountMismatch {
        /// Explicit logical bit length.
        bit_len: usize,
        /// Canonical number of words for `bit_len`.
        expected_words: usize,
        /// Number of supplied words.
        actual_words: usize,
    },
    /// Bits outside the explicit logical length were set.
    NonZeroPadding {
        /// Explicit logical bit length.
        bit_len: usize,
        /// Index of the rejected final word.
        word_index: usize,
        /// The complete rejected final word.
        word: u64,
        /// Exactly the non-zero bits outside `bit_len`.
        non_zero_padding: u64,
    },
    /// Directory-size arithmetic exceeded `usize`.
    SizeOverflow {
        /// Component whose element count could not be represented.
        target: AllocationTarget,
    },
    /// Reserving one representation component failed before publication.
    AllocationFailed {
        /// Component that could not be reserved.
        target: AllocationTarget,
        /// Exact number of elements requested for that component.
        requested: usize,
    },
}

impl fmt::Display for BitVectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::WordCountMismatch {
                bit_len,
                expected_words,
                actual_words,
            } => write!(
                formatter,
                "bit length {bit_len} requires exactly {expected_words} words, got {actual_words}"
            ),
            Self::NonZeroPadding {
                bit_len,
                word_index,
                non_zero_padding,
                ..
            } => write!(
                formatter,
                "word {word_index} has non-zero padding {non_zero_padding:#018x} outside bit length {bit_len}"
            ),
            Self::SizeOverflow { target } => {
                write!(formatter, "{target:?} directory size overflowed")
            }
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "could not reserve {requested} elements for {target:?}"
            ),
        }
    }
}

impl std::error::Error for BitVectorError {}

/// Exact byte accounting for the vector's three heap-owned arrays.
///
/// Values use logical element widths and exact array lengths. They exclude the
/// inline `SuccinctBitVector` fields and allocator bookkeeping/rounding, neither
/// of which is part of the represented collection storage.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct StorageBreakdown {
    /// Canonical `u64` bit-word bytes, including zero final-word padding.
    pub word_bytes: usize,
    /// Cumulative 512-bit rank-directory bytes.
    pub rank_superblock_bytes: usize,
    /// Relative 64-bit rank-directory bytes.
    pub rank_subblock_bytes: usize,
}

impl StorageBreakdown {
    /// Exact sum of all heap-owned representation arrays.
    #[must_use]
    pub const fn total_bytes(self) -> usize {
        self.word_bytes + self.rank_superblock_bytes + self.rank_subblock_bytes
    }
}

/// Immutable canonical bitvector with scalar rank/select support.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SuccinctBitVector {
    bit_len: usize,
    words: Vec<u64>,
    rank_superblocks: Vec<usize>,
    rank_subblocks: Vec<u16>,
    one_count: usize,
}

impl SuccinctBitVector {
    /// Constructs a vector from canonical little-bit-endian words.
    ///
    /// `words.len()` must equal `ceil(bit_len / 64)`. When the final word is
    /// partial, every bit at an index greater than or equal to `bit_len` must
    /// be zero. Validation completes before directory allocation begins.
    pub fn try_from_words(bit_len: usize, words: Vec<u64>) -> Result<Self, BitVectorError> {
        let expected_words = word_count(bit_len);
        if words.len() != expected_words {
            return Err(BitVectorError::WordCountMismatch {
                bit_len,
                expected_words,
                actual_words: words.len(),
            });
        }

        validate_padding(bit_len, &words)?;

        let superblock_count = words.len().div_ceil(SUPERBLOCK_WORDS);
        let superblock_entries =
            superblock_count
                .checked_add(1)
                .ok_or(BitVectorError::SizeOverflow {
                    target: AllocationTarget::RankSuperblocks,
                })?;

        let mut rank_superblocks = Vec::new();
        reserve_exact(
            &mut rank_superblocks,
            superblock_entries,
            AllocationTarget::RankSuperblocks,
        )?;

        let mut rank_subblocks = Vec::new();
        reserve_exact(
            &mut rank_subblocks,
            words.len(),
            AllocationTarget::RankSubblocks,
        )?;

        let mut total = 0_usize;
        let mut within_superblock = 0_u16;
        for (word_index, &word) in words.iter().enumerate() {
            if word_index % SUPERBLOCK_WORDS == 0 {
                rank_superblocks.push(total);
                within_superblock = 0;
            }
            rank_subblocks.push(within_superblock);
            let word_ones = word.count_ones() as u16;
            within_superblock += word_ones;
            total += usize::from(word_ones);
        }
        rank_superblocks.push(total);

        debug_assert_eq!(rank_superblocks.len(), superblock_entries);
        debug_assert_eq!(rank_subblocks.len(), words.len());

        Ok(Self {
            bit_len,
            words,
            rank_superblocks,
            rank_subblocks,
            one_count: total,
        })
    }

    /// Constructs a canonical vector from a logical bit slice.
    ///
    /// Word storage and both rank-directory allocations are fallible. The
    /// resulting final-word padding is zero by construction.
    pub fn try_from_bits(bits: &[bool]) -> Result<Self, BitVectorError> {
        let words_needed = word_count(bits.len());
        let mut words = Vec::new();
        reserve_exact(&mut words, words_needed, AllocationTarget::Words)?;
        words.resize(words_needed, 0);

        for (bit_index, &bit) in bits.iter().enumerate() {
            if bit {
                words[bit_index / WORD_BITS] |= 1_u64 << (bit_index % WORD_BITS);
            }
        }

        Self::try_from_words(bits.len(), words)
    }

    /// Explicit logical length in bits.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.bit_len
    }

    /// Whether the vector contains no logical bits.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bit_len == 0
    }

    /// Canonical little-bit-endian words.
    ///
    /// The last word has zero bits outside [`Self::len`].
    #[must_use]
    pub fn as_words(&self) -> &[u64] {
        &self.words
    }

    /// Returns the bit at `index`, or `None` when `index >= len`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<bool> {
        if index >= self.bit_len {
            return None;
        }
        let word = self.words[index / WORD_BITS];
        Some(word & (1_u64 << (index % WORD_BITS)) != 0)
    }

    /// Number of one bits in the complete logical vector.
    #[must_use]
    pub const fn count_ones(&self) -> usize {
        self.one_count
    }

    /// Number of zero bits in the complete logical vector.
    #[must_use]
    pub const fn count_zeros(&self) -> usize {
        self.bit_len - self.one_count
    }

    /// Counts one bits in the half-open prefix `[0, end)`.
    ///
    /// This is `O(1)`. Returns `None` when `end > len`; `end == len` is valid.
    #[must_use]
    pub fn rank1(&self, end: usize) -> Option<usize> {
        if end > self.bit_len {
            return None;
        }
        if end == self.bit_len {
            return Some(self.one_count);
        }

        let word_index = end / WORD_BITS;
        let bit_in_word = end % WORD_BITS;
        let superblock_index = word_index / SUPERBLOCK_WORDS;
        let preceding =
            self.rank_superblocks[superblock_index] + usize::from(self.rank_subblocks[word_index]);
        let word_prefix = self.words[word_index] & low_mask(bit_in_word);
        Some(preceding + word_prefix.count_ones() as usize)
    }

    /// Counts zero bits in the half-open prefix `[0, end)`.
    ///
    /// This is `O(1)`. Returns `None` when `end > len`; `end == len` is valid.
    #[must_use]
    pub fn rank0(&self, end: usize) -> Option<usize> {
        self.rank1(end).map(|ones| end - ones)
    }

    /// Returns the position of the zero-based `ordinal`th one bit.
    ///
    /// The query binary-searches cumulative superblock counts, scans no more
    /// than eight words, and clears no more than 63 low set bits in the
    /// selected word. See the module-level complexity contract.
    #[must_use]
    pub fn select1(&self, ordinal: usize) -> Option<usize> {
        if ordinal >= self.one_count {
            return None;
        }

        let superblock_index = self
            .rank_superblocks
            .partition_point(|&prefix| prefix <= ordinal)
            - 1;
        let mut within = ordinal - self.rank_superblocks[superblock_index];
        let word_start = superblock_index * SUPERBLOCK_WORDS;
        let word_end = (word_start + SUPERBLOCK_WORDS).min(self.words.len());

        for word_index in word_start..word_end {
            let word = self.words[word_index];
            let count = word.count_ones() as usize;
            if within < count {
                let bit = select_set_bit(word, within);
                return word_index
                    .checked_mul(WORD_BITS)
                    .and_then(|base| base.checked_add(bit));
            }
            within -= count;
        }

        debug_assert!(false, "rank directory must locate a one-containing block");
        None
    }

    /// Returns the position of the zero-based `ordinal`th zero bit.
    ///
    /// The query binary-searches zero counts derived from the same superblock
    /// directory, scans no more than eight words, and ignores final padding.
    /// See the module-level complexity contract.
    #[must_use]
    pub fn select0(&self, ordinal: usize) -> Option<usize> {
        if ordinal >= self.count_zeros() {
            return None;
        }

        let superblock_count = self.rank_superblocks.len() - 1;
        let mut left = 0_usize;
        let mut right = superblock_count + 1;
        while left < right {
            let middle = left + (right - left) / 2;
            if self.zeros_before_superblock(middle) <= ordinal {
                left = middle + 1;
            } else {
                right = middle;
            }
        }
        let superblock_index = left - 1;
        let mut within = ordinal - self.zeros_before_superblock(superblock_index);
        let word_start = superblock_index * SUPERBLOCK_WORDS;
        let word_end = (word_start + SUPERBLOCK_WORDS).min(self.words.len());

        for word_index in word_start..word_end {
            let valid_bits = self.valid_bits_in_word(word_index);
            let zero_word = !self.words[word_index] & low_mask(valid_bits);
            let count = zero_word.count_ones() as usize;
            if within < count {
                let bit = select_set_bit(zero_word, within);
                return word_index
                    .checked_mul(WORD_BITS)
                    .and_then(|base| base.checked_add(bit));
            }
            within -= count;
        }

        debug_assert!(false, "rank directory must locate a zero-containing block");
        None
    }

    /// Iterates all one-bit positions in strictly increasing order.
    #[must_use]
    pub fn iter_ones(&self) -> Ones<'_> {
        Ones {
            words: &self.words,
            next_word_index: 0,
            current_word_index: 0,
            remaining_word: 0,
            remaining: self.one_count,
        }
    }

    /// Exact byte accounting for heap-owned representation arrays.
    #[must_use]
    pub fn storage_breakdown(&self) -> StorageBreakdown {
        StorageBreakdown {
            word_bytes: self.words.len() * size_of::<u64>(),
            rank_superblock_bytes: self.rank_superblocks.len() * size_of::<usize>(),
            rank_subblock_bytes: self.rank_subblocks.len() * size_of::<u16>(),
        }
    }

    /// Exact total bytes in the heap-owned representation arrays.
    ///
    /// This is the sum returned by [`StorageBreakdown::total_bytes`].
    #[must_use]
    pub fn logical_storage_bytes(&self) -> usize {
        self.storage_breakdown().total_bytes()
    }

    fn zeros_before_superblock(&self, superblock_index: usize) -> usize {
        let bit_start = superblock_index
            .checked_mul(SUPERBLOCK_BITS)
            .unwrap_or(self.bit_len)
            .min(self.bit_len);
        bit_start - self.rank_superblocks[superblock_index]
    }

    fn valid_bits_in_word(&self, word_index: usize) -> usize {
        let bit_start = word_index.checked_mul(WORD_BITS).unwrap_or(self.bit_len);
        self.bit_len.saturating_sub(bit_start).min(WORD_BITS)
    }
}

impl<'a> IntoIterator for &'a SuccinctBitVector {
    type Item = usize;
    type IntoIter = Ones<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_ones()
    }
}

/// Allocation-free ascending iterator over one-bit positions.
#[derive(Clone, Debug)]
pub struct Ones<'a> {
    words: &'a [u64],
    next_word_index: usize,
    current_word_index: usize,
    remaining_word: u64,
    remaining: usize,
}

impl Iterator for Ones<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.remaining_word != 0 {
                let bit = self.remaining_word.trailing_zeros() as usize;
                self.remaining_word &= self.remaining_word - 1;
                self.remaining -= 1;
                return self
                    .current_word_index
                    .checked_mul(WORD_BITS)
                    .and_then(|base| base.checked_add(bit));
            }

            self.remaining_word = *self.words.get(self.next_word_index)?;
            self.current_word_index = self.next_word_index;
            self.next_word_index += 1;
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for Ones<'_> {}
impl FusedIterator for Ones<'_> {}

fn word_count(bit_len: usize) -> usize {
    bit_len / WORD_BITS + usize::from(!bit_len.is_multiple_of(WORD_BITS))
}

fn validate_padding(bit_len: usize, words: &[u64]) -> Result<(), BitVectorError> {
    let final_bits = bit_len % WORD_BITS;
    if final_bits == 0 {
        return Ok(());
    }

    let word_index = words.len() - 1;
    let word = words[word_index];
    let non_zero_padding = word & !low_mask(final_bits);
    if non_zero_padding != 0 {
        return Err(BitVectorError::NonZeroPadding {
            bit_len,
            word_index,
            word,
            non_zero_padding,
        });
    }
    Ok(())
}

fn low_mask(bits: usize) -> u64 {
    match bits {
        0 => 0,
        WORD_BITS => u64::MAX,
        _ => (1_u64 << bits) - 1,
    }
}

fn select_set_bit(mut word: u64, ordinal: usize) -> usize {
    debug_assert!(ordinal < word.count_ones() as usize);
    for _ in 0..ordinal {
        word &= word - 1;
    }
    word.trailing_zeros() as usize
}

fn reserve_exact<T>(
    values: &mut Vec<T>,
    requested: usize,
    target: AllocationTarget,
) -> Result<(), BitVectorError> {
    values
        .try_reserve_exact(requested)
        .map_err(|_| BitVectorError::AllocationFailed { target, requested })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_rejects_noncanonical_shapes_and_padding() {
        assert_eq!(
            SuccinctBitVector::try_from_words(0, vec![0]),
            Err(BitVectorError::WordCountMismatch {
                bit_len: 0,
                expected_words: 0,
                actual_words: 1,
            })
        );
        assert_eq!(
            SuccinctBitVector::try_from_words(65, vec![0]),
            Err(BitVectorError::WordCountMismatch {
                bit_len: 65,
                expected_words: 2,
                actual_words: 1,
            })
        );
        assert_eq!(
            SuccinctBitVector::try_from_words(65, vec![0, 2]),
            Err(BitVectorError::NonZeroPadding {
                bit_len: 65,
                word_index: 1,
                word: 2,
                non_zero_padding: 2,
            })
        );

        let exact = SuccinctBitVector::try_from_words(65, vec![u64::MAX, 1])
            .expect("canonical words construct");
        assert_eq!(exact.as_words(), &[u64::MAX, 1]);
        assert_eq!(exact.len(), 65);
        assert_eq!(exact.count_ones(), 65);
    }

    #[test]
    fn empty_vector_has_defined_boundaries_and_storage() {
        let vector =
            SuccinctBitVector::try_from_words(0, Vec::new()).expect("empty vector constructs");
        assert!(vector.is_empty());
        assert_eq!(vector.len(), 0);
        assert_eq!(vector.as_words(), &[]);
        assert_eq!(vector.get(0), None);
        assert_eq!(vector.rank1(0), Some(0));
        assert_eq!(vector.rank0(0), Some(0));
        assert_eq!(vector.rank1(1), None);
        assert_eq!(vector.rank0(1), None);
        assert_eq!(vector.select1(0), None);
        assert_eq!(vector.select0(0), None);
        assert_eq!(vector.iter_ones().next(), None);
        assert_eq!(
            vector.storage_breakdown(),
            StorageBreakdown {
                word_bytes: 0,
                rank_superblock_bytes: size_of::<usize>(),
                rank_subblock_bytes: 0,
            }
        );
    }

    #[test]
    fn exhaustive_small_universes_match_naive_queries() {
        for len in 0..=12_usize {
            let pattern_count = 1_usize << len;
            for pattern in 0..pattern_count {
                let bits = (0..len)
                    .map(|index| pattern & (1_usize << index) != 0)
                    .collect::<Vec<_>>();
                assert_matches_naive(&bits);
            }
        }
    }

    #[test]
    fn word_and_superblock_boundaries_match_naive_queries() {
        for len in [
            1_usize, 2, 63, 64, 65, 127, 128, 129, 511, 512, 513, 1023, 1024, 1025,
        ] {
            let patterns = [
                make_bits(len, |_: usize| false),
                make_bits(len, |_: usize| true),
                make_bits(len, |index| index % 2 == 0),
                make_bits(len, |index| index % 3 == 1),
                make_bits(len, |index| {
                    index == 0 || index + 1 == len || index % 64 == 0 || index % 64 == 63
                }),
            ];
            for bits in patterns {
                assert_matches_naive(&bits);
            }
        }
    }

    #[test]
    fn select_directory_skips_repeated_superblock_prefixes() {
        let sparse_ones = make_bits(2_049, |index| matches!(index, 0 | 1_024 | 2_048));
        let ones_vector =
            SuccinctBitVector::try_from_bits(&sparse_ones).expect("sparse ones construct");
        assert_eq!(ones_vector.select1(0), Some(0));
        assert_eq!(ones_vector.select1(1), Some(1_024));
        assert_eq!(ones_vector.select1(2), Some(2_048));

        let sparse_zeros = make_bits(2_049, |index| !matches!(index, 0 | 1_024 | 2_048));
        let zeros_vector =
            SuccinctBitVector::try_from_bits(&sparse_zeros).expect("sparse zeros construct");
        assert_eq!(zeros_vector.select0(0), Some(0));
        assert_eq!(zeros_vector.select0(1), Some(1_024));
        assert_eq!(zeros_vector.select0(2), Some(2_048));
    }

    #[test]
    fn deterministic_large_differential_matches_naive_queries() {
        for (case, len) in [257_usize, 511, 512, 513, 1_001, 4_097, 16_385]
            .into_iter()
            .enumerate()
        {
            for seed_offset in 0..4_u64 {
                let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ (case as u64) ^ (seed_offset << 32);
                let bits = make_bits(len, |index| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state.wrapping_add(index as u64).count_ones() % 5 <= 1
                });
                assert_matches_naive(&bits);
            }
        }
    }

    #[test]
    fn final_padding_never_appears_as_a_zero_or_one() {
        let vector =
            SuccinctBitVector::try_from_words(65, vec![0, 1]).expect("canonical partial word");
        assert_eq!(vector.count_ones(), 1);
        assert_eq!(vector.count_zeros(), 64);
        assert_eq!(vector.select1(0), Some(64));
        assert_eq!(vector.select1(1), None);
        assert_eq!(vector.select0(63), Some(63));
        assert_eq!(vector.select0(64), None);
        assert_eq!(vector.iter_ones().collect::<Vec<_>>(), vec![64]);
    }

    #[test]
    fn storage_accounting_uses_exact_array_lengths() {
        let vector = SuccinctBitVector::try_from_words(513, vec![0; 9]).expect("canonical vector");
        let expected = StorageBreakdown {
            word_bytes: 9 * size_of::<u64>(),
            rank_superblock_bytes: 3 * size_of::<usize>(),
            rank_subblock_bytes: 9 * size_of::<u16>(),
        };
        assert_eq!(vector.storage_breakdown(), expected);
        assert_eq!(vector.logical_storage_bytes(), expected.total_bytes());
    }

    #[test]
    fn one_iterator_is_exact_sized_and_fused() {
        let bits = make_bits(200, |index| matches!(index, 0 | 63 | 64 | 199));
        let vector = SuccinctBitVector::try_from_bits(&bits).expect("bits construct");
        let mut ones = vector.iter_ones();
        assert_eq!(ones.len(), 4);
        assert_eq!(ones.next(), Some(0));
        assert_eq!(ones.len(), 3);
        assert_eq!(ones.by_ref().collect::<Vec<_>>(), vec![63, 64, 199]);
        assert_eq!(ones.len(), 0);
        assert_eq!(ones.next(), None);
        assert_eq!(ones.next(), None);
        assert_eq!(
            (&vector).into_iter().collect::<Vec<_>>(),
            vec![0, 63, 64, 199]
        );
    }

    fn make_bits(mut len: usize, mut predicate: impl FnMut(usize) -> bool) -> Vec<bool> {
        let original_len = len;
        let mut bits = Vec::with_capacity(len);
        while len != 0 {
            let index = original_len - len;
            bits.push(predicate(index));
            len -= 1;
        }
        bits
    }

    fn assert_matches_naive(bits: &[bool]) {
        let vector = SuccinctBitVector::try_from_bits(bits).expect("bits construct");
        assert_eq!(vector.len(), bits.len());
        assert_eq!(vector.is_empty(), bits.is_empty());

        for (index, &expected) in bits.iter().enumerate() {
            assert_eq!(
                vector.get(index),
                Some(expected),
                "get mismatch at {index} for len {}",
                bits.len()
            );
        }
        assert_eq!(vector.get(bits.len()), None);

        let expected_ones = bits
            .iter()
            .enumerate()
            .filter_map(|(index, &bit)| bit.then_some(index))
            .collect::<Vec<_>>();
        let expected_zeros = bits
            .iter()
            .enumerate()
            .filter_map(|(index, &bit)| (!bit).then_some(index))
            .collect::<Vec<_>>();

        assert_eq!(vector.count_ones(), expected_ones.len());
        assert_eq!(vector.count_zeros(), expected_zeros.len());
        assert_eq!(vector.iter_ones().collect::<Vec<_>>(), expected_ones);

        for end in 0..=bits.len() {
            let naive_ones = bits[..end].iter().filter(|&&bit| bit).count();
            assert_eq!(
                vector.rank1(end),
                Some(naive_ones),
                "rank1 mismatch at {end} for len {}",
                bits.len()
            );
            assert_eq!(
                vector.rank0(end),
                Some(end - naive_ones),
                "rank0 mismatch at {end} for len {}",
                bits.len()
            );
        }
        assert_eq!(vector.rank1(bits.len().saturating_add(1)), None);
        assert_eq!(vector.rank0(bits.len().saturating_add(1)), None);

        for (ordinal, &position) in expected_ones.iter().enumerate() {
            assert_eq!(
                vector.select1(ordinal),
                Some(position),
                "select1 mismatch at {ordinal} for len {}",
                bits.len()
            );
        }
        assert_eq!(vector.select1(expected_ones.len()), None);

        for (ordinal, &position) in expected_zeros.iter().enumerate() {
            assert_eq!(
                vector.select0(ordinal),
                Some(position),
                "select0 mismatch at {ordinal} for len {}",
                bits.len()
            );
        }
        assert_eq!(vector.select0(expected_zeros.len()), None);
    }
}
