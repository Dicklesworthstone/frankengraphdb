//! Honest scalar neighbor-list representations.
//!
//! This module is deliberately below durable framing and codec registration.
//! It defines no durable codec ID and never chooses a representation
//! adaptively: callers construct one explicit [`EncodedNeighbors`] arm.
//!
//! The three arms have intentionally different access contracts:
//!
//! - [`EliasFanoNeighbors`] delegates to the succinct Elias-Fano index. Select
//!   does not decode a separate byte stream; rank is the Elias-Fano module's
//!   binary search over selects.
//! - [`StreamVByteNeighbors`] stores deterministic fixed-size blocks with
//!   fences. A lookup binary-searches fences where applicable and decodes at
//!   most [`STREAM_BLOCK_ENTRIES`] values. It makes no direct rank/select claim
//!   for a separate varint stream.
//! - [`DenseIntervals`] stores maximal inclusive runs. Membership, rank, and
//!   select use interval arithmetic plus a binary search over interval ends.
//!
//! Intersections merge compressed access paths and materialize only the sorted
//! result. Dense/dense intersection operates directly on interval pairs. The
//! generic merge retains each current value and caches one bounded
//! StreamVByte block per input, so a block is decoded at most once per merge
//! pass without expanding a complete input list.

#![forbid(unsafe_code)]

use core::{cmp, fmt};

pub use crate::elias_fano::EntryLimit;
use crate::elias_fano::{EliasFano, EliasFanoError};

/// Number of logical values in every complete StreamVByte block.
pub const STREAM_BLOCK_ENTRIES: usize = 128;

const WIDTHS_PER_CONTROL_BYTE: usize = 2;

#[cfg(test)]
std::thread_local! {
    static STREAM_BLOCK_DECODE_ATTEMPTS: core::cell::Cell<usize> =
        const { core::cell::Cell::new(0) };
    static ELIAS_FANO_SELECT_ATTEMPTS: core::cell::Cell<usize> =
        const { core::cell::Cell::new(0) };
    static DENSE_INTERVAL_SELECT_ATTEMPTS: core::cell::Cell<usize> =
        const { core::cell::Cell::new(0) };
}

#[cfg(test)]
fn record_stream_block_decode_attempt() {
    STREAM_BLOCK_DECODE_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
}

#[cfg(test)]
fn reset_stream_block_decode_attempts() {
    STREAM_BLOCK_DECODE_ATTEMPTS.with(|attempts| attempts.set(0));
}

#[cfg(test)]
fn stream_block_decode_attempts() -> usize {
    STREAM_BLOCK_DECODE_ATTEMPTS.with(core::cell::Cell::get)
}

#[cfg(test)]
fn reset_random_access_select_attempts() {
    ELIAS_FANO_SELECT_ATTEMPTS.with(|attempts| attempts.set(0));
    DENSE_INTERVAL_SELECT_ATTEMPTS.with(|attempts| attempts.set(0));
}

#[cfg(test)]
fn random_access_select_attempts(codec: NeighborCodec) -> usize {
    match codec {
        NeighborCodec::EliasFano => ELIAS_FANO_SELECT_ATTEMPTS.with(core::cell::Cell::get),
        NeighborCodec::DenseIntervals => DENSE_INTERVAL_SELECT_ATTEMPTS.with(core::cell::Cell::get),
        NeighborCodec::StreamVByte => 0,
    }
}

/// Closed scalar neighbor representation union.
///
/// This is an in-memory capability tag, not a durable codec identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NeighborCodec {
    /// Succinct Elias-Fano index.
    EliasFano,
    /// Fixed-block stream-variable-byte encoding with fences.
    StreamVByte,
    /// Maximal inclusive dense intervals.
    DenseIntervals,
}

/// Internal allocation named by a neighbor-codec failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocationTarget {
    /// StreamVByte control and payload bytes.
    StreamBytes,
    /// StreamVByte block fences.
    StreamFences,
    /// Maximal dense intervals.
    DenseIntervals,
    /// Materialized sorted intersection result.
    Intersection,
}

/// Checked size calculation named by a neighbor-codec failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SizeCalculation {
    /// Total StreamVByte encoded byte length.
    StreamEncodedLength,
    /// A StreamVByte block byte range.
    StreamByteRange,
    /// A StreamVByte block's logical start.
    StreamLogicalStart,
    /// A decoded StreamVByte value.
    StreamValue,
    /// A dense interval's logical length.
    DenseIntervalLength,
    /// Exact sorted-intersection cardinality.
    IntersectionCardinality,
}

/// Typed reason a private StreamVByte representation is malformed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MalformedStream {
    /// The logical fence start differs from its fixed block position.
    FenceStart {
        /// Start implied by the block ordinal.
        expected: usize,
        /// Start recorded in the fence.
        actual: usize,
    },
    /// A block declared zero or too many logical entries.
    InvalidEntryCount {
        /// Rejected count.
        actual: usize,
    },
    /// A block's bytes do not begin immediately after the preceding block.
    NonContiguousBytes {
        /// Expected byte offset.
        expected: usize,
        /// Recorded byte offset.
        actual: usize,
    },
    /// Adjacent blocks do not form one globally strictly increasing sequence.
    CrossBlockOrder {
        /// Last decoded value in the preceding block.
        previous_last: u64,
        /// First decoded value in the current block.
        next_first: u64,
    },
    /// A checked block end exceeds the backing byte stream.
    ByteRangeOutOfBounds {
        /// Checked exclusive block end.
        end: usize,
        /// Available encoded bytes.
        available: usize,
    },
    /// An odd-sized block set noncanonical unused control bits.
    NonZeroUnusedControl {
        /// Rejected final control byte.
        control: u8,
    },
    /// A control nibble declared a component wider than `u64`.
    InvalidComponentWidth {
        /// Width declared by the control nibble.
        encoded: usize,
        /// Largest supported width.
        maximum: usize,
    },
    /// A component's declared byte width exceeds the block payload.
    TruncatedComponent {
        /// Declared component width.
        width: usize,
        /// Bytes remaining in this block.
        available: usize,
    },
    /// A component used more bytes than its canonical little-endian width.
    NonCanonicalWidth {
        /// Reconstructed component.
        component: u64,
        /// Width declared by the control nibble.
        encoded: usize,
        /// Canonical width for `component`.
        canonical: usize,
    },
    /// A noninitial component encoded a zero gap.
    ZeroDelta,
    /// Adding a positive gap to the previous value overflowed `u64`.
    ValueOverflow {
        /// Previous absolute value.
        previous: u64,
        /// Decoded positive gap.
        delta: u64,
    },
    /// Payload bytes remained after the declared entries were decoded.
    TrailingBlockBytes {
        /// Remaining bytes.
        trailing: usize,
    },
    /// Bytes remain after the final fence-owned block.
    TrailingStreamBytes {
        /// Unreferenced trailing bytes.
        trailing: usize,
    },
    /// Decoded boundary values disagree with the fence.
    FenceValues {
        /// Fence first value.
        expected_first: u64,
        /// Decoded first value.
        actual_first: u64,
        /// Fence last value.
        expected_last: u64,
        /// Decoded last value.
        actual_last: u64,
    },
    /// Fence counts do not sum to the representation's logical length.
    LogicalLength {
        /// Length declared by the representation.
        expected: usize,
        /// Length reconstructed from fences.
        actual: usize,
    },
}

/// Checked neighbor representation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NeighborError {
    /// The input contains more entries than the caller authorized.
    EntryLimitExceeded {
        /// Number of input entries.
        entries: usize,
        /// Caller-selected ceiling.
        limit: usize,
    },
    /// Strictly increasing order failed at `index`.
    NotStrictlyIncreasing {
        /// Index of the rejected value.
        index: usize,
        /// Value immediately before `index`.
        previous: u64,
        /// Rejected value.
        current: u64,
    },
    /// Representation-size arithmetic overflowed.
    SizeOverflow {
        /// Stable calculation name.
        calculation: SizeCalculation,
    },
    /// Reserving one bounded representation component failed.
    AllocationFailed {
        /// Component being allocated.
        target: AllocationTarget,
        /// Requested entries or bytes, according to `target`.
        requested: usize,
    },
    /// Elias-Fano's checked constructor rejected the validated input.
    EliasFano {
        /// Underlying typed failure.
        source: EliasFanoError,
    },
    /// Private StreamVByte bytes or metadata violated their scalar grammar.
    MalformedStream {
        /// Block containing the violation.
        block_index: usize,
        /// Absolute byte offset nearest the violation.
        byte_offset: usize,
        /// Typed violation.
        cause: MalformedStream,
    },
    /// A sorted intersection exceeded its caller-authorized output entries.
    IntersectionLimitExceeded {
        /// Caller-selected output ceiling.
        limit: usize,
    },
    /// A private representation failed to return an in-range value.
    InternalValueMissing {
        /// Representation whose private invariant failed.
        codec: NeighborCodec,
        /// In-range logical position.
        index: usize,
    },
}

/// Input side named by a logical-equivalence verification failure.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NeighborEquivalenceSide {
    /// The receiver passed to
    /// [`EncodedNeighbors::verify_logical_equivalence`].
    Left,
    /// The `other` representation passed to
    /// [`EncodedNeighbors::verify_logical_equivalence`].
    Right,
}

/// Failure while comparing the logical values of two neighbor representations.
///
/// This error describes an in-memory, registry-independent comparison. It is
/// not a durable digest, a codec-identity check, or evidence about graph
/// visibility or authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NeighborEquivalenceError {
    /// The representations differ at their first unequal logical position.
    ValueMismatch {
        /// Zero-based logical position of the first difference.
        index: usize,
        /// Value from the left representation, or `None` if it ended.
        left: Option<u64>,
        /// Value from the right representation, or `None` if it ended.
        right: Option<u64>,
    },
    /// A representation failed to produce an in-range logical value.
    Internal {
        /// Representation side that failed.
        side: NeighborEquivalenceSide,
        /// Explicit in-memory representation arm that failed.
        codec: NeighborCodec,
        /// Logical position being read when the failure occurred.
        index: usize,
        /// Typed private-representation failure.
        source: NeighborError,
    },
}

impl fmt::Display for NeighborError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EntryLimitExceeded { entries, limit } => write!(
                formatter,
                "neighbor input has {entries} entries, limit is {limit}"
            ),
            Self::NotStrictlyIncreasing {
                index,
                previous,
                current,
            } => write!(
                formatter,
                "neighbor input is not strictly increasing at index {index}: \
                 {previous} then {current}"
            ),
            Self::SizeOverflow { calculation } => {
                write!(formatter, "neighbor {calculation:?} arithmetic overflowed")
            }
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "could not reserve {requested} units for neighbor {target:?}"
            ),
            Self::EliasFano { source } => write!(formatter, "Elias-Fano construction: {source}"),
            Self::MalformedStream {
                block_index,
                byte_offset,
                cause,
            } => write!(
                formatter,
                "StreamVByte block {block_index} is malformed near byte \
                 {byte_offset}: {cause:?}"
            ),
            Self::IntersectionLimitExceeded { limit } => write!(
                formatter,
                "neighbor intersection exceeds output entry limit {limit}"
            ),
            Self::InternalValueMissing { codec, index } => write!(
                formatter,
                "private {codec:?} representation lost in-range value {index}"
            ),
        }
    }
}

impl fmt::Display for NeighborEquivalenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ValueMismatch { index, left, right } => write!(
                formatter,
                "neighbor representations first differ at index {index}: \
                 left={left:?}, right={right:?}"
            ),
            Self::Internal {
                side,
                codec,
                index,
                source,
            } => write!(
                formatter,
                "{side:?} {codec:?} neighbor representation failed at index \
                 {index}: {source}"
            ),
        }
    }
}

impl std::error::Error for NeighborError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EliasFano { source } => Some(source),
            Self::EntryLimitExceeded { .. }
            | Self::NotStrictlyIncreasing { .. }
            | Self::SizeOverflow { .. }
            | Self::AllocationFailed { .. }
            | Self::MalformedStream { .. }
            | Self::IntersectionLimitExceeded { .. }
            | Self::InternalValueMissing { .. } => None,
        }
    }
}

impl std::error::Error for NeighborEquivalenceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal { source, .. } => Some(source),
            Self::ValueMismatch { .. } => None,
        }
    }
}

/// Strict-neighbor wrapper around the scalar Elias-Fano representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EliasFanoNeighbors {
    inner: EliasFano,
}

impl EliasFanoNeighbors {
    /// Constructs Elias-Fano from explicitly selected, strictly increasing
    /// neighbors.
    pub fn try_new(values: &[u64], limit: EntryLimit) -> Result<Self, NeighborError> {
        validate_input(values, limit)?;
        let inner = EliasFano::try_new(values, limit)
            .map_err(|source| NeighborError::EliasFano { source })?;
        Ok(Self { inner })
    }

    /// Number of neighbors.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when there are no neighbors.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Explicit representation arm.
    #[must_use]
    pub const fn codec(&self) -> NeighborCodec {
        NeighborCodec::EliasFano
    }

    /// True when `value` is present.
    #[must_use]
    pub fn contains(&self, value: u64) -> bool {
        let rank = self.inner.rank_le(value);
        rank.checked_sub(1)
            .and_then(|index| self.inner.select(index))
            == Some(value)
    }

    /// Number of neighbors less than or equal to `value`.
    ///
    /// The Elias-Fano scalar kernel implements this as binary search over its
    /// succinct selects; it does not decode a separate varint stream.
    #[must_use]
    pub fn rank_le(&self, value: u64) -> usize {
        self.inner.rank_le(value)
    }

    /// Neighbor at `index`, if present.
    #[must_use]
    pub fn select(&self, index: usize) -> Option<u64> {
        #[cfg(test)]
        ELIAS_FANO_SELECT_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
        self.inner.select(index)
    }

    /// Materializes the sorted intersection without expanding either input
    /// into an intermediate list.
    pub fn intersection(
        &self,
        other: &EncodedNeighbors,
        limit: EntryLimit,
    ) -> Result<Vec<u64>, NeighborError> {
        intersect_sequences(self, other, limit)
    }
}

/// Non-durable metadata for one deterministic StreamVByte block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamFence {
    logical_start: usize,
    entry_count: u16,
    byte_offset: usize,
    byte_len: usize,
    first: u64,
    last: u64,
}

impl StreamFence {
    /// First logical index in this block.
    #[must_use]
    pub const fn logical_start(self) -> usize {
        self.logical_start
    }

    /// Number of logical values in this block.
    #[must_use]
    pub const fn entry_count(self) -> usize {
        self.entry_count as usize
    }

    /// First byte of this block's versionless internal encoding.
    #[must_use]
    pub const fn byte_offset(self) -> usize {
        self.byte_offset
    }

    /// Number of bytes in this block's versionless internal encoding.
    #[must_use]
    pub const fn byte_len(self) -> usize {
        self.byte_len
    }

    /// First absolute neighbor in the block.
    #[must_use]
    pub const fn first(self) -> u64 {
        self.first
    }

    /// Last absolute neighbor in the block.
    #[must_use]
    pub const fn last(self) -> u64 {
        self.last
    }
}

/// Deterministic fixed-block scalar stream-variable-byte neighbors.
///
/// Each block has at most [`STREAM_BLOCK_ENTRIES`] entries. Two four-bit
/// control widths share each control byte; payload components are minimal-width
/// little-endian integers. The first component is absolute and the remainder
/// are positive deltas. Controls precede payload within each block. This is a
/// versionless internal scalar grammar, not compatibility with an external
/// StreamVByte wire format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamVByteNeighbors {
    len: usize,
    bytes: Vec<u8>,
    fences: Vec<StreamFence>,
}

impl StreamVByteNeighbors {
    /// Encodes explicitly selected, strictly increasing neighbors.
    pub fn try_new(values: &[u64], limit: EntryLimit) -> Result<Self, NeighborError> {
        validate_input(values, limit)?;
        if values.is_empty() {
            return Ok(Self {
                len: 0,
                bytes: Vec::new(),
                fences: Vec::new(),
            });
        }

        let block_count = values.len().div_ceil(STREAM_BLOCK_ENTRIES);
        let encoded_len = stream_encoded_len(values)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(encoded_len)
            .map_err(|_| NeighborError::AllocationFailed {
                target: AllocationTarget::StreamBytes,
                requested: encoded_len,
            })?;
        let mut fences = Vec::new();
        fences
            .try_reserve_exact(block_count)
            .map_err(|_| NeighborError::AllocationFailed {
                target: AllocationTarget::StreamFences,
                requested: block_count,
            })?;

        for (block_index, block) in values.chunks(STREAM_BLOCK_ENTRIES).enumerate() {
            let logical_start = block_index.checked_mul(STREAM_BLOCK_ENTRIES).ok_or(
                NeighborError::SizeOverflow {
                    calculation: SizeCalculation::StreamLogicalStart,
                },
            )?;
            let byte_offset = bytes.len();
            let control_count = block.len().div_ceil(WIDTHS_PER_CONTROL_BYTE);
            let control_end =
                byte_offset
                    .checked_add(control_count)
                    .ok_or(NeighborError::SizeOverflow {
                        calculation: SizeCalculation::StreamByteRange,
                    })?;
            bytes.resize(control_end, 0);

            let mut previous = 0_u64;
            for (within_block, &value) in block.iter().enumerate() {
                let component = if within_block == 0 {
                    value
                } else {
                    value - previous
                };
                let width = component_width(component);
                let control_index = byte_offset + within_block / WIDTHS_PER_CONTROL_BYTE;
                let shift = (within_block % WIDTHS_PER_CONTROL_BYTE) * 4;
                bytes[control_index] |= ((width - 1) as u8) << shift;
                bytes.extend_from_slice(&component.to_le_bytes()[..width]);
                previous = value;
            }

            let byte_len =
                bytes
                    .len()
                    .checked_sub(byte_offset)
                    .ok_or(NeighborError::SizeOverflow {
                        calculation: SizeCalculation::StreamByteRange,
                    })?;
            fences.push(StreamFence {
                logical_start,
                entry_count: u16::try_from(block.len()).map_err(|_| {
                    NeighborError::SizeOverflow {
                        calculation: SizeCalculation::StreamLogicalStart,
                    }
                })?,
                byte_offset,
                byte_len,
                first: block[0],
                last: block[block.len() - 1],
            });
        }

        debug_assert_eq!(bytes.len(), encoded_len);
        let encoded = Self {
            len: values.len(),
            bytes,
            fences,
        };
        debug_assert!(encoded.validate().is_ok());
        Ok(encoded)
    }

    /// Number of neighbors.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// True when there are no neighbors.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Explicit representation arm.
    #[must_use]
    pub const fn codec(&self) -> NeighborCodec {
        NeighborCodec::StreamVByte
    }

    /// Versionless internal bytes, exposed read-only for accounting and
    /// deterministic evidence.
    #[must_use]
    pub fn encoded_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Versionless block metadata, exposed read-only for accounting and
    /// deterministic evidence.
    #[must_use]
    pub fn fences(&self) -> &[StreamFence] {
        &self.fences
    }

    /// Revalidates the private block grammar and all fence bindings.
    pub fn validate(&self) -> Result<(), NeighborError> {
        let mut expected_logical_start = 0_usize;
        let mut expected_byte_offset = 0_usize;
        let mut previous_last = None;
        let mut scratch = [0_u64; STREAM_BLOCK_ENTRIES];

        for (block_index, &fence) in self.fences.iter().enumerate() {
            if fence.logical_start != expected_logical_start {
                return Err(malformed(
                    block_index,
                    fence.byte_offset,
                    MalformedStream::FenceStart {
                        expected: expected_logical_start,
                        actual: fence.logical_start,
                    },
                ));
            }
            if fence.byte_offset != expected_byte_offset {
                return Err(malformed(
                    block_index,
                    fence.byte_offset,
                    MalformedStream::NonContiguousBytes {
                        expected: expected_byte_offset,
                        actual: fence.byte_offset,
                    },
                ));
            }

            let decoded = self.decode_block_into(block_index, &mut scratch)?;
            if let Some(previous) = previous_last
                && previous >= scratch[0]
            {
                return Err(malformed(
                    block_index,
                    fence.byte_offset,
                    MalformedStream::CrossBlockOrder {
                        previous_last: previous,
                        next_first: scratch[0],
                    },
                ));
            }
            previous_last = Some(scratch[decoded - 1]);
            expected_logical_start =
                expected_logical_start
                    .checked_add(decoded)
                    .ok_or(NeighborError::SizeOverflow {
                        calculation: SizeCalculation::StreamLogicalStart,
                    })?;
            expected_byte_offset = fence.byte_offset.checked_add(fence.byte_len).ok_or(
                NeighborError::SizeOverflow {
                    calculation: SizeCalculation::StreamByteRange,
                },
            )?;
        }

        if expected_logical_start != self.len {
            return Err(malformed(
                self.fences.len(),
                expected_byte_offset,
                MalformedStream::LogicalLength {
                    expected: self.len,
                    actual: expected_logical_start,
                },
            ));
        }
        if expected_byte_offset != self.bytes.len() {
            let trailing = self.bytes.len().checked_sub(expected_byte_offset).ok_or(
                NeighborError::SizeOverflow {
                    calculation: SizeCalculation::StreamByteRange,
                },
            )?;
            return Err(malformed(
                self.fences.len(),
                expected_byte_offset,
                MalformedStream::TrailingStreamBytes { trailing },
            ));
        }
        Ok(())
    }

    /// True when `value` is present.
    ///
    /// Fence search is logarithmic; at most one fixed-size block is decoded.
    #[must_use]
    pub fn contains(&self, value: u64) -> bool {
        let block_index = self.fences.partition_point(|fence| fence.last < value);
        let Some(fence) = self.fences.get(block_index) else {
            return false;
        };
        if value < fence.first {
            return false;
        }
        let mut scratch = [0_u64; STREAM_BLOCK_ENTRIES];
        let Ok(count) = self.decode_block_into(block_index, &mut scratch) else {
            return false;
        };
        scratch[..count].binary_search(&value).is_ok()
    }

    /// Number of neighbors less than or equal to `value`.
    ///
    /// Fence search is logarithmic; at most one fixed-size block is decoded.
    /// This is not direct rank over a separate varint stream.
    #[must_use]
    pub fn rank_le(&self, value: u64) -> usize {
        let complete_blocks = self.fences.partition_point(|fence| fence.last <= value);
        if complete_blocks == self.fences.len() {
            return self.len;
        }
        let fence = self.fences[complete_blocks];
        let mut scratch = [0_u64; STREAM_BLOCK_ENTRIES];
        let Ok(count) = self.decode_block_into(complete_blocks, &mut scratch) else {
            return fence.logical_start;
        };
        fence.logical_start + scratch[..count].partition_point(|&candidate| candidate <= value)
    }

    /// Neighbor at `index`, if present.
    ///
    /// The block is found arithmetically and at most
    /// [`STREAM_BLOCK_ENTRIES`] values are decoded.
    #[must_use]
    pub fn select(&self, index: usize) -> Option<u64> {
        if index >= self.len {
            return None;
        }
        let block_index = index / STREAM_BLOCK_ENTRIES;
        let fence = *self.fences.get(block_index)?;
        let within_block = index.checked_sub(fence.logical_start)?;
        let mut scratch = [0_u64; STREAM_BLOCK_ENTRIES];
        let count = self.decode_block_into(block_index, &mut scratch).ok()?;
        scratch
            .get(within_block)
            .copied()
            .filter(|_| within_block < count)
    }

    /// Materializes the sorted intersection without expanding either complete
    /// input list. This representation decodes only fixed-size blocks.
    pub fn intersection(
        &self,
        other: &EncodedNeighbors,
        limit: EntryLimit,
    ) -> Result<Vec<u64>, NeighborError> {
        intersect_sequences(self, other, limit)
    }

    fn decode_block_into(
        &self,
        block_index: usize,
        output: &mut [u64; STREAM_BLOCK_ENTRIES],
    ) -> Result<usize, NeighborError> {
        #[cfg(test)]
        record_stream_block_decode_attempt();

        let Some(&fence) = self.fences.get(block_index) else {
            return Err(malformed(
                block_index,
                self.bytes.len(),
                MalformedStream::LogicalLength {
                    expected: self.len,
                    actual: block_index.saturating_mul(STREAM_BLOCK_ENTRIES),
                },
            ));
        };
        let entry_count = fence.entry_count();
        if entry_count == 0 || entry_count > STREAM_BLOCK_ENTRIES {
            return Err(malformed(
                block_index,
                fence.byte_offset,
                MalformedStream::InvalidEntryCount {
                    actual: entry_count,
                },
            ));
        }
        let expected_start =
            block_index
                .checked_mul(STREAM_BLOCK_ENTRIES)
                .ok_or(NeighborError::SizeOverflow {
                    calculation: SizeCalculation::StreamLogicalStart,
                })?;
        if fence.logical_start != expected_start {
            return Err(malformed(
                block_index,
                fence.byte_offset,
                MalformedStream::FenceStart {
                    expected: expected_start,
                    actual: fence.logical_start,
                },
            ));
        }

        let block_end =
            fence
                .byte_offset
                .checked_add(fence.byte_len)
                .ok_or(NeighborError::SizeOverflow {
                    calculation: SizeCalculation::StreamByteRange,
                })?;
        if block_end > self.bytes.len() {
            return Err(malformed(
                block_index,
                fence.byte_offset,
                MalformedStream::ByteRangeOutOfBounds {
                    end: block_end,
                    available: self.bytes.len(),
                },
            ));
        }
        let control_count = entry_count.div_ceil(WIDTHS_PER_CONTROL_BYTE);
        let mut cursor =
            fence
                .byte_offset
                .checked_add(control_count)
                .ok_or(NeighborError::SizeOverflow {
                    calculation: SizeCalculation::StreamByteRange,
                })?;
        if cursor > block_end {
            return Err(malformed(
                block_index,
                fence.byte_offset,
                MalformedStream::TruncatedComponent {
                    width: control_count,
                    available: fence.byte_len,
                },
            ));
        }
        if !entry_count.is_multiple_of(WIDTHS_PER_CONTROL_BYTE) {
            let last_control = self.bytes[fence.byte_offset + control_count - 1];
            if last_control & 0xf0 != 0 {
                return Err(malformed(
                    block_index,
                    fence.byte_offset + control_count - 1,
                    MalformedStream::NonZeroUnusedControl {
                        control: last_control,
                    },
                ));
            }
        }

        let mut previous = 0_u64;
        for (within_block, slot) in output[..entry_count].iter_mut().enumerate() {
            let control = self.bytes[fence.byte_offset + within_block / WIDTHS_PER_CONTROL_BYTE];
            let shift = (within_block % WIDTHS_PER_CONTROL_BYTE) * 4;
            let width = usize::from((control >> shift) & 0x0f) + 1;
            if width > core::mem::size_of::<u64>() {
                return Err(malformed(
                    block_index,
                    fence.byte_offset + within_block / WIDTHS_PER_CONTROL_BYTE,
                    MalformedStream::InvalidComponentWidth {
                        encoded: width,
                        maximum: core::mem::size_of::<u64>(),
                    },
                ));
            }
            let component_end = cursor
                .checked_add(width)
                .ok_or(NeighborError::SizeOverflow {
                    calculation: SizeCalculation::StreamByteRange,
                })?;
            if component_end > block_end {
                return Err(malformed(
                    block_index,
                    cursor,
                    MalformedStream::TruncatedComponent {
                        width,
                        available: block_end.saturating_sub(cursor),
                    },
                ));
            }

            let mut component = 0_u64;
            for (byte_index, &byte) in self.bytes[cursor..component_end].iter().enumerate() {
                component |= u64::from(byte) << (byte_index * 8);
            }
            let canonical = component_width(component);
            if width != canonical {
                return Err(malformed(
                    block_index,
                    cursor,
                    MalformedStream::NonCanonicalWidth {
                        component,
                        encoded: width,
                        canonical,
                    },
                ));
            }
            let value = if within_block == 0 {
                component
            } else {
                if component == 0 {
                    return Err(malformed(block_index, cursor, MalformedStream::ZeroDelta));
                }
                previous.checked_add(component).ok_or_else(|| {
                    malformed(
                        block_index,
                        cursor,
                        MalformedStream::ValueOverflow {
                            previous,
                            delta: component,
                        },
                    )
                })?
            };
            *slot = value;
            previous = value;
            cursor = component_end;
        }

        if cursor != block_end {
            return Err(malformed(
                block_index,
                cursor,
                MalformedStream::TrailingBlockBytes {
                    trailing: block_end - cursor,
                },
            ));
        }
        let actual_first = output[0];
        let actual_last = output[entry_count - 1];
        if actual_first != fence.first || actual_last != fence.last {
            return Err(malformed(
                block_index,
                fence.byte_offset,
                MalformedStream::FenceValues {
                    expected_first: fence.first,
                    actual_first,
                    expected_last: fence.last,
                    actual_last,
                },
            ));
        }
        Ok(entry_count)
    }
}

/// One maximal inclusive dense-neighbor interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DenseInterval {
    start: u64,
    end: u64,
    logical_end: usize,
}

impl DenseInterval {
    /// Inclusive first neighbor.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Inclusive last neighbor.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Exclusive logical end position across all preceding intervals.
    #[must_use]
    pub const fn logical_end(self) -> usize {
        self.logical_end
    }
}

/// Strictly increasing neighbors represented as maximal inclusive intervals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DenseIntervals {
    len: usize,
    intervals: Vec<DenseInterval>,
}

impl DenseIntervals {
    /// Constructs maximal intervals from explicitly selected, strictly
    /// increasing neighbors.
    pub fn try_new(values: &[u64], limit: EntryLimit) -> Result<Self, NeighborError> {
        validate_input(values, limit)?;
        if values.is_empty() {
            return Ok(Self {
                len: 0,
                intervals: Vec::new(),
            });
        }

        let mut interval_count = 1_usize;
        for pair in values.windows(2) {
            if pair[0].checked_add(1) != Some(pair[1]) {
                interval_count =
                    interval_count
                        .checked_add(1)
                        .ok_or(NeighborError::SizeOverflow {
                            calculation: SizeCalculation::DenseIntervalLength,
                        })?;
            }
        }

        let mut intervals = Vec::new();
        intervals.try_reserve_exact(interval_count).map_err(|_| {
            NeighborError::AllocationFailed {
                target: AllocationTarget::DenseIntervals,
                requested: interval_count,
            }
        })?;
        let mut start = values[0];
        let mut previous = values[0];
        for (index, &value) in values.iter().enumerate().skip(1) {
            if previous.checked_add(1) != Some(value) {
                intervals.push(DenseInterval {
                    start,
                    end: previous,
                    logical_end: index,
                });
                start = value;
            }
            previous = value;
        }
        intervals.push(DenseInterval {
            start,
            end: previous,
            logical_end: values.len(),
        });

        Ok(Self {
            len: values.len(),
            intervals,
        })
    }

    /// Number of neighbors.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// True when there are no neighbors.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Explicit representation arm.
    #[must_use]
    pub const fn codec(&self) -> NeighborCodec {
        NeighborCodec::DenseIntervals
    }

    /// Maximal intervals, exposed read-only for accounting and deterministic
    /// evidence.
    #[must_use]
    pub fn intervals(&self) -> &[DenseInterval] {
        &self.intervals
    }

    #[cfg(test)]
    fn retained_interval_capacity(&self) -> usize {
        self.intervals.capacity()
    }

    /// True when `value` is present.
    ///
    /// This performs a binary search over interval ends and then interval
    /// arithmetic.
    #[must_use]
    pub fn contains(&self, value: u64) -> bool {
        let index = self
            .intervals
            .partition_point(|interval| interval.end < value);
        self.intervals
            .get(index)
            .is_some_and(|interval| interval.start <= value)
    }

    /// Number of neighbors less than or equal to `value`.
    ///
    /// This performs a binary search over interval ends and then interval
    /// arithmetic.
    #[must_use]
    pub fn rank_le(&self, value: u64) -> usize {
        let interval_index = self
            .intervals
            .partition_point(|interval| interval.end <= value);
        if interval_index == self.intervals.len() {
            return self.len;
        }
        let interval = self.intervals[interval_index];
        let preceding = if interval_index == 0 {
            0
        } else {
            self.intervals[interval_index - 1].logical_end
        };
        if value < interval.start {
            return preceding;
        }
        let within = value
            .checked_sub(interval.start)
            .and_then(|offset| offset.checked_add(1))
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(interval.logical_end - preceding);
        preceding + within
    }

    /// Neighbor at `index`, if present.
    ///
    /// This performs a binary search over cumulative interval ends and then
    /// interval arithmetic.
    #[must_use]
    pub fn select(&self, index: usize) -> Option<u64> {
        #[cfg(test)]
        DENSE_INTERVAL_SELECT_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
        if index >= self.len {
            return None;
        }
        let interval_index = self
            .intervals
            .partition_point(|interval| interval.logical_end <= index);
        let interval = *self.intervals.get(interval_index)?;
        let preceding = if interval_index == 0 {
            0
        } else {
            self.intervals[interval_index - 1].logical_end
        };
        let offset = u64::try_from(index.checked_sub(preceding)?).ok()?;
        interval.start.checked_add(offset)
    }

    /// Materializes the sorted intersection without expanding either complete
    /// input list. Dense/dense uses direct interval overlap arithmetic.
    pub fn intersection(
        &self,
        other: &EncodedNeighbors,
        limit: EntryLimit,
    ) -> Result<Vec<u64>, NeighborError> {
        match other {
            EncodedNeighbors::DenseIntervals(other) => {
                intersect_dense_intervals(self, other, limit)
            }
            EncodedNeighbors::EliasFano(_) | EncodedNeighbors::StreamVByte(_) => {
                intersect_sequences(self, other, limit)
            }
        }
    }
}

/// Explicitly selected scalar neighbor representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EncodedNeighbors {
    /// Succinct Elias-Fano index.
    EliasFano(EliasFanoNeighbors),
    /// Fixed-block stream-variable-byte encoding with fences.
    StreamVByte(StreamVByteNeighbors),
    /// Maximal inclusive dense intervals.
    DenseIntervals(DenseIntervals),
}

/// Allocation-free forward cursor over one explicit neighbor representation.
///
/// Elias-Fano and dense-interval arms retain state for linear sequential scans.
/// One fixed [`STREAM_BLOCK_ENTRIES`]-value buffer bounds Elias-Fano fills and
/// StreamVByte block decodes; StreamVByte decodes each visited block at most
/// once. No arm materializes a complete decoded list.
///
/// The cursor is fused: after returning `Ok(None)` or one `Err`, every later
/// [`try_next`](Self::try_next) call returns `Ok(None)`.
#[must_use = "a neighbor cursor does no work until try_next is called"]
pub struct NeighborCursor<'a> {
    codec: NeighborCodec,
    logical_index: usize,
    logical_len: usize,
    fused: bool,
    state: NeighborCursorState<'a>,
    buffer_index: usize,
    buffer_len: usize,
    buffer: [u64; STREAM_BLOCK_ENTRIES],
}

enum NeighborCursorState<'a> {
    EliasFano(crate::elias_fano::EliasFanoSequentialCursor<'a>),
    StreamVByte(&'a StreamVByteNeighbors),
    DenseIntervals(DenseSequentialCursor<'a>),
}

struct DenseSequentialCursor<'a> {
    values: &'a DenseIntervals,
    logical_index: usize,
    interval_index: usize,
    #[cfg(test)]
    interval_advances: usize,
    #[cfg(test)]
    values_yielded: usize,
}

impl<'a> NeighborCursor<'a> {
    fn new(values: &'a EncodedNeighbors) -> Self {
        let codec = values.codec();
        let logical_len = values.len();
        let state = match values {
            EncodedNeighbors::EliasFano(values) => {
                NeighborCursorState::EliasFano(values.inner.sequential_cursor())
            }
            EncodedNeighbors::StreamVByte(values) => NeighborCursorState::StreamVByte(values),
            EncodedNeighbors::DenseIntervals(values) => {
                NeighborCursorState::DenseIntervals(DenseSequentialCursor::new(values))
            }
        };
        Self {
            codec,
            logical_index: 0,
            logical_len,
            fused: false,
            state,
            buffer_index: 0,
            buffer_len: 0,
            buffer: [0; STREAM_BLOCK_ENTRIES],
        }
    }

    /// Returns the next logical neighbor without allocating.
    ///
    /// Private representation corruption is returned as the existing typed
    /// [`NeighborError`] at the first affected logical position. That error
    /// fuses the cursor, so partially decoded data can never be resumed.
    pub fn try_next(&mut self) -> Result<Option<u64>, NeighborError> {
        if self.fused {
            return Ok(None);
        }
        if self.logical_index >= self.logical_len {
            self.fused = true;
            return Ok(None);
        }

        let next = match &mut self.state {
            NeighborCursorState::EliasFano(cursor) => next_buffered_elias_fano(
                cursor,
                self.logical_index,
                &mut self.buffer,
                &mut self.buffer_index,
                &mut self.buffer_len,
            ),
            NeighborCursorState::StreamVByte(values) => next_buffered_stream_vbyte(
                values,
                self.logical_index,
                &mut self.buffer,
                &mut self.buffer_index,
                &mut self.buffer_len,
            ),
            NeighborCursorState::DenseIntervals(cursor) => cursor.try_next(),
        };
        let value = match next {
            Ok(Some(value)) => value,
            Ok(None) => {
                self.fused = true;
                return Err(NeighborError::InternalValueMissing {
                    codec: self.codec,
                    index: self.logical_index,
                });
            }
            Err(error) => {
                self.fused = true;
                return Err(error);
            }
        };

        self.logical_index += 1;
        if self.logical_index == self.logical_len {
            self.fused = true;
        }
        Ok(Some(value))
    }

    /// Explicit representation arm traversed by this cursor.
    #[must_use]
    pub const fn codec(&self) -> NeighborCodec {
        self.codec
    }

    /// Number of logical values yielded successfully.
    #[must_use]
    pub const fn logical_index(&self) -> usize {
        self.logical_index
    }

    /// Number of values still available before normal exhaustion.
    ///
    /// A cursor fused by an error reports zero because it cannot be resumed.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        if self.fused {
            0
        } else {
            self.logical_len - self.logical_index
        }
    }

    /// Whether normal exhaustion or an error has fused the cursor.
    #[must_use]
    pub const fn is_fused(&self) -> bool {
        self.fused
    }
}

impl<'a> DenseSequentialCursor<'a> {
    fn new(values: &'a DenseIntervals) -> Self {
        Self {
            values,
            logical_index: 0,
            interval_index: 0,
            #[cfg(test)]
            interval_advances: 0,
            #[cfg(test)]
            values_yielded: 0,
        }
    }

    fn try_next(&mut self) -> Result<Option<u64>, NeighborError> {
        if self.logical_index >= self.values.len {
            return Ok(None);
        }
        while self
            .values
            .intervals
            .get(self.interval_index)
            .is_some_and(|interval| interval.logical_end <= self.logical_index)
        {
            self.interval_index =
                self.interval_index
                    .checked_add(1)
                    .ok_or(NeighborError::SizeOverflow {
                        calculation: SizeCalculation::DenseIntervalLength,
                    })?;
            #[cfg(test)]
            {
                self.interval_advances += 1;
            }
        }

        let interval = self.values.intervals.get(self.interval_index).ok_or(
            NeighborError::InternalValueMissing {
                codec: NeighborCodec::DenseIntervals,
                index: self.logical_index,
            },
        )?;
        let preceding = if self.interval_index == 0 {
            0
        } else {
            self.values.intervals[self.interval_index - 1].logical_end
        };
        let offset = self.logical_index.checked_sub(preceding).and_then(|value| {
            u64::try_from(value)
                .ok()
                .and_then(|value| interval.start.checked_add(value))
        });
        let value = offset.ok_or(NeighborError::InternalValueMissing {
            codec: NeighborCodec::DenseIntervals,
            index: self.logical_index,
        })?;
        self.logical_index += 1;
        #[cfg(test)]
        {
            self.values_yielded += 1;
        }
        Ok(Some(value))
    }
}

fn next_buffered_elias_fano(
    cursor: &mut crate::elias_fano::EliasFanoSequentialCursor<'_>,
    logical_index: usize,
    buffer: &mut [u64; STREAM_BLOCK_ENTRIES],
    buffer_index: &mut usize,
    buffer_len: &mut usize,
) -> Result<Option<u64>, NeighborError> {
    if *buffer_index == *buffer_len {
        *buffer_len = cursor.fill(buffer);
        *buffer_index = 0;
        if *buffer_len == 0 {
            return Ok(None);
        }
    }
    let value = buffer
        .get(*buffer_index)
        .copied()
        .ok_or(NeighborError::InternalValueMissing {
            codec: NeighborCodec::EliasFano,
            index: logical_index,
        })?;
    *buffer_index += 1;
    Ok(Some(value))
}

fn next_buffered_stream_vbyte(
    values: &StreamVByteNeighbors,
    logical_index: usize,
    buffer: &mut [u64; STREAM_BLOCK_ENTRIES],
    buffer_index: &mut usize,
    buffer_len: &mut usize,
) -> Result<Option<u64>, NeighborError> {
    if logical_index >= values.len {
        return Ok(None);
    }
    if *buffer_index == *buffer_len {
        let block_index = logical_index / STREAM_BLOCK_ENTRIES;
        *buffer_len = values.decode_block_into(block_index, buffer)?;
        *buffer_index = 0;
        if *buffer_len == 0 {
            return Ok(None);
        }
    }
    let value = buffer
        .get(*buffer_index)
        .copied()
        .ok_or(NeighborError::InternalValueMissing {
            codec: NeighborCodec::StreamVByte,
            index: logical_index,
        })?;
    *buffer_index += 1;
    Ok(Some(value))
}

impl EncodedNeighbors {
    /// Explicitly constructs the Elias-Fano arm.
    pub fn try_elias_fano(values: &[u64], limit: EntryLimit) -> Result<Self, NeighborError> {
        EliasFanoNeighbors::try_new(values, limit).map(Self::EliasFano)
    }

    /// Explicitly constructs the StreamVByte arm.
    pub fn try_stream_vbyte(values: &[u64], limit: EntryLimit) -> Result<Self, NeighborError> {
        StreamVByteNeighbors::try_new(values, limit).map(Self::StreamVByte)
    }

    /// Explicitly constructs the dense-interval arm.
    pub fn try_dense_intervals(values: &[u64], limit: EntryLimit) -> Result<Self, NeighborError> {
        DenseIntervals::try_new(values, limit).map(Self::DenseIntervals)
    }

    /// Number of neighbors.
    #[must_use]
    pub const fn len(&self) -> usize {
        match self {
            Self::EliasFano(values) => values.len(),
            Self::StreamVByte(values) => values.len(),
            Self::DenseIntervals(values) => values.len(),
        }
    }

    /// True when there are no neighbors.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Explicit representation arm.
    #[must_use]
    pub const fn codec(&self) -> NeighborCodec {
        match self {
            Self::EliasFano(_) => NeighborCodec::EliasFano,
            Self::StreamVByte(_) => NeighborCodec::StreamVByte,
            Self::DenseIntervals(_) => NeighborCodec::DenseIntervals,
        }
    }

    /// Creates an allocation-free, fused forward cursor over logical values.
    pub fn cursor(&self) -> NeighborCursor<'_> {
        NeighborCursor::new(self)
    }

    /// True when `value` is present.
    #[must_use]
    pub fn contains(&self, value: u64) -> bool {
        match self {
            Self::EliasFano(values) => values.contains(value),
            Self::StreamVByte(values) => values.contains(value),
            Self::DenseIntervals(values) => values.contains(value),
        }
    }

    /// Number of neighbors less than or equal to `value`.
    #[must_use]
    pub fn rank_le(&self, value: u64) -> usize {
        match self {
            Self::EliasFano(values) => values.rank_le(value),
            Self::StreamVByte(values) => values.rank_le(value),
            Self::DenseIntervals(values) => values.rank_le(value),
        }
    }

    /// Neighbor at `index`, if present.
    #[must_use]
    pub fn select(&self, index: usize) -> Option<u64> {
        match self {
            Self::EliasFano(values) => values.select(index),
            Self::StreamVByte(values) => values.select(index),
            Self::DenseIntervals(values) => values.select(index),
        }
    }

    /// Materializes the sorted intersection without materializing either
    /// complete input list.
    pub fn intersection(&self, other: &Self, limit: EntryLimit) -> Result<Vec<u64>, NeighborError> {
        match self {
            Self::EliasFano(values) => values.intersection(other, limit),
            Self::StreamVByte(values) => values.intersection(other, limit),
            Self::DenseIntervals(values) => values.intersection(other, limit),
        }
    }

    /// Verifies that two explicit representations contain exactly the same
    /// sorted stable neighbor values.
    ///
    /// The comparison ignores the representation arms and does not allocate a
    /// complete decoded list. Each side retains at most one fixed-size scalar
    /// cursor buffer; consequently, a StreamVByte input decodes each visited
    /// block at most once. The first value or length difference is returned
    /// with its logical position.
    ///
    /// This is a registry-independent in-memory verifier. It does not define a
    /// durable codec identifier or logical digest, and it makes no claim about
    /// graph visibility or authorization.
    pub fn verify_logical_equivalence(&self, other: &Self) -> Result<(), NeighborEquivalenceError> {
        verify_neighbor_sequences(self, other)
    }
}

trait SortedNeighbors {
    fn sequence_len(&self) -> usize;
    fn sequence_codec(&self) -> NeighborCodec;
    fn sequence_select(&self, index: usize) -> Option<u64>;

    fn sequence_fill(
        &self,
        start: usize,
        output: &mut [u64; STREAM_BLOCK_ENTRIES],
    ) -> Result<usize, NeighborError> {
        if start >= self.sequence_len() {
            return Ok(0);
        }
        output[0] = self
            .sequence_select(start)
            .ok_or(NeighborError::InternalValueMissing {
                codec: self.sequence_codec(),
                index: start,
            })?;
        Ok(1)
    }
}

impl SortedNeighbors for EliasFanoNeighbors {
    fn sequence_len(&self) -> usize {
        self.len()
    }

    fn sequence_codec(&self) -> NeighborCodec {
        self.codec()
    }

    fn sequence_select(&self, index: usize) -> Option<u64> {
        self.select(index)
    }
}

impl SortedNeighbors for StreamVByteNeighbors {
    fn sequence_len(&self) -> usize {
        self.len()
    }

    fn sequence_codec(&self) -> NeighborCodec {
        self.codec()
    }

    fn sequence_select(&self, index: usize) -> Option<u64> {
        self.select(index)
    }

    fn sequence_fill(
        &self,
        start: usize,
        output: &mut [u64; STREAM_BLOCK_ENTRIES],
    ) -> Result<usize, NeighborError> {
        if start >= self.len {
            return Ok(0);
        }
        let block_index = start / STREAM_BLOCK_ENTRIES;
        let block_start = block_index * STREAM_BLOCK_ENTRIES;
        let within_block = start - block_start;
        let count = self.decode_block_into(block_index, output)?;
        if within_block >= count {
            return Err(NeighborError::InternalValueMissing {
                codec: self.codec(),
                index: start,
            });
        }
        output.copy_within(within_block..count, 0);
        Ok(count - within_block)
    }
}

impl SortedNeighbors for DenseIntervals {
    fn sequence_len(&self) -> usize {
        self.len()
    }

    fn sequence_codec(&self) -> NeighborCodec {
        self.codec()
    }

    fn sequence_select(&self, index: usize) -> Option<u64> {
        self.select(index)
    }
}

impl SortedNeighbors for EncodedNeighbors {
    fn sequence_len(&self) -> usize {
        self.len()
    }

    fn sequence_codec(&self) -> NeighborCodec {
        self.codec()
    }

    fn sequence_select(&self, index: usize) -> Option<u64> {
        self.select(index)
    }

    fn sequence_fill(
        &self,
        start: usize,
        output: &mut [u64; STREAM_BLOCK_ENTRIES],
    ) -> Result<usize, NeighborError> {
        match self {
            Self::EliasFano(values) => values.sequence_fill(start, output),
            Self::StreamVByte(values) => values.sequence_fill(start, output),
            Self::DenseIntervals(values) => values.sequence_fill(start, output),
        }
    }
}

fn validate_input(values: &[u64], limit: EntryLimit) -> Result<(), NeighborError> {
    if values.len() > limit.max_entries() {
        return Err(NeighborError::EntryLimitExceeded {
            entries: values.len(),
            limit: limit.max_entries(),
        });
    }
    for (offset, pair) in values.windows(2).enumerate() {
        if pair[0] >= pair[1] {
            return Err(NeighborError::NotStrictlyIncreasing {
                index: offset + 1,
                previous: pair[0],
                current: pair[1],
            });
        }
    }
    Ok(())
}

fn component_width(value: u64) -> usize {
    let significant_bits = (u64::BITS - value.leading_zeros()) as usize;
    cmp::max(1, significant_bits.div_ceil(8))
}

fn stream_encoded_len(values: &[u64]) -> Result<usize, NeighborError> {
    let mut total = 0_usize;
    for block in values.chunks(STREAM_BLOCK_ENTRIES) {
        total = total
            .checked_add(block.len().div_ceil(WIDTHS_PER_CONTROL_BYTE))
            .ok_or(NeighborError::SizeOverflow {
                calculation: SizeCalculation::StreamEncodedLength,
            })?;
        let mut previous = 0_u64;
        for (index, &value) in block.iter().enumerate() {
            let component = if index == 0 { value } else { value - previous };
            total = total.checked_add(component_width(component)).ok_or(
                NeighborError::SizeOverflow {
                    calculation: SizeCalculation::StreamEncodedLength,
                },
            )?;
            previous = value;
        }
    }
    Ok(total)
}

fn malformed(block_index: usize, byte_offset: usize, cause: MalformedStream) -> NeighborError {
    NeighborError::MalformedStream {
        block_index,
        byte_offset,
        cause,
    }
}

fn intersection_output_exact(
    cardinality: usize,
    limit: EntryLimit,
) -> Result<Vec<u64>, NeighborError> {
    if cardinality > limit.max_entries() {
        return Err(NeighborError::IntersectionLimitExceeded {
            limit: limit.max_entries(),
        });
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(cardinality)
        .map_err(|_| NeighborError::AllocationFailed {
            target: AllocationTarget::Intersection,
            requested: cardinality,
        })?;
    Ok(output)
}

struct SequenceCursor<'a, S> {
    sequence: &'a S,
    logical_index: usize,
    buffer_index: usize,
    buffer_len: usize,
    buffer: [u64; STREAM_BLOCK_ENTRIES],
}

impl<'a, S: SortedNeighbors> SequenceCursor<'a, S> {
    fn new(sequence: &'a S) -> Self {
        Self {
            sequence,
            logical_index: 0,
            buffer_index: 0,
            buffer_len: 0,
            buffer: [0; STREAM_BLOCK_ENTRIES],
        }
    }

    fn current(&mut self) -> Result<Option<u64>, NeighborError> {
        if self.logical_index >= self.sequence.sequence_len() {
            return Ok(None);
        }
        if self.buffer_index == self.buffer_len {
            self.buffer_len = self
                .sequence
                .sequence_fill(self.logical_index, &mut self.buffer)?;
            self.buffer_index = 0;
            if self.buffer_len == 0 {
                return Err(NeighborError::InternalValueMissing {
                    codec: self.sequence.sequence_codec(),
                    index: self.logical_index,
                });
            }
        }
        Ok(Some(self.buffer[self.buffer_index]))
    }

    fn advance(&mut self) {
        debug_assert!(self.logical_index < self.sequence.sequence_len());
        debug_assert!(self.buffer_index < self.buffer_len);
        self.logical_index += 1;
        self.buffer_index += 1;
    }
}

fn verify_neighbor_sequences(
    left: &EncodedNeighbors,
    right: &EncodedNeighbors,
) -> Result<(), NeighborEquivalenceError> {
    let mut left_cursor = left.cursor();
    let mut right_cursor = right.cursor();
    let mut index = 0_usize;

    loop {
        let left_value =
            left_cursor
                .try_next()
                .map_err(|source| NeighborEquivalenceError::Internal {
                    side: NeighborEquivalenceSide::Left,
                    codec: left.codec(),
                    index,
                    source,
                })?;
        let right_value =
            right_cursor
                .try_next()
                .map_err(|source| NeighborEquivalenceError::Internal {
                    side: NeighborEquivalenceSide::Right,
                    codec: right.codec(),
                    index,
                    source,
                })?;

        match (left_value, right_value) {
            (None, None) => return Ok(()),
            (Some(left), Some(right)) if left == right => {
                index += 1;
            }
            (left, right) => {
                return Err(NeighborEquivalenceError::ValueMismatch { index, left, right });
            }
        }
    }
}

fn visit_sequence_intersection<L, R>(
    left: &L,
    right: &R,
    mut visit: impl FnMut(u64) -> Result<(), NeighborError>,
) -> Result<(), NeighborError>
where
    L: SortedNeighbors,
    R: SortedNeighbors,
{
    let mut left = SequenceCursor::new(left);
    let mut right = SequenceCursor::new(right);
    while let Some(left_value) = left.current()? {
        let Some(right_value) = right.current()? else {
            break;
        };
        match left_value.cmp(&right_value) {
            cmp::Ordering::Less => left.advance(),
            cmp::Ordering::Greater => right.advance(),
            cmp::Ordering::Equal => {
                visit(left_value)?;
                left.advance();
                right.advance();
            }
        }
    }
    Ok(())
}

fn sequence_intersection_cardinality<L: SortedNeighbors, R: SortedNeighbors>(
    left: &L,
    right: &R,
    limit: EntryLimit,
) -> Result<usize, NeighborError> {
    let mut cardinality = 0_usize;
    visit_sequence_intersection(left, right, |_| {
        cardinality = cardinality
            .checked_add(1)
            .ok_or(NeighborError::SizeOverflow {
                calculation: SizeCalculation::IntersectionCardinality,
            })?;
        if cardinality > limit.max_entries() {
            return Err(NeighborError::IntersectionLimitExceeded {
                limit: limit.max_entries(),
            });
        }
        Ok(())
    })?;
    Ok(cardinality)
}

fn intersect_sequences<L: SortedNeighbors, R: SortedNeighbors>(
    left: &L,
    right: &R,
    limit: EntryLimit,
) -> Result<Vec<u64>, NeighborError> {
    let cardinality = sequence_intersection_cardinality(left, right, limit)?;
    let mut output = intersection_output_exact(cardinality, limit)?;
    if cardinality == 0 {
        return Ok(output);
    }
    visit_sequence_intersection(left, right, |value| {
        output.push(value);
        Ok(())
    })?;
    debug_assert_eq!(output.len(), cardinality);
    Ok(output)
}

fn dense_intersection_cardinality(
    left: &DenseIntervals,
    right: &DenseIntervals,
    limit: EntryLimit,
) -> Result<usize, NeighborError> {
    dense_intersection_cardinality_with(left, right, limit, || {})
}

fn dense_intersection_cardinality_with(
    left: &DenseIntervals,
    right: &DenseIntervals,
    limit: EntryLimit,
    mut inspect_pair: impl FnMut(),
) -> Result<usize, NeighborError> {
    let mut cardinality = 0_usize;
    let mut left_index = 0_usize;
    let mut right_index = 0_usize;
    while left_index < left.intervals.len() && right_index < right.intervals.len() {
        inspect_pair();
        let left_interval = left.intervals[left_index];
        let right_interval = right.intervals[right_index];
        let overlap_start = cmp::max(left_interval.start, right_interval.start);
        let overlap_end = cmp::min(left_interval.end, right_interval.end);
        if overlap_start <= overlap_end {
            let overlap_len = overlap_end
                .checked_sub(overlap_start)
                .and_then(|difference| difference.checked_add(1))
                .and_then(|count| usize::try_from(count).ok())
                .ok_or(NeighborError::SizeOverflow {
                    calculation: SizeCalculation::DenseIntervalLength,
                })?;
            cardinality =
                cardinality
                    .checked_add(overlap_len)
                    .ok_or(NeighborError::SizeOverflow {
                        calculation: SizeCalculation::IntersectionCardinality,
                    })?;
            if cardinality > limit.max_entries() {
                return Err(NeighborError::IntersectionLimitExceeded {
                    limit: limit.max_entries(),
                });
            }
        }

        match left_interval.end.cmp(&right_interval.end) {
            cmp::Ordering::Less => left_index += 1,
            cmp::Ordering::Greater => right_index += 1,
            cmp::Ordering::Equal => {
                left_index += 1;
                right_index += 1;
            }
        }
    }
    Ok(cardinality)
}

fn intersect_dense_intervals(
    left: &DenseIntervals,
    right: &DenseIntervals,
    limit: EntryLimit,
) -> Result<Vec<u64>, NeighborError> {
    let cardinality = dense_intersection_cardinality(left, right, limit)?;
    let mut output = intersection_output_exact(cardinality, limit)?;
    let mut left_index = 0_usize;
    let mut right_index = 0_usize;
    while left_index < left.intervals.len() && right_index < right.intervals.len() {
        let left_interval = left.intervals[left_index];
        let right_interval = right.intervals[right_index];
        let overlap_start = cmp::max(left_interval.start, right_interval.start);
        let overlap_end = cmp::min(left_interval.end, right_interval.end);
        if overlap_start <= overlap_end {
            let overlap_len = overlap_end
                .checked_sub(overlap_start)
                .and_then(|difference| difference.checked_add(1))
                .and_then(|count| usize::try_from(count).ok())
                .ok_or(NeighborError::SizeOverflow {
                    calculation: SizeCalculation::DenseIntervalLength,
                })?;
            debug_assert!(output.len().checked_add(overlap_len) <= Some(cardinality));
            let mut value = overlap_start;
            loop {
                output.push(value);
                if value == overlap_end {
                    break;
                }
                value = value.checked_add(1).ok_or(NeighborError::SizeOverflow {
                    calculation: SizeCalculation::DenseIntervalLength,
                })?;
            }
        }

        match left_interval.end.cmp(&right_interval.end) {
            cmp::Ordering::Less => left_index += 1,
            cmp::Ordering::Greater => right_index += 1,
            cmp::Ordering::Equal => {
                left_index += 1;
                right_index += 1;
            }
        }
    }
    debug_assert_eq!(output.len(), cardinality);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CountingSequence<'a> {
        values: &'a [u64],
        selects: core::cell::Cell<usize>,
        codec: NeighborCodec,
    }

    impl SortedNeighbors for CountingSequence<'_> {
        fn sequence_len(&self) -> usize {
            self.values.len()
        }

        fn sequence_codec(&self) -> NeighborCodec {
            self.codec
        }

        fn sequence_select(&self, index: usize) -> Option<u64> {
            self.selects.set(self.selects.get() + 1);
            self.values.get(index).copied()
        }
    }

    fn encodings(values: &[u64]) -> [EncodedNeighbors; 3] {
        let limit = EntryLimit::new(values.len());
        [
            EncodedNeighbors::try_elias_fano(values, limit).unwrap(),
            EncodedNeighbors::try_stream_vbyte(values, limit).unwrap(),
            EncodedNeighbors::try_dense_intervals(values, limit).unwrap(),
        ]
    }

    fn drain_cursor(encoded: &EncodedNeighbors) -> Vec<u64> {
        let mut cursor = encoded.cursor();
        assert_eq!(cursor.codec(), encoded.codec());
        assert_eq!(cursor.logical_index(), 0);
        assert_eq!(cursor.remaining(), encoded.len());
        assert!(!cursor.is_fused());

        let mut values = Vec::with_capacity(encoded.len());
        while let Some(value) = cursor.try_next().expect("valid representation must scan") {
            values.push(value);
        }
        assert_eq!(cursor.logical_index(), encoded.len());
        assert_eq!(cursor.remaining(), 0);
        assert!(cursor.is_fused());
        assert_eq!(cursor.try_next(), Ok(None));
        assert_eq!(cursor.try_next(), Ok(None));
        values
    }

    fn assert_cross_arm_equivalent(values: &[u64]) {
        let encoded = encodings(values);
        for representation in &encoded {
            assert_eq!(
                drain_cursor(representation),
                values,
                "{:?} cursor",
                representation.codec()
            );
        }
        for left in &encoded {
            for right in &encoded {
                assert_eq!(
                    left.verify_logical_equivalence(right),
                    Ok(()),
                    "{:?} x {:?}",
                    left.codec(),
                    right.codec()
                );
            }
        }
    }

    fn next_deterministic_random(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn assert_matches_naive(values: &[u64]) {
        for encoded in encodings(values) {
            assert_eq!(encoded.len(), values.len());
            assert_eq!(encoded.is_empty(), values.is_empty());
            for (index, &value) in values.iter().enumerate() {
                assert_eq!(encoded.select(index), Some(value), "{:?}", encoded.codec());
            }
            assert_eq!(encoded.select(values.len()), None);

            let mut probes = vec![0, 1, 2, 7, 31, 127, 128, u64::MAX - 1, u64::MAX];
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
                assert_eq!(
                    encoded.contains(probe),
                    values.binary_search(&probe).is_ok(),
                    "{:?}.contains({probe})",
                    encoded.codec()
                );
                assert_eq!(
                    encoded.rank_le(probe),
                    values.partition_point(|&value| value <= probe),
                    "{:?}.rank_le({probe})",
                    encoded.codec()
                );
            }
        }
    }

    #[test]
    fn constructors_reject_limits_duplicates_and_decreases() {
        for constructor in [
            EncodedNeighbors::try_elias_fano,
            EncodedNeighbors::try_stream_vbyte,
            EncodedNeighbors::try_dense_intervals,
        ] {
            assert_eq!(
                constructor(&[1, 2], EntryLimit::new(1)),
                Err(NeighborError::EntryLimitExceeded {
                    entries: 2,
                    limit: 1,
                })
            );
            assert_eq!(
                constructor(&[1, 1], EntryLimit::new(2)),
                Err(NeighborError::NotStrictlyIncreasing {
                    index: 1,
                    previous: 1,
                    current: 1,
                })
            );
            assert_eq!(
                constructor(&[1, 3, 2], EntryLimit::new(3)),
                Err(NeighborError::NotStrictlyIncreasing {
                    index: 2,
                    previous: 3,
                    current: 2,
                })
            );
        }
    }

    #[test]
    fn exhaustive_small_subsets_match_naive() {
        for mask in 0_u16..(1 << 9) {
            let values: Vec<u64> = (0..9)
                .filter(|bit| mask & (1 << bit) != 0)
                .map(|bit| bit as u64)
                .collect();
            assert_matches_naive(&values);
        }
    }

    #[test]
    fn logical_equivalence_is_exhaustive_across_small_cross_arm_subsets() {
        for mask in 0_u16..(1 << 10) {
            let values: Vec<u64> = (0..10)
                .filter(|bit| mask & (1 << bit) != 0)
                .map(|bit| bit as u64)
                .collect();
            assert_cross_arm_equivalent(&values);
        }
    }

    #[test]
    fn logical_equivalence_matches_deterministic_randomized_cross_arm_inputs() {
        let mut state = 0x0ddc_0ffe_e15e_baad_u64;
        for case in 0..128 {
            let len = usize::try_from(
                next_deterministic_random(&mut state)
                    % u64::try_from(3 * STREAM_BLOCK_ENTRIES + 19).unwrap(),
            )
            .unwrap();
            let mut values = Vec::with_capacity(len);
            let mut value = next_deterministic_random(&mut state) % 97;
            for index in 0..len {
                if index != 0 {
                    value = value
                        .checked_add(next_deterministic_random(&mut state) % 1_009 + 1)
                        .unwrap();
                }
                values.push(value);
            }
            assert_cross_arm_equivalent(&values);
            assert_eq!(values.len(), len, "randomized case {case}");
        }
    }

    #[test]
    fn neighbor_cursor_crosses_block_and_full_width_boundaries_for_every_arm() {
        let mut values = (0..(2 * STREAM_BLOCK_ENTRIES + 3))
            .map(|value| (value as u64) * 3)
            .collect::<Vec<_>>();
        values.push(u64::MAX);

        for encoded in encodings(&values) {
            assert_eq!(drain_cursor(&encoded), values, "{:?}", encoded.codec());
        }
        for encoded in encodings(&[]) {
            assert!(drain_cursor(&encoded).is_empty(), "{:?}", encoded.codec());
        }
    }

    #[test]
    fn neighbor_cursor_uses_linear_native_elias_fano_and_dense_state() -> Result<(), &'static str> {
        let values = (0..4_097_u64).map(|value| value * 2).collect::<Vec<_>>();
        let encoded = encodings(&values);

        reset_random_access_select_attempts();
        let mut elias_cursor = encoded[0].cursor();
        for &expected in &values {
            assert_eq!(elias_cursor.try_next(), Ok(Some(expected)));
        }
        assert_eq!(elias_cursor.try_next(), Ok(None));
        assert_eq!(
            random_access_select_attempts(NeighborCodec::EliasFano),
            0,
            "sequential Elias-Fano traversal must not re-enter logarithmic select"
        );
        let NeighborCursorState::EliasFano(cursor) = &elias_cursor.state else {
            return Err("explicit Elias-Fano representation returned another cursor arm");
        };
        assert_eq!(cursor.len(), 0);

        reset_random_access_select_attempts();
        let mut dense_cursor = encoded[2].cursor();
        for &expected in &values {
            assert_eq!(dense_cursor.try_next(), Ok(Some(expected)));
        }
        assert_eq!(dense_cursor.try_next(), Ok(None));
        assert_eq!(
            random_access_select_attempts(NeighborCodec::DenseIntervals),
            0,
            "sequential dense traversal must not re-enter logarithmic select"
        );
        let NeighborCursorState::DenseIntervals(cursor) = &dense_cursor.state else {
            return Err("explicit dense representation returned another cursor arm");
        };
        assert_eq!(cursor.values_yielded, values.len());
        assert_eq!(
            cursor.interval_advances + 1,
            cursor.values.intervals().len(),
            "each dense interval boundary must be crossed exactly once"
        );
        Ok(())
    }

    #[test]
    fn logical_equivalence_reports_first_value_and_length_mismatches() {
        let left = encodings(&[2, 4, 6, 8]);
        let right = encodings(&[2, 4, 7, 8]);
        for left_encoding in &left {
            for right_encoding in &right {
                assert_eq!(
                    left_encoding.verify_logical_equivalence(right_encoding),
                    Err(NeighborEquivalenceError::ValueMismatch {
                        index: 2,
                        left: Some(6),
                        right: Some(7),
                    }),
                    "{:?} x {:?}",
                    left_encoding.codec(),
                    right_encoding.codec()
                );
            }
        }

        let short = EncodedNeighbors::try_stream_vbyte(&[2, 4], EntryLimit::new(2)).unwrap();
        let long = EncodedNeighbors::try_dense_intervals(&[2, 4, 6], EntryLimit::new(3)).unwrap();
        assert_eq!(
            short.verify_logical_equivalence(&long),
            Err(NeighborEquivalenceError::ValueMismatch {
                index: 2,
                left: None,
                right: Some(6),
            })
        );
        assert_eq!(
            long.verify_logical_equivalence(&short),
            Err(NeighborEquivalenceError::ValueMismatch {
                index: 2,
                left: Some(6),
                right: None,
            })
        );
    }

    #[test]
    fn logical_equivalence_preserves_typed_internal_representation_failures() {
        let mut malformed = StreamVByteNeighbors::try_new(&[7, 11], EntryLimit::new(2)).unwrap();
        malformed.fences[0].entry_count = 0;
        let malformed = EncodedNeighbors::StreamVByte(malformed);
        let valid = EncodedNeighbors::try_dense_intervals(&[7, 11], EntryLimit::new(2)).unwrap();

        assert!(matches!(
            malformed.verify_logical_equivalence(&valid),
            Err(NeighborEquivalenceError::Internal {
                side: NeighborEquivalenceSide::Left,
                codec: NeighborCodec::StreamVByte,
                index: 0,
                source: NeighborError::MalformedStream {
                    cause: MalformedStream::InvalidEntryCount { actual: 0 },
                    ..
                },
            })
        ));
        assert!(matches!(
            valid.verify_logical_equivalence(&malformed),
            Err(NeighborEquivalenceError::Internal {
                side: NeighborEquivalenceSide::Right,
                codec: NeighborCodec::StreamVByte,
                index: 0,
                source: NeighborError::MalformedStream {
                    cause: MalformedStream::InvalidEntryCount { actual: 0 },
                    ..
                },
            })
        ));
    }

    #[test]
    fn neighbor_cursor_preserves_stream_errors_and_fuses_after_partial_decode() {
        let values = (0..(STREAM_BLOCK_ENTRIES + 1))
            .map(|value| (value as u64) * 2)
            .collect::<Vec<_>>();
        let mut malformed =
            StreamVByteNeighbors::try_new(&values, EntryLimit::new(values.len())).unwrap();
        malformed.fences[1].entry_count = 0;
        let malformed = EncodedNeighbors::StreamVByte(malformed);
        let mut cursor = malformed.cursor();

        reset_stream_block_decode_attempts();
        for &expected in &values[..STREAM_BLOCK_ENTRIES] {
            assert_eq!(cursor.try_next(), Ok(Some(expected)));
        }
        assert!(matches!(
            cursor.try_next(),
            Err(NeighborError::MalformedStream {
                block_index: 1,
                cause: MalformedStream::InvalidEntryCount { actual: 0 },
                ..
            })
        ));
        assert_eq!(cursor.logical_index(), STREAM_BLOCK_ENTRIES);
        assert_eq!(cursor.remaining(), 0);
        assert!(cursor.is_fused());
        assert_eq!(cursor.try_next(), Ok(None));
        assert_eq!(cursor.try_next(), Ok(None));
        assert_eq!(
            stream_block_decode_attempts(),
            2,
            "the cursor must decode the first block once and attempt the malformed block once"
        );
    }

    #[test]
    fn logical_equivalence_decodes_each_visited_stream_block_once_per_side() {
        let values: Vec<u64> = (0..(3 * STREAM_BLOCK_ENTRIES + 17))
            .map(|value| (value as u64) * 2)
            .collect();
        let left =
            EncodedNeighbors::try_stream_vbyte(&values, EntryLimit::new(values.len())).unwrap();
        let right =
            EncodedNeighbors::try_stream_vbyte(&values, EntryLimit::new(values.len())).unwrap();
        let block_count = values.len().div_ceil(STREAM_BLOCK_ENTRIES);

        reset_stream_block_decode_attempts();
        assert_eq!(left.verify_logical_equivalence(&right), Ok(()));
        assert_eq!(
            stream_block_decode_attempts(),
            2 * block_count,
            "each complete input must decode each block exactly once"
        );

        let dense =
            EncodedNeighbors::try_dense_intervals(&values, EntryLimit::new(values.len())).unwrap();
        reset_stream_block_decode_attempts();
        assert_eq!(left.verify_logical_equivalence(&dense), Ok(()));
        assert_eq!(
            stream_block_decode_attempts(),
            block_count,
            "the single StreamVByte input must decode each block exactly once"
        );

        let mut changed = values.clone();
        changed[STREAM_BLOCK_ENTRIES + 7] += 1;
        let changed =
            EncodedNeighbors::try_stream_vbyte(&changed, EntryLimit::new(changed.len())).unwrap();
        reset_stream_block_decode_attempts();
        assert_eq!(
            left.verify_logical_equivalence(&changed),
            Err(NeighborEquivalenceError::ValueMismatch {
                index: STREAM_BLOCK_ENTRIES + 7,
                left: Some(2 * (STREAM_BLOCK_ENTRIES as u64 + 7)),
                right: Some(2 * (STREAM_BLOCK_ENTRIES as u64 + 7) + 1),
            })
        );
        assert_eq!(
            stream_block_decode_attempts(),
            4,
            "a second-block mismatch must decode only two blocks per side"
        );
    }

    #[test]
    fn stream_blocks_are_deterministic_and_cross_skip_boundaries() {
        let values: Vec<u64> = (0..390_u64)
            .map(|index| {
                index
                    .checked_mul(index % 17 + 1)
                    .and_then(|value| value.checked_add(index))
                    .unwrap()
            })
            .scan(None, |previous, candidate| {
                let value = previous.map_or(candidate, |old| cmp::max(candidate, old + 1));
                *previous = Some(value);
                Some(value)
            })
            .collect();
        let first = StreamVByteNeighbors::try_new(&values, EntryLimit::new(values.len())).unwrap();
        let second = StreamVByteNeighbors::try_new(&values, EntryLimit::new(values.len())).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.fences().len(), 4);
        for index in [0, 1, 126, 127, 128, 129, 254, 255, 256, 257, 389] {
            assert_eq!(first.select(index), values.get(index).copied());
        }
        for (block_index, fence) in first.fences().iter().copied().enumerate() {
            assert_eq!(fence.logical_start(), block_index * STREAM_BLOCK_ENTRIES);
            assert!(fence.entry_count() <= STREAM_BLOCK_ENTRIES);
            if block_index > 0 {
                let prior = first.fences()[block_index - 1];
                assert_eq!(fence.byte_offset(), prior.byte_offset() + prior.byte_len());
            }
        }
        first.validate().unwrap();
    }

    #[test]
    fn stream_handles_full_width_components_and_typed_malformed_bytes() {
        let values = [0, 1, 256, 1_u64 << 40, u64::MAX];
        let encoded =
            StreamVByteNeighbors::try_new(&values, EntryLimit::new(values.len())).unwrap();
        assert_matches_naive(&values);
        assert_eq!(encoded.select(values.len() - 1), Some(u64::MAX));

        let mut malformed = StreamVByteNeighbors::try_new(&[7], EntryLimit::new(1)).unwrap();
        malformed.bytes[0] |= 0xf0;
        assert!(matches!(
            malformed.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::NonZeroUnusedControl { .. },
                ..
            })
        ));

        let mut invalid_width = StreamVByteNeighbors::try_new(&[7], EntryLimit::new(1)).unwrap();
        invalid_width.bytes[0] = 0x08;
        assert!(matches!(
            invalid_width.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::InvalidComponentWidth {
                    encoded: 9,
                    maximum: 8,
                },
                ..
            })
        ));

        let mut truncated = encoded.clone();
        truncated.bytes.pop();
        assert!(matches!(
            truncated.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::ByteRangeOutOfBounds { .. },
                ..
            })
        ));
    }

    #[test]
    fn stream_validation_rejects_every_private_invariant_violation() {
        let valid = StreamVByteNeighbors::try_new(&[7], EntryLimit::new(1)).unwrap();

        let mut fence_start = valid.clone();
        fence_start.fences[0].logical_start = 1;
        assert!(matches!(
            fence_start.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::FenceStart {
                    expected: 0,
                    actual: 1,
                },
                ..
            })
        ));

        let mut invalid_count = valid.clone();
        invalid_count.fences[0].entry_count = 0;
        assert!(matches!(
            invalid_count.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::InvalidEntryCount { actual: 0 },
                ..
            })
        ));

        let mut noncontiguous = valid.clone();
        noncontiguous.fences[0].byte_offset = 1;
        assert!(matches!(
            noncontiguous.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::NonContiguousBytes {
                    expected: 0,
                    actual: 1,
                },
                ..
            })
        ));

        let mut truncated_component = valid.clone();
        truncated_component.bytes[0] = 0x07;
        assert!(matches!(
            truncated_component.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::TruncatedComponent {
                    width: 8,
                    available: 1,
                },
                ..
            })
        ));

        let mut noncanonical = StreamVByteNeighbors::try_new(&[256], EntryLimit::new(1)).unwrap();
        noncanonical.bytes[1] = 7;
        noncanonical.bytes[2] = 0;
        assert!(matches!(
            noncanonical.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::NonCanonicalWidth {
                    component: 7,
                    encoded: 2,
                    canonical: 1,
                },
                ..
            })
        ));

        let mut zero_delta = StreamVByteNeighbors::try_new(&[7, 8], EntryLimit::new(2)).unwrap();
        zero_delta.bytes[2] = 0;
        assert!(matches!(
            zero_delta.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::ZeroDelta,
                ..
            })
        ));

        let mut value_overflow =
            StreamVByteNeighbors::try_new(&[u64::MAX - 1, u64::MAX], EntryLimit::new(2)).unwrap();
        value_overflow.bytes[9] = 2;
        assert!(matches!(
            value_overflow.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::ValueOverflow {
                    previous,
                    delta,
                },
                ..
            }) if previous == u64::MAX - 1 && delta == 2
        ));

        let mut trailing_block = valid.clone();
        trailing_block.bytes.push(0);
        trailing_block.fences[0].byte_len += 1;
        assert!(matches!(
            trailing_block.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::TrailingBlockBytes { trailing: 1 },
                ..
            })
        ));

        let mut fence_values = valid.clone();
        fence_values.fences[0].first = 8;
        assert!(matches!(
            fence_values.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::FenceValues { .. },
                ..
            })
        ));

        let mut logical_length = valid.clone();
        logical_length.len = 2;
        assert!(matches!(
            logical_length.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::LogicalLength {
                    expected: 2,
                    actual: 1,
                },
                ..
            })
        ));

        let mut cross_block =
            StreamVByteNeighbors::try_new(&(0..=128_u64).collect::<Vec<_>>(), EntryLimit::new(129))
                .unwrap();
        let second_payload = cross_block.fences[1].byte_offset + 1;
        cross_block.bytes[second_payload] = 127;
        cross_block.fences[1].first = 127;
        cross_block.fences[1].last = 127;
        assert!(matches!(
            cross_block.validate(),
            Err(NeighborError::MalformedStream {
                block_index: 1,
                cause: MalformedStream::CrossBlockOrder {
                    previous_last: 127,
                    next_first: 127,
                },
                ..
            })
        ));

        let mut trailing_stream = valid;
        trailing_stream.bytes.push(0);
        assert!(matches!(
            trailing_stream.validate(),
            Err(NeighborError::MalformedStream {
                cause: MalformedStream::TrailingStreamBytes { trailing: 1 },
                ..
            })
        ));
    }

    #[test]
    fn dense_intervals_cover_boundaries_and_u64_max() {
        let values = [0, 1, 2, 4, 5, 99, u64::MAX - 1, u64::MAX];
        let encoded = DenseIntervals::try_new(&values, EntryLimit::new(values.len())).unwrap();
        let bounds: Vec<(u64, u64)> = encoded
            .intervals()
            .iter()
            .map(|interval| (interval.start(), interval.end()))
            .collect();
        assert_eq!(
            bounds,
            vec![(0, 2), (4, 5), (99, 99), (u64::MAX - 1, u64::MAX)]
        );
        assert_matches_naive(&values);
    }

    #[test]
    fn dense_interval_allocation_tracks_runs_not_input_entries() {
        let values: Vec<u64> = (0..1_000_000).collect();
        let encoded = DenseIntervals::try_new(&values, EntryLimit::new(values.len())).unwrap();
        assert_eq!(encoded.intervals().len(), 1);
        assert!(
            encoded.retained_interval_capacity() <= 4,
            "one dense run retained capacity for {} intervals",
            encoded.retained_interval_capacity()
        );
        assert_eq!(encoded.select(values.len() - 1), Some(999_999));
    }

    #[test]
    fn every_cross_codec_intersection_matches_naive() {
        let left_values = [0, 1, 2, 9, 10, 11, 127, 128, 129, u64::MAX];
        let right_values = [1, 3, 9, 11, 12, 128, 130, u64::MAX];
        let expected = vec![1, 9, 11, 128, u64::MAX];
        let left = encodings(&left_values);
        let right = encodings(&right_values);
        for left_encoding in &left {
            for right_encoding in &right {
                assert_eq!(
                    left_encoding.intersection(right_encoding, EntryLimit::new(expected.len())),
                    Ok(expected.clone()),
                    "{:?} x {:?}",
                    left_encoding.codec(),
                    right_encoding.codec()
                );
                assert_eq!(
                    right_encoding.intersection(left_encoding, EntryLimit::new(expected.len())),
                    Ok(expected.clone())
                );
            }
        }
    }

    #[test]
    fn intersection_limit_is_checked_before_reservation_and_exact_limit_succeeds() {
        let values = [1, 2, 3];
        for encoded in encodings(&values) {
            assert_eq!(
                encoded.intersection(&encoded, EntryLimit::new(2)),
                Err(NeighborError::IntersectionLimitExceeded { limit: 2 })
            );
        }
        let disjoint = EncodedNeighbors::try_stream_vbyte(&[8, 9], EntryLimit::new(2)).unwrap();
        let left = EncodedNeighbors::try_elias_fano(&[1, 2, 3], EntryLimit::new(3)).unwrap();
        assert_eq!(
            left.intersection(&disjoint, EntryLimit::new(0)),
            Ok(Vec::new())
        );

        let exact_left =
            EncodedNeighbors::try_stream_vbyte(&[1, 2, 3, 4], EntryLimit::new(4)).unwrap();
        let exact_right =
            EncodedNeighbors::try_dense_intervals(&[2, 4, 6], EntryLimit::new(3)).unwrap();
        assert_eq!(
            exact_left.intersection(&exact_right, EntryLimit::new(2)),
            Ok(vec![2, 4])
        );
    }

    #[test]
    fn disjoint_large_intersections_reserve_no_result_storage() {
        let left_values: Vec<u64> = (0..100_000).collect();
        let right_values: Vec<u64> = (200_000..300_000).collect();

        let left_stream =
            StreamVByteNeighbors::try_new(&left_values, EntryLimit::new(left_values.len()))
                .unwrap();
        let right_elias =
            EliasFanoNeighbors::try_new(&right_values, EntryLimit::new(right_values.len()))
                .unwrap();
        assert_eq!(
            sequence_intersection_cardinality(&left_stream, &right_elias, EntryLimit::new(0)),
            Ok(0)
        );
        let generic = intersect_sequences(&left_stream, &right_elias, EntryLimit::new(0)).unwrap();
        assert!(generic.is_empty());
        assert_eq!(generic.capacity(), 0);

        let left_dense =
            DenseIntervals::try_new(&left_values, EntryLimit::new(left_values.len())).unwrap();
        let right_dense =
            DenseIntervals::try_new(&right_values, EntryLimit::new(right_values.len())).unwrap();
        assert_eq!(
            dense_intersection_cardinality(&left_dense, &right_dense, EntryLimit::new(0)),
            Ok(0)
        );
        let dense =
            intersect_dense_intervals(&left_dense, &right_dense, EntryLimit::new(0)).unwrap();
        assert!(dense.is_empty());
        assert_eq!(dense.capacity(), 0);
    }

    #[test]
    fn sequence_cursor_retains_the_unadvanced_value_and_skips_empty_replay() {
        let left_values: Vec<u64> = (0..1_024).collect();
        let right_values = [10_000];
        let counted_left = CountingSequence {
            values: &left_values,
            selects: core::cell::Cell::new(0),
            codec: NeighborCodec::EliasFano,
        };
        let counted_right = CountingSequence {
            values: &right_values,
            selects: core::cell::Cell::new(0),
            codec: NeighborCodec::DenseIntervals,
        };

        let output =
            intersect_sequences(&counted_left, &counted_right, EntryLimit::new(0)).unwrap();

        assert!(output.is_empty());
        assert_eq!(output.capacity(), 0);
        assert_eq!(counted_left.selects.get(), left_values.len());
        assert_eq!(counted_right.selects.get(), 1);
    }

    #[test]
    fn stream_intersection_decodes_each_block_at_most_once_per_merge_pass() {
        let disjoint_left_values: Vec<u64> = (0..(3 * STREAM_BLOCK_ENTRIES + 7))
            .map(|value| value as u64)
            .collect();
        let disjoint_right_values = [10_000_u64];
        let disjoint_left = StreamVByteNeighbors::try_new(
            &disjoint_left_values,
            EntryLimit::new(disjoint_left_values.len()),
        )
        .unwrap();
        let disjoint_right = StreamVByteNeighbors::try_new(
            &disjoint_right_values,
            EntryLimit::new(disjoint_right_values.len()),
        )
        .unwrap();

        reset_stream_block_decode_attempts();
        let output =
            intersect_sequences(&disjoint_left, &disjoint_right, EntryLimit::new(0)).unwrap();
        assert!(output.is_empty());
        assert_eq!(
            stream_block_decode_attempts(),
            disjoint_left.fences().len() + 1,
            "an empty result must perform one merge pass and decode each visited block once"
        );

        let identical_values: Vec<u64> = (0..(2 * STREAM_BLOCK_ENTRIES + 1))
            .map(|value| value as u64)
            .collect();
        let identical_left = StreamVByteNeighbors::try_new(
            &identical_values,
            EntryLimit::new(identical_values.len()),
        )
        .unwrap();
        let identical_right = StreamVByteNeighbors::try_new(
            &identical_values,
            EntryLimit::new(identical_values.len()),
        )
        .unwrap();

        reset_stream_block_decode_attempts();
        assert_eq!(
            intersect_sequences(
                &identical_left,
                &identical_right,
                EntryLimit::new(identical_values.len()),
            ),
            Ok(identical_values)
        );
        assert_eq!(
            stream_block_decode_attempts(),
            2 * (identical_left.fences().len() + identical_right.fences().len()),
            "a nonempty exact-allocation result performs two passes with one decode per block"
        );
    }

    #[test]
    fn zero_limit_stops_cardinality_work_after_first_identical_match() {
        let values: Vec<u64> = (0..100_000).map(|value| value * 2).collect();
        let counted_left = CountingSequence {
            values: &values,
            selects: core::cell::Cell::new(0),
            codec: NeighborCodec::StreamVByte,
        };
        let counted_right = CountingSequence {
            values: &values,
            selects: core::cell::Cell::new(0),
            codec: NeighborCodec::EliasFano,
        };
        assert_eq!(
            sequence_intersection_cardinality(&counted_left, &counted_right, EntryLimit::new(0)),
            Err(NeighborError::IntersectionLimitExceeded { limit: 0 })
        );
        assert_eq!(counted_left.selects.get(), 1);
        assert_eq!(counted_right.selects.get(), 1);

        let stream = StreamVByteNeighbors::try_new(&values, EntryLimit::new(values.len())).unwrap();
        let elias = EliasFanoNeighbors::try_new(&values, EntryLimit::new(values.len())).unwrap();
        assert_eq!(
            intersect_sequences(&stream, &elias, EntryLimit::new(0)),
            Err(NeighborError::IntersectionLimitExceeded { limit: 0 })
        );

        let dense_left = DenseIntervals::try_new(&values, EntryLimit::new(values.len())).unwrap();
        let dense_right = DenseIntervals::try_new(&values, EntryLimit::new(values.len())).unwrap();
        let mut inspected_pairs = 0_usize;
        assert_eq!(
            dense_intersection_cardinality_with(
                &dense_left,
                &dense_right,
                EntryLimit::new(0),
                || inspected_pairs += 1,
            ),
            Err(NeighborError::IntersectionLimitExceeded { limit: 0 })
        );
        assert_eq!(inspected_pairs, 1);
        assert_eq!(
            intersect_dense_intervals(&dense_left, &dense_right, EntryLimit::new(0)),
            Err(NeighborError::IntersectionLimitExceeded { limit: 0 })
        );
    }
}
