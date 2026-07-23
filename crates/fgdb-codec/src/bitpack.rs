//! Canonical scalar bitpacking and frame-of-reference encoding.
//!
//! Values are concatenated least-significant bit first; bytes likewise carry
//! the earlier bits in their least-significant positions. Unused high bits in
//! the final byte must be zero, making each `(count, width, values)` tuple's
//! representation unique. Decoders validate the exact input length and final
//! padding before allocating their result.

#![forbid(unsafe_code)]

use core::fmt;

/// Largest supported fixed bit width for `u64` values.
pub const MAX_BIT_WIDTH: u8 = 64;

/// Hard ceiling on values materialized by one scalar decode call.
///
/// Larger logical sequences must be framed and decoded in bounded chunks. The
/// ceiling is independent of encoded byte length so a zero-width payload cannot
/// authorize an unbounded allocation. This is only the scalar kernel's final
/// 128 MiB materialization ceiling; registered durable callers must enforce
/// their tighter per-kind maximum and active resource budget before calling.
pub const MAX_DECODED_VALUES: usize = 1 << 24;

/// Allocation whose reservation failed during codec operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocationTarget {
    /// Canonical packed output bytes.
    EncodedBytes,
    /// Decoded `u64` values.
    DecodedValues,
    /// Temporary frame-of-reference deltas.
    FrameOfReferenceDeltas,
}

/// Checked bitpacking or frame-of-reference failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BitpackError {
    /// Width lies outside the closed `0..=64` domain.
    InvalidWidth {
        /// Rejected width.
        width: u8,
    },
    /// The exact packed byte length is not representable as `usize`.
    ByteLengthOverflow {
        /// Number of logical values.
        count: usize,
        /// Bits allocated to each value.
        width: u8,
    },
    /// One decode requested more materialized values than the hard ceiling.
    ValueCountLimitExceeded {
        /// Requested logical value count.
        count: usize,
        /// Enforced ceiling.
        limit: usize,
    },
    /// Packed input ends before the exact expected length.
    TruncatedInput {
        /// Exact number of bytes required by `(count, width)`.
        expected: usize,
        /// Number of bytes supplied.
        actual: usize,
    },
    /// Packed input contains bytes beyond the exact expected length.
    TrailingBytes {
        /// Exact number of bytes required by `(count, width)`.
        expected: usize,
        /// Number of bytes supplied.
        actual: usize,
    },
    /// A caller-provided output slice is too short.
    OutputTooSmall {
        /// Exact number of bytes required.
        required: usize,
        /// Number of bytes available.
        available: usize,
    },
    /// A logical value does not fit the selected width.
    ValueOutOfRange {
        /// Zero-based logical value index.
        index: usize,
        /// Rejected value (or FOR delta).
        value: u64,
        /// Selected bit width.
        width: u8,
    },
    /// Unused high bits in the final byte are nonzero.
    NonZeroPadding {
        /// Index of the final encoded byte.
        byte_index: usize,
        /// Rejected byte value.
        byte: u8,
        /// Mask selecting the bits which must have been zero.
        forbidden_mask: u8,
    },
    /// A frame-of-reference value is smaller than its declared base.
    ValueBelowBase {
        /// Zero-based logical value index.
        index: usize,
        /// Rejected source value.
        value: u64,
        /// Declared frame base.
        base: u64,
    },
    /// Adding a decoded delta to the frame base overflowed `u64`.
    DecodedValueOverflow {
        /// Zero-based logical value index.
        index: usize,
        /// Declared frame base.
        base: u64,
        /// Decoded delta.
        delta: u64,
    },
    /// Reserving output storage failed before mutation.
    AllocationFailed {
        /// Storage being reserved.
        target: AllocationTarget,
        /// Number of bytes or elements requested, according to `target`.
        requested: usize,
    },
}

impl fmt::Display for BitpackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::InvalidWidth { width } => {
                write!(formatter, "bit width {width} is outside 0..=64")
            }
            Self::ByteLengthOverflow { count, width } => write!(
                formatter,
                "packed byte length overflows usize for count {count} and width {width}"
            ),
            Self::ValueCountLimitExceeded { count, limit } => write!(
                formatter,
                "packed decode count {count} exceeds the hard limit {limit}"
            ),
            Self::TruncatedInput { expected, actual } => write!(
                formatter,
                "packed input is truncated: expected {expected} bytes, got {actual}"
            ),
            Self::TrailingBytes { expected, actual } => write!(
                formatter,
                "packed input has trailing bytes: expected {expected}, got {actual}"
            ),
            Self::OutputTooSmall {
                required,
                available,
            } => write!(
                formatter,
                "packed output needs {required} bytes but has {available}"
            ),
            Self::ValueOutOfRange {
                index,
                value,
                width,
            } => write!(
                formatter,
                "value {value} at index {index} does not fit width {width}"
            ),
            Self::NonZeroPadding {
                byte_index,
                byte,
                forbidden_mask,
            } => write!(
                formatter,
                "packed byte {byte_index} ({byte:#04x}) has nonzero padding under mask {forbidden_mask:#04x}"
            ),
            Self::ValueBelowBase { index, value, base } => write!(
                formatter,
                "FOR value {value} at index {index} is below base {base}"
            ),
            Self::DecodedValueOverflow { index, base, delta } => write!(
                formatter,
                "FOR base {base} plus delta {delta} overflows at index {index}"
            ),
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "could not reserve {requested} units for {target:?}"
            ),
        }
    }
}

impl std::error::Error for BitpackError {}

/// Computes the exact byte length for `count` values of `width` bits each.
///
/// The division is performed before multiplication where possible. This both
/// checks real output-size overflow and avoids rejecting representable cases
/// such as `(usize::MAX, 1)` merely because the intermediate bit count would
/// overflow.
pub fn expected_byte_len(count: usize, width: u8) -> Result<usize, BitpackError> {
    validate_width(width)?;

    let groups_of_eight = count / 8;
    let remaining_values = count % 8;
    let complete_group_bytes = groups_of_eight
        .checked_mul(usize::from(width))
        .ok_or(BitpackError::ByteLengthOverflow { count, width })?;
    let remaining_bits = remaining_values * usize::from(width);
    let remaining_bytes = remaining_bits.div_ceil(8);
    complete_group_bytes
        .checked_add(remaining_bytes)
        .ok_or(BitpackError::ByteLengthOverflow { count, width })
}

/// Encodes fixed-width values into a newly allocated canonical byte vector.
pub fn encode(values: &[u64], width: u8) -> Result<Vec<u8>, BitpackError> {
    let expected = expected_byte_len(values.len(), width)?;
    validate_values(values, width)?;

    let mut output = Vec::new();
    output
        .try_reserve_exact(expected)
        .map_err(|_| BitpackError::AllocationFailed {
            target: AllocationTarget::EncodedBytes,
            requested: expected,
        })?;
    output.resize(expected, 0);
    pack_validated(values, width, &mut output)?;
    Ok(output)
}

/// Encodes into the start of caller storage and returns the exact byte count.
///
/// Values and capacity are checked before `output` is changed. Bytes after the
/// returned count are left untouched.
pub fn encode_into(values: &[u64], width: u8, output: &mut [u8]) -> Result<usize, BitpackError> {
    let expected = expected_byte_len(values.len(), width)?;
    validate_values(values, width)?;
    if output.len() < expected {
        return Err(BitpackError::OutputTooSmall {
            required: expected,
            available: output.len(),
        });
    }

    output[..expected].fill(0);
    pack_validated(values, width, &mut output[..expected])?;
    Ok(expected)
}

/// Decodes exactly `count` fixed-width values.
///
/// Exact length and canonical padding are validated before result allocation.
pub fn decode(input: &[u8], count: usize, width: u8) -> Result<Vec<u64>, BitpackError> {
    if count > MAX_DECODED_VALUES {
        return Err(BitpackError::ValueCountLimitExceeded {
            count,
            limit: MAX_DECODED_VALUES,
        });
    }
    let expected = expected_byte_len(count, width)?;
    validate_input(input, count, width, expected)?;

    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| BitpackError::AllocationFailed {
            target: AllocationTarget::DecodedValues,
            requested: count,
        })?;

    if width == 0 {
        values.resize(count, 0);
        return Ok(values);
    }

    let width_usize = usize::from(width);
    let mut bit_cursor = 0_usize;
    for _ in 0..count {
        let mut value = 0_u64;
        let mut value_bit = 0_usize;
        while value_bit < width_usize {
            let byte_index = bit_cursor / 8;
            let bit_in_byte = bit_cursor % 8;
            let take = (8 - bit_in_byte).min(width_usize - value_bit);
            let mask = low_mask(take);
            let chunk = (input[byte_index] >> bit_in_byte) & mask;
            value |= u64::from(chunk) << value_bit;
            value_bit += take;
            bit_cursor = bit_cursor
                .checked_add(take)
                .ok_or(BitpackError::ByteLengthOverflow { count, width })?;
        }
        values.push(value);
    }

    Ok(values)
}

/// Frame-of-reference encodes `values` as checked `value - base` deltas.
///
/// This is a deterministic kernel for a caller-selected `(base, width)`, not a
/// canonical base/width selector. The registered enclosing format or codec
/// profile owns that choice and records it in its frame.
pub fn encode_for(values: &[u64], base: u64, width: u8) -> Result<Vec<u8>, BitpackError> {
    // Establish width and byte-length validity before allocating temporary
    // deltas. `encode` repeats this inexpensive check at the trust boundary.
    let _ = expected_byte_len(values.len(), width)?;

    let mut deltas = Vec::new();
    deltas
        .try_reserve_exact(values.len())
        .map_err(|_| BitpackError::AllocationFailed {
            target: AllocationTarget::FrameOfReferenceDeltas,
            requested: values.len(),
        })?;
    for (index, &value) in values.iter().enumerate() {
        let delta = value
            .checked_sub(base)
            .ok_or(BitpackError::ValueBelowBase { index, value, base })?;
        deltas.push(delta);
    }
    encode(&deltas, width)
}

/// Decodes exactly `count` FOR deltas and checked-adds `base` to each one.
pub fn decode_for(
    input: &[u8],
    count: usize,
    base: u64,
    width: u8,
) -> Result<Vec<u64>, BitpackError> {
    let mut values = decode(input, count, width)?;
    for (index, value) in values.iter_mut().enumerate() {
        let delta = *value;
        *value = base
            .checked_add(delta)
            .ok_or(BitpackError::DecodedValueOverflow { index, base, delta })?;
    }
    Ok(values)
}

fn validate_width(width: u8) -> Result<(), BitpackError> {
    if width > MAX_BIT_WIDTH {
        return Err(BitpackError::InvalidWidth { width });
    }
    Ok(())
}

fn validate_values(values: &[u64], width: u8) -> Result<(), BitpackError> {
    debug_assert!(width <= MAX_BIT_WIDTH);
    if width == MAX_BIT_WIDTH {
        return Ok(());
    }

    let exclusive_limit = 1_u64 << width;
    for (index, &value) in values.iter().enumerate() {
        if value >= exclusive_limit {
            return Err(BitpackError::ValueOutOfRange {
                index,
                value,
                width,
            });
        }
    }
    Ok(())
}

fn validate_input(
    input: &[u8],
    count: usize,
    width: u8,
    expected: usize,
) -> Result<(), BitpackError> {
    match input.len().cmp(&expected) {
        core::cmp::Ordering::Less => {
            return Err(BitpackError::TruncatedInput {
                expected,
                actual: input.len(),
            });
        }
        core::cmp::Ordering::Greater => {
            return Err(BitpackError::TrailingBytes {
                expected,
                actual: input.len(),
            });
        }
        core::cmp::Ordering::Equal => {}
    }

    let used_bits = ((count % 8) * (usize::from(width) % 8)) % 8;
    if used_bits != 0 {
        let allowed_mask = low_mask(used_bits);
        let forbidden_mask = !allowed_mask;
        let byte_index = expected - 1;
        let byte = input[byte_index];
        if byte & forbidden_mask != 0 {
            return Err(BitpackError::NonZeroPadding {
                byte_index,
                byte,
                forbidden_mask,
            });
        }
    }
    Ok(())
}

fn pack_validated(values: &[u64], width: u8, output: &mut [u8]) -> Result<(), BitpackError> {
    if width == 0 {
        return Ok(());
    }

    let width_usize = usize::from(width);
    let mut bit_cursor = 0_usize;
    for &value in values {
        let mut value_bit = 0_usize;
        while value_bit < width_usize {
            let byte_index = bit_cursor / 8;
            let bit_in_byte = bit_cursor % 8;
            let take = (8 - bit_in_byte).min(width_usize - value_bit);
            let mask = low_mask(take);
            let chunk = ((value >> value_bit) as u8) & mask;
            output[byte_index] |= chunk << bit_in_byte;
            value_bit += take;
            bit_cursor = bit_cursor
                .checked_add(take)
                .ok_or(BitpackError::ByteLengthOverflow {
                    count: values.len(),
                    width,
                })?;
        }
    }
    Ok(())
}

fn low_mask(bits: usize) -> u8 {
    debug_assert!((1..=8).contains(&bits));
    ((1_u16 << bits) - 1) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_length_handles_boundaries_without_spurious_bit_overflow() {
        assert_eq!(expected_byte_len(0, 0), Ok(0));
        assert_eq!(expected_byte_len(100, 0), Ok(0));
        assert_eq!(expected_byte_len(1, 1), Ok(1));
        assert_eq!(expected_byte_len(8, 1), Ok(1));
        assert_eq!(expected_byte_len(9, 1), Ok(2));
        assert_eq!(expected_byte_len(3, 7), Ok(3));
        assert_eq!(expected_byte_len(3, 8), Ok(3));
        assert_eq!(expected_byte_len(3, 9), Ok(4));
        assert_eq!(expected_byte_len(3, 64), Ok(24));
        assert_eq!(expected_byte_len(usize::MAX, 1), Ok(usize::MAX / 8 + 1));
        assert_eq!(expected_byte_len(usize::MAX, 8), Ok(usize::MAX));
        assert_eq!(
            expected_byte_len(usize::MAX, 64),
            Err(BitpackError::ByteLengthOverflow {
                count: usize::MAX,
                width: 64,
            })
        );
        assert_eq!(
            expected_byte_len(1, 65),
            Err(BitpackError::InvalidWidth { width: 65 })
        );
    }

    #[test]
    fn scalar_layout_has_stable_golden_vectors() {
        assert_eq!(encode(&[1, 2, 3, 4], 3), Ok(vec![0xd1, 0x08]));
        assert_eq!(decode(&[0xd1, 0x08], 4, 3), Ok(vec![1, 2, 3, 4]));
        assert_eq!(encode(&[0, 1, 15, 8], 4), Ok(vec![0x10, 0x8f]));
        assert_eq!(decode(&[0x10, 0x8f], 4, 4), Ok(vec![0, 1, 15, 8]));
        assert_eq!(
            encode(&[0x0123_4567_89ab_cdef], 64),
            Ok(vec![0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01])
        );
        assert_eq!(
            decode(&[0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01], 1, 64),
            Ok(vec![0x0123_4567_89ab_cdef])
        );
    }

    #[test]
    fn zero_width_is_canonical_and_still_honors_count() {
        assert_eq!(encode(&[0, 0, 0], 0), Ok(Vec::new()));
        assert_eq!(decode(&[], 3, 0), Ok(vec![0, 0, 0]));
        assert_eq!(
            encode(&[0, 1, 0], 0),
            Err(BitpackError::ValueOutOfRange {
                index: 1,
                value: 1,
                width: 0,
            })
        );
        assert_eq!(
            decode(&[0], 3, 0),
            Err(BitpackError::TrailingBytes {
                expected: 0,
                actual: 1,
            })
        );
        assert_eq!(
            decode(&[], MAX_DECODED_VALUES + 1, 0),
            Err(BitpackError::ValueCountLimitExceeded {
                count: MAX_DECODED_VALUES + 1,
                limit: MAX_DECODED_VALUES,
            })
        );
    }

    #[test]
    fn decoder_rejects_lengths_and_noncanonical_padding_before_decode() {
        assert_eq!(
            decode(&[], 1, 1),
            Err(BitpackError::TruncatedInput {
                expected: 1,
                actual: 0,
            })
        );
        assert_eq!(
            decode(&[0, 0], 1, 1),
            Err(BitpackError::TrailingBytes {
                expected: 1,
                actual: 2,
            })
        );
        assert_eq!(
            decode(&[0x02], 1, 1),
            Err(BitpackError::NonZeroPadding {
                byte_index: 0,
                byte: 0x02,
                forbidden_mask: 0xfe,
            })
        );
        assert_eq!(
            decode(&[0xf8], 1, 3),
            Err(BitpackError::NonZeroPadding {
                byte_index: 0,
                byte: 0xf8,
                forbidden_mask: 0xf8,
            })
        );
    }

    #[test]
    fn encoder_rejects_out_of_range_values_without_mutating_output() {
        assert_eq!(
            encode(&[7, 8], 3),
            Err(BitpackError::ValueOutOfRange {
                index: 1,
                value: 8,
                width: 3,
            })
        );

        let mut output = [0xa5; 2];
        assert_eq!(
            encode_into(&[7, 8], 3, &mut output),
            Err(BitpackError::ValueOutOfRange {
                index: 1,
                value: 8,
                width: 3,
            })
        );
        assert_eq!(output, [0xa5; 2]);

        let mut short = [0xa5; 1];
        assert_eq!(
            encode_into(&[1, 2, 3, 4], 3, &mut short),
            Err(BitpackError::OutputTooSmall {
                required: 2,
                available: 1,
            })
        );
        assert_eq!(short, [0xa5]);
    }

    #[test]
    fn frame_of_reference_uses_checked_subtraction_and_addition() {
        let values = [10, 11, 13, 17];
        assert_eq!(encode_for(&values, 10, 3), Ok(vec![0xc8, 0x0e]));
        assert_eq!(decode_for(&[0xc8, 0x0e], 4, 10, 3), Ok(values.to_vec()));
        assert_eq!(
            encode_for(&[9], 10, 1),
            Err(BitpackError::ValueBelowBase {
                index: 0,
                value: 9,
                base: 10,
            })
        );
        assert_eq!(
            decode_for(&[1], 1, u64::MAX, 1),
            Err(BitpackError::DecodedValueOverflow {
                index: 0,
                base: u64::MAX,
                delta: 1,
            })
        );
        assert_eq!(decode_for(&[0], 1, u64::MAX, 1), Ok(vec![u64::MAX]));
    }

    #[test]
    fn deterministic_property_round_trip_covers_every_width_and_alignment() {
        let counts = [0_usize, 1, 2, 3, 7, 8, 9, 15, 16, 31, 64, 127];
        let mut state = 0x243f_6a88_85a3_08d3_u64;

        for width in 0..=MAX_BIT_WIDTH {
            let mask = if width == MAX_BIT_WIDTH {
                u64::MAX
            } else if width == 0 {
                0
            } else {
                (1_u64 << width) - 1
            };
            for count in counts {
                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    values.push(state & mask);
                }

                let first = encode(&values, width).expect("generated values fit width");
                let second = encode(&values, width).expect("encoding is deterministic");
                assert_eq!(first, second);
                assert_eq!(first.len(), expected_byte_len(count, width).unwrap());
                assert_eq!(decode(&first, count, width), Ok(values.clone()));

                let mut destination = vec![0xa5; first.len() + 3];
                assert_eq!(
                    encode_into(&values, width, &mut destination),
                    Ok(first.len())
                );
                assert_eq!(&destination[..first.len()], first);
                assert_eq!(&destination[first.len()..], &[0xa5; 3]);
            }
        }
    }

    #[test]
    fn deterministic_for_round_trip_covers_every_width() {
        let mut state = 0x1319_8a2e_0370_7344_u64;
        for width in 0..=MAX_BIT_WIDTH {
            let mask = if width == MAX_BIT_WIDTH {
                u64::MAX
            } else if width == 0 {
                0
            } else {
                (1_u64 << width) - 1
            };
            let base = if width <= 52 { 1_000_u64 } else { 0_u64 };
            let mut values = Vec::new();
            for _ in 0..33 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let delta = state & mask;
                values.push(
                    base.checked_add(delta)
                        .expect("chosen base cannot overflow"),
                );
            }

            let encoded = encode_for(&values, base, width).expect("valid FOR frame");
            assert_eq!(decode_for(&encoded, values.len(), base, width), Ok(values));
        }
    }

    #[test]
    fn expected_length_matches_a_bounded_naive_oracle() {
        for count in 0_usize..=257 {
            for width in 0..=MAX_BIT_WIDTH {
                let naive = (count * usize::from(width)).div_ceil(8);
                assert_eq!(expected_byte_len(count, width), Ok(naive));
            }
        }
    }
}
