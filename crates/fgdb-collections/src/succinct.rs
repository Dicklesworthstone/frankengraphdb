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

use fgdb_types::QueryCx;
use fgdb_unsafe_arena::{RegionScope, RegionVec, RegionVecError};

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
    /// Exact byte accounting for the completed representation.
    StorageAccounting,
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
    /// Appending bits would exceed a builder's explicit maximum.
    BitLimitExceeded {
        /// Length the operation would produce.
        attempted_bits: usize,
        /// Builder maximum fixed at construction.
        max_bits: usize,
    },
    /// Computing an appended length exceeded the platform index domain.
    LengthOverflow {
        /// Builder length before the rejected operation.
        current_bits: usize,
        /// Number of bits the operation attempted to append.
        additional_bits: usize,
    },
    /// Directory-size arithmetic exceeded `usize`.
    SizeOverflow {
        /// Component whose element count could not be represented.
        target: AllocationTarget,
    },
    /// Task-local region storage refused one representation component.
    Region {
        /// Component whose storage operation was refused.
        target: AllocationTarget,
        /// Exact region-vector refusal.
        source: RegionVecError,
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
            Self::BitLimitExceeded {
                attempted_bits,
                max_bits,
            } => write!(
                formatter,
                "bitvector builder would reach {attempted_bits} bits, maximum is {max_bits}"
            ),
            Self::LengthOverflow {
                current_bits,
                additional_bits,
            } => write!(
                formatter,
                "adding {additional_bits} bits to builder length {current_bits} overflowed"
            ),
            Self::SizeOverflow { target } => {
                write!(formatter, "{target:?} representation size overflowed")
            }
            Self::Region { target, source } => {
                write!(formatter, "{target:?} region storage refused: {source}")
            }
        }
    }
}

impl std::error::Error for BitVectorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Region { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Exact byte accounting for the vector's three region-owned arrays.
///
/// Values use logical element widths and exact array lengths. They exclude the
/// inline `SuccinctBitVector` fields and allocator bookkeeping/rounding, neither
/// of which is part of the represented collection storage.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct StorageBreakdown {
    /// Canonical `u64` bit-word bytes, including zero final-word padding.
    word_bytes: usize,
    /// Cumulative 512-bit rank-directory bytes.
    rank_superblock_bytes: usize,
    /// Relative 64-bit rank-directory bytes.
    rank_subblock_bytes: usize,
    total_bytes: usize,
}

impl StorageBreakdown {
    /// Bytes in canonical bit-word elements.
    #[must_use]
    pub const fn word_bytes(self) -> usize {
        self.word_bytes
    }

    /// Bytes in cumulative superblock-directory elements.
    #[must_use]
    pub const fn rank_superblock_bytes(self) -> usize {
        self.rank_superblock_bytes
    }

    /// Bytes in relative subblock-directory elements.
    #[must_use]
    pub const fn rank_subblock_bytes(self) -> usize {
        self.rank_subblock_bytes
    }

    /// Exact sum of all region-owned representation arrays.
    #[must_use]
    pub const fn total_bytes(self) -> usize {
        self.total_bytes
    }

    fn try_from_elements(
        word_elements: usize,
        superblock_elements: usize,
        subblock_elements: usize,
    ) -> Result<Self, BitVectorError> {
        let word_bytes =
            word_elements
                .checked_mul(size_of::<u64>())
                .ok_or(BitVectorError::SizeOverflow {
                    target: AllocationTarget::StorageAccounting,
                })?;
        let rank_superblock_bytes = superblock_elements.checked_mul(size_of::<usize>()).ok_or(
            BitVectorError::SizeOverflow {
                target: AllocationTarget::StorageAccounting,
            },
        )?;
        let rank_subblock_bytes = subblock_elements.checked_mul(size_of::<u16>()).ok_or(
            BitVectorError::SizeOverflow {
                target: AllocationTarget::StorageAccounting,
            },
        )?;
        let total_bytes = word_bytes
            .checked_add(rank_superblock_bytes)
            .and_then(|total| total.checked_add(rank_subblock_bytes))
            .ok_or(BitVectorError::SizeOverflow {
                target: AllocationTarget::StorageAccounting,
            })?;
        Ok(Self {
            word_bytes,
            rank_superblock_bytes,
            rank_subblock_bytes,
            total_bytes,
        })
    }
}

/// Immutable canonical bitvector with scalar rank/select support.
#[derive(Debug)]
pub struct SuccinctBitVector<'region> {
    bit_len: usize,
    words: RegionVec<'region, u64>,
    rank_superblocks: RegionVec<'region, usize>,
    rank_subblocks: RegionVec<'region, u16>,
    one_count: usize,
    logical_storage: StorageBreakdown,
    retained_storage: StorageBreakdown,
}

impl PartialEq for SuccinctBitVector<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.bit_len == other.bit_len
            && self.words == other.words
            && self.rank_superblocks == other.rank_superblocks
            && self.rank_subblocks == other.rank_subblocks
            && self.one_count == other.one_count
    }
}

impl Eq for SuccinctBitVector<'_> {}

impl<'region> SuccinctBitVector<'region> {
    /// Constructs a vector from canonical little-bit-endian words.
    ///
    /// `words.len()` must equal `ceil(bit_len / 64)`. When the final word is
    /// partial, every bit at an index greater than or equal to `bit_len` must
    /// be zero. Validation completes before directory allocation begins.
    pub fn try_from_words(
        scope: &'region RegionScope,
        cx: &QueryCx,
        bit_len: usize,
        words: &[u64],
    ) -> Result<Self, BitVectorError> {
        let expected_words = word_count(bit_len);
        if words.len() != expected_words {
            return Err(BitVectorError::WordCountMismatch {
                bit_len,
                expected_words,
                actual_words: words.len(),
            });
        }
        validate_padding(bit_len, words)?;

        let mut regional_words =
            RegionVec::with_capacity_in(scope, cx, words.len()).map_err(|source| {
                BitVectorError::Region {
                    target: AllocationTarget::Words,
                    source,
                }
            })?;
        for &word in words {
            regional_words
                .try_push(cx, word)
                .map_err(|source| BitVectorError::Region {
                    target: AllocationTarget::Words,
                    source,
                })?;
        }
        Self::try_from_region_words(scope, cx, bit_len, regional_words)
    }

    fn try_from_region_words(
        scope: &'region RegionScope,
        cx: &QueryCx,
        bit_len: usize,
        words: RegionVec<'region, u64>,
    ) -> Result<Self, BitVectorError> {
        let expected_words = word_count(bit_len);
        if words.len() != expected_words {
            return Err(BitVectorError::WordCountMismatch {
                bit_len,
                expected_words,
                actual_words: words.len(),
            });
        }

        validate_padding(bit_len, words.as_slice())?;

        let superblock_count = words.len().div_ceil(SUPERBLOCK_WORDS);
        let superblock_entries =
            superblock_count
                .checked_add(1)
                .ok_or(BitVectorError::SizeOverflow {
                    target: AllocationTarget::RankSuperblocks,
                })?;

        let mut rank_superblocks = RegionVec::with_capacity_in(scope, cx, superblock_entries)
            .map_err(|source| BitVectorError::Region {
                target: AllocationTarget::RankSuperblocks,
                source,
            })?;

        let mut rank_subblocks =
            RegionVec::with_capacity_in(scope, cx, words.len()).map_err(|source| {
                BitVectorError::Region {
                    target: AllocationTarget::RankSubblocks,
                    source,
                }
            })?;

        let mut total = 0_usize;
        let mut within_superblock = 0_u16;
        for (word_index, &word) in words.iter().enumerate() {
            if word_index % SUPERBLOCK_WORDS == 0 {
                rank_superblocks
                    .try_push(cx, total)
                    .map_err(|source| BitVectorError::Region {
                        target: AllocationTarget::RankSuperblocks,
                        source,
                    })?;
                within_superblock = 0;
            }
            rank_subblocks
                .try_push(cx, within_superblock)
                .map_err(|source| BitVectorError::Region {
                    target: AllocationTarget::RankSubblocks,
                    source,
                })?;
            let word_ones = word.count_ones() as u16;
            within_superblock =
                within_superblock
                    .checked_add(word_ones)
                    .ok_or(BitVectorError::SizeOverflow {
                        target: AllocationTarget::RankSubblocks,
                    })?;
            total =
                total
                    .checked_add(usize::from(word_ones))
                    .ok_or(BitVectorError::SizeOverflow {
                        target: AllocationTarget::RankSuperblocks,
                    })?;
        }
        rank_superblocks
            .try_push(cx, total)
            .map_err(|source| BitVectorError::Region {
                target: AllocationTarget::RankSuperblocks,
                source,
            })?;

        debug_assert_eq!(rank_superblocks.len(), superblock_entries);
        debug_assert_eq!(rank_subblocks.len(), words.len());

        let logical_storage = StorageBreakdown::try_from_elements(
            words.len(),
            rank_superblocks.len(),
            rank_subblocks.len(),
        )?;
        let retained_storage = StorageBreakdown::try_from_elements(
            words.capacity(),
            rank_superblocks.capacity(),
            rank_subblocks.capacity(),
        )?;

        Ok(Self {
            bit_len,
            words,
            rank_superblocks,
            rank_subblocks,
            one_count: total,
            logical_storage,
            retained_storage,
        })
    }

    /// Constructs a canonical vector from a logical bit slice.
    ///
    /// Word storage and both rank-directory allocations are fallible. The
    /// resulting final-word padding is zero by construction.
    pub fn try_from_bits(
        scope: &'region RegionScope,
        cx: &QueryCx,
        bits: &[bool],
    ) -> Result<Self, BitVectorError> {
        let mut builder =
            SuccinctBitVectorBuilder::try_with_capacity_in(scope, cx, bits.len(), bits.len())?;
        builder.extend(cx, bits)?;
        builder.finish(cx)
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
        self.words.as_slice()
    }

    /// Returns the bit at `index`, or `None` when `index >= len`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<bool> {
        if index >= self.bit_len {
            return None;
        }
        let word = self.words.as_slice()[index / WORD_BITS];
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
        let preceding = self.rank_superblocks.as_slice()[superblock_index]
            + usize::from(self.rank_subblocks.as_slice()[word_index]);
        let word_prefix = self.words.as_slice()[word_index] & low_mask(bit_in_word);
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
            .as_slice()
            .partition_point(|&prefix| prefix <= ordinal)
            - 1;
        let mut within = ordinal - self.rank_superblocks.as_slice()[superblock_index];
        let word_start = superblock_index * SUPERBLOCK_WORDS;
        let word_end = (word_start + SUPERBLOCK_WORDS).min(self.words.len());

        for word_index in word_start..word_end {
            let word = self.words.as_slice()[word_index];
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
            let zero_word = !self.words.as_slice()[word_index] & low_mask(valid_bits);
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
            words: self.words.as_slice(),
            next_word_index: 0,
            current_word_index: 0,
            remaining_word: 0,
            remaining: self.one_count,
        }
    }

    /// Exact byte accounting for region-owned representation arrays.
    #[must_use]
    pub fn storage_breakdown(&self) -> StorageBreakdown {
        self.logical_storage
    }

    /// Exact total bytes in the region-owned representation arrays.
    ///
    /// This is the sum returned by [`StorageBreakdown::total_bytes`].
    #[must_use]
    pub fn logical_storage_bytes(&self) -> usize {
        self.logical_storage.total_bytes()
    }

    /// Exact byte accounting from the three backing-vector capacities.
    ///
    /// This includes spare elements retained by `RegionVec`, but still excludes
    /// allocator bookkeeping and allocation-size-class rounding. It may exceed
    /// [`Self::logical_storage_bytes`] when a caller-supplied word vector or a
    /// builder retained spare capacity.
    #[must_use]
    pub const fn retained_storage_breakdown(&self) -> StorageBreakdown {
        self.retained_storage
    }

    /// Exact element-capacity bytes retained by the three backing vectors.
    #[must_use]
    pub const fn retained_storage_bytes(&self) -> usize {
        self.retained_storage.total_bytes()
    }

    fn zeros_before_superblock(&self, superblock_index: usize) -> usize {
        let bit_start = superblock_index
            .checked_mul(SUPERBLOCK_BITS)
            .unwrap_or(self.bit_len)
            .min(self.bit_len);
        bit_start - self.rank_superblocks.as_slice()[superblock_index]
    }

    fn valid_bits_in_word(&self, word_index: usize) -> usize {
        let bit_start = word_index.checked_mul(WORD_BITS).unwrap_or(self.bit_len);
        self.bit_len.saturating_sub(bit_start).min(WORD_BITS)
    }
}

/// Fallible canonical word builder for [`SuccinctBitVector`].
///
/// The maximum is immutable and checked before any mutating append. Slice
/// extension is transactional with respect to limit and allocation failures:
/// either every bit is appended or the builder remains unchanged. At every
/// observable state, unused bits in the final word are zero.
#[derive(Debug)]
pub struct SuccinctBitVectorBuilder<'region> {
    scope: &'region RegionScope,
    max_bits: usize,
    bit_len: usize,
    words: RegionVec<'region, u64>,
}

impl PartialEq for SuccinctBitVectorBuilder<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.max_bits == other.max_bits
            && self.bit_len == other.bit_len
            && self.words == other.words
    }
}

impl Eq for SuccinctBitVectorBuilder<'_> {}

impl<'region> SuccinctBitVectorBuilder<'region> {
    /// Creates an empty builder without allocating.
    pub fn new_in(scope: &'region RegionScope, max_bits: usize) -> Result<Self, BitVectorError> {
        Ok(Self {
            scope,
            max_bits,
            bit_len: 0,
            words: RegionVec::new_in(scope).map_err(|source| BitVectorError::Region {
                target: AllocationTarget::Words,
                source,
            })?,
        })
    }

    /// Creates an empty builder and fallibly reserves for `capacity_bits`.
    ///
    /// `capacity_bits` is an allocation hint, not the logical length, and may
    /// not exceed `max_bits`. The allocator may retain more whole-word capacity
    /// than requested; [`Self::capacity_bits`] reports the exact usable result,
    /// capped by the explicit maximum.
    pub fn try_with_capacity_in(
        scope: &'region RegionScope,
        cx: &QueryCx,
        max_bits: usize,
        capacity_bits: usize,
    ) -> Result<Self, BitVectorError> {
        if capacity_bits > max_bits {
            return Err(BitVectorError::BitLimitExceeded {
                attempted_bits: capacity_bits,
                max_bits,
            });
        }

        let words = RegionVec::with_capacity_in(scope, cx, word_count(capacity_bits)).map_err(
            |source| BitVectorError::Region {
                target: AllocationTarget::Words,
                source,
            },
        )?;
        Ok(Self {
            scope,
            max_bits,
            bit_len: 0,
            words,
        })
    }

    /// Current logical length.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.bit_len
    }

    /// Whether no bits have been appended.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bit_len == 0
    }

    /// Immutable maximum logical length.
    #[must_use]
    pub const fn max_bits(&self) -> usize {
        self.max_bits
    }

    /// Number of bits that may still be appended before reaching the maximum.
    #[must_use]
    pub const fn remaining_bits(&self) -> usize {
        self.max_bits - self.bit_len
    }

    /// Current packed-word length.
    #[must_use]
    pub fn word_len(&self) -> usize {
        self.words.len()
    }

    /// Actual packed-word capacity retained by the allocator.
    #[must_use]
    pub fn word_capacity(&self) -> usize {
        self.words.capacity()
    }

    /// Exact logical capacity available without another word-vector allocation.
    ///
    /// The value is capped at `max_bits`; allocator capacity can never widen
    /// the builder's explicit resource limit.
    #[must_use]
    pub fn capacity_bits(&self) -> usize {
        let maximum_words = word_count(self.max_bits);
        if self.words.capacity() >= maximum_words {
            self.max_bits
        } else {
            // Here capacity < ceil(max_bits / 64), so this multiplication is
            // bounded by max_bits and cannot overflow.
            self.words.capacity() * WORD_BITS
        }
    }

    /// Bits appendable without a word-vector allocation.
    #[must_use]
    pub fn spare_capacity_bits(&self) -> usize {
        self.capacity_bits() - self.bit_len
    }

    /// Exact bytes occupied by current logical word elements.
    #[must_use]
    pub fn logical_word_bytes(&self) -> usize {
        self.words.len() * size_of::<u64>()
    }

    /// Exact bytes represented by the word vector's element capacity.
    ///
    /// Allocator bookkeeping and size-class rounding are intentionally absent.
    #[must_use]
    pub fn retained_word_bytes(&self) -> usize {
        self.words.capacity() * size_of::<u64>()
    }

    /// Canonical packed words accumulated so far.
    ///
    /// Unused bits in the final word are always zero.
    #[must_use]
    pub fn as_words(&self) -> &[u64] {
        self.words.as_slice()
    }

    /// Fallibly appends one bit.
    ///
    /// A limit or allocation failure leaves the builder unchanged.
    pub fn push(&mut self, cx: &QueryCx, bit: bool) -> Result<(), BitVectorError> {
        self.extend(cx, core::slice::from_ref(&bit))
    }

    /// Fallibly appends a complete bit slice.
    ///
    /// Length overflow, the explicit maximum, and any necessary allocation are
    /// checked before the first word is changed. Chunking therefore cannot
    /// change the canonical finished representation.
    pub fn extend(&mut self, cx: &QueryCx, bits: &[bool]) -> Result<(), BitVectorError> {
        let target_len = checked_appended_len(self.bit_len, bits.len(), self.max_bits)?;
        cx.checkpoint().map_err(|_| BitVectorError::Region {
            target: AllocationTarget::Words,
            source: RegionVecError::CheckpointRefused,
        })?;

        let target_words = word_count(target_len);
        if target_words > self.words.len() {
            self.words
                .try_resize(cx, target_words, 0)
                .map_err(|source| BitVectorError::Region {
                    target: AllocationTarget::Words,
                    source,
                })?;
        }

        for (offset, &bit) in bits.iter().enumerate() {
            if bit {
                let bit_index = self.bit_len + offset;
                let word = &mut self.words.as_mut_slice()[bit_index / WORD_BITS];
                *word |= 1_u64 << (bit_index % WORD_BITS);
            }
        }
        self.bit_len = target_len;
        Ok(())
    }

    /// Finishes the vector and fallibly constructs its rank directories.
    ///
    /// The packed-word `RegionVec` is moved directly into the immutable vector:
    /// this
    /// performs no clone, copy, shrink, or replacement allocation. Its exact
    /// retained capacity remains visible through
    /// [`SuccinctBitVector::retained_storage_breakdown`].
    pub fn finish(self, cx: &QueryCx) -> Result<SuccinctBitVector<'region>, BitVectorError> {
        SuccinctBitVector::try_from_region_words(self.scope, cx, self.bit_len, self.words)
    }
}

impl<'a, 'region> IntoIterator for &'a SuccinctBitVector<'region> {
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

fn checked_appended_len(
    current_bits: usize,
    additional_bits: usize,
    max_bits: usize,
) -> Result<usize, BitVectorError> {
    let attempted_bits =
        current_bits
            .checked_add(additional_bits)
            .ok_or(BitVectorError::LengthOverflow {
                current_bits,
                additional_bits,
            })?;
    if attempted_bits > max_bits {
        return Err(BitVectorError::BitLimitExceeded {
            attempted_bits,
            max_bits,
        });
    }
    Ok(attempted_bits)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(miri)]
    use asupersync::Cx;
    #[cfg(miri)]
    use asupersync::cx::cap;
    #[cfg(not(miri))]
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::PurposeContexts;

    #[cfg(not(miri))]
    fn query_cx() -> QueryCx {
        let (query, report) = run_async_under_lab(0x5acc_1acc, |root| async move {
            PurposeContexts::narrow_runtime_root(&root).query()
        });
        assert!(
            report.invariant_violations.is_empty(),
            "lab invariant violation: {report:?}"
        );
        query
    }

    #[cfg(miri)]
    fn query_cx() -> QueryCx {
        let root = Cx::<cap::All>::for_testing();
        PurposeContexts::narrow_runtime_root(&root).query()
    }

    fn test_scope() -> RegionScope {
        RegionScope::with_capacity(1 << 20, 1 << 28, 1 << 30)
    }

    #[test]
    fn construction_rejects_noncanonical_shapes_and_padding() {
        let scope = test_scope();
        let cx = query_cx();
        assert_eq!(
            SuccinctBitVector::try_from_words(&scope, &cx, 0, &[0]),
            Err(BitVectorError::WordCountMismatch {
                bit_len: 0,
                expected_words: 0,
                actual_words: 1,
            })
        );
        assert_eq!(
            SuccinctBitVector::try_from_words(&scope, &cx, 65, &[0]),
            Err(BitVectorError::WordCountMismatch {
                bit_len: 65,
                expected_words: 2,
                actual_words: 1,
            })
        );
        assert_eq!(
            SuccinctBitVector::try_from_words(&scope, &cx, 65, &[0, 2]),
            Err(BitVectorError::NonZeroPadding {
                bit_len: 65,
                word_index: 1,
                word: 2,
                non_zero_padding: 2,
            })
        );

        let exact = SuccinctBitVector::try_from_words(&scope, &cx, 65, &[u64::MAX, 1])
            .expect("canonical words construct");
        assert_eq!(exact.as_words(), &[u64::MAX, 1]);
        assert_eq!(exact.len(), 65);
        assert_eq!(exact.count_ones(), 65);
    }

    #[test]
    fn empty_vector_has_defined_boundaries_and_storage() {
        let scope = test_scope();
        let cx = query_cx();
        let vector = SuccinctBitVector::try_from_words(&scope, &cx, 0, &[])
            .expect("empty vector constructs");
        assert!(vector.is_empty());
        assert_eq!(vector.len(), 0);
        assert_eq!(vector.as_words(), &[] as &[u64]);
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
                total_bytes: size_of::<usize>(),
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
            0_usize, 1, 2, 63, 64, 65, 127, 128, 129, 510, 511, 512, 513, 514, 1023, 1024, 1025,
            1535, 1536, 1537, 2047, 2048, 2049,
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
        let scope = test_scope();
        let cx = query_cx();
        let sparse_ones = make_bits(2_049, |index| matches!(index, 0 | 1_024 | 2_048));
        let ones_vector = SuccinctBitVector::try_from_bits(&scope, &cx, &sparse_ones)
            .expect("sparse ones construct");
        assert_eq!(ones_vector.select1(0), Some(0));
        assert_eq!(ones_vector.select1(1), Some(1_024));
        assert_eq!(ones_vector.select1(2), Some(2_048));

        let sparse_zeros = make_bits(2_049, |index| !matches!(index, 0 | 1_024 | 2_048));
        let zeros_vector = SuccinctBitVector::try_from_bits(&scope, &cx, &sparse_zeros)
            .expect("sparse zeros construct");
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
        let scope = test_scope();
        let cx = query_cx();
        let vector = SuccinctBitVector::try_from_words(&scope, &cx, 65, &[0, 1])
            .expect("canonical partial word");
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
        let scope = test_scope();
        let cx = query_cx();
        let vector =
            SuccinctBitVector::try_from_words(&scope, &cx, 513, &[0; 9]).expect("canonical vector");
        let expected = StorageBreakdown {
            word_bytes: 9 * size_of::<u64>(),
            rank_superblock_bytes: 3 * size_of::<usize>(),
            rank_subblock_bytes: 9 * size_of::<u16>(),
            total_bytes: 9 * size_of::<u64>() + 3 * size_of::<usize>() + 9 * size_of::<u16>(),
        };
        assert_eq!(vector.storage_breakdown(), expected);
        assert_eq!(vector.logical_storage_bytes(), expected.total_bytes());

        let retained = vector.retained_storage_breakdown();
        assert_eq!(
            retained.word_bytes,
            vector.words.capacity() * size_of::<u64>()
        );
        assert_eq!(
            retained.rank_superblock_bytes,
            vector.rank_superblocks.capacity() * size_of::<usize>()
        );
        assert_eq!(
            retained.rank_subblock_bytes,
            vector.rank_subblocks.capacity() * size_of::<u16>()
        );
        assert_eq!(vector.retained_storage_bytes(), retained.total_bytes());
        assert!(vector.retained_storage_bytes() >= vector.logical_storage_bytes());
    }

    #[test]
    fn builder_enforces_limit_atomically_at_bit_boundaries() {
        let scope = test_scope();
        let cx = query_cx();
        let mut builder = SuccinctBitVectorBuilder::try_with_capacity_in(&scope, &cx, 65, 65)
            .expect("builder reserves");
        assert_eq!(builder.len(), 0);
        assert!(builder.is_empty());
        assert_eq!(builder.max_bits(), 65);
        assert_eq!(builder.remaining_bits(), 65);
        assert_eq!(builder.word_len(), 0);
        assert!(builder.word_capacity() >= 2);
        assert_eq!(builder.capacity_bits(), 65);
        assert_eq!(builder.spare_capacity_bits(), 65);
        assert_eq!(builder.logical_word_bytes(), 0);
        assert_eq!(
            builder.retained_word_bytes(),
            builder.word_capacity() * size_of::<u64>()
        );

        let first_word = make_bits(64, |index| index % 3 == 0);
        builder
            .extend(&cx, &first_word)
            .expect("first word extends");
        builder.push(&cx, true).expect("boundary bit appends");
        assert_eq!(builder.len(), 65);
        assert_eq!(builder.remaining_bits(), 0);
        assert_eq!(builder.word_len(), 2);
        assert_eq!(builder.logical_word_bytes(), 2 * size_of::<u64>());
        assert_eq!(builder.as_words()[1], 1);

        let before_words = builder.as_words().to_vec();
        let before_len = builder.len();
        assert_eq!(
            builder.push(&cx, false),
            Err(BitVectorError::BitLimitExceeded {
                attempted_bits: 66,
                max_bits: 65,
            })
        );
        assert_eq!(builder.len(), before_len);
        assert_eq!(builder.as_words(), before_words);

        let vector = builder.finish(&cx).expect("builder finishes");
        let mut expected = first_word;
        expected.push(true);
        assert_eq!(
            vector,
            SuccinctBitVector::try_from_bits(&scope, &cx, &expected).expect("reference constructs")
        );
    }

    #[test]
    fn builder_rejected_extend_does_not_partially_mutate() {
        let scope = test_scope();
        let cx = query_cx();
        let mut builder = SuccinctBitVectorBuilder::new_in(&scope, 5).expect("builder opens");
        builder
            .extend(&cx, &[true, false, true])
            .expect("prefix extends");
        let before_words = builder.as_words().to_vec();
        let before_len = builder.len();
        assert_eq!(
            builder.extend(&cx, &[false, true, false]),
            Err(BitVectorError::BitLimitExceeded {
                attempted_bits: 6,
                max_bits: 5,
            })
        );
        assert_eq!(builder.len(), before_len);
        assert_eq!(builder.as_words(), before_words);
        assert_eq!(
            SuccinctBitVectorBuilder::try_with_capacity_in(&scope, &cx, 4, 5),
            Err(BitVectorError::BitLimitExceeded {
                attempted_bits: 5,
                max_bits: 4,
            })
        );
        assert_eq!(
            checked_appended_len(usize::MAX, 1, usize::MAX),
            Err(BitVectorError::LengthOverflow {
                current_bits: usize::MAX,
                additional_bits: 1,
            })
        );
    }

    #[test]
    fn builder_chunking_is_canonically_equivalent() {
        let scope = test_scope();
        let cx = query_cx();
        for len in [0_usize, 1, 63, 64, 65, 511, 512, 513, 2_049] {
            let bits = make_bits(len, |index| {
                index == 0 || index + 1 == len || index % 7 == 2 || index % 127 == 64
            });
            let reference =
                SuccinctBitVector::try_from_bits(&scope, &cx, &bits).expect("reference constructs");

            let mut pushed = SuccinctBitVectorBuilder::new_in(&scope, len).expect("builder opens");
            for &bit in &bits {
                pushed.push(&cx, bit).expect("single bit appends");
            }
            let pushed = pushed.finish(&cx).expect("pushed builder finishes");

            let mut chunked =
                SuccinctBitVectorBuilder::try_with_capacity_in(&scope, &cx, len, len.min(65))
                    .expect("chunked builder reserves");
            let chunk_sizes = [2_usize, 1, 63, 7, 129, 3, 64];
            let mut start = 0_usize;
            let mut chunk_index = 0_usize;
            while start < bits.len() {
                let end = start
                    .saturating_add(chunk_sizes[chunk_index % chunk_sizes.len()])
                    .min(bits.len());
                chunked
                    .extend(&cx, &bits[start..end])
                    .expect("chunk appends");
                let final_bits = chunked.len() % WORD_BITS;
                if final_bits != 0 {
                    let padding =
                        chunked.as_words()[chunked.word_len() - 1] & !low_mask(final_bits);
                    assert_eq!(padding, 0);
                }
                start = end;
                chunk_index += 1;
            }
            let chunked = chunked.finish(&cx).expect("chunked builder finishes");

            assert_eq!(pushed, reference, "push mismatch at length {len}");
            assert_eq!(chunked, reference, "chunk mismatch at length {len}");
        }
    }

    #[test]
    fn builder_finish_reuses_word_allocation_and_capacity() {
        let scope = test_scope();
        let cx = query_cx();
        let bits = make_bits(513, |index| index % 11 == 4);
        let mut builder =
            SuccinctBitVectorBuilder::try_with_capacity_in(&scope, &cx, bits.len(), bits.len())
                .expect("builder reserves");
        builder.extend(&cx, &bits).expect("bits append");
        let words_pointer = builder.words.as_slice().as_ptr();
        let words_capacity = builder.words.capacity();

        let vector = builder.finish(&cx).expect("builder finishes");
        assert_eq!(vector.words.as_slice().as_ptr(), words_pointer);
        assert_eq!(vector.words.capacity(), words_capacity);
        assert_eq!(
            vector.retained_storage_breakdown().word_bytes,
            words_capacity * size_of::<u64>()
        );
        let reference =
            SuccinctBitVector::try_from_bits(&scope, &cx, &bits).expect("reference constructs");
        assert_eq!(vector, reference);
    }

    #[test]
    fn one_iterator_is_exact_sized_and_fused() {
        let scope = test_scope();
        let cx = query_cx();
        let bits = make_bits(200, |index| matches!(index, 0 | 63 | 64 | 199));
        let vector = SuccinctBitVector::try_from_bits(&scope, &cx, &bits).expect("bits construct");
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
        let scope = test_scope();
        let cx = query_cx();
        let vector = SuccinctBitVector::try_from_bits(&scope, &cx, bits).expect("bits construct");
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
        for invalid_end in [
            bits.len().saturating_add(1),
            bits.len().saturating_add(SUPERBLOCK_BITS + 1),
            usize::MAX,
        ] {
            assert_eq!(vector.rank1(invalid_end), None);
            assert_eq!(vector.rank0(invalid_end), None);
        }

        for (ordinal, &position) in expected_ones.iter().enumerate() {
            assert_eq!(
                vector.select1(ordinal),
                Some(position),
                "select1 mismatch at {ordinal} for len {}",
                bits.len()
            );
        }
        for invalid_ordinal in [
            expected_ones.len(),
            expected_ones.len().saturating_add(1),
            expected_ones.len().saturating_add(SUPERBLOCK_BITS + 1),
            usize::MAX,
        ] {
            assert_eq!(vector.select1(invalid_ordinal), None);
        }

        for (ordinal, &position) in expected_zeros.iter().enumerate() {
            assert_eq!(
                vector.select0(ordinal),
                Some(position),
                "select0 mismatch at {ordinal} for len {}",
                bits.len()
            );
        }
        for invalid_ordinal in [
            expected_zeros.len(),
            expected_zeros.len().saturating_add(1),
            expected_zeros.len().saturating_add(SUPERBLOCK_BITS + 1),
            usize::MAX,
        ] {
            assert_eq!(vector.select0(invalid_ordinal), None);
        }
    }
}
