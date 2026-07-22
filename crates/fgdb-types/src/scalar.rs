//! Canonical scalar values under the `STRICT_PORTABLE` profile (plan §8.6).
//!
//! This module carries the FG-INV-12 binding clause: *canonical scalar
//! equality, hashing, ordering, and encoding are coherent*. Concretely:
//!
//! - `a == b` ⇒ `hash(a) == hash(b)` ⇒ `encode(a) == encode(b)`,
//! - `Ord` is total and transitive over all scalars (cross-type order is the
//!   fixed profile type rank: Null < Bool < Int < Decimal < Float < Text <
//!   Timestamp < Bytes),
//! - `a.cmp(b) == encode(a).cmp(encode(b))`, so byte-keyed maps and `LexMin`
//!   use exactly the semantic scalar order,
//! - `decode(encode(v)) == v` for every value, and decode rejects malformed
//!   input under length-before-allocation bounds.
//!
//! `STRICT_PORTABLE` float canonicalization collapses every NaN to one quiet
//! NaN and `-0.0` to `+0.0`. Decimal, timestamp, and text values use their
//! dedicated canonical modules; this remains the only scalar union.

use std::cmp::Ordering;

use crate::{
    bytes::{BoundedBytes, BoundedBytesError},
    decimal::{CanonicalDecimal, DecimalDecodeError},
    ids::ObjectId,
    temporal::{
        CanonicalTimestamp, MAX_ZONE_IDENTIFIER_BYTES, TimestampDecodeError, TzdbResolver,
        validate_zone_identifier,
    },
    text::{
        CanonicalText, CanonicalTextError, CollationResolver, MAX_CANONICAL_SORT_KEY_BYTES,
        MAX_CANONICAL_TEXT_BYTES, NonBinaryTextBinding, TextBinding,
    },
};

/// The single canonical quiet-NaN bit pattern under `STRICT_PORTABLE`.
const CANONICAL_NAN_BITS: u64 = 0x7FF8_0000_0000_0000;
const F64_SIGN_BIT: u64 = 1 << 63;
const I64_SIGN_BIT: u64 = 1 << 63;
const I32_SIGN_BIT: u32 = 1 << 31;
const I128_SIGN_BIT: u128 = 1 << 127;

const TAG_NULL: u8 = 0x00;
const TAG_BOOL: u8 = 0x01;
const TAG_INT: u8 = 0x02;
const TAG_DECIMAL: u8 = 0x03;
const TAG_FLOAT: u8 = 0x04;
const TAG_TEXT: u8 = 0x05;
const TAG_TIMESTAMP: u8 = 0x06;
const TAG_BYTES: u8 = 0x07;

const COMPARABLE_GROUP_BYTES: usize = 8;
const COMPARABLE_ENCODED_GROUP_BYTES: usize = 9;
const COMPARABLE_FULL_GROUP_MARKER: u8 = 0xFF;
const COMPARABLE_TERMINAL_MARKER: u8 = 0xF7;

/// Maximum complete encoded payload (excluding the scalar tag) accepted by
/// this profile. This is an aggregate bound: non-binary text cannot consume
/// one full bound for text and another full bound for its collation key.
pub const MAX_SCALAR_PAYLOAD: usize = 64 * 1024 * 1024;

/// Largest raw byte-string whose memcomparable representation fits the
/// aggregate scalar payload bound, including its mandatory terminal group.
pub const MAX_SCALAR_BYTES: usize =
    (MAX_SCALAR_PAYLOAD / COMPARABLE_ENCODED_GROUP_BYTES) * COMPARABLE_GROUP_BYTES - 1;

/// Byte scalar whose length bound is enforced at construction.
pub type CanonicalBytes = BoundedBytes<MAX_SCALAR_BYTES>;

/// Complete artifact capability required to admit or decode every
/// `STRICT_PORTABLE` scalar arm.
pub trait CanonicalScalarResolver: CollationResolver + TzdbResolver {}

impl<T: CollationResolver + TzdbResolver> CanonicalScalarResolver for T {}

/// An `f64` in canonical form: unique NaN, no negative zero. Constructing
/// one is the only way floats enter the scalar domain, so equality, hashing,
/// ordering, and encoding all see canonical bits only.
#[derive(Clone, Copy, Debug)]
pub struct CanonicalF64(u64);

impl CanonicalF64 {
    pub fn new(v: f64) -> Self {
        if v.is_nan() {
            return CanonicalF64(CANONICAL_NAN_BITS);
        }
        if v == 0.0 {
            // Collapses -0.0; 0.0f64.to_bits() is the +0 pattern.
            return CanonicalF64(0);
        }
        CanonicalF64(v.to_bits())
    }

    pub fn get(&self) -> f64 {
        f64::from_bits(self.0)
    }

    pub const fn to_bits(&self) -> u64 {
        self.0
    }

    /// Rejects non-canonical bit patterns instead of re-canonicalizing:
    /// durable inputs must already be canonical (fail closed, never repair).
    pub fn from_bits_canonical(bits: u64) -> Option<Self> {
        let v = f64::from_bits(bits);
        let canonical = if v.is_nan() {
            bits == CANONICAL_NAN_BITS
        } else {
            !(v == 0.0 && bits != 0)
        };
        canonical.then_some(CanonicalF64(bits))
    }
}

impl PartialEq for CanonicalF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for CanonicalF64 {}
impl std::hash::Hash for CanonicalF64 {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}
impl PartialOrd for CanonicalF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for CanonicalF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        // total_cmp is IEEE totalOrder; with the single positive quiet NaN it
        // is numeric order with NaN greatest, and -0 cannot occur.
        self.get().total_cmp(&other.get())
    }
}

/// The canonical scalar union under `STRICT_PORTABLE`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum CanonicalScalar {
    Null,
    Bool(bool),
    Int(i64),
    Decimal(CanonicalDecimal),
    Float(CanonicalF64),
    Text(CanonicalText),
    Timestamp(CanonicalTimestamp),
    Bytes(CanonicalBytes),
}

impl CanonicalScalar {
    /// Constructs a UCS_BASIC text scalar through the bounded canonical-text
    /// admission path.
    pub fn ucs_basic_text(value: &str) -> Result<Self, CanonicalTextError> {
        CanonicalText::new_ucs_basic(value).map(Self::Text)
    }

    /// Constructs a byte scalar only when it satisfies the profile bound.
    pub fn bytes(value: Vec<u8>) -> Result<Self, BoundedBytesError> {
        CanonicalBytes::new(value).map(Self::Bytes)
    }

    /// Fixed cross-type rank of the `STRICT_PORTABLE` profile.
    fn type_rank(&self) -> u8 {
        match self {
            CanonicalScalar::Null => 0,
            CanonicalScalar::Bool(_) => 1,
            CanonicalScalar::Int(_) => 2,
            CanonicalScalar::Decimal(_) => 3,
            CanonicalScalar::Float(_) => 4,
            CanonicalScalar::Text(_) => 5,
            CanonicalScalar::Timestamp(_) => 6,
            CanonicalScalar::Bytes(_) => 7,
        }
    }

    /// Canonical order-preserving value encoding: `rank-tag ‖ payload`.
    ///
    /// Signed fixed-width values use sign-bit-flipped big endian; canonical
    /// floats use the standard total-order transform; variable byte strings
    /// use 8-byte memcomparable groups. Therefore ordinary bytewise
    /// lexicographic comparison is exactly [`Ord`] for every scalar arm.
    /// Allocation is fallible and the aggregate payload bound is checked
    /// before the output buffer is allocated.
    pub fn encode(&self) -> Result<Vec<u8>, ScalarEncodeError> {
        match self {
            CanonicalScalar::Null => tagged_fixed_payload(TAG_NULL, &[]),
            CanonicalScalar::Bool(value) => tagged_fixed_payload(TAG_BOOL, &[u8::from(*value)]),
            CanonicalScalar::Int(value) => {
                tagged_fixed_payload(TAG_INT, &ordered_i64(*value).to_be_bytes())
            }
            CanonicalScalar::Decimal(value) => tagged_fixed_payload(
                TAG_DECIMAL,
                &ordered_i128(value.coefficient()).to_be_bytes(),
            ),
            CanonicalScalar::Float(value) => {
                tagged_fixed_payload(TAG_FLOAT, &ordered_f64_bits(value.to_bits()).to_be_bytes())
            }
            CanonicalScalar::Text(value) => encode_text_scalar(value),
            CanonicalScalar::Timestamp(value) => encode_timestamp_scalar(value),
            CanonicalScalar::Bytes(value) => {
                encode_one_comparable_field(TAG_BYTES, value.as_slice())
            }
        }
    }

    /// Decodes one scalar, consuming the entire input. Every malformed form
    /// is a typed rejection; declared lengths are validated against both the
    /// profile bound and the actually-present input before any allocation.
    pub fn decode(bytes: &[u8]) -> Result<Self, ScalarDecodeError> {
        Self::decode_inner(bytes, None)
    }

    /// Decodes one scalar against an explicit content-addressed artifact
    /// resolver. Artifact-bound text and timestamps are recomputed against
    /// the exact pinned artifacts; no host locale or tzdb is consulted.
    pub fn decode_with_resolver<R: CanonicalScalarResolver>(
        bytes: &[u8],
        resolver: &R,
    ) -> Result<Self, ScalarDecodeError> {
        Self::decode_inner(bytes, Some(resolver))
    }

    fn decode_inner(
        bytes: &[u8],
        resolver: Option<&dyn CanonicalScalarResolver>,
    ) -> Result<Self, ScalarDecodeError> {
        let (&tag, rest) = bytes.split_first().ok_or(ScalarDecodeError::Empty)?;
        let exact = |want: usize| -> Result<&[u8], ScalarDecodeError> {
            if rest.len() != want {
                return Err(ScalarDecodeError::WrongPayloadLength {
                    tag,
                    expected: want,
                    got: rest.len(),
                });
            }
            Ok(rest)
        };
        match tag {
            TAG_NULL => {
                exact(0)?;
                Ok(CanonicalScalar::Null)
            }
            TAG_BOOL => match exact(1)?[0] {
                0 => Ok(CanonicalScalar::Bool(false)),
                1 => Ok(CanonicalScalar::Bool(true)),
                other => Err(ScalarDecodeError::BadBool(other)),
            },
            TAG_INT => {
                let raw = array_from_exact::<8>(exact(8)?, tag)?;
                Ok(CanonicalScalar::Int(decode_ordered_i64(
                    u64::from_be_bytes(raw),
                )))
            }
            TAG_DECIMAL => {
                let raw = array_from_exact::<16>(exact(16)?, tag)?;
                let coefficient = decode_ordered_i128(u128::from_be_bytes(raw));
                CanonicalDecimal::decode(&coefficient.to_le_bytes())
                    .map(CanonicalScalar::Decimal)
                    .map_err(ScalarDecodeError::Decimal)
            }
            TAG_FLOAT => {
                let raw = array_from_exact::<8>(exact(8)?, tag)?;
                let bits = decode_ordered_f64_bits(u64::from_be_bytes(raw));
                CanonicalF64::from_bits_canonical(bits)
                    .map(CanonicalScalar::Float)
                    .ok_or(ScalarDecodeError::NonCanonicalFloat { bits })
            }
            TAG_TEXT => decode_text_scalar(rest, resolver),
            TAG_TIMESTAMP => decode_timestamp_scalar(rest, resolver),
            TAG_BYTES => decode_bytes_scalar(rest),
            other => Err(ScalarDecodeError::UnknownTag(other)),
        }
    }
}

fn tagged_fixed_payload(tag: u8, payload: &[u8]) -> Result<Vec<u8>, ScalarEncodeError> {
    let requested = payload
        .len()
        .checked_add(1)
        .ok_or(ScalarEncodeError::EncodedSizeOverflow)?;
    let mut out = Vec::new();
    out.try_reserve_exact(requested)
        .map_err(|_| ScalarEncodeError::AllocationFailed { requested })?;
    out.push(tag);
    out.extend_from_slice(payload);
    Ok(out)
}

fn allocate_tagged_payload(tag: u8, payload_len: usize) -> Result<Vec<u8>, ScalarEncodeError> {
    if payload_len > MAX_SCALAR_PAYLOAD {
        return Err(ScalarEncodeError::PayloadTooLarge {
            tag,
            length: payload_len,
            maximum: MAX_SCALAR_PAYLOAD,
        });
    }
    let requested = payload_len
        .checked_add(1)
        .ok_or(ScalarEncodeError::EncodedSizeOverflow)?;
    let mut out = Vec::new();
    out.try_reserve_exact(requested)
        .map_err(|_| ScalarEncodeError::AllocationFailed { requested })?;
    out.push(tag);
    Ok(out)
}

fn encode_text_scalar(value: &CanonicalText) -> Result<Vec<u8>, ScalarEncodeError> {
    let text_len = comparable_encoded_len(value.as_bytes())?;
    let payload_len = match value.binding() {
        TextBinding::UcsBasic => 1usize
            .checked_add(text_len)
            .ok_or(ScalarEncodeError::EncodedSizeOverflow)?,
        TextBinding::NonBinary(_) => {
            let sort_key = value
                .canonical_sort_key()
                .ok_or(ScalarEncodeError::MissingCanonicalSortKey)?;
            1usize
                .checked_add(4 * 32)
                .and_then(|len| len.checked_add(comparable_encoded_len(sort_key).ok()?))
                .and_then(|len| len.checked_add(text_len))
                .ok_or(ScalarEncodeError::EncodedSizeOverflow)?
        }
    };
    let mut out = allocate_tagged_payload(TAG_TEXT, payload_len)?;
    match value.binding() {
        TextBinding::UcsBasic => {
            out.push(0);
            append_comparable(value.as_bytes(), &mut out);
        }
        TextBinding::NonBinary(binding) => {
            out.push(1);
            append_oid(&mut out, binding.unicode_data_oid);
            append_oid(&mut out, binding.normalization_oid);
            append_oid(&mut out, binding.segmentation_oid);
            append_oid(&mut out, binding.collation_oid);
            let sort_key = value
                .canonical_sort_key()
                .ok_or(ScalarEncodeError::MissingCanonicalSortKey)?;
            append_comparable(sort_key, &mut out);
            append_comparable(value.as_bytes(), &mut out);
        }
    }
    Ok(out)
}

fn encode_timestamp_scalar(value: &CanonicalTimestamp) -> Result<Vec<u8>, ScalarEncodeError> {
    let zone_payload_len = match value.zone() {
        None => 0,
        Some(zone) => comparable_encoded_len(zone.identifier().as_bytes())?
            .checked_add(32)
            .ok_or(ScalarEncodeError::EncodedSizeOverflow)?,
    };
    let payload_len = 16usize
        .checked_add(4)
        .and_then(|len| len.checked_add(1))
        .and_then(|len| len.checked_add(zone_payload_len))
        .ok_or(ScalarEncodeError::EncodedSizeOverflow)?;
    let mut out = allocate_tagged_payload(TAG_TIMESTAMP, payload_len)?;
    out.extend_from_slice(&ordered_i128(value.instant_utc_nanos()).to_be_bytes());
    out.extend_from_slice(&ordered_i32(value.utc_offset_seconds()).to_be_bytes());
    match value.zone() {
        None => out.push(0),
        Some(zone) => {
            out.push(1);
            append_comparable(zone.identifier().as_bytes(), &mut out);
            append_oid(&mut out, zone.tzdb_oid());
        }
    }
    Ok(out)
}

fn encode_one_comparable_field(tag: u8, value: &[u8]) -> Result<Vec<u8>, ScalarEncodeError> {
    let payload_len = comparable_encoded_len(value)?;
    let mut out = allocate_tagged_payload(tag, payload_len)?;
    append_comparable(value, &mut out);
    Ok(out)
}

fn comparable_encoded_len(value: &[u8]) -> Result<usize, ScalarEncodeError> {
    comparable_encoded_len_for_len(value.len())
}

fn comparable_encoded_len_for_len(decoded_len: usize) -> Result<usize, ScalarEncodeError> {
    decoded_len
        .checked_div(COMPARABLE_GROUP_BYTES)
        .and_then(|groups| groups.checked_add(1))
        .and_then(|groups| groups.checked_mul(COMPARABLE_ENCODED_GROUP_BYTES))
        .ok_or(ScalarEncodeError::EncodedSizeOverflow)
}

fn append_comparable(value: &[u8], out: &mut Vec<u8>) {
    let (chunks, remainder) = value.as_chunks::<COMPARABLE_GROUP_BYTES>();
    for chunk in chunks {
        out.extend_from_slice(chunk);
        out.push(COMPARABLE_FULL_GROUP_MARKER);
    }
    out.extend_from_slice(remainder);
    let padding = COMPARABLE_GROUP_BYTES - remainder.len();
    out.resize(out.len() + padding, 0);
    out.push(COMPARABLE_FULL_GROUP_MARKER - padding as u8);
}

fn append_oid(out: &mut Vec<u8>, oid: ObjectId) {
    out.extend_from_slice(oid.as_bytes());
}

const fn ordered_i64(value: i64) -> u64 {
    (value as u64) ^ I64_SIGN_BIT
}

const fn decode_ordered_i64(value: u64) -> i64 {
    (value ^ I64_SIGN_BIT) as i64
}

const fn ordered_i32(value: i32) -> u32 {
    (value as u32) ^ I32_SIGN_BIT
}

const fn decode_ordered_i32(value: u32) -> i32 {
    (value ^ I32_SIGN_BIT) as i32
}

const fn ordered_i128(value: i128) -> u128 {
    (value as u128) ^ I128_SIGN_BIT
}

const fn decode_ordered_i128(value: u128) -> i128 {
    (value ^ I128_SIGN_BIT) as i128
}

const fn ordered_f64_bits(bits: u64) -> u64 {
    if bits & F64_SIGN_BIT == 0 {
        bits ^ F64_SIGN_BIT
    } else {
        !bits
    }
}

const fn decode_ordered_f64_bits(value: u64) -> u64 {
    if value & F64_SIGN_BIT == 0 {
        !value
    } else {
        value ^ F64_SIGN_BIT
    }
}

fn array_from_exact<const N: usize>(bytes: &[u8], tag: u8) -> Result<[u8; N], ScalarDecodeError> {
    bytes
        .try_into()
        .map_err(|_| ScalarDecodeError::WrongPayloadLength {
            tag,
            expected: N,
            got: bytes.len(),
        })
}

fn decode_text_scalar(
    payload: &[u8],
    resolver: Option<&dyn CanonicalScalarResolver>,
) -> Result<CanonicalScalar, ScalarDecodeError> {
    let mut cursor = ScalarCursor::new(TAG_TEXT, payload)?;
    let binding_tag = cursor.read_u8(ScalarField::TextBinding)?;
    let text = match binding_tag {
        0 => {
            let text_bytes = cursor.read_comparable(ScalarField::Text, MAX_CANONICAL_TEXT_BYTES)?;
            cursor.finish()?;
            let text = String::from_utf8(text_bytes)
                .map_err(|_| ScalarDecodeError::Text(CanonicalTextError::InvalidUtf8))?;
            CanonicalText::from_owned_ucs_basic(text).map_err(ScalarDecodeError::Text)?
        }
        1 => {
            let binding = NonBinaryTextBinding::new(
                cursor.read_oid(ScalarField::UnicodeDataObjectId)?,
                cursor.read_oid(ScalarField::NormalizationObjectId)?,
                cursor.read_oid(ScalarField::SegmentationObjectId)?,
                cursor.read_oid(ScalarField::CollationObjectId)?,
            );
            let resolver = resolver.ok_or(ScalarDecodeError::Text(
                CanonicalTextError::ResolverRequired,
            ))?;
            binding
                .validate_artifacts(resolver)
                .map_err(ScalarDecodeError::Text)?;
            let sort_key =
                cursor.read_comparable(ScalarField::SortKey, MAX_CANONICAL_SORT_KEY_BYTES)?;
            let text_bytes = cursor.read_comparable(ScalarField::Text, MAX_CANONICAL_TEXT_BYTES)?;
            cursor.finish()?;
            let text = String::from_utf8(text_bytes)
                .map_err(|_| ScalarDecodeError::Text(CanonicalTextError::InvalidUtf8))?;
            CanonicalText::from_ordered_scalar_parts(text, binding, sort_key, resolver)
                .map_err(ScalarDecodeError::Text)?
        }
        other => {
            return Err(ScalarDecodeError::Text(
                CanonicalTextError::UnknownBindingTag(other),
            ));
        }
    };
    Ok(CanonicalScalar::Text(text))
}

fn decode_timestamp_scalar(
    payload: &[u8],
    resolver: Option<&dyn CanonicalScalarResolver>,
) -> Result<CanonicalScalar, ScalarDecodeError> {
    let mut cursor = ScalarCursor::new(TAG_TIMESTAMP, payload)?;
    let instant = decode_ordered_i128(u128::from_be_bytes(
        cursor.read_array(ScalarField::TimestampInstant)?,
    ));
    let offset = decode_ordered_i32(u32::from_be_bytes(
        cursor.read_array(ScalarField::TimestampOffset)?,
    ));
    let zone_flag = cursor.read_u8(ScalarField::TimestampZoneFlag)?;
    let timestamp = match zone_flag {
        0 => {
            cursor.finish()?;
            CanonicalTimestamp::offset_only(instant, offset).map_err(|error| {
                ScalarDecodeError::Timestamp(TimestampDecodeError::InvalidValue(error))
            })?
        }
        1 => {
            let zone_bytes = cursor.read_comparable(
                ScalarField::TimestampZoneIdentifier,
                MAX_ZONE_IDENTIFIER_BYTES,
            )?;
            let tzdb_oid = cursor.read_oid(ScalarField::TzdbObjectId)?;
            cursor.finish()?;
            let zone = String::from_utf8(zone_bytes)
                .map_err(|_| ScalarDecodeError::Timestamp(TimestampDecodeError::InvalidZoneUtf8))?;
            validate_zone_identifier(&zone).map_err(|error| {
                ScalarDecodeError::Timestamp(TimestampDecodeError::InvalidValue(error))
            })?;
            let resolver = resolver.ok_or(ScalarDecodeError::Timestamp(
                TimestampDecodeError::TzdbResolverRequired,
            ))?;
            CanonicalTimestamp::zoned(instant, offset, &zone, tzdb_oid, resolver).map_err(
                |error| ScalarDecodeError::Timestamp(TimestampDecodeError::InvalidValue(error)),
            )?
        }
        other => {
            return Err(ScalarDecodeError::Timestamp(
                TimestampDecodeError::UnknownFlags(other),
            ));
        }
    };
    Ok(CanonicalScalar::Timestamp(timestamp))
}

fn decode_bytes_scalar(payload: &[u8]) -> Result<CanonicalScalar, ScalarDecodeError> {
    let mut cursor = ScalarCursor::new(TAG_BYTES, payload)?;
    let value = cursor.read_comparable(ScalarField::Bytes, MAX_SCALAR_BYTES)?;
    cursor.finish()?;
    CanonicalBytes::new(value)
        .map(CanonicalScalar::Bytes)
        .map_err(ScalarDecodeError::Bytes)
}

struct ScalarCursor<'a> {
    tag: u8,
    remaining: &'a [u8],
}

impl<'a> ScalarCursor<'a> {
    fn new(tag: u8, payload: &'a [u8]) -> Result<Self, ScalarDecodeError> {
        if payload.len() > MAX_SCALAR_PAYLOAD {
            return Err(ScalarDecodeError::LengthOverflow {
                tag,
                declared: u64::try_from(payload.len()).unwrap_or(u64::MAX),
            });
        }
        Ok(Self {
            tag,
            remaining: payload,
        })
    }

    fn take(&mut self, field: ScalarField, needed: usize) -> Result<&'a [u8], ScalarDecodeError> {
        if self.remaining.len() < needed {
            return Err(ScalarDecodeError::TruncatedField {
                tag: self.tag,
                field,
                needed,
                remaining: self.remaining.len(),
            });
        }
        let (head, tail) = self.remaining.split_at(needed);
        self.remaining = tail;
        Ok(head)
    }

    fn read_u8(&mut self, field: ScalarField) -> Result<u8, ScalarDecodeError> {
        self.take(field, 1)?
            .first()
            .copied()
            .ok_or(ScalarDecodeError::TruncatedField {
                tag: self.tag,
                field,
                needed: 1,
                remaining: 0,
            })
    }

    fn read_array<const N: usize>(
        &mut self,
        field: ScalarField,
    ) -> Result<[u8; N], ScalarDecodeError> {
        let bytes = self.take(field, N)?;
        bytes
            .try_into()
            .map_err(|_| ScalarDecodeError::TruncatedField {
                tag: self.tag,
                field,
                needed: N,
                remaining: bytes.len(),
            })
    }

    fn read_oid(&mut self, field: ScalarField) -> Result<ObjectId, ScalarDecodeError> {
        self.read_array::<32>(field).map(ObjectId)
    }

    fn read_comparable(
        &mut self,
        field: ScalarField,
        maximum: usize,
    ) -> Result<Vec<u8>, ScalarDecodeError> {
        let mut scan = self.remaining;
        let mut consumed = 0usize;
        let mut decoded_len = 0usize;
        loop {
            if scan.len() < COMPARABLE_ENCODED_GROUP_BYTES {
                return Err(ScalarDecodeError::TruncatedField {
                    tag: self.tag,
                    field,
                    needed: COMPARABLE_ENCODED_GROUP_BYTES,
                    remaining: scan.len(),
                });
            }
            let marker = scan[COMPARABLE_GROUP_BYTES];
            if marker < COMPARABLE_TERMINAL_MARKER {
                return Err(ScalarDecodeError::InvalidComparableMarker {
                    tag: self.tag,
                    field,
                    marker,
                });
            }
            let padding = usize::from(COMPARABLE_FULL_GROUP_MARKER - marker);
            let data_len = COMPARABLE_GROUP_BYTES - padding;
            if scan[data_len..COMPARABLE_GROUP_BYTES]
                .iter()
                .any(|byte| *byte != 0)
            {
                return Err(ScalarDecodeError::NonZeroComparablePadding {
                    tag: self.tag,
                    field,
                });
            }
            decoded_len = decoded_len.checked_add(data_len).ok_or(
                ScalarDecodeError::ComparableLengthOverflow {
                    tag: self.tag,
                    field,
                    decoded: usize::MAX,
                    maximum,
                },
            )?;
            if decoded_len > maximum {
                return Err(ScalarDecodeError::ComparableLengthOverflow {
                    tag: self.tag,
                    field,
                    decoded: decoded_len,
                    maximum,
                });
            }
            consumed = consumed.checked_add(COMPARABLE_ENCODED_GROUP_BYTES).ok_or(
                ScalarDecodeError::ComparableLengthOverflow {
                    tag: self.tag,
                    field,
                    decoded: decoded_len,
                    maximum,
                },
            )?;
            scan = &scan[COMPARABLE_ENCODED_GROUP_BYTES..];
            if padding != 0 {
                break;
            }
        }

        let mut decoded = Vec::new();
        decoded.try_reserve_exact(decoded_len).map_err(|_| {
            ScalarDecodeError::AllocationFailed {
                tag: self.tag,
                field,
                requested: decoded_len,
            }
        })?;
        let mut encoded_offset = 0usize;
        while encoded_offset < consumed {
            let group =
                &self.remaining[encoded_offset..encoded_offset + COMPARABLE_ENCODED_GROUP_BYTES];
            let padding = usize::from(COMPARABLE_FULL_GROUP_MARKER - group[8]);
            decoded.extend_from_slice(&group[..COMPARABLE_GROUP_BYTES - padding]);
            encoded_offset += COMPARABLE_ENCODED_GROUP_BYTES;
        }
        self.remaining = &self.remaining[consumed..];
        Ok(decoded)
    }

    fn finish(self) -> Result<(), ScalarDecodeError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(ScalarDecodeError::TrailingBytes {
                tag: self.tag,
                count: self.remaining.len(),
            })
        }
    }
}

impl PartialOrd for CanonicalScalar {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CanonicalScalar {
    fn cmp(&self, other: &Self) -> Ordering {
        use CanonicalScalar as S;
        match (self, other) {
            (S::Null, S::Null) => Ordering::Equal,
            (S::Bool(a), S::Bool(b)) => a.cmp(b),
            (S::Int(a), S::Int(b)) => a.cmp(b),
            (S::Decimal(a), S::Decimal(b)) => a.cmp(b),
            (S::Float(a), S::Float(b)) => a.cmp(b),
            (S::Text(a), S::Text(b)) => a.cmp(b),
            (S::Timestamp(a), S::Timestamp(b)) => a.cmp(b),
            (S::Bytes(a), S::Bytes(b)) => a.cmp(b),
            _ => self.type_rank().cmp(&other.type_rank()),
        }
    }
}

/// Typed rejections from [`CanonicalScalar::encode`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ScalarEncodeError {
    EncodedSizeOverflow,
    PayloadTooLarge {
        tag: u8,
        length: usize,
        maximum: usize,
    },
    AllocationFailed {
        requested: usize,
    },
    MissingCanonicalSortKey,
}

impl std::fmt::Display for ScalarEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EncodedSizeOverflow => write!(f, "scalar encoded size overflow"),
            Self::PayloadTooLarge {
                tag,
                length,
                maximum,
            } => write!(
                f,
                "scalar tag {tag:#04x} payload length {length} exceeds profile bound {maximum}"
            ),
            Self::AllocationFailed { requested } => {
                write!(
                    f,
                    "unable to allocate {requested} bytes for scalar encoding"
                )
            }
            Self::MissingCanonicalSortKey => {
                write!(
                    f,
                    "non-binary canonical text is missing its derived sort key"
                )
            }
        }
    }
}

impl std::error::Error for ScalarEncodeError {}

/// Field named by a malformed ordered scalar encoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScalarField {
    TextBinding,
    UnicodeDataObjectId,
    NormalizationObjectId,
    SegmentationObjectId,
    CollationObjectId,
    SortKey,
    Text,
    TimestampInstant,
    TimestampOffset,
    TimestampZoneFlag,
    TimestampZoneIdentifier,
    TzdbObjectId,
    Bytes,
}

/// Typed rejections from [`CanonicalScalar::decode`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ScalarDecodeError {
    Empty,
    UnknownTag(u8),
    BadBool(u8),
    LengthOverflow {
        tag: u8,
        declared: u64,
    },
    WrongPayloadLength {
        tag: u8,
        expected: usize,
        got: usize,
    },
    TruncatedField {
        tag: u8,
        field: ScalarField,
        needed: usize,
        remaining: usize,
    },
    InvalidComparableMarker {
        tag: u8,
        field: ScalarField,
        marker: u8,
    },
    NonZeroComparablePadding {
        tag: u8,
        field: ScalarField,
    },
    ComparableLengthOverflow {
        tag: u8,
        field: ScalarField,
        decoded: usize,
        maximum: usize,
    },
    AllocationFailed {
        tag: u8,
        field: ScalarField,
        requested: usize,
    },
    TrailingBytes {
        tag: u8,
        count: usize,
    },
    NonCanonicalFloat {
        bits: u64,
    },
    Decimal(DecimalDecodeError),
    Bytes(BoundedBytesError),
    Text(CanonicalTextError),
    Timestamp(TimestampDecodeError),
}

impl std::fmt::Display for ScalarDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScalarDecodeError::Empty => write!(f, "empty scalar encoding"),
            ScalarDecodeError::UnknownTag(t) => write!(f, "unknown scalar tag {t:#04x}"),
            ScalarDecodeError::BadBool(b) => write!(f, "bool payload {b:#04x} is not 0/1"),
            ScalarDecodeError::LengthOverflow { tag, declared } => {
                write!(
                    f,
                    "tag {tag:#04x}: encoded payload length {declared} exceeds profile bound"
                )
            }
            ScalarDecodeError::WrongPayloadLength { tag, expected, got } => {
                write!(
                    f,
                    "tag {tag:#04x}: payload length {got}, expected {expected}"
                )
            }
            ScalarDecodeError::TruncatedField {
                tag,
                field,
                needed,
                remaining,
            } => write!(
                f,
                "tag {tag:#04x}: truncated {field:?}; need {needed} bytes, have {remaining}"
            ),
            ScalarDecodeError::InvalidComparableMarker { tag, field, marker } => write!(
                f,
                "tag {tag:#04x}: {field:?} has invalid memcomparable marker {marker:#04x}"
            ),
            ScalarDecodeError::NonZeroComparablePadding { tag, field } => write!(
                f,
                "tag {tag:#04x}: {field:?} has non-zero memcomparable padding"
            ),
            ScalarDecodeError::ComparableLengthOverflow {
                tag,
                field,
                decoded,
                maximum,
            } => write!(
                f,
                "tag {tag:#04x}: decoded {field:?} length {decoded} exceeds bound {maximum}"
            ),
            ScalarDecodeError::AllocationFailed {
                tag,
                field,
                requested,
            } => write!(
                f,
                "tag {tag:#04x}: unable to allocate {requested} bytes for {field:?}"
            ),
            ScalarDecodeError::TrailingBytes { tag, count } => {
                write!(
                    f,
                    "tag {tag:#04x}: canonical scalar has {count} trailing bytes"
                )
            }
            ScalarDecodeError::NonCanonicalFloat { bits } => {
                write!(
                    f,
                    "float bits {bits:#018x} are not STRICT_PORTABLE-canonical"
                )
            }
            ScalarDecodeError::Decimal(error) => write!(f, "invalid decimal: {error}"),
            ScalarDecodeError::Bytes(error) => write!(f, "invalid byte scalar: {error}"),
            ScalarDecodeError::Text(error) => write!(f, "invalid canonical text: {error}"),
            ScalarDecodeError::Timestamp(error) => {
                write!(f, "invalid canonical timestamp: {error}")
            }
        }
    }
}

impl std::error::Error for ScalarDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decimal(error) => Some(error),
            Self::Bytes(error) => Some(error),
            Self::Text(error) => Some(error),
            Self::Timestamp(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        temporal::{TimestampArtifactError, TimestampConstructionError},
        text::{CollationResolverError, NonBinaryTextBinding, TextArtifactRole},
    };
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    const AVAILABLE_RESOLVER: FixtureResolver = FixtureResolver {
        collation_artifacts_available: true,
        tzdb_available: true,
    };
    const MISSING_RESOLVER: FixtureResolver = FixtureResolver {
        collation_artifacts_available: false,
        tzdb_available: false,
    };

    struct FixtureResolver {
        collation_artifacts_available: bool,
        tzdb_available: bool,
    }

    impl CollationResolver for FixtureResolver {
        fn artifact_available(&self, _: &ObjectId) -> bool {
            self.collation_artifacts_available
        }

        fn canonical_sort_key_len(
            &self,
            _: &NonBinaryTextBinding,
            text: &str,
        ) -> Result<usize, CollationResolverError> {
            text.len()
                .checked_add(4)
                .ok_or(CollationResolverError::new(1))
        }

        fn write_canonical_sort_key(
            &self,
            binding: &NonBinaryTextBinding,
            text: &str,
            output: &mut [u8],
        ) -> Result<usize, CollationResolverError> {
            let expected = self.canonical_sort_key_len(binding, text)?;
            if output.len() != expected {
                return Err(CollationResolverError::new(2));
            }
            output[..4].copy_from_slice(&[
                binding.unicode_data_oid.as_bytes()[0],
                binding.normalization_oid.as_bytes()[0],
                binding.segmentation_oid.as_bytes()[0],
                binding.collation_oid.as_bytes()[0],
            ]);
            output[4..].copy_from_slice(text.as_bytes());
            Ok(expected)
        }

        fn canonical_sort_key_matches(
            &self,
            binding: &NonBinaryTextBinding,
            text: &str,
            candidate: &[u8],
        ) -> Result<bool, CollationResolverError> {
            let prefix = [
                binding.unicode_data_oid.as_bytes()[0],
                binding.normalization_oid.as_bytes()[0],
                binding.segmentation_oid.as_bytes()[0],
                binding.collation_oid.as_bytes()[0],
            ];
            Ok(candidate.len() == text.len() + prefix.len()
                && candidate.starts_with(&prefix)
                && &candidate[prefix.len()..] == text.as_bytes())
        }
    }

    impl TzdbResolver for FixtureResolver {
        fn contains_tzdb(&self, tzdb_oid: &ObjectId) -> bool {
            self.tzdb_available && tzdb_oid == &ObjectId([9; 32])
        }

        fn canonical_utc_offset_seconds(
            &self,
            tzdb_oid: &ObjectId,
            zone_identifier: &str,
            instant_utc_nanos: i128,
        ) -> Option<i32> {
            (tzdb_oid == &ObjectId([9; 32])
                && zone_identifier == "America/New_York"
                && instant_utc_nanos == 1_735_689_600_123_456_789)
                .then_some(-5 * 60 * 60)
        }
    }

    fn ucs_basic(value: &str) -> CanonicalText {
        CanonicalText::new_ucs_basic(value)
            .unwrap_or_else(|error| panic!("small test text rejected: {error}"))
    }

    struct SplitMix64(u64);
    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }
        fn scalar(&mut self) -> CanonicalScalar {
            match self.next() % 10 {
                0 => CanonicalScalar::Null,
                1 => CanonicalScalar::Bool(self.next() % 2 == 1),
                2 => CanonicalScalar::Int(self.next() as i64),
                3 => CanonicalScalar::Decimal(
                    CanonicalDecimal::from_coefficient(i128::from(self.next()))
                        .unwrap_or_else(|error| panic!("seeded decimal rejected: {error}")),
                ),
                4 => CanonicalScalar::Float(CanonicalF64::new(f64::from_bits(self.next()))),
                5 => {
                    let n = (self.next() % 12) as usize;
                    let text: String = (0..n)
                        .map(|_| char::from(b'a' + (self.next() % 26) as u8))
                        .collect();
                    CanonicalScalar::Text(ucs_basic(&text))
                }
                6 => CanonicalScalar::Timestamp(
                    CanonicalTimestamp::offset_only(i128::from(self.next()), 0)
                        .unwrap_or_else(|error| panic!("seeded timestamp rejected: {error}")),
                ),
                7 => {
                    let n = (self.next() % 12) as usize;
                    CanonicalScalar::bytes((0..n).map(|_| self.next() as u8).collect())
                        .unwrap_or_else(|error| panic!("seeded byte scalar rejected: {error}"))
                }
                8 => {
                    let n = (self.next() % 12) as usize;
                    let text: String = (0..n)
                        .map(|_| char::from(b'a' + (self.next() % 26) as u8))
                        .collect();
                    let binding = NonBinaryTextBinding::new(
                        ObjectId([1; 32]),
                        ObjectId([2; 32]),
                        ObjectId([3; 32]),
                        ObjectId([4; 32]),
                    );
                    CanonicalText::new_non_binary(&text, binding, &AVAILABLE_RESOLVER)
                        .map(CanonicalScalar::Text)
                        .unwrap_or_else(|error| panic!("seeded non-binary text rejected: {error}"))
                }
                9 => CanonicalTimestamp::zoned(
                    1_735_689_600_123_456_789,
                    -5 * 60 * 60,
                    "America/New_York",
                    ObjectId([9; 32]),
                    &AVAILABLE_RESOLVER,
                )
                .map(CanonicalScalar::Timestamp)
                .unwrap_or_else(|error| panic!("seeded zoned timestamp rejected: {error}")),
                _ => unreachable!("modulo ten is within the scalar arm table"),
            }
        }
    }

    fn hash_of(v: &CanonicalScalar) -> u64 {
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    #[test]
    fn strict_portable_float_canonicalization() {
        // Every NaN collapses to the one canonical NaN.
        let nans = [
            f64::NAN,
            -f64::NAN,
            f64::from_bits(0x7FF0_0000_0000_0001),
            f64::from_bits(0xFFF8_0000_0000_1234),
        ];
        for n in nans {
            assert_eq!(
                CanonicalF64::new(n).to_bits(),
                CANONICAL_NAN_BITS,
                "{:#x}",
                n.to_bits()
            );
        }
        // -0.0 collapses to +0.0 and compares equal.
        assert_eq!(CanonicalF64::new(-0.0), CanonicalF64::new(0.0));
        assert_eq!(CanonicalF64::new(-0.0).to_bits(), 0);
        // NaN sorts greatest; the rest is numeric order.
        let mut vals: Vec<CanonicalF64> =
            [f64::NAN, 1.0, f64::NEG_INFINITY, -0.0, f64::INFINITY, -1.5]
                .into_iter()
                .map(CanonicalF64::new)
                .collect();
        vals.sort();
        let got: Vec<f64> = vals.iter().map(CanonicalF64::get).collect();
        assert_eq!(got[0], f64::NEG_INFINITY);
        assert_eq!(got[1], -1.5);
        assert_eq!(got[2], 0.0);
        assert_eq!(got[3], 1.0);
        assert_eq!(got[4], f64::INFINITY);
        assert!(got[5].is_nan());
    }

    #[test]
    fn noncanonical_float_bits_are_rejected_not_repaired() {
        assert!(CanonicalF64::from_bits_canonical(CANONICAL_NAN_BITS).is_some());
        assert!(CanonicalF64::from_bits_canonical(0x7FF0_0000_0000_0001).is_none());
        assert!(CanonicalF64::from_bits_canonical((-0.0f64).to_bits()).is_none());
        assert!(CanonicalF64::from_bits_canonical(1.5f64.to_bits()).is_some());
    }

    #[test]
    fn equal_implies_same_hash_and_same_encoding() {
        for seed in [1u64, 0xFEED, 0x00C0FFEE] {
            let mut rng = SplitMix64(seed);
            for _ in 0..500 {
                let a = rng.scalar();
                let b = a.clone();
                assert_eq!(a, b);
                assert_eq!(hash_of(&a), hash_of(&b), "seed={seed} value={a:?}");
                let a_encoding = a
                    .encode()
                    .unwrap_or_else(|error| panic!("seed={seed} encode {a:?}: {error}"));
                let b_encoding = b
                    .encode()
                    .unwrap_or_else(|error| panic!("seed={seed} encode {b:?}: {error}"));
                assert_eq!(a_encoding, b_encoding, "seed={seed} value={a:?}");
            }
        }

        let independently_equal = [
            (
                CanonicalScalar::Float(CanonicalF64::new(-0.0)),
                CanonicalScalar::Float(CanonicalF64::new(0.0)),
            ),
            (
                CanonicalScalar::Float(CanonicalF64::new(f64::NAN)),
                CanonicalScalar::Float(CanonicalF64::new(f64::from_bits(0xFFF8_0000_0000_1234))),
            ),
            (
                CanonicalScalar::Decimal(
                    CanonicalDecimal::from_scaled_half_even(12, 0)
                        .expect("small decimal is canonical"),
                ),
                CanonicalScalar::Decimal(
                    CanonicalDecimal::from_scaled_half_even(120, 1)
                        .expect("equivalent scaled decimal is canonical"),
                ),
            ),
            (
                CanonicalScalar::Text(ucs_basic("same")),
                CanonicalScalar::ucs_basic_text("same").expect("small text is canonical"),
            ),
        ];
        for (left, right) in independently_equal {
            assert_eq!(left, right);
            assert_eq!(hash_of(&left), hash_of(&right));
            assert_eq!(left.encode(), right.encode());
        }
    }

    #[test]
    fn encode_decode_round_trips_all_variants() {
        for seed in [2u64, 0xDECAF, u64::MAX / 3] {
            let mut rng = SplitMix64(seed);
            for _ in 0..500 {
                let v = rng.scalar();
                let enc = v
                    .encode()
                    .unwrap_or_else(|error| panic!("seed={seed} encode {v:?}: {error}"));
                let back = CanonicalScalar::decode_with_resolver(&enc, &AVAILABLE_RESOLVER)
                    .unwrap_or_else(|e| {
                        panic!("seed={seed} decode({enc:02x?}) of {v:?} failed: {e}")
                    });
                assert_eq!(back, v, "seed={seed} enc={enc:02x?}");
            }
        }
    }

    #[test]
    fn ordering_is_total_transitive_and_type_ranked() {
        for seed in [5u64, 0xBEEF] {
            let mut rng = SplitMix64(seed);
            for _ in 0..400 {
                let (a, b, c) = (rng.scalar(), rng.scalar(), rng.scalar());
                // Totality: cmp never panics and is antisymmetric.
                assert_eq!(a.cmp(&b).reverse(), b.cmp(&a), "seed={seed} {a:?} {b:?}");
                let encoded_a = a.encode().expect("small generated scalar encodes");
                let encoded_b = b.encode().expect("small generated scalar encodes");
                assert_eq!(
                    a.cmp(&b),
                    encoded_a.cmp(&encoded_b),
                    "semantic and canonical-byte order diverged for seed={seed}: {a:?} vs {b:?}"
                );
                assert_eq!(a == b, encoded_a == encoded_b);
                // Transitivity via sort correctness on the triple.
                let mut v = [a.clone(), b.clone(), c.clone()];
                v.sort();
                assert!(
                    v[0] <= v[1] && v[1] <= v[2] && v[0] <= v[2],
                    "seed={seed} {v:?}"
                );
            }
        }
        assert!(CanonicalScalar::Null < CanonicalScalar::Bool(false));
        assert!(CanonicalScalar::Bool(true) < CanonicalScalar::Int(i64::MIN));
        let decimal = CanonicalDecimal::from_coefficient(0).expect("zero decimal is canonical");
        assert!(CanonicalScalar::Int(i64::MAX) < CanonicalScalar::Decimal(decimal));
        assert!(CanonicalScalar::Decimal(decimal) < CanonicalScalar::Float(CanonicalF64::new(0.0)));
        assert!(
            CanonicalScalar::Float(CanonicalF64::new(f64::INFINITY))
                < CanonicalScalar::Text(ucs_basic(""))
        );
        let timestamp = CanonicalTimestamp::offset_only(0, 0).expect("epoch is canonical");
        assert!(
            CanonicalScalar::Text(ucs_basic("zzz")) < CanonicalScalar::Timestamp(timestamp.clone())
        );
        assert!(
            CanonicalScalar::Timestamp(timestamp)
                < CanonicalScalar::bytes(vec![]).expect("empty bytes are bounded")
        );
    }

    #[test]
    fn ordered_encoding_pins_numeric_and_variable_length_boundaries() {
        let mut samples = vec![
            CanonicalScalar::Null,
            CanonicalScalar::Bool(false),
            CanonicalScalar::Bool(true),
        ];
        samples.extend(
            [i64::MIN, -2, -1, 0, 1, 255, 256, i64::MAX]
                .into_iter()
                .map(CanonicalScalar::Int),
        );
        samples.extend(
            [
                f64::NEG_INFINITY,
                -2.0,
                -1.0,
                -f64::from_bits(1),
                0.0,
                f64::from_bits(1),
                1.0,
                2.0,
                f64::INFINITY,
                f64::NAN,
            ]
            .into_iter()
            .map(CanonicalF64::new)
            .map(CanonicalScalar::Float),
        );
        samples.extend(
            ["", "\0", "a", "aa", "b", "abcdefgh", "abcdefghi"]
                .into_iter()
                .map(|text| CanonicalScalar::Text(ucs_basic(text))),
        );
        samples.extend(
            [
                vec![],
                vec![0],
                vec![b'a'],
                b"abcdefgh".to_vec(),
                b"abcdefghi".to_vec(),
            ]
            .into_iter()
            .map(|bytes| CanonicalScalar::bytes(bytes).expect("small bytes are bounded")),
        );

        for left in &samples {
            for right in &samples {
                let left_encoding = left.encode().expect("boundary scalar encodes");
                let right_encoding = right.encode().expect("boundary scalar encodes");
                assert_eq!(left.cmp(right), left_encoding.cmp(&right_encoding));
                assert_eq!(left == right, left_encoding == right_encoding);
            }
        }

        assert_eq!(CanonicalBytes::max_len(), MAX_SCALAR_BYTES);
        assert!(
            comparable_encoded_len_for_len(MAX_SCALAR_BYTES)
                .expect("profile maximum length arithmetic is representable")
                <= MAX_SCALAR_PAYLOAD
        );
        assert!(
            comparable_encoded_len_for_len(MAX_SCALAR_BYTES + 1)
                .expect("one-past profile length arithmetic is representable")
                > MAX_SCALAR_PAYLOAD
        );
    }

    #[test]
    fn malformed_decodes_are_typed_rejections() {
        use ScalarDecodeError as E;
        assert_eq!(CanonicalScalar::decode(&[]), Err(E::Empty));
        assert_eq!(CanonicalScalar::decode(&[0x77]), Err(E::UnknownTag(0x77)));
        assert_eq!(CanonicalScalar::decode(&[0x01, 2]), Err(E::BadBool(2)));
        assert_eq!(
            CanonicalScalar::decode(&[0x00, 0xAA]),
            Err(E::WrongPayloadLength {
                tag: 0x00,
                expected: 0,
                got: 1
            })
        );
        assert_eq!(
            CanonicalScalar::decode(&[TAG_TEXT]),
            Err(E::TruncatedField {
                tag: TAG_TEXT,
                field: ScalarField::TextBinding,
                needed: 1,
                remaining: 0,
            })
        );
        let mut invalid_marker = vec![TAG_BYTES];
        invalid_marker.extend_from_slice(&[0; 8]);
        invalid_marker.push(0xF6);
        assert_eq!(
            CanonicalScalar::decode(&invalid_marker),
            Err(E::InvalidComparableMarker {
                tag: TAG_BYTES,
                field: ScalarField::Bytes,
                marker: 0xF6,
            })
        );
        let mut short = vec![TAG_BYTES];
        short.extend_from_slice(&[0; 8]);
        assert_eq!(
            CanonicalScalar::decode(&short),
            Err(E::TruncatedField {
                tag: TAG_BYTES,
                field: ScalarField::Bytes,
                needed: 9,
                remaining: 8,
            })
        );
        // Non-canonical float bits in a durable image: fail closed.
        let mut nc = vec![TAG_FLOAT];
        nc.extend_from_slice(&ordered_f64_bits(0x7FF0_0000_0000_0001).to_be_bytes());
        assert_eq!(
            CanonicalScalar::decode(&nc),
            Err(E::NonCanonicalFloat {
                bits: 0x7FF0_0000_0000_0001
            })
        );
        // Invalid UTF-8 text.
        let mut bad = vec![TAG_TEXT, 0];
        append_comparable(&[0xFF, 0xFE], &mut bad);
        assert_eq!(
            CanonicalScalar::decode(&bad),
            Err(E::Text(CanonicalTextError::InvalidUtf8))
        );

        let mut bad_decimal = vec![TAG_DECIMAL];
        bad_decimal.extend_from_slice(&ordered_i128(i128::MAX).to_be_bytes());
        assert_eq!(
            CanonicalScalar::decode(&bad_decimal),
            Err(E::Decimal(DecimalDecodeError::CoefficientOutOfRange {
                coefficient: i128::MAX,
            }))
        );

        let mut bounded_payload = Vec::new();
        append_comparable(b"too long", &mut bounded_payload);
        let mut cursor = ScalarCursor::new(TAG_BYTES, &bounded_payload)
            .expect("small encoded payload is below aggregate bound");
        assert!(matches!(
            cursor.read_comparable(ScalarField::Bytes, 2),
            Err(E::ComparableLengthOverflow {
                decoded: 8,
                maximum: 2,
                ..
            })
        ));
    }

    #[test]
    fn artifact_bound_scalars_fail_closed_without_exact_objects() {
        use ScalarDecodeError as E;

        let oid = |fill| ObjectId([fill; 32]);
        let binding = NonBinaryTextBinding::new(oid(1), oid(2), oid(3), oid(4));
        let text = CanonicalText::new_non_binary("Straße", binding, &AVAILABLE_RESOLVER)
            .expect("fixture artifacts are available");
        let scalar = CanonicalScalar::Text(text);
        let encoded = scalar.encode().expect("small scalar encoding allocates");
        assert_eq!(
            CanonicalScalar::decode(&encoded),
            Err(E::Text(CanonicalTextError::ResolverRequired))
        );
        assert_eq!(
            CanonicalScalar::decode_with_resolver(&encoded, &MISSING_RESOLVER),
            Err(E::Text(CanonicalTextError::MissingArtifact {
                role: TextArtifactRole::UnicodeData,
                object_id: oid(1),
            }))
        );
        assert_eq!(
            CanonicalScalar::decode_with_resolver(&encoded, &AVAILABLE_RESOLVER),
            Ok(scalar)
        );

        let mut forged = encoded.clone();
        forged[1 + 1 + 4 * 32] ^= 1;
        assert!(matches!(
            CanonicalScalar::decode_with_resolver(&forged, &AVAILABLE_RESOLVER),
            Err(E::Text(CanonicalTextError::SortKeyMismatch { .. }))
        ));

        let timestamp = CanonicalTimestamp::zoned(
            1_735_689_600_123_456_789,
            -5 * 60 * 60,
            "America/New_York",
            oid(9),
            &AVAILABLE_RESOLVER,
        )
        .expect("zoned fixture is canonical");
        let scalar = CanonicalScalar::Timestamp(timestamp);
        let encoded = scalar.encode().expect("small scalar encoding allocates");
        assert_eq!(
            CanonicalScalar::decode(&encoded),
            Err(E::Timestamp(TimestampDecodeError::TzdbResolverRequired))
        );
        assert_eq!(
            CanonicalScalar::decode_with_resolver(&encoded, &MISSING_RESOLVER),
            Err(E::Timestamp(TimestampDecodeError::InvalidValue(
                TimestampConstructionError::Tzdb(TimestampArtifactError::MissingTzdbArtifact {
                    required: oid(9)
                })
            )))
        );
        assert_eq!(
            CanonicalScalar::decode_with_resolver(&encoded, &AVAILABLE_RESOLVER),
            Ok(scalar)
        );
        assert!(matches!(
            CanonicalTimestamp::zoned(
                1_735_689_600_123_456_789,
                -4 * 60 * 60,
                "America/New_York",
                oid(9),
                &AVAILABLE_RESOLVER,
            ),
            Err(TimestampConstructionError::Tzdb(
                TimestampArtifactError::UtcOffsetMismatch {
                    stored_offset_seconds: -14_400,
                    resolved_offset_seconds: -18_000,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn arbitrary_bounded_scalar_bytes_are_a_total_decode_surface() {
        for seed in [0, 1, 0x0BAD_5EED, u64::MAX] {
            let mut rng = SplitMix64(seed);
            for length in 0..=128 {
                let bytes: Vec<u8> = (0..length).map(|_| rng.next() as u8).collect();
                let _ = CanonicalScalar::decode(&bytes);
                let _ = CanonicalScalar::decode_with_resolver(&bytes, &AVAILABLE_RESOLVER);
            }
        }
    }
}
