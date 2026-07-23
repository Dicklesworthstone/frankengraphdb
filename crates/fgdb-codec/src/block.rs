//! Deterministic scalar block compression with bounded back-references.
//!
//! This is a small, Snappy-class LZ kernel owned by FrankenGraphDB. It is
//! **not** Snappy wire-compatible and deliberately defines no durable codec
//! identifier, version, checksum, or block-length framing. A registered format
//! must supply those fields, select an immutable [`CodecProfile`], and pass its
//! independently authenticated decoded length to [`decompress`].
//!
//! # Internal token grammar
//!
//! The versionless token stream is a concatenation of:
//!
//! - `0lllllll <literal bytes...>`: a literal. Its payload length is the
//!   unsigned seven-bit value `l + 1`, so one token carries `1..=128` bytes.
//! - `1ccccccc <distance_lo> <distance_hi>`: a back-reference. Its copy length
//!   is `c + 4`, so one token copies `4..=131` bytes. Distance is an unsigned
//!   little-endian `u16` in `1..=65535` and counts backwards from the output
//!   cursor. Copies use byte-at-a-time LZ semantics, so source and destination
//!   may overlap.
//!
//! There is no terminator. The decoder stops only at the caller's expected
//! decoded length and then requires the encoded input to be exhausted exactly.
//! This grammar is an internal scalar mechanic, not a durable envelope.
//!
//! # Pinned scalar policy
//!
//! Compression scans left-to-right. Each four-byte prefix maps to one hash
//! bucket containing only the most recently observed position. At a cursor the
//! old bucket is probed before that cursor replaces it. A valid candidate must
//! be within the profile window; the compressor emits its longest match,
//! capped at 131 bytes, whenever at least four bytes match. Thus the tie-break
//! is always "most recent source position", with no platform-dependent search
//! order. Interior positions skipped by a match are not inserted. Literals are
//! emitted in canonical chunks of at most 128 bytes. The same input and
//! profile therefore produce byte-identical output.
//!
//! Scalar compression and decompression are linear in block length because a
//! match comparison is capped at 131 bytes. Compression requests
//! `O(hash_table_entries + encoded_bound)` temporary/output capacity;
//! decompression requests the caller-authorized logical output length and
//! materializes exactly that many bytes.

#![forbid(unsafe_code)]

use core::fmt;

/// Smallest back-reference emitted by the scalar policy.
pub const MIN_COPY_LENGTH: usize = 4;

/// Largest back-reference representable by one copy token.
pub const MAX_COPY_LENGTH: usize = 131;

/// Largest literal payload representable by one literal token.
pub const MAX_LITERAL_LENGTH: usize = 128;

/// Smallest legal history window.
pub const MIN_WINDOW_SIZE: usize = 1;

/// Largest distance representable by the internal token grammar.
pub const MAX_WINDOW_SIZE: usize = u16::MAX as usize;

/// Smallest supported deterministic match table.
pub const MIN_HASH_TABLE_ENTRIES: usize = 256;

/// Largest supported deterministic match table.
pub const MAX_HASH_TABLE_ENTRIES: usize = 1 << 20;

const EMPTY_POSITION: usize = usize::MAX;
const COPY_TAG_MASK: u8 = 0x80;

/// Invalid immutable scalar-compression profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileError {
    /// The history window cannot represent a positive distance.
    WindowTooSmall {
        /// Rejected window in bytes.
        actual: usize,
        /// Minimum legal window in bytes.
        minimum: usize,
    },
    /// The history window exceeds the grammar's `u16` distance domain.
    WindowTooLarge {
        /// Rejected window in bytes.
        actual: usize,
        /// Maximum legal window in bytes.
        maximum: usize,
    },
    /// The hash table is too small for the pinned scalar profile family.
    HashTableTooSmall {
        /// Rejected entry count.
        actual: usize,
        /// Minimum legal entry count.
        minimum: usize,
    },
    /// The hash table exceeds the bounded scalar profile family.
    HashTableTooLarge {
        /// Rejected entry count.
        actual: usize,
        /// Maximum legal entry count.
        maximum: usize,
    },
    /// The hash entry count must be a power of two so bucket selection is
    /// platform-independent.
    HashTableNotPowerOfTwo {
        /// Rejected entry count.
        actual: usize,
    },
    /// The worst-case encoded size of an authorized block is not
    /// representable on this platform.
    BlockLimitTooLarge {
        /// Rejected decoded block ceiling.
        max_block_len: usize,
    },
}

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::WindowTooSmall { actual, minimum } => write!(
                formatter,
                "block-codec window {actual} is smaller than minimum {minimum}"
            ),
            Self::WindowTooLarge { actual, maximum } => write!(
                formatter,
                "block-codec window {actual} exceeds maximum {maximum}"
            ),
            Self::HashTableTooSmall { actual, minimum } => write!(
                formatter,
                "block-codec hash table has {actual} entries, minimum is {minimum}"
            ),
            Self::HashTableTooLarge { actual, maximum } => write!(
                formatter,
                "block-codec hash table has {actual} entries, maximum is {maximum}"
            ),
            Self::HashTableNotPowerOfTwo { actual } => write!(
                formatter,
                "block-codec hash table entry count {actual} is not a power of two"
            ),
            Self::BlockLimitTooLarge { max_block_len } => write!(
                formatter,
                "block-codec block limit {max_block_len} has no representable encoded bound"
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

/// Immutable parameters for the pinned scalar match policy.
///
/// The private fields and absence of setters ensure one compression operation
/// cannot observe a mid-stream policy change. Durable registered profiles may
/// construct one of these values, but this type itself carries no profile ID.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CodecProfile {
    window_size: usize,
    hash_table_entries: usize,
    max_block_len: usize,
    max_encoded_bound: usize,
}

impl CodecProfile {
    /// Validates and constructs a scalar profile.
    ///
    /// `hash_table_entries` must be a power of two between
    /// [`MIN_HASH_TABLE_ENTRIES`] and [`MAX_HASH_TABLE_ENTRIES`], inclusive.
    /// `max_block_len` is the largest caller-owned input this profile permits;
    /// construction also proves its literal-only encoded bound fits `usize`.
    pub fn try_new(
        window_size: usize,
        hash_table_entries: usize,
        max_block_len: usize,
    ) -> Result<Self, ProfileError> {
        if window_size < MIN_WINDOW_SIZE {
            return Err(ProfileError::WindowTooSmall {
                actual: window_size,
                minimum: MIN_WINDOW_SIZE,
            });
        }
        if window_size > MAX_WINDOW_SIZE {
            return Err(ProfileError::WindowTooLarge {
                actual: window_size,
                maximum: MAX_WINDOW_SIZE,
            });
        }
        if hash_table_entries < MIN_HASH_TABLE_ENTRIES {
            return Err(ProfileError::HashTableTooSmall {
                actual: hash_table_entries,
                minimum: MIN_HASH_TABLE_ENTRIES,
            });
        }
        if hash_table_entries > MAX_HASH_TABLE_ENTRIES {
            return Err(ProfileError::HashTableTooLarge {
                actual: hash_table_entries,
                maximum: MAX_HASH_TABLE_ENTRIES,
            });
        }
        if !hash_table_entries.is_power_of_two() {
            return Err(ProfileError::HashTableNotPowerOfTwo {
                actual: hash_table_entries,
            });
        }
        let max_encoded_bound = max_encoded_len(max_block_len)
            .ok_or(ProfileError::BlockLimitTooLarge { max_block_len })?;

        Ok(Self {
            window_size,
            hash_table_entries,
            max_block_len,
            max_encoded_bound,
        })
    }

    /// Returns the maximum permitted copy distance.
    #[must_use]
    pub const fn window_size(self) -> usize {
        self.window_size
    }

    /// Returns the fixed number of most-recent-position hash buckets.
    #[must_use]
    pub const fn hash_table_entries(self) -> usize {
        self.hash_table_entries
    }

    /// Returns the largest decoded block accepted for compression.
    #[must_use]
    pub const fn max_block_len(self) -> usize {
        self.max_block_len
    }
}

/// Allocation involved in scalar compression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompressionAllocation {
    /// Most-recent-position hash table.
    HashTable,
    /// Bounded encoded output.
    EncodedOutput,
}

/// Checked scalar compression failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompressionError {
    /// The caller supplied a block larger than its immutable profile permits.
    InputLimitExceeded {
        /// Actual input bytes.
        actual: usize,
        /// Profile ceiling.
        limit: usize,
    },
    /// Reserving a bounded internal vector failed before output publication.
    AllocationFailed {
        /// Storage whose reservation failed.
        target: CompressionAllocation,
        /// Entries or bytes requested, according to `target`.
        requested: usize,
    },
}

impl fmt::Display for CompressionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::InputLimitExceeded { actual, limit } => write!(
                formatter,
                "block-codec input has {actual} bytes, profile limit is {limit}"
            ),
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "could not reserve {requested} units for block-codec {target:?}"
            ),
        }
    }
}

impl std::error::Error for CompressionError {}

/// Explicit ceiling on bytes materialized by one scalar decompression call.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OutputLimit(usize);

impl OutputLimit {
    /// Creates an exact caller-selected decoded-output ceiling.
    #[must_use]
    pub const fn new(max_bytes: usize) -> Self {
        Self(max_bytes)
    }

    /// Returns the configured decoded-output ceiling.
    #[must_use]
    pub const fn max_bytes(self) -> usize {
        self.0
    }
}

/// Specific reason a scalar token stream was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeErrorKind {
    /// The authenticated expected length exceeds the caller's resource limit.
    OutputLimitExceeded {
        /// Authenticated decoded length.
        expected: usize,
        /// Caller-selected output ceiling.
        limit: usize,
    },
    /// Reserving the authorized logical output length failed.
    AllocationFailed {
        /// Decoded bytes requested.
        requested: usize,
    },
    /// Input ended where another token tag was required.
    TruncatedToken,
    /// A literal token did not contain all of its declared payload.
    TruncatedLiteral {
        /// Bytes declared by the tag.
        declared: usize,
        /// Payload bytes actually available after the tag.
        available: usize,
    },
    /// A copy token did not contain both little-endian distance bytes.
    TruncatedCopy {
        /// Distance bytes available after the tag.
        available: usize,
    },
    /// A token would produce more than the authenticated expected length.
    OutputOvershoot {
        /// Authenticated decoded length.
        expected: usize,
        /// Decoded length after applying the rejected token, saturated at
        /// `usize::MAX` when the addition itself is unrepresentable.
        attempted: usize,
    },
    /// Copy distance zero has no source byte and is never legal.
    ZeroDistance,
    /// A copy reaches before the start of already decoded output.
    DistanceTooFar {
        /// Rejected distance.
        distance: usize,
        /// Bytes available behind the output cursor.
        produced: usize,
    },
    /// The expected output was complete before encoded input was exhausted.
    TrailingBytes {
        /// Unconsumed encoded bytes.
        trailing: usize,
    },
}

/// Scalar decompression failure carrying the rejected token's byte offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeError {
    offset: usize,
    kind: DecodeErrorKind,
}

impl DecodeError {
    /// Returns the zero-based encoded byte offset associated with the failure.
    #[must_use]
    pub const fn offset(self) -> usize {
        self.offset
    }

    /// Returns the typed rejection reason.
    #[must_use]
    pub const fn kind(self) -> DecodeErrorKind {
        self.kind
    }

    const fn at(offset: usize, kind: DecodeErrorKind) -> Self {
        Self { offset, kind }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "block-codec decode error at byte {}: ",
            self.offset
        )?;
        match self.kind {
            DecodeErrorKind::OutputLimitExceeded { expected, limit } => write!(
                formatter,
                "expected output {expected} exceeds limit {limit}"
            ),
            DecodeErrorKind::AllocationFailed { requested } => {
                write!(formatter, "could not reserve {requested} output bytes")
            }
            DecodeErrorKind::TruncatedToken => formatter.write_str("missing token tag"),
            DecodeErrorKind::TruncatedLiteral {
                declared,
                available,
            } => write!(
                formatter,
                "literal declares {declared} bytes but only {available} are available"
            ),
            DecodeErrorKind::TruncatedCopy { available } => write!(
                formatter,
                "copy needs two distance bytes but only {available} are available"
            ),
            DecodeErrorKind::OutputOvershoot {
                expected,
                attempted,
            } => write!(
                formatter,
                "token would reach decoded length {attempted} (saturated at usize::MAX), expected exactly {expected}"
            ),
            DecodeErrorKind::ZeroDistance => formatter.write_str("copy distance is zero"),
            DecodeErrorKind::DistanceTooFar { distance, produced } => write!(
                formatter,
                "copy distance {distance} exceeds {produced} produced bytes"
            ),
            DecodeErrorKind::TrailingBytes { trailing } => {
                write!(formatter, "{trailing} trailing encoded bytes")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// Returns the literal-only upper bound for a block's encoded length.
///
/// The compressor never exceeds this bound: every copy replaces at least four
/// input bytes with three token bytes, which pays for any literal run split
/// introduced by that copy. `None` means the bound is not representable.
#[must_use]
pub const fn max_encoded_len(input_len: usize) -> Option<usize> {
    let literal_tokens = input_len.div_ceil(MAX_LITERAL_LENGTH);
    input_len.checked_add(literal_tokens)
}

/// Compresses one caller-framed block under an immutable scalar profile.
///
/// The output vector requests the checked literal-only upper bound before any
/// token is emitted, and the match table requests its exact logical profile
/// length. Consequently no emitted token can trigger an additional allocation.
pub fn compress(input: &[u8], profile: CodecProfile) -> Result<Vec<u8>, CompressionError> {
    compress_with_reservation(input, profile, &mut HeapReservation)
}

trait Reservation {
    fn encoded_output(
        &mut self,
        output: &mut Vec<u8>,
        requested: usize,
    ) -> Result<(), CompressionError>;

    fn hash_table(
        &mut self,
        positions: &mut Vec<usize>,
        requested: usize,
    ) -> Result<(), CompressionError>;
}

struct HeapReservation;

impl Reservation for HeapReservation {
    fn encoded_output(
        &mut self,
        output: &mut Vec<u8>,
        requested: usize,
    ) -> Result<(), CompressionError> {
        output
            .try_reserve_exact(requested)
            .map_err(|_| CompressionError::AllocationFailed {
                target: CompressionAllocation::EncodedOutput,
                requested,
            })
    }

    fn hash_table(
        &mut self,
        positions: &mut Vec<usize>,
        requested: usize,
    ) -> Result<(), CompressionError> {
        positions
            .try_reserve_exact(requested)
            .map_err(|_| CompressionError::AllocationFailed {
                target: CompressionAllocation::HashTable,
                requested,
            })
    }
}

fn compress_with_reservation<R: Reservation>(
    input: &[u8],
    profile: CodecProfile,
    reservation: &mut R,
) -> Result<Vec<u8>, CompressionError> {
    if input.len() > profile.max_block_len {
        return Err(CompressionError::InputLimitExceeded {
            actual: input.len(),
            limit: profile.max_block_len,
        });
    }

    // Construction proved that the bound exists for `max_block_len`, and this
    // input is no larger. The stored profile bound is a safe defensive
    // fallback that keeps the allocation bounded without exposing an
    // unreachable arithmetic-error variant.
    let encoded_bound = max_encoded_len(input.len()).unwrap_or(profile.max_encoded_bound);

    let mut output = Vec::new();
    reservation.encoded_output(&mut output, encoded_bound)?;

    if input.len() < MIN_COPY_LENGTH {
        emit_literals(&mut output, input);
        return Ok(output);
    }

    let mut latest_positions = Vec::new();
    reservation.hash_table(&mut latest_positions, profile.hash_table_entries)?;
    latest_positions.resize(profile.hash_table_entries, EMPTY_POSITION);

    let mut cursor = 0_usize;
    let mut literal_start = 0_usize;
    while input.len() - cursor >= MIN_COPY_LENGTH {
        let bucket = hash_bucket(input, cursor, profile.hash_table_entries);
        let candidate = latest_positions[bucket];
        latest_positions[bucket] = cursor;

        let match_length = candidate_match_length(input, cursor, candidate, profile.window_size);
        if match_length >= MIN_COPY_LENGTH {
            emit_literals(&mut output, &input[literal_start..cursor]);
            emit_copy(&mut output, cursor - candidate, match_length);
            cursor += match_length;
            literal_start = cursor;
        } else {
            cursor += 1;
        }
    }
    emit_literals(&mut output, &input[literal_start..]);

    debug_assert!(output.len() <= encoded_bound);
    Ok(output)
}

/// Decompresses one token stream to exactly `expected_decoded_len` bytes.
///
/// The expected length must come from trustworthy enclosing framing; it is
/// checked against `output_limit` before any allocation. The encoded stream
/// must end exactly when that many bytes have been produced.
pub fn decompress(
    input: &[u8],
    expected_decoded_len: usize,
    output_limit: OutputLimit,
) -> Result<Vec<u8>, DecodeError> {
    if expected_decoded_len > output_limit.max_bytes() {
        return Err(DecodeError::at(
            0,
            DecodeErrorKind::OutputLimitExceeded {
                expected: expected_decoded_len,
                limit: output_limit.max_bytes(),
            },
        ));
    }

    let mut output = Vec::new();
    output
        .try_reserve_exact(expected_decoded_len)
        .map_err(|_| {
            DecodeError::at(
                0,
                DecodeErrorKind::AllocationFailed {
                    requested: expected_decoded_len,
                },
            )
        })?;

    let mut cursor = 0_usize;
    while output.len() < expected_decoded_len {
        let token_offset = cursor;
        let Some(&tag) = input.get(cursor) else {
            return Err(DecodeError::at(
                token_offset,
                DecodeErrorKind::TruncatedToken,
            ));
        };
        cursor += 1;

        if tag & COPY_TAG_MASK == 0 {
            let literal_len = usize::from(tag) + 1;
            let available = input.len() - cursor;
            if available < literal_len {
                return Err(DecodeError::at(
                    token_offset,
                    DecodeErrorKind::TruncatedLiteral {
                        declared: literal_len,
                        available,
                    },
                ));
            }
            // `literal_len <= available` proves this addition cannot overflow
            // and the resulting range lies inside `input`.
            let literal_end = cursor + literal_len;
            checked_decoded_end(
                output.len(),
                literal_len,
                expected_decoded_len,
                token_offset,
            )?;

            output.extend_from_slice(&input[cursor..literal_end]);
            cursor = literal_end;
        } else {
            let available = input.len() - cursor;
            if available < 2 {
                return Err(DecodeError::at(
                    token_offset,
                    DecodeErrorKind::TruncatedCopy { available },
                ));
            }
            let copy_len = usize::from(tag & !COPY_TAG_MASK) + MIN_COPY_LENGTH;
            let distance = usize::from(u16::from_le_bytes([input[cursor], input[cursor + 1]]));
            cursor += 2;

            if distance == 0 {
                return Err(DecodeError::at(token_offset, DecodeErrorKind::ZeroDistance));
            }
            if distance > output.len() {
                return Err(DecodeError::at(
                    token_offset,
                    DecodeErrorKind::DistanceTooFar {
                        distance,
                        produced: output.len(),
                    },
                ));
            }
            let output_end =
                checked_decoded_end(output.len(), copy_len, expected_decoded_len, token_offset)?;

            while output.len() < output_end {
                let source = output.len() - distance;
                let byte = output[source];
                output.push(byte);
            }
        }
    }

    if cursor != input.len() {
        return Err(DecodeError::at(
            cursor,
            DecodeErrorKind::TrailingBytes {
                trailing: input.len() - cursor,
            },
        ));
    }

    Ok(output)
}

fn checked_decoded_end(
    produced: usize,
    additional: usize,
    expected: usize,
    token_offset: usize,
) -> Result<usize, DecodeError> {
    let Some(attempted) = produced.checked_add(additional) else {
        return Err(DecodeError::at(
            token_offset,
            DecodeErrorKind::OutputOvershoot {
                expected,
                attempted: usize::MAX,
            },
        ));
    };
    if attempted > expected {
        return Err(DecodeError::at(
            token_offset,
            DecodeErrorKind::OutputOvershoot {
                expected,
                attempted,
            },
        ));
    }
    Ok(attempted)
}

fn hash_bucket(input: &[u8], cursor: usize, entries: usize) -> usize {
    debug_assert!(input.len() - cursor >= MIN_COPY_LENGTH);
    debug_assert!(entries.is_power_of_two());
    let word = u32::from_le_bytes([
        input[cursor],
        input[cursor + 1],
        input[cursor + 2],
        input[cursor + 3],
    ]);
    let mixed = word.wrapping_mul(0x1e35_a7bd);
    let hash_bits = entries.trailing_zeros();
    let shift = u32::BITS - hash_bits;
    (mixed >> shift) as usize
}

fn candidate_match_length(
    input: &[u8],
    cursor: usize,
    candidate: usize,
    window_size: usize,
) -> usize {
    if candidate == EMPTY_POSITION {
        return 0;
    }
    debug_assert!(candidate < cursor);
    let distance = cursor - candidate;
    if distance > window_size {
        return 0;
    }

    let maximum = MAX_COPY_LENGTH.min(input.len() - cursor);
    let mut matched = 0_usize;
    // ubs:ignore — these are ordinary compression payload bytes, never authentication secrets.
    while matched < maximum && input[candidate + matched] == input[cursor + matched] {
        matched += 1;
    }
    matched
}

fn emit_literals(output: &mut Vec<u8>, mut literals: &[u8]) {
    while !literals.is_empty() {
        let chunk_len = literals.len().min(MAX_LITERAL_LENGTH);
        output.push((chunk_len - 1) as u8);
        output.extend_from_slice(&literals[..chunk_len]);
        literals = &literals[chunk_len..];
    }
}

fn emit_copy(output: &mut Vec<u8>, distance: usize, copy_len: usize) {
    debug_assert!((MIN_COPY_LENGTH..=MAX_COPY_LENGTH).contains(&copy_len));
    debug_assert!((MIN_WINDOW_SIZE..=MAX_WINDOW_SIZE).contains(&distance));
    output.push(COPY_TAG_MASK | (copy_len - MIN_COPY_LENGTH) as u8);
    output.extend_from_slice(&(distance as u16).to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_BLOCK_LIMIT: usize = 1 << 20;

    fn profile() -> CodecProfile {
        CodecProfile::try_new(4096, 256, TEST_BLOCK_LIMIT).expect("valid test profile")
    }

    struct FailingReservation {
        target: CompressionAllocation,
    }

    impl Reservation for FailingReservation {
        fn encoded_output(
            &mut self,
            output: &mut Vec<u8>,
            requested: usize,
        ) -> Result<(), CompressionError> {
            if self.target == CompressionAllocation::EncodedOutput {
                return Err(CompressionError::AllocationFailed {
                    target: self.target,
                    requested,
                });
            }
            HeapReservation.encoded_output(output, requested)
        }

        fn hash_table(
            &mut self,
            positions: &mut Vec<usize>,
            requested: usize,
        ) -> Result<(), CompressionError> {
            if self.target == CompressionAllocation::HashTable {
                return Err(CompressionError::AllocationFailed {
                    target: self.target,
                    requested,
                });
            }
            HeapReservation.hash_table(positions, requested)
        }
    }

    fn round_trip(bytes: &[u8]) -> Vec<u8> {
        let encoded = compress(bytes, profile()).expect("compression should succeed");
        decompress(&encoded, bytes.len(), OutputLimit::new(TEST_BLOCK_LIMIT))
            .expect("compressed output should decode")
    }

    fn reference_decompress(input: &[u8], expected: usize) -> Vec<u8> {
        let mut output = Vec::new();
        let mut cursor = 0_usize;
        while output.len() < expected {
            let tag = input[cursor];
            cursor += 1;
            if tag & COPY_TAG_MASK == 0 {
                let len = usize::from(tag) + 1;
                output.extend_from_slice(&input[cursor..cursor + len]);
                cursor += len;
            } else {
                let len = usize::from(tag & !COPY_TAG_MASK) + MIN_COPY_LENGTH;
                let distance = usize::from(u16::from_le_bytes([input[cursor], input[cursor + 1]]));
                cursor += 2;
                for _ in 0..len {
                    let source = output.len() - distance;
                    let byte = output[source];
                    output.push(byte);
                }
            }
        }
        assert_eq!(cursor, input.len());
        output
    }

    #[test]
    fn profile_validation_covers_every_bound() {
        assert_eq!(
            CodecProfile::try_new(0, 256, 1),
            Err(ProfileError::WindowTooSmall {
                actual: 0,
                minimum: MIN_WINDOW_SIZE,
            })
        );
        assert_eq!(
            CodecProfile::try_new(MAX_WINDOW_SIZE + 1, 256, 1),
            Err(ProfileError::WindowTooLarge {
                actual: MAX_WINDOW_SIZE + 1,
                maximum: MAX_WINDOW_SIZE,
            })
        );
        assert_eq!(
            CodecProfile::try_new(1, 128, 1),
            Err(ProfileError::HashTableTooSmall {
                actual: 128,
                minimum: MIN_HASH_TABLE_ENTRIES,
            })
        );
        assert_eq!(
            CodecProfile::try_new(1, MAX_HASH_TABLE_ENTRIES * 2, 1),
            Err(ProfileError::HashTableTooLarge {
                actual: MAX_HASH_TABLE_ENTRIES * 2,
                maximum: MAX_HASH_TABLE_ENTRIES,
            })
        );
        assert_eq!(
            CodecProfile::try_new(1, 768, 1),
            Err(ProfileError::HashTableNotPowerOfTwo { actual: 768 })
        );
        assert_eq!(
            CodecProfile::try_new(1, 256, usize::MAX),
            Err(ProfileError::BlockLimitTooLarge {
                max_block_len: usize::MAX,
            })
        );

        let boundary = CodecProfile::try_new(MAX_WINDOW_SIZE, MAX_HASH_TABLE_ENTRIES, 6).unwrap();
        assert_eq!(boundary.window_size(), MAX_WINDOW_SIZE);
        assert_eq!(boundary.hash_table_entries(), MAX_HASH_TABLE_ENTRIES);
        assert_eq!(boundary.max_block_len(), 6);
        assert_eq!(
            compress(b"aaaaaa", boundary).unwrap(),
            [0, b'a', 0x81, 1, 0]
        );
    }

    #[test]
    fn reservation_failures_are_typed_and_tiny_blocks_skip_the_hash_table() {
        let test_profile = CodecProfile::try_new(64, 256, 4).unwrap();
        let mut fail_output = FailingReservation {
            target: CompressionAllocation::EncodedOutput,
        };
        assert_eq!(
            compress_with_reservation(b"abcd", test_profile, &mut fail_output),
            Err(CompressionError::AllocationFailed {
                target: CompressionAllocation::EncodedOutput,
                requested: 5,
            })
        );

        let mut fail_hash = FailingReservation {
            target: CompressionAllocation::HashTable,
        };
        assert_eq!(
            compress_with_reservation(b"abcd", test_profile, &mut fail_hash),
            Err(CompressionError::AllocationFailed {
                target: CompressionAllocation::HashTable,
                requested: 256,
            })
        );

        let maximum_table_tiny =
            CodecProfile::try_new(MAX_WINDOW_SIZE, MAX_HASH_TABLE_ENTRIES, 3).unwrap();
        let mut must_not_touch_hash = FailingReservation {
            target: CompressionAllocation::HashTable,
        };
        assert_eq!(
            compress_with_reservation(b"abc", maximum_table_tiny, &mut must_not_touch_hash)
                .unwrap(),
            [2, b'a', b'b', b'c']
        );
    }

    #[test]
    fn golden_literal_and_overlapping_copy_vectors() {
        assert_eq!(compress(b"", profile()).unwrap(), b"");
        assert_eq!(compress(b"abc", profile()).unwrap(), [2, b'a', b'b', b'c']);
        assert_eq!(
            compress(b"aaaaaa", profile()).unwrap(),
            [0, b'a', 0x81, 1, 0]
        );

        assert_eq!(
            decompress(
                &[0, b'a', 0x81, 1, 0],
                6,
                OutputLimit::new(TEST_BLOCK_LIMIT)
            )
            .unwrap(),
            b"aaaaaa"
        );
    }

    #[test]
    fn golden_hash_collision_keeps_only_the_most_recent_candidate() {
        let input = [
            0x02, 0x00, 0x00, 0x00, 0xa3, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
        ];
        assert_eq!(hash_bucket(&input, 0, 256), 60);
        assert_eq!(hash_bucket(&input, 4, 256), 60);
        assert_eq!(hash_bucket(&input, 8, 256), 60);
        for cursor in [1, 2, 3, 5, 6, 7] {
            assert_ne!(hash_bucket(&input, cursor, 256), 60);
        }

        // The most recent colliding prefix at offset four differs, so the
        // older matching prefix at zero is intentionally not searched.
        assert_eq!(
            compress(&input, profile()).unwrap(),
            [
                11, 0x02, 0x00, 0x00, 0x00, 0xa3, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
            ]
        );
    }

    #[test]
    fn golden_window_boundary_is_inclusive() {
        let input = b"abcd12345abcd";
        let exact_window = CodecProfile::try_new(9, 256, input.len()).unwrap();
        let distance_is_window_plus_one = CodecProfile::try_new(8, 256, input.len()).unwrap();

        assert_eq!(
            compress(input, exact_window).unwrap(),
            [
                8, b'a', b'b', b'c', b'd', b'1', b'2', b'3', b'4', b'5', 0x80, 9, 0,
            ]
        );
        assert_eq!(
            compress(input, distance_is_window_plus_one).unwrap(),
            [
                12, b'a', b'b', b'c', b'd', b'1', b'2', b'3', b'4', b'5', b'a', b'b', b'c', b'd',
            ]
        );
    }

    #[test]
    fn golden_skipped_interiors_and_copy_cap_are_pinned() {
        // The first copy starts at one and skips offsets two through six.
        // Therefore the final "aaaa" resolves to source one (distance seven),
        // rather than to an interior position.
        assert_eq!(
            compress(b"aaaaaaabaaaa", profile()).unwrap(),
            [0, b'a', 0x82, 1, 0, 0, b'b', 0x80, 7, 0]
        );

        let chained = vec![b'a'; 1 + 2 * MAX_COPY_LENGTH];
        assert_eq!(
            compress(&chained, profile()).unwrap(),
            [0, b'a', 0xff, 1, 0, 0xff, 131, 0]
        );
    }

    #[test]
    fn golden_literal_chunk_boundary_is_pinned() {
        let literal_128: Vec<u8> = (0_u8..=127).collect();
        let mut expected_128 = Vec::with_capacity(129);
        expected_128.push(0x7f);
        expected_128.extend_from_slice(&literal_128);
        assert_eq!(compress(&literal_128, profile()).unwrap(), expected_128);

        let literal_129: Vec<u8> = (0_u8..=128).collect();
        let mut expected_129 = Vec::with_capacity(131);
        expected_129.push(0x7f);
        expected_129.extend_from_slice(&literal_129[..128]);
        expected_129.extend_from_slice(&[0, 128]);
        assert_eq!(compress(&literal_129, profile()).unwrap(), expected_129);
    }

    #[test]
    fn exhaustive_small_alphabet_round_trips_and_matches_reference() {
        for len in 0_u32..=8 {
            let cases = 3_usize.pow(len);
            for mut ordinal in 0..cases {
                let mut input = vec![0_u8; len as usize];
                for byte in &mut input {
                    *byte = (ordinal % 3) as u8;
                    ordinal /= 3;
                }

                let encoded = compress(&input, profile()).unwrap();
                let decoded =
                    decompress(&encoded, input.len(), OutputLimit::new(TEST_BLOCK_LIMIT)).unwrap();
                assert_eq!(decoded, input);
                assert_eq!(reference_decompress(&encoded, input.len()), input);
            }
        }
    }

    #[test]
    fn repetitive_and_incompressible_blocks_round_trip() {
        let repetitive = vec![b'x'; 32 * 1024];
        let repetitive_encoded = compress(&repetitive, profile()).unwrap();
        assert!(
            repetitive_encoded.len() < repetitive.len(),
            "repetition should provide concrete compression evidence"
        );
        assert_eq!(
            decompress(
                &repetitive_encoded,
                repetitive.len(),
                OutputLimit::new(TEST_BLOCK_LIMIT)
            )
            .unwrap(),
            repetitive
        );

        let mut state = 0x243f_6a88_u32;
        let mut incompressible = Vec::new();
        incompressible.try_reserve_exact(16 * 1024).unwrap();
        for _ in 0..16 * 1024 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            incompressible.push(state as u8);
        }
        let incompressible_encoded = compress(&incompressible, profile()).unwrap();
        assert_eq!(
            decompress(
                &incompressible_encoded,
                incompressible.len(),
                OutputLimit::new(TEST_BLOCK_LIMIT)
            )
            .unwrap(),
            incompressible
        );
        assert!(incompressible_encoded.len() <= max_encoded_len(incompressible.len()).unwrap());
    }

    #[test]
    fn deterministic_for_same_input_and_profile() {
        let input = b"abcdabcdabcd-0123456789-abcdabcdabcd";
        let expected = compress(input, profile()).unwrap();
        for _ in 0..128 {
            assert_eq!(compress(input, profile()).unwrap(), expected);
        }
    }

    #[test]
    fn window_changes_are_explicit_and_deterministic() {
        let input = b"abcdefgh-abcdefgh";
        let narrow = CodecProfile::try_new(4, 256, input.len()).unwrap();
        let wide = CodecProfile::try_new(64, 256, input.len()).unwrap();
        let narrow_bytes = compress(input, narrow).unwrap();
        let wide_bytes = compress(input, wide).unwrap();

        assert_ne!(narrow_bytes, wide_bytes);
        assert_eq!(
            decompress(&narrow_bytes, input.len(), OutputLimit::new(input.len())).unwrap(),
            input
        );
        assert_eq!(
            decompress(&wide_bytes, input.len(), OutputLimit::new(input.len())).unwrap(),
            input
        );
    }

    #[test]
    fn every_literal_payload_position_is_checked() {
        for declared in 1_usize..=MAX_LITERAL_LENGTH {
            for available in 0..declared {
                let mut encoded = Vec::new();
                encoded.push((declared - 1) as u8);
                encoded.extend(std::iter::repeat_n(b'z', available));
                let error = decompress(&encoded, declared, OutputLimit::new(declared)).unwrap_err();
                assert_eq!(error.offset(), 0);
                assert_eq!(
                    error.kind(),
                    DecodeErrorKind::TruncatedLiteral {
                        declared,
                        available,
                    }
                );
            }
        }
    }

    #[test]
    fn every_copy_header_position_is_checked() {
        let missing_both = decompress(&[0x80], 4, OutputLimit::new(4)).unwrap_err();
        assert_eq!(
            missing_both,
            DecodeError::at(0, DecodeErrorKind::TruncatedCopy { available: 0 })
        );

        let missing_high = decompress(&[0x80, 1], 4, OutputLimit::new(4)).unwrap_err();
        assert_eq!(
            missing_high,
            DecodeError::at(0, DecodeErrorKind::TruncatedCopy { available: 1 })
        );

        let after_literal = decompress(&[0, b'a', 0x80, 1], 5, OutputLimit::new(5)).unwrap_err();
        assert_eq!(
            after_literal,
            DecodeError::at(2, DecodeErrorKind::TruncatedCopy { available: 1 })
        );
    }

    #[test]
    fn malformed_distances_are_rejected_at_token_offset() {
        let zero = decompress(&[0x80, 0, 0], 4, OutputLimit::new(4)).unwrap_err();
        assert_eq!(zero, DecodeError::at(0, DecodeErrorKind::ZeroDistance));

        let too_far = decompress(&[0, b'a', 0x80, 2, 0], 5, OutputLimit::new(5)).unwrap_err();
        assert_eq!(
            too_far,
            DecodeError::at(
                2,
                DecodeErrorKind::DistanceTooFar {
                    distance: 2,
                    produced: 1,
                }
            )
        );
    }

    #[test]
    fn missing_token_overshoot_and_trailing_bytes_are_distinct() {
        assert_eq!(
            decompress(&[], 1, OutputLimit::new(1)).unwrap_err(),
            DecodeError::at(0, DecodeErrorKind::TruncatedToken)
        );

        assert_eq!(
            decompress(&[1, b'a', b'b'], 1, OutputLimit::new(2)).unwrap_err(),
            DecodeError::at(
                0,
                DecodeErrorKind::OutputOvershoot {
                    expected: 1,
                    attempted: 2,
                }
            )
        );

        assert_eq!(
            decompress(&[0, b'a', 0x80, 1, 0], 4, OutputLimit::new(5)).unwrap_err(),
            DecodeError::at(
                2,
                DecodeErrorKind::OutputOvershoot {
                    expected: 4,
                    attempted: 5,
                }
            )
        );

        assert_eq!(
            decompress(&[0, b'a', 0, b'b'], 1, OutputLimit::new(1)).unwrap_err(),
            DecodeError::at(2, DecodeErrorKind::TrailingBytes { trailing: 2 })
        );
        assert_eq!(
            decompress(&[0], 0, OutputLimit::new(0)).unwrap_err(),
            DecodeError::at(0, DecodeErrorKind::TrailingBytes { trailing: 1 })
        );
    }

    #[test]
    fn resource_limits_are_checked_before_materialization() {
        let output_error = decompress(&[], 65, OutputLimit::new(64)).unwrap_err();
        assert_eq!(
            output_error,
            DecodeError::at(
                0,
                DecodeErrorKind::OutputLimitExceeded {
                    expected: 65,
                    limit: 64,
                }
            )
        );

        let tiny_profile = CodecProfile::try_new(64, 256, 3).unwrap();
        assert_eq!(
            compress(b"four", tiny_profile),
            Err(CompressionError::InputLimitExceeded {
                actual: 4,
                limit: 3,
            })
        );

        assert_eq!(
            decompress(&[], usize::MAX, OutputLimit::new(usize::MAX)),
            Err(DecodeError::at(
                0,
                DecodeErrorKind::AllocationFailed {
                    requested: usize::MAX,
                }
            ))
        );
    }

    #[test]
    fn checked_size_helpers_cover_unrepresentable_lengths() {
        assert_eq!(max_encoded_len(0), Some(0));
        assert_eq!(max_encoded_len(128), Some(129));
        assert_eq!(max_encoded_len(129), Some(131));
        assert_eq!(max_encoded_len(usize::MAX), None);

        assert_eq!(
            checked_decoded_end(usize::MAX, 1, usize::MAX, 17),
            Err(DecodeError::at(
                17,
                DecodeErrorKind::OutputOvershoot {
                    expected: usize::MAX,
                    attempted: usize::MAX,
                }
            ))
        );
    }

    #[test]
    fn maximum_token_lengths_decode_correctly() {
        let literal = vec![0x7f; MAX_LITERAL_LENGTH];
        let mut literal_token = Vec::new();
        literal_token.push(0x7f);
        literal_token.extend_from_slice(&literal);
        assert_eq!(
            decompress(
                &literal_token,
                MAX_LITERAL_LENGTH,
                OutputLimit::new(MAX_LITERAL_LENGTH)
            )
            .unwrap(),
            literal
        );

        let encoded = [0, b'q', 0xff, 1, 0];
        assert_eq!(
            decompress(
                &encoded,
                1 + MAX_COPY_LENGTH,
                OutputLimit::new(1 + MAX_COPY_LENGTH)
            )
            .unwrap(),
            vec![b'q'; 1 + MAX_COPY_LENGTH]
        );
    }

    #[test]
    fn every_copy_tag_and_overlap_distance_three_decode_correctly() {
        for copy_payload in 0_u8..=127 {
            let copy_len = usize::from(copy_payload) + MIN_COPY_LENGTH;
            let encoded = [2, b'a', b'b', b'c', 0x80 | copy_payload, 3, 0];
            let decoded =
                decompress(&encoded, 3 + copy_len, OutputLimit::new(3 + copy_len)).unwrap();
            let expected: Vec<u8> = (0..3 + copy_len).map(|index| b"abc"[index % 3]).collect();
            assert_eq!(decoded, expected, "copy tag {copy_payload:#04x}");
        }
    }

    #[test]
    fn maximum_copy_distance_decodes_correctly() {
        let prefix: Vec<u8> = (0..MAX_WINDOW_SIZE).map(|index| index as u8).collect();
        let mut encoded = Vec::new();
        encoded
            .try_reserve_exact(max_encoded_len(prefix.len()).unwrap() + 3)
            .unwrap();
        emit_literals(&mut encoded, &prefix);
        encoded.extend_from_slice(&[0x80, 0xff, 0xff]);

        let mut expected = prefix.clone();
        expected.extend_from_slice(&prefix[..MIN_COPY_LENGTH]);
        assert_eq!(
            decompress(&encoded, expected.len(), OutputLimit::new(expected.len())).unwrap(),
            expected
        );
    }

    #[test]
    fn literal_only_output_respects_documented_bound() {
        for len in 0..=4096 {
            let input: Vec<u8> = (0..len).map(|index| index as u8).collect();
            let encoded = compress(&input, profile()).unwrap();
            assert!(encoded.len() <= max_encoded_len(input.len()).unwrap());
            assert_eq!(round_trip(&input), input);
        }
    }
}
