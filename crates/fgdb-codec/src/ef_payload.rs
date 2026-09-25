//! Canonical scalar Elias-Fano payload bytes, independent of durable framing.
//!
//! The owner frames the entry count and maximum; those determine the unique
//! low-bit width and exact payload length. Bytes are little-endian packed low
//! words followed by unary-high words. Padding bits MUST be zero. The derived
//! select directory is rebuilt by the existing scalar kernel, not persisted.
//! No object kind, codec registry number, checksum or authorization is assigned
//! here. This is the payload seam, not an alternative Elias-Fano query engine.

use crate::elias_fano::{EliasFano, EliasFanoError, EntryLimit};
use core::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EfPayloadLimits {
    pub max_entries: usize,
    pub max_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EfPayloadError {
    EntryLimit { count: usize, limit: usize },
    ByteLimit { bytes: usize, limit: usize },
    SizeOverflow,
    Length { expected: usize, found: usize },
    NonzeroPadding,
    InvalidMaximum,
    InvalidHighBits,
    AllocationFailed,
    Scalar(EliasFanoError),
}

impl fmt::Display for EfPayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Elias-Fano payload: {self:?}")
    }
}

impl std::error::Error for EfPayloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Scalar(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct Layout {
    width: u32,
    low_bits: usize,
    low_words: usize,
    high_bits: usize,
    high_words: usize,
    bytes: usize,
}

fn layout(count: usize, maximum: u64, limits: EfPayloadLimits) -> Result<Layout, EfPayloadError> {
    if count > limits.max_entries {
        return Err(EfPayloadError::EntryLimit {
            count,
            limit: limits.max_entries,
        });
    }
    u32::try_from(count).map_err(|_| EfPayloadError::SizeOverflow)?;
    if count == 0 {
        if maximum != 0 {
            return Err(EfPayloadError::InvalidMaximum);
        }
        return Ok(Layout {
            width: 0,
            low_bits: 0,
            low_words: 0,
            high_bits: 0,
            high_words: 0,
            bytes: 0,
        });
    }
    let ratio = maximum / count as u64;
    let width = if ratio == 0 {
        0
    } else {
        u64::BITS - 1 - ratio.leading_zeros()
    };
    let low_bits = count
        .checked_mul(width as usize)
        .ok_or(EfPayloadError::SizeOverflow)?;
    let high_bits = (maximum >> width)
        .checked_add(count as u64)
        .and_then(|bits| usize::try_from(bits).ok())
        .ok_or(EfPayloadError::SizeOverflow)?;
    let low_words = low_bits.div_ceil(64);
    let high_words = high_bits.div_ceil(64);
    let bytes = low_words
        .checked_add(high_words)
        .and_then(|words| words.checked_mul(8))
        .ok_or(EfPayloadError::SizeOverflow)?;
    if bytes > limits.max_bytes {
        return Err(EfPayloadError::ByteLimit {
            bytes,
            limit: limits.max_bytes,
        });
    }
    Ok(Layout {
        width,
        low_bits,
        low_words,
        high_bits,
        high_words,
        bytes,
    })
}

pub fn encoded_len(
    count: usize,
    maximum: u64,
    limits: EfPayloadLimits,
) -> Result<usize, EfPayloadError> {
    Ok(layout(count, maximum, limits)?.bytes)
}

fn word(bytes: &[u8], index: usize) -> u64 {
    u64::from_le_bytes(
        bytes[index * 8..index * 8 + 8]
            .try_into()
            .expect("bounded EF word"),
    )
}

fn or_word(bytes: &mut [u8], index: usize, value: u64) {
    let combined = word(bytes, index) | value;
    bytes[index * 8..index * 8 + 8].copy_from_slice(&combined.to_le_bytes());
}

fn mask(width: u32) -> u64 {
    if width == 0 { 0 } else { (1u64 << width) - 1 }
}

/// Encode the existing immutable scalar representation, without changing its
/// identity-independent value sequence. The caller owns count/max framing.
pub fn encode(encoded: &EliasFano, limits: EfPayloadLimits) -> Result<Vec<u8>, EfPayloadError> {
    let plan = layout(encoded.len(), encoded.max_value().unwrap_or(0), limits)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(plan.bytes)
        .map_err(|_| EfPayloadError::AllocationFailed)?;
    bytes.resize(plan.bytes, 0);
    for index in 0..encoded.len() {
        let value = encoded
            .select(index)
            .expect("scalar EF retains all entries");
        if plan.width != 0 {
            let offset = index * plan.width as usize;
            let shift = offset % 64;
            let low = value & mask(plan.width);
            or_word(&mut bytes, offset / 64, low << shift);
            if shift + plan.width as usize > 64 {
                or_word(&mut bytes, offset / 64 + 1, low >> (64 - shift));
            }
        }
        let position = usize::try_from(value >> plan.width)
            .map_err(|_| EfPayloadError::SizeOverflow)?
            .checked_add(index)
            .ok_or(EfPayloadError::SizeOverflow)?;
        or_word(
            &mut bytes,
            plan.low_words + position / 64,
            1u64 << (position % 64),
        );
    }
    Ok(bytes)
}

fn validate_padding(
    bytes: &[u8],
    base: usize,
    bits: usize,
    words: usize,
) -> Result<(), EfPayloadError> {
    if words != 0 && !bits.is_multiple_of(64) && word(bytes, base + words - 1) >> (bits % 64) != 0 {
        return Err(EfPayloadError::NonzeroPadding);
    }
    Ok(())
}

/// Decode canonical bytes under explicit bounds and rebuild the scalar select
/// directory. Count, exact length, and padding are checked before materializing
/// values. Corrupt high bits cannot request allocation beyond the framed count.
pub fn decode(
    bytes: &[u8],
    count: usize,
    maximum: u64,
    limits: EfPayloadLimits,
) -> Result<EliasFano, EfPayloadError> {
    let plan = layout(count, maximum, limits)?;
    if bytes.len() != plan.bytes {
        return Err(EfPayloadError::Length {
            expected: plan.bytes,
            found: bytes.len(),
        });
    }
    validate_padding(bytes, 0, plan.low_bits, plan.low_words)?;
    validate_padding(bytes, plan.low_words, plan.high_bits, plan.high_words)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| EfPayloadError::AllocationFailed)?;
    for high_word in 0..plan.high_words {
        let mut remaining = word(bytes, plan.low_words + high_word);
        while remaining != 0 {
            if values.len() == count {
                return Err(EfPayloadError::InvalidHighBits);
            }
            let index = values.len();
            let position = high_word * 64 + remaining.trailing_zeros() as usize;
            remaining &= remaining - 1;
            let high = position
                .checked_sub(index)
                .ok_or(EfPayloadError::InvalidHighBits)?;
            let low = if plan.width == 0 {
                0
            } else {
                let offset = index * plan.width as usize;
                let shift = offset % 64;
                let mut value = word(bytes, offset / 64) >> shift;
                if shift + plan.width as usize > 64 {
                    value |= word(bytes, offset / 64 + 1) << (64 - shift);
                }
                value & mask(plan.width)
            };
            let wide = ((high as u128) << plan.width) | u128::from(low);
            if wide > u128::from(maximum) {
                return Err(EfPayloadError::InvalidMaximum);
            }
            values.push(wide as u64);
        }
    }
    if values.len() != count {
        return Err(EfPayloadError::InvalidHighBits);
    }
    if values.last().copied().unwrap_or(0) != maximum {
        return Err(EfPayloadError::InvalidMaximum);
    }
    EliasFano::try_new(&values, EntryLimit::new(count)).map_err(EfPayloadError::Scalar)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> EfPayloadLimits {
        EfPayloadLimits {
            max_entries: 10000,
            max_bytes: 100000,
        }
    }

    #[test]
    fn golden_unary_high_word_and_zero_padding() {
        let scalar = EliasFano::try_new(&[0, 1, 1, 3], EntryLimit::new(4)).unwrap();
        assert_eq!(encode(&scalar, limits()).unwrap(), 0x4du64.to_le_bytes());
        assert_eq!(
            decode(&0x4du64.to_le_bytes(), 4, 3, limits()).unwrap(),
            scalar
        );
        assert!(matches!(
            decode(&0xcdu64.to_le_bytes(), 4, 3, limits()),
            Err(EfPayloadError::NonzeroPadding)
        ));
    }

    #[test]
    fn extreme_and_repeated_sequences_have_one_round_trip() {
        for values in [
            vec![],
            vec![0],
            vec![u64::MAX],
            vec![0, u64::MAX],
            vec![42; 1025],
            (0..1000).collect(),
        ] {
            let scalar = EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();
            let payload = encode(&scalar, limits()).unwrap();
            let decoded = decode(
                &payload,
                values.len(),
                values.last().copied().unwrap_or(0),
                limits(),
            )
            .unwrap();
            assert_eq!(decoded, scalar);
            assert_eq!(encode(&decoded, limits()).unwrap(), payload);
        }
    }

    #[test]
    fn exact_count_length_and_maximum_are_not_hints() {
        assert!(decode(&[], 0, 1, limits()).is_err());
        assert!(decode(&[0], 0, 0, limits()).is_err());
        assert!(decode(&[0; 7], 1, 0, limits()).is_err());
        assert!(decode(&[0; 8], 1, 0, limits()).is_err());
        assert!(decode(&3u64.to_le_bytes(), 1, 1, limits()).is_err());
        assert!(decode(&1u64.to_le_bytes(), 1, 1, limits()).is_err());
    }

    #[test]
    fn padding_in_the_low_vector_is_rejected() {
        let scalar = EliasFano::try_new(&[u64::MAX], EntryLimit::new(1)).unwrap();
        let mut bytes = encode(&scalar, limits()).unwrap();
        bytes[7] |= 0x80; // one value uses 63 low bits, not 64
        assert!(matches!(
            decode(&bytes, 1, u64::MAX, limits()),
            Err(EfPayloadError::NonzeroPadding)
        ));
    }

    #[test]
    fn byte_and_entry_limits_precede_materialization() {
        let scalar = EliasFano::try_new(&[7], EntryLimit::new(1)).unwrap();
        let payload = encode(&scalar, limits()).unwrap();
        assert!(matches!(
            encode(
                &scalar,
                EfPayloadLimits {
                    max_entries: 0,
                    ..limits()
                }
            ),
            Err(EfPayloadError::EntryLimit { .. })
        ));
        assert!(matches!(
            decode(
                &payload,
                1,
                7,
                EfPayloadLimits {
                    max_bytes: payload.len() - 1,
                    ..limits()
                }
            ),
            Err(EfPayloadError::ByteLimit { .. })
        ));
        assert!(
            decode(
                &payload,
                1,
                7,
                EfPayloadLimits {
                    max_bytes: payload.len(),
                    ..limits()
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn deterministic_generated_sequences_round_trip_across_word_boundaries() {
        let mut seed = 0x243f_6a88_85a3_08d3u64;
        for count in 0..256 {
            let mut values = Vec::new();
            for _ in 0..count {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                values.push(seed >> (count % 64));
            }
            values.sort_unstable();
            let scalar = EliasFano::try_new(&values, EntryLimit::new(count)).unwrap();
            let bytes = encode(&scalar, limits()).unwrap();
            assert_eq!(
                decode(&bytes, count, values.last().copied().unwrap_or(0), limits()).unwrap(),
                scalar
            );
        }
    }
}
