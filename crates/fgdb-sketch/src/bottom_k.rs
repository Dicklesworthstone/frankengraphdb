//! Deterministic bottom-k sampling over canonical byte observations.
//!
//! This sketch has distinct-set semantics: observing the same byte string more
//! than once is idempotent. Samples are ordered by `(stable_hash, bytes)`, so a
//! hash collision cannot make the logical state ambiguous. The retained vector
//! is always sorted, deduplicated, and no longer than the profile's `k`.
//!
//! Deletion is deliberately conservative. While the sample is not saturated,
//! it contains the complete distinct universe and a retained observation can be
//! removed exactly. Once saturated, removing a retained observation would
//! require the unknown `(k + 1)`th candidate and therefore returns
//! [`BottomKError::RebuildRequired`] without changing state. Removing an
//! unretained observation is always an exact no-op: in a saturated sketch it
//! cannot affect the current bottom-k, and in an unsaturated sketch its absence
//! proves it was never part of the represented distinct set.

#![forbid(unsafe_code)]

use core::cmp::Ordering;
use core::fmt;
use core::hash::Hasher;
use fgdb_collections::hash_table::SeededHasher;

const HASH_DOMAIN: &[u8] = b"fgdb:bottom-k:observation:v1";
const CANONICAL_MAGIC: [u8; 8] = *b"FGDBBTK1";
const CANONICAL_VERSION: u16 = 1;
const CANONICAL_HEADER_BYTES: usize = 8 + 2 + 1 + (6 * 8);
const CANONICAL_SAMPLE_HEADER_BYTES: usize = 2 * 8;

/// Stable hash algorithm named by a bottom-k profile.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum BottomKHashAlgorithm {
    /// Domain-separated [`SeededHasher`] stream frozen by canonical vectors.
    SeededHasherV1 = 1,
}

impl BottomKHashAlgorithm {
    const fn canonical_tag(self) -> u8 {
        self as u8
    }
}

/// Complete behavior, hash, and resource profile for a bottom-k sketch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BottomKProfile {
    /// Maximum number of distinct samples retained.
    pub k: usize,
    /// Stable hash algorithm included in profile identity.
    pub hash_algorithm: BottomKHashAlgorithm,
    /// Explicit deterministic seed for the stable hash.
    pub seed: u64,
    /// Maximum accepted length of one canonical observation.
    pub max_observation_bytes: usize,
    /// Maximum sum of observation bytes retained in the sample.
    pub max_sample_bytes: usize,
}

impl BottomKProfile {
    /// Creates a complete profile.
    #[must_use]
    pub const fn new(
        k: usize,
        seed: u64,
        max_observation_bytes: usize,
        max_sample_bytes: usize,
    ) -> Self {
        Self {
            k,
            hash_algorithm: BottomKHashAlgorithm::SeededHasherV1,
            seed,
            max_observation_bytes,
            max_sample_bytes,
        }
    }

    /// Returns the idealized continuous-rank error contract when `k >= 3`.
    ///
    /// This contract deliberately does not claim that a deterministic 64-bit
    /// hash has continuous independent ranks. It is a calibration model for
    /// callers that explicitly accept that idealization, not a finite-domain
    /// guarantee for [`BottomKSketch::try_estimate_distinct_count`]. An
    /// unsaturated sketch remains exact without any statistical model.
    #[must_use]
    pub fn idealized_distinct_count_error_contract(
        self,
    ) -> Option<BottomKIdealizedDistinctCountErrorContract> {
        let variance_denominator = self.k.checked_sub(2)?.isqrt();
        if variance_denominator == 0 {
            return None;
        }
        Some(BottomKIdealizedDistinctCountErrorContract {
            sample_size: self.k,
            deviation_multiplier: 4,
            variance_denominator,
            model: BottomKDistinctCountModel::IdealizedIndependentUniformContinuousRanks,
        })
    }
}

/// Explicit model under which a bottom-k probability statement is interpreted.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BottomKDistinctCountModel {
    /// Ranks are independent draws from the continuous uniform distribution.
    ///
    /// This idealization has zero collision probability. It is not a claim
    /// about the stable finite hash algorithm and excludes 64-bit quantization,
    /// finite-population correction, and adversarially selected observations.
    IdealizedIndependentUniformContinuousRanks,
}

/// Profile-derived error contract for an idealized bottom-k model.
///
/// For the unbiased continuous-rank estimator `(k - 1) / U[k]`, relative
/// variance is no greater than `1 / (k - 2)`. This contract applies
/// Chebyshev's inequality at four standard deviations, yielding confidence at
/// least `15/16`. The integer denominator is rounded down, which rounds the
/// permitted error outward.
///
/// This type does not bound the crate's finite 64-bit estimator. Doing so would
/// additionally require a finite-domain proof and an explicit collision term.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BottomKIdealizedDistinctCountErrorContract {
    /// Retained sample count in the governing profile.
    sample_size: usize,
    /// Number of modeled standard deviations admitted by the contract.
    deviation_multiplier: u64,
    /// `floor(sqrt(k - 2))`, used as the conservative relative-error divisor.
    variance_denominator: usize,
    /// Named statistical model for the probability statement.
    model: BottomKDistinctCountModel,
}

impl BottomKIdealizedDistinctCountErrorContract {
    /// Retained sample count in the governing profile.
    #[must_use]
    pub const fn sample_size(self) -> usize {
        self.sample_size
    }

    /// Number of modeled standard deviations admitted by the contract.
    #[must_use]
    pub const fn deviation_multiplier(self) -> u64 {
        self.deviation_multiplier
    }

    /// Conservative `floor(sqrt(k - 2))` relative-error divisor.
    #[must_use]
    pub const fn variance_denominator(self) -> usize {
        self.variance_denominator
    }

    /// Named statistical model for the probability statement.
    #[must_use]
    pub const fn model(self) -> BottomKDistinctCountModel {
        self.model
    }

    /// Conservative absolute-error ceiling for an exact population size.
    #[must_use]
    pub fn maximum_absolute_error(self, exact_distinct_count: u64) -> u64 {
        let numerator = u128::from(exact_distinct_count) * u128::from(self.deviation_multiplier);
        let denominator = self.variance_denominator as u128;
        u64::try_from(numerator.div_ceil(denominator)).unwrap_or(u64::MAX)
    }

    /// Conservative relative-error ceiling in parts per million.
    #[must_use]
    pub fn relative_error_parts_per_million_ceiling(self) -> u64 {
        let numerator = u128::from(self.deviation_multiplier) * 1_000_000_u128;
        let denominator = self.variance_denominator as u128;
        u64::try_from(numerator.div_ceil(denominator)).unwrap_or(u64::MAX)
    }

    /// Confidence floor in parts per million (`15/16 = 937,500 ppm`).
    #[must_use]
    pub const fn confidence_parts_per_million_floor(self) -> u64 {
        937_500
    }
}

/// Caller-owned admission bounds for decoding one bottom-k value.
///
/// These bounds are independent of the profile embedded in untrusted bytes.
/// In particular, `max_samples` caps the profile's `k` even when the encoded
/// sample is empty.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BottomKDecodeLimits {
    /// Maximum accepted canonical value length.
    pub max_encoded_bytes: usize,
    /// Maximum accepted profile `k` and retained sample count.
    pub max_samples: usize,
    /// Maximum accepted per-observation profile ceiling and payload length.
    pub max_observation_bytes: usize,
    /// Maximum accepted profile and retained sample byte total.
    pub max_sample_bytes: usize,
}

impl BottomKDecodeLimits {
    /// Creates explicit caller-owned decode admission bounds.
    #[must_use]
    pub const fn new(
        max_encoded_bytes: usize,
        max_samples: usize,
        max_observation_bytes: usize,
        max_sample_bytes: usize,
    ) -> Self {
        Self {
            max_encoded_bytes,
            max_samples,
            max_observation_bytes,
            max_sample_bytes,
        }
    }
}

/// Allocation owned by a failed bottom-k transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BottomKAllocationTarget {
    /// Sorted sample directory.
    SampleDirectory,
    /// Bytes for one retained observation.
    ObservationBytes,
    /// Temporary sorted directory for an atomic merge.
    MergeDirectory,
    /// Observation bytes cloned into an atomic merge result.
    MergeObservationBytes,
}

/// Typed construction or transition failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BottomKError {
    /// A zero-sized sample has no useful bottom-k semantics.
    EmptySampleSize,
    /// One observation exceeds the complete profile's byte ceiling.
    ObservationTooLarge {
        /// Rejected observation length.
        bytes: usize,
        /// Profile ceiling.
        maximum: usize,
    },
    /// Retaining a candidate would exceed the sample payload ceiling.
    SampleByteLimitExceeded {
        /// Payload bytes the transition would retain.
        requested: usize,
        /// Profile ceiling.
        maximum: usize,
    },
    /// Checked payload-size arithmetic overflowed.
    SampleByteCountOverflow,
    /// Checked sample-directory size arithmetic overflowed.
    SampleCountOverflow,
    /// The allocator rejected a checked reservation.
    AllocationFailed {
        /// Component whose reservation failed.
        target: BottomKAllocationTarget,
        /// Exact entries or bytes requested.
        requested: usize,
    },
    /// Merge operands do not use the identical complete profile.
    ProfileMismatch,
    /// Exact deletion requires candidates that bottom-k does not retain.
    RebuildRequired {
        /// Stable hash of the retained observation whose removal was refused.
        observation_hash: u64,
    },
}

/// Typed failure to derive a bottom-k distinct-count estimate.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BottomKDistinctEstimateError {
    /// A saturated order-statistic estimate requires at least two ranks.
    InsufficientSampleSize {
        /// Retained sample count.
        retained: usize,
        /// Smallest supported saturated sample count.
        minimum: usize,
    },
    /// The platform-sized sample count cannot enter the canonical estimator.
    SampleSizeUnrepresentable {
        /// Profile sample count.
        sample_size: usize,
    },
}

impl fmt::Display for BottomKDistinctEstimateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for BottomKDistinctEstimateError {}

impl fmt::Display for BottomKError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EmptySampleSize => formatter.write_str("bottom-k sample size must be nonzero"),
            Self::ObservationTooLarge { bytes, maximum } => write!(
                formatter,
                "bottom-k observation has {bytes} bytes, maximum is {maximum}"
            ),
            Self::SampleByteLimitExceeded { requested, maximum } => write!(
                formatter,
                "bottom-k sample would retain {requested} observation bytes, maximum is {maximum}"
            ),
            Self::SampleByteCountOverflow => {
                formatter.write_str("bottom-k sample byte count overflowed")
            }
            Self::SampleCountOverflow => formatter.write_str("bottom-k sample count overflowed"),
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "could not reserve {requested} units for bottom-k {target:?}"
            ),
            Self::ProfileMismatch => {
                formatter.write_str("cannot merge bottom-k sketches with different profiles")
            }
            Self::RebuildRequired { observation_hash } => write!(
                formatter,
                "removing retained bottom-k observation {observation_hash:#018x} requires rebuild"
            ),
        }
    }
}

impl std::error::Error for BottomKError {}

/// Caller-owned resource checked while admitting canonical bottom-k bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BottomKDecodeResource {
    /// Total canonical input bytes.
    EncodedBytes,
    /// Profile `k` or retained directory entries.
    Samples,
    /// Per-observation profile ceiling or one retained payload.
    ObservationBytes,
    /// Profile or retained aggregate payload bytes.
    SampleBytes,
}

/// Strict canonical-codec failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BottomKCodecError {
    /// The in-memory or decoded sketch violates a construction invariant.
    State(BottomKError),
    /// A platform-sized profile or state field cannot be represented canonically.
    IntegerUnrepresentable,
    /// Computing the exact canonical value length overflowed.
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
    /// The encoded hash-algorithm discriminator is not implemented.
    UnsupportedHashAlgorithm {
        /// Discriminator found in the canonical profile.
        actual: u8,
    },
    /// The encoded complete profile does not equal the trusted expected profile.
    ProfileMismatch {
        /// Trusted profile supplied by the caller.
        expected: BottomKProfile,
        /// Profile decoded from the canonical header.
        actual: BottomKProfile,
    },
    /// A caller-owned decode admission bound was exceeded.
    DecodeLimitExceeded {
        /// Resource whose admission bound was exceeded.
        resource: BottomKDecodeResource,
        /// Encoded profile, state, or input value.
        actual: usize,
        /// Caller-owned maximum.
        maximum: usize,
    },
    /// Input ended before a complete field or payload could be read.
    Truncated {
        /// Byte offset of the field or payload.
        offset: usize,
        /// Bytes needed at that offset.
        needed: usize,
        /// Bytes remaining at the offset.
        remaining: usize,
    },
    /// The retained sample count exceeds the encoded profile's `k`.
    SampleCountExceedsProfile {
        /// Encoded retained sample count.
        actual: usize,
        /// Encoded profile ceiling.
        maximum: usize,
    },
    /// The encoded or in-memory byte total disagrees with the sample payloads.
    SampleByteCountMismatch {
        /// Declared byte total.
        declared: usize,
        /// Sum of retained observation lengths.
        actual: usize,
    },
    /// A retained hash is not the profile hash of its observation.
    HashMismatch {
        /// Zero-based sample index.
        index: usize,
        /// Hash stored in the canonical state.
        actual: u64,
        /// Hash derived from the complete profile and observation.
        expected: u64,
    },
    /// A retained sample repeats its predecessor.
    DuplicateSample {
        /// Zero-based index of the repeated sample.
        index: usize,
    },
    /// Retained samples are not strictly increasing by `(hash, bytes)`.
    SamplesOutOfOrder {
        /// Zero-based index of the first out-of-order sample.
        index: usize,
    },
    /// Input contains bytes after the one canonical value.
    TrailingBytes {
        /// First trailing byte.
        offset: usize,
        /// Number of trailing bytes.
        remaining: usize,
    },
}

impl fmt::Display for BottomKCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for BottomKCodecError {}

impl From<BottomKError> for BottomKCodecError {
    fn from(error: BottomKError) -> Self {
        Self::State(error)
    }
}

/// One canonical retained sample.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BottomKSample {
    hash: u64,
    observation: Vec<u8>,
}

impl BottomKSample {
    /// Seeded stable hash used for sample ordering.
    #[must_use]
    pub const fn hash(&self) -> u64 {
        self.hash
    }

    /// Canonical observation bytes used to break hash collisions.
    #[must_use]
    pub fn observation(&self) -> &[u8] {
        &self.observation
    }
}

impl Ord for BottomKSample {
    fn cmp(&self, other: &Self) -> Ordering {
        self.hash
            .cmp(&other.hash)
            .then_with(|| self.observation.cmp(&other.observation))
    }
}

impl PartialOrd for BottomKSample {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Borrowed canonical logical state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BottomKState<'sketch> {
    /// Complete behavior and resource profile.
    pub profile: BottomKProfile,
    /// Sum of retained canonical observation lengths.
    pub sample_bytes: usize,
    /// Samples sorted by `(hash, observation bytes)` with no duplicates.
    pub samples: &'sketch [BottomKSample],
}

/// Estimator branch selected from bottom-k saturation state.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BottomKDistinctEstimateMethod {
    /// Fewer than `k` samples means the complete distinct population is held.
    ExactUnsaturated,
    /// A saturated sample uses a finite 64-bit kth-order-statistic approximation.
    FiniteHash64KthOrderStatistic,
}

/// Deterministic integer distinct-count estimate from a bottom-k sample.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BottomKDistinctEstimate {
    /// Estimated distinct population: exact while unsaturated, otherwise
    /// rounded to nearest and clamped to at least `k`.
    pub value: u64,
    /// Estimator branch selected by saturation state.
    pub method: BottomKDistinctEstimateMethod,
    /// Largest retained hash for the order-statistic branch.
    pub threshold_hash: Option<u64>,
}

/// Mergeable deterministic distinct bottom-k sample.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BottomKSketch {
    profile: BottomKProfile,
    samples: Vec<BottomKSample>,
    sample_bytes: usize,
}

impl BottomKSketch {
    /// Creates an empty sketch without allocating its potentially large directory.
    pub fn try_new(profile: BottomKProfile) -> Result<Self, BottomKError> {
        if profile.k == 0 {
            return Err(BottomKError::EmptySampleSize);
        }
        Ok(Self {
            profile,
            samples: Vec::new(),
            sample_bytes: 0,
        })
    }

    /// Complete immutable profile.
    #[must_use]
    pub const fn profile(&self) -> BottomKProfile {
        self.profile
    }

    /// Number of retained distinct samples.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether no observation is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Whether the sample has reached `k` and may hide larger candidates.
    #[must_use]
    pub fn is_saturated(&self) -> bool {
        self.samples.len() == self.profile.k
    }

    /// Sum of retained canonical observation lengths.
    #[must_use]
    pub const fn sample_bytes(&self) -> usize {
        self.sample_bytes
    }

    /// Canonical sorted and deduplicated logical state.
    #[must_use]
    pub fn canonical_state(&self) -> BottomKState<'_> {
        BottomKState {
            profile: self.profile,
            sample_bytes: self.sample_bytes,
            samples: &self.samples,
        }
    }

    /// Encodes the complete profile and logical state into one canonical value.
    ///
    /// The representation uses fixed-width big-endian profile and state fields,
    /// followed by strictly sorted `(hash, observation length, observation)`
    /// records. Equal logical states therefore produce byte-identical values
    /// without relying on host word size.
    pub fn try_to_canonical_bytes(&self) -> Result<Vec<u8>, BottomKCodecError> {
        let layout = validate_canonical_state(self.profile, self.sample_bytes, &self.samples)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(layout.encoded_len).map_err(|_| {
            BottomKCodecError::AllocationFailed {
                requested: layout.encoded_len,
            }
        })?;
        bytes.extend_from_slice(&CANONICAL_MAGIC);
        push_u16(&mut bytes, CANONICAL_VERSION);
        bytes.push(self.profile.hash_algorithm.canonical_tag());
        push_u64(&mut bytes, layout.k);
        push_u64(&mut bytes, self.profile.seed);
        push_u64(&mut bytes, layout.max_observation_bytes);
        push_u64(&mut bytes, layout.max_sample_bytes);
        push_u64(&mut bytes, layout.sample_bytes);
        push_u64(&mut bytes, layout.sample_count);
        for sample in &self.samples {
            push_u64(&mut bytes, sample.hash);
            push_u64(&mut bytes, canonical_usize(sample.observation.len())?);
            bytes.extend_from_slice(&sample.observation);
        }
        debug_assert_eq!(bytes.len(), layout.encoded_len);
        Ok(bytes)
    }

    /// Decodes exactly one strict canonical value and revalidates every law.
    ///
    /// The complete header, exact encoded length, profile bounds, hashes,
    /// strict ordering, distinctness, and payload totals are validated in an
    /// allocation-free preflight. Only then are the bounded sample directory
    /// and observation payloads allocated.
    pub fn try_from_canonical_bytes(
        bytes: &[u8],
        expected_profile: BottomKProfile,
        limits: BottomKDecodeLimits,
    ) -> Result<Self, BottomKCodecError> {
        let header = preflight_canonical_bytes(bytes, expected_profile, limits)?;

        let mut samples = Vec::new();
        reserve_samples(
            &mut samples,
            header.sample_count,
            BottomKAllocationTarget::SampleDirectory,
        )?;
        let mut decoder = BottomKDecoder {
            bytes,
            offset: CANONICAL_HEADER_BYTES,
        };
        for _ in 0..header.sample_count {
            let hash = decoder.read_u64()?;
            let observation_bytes = decoded_usize(decoder.read_u64()?)?;
            let observation = decoder.take(observation_bytes)?;
            samples.push(try_clone_sample(
                hash,
                observation,
                BottomKAllocationTarget::ObservationBytes,
            )?);
        }
        decoder.finish()?;

        let sketch = Self {
            profile: header.profile,
            samples,
            sample_bytes: header.sample_bytes,
        };
        validate_canonical_state(sketch.profile, sketch.sample_bytes, &sketch.samples)?;
        Ok(sketch)
    }

    /// Largest retained sample, or `None` while empty.
    #[must_use]
    pub fn threshold(&self) -> Option<&BottomKSample> {
        self.samples.last()
    }

    /// Estimates the distinct observation population.
    ///
    /// Unsaturated state is exact. Saturated state with at least two samples
    /// uses the finite-rank approximation
    /// `(k - 1) * 2^64 / (threshold_hash + 1)`, rounded to nearest. The `+1`
    /// maps the finite unsigned hash range to `(0, 1]` and avoids a zero
    /// denominator.
    ///
    /// Unlike the analogous continuous-rank estimator, this discrete estimator
    /// is not claimed to be exactly unbiased. The profile's
    /// [`BottomKProfile::idealized_distinct_count_error_contract`] is an
    /// explicitly idealized calibration model, not a finite-domain guarantee.
    pub fn try_estimate_distinct_count(
        &self,
    ) -> Result<BottomKDistinctEstimate, BottomKDistinctEstimateError> {
        let retained = u64::try_from(self.samples.len()).map_err(|_| {
            BottomKDistinctEstimateError::SampleSizeUnrepresentable {
                sample_size: self.samples.len(),
            }
        })?;
        if !self.is_saturated() {
            return Ok(BottomKDistinctEstimate {
                value: retained,
                method: BottomKDistinctEstimateMethod::ExactUnsaturated,
                threshold_hash: None,
            });
        }
        if self.samples.len() < 2 {
            return Err(BottomKDistinctEstimateError::InsufficientSampleSize {
                retained: self.samples.len(),
                minimum: 2,
            });
        }

        let threshold_hash = self
            .threshold()
            .ok_or(BottomKDistinctEstimateError::InsufficientSampleSize {
                retained: self.samples.len(),
                minimum: 2,
            })?
            .hash();
        let value = finite_hash64_distinct_estimate(retained, threshold_hash);
        Ok(BottomKDistinctEstimate {
            value,
            method: BottomKDistinctEstimateMethod::FiniteHash64KthOrderStatistic,
            threshold_hash: Some(threshold_hash),
        })
    }

    /// Computes this profile's stable hash for a bounded observation.
    pub fn try_hash(&self, observation: &[u8]) -> Result<u64, BottomKError> {
        self.validate_observation(observation)?;
        stable_hash(self.profile.hash_algorithm, self.profile.seed, observation)
    }

    /// Returns whether the canonical observation is currently retained.
    pub fn try_contains(&self, observation: &[u8]) -> Result<bool, BottomKError> {
        let hash = self.try_hash(observation)?;
        Ok(self.find(hash, observation).is_ok())
    }

    /// Observes one canonical byte string with distinct-set semantics.
    ///
    /// Duplicate observations and candidates above a saturated threshold are
    /// exact no-ops. Every limit and allocation needed for a retained candidate
    /// is checked before the state changes.
    pub fn try_observe(&mut self, observation: &[u8]) -> Result<(), BottomKError> {
        self.validate_observation(observation)?;
        let hash = stable_hash(self.profile.hash_algorithm, self.profile.seed, observation)?;
        let insertion_index = match self.find(hash, observation) {
            Ok(_) => return Ok(()),
            Err(index) => index,
        };

        if self.is_saturated() && insertion_index == self.profile.k {
            return Ok(());
        }

        let evicted_bytes = if self.is_saturated() {
            self.samples
                .last()
                .map_or(0, |sample| sample.observation.len())
        } else {
            0
        };
        let next_sample_bytes = self
            .sample_bytes
            .checked_sub(evicted_bytes)
            .and_then(|bytes| bytes.checked_add(observation.len()))
            .ok_or(BottomKError::SampleByteCountOverflow)?;
        if next_sample_bytes > self.profile.max_sample_bytes {
            return Err(BottomKError::SampleByteLimitExceeded {
                requested: next_sample_bytes,
                maximum: self.profile.max_sample_bytes,
            });
        }

        if !self.is_saturated() && self.samples.len() == self.samples.capacity() {
            self.samples
                .try_reserve_exact(1)
                .map_err(|_| BottomKError::AllocationFailed {
                    target: BottomKAllocationTarget::SampleDirectory,
                    requested: 1,
                })?;
        }
        let sample =
            try_clone_sample(hash, observation, BottomKAllocationTarget::ObservationBytes)?;

        if self.is_saturated() {
            self.samples.pop();
        }
        self.samples.insert(insertion_index, sample);
        self.sample_bytes = next_sample_bytes;
        Ok(())
    }

    /// Merges the exact bottom-k union of an identical-profile sketch.
    ///
    /// The complete result is built in private fallible storage before the
    /// receiver changes, making profile, resource, and allocation failures
    /// atomic. The algebra is commutative, associative, and idempotent over
    /// successful identical-profile merges.
    pub fn try_merge(&mut self, other: &Self) -> Result<(), BottomKError> {
        if self.profile != other.profile {
            return Err(BottomKError::ProfileMismatch);
        }

        let mut merged = Vec::new();
        let merged_capacity = self
            .samples
            .len()
            .checked_add(other.samples.len())
            .ok_or(BottomKError::SampleCountOverflow)?
            .min(self.profile.k);
        reserve_samples(
            &mut merged,
            merged_capacity,
            BottomKAllocationTarget::MergeDirectory,
        )?;
        let mut merged_bytes = 0_usize;
        let mut left = 0_usize;
        let mut right = 0_usize;

        while merged.len() < self.profile.k
            && (left < self.samples.len() || right < other.samples.len())
        {
            let selected = match (self.samples.get(left), other.samples.get(right)) {
                (Some(left_sample), Some(right_sample)) => match left_sample.cmp(right_sample) {
                    Ordering::Less => {
                        left += 1;
                        left_sample
                    }
                    Ordering::Greater => {
                        right += 1;
                        right_sample
                    }
                    Ordering::Equal => {
                        left += 1;
                        right += 1;
                        left_sample
                    }
                },
                (Some(left_sample), None) => {
                    left += 1;
                    left_sample
                }
                (None, Some(right_sample)) => {
                    right += 1;
                    right_sample
                }
                (None, None) => break,
            };

            let next_bytes = merged_bytes
                .checked_add(selected.observation.len())
                .ok_or(BottomKError::SampleByteCountOverflow)?;
            if next_bytes > self.profile.max_sample_bytes {
                return Err(BottomKError::SampleByteLimitExceeded {
                    requested: next_bytes,
                    maximum: self.profile.max_sample_bytes,
                });
            }
            let cloned = try_clone_sample(
                selected.hash,
                &selected.observation,
                BottomKAllocationTarget::MergeObservationBytes,
            )?;
            merged.push(cloned);
            merged_bytes = next_bytes;
        }

        self.samples = merged;
        self.sample_bytes = merged_bytes;
        Ok(())
    }

    /// Removes one distinct observation when bottom-k has enough information.
    ///
    /// See the module-level deletion contract. A rebuild-required result never
    /// mutates the sketch.
    pub fn try_remove(&mut self, observation: &[u8]) -> Result<(), BottomKError> {
        self.validate_observation(observation)?;
        let hash = stable_hash(self.profile.hash_algorithm, self.profile.seed, observation)?;
        let Ok(index) = self.find(hash, observation) else {
            return Ok(());
        };
        if self.is_saturated() {
            return Err(BottomKError::RebuildRequired {
                observation_hash: hash,
            });
        }

        let removed_bytes = self.samples[index].observation.len();
        self.samples.remove(index);
        self.sample_bytes -= removed_bytes;
        Ok(())
    }

    fn validate_observation(&self, observation: &[u8]) -> Result<(), BottomKError> {
        if observation.len() > self.profile.max_observation_bytes {
            return Err(BottomKError::ObservationTooLarge {
                bytes: observation.len(),
                maximum: self.profile.max_observation_bytes,
            });
        }
        Ok(())
    }

    fn find(&self, hash: u64, observation: &[u8]) -> Result<usize, usize> {
        self.samples.binary_search_by(|sample| {
            sample
                .hash
                .cmp(&hash)
                .then_with(|| sample.observation.as_slice().cmp(observation))
        })
    }
}

struct CanonicalBottomKLayout {
    k: u64,
    max_observation_bytes: u64,
    max_sample_bytes: u64,
    sample_bytes: u64,
    sample_count: u64,
    encoded_len: usize,
}

struct DecodedBottomKHeader {
    profile: BottomKProfile,
    sample_bytes: usize,
    sample_count: usize,
}

fn validate_canonical_state(
    profile: BottomKProfile,
    sample_bytes: usize,
    samples: &[BottomKSample],
) -> Result<CanonicalBottomKLayout, BottomKCodecError> {
    if profile.k == 0 {
        return Err(BottomKError::EmptySampleSize.into());
    }
    if samples.len() > profile.k {
        return Err(BottomKCodecError::SampleCountExceedsProfile {
            actual: samples.len(),
            maximum: profile.k,
        });
    }
    if sample_bytes > profile.max_sample_bytes {
        return Err(BottomKError::SampleByteLimitExceeded {
            requested: sample_bytes,
            maximum: profile.max_sample_bytes,
        }
        .into());
    }

    let mut actual_sample_bytes = 0_usize;
    let mut encoded_len = expected_canonical_len(samples.len(), 0)?;
    let mut previous: Option<(u64, &[u8])> = None;
    for (index, sample) in samples.iter().enumerate() {
        let observation = sample.observation.as_slice();
        if observation.len() > profile.max_observation_bytes {
            return Err(BottomKError::ObservationTooLarge {
                bytes: observation.len(),
                maximum: profile.max_observation_bytes,
            }
            .into());
        }
        actual_sample_bytes = actual_sample_bytes
            .checked_add(observation.len())
            .ok_or(BottomKError::SampleByteCountOverflow)?;
        if actual_sample_bytes > profile.max_sample_bytes {
            return Err(BottomKError::SampleByteLimitExceeded {
                requested: actual_sample_bytes,
                maximum: profile.max_sample_bytes,
            }
            .into());
        }
        let expected_hash = stable_hash(profile.hash_algorithm, profile.seed, observation)?;
        if sample.hash != expected_hash {
            return Err(BottomKCodecError::HashMismatch {
                index,
                actual: sample.hash,
                expected: expected_hash,
            });
        }
        validate_sample_successor(previous, sample.hash, observation, index)?;
        previous = Some((sample.hash, observation));
        canonical_usize(observation.len())?;
        encoded_len = encoded_len
            .checked_add(observation.len())
            .ok_or(BottomKCodecError::LengthOverflow)?;
    }
    if sample_bytes != actual_sample_bytes {
        return Err(BottomKCodecError::SampleByteCountMismatch {
            declared: sample_bytes,
            actual: actual_sample_bytes,
        });
    }

    Ok(CanonicalBottomKLayout {
        k: canonical_usize(profile.k)?,
        max_observation_bytes: canonical_usize(profile.max_observation_bytes)?,
        max_sample_bytes: canonical_usize(profile.max_sample_bytes)?,
        sample_bytes: canonical_usize(sample_bytes)?,
        sample_count: canonical_usize(samples.len())?,
        encoded_len,
    })
}

fn preflight_canonical_bytes(
    bytes: &[u8],
    expected_profile: BottomKProfile,
    limits: BottomKDecodeLimits,
) -> Result<DecodedBottomKHeader, BottomKCodecError> {
    enforce_decode_limit(
        BottomKDecodeResource::EncodedBytes,
        bytes.len(),
        limits.max_encoded_bytes,
    )?;
    let mut decoder = BottomKDecoder::new(bytes);
    let magic = decoder.read_array::<8>()?;
    if magic != CANONICAL_MAGIC {
        return Err(BottomKCodecError::MagicMismatch { actual: magic });
    }
    let version = decoder.read_u16()?;
    if version != CANONICAL_VERSION {
        return Err(BottomKCodecError::UnsupportedVersion { actual: version });
    }
    let hash_algorithm = decode_hash_algorithm(decoder.read_u8()?)?;

    let k = decoded_usize(decoder.read_u64()?)?;
    let seed = decoder.read_u64()?;
    let max_observation_bytes = decoded_usize(decoder.read_u64()?)?;
    let max_sample_bytes = decoded_usize(decoder.read_u64()?)?;
    let sample_bytes = decoded_usize(decoder.read_u64()?)?;
    let sample_count = decoded_usize(decoder.read_u64()?)?;
    let profile = BottomKProfile {
        k,
        hash_algorithm,
        seed,
        max_observation_bytes,
        max_sample_bytes,
    };

    if profile != expected_profile {
        return Err(BottomKCodecError::ProfileMismatch {
            expected: expected_profile,
            actual: profile,
        });
    }
    enforce_decode_limit(
        BottomKDecodeResource::Samples,
        profile.k,
        limits.max_samples,
    )?;
    enforce_decode_limit(
        BottomKDecodeResource::ObservationBytes,
        profile.max_observation_bytes,
        limits.max_observation_bytes,
    )?;
    enforce_decode_limit(
        BottomKDecodeResource::SampleBytes,
        profile.max_sample_bytes,
        limits.max_sample_bytes,
    )?;
    if profile.k == 0 {
        return Err(BottomKError::EmptySampleSize.into());
    }
    if sample_count > profile.k {
        return Err(BottomKCodecError::SampleCountExceedsProfile {
            actual: sample_count,
            maximum: profile.k,
        });
    }
    if sample_bytes > profile.max_sample_bytes {
        return Err(BottomKError::SampleByteLimitExceeded {
            requested: sample_bytes,
            maximum: profile.max_sample_bytes,
        }
        .into());
    }
    enforce_decode_limit(
        BottomKDecodeResource::Samples,
        sample_count,
        limits.max_samples,
    )?;
    enforce_decode_limit(
        BottomKDecodeResource::SampleBytes,
        sample_bytes,
        limits.max_sample_bytes,
    )?;

    let expected_len = expected_canonical_len(sample_count, sample_bytes)?;
    if bytes.len() < expected_len {
        return Err(BottomKCodecError::Truncated {
            offset: CANONICAL_HEADER_BYTES,
            needed: expected_len - CANONICAL_HEADER_BYTES,
            remaining: bytes.len().saturating_sub(CANONICAL_HEADER_BYTES),
        });
    }
    if bytes.len() > expected_len {
        return Err(BottomKCodecError::TrailingBytes {
            offset: expected_len,
            remaining: bytes.len() - expected_len,
        });
    }

    let mut actual_sample_bytes = 0_usize;
    let mut previous: Option<(u64, &[u8])> = None;
    for index in 0..sample_count {
        let hash = decoder.read_u64()?;
        let observation_bytes = decoded_usize(decoder.read_u64()?)?;
        if observation_bytes > profile.max_observation_bytes {
            return Err(BottomKError::ObservationTooLarge {
                bytes: observation_bytes,
                maximum: profile.max_observation_bytes,
            }
            .into());
        }
        enforce_decode_limit(
            BottomKDecodeResource::ObservationBytes,
            observation_bytes,
            limits.max_observation_bytes,
        )?;
        let observation = decoder.take(observation_bytes)?;
        actual_sample_bytes = actual_sample_bytes
            .checked_add(observation_bytes)
            .ok_or(BottomKError::SampleByteCountOverflow)?;
        if actual_sample_bytes > profile.max_sample_bytes {
            return Err(BottomKError::SampleByteLimitExceeded {
                requested: actual_sample_bytes,
                maximum: profile.max_sample_bytes,
            }
            .into());
        }
        let expected_hash = stable_hash(profile.hash_algorithm, profile.seed, observation)?;
        if hash != expected_hash {
            return Err(BottomKCodecError::HashMismatch {
                index,
                actual: hash,
                expected: expected_hash,
            });
        }
        validate_sample_successor(previous, hash, observation, index)?;
        previous = Some((hash, observation));
    }
    if sample_bytes != actual_sample_bytes {
        return Err(BottomKCodecError::SampleByteCountMismatch {
            declared: sample_bytes,
            actual: actual_sample_bytes,
        });
    }
    decoder.finish()?;

    Ok(DecodedBottomKHeader {
        profile,
        sample_bytes,
        sample_count,
    })
}

fn enforce_decode_limit(
    resource: BottomKDecodeResource,
    actual: usize,
    maximum: usize,
) -> Result<(), BottomKCodecError> {
    if actual > maximum {
        Err(BottomKCodecError::DecodeLimitExceeded {
            resource,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn validate_sample_successor(
    previous: Option<(u64, &[u8])>,
    hash: u64,
    observation: &[u8],
    index: usize,
) -> Result<(), BottomKCodecError> {
    let Some((previous_hash, previous_observation)) = previous else {
        return Ok(());
    };
    match previous_hash
        .cmp(&hash)
        .then_with(|| previous_observation.cmp(observation))
    {
        Ordering::Less => Ok(()),
        Ordering::Equal => Err(BottomKCodecError::DuplicateSample { index }),
        Ordering::Greater => Err(BottomKCodecError::SamplesOutOfOrder { index }),
    }
}

fn expected_canonical_len(
    sample_count: usize,
    sample_bytes: usize,
) -> Result<usize, BottomKCodecError> {
    let sample_headers = sample_count
        .checked_mul(CANONICAL_SAMPLE_HEADER_BYTES)
        .ok_or(BottomKCodecError::LengthOverflow)?;
    CANONICAL_HEADER_BYTES
        .checked_add(sample_headers)
        .and_then(|length| length.checked_add(sample_bytes))
        .ok_or(BottomKCodecError::LengthOverflow)
}

fn canonical_usize(value: usize) -> Result<u64, BottomKCodecError> {
    u64::try_from(value).map_err(|_| BottomKCodecError::IntegerUnrepresentable)
}

fn decoded_usize(value: u64) -> Result<usize, BottomKCodecError> {
    usize::try_from(value).map_err(|_| BottomKCodecError::IntegerUnrepresentable)
}

fn decode_hash_algorithm(actual: u8) -> Result<BottomKHashAlgorithm, BottomKCodecError> {
    match actual {
        value if value == BottomKHashAlgorithm::SeededHasherV1.canonical_tag() => {
            Ok(BottomKHashAlgorithm::SeededHasherV1)
        }
        actual => Err(BottomKCodecError::UnsupportedHashAlgorithm { actual }),
    }
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

struct BottomKDecoder<'bytes> {
    bytes: &'bytes [u8],
    offset: usize,
}

impl<'bytes> BottomKDecoder<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, needed: usize) -> Result<&'bytes [u8], BottomKCodecError> {
        let end = self
            .offset
            .checked_add(needed)
            .ok_or(BottomKCodecError::LengthOverflow)?;
        let Some(value) = self.bytes.get(self.offset..end) else {
            return Err(BottomKCodecError::Truncated {
                offset: self.offset,
                needed,
                remaining: self.bytes.len().saturating_sub(self.offset),
            });
        };
        self.offset = end;
        Ok(value)
    }

    fn read_array<const LENGTH: usize>(&mut self) -> Result<[u8; LENGTH], BottomKCodecError> {
        let source = self.take(LENGTH)?;
        let mut value = [0_u8; LENGTH];
        value.copy_from_slice(source);
        Ok(value)
    }

    fn read_u16(&mut self) -> Result<u16, BottomKCodecError> {
        Ok(u16::from_be_bytes(self.read_array::<2>()?))
    }

    fn read_u8(&mut self) -> Result<u8, BottomKCodecError> {
        Ok(self.read_array::<1>()?[0])
    }

    fn read_u64(&mut self) -> Result<u64, BottomKCodecError> {
        Ok(u64::from_be_bytes(self.read_array::<8>()?))
    }

    fn finish(self) -> Result<(), BottomKCodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(BottomKCodecError::TrailingBytes {
                offset: self.offset,
                remaining: self.bytes.len() - self.offset,
            })
        }
    }
}

fn finite_hash64_distinct_estimate(retained: u64, threshold_hash: u64) -> u64 {
    debug_assert!(retained >= 2);
    let rank = u128::from(threshold_hash) + 1;
    let numerator = u128::from(retained - 1) << 64;
    // `numerator <= 2^128 - 2^65` and `rank / 2 <= 2^63`, so the sum is
    // strictly smaller than 2^128 and nearest-integer rounding cannot overflow.
    let rounded = (numerator + rank / 2) / rank;
    u64::try_from(rounded).unwrap_or(u64::MAX).max(retained)
}

fn stable_hash(
    algorithm: BottomKHashAlgorithm,
    seed: u64,
    observation: &[u8],
) -> Result<u64, BottomKError> {
    let observation_bytes =
        u64::try_from(observation.len()).map_err(|_| BottomKError::SampleByteCountOverflow)?;
    match algorithm {
        BottomKHashAlgorithm::SeededHasherV1 => {
            let mut hasher = SeededHasher::new(seed);
            hasher.write(HASH_DOMAIN);
            hasher.write_u64(observation_bytes);
            hasher.write(observation);
            Ok(hasher.finish())
        }
    }
}

fn try_clone_sample(
    hash: u64,
    observation: &[u8],
    target: BottomKAllocationTarget,
) -> Result<BottomKSample, BottomKError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(observation.len())
        .map_err(|_| BottomKError::AllocationFailed {
            target,
            requested: observation.len(),
        })?;
    bytes.extend_from_slice(observation);
    Ok(BottomKSample {
        hash,
        observation: bytes,
    })
}

fn reserve_samples(
    samples: &mut Vec<BottomKSample>,
    requested: usize,
    target: BottomKAllocationTarget,
) -> Result<(), BottomKError> {
    samples
        .try_reserve_exact(requested)
        .map_err(|_| BottomKError::AllocationFailed { target, requested })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_accuracy_fixtures::{canonical_edge_bytes, named_graph_fixtures};

    const VERSION_OFFSET: usize = CANONICAL_MAGIC.len();
    const HASH_ALGORITHM_OFFSET: usize = VERSION_OFFSET + 2;
    const K_OFFSET: usize = HASH_ALGORITHM_OFFSET + 1;
    const SEED_OFFSET: usize = K_OFFSET + 8;
    const MAX_OBSERVATION_BYTES_OFFSET: usize = SEED_OFFSET + 8;
    const MAX_SAMPLE_BYTES_OFFSET: usize = MAX_OBSERVATION_BYTES_OFFSET + 8;
    const SAMPLE_BYTES_OFFSET: usize = MAX_SAMPLE_BYTES_OFFSET + 8;
    const SAMPLE_COUNT_OFFSET: usize = SAMPLE_BYTES_OFFSET + 8;

    fn profile(k: usize) -> BottomKProfile {
        BottomKProfile::new(k, 0x424f_5454_4f4d_4b31, 64, 256)
    }

    fn sketch(k: usize) -> BottomKSketch {
        BottomKSketch::try_new(profile(k)).expect("bounded profile")
    }

    fn decode_limits(profile: BottomKProfile) -> BottomKDecodeLimits {
        BottomKDecodeLimits::new(
            usize::MAX,
            profile.k,
            profile.max_observation_bytes,
            profile.max_sample_bytes,
        )
    }

    fn read_fixture(
        bytes: &[u8],
        expected_profile: BottomKProfile,
    ) -> Result<BottomKSketch, BottomKCodecError> {
        BottomKSketch::try_from_canonical_bytes(
            bytes,
            expected_profile,
            decode_limits(expected_profile),
        )
    }

    fn observe_all(sketch: &mut BottomKSketch, observations: &[&[u8]]) {
        for &observation in observations {
            sketch
                .try_observe(observation)
                .expect("bounded observation");
        }
    }

    fn raw_canonical_bytes(
        profile: BottomKProfile,
        declared_sample_bytes: usize,
        samples: &[(u64, &[u8])],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&CANONICAL_MAGIC);
        push_u16(&mut bytes, CANONICAL_VERSION);
        bytes.push(profile.hash_algorithm.canonical_tag());
        push_u64(
            &mut bytes,
            u64::try_from(profile.k).expect("test profile fits u64"),
        );
        push_u64(&mut bytes, profile.seed);
        push_u64(
            &mut bytes,
            u64::try_from(profile.max_observation_bytes).expect("test profile fits u64"),
        );
        push_u64(
            &mut bytes,
            u64::try_from(profile.max_sample_bytes).expect("test profile fits u64"),
        );
        push_u64(
            &mut bytes,
            u64::try_from(declared_sample_bytes).expect("test payload fits u64"),
        );
        push_u64(
            &mut bytes,
            u64::try_from(samples.len()).expect("test sample count fits u64"),
        );
        for &(hash, observation) in samples {
            push_u64(&mut bytes, hash);
            push_u64(
                &mut bytes,
                u64::try_from(observation.len()).expect("test observation fits u64"),
            );
            bytes.extend_from_slice(observation);
        }
        bytes
    }

    #[test]
    fn stable_hash_and_order_are_deterministic() {
        let observations = [
            &b"alpha"[..],
            &b"beta"[..],
            &b"gamma"[..],
            &b"delta"[..],
            &b"epsilon"[..],
        ];
        let mut forward = sketch(3);
        let mut reverse = sketch(3);
        observe_all(&mut forward, &observations);
        for &observation in observations.iter().rev() {
            reverse
                .try_observe(observation)
                .expect("bounded observation");
        }

        assert_eq!(forward, reverse);
        assert!(
            forward
                .canonical_state()
                .samples
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert_eq!(
            stable_hash(profile(3).hash_algorithm, profile(3).seed, b"alpha")
                .expect("bounded hash input"),
            0x36f6_5c36_d3c8_d99c
        );
        assert_ne!(
            stable_hash(profile(3).hash_algorithm, profile(3).seed, b"alpha")
                .expect("bounded hash input"),
            stable_hash(profile(3).hash_algorithm, profile(3).seed ^ 1, b"alpha")
                .expect("bounded hash input")
        );
    }

    #[test]
    fn idealized_distinct_count_contract_matches_exact_vectors() {
        assert_eq!(profile(1).idealized_distinct_count_error_contract(), None);
        assert_eq!(profile(2).idealized_distinct_count_error_contract(), None);

        let contract = profile(258)
            .idealized_distinct_count_error_contract()
            .expect("k >= 3 has finite variance in the idealized model");
        assert_eq!(contract.sample_size(), 258);
        assert_eq!(contract.deviation_multiplier(), 4);
        assert_eq!(contract.variance_denominator(), 16);
        assert_eq!(
            contract.model(),
            BottomKDistinctCountModel::IdealizedIndependentUniformContinuousRanks
        );
        assert_eq!(contract.confidence_parts_per_million_floor(), 937_500);
        assert_eq!(contract.relative_error_parts_per_million_ceiling(), 250_000);
        assert_eq!(contract.maximum_absolute_error(1_025), 257);

        let smallest_contract = profile(3)
            .idealized_distinct_count_error_contract()
            .expect("k = 3 is the smallest finite-variance profile");
        assert_eq!(smallest_contract.variance_denominator(), 1);
        assert_eq!(smallest_contract.maximum_absolute_error(u64::MAX), u64::MAX);
    }

    #[test]
    fn finite_hash64_estimator_rounding_and_saturation_match_exact_vectors() {
        assert_eq!(
            finite_hash64_distinct_estimate(3, 2),
            12_297_829_382_473_034_411
        );
        assert_eq!(
            finite_hash64_distinct_estimate(3, 4),
            7_378_697_629_483_820_646
        );
        assert_eq!(
            finite_hash64_distinct_estimate(4, 9),
            5_534_023_222_112_865_485
        );
        assert_eq!(finite_hash64_distinct_estimate(3, u64::MAX), 3);
        assert_eq!(finite_hash64_distinct_estimate(2, 0), u64::MAX);
    }

    #[test]
    fn distinct_count_is_exact_before_saturation_and_typed_when_k_is_one() {
        let mut complete = sketch(4);
        observe_all(&mut complete, &[b"a", b"b", b"c"]);
        assert_eq!(
            complete
                .try_estimate_distinct_count()
                .expect("unsaturated state is exact"),
            BottomKDistinctEstimate {
                value: 3,
                method: BottomKDistinctEstimateMethod::ExactUnsaturated,
                threshold_hash: None,
            }
        );

        let mut too_small = sketch(1);
        too_small.try_observe(b"a").expect("bounded observation");
        assert_eq!(
            too_small.try_estimate_distinct_count(),
            Err(BottomKDistinctEstimateError::InsufficientSampleSize {
                retained: 1,
                minimum: 2,
            })
        );
    }

    #[test]
    fn named_graph_finite_rank_calibration_matches_independent_bounds() {
        const SAMPLE_SIZE: usize = 256;
        const SEEDS: [u64; 4] = [
            0x424b_4143_4300_0001,
            0x424b_4143_4300_0002,
            0x424b_4143_4300_0003,
            0x424b_4143_4300_0004,
        ];

        for fixture in named_graph_fixtures() {
            let exact =
                u64::try_from(fixture.edges.len()).expect("fixture edge count fits canonical u64");
            assert!(fixture.edges.len() > SAMPLE_SIZE);

            for seed in SEEDS {
                let profile = BottomKProfile::new(SAMPLE_SIZE, seed, 16, SAMPLE_SIZE * 16);
                let contract = profile
                    .idealized_distinct_count_error_contract()
                    .expect("accuracy profile has finite idealized variance");
                assert!(
                    matches!(exact, 1_023 | 1_024 | 3_072),
                    "fixture {} has unexpected edge count {exact}",
                    fixture.name
                );
                let independent_bound = if exact == 3_072 {
                    820
                } else if exact == 1_024 {
                    274
                } else {
                    273
                };
                assert_eq!(contract.maximum_absolute_error(exact), independent_bound);
                assert_eq!(contract.relative_error_parts_per_million_ceiling(), 266_667);
                let mut value =
                    BottomKSketch::try_new(profile).expect("accuracy profile is bounded");
                let mut population_hashes = Vec::with_capacity(fixture.edges.len());
                for &(left, right) in &fixture.edges {
                    let observation = canonical_edge_bytes(left, right);
                    population_hashes.push(
                        value
                            .try_hash(&observation)
                            .expect("canonical edge fits accuracy profile"),
                    );
                    value
                        .try_observe(&observation)
                        .expect("canonical edge fits accuracy profile");
                }

                population_hashes.sort_unstable();
                assert!(
                    population_hashes.windows(2).all(|pair| pair[0] != pair[1]),
                    "fixture={} seed={seed:#018x} profile={profile:?}: the complete measured \
                     population contains a finite-rank collision",
                    fixture.name
                );
                assert!(value.is_saturated());
                let estimate = value
                    .try_estimate_distinct_count()
                    .expect("saturated k >= 2 profile is estimable");
                let error = estimate.value.abs_diff(exact);
                assert!(
                    error <= independent_bound,
                    "fixture={} seed={seed:#018x} profile={profile:?} exact={exact} estimate={} \
                     error={error} calibration_bound={independent_bound} \
                     idealized_confidence_ppm={} idealized_relative_bound_ppm={} model={:?}",
                    fixture.name,
                    estimate.value,
                    contract.confidence_parts_per_million_floor(),
                    contract.relative_error_parts_per_million_ceiling(),
                    contract.model()
                );
                assert_eq!(
                    estimate.method,
                    BottomKDistinctEstimateMethod::FiniteHash64KthOrderStatistic
                );
            }
        }
    }

    #[test]
    fn canonical_codec_round_trips_and_collapses_observation_order() {
        let observations = [
            &b"alpha"[..],
            &b"beta"[..],
            &b"gamma"[..],
            &b"delta"[..],
            &b"epsilon"[..],
        ];
        let mut forward = sketch(4);
        let mut reverse = sketch(4);
        observe_all(&mut forward, &observations);
        for &observation in observations.iter().rev() {
            reverse
                .try_observe(observation)
                .expect("bounded observation");
        }

        let forward_bytes = forward
            .try_to_canonical_bytes()
            .expect("valid state encodes");
        let reverse_bytes = reverse
            .try_to_canonical_bytes()
            .expect("valid state encodes");
        assert_eq!(forward_bytes, reverse_bytes);
        assert_eq!(&forward_bytes[..8], b"FGDBBTK1");
        assert_eq!(
            &forward_bytes[VERSION_OFFSET..HASH_ALGORITHM_OFFSET],
            &1_u16.to_be_bytes()
        );
        assert_eq!(
            forward_bytes[HASH_ALGORITHM_OFFSET],
            BottomKHashAlgorithm::SeededHasherV1.canonical_tag()
        );

        let decoded = read_fixture(&forward_bytes, forward.profile()).expect("canonical value");
        assert_eq!(decoded, forward);
        assert_eq!(
            decoded
                .try_to_canonical_bytes()
                .expect("decoded state re-encodes"),
            forward_bytes
        );
    }

    #[test]
    fn canonical_decoder_requires_exact_profile_and_caller_owned_limits() {
        let mut value = sketch(4);
        observe_all(&mut value, &[b"alpha", b"beta"]);
        let encoded = value.try_to_canonical_bytes().expect("valid state encodes");
        let expected_profile = value.profile();
        let exact_limits = BottomKDecodeLimits::new(
            encoded.len(),
            expected_profile.k,
            expected_profile.max_observation_bytes,
            expected_profile.max_sample_bytes,
        );
        assert_eq!(
            BottomKSketch::try_from_canonical_bytes(&encoded, expected_profile, exact_limits)
                .expect("exact admission bounds accept the value"),
            value
        );

        let wrong_profile = BottomKProfile {
            seed: expected_profile.seed ^ 1,
            ..expected_profile
        };
        assert_eq!(
            BottomKSketch::try_from_canonical_bytes(&encoded, wrong_profile, exact_limits),
            Err(BottomKCodecError::ProfileMismatch {
                expected: wrong_profile,
                actual: expected_profile,
            })
        );
        assert_eq!(
            BottomKSketch::try_from_canonical_bytes(
                &encoded,
                expected_profile,
                BottomKDecodeLimits {
                    max_encoded_bytes: encoded.len() - 1,
                    ..exact_limits
                },
            ),
            Err(BottomKCodecError::DecodeLimitExceeded {
                resource: BottomKDecodeResource::EncodedBytes,
                actual: encoded.len(),
                maximum: encoded.len() - 1,
            })
        );
        assert_eq!(
            BottomKSketch::try_from_canonical_bytes(
                &encoded,
                expected_profile,
                BottomKDecodeLimits {
                    max_samples: expected_profile.k - 1,
                    ..exact_limits
                },
            ),
            Err(BottomKCodecError::DecodeLimitExceeded {
                resource: BottomKDecodeResource::Samples,
                actual: expected_profile.k,
                maximum: expected_profile.k - 1,
            })
        );
    }

    #[test]
    fn empty_huge_k_profile_does_not_allocate_and_decode_admission_caps_k() {
        let huge_profile = BottomKProfile::new(usize::MAX, 7, 0, 0);
        let mut left = BottomKSketch::try_new(huge_profile).expect("nonzero profile");
        let right = BottomKSketch::try_new(huge_profile).expect("nonzero profile");
        assert_eq!(left.samples.capacity(), 0);
        left.try_merge(&right)
            .expect("empty merge needs no reserve");
        assert_eq!(left.samples.capacity(), 0);

        let encoded = left.try_to_canonical_bytes().expect("empty state encodes");
        assert_eq!(
            BottomKSketch::try_from_canonical_bytes(
                &encoded,
                huge_profile,
                BottomKDecodeLimits::new(encoded.len(), 1_024, 0, 0),
            ),
            Err(BottomKCodecError::DecodeLimitExceeded {
                resource: BottomKDecodeResource::Samples,
                actual: usize::MAX,
                maximum: 1_024,
            })
        );
    }

    #[test]
    fn canonical_codec_preserves_partitioned_merge_results() {
        let left_values = [&b"a"[..], &b"d"[..], &b"g"[..], &b"shared"[..]];
        let middle_values = [&b"b"[..], &b"e"[..], &b"h"[..], &b"shared"[..]];
        let right_values = [&b"c"[..], &b"f"[..], &b"i"[..], &b"shared"[..]];

        let mut left = sketch(5);
        let mut middle = sketch(5);
        let mut right = sketch(5);
        observe_all(&mut left, &left_values);
        observe_all(&mut middle, &middle_values);
        observe_all(&mut right, &right_values);

        let mut left_associated = left.clone();
        left_associated
            .try_merge(&middle)
            .expect("matching profile");
        left_associated.try_merge(&right).expect("matching profile");

        middle.try_merge(&right).expect("matching profile");
        left.try_merge(&middle).expect("matching profile");

        let left_associated_bytes = left_associated
            .try_to_canonical_bytes()
            .expect("valid merged state");
        let right_associated_bytes = left.try_to_canonical_bytes().expect("valid merged state");
        assert_eq!(left_associated_bytes, right_associated_bytes);
        assert_eq!(
            read_fixture(&left_associated_bytes, left_associated.profile())
                .expect("canonical merged state"),
            left_associated
        );
    }

    #[test]
    fn canonical_encoder_rejects_forged_noncanonical_state() {
        let mut valid = sketch(4);
        observe_all(&mut valid, &[b"alpha", b"beta", b"gamma"]);

        let mut wrong_hash = valid.clone();
        wrong_hash.samples[0].hash ^= 1;
        assert!(matches!(
            wrong_hash.try_to_canonical_bytes(),
            Err(BottomKCodecError::HashMismatch { index: 0, .. })
        ));

        let mut out_of_order = valid.clone();
        out_of_order.samples.swap(0, 1);
        assert_eq!(
            out_of_order.try_to_canonical_bytes(),
            Err(BottomKCodecError::SamplesOutOfOrder { index: 1 })
        );

        let mut duplicate = valid.clone();
        duplicate.samples[1] = duplicate.samples[0].clone();
        duplicate.sample_bytes = duplicate
            .samples
            .iter()
            .map(|sample| sample.observation.len())
            .sum();
        assert_eq!(
            duplicate.try_to_canonical_bytes(),
            Err(BottomKCodecError::DuplicateSample { index: 1 })
        );

        let mut wrong_total = valid;
        wrong_total.sample_bytes += 1;
        assert!(matches!(
            wrong_total.try_to_canonical_bytes(),
            Err(BottomKCodecError::SampleByteCountMismatch { .. })
        ));
    }

    #[test]
    fn canonical_decoder_rejects_malformed_headers_and_lengths_before_allocation() {
        let mut value = sketch(4);
        observe_all(&mut value, &[b"alpha", b"beta", b"gamma"]);
        let encoded = value.try_to_canonical_bytes().expect("valid state encodes");

        let mut wrong_magic = encoded.clone();
        wrong_magic[0] ^= 1;
        assert!(matches!(
            read_fixture(&wrong_magic, value.profile()),
            Err(BottomKCodecError::MagicMismatch { .. })
        ));

        let mut wrong_version = encoded.clone();
        wrong_version[VERSION_OFFSET..HASH_ALGORITHM_OFFSET].copy_from_slice(&2_u16.to_be_bytes());
        assert_eq!(
            read_fixture(&wrong_version, value.profile()),
            Err(BottomKCodecError::UnsupportedVersion { actual: 2 })
        );

        let mut wrong_hash_algorithm = encoded.clone();
        wrong_hash_algorithm[HASH_ALGORITHM_OFFSET] = 2;
        assert_eq!(
            read_fixture(&wrong_hash_algorithm, value.profile()),
            Err(BottomKCodecError::UnsupportedHashAlgorithm { actual: 2 })
        );

        let mut count_exceeds_k = encoded.clone();
        count_exceeds_k[SAMPLE_COUNT_OFFSET..CANONICAL_HEADER_BYTES]
            .copy_from_slice(&5_u64.to_be_bytes());
        assert_eq!(
            read_fixture(&count_exceeds_k, value.profile()),
            Err(BottomKCodecError::SampleCountExceedsProfile {
                actual: 5,
                maximum: 4,
            })
        );

        let mut impossible_count = raw_canonical_bytes(profile(1_000_000), 0, &[]);
        impossible_count[SAMPLE_COUNT_OFFSET..CANONICAL_HEADER_BYTES]
            .copy_from_slice(&1_000_000_u64.to_be_bytes());
        assert!(matches!(
            read_fixture(&impossible_count, profile(1_000_000)),
            Err(BottomKCodecError::Truncated {
                offset: CANONICAL_HEADER_BYTES,
                ..
            })
        ));

        for end in 0..encoded.len() {
            assert!(
                matches!(
                    read_fixture(&encoded[..end], value.profile()),
                    Err(BottomKCodecError::Truncated { .. })
                ),
                "prefix ending at byte {end} was not rejected as truncated"
            );
        }

        let mut trailing = encoded;
        trailing.push(0);
        assert_eq!(
            read_fixture(&trailing, value.profile()),
            Err(BottomKCodecError::TrailingBytes {
                offset: trailing.len() - 1,
                remaining: 1,
            })
        );
    }

    #[test]
    fn canonical_decoder_rejects_invalid_hash_order_distinctness_and_byte_state() {
        let mut value = sketch(4);
        observe_all(&mut value, &[b"alpha", b"beta", b"gamma"]);
        let state = value.canonical_state();
        let first = &state.samples[0];
        let second = &state.samples[1];

        let mut wrong_hash = raw_canonical_bytes(
            state.profile,
            first.observation.len(),
            &[(first.hash ^ 1, &first.observation)],
        );
        assert!(matches!(
            read_fixture(&wrong_hash, state.profile),
            Err(BottomKCodecError::HashMismatch { index: 0, .. })
        ));

        let reversed_bytes = first
            .observation
            .len()
            .checked_add(second.observation.len())
            .expect("test payload fits");
        let reversed = raw_canonical_bytes(
            state.profile,
            reversed_bytes,
            &[
                (second.hash, &second.observation),
                (first.hash, &first.observation),
            ],
        );
        assert_eq!(
            read_fixture(&reversed, state.profile),
            Err(BottomKCodecError::SamplesOutOfOrder { index: 1 })
        );

        let duplicate_bytes = first
            .observation
            .len()
            .checked_mul(2)
            .expect("test payload fits");
        let duplicate = raw_canonical_bytes(
            state.profile,
            duplicate_bytes,
            &[
                (first.hash, &first.observation),
                (first.hash, &first.observation),
            ],
        );
        assert_eq!(
            read_fixture(&duplicate, state.profile),
            Err(BottomKCodecError::DuplicateSample { index: 1 })
        );

        wrong_hash[MAX_OBSERVATION_BYTES_OFFSET..MAX_SAMPLE_BYTES_OFFSET]
            .copy_from_slice(&0_u64.to_be_bytes());
        let zero_observation_profile = BottomKProfile {
            max_observation_bytes: 0,
            ..state.profile
        };
        assert!(matches!(
            read_fixture(&wrong_hash, zero_observation_profile),
            Err(BottomKCodecError::State(
                BottomKError::ObservationTooLarge { maximum: 0, .. }
            ))
        ));

        let actual_bytes = first.observation.len();
        let mut wrong_total = raw_canonical_bytes(
            state.profile,
            actual_bytes + 1,
            &[(first.hash, &first.observation)],
        );
        wrong_total.push(0);
        assert_eq!(
            read_fixture(&wrong_total, state.profile),
            Err(BottomKCodecError::SampleByteCountMismatch {
                declared: actual_bytes + 1,
                actual: actual_bytes,
            })
        );

        let mut over_limit = raw_canonical_bytes(
            state.profile,
            actual_bytes,
            &[(first.hash, &first.observation)],
        );
        over_limit[MAX_SAMPLE_BYTES_OFFSET..SAMPLE_BYTES_OFFSET].copy_from_slice(
            &u64::try_from(actual_bytes - 1)
                .expect("test payload fits")
                .to_be_bytes(),
        );
        let over_limit_profile = BottomKProfile {
            max_sample_bytes: actual_bytes - 1,
            ..state.profile
        };
        assert_eq!(
            read_fixture(&over_limit, over_limit_profile),
            Err(BottomKCodecError::State(
                BottomKError::SampleByteLimitExceeded {
                    requested: actual_bytes,
                    maximum: actual_bytes - 1,
                }
            ))
        );
    }

    #[test]
    fn duplicate_observations_are_idempotent() {
        let mut value = sketch(4);
        value.try_observe(b"same").expect("first observation");
        let before = value.clone();
        for _ in 0..20 {
            value.try_observe(b"same").expect("duplicate observation");
        }
        assert_eq!(value, before);
        assert_eq!(value.len(), 1);
        assert_eq!(value.sample_bytes(), 4);
    }

    #[test]
    fn merge_is_commutative_associative_and_idempotent() {
        fn part(entries: &[&[u8]]) -> BottomKSketch {
            let mut value = sketch(5);
            observe_all(&mut value, entries);
            value
        }

        let a = part(&[b"a", b"d", b"g", b"shared"]);
        let b = part(&[b"b", b"e", b"h", b"shared"]);
        let c = part(&[b"c", b"f", b"i", b"shared"]);

        let mut ab = a.clone();
        ab.try_merge(&b).expect("matching profile");
        let mut ba = b.clone();
        ba.try_merge(&a).expect("matching profile");
        assert_eq!(ab, ba);

        let mut ab_c = ab;
        ab_c.try_merge(&c).expect("matching profile");
        let mut bc = b;
        bc.try_merge(&c).expect("matching profile");
        let mut a_bc = a.clone();
        a_bc.try_merge(&bc).expect("matching profile");
        assert_eq!(ab_c, a_bc);

        let before = a.clone();
        let mut idempotent = a;
        idempotent
            .try_merge(&before)
            .expect("identical profile and state");
        assert_eq!(idempotent, before);
    }

    #[test]
    fn merged_sample_equals_naive_sorted_distinct_union() {
        let left_values = [&b"one"[..], &b"two"[..], &b"three"[..], &b"shared"[..]];
        let right_values = [&b"four"[..], &b"five"[..], &b"six"[..], &b"shared"[..]];
        let mut left = sketch(4);
        let mut right = sketch(4);
        observe_all(&mut left, &left_values);
        observe_all(&mut right, &right_values);
        left.try_merge(&right).expect("matching profile");

        let mut expected = left_values
            .into_iter()
            .chain(right_values)
            .map(|bytes| {
                (
                    stable_hash(profile(4).hash_algorithm, profile(4).seed, bytes)
                        .expect("bounded hash input"),
                    bytes,
                )
            })
            .collect::<Vec<_>>();
        expected.sort();
        expected.dedup();
        expected.truncate(4);
        let actual = left
            .canonical_state()
            .samples
            .iter()
            .map(|sample| (sample.hash(), sample.observation()))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn construction_observation_and_merge_limits_are_atomic() {
        assert_eq!(
            BottomKSketch::try_new(profile(0)),
            Err(BottomKError::EmptySampleSize)
        );

        let constrained_profile = BottomKProfile::new(2, 7, 3, 5);
        let mut constrained = BottomKSketch::try_new(constrained_profile).expect("bounded profile");
        assert_eq!(
            constrained.try_observe(b"four"),
            Err(BottomKError::ObservationTooLarge {
                bytes: 4,
                maximum: 3,
            })
        );
        constrained.try_observe(b"aaa").expect("first sample fits");
        let before = constrained.clone();
        assert_eq!(
            constrained.try_observe(b"bbb"),
            Err(BottomKError::SampleByteLimitExceeded {
                requested: 6,
                maximum: 5,
            })
        );
        assert_eq!(constrained, before);

        let mut left = BottomKSketch::try_new(constrained_profile).expect("bounded profile");
        let mut right = BottomKSketch::try_new(constrained_profile).expect("bounded profile");
        left.try_observe(b"aaa").expect("individual sample fits");
        right.try_observe(b"bbb").expect("individual sample fits");
        let before = left.clone();
        assert_eq!(
            left.try_merge(&right),
            Err(BottomKError::SampleByteLimitExceeded {
                requested: 6,
                maximum: 5,
            })
        );
        assert_eq!(left, before);

        let different = BottomKSketch::try_new(BottomKProfile {
            seed: constrained_profile.seed ^ 1,
            ..constrained_profile
        })
        .expect("bounded profile");
        assert_eq!(
            left.try_merge(&different),
            Err(BottomKError::ProfileMismatch)
        );
        assert_eq!(left, before);
    }

    #[test]
    fn deletion_is_exact_when_complete_and_rebuilds_when_saturated() {
        let mut complete = sketch(4);
        observe_all(&mut complete, &[b"a", b"b", b"c"]);
        assert_eq!(complete.try_remove(b"absent"), Ok(()));
        complete.try_remove(b"b").expect("complete set can delete");
        assert_eq!(complete.try_contains(b"b"), Ok(false));
        assert_eq!(complete.len(), 2);

        let mut saturated = sketch(2);
        let candidates = [&b"a"[..], &b"b"[..], &b"c"[..], &b"d"[..], &b"e"[..]];
        observe_all(&mut saturated, &candidates);
        let retained = saturated
            .canonical_state()
            .samples
            .iter()
            .map(|sample| sample.observation().to_vec())
            .collect::<Vec<_>>();
        let retained_observation = retained[0].clone();
        let unretained = candidates
            .iter()
            .find(|candidate| !retained.iter().any(|sample| sample == *candidate))
            .copied()
            .expect("more candidates than retained samples");

        let before = saturated.clone();
        assert_eq!(
            saturated.try_remove(&retained_observation),
            Err(BottomKError::RebuildRequired {
                observation_hash: stable_hash(
                    profile(2).hash_algorithm,
                    profile(2).seed,
                    &retained_observation,
                )
                .expect("bounded hash input"),
            })
        );
        assert_eq!(saturated, before);
        assert_eq!(saturated.try_remove(unretained), Ok(()));
        assert_eq!(saturated, before);
    }
}
