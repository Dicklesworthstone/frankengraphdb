//! Canonical unsigned LEB128 encoding for `u64` values.
//!
//! The exact decoder accepts one value and therefore rejects trailing bytes.
//! Callers decoding a framed sequence may use [`decode_u64_prefix`] and must
//! account for the returned byte count in their enclosing length check.

#![forbid(unsafe_code)]

use core::fmt;

/// Maximum number of bytes in an unsigned LEB128 encoding of a `u64`.
pub const MAX_U64_VARINT_BYTES: usize = 10;

/// A canonical unsigned LEB128 encoding backed by an inline fixed-size array.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct EncodedU64 {
    bytes: [u8; MAX_U64_VARINT_BYTES],
    len: u8,
}

impl EncodedU64 {
    /// Returns the canonical encoded bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// Returns the number of canonical encoded bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Unsigned LEB128 encodings are never empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }
}

impl AsRef<[u8]> for EncodedU64 {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Debug for EncodedU64 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("EncodedU64")
            .field(&self.as_bytes())
            .finish()
    }
}

/// Failure to place a canonical unsigned LEB128 encoding in caller storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VarintEncodeError {
    /// The caller-provided byte slice cannot hold the complete encoding.
    BufferTooSmall {
        /// Exact number of bytes required.
        required: usize,
        /// Number of bytes available.
        available: usize,
    },
    /// Growing an output vector to the requested length failed.
    AllocationFailed {
        /// Number of additional bytes requested.
        additional: usize,
    },
}

impl fmt::Display for VarintEncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::BufferTooSmall {
                required,
                available,
            } => write!(
                formatter,
                "varint output buffer needs {required} bytes but has {available}"
            ),
            Self::AllocationFailed { additional } => write!(
                formatter,
                "could not reserve {additional} bytes for varint output"
            ),
        }
    }
}

impl std::error::Error for VarintEncodeError {}

/// Strict unsigned LEB128 decoding failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VarintDecodeError {
    /// No first byte was present.
    Empty,
    /// Input ended while the continuation bit still required another byte.
    Truncated {
        /// Number of bytes inspected before input ended.
        consumed: usize,
    },
    /// The encoded integer cannot fit in a `u64`.
    Overflow {
        /// Zero-based byte position at which overflow became certain.
        byte_index: usize,
    },
    /// The value used more bytes than its unique canonical representation.
    NonMinimal {
        /// Number of bytes supplied for the value.
        encoded_len: usize,
        /// Number of bytes in the canonical representation.
        canonical_len: usize,
    },
    /// A complete canonical value was followed by unconsumed input.
    TrailingBytes {
        /// Number of bytes consumed by the value.
        consumed: usize,
        /// Number of bytes remaining after the value.
        trailing: usize,
    },
}

impl fmt::Display for VarintDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Empty => formatter.write_str("varint input is empty"),
            Self::Truncated { consumed } => {
                write!(formatter, "varint is truncated after {consumed} bytes")
            }
            Self::Overflow { byte_index } => {
                write!(formatter, "varint overflows u64 at byte index {byte_index}")
            }
            Self::NonMinimal {
                encoded_len,
                canonical_len,
            } => write!(
                formatter,
                "varint uses {encoded_len} bytes but its canonical length is {canonical_len}"
            ),
            Self::TrailingBytes { consumed, trailing } => write!(
                formatter,
                "varint consumed {consumed} bytes with {trailing} trailing bytes"
            ),
        }
    }
}

impl std::error::Error for VarintDecodeError {}

/// Returns the exact canonical unsigned LEB128 length of `value`.
#[must_use]
pub const fn encoded_len_u64(value: u64) -> usize {
    let significant_bits = (u64::BITS - value.leading_zeros()) as usize;
    let nonzero_len = significant_bits.div_ceil(7);
    if nonzero_len == 0 { 1 } else { nonzero_len }
}

/// Encodes `value` into an inline, allocation-free canonical representation.
#[must_use]
pub fn encode_u64(mut value: u64) -> EncodedU64 {
    let mut bytes = [0_u8; MAX_U64_VARINT_BYTES];
    let mut len = 0_usize;

    loop {
        let payload = (value & 0x7f) as u8;
        value >>= 7;
        bytes[len] = if value == 0 { payload } else { payload | 0x80 };
        len += 1;
        if value == 0 {
            break;
        }
    }

    debug_assert_eq!(len, encoded_len_u64(decode_inline(&bytes[..len])));
    EncodedU64 {
        bytes,
        len: len as u8,
    }
}

/// Writes the canonical encoding of `value` into the start of `output`.
///
/// Bytes after the returned count are not modified.
pub fn write_u64(value: u64, output: &mut [u8]) -> Result<usize, VarintEncodeError> {
    let encoded = encode_u64(value);
    let required = encoded.len();
    if output.len() < required {
        return Err(VarintEncodeError::BufferTooSmall {
            required,
            available: output.len(),
        });
    }
    output[..required].copy_from_slice(encoded.as_bytes());
    Ok(required)
}

/// Appends the canonical encoding of `value` and returns its byte count.
///
/// Reservation is attempted before the vector is modified, so allocation
/// failure leaves the existing bytes unchanged.
pub fn append_u64(value: u64, output: &mut Vec<u8>) -> Result<usize, VarintEncodeError> {
    let encoded = encode_u64(value);
    let additional = encoded.len();
    output
        .try_reserve(additional)
        .map_err(|_| VarintEncodeError::AllocationFailed { additional })?;
    output.extend_from_slice(encoded.as_bytes());
    Ok(additional)
}

/// Decodes one canonical unsigned LEB128 value from the beginning of `input`.
///
/// The returned byte count lets an enclosing, length-delimited decoder advance
/// safely. This function does not consider remaining bytes an error; use
/// [`decode_u64`] when `input` must contain exactly one value.
pub fn decode_u64_prefix(input: &[u8]) -> Result<(u64, usize), VarintDecodeError> {
    if input.is_empty() {
        return Err(VarintDecodeError::Empty);
    }

    let mut value = 0_u64;
    for (index, &byte) in input.iter().take(MAX_U64_VARINT_BYTES).enumerate() {
        let payload = byte & 0x7f;

        // Byte ten may carry only bit 63. A continuation on byte ten can
        // never be completed within a u64, even if its payload is zero.
        if index == MAX_U64_VARINT_BYTES - 1 && (payload > 1 || byte & 0x80 != 0) {
            return Err(VarintDecodeError::Overflow { byte_index: index });
        }

        value |= u64::from(payload) << (index * 7);
        if byte & 0x80 == 0 {
            let consumed = index + 1;
            let canonical_len = encoded_len_u64(value);
            if consumed != canonical_len {
                return Err(VarintDecodeError::NonMinimal {
                    encoded_len: consumed,
                    canonical_len,
                });
            }
            return Ok((value, consumed));
        }
    }

    // An input of fewer than ten continuation bytes needs another byte. Ten
    // continuation bytes are classified as overflow in the loop above.
    Err(VarintDecodeError::Truncated {
        consumed: input.len().min(MAX_U64_VARINT_BYTES),
    })
}

/// Decodes an input that must contain exactly one canonical unsigned LEB128.
pub fn decode_u64(input: &[u8]) -> Result<u64, VarintDecodeError> {
    let (value, consumed) = decode_u64_prefix(input)?;
    if consumed != input.len() {
        return Err(VarintDecodeError::TrailingBytes {
            consumed,
            trailing: input.len() - consumed,
        });
    }
    Ok(value)
}

fn decode_inline(input: &[u8]) -> u64 {
    let mut value = 0_u64;
    let mut index = 0_usize;
    while index < input.len() {
        value |= u64::from(input[index] & 0x7f) << (index * 7);
        index += 1;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_golden_vectors() {
        let cases: &[(u64, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7f]),
            (128, &[0x80, 0x01]),
            (255, &[0xff, 0x01]),
            (300, &[0xac, 0x02]),
            (16_384, &[0x80, 0x80, 0x01]),
            (u64::from(u32::MAX), &[0xff, 0xff, 0xff, 0xff, 0x0f]),
            (
                u64::MAX,
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
            ),
        ];

        for &(value, expected) in cases {
            let encoded = encode_u64(value);
            assert_eq!(encoded.as_bytes(), expected, "value {value}");
            assert_eq!(encoded.len(), encoded_len_u64(value));
            assert_eq!(decode_u64(expected), Ok(value));
        }
    }

    #[test]
    fn encoded_len_is_exact_at_every_seven_bit_boundary() {
        assert_eq!(encoded_len_u64(0), 1);
        for bits in (7_u32..=63).step_by(7) {
            let below = (1_u64 << bits) - 1;
            let at = 1_u64 << bits;
            assert_eq!(encoded_len_u64(below), (bits as usize).div_ceil(7));
            assert_eq!(encoded_len_u64(at), (bits as usize).div_ceil(7) + 1);
        }
        assert_eq!(encoded_len_u64(u64::MAX), MAX_U64_VARINT_BYTES);
    }

    #[test]
    fn exact_decoder_distinguishes_all_malformed_classes() {
        assert_eq!(decode_u64(&[]), Err(VarintDecodeError::Empty));
        assert_eq!(
            decode_u64(&[0x80]),
            Err(VarintDecodeError::Truncated { consumed: 1 })
        );
        assert_eq!(
            decode_u64(&[0x80; 9]),
            Err(VarintDecodeError::Truncated { consumed: 9 })
        );
        assert_eq!(
            decode_u64(&[0x80, 0x00]),
            Err(VarintDecodeError::NonMinimal {
                encoded_len: 2,
                canonical_len: 1,
            })
        );
        assert_eq!(
            decode_u64(&[0xff, 0x00]),
            Err(VarintDecodeError::NonMinimal {
                encoded_len: 2,
                canonical_len: 1,
            })
        );
        assert_eq!(
            decode_u64(&[0x01, 0x00]),
            Err(VarintDecodeError::TrailingBytes {
                consumed: 1,
                trailing: 1,
            })
        );

        let mut terminal_overflow = [0xff; MAX_U64_VARINT_BYTES];
        terminal_overflow[MAX_U64_VARINT_BYTES - 1] = 0x02;
        assert_eq!(
            decode_u64(&terminal_overflow),
            Err(VarintDecodeError::Overflow { byte_index: 9 })
        );
        assert_eq!(
            decode_u64(&[0x80; MAX_U64_VARINT_BYTES]),
            Err(VarintDecodeError::Overflow { byte_index: 9 })
        );
        assert_eq!(
            decode_u64(&[0x80; MAX_U64_VARINT_BYTES + 1]),
            Err(VarintDecodeError::Overflow { byte_index: 9 })
        );
    }

    #[test]
    fn prefix_decoder_reports_the_exact_consumed_length() {
        assert_eq!(decode_u64_prefix(&[0xac, 0x02, 0xff]), Ok((300, 2)));
        assert_eq!(
            decode_u64(&[0xac, 0x02, 0xff]),
            Err(VarintDecodeError::TrailingBytes {
                consumed: 2,
                trailing: 1,
            })
        );
    }

    #[test]
    fn write_and_append_preserve_unaddressed_bytes() {
        let mut bytes = [0xa5; 4];
        assert_eq!(write_u64(300, &mut bytes), Ok(2));
        assert_eq!(bytes, [0xac, 0x02, 0xa5, 0xa5]);

        let mut short = [0xa5; 1];
        assert_eq!(
            write_u64(300, &mut short),
            Err(VarintEncodeError::BufferTooSmall {
                required: 2,
                available: 1,
            })
        );
        assert_eq!(short, [0xa5]);

        let mut appended = vec![0xde, 0xad];
        assert_eq!(append_u64(300, &mut appended), Ok(2));
        assert_eq!(appended, [0xde, 0xad, 0xac, 0x02]);
    }

    #[test]
    fn deterministic_property_round_trip_covers_full_u64_domain() {
        let mut state = 0xd6e8_feb8_6659_fd93_u64;
        for iteration in 0..20_000_u64 {
            // A fixed SplitMix64 stream provides stable, broad bit coverage
            // without adding a randomness dependency to the crate.
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut value = state;
            value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            value ^= value >> 31;

            let sample = match iteration % 8 {
                0 => iteration,
                1 => value & 0x7f,
                2 => value & 0x3fff,
                3 => value & 0x1f_ffff,
                4 => value & u64::from(u32::MAX),
                5 => value | (1_u64 << 63),
                6 => value,
                _ => u64::MAX - iteration,
            };
            let encoded = encode_u64(sample);
            assert_eq!(encoded.len(), encoded_len_u64(sample));
            assert_eq!(decode_u64(encoded.as_bytes()), Ok(sample));
            assert_eq!(encode_u64(sample), encoded);
        }
    }
}
