//! Honest dispatch seams for the safe codec kernels.
//!
//! This module currently exposes exactly one implementation path:
//! [`DispatchPath::Scalar`]. It does not probe the host or imply that a SIMD
//! implementation exists. Future dispatch work can add a separately verified
//! path without changing callers from direct function calls a second time.
//!
//! These traits select reusable kernel mechanics only. They do not select a
//! durable codec identifier, format envelope, or adaptive policy.

#![forbid(unsafe_code)]

use core::fmt;

use crate::{bitpack, block, delta_varint, elias_fano, identity, neighbor, roaring, varint};

const STREAM_ACCOUNTING_MAGIC: &[u8] = b"FGDB-STREAM-ACCOUNTING-V1\0";
const STREAM_ACCOUNTING_FENCE_FIELDS: usize = 6;

mod private {
    pub trait Sealed {}
}

/// Implementation path used by one codec operation.
///
/// The closed enum is intentionally honest about the implementation currently
/// available in this crate.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DispatchPath {
    /// Portable, safe scalar Rust.
    Scalar,
}

impl DispatchPath {
    /// Returns the stable symbolic label used in diagnostic evidence.
    ///
    /// This label is not a durable codec identifier.
    #[must_use]
    pub const fn evidence_label(self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
        }
    }
}

/// Common implementation-path identity shared by every codec kernel seam.
///
/// A single associated constant prevents one implementation from labeling
/// different codec families with contradictory dispatch paths. Diagnostic
/// evidence can bind to this trait instead of accepting a caller-supplied
/// path label.
pub trait KernelDispatch: private::Sealed {
    /// Implementation path selected by this kernel set.
    const DISPATCH_PATH: DispatchPath;

    /// Returns the selected implementation path.
    #[must_use]
    fn dispatch_path(&self) -> DispatchPath {
        Self::DISPATCH_PATH
    }
}

/// Owned bytes inseparably tagged with the kernel path that produced them.
///
/// Construction is private to this module, and the dispatch traits are sealed.
/// Evidence consumers can therefore read the bytes and path but cannot attach
/// an arbitrary path label to unrelated bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelOutput {
    bytes: Vec<u8>,
    dispatch_path: DispatchPath,
}

impl KernelOutput {
    fn new<K: KernelDispatch + ?Sized>(kernel: &K, bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            dispatch_path: kernel.dispatch_path(),
        }
    }

    /// Exact bytes produced by the selected kernel path.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Exact output byte count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the selected kernel produced no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Dispatch path that produced these exact bytes.
    #[must_use]
    pub const fn dispatch_path(&self) -> DispatchPath {
        self.dispatch_path
    }

    /// Consumes the provenance wrapper and returns the exact bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Checked construction failure for a registry-independent diagnostic output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticOutputError {
    /// Exact output-size arithmetic overflowed.
    LengthOverflow,
    /// A host-sized count could not be represented by the diagnostic grammar.
    CountNotRepresentable {
        /// Rejected count.
        value: usize,
    },
    /// The exact output exceeds the caller-selected byte ceiling.
    OutputLimitExceeded {
        /// Exact bytes required.
        required: usize,
        /// Caller-selected ceiling.
        limit: usize,
    },
    /// Reserving the already-bounded output failed.
    AllocationFailed {
        /// Exact bytes requested.
        requested: usize,
    },
    /// Materialized bytes disagreed with checked preflight accounting.
    LengthInvariant {
        /// Exact bytes calculated before allocation.
        expected: usize,
        /// Bytes actually materialized.
        actual: usize,
    },
}

impl fmt::Display for DiagnosticOutputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::LengthOverflow => {
                formatter.write_str("diagnostic kernel-output length overflows usize")
            }
            Self::CountNotRepresentable { value } => write!(
                formatter,
                "diagnostic kernel-output count {value} does not fit u64"
            ),
            Self::OutputLimitExceeded { required, limit } => write!(
                formatter,
                "diagnostic kernel output needs {required} bytes, limit is {limit}"
            ),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve {requested} bytes for diagnostic kernel output"
            ),
            Self::LengthInvariant { expected, actual } => write!(
                formatter,
                "diagnostic kernel output materialized {actual} bytes, expected {expected}"
            ),
        }
    }
}

impl std::error::Error for DiagnosticOutputError {}

/// Canonical unsigned LEB128 operations for individual `u64` values.
pub trait VarintKernel: KernelDispatch {
    /// Encodes one value into the allocation-free canonical representation.
    fn encode_varint(&self, value: u64) -> varint::EncodedU64;

    /// Writes one canonical value into caller-owned storage.
    fn write_varint(
        &self,
        value: u64,
        output: &mut [u8],
    ) -> Result<usize, varint::VarintEncodeError>;

    /// Decodes one exact canonical value and rejects trailing bytes.
    fn decode_varint(&self, input: &[u8]) -> Result<u64, varint::VarintDecodeError>;

    /// Decodes one canonical prefix and returns its consumed byte count.
    fn decode_varint_prefix(&self, input: &[u8])
    -> Result<(u64, usize), varint::VarintDecodeError>;
}

/// Checked fixed-width and frame-of-reference bitpacking operations.
pub trait BitpackKernel: KernelDispatch {
    /// Encodes fixed-width values into canonical bytes.
    fn encode(&self, values: &[u64], width: u8) -> Result<Vec<u8>, bitpack::BitpackError>;

    /// Encodes fixed-width values into caller-owned storage.
    fn encode_into(
        &self,
        values: &[u64],
        width: u8,
        output: &mut [u8],
    ) -> Result<usize, bitpack::BitpackError>;

    /// Decodes exactly `count` fixed-width values.
    fn decode(
        &self,
        input: &[u8],
        count: usize,
        width: u8,
    ) -> Result<Vec<u64>, bitpack::BitpackError>;

    /// Frame-of-reference encodes values relative to `base`.
    fn encode_for(
        &self,
        values: &[u64],
        base: u64,
        width: u8,
    ) -> Result<Vec<u8>, bitpack::BitpackError>;

    /// Decodes frame-of-reference values relative to `base`.
    fn decode_for(
        &self,
        input: &[u8],
        count: usize,
        base: u64,
        width: u8,
    ) -> Result<Vec<u64>, bitpack::BitpackError>;
}

/// Canonical delta-varint operations for nondecreasing `u64` sequences.
pub trait DeltaVarintKernel: KernelDispatch {
    /// Encodes values using the canonical absolute-first delta-varint form.
    fn encode_delta_varint(
        &self,
        values: &[u64],
    ) -> Result<Vec<u8>, delta_varint::DeltaVarintEncodeError>;

    /// Decodes exactly `count` values under an explicit materialization bound.
    fn decode_delta_varint(
        &self,
        input: &[u8],
        count: usize,
        limit: delta_varint::EntryLimit,
    ) -> Result<Vec<u64>, delta_varint::DeltaVarintDecodeError>;
}

/// Checked construction and access for the scalar Elias-Fano representation.
pub trait EliasFanoKernel: KernelDispatch {
    /// Constructs the canonical scalar Elias-Fano representation.
    fn build_elias_fano(
        &self,
        values: &[u64],
        limit: elias_fano::EntryLimit,
    ) -> Result<elias_fano::EliasFano, elias_fano::EliasFanoError>;

    /// Returns the value at `index`, if present.
    fn elias_fano_select(&self, values: &elias_fano::EliasFano, index: usize) -> Option<u64>;

    /// Counts represented values less than or equal to `value`.
    fn elias_fano_rank_le(&self, values: &elias_fano::EliasFano, value: u64) -> usize;

    /// Counts represented values strictly less than `value`.
    fn elias_fano_rank_lt(&self, values: &elias_fano::EliasFano, value: u64) -> usize;

    /// Returns the last represented value not greater than `value`.
    fn elias_fano_predecessor(&self, values: &elias_fano::EliasFano, value: u64) -> Option<u64>;

    /// Returns the first represented value not less than `value`.
    fn elias_fano_successor(&self, values: &elias_fano::EliasFano, value: u64) -> Option<u64>;
}

/// Checked construction and set operations for scalar roaring-like bitmaps.
pub trait RoaringKernel: KernelDispatch {
    /// Constructs a canonical bitmap from strictly increasing values.
    fn build_roaring(
        &self,
        values: &[u32],
        limit: roaring::EntryLimit,
    ) -> Result<roaring::RoaringBitmap, roaring::RoaringError>;

    /// Returns whether `value` belongs to the set.
    fn roaring_contains(&self, values: &roaring::RoaringBitmap, value: u32) -> bool;

    /// Counts represented values less than or equal to `value`.
    fn roaring_rank_le(&self, values: &roaring::RoaringBitmap, value: u32) -> usize;

    /// Returns the value at `index`, if present.
    fn roaring_select(&self, values: &roaring::RoaringBitmap, index: usize) -> Option<u32>;

    /// Computes a canonical set intersection under an explicit result bound.
    fn roaring_intersection(
        &self,
        left: &roaring::RoaringBitmap,
        right: &roaring::RoaringBitmap,
        limit: roaring::EntryLimit,
    ) -> Result<roaring::RoaringBitmap, roaring::RoaringError>;
}

/// Explicit scalar neighbor representation construction and access.
///
/// The caller supplies the [`neighbor::NeighborCodec`] arm. This trait performs
/// no adaptive selection and the arm remains an in-memory capability tag, not
/// a durable codec identifier.
pub trait NeighborKernel: KernelDispatch {
    /// Constructs exactly the requested scalar representation arm.
    fn build_neighbors(
        &self,
        codec: neighbor::NeighborCodec,
        values: &[u64],
        limit: neighbor::EntryLimit,
    ) -> Result<neighbor::EncodedNeighbors, neighbor::NeighborError>;

    /// Returns whether `value` belongs to the neighbor list.
    fn neighbors_contains(&self, values: &neighbor::EncodedNeighbors, value: u64) -> bool;

    /// Counts represented neighbors less than or equal to `value`.
    fn neighbors_rank_le(&self, values: &neighbor::EncodedNeighbors, value: u64) -> usize;

    /// Returns the neighbor at `index`, if present.
    fn neighbors_select(&self, values: &neighbor::EncodedNeighbors, index: usize) -> Option<u64>;

    /// Materializes a sorted intersection under an explicit result bound.
    fn neighbors_intersection(
        &self,
        left: &neighbor::EncodedNeighbors,
        right: &neighbor::EncodedNeighbors,
        limit: neighbor::EntryLimit,
    ) -> Result<Vec<u64>, neighbor::NeighborError>;

    /// Encodes complete StreamVByte payload/fence accounting for diagnostics.
    ///
    /// This versionless transcript is not a durable codec envelope. The
    /// returned bytes are provenance-bound to this kernel path so structured
    /// evidence cannot relabel caller-supplied bytes.
    fn stream_vbyte_accounting_output(
        &self,
        stream: &neighbor::StreamVByteNeighbors,
        max_output_bytes: usize,
    ) -> Result<KernelOutput, DiagnosticOutputError>;
}

/// Checked construction and typed access for scalar identity columns.
///
/// This seam exposes the registry-independent in-memory representation only.
/// It does not encode durable identity bytes, framing, or codec identifiers.
pub trait IdentityColumnKernel: KernelDispatch {
    /// Constructs an identity column while preserving arbitrary row order.
    fn build_identity_column<T: identity::ElementIdentity>(
        &self,
        values: &[T],
        limits: identity::IdentityColumnLimits,
    ) -> Result<identity::IdentityColumn<T>, identity::IdentityColumnError>;

    /// Constructs an identity column after validating monotone row order.
    fn build_sorted_identity_column<T: identity::ElementIdentity>(
        &self,
        values: &[T],
        limits: identity::IdentityColumnLimits,
    ) -> Result<identity::SortedIdentityColumn<T>, identity::IdentityColumnError>;

    /// Reconstructs one typed identity from an arbitrary-order column.
    fn identity_at<T: identity::ElementIdentity>(
        &self,
        column: &identity::IdentityColumn<T>,
        row: usize,
    ) -> Option<T>;

    /// Reconstructs one typed identity from a sorted column.
    fn sorted_identity_at<T: identity::ElementIdentity>(
        &self,
        column: &identity::SortedIdentityColumn<T>,
        row: usize,
    ) -> Option<T>;

    /// Returns the first row whose typed identity is not less than `probe`.
    fn identity_lower_bound<T: identity::ElementIdentity>(
        &self,
        column: &identity::SortedIdentityColumn<T>,
        probe: T,
    ) -> usize;

    /// Materializes the exact bounded scalar payload without durable framing.
    fn encode_identity_payload<T: identity::ElementIdentity>(
        &self,
        column: &identity::IdentityColumn<T>,
        max_output_bytes: usize,
    ) -> Result<KernelOutput, identity::IdentityColumnError>;
}

/// Checked deterministic block compression and decompression operations.
pub trait BlockKernel: KernelDispatch {
    /// Compresses one caller-framed block under an immutable scalar profile.
    fn compress(
        &self,
        input: &[u8],
        profile: block::CodecProfile,
    ) -> Result<Vec<u8>, block::CompressionError>;

    /// Compresses one block and binds the exact bytes to this dispatch path.
    fn compress_output(
        &self,
        input: &[u8],
        profile: block::CodecProfile,
    ) -> Result<KernelOutput, block::CompressionError>;

    /// Decompresses one token stream to an exact authenticated length.
    fn decompress(
        &self,
        input: &[u8],
        expected_decoded_len: usize,
        output_limit: block::OutputLimit,
    ) -> Result<Vec<u8>, block::DecodeError>;
}

/// Zero-sized selector for the crate's safe scalar kernels.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ScalarKernels;

impl ScalarKernels {
    /// Returns the only implementation path currently available.
    #[must_use]
    pub const fn dispatch_path(self) -> DispatchPath {
        <Self as KernelDispatch>::DISPATCH_PATH
    }
}

impl private::Sealed for ScalarKernels {}

impl KernelDispatch for ScalarKernels {
    const DISPATCH_PATH: DispatchPath = DispatchPath::Scalar;
}

impl VarintKernel for ScalarKernels {
    fn encode_varint(&self, value: u64) -> varint::EncodedU64 {
        varint::encode_u64(value)
    }

    fn write_varint(
        &self,
        value: u64,
        output: &mut [u8],
    ) -> Result<usize, varint::VarintEncodeError> {
        varint::write_u64(value, output)
    }

    fn decode_varint(&self, input: &[u8]) -> Result<u64, varint::VarintDecodeError> {
        varint::decode_u64(input)
    }

    fn decode_varint_prefix(
        &self,
        input: &[u8],
    ) -> Result<(u64, usize), varint::VarintDecodeError> {
        varint::decode_u64_prefix(input)
    }
}

impl BitpackKernel for ScalarKernels {
    fn encode(&self, values: &[u64], width: u8) -> Result<Vec<u8>, bitpack::BitpackError> {
        bitpack::encode(values, width)
    }

    fn encode_into(
        &self,
        values: &[u64],
        width: u8,
        output: &mut [u8],
    ) -> Result<usize, bitpack::BitpackError> {
        bitpack::encode_into(values, width, output)
    }

    fn decode(
        &self,
        input: &[u8],
        count: usize,
        width: u8,
    ) -> Result<Vec<u64>, bitpack::BitpackError> {
        bitpack::decode(input, count, width)
    }

    fn encode_for(
        &self,
        values: &[u64],
        base: u64,
        width: u8,
    ) -> Result<Vec<u8>, bitpack::BitpackError> {
        bitpack::encode_for(values, base, width)
    }

    fn decode_for(
        &self,
        input: &[u8],
        count: usize,
        base: u64,
        width: u8,
    ) -> Result<Vec<u64>, bitpack::BitpackError> {
        bitpack::decode_for(input, count, base, width)
    }
}

impl DeltaVarintKernel for ScalarKernels {
    fn encode_delta_varint(
        &self,
        values: &[u64],
    ) -> Result<Vec<u8>, delta_varint::DeltaVarintEncodeError> {
        delta_varint::encode(values)
    }

    fn decode_delta_varint(
        &self,
        input: &[u8],
        count: usize,
        limit: delta_varint::EntryLimit,
    ) -> Result<Vec<u64>, delta_varint::DeltaVarintDecodeError> {
        delta_varint::decode(input, count, limit)
    }
}

impl EliasFanoKernel for ScalarKernels {
    fn build_elias_fano(
        &self,
        values: &[u64],
        limit: elias_fano::EntryLimit,
    ) -> Result<elias_fano::EliasFano, elias_fano::EliasFanoError> {
        elias_fano::EliasFano::try_new(values, limit)
    }

    fn elias_fano_select(&self, values: &elias_fano::EliasFano, index: usize) -> Option<u64> {
        values.select(index)
    }

    fn elias_fano_rank_le(&self, values: &elias_fano::EliasFano, value: u64) -> usize {
        values.rank_le(value)
    }

    fn elias_fano_rank_lt(&self, values: &elias_fano::EliasFano, value: u64) -> usize {
        values.rank_lt(value)
    }

    fn elias_fano_predecessor(&self, values: &elias_fano::EliasFano, value: u64) -> Option<u64> {
        values.predecessor(value)
    }

    fn elias_fano_successor(&self, values: &elias_fano::EliasFano, value: u64) -> Option<u64> {
        values.successor(value)
    }
}

impl RoaringKernel for ScalarKernels {
    fn build_roaring(
        &self,
        values: &[u32],
        limit: roaring::EntryLimit,
    ) -> Result<roaring::RoaringBitmap, roaring::RoaringError> {
        roaring::RoaringBitmap::try_from_sorted(values, limit)
    }

    fn roaring_contains(&self, values: &roaring::RoaringBitmap, value: u32) -> bool {
        values.contains(value)
    }

    fn roaring_rank_le(&self, values: &roaring::RoaringBitmap, value: u32) -> usize {
        values.rank_le(value)
    }

    fn roaring_select(&self, values: &roaring::RoaringBitmap, index: usize) -> Option<u32> {
        values.select(index)
    }

    fn roaring_intersection(
        &self,
        left: &roaring::RoaringBitmap,
        right: &roaring::RoaringBitmap,
        limit: roaring::EntryLimit,
    ) -> Result<roaring::RoaringBitmap, roaring::RoaringError> {
        left.intersection(right, limit)
    }
}

impl NeighborKernel for ScalarKernels {
    fn build_neighbors(
        &self,
        codec: neighbor::NeighborCodec,
        values: &[u64],
        limit: neighbor::EntryLimit,
    ) -> Result<neighbor::EncodedNeighbors, neighbor::NeighborError> {
        match codec {
            neighbor::NeighborCodec::EliasFano => {
                neighbor::EncodedNeighbors::try_elias_fano(values, limit)
            }
            neighbor::NeighborCodec::StreamVByte => {
                neighbor::EncodedNeighbors::try_stream_vbyte(values, limit)
            }
            neighbor::NeighborCodec::DenseIntervals => {
                neighbor::EncodedNeighbors::try_dense_intervals(values, limit)
            }
        }
    }

    fn neighbors_contains(&self, values: &neighbor::EncodedNeighbors, value: u64) -> bool {
        values.contains(value)
    }

    fn neighbors_rank_le(&self, values: &neighbor::EncodedNeighbors, value: u64) -> usize {
        values.rank_le(value)
    }

    fn neighbors_select(&self, values: &neighbor::EncodedNeighbors, index: usize) -> Option<u64> {
        values.select(index)
    }

    fn neighbors_intersection(
        &self,
        left: &neighbor::EncodedNeighbors,
        right: &neighbor::EncodedNeighbors,
        limit: neighbor::EntryLimit,
    ) -> Result<Vec<u64>, neighbor::NeighborError> {
        left.intersection(right, limit)
    }

    fn stream_vbyte_accounting_output(
        &self,
        stream: &neighbor::StreamVByteNeighbors,
        max_output_bytes: usize,
    ) -> Result<KernelOutput, DiagnosticOutputError> {
        let bytes = stream_vbyte_accounting_bytes(stream, max_output_bytes)?;
        Ok(KernelOutput::new(self, bytes))
    }
}

impl IdentityColumnKernel for ScalarKernels {
    fn build_identity_column<T: identity::ElementIdentity>(
        &self,
        values: &[T],
        limits: identity::IdentityColumnLimits,
    ) -> Result<identity::IdentityColumn<T>, identity::IdentityColumnError> {
        identity::IdentityColumn::try_new(values, limits)
    }

    fn build_sorted_identity_column<T: identity::ElementIdentity>(
        &self,
        values: &[T],
        limits: identity::IdentityColumnLimits,
    ) -> Result<identity::SortedIdentityColumn<T>, identity::IdentityColumnError> {
        identity::SortedIdentityColumn::try_new(values, limits)
    }

    fn identity_at<T: identity::ElementIdentity>(
        &self,
        column: &identity::IdentityColumn<T>,
        row: usize,
    ) -> Option<T> {
        column.get(row)
    }

    fn sorted_identity_at<T: identity::ElementIdentity>(
        &self,
        column: &identity::SortedIdentityColumn<T>,
        row: usize,
    ) -> Option<T> {
        column.get(row)
    }

    fn identity_lower_bound<T: identity::ElementIdentity>(
        &self,
        column: &identity::SortedIdentityColumn<T>,
        probe: T,
    ) -> usize {
        column.lower_bound(probe)
    }

    fn encode_identity_payload<T: identity::ElementIdentity>(
        &self,
        column: &identity::IdentityColumn<T>,
        max_output_bytes: usize,
    ) -> Result<KernelOutput, identity::IdentityColumnError> {
        column
            .try_scalar_payload(max_output_bytes)
            .map(|bytes| KernelOutput::new(self, bytes))
    }
}

impl BlockKernel for ScalarKernels {
    fn compress(
        &self,
        input: &[u8],
        profile: block::CodecProfile,
    ) -> Result<Vec<u8>, block::CompressionError> {
        block::compress(input, profile)
    }

    fn compress_output(
        &self,
        input: &[u8],
        profile: block::CodecProfile,
    ) -> Result<KernelOutput, block::CompressionError> {
        block::compress(input, profile).map(|bytes| KernelOutput::new(self, bytes))
    }

    fn decompress(
        &self,
        input: &[u8],
        expected_decoded_len: usize,
        output_limit: block::OutputLimit,
    ) -> Result<Vec<u8>, block::DecodeError> {
        block::decompress(input, expected_decoded_len, output_limit)
    }
}

fn stream_vbyte_accounting_bytes(
    stream: &neighbor::StreamVByteNeighbors,
    max_output_bytes: usize,
) -> Result<Vec<u8>, DiagnosticOutputError> {
    let fence_bytes = stream
        .fences()
        .len()
        .checked_mul(STREAM_ACCOUNTING_FENCE_FIELDS * core::mem::size_of::<u64>())
        .ok_or(DiagnosticOutputError::LengthOverflow)?;
    let capacity = STREAM_ACCOUNTING_MAGIC
        .len()
        .checked_add(2 * core::mem::size_of::<u64>())
        .and_then(|length| length.checked_add(stream.encoded_bytes().len()))
        .and_then(|length| length.checked_add(fence_bytes))
        .ok_or(DiagnosticOutputError::LengthOverflow)?;
    if capacity > max_output_bytes {
        return Err(DiagnosticOutputError::OutputLimitExceeded {
            required: capacity,
            limit: max_output_bytes,
        });
    }

    let encoded_len = diagnostic_u64(stream.encoded_bytes().len())?;
    let fence_count = diagnostic_u64(stream.fences().len())?;
    for fence in stream.fences() {
        let _ = diagnostic_u64(fence.logical_start())?;
        let _ = diagnostic_u64(fence.entry_count())?;
        let _ = diagnostic_u64(fence.byte_offset())?;
        let _ = diagnostic_u64(fence.byte_len())?;
    }

    let mut output = Vec::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| DiagnosticOutputError::AllocationFailed {
            requested: capacity,
        })?;
    output.extend_from_slice(STREAM_ACCOUNTING_MAGIC);
    output.extend_from_slice(&encoded_len.to_le_bytes());
    output.extend_from_slice(&fence_count.to_le_bytes());
    output.extend_from_slice(stream.encoded_bytes());
    for fence in stream.fences() {
        for count in [
            fence.logical_start(),
            fence.entry_count(),
            fence.byte_offset(),
            fence.byte_len(),
        ] {
            output.extend_from_slice(&diagnostic_u64(count)?.to_le_bytes());
        }
        output.extend_from_slice(&fence.first().to_le_bytes());
        output.extend_from_slice(&fence.last().to_le_bytes());
    }
    if output.len() != capacity {
        return Err(DiagnosticOutputError::LengthInvariant {
            expected: capacity,
            actual: output.len(),
        });
    }
    Ok(output)
}

fn diagnostic_u64(value: usize) -> Result<u64, DiagnosticOutputError> {
    u64::try_from(value).map_err(|_| DiagnosticOutputError::CountNotRepresentable { value })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::{EId, VId};

    const KERNELS: ScalarKernels = ScalarKernels;

    #[test]
    fn selector_is_zero_sized_and_reports_only_scalar() {
        fn assert_all_scalar_seams<T>()
        where
            T: VarintKernel
                + BitpackKernel
                + DeltaVarintKernel
                + EliasFanoKernel
                + RoaringKernel
                + NeighborKernel
                + IdentityColumnKernel
                + BlockKernel,
        {
        }

        assert_all_scalar_seams::<ScalarKernels>();
        assert_eq!(core::mem::size_of::<ScalarKernels>(), 0);
        assert_eq!(KERNELS.dispatch_path(), DispatchPath::Scalar);
        assert_eq!(
            <ScalarKernels as KernelDispatch>::DISPATCH_PATH,
            DispatchPath::Scalar
        );
        assert_eq!(DispatchPath::Scalar.evidence_label(), "scalar");
    }

    #[test]
    fn varint_trait_matches_direct_bytes_writes_decode_and_errors() {
        for value in [0_u64, 127, 128, 16_384, u64::MAX] {
            let direct = varint::encode_u64(value);
            let dispatched = VarintKernel::encode_varint(&KERNELS, value);
            assert_eq!(dispatched, direct);
            assert_eq!(
                VarintKernel::decode_varint(&KERNELS, dispatched.as_bytes()),
                varint::decode_u64(direct.as_bytes())
            );

            let mut direct_output = [0xa5_u8; varint::MAX_U64_VARINT_BYTES + 1];
            let mut dispatched_output = direct_output;
            assert_eq!(
                VarintKernel::write_varint(&KERNELS, value, &mut dispatched_output),
                varint::write_u64(value, &mut direct_output)
            );
            assert_eq!(dispatched_output, direct_output);
        }

        assert_eq!(
            VarintKernel::decode_varint_prefix(&KERNELS, &[0xac, 0x02, 0xff]),
            varint::decode_u64_prefix(&[0xac, 0x02, 0xff])
        );
        assert_eq!(
            VarintKernel::decode_varint(&KERNELS, &[0x80, 0x00]),
            varint::decode_u64(&[0x80, 0x00])
        );
        assert_eq!(
            VarintKernel::write_varint(&KERNELS, u64::MAX, &mut [0_u8; 9]),
            varint::write_u64(u64::MAX, &mut [0_u8; 9])
        );
    }

    #[test]
    fn bitpack_trait_matches_direct_bytes_decode_and_errors() {
        let values = [3_u64, 0, 17, 31, 9];
        let direct = bitpack::encode(&values, 5);
        let dispatched = BitpackKernel::encode(&KERNELS, &values, 5);
        assert_eq!(dispatched, direct);

        let encoded = direct.expect("valid fixed-width fixture");
        assert_eq!(
            BitpackKernel::decode(&KERNELS, &encoded, values.len(), 5),
            bitpack::decode(&encoded, values.len(), 5)
        );

        let mut direct_output = [0xa5_u8; 8];
        let mut dispatched_output = direct_output;
        assert_eq!(
            BitpackKernel::encode_into(&KERNELS, &values, 5, &mut dispatched_output),
            bitpack::encode_into(&values, 5, &mut direct_output)
        );
        assert_eq!(dispatched_output, direct_output);

        let invalid_values = [0_u64, 8];
        assert_eq!(
            BitpackKernel::encode(&KERNELS, &invalid_values, 3),
            bitpack::encode(&invalid_values, 3)
        );
        assert_eq!(
            BitpackKernel::decode(&KERNELS, &[0x80], 1, 1),
            bitpack::decode(&[0x80], 1, 1)
        );
    }

    #[test]
    fn bitpack_trait_matches_direct_for_operations() {
        let values = [1_000_u64, 1_003, 1_007, 1_015];
        let direct = bitpack::encode_for(&values, 1_000, 4);
        let dispatched = BitpackKernel::encode_for(&KERNELS, &values, 1_000, 4);
        assert_eq!(dispatched, direct);

        let encoded = direct.expect("valid frame-of-reference fixture");
        assert_eq!(
            BitpackKernel::decode_for(&KERNELS, &encoded, values.len(), 1_000, 4),
            bitpack::decode_for(&encoded, values.len(), 1_000, 4)
        );

        let invalid_values = [999_u64];
        assert_eq!(
            BitpackKernel::encode_for(&KERNELS, &invalid_values, 1_000, 4),
            bitpack::encode_for(&invalid_values, 1_000, 4)
        );
    }

    #[test]
    fn delta_varint_trait_matches_direct_bytes_decode_and_errors() {
        let values = [7_u64, 7, 130, 65_536];
        let direct = delta_varint::encode(&values);
        assert_eq!(
            DeltaVarintKernel::encode_delta_varint(&KERNELS, &values),
            direct
        );

        let encoded = direct.expect("valid delta-varint fixture");
        let limit = delta_varint::EntryLimit::new(values.len());
        assert_eq!(
            DeltaVarintKernel::decode_delta_varint(&KERNELS, &encoded, values.len(), limit),
            delta_varint::decode(&encoded, values.len(), limit)
        );

        let decreasing = [4_u64, 3];
        assert_eq!(
            DeltaVarintKernel::encode_delta_varint(&KERNELS, &decreasing),
            delta_varint::encode(&decreasing)
        );
        let small_limit = delta_varint::EntryLimit::new(values.len() - 1);
        assert_eq!(
            DeltaVarintKernel::decode_delta_varint(&KERNELS, &encoded, values.len(), small_limit),
            delta_varint::decode(&encoded, values.len(), small_limit)
        );
    }

    #[test]
    fn elias_fano_trait_matches_direct_construction_access_and_errors() {
        let values = [0_u64, 0, 3, 9, 65_536];
        let limit = elias_fano::EntryLimit::new(values.len());
        let direct = elias_fano::EliasFano::try_new(&values, limit);
        assert_eq!(
            EliasFanoKernel::build_elias_fano(&KERNELS, &values, limit),
            direct
        );

        let encoded = direct.expect("valid Elias-Fano fixture");
        for index in 0..=values.len() {
            assert_eq!(
                EliasFanoKernel::elias_fano_select(&KERNELS, &encoded, index),
                encoded.select(index)
            );
        }
        for probe in [0_u64, 1, 9, 10, u64::MAX] {
            assert_eq!(
                EliasFanoKernel::elias_fano_rank_le(&KERNELS, &encoded, probe),
                encoded.rank_le(probe)
            );
            assert_eq!(
                EliasFanoKernel::elias_fano_rank_lt(&KERNELS, &encoded, probe),
                encoded.rank_lt(probe)
            );
            assert_eq!(
                EliasFanoKernel::elias_fano_predecessor(&KERNELS, &encoded, probe),
                encoded.predecessor(probe)
            );
            assert_eq!(
                EliasFanoKernel::elias_fano_successor(&KERNELS, &encoded, probe),
                encoded.successor(probe)
            );
        }

        let decreasing = [4_u64, 3];
        assert_eq!(
            EliasFanoKernel::build_elias_fano(&KERNELS, &decreasing, limit),
            elias_fano::EliasFano::try_new(&decreasing, limit)
        );
    }

    #[test]
    fn roaring_trait_matches_direct_set_operations_and_errors() {
        let left_values = [1_u32, 2, 3, 65_537, 65_538];
        let right_values = [2_u32, 3, 5, 65_538];
        let left_limit = roaring::EntryLimit::new(left_values.len());
        let right_limit = roaring::EntryLimit::new(right_values.len());
        let direct_left = roaring::RoaringBitmap::try_from_sorted(&left_values, left_limit);
        assert_eq!(
            RoaringKernel::build_roaring(&KERNELS, &left_values, left_limit),
            direct_left
        );

        let left = direct_left.expect("valid roaring fixture");
        let right = roaring::RoaringBitmap::try_from_sorted(&right_values, right_limit)
            .expect("valid roaring fixture");
        for probe in [0_u32, 2, 4, 65_538, u32::MAX] {
            assert_eq!(
                RoaringKernel::roaring_contains(&KERNELS, &left, probe),
                left.contains(probe)
            );
            assert_eq!(
                RoaringKernel::roaring_rank_le(&KERNELS, &left, probe),
                left.rank_le(probe)
            );
        }
        for index in 0..=left.len() {
            assert_eq!(
                RoaringKernel::roaring_select(&KERNELS, &left, index),
                left.select(index)
            );
        }

        let result_limit = roaring::EntryLimit::new(3);
        assert_eq!(
            RoaringKernel::roaring_intersection(&KERNELS, &left, &right, result_limit),
            left.intersection(&right, result_limit)
        );
        let too_small = roaring::EntryLimit::new(2);
        assert_eq!(
            RoaringKernel::roaring_intersection(&KERNELS, &left, &right, too_small),
            left.intersection(&right, too_small)
        );

        let duplicate = [1_u32, 1];
        assert_eq!(
            RoaringKernel::build_roaring(&KERNELS, &duplicate, left_limit),
            roaring::RoaringBitmap::try_from_sorted(&duplicate, left_limit)
        );
    }

    #[test]
    fn neighbor_trait_matches_every_explicit_arm_and_access() {
        let values = [1_u64, 3, 4, 8, 65_536];
        let limit = neighbor::EntryLimit::new(values.len());
        for codec in [
            neighbor::NeighborCodec::EliasFano,
            neighbor::NeighborCodec::StreamVByte,
            neighbor::NeighborCodec::DenseIntervals,
        ] {
            let direct = match codec {
                neighbor::NeighborCodec::EliasFano => {
                    neighbor::EncodedNeighbors::try_elias_fano(&values, limit)
                }
                neighbor::NeighborCodec::StreamVByte => {
                    neighbor::EncodedNeighbors::try_stream_vbyte(&values, limit)
                }
                neighbor::NeighborCodec::DenseIntervals => {
                    neighbor::EncodedNeighbors::try_dense_intervals(&values, limit)
                }
            };
            assert_eq!(
                NeighborKernel::build_neighbors(&KERNELS, codec, &values, limit),
                direct
            );

            let encoded = direct.expect("valid neighbor fixture");
            assert_eq!(encoded.codec(), codec);
            for probe in [0_u64, 3, 5, 65_536, u64::MAX] {
                assert_eq!(
                    NeighborKernel::neighbors_contains(&KERNELS, &encoded, probe),
                    encoded.contains(probe)
                );
                assert_eq!(
                    NeighborKernel::neighbors_rank_le(&KERNELS, &encoded, probe),
                    encoded.rank_le(probe)
                );
            }
            for index in 0..=values.len() {
                assert_eq!(
                    NeighborKernel::neighbors_select(&KERNELS, &encoded, index),
                    encoded.select(index)
                );
            }
        }
    }

    #[test]
    fn stream_accounting_output_is_bounded_deterministic_and_path_bound() {
        let values = [1_u64, 3, 4, 8, 65_536];
        let stream = neighbor::StreamVByteNeighbors::try_new(
            &values,
            neighbor::EntryLimit::new(values.len()),
        )
        .expect("valid stream fixture");

        let first = NeighborKernel::stream_vbyte_accounting_output(&KERNELS, &stream, 4_096)
            .expect("bounded accounting output");
        let second = NeighborKernel::stream_vbyte_accounting_output(&KERNELS, &stream, 4_096)
            .expect("bounded accounting output");
        assert_eq!(first, second);
        assert_eq!(first.dispatch_path(), DispatchPath::Scalar);
        assert!(first.as_bytes().starts_with(STREAM_ACCOUNTING_MAGIC));
        assert_eq!(
            NeighborKernel::stream_vbyte_accounting_output(&KERNELS, &stream, first.len() - 1),
            Err(DiagnosticOutputError::OutputLimitExceeded {
                required: first.len(),
                limit: first.len() - 1,
            })
        );
    }

    #[test]
    fn neighbor_trait_matches_cross_arm_intersection_and_errors() {
        let left = neighbor::EncodedNeighbors::try_stream_vbyte(
            &[1, 3, 4, 8],
            neighbor::EntryLimit::new(4),
        )
        .expect("valid stream fixture");
        let right = neighbor::EncodedNeighbors::try_dense_intervals(
            &[3, 4, 9],
            neighbor::EntryLimit::new(3),
        )
        .expect("valid interval fixture");
        let limit = neighbor::EntryLimit::new(2);
        assert_eq!(
            NeighborKernel::neighbors_intersection(&KERNELS, &left, &right, limit),
            left.intersection(&right, limit)
        );
        let too_small = neighbor::EntryLimit::new(1);
        assert_eq!(
            NeighborKernel::neighbors_intersection(&KERNELS, &left, &right, too_small),
            left.intersection(&right, too_small)
        );

        let duplicate = [1_u64, 1];
        let input_limit = neighbor::EntryLimit::new(duplicate.len());
        assert_eq!(
            NeighborKernel::build_neighbors(
                &KERNELS,
                neighbor::NeighborCodec::StreamVByte,
                &duplicate,
                input_limit
            ),
            neighbor::EncodedNeighbors::try_stream_vbyte(&duplicate, input_limit)
        );
    }

    #[test]
    fn identity_trait_matches_direct_typed_construction_access_and_errors() {
        fn vid(epoch: u64, partition: u32, slot: u64) -> VId {
            VId(identity::IdentityParts::try_new(epoch, partition, slot)
                .expect("valid identity fixture")
                .pack())
        }

        let arbitrary = [vid(7, 2, 9), vid(7, 2, 3), vid(8, 1, 0)];
        let limits = identity::IdentityColumnLimits::new(arbitrary.len(), 3, usize::MAX);
        let direct = identity::IdentityColumn::try_new(&arbitrary, limits);
        assert_eq!(
            IdentityColumnKernel::build_identity_column(&KERNELS, &arbitrary, limits),
            direct
        );
        let column = direct.expect("valid arbitrary identity fixture");
        for row in 0..=arbitrary.len() {
            assert_eq!(
                IdentityColumnKernel::identity_at(&KERNELS, &column, row),
                column.get(row)
            );
        }
        assert_eq!(
            IdentityColumnKernel::encode_identity_payload(
                &KERNELS,
                &column,
                column.encoded_payload_len()
            )
            .map(KernelOutput::into_bytes),
            column.try_scalar_payload(column.encoded_payload_len())
        );
        assert_eq!(
            IdentityColumnKernel::encode_identity_payload(&KERNELS, &column, 0)
                .map(KernelOutput::into_bytes),
            column.try_scalar_payload(0)
        );

        let sorted = [vid(7, 2, 3), vid(7, 2, 9), vid(8, 1, 0)];
        let direct_sorted = identity::SortedIdentityColumn::try_new(&sorted, limits);
        assert_eq!(
            IdentityColumnKernel::build_sorted_identity_column(&KERNELS, &sorted, limits),
            direct_sorted
        );
        let sorted_column = direct_sorted.expect("valid sorted identity fixture");
        for row in 0..=sorted.len() {
            assert_eq!(
                IdentityColumnKernel::sorted_identity_at(&KERNELS, &sorted_column, row),
                sorted_column.get(row)
            );
        }
        for probe in [vid(7, 2, 2), vid(7, 2, 9), vid(9, 0, 0)] {
            assert_eq!(
                IdentityColumnKernel::identity_lower_bound(&KERNELS, &sorted_column, probe),
                sorted_column.lower_bound(probe)
            );
        }

        let edges = [EId(sorted[0].0), EId(sorted[1].0)];
        let edge_limits = identity::IdentityColumnLimits::new(edges.len(), 1, usize::MAX);
        assert_eq!(
            IdentityColumnKernel::build_identity_column(&KERNELS, &edges, edge_limits),
            identity::IdentityColumn::try_new(&edges, edge_limits)
        );

        let small_limit = identity::IdentityColumnLimits::new(1, 1, usize::MAX);
        assert_eq!(
            IdentityColumnKernel::build_identity_column(&KERNELS, &arbitrary, small_limit),
            identity::IdentityColumn::try_new(&arbitrary, small_limit)
        );
        assert_eq!(
            IdentityColumnKernel::build_sorted_identity_column(&KERNELS, &arbitrary, limits),
            identity::SortedIdentityColumn::try_new(&arbitrary, limits)
        );
    }

    #[test]
    fn block_trait_matches_direct_bytes_decode_and_errors() {
        let profile =
            block::CodecProfile::try_new(4_096, 256, 4_096).expect("valid scalar profile");
        let input = b"abcdefghabcdefghabcdefgh:scalar-block-fixture";

        let direct = block::compress(input, profile);
        let dispatched = BlockKernel::compress(&KERNELS, input, profile);
        assert_eq!(dispatched, direct);
        let provenance =
            BlockKernel::compress_output(&KERNELS, input, profile).expect("valid block fixture");
        assert_eq!(provenance.as_bytes(), direct.as_deref().unwrap_or_default());
        assert_eq!(provenance.dispatch_path(), DispatchPath::Scalar);

        let encoded = direct.expect("valid block fixture");
        let limit = block::OutputLimit::new(input.len());
        assert_eq!(
            BlockKernel::decompress(&KERNELS, &encoded, input.len(), limit),
            block::decompress(&encoded, input.len(), limit)
        );

        let too_large = vec![0_u8; profile.max_block_len() + 1];
        assert_eq!(
            BlockKernel::compress(&KERNELS, &too_large, profile),
            block::compress(&too_large, profile)
        );

        let malformed = [0x80_u8, 0, 0];
        assert_eq!(
            BlockKernel::decompress(&KERNELS, &malformed, 4, block::OutputLimit::new(4)),
            block::decompress(&malformed, 4, block::OutputLimit::new(4))
        );
    }
}
