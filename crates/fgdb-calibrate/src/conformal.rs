//! Identity-bound graph-metric calibration over asupersync's conformal core.
//!
//! The foundation owns conformal quantile calculation. This module binds that
//! calculation to an immutable graph population, selection policy, sequence
//! window, regime, candidate decision, and pinned deterministic fallback. It
//! also turns every accepted assessment into bit-exact replay evidence.

use std::fmt;

use asupersync::lab::conformal::{HealthThresholdCalibrator, HealthThresholdConfig, ThresholdMode};

/// Maximum byte length accepted for a stable identity component.
pub const MAX_ID_BYTES: usize = 256;

/// Absolute resource ceiling for one exact calibration set.
///
/// The foundation retains every calibration value and may clone the set while
/// computing a threshold, so the caller-selected maximum is itself bounded.
pub const MAX_CALIBRATION_SAMPLES: usize = 1_048_576;

/// Absolute number of calibration plus assessment positions in one trial.
///
/// Besides bounding total evaluation work, this keeps every realized-coverage
/// counter exactly representable as `f64` when projecting the diagnostic rate.
pub const MAX_CONFORMAL_TRIAL_OBSERVATIONS: usize = 2_097_152;

/// Names an identity component that failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentityField {
    /// Graph metric identity.
    Metric,
    /// Population over which the metric is interpreted.
    Population,
    /// Selection policy used to form the calibration stream.
    Selection,
    /// Candidate decision-policy identity.
    CandidateDecision,
    /// Pinned deterministic fallback-policy identity.
    PinnedFallback,
}

impl fmt::Display for IdentityField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Metric => "metric",
            Self::Population => "population",
            Self::Selection => "selection",
            Self::CandidateDecision => "candidate decision",
            Self::PinnedFallback => "pinned fallback",
        })
    }
}

/// Construction failures for conformal identities, profiles, and trials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// An identity component was empty.
    EmptyIdentity(IdentityField),
    /// An identity component exceeded [`MAX_ID_BYTES`].
    IdentityTooLong {
        /// Component that exceeded the limit.
        field: IdentityField,
        /// Actual byte length.
        actual: usize,
        /// Maximum accepted byte length.
        maximum: usize,
    },
    /// An identity component contained a byte outside canonical printable ASCII.
    NonCanonicalIdentity {
        /// Component containing the invalid byte.
        field: IdentityField,
        /// Offset of the first invalid byte.
        offset: usize,
    },
    /// Candidate and fallback identities were equal.
    CandidateEqualsFallback,
    /// A sequence window ended before it began.
    ReversedWindow {
        /// Inclusive first sequence.
        first: u64,
        /// Inclusive last sequence.
        last: u64,
    },
    /// A sequence window's inclusive length was not representable.
    WindowLengthOverflow {
        /// Inclusive first sequence.
        first: u64,
        /// Inclusive last sequence.
        last: u64,
    },
    /// Alpha was not finite and strictly between zero and one.
    InvalidAlpha {
        /// Exact IEEE-754 bits supplied by the caller.
        alpha_bits: u64,
    },
    /// Minimum calibration sample count was zero.
    ZeroMinimumCalibrationSamples,
    /// Maximum calibration sample count was less than the minimum.
    MaximumBelowMinimum {
        /// Configured minimum.
        minimum: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// The selected maximum exceeded the exact-calibration resource ceiling.
    CalibrationMaximumTooLarge {
        /// Maximum requested by the caller.
        actual: usize,
        /// Absolute supported maximum.
        maximum: usize,
    },
    /// The platform's sample count could not be represented canonically.
    CalibrationBoundUnrepresentable {
        /// Unrepresentable bound.
        maximum: usize,
    },
    /// The calibration bound exceeded the complete trial window.
    CalibrationBoundExceedsWindow {
        /// Configured maximum calibration observations.
        maximum: usize,
        /// Number of events in the complete trial window.
        window_length: u64,
    },
    /// Consuming the maximum calibration set would leave no assessment slot.
    NoAssessmentCapacity {
        /// Configured maximum calibration observations.
        maximum: usize,
        /// Number of events in the complete trial window.
        window_length: u64,
    },
    /// The complete calibration-plus-assessment window exceeded its ceiling.
    TrialWindowTooLarge {
        /// Requested number of observations.
        actual: u64,
        /// Absolute supported number of observations.
        maximum: usize,
    },
    /// Space for an owned identity component could not be reserved.
    IdentityAllocationFailed(IdentityField),
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyIdentity(field) => write!(formatter, "{field} identity must not be empty"),
            Self::IdentityTooLong {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "{field} identity is {actual} bytes; maximum is {maximum}"
            ),
            Self::NonCanonicalIdentity { field, offset } => write!(
                formatter,
                "{field} identity contains a non-canonical byte at offset {offset}"
            ),
            Self::CandidateEqualsFallback => {
                formatter.write_str("candidate and pinned fallback identities must differ")
            }
            Self::ReversedWindow { first, last } => {
                write!(formatter, "sequence window {first}..={last} is reversed")
            }
            Self::WindowLengthOverflow { first, last } => write!(
                formatter,
                "sequence window {first}..={last} has an unrepresentable length"
            ),
            Self::InvalidAlpha { alpha_bits } => {
                write!(formatter, "alpha bits 0x{alpha_bits:016x} are invalid")
            }
            Self::ZeroMinimumCalibrationSamples => {
                formatter.write_str("minimum calibration samples must be greater than zero")
            }
            Self::MaximumBelowMinimum { minimum, maximum } => write!(
                formatter,
                "maximum calibration samples {maximum} is below minimum {minimum}"
            ),
            Self::CalibrationMaximumTooLarge { actual, maximum } => write!(
                formatter,
                "maximum calibration samples {actual} exceeds resource ceiling {maximum}"
            ),
            Self::CalibrationBoundUnrepresentable { maximum } => write!(
                formatter,
                "calibration bound {maximum} cannot be represented canonically"
            ),
            Self::CalibrationBoundExceedsWindow {
                maximum,
                window_length,
            } => write!(
                formatter,
                "calibration bound {maximum} exceeds trial-window length {window_length}"
            ),
            Self::NoAssessmentCapacity {
                maximum,
                window_length,
            } => write!(
                formatter,
                "calibration bound {maximum} leaves no assessment slot in trial-window length {window_length}"
            ),
            Self::TrialWindowTooLarge { actual, maximum } => write!(
                formatter,
                "conformal trial window {actual} exceeds resource ceiling {maximum}"
            ),
            Self::IdentityAllocationFailed(field) => {
                write!(formatter, "could not allocate the {field} identity")
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// A finite inclusive source-stream sequence window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SequenceWindow {
    first: u64,
    last: u64,
    length: u64,
}

impl SequenceWindow {
    /// Validates an inclusive sequence window.
    pub fn try_new(first: u64, last: u64) -> Result<Self, BuildError> {
        let distance = last
            .checked_sub(first)
            .ok_or(BuildError::ReversedWindow { first, last })?;
        let length = distance
            .checked_add(1)
            .ok_or(BuildError::WindowLengthOverflow { first, last })?;
        Ok(Self {
            first,
            last,
            length,
        })
    }

    /// Returns the first accepted sequence.
    #[must_use]
    pub const fn first(self) -> u64 {
        self.first
    }

    /// Returns the last accepted sequence.
    #[must_use]
    pub const fn last(self) -> u64 {
        self.last
    }

    /// Returns the inclusive number of sequence positions.
    #[must_use]
    pub const fn len(self) -> u64 {
        self.length
    }

    /// A validated sequence window is never empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        false
    }
}

/// Complete immutable identity of one graph-metric calibration trial.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphMetricIdentity {
    metric_id: String,
    population_id: String,
    selection_id: String,
    window: SequenceWindow,
    regime_epoch: u64,
    candidate_decision_id: String,
    pinned_fallback_id: String,
}

impl GraphMetricIdentity {
    /// Validates and owns a complete trial identity.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        metric_id: &str,
        population_id: &str,
        selection_id: &str,
        window: SequenceWindow,
        regime_epoch: u64,
        candidate_decision_id: &str,
        pinned_fallback_id: &str,
    ) -> Result<Self, BuildError> {
        let metric_id = copy_identity(IdentityField::Metric, metric_id)?;
        let population_id = copy_identity(IdentityField::Population, population_id)?;
        let selection_id = copy_identity(IdentityField::Selection, selection_id)?;
        let candidate_decision_id =
            copy_identity(IdentityField::CandidateDecision, candidate_decision_id)?;
        let pinned_fallback_id = copy_identity(IdentityField::PinnedFallback, pinned_fallback_id)?;
        if candidate_decision_id == pinned_fallback_id {
            return Err(BuildError::CandidateEqualsFallback);
        }

        Ok(Self {
            metric_id,
            population_id,
            selection_id,
            window,
            regime_epoch,
            candidate_decision_id,
            pinned_fallback_id,
        })
    }

    /// Returns the graph metric identity.
    #[must_use]
    pub fn metric_id(&self) -> &str {
        &self.metric_id
    }

    /// Returns the population identity.
    #[must_use]
    pub fn population_id(&self) -> &str {
        &self.population_id
    }

    /// Returns the calibration-selection identity.
    #[must_use]
    pub fn selection_id(&self) -> &str {
        &self.selection_id
    }

    /// Returns the complete source-stream window.
    #[must_use]
    pub const fn window(&self) -> SequenceWindow {
        self.window
    }

    /// Returns the measured regime epoch.
    #[must_use]
    pub const fn regime_epoch(&self) -> u64 {
        self.regime_epoch
    }

    /// Returns the candidate decision identity.
    #[must_use]
    pub fn candidate_decision_id(&self) -> &str {
        &self.candidate_decision_id
    }

    /// Returns the pinned deterministic fallback identity.
    #[must_use]
    pub fn pinned_fallback_id(&self) -> &str {
        &self.pinned_fallback_id
    }
}

/// Direction of a graph-metric conformal threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MetricThresholdMode {
    /// Only values above the calibrated threshold are nonconforming.
    Upper,
    /// Values far above or below the calibration median are nonconforming.
    TwoSided,
}

impl MetricThresholdMode {
    const fn foundation(self) -> ThresholdMode {
        match self {
            Self::Upper => ThresholdMode::Upper,
            Self::TwoSided => ThresholdMode::TwoSided,
        }
    }
}

/// Immutable conformal profile with a canonical floating-point identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConformalProfile {
    alpha_bits: u64,
    mode: MetricThresholdMode,
    minimum_calibration_samples: usize,
    maximum_calibration_samples: usize,
}

impl ConformalProfile {
    /// Validates a graph-metric conformal profile.
    pub fn try_new(
        alpha: f64,
        mode: MetricThresholdMode,
        minimum_calibration_samples: usize,
        maximum_calibration_samples: usize,
    ) -> Result<Self, BuildError> {
        validate_alpha(alpha)?;
        if minimum_calibration_samples == 0 {
            return Err(BuildError::ZeroMinimumCalibrationSamples);
        }
        if maximum_calibration_samples < minimum_calibration_samples {
            return Err(BuildError::MaximumBelowMinimum {
                minimum: minimum_calibration_samples,
                maximum: maximum_calibration_samples,
            });
        }
        if maximum_calibration_samples > MAX_CALIBRATION_SAMPLES {
            return Err(BuildError::CalibrationMaximumTooLarge {
                actual: maximum_calibration_samples,
                maximum: MAX_CALIBRATION_SAMPLES,
            });
        }
        if u64::try_from(maximum_calibration_samples).is_err() {
            return Err(BuildError::CalibrationBoundUnrepresentable {
                maximum: maximum_calibration_samples,
            });
        }

        Ok(Self {
            alpha_bits: alpha.to_bits(),
            mode,
            minimum_calibration_samples,
            maximum_calibration_samples,
        })
    }

    /// Returns alpha's exact canonical IEEE-754 bits.
    #[must_use]
    pub const fn alpha_bits(&self) -> u64 {
        self.alpha_bits
    }

    /// Returns the threshold direction.
    #[must_use]
    pub const fn mode(&self) -> MetricThresholdMode {
        self.mode
    }

    /// Returns the minimum calibration sample count.
    #[must_use]
    pub const fn minimum_calibration_samples(&self) -> usize {
        self.minimum_calibration_samples
    }

    /// Returns the hard maximum calibration sample count.
    #[must_use]
    pub const fn maximum_calibration_samples(&self) -> usize {
        self.maximum_calibration_samples
    }

    fn validated_foundation_config(&self) -> Result<HealthThresholdConfig, BuildError> {
        let alpha = f64::from_bits(self.alpha_bits);
        validate_alpha(alpha)?;
        if self.minimum_calibration_samples == 0 {
            return Err(BuildError::ZeroMinimumCalibrationSamples);
        }
        if self.maximum_calibration_samples < self.minimum_calibration_samples {
            return Err(BuildError::MaximumBelowMinimum {
                minimum: self.minimum_calibration_samples,
                maximum: self.maximum_calibration_samples,
            });
        }
        if self.maximum_calibration_samples > MAX_CALIBRATION_SAMPLES {
            return Err(BuildError::CalibrationMaximumTooLarge {
                actual: self.maximum_calibration_samples,
                maximum: MAX_CALIBRATION_SAMPLES,
            });
        }

        Ok(HealthThresholdConfig::new(alpha, self.mode.foundation())
            .min_samples(self.minimum_calibration_samples))
    }
}

/// An identity-bound graph-metric value at an exact stream sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencedMetricValue {
    identity: GraphMetricIdentity,
    profile: ConformalProfile,
    stream_sequence: u64,
    value_bits: u64,
}

impl SequencedMetricValue {
    /// Creates a value envelope without changing its IEEE-754 representation.
    #[must_use]
    pub fn new(
        identity: GraphMetricIdentity,
        profile: ConformalProfile,
        stream_sequence: u64,
        value: f64,
    ) -> Self {
        Self {
            identity,
            profile,
            stream_sequence,
            value_bits: value.to_bits(),
        }
    }

    /// Returns the complete trial identity.
    #[must_use]
    pub const fn identity(&self) -> &GraphMetricIdentity {
        &self.identity
    }

    /// Returns the immutable conformal profile.
    #[must_use]
    pub const fn profile(&self) -> &ConformalProfile {
        &self.profile
    }

    /// Returns the exact source-stream sequence.
    #[must_use]
    pub const fn stream_sequence(&self) -> u64 {
        self.stream_sequence
    }

    /// Returns the metric value.
    #[must_use]
    pub fn value(&self) -> f64 {
        f64::from_bits(self.value_bits)
    }

    /// Returns the value's exact IEEE-754 bits.
    #[must_use]
    pub const fn value_bits(&self) -> u64 {
        self.value_bits
    }
}

/// The only policy selections emitted for an accepted assessment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PolicySelection {
    /// Select the named candidate decision.
    CandidateDecision,
    /// Select the pinned deterministic fallback.
    PinnedFallback,
}

/// Why an accepted assessment selected its policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AssessmentDisposition {
    /// The candidate value conformed to a ready calibration set.
    CandidateConforming,
    /// The assessment began before the minimum calibration count was reached.
    CalibrationNotReady,
    /// The value was finite but outside the calibrated prediction set.
    OutsideCalibratedSet,
    /// The value conformed but realized coverage remained below the target.
    RealizedCoverageBelowTarget,
    /// A non-finite assessment value was conservatively rejected.
    NonFiniteValue,
    /// The foundation returned a non-finite threshold or score.
    NonFiniteFoundationStatistic,
    /// The foundation unexpectedly had no result for a reportedly ready metric.
    FoundationResultUnavailable,
    /// The foundation result did not name the wrapper's exact calibration set.
    FoundationCalibrationCountMismatch,
}

/// Deterministic evidence for one accepted calibration observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationEvidence {
    identity: GraphMetricIdentity,
    profile: ConformalProfile,
    stream_sequence: u64,
    value_bits: u64,
    calibration_samples: usize,
    ready: bool,
}

impl CalibrationEvidence {
    /// Returns the complete immutable trial identity.
    #[must_use]
    pub const fn identity(&self) -> &GraphMetricIdentity {
        &self.identity
    }

    /// Returns the complete immutable conformal profile.
    #[must_use]
    pub const fn profile(&self) -> &ConformalProfile {
        &self.profile
    }

    /// Returns the accepted source-stream sequence.
    #[must_use]
    pub const fn stream_sequence(&self) -> u64 {
        self.stream_sequence
    }

    /// Returns the accepted value's exact IEEE-754 bits.
    #[must_use]
    pub const fn value_bits(&self) -> u64 {
        self.value_bits
    }

    /// Returns the number of accepted calibration observations.
    #[must_use]
    pub const fn calibration_samples(&self) -> usize {
        self.calibration_samples
    }

    /// Returns whether the minimum calibration count has been reached.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        self.ready
    }
}

/// Deterministic evidence for one accepted graph-metric assessment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssessmentEvidence {
    identity: GraphMetricIdentity,
    profile: ConformalProfile,
    stream_sequence: u64,
    calibration_through_sequence: Option<u64>,
    calibration_samples: usize,
    foundation_calibration_samples: Option<usize>,
    value_bits: u64,
    threshold_bits: Option<u64>,
    nonconformity_score_bits: Option<u64>,
    coverage_target_bits: Option<u64>,
    conforming: Option<bool>,
    ready_assessments: u64,
    covered_ready_assessments: u64,
    realized_coverage_bits: Option<u64>,
    disposition: AssessmentDisposition,
    selection: PolicySelection,
}

impl AssessmentEvidence {
    /// Returns the complete immutable trial identity.
    #[must_use]
    pub const fn identity(&self) -> &GraphMetricIdentity {
        &self.identity
    }

    /// Returns the complete immutable conformal profile.
    #[must_use]
    pub const fn profile(&self) -> &ConformalProfile {
        &self.profile
    }

    /// Returns the assessment's exact source-stream sequence.
    #[must_use]
    pub const fn stream_sequence(&self) -> u64 {
        self.stream_sequence
    }

    /// Returns the last accepted calibration sequence.
    #[must_use]
    pub const fn calibration_through_sequence(&self) -> Option<u64> {
        self.calibration_through_sequence
    }

    /// Returns the number of calibration observations used.
    #[must_use]
    pub const fn calibration_samples(&self) -> usize {
        self.calibration_samples
    }

    /// Returns the calibration count projected from the foundation result.
    #[must_use]
    pub const fn foundation_calibration_samples(&self) -> Option<usize> {
        self.foundation_calibration_samples
    }

    /// Returns the assessed value's exact IEEE-754 bits.
    #[must_use]
    pub const fn value_bits(&self) -> u64 {
        self.value_bits
    }

    /// Returns the foundation threshold's exact IEEE-754 bits, when available.
    #[must_use]
    pub const fn threshold_bits(&self) -> Option<u64> {
        self.threshold_bits
    }

    /// Returns the nonconformity score's exact IEEE-754 bits, when available.
    #[must_use]
    pub const fn nonconformity_score_bits(&self) -> Option<u64> {
        self.nonconformity_score_bits
    }

    /// Returns the coverage target's exact IEEE-754 bits, when available.
    #[must_use]
    pub const fn coverage_target_bits(&self) -> Option<u64> {
        self.coverage_target_bits
    }

    /// Returns the foundation conformance result, when available.
    #[must_use]
    pub const fn conforming(&self) -> Option<bool> {
        self.conforming
    }

    /// Returns the number of ready assessments included in realized coverage.
    #[must_use]
    pub const fn ready_assessments(&self) -> u64 {
        self.ready_assessments
    }

    /// Returns the number of conforming ready assessments.
    #[must_use]
    pub const fn covered_ready_assessments(&self) -> u64 {
        self.covered_ready_assessments
    }

    /// Returns realized ready-assessment coverage as exact IEEE-754 bits.
    ///
    /// This is `None` when the current assessment had no ready foundation
    /// result. The exact rational counters remain available alongside the rate.
    #[must_use]
    pub const fn realized_coverage_bits(&self) -> Option<u64> {
        self.realized_coverage_bits
    }

    /// Returns why the selected policy was emitted.
    #[must_use]
    pub const fn disposition(&self) -> AssessmentDisposition {
        self.disposition
    }

    /// Returns the selected policy class.
    #[must_use]
    pub const fn selection(&self) -> PolicySelection {
        self.selection
    }

    /// Returns the selected immutable policy identity.
    #[must_use]
    pub fn selected_policy_id(&self) -> &str {
        match self.selection {
            PolicySelection::CandidateDecision => self.identity.candidate_decision_id(),
            PolicySelection::PinnedFallback => self.identity.pinned_fallback_id(),
        }
    }
}

/// Non-mutating failures while accepting a calibration value or assessment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputError {
    /// The envelope belongs to a different immutable trial identity.
    IdentityMismatch,
    /// The envelope names a different immutable profile.
    ProfileMismatch,
    /// The envelope did not carry the exact next stream sequence.
    UnexpectedSequence {
        /// Exact sequence required next.
        expected: u64,
        /// Sequence supplied by the caller.
        actual: u64,
    },
    /// The finite trial window has already been consumed.
    WindowComplete {
        /// Inclusive final sequence of the trial.
        last: u64,
    },
    /// Calibration was attempted after the first accepted assessment.
    CalibrationClosed,
    /// The configured calibration sample bound was reached.
    CalibrationLimitReached {
        /// Hard configured sample bound.
        maximum: usize,
    },
    /// A calibration value was NaN or infinite.
    NonFiniteCalibrationValue {
        /// Exact rejected IEEE-754 bits.
        value_bits: u64,
    },
    /// The internal calibration counter could not represent another sample.
    CalibrationCounterExhausted,
    /// The ready-assessment counter could not represent another result.
    ReadyAssessmentCounterExhausted,
    /// The covered ready-assessment counter could not represent another result.
    CoveredAssessmentCounterExhausted,
}

impl fmt::Display for InputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentityMismatch => formatter.write_str("metric trial identity does not match"),
            Self::ProfileMismatch => formatter.write_str("conformal profile does not match"),
            Self::UnexpectedSequence { expected, actual } => write!(
                formatter,
                "expected stream sequence {expected}, received {actual}"
            ),
            Self::WindowComplete { last } => {
                write!(formatter, "trial sequence window ended at {last}")
            }
            Self::CalibrationClosed => {
                formatter.write_str("calibration closed when assessment began")
            }
            Self::CalibrationLimitReached { maximum } => {
                write!(formatter, "calibration sample bound {maximum} was reached")
            }
            Self::NonFiniteCalibrationValue { value_bits } => write!(
                formatter,
                "calibration value bits 0x{value_bits:016x} are non-finite"
            ),
            Self::CalibrationCounterExhausted => {
                formatter.write_str("calibration sample counter is exhausted")
            }
            Self::ReadyAssessmentCounterExhausted => {
                formatter.write_str("ready-assessment counter is exhausted")
            }
            Self::CoveredAssessmentCounterExhausted => {
                formatter.write_str("covered ready-assessment counter is exhausted")
            }
        }
    }
}

impl std::error::Error for InputError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrialPhase {
    Calibration,
    Assessment,
}

/// A bounded, sequenced graph-metric trial backed by asupersync.
#[derive(Debug)]
pub struct GraphMetricConformal {
    identity: GraphMetricIdentity,
    profile: ConformalProfile,
    core: HealthThresholdCalibrator,
    phase: TrialPhase,
    next_sequence: Option<u64>,
    calibration_through_sequence: Option<u64>,
    calibration_samples: usize,
    ready_assessments: u64,
    covered_ready_assessments: u64,
}

impl GraphMetricConformal {
    /// Constructs a trial after validating every value that foundation
    /// constructors assume to be valid.
    pub fn try_new(
        identity: GraphMetricIdentity,
        profile: ConformalProfile,
    ) -> Result<Self, BuildError> {
        let maximum_trial_observations =
            u64::try_from(MAX_CONFORMAL_TRIAL_OBSERVATIONS).map_err(|_| {
                BuildError::CalibrationBoundUnrepresentable {
                    maximum: MAX_CONFORMAL_TRIAL_OBSERVATIONS,
                }
            })?;
        if identity.window.len() > maximum_trial_observations {
            return Err(BuildError::TrialWindowTooLarge {
                actual: identity.window.len(),
                maximum: MAX_CONFORMAL_TRIAL_OBSERVATIONS,
            });
        }
        let maximum = u64::try_from(profile.maximum_calibration_samples).map_err(|_| {
            BuildError::CalibrationBoundUnrepresentable {
                maximum: profile.maximum_calibration_samples,
            }
        })?;
        if maximum > identity.window.len() {
            return Err(BuildError::CalibrationBoundExceedsWindow {
                maximum: profile.maximum_calibration_samples,
                window_length: identity.window.len(),
            });
        }
        if maximum == identity.window.len() {
            return Err(BuildError::NoAssessmentCapacity {
                maximum: profile.maximum_calibration_samples,
                window_length: identity.window.len(),
            });
        }

        let config = profile.validated_foundation_config()?;
        let first_sequence = identity.window.first();
        let core = HealthThresholdCalibrator::new(config);
        Ok(Self {
            identity,
            profile,
            core,
            phase: TrialPhase::Calibration,
            next_sequence: Some(first_sequence),
            calibration_through_sequence: None,
            calibration_samples: 0,
            ready_assessments: 0,
            covered_ready_assessments: 0,
        })
    }

    /// Returns the complete immutable trial identity.
    #[must_use]
    pub const fn identity(&self) -> &GraphMetricIdentity {
        &self.identity
    }

    /// Returns the complete immutable profile.
    #[must_use]
    pub const fn profile(&self) -> &ConformalProfile {
        &self.profile
    }

    /// Returns the exact next accepted sequence, or `None` at window end.
    #[must_use]
    pub const fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
    }

    /// Returns the number of accepted calibration values.
    #[must_use]
    pub const fn calibration_samples(&self) -> usize {
        self.calibration_samples
    }

    /// Returns the number of ready assessments tracked for realized coverage.
    #[must_use]
    pub const fn ready_assessments(&self) -> u64 {
        self.ready_assessments
    }

    /// Returns the number of conforming ready assessments.
    #[must_use]
    pub const fn covered_ready_assessments(&self) -> u64 {
        self.covered_ready_assessments
    }

    /// Returns whether the foundation has reached the profile's minimum count.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.core.is_metric_calibrated(self.identity.metric_id())
    }

    /// Returns whether calibration values may still be accepted.
    #[must_use]
    pub const fn is_calibration_open(&self) -> bool {
        matches!(self.phase, TrialPhase::Calibration)
    }

    /// Accepts one finite, identity-bound calibration value at the exact next
    /// sequence.
    ///
    /// Every rejection check occurs before the foundation state is changed.
    pub fn calibrate(
        &mut self,
        sample: SequencedMetricValue,
    ) -> Result<CalibrationEvidence, InputError> {
        self.validate_envelope(&sample)?;
        if self.phase == TrialPhase::Assessment {
            return Err(InputError::CalibrationClosed);
        }
        if self.calibration_samples >= self.profile.maximum_calibration_samples {
            return Err(InputError::CalibrationLimitReached {
                maximum: self.profile.maximum_calibration_samples,
            });
        }

        let value = sample.value();
        if !value.is_finite() {
            return Err(InputError::NonFiniteCalibrationValue {
                value_bits: sample.value_bits,
            });
        }
        let next_count = self
            .calibration_samples
            .checked_add(1)
            .ok_or(InputError::CalibrationCounterExhausted)?;
        let stream_sequence = sample.stream_sequence;
        let next_sequence = sequence_after(stream_sequence, self.identity.window.last());

        self.core.calibrate(self.identity.metric_id(), value);
        self.calibration_samples = next_count;
        self.calibration_through_sequence = Some(stream_sequence);
        self.next_sequence = next_sequence;

        Ok(CalibrationEvidence {
            identity: sample.identity,
            profile: sample.profile,
            stream_sequence,
            value_bits: sample.value_bits,
            calibration_samples: next_count,
            ready: self.is_ready(),
        })
    }

    /// Assesses one value and emits either the candidate or the pinned fallback.
    ///
    /// The first accepted assessment permanently closes the split-calibration
    /// phase. Insufficient calibration and all unavailable/nonconforming
    /// results fail closed to the pinned fallback.
    pub fn assess(
        &mut self,
        sample: SequencedMetricValue,
    ) -> Result<AssessmentEvidence, InputError> {
        self.validate_envelope(&sample)?;

        let value = sample.value();
        let ready = self.is_ready();
        let (
            threshold_bits,
            score_bits,
            coverage_bits,
            conforming,
            foundation_calibration_samples,
            next_coverage,
            disposition,
            selection,
        ) = if !ready {
            let disposition = if value.is_finite() {
                AssessmentDisposition::CalibrationNotReady
            } else {
                AssessmentDisposition::NonFiniteValue
            };
            (
                None,
                None,
                None,
                None,
                None,
                None,
                disposition,
                PolicySelection::PinnedFallback,
            )
        } else {
            match self.core.check(self.identity.metric_id(), value) {
                Some(check) => {
                    let next_ready = self
                        .ready_assessments
                        .checked_add(1)
                        .ok_or(InputError::ReadyAssessmentCounterExhausted)?;
                    let next_covered = self
                        .covered_ready_assessments
                        .checked_add(u64::from(check.conforming))
                        .ok_or(InputError::CoveredAssessmentCounterExhausted)?;
                    let realized_coverage = (next_covered as f64) / (next_ready as f64);
                    let disposition = if !value.is_finite() {
                        AssessmentDisposition::NonFiniteValue
                    } else if !check.threshold.is_finite()
                        || !check.nonconformity_score.is_finite()
                        || !check.coverage_target.is_finite()
                    {
                        AssessmentDisposition::NonFiniteFoundationStatistic
                    } else if check.calibration_n != self.calibration_samples {
                        AssessmentDisposition::FoundationCalibrationCountMismatch
                    } else if !check.conforming {
                        AssessmentDisposition::OutsideCalibratedSet
                    } else if realized_coverage < check.coverage_target {
                        AssessmentDisposition::RealizedCoverageBelowTarget
                    } else {
                        AssessmentDisposition::CandidateConforming
                    };
                    let selection = if disposition == AssessmentDisposition::CandidateConforming {
                        PolicySelection::CandidateDecision
                    } else {
                        PolicySelection::PinnedFallback
                    };
                    (
                        Some(canonical_float_bits(check.threshold)),
                        Some(canonical_float_bits(check.nonconformity_score)),
                        Some(canonical_float_bits(check.coverage_target)),
                        Some(check.conforming),
                        Some(check.calibration_n),
                        Some((
                            next_ready,
                            next_covered,
                            canonical_float_bits(realized_coverage),
                        )),
                        disposition,
                        selection,
                    )
                }
                None => (
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    AssessmentDisposition::FoundationResultUnavailable,
                    PolicySelection::PinnedFallback,
                ),
            }
        };

        let (ready_assessments, covered_ready_assessments, realized_coverage_bits) =
            match next_coverage {
                Some((ready_assessments, covered_ready_assessments, realized_coverage_bits)) => {
                    self.ready_assessments = ready_assessments;
                    self.covered_ready_assessments = covered_ready_assessments;
                    (
                        ready_assessments,
                        covered_ready_assessments,
                        Some(realized_coverage_bits),
                    )
                }
                None => (self.ready_assessments, self.covered_ready_assessments, None),
            };

        let stream_sequence = sample.stream_sequence;
        self.phase = TrialPhase::Assessment;
        self.next_sequence = sequence_after(stream_sequence, self.identity.window.last());

        Ok(AssessmentEvidence {
            identity: sample.identity,
            profile: sample.profile,
            stream_sequence,
            calibration_through_sequence: self.calibration_through_sequence,
            calibration_samples: self.calibration_samples,
            foundation_calibration_samples,
            value_bits: sample.value_bits,
            threshold_bits,
            nonconformity_score_bits: score_bits,
            coverage_target_bits: coverage_bits,
            conforming,
            ready_assessments,
            covered_ready_assessments,
            realized_coverage_bits,
            disposition,
            selection,
        })
    }

    fn validate_envelope(&self, sample: &SequencedMetricValue) -> Result<(), InputError> {
        if sample.identity != self.identity {
            return Err(InputError::IdentityMismatch);
        }
        if sample.profile != self.profile {
            return Err(InputError::ProfileMismatch);
        }
        let Some(expected) = self.next_sequence else {
            return Err(InputError::WindowComplete {
                last: self.identity.window.last(),
            });
        };
        if sample.stream_sequence != expected {
            return Err(InputError::UnexpectedSequence {
                expected,
                actual: sample.stream_sequence,
            });
        }
        Ok(())
    }
}

fn validate_alpha(alpha: f64) -> Result<(), BuildError> {
    if alpha.is_finite() && alpha > 0.0 && alpha < 1.0 {
        Ok(())
    } else {
        Err(BuildError::InvalidAlpha {
            alpha_bits: alpha.to_bits(),
        })
    }
}

fn canonical_float_bits(value: f64) -> u64 {
    if value == 0.0 {
        0.0_f64.to_bits()
    } else {
        value.to_bits()
    }
}

fn sequence_after(current: u64, last: u64) -> Option<u64> {
    if current == last {
        None
    } else {
        current.checked_add(1)
    }
}

fn copy_identity(field: IdentityField, value: &str) -> Result<String, BuildError> {
    if value.is_empty() {
        return Err(BuildError::EmptyIdentity(field));
    }
    if value.len() > MAX_ID_BYTES {
        return Err(BuildError::IdentityTooLong {
            field,
            actual: value.len(),
            maximum: MAX_ID_BYTES,
        });
    }
    if let Some((offset, _)) = value
        .as_bytes()
        .iter()
        .enumerate()
        .find(|(_, byte)| !(0x21..=0x7e).contains(*byte))
    {
        return Err(BuildError::NonCanonicalIdentity { field, offset });
    }

    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| BuildError::IdentityAllocationFailed(field))?;
    owned.push_str(value);
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn window() -> Result<SequenceWindow, BuildError> {
        SequenceWindow::try_new(100, 120)
    }

    fn identity() -> Result<GraphMetricIdentity, BuildError> {
        GraphMetricIdentity::try_new(
            "metric:authorized-degree-p99",
            "population:tenant-7-vertices",
            "selection:keyed-snapshot-v1",
            window()?,
            12,
            "decision:csr-parallel-v4",
            "fallback:scalar-stable-v2",
        )
    }

    fn profile() -> Result<ConformalProfile, BuildError> {
        ConformalProfile::try_new(0.2, MetricThresholdMode::Upper, 5, 10)
    }

    fn trial() -> Result<GraphMetricConformal, BuildError> {
        GraphMetricConformal::try_new(identity()?, profile()?)
    }

    fn sample(sequence: u64, value: f64) -> Result<SequencedMetricValue, BuildError> {
        Ok(SequencedMetricValue::new(
            identity()?,
            profile()?,
            sequence,
            value,
        ))
    }

    fn calibrate(
        trial: &mut GraphMetricConformal,
        sequence: u64,
        value: f64,
    ) -> TestResult<CalibrationEvidence> {
        Ok(trial.calibrate(sample(sequence, value)?)?)
    }

    fn assess(
        trial: &mut GraphMetricConformal,
        sequence: u64,
        value: f64,
    ) -> TestResult<AssessmentEvidence> {
        Ok(trial.assess(sample(sequence, value)?)?)
    }

    fn calibrate_normal_range(trial: &mut GraphMetricConformal) -> TestResult {
        for offset in 0_u64..10 {
            let value = (offset + 1) as f64;
            let _ = calibrate(trial, 100 + offset, value)?;
        }
        Ok(())
    }

    #[test]
    fn construction_rejects_invalid_identity_profile_and_bounds() -> TestResult {
        assert_eq!(
            SequenceWindow::try_new(8, 7),
            Err(BuildError::ReversedWindow { first: 8, last: 7 })
        );
        assert_eq!(
            SequenceWindow::try_new(0, u64::MAX),
            Err(BuildError::WindowLengthOverflow {
                first: 0,
                last: u64::MAX
            })
        );
        assert!(matches!(
            GraphMetricIdentity::try_new("", "p", "s", window()?, 0, "c", "f"),
            Err(BuildError::EmptyIdentity(IdentityField::Metric))
        ));
        assert!(matches!(
            GraphMetricIdentity::try_new("m", "bad population", "s", window()?, 0, "c", "f"),
            Err(BuildError::NonCanonicalIdentity {
                field: IdentityField::Population,
                ..
            })
        ));
        assert_eq!(
            GraphMetricIdentity::try_new("m", "p", "s", window()?, 0, "same", "same"),
            Err(BuildError::CandidateEqualsFallback)
        );
        assert!(matches!(
            ConformalProfile::try_new(f64::NAN, MetricThresholdMode::Upper, 1, 2),
            Err(BuildError::InvalidAlpha { .. })
        ));
        assert_eq!(
            ConformalProfile::try_new(0.2, MetricThresholdMode::Upper, 0, 2),
            Err(BuildError::ZeroMinimumCalibrationSamples)
        );
        assert_eq!(
            ConformalProfile::try_new(0.2, MetricThresholdMode::Upper, 3, 2),
            Err(BuildError::MaximumBelowMinimum {
                minimum: 3,
                maximum: 2
            })
        );
        assert_eq!(
            ConformalProfile::try_new(
                0.2,
                MetricThresholdMode::Upper,
                1,
                MAX_CALIBRATION_SAMPLES + 1
            ),
            Err(BuildError::CalibrationMaximumTooLarge {
                actual: MAX_CALIBRATION_SAMPLES + 1,
                maximum: MAX_CALIBRATION_SAMPLES
            })
        );

        let too_large = ConformalProfile::try_new(0.2, MetricThresholdMode::Upper, 1, 22)?;
        assert!(matches!(
            GraphMetricConformal::try_new(identity()?, too_large),
            Err(BuildError::CalibrationBoundExceedsWindow {
                maximum: 22,
                window_length: 21
            })
        ));

        let maximum_trial = u64::try_from(MAX_CONFORMAL_TRIAL_OBSERVATIONS)?;
        let oversized_window = SequenceWindow::try_new(0, maximum_trial)?;
        let oversized_identity =
            GraphMetricIdentity::try_new("m", "p", "s", oversized_window, 0, "c", "f")?;
        let small_profile = ConformalProfile::try_new(0.2, MetricThresholdMode::Upper, 1, 1)?;
        assert!(matches!(
            GraphMetricConformal::try_new(oversized_identity, small_profile),
            Err(BuildError::TrialWindowTooLarge {
                actual,
                maximum: MAX_CONFORMAL_TRIAL_OBSERVATIONS,
            }) if actual == maximum_trial + 1
        ));

        let one_event = SequenceWindow::try_new(7, 7)?;
        let no_assessment_identity =
            GraphMetricIdentity::try_new("m", "p", "s", one_event, 0, "c", "f")?;
        let one_sample = ConformalProfile::try_new(0.2, MetricThresholdMode::Upper, 1, 1)?;
        assert!(matches!(
            GraphMetricConformal::try_new(no_assessment_identity, one_sample),
            Err(BuildError::NoAssessmentCapacity {
                maximum: 1,
                window_length: 1
            })
        ));
        Ok(())
    }

    #[test]
    fn sequencing_and_non_finite_rejections_leave_state_unchanged() -> TestResult {
        let mut trial = trial()?;
        let skipped = sample(101, 1.0)?;
        assert_eq!(
            trial.calibrate(skipped),
            Err(InputError::UnexpectedSequence {
                expected: 100,
                actual: 101
            })
        );
        assert_eq!(trial.next_sequence(), Some(100));
        assert_eq!(trial.calibration_samples(), 0);

        let non_finite = sample(100, f64::NAN)?;
        assert!(matches!(
            trial.calibrate(non_finite),
            Err(InputError::NonFiniteCalibrationValue { .. })
        ));
        assert_eq!(trial.next_sequence(), Some(100));
        assert_eq!(trial.calibration_samples(), 0);

        let accepted = calibrate(&mut trial, 100, 1.0)?;
        assert_eq!(accepted.stream_sequence(), 100);
        assert_eq!(trial.next_sequence(), Some(101));

        let duplicate = sample(100, 1.0)?;
        assert_eq!(
            trial.calibrate(duplicate),
            Err(InputError::UnexpectedSequence {
                expected: 101,
                actual: 100
            })
        );
        assert_eq!(trial.calibration_samples(), 1);
        Ok(())
    }

    #[test]
    fn readiness_tracks_the_foundation_minimum_exactly() -> TestResult {
        let mut trial = trial()?;
        assert!(!trial.is_ready());
        for offset in 0_u64..4 {
            let evidence = calibrate(&mut trial, 100 + offset, (offset + 1) as f64)?;
            assert!(!evidence.is_ready());
        }
        let fifth = calibrate(&mut trial, 104, 5.0)?;
        assert!(fifth.is_ready());
        assert!(trial.is_ready());
        assert_eq!(trial.calibration_samples(), 5);
        Ok(())
    }

    #[test]
    fn early_assessment_selects_fallback_and_closes_calibration() -> TestResult {
        let mut trial = trial()?;
        let _ = calibrate(&mut trial, 100, 1.0)?;
        let evidence = assess(&mut trial, 101, 1.0)?;
        assert_eq!(
            evidence.disposition(),
            AssessmentDisposition::CalibrationNotReady
        );
        assert_eq!(evidence.selection(), PolicySelection::PinnedFallback);
        assert_eq!(
            evidence.selected_policy_id(),
            identity()?.pinned_fallback_id()
        );
        assert_eq!(evidence.ready_assessments(), 0);
        assert_eq!(evidence.covered_ready_assessments(), 0);
        assert_eq!(evidence.realized_coverage_bits(), None);
        assert_eq!(
            trial.calibrate(sample(102, 2.0)?),
            Err(InputError::CalibrationClosed)
        );
        Ok(())
    }

    #[test]
    fn early_non_finite_assessment_records_its_immediate_failure() -> TestResult {
        let mut trial = trial()?;
        let evidence = assess(&mut trial, 100, f64::NAN)?;
        assert_eq!(
            evidence.disposition(),
            AssessmentDisposition::NonFiniteValue
        );
        assert_eq!(evidence.selection(), PolicySelection::PinnedFallback);
        assert_eq!(evidence.conforming(), None);
        assert_eq!(evidence.ready_assessments(), 0);
        assert_eq!(evidence.realized_coverage_bits(), None);
        Ok(())
    }

    #[test]
    fn outlier_selects_the_pinned_fallback() -> TestResult {
        let mut trial = trial()?;
        calibrate_normal_range(&mut trial)?;
        let evidence = assess(&mut trial, 110, 1_000.0)?;
        assert_eq!(
            evidence.disposition(),
            AssessmentDisposition::OutsideCalibratedSet
        );
        assert_eq!(evidence.selection(), PolicySelection::PinnedFallback);
        assert_eq!(
            evidence.selected_policy_id(),
            identity()?.pinned_fallback_id()
        );
        assert_eq!(evidence.conforming(), Some(false));
        assert!(evidence.threshold_bits().is_some());
        assert_eq!(evidence.foundation_calibration_samples(), Some(10));
        Ok(())
    }

    #[test]
    fn non_finite_assessment_selects_the_pinned_fallback() -> TestResult {
        let mut trial = trial()?;
        calibrate_normal_range(&mut trial)?;
        let evidence = assess(&mut trial, 110, f64::INFINITY)?;
        assert_eq!(
            evidence.disposition(),
            AssessmentDisposition::NonFiniteValue
        );
        assert_eq!(evidence.selection(), PolicySelection::PinnedFallback);
        assert_eq!(evidence.conforming(), Some(false));
        Ok(())
    }

    #[test]
    fn vacuous_foundation_threshold_cannot_select_the_candidate() -> TestResult {
        let conservative_profile =
            ConformalProfile::try_new(0.05, MetricThresholdMode::Upper, 5, 5)?;
        let mut trial = GraphMetricConformal::try_new(identity()?, conservative_profile.clone())?;
        for offset in 0_u64..5 {
            let envelope = SequencedMetricValue::new(
                identity()?,
                conservative_profile.clone(),
                100 + offset,
                (offset + 1) as f64,
            );
            let _ = trial.calibrate(envelope)?;
        }

        let envelope = SequencedMetricValue::new(identity()?, conservative_profile, 105, 3.0);
        let evidence = trial.assess(envelope)?;
        assert_eq!(evidence.conforming(), Some(true));
        assert_eq!(
            evidence.disposition(),
            AssessmentDisposition::NonFiniteFoundationStatistic
        );
        assert_eq!(evidence.selection(), PolicySelection::PinnedFallback);
        assert_eq!(
            evidence.selected_policy_id(),
            identity()?.pinned_fallback_id()
        );
        assert_eq!(evidence.threshold_bits(), Some(f64::INFINITY.to_bits()));
        Ok(())
    }

    #[test]
    fn deterministic_replay_produces_equal_bit_records() -> TestResult {
        fn replay() -> TestResult<Vec<AssessmentEvidence>> {
            let mut trial = trial()?;
            calibrate_normal_range(&mut trial)?;
            Ok(vec![
                assess(&mut trial, 110, 5.0)?,
                assess(&mut trial, 111, 100.0)?,
                assess(&mut trial, 112, f64::INFINITY)?,
            ])
        }

        let left = replay()?;
        let right = replay()?;
        assert_eq!(left, right);
        assert_eq!(left[0].value_bits(), 5.0_f64.to_bits());
        assert_eq!(left[0].ready_assessments(), 1);
        assert_eq!(left[0].covered_ready_assessments(), 1);
        assert_eq!(left[0].realized_coverage_bits(), Some(1.0_f64.to_bits()));
        assert_eq!(left[1].selection(), PolicySelection::PinnedFallback);
        assert_eq!(left[1].ready_assessments(), 2);
        assert_eq!(left[1].covered_ready_assessments(), 1);
        assert_eq!(left[1].realized_coverage_bits(), Some(0.5_f64.to_bits()));
        assert_eq!(left[2].value_bits(), f64::INFINITY.to_bits());
        assert_eq!(left[2].ready_assessments(), 3);
        assert_eq!(left[2].covered_ready_assessments(), 1);
        assert_eq!(
            left[2].realized_coverage_bits(),
            Some((1.0_f64 / 3.0).to_bits())
        );
        assert_eq!(left[0].identity().regime_epoch(), 12);
        assert_eq!(left[0].profile().alpha_bits(), 0.2_f64.to_bits());
        assert_eq!(left[0].foundation_calibration_samples(), Some(10));
        Ok(())
    }

    #[test]
    fn foundation_zero_statistics_are_canonicalized_without_changing_input_bits() -> TestResult {
        let mut trial = trial()?;
        for sequence in 100..=109 {
            let _ = calibrate(&mut trial, sequence, -0.0_f64)?;
        }
        let evidence = assess(&mut trial, 110, -0.0_f64)?;

        assert_eq!(evidence.value_bits(), (-0.0_f64).to_bits());
        assert_eq!(evidence.threshold_bits(), Some(0.0_f64.to_bits()));
        assert_eq!(evidence.nonconformity_score_bits(), Some(0.0_f64.to_bits()));
        assert_eq!(evidence.selection(), PolicySelection::CandidateDecision);
        Ok(())
    }

    #[test]
    fn degraded_realized_coverage_blocks_a_conforming_candidate() -> TestResult {
        let mut trial = trial()?;
        calibrate_normal_range(&mut trial)?;

        let initial = assess(&mut trial, 110, 5.0)?;
        assert_eq!(initial.selection(), PolicySelection::CandidateDecision);
        let shifted = assess(&mut trial, 111, 1_000.0)?;
        assert_eq!(shifted.selection(), PolicySelection::PinnedFallback);

        let conforming_but_degraded = assess(&mut trial, 112, 5.0)?;
        assert_eq!(conforming_but_degraded.conforming(), Some(true));
        assert_eq!(
            conforming_but_degraded.disposition(),
            AssessmentDisposition::RealizedCoverageBelowTarget
        );
        assert_eq!(
            conforming_but_degraded.selection(),
            PolicySelection::PinnedFallback
        );
        assert_eq!(conforming_but_degraded.ready_assessments(), 3);
        assert_eq!(conforming_but_degraded.covered_ready_assessments(), 2);
        assert_eq!(
            conforming_but_degraded.realized_coverage_bits(),
            Some((2.0_f64 / 3.0).to_bits())
        );
        Ok(())
    }

    #[test]
    fn coverage_counter_overflow_is_preflighted_without_state_change() -> TestResult {
        let mut ready_exhausted = trial()?;
        calibrate_normal_range(&mut ready_exhausted)?;
        ready_exhausted.ready_assessments = u64::MAX;
        let next_before = ready_exhausted.next_sequence();
        assert_eq!(
            ready_exhausted.assess(sample(110, 5.0)?),
            Err(InputError::ReadyAssessmentCounterExhausted)
        );
        assert_eq!(ready_exhausted.next_sequence(), next_before);
        assert!(ready_exhausted.is_calibration_open());
        assert_eq!(ready_exhausted.ready_assessments(), u64::MAX);

        let mut covered_exhausted = trial()?;
        calibrate_normal_range(&mut covered_exhausted)?;
        covered_exhausted.covered_ready_assessments = u64::MAX;
        let next_before = covered_exhausted.next_sequence();
        assert_eq!(
            covered_exhausted.assess(sample(110, 5.0)?),
            Err(InputError::CoveredAssessmentCounterExhausted)
        );
        assert_eq!(covered_exhausted.next_sequence(), next_before);
        assert!(covered_exhausted.is_calibration_open());
        assert_eq!(covered_exhausted.ready_assessments(), 0);
        assert_eq!(covered_exhausted.covered_ready_assessments(), u64::MAX);
        Ok(())
    }

    #[test]
    fn measured_regime_change_returns_to_the_pinned_fallback() -> TestResult {
        let two_sided = ConformalProfile::try_new(0.2, MetricThresholdMode::TwoSided, 5, 10)?;
        let mut trial = GraphMetricConformal::try_new(identity()?, two_sided.clone())?;
        let baseline = [
            98.0, 100.0, 102.0, 99.0, 101.0, 100.0, 98.0, 102.0, 99.0, 101.0,
        ];
        for (offset, value) in baseline.into_iter().enumerate() {
            let sequence = 100_u64 + u64::try_from(offset)?;
            let envelope =
                SequencedMetricValue::new(identity()?, two_sided.clone(), sequence, value);
            let _ = trial.calibrate(envelope)?;
        }

        let stable = SequencedMetricValue::new(identity()?, two_sided.clone(), 110, 100.0);
        let stable = trial.assess(stable)?;
        assert_eq!(stable.selection(), PolicySelection::CandidateDecision);

        for (sequence, shifted) in [(111, 500.0), (112, 520.0), (113, 540.0)] {
            let envelope =
                SequencedMetricValue::new(identity()?, two_sided.clone(), sequence, shifted);
            let evidence = trial.assess(envelope)?;
            assert_eq!(
                evidence.disposition(),
                AssessmentDisposition::OutsideCalibratedSet
            );
            assert_eq!(evidence.selection(), PolicySelection::PinnedFallback);
            assert_eq!(
                evidence.selected_policy_id(),
                identity()?.pinned_fallback_id()
            );
        }
        Ok(())
    }

    #[test]
    fn identity_and_profile_mismatches_are_non_mutating() -> TestResult {
        let mut trial = trial()?;
        let alternate_identity = GraphMetricIdentity::try_new(
            "metric:authorized-degree-p99",
            "population:tenant-8-vertices",
            "selection:keyed-snapshot-v1",
            window()?,
            12,
            "decision:csr-parallel-v4",
            "fallback:scalar-stable-v2",
        )?;
        let wrong_identity = SequencedMetricValue::new(alternate_identity, profile()?, 100, 1.0);
        assert_eq!(
            trial.calibrate(wrong_identity),
            Err(InputError::IdentityMismatch)
        );

        let alternate_profile = ConformalProfile::try_new(0.1, MetricThresholdMode::Upper, 5, 10)?;
        let wrong_profile = SequencedMetricValue::new(identity()?, alternate_profile, 100, 1.0);
        assert_eq!(
            trial.calibrate(wrong_profile),
            Err(InputError::ProfileMismatch)
        );
        assert_eq!(trial.next_sequence(), Some(100));
        assert_eq!(trial.calibration_samples(), 0);
        Ok(())
    }

    #[test]
    fn calibration_bound_and_window_end_are_enforced() -> TestResult {
        let bounded_profile = ConformalProfile::try_new(0.2, MetricThresholdMode::Upper, 1, 1)?;
        let one_window = SequenceWindow::try_new(7, 8)?;
        let one_identity =
            GraphMetricIdentity::try_new("m", "p", "s", one_window, 1, "candidate", "fallback")?;
        let mut one = GraphMetricConformal::try_new(one_identity.clone(), bounded_profile.clone())?;
        let first =
            SequencedMetricValue::new(one_identity.clone(), bounded_profile.clone(), 7, 1.0);
        assert!(one.calibrate(first).is_ok());
        assert_eq!(one.next_sequence(), Some(8));
        let assessment =
            SequencedMetricValue::new(one_identity.clone(), bounded_profile.clone(), 8, 2.0);
        assert!(one.assess(assessment).is_ok());
        assert_eq!(one.next_sequence(), None);
        let after = SequencedMetricValue::new(one_identity, bounded_profile, 9, 2.0);
        assert_eq!(
            one.assess(after),
            Err(InputError::WindowComplete { last: 8 })
        );

        let mut bounded = trial()?;
        calibrate_normal_range(&mut bounded)?;
        assert_eq!(
            bounded.calibrate(sample(110, 11.0)?),
            Err(InputError::CalibrationLimitReached { maximum: 10 })
        );
        assert_eq!(bounded.next_sequence(), Some(110));
        Ok(())
    }
}
