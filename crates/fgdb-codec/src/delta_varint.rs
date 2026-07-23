//! Canonical delta-varint coding for nondecreasing `u64` sequences.
//!
//! The first value is encoded as an absolute canonical unsigned LEB128 value;
//! every later value is encoded as its checked, nonnegative gap from the
//! previous value. Duplicates therefore have a one-byte zero gap. Sequence
//! length is deliberately supplied out of band: this module is a scalar
//! kernel, not a durable frame, and defines no registered codec or format ID.
//! Durable callers must provide their own versioned framing and tighter
//! per-kind limits.

#![forbid(unsafe_code)]

use core::fmt;

use crate::varint::{VarintDecodeError, decode_u64_prefix, encode_u64, encoded_len_u64};

/// Maximum logical entries one decode call may materialize.
///
/// The limit is explicit at each call site so a framed reader can apply its
/// registered per-kind bound before this kernel reserves any storage.
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

/// Value-specific reason delta-varint encoding could not proceed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeltaVarintEncodeCause {
    /// The current value is smaller than the preceding value.
    NotMonotone {
        /// Value immediately before the rejected value.
        previous: u64,
        /// Rejected value.
        current: u64,
    },
    /// Summing the canonical component lengths overflowed `usize`.
    OutputLengthOverflow {
        /// Canonical bytes needed by the component at the reported index.
        component_len: usize,
    },
}

impl fmt::Display for DeltaVarintEncodeCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::NotMonotone { previous, current } => {
                write!(formatter, "sequence decreases from {previous} to {current}")
            }
            Self::OutputLengthOverflow { component_len } => write!(
                formatter,
                "adding a {component_len}-byte component overflows the output length"
            ),
        }
    }
}

impl std::error::Error for DeltaVarintEncodeCause {}

/// Checked delta-varint encoding failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeltaVarintEncodeError {
    /// A particular logical value cannot be encoded.
    Value {
        /// Zero-based logical value index.
        value_index: usize,
        /// Byte offset at which this value's component would begin.
        byte_offset: usize,
        /// Typed reason encoding failed.
        cause: DeltaVarintEncodeCause,
    },
    /// Reserving the exact canonical output length failed before publication.
    AllocationFailed {
        /// Number of output bytes requested.
        requested: usize,
    },
}

impl fmt::Display for DeltaVarintEncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Value {
                value_index,
                byte_offset,
                cause,
            } => write!(
                formatter,
                "delta-varint encode failed at value {value_index}, byte {byte_offset}: {cause}"
            ),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve {requested} bytes for delta-varint output"
            ),
        }
    }
}

impl std::error::Error for DeltaVarintEncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Value { cause, .. } => Some(cause),
            Self::AllocationFailed { .. } => None,
        }
    }
}

/// Input-specific reason delta-varint decoding could not proceed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeltaVarintDecodeCause {
    /// One component is not a canonical unsigned LEB128 value.
    Varint(VarintDecodeError),
    /// Adding a decoded gap to the previous value overflowed `u64`.
    ValueOverflow {
        /// Previously reconstructed absolute value.
        previous: u64,
        /// Decoded nonnegative gap.
        gap: u64,
    },
    /// Bytes remain after the requested number of values was reconstructed.
    TrailingBytes {
        /// Number of bytes after the exact requested sequence.
        trailing: usize,
    },
}

impl fmt::Display for DeltaVarintDecodeCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Varint(source) => write!(formatter, "{source}"),
            Self::ValueOverflow { previous, gap } => write!(
                formatter,
                "previous value {previous} plus decoded gap {gap} overflows u64"
            ),
            Self::TrailingBytes { trailing } => {
                write!(formatter, "input has {trailing} trailing bytes")
            }
        }
    }
}

impl std::error::Error for DeltaVarintDecodeCause {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Varint(source) => Some(source),
            Self::ValueOverflow { .. } | Self::TrailingBytes { .. } => None,
        }
    }
}

/// Allocation-bounded, exact delta-varint decoding failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeltaVarintDecodeError {
    /// The caller-requested count exceeds its explicit materialization bound.
    EntryLimitExceeded {
        /// Requested logical value count.
        count: usize,
        /// Caller-provided ceiling.
        limit: usize,
    },
    /// Reserving result storage failed before any value was materialized.
    AllocationFailed {
        /// Number of `u64` entries requested.
        requested: usize,
    },
    /// Input at a particular logical value and byte offset was invalid.
    Value {
        /// Zero-based logical value index. For trailing bytes this equals the
        /// exact requested count.
        value_index: usize,
        /// Absolute byte offset in the supplied input.
        byte_offset: usize,
        /// Typed reason decoding failed.
        cause: DeltaVarintDecodeCause,
    },
}

impl fmt::Display for DeltaVarintDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EntryLimitExceeded { count, limit } => write!(
                formatter,
                "delta-varint decode count {count} exceeds entry limit {limit}"
            ),
            Self::AllocationFailed { requested } => write!(
                formatter,
                "could not reserve {requested} values for delta-varint decode"
            ),
            Self::Value {
                value_index,
                byte_offset,
                cause,
            } => write!(
                formatter,
                "delta-varint decode failed at value {value_index}, byte {byte_offset}: {cause}"
            ),
        }
    }
}

impl std::error::Error for DeltaVarintDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Value { cause, .. } => Some(cause),
            Self::EntryLimitExceeded { .. } | Self::AllocationFailed { .. } => None,
        }
    }
}

/// Encodes a nondecreasing sequence in its unique canonical delta-varint form.
///
/// The complete sequence is validated and its exact length is computed before
/// output allocation. An error therefore never publishes a partial encoding.
pub fn encode(values: &[u64]) -> Result<Vec<u8>, DeltaVarintEncodeError> {
    let mut output_len = 0_usize;
    let mut previous: Option<u64> = None;

    for (value_index, &current) in values.iter().enumerate() {
        let component = match previous {
            None => current,
            Some(previous_value) => {
                current
                    .checked_sub(previous_value)
                    .ok_or(DeltaVarintEncodeError::Value {
                        value_index,
                        byte_offset: output_len,
                        cause: DeltaVarintEncodeCause::NotMonotone {
                            previous: previous_value,
                            current,
                        },
                    })?
            }
        };
        let component_len = encoded_len_u64(component);
        output_len =
            output_len
                .checked_add(component_len)
                .ok_or(DeltaVarintEncodeError::Value {
                    value_index,
                    byte_offset: output_len,
                    cause: DeltaVarintEncodeCause::OutputLengthOverflow { component_len },
                })?;
        previous = Some(current);
    }

    let mut output = Vec::new();
    output
        .try_reserve_exact(output_len)
        .map_err(|_| DeltaVarintEncodeError::AllocationFailed {
            requested: output_len,
        })?;

    previous = None;
    for &current in values {
        let component = previous.map_or(current, |previous_value| current - previous_value);
        output.extend_from_slice(encode_u64(component).as_bytes());
        previous = Some(current);
    }
    debug_assert_eq!(output.len(), output_len);
    Ok(output)
}

/// Decodes exactly `count` values and consumes exactly all supplied bytes.
///
/// The explicit [`EntryLimit`] is checked before even an empty result is
/// constructed. A first, allocation-free pass validates every canonical
/// component, checked prefix sum, and exact input consumption. Only then is
/// nonzero result storage reserved in one fallible operation; a second pass
/// materializes the already-validated values. Each component failure retains
/// both its typed unsigned LEB128 cause and global sequence position.
pub fn decode(
    input: &[u8],
    count: usize,
    limit: EntryLimit,
) -> Result<Vec<u64>, DeltaVarintDecodeError> {
    if count > limit.max_entries() {
        return Err(DeltaVarintDecodeError::EntryLimitExceeded {
            count,
            limit: limit.max_entries(),
        });
    }

    walk_values(input, count, |_| {})?;

    let mut values = Vec::new();
    if count != 0 {
        values
            .try_reserve_exact(count)
            .map_err(|_| DeltaVarintDecodeError::AllocationFailed { requested: count })?;
    }

    walk_values(input, count, |value| values.push(value))?;
    debug_assert_eq!(values.len(), count);
    Ok(values)
}

fn walk_values(
    input: &[u8],
    count: usize,
    mut observe: impl FnMut(u64),
) -> Result<(), DeltaVarintDecodeError> {
    let mut byte_offset = 0_usize;
    let mut previous: Option<u64> = None;
    for value_index in 0..count {
        let (component, consumed) = decode_u64_prefix(&input[byte_offset..]).map_err(|source| {
            DeltaVarintDecodeError::Value {
                value_index,
                byte_offset,
                cause: DeltaVarintDecodeCause::Varint(source),
            }
        })?;

        let current = match previous {
            None => component,
            Some(previous_value) => {
                previous_value
                    .checked_add(component)
                    .ok_or(DeltaVarintDecodeError::Value {
                        value_index,
                        byte_offset,
                        cause: DeltaVarintDecodeCause::ValueOverflow {
                            previous: previous_value,
                            gap: component,
                        },
                    })?
            }
        };
        observe(current);
        previous = Some(current);
        byte_offset += consumed;
    }

    if byte_offset != input.len() {
        return Err(DeltaVarintDecodeError::Value {
            value_index: count,
            byte_offset,
            cause: DeltaVarintDecodeCause::TrailingBytes {
                trailing: input.len() - byte_offset,
            },
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_round_trip(values: &[u64]) {
        let encoded = encode(values).expect("test sequence must encode");
        assert_eq!(
            decode(&encoded, values.len(), EntryLimit::new(values.len())),
            Ok(values.to_vec())
        );
        assert_eq!(encode(values), Ok(encoded));
    }

    fn enumerate_nondecreasing(prefix: &mut Vec<u64>, remaining: usize, minimum: u64) {
        if remaining == 0 {
            assert_round_trip(prefix);
            return;
        }

        for value in minimum..=6 {
            prefix.push(value);
            enumerate_nondecreasing(prefix, remaining - 1, value);
            prefix.pop();
        }
    }

    fn reference_accepts(input: &[u8], count: usize) -> Option<Vec<u64>> {
        let mut values = Vec::new();
        let mut cursor = 0_usize;
        let mut previous: Option<u64> = None;

        for _ in 0..count {
            let component_start = cursor;
            let mut component = 0_u64;
            let mut shift = 0_u32;

            loop {
                let byte = *input.get(cursor)?;
                let component_index = cursor - component_start;
                let payload = byte & 0x7f;
                if component_index == 9 && (payload > 1 || byte & 0x80 != 0) {
                    return None;
                }
                if component_index >= 10 {
                    return None;
                }

                component |= u64::from(payload) << shift;
                cursor += 1;
                if byte & 0x80 == 0 {
                    if component_index != 0 && payload == 0 {
                        return None;
                    }
                    break;
                }
                shift += 7;
            }

            let current = previous.map_or(Some(component), |value| value.checked_add(component))?;
            values.push(current);
            previous = Some(current);
        }

        (cursor == input.len()).then_some(values)
    }

    #[test]
    fn canonical_byte_vectors_are_stable() {
        let cases: &[(&[u64], &[u8])] = &[
            (&[], &[]),
            (&[0], &[0x00]),
            (
                &[127, 127, 255, 16_384],
                &[0x7f, 0x00, 0x80, 0x01, 0x81, 0x7e],
            ),
            (&[128, 300, 300], &[0x80, 0x01, 0xac, 0x01, 0x00]),
        ];

        for &(values, expected) in cases {
            assert_eq!(encode(values).as_deref(), Ok(expected));
            assert_eq!(
                decode(expected, values.len(), EntryLimit::new(values.len())).as_deref(),
                Ok(values)
            );
        }
    }

    #[test]
    fn exhaustive_small_nondecreasing_sequences_round_trip() {
        let mut values = Vec::new();
        for len in 0..=6 {
            enumerate_nondecreasing(&mut values, len, 0);
        }
    }

    #[test]
    fn duplicates_and_full_u64_domain_round_trip() {
        let cases: &[&[u64]] = &[
            &[u64::MAX],
            &[u64::MAX, u64::MAX, u64::MAX],
            &[0, u64::MAX, u64::MAX],
            &[0, 0, 1, 1, 127, 128, u64::MAX],
        ];
        for values in cases {
            assert_round_trip(values);
        }

        let encoded_max = encode(&[u64::MAX]).expect("u64::MAX must encode");
        assert_eq!(
            encoded_max,
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
        );
        assert_eq!(encode(&[u64::MAX, u64::MAX]).map(|bytes| bytes[10]), Ok(0));
    }

    #[test]
    fn decreasing_input_reports_exact_value_and_byte_positions() {
        assert_eq!(
            encode(&[300, 299]),
            Err(DeltaVarintEncodeError::Value {
                value_index: 1,
                byte_offset: 2,
                cause: DeltaVarintEncodeCause::NotMonotone {
                    previous: 300,
                    current: 299,
                },
            })
        );
    }

    #[test]
    fn malformed_varints_retain_cause_and_position() {
        let nonminimal = [0x80, 0x00];
        let truncated = [0x80];
        let mut overflow = [0x80; 10];
        overflow[9] = 0x02;

        for value_index in 0..3 {
            let mut bytes = vec![0x01; value_index];
            let byte_offset = bytes.len();
            bytes.extend_from_slice(&nonminimal);
            bytes.extend(core::iter::repeat_n(0x01, 2 - value_index));
            assert_eq!(
                decode(&bytes, 3, EntryLimit::new(3)),
                Err(DeltaVarintDecodeError::Value {
                    value_index,
                    byte_offset,
                    cause: DeltaVarintDecodeCause::Varint(VarintDecodeError::NonMinimal {
                        encoded_len: 2,
                        canonical_len: 1,
                    }),
                })
            );
        }

        for value_index in 0..3 {
            let mut bytes = vec![0x01; value_index];
            let byte_offset = bytes.len();
            bytes.extend_from_slice(&overflow);
            bytes.extend(core::iter::repeat_n(0x01, 2 - value_index));
            assert_eq!(
                decode(&bytes, 3, EntryLimit::new(3)),
                Err(DeltaVarintDecodeError::Value {
                    value_index,
                    byte_offset,
                    cause: DeltaVarintDecodeCause::Varint(VarintDecodeError::Overflow {
                        byte_index: 9,
                    }),
                })
            );
        }

        for value_index in 0..3 {
            let mut bytes = vec![0x01; value_index];
            let byte_offset = bytes.len();
            bytes.extend_from_slice(&truncated);
            assert_eq!(
                decode(&bytes, value_index + 1, EntryLimit::new(value_index + 1)),
                Err(DeltaVarintDecodeError::Value {
                    value_index,
                    byte_offset,
                    cause: DeltaVarintDecodeCause::Varint(VarintDecodeError::Truncated {
                        consumed: 1,
                    }),
                })
            );

            bytes.pop();
            assert_eq!(
                decode(&bytes, value_index + 1, EntryLimit::new(value_index + 1)),
                Err(DeltaVarintDecodeError::Value {
                    value_index,
                    byte_offset,
                    cause: DeltaVarintDecodeCause::Varint(VarintDecodeError::Empty),
                })
            );
        }
    }

    #[test]
    fn count_and_input_consumption_are_exact_including_zero() {
        assert_eq!(decode(&[], 0, EntryLimit::new(0)), Ok(Vec::new()));
        assert_eq!(
            decode(&[0x00], 0, EntryLimit::new(0)),
            Err(DeltaVarintDecodeError::Value {
                value_index: 0,
                byte_offset: 0,
                cause: DeltaVarintDecodeCause::TrailingBytes { trailing: 1 },
            })
        );

        let encoded = encode(&[1, 2, 3]).expect("canonical sequence must encode");
        assert_eq!(
            decode(&encoded, 2, EntryLimit::new(2)),
            Err(DeltaVarintDecodeError::Value {
                value_index: 2,
                byte_offset: 2,
                cause: DeltaVarintDecodeCause::TrailingBytes { trailing: 1 },
            })
        );
        assert_eq!(
            decode(&encoded, 4, EntryLimit::new(4)),
            Err(DeltaVarintDecodeError::Value {
                value_index: 3,
                byte_offset: 3,
                cause: DeltaVarintDecodeCause::Varint(VarintDecodeError::Empty),
            })
        );
    }

    #[test]
    fn entry_limit_precedes_parsing_and_allocation() {
        assert_eq!(
            decode(&[0x80], 1, EntryLimit::new(0)),
            Err(DeltaVarintDecodeError::EntryLimitExceeded { count: 1, limit: 0 })
        );
        assert_eq!(
            decode(&[0x80], usize::MAX, EntryLimit::new(usize::MAX)),
            Err(DeltaVarintDecodeError::Value {
                value_index: 0,
                byte_offset: 0,
                cause: DeltaVarintDecodeCause::Varint(VarintDecodeError::Truncated { consumed: 1 }),
            })
        );
    }

    #[test]
    fn exhaustive_short_byte_streams_match_reference_and_preserve_typed_bounds() {
        for input_len in 0..=2_usize {
            let case_count = 1_usize << (input_len * 8);
            for packed in 0..case_count {
                let bytes = packed.to_le_bytes();
                let input = &bytes[..input_len];

                for count in 0..=3_usize {
                    let expected = reference_accepts(input, count);
                    let actual = decode(input, count, EntryLimit::new(count));
                    match expected {
                        Some(expected) => {
                            assert_eq!(
                                actual.as_ref(),
                                Ok(&expected),
                                "input={input:02x?}, count={count}"
                            );
                            let Ok(actual) = actual else {
                                continue;
                            };
                            assert_eq!(
                                encode(&actual).as_deref(),
                                Ok(input),
                                "accepted input must be the unique canonical encoding"
                            );
                        }
                        None => {
                            assert!(
                                matches!(&actual, Err(DeltaVarintDecodeError::Value { .. })),
                                "bounded malformed input returned non-value error: \
                                 input={input:02x?}, count={count}, actual={actual:?}"
                            );
                            let Err(DeltaVarintDecodeError::Value {
                                value_index,
                                byte_offset,
                                ..
                            }) = actual
                            else {
                                continue;
                            };
                            assert!(
                                value_index <= count,
                                "input={input:02x?}, count={count}, value_index={value_index}"
                            );
                            assert!(
                                byte_offset <= input.len(),
                                "input={input:02x?}, count={count}, byte_offset={byte_offset}"
                            );
                        }
                    }

                    if count != 0 {
                        assert_eq!(
                            decode(input, count, EntryLimit::new(count - 1)),
                            Err(DeltaVarintDecodeError::EntryLimitExceeded {
                                count,
                                limit: count - 1,
                            }),
                            "entry bound must win for input={input:02x?}, count={count}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn checked_gap_addition_rejects_value_overflow() {
        let mut bytes = encode_u64(u64::MAX).as_bytes().to_vec();
        bytes.push(0x01);
        assert_eq!(
            decode(&bytes, 2, EntryLimit::new(2)),
            Err(DeltaVarintDecodeError::Value {
                value_index: 1,
                byte_offset: 10,
                cause: DeltaVarintDecodeCause::ValueOverflow {
                    previous: u64::MAX,
                    gap: 1,
                },
            })
        );
    }

    #[test]
    fn deterministic_adversarial_gap_streams_are_byte_stable() {
        let mut state = 0x6a09_e667_f3bc_c909_u64;
        for sequence_index in 0..512_u64 {
            let mut values = Vec::new();
            let mut current = sequence_index & 0x3fff;
            values.push(current);

            for gap_index in 0..255_u64 {
                state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut mixed = state;
                mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                mixed ^= mixed >> 31;

                let gap = match gap_index % 10 {
                    0 | 1 => 0,
                    2 => 1,
                    3 => 127,
                    4 => 128,
                    5 => 16_383,
                    6 => 16_384,
                    7 => mixed & 0x7f,
                    8 => mixed & 0xffff_ffff,
                    _ => mixed,
                };
                let Some(next) = current.checked_add(gap) else {
                    values.push(u64::MAX);
                    break;
                };
                current = next;
                values.push(current);
            }

            let first = encode(&values).expect("generated sequence must be monotone");
            let second = encode(&values).expect("generated sequence must encode twice");
            assert_eq!(first, second);
            assert_eq!(
                decode(&first, values.len(), EntryLimit::new(values.len())),
                Ok(values)
            );
        }
    }
}
