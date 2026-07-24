//! Deterministic sampled correlation summaries for canonical two-edge paths.
//!
//! Each logical observation identifies one distinct graph path together with
//! the canonical pattern on its first and second edge. Observations have set
//! semantics and are ranked by a stable, domain-separated hash, with their full
//! bytes as the collision tie-breaker. The sketch retains the bottom `k`
//! observations in that total order.
//!
//! For profile-identical states, merge computes the exact bottom `k` of the set
//! union. Consequently successful merges are commutative, associative, and
//! idempotent. Deletion is exact while the retained set is unsaturated. Once it
//! is saturated, removing a retained observation requires a rebuild because
//! the next-ranked observation is unknown; removing an unretained observation
//! remains an exact no-op.
//!
//! Statistical metadata is explicitly qualified by an idealized sampling
//! model. It is advisory planner evidence, not a graph invariant and not a
//! guarantee about the deterministic finite-width production hash.

#![forbid(unsafe_code)]

use core::cmp::Ordering;
use core::fmt;
use core::hash::Hasher;
use fgdb_collections::hash_table::SeededHasher;

const HASH_DOMAIN: &[u8] = b"fgdb:path-correlation:observation:v1";
const CANONICAL_MAGIC: [u8; 8] = *b"FGDBPCR1";
const CANONICAL_VERSION: u16 = 1;
const CANONICAL_HEADER_BYTES: usize = 8 + 2 + 1 + (7 * 8);
const CANONICAL_SAMPLE_HEADER_BYTES: usize = 4 * 8;
const PARTS_PER_MILLION: u64 = 1_000_000;

/// Stable rank algorithm included in complete profile identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PathCorrelationHashAlgorithm {
    /// Domain-separated [`SeededHasher`] stream frozen by canonical vectors.
    SeededHasherV1 = 1,
}

impl PathCorrelationHashAlgorithm {
    const fn canonical_tag(self) -> u8 {
        self as u8
    }
}

/// Canonical byte component of one path observation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PathCorrelationComponent {
    /// Identity of the distinct path instance.
    PathKey,
    /// Pattern on the first edge of the path.
    FirstPattern,
    /// Pattern on the second edge of the path.
    SecondPattern,
}

/// Complete deterministic behavior and resource profile.
///
/// `max_sample_bytes` must cover `sample_capacity` observations at the
/// component maxima. This makes the valid-state family closed under observe
/// and merge: a profile cannot admit two states whose exact bottom-k union
/// exceeds its own payload ceiling.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PathCorrelationProfile {
    /// Maximum number of distinct path observations retained.
    pub sample_capacity: usize,
    /// Stable rank algorithm included in merge identity.
    pub hash_algorithm: PathCorrelationHashAlgorithm,
    /// Explicit deterministic rank seed.
    pub seed: u64,
    /// Maximum bytes in one canonical path identity.
    pub max_path_key_bytes: usize,
    /// Maximum bytes in either canonical edge-pattern identity.
    pub max_pattern_bytes: usize,
    /// Maximum aggregate payload bytes retained by the sample.
    pub max_sample_bytes: usize,
}

impl PathCorrelationProfile {
    /// Creates a complete scalar sampling profile.
    #[must_use]
    pub const fn new(
        sample_capacity: usize,
        seed: u64,
        max_path_key_bytes: usize,
        max_pattern_bytes: usize,
        max_sample_bytes: usize,
    ) -> Self {
        Self {
            sample_capacity,
            hash_algorithm: PathCorrelationHashAlgorithm::SeededHasherV1,
            seed,
            max_path_key_bytes,
            max_pattern_bytes,
            max_sample_bytes,
        }
    }
}

/// Borrowed canonical two-edge path observation.
///
/// `path_key` identifies a distinct logical path occurrence. Re-observing the
/// same three byte strings is idempotent.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PathCorrelationObservation<'observation> {
    path_key: &'observation [u8],
    first_pattern: &'observation [u8],
    second_pattern: &'observation [u8],
}

impl<'observation> PathCorrelationObservation<'observation> {
    /// Creates one borrowed canonical path-pattern observation.
    #[must_use]
    pub const fn new(
        path_key: &'observation [u8],
        first_pattern: &'observation [u8],
        second_pattern: &'observation [u8],
    ) -> Self {
        Self {
            path_key,
            first_pattern,
            second_pattern,
        }
    }

    /// Canonical identity of the distinct graph path.
    #[must_use]
    pub const fn path_key(self) -> &'observation [u8] {
        self.path_key
    }

    /// Canonical pattern on the first edge.
    #[must_use]
    pub const fn first_pattern(self) -> &'observation [u8] {
        self.first_pattern
    }

    /// Canonical pattern on the second edge.
    #[must_use]
    pub const fn second_pattern(self) -> &'observation [u8] {
        self.second_pattern
    }
}

/// Caller-owned admission bounds for one canonical path-correlation value.
///
/// These limits are independent of the encoded profile. Untrusted bytes cannot
/// grant themselves more memory, per-record work, or per-component work.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PathCorrelationDecodeLimits {
    /// Maximum accepted canonical value length.
    pub max_encoded_bytes: usize,
    /// Maximum accepted profile capacity and retained sample count.
    pub max_samples: usize,
    /// Maximum accepted path-key profile ceiling and payload length.
    pub max_path_key_bytes: usize,
    /// Maximum accepted pattern profile ceiling and payload length.
    pub max_pattern_bytes: usize,
    /// Maximum accepted profile and retained aggregate payload bytes.
    pub max_sample_bytes: usize,
}

impl PathCorrelationDecodeLimits {
    /// Creates explicit decode admission bounds.
    #[must_use]
    pub const fn new(
        max_encoded_bytes: usize,
        max_samples: usize,
        max_path_key_bytes: usize,
        max_pattern_bytes: usize,
        max_sample_bytes: usize,
    ) -> Self {
        Self {
            max_encoded_bytes,
            max_samples,
            max_path_key_bytes,
            max_pattern_bytes,
            max_sample_bytes,
        }
    }
}

/// Allocation owned by a failed transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PathCorrelationAllocationTarget {
    /// Sorted retained-sample directory.
    SampleDirectory,
    /// Canonical path-key bytes.
    PathKey,
    /// First edge-pattern bytes.
    FirstPattern,
    /// Second edge-pattern bytes.
    SecondPattern,
    /// Temporary directory used by an atomic merge.
    MergeDirectory,
    /// Payload cloned into an atomic merge result.
    MergePayload,
}

/// Typed construction or state-transition failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathCorrelationError {
    /// A zero-capacity sample has no correlation semantics.
    EmptySampleCapacity,
    /// A canonical component ceiling must be nonzero.
    EmptyComponentLimit {
        /// Invalid profile component.
        component: PathCorrelationComponent,
    },
    /// Checked maximum-observation or maximum-sample arithmetic overflowed.
    ProfileSizeOverflow,
    /// The aggregate ceiling cannot hold `k` maximum-sized observations.
    ProfileSampleByteLimitTooSmall {
        /// Required bytes for closure under observe and merge.
        required: usize,
        /// Configured aggregate ceiling.
        actual: usize,
    },
    /// A canonical observation component is empty.
    EmptyComponent {
        /// Empty component.
        component: PathCorrelationComponent,
    },
    /// A canonical observation component exceeds its profile ceiling.
    ComponentTooLarge {
        /// Rejected component.
        component: PathCorrelationComponent,
        /// Observed bytes.
        actual: usize,
        /// Profile ceiling.
        maximum: usize,
    },
    /// Checked payload arithmetic overflowed.
    SampleByteCountOverflow,
    /// A platform-sized component cannot enter the stable hash stream.
    IntegerUnrepresentable,
    /// The allocator rejected a checked reservation.
    AllocationFailed {
        /// Component whose reservation failed.
        target: PathCorrelationAllocationTarget,
        /// Entries or bytes requested.
        requested: usize,
    },
    /// Merge operands use different complete profiles.
    ProfileMismatch,
    /// Private retained-state fields disagree despite validated construction.
    InvariantViolation,
    /// Removing a retained observation from a saturated sample is inexact.
    RebuildRequired {
        /// Stable rank of the retained observation.
        rank_hash: u64,
    },
}

impl fmt::Display for PathCorrelationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EmptySampleCapacity => {
                formatter.write_str("path-correlation sample capacity must be nonzero")
            }
            Self::EmptyComponentLimit { component } => {
                write!(
                    formatter,
                    "path-correlation {component:?} limit must be nonzero"
                )
            }
            Self::ProfileSizeOverflow => {
                formatter.write_str("path-correlation profile size arithmetic overflowed")
            }
            Self::ProfileSampleByteLimitTooSmall { required, actual } => write!(
                formatter,
                "path-correlation profile requires {required} sample bytes for algebra closure, \
                 configured ceiling is {actual}"
            ),
            Self::EmptyComponent { component } => {
                write!(
                    formatter,
                    "path-correlation {component:?} must not be empty"
                )
            }
            Self::ComponentTooLarge {
                component,
                actual,
                maximum,
            } => write!(
                formatter,
                "path-correlation {component:?} has {actual} bytes, maximum is {maximum}"
            ),
            Self::SampleByteCountOverflow => {
                formatter.write_str("path-correlation sample byte count overflowed")
            }
            Self::IntegerUnrepresentable => formatter.write_str(
                "path-correlation component length cannot enter the canonical hash stream",
            ),
            Self::AllocationFailed { target, requested } => write!(
                formatter,
                "could not reserve {requested} units for path-correlation {target:?}"
            ),
            Self::ProfileMismatch => formatter
                .write_str("cannot merge path-correlation sketches with different profiles"),
            Self::InvariantViolation => {
                formatter.write_str("path-correlation private state invariant was violated")
            }
            Self::RebuildRequired { rank_hash } => write!(
                formatter,
                "removing retained path-correlation observation {rank_hash:#018x} requires rebuild"
            ),
        }
    }
}

impl std::error::Error for PathCorrelationError {}

/// Result of observing one logical path.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PathCorrelationObserveOutcome {
    /// A new observation entered the retained sample.
    Inserted,
    /// The exact observation was already retained.
    AlreadyPresent,
    /// The observation ranked above a saturated bottom-k sample.
    OutsideRetainedSample,
}

/// Result of an exact deletion.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PathCorrelationDeleteOutcome {
    /// The complete unsaturated sample contained and removed the observation.
    Removed,
    /// The observation was not retained, so deletion leaves the sample exact.
    UnchangedUnretained,
}

/// Caller-owned resource checked while admitting canonical bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PathCorrelationDecodeResource {
    /// Total canonical input bytes.
    EncodedBytes,
    /// Profile capacity or retained directory entries.
    Samples,
    /// Path-key profile ceiling or one retained path key.
    PathKeyBytes,
    /// Pattern profile ceiling or one retained pattern.
    PatternBytes,
    /// Profile or retained aggregate payload bytes.
    SampleBytes,
}

/// Strict canonical-codec failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathCorrelationCodecError {
    /// The in-memory or decoded state violates a construction law.
    State(PathCorrelationError),
    /// A platform-sized field cannot be represented canonically.
    IntegerUnrepresentable,
    /// Computing the exact canonical value length overflowed.
    LengthOverflow,
    /// The allocator rejected an exact canonical reservation.
    AllocationFailed {
        /// Entries or bytes requested.
        requested: usize,
    },
    /// The eight-byte format discriminator did not match.
    MagicMismatch {
        /// Bytes found at the magic position.
        actual: [u8; 8],
    },
    /// The format version is not implemented.
    UnsupportedVersion {
        /// Version found in the input.
        actual: u16,
    },
    /// The hash-algorithm discriminator is not implemented.
    UnsupportedHashAlgorithm {
        /// Discriminator found in the encoded profile.
        actual: u8,
    },
    /// The encoded complete profile differs from the caller's trusted profile.
    ProfileMismatch {
        /// Trusted profile.
        expected: PathCorrelationProfile,
        /// Encoded profile.
        actual: PathCorrelationProfile,
    },
    /// A caller-owned decode bound was exceeded.
    DecodeLimitExceeded {
        /// Bounded resource.
        resource: PathCorrelationDecodeResource,
        /// Encoded or derived value.
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
        /// Bytes remaining at that offset.
        remaining: usize,
    },
    /// Retained sample count exceeds the encoded profile capacity.
    SampleCountExceedsProfile {
        /// Retained count.
        actual: usize,
        /// Encoded capacity.
        maximum: usize,
    },
    /// Declared aggregate payload bytes disagree with the records.
    SampleByteCountMismatch {
        /// Header value.
        declared: usize,
        /// Sum derived from retained records.
        actual: usize,
    },
    /// A retained rank is not the profile hash of its observation.
    HashMismatch {
        /// Zero-based sample index.
        index: usize,
        /// Encoded rank.
        actual: u64,
        /// Recomputed rank.
        expected: u64,
    },
    /// A retained observation repeats its predecessor.
    DuplicateSample {
        /// Zero-based index of the repeated sample.
        index: usize,
    },
    /// Retained observations are not strictly increasing by canonical rank.
    SamplesOutOfOrder {
        /// Zero-based index of the first out-of-order sample.
        index: usize,
    },
    /// Input contains bytes after one complete canonical value.
    TrailingBytes {
        /// First trailing byte.
        offset: usize,
        /// Number of trailing bytes.
        remaining: usize,
    },
}

impl fmt::Display for PathCorrelationCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for PathCorrelationCodecError {}

impl From<PathCorrelationError> for PathCorrelationCodecError {
    fn from(error: PathCorrelationError) -> Self {
        Self::State(error)
    }
}

/// One retained canonical sampled path.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PathCorrelationSample {
    rank_hash: u64,
    path_key: Vec<u8>,
    first_pattern: Vec<u8>,
    second_pattern: Vec<u8>,
}

impl PathCorrelationSample {
    /// Stable rank used for bottom-k ordering.
    #[must_use]
    pub const fn rank_hash(&self) -> u64 {
        self.rank_hash
    }

    /// Canonical identity of the distinct graph path.
    #[must_use]
    pub fn path_key(&self) -> &[u8] {
        &self.path_key
    }

    /// Canonical first-edge pattern.
    #[must_use]
    pub fn first_pattern(&self) -> &[u8] {
        &self.first_pattern
    }

    /// Canonical second-edge pattern.
    #[must_use]
    pub fn second_pattern(&self) -> &[u8] {
        &self.second_pattern
    }

    fn observation(&self) -> PathCorrelationObservation<'_> {
        PathCorrelationObservation::new(&self.path_key, &self.first_pattern, &self.second_pattern)
    }

    fn payload_bytes(&self) -> usize {
        self.path_key.len() + self.first_pattern.len() + self.second_pattern.len()
    }

    fn compare_key(&self, rank_hash: u64, observation: PathCorrelationObservation<'_>) -> Ordering {
        self.rank_hash
            .cmp(&rank_hash)
            .then_with(|| self.path_key.as_slice().cmp(observation.path_key))
            .then_with(|| self.first_pattern.as_slice().cmp(observation.first_pattern))
            .then_with(|| {
                self.second_pattern
                    .as_slice()
                    .cmp(observation.second_pattern)
            })
    }
}

impl Ord for PathCorrelationSample {
    fn cmp(&self, other: &Self) -> Ordering {
        self.compare_key(other.rank_hash, other.observation())
    }
}

impl PartialOrd for PathCorrelationSample {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Borrowed canonical logical state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathCorrelationState<'sketch> {
    /// Complete behavior and resource profile.
    pub profile: PathCorrelationProfile,
    /// Aggregate retained path-key and pattern bytes.
    pub sample_bytes: usize,
    /// Strictly sorted, duplicate-free retained bottom-k sample.
    pub samples: &'sketch [PathCorrelationSample],
}

/// Sample counts used to correct a binary-independence cardinality estimate.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PathCorrelationCounts {
    /// Retained observations.
    pub sample_size: usize,
    /// Retained observations matching the first-edge pattern.
    pub first_marginal: usize,
    /// Retained observations matching the second-edge pattern.
    pub second_marginal: usize,
    /// Retained observations matching both patterns.
    pub joint: usize,
    /// Whether the retained sample is provably the complete logical population.
    pub complete_population: bool,
}

impl PathCorrelationCounts {
    /// Returns a reduced sampled joint-to-independence ratio.
    ///
    /// The ratio is `joint * sample_size / (first_marginal *
    /// second_marginal)`. `None` means the sampled independence denominator is
    /// zero. This is a deterministic sample statistic; its statistical quality
    /// is governed separately by [`PathCorrelationAccuracy`].
    #[must_use]
    pub fn correlation_ratio(self) -> Option<PathCorrelationRatio> {
        let numerator = (self.joint as u128) * (self.sample_size as u128);
        let denominator = (self.first_marginal as u128) * (self.second_marginal as u128);
        if denominator == 0 {
            return None;
        }
        let divisor = greatest_common_divisor(numerator, denominator);
        Some(PathCorrelationRatio {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }
}

/// Reduced nonnegative rational sampled correlation ratio.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PathCorrelationRatio {
    /// Reduced numerator.
    pub numerator: u128,
    /// Reduced positive denominator.
    pub denominator: u128,
}

/// Explicit model under which sampled-frequency accuracy is interpreted.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PathCorrelationAccuracyModel {
    /// Each distinct path receives an independent continuous-uniform rank, so
    /// the retained bottom-k is a uniform sample without replacement.
    ///
    /// This idealization does not claim independence or continuity for the
    /// production 64-bit stable hash. It excludes finite-rank collisions,
    /// adversarially selected path bytes, serial dependence, selection bias,
    /// and instability when converting marginal errors into a ratio.
    IdealizedIndependentUniformContinuousRanks,
}

/// Model-qualified sampled-frequency error metadata.
///
/// For any one fixed joint or marginal predicate, population frequency is
/// modeled as a finite-population Bernoulli mean. Four modeled standard
/// deviations and the variance ceiling `1 / (4n)` give an additive-frequency
/// radius `2 / sqrt(n)` with Chebyshev confidence at least `15/16`. Integer
/// accessors round the radius outward.
///
/// This metadata does not bound the correlation ratio itself and must not be
/// promoted to an invariant.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PathCorrelationModeledAccuracy {
    sample_size: usize,
    deviation_multiplier: u64,
    variance_denominator: usize,
    model: PathCorrelationAccuracyModel,
}

impl PathCorrelationModeledAccuracy {
    /// Retained sample size used by the model.
    #[must_use]
    pub const fn sample_size(self) -> usize {
        self.sample_size
    }

    /// Number of modeled standard deviations admitted.
    #[must_use]
    pub const fn deviation_multiplier(self) -> u64 {
        self.deviation_multiplier
    }

    /// Conservative `floor(sqrt(sample_size))` denominator.
    #[must_use]
    pub const fn variance_denominator(self) -> usize {
        self.variance_denominator
    }

    /// Named idealized model.
    #[must_use]
    pub const fn model(self) -> PathCorrelationAccuracyModel {
        self.model
    }

    /// Additive joint-or-marginal frequency error in parts per million.
    #[must_use]
    pub fn additive_frequency_error_parts_per_million_ceiling(self) -> u64 {
        let numerator = 2_u128 * u128::from(PARTS_PER_MILLION);
        let denominator = self.variance_denominator as u128;
        u64::try_from(numerator.div_ceil(denominator))
            .unwrap_or(u64::MAX)
            .min(PARTS_PER_MILLION)
    }

    /// Model confidence floor in parts per million (`15/16`).
    #[must_use]
    pub const fn confidence_parts_per_million_floor(self) -> u64 {
        937_500
    }
}

/// Exact-or-modeled status of sampled joint and marginal frequencies.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PathCorrelationAccuracy {
    /// The unsaturated retained state contains the complete population.
    ExactCompletePopulation {
        /// Complete distinct path-observation count.
        population_size: usize,
    },
    /// The saturated state requires an explicit idealized sampling model.
    Modeled(PathCorrelationModeledAccuracy),
}

/// Bounded deterministic sampled two-path correlation table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathCorrelationSketch {
    profile: PathCorrelationProfile,
    samples: Vec<PathCorrelationSample>,
    sample_bytes: usize,
}

impl PathCorrelationSketch {
    /// Creates an empty sketch without allocating its maximum directory.
    pub fn try_new(profile: PathCorrelationProfile) -> Result<Self, PathCorrelationError> {
        validate_profile(profile)?;
        Ok(Self {
            profile,
            samples: Vec::new(),
            sample_bytes: 0,
        })
    }

    /// Complete immutable profile.
    #[must_use]
    pub const fn profile(&self) -> PathCorrelationProfile {
        self.profile
    }

    /// Number of retained distinct observations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether no observation is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Whether the sample may hide larger-ranked observations.
    #[must_use]
    pub fn is_saturated(&self) -> bool {
        self.samples.len() == self.profile.sample_capacity
    }

    /// Aggregate retained component bytes.
    #[must_use]
    pub const fn sample_bytes(&self) -> usize {
        self.sample_bytes
    }

    /// Canonical sorted and duplicate-free logical state.
    #[must_use]
    pub fn canonical_state(&self) -> PathCorrelationState<'_> {
        PathCorrelationState {
            profile: self.profile,
            sample_bytes: self.sample_bytes,
            samples: &self.samples,
        }
    }

    /// Exact-or-model-qualified accuracy metadata for joint and marginal counts.
    #[must_use]
    pub fn accuracy(&self) -> PathCorrelationAccuracy {
        if self.is_saturated() {
            PathCorrelationAccuracy::Modeled(PathCorrelationModeledAccuracy {
                sample_size: self.samples.len(),
                deviation_multiplier: 4,
                variance_denominator: self.samples.len().isqrt().max(1),
                model: PathCorrelationAccuracyModel::IdealizedIndependentUniformContinuousRanks,
            })
        } else {
            PathCorrelationAccuracy::ExactCompletePopulation {
                population_size: self.samples.len(),
            }
        }
    }

    /// Computes the stable rank for a profile-valid observation.
    pub fn try_rank(
        &self,
        observation: PathCorrelationObservation<'_>,
    ) -> Result<u64, PathCorrelationError> {
        validate_observation(self.profile, observation)?;
        stable_hash(self.profile.hash_algorithm, self.profile.seed, observation)
    }

    /// Observes one canonical two-edge path with distinct-set semantics.
    ///
    /// Every error leaves the logical state unchanged.
    pub fn try_observe(
        &mut self,
        observation: PathCorrelationObservation<'_>,
    ) -> Result<PathCorrelationObserveOutcome, PathCorrelationError> {
        let payload_bytes = validate_observation(self.profile, observation)?;
        let rank_hash = stable_hash(self.profile.hash_algorithm, self.profile.seed, observation)?;
        let insertion = self
            .samples
            .binary_search_by(|sample| sample.compare_key(rank_hash, observation));
        let index = match insertion {
            Ok(_) => return Ok(PathCorrelationObserveOutcome::AlreadyPresent),
            Err(index) => index,
        };
        if self.is_saturated() && index >= self.profile.sample_capacity {
            return Ok(PathCorrelationObserveOutcome::OutsideRetainedSample);
        }

        let next_sample_bytes = if self.is_saturated() {
            let removed_bytes = self
                .samples
                .last()
                .ok_or(PathCorrelationError::InvariantViolation)?
                .payload_bytes();
            self.sample_bytes
                .checked_sub(removed_bytes)
                .and_then(|bytes| bytes.checked_add(payload_bytes))
                .ok_or(PathCorrelationError::SampleByteCountOverflow)?
        } else {
            self.sample_bytes
                .checked_add(payload_bytes)
                .ok_or(PathCorrelationError::SampleByteCountOverflow)?
        };
        debug_assert!(next_sample_bytes <= self.profile.max_sample_bytes);

        if !self.is_saturated() {
            self.samples
                .try_reserve(1)
                .map_err(|_| PathCorrelationError::AllocationFailed {
                    target: PathCorrelationAllocationTarget::SampleDirectory,
                    requested: self.samples.len() + 1,
                })?;
        }
        let candidate = try_clone_sample(
            rank_hash,
            observation,
            PathCorrelationAllocationTarget::PathKey,
            PathCorrelationAllocationTarget::FirstPattern,
            PathCorrelationAllocationTarget::SecondPattern,
        )?;

        if self.is_saturated() {
            let removed = self
                .samples
                .pop()
                .ok_or(PathCorrelationError::InvariantViolation)?;
            debug_assert!(candidate < removed);
        }
        self.samples.insert(index, candidate);
        self.sample_bytes = next_sample_bytes;
        Ok(PathCorrelationObserveOutcome::Inserted)
    }

    /// Merges the exact bottom-k union of an identical-profile state.
    ///
    /// Successful profile-identical merges are commutative, associative, and
    /// idempotent because sorted duplicate elimination followed by bottom-k
    /// truncation is a join-semilattice operation.
    pub fn try_merge(&mut self, other: &Self) -> Result<(), PathCorrelationError> {
        if self.profile != other.profile {
            return Err(PathCorrelationError::ProfileMismatch);
        }
        let capacity = self.profile.sample_capacity;
        let merged_capacity = capacity.min(
            self.samples
                .len()
                .checked_add(other.samples.len())
                .ok_or(PathCorrelationError::ProfileSizeOverflow)?,
        );
        let mut merged = Vec::new();
        merged.try_reserve_exact(merged_capacity).map_err(|_| {
            PathCorrelationError::AllocationFailed {
                target: PathCorrelationAllocationTarget::MergeDirectory,
                requested: merged_capacity,
            }
        })?;

        let mut left = 0_usize;
        let mut right = 0_usize;
        let mut merged_bytes = 0_usize;
        while merged.len() < capacity && (left < self.samples.len() || right < other.samples.len())
        {
            let next = match (self.samples.get(left), other.samples.get(right)) {
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
            let cloned = try_clone_sample(
                next.rank_hash,
                next.observation(),
                PathCorrelationAllocationTarget::MergePayload,
                PathCorrelationAllocationTarget::MergePayload,
                PathCorrelationAllocationTarget::MergePayload,
            )?;
            merged_bytes = merged_bytes
                .checked_add(cloned.payload_bytes())
                .ok_or(PathCorrelationError::SampleByteCountOverflow)?;
            merged.push(cloned);
        }
        debug_assert!(merged_bytes <= self.profile.max_sample_bytes);
        self.samples = merged;
        self.sample_bytes = merged_bytes;
        Ok(())
    }

    /// Removes one observation exactly or returns a typed rebuild requirement.
    ///
    /// Failure does not mutate the sketch. An unretained observation is always
    /// an exact no-op: if it is outside a saturated sample, removing it cannot
    /// change the retained bottom-k.
    pub fn try_remove(
        &mut self,
        observation: PathCorrelationObservation<'_>,
    ) -> Result<PathCorrelationDeleteOutcome, PathCorrelationError> {
        validate_observation(self.profile, observation)?;
        let rank_hash = stable_hash(self.profile.hash_algorithm, self.profile.seed, observation)?;
        let Ok(index) = self
            .samples
            .binary_search_by(|sample| sample.compare_key(rank_hash, observation))
        else {
            return Ok(PathCorrelationDeleteOutcome::UnchangedUnretained);
        };
        if self.is_saturated() {
            return Err(PathCorrelationError::RebuildRequired { rank_hash });
        }
        let removed_payload_bytes = self
            .samples
            .get(index)
            .ok_or(PathCorrelationError::InvariantViolation)?
            .payload_bytes();
        let next_sample_bytes = self
            .sample_bytes
            .checked_sub(removed_payload_bytes)
            .ok_or(PathCorrelationError::InvariantViolation)?;
        let removed = self.samples.remove(index);
        debug_assert_eq!(removed.payload_bytes(), removed_payload_bytes);
        debug_assert_eq!(
            next_sample_bytes + removed.payload_bytes(),
            self.sample_bytes
        );
        self.sample_bytes = next_sample_bytes;
        Ok(PathCorrelationDeleteOutcome::Removed)
    }

    /// Counts retained joint and marginal occurrences for two patterns.
    #[must_use]
    pub fn sample_counts(
        &self,
        first_pattern: &[u8],
        second_pattern: &[u8],
    ) -> PathCorrelationCounts {
        let mut first_marginal = 0_usize;
        let mut second_marginal = 0_usize;
        let mut joint = 0_usize;
        for sample in &self.samples {
            let first_matches = sample.first_pattern == first_pattern;
            let second_matches = sample.second_pattern == second_pattern;
            first_marginal += usize::from(first_matches);
            second_marginal += usize::from(second_matches);
            joint += usize::from(first_matches && second_matches);
        }
        PathCorrelationCounts {
            sample_size: self.samples.len(),
            first_marginal,
            second_marginal,
            joint,
            complete_population: !self.is_saturated(),
        }
    }

    /// Encodes the complete profile and canonical retained state.
    pub fn try_to_canonical_bytes(&self) -> Result<Vec<u8>, PathCorrelationCodecError> {
        let layout = validate_canonical_state(self.profile, self.sample_bytes, &self.samples)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(layout.encoded_len).map_err(|_| {
            PathCorrelationCodecError::AllocationFailed {
                requested: layout.encoded_len,
            }
        })?;
        bytes.extend_from_slice(&CANONICAL_MAGIC);
        push_u16(&mut bytes, CANONICAL_VERSION);
        bytes.push(self.profile.hash_algorithm.canonical_tag());
        push_u64(&mut bytes, layout.sample_capacity);
        push_u64(&mut bytes, self.profile.seed);
        push_u64(&mut bytes, layout.max_path_key_bytes);
        push_u64(&mut bytes, layout.max_pattern_bytes);
        push_u64(&mut bytes, layout.max_sample_bytes);
        push_u64(&mut bytes, layout.sample_bytes);
        push_u64(&mut bytes, layout.sample_count);
        for sample in &self.samples {
            push_u64(&mut bytes, sample.rank_hash);
            push_u64(&mut bytes, canonical_usize(sample.path_key.len())?);
            push_u64(&mut bytes, canonical_usize(sample.first_pattern.len())?);
            push_u64(&mut bytes, canonical_usize(sample.second_pattern.len())?);
            bytes.extend_from_slice(&sample.path_key);
            bytes.extend_from_slice(&sample.first_pattern);
            bytes.extend_from_slice(&sample.second_pattern);
        }
        debug_assert_eq!(bytes.len(), layout.encoded_len);
        Ok(bytes)
    }

    /// Decodes exactly one canonical value under a trusted expected profile and
    /// independent caller-owned resource bounds.
    pub fn try_from_canonical_bytes(
        bytes: &[u8],
        expected_profile: PathCorrelationProfile,
        limits: PathCorrelationDecodeLimits,
    ) -> Result<Self, PathCorrelationCodecError> {
        let header = preflight_canonical_bytes(bytes, expected_profile, limits)?;
        let mut decoder = PathCorrelationDecoder::new(bytes);
        decoder.take(CANONICAL_HEADER_BYTES)?;

        let mut samples = Vec::new();
        samples
            .try_reserve_exact(header.sample_count)
            .map_err(|_| PathCorrelationCodecError::AllocationFailed {
                requested: header.sample_count,
            })?;
        for _ in 0..header.sample_count {
            let rank_hash = decoder.read_u64()?;
            let path_key_bytes = decoded_usize(decoder.read_u64()?)?;
            let first_pattern_bytes = decoded_usize(decoder.read_u64()?)?;
            let second_pattern_bytes = decoded_usize(decoder.read_u64()?)?;
            let path_key = decoder.take(path_key_bytes)?;
            let first_pattern = decoder.take(first_pattern_bytes)?;
            let second_pattern = decoder.take(second_pattern_bytes)?;
            samples.push(try_clone_sample(
                rank_hash,
                PathCorrelationObservation::new(path_key, first_pattern, second_pattern),
                PathCorrelationAllocationTarget::PathKey,
                PathCorrelationAllocationTarget::FirstPattern,
                PathCorrelationAllocationTarget::SecondPattern,
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CanonicalLayout {
    encoded_len: usize,
    sample_capacity: u64,
    max_path_key_bytes: u64,
    max_pattern_bytes: u64,
    max_sample_bytes: u64,
    sample_bytes: u64,
    sample_count: u64,
}

fn validate_profile(profile: PathCorrelationProfile) -> Result<(), PathCorrelationError> {
    if profile.sample_capacity == 0 {
        return Err(PathCorrelationError::EmptySampleCapacity);
    }
    if profile.max_path_key_bytes == 0 {
        return Err(PathCorrelationError::EmptyComponentLimit {
            component: PathCorrelationComponent::PathKey,
        });
    }
    if profile.max_pattern_bytes == 0 {
        return Err(PathCorrelationError::EmptyComponentLimit {
            component: PathCorrelationComponent::FirstPattern,
        });
    }
    let maximum_observation_bytes = profile
        .max_pattern_bytes
        .checked_mul(2)
        .and_then(|patterns| patterns.checked_add(profile.max_path_key_bytes))
        .ok_or(PathCorrelationError::ProfileSizeOverflow)?;
    let required = maximum_observation_bytes
        .checked_mul(profile.sample_capacity)
        .ok_or(PathCorrelationError::ProfileSizeOverflow)?;
    if profile.max_sample_bytes < required {
        return Err(PathCorrelationError::ProfileSampleByteLimitTooSmall {
            required,
            actual: profile.max_sample_bytes,
        });
    }
    Ok(())
}

fn validate_observation(
    profile: PathCorrelationProfile,
    observation: PathCorrelationObservation<'_>,
) -> Result<usize, PathCorrelationError> {
    validate_component(
        PathCorrelationComponent::PathKey,
        observation.path_key,
        profile.max_path_key_bytes,
    )?;
    validate_component(
        PathCorrelationComponent::FirstPattern,
        observation.first_pattern,
        profile.max_pattern_bytes,
    )?;
    validate_component(
        PathCorrelationComponent::SecondPattern,
        observation.second_pattern,
        profile.max_pattern_bytes,
    )?;
    observation
        .path_key
        .len()
        .checked_add(observation.first_pattern.len())
        .and_then(|bytes| bytes.checked_add(observation.second_pattern.len()))
        .ok_or(PathCorrelationError::SampleByteCountOverflow)
}

fn validate_component(
    component: PathCorrelationComponent,
    bytes: &[u8],
    maximum: usize,
) -> Result<(), PathCorrelationError> {
    if bytes.is_empty() {
        Err(PathCorrelationError::EmptyComponent { component })
    } else if bytes.len() > maximum {
        Err(PathCorrelationError::ComponentTooLarge {
            component,
            actual: bytes.len(),
            maximum,
        })
    } else {
        Ok(())
    }
}

fn validate_canonical_state(
    profile: PathCorrelationProfile,
    sample_bytes: usize,
    samples: &[PathCorrelationSample],
) -> Result<CanonicalLayout, PathCorrelationCodecError> {
    validate_profile(profile)?;
    if samples.len() > profile.sample_capacity {
        return Err(PathCorrelationCodecError::SampleCountExceedsProfile {
            actual: samples.len(),
            maximum: profile.sample_capacity,
        });
    }
    let mut actual_sample_bytes = 0_usize;
    let mut previous: Option<&PathCorrelationSample> = None;
    for (index, sample) in samples.iter().enumerate() {
        let observation = sample.observation();
        let payload_bytes = validate_observation(profile, observation)?;
        actual_sample_bytes = actual_sample_bytes
            .checked_add(payload_bytes)
            .ok_or(PathCorrelationError::SampleByteCountOverflow)?;
        let expected_hash = stable_hash(profile.hash_algorithm, profile.seed, observation)?;
        if sample.rank_hash != expected_hash {
            return Err(PathCorrelationCodecError::HashMismatch {
                index,
                actual: sample.rank_hash,
                expected: expected_hash,
            });
        }
        if let Some(prior) = previous {
            match prior.cmp(sample) {
                Ordering::Less => {}
                Ordering::Equal => {
                    return Err(PathCorrelationCodecError::DuplicateSample { index });
                }
                Ordering::Greater => {
                    return Err(PathCorrelationCodecError::SamplesOutOfOrder { index });
                }
            }
        }
        previous = Some(sample);
    }
    if actual_sample_bytes != sample_bytes {
        return Err(PathCorrelationCodecError::SampleByteCountMismatch {
            declared: sample_bytes,
            actual: actual_sample_bytes,
        });
    }
    if actual_sample_bytes > profile.max_sample_bytes {
        return Err(PathCorrelationError::ProfileSampleByteLimitTooSmall {
            required: actual_sample_bytes,
            actual: profile.max_sample_bytes,
        }
        .into());
    }
    let encoded_len = expected_canonical_len(samples.len(), sample_bytes)?;
    Ok(CanonicalLayout {
        encoded_len,
        sample_capacity: canonical_usize(profile.sample_capacity)?,
        max_path_key_bytes: canonical_usize(profile.max_path_key_bytes)?,
        max_pattern_bytes: canonical_usize(profile.max_pattern_bytes)?,
        max_sample_bytes: canonical_usize(profile.max_sample_bytes)?,
        sample_bytes: canonical_usize(sample_bytes)?,
        sample_count: canonical_usize(samples.len())?,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DecodedHeader {
    profile: PathCorrelationProfile,
    sample_bytes: usize,
    sample_count: usize,
}

type BorrowedSampleKey<'sample> = (u64, &'sample [u8], &'sample [u8], &'sample [u8]);

fn preflight_canonical_bytes(
    bytes: &[u8],
    expected_profile: PathCorrelationProfile,
    limits: PathCorrelationDecodeLimits,
) -> Result<DecodedHeader, PathCorrelationCodecError> {
    enforce_decode_limit(
        PathCorrelationDecodeResource::EncodedBytes,
        bytes.len(),
        limits.max_encoded_bytes,
    )?;
    let mut decoder = PathCorrelationDecoder::new(bytes);
    let magic = decoder.read_array::<8>()?;
    if magic != CANONICAL_MAGIC {
        return Err(PathCorrelationCodecError::MagicMismatch { actual: magic });
    }
    let version = decoder.read_u16()?;
    if version != CANONICAL_VERSION {
        return Err(PathCorrelationCodecError::UnsupportedVersion { actual: version });
    }
    let hash_algorithm = decode_hash_algorithm(decoder.read_u8()?)?;
    let sample_capacity = decoded_usize(decoder.read_u64()?)?;
    let seed = decoder.read_u64()?;
    let max_path_key_bytes = decoded_usize(decoder.read_u64()?)?;
    let max_pattern_bytes = decoded_usize(decoder.read_u64()?)?;
    let max_sample_bytes = decoded_usize(decoder.read_u64()?)?;
    let sample_bytes = decoded_usize(decoder.read_u64()?)?;
    let sample_count = decoded_usize(decoder.read_u64()?)?;

    enforce_decode_limit(
        PathCorrelationDecodeResource::Samples,
        sample_capacity,
        limits.max_samples,
    )?;
    enforce_decode_limit(
        PathCorrelationDecodeResource::Samples,
        sample_count,
        limits.max_samples,
    )?;
    enforce_decode_limit(
        PathCorrelationDecodeResource::PathKeyBytes,
        max_path_key_bytes,
        limits.max_path_key_bytes,
    )?;
    enforce_decode_limit(
        PathCorrelationDecodeResource::PatternBytes,
        max_pattern_bytes,
        limits.max_pattern_bytes,
    )?;
    enforce_decode_limit(
        PathCorrelationDecodeResource::SampleBytes,
        max_sample_bytes,
        limits.max_sample_bytes,
    )?;
    enforce_decode_limit(
        PathCorrelationDecodeResource::SampleBytes,
        sample_bytes,
        limits.max_sample_bytes,
    )?;

    let profile = PathCorrelationProfile {
        sample_capacity,
        hash_algorithm,
        seed,
        max_path_key_bytes,
        max_pattern_bytes,
        max_sample_bytes,
    };
    validate_profile(profile)?;
    if profile != expected_profile {
        return Err(PathCorrelationCodecError::ProfileMismatch {
            expected: expected_profile,
            actual: profile,
        });
    }
    if sample_count > sample_capacity {
        return Err(PathCorrelationCodecError::SampleCountExceedsProfile {
            actual: sample_count,
            maximum: sample_capacity,
        });
    }
    let expected_len = expected_canonical_len(sample_count, sample_bytes)?;
    if bytes.len() < expected_len {
        return Err(PathCorrelationCodecError::Truncated {
            offset: bytes.len(),
            needed: expected_len - bytes.len(),
            remaining: 0,
        });
    }

    let mut actual_sample_bytes = 0_usize;
    let mut previous: Option<BorrowedSampleKey<'_>> = None;
    for index in 0..sample_count {
        let rank_hash = decoder.read_u64()?;
        let path_key_bytes = decoded_usize(decoder.read_u64()?)?;
        let first_pattern_bytes = decoded_usize(decoder.read_u64()?)?;
        let second_pattern_bytes = decoded_usize(decoder.read_u64()?)?;
        enforce_decode_limit(
            PathCorrelationDecodeResource::PathKeyBytes,
            path_key_bytes,
            limits.max_path_key_bytes,
        )?;
        enforce_decode_limit(
            PathCorrelationDecodeResource::PatternBytes,
            first_pattern_bytes,
            limits.max_pattern_bytes,
        )?;
        enforce_decode_limit(
            PathCorrelationDecodeResource::PatternBytes,
            second_pattern_bytes,
            limits.max_pattern_bytes,
        )?;
        let path_key = decoder.take(path_key_bytes)?;
        let first_pattern = decoder.take(first_pattern_bytes)?;
        let second_pattern = decoder.take(second_pattern_bytes)?;
        let observation = PathCorrelationObservation::new(path_key, first_pattern, second_pattern);
        let payload_bytes = validate_observation(profile, observation)?;
        actual_sample_bytes = actual_sample_bytes
            .checked_add(payload_bytes)
            .ok_or(PathCorrelationError::SampleByteCountOverflow)?;
        enforce_decode_limit(
            PathCorrelationDecodeResource::SampleBytes,
            actual_sample_bytes,
            limits.max_sample_bytes,
        )?;
        let expected_hash = stable_hash(profile.hash_algorithm, profile.seed, observation)?;
        if rank_hash != expected_hash {
            return Err(PathCorrelationCodecError::HashMismatch {
                index,
                actual: rank_hash,
                expected: expected_hash,
            });
        }
        validate_sample_successor(
            previous,
            (rank_hash, path_key, first_pattern, second_pattern),
            index,
        )?;
        previous = Some((rank_hash, path_key, first_pattern, second_pattern));
    }
    if sample_bytes != actual_sample_bytes {
        return Err(PathCorrelationCodecError::SampleByteCountMismatch {
            declared: sample_bytes,
            actual: actual_sample_bytes,
        });
    }
    decoder.finish()?;
    Ok(DecodedHeader {
        profile,
        sample_bytes,
        sample_count,
    })
}

fn validate_sample_successor(
    previous: Option<BorrowedSampleKey<'_>>,
    current: BorrowedSampleKey<'_>,
    index: usize,
) -> Result<(), PathCorrelationCodecError> {
    let Some(previous) = previous else {
        return Ok(());
    };
    match compare_observation_keys(previous, current) {
        Ordering::Less => Ok(()),
        Ordering::Equal => Err(PathCorrelationCodecError::DuplicateSample { index }),
        Ordering::Greater => Err(PathCorrelationCodecError::SamplesOutOfOrder { index }),
    }
}

fn compare_observation_keys(left: BorrowedSampleKey<'_>, right: BorrowedSampleKey<'_>) -> Ordering {
    left.0
        .cmp(&right.0)
        .then_with(|| left.1.cmp(right.1))
        .then_with(|| left.2.cmp(right.2))
        .then_with(|| left.3.cmp(right.3))
}

fn enforce_decode_limit(
    resource: PathCorrelationDecodeResource,
    actual: usize,
    maximum: usize,
) -> Result<(), PathCorrelationCodecError> {
    if actual > maximum {
        Err(PathCorrelationCodecError::DecodeLimitExceeded {
            resource,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn expected_canonical_len(
    sample_count: usize,
    sample_bytes: usize,
) -> Result<usize, PathCorrelationCodecError> {
    let headers = sample_count
        .checked_mul(CANONICAL_SAMPLE_HEADER_BYTES)
        .ok_or(PathCorrelationCodecError::LengthOverflow)?;
    CANONICAL_HEADER_BYTES
        .checked_add(headers)
        .and_then(|length| length.checked_add(sample_bytes))
        .ok_or(PathCorrelationCodecError::LengthOverflow)
}

fn stable_hash(
    algorithm: PathCorrelationHashAlgorithm,
    seed: u64,
    observation: PathCorrelationObservation<'_>,
) -> Result<u64, PathCorrelationError> {
    let path_key_bytes = u64::try_from(observation.path_key.len())
        .map_err(|_| PathCorrelationError::IntegerUnrepresentable)?;
    let first_pattern_bytes = u64::try_from(observation.first_pattern.len())
        .map_err(|_| PathCorrelationError::IntegerUnrepresentable)?;
    let second_pattern_bytes = u64::try_from(observation.second_pattern.len())
        .map_err(|_| PathCorrelationError::IntegerUnrepresentable)?;
    match algorithm {
        PathCorrelationHashAlgorithm::SeededHasherV1 => {
            let mut hasher = SeededHasher::new(seed);
            hasher.write(HASH_DOMAIN);
            hasher.write_u64(path_key_bytes);
            hasher.write(observation.path_key);
            hasher.write_u64(first_pattern_bytes);
            hasher.write(observation.first_pattern);
            hasher.write_u64(second_pattern_bytes);
            hasher.write(observation.second_pattern);
            Ok(hasher.finish())
        }
    }
}

fn try_clone_sample(
    rank_hash: u64,
    observation: PathCorrelationObservation<'_>,
    path_target: PathCorrelationAllocationTarget,
    first_target: PathCorrelationAllocationTarget,
    second_target: PathCorrelationAllocationTarget,
) -> Result<PathCorrelationSample, PathCorrelationError> {
    Ok(PathCorrelationSample {
        rank_hash,
        path_key: try_clone_bytes(observation.path_key, path_target)?,
        first_pattern: try_clone_bytes(observation.first_pattern, first_target)?,
        second_pattern: try_clone_bytes(observation.second_pattern, second_target)?,
    })
}

fn try_clone_bytes(
    source: &[u8],
    target: PathCorrelationAllocationTarget,
) -> Result<Vec<u8>, PathCorrelationError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(source.len())
        .map_err(|_| PathCorrelationError::AllocationFailed {
            target,
            requested: source.len(),
        })?;
    bytes.extend_from_slice(source);
    Ok(bytes)
}

fn greatest_common_divisor(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left.max(1)
}

fn canonical_usize(value: usize) -> Result<u64, PathCorrelationCodecError> {
    u64::try_from(value).map_err(|_| PathCorrelationCodecError::IntegerUnrepresentable)
}

fn decoded_usize(value: u64) -> Result<usize, PathCorrelationCodecError> {
    usize::try_from(value).map_err(|_| PathCorrelationCodecError::IntegerUnrepresentable)
}

fn decode_hash_algorithm(
    actual: u8,
) -> Result<PathCorrelationHashAlgorithm, PathCorrelationCodecError> {
    match actual {
        value if value == PathCorrelationHashAlgorithm::SeededHasherV1.canonical_tag() => {
            Ok(PathCorrelationHashAlgorithm::SeededHasherV1)
        }
        actual => Err(PathCorrelationCodecError::UnsupportedHashAlgorithm { actual }),
    }
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

struct PathCorrelationDecoder<'bytes> {
    bytes: &'bytes [u8],
    offset: usize,
}

impl<'bytes> PathCorrelationDecoder<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, needed: usize) -> Result<&'bytes [u8], PathCorrelationCodecError> {
        let end = self
            .offset
            .checked_add(needed)
            .ok_or(PathCorrelationCodecError::LengthOverflow)?;
        let Some(value) = self.bytes.get(self.offset..end) else {
            return Err(PathCorrelationCodecError::Truncated {
                offset: self.offset,
                needed,
                remaining: self.bytes.len().saturating_sub(self.offset),
            });
        };
        self.offset = end;
        Ok(value)
    }

    fn read_array<const LENGTH: usize>(
        &mut self,
    ) -> Result<[u8; LENGTH], PathCorrelationCodecError> {
        let source = self.take(LENGTH)?;
        let mut value = [0_u8; LENGTH];
        value.copy_from_slice(source);
        Ok(value)
    }

    fn read_u8(&mut self) -> Result<u8, PathCorrelationCodecError> {
        Ok(self.read_array::<1>()?[0])
    }

    fn read_u16(&mut self) -> Result<u16, PathCorrelationCodecError> {
        Ok(u16::from_be_bytes(self.read_array::<2>()?))
    }

    fn read_u64(&mut self) -> Result<u64, PathCorrelationCodecError> {
        Ok(u64::from_be_bytes(self.read_array::<8>()?))
    }

    fn finish(self) -> Result<(), PathCorrelationCodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(PathCorrelationCodecError::TrailingBytes {
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

    const VERSION_OFFSET: usize = CANONICAL_MAGIC.len();
    const HASH_ALGORITHM_OFFSET: usize = VERSION_OFFSET + 2;
    const SAMPLE_CAPACITY_OFFSET: usize = HASH_ALGORITHM_OFFSET + 1;
    const SEED_OFFSET: usize = SAMPLE_CAPACITY_OFFSET + 8;
    const MAX_PATH_KEY_BYTES_OFFSET: usize = SEED_OFFSET + 8;
    const MAX_PATTERN_BYTES_OFFSET: usize = MAX_PATH_KEY_BYTES_OFFSET + 8;
    const MAX_SAMPLE_BYTES_OFFSET: usize = MAX_PATTERN_BYTES_OFFSET + 8;
    const SAMPLE_BYTES_OFFSET: usize = MAX_SAMPLE_BYTES_OFFSET + 8;
    const SAMPLE_COUNT_OFFSET: usize = SAMPLE_BYTES_OFFSET + 8;

    fn profile(sample_capacity: usize) -> PathCorrelationProfile {
        PathCorrelationProfile::new(
            sample_capacity,
            0x5041_5448_434f_5252,
            24,
            16,
            sample_capacity * (24 + 2 * 16),
        )
    }

    fn limits(
        profile: PathCorrelationProfile,
        encoded_bytes: usize,
    ) -> PathCorrelationDecodeLimits {
        PathCorrelationDecodeLimits::new(
            encoded_bytes,
            profile.sample_capacity,
            profile.max_path_key_bytes,
            profile.max_pattern_bytes,
            profile.max_sample_bytes,
        )
    }

    fn observation<'a>(
        path_key: &'a [u8],
        first_pattern: &'a [u8],
        second_pattern: &'a [u8],
    ) -> PathCorrelationObservation<'a> {
        PathCorrelationObservation::new(path_key, first_pattern, second_pattern)
    }

    fn observe_all(
        sketch: &mut PathCorrelationSketch,
        observations: &[PathCorrelationObservation<'_>],
    ) {
        for &value in observations {
            sketch.try_observe(value).expect("bounded observation");
        }
    }

    fn path_key(index: u64) -> [u8; 8] {
        index.to_be_bytes()
    }

    fn to_hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(bytes.len() * 2);
        for &byte in bytes {
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        output
    }

    #[test]
    fn profile_is_bounded_and_closed_under_merge() {
        assert_eq!(
            PathCorrelationSketch::try_new(PathCorrelationProfile::new(0, 1, 8, 8, 1)),
            Err(PathCorrelationError::EmptySampleCapacity)
        );
        assert_eq!(
            PathCorrelationSketch::try_new(PathCorrelationProfile::new(2, 1, 8, 8, 47)),
            Err(PathCorrelationError::ProfileSampleByteLimitTooSmall {
                required: 48,
                actual: 47,
            })
        );
        assert!(
            PathCorrelationSketch::try_new(PathCorrelationProfile::new(2, 1, 8, 8, 48)).is_ok()
        );
    }

    #[test]
    fn observations_are_order_independent_and_idempotent() {
        let keys = [path_key(1), path_key(2), path_key(3), path_key(4)];
        let values = [
            observation(&keys[0], b"knows", b"works_at"),
            observation(&keys[1], b"knows", b"lives_in"),
            observation(&keys[2], b"follows", b"works_at"),
            observation(&keys[3], b"follows", b"lives_in"),
        ];
        let mut forward = PathCorrelationSketch::try_new(profile(3)).expect("valid profile");
        let mut reverse = PathCorrelationSketch::try_new(profile(3)).expect("valid profile");
        observe_all(&mut forward, &values);
        for &value in values.iter().rev() {
            reverse.try_observe(value).expect("bounded observation");
        }
        assert_eq!(forward, reverse);
        let before = forward.clone();
        for value in values {
            forward
                .try_observe(value)
                .expect("duplicate or outside sample");
        }
        assert_eq!(forward, before);
        assert!(
            forward
                .canonical_state()
                .samples
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
    }

    #[test]
    fn invalid_observations_and_profile_mismatch_leave_state_unchanged() {
        let mut value = PathCorrelationSketch::try_new(profile(3)).expect("valid profile");
        let key = path_key(1);
        value
            .try_observe(observation(&key, b"a", b"x"))
            .expect("bounded observation");
        let before = value.clone();
        assert_eq!(
            value.try_observe(observation(b"", b"a", b"x")),
            Err(PathCorrelationError::EmptyComponent {
                component: PathCorrelationComponent::PathKey,
            })
        );
        assert_eq!(
            value.try_observe(observation(&key, b"pattern-too-large", b"x")),
            Err(PathCorrelationError::ComponentTooLarge {
                component: PathCorrelationComponent::FirstPattern,
                actual: 17,
                maximum: 16,
            })
        );
        assert_eq!(value, before);

        let other_profile = PathCorrelationProfile {
            seed: value.profile().seed ^ 1,
            ..value.profile()
        };
        let other = PathCorrelationSketch::try_new(other_profile).expect("valid profile");
        assert_eq!(
            value.try_merge(&other),
            Err(PathCorrelationError::ProfileMismatch)
        );
        assert_eq!(value, before);
    }

    #[test]
    fn merge_is_commutative_associative_and_idempotent() {
        let keys = (0..12_u64).map(path_key).collect::<Vec<_>>();
        let observations = keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                observation(
                    key,
                    if index % 2 == 0 { b"a" } else { b"b" },
                    if index % 3 == 0 { b"x" } else { b"y" },
                )
            })
            .collect::<Vec<_>>();
        let make_partition = |partition: usize| {
            let mut value = PathCorrelationSketch::try_new(profile(5)).expect("valid profile");
            for &item in observations
                .iter()
                .enumerate()
                .filter_map(|(index, item)| (index % 3 == partition).then_some(item))
            {
                value.try_observe(item).expect("bounded observation");
            }
            value
        };
        let a = make_partition(0);
        let b = make_partition(1);
        let c = make_partition(2);

        let mut ab = a.clone();
        ab.try_merge(&b).expect("matching profiles");
        let mut ba = b.clone();
        ba.try_merge(&a).expect("matching profiles");
        assert_eq!(ab, ba);

        let mut left_associative = a.clone();
        left_associative.try_merge(&b).expect("matching profiles");
        left_associative.try_merge(&c).expect("matching profiles");
        let mut right_tail = b.clone();
        right_tail.try_merge(&c).expect("matching profiles");
        let mut right_associative = a.clone();
        right_associative
            .try_merge(&right_tail)
            .expect("matching profiles");
        assert_eq!(left_associative, right_associative);

        let mut idempotent = a.clone();
        idempotent.try_merge(&a).expect("matching profiles");
        assert_eq!(idempotent, a);

        let mut direct = PathCorrelationSketch::try_new(profile(5)).expect("valid profile");
        observe_all(&mut direct, &observations);
        assert_eq!(left_associative, direct);
        assert_eq!(
            left_associative
                .try_to_canonical_bytes()
                .expect("valid state"),
            right_associative
                .try_to_canonical_bytes()
                .expect("valid state")
        );
    }

    #[test]
    fn deletion_is_exact_or_requests_rebuild_without_mutation() {
        let keys = [path_key(1), path_key(2), path_key(3)];
        let values = [
            observation(&keys[0], b"a", b"x"),
            observation(&keys[1], b"a", b"y"),
            observation(&keys[2], b"b", b"x"),
        ];
        let mut complete = PathCorrelationSketch::try_new(profile(4)).expect("valid profile");
        observe_all(&mut complete, &values[..2]);
        assert_eq!(
            complete.try_remove(values[0]),
            Ok(PathCorrelationDeleteOutcome::Removed)
        );
        assert_eq!(
            complete.try_remove(values[2]),
            Ok(PathCorrelationDeleteOutcome::UnchangedUnretained)
        );

        let mut saturated = PathCorrelationSketch::try_new(profile(2)).expect("valid profile");
        observe_all(&mut saturated, &values);
        let retained_sample = &saturated.canonical_state().samples[0];
        let retained_path_key = retained_sample.path_key().to_vec();
        let retained_first_pattern = retained_sample.first_pattern().to_vec();
        let retained_second_pattern = retained_sample.second_pattern().to_vec();
        let retained = observation(
            &retained_path_key,
            &retained_first_pattern,
            &retained_second_pattern,
        );
        let before = saturated.clone();
        assert!(matches!(
            saturated.try_remove(retained),
            Err(PathCorrelationError::RebuildRequired { .. })
        ));
        assert_eq!(saturated, before);

        let absent_key = path_key(99);
        let absent = observation(&absent_key, b"not", b"retained");
        assert_eq!(
            saturated.try_remove(absent),
            Ok(PathCorrelationDeleteOutcome::UnchangedUnretained)
        );
        assert_eq!(saturated, before);
    }

    #[test]
    fn counts_and_reduced_correlation_ratio_are_exact_on_complete_state() {
        let keys = (0..8_u64).map(path_key).collect::<Vec<_>>();
        let mut sketch = PathCorrelationSketch::try_new(profile(16)).expect("valid profile");
        for (index, key) in keys.iter().enumerate() {
            let first = if index < 4 { b"a".as_slice() } else { b"b" };
            let second = if matches!(index, 0 | 1 | 4) {
                b"x".as_slice()
            } else {
                b"y"
            };
            sketch
                .try_observe(observation(key, first, second))
                .expect("bounded observation");
        }
        let counts = sketch.sample_counts(b"a", b"x");
        assert_eq!(
            counts,
            PathCorrelationCounts {
                sample_size: 8,
                first_marginal: 4,
                second_marginal: 3,
                joint: 2,
                complete_population: true,
            }
        );
        assert_eq!(
            counts.correlation_ratio(),
            Some(PathCorrelationRatio {
                numerator: 4,
                denominator: 3,
            })
        );
        assert_eq!(
            sketch.sample_counts(b"missing", b"x").correlation_ratio(),
            None
        );
    }

    #[test]
    fn accuracy_metadata_is_explicitly_exact_or_model_qualified() {
        let mut complete = PathCorrelationSketch::try_new(profile(4)).expect("valid profile");
        let key = path_key(1);
        complete
            .try_observe(observation(&key, b"a", b"x"))
            .expect("bounded observation");
        assert_eq!(
            complete.accuracy(),
            PathCorrelationAccuracy::ExactCompletePopulation { population_size: 1 }
        );

        let mut sampled = PathCorrelationSketch::try_new(profile(256)).expect("valid profile");
        for index in 0..256_u64 {
            let key = path_key(index);
            sampled
                .try_observe(observation(&key, b"a", b"x"))
                .expect("bounded observation");
        }
        let PathCorrelationAccuracy::Modeled(metadata) = sampled.accuracy() else {
            unreachable!("a saturated sample uses modeled metadata");
        };
        assert_eq!(metadata.sample_size(), 256);
        assert_eq!(metadata.deviation_multiplier(), 4);
        assert_eq!(metadata.variance_denominator(), 16);
        assert_eq!(
            metadata.model(),
            PathCorrelationAccuracyModel::IdealizedIndependentUniformContinuousRanks
        );
        assert_eq!(
            metadata.additive_frequency_error_parts_per_million_ceiling(),
            125_000
        );
        assert_eq!(metadata.confidence_parts_per_million_floor(), 937_500);
    }

    #[test]
    fn canonical_codec_round_trips_and_collapses_input_order() {
        let keys = [path_key(1), path_key(2), path_key(3), path_key(4)];
        let values = [
            observation(&keys[0], b"knows", b"works_at"),
            observation(&keys[1], b"knows", b"lives_in"),
            observation(&keys[2], b"follows", b"works_at"),
            observation(&keys[3], b"follows", b"lives_in"),
        ];
        let mut forward = PathCorrelationSketch::try_new(profile(3)).expect("valid profile");
        let mut reverse = PathCorrelationSketch::try_new(profile(3)).expect("valid profile");
        observe_all(&mut forward, &values);
        for &value in values.iter().rev() {
            reverse.try_observe(value).expect("bounded observation");
        }
        let forward_bytes = forward.try_to_canonical_bytes().expect("valid state");
        let reverse_bytes = reverse.try_to_canonical_bytes().expect("valid state");
        assert_eq!(forward_bytes, reverse_bytes);
        assert_eq!(&forward_bytes[..8], b"FGDBPCR1");
        assert_eq!(
            &forward_bytes[VERSION_OFFSET..HASH_ALGORITHM_OFFSET],
            &1_u16.to_be_bytes()
        );
        assert_eq!(
            forward_bytes[HASH_ALGORITHM_OFFSET],
            PathCorrelationHashAlgorithm::SeededHasherV1.canonical_tag()
        );
        let decoded = PathCorrelationSketch::try_from_canonical_bytes(
            &forward_bytes,
            forward.profile(),
            limits(forward.profile(), forward_bytes.len()),
        )
        .expect("canonical state");
        assert_eq!(decoded, forward);
        assert_eq!(
            decoded.try_to_canonical_bytes().expect("valid state"),
            forward_bytes
        );
    }

    #[test]
    fn canonical_decoder_requires_profile_and_caller_owned_bounds() {
        let key = path_key(1);
        let mut value = PathCorrelationSketch::try_new(profile(2)).expect("valid profile");
        value
            .try_observe(observation(&key, b"a", b"x"))
            .expect("bounded observation");
        let encoded = value.try_to_canonical_bytes().expect("valid state");
        let expected_profile = value.profile();
        let exact_limits = limits(expected_profile, encoded.len());
        assert_eq!(
            PathCorrelationSketch::try_from_canonical_bytes(
                &encoded,
                expected_profile,
                exact_limits
            )
            .expect("exact limits"),
            value
        );
        let wrong_profile = PathCorrelationProfile {
            seed: expected_profile.seed ^ 1,
            ..expected_profile
        };
        assert!(matches!(
            PathCorrelationSketch::try_from_canonical_bytes(&encoded, wrong_profile, exact_limits),
            Err(PathCorrelationCodecError::ProfileMismatch { .. })
        ));
        let too_few_samples = PathCorrelationDecodeLimits {
            max_samples: 1,
            ..exact_limits
        };
        assert_eq!(
            PathCorrelationSketch::try_from_canonical_bytes(
                &encoded,
                expected_profile,
                too_few_samples
            ),
            Err(PathCorrelationCodecError::DecodeLimitExceeded {
                resource: PathCorrelationDecodeResource::Samples,
                actual: 2,
                maximum: 1,
            })
        );
        let too_few_bytes = PathCorrelationDecodeLimits {
            max_encoded_bytes: encoded.len() - 1,
            ..exact_limits
        };
        assert_eq!(
            PathCorrelationSketch::try_from_canonical_bytes(
                &encoded,
                expected_profile,
                too_few_bytes
            ),
            Err(PathCorrelationCodecError::DecodeLimitExceeded {
                resource: PathCorrelationDecodeResource::EncodedBytes,
                actual: encoded.len(),
                maximum: encoded.len() - 1,
            })
        );
    }

    #[test]
    fn malformed_canonical_values_fail_closed() {
        let keys = [path_key(1), path_key(2)];
        let mut value = PathCorrelationSketch::try_new(profile(3)).expect("valid profile");
        value
            .try_observe(observation(&keys[0], b"a", b"x"))
            .expect("bounded observation");
        value
            .try_observe(observation(&keys[1], b"b", b"y"))
            .expect("bounded observation");
        let encoded = value.try_to_canonical_bytes().expect("valid state");
        let expected_profile = value.profile();
        let exact_limits = limits(expected_profile, encoded.len() + 1);

        for cut in 0..encoded.len() {
            assert!(
                PathCorrelationSketch::try_from_canonical_bytes(
                    &encoded[..cut],
                    expected_profile,
                    exact_limits
                )
                .is_err(),
                "truncation at {cut} must fail"
            );
        }

        let mut wrong_magic = encoded.clone();
        wrong_magic[0] ^= 0xff;
        assert!(matches!(
            PathCorrelationSketch::try_from_canonical_bytes(
                &wrong_magic,
                expected_profile,
                exact_limits
            ),
            Err(PathCorrelationCodecError::MagicMismatch { .. })
        ));

        let mut wrong_version = encoded.clone();
        wrong_version[VERSION_OFFSET..HASH_ALGORITHM_OFFSET].copy_from_slice(&2_u16.to_be_bytes());
        assert_eq!(
            PathCorrelationSketch::try_from_canonical_bytes(
                &wrong_version,
                expected_profile,
                exact_limits
            ),
            Err(PathCorrelationCodecError::UnsupportedVersion { actual: 2 })
        );

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            PathCorrelationSketch::try_from_canonical_bytes(
                &trailing,
                expected_profile,
                exact_limits
            ),
            Err(PathCorrelationCodecError::TrailingBytes {
                offset: encoded.len(),
                remaining: 1,
            })
        );

        let mut wrong_hash = encoded.clone();
        wrong_hash[CANONICAL_HEADER_BYTES] ^= 0x80;
        assert!(matches!(
            PathCorrelationSketch::try_from_canonical_bytes(
                &wrong_hash,
                expected_profile,
                exact_limits
            ),
            Err(PathCorrelationCodecError::HashMismatch { index: 0, .. })
        ));
    }

    #[test]
    fn malformed_resource_headers_are_rejected_before_record_allocation() {
        let key = path_key(1);
        let mut value = PathCorrelationSketch::try_new(profile(2)).expect("valid profile");
        value
            .try_observe(observation(&key, b"a", b"x"))
            .expect("bounded observation");
        let encoded = value.try_to_canonical_bytes().expect("valid state");
        let expected_profile = value.profile();
        let exact_limits = limits(expected_profile, encoded.len());

        let mut huge_count = encoded.clone();
        huge_count[SAMPLE_COUNT_OFFSET..SAMPLE_COUNT_OFFSET + 8]
            .copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(matches!(
            PathCorrelationSketch::try_from_canonical_bytes(
                &huge_count,
                expected_profile,
                exact_limits
            ),
            Err(PathCorrelationCodecError::IntegerUnrepresentable)
                | Err(PathCorrelationCodecError::DecodeLimitExceeded {
                    resource: PathCorrelationDecodeResource::Samples,
                    ..
                })
        ));

        let mut excessive_payload = encoded.clone();
        excessive_payload[SAMPLE_BYTES_OFFSET..SAMPLE_BYTES_OFFSET + 8]
            .copy_from_slice(&(expected_profile.max_sample_bytes as u64 + 1).to_be_bytes());
        assert_eq!(
            PathCorrelationSketch::try_from_canonical_bytes(
                &excessive_payload,
                expected_profile,
                exact_limits
            ),
            Err(PathCorrelationCodecError::DecodeLimitExceeded {
                resource: PathCorrelationDecodeResource::SampleBytes,
                actual: expected_profile.max_sample_bytes + 1,
                maximum: expected_profile.max_sample_bytes,
            })
        );

        let mut excessive_path_key = encoded.clone();
        excessive_path_key[CANONICAL_HEADER_BYTES + 8..CANONICAL_HEADER_BYTES + 16]
            .copy_from_slice(&(expected_profile.max_path_key_bytes as u64 + 1).to_be_bytes());
        assert_eq!(
            PathCorrelationSketch::try_from_canonical_bytes(
                &excessive_path_key,
                expected_profile,
                exact_limits
            ),
            Err(PathCorrelationCodecError::DecodeLimitExceeded {
                resource: PathCorrelationDecodeResource::PathKeyBytes,
                actual: expected_profile.max_path_key_bytes + 1,
                maximum: expected_profile.max_path_key_bytes,
            })
        );
    }

    #[test]
    fn deterministic_rank_and_canonical_byte_vector_are_frozen() {
        let vector_profile = PathCorrelationProfile::new(1, 7, 1, 1, 3);
        let value_observation = observation(b"p", b"a", b"b");
        let mut value =
            PathCorrelationSketch::try_new(vector_profile).expect("valid vector profile");
        let rank = value
            .try_rank(value_observation)
            .expect("valid observation");
        value
            .try_observe(value_observation)
            .expect("valid observation");
        let bytes = value.try_to_canonical_bytes().expect("valid state");

        assert_eq!(rank, 0x545e_3020_a1d4_66d5);
        assert_eq!(
            to_hex(&bytes),
            "4647444250435231000101000000000000000100000000000000070000000000000001\
             0000000000000001000000000000000300000000000000030000000000000001\
             545e3020a1d466d5000000000000000100000000000000010000000000000001706162"
        );
    }

    #[test]
    fn named_graph_path_samples_are_deterministic_and_bounded() {
        const SAMPLE_CAPACITY: usize = 128;
        const MAX_FIXTURE_PATHS: usize = 4_096;
        for fixture in named_graph_fixtures() {
            let mut adjacency = vec![Vec::<u64>::new(); fixture.node_count];
            for &(left, right) in &fixture.edges {
                adjacency[left as usize].push(right);
                adjacency[right as usize].push(left);
            }
            for neighbors in &mut adjacency {
                neighbors.sort_unstable();
            }

            let mut path_keys = Vec::new();
            let mut first_patterns = Vec::new();
            let mut second_patterns = Vec::new();
            'paths: for (middle, neighbors) in adjacency.iter().enumerate() {
                for &start in neighbors {
                    for &end in neighbors {
                        if start == end {
                            continue;
                        }
                        let mut key = [0_u8; 24];
                        key[..8].copy_from_slice(&start.to_be_bytes());
                        key[8..16].copy_from_slice(&(middle as u64).to_be_bytes());
                        key[16..].copy_from_slice(&end.to_be_bytes());
                        path_keys.push(key);
                        first_patterns.push(if start % 2 == 0 {
                            b"endpoint:even".as_slice()
                        } else {
                            b"endpoint:odd".as_slice()
                        });
                        second_patterns.push(if end % 2 == 0 {
                            b"endpoint:even".as_slice()
                        } else {
                            b"endpoint:odd".as_slice()
                        });
                        if path_keys.len() == MAX_FIXTURE_PATHS {
                            break 'paths;
                        }
                    }
                }
            }
            assert!(
                !path_keys.is_empty(),
                "{} must expose two-edge paths",
                fixture.name
            );

            let fixture_profile = PathCorrelationProfile::new(
                SAMPLE_CAPACITY,
                0x4e41_4d45_4450_4154,
                24,
                16,
                SAMPLE_CAPACITY * (24 + 2 * 16),
            );
            let mut forward =
                PathCorrelationSketch::try_new(fixture_profile).expect("valid profile");
            let mut reverse =
                PathCorrelationSketch::try_new(fixture_profile).expect("valid profile");
            for index in 0..path_keys.len() {
                forward
                    .try_observe(observation(
                        &path_keys[index],
                        first_patterns[index],
                        second_patterns[index],
                    ))
                    .expect("bounded named-graph path");
            }
            for index in (0..path_keys.len()).rev() {
                reverse
                    .try_observe(observation(
                        &path_keys[index],
                        first_patterns[index],
                        second_patterns[index],
                    ))
                    .expect("bounded named-graph path");
            }
            assert_eq!(forward, reverse, "fixture={}", fixture.name);
            assert_eq!(
                forward.try_to_canonical_bytes().expect("valid state"),
                reverse.try_to_canonical_bytes().expect("valid state"),
                "fixture={}",
                fixture.name
            );
            assert_eq!(
                forward.len(),
                path_keys.len().min(SAMPLE_CAPACITY),
                "fixture={}",
                fixture.name
            );
            let counts = forward.sample_counts(b"endpoint:even", b"endpoint:even");
            assert_eq!(counts.sample_size, forward.len());
            assert_eq!(counts.complete_population, !forward.is_saturated());
            let exact_joint = (0..path_keys.len())
                .filter(|&index| {
                    first_patterns[index] == b"endpoint:even"
                        && second_patterns[index] == b"endpoint:even"
                })
                .count();
            let cross_product_error = (counts.joint as u128)
                .checked_mul(path_keys.len() as u128)
                .expect("bounded fixture product")
                .abs_diff(
                    (exact_joint as u128)
                        .checked_mul(counts.sample_size as u128)
                        .expect("bounded fixture product"),
                );
            let observed_error_ppm = u64::try_from(
                (cross_product_error * u128::from(PARTS_PER_MILLION))
                    .div_ceil((counts.sample_size as u128) * (path_keys.len() as u128)),
            )
            .expect("observed frequency error is at most one million ppm");
            let PathCorrelationAccuracy::Modeled(metadata) = forward.accuracy() else {
                unreachable!("all named-graph fixture populations saturate the sample");
            };
            assert!(
                observed_error_ppm <= metadata.additive_frequency_error_parts_per_million_ceiling(),
                "fixture={} exact_joint={exact_joint} population={} sampled_joint={} \
                 sample_size={} observed_error_ppm={observed_error_ppm} modeled_error_ppm={} \
                 model={:?}",
                fixture.name,
                path_keys.len(),
                counts.joint,
                counts.sample_size,
                metadata.additive_frequency_error_parts_per_million_ceiling(),
                metadata.model(),
            );
        }
    }

    #[test]
    fn canonical_header_offsets_are_frozen() {
        assert_eq!(CANONICAL_HEADER_BYTES, 67);
        assert_eq!(SAMPLE_CAPACITY_OFFSET, 11);
        assert_eq!(SEED_OFFSET, 19);
        assert_eq!(MAX_PATH_KEY_BYTES_OFFSET, 27);
        assert_eq!(MAX_PATTERN_BYTES_OFFSET, 35);
        assert_eq!(MAX_SAMPLE_BYTES_OFFSET, 43);
        assert_eq!(SAMPLE_BYTES_OFFSET, 51);
        assert_eq!(SAMPLE_COUNT_OFFSET, 59);
    }
}
