//! Exact bounded quantile summaries for small statistics windows.
//!
//! This profile stores a canonical sorted multiset until its explicit ceiling.
//! Reaching that ceiling is a typed escalation signal; it never silently swaps
//! to an approximate representation with a different error contract.

use core::fmt;
use std::collections::TryReserveError;

const CANONICAL_MAGIC: [u8; 8] = *b"FGDBEQS1";
const CANONICAL_VERSION: u16 = 1;
const CANONICAL_HEADER_BYTES: usize = 8 + 2 + 8 + 8;
const VALUE_BYTES: usize = 8;
const DEFAULT_MAX_DECODED_OBSERVATIONS: usize = 1 << 20;
const DEFAULT_MAX_ENCODED_BYTES: usize =
    CANONICAL_HEADER_BYTES + (DEFAULT_MAX_DECODED_OBSERVATIONS * VALUE_BYTES);

/// Caller-owned admission bounds for a canonical exact-quantile value.
///
/// The encoded ceiling remains part of merge identity, while these limits
/// independently cap work and memory before any decoded state is allocated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExactQuantileDecodeLimits {
    /// Largest accepted profile or concrete multiset length.
    pub max_observations: usize,
    /// Largest complete canonical value.
    pub max_encoded_bytes: usize,
}

impl ExactQuantileDecodeLimits {
    /// Conservative crate-level admission policy for small statistics windows.
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            max_observations: DEFAULT_MAX_DECODED_OBSERVATIONS,
            max_encoded_bytes: DEFAULT_MAX_ENCODED_BYTES,
        }
    }
}

/// Typed exact-summary transition failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExactQuantileError {
    /// Adding observations would exceed the immutable profile ceiling.
    ObservationLimitExceeded {
        /// Attempted completed length.
        attempted: usize,
        /// Immutable profile ceiling.
        maximum: usize,
    },
    /// Length arithmetic exceeded the platform index domain.
    LengthOverflow,
    /// The allocator rejected a checked reservation.
    AllocationFailed {
        /// Values requested for the replacement representation.
        requested: usize,
    },
    /// Merge operands have different ceilings.
    ProfileMismatch {
        /// Receiver ceiling.
        left_maximum: usize,
        /// Other ceiling.
        right_maximum: usize,
    },
    /// The requested value does not occur in the multiset.
    MissingObservation {
        /// Value the caller attempted to remove.
        value: u64,
    },
    /// A rational quantile must satisfy `denominator > 0` and
    /// `numerator <= denominator`.
    InvalidQuantile {
        /// Requested numerator.
        numerator: u64,
        /// Requested denominator.
        denominator: u64,
    },
}

impl fmt::Display for ExactQuantileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ObservationLimitExceeded { attempted, maximum } => write!(
                formatter,
                "exact quantile summary would contain {attempted} observations, maximum is {maximum}"
            ),
            Self::LengthOverflow => formatter.write_str("exact quantile summary length overflow"),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve {requested} exact quantile observations"
            ),
            Self::ProfileMismatch {
                left_maximum,
                right_maximum,
            } => write!(
                formatter,
                "exact quantile ceilings differ: {left_maximum} versus {right_maximum}"
            ),
            Self::MissingObservation { value } => {
                write!(
                    formatter,
                    "exact quantile summary does not contain value {value}"
                )
            }
            Self::InvalidQuantile {
                numerator,
                denominator,
            } => write!(
                formatter,
                "invalid quantile {numerator}/{denominator}; require 0 <= numerator <= denominator and denominator > 0"
            ),
        }
    }
}

impl std::error::Error for ExactQuantileError {}

/// Strict canonical-codec failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExactQuantileCodecError {
    /// A platform-sized field cannot be represented canonically.
    IntegerUnrepresentable,
    /// Exact encoded-size arithmetic overflowed.
    LengthOverflow,
    /// The allocator rejected the exact value or byte reservation.
    AllocationFailed {
        /// Requested element or byte count.
        requested: usize,
    },
    /// The eight-byte format discriminator did not match.
    MagicMismatch {
        /// Bytes found at the format discriminator.
        actual: [u8; 8],
    },
    /// The encoded version is unsupported.
    UnsupportedVersion {
        /// Version found in the input.
        actual: u16,
    },
    /// Input ended before a complete field could be read.
    Truncated {
        /// Byte offset of the field.
        offset: usize,
        /// Bytes needed for the field.
        needed: usize,
        /// Bytes remaining at the offset.
        remaining: usize,
    },
    /// Input contains bytes after the one canonical value.
    TrailingBytes {
        /// First trailing byte.
        offset: usize,
        /// Number of trailing bytes.
        remaining: usize,
    },
    /// The encoded multiset exceeds its immutable profile ceiling.
    ObservationLimitExceeded {
        /// Encoded multiset length.
        actual: usize,
        /// Encoded profile ceiling.
        maximum: usize,
    },
    /// Values are not in canonical nondecreasing order.
    ValuesOutOfOrder {
        /// First invalid index.
        index: usize,
        /// Preceding value.
        previous: u64,
        /// Current value.
        current: u64,
    },
    /// The complete encoded value exceeds the caller-owned byte budget.
    EncodedByteLimitExceeded {
        /// Input byte length.
        actual: usize,
        /// Caller-owned byte ceiling.
        maximum: usize,
    },
    /// The encoded profile or multiset exceeds the caller-owned value budget.
    DecodeObservationLimitExceeded {
        /// Encoded ceiling or concrete multiset length.
        actual: usize,
        /// Caller-owned observation ceiling.
        maximum: usize,
    },
    /// The encoded profile is not the registry-selected profile.
    ProfileMismatch {
        /// Ceiling selected by trusted metadata.
        expected_max_observations: usize,
        /// Ceiling found in the canonical value.
        actual_max_observations: usize,
    },
}

impl fmt::Display for ExactQuantileCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ExactQuantileCodecError {}

/// Canonical sorted multiset with exact selection and deletion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactQuantileSketch {
    max_observations: usize,
    values: Vec<u64>,
}

impl ExactQuantileSketch {
    /// Creates an empty, allocation-free exact summary.
    #[must_use]
    pub const fn new(max_observations: usize) -> Self {
        Self {
            max_observations,
            values: Vec::new(),
        }
    }

    /// Creates an empty summary with a checked allocation hint.
    pub fn try_with_capacity(
        max_observations: usize,
        capacity: usize,
    ) -> Result<Self, ExactQuantileError> {
        if capacity > max_observations {
            return Err(ExactQuantileError::ObservationLimitExceeded {
                attempted: capacity,
                maximum: max_observations,
            });
        }
        let mut values = Vec::new();
        values
            .try_reserve_exact(capacity)
            .map_err(|_: TryReserveError| ExactQuantileError::AllocationFailed {
                requested: capacity,
            })?;
        Ok(Self {
            max_observations,
            values,
        })
    }

    /// Immutable observation ceiling.
    #[must_use]
    pub const fn max_observations(&self) -> usize {
        self.max_observations
    }

    /// Current multiset cardinality, including duplicates.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the multiset is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Canonical nondecreasing multiset state.
    #[must_use]
    pub fn canonical_values(&self) -> &[u64] {
        &self.values
    }

    /// Encodes the complete ceiling and sorted multiset into canonical bytes.
    /// Fixed-width integers use the workspace's little-endian durable law.
    pub fn try_to_canonical_bytes(&self) -> Result<Vec<u8>, ExactQuantileCodecError> {
        validate_canonical_values(self.max_observations, &self.values)?;
        let payload_bytes = self
            .values
            .len()
            .checked_mul(VALUE_BYTES)
            .ok_or(ExactQuantileCodecError::LengthOverflow)?;
        let encoded_len = CANONICAL_HEADER_BYTES
            .checked_add(payload_bytes)
            .ok_or(ExactQuantileCodecError::LengthOverflow)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(encoded_len)
            .map_err(
                |_: TryReserveError| ExactQuantileCodecError::AllocationFailed {
                    requested: encoded_len,
                },
            )?;
        bytes.extend_from_slice(&CANONICAL_MAGIC);
        push_u16(&mut bytes, CANONICAL_VERSION);
        push_u64(&mut bytes, canonical_usize(self.max_observations)?);
        push_u64(&mut bytes, canonical_usize(self.values.len())?);
        for value in &self.values {
            push_u64(&mut bytes, *value);
        }
        debug_assert_eq!(bytes.len(), encoded_len);
        Ok(bytes)
    }

    /// Decodes exactly one canonical sorted multiset.
    ///
    /// The byte length and immutable ceiling are checked before allocating
    /// storage for decoded observations.
    pub fn try_from_canonical_bytes(
        bytes: &[u8],
        expected_max_observations: usize,
        limits: ExactQuantileDecodeLimits,
    ) -> Result<Self, ExactQuantileCodecError> {
        if bytes.len() > limits.max_encoded_bytes {
            return Err(ExactQuantileCodecError::EncodedByteLimitExceeded {
                actual: bytes.len(),
                maximum: limits.max_encoded_bytes,
            });
        }
        let mut decoder = ExactQuantileDecoder::new(bytes);
        let magic = decoder.read_array::<8>()?;
        if magic != CANONICAL_MAGIC {
            return Err(ExactQuantileCodecError::MagicMismatch { actual: magic });
        }
        let version = decoder.read_u16()?;
        if version != CANONICAL_VERSION {
            return Err(ExactQuantileCodecError::UnsupportedVersion { actual: version });
        }
        let max_observations = decoded_usize(decoder.read_u64()?)?;
        let value_count = decoded_usize(decoder.read_u64()?)?;
        if max_observations != expected_max_observations {
            return Err(ExactQuantileCodecError::ProfileMismatch {
                expected_max_observations,
                actual_max_observations: max_observations,
            });
        }
        if max_observations > limits.max_observations {
            return Err(ExactQuantileCodecError::DecodeObservationLimitExceeded {
                actual: max_observations,
                maximum: limits.max_observations,
            });
        }
        if value_count > limits.max_observations {
            return Err(ExactQuantileCodecError::DecodeObservationLimitExceeded {
                actual: value_count,
                maximum: limits.max_observations,
            });
        }
        if value_count > max_observations {
            return Err(ExactQuantileCodecError::ObservationLimitExceeded {
                actual: value_count,
                maximum: max_observations,
            });
        }
        let payload_bytes = value_count
            .checked_mul(VALUE_BYTES)
            .ok_or(ExactQuantileCodecError::LengthOverflow)?;
        let expected_len = CANONICAL_HEADER_BYTES
            .checked_add(payload_bytes)
            .ok_or(ExactQuantileCodecError::LengthOverflow)?;
        if bytes.len() < expected_len {
            return Err(ExactQuantileCodecError::Truncated {
                offset: decoder.offset,
                needed: payload_bytes,
                remaining: bytes.len().saturating_sub(decoder.offset),
            });
        }
        if bytes.len() > expected_len {
            return Err(ExactQuantileCodecError::TrailingBytes {
                offset: expected_len,
                remaining: bytes.len() - expected_len,
            });
        }

        let payload_offset = decoder.offset;
        let mut preflight = ExactQuantileDecoder {
            bytes,
            offset: payload_offset,
        };
        let mut previous = None;
        for index in 0..value_count {
            let current = preflight.read_u64()?;
            if let Some(previous) = previous
                && previous > current
            {
                return Err(ExactQuantileCodecError::ValuesOutOfOrder {
                    index,
                    previous,
                    current,
                });
            }
            previous = Some(current);
        }
        preflight.finish()?;

        let mut values = Vec::new();
        values
            .try_reserve_exact(value_count)
            .map_err(
                |_: TryReserveError| ExactQuantileCodecError::AllocationFailed {
                    requested: value_count,
                },
            )?;
        let mut decoder = ExactQuantileDecoder {
            bytes,
            offset: payload_offset,
        };
        for _ in 0..value_count {
            values.push(decoder.read_u64()?);
        }
        decoder.finish()?;
        Ok(Self {
            max_observations,
            values,
        })
    }

    /// Inserts one value while preserving canonical order.
    ///
    /// Limit and allocation failures occur before the multiset changes.
    pub fn try_observe(&mut self, value: u64) -> Result<(), ExactQuantileError> {
        let attempted = self
            .values
            .len()
            .checked_add(1)
            .ok_or(ExactQuantileError::LengthOverflow)?;
        if attempted > self.max_observations {
            return Err(ExactQuantileError::ObservationLimitExceeded {
                attempted,
                maximum: self.max_observations,
            });
        }
        self.values
            .try_reserve_exact(1)
            .map_err(|_: TryReserveError| ExactQuantileError::AllocationFailed {
                requested: attempted,
            })?;
        let insertion = self.values.partition_point(|existing| *existing <= value);
        self.values.insert(insertion, value);
        Ok(())
    }

    /// Removes one occurrence of `value`.
    pub fn try_remove(&mut self, value: u64) -> Result<(), ExactQuantileError> {
        let index = self
            .values
            .binary_search(&value)
            .map_err(|_| ExactQuantileError::MissingObservation { value })?;
        self.values.remove(index);
        Ok(())
    }

    /// Atomically merges another summary with the identical ceiling.
    pub fn try_merge(&mut self, other: &Self) -> Result<(), ExactQuantileError> {
        if self.max_observations != other.max_observations {
            return Err(ExactQuantileError::ProfileMismatch {
                left_maximum: self.max_observations,
                right_maximum: other.max_observations,
            });
        }
        let merged_len = self
            .values
            .len()
            .checked_add(other.values.len())
            .ok_or(ExactQuantileError::LengthOverflow)?;
        if merged_len > self.max_observations {
            return Err(ExactQuantileError::ObservationLimitExceeded {
                attempted: merged_len,
                maximum: self.max_observations,
            });
        }

        let mut merged = Vec::new();
        merged
            .try_reserve_exact(merged_len)
            .map_err(|_: TryReserveError| ExactQuantileError::AllocationFailed {
                requested: merged_len,
            })?;
        let mut left = self.values.iter().copied().peekable();
        let mut right = other.values.iter().copied().peekable();
        while let (Some(left_value), Some(right_value)) = (left.peek(), right.peek()) {
            if left_value <= right_value {
                if let Some(value) = left.next() {
                    merged.push(value);
                }
            } else if let Some(value) = right.next() {
                merged.push(value);
            }
        }
        merged.extend(left);
        merged.extend(right);
        self.values = merged;
        Ok(())
    }

    /// Returns the zero-based ordered observation.
    #[must_use]
    pub fn select(&self, ordinal: usize) -> Option<u64> {
        self.values.get(ordinal).copied()
    }

    /// Returns the deterministic lower rational quantile.
    ///
    /// For `n` observations the selected index is
    /// `floor(numerator * (n - 1) / denominator)`, making `0/denominator` the
    /// minimum and `denominator/denominator` the maximum.
    pub fn quantile(
        &self,
        numerator: u64,
        denominator: u64,
    ) -> Result<Option<u64>, ExactQuantileError> {
        if denominator == 0 || numerator > denominator {
            return Err(ExactQuantileError::InvalidQuantile {
                numerator,
                denominator,
            });
        }
        let Some(last_index) = self.values.len().checked_sub(1) else {
            return Ok(None);
        };
        let scaled = u128::from(numerator) * last_index as u128;
        let index = (scaled / u128::from(denominator)) as usize;
        Ok(self.values.get(index).copied())
    }
}

fn validate_canonical_values(
    max_observations: usize,
    values: &[u64],
) -> Result<(), ExactQuantileCodecError> {
    if values.len() > max_observations {
        return Err(ExactQuantileCodecError::ObservationLimitExceeded {
            actual: values.len(),
            maximum: max_observations,
        });
    }
    for (offset, adjacent) in values.windows(2).enumerate() {
        let [previous, current] = adjacent else {
            continue;
        };
        if previous > current {
            return Err(ExactQuantileCodecError::ValuesOutOfOrder {
                index: offset + 1,
                previous: *previous,
                current: *current,
            });
        }
    }
    Ok(())
}

fn canonical_usize(value: usize) -> Result<u64, ExactQuantileCodecError> {
    u64::try_from(value).map_err(|_| ExactQuantileCodecError::IntegerUnrepresentable)
}

fn decoded_usize(value: u64) -> Result<usize, ExactQuantileCodecError> {
    usize::try_from(value).map_err(|_| ExactQuantileCodecError::IntegerUnrepresentable)
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

struct ExactQuantileDecoder<'bytes> {
    bytes: &'bytes [u8],
    offset: usize,
}

impl<'bytes> ExactQuantileDecoder<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_array<const LENGTH: usize>(&mut self) -> Result<[u8; LENGTH], ExactQuantileCodecError> {
        let end = self
            .offset
            .checked_add(LENGTH)
            .ok_or(ExactQuantileCodecError::LengthOverflow)?;
        let Some(source) = self.bytes.get(self.offset..end) else {
            return Err(ExactQuantileCodecError::Truncated {
                offset: self.offset,
                needed: LENGTH,
                remaining: self.bytes.len().saturating_sub(self.offset),
            });
        };
        let mut value = [0_u8; LENGTH];
        value.copy_from_slice(source);
        self.offset = end;
        Ok(value)
    }

    fn read_u16(&mut self) -> Result<u16, ExactQuantileCodecError> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_u64(&mut self) -> Result<u64, ExactQuantileCodecError> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    fn finish(self) -> Result<(), ExactQuantileCodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(ExactQuantileCodecError::TrailingBytes {
                offset: self.offset,
                remaining: self.bytes.len() - self.offset,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(values: &[u64]) -> ExactQuantileSketch {
        let mut sketch = ExactQuantileSketch::new(100);
        for &value in values {
            sketch.try_observe(value).expect("within ceiling");
        }
        sketch
    }

    fn read_fixture(bytes: &[u8]) -> Result<ExactQuantileSketch, ExactQuantileCodecError> {
        ExactQuantileSketch::try_from_canonical_bytes(
            bytes,
            100,
            ExactQuantileDecodeLimits::conservative(),
        )
    }

    #[test]
    fn insertion_order_canonicalizes_the_multiset() {
        let forward = summary(&[9, 1, 5, 5, 2, u64::MAX, 0]);
        let reverse = summary(&[0, u64::MAX, 2, 5, 5, 1, 9]);
        assert_eq!(forward, reverse);
        assert_eq!(forward.canonical_values(), &[0, 1, 2, 5, 5, 9, u64::MAX]);
    }

    #[test]
    fn incremental_growth_never_reserves_past_the_profile_ceiling() {
        let mut sketch = ExactQuantileSketch::new(17);
        for value in 0_u64..17 {
            sketch.try_observe(value).expect("observation fits");
            assert!(sketch.values.capacity() <= sketch.max_observations);
        }
    }

    #[test]
    fn canonical_codec_round_trips_and_is_insertion_order_independent() {
        let forward = summary(&[9, 1, 5, 5, 2, u64::MAX, 0]);
        let reverse = summary(&[0, u64::MAX, 2, 5, 5, 1, 9]);
        let forward_bytes = forward
            .try_to_canonical_bytes()
            .expect("valid sorted multiset");
        let reverse_bytes = reverse
            .try_to_canonical_bytes()
            .expect("valid sorted multiset");
        assert_eq!(forward_bytes, reverse_bytes);
        assert_eq!(&forward_bytes[..8], b"FGDBEQS1");
        assert_eq!(&forward_bytes[8..10], &1_u16.to_le_bytes());

        let decoded = read_fixture(&forward_bytes).expect("canonical summary");
        assert_eq!(decoded, forward);
        assert_eq!(
            decoded.try_to_canonical_bytes().expect("decoded summary"),
            forward_bytes
        );
    }

    #[test]
    fn canonical_decoder_rejects_noncanonical_or_incomplete_values() {
        let encoded = summary(&[1, 2, 3])
            .try_to_canonical_bytes()
            .expect("valid summary");

        let mut wrong_magic = encoded.clone();
        wrong_magic[0] ^= 1;
        assert!(matches!(
            read_fixture(&wrong_magic),
            Err(ExactQuantileCodecError::MagicMismatch { .. })
        ));

        let mut wrong_version = encoded.clone();
        wrong_version[8..10].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            read_fixture(&wrong_version),
            Err(ExactQuantileCodecError::UnsupportedVersion { actual: 2 })
        );

        let mut above_limit = encoded.clone();
        above_limit[18..26].copy_from_slice(&101_u64.to_le_bytes());
        assert_eq!(
            read_fixture(&above_limit),
            Err(ExactQuantileCodecError::ObservationLimitExceeded {
                actual: 101,
                maximum: 100,
            })
        );

        let mut out_of_order = encoded.clone();
        out_of_order[26..34].copy_from_slice(&3_u64.to_le_bytes());
        out_of_order[34..42].copy_from_slice(&2_u64.to_le_bytes());
        assert_eq!(
            read_fixture(&out_of_order),
            Err(ExactQuantileCodecError::ValuesOutOfOrder {
                index: 1,
                previous: 3,
                current: 2,
            })
        );

        assert!(matches!(
            read_fixture(&encoded[..encoded.len() - 1]),
            Err(ExactQuantileCodecError::Truncated { .. })
        ));

        let mut trailing = encoded;
        trailing.push(0);
        assert!(matches!(
            read_fixture(&trailing),
            Err(ExactQuantileCodecError::TrailingBytes { remaining: 1, .. })
        ));
    }

    #[test]
    fn canonical_decoder_enforces_trusted_profile_and_resource_bounds() {
        let encoded = summary(&[1, 2, 3])
            .try_to_canonical_bytes()
            .expect("valid summary");

        assert_eq!(
            ExactQuantileSketch::try_from_canonical_bytes(
                &encoded,
                99,
                ExactQuantileDecodeLimits::conservative(),
            ),
            Err(ExactQuantileCodecError::ProfileMismatch {
                expected_max_observations: 99,
                actual_max_observations: 100,
            })
        );

        let limits = ExactQuantileDecodeLimits {
            max_observations: 99,
            max_encoded_bytes: encoded.len(),
        };
        assert_eq!(
            ExactQuantileSketch::try_from_canonical_bytes(&encoded, 100, limits),
            Err(ExactQuantileCodecError::DecodeObservationLimitExceeded {
                actual: 100,
                maximum: 99,
            })
        );

        let limits = ExactQuantileDecodeLimits {
            max_observations: 100,
            max_encoded_bytes: encoded.len() - 1,
        };
        assert_eq!(
            ExactQuantileSketch::try_from_canonical_bytes(&encoded, 100, limits),
            Err(ExactQuantileCodecError::EncodedByteLimitExceeded {
                actual: encoded.len(),
                maximum: encoded.len() - 1,
            })
        );
    }

    #[test]
    fn rational_quantiles_have_fixed_endpoint_and_rounding_semantics() {
        let sketch = summary(&[10, 20, 30, 40, 50]);
        assert_eq!(sketch.quantile(0, 1), Ok(Some(10)));
        assert_eq!(sketch.quantile(1, 4), Ok(Some(20)));
        assert_eq!(sketch.quantile(1, 2), Ok(Some(30)));
        assert_eq!(sketch.quantile(3, 4), Ok(Some(40)));
        assert_eq!(sketch.quantile(1, 1), Ok(Some(50)));
        assert_eq!(
            sketch.quantile(2, 1),
            Err(ExactQuantileError::InvalidQuantile {
                numerator: 2,
                denominator: 1,
            })
        );
        assert_eq!(
            sketch.quantile(0, 0),
            Err(ExactQuantileError::InvalidQuantile {
                numerator: 0,
                denominator: 0,
            })
        );
        assert_eq!(ExactQuantileSketch::new(0).quantile(0, 1), Ok(None));
    }

    #[test]
    fn deletion_is_exact_for_duplicates_and_missing_values_are_typed() {
        let mut sketch = summary(&[1, 2, 2, 2, 3]);
        sketch.try_remove(2).expect("duplicate exists");
        assert_eq!(sketch.canonical_values(), &[1, 2, 2, 3]);
        sketch.try_remove(2).expect("duplicate exists");
        sketch.try_remove(2).expect("duplicate exists");
        assert_eq!(
            sketch.try_remove(2),
            Err(ExactQuantileError::MissingObservation { value: 2 })
        );
        assert_eq!(sketch.canonical_values(), &[1, 3]);
    }

    #[test]
    fn merge_preserves_sorted_order_and_is_split_exact() {
        // `merge_is_commutative_and_associative` cannot constrain this kernel.
        // The merge walks two sorted runs and picks a side each step; reversing
        // that choice still yields a commutative, associative merge of the same
        // multiset -- just in the wrong ORDER. Verified by mutation: flipping
        // `left_value <= right_value` to `>=` left the entire crate suite green.
        //
        // Order is not cosmetic here. `select` and `quantile` index into the
        // values array, so an unsorted merge silently returns the wrong
        // quantile rather than failing.
        fn part(values: &[u64]) -> ExactQuantileSketch {
            let mut sketch =
                ExactQuantileSketch::try_with_capacity(64, 16).expect("bounded capacity");
            for &value in values {
                sketch.try_observe(value).expect("within ceiling");
            }
            sketch
        }

        // 1. SORTEDNESS. Interleaved runs are used deliberately: two runs whose
        // ranges do not overlap would merge correctly under either choice, so
        // they could not distinguish the operators.
        let mut left = part(&[1, 4, 6, 9]);
        let right = part(&[2, 3, 7, 8]);
        left.try_merge(&right).expect("matching profile");
        let merged = left.canonical_values();
        assert!(
            merged.windows(2).all(|pair| pair[0] <= pair[1]),
            "merged values must be sorted ascending, got {merged:?}"
        );
        assert_eq!(merged, &[1, 2, 3, 4, 6, 7, 8, 9]);

        // Duplicates must survive: this is a multiset, so a merge that deduped
        // would change every quantile above the duplicate.
        let mut dup_left = part(&[5, 5, 7]);
        dup_left
            .try_merge(&part(&[5, 6]))
            .expect("matching profile");
        assert_eq!(dup_left.canonical_values(), &[5, 5, 5, 6, 7]);

        // 2. SPLIT EXACTNESS, on canonical bytes. Partitioning a stream and
        // merging must equal accumulating it whole -- asserted on bytes rather
        // than on a selected quantile, since two different orderings can agree
        // at one ordinal and disagree elsewhere.
        let a = [9_u64, 1, 6, 4];
        let b = [3_u64, 8, 2, 7];
        let mut whole = ExactQuantileSketch::try_with_capacity(64, 16).expect("bounded capacity");
        for &value in a.iter().chain(b.iter()) {
            whole.try_observe(value).expect("within ceiling");
        }
        let mut split = part(&a);
        split.try_merge(&part(&b)).expect("matching profile");
        assert_eq!(
            split.try_to_canonical_bytes().expect("canonical bytes"),
            whole.try_to_canonical_bytes().expect("canonical bytes"),
            "splitting the stream and merging must equal accumulating it whole"
        );

        // 3. The quantile actually read through the public selector, so the
        // ordering law is tied to the observable it protects.
        assert_eq!(split.select(0), Some(1));
        assert_eq!(split.select(7), Some(9));
    }

    #[test]
    fn merge_is_commutative_and_associative() {
        let a = summary(&[9, 1, 5]);
        let b = summary(&[4, 4, 8]);
        let c = summary(&[0, 10]);

        let mut left = a.clone();
        left.try_merge(&b).expect("same profile");
        let mut right = b.clone();
        right.try_merge(&a).expect("same profile");
        assert_eq!(left, right);

        let mut ab_c = left;
        ab_c.try_merge(&c).expect("same profile");
        let mut bc = b;
        bc.try_merge(&c).expect("same profile");
        let mut a_bc = a;
        a_bc.try_merge(&bc).expect("same profile");
        assert_eq!(ab_c, a_bc);
    }

    #[test]
    fn limit_and_profile_failures_are_atomic() {
        let mut bounded = ExactQuantileSketch::new(2);
        bounded.try_observe(1).expect("within ceiling");
        bounded.try_observe(2).expect("within ceiling");
        let before = bounded.clone();
        assert_eq!(
            bounded.try_observe(3),
            Err(ExactQuantileError::ObservationLimitExceeded {
                attempted: 3,
                maximum: 2,
            })
        );
        assert_eq!(bounded, before);

        let other = ExactQuantileSketch::new(3);
        assert_eq!(
            bounded.try_merge(&other),
            Err(ExactQuantileError::ProfileMismatch {
                left_maximum: 2,
                right_maximum: 3,
            })
        );
        assert_eq!(bounded, before);
    }
}
