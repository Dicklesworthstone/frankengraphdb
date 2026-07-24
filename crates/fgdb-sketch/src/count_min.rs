//! Deterministic insertion-only Count-Min frequency sketches.
//!
//! The sketch is deliberately explicit about the operation it cannot support:
//! deletion returns [`CountMinError::RebuildRequired`] without changing state.
//! This prevents an advisory frequency estimate from silently drifting after a
//! property or edge is removed.

use core::fmt;
use core::hash::Hasher;
use fgdb_collections::hash_table::SeededHasher;
use std::collections::TryReserveError;

/// Conservative default ceiling for one sketch's counter matrix.
pub const DEFAULT_MAX_CELLS: usize = 16 * 1024 * 1024;

const CANONICAL_MAGIC: [u8; 8] = *b"FGDBCMS1";
const CANONICAL_VERSION: u16 = 1;
const CANONICAL_HEADER_BYTES: usize = 8 + 2 + (2 * 8) + 2 + (5 * 8);
const COUNTER_BYTES: usize = 8;
const DEFAULT_MAX_DECODED_DEPTH: usize = 64;
const DEFAULT_MAX_ENCODED_BYTES: usize =
    CANONICAL_HEADER_BYTES + (DEFAULT_MAX_CELLS * COUNTER_BYTES);
const PARTS_PER_MILLION: u64 = 1_000_000;
const E_UPPER_PARTS_PER_MILLION: u64 = 2_718_282;
const E_LOWER_PARTS_PER_MILLION: u64 = 2_718_281;

/// Complete profile governing shape, hashing, and resource bounds.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CountMinProfile {
    /// Counters in each row.
    pub width: usize,
    /// Independently seeded rows.
    pub depth: usize,
    /// Stable bucket-mapping algorithm retained in merge identity.
    pub hash_algorithm: CountMinHashAlgorithm,
    /// Explicit deterministic root seed.
    pub seed: u64,
    /// Maximum accepted sum of observation weights.
    pub max_total_weight: u64,
    /// Maximum allocated counters.
    pub max_cells: usize,
}

impl CountMinProfile {
    /// Constructs a profile with the crate's conservative cell ceiling.
    #[must_use]
    pub const fn new(width: usize, depth: usize, seed: u64, max_total_weight: u64) -> Self {
        Self {
            width,
            depth,
            hash_algorithm: CountMinHashAlgorithm::SeededFnvMix64V1,
            seed,
            max_total_weight,
            max_cells: DEFAULT_MAX_CELLS,
        }
    }

    /// Validates this profile and returns its model-qualified overestimate contract.
    ///
    /// This is a statistical accuracy statement under the named row-collision
    /// model, not a safety invariant. The one-sided property that estimates
    /// never understate accepted positive weight is unconditional. A profile
    /// that fails structural or resource-bound validation cannot produce a
    /// contract.
    pub fn error_contract(self) -> Result<CountMinErrorContract, CountMinError> {
        validate_profile(self)?;
        Ok(CountMinErrorContract {
            width: self.width,
            independent_rows: self.depth,
            assumption: CountMinAccuracyAssumption::IndependentUniformRowCollisions,
        })
    }
}

/// Assumption under which a Count-Min probability statement is interpreted.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CountMinAccuracyAssumption {
    /// Each row collision behaves as an independent uniform bucket choice.
    ///
    /// The stable production hash makes runs replayable; this assumption still
    /// needs workload calibration and must not be promoted to an invariant.
    IndependentUniformRowCollisions,
}

/// Profile-derived one-sided Count-Min accuracy contract.
///
/// Under [`CountMinAccuracyAssumption::IndependentUniformRowCollisions`], the
/// estimate for one queried key is at most
/// `truth + ceil(e * total_weight / width)` with failure probability at most
/// `exp(-independent_rows)`. Integer accessors below round the error and failure
/// probability outward so test gates never gain confidence from rounding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CountMinErrorContract {
    /// Counters per row in the governing profile.
    width: usize,
    /// Independently modeled rows in the governing profile.
    independent_rows: usize,
    /// Named statistical assumption for the probability statement.
    assumption: CountMinAccuracyAssumption,
}

impl CountMinErrorContract {
    /// Counters per row in the governing profile.
    #[must_use]
    pub const fn width(self) -> usize {
        self.width
    }

    /// Independently modeled rows in the governing profile.
    #[must_use]
    pub const fn independent_rows(self) -> usize {
        self.independent_rows
    }

    /// Named statistical assumption for the probability statement.
    #[must_use]
    pub const fn assumption(self) -> CountMinAccuracyAssumption {
        self.assumption
    }

    /// Conservative integer ceiling on the permitted overestimate.
    #[must_use]
    pub fn maximum_overestimate(self, total_weight: u64) -> u64 {
        let numerator = u128::from(total_weight) * u128::from(E_UPPER_PARTS_PER_MILLION);
        let denominator = (self.width as u128) * u128::from(PARTS_PER_MILLION);
        let rounded_up = numerator.div_ceil(denominator);
        u64::try_from(rounded_up)
            .unwrap_or(u64::MAX)
            .min(total_weight)
    }

    /// Conservative lower bound on confidence, in parts per million.
    ///
    /// The complementary failure probability is rounded up after every
    /// multiplication by the rational upper bound `1 / 2.718281`.
    #[must_use]
    pub fn confidence_parts_per_million_floor(self) -> u64 {
        let mut failure = PARTS_PER_MILLION;
        for _ in 0..self.independent_rows {
            failure = (failure * PARTS_PER_MILLION)
                .div_ceil(E_LOWER_PARTS_PER_MILLION)
                .max(1);
            if failure == 1 {
                break;
            }
        }
        PARTS_PER_MILLION - failure
    }
}

/// Stable Count-Min bucket-mapping algorithm.
///
/// Retaining this discriminator in the in-memory profile prevents states made
/// by different hash revisions from comparing profile-equal or merging.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u16)]
pub enum CountMinHashAlgorithm {
    /// `fgdb-collections` seeded FNV-1a stream followed by the pinned mix64
    /// finalizer, with the row number written as little-endian `u64`.
    SeededFnvMix64V1 = 1,
}

/// Caller-owned admission bounds for a canonical Count-Min value.
///
/// These limits are deliberately separate from the encoded profile: bytes
/// being decoded cannot grant themselves more memory or per-update CPU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CountMinDecodeLimits {
    /// Largest accepted row width.
    pub max_width: usize,
    /// Largest accepted row count and therefore per-key update cost.
    pub max_depth: usize,
    /// Largest accepted matrix, independent of its encoded profile ceiling.
    pub max_cells: usize,
    /// Largest complete canonical value.
    pub max_encoded_bytes: usize,
}

impl CountMinDecodeLimits {
    /// Conservative crate-level admission policy.
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            max_width: DEFAULT_MAX_CELLS,
            max_depth: DEFAULT_MAX_DECODED_DEPTH,
            max_cells: DEFAULT_MAX_CELLS,
            max_encoded_bytes: DEFAULT_MAX_ENCODED_BYTES,
        }
    }
}

/// Typed failure from construction or a state transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CountMinError {
    /// Width and depth must both be nonzero.
    EmptyDimension {
        /// Rejected width.
        width: usize,
        /// Rejected depth.
        depth: usize,
    },
    /// Multiplying width by depth overflowed.
    CellCountOverflow,
    /// The configured matrix exceeds its explicit resource ceiling.
    CellLimitExceeded {
        /// Requested counters.
        requested: usize,
        /// Configured counter ceiling.
        limit: usize,
    },
    /// The allocator rejected the checked matrix reservation.
    AllocationFailed {
        /// Requested counters.
        requested: usize,
    },
    /// An update or merge would exceed a counter or total-weight bound.
    WeightOverflow,
    /// Merge operands use different complete profiles.
    ProfileMismatch,
    /// This profile cannot subtract observations exactly.
    RebuildRequired {
        /// Weight the caller wanted to remove.
        requested_weight: u64,
    },
}

impl fmt::Display for CountMinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EmptyDimension { width, depth } => {
                write!(
                    formatter,
                    "Count-Min dimensions must be nonzero, got {width}x{depth}"
                )
            }
            Self::CellCountOverflow => {
                formatter.write_str("Count-Min counter count overflows usize")
            }
            Self::CellLimitExceeded { requested, limit } => write!(
                formatter,
                "Count-Min requires {requested} counters, configured limit is {limit}"
            ),
            Self::AllocationFailed { requested } => {
                write!(
                    formatter,
                    "could not reserve {requested} Count-Min counters"
                )
            }
            Self::WeightOverflow => {
                formatter.write_str("Count-Min weight transition exceeds its exact bound")
            }
            Self::ProfileMismatch => {
                formatter.write_str("cannot merge Count-Min sketches with different profiles")
            }
            Self::RebuildRequired { requested_weight } => write!(
                formatter,
                "Count-Min cannot remove weight {requested_weight}; rebuild is required"
            ),
        }
    }
}

impl std::error::Error for CountMinError {}

/// Strict canonical-codec failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CountMinCodecError {
    /// The in-memory or decoded sketch violates a construction invariant.
    State(CountMinError),
    /// A platform-sized profile field cannot be represented canonically.
    IntegerUnrepresentable,
    /// Computing the exact encoded size overflowed.
    LengthOverflow,
    /// The allocator rejected the exact canonical byte reservation.
    AllocationFailed {
        /// Requested byte count.
        requested: usize,
    },
    /// The eight-byte format discriminator did not match.
    MagicMismatch {
        /// Bytes found at the canonical magic position.
        actual: [u8; 8],
    },
    /// The format version is not implemented.
    UnsupportedVersion {
        /// Version found in the input.
        actual: u16,
    },
    /// The encoded bucket-mapping algorithm is unsupported.
    UnsupportedHashAlgorithm {
        /// Hash-algorithm discriminator found in the input.
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
    /// The encoded counter count disagrees with width times depth.
    CounterCountMismatch {
        /// Counter count implied by the profile.
        expected: usize,
        /// Counter count declared by the input.
        actual: usize,
    },
    /// Input contains bytes after the one canonical value.
    TrailingBytes {
        /// First trailing byte.
        offset: usize,
        /// Number of trailing bytes.
        remaining: usize,
    },
    /// The encoded total exceeds its declared profile ceiling.
    TotalWeightExceedsProfile {
        /// Encoded total weight.
        actual: u64,
        /// Profile ceiling.
        maximum: u64,
    },
    /// A counter exceeds the complete accepted weight.
    CounterExceedsTotal {
        /// Row-major counter index.
        index: usize,
        /// Encoded counter.
        counter: u64,
        /// Encoded total weight.
        total_weight: u64,
    },
    /// One row does not sum to the complete accepted weight.
    RowWeightMismatch {
        /// Zero-based row index.
        row: usize,
        /// Required row sum.
        expected: u64,
        /// Encoded row sum.
        actual: u64,
    },
    /// The complete encoded value exceeds the caller-owned byte budget.
    EncodedByteLimitExceeded {
        /// Input byte length.
        actual: usize,
        /// Caller-owned byte ceiling.
        maximum: usize,
    },
    /// The encoded width exceeds the caller-owned CPU or memory budget.
    WidthLimitExceeded {
        /// Encoded width.
        actual: usize,
        /// Caller-owned width ceiling.
        maximum: usize,
    },
    /// The encoded depth exceeds the caller-owned per-key CPU budget.
    DepthLimitExceeded {
        /// Encoded depth.
        actual: usize,
        /// Caller-owned depth ceiling.
        maximum: usize,
    },
    /// The encoded profile or matrix exceeds the caller-owned cell budget.
    DecodeCellLimitExceeded {
        /// Encoded ceiling or concrete matrix size.
        actual: usize,
        /// Caller-owned cell ceiling.
        maximum: usize,
    },
    /// The encoded profile is not the registry-selected profile.
    ProfileMismatch {
        /// Profile selected by trusted metadata.
        expected: CountMinProfile,
        /// Profile found in the canonical value.
        actual: CountMinProfile,
    },
}

impl fmt::Display for CountMinCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for CountMinCodecError {}

impl From<CountMinError> for CountMinCodecError {
    fn from(error: CountMinError) -> Self {
        Self::State(error)
    }
}

/// Canonical logical state borrowed from a sketch.
///
/// Counters are row-major. Equal states have identical profiles, total weight,
/// and counter slices.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CountMinState<'sketch> {
    /// Complete behavior and hashing profile.
    pub profile: CountMinProfile,
    /// Sum of accepted observation weights.
    pub total_weight: u64,
    /// Row-major counter matrix.
    pub counters: &'sketch [u64],
}

/// Mergeable frequency upper-bound summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CountMinSketch {
    profile: CountMinProfile,
    counters: Vec<u64>,
    total_weight: u64,
}

impl CountMinSketch {
    /// Allocates an all-zero matrix after validating every resource bound.
    pub fn try_new(profile: CountMinProfile) -> Result<Self, CountMinError> {
        let cell_count = validate_profile(profile)?;

        let mut counters = Vec::new();
        counters
            .try_reserve_exact(cell_count)
            .map_err(|_: TryReserveError| CountMinError::AllocationFailed {
                requested: cell_count,
            })?;
        counters.resize(cell_count, 0);
        Ok(Self {
            profile,
            counters,
            total_weight: 0,
        })
    }

    /// Returns the complete immutable profile.
    #[must_use]
    pub const fn profile(&self) -> CountMinProfile {
        self.profile
    }

    /// Returns the sum of accepted observation weights.
    #[must_use]
    pub const fn total_weight(&self) -> u64 {
        self.total_weight
    }

    /// Returns the canonical row-major state.
    #[must_use]
    pub fn canonical_state(&self) -> CountMinState<'_> {
        CountMinState {
            profile: self.profile,
            total_weight: self.total_weight,
            counters: &self.counters,
        }
    }

    /// Encodes the complete profile and logical state into one canonical value.
    ///
    /// The representation uses fixed-width big-endian fields followed by the
    /// row-major counter matrix. Equal logical states therefore produce
    /// byte-identical values without relying on host word size.
    pub fn try_to_canonical_bytes(&self) -> Result<Vec<u8>, CountMinCodecError> {
        validate_counter_rows(self.profile, self.total_weight, &self.counters)?;
        let payload_bytes = self
            .counters
            .len()
            .checked_mul(COUNTER_BYTES)
            .ok_or(CountMinCodecError::LengthOverflow)?;
        let encoded_len = CANONICAL_HEADER_BYTES
            .checked_add(payload_bytes)
            .ok_or(CountMinCodecError::LengthOverflow)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(encoded_len)
            .map_err(|_: TryReserveError| CountMinCodecError::AllocationFailed {
                requested: encoded_len,
            })?;
        bytes.extend_from_slice(&CANONICAL_MAGIC);
        push_u16(&mut bytes, CANONICAL_VERSION);
        push_u64(&mut bytes, canonical_usize(self.profile.width)?);
        push_u64(&mut bytes, canonical_usize(self.profile.depth)?);
        push_u16(&mut bytes, self.profile.hash_algorithm as u16);
        push_u64(&mut bytes, self.profile.seed);
        push_u64(&mut bytes, self.profile.max_total_weight);
        push_u64(&mut bytes, canonical_usize(self.profile.max_cells)?);
        push_u64(&mut bytes, self.total_weight);
        push_u64(&mut bytes, canonical_usize(self.counters.len())?);
        for counter in &self.counters {
            push_u64(&mut bytes, *counter);
        }
        debug_assert_eq!(bytes.len(), encoded_len);
        Ok(bytes)
    }

    /// Decodes one strict canonical value and revalidates every state law.
    ///
    /// Length and profile bounds are checked before allocating the matrix.
    pub fn try_from_canonical_bytes(
        bytes: &[u8],
        expected_profile: CountMinProfile,
        limits: CountMinDecodeLimits,
    ) -> Result<Self, CountMinCodecError> {
        if bytes.len() > limits.max_encoded_bytes {
            return Err(CountMinCodecError::EncodedByteLimitExceeded {
                actual: bytes.len(),
                maximum: limits.max_encoded_bytes,
            });
        }
        let mut decoder = CountMinDecoder::new(bytes);
        let magic = decoder.read_array::<8>()?;
        if magic != CANONICAL_MAGIC {
            return Err(CountMinCodecError::MagicMismatch { actual: magic });
        }
        let version = decoder.read_u16()?;
        if version != CANONICAL_VERSION {
            return Err(CountMinCodecError::UnsupportedVersion { actual: version });
        }
        let width = decoded_usize(decoder.read_u64()?)?;
        let depth = decoded_usize(decoder.read_u64()?)?;
        let hash_algorithm = match decoder.read_u16()? {
            1 => CountMinHashAlgorithm::SeededFnvMix64V1,
            actual => {
                return Err(CountMinCodecError::UnsupportedHashAlgorithm { actual });
            }
        };
        let seed = decoder.read_u64()?;
        let max_total_weight = decoder.read_u64()?;
        let max_cells = decoded_usize(decoder.read_u64()?)?;
        let total_weight = decoder.read_u64()?;
        let encoded_counter_count = decoded_usize(decoder.read_u64()?)?;

        let profile = CountMinProfile {
            width,
            depth,
            hash_algorithm,
            seed,
            max_total_weight,
            max_cells,
        };
        if profile != expected_profile {
            return Err(CountMinCodecError::ProfileMismatch {
                expected: expected_profile,
                actual: profile,
            });
        }
        if width > limits.max_width {
            return Err(CountMinCodecError::WidthLimitExceeded {
                actual: width,
                maximum: limits.max_width,
            });
        }
        if depth > limits.max_depth {
            return Err(CountMinCodecError::DepthLimitExceeded {
                actual: depth,
                maximum: limits.max_depth,
            });
        }
        if max_cells > limits.max_cells {
            return Err(CountMinCodecError::DecodeCellLimitExceeded {
                actual: max_cells,
                maximum: limits.max_cells,
            });
        }
        let expected_counter_count = validate_profile(profile)?;
        if expected_counter_count > limits.max_cells {
            return Err(CountMinCodecError::DecodeCellLimitExceeded {
                actual: expected_counter_count,
                maximum: limits.max_cells,
            });
        }
        if encoded_counter_count != expected_counter_count {
            return Err(CountMinCodecError::CounterCountMismatch {
                expected: expected_counter_count,
                actual: encoded_counter_count,
            });
        }
        let payload_bytes = encoded_counter_count
            .checked_mul(COUNTER_BYTES)
            .ok_or(CountMinCodecError::LengthOverflow)?;
        let expected_len = CANONICAL_HEADER_BYTES
            .checked_add(payload_bytes)
            .ok_or(CountMinCodecError::LengthOverflow)?;
        if bytes.len() < expected_len {
            return Err(CountMinCodecError::Truncated {
                offset: decoder.offset,
                needed: payload_bytes,
                remaining: bytes.len().saturating_sub(decoder.offset),
            });
        }
        if bytes.len() > expected_len {
            return Err(CountMinCodecError::TrailingBytes {
                offset: expected_len,
                remaining: bytes.len() - expected_len,
            });
        }
        if total_weight > max_total_weight {
            return Err(CountMinCodecError::TotalWeightExceedsProfile {
                actual: total_weight,
                maximum: max_total_weight,
            });
        }

        let mut preflight = CountMinDecoder {
            bytes,
            offset: decoder.offset,
        };
        validate_encoded_counter_rows(profile, total_weight, &mut preflight)?;
        preflight.finish()?;

        let mut sketch = Self::try_new(profile)?;
        let mut materialize = CountMinDecoder {
            bytes,
            offset: decoder.offset,
        };
        for counter in &mut sketch.counters {
            *counter = materialize.read_u64()?;
        }
        materialize.finish()?;
        sketch.total_weight = total_weight;
        Ok(sketch)
    }

    /// Adds `weight` to a canonical byte key.
    ///
    /// The transition validates the total and every addressed counter before
    /// changing any state, so a typed failure leaves the sketch unchanged.
    pub fn try_observe(&mut self, key: &[u8], weight: u64) -> Result<(), CountMinError> {
        let next_total = self
            .total_weight
            .checked_add(weight)
            .filter(|total| *total <= self.profile.max_total_weight)
            .ok_or(CountMinError::WeightOverflow)?;

        for row in 0..self.profile.depth {
            let index = self.counter_index(row, key);
            self.counters[index]
                .checked_add(weight)
                .ok_or(CountMinError::WeightOverflow)?;
        }
        for row in 0..self.profile.depth {
            let index = self.counter_index(row, key);
            self.counters[index] += weight;
        }
        self.total_weight = next_total;
        Ok(())
    }

    /// Returns the Count-Min upper-bound estimate for `key`.
    #[must_use]
    pub fn estimate(&self, key: &[u8]) -> u64 {
        let mut estimate = u64::MAX;
        for row in 0..self.profile.depth {
            estimate = estimate.min(self.counters[self.counter_index(row, key)]);
        }
        estimate
    }

    /// Rejects deletion without mutating the insertion-only sketch.
    pub const fn try_remove(&mut self, _key: &[u8], weight: u64) -> Result<(), CountMinError> {
        Err(CountMinError::RebuildRequired {
            requested_weight: weight,
        })
    }

    /// Merges another sketch with the identical complete profile.
    ///
    /// Overflow checks cover the full matrix before either operand changes.
    pub fn try_merge(&mut self, other: &Self) -> Result<(), CountMinError> {
        if self.profile != other.profile {
            return Err(CountMinError::ProfileMismatch);
        }
        let next_total = self
            .total_weight
            .checked_add(other.total_weight)
            .filter(|total| *total <= self.profile.max_total_weight)
            .ok_or(CountMinError::WeightOverflow)?;
        for (&left, &right) in self.counters.iter().zip(&other.counters) {
            left.checked_add(right)
                .ok_or(CountMinError::WeightOverflow)?;
        }
        for (left, &right) in self.counters.iter_mut().zip(&other.counters) {
            *left += right;
        }
        self.total_weight = next_total;
        Ok(())
    }

    fn counter_index(&self, row: usize, key: &[u8]) -> usize {
        match self.profile.hash_algorithm {
            CountMinHashAlgorithm::SeededFnvMix64V1 => {
                let mut hasher = SeededHasher::new(self.profile.seed);
                hasher.write_u64(row as u64);
                hasher.write(key);
                let width = self.profile.width as u64;
                row * self.profile.width + (hasher.finish() % width) as usize
            }
        }
    }
}

fn validate_counter_rows(
    profile: CountMinProfile,
    total_weight: u64,
    counters: &[u64],
) -> Result<(), CountMinCodecError> {
    if total_weight > profile.max_total_weight {
        return Err(CountMinCodecError::TotalWeightExceedsProfile {
            actual: total_weight,
            maximum: profile.max_total_weight,
        });
    }
    let expected_counter_count = validate_profile(profile)?;
    if counters.len() != expected_counter_count {
        return Err(CountMinCodecError::CounterCountMismatch {
            expected: expected_counter_count,
            actual: counters.len(),
        });
    }
    for (row, row_counters) in counters.chunks_exact(profile.width).enumerate() {
        let mut row_weight = 0_u64;
        for (column, counter) in row_counters.iter().copied().enumerate() {
            if counter > total_weight {
                let index = row
                    .checked_mul(profile.width)
                    .and_then(|offset| offset.checked_add(column))
                    .ok_or(CountMinCodecError::LengthOverflow)?;
                return Err(CountMinCodecError::CounterExceedsTotal {
                    index,
                    counter,
                    total_weight,
                });
            }
            row_weight = row_weight
                .checked_add(counter)
                .ok_or(CountMinCodecError::State(CountMinError::WeightOverflow))?;
        }
        if row_weight != total_weight {
            return Err(CountMinCodecError::RowWeightMismatch {
                row,
                expected: total_weight,
                actual: row_weight,
            });
        }
    }
    Ok(())
}

fn validate_encoded_counter_rows(
    profile: CountMinProfile,
    total_weight: u64,
    decoder: &mut CountMinDecoder<'_>,
) -> Result<(), CountMinCodecError> {
    for row in 0..profile.depth {
        let mut row_weight = 0_u64;
        for column in 0..profile.width {
            let counter = decoder.read_u64()?;
            let index = row
                .checked_mul(profile.width)
                .and_then(|offset| offset.checked_add(column))
                .ok_or(CountMinCodecError::LengthOverflow)?;
            if counter > total_weight {
                return Err(CountMinCodecError::CounterExceedsTotal {
                    index,
                    counter,
                    total_weight,
                });
            }
            row_weight = row_weight
                .checked_add(counter)
                .ok_or(CountMinCodecError::State(CountMinError::WeightOverflow))?;
        }
        if row_weight != total_weight {
            return Err(CountMinCodecError::RowWeightMismatch {
                row,
                expected: total_weight,
                actual: row_weight,
            });
        }
    }
    Ok(())
}

fn validate_profile(profile: CountMinProfile) -> Result<usize, CountMinError> {
    if profile.width == 0 || profile.depth == 0 {
        return Err(CountMinError::EmptyDimension {
            width: profile.width,
            depth: profile.depth,
        });
    }
    let cell_count = profile
        .width
        .checked_mul(profile.depth)
        .ok_or(CountMinError::CellCountOverflow)?;
    if cell_count > profile.max_cells {
        return Err(CountMinError::CellLimitExceeded {
            requested: cell_count,
            limit: profile.max_cells,
        });
    }
    Ok(cell_count)
}

fn canonical_usize(value: usize) -> Result<u64, CountMinCodecError> {
    u64::try_from(value).map_err(|_| CountMinCodecError::IntegerUnrepresentable)
}

fn decoded_usize(value: u64) -> Result<usize, CountMinCodecError> {
    usize::try_from(value).map_err(|_| CountMinCodecError::IntegerUnrepresentable)
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

struct CountMinDecoder<'bytes> {
    bytes: &'bytes [u8],
    offset: usize,
}

impl<'bytes> CountMinDecoder<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_array<const LENGTH: usize>(&mut self) -> Result<[u8; LENGTH], CountMinCodecError> {
        let end = self
            .offset
            .checked_add(LENGTH)
            .ok_or(CountMinCodecError::LengthOverflow)?;
        let Some(source) = self.bytes.get(self.offset..end) else {
            return Err(CountMinCodecError::Truncated {
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

    fn read_u16(&mut self) -> Result<u16, CountMinCodecError> {
        Ok(u16::from_be_bytes(self.read_array::<2>()?))
    }

    fn read_u64(&mut self) -> Result<u64, CountMinCodecError> {
        Ok(u64::from_be_bytes(self.read_array::<8>()?))
    }

    fn finish(self) -> Result<(), CountMinCodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(CountMinCodecError::TrailingBytes {
                offset: self.offset,
                remaining: self.bytes.len() - self.offset,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_accuracy_fixtures::named_graph_fixtures;

    fn profile() -> CountMinProfile {
        CountMinProfile {
            width: 256,
            depth: 5,
            hash_algorithm: CountMinHashAlgorithm::SeededFnvMix64V1,
            seed: 0x434d_534b_4554_4348,
            max_total_weight: 1_000_000,
            max_cells: 2_000,
        }
    }

    fn sketch() -> CountMinSketch {
        CountMinSketch::try_new(profile()).expect("bounded profile")
    }

    fn read_fixture(bytes: &[u8]) -> Result<CountMinSketch, CountMinCodecError> {
        CountMinSketch::try_from_canonical_bytes(
            bytes,
            profile(),
            CountMinDecodeLimits::conservative(),
        )
    }

    #[test]
    fn construction_enforces_shape_and_cell_ceiling() {
        assert_eq!(
            CountMinSketch::try_new(CountMinProfile::new(0, 3, 1, 10)),
            Err(CountMinError::EmptyDimension { width: 0, depth: 3 })
        );
        assert_eq!(
            CountMinSketch::try_new(CountMinProfile {
                width: usize::MAX,
                depth: 2,
                hash_algorithm: CountMinHashAlgorithm::SeededFnvMix64V1,
                seed: 1,
                max_total_weight: 10,
                max_cells: usize::MAX,
            }),
            Err(CountMinError::CellCountOverflow)
        );
        assert_eq!(
            CountMinSketch::try_new(CountMinProfile {
                width: 8,
                depth: 3,
                hash_algorithm: CountMinHashAlgorithm::SeededFnvMix64V1,
                seed: 1,
                max_total_weight: 10,
                max_cells: 23,
            }),
            Err(CountMinError::CellLimitExceeded {
                requested: 24,
                limit: 23,
            })
        );
    }

    #[test]
    fn estimates_never_understate_observed_weight() {
        let mut sketch = sketch();
        let observations = [
            (&b"person"[..], 11),
            (&b"knows"[..], 7),
            (&b"person"[..], 13),
            (&b"city"[..], 5),
        ];
        for (key, weight) in observations {
            sketch.try_observe(key, weight).expect("bounded update");
        }
        assert!(sketch.estimate(b"person") >= 24);
        assert!(sketch.estimate(b"knows") >= 7);
        assert!(sketch.estimate(b"city") >= 5);
        assert_eq!(sketch.total_weight(), 36);
    }

    #[test]
    fn error_contract_rejects_invalid_profiles_and_matches_exact_vectors() {
        assert_eq!(
            CountMinProfile::new(0, 3, 1, 10).error_contract(),
            Err(CountMinError::EmptyDimension { width: 0, depth: 3 })
        );
        assert_eq!(
            CountMinProfile::new(8, 0, 1, 10).error_contract(),
            Err(CountMinError::EmptyDimension { width: 8, depth: 0 })
        );
        assert_eq!(
            CountMinProfile {
                width: usize::MAX,
                depth: 2,
                hash_algorithm: CountMinHashAlgorithm::SeededFnvMix64V1,
                seed: 1,
                max_total_weight: 10,
                max_cells: usize::MAX,
            }
            .error_contract(),
            Err(CountMinError::CellCountOverflow)
        );
        assert_eq!(
            CountMinProfile {
                width: 8,
                depth: 3,
                hash_algorithm: CountMinHashAlgorithm::SeededFnvMix64V1,
                seed: 1,
                max_total_weight: 10,
                max_cells: 23,
            }
            .error_contract(),
            Err(CountMinError::CellLimitExceeded {
                requested: 24,
                limit: 23,
            })
        );

        let contract = profile()
            .error_contract()
            .expect("the standard test profile is valid");
        assert_eq!(contract.width(), 256);
        assert_eq!(contract.independent_rows(), 5);
        assert_eq!(
            contract.assumption(),
            CountMinAccuracyAssumption::IndependentUniformRowCollisions
        );
        assert_eq!(contract.maximum_overestimate(0), 0);
        assert_eq!(contract.maximum_overestimate(2_046), 22);
        assert_eq!(contract.maximum_overestimate(6_144), 66);
        assert_eq!(contract.confidence_parts_per_million_floor(), 993_261);

        let rounding_contract = CountMinProfile {
            width: 4,
            depth: 1,
            hash_algorithm: CountMinHashAlgorithm::SeededFnvMix64V1,
            seed: 1,
            max_total_weight: 10,
            max_cells: 4,
        }
        .error_contract()
        .expect("4x1 profile is valid");
        assert_eq!(rounding_contract.maximum_overestimate(10), 7);
        assert_eq!(
            rounding_contract.confidence_parts_per_million_floor(),
            632_120
        );

        let deep_contract = CountMinProfile {
            width: 1,
            depth: 15,
            hash_algorithm: CountMinHashAlgorithm::SeededFnvMix64V1,
            seed: 1,
            max_total_weight: u64::MAX,
            max_cells: 15,
        }
        .error_contract()
        .expect("1x15 profile is valid");
        assert_eq!(deep_contract.maximum_overestimate(u64::MAX), u64::MAX);
        assert_eq!(deep_contract.confidence_parts_per_million_floor(), 999_999);
    }

    #[test]
    fn named_graph_frequency_calibration_matches_independent_bounds() {
        const SEEDS: [u64; 3] = [
            0x434d_534b_0000_0001,
            0x434d_534b_0000_0002,
            0x434d_534b_0000_0003,
        ];

        for fixture in named_graph_fixtures() {
            let total_weight = u64::try_from(fixture.edges.len())
                .expect("fixture edge count fits u64")
                .checked_mul(2)
                .expect("fixture endpoint count fits u64");
            let mut exact = vec![0_u64; fixture.node_count];
            for &(left, right) in &fixture.edges {
                exact[left as usize] += 1;
                exact[right as usize] += 1;
            }

            for seed in SEEDS {
                let profile = CountMinProfile {
                    width: 256,
                    depth: 5,
                    hash_algorithm: CountMinHashAlgorithm::SeededFnvMix64V1,
                    seed,
                    max_total_weight: total_weight,
                    max_cells: 256 * 5,
                };
                let contract = profile.error_contract().expect("accuracy profile is valid");
                assert!(
                    matches!(total_weight, 2_046 | 2_048 | 6_144),
                    "fixture {} has unexpected total weight {total_weight}",
                    fixture.name
                );
                let independent_bound = if total_weight == 6_144 { 66 } else { 22 };
                assert_eq!(
                    contract.maximum_overestimate(total_weight),
                    independent_bound
                );
                assert_eq!(contract.confidence_parts_per_million_floor(), 993_261);
                let mut value =
                    CountMinSketch::try_new(profile).expect("accuracy profile is bounded");
                for &(left, right) in &fixture.edges {
                    value
                        .try_observe(&left.to_be_bytes(), 1)
                        .expect("fixture fits declared total weight");
                    value
                        .try_observe(&right.to_be_bytes(), 1)
                        .expect("fixture fits declared total weight");
                }

                assert_eq!(value.total_weight(), total_weight);
                for (node, &truth) in exact.iter().enumerate() {
                    let estimate = value.estimate(&(node as u64).to_be_bytes());
                    assert!(
                        estimate >= truth,
                        "fixture={} seed={seed:#018x} profile={profile:?} exact={truth} \
                         estimate={estimate} bound={independent_bound}: Count-Min understated \
                         frequency",
                        fixture.name
                    );
                    let overestimate = estimate - truth;
                    assert!(
                        overestimate <= independent_bound,
                        "fixture={} seed={seed:#018x} profile={profile:?} exact={truth} \
                         estimate={estimate} error={overestimate} bound={independent_bound} \
                         confidence_ppm={} assumption={:?}",
                        fixture.name,
                        contract.confidence_parts_per_million_floor(),
                        contract.assumption()
                    );
                }
            }
        }
    }

    #[test]
    fn observation_order_has_identical_canonical_state() {
        let observations = [
            (&b"alpha"[..], 3),
            (&b"beta"[..], 5),
            (&b"gamma"[..], 7),
            (&b"alpha"[..], 11),
        ];
        let mut forward = sketch();
        let mut reverse = sketch();
        for &(key, weight) in &observations {
            forward.try_observe(key, weight).expect("bounded update");
        }
        for &(key, weight) in observations.iter().rev() {
            reverse.try_observe(key, weight).expect("bounded update");
        }
        assert_eq!(forward.canonical_state(), reverse.canonical_state());
    }

    #[test]
    fn canonical_codec_round_trips_and_collapses_observation_order() {
        let observations = [
            (&b"alpha"[..], 3),
            (&b"beta"[..], 5),
            (&b"gamma"[..], 7),
            (&b"alpha"[..], 11),
        ];
        let mut forward = sketch();
        let mut reverse = sketch();
        for &(key, weight) in &observations {
            forward.try_observe(key, weight).expect("bounded update");
        }
        for &(key, weight) in observations.iter().rev() {
            reverse.try_observe(key, weight).expect("bounded update");
        }

        let forward_bytes = forward
            .try_to_canonical_bytes()
            .expect("valid state encodes");
        let reverse_bytes = reverse
            .try_to_canonical_bytes()
            .expect("valid state encodes");
        assert_eq!(forward_bytes, reverse_bytes);
        assert_eq!(&forward_bytes[..8], b"FGDBCMS1");
        assert_eq!(&forward_bytes[8..10], &1_u16.to_be_bytes());

        let decoded = read_fixture(&forward_bytes).expect("canonical value");
        assert_eq!(decoded, forward);
        assert_eq!(
            decoded
                .try_to_canonical_bytes()
                .expect("decoded state re-encodes"),
            forward_bytes
        );
    }

    #[test]
    fn hash_profile_and_complete_canonical_value_have_frozen_vectors() {
        use core::fmt::Write as _;

        let profile = CountMinProfile {
            width: 2,
            depth: 2,
            hash_algorithm: CountMinHashAlgorithm::SeededFnvMix64V1,
            seed: 7,
            max_total_weight: 100,
            max_cells: 4,
        };
        let mut value = CountMinSketch::try_new(profile).expect("bounded profile");
        assert_eq!(value.counter_index(0, b"edge"), 0);
        assert_eq!(value.counter_index(1, b"edge"), 2);
        value.try_observe(b"edge", 3).expect("bounded update");

        let encoded = value.try_to_canonical_bytes().expect("canonical state");
        let mut actual_hex = String::with_capacity(encoded.len() * 2);
        for byte in encoded {
            write!(&mut actual_hex, "{byte:02x}").expect("writing to String cannot fail");
        }
        assert_eq!(
            actual_hex,
            "46474442434d53310001000000000000000200000000000000020001000000000000000700000000000000640000000000000004000000000000000300000000000000040000000000000003000000000000000000000000000000030000000000000000"
        );
    }

    #[test]
    fn canonical_decoder_rejects_malformed_state_without_allocation_ambiguity() {
        let mut value = sketch();
        value.try_observe(b"edge", 9).expect("bounded update");
        let encoded = value.try_to_canonical_bytes().expect("valid state encodes");

        let mut wrong_magic = encoded.clone();
        wrong_magic[0] ^= 1;
        assert!(matches!(
            read_fixture(&wrong_magic),
            Err(CountMinCodecError::MagicMismatch { .. })
        ));

        let mut wrong_version = encoded.clone();
        wrong_version[8..10].copy_from_slice(&2_u16.to_be_bytes());
        assert_eq!(
            read_fixture(&wrong_version),
            Err(CountMinCodecError::UnsupportedVersion { actual: 2 })
        );

        let mut wrong_hash_algorithm = encoded.clone();
        wrong_hash_algorithm[26..28].copy_from_slice(&2_u16.to_be_bytes());
        assert_eq!(
            read_fixture(&wrong_hash_algorithm),
            Err(CountMinCodecError::UnsupportedHashAlgorithm { actual: 2 })
        );

        let mut wrong_count = encoded.clone();
        wrong_count[60..68].copy_from_slice(&1_u64.to_be_bytes());
        assert!(matches!(
            read_fixture(&wrong_count),
            Err(CountMinCodecError::CounterCountMismatch { .. })
        ));

        let mut wrong_row_sum = encoded.clone();
        wrong_row_sum[73] ^= 1;
        assert!(matches!(
            read_fixture(&wrong_row_sum),
            Err(CountMinCodecError::RowWeightMismatch { .. })
                | Err(CountMinCodecError::CounterExceedsTotal { .. })
        ));

        assert!(matches!(
            read_fixture(&encoded[..encoded.len() - 1]),
            Err(CountMinCodecError::Truncated { .. })
        ));

        let mut trailing = encoded;
        trailing.push(0);
        assert!(matches!(
            read_fixture(&trailing),
            Err(CountMinCodecError::TrailingBytes { remaining: 1, .. })
        ));
    }

    #[test]
    fn canonical_decoder_enforces_trusted_profile_and_resource_bounds() {
        let encoded = sketch()
            .try_to_canonical_bytes()
            .expect("valid state encodes");

        let mut different_profile = profile();
        different_profile.seed ^= 1;
        assert!(matches!(
            CountMinSketch::try_from_canonical_bytes(
                &encoded,
                different_profile,
                CountMinDecodeLimits::conservative(),
            ),
            Err(CountMinCodecError::ProfileMismatch { .. })
        ));

        let mut limits = CountMinDecodeLimits::conservative();
        limits.max_depth = profile().depth - 1;
        assert_eq!(
            CountMinSketch::try_from_canonical_bytes(&encoded, profile(), limits),
            Err(CountMinCodecError::DepthLimitExceeded {
                actual: profile().depth,
                maximum: profile().depth - 1,
            })
        );

        let mut limits = CountMinDecodeLimits::conservative();
        limits.max_encoded_bytes = encoded.len() - 1;
        assert_eq!(
            CountMinSketch::try_from_canonical_bytes(&encoded, profile(), limits),
            Err(CountMinCodecError::EncodedByteLimitExceeded {
                actual: encoded.len(),
                maximum: encoded.len() - 1,
            })
        );
    }

    #[test]
    fn merge_is_commutative_and_associative_for_identical_profiles() {
        fn part(entries: &[(&[u8], u64)]) -> CountMinSketch {
            let mut value = sketch();
            for &(key, weight) in entries {
                value.try_observe(key, weight).expect("bounded update");
            }
            value
        }

        let a = part(&[(b"a", 2), (b"d", 3)]);
        let b = part(&[(b"b", 5), (b"a", 7)]);
        let c = part(&[(b"c", 11), (b"d", 13)]);

        let mut left = a.clone();
        left.try_merge(&b).expect("matching profile");
        let mut right = b.clone();
        right.try_merge(&a).expect("matching profile");
        assert_eq!(left, right);

        let mut ab_c = a.clone();
        ab_c.try_merge(&b).expect("matching profile");
        ab_c.try_merge(&c).expect("matching profile");
        let mut bc = b;
        bc.try_merge(&c).expect("matching profile");
        let mut a_bc = a;
        a_bc.try_merge(&bc).expect("matching profile");
        assert_eq!(ab_c, a_bc);
    }

    #[test]
    fn deletion_and_profile_mismatch_leave_state_unchanged() {
        let mut value = sketch();
        value.try_observe(b"edge", 9).expect("bounded update");
        let before = value.clone();
        assert_eq!(
            value.try_remove(b"edge", 4),
            Err(CountMinError::RebuildRequired {
                requested_weight: 4,
            })
        );
        assert_eq!(value, before);

        let mut other_profile = profile();
        other_profile.seed ^= 1;
        let other = CountMinSketch::try_new(other_profile).expect("bounded profile");
        assert_eq!(value.try_merge(&other), Err(CountMinError::ProfileMismatch));
        assert_eq!(value, before);
    }

    #[test]
    fn overflow_is_atomic() {
        let mut bounded = CountMinSketch::try_new(CountMinProfile {
            max_total_weight: 10,
            ..profile()
        })
        .expect("bounded profile");
        bounded.try_observe(b"x", 9).expect("within total bound");
        let before = bounded.clone();
        assert_eq!(
            bounded.try_observe(b"x", 2),
            Err(CountMinError::WeightOverflow)
        );
        assert_eq!(bounded, before);
    }
}
