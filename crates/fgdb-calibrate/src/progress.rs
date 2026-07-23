//! Identity-bound drain-progress evidence.
//!
//! Asupersync owns the concentration-bound and progress-certificate engine.
//! This module binds that engine to immutable FrankenGraphDB identities,
//! bounded stream windows, exact sequence ordering, and a deterministic
//! fallback decision.

use core::fmt;

use asupersync::cancel::progress_certificate::{
    DrainPhase as FoundationDrainPhase, ProgressCertificate, ProgressConfig,
};
use fgdb_types::ObjectId;

/// Immutable identity of one drain-progress evidence stream.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DrainProgressIdentity {
    monitor_oid: ObjectId,
    filtration_oid: ObjectId,
    first_sequence: u64,
    last_sequence: u64,
    observation_capacity: u64,
    regime_epoch: u64,
    decision_oid: ObjectId,
    fallback_oid: ObjectId,
}

impl DrainProgressIdentity {
    /// Constructs a complete identity and its finite inclusive stream window.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        monitor_oid: ObjectId,
        filtration_oid: ObjectId,
        first_sequence: u64,
        last_sequence: u64,
        regime_epoch: u64,
        decision_oid: ObjectId,
        fallback_oid: ObjectId,
    ) -> Result<Self, ProgressBuildError> {
        let distance = last_sequence.checked_sub(first_sequence).ok_or(
            ProgressBuildError::ReversedWindow {
                first: first_sequence,
                last: last_sequence,
            },
        )?;
        let observation_capacity =
            distance
                .checked_add(1)
                .ok_or(ProgressBuildError::WindowLengthOverflow {
                    first: first_sequence,
                    last: last_sequence,
                })?;
        if decision_oid == fallback_oid {
            return Err(ProgressBuildError::DecisionEqualsFallback);
        }
        Ok(Self {
            monitor_oid,
            filtration_oid,
            first_sequence,
            last_sequence,
            observation_capacity,
            regime_epoch,
            decision_oid,
            fallback_oid,
        })
    }

    /// Stable monitor identity.
    #[must_use]
    pub const fn monitor_oid(self) -> ObjectId {
        self.monitor_oid
    }

    /// Stable filtration identity.
    #[must_use]
    pub const fn filtration_oid(self) -> ObjectId {
        self.filtration_oid
    }

    /// Inclusive first stream sequence.
    #[must_use]
    pub const fn first_sequence(self) -> u64 {
        self.first_sequence
    }

    /// Inclusive last stream sequence.
    #[must_use]
    pub const fn last_sequence(self) -> u64 {
        self.last_sequence
    }

    /// Number of sequence positions in the fixed window.
    #[must_use]
    pub const fn observation_capacity(self) -> u64 {
        self.observation_capacity
    }

    /// Regime epoch under which this evidence is meaningful.
    #[must_use]
    pub const fn regime_epoch(self) -> u64 {
        self.regime_epoch
    }

    /// Candidate decision selected by sufficient progress evidence.
    #[must_use]
    pub const fn decision_oid(self) -> ObjectId {
        self.decision_oid
    }

    /// Pinned deterministic fallback.
    #[must_use]
    pub const fn fallback_oid(self) -> ObjectId {
        self.fallback_oid
    }
}

/// Canonical, resource-bounded profile for asupersync's progress engine.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DrainProgressProfile {
    confidence_bits: u64,
    max_step_bound_bits: u64,
    stall_threshold: usize,
    min_observations: usize,
    epsilon_bits: u64,
    max_observations: usize,
}

impl DrainProgressProfile {
    /// Validates and canonicalizes a foundation profile plus a hard history cap.
    #[allow(clippy::cast_precision_loss)]
    pub fn try_new(
        config: ProgressConfig,
        max_observations: usize,
    ) -> Result<Self, ProgressBuildError> {
        config
            .validate()
            .map_err(|_| ProgressBuildError::InvalidFoundationConfig)?;
        if max_observations < config.min_observations {
            return Err(ProgressBuildError::ObservationLimitBelowMinimum {
                maximum: max_observations,
                minimum: config.min_observations,
            });
        }
        let max_observations_u64 = u64::try_from(max_observations).map_err(|_| {
            ProgressBuildError::ObservationLimitUnrepresentable {
                maximum: max_observations,
            }
        })?;
        let maximum_squared_step_sum = config.max_step_bound
            * config.max_step_bound
            * max_observations_u64.saturating_sub(1) as f64;
        if !maximum_squared_step_sum.is_finite() {
            return Err(ProgressBuildError::NumericAccumulatorOverflowRisk {
                max_step_bound_bits: canonical_float_bits(config.max_step_bound),
                max_observations,
            });
        }
        Ok(Self {
            confidence_bits: canonical_float_bits(config.confidence),
            max_step_bound_bits: canonical_float_bits(config.max_step_bound),
            stall_threshold: config.stall_threshold,
            min_observations: config.min_observations,
            epsilon_bits: canonical_float_bits(config.epsilon),
            max_observations,
        })
    }

    /// Confidence level bits.
    #[must_use]
    pub const fn confidence_bits(&self) -> u64 {
        self.confidence_bits
    }

    /// Maximum step-bound bits.
    #[must_use]
    pub const fn max_step_bound_bits(&self) -> u64 {
        self.max_step_bound_bits
    }

    /// Consecutive non-progress steps that constitute a stall.
    #[must_use]
    pub const fn stall_threshold(&self) -> usize {
        self.stall_threshold
    }

    /// Minimum observations required for a non-provisional verdict.
    #[must_use]
    pub const fn min_observations(&self) -> usize {
        self.min_observations
    }

    /// Floating comparison epsilon bits.
    #[must_use]
    pub const fn epsilon_bits(&self) -> u64 {
        self.epsilon_bits
    }

    /// Hard retained-observation ceiling.
    #[must_use]
    pub const fn max_observations(&self) -> usize {
        self.max_observations
    }

    fn foundation_config(&self) -> ProgressConfig {
        ProgressConfig {
            confidence: f64::from_bits(self.confidence_bits),
            max_step_bound: f64::from_bits(self.max_step_bound_bits),
            stall_threshold: self.stall_threshold,
            min_observations: self.min_observations,
            epsilon: f64::from_bits(self.epsilon_bits),
        }
    }
}

/// Profile or identity construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressBuildError {
    /// The inclusive stream window is reversed.
    ReversedWindow { first: u64, last: u64 },
    /// The inclusive stream-window length cannot be represented.
    WindowLengthOverflow { first: u64, last: u64 },
    /// Candidate and fallback identities must be distinct.
    DecisionEqualsFallback,
    /// Asupersync rejected the supplied progress configuration.
    InvalidFoundationConfig,
    /// The hard cap cannot reach the foundation's minimum sample count.
    ObservationLimitBelowMinimum { maximum: usize, minimum: usize },
    /// The hard cap cannot be represented in the canonical counter domain.
    ObservationLimitUnrepresentable { maximum: usize },
    /// The hard cap is larger than the identity's sequence window.
    ObservationLimitExceedsWindow { maximum: usize, window: u64 },
    /// Bound-respecting input could overflow the foundation's squared-delta sum.
    NumericAccumulatorOverflowRisk {
        max_step_bound_bits: u64,
        max_observations: usize,
    },
}

impl fmt::Display for ProgressBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ReversedWindow { first, last } => {
                write!(formatter, "progress window {first}..={last} is reversed")
            }
            Self::WindowLengthOverflow { first, last } => write!(
                formatter,
                "progress window {first}..={last} has an unrepresentable length"
            ),
            Self::DecisionEqualsFallback => {
                formatter.write_str("progress decision and fallback identities must differ")
            }
            Self::InvalidFoundationConfig => {
                formatter.write_str("asupersync rejected the progress configuration")
            }
            Self::ObservationLimitBelowMinimum { maximum, minimum } => write!(
                formatter,
                "progress observation limit {maximum} is below minimum {minimum}"
            ),
            Self::ObservationLimitUnrepresentable { maximum } => write!(
                formatter,
                "progress observation limit {maximum} is not representable"
            ),
            Self::ObservationLimitExceedsWindow { maximum, window } => write!(
                formatter,
                "progress observation limit {maximum} exceeds window length {window}"
            ),
            Self::NumericAccumulatorOverflowRisk {
                max_step_bound_bits,
                max_observations,
            } => write!(
                formatter,
                "progress step bound 0x{max_step_bound_bits:016x} across {max_observations} observations can overflow the squared-delta accumulator"
            ),
        }
    }
}

impl std::error::Error for ProgressBuildError {}

/// Identity- and profile-bound Lyapunov potential at one stream sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequencedPotential {
    identity: DrainProgressIdentity,
    profile: DrainProgressProfile,
    stream_sequence: u64,
    potential_bits: u64,
}

impl SequencedPotential {
    /// Creates an input envelope without accepting it into a monitor.
    #[must_use]
    pub fn new(
        identity: DrainProgressIdentity,
        profile: DrainProgressProfile,
        stream_sequence: u64,
        potential: f64,
    ) -> Self {
        Self {
            identity,
            profile,
            stream_sequence,
            potential_bits: canonical_float_bits(potential),
        }
    }

    /// Complete immutable trial identity.
    #[must_use]
    pub const fn identity(&self) -> DrainProgressIdentity {
        self.identity
    }

    /// Complete immutable profile.
    #[must_use]
    pub const fn profile(&self) -> &DrainProgressProfile {
        &self.profile
    }

    /// Exact source-stream sequence.
    #[must_use]
    pub const fn stream_sequence(&self) -> u64 {
        self.stream_sequence
    }

    /// Exact canonical potential bits.
    #[must_use]
    pub const fn potential_bits(&self) -> u64 {
        self.potential_bits
    }
}

/// Stable projection of asupersync's drain-phase vocabulary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DrainProgressPhase {
    Warmup,
    RapidDrain,
    SlowTail,
    Stalled,
    Quiescent,
}

/// Policy selected by the current progress verdict.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DrainProgressSelection {
    CandidateDecision,
    PinnedFallback,
}

/// Deterministic, immutable projection of one foundation verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrainProgressEvidence {
    identity: DrainProgressIdentity,
    profile: DrainProgressProfile,
    through_sequence: Option<u64>,
    total_observations: u64,
    current_potential_bits: u64,
    initial_potential_bits: u64,
    mean_credit_bits: u64,
    max_observed_step_bits: u64,
    estimated_remaining_steps_bits: Option<u64>,
    confidence_bound_bits: u64,
    azuma_bound_bits: u64,
    freedman_bound_bits: u64,
    empirical_variance_bits: Option<u64>,
    sufficient_observations: bool,
    step_bound_respected: bool,
    statistics_valid: bool,
    candidate_eligible: bool,
    converging: bool,
    stall_detected: bool,
    phase: DrainProgressPhase,
    selection: DrainProgressSelection,
}

impl DrainProgressEvidence {
    #[must_use]
    pub const fn identity(&self) -> DrainProgressIdentity {
        self.identity
    }

    #[must_use]
    pub const fn profile(&self) -> &DrainProgressProfile {
        &self.profile
    }

    #[must_use]
    pub const fn through_sequence(&self) -> Option<u64> {
        self.through_sequence
    }

    #[must_use]
    pub const fn total_observations(&self) -> u64 {
        self.total_observations
    }

    #[must_use]
    pub const fn current_potential_bits(&self) -> u64 {
        self.current_potential_bits
    }

    #[must_use]
    pub const fn initial_potential_bits(&self) -> u64 {
        self.initial_potential_bits
    }

    #[must_use]
    pub const fn mean_credit_bits(&self) -> u64 {
        self.mean_credit_bits
    }

    #[must_use]
    pub const fn max_observed_step_bits(&self) -> u64 {
        self.max_observed_step_bits
    }

    #[must_use]
    pub const fn estimated_remaining_steps_bits(&self) -> Option<u64> {
        self.estimated_remaining_steps_bits
    }

    #[must_use]
    pub const fn confidence_bound_bits(&self) -> u64 {
        self.confidence_bound_bits
    }

    #[must_use]
    pub const fn azuma_bound_bits(&self) -> u64 {
        self.azuma_bound_bits
    }

    #[must_use]
    pub const fn freedman_bound_bits(&self) -> u64 {
        self.freedman_bound_bits
    }

    #[must_use]
    pub const fn empirical_variance_bits(&self) -> Option<u64> {
        self.empirical_variance_bits
    }

    /// Whether the foundation has consumed its configured minimum sample count.
    #[must_use]
    pub const fn has_sufficient_observations(&self) -> bool {
        self.sufficient_observations
    }

    /// Whether every observed delta remained within the declared step bound.
    #[must_use]
    pub const fn step_bound_respected(&self) -> bool {
        self.step_bound_respected
    }

    /// Whether every projected numeric statistic is finite and in its domain.
    #[must_use]
    pub const fn statistics_valid(&self) -> bool {
        self.statistics_valid
    }

    /// Whether all wrapper gates permit selecting the candidate decision.
    #[must_use]
    pub const fn candidate_eligible(&self) -> bool {
        self.candidate_eligible
    }

    #[must_use]
    pub const fn is_converging(&self) -> bool {
        self.converging
    }

    #[must_use]
    pub const fn stall_detected(&self) -> bool {
        self.stall_detected
    }

    #[must_use]
    pub const fn phase(&self) -> DrainProgressPhase {
        self.phase
    }

    #[must_use]
    pub const fn selection(&self) -> DrainProgressSelection {
        self.selection
    }

    /// Exact selected policy identity.
    #[must_use]
    pub const fn selected_policy_oid(&self) -> ObjectId {
        match self.selection {
            DrainProgressSelection::CandidateDecision => self.identity.decision_oid,
            DrainProgressSelection::PinnedFallback => self.identity.fallback_oid,
        }
    }
}

/// Non-mutating input rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProgressObserveError {
    IdentityMismatch,
    ProfileMismatch,
    UnexpectedSequence { expected: u64, actual: u64 },
    WindowComplete { last: u64 },
    ObservationLimitReached { maximum: usize },
    NonFinitePotential { bits: u64 },
    NegativePotential { bits: u64 },
    CounterExhausted,
}

impl fmt::Display for ProgressObserveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::IdentityMismatch => formatter.write_str("progress identity does not match"),
            Self::ProfileMismatch => formatter.write_str("progress profile does not match"),
            Self::UnexpectedSequence { expected, actual } => {
                write!(
                    formatter,
                    "expected progress sequence {expected}, got {actual}"
                )
            }
            Self::WindowComplete { last } => {
                write!(formatter, "progress window ended at {last}")
            }
            Self::ObservationLimitReached { maximum } => {
                write!(formatter, "progress observation limit {maximum} reached")
            }
            Self::NonFinitePotential { bits } => {
                write!(formatter, "progress potential 0x{bits:016x} is non-finite")
            }
            Self::NegativePotential { bits } => {
                write!(formatter, "progress potential 0x{bits:016x} is negative")
            }
            Self::CounterExhausted => formatter.write_str("progress counter is exhausted"),
        }
    }
}

impl std::error::Error for ProgressObserveError {}

/// Bounded, exactly sequenced wrapper around [`ProgressCertificate`].
#[derive(Debug)]
pub struct DrainProgressMonitor {
    identity: DrainProgressIdentity,
    profile: DrainProgressProfile,
    core: ProgressCertificate,
    next_sequence: Option<u64>,
    through_sequence: Option<u64>,
    observations: usize,
}

impl DrainProgressMonitor {
    /// Constructs a monitor after validating every foundation precondition.
    pub fn try_new(
        identity: DrainProgressIdentity,
        profile: DrainProgressProfile,
    ) -> Result<Self, ProgressBuildError> {
        let max_observations = u64::try_from(profile.max_observations).map_err(|_| {
            ProgressBuildError::ObservationLimitUnrepresentable {
                maximum: profile.max_observations,
            }
        })?;
        if max_observations > identity.observation_capacity {
            return Err(ProgressBuildError::ObservationLimitExceedsWindow {
                maximum: profile.max_observations,
                window: identity.observation_capacity,
            });
        }
        let config = profile.foundation_config();
        config
            .validate()
            .map_err(|_| ProgressBuildError::InvalidFoundationConfig)?;
        let first_sequence = identity.first_sequence;
        Ok(Self {
            identity,
            profile,
            core: ProgressCertificate::new(config),
            next_sequence: Some(first_sequence),
            through_sequence: None,
            observations: 0,
        })
    }

    #[must_use]
    pub const fn identity(&self) -> DrainProgressIdentity {
        self.identity
    }

    #[must_use]
    pub const fn profile(&self) -> &DrainProgressProfile {
        &self.profile
    }

    #[must_use]
    pub const fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
    }

    #[must_use]
    pub const fn observations(&self) -> usize {
        self.observations
    }

    /// Current evidence without changing the monitor.
    #[must_use]
    pub fn evidence(&self) -> DrainProgressEvidence {
        self.project_verdict()
    }

    /// Accepts one exact sequence position after preflighting every rejection.
    pub fn observe(
        &mut self,
        input: SequencedPotential,
    ) -> Result<DrainProgressEvidence, ProgressObserveError> {
        if input.identity != self.identity {
            return Err(ProgressObserveError::IdentityMismatch);
        }
        if input.profile != self.profile {
            return Err(ProgressObserveError::ProfileMismatch);
        }
        let Some(expected) = self.next_sequence else {
            return Err(ProgressObserveError::WindowComplete {
                last: self.identity.last_sequence,
            });
        };
        if input.stream_sequence != expected {
            return Err(ProgressObserveError::UnexpectedSequence {
                expected,
                actual: input.stream_sequence,
            });
        }
        if self.observations >= self.profile.max_observations {
            return Err(ProgressObserveError::ObservationLimitReached {
                maximum: self.profile.max_observations,
            });
        }
        let potential = f64::from_bits(input.potential_bits);
        if !potential.is_finite() {
            return Err(ProgressObserveError::NonFinitePotential {
                bits: input.potential_bits,
            });
        }
        if potential < 0.0 {
            return Err(ProgressObserveError::NegativePotential {
                bits: input.potential_bits,
            });
        }
        let next_count = self
            .observations
            .checked_add(1)
            .ok_or(ProgressObserveError::CounterExhausted)?;
        let next_sequence = if input.stream_sequence == self.identity.last_sequence {
            None
        } else {
            Some(
                input
                    .stream_sequence
                    .checked_add(1)
                    .ok_or(ProgressObserveError::CounterExhausted)?,
            )
        };

        self.core.observe(potential);
        self.observations = next_count;
        self.through_sequence = Some(input.stream_sequence);
        self.next_sequence = next_sequence;
        Ok(self.project_verdict())
    }

    fn project_verdict(&self) -> DrainProgressEvidence {
        let verdict = self.core.verdict();
        let phase = project_phase(verdict.drain_phase);
        let sufficient_observations = verdict.total_steps >= self.profile.min_observations;
        let step_bound_respected =
            verdict.max_observed_step <= f64::from_bits(self.profile.max_step_bound_bits);
        let statistics_valid = verdict_statistics_valid(&verdict);
        let candidate_eligible = sufficient_observations
            && step_bound_respected
            && statistics_valid
            && (verdict.converging || phase == DrainProgressPhase::Quiescent)
            && !verdict.stall_detected;
        let selection = if candidate_eligible {
            DrainProgressSelection::CandidateDecision
        } else {
            DrainProgressSelection::PinnedFallback
        };
        DrainProgressEvidence {
            identity: self.identity,
            profile: self.profile.clone(),
            through_sequence: self.through_sequence,
            total_observations: canonical_count(verdict.total_steps),
            current_potential_bits: canonical_float_bits(verdict.current_potential),
            initial_potential_bits: canonical_float_bits(verdict.initial_potential),
            mean_credit_bits: canonical_float_bits(verdict.mean_credit),
            max_observed_step_bits: canonical_float_bits(verdict.max_observed_step),
            estimated_remaining_steps_bits: verdict
                .estimated_remaining_steps
                .map(canonical_float_bits),
            confidence_bound_bits: canonical_float_bits(verdict.confidence_bound),
            azuma_bound_bits: canonical_float_bits(verdict.azuma_bound),
            freedman_bound_bits: canonical_float_bits(verdict.freedman_bound),
            empirical_variance_bits: verdict.empirical_variance.map(canonical_float_bits),
            sufficient_observations,
            step_bound_respected,
            statistics_valid,
            candidate_eligible,
            converging: verdict.converging,
            stall_detected: verdict.stall_detected,
            phase,
            selection,
        }
    }
}

const fn project_phase(phase: FoundationDrainPhase) -> DrainProgressPhase {
    match phase {
        FoundationDrainPhase::Warmup => DrainProgressPhase::Warmup,
        FoundationDrainPhase::RapidDrain => DrainProgressPhase::RapidDrain,
        FoundationDrainPhase::SlowTail => DrainProgressPhase::SlowTail,
        FoundationDrainPhase::Stalled => DrainProgressPhase::Stalled,
        FoundationDrainPhase::Quiescent => DrainProgressPhase::Quiescent,
    }
}

fn canonical_float_bits(value: f64) -> u64 {
    if value.is_nan() {
        f64::NAN.to_bits()
    } else if value == 0.0 {
        0.0_f64.to_bits()
    } else {
        value.to_bits()
    }
}

fn verdict_statistics_valid(
    verdict: &asupersync::cancel::progress_certificate::CertificateVerdict,
) -> bool {
    nonnegative_finite(verdict.current_potential)
        && nonnegative_finite(verdict.initial_potential)
        && nonnegative_finite(verdict.mean_credit)
        && nonnegative_finite(verdict.max_observed_step)
        && verdict
            .estimated_remaining_steps
            .is_none_or(nonnegative_finite)
        && probability(verdict.confidence_bound)
        && probability(verdict.azuma_bound)
        && probability(verdict.freedman_bound)
        && verdict.empirical_variance.is_none_or(nonnegative_finite)
}

fn nonnegative_finite(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

fn probability(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn canonical_count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(fill: u8) -> ObjectId {
        ObjectId([fill; 32])
    }

    fn identity() -> Result<DrainProgressIdentity, ProgressBuildError> {
        DrainProgressIdentity::try_new(oid(1), oid(2), 100, 119, 7, oid(3), oid(4))
    }

    fn profile() -> Result<DrainProgressProfile, ProgressBuildError> {
        DrainProgressProfile::try_new(
            ProgressConfig {
                confidence: 0.9,
                max_step_bound: 25.0,
                stall_threshold: 3,
                min_observations: 3,
                epsilon: 1e-12,
            },
            20,
        )
    }

    fn monitor() -> Result<DrainProgressMonitor, ProgressBuildError> {
        DrainProgressMonitor::try_new(identity()?, profile()?)
    }

    fn input(sequence: u64, potential: f64) -> Result<SequencedPotential, ProgressBuildError> {
        Ok(SequencedPotential::new(
            identity()?,
            profile()?,
            sequence,
            potential,
        ))
    }

    #[test]
    fn construction_rejects_invalid_windows_profiles_and_aliases()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            DrainProgressIdentity::try_new(oid(1), oid(2), 9, 8, 0, oid(3), oid(4)),
            Err(ProgressBuildError::ReversedWindow { first: 9, last: 8 })
        );
        assert_eq!(
            DrainProgressIdentity::try_new(oid(1), oid(2), 0, 1, 0, oid(3), oid(3)),
            Err(ProgressBuildError::DecisionEqualsFallback)
        );
        assert!(matches!(
            DrainProgressProfile::try_new(
                ProgressConfig {
                    confidence: f64::NAN,
                    ..ProgressConfig::default()
                },
                10
            ),
            Err(ProgressBuildError::InvalidFoundationConfig)
        ));
        assert_eq!(
            DrainProgressProfile::try_new(ProgressConfig::default(), 2),
            Err(ProgressBuildError::ObservationLimitBelowMinimum {
                maximum: 2,
                minimum: ProgressConfig::default().min_observations,
            })
        );
        assert_eq!(
            DrainProgressProfile::try_new(
                ProgressConfig {
                    max_step_bound: f64::MAX,
                    min_observations: 2,
                    ..ProgressConfig::default()
                },
                2,
            ),
            Err(ProgressBuildError::NumericAccumulatorOverflowRisk {
                max_step_bound_bits: f64::MAX.to_bits(),
                max_observations: 2,
            })
        );

        let short_identity =
            DrainProgressIdentity::try_new(oid(1), oid(2), 0, 2, 0, oid(3), oid(4))?;
        let long_profile = DrainProgressProfile::try_new(ProgressConfig::default(), 5)?;
        assert!(matches!(
            DrainProgressMonitor::try_new(short_identity, long_profile),
            Err(ProgressBuildError::ObservationLimitExceedsWindow {
                maximum: 5,
                window: 3,
            })
        ));
        Ok(())
    }

    #[test]
    fn sequence_and_value_rejections_are_atomic() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = monitor()?;
        let before = value.evidence();
        let wrong_identity =
            DrainProgressIdentity::try_new(oid(9), oid(2), 100, 119, 7, oid(3), oid(4))?;
        assert_eq!(
            value.observe(SequencedPotential::new(
                wrong_identity,
                profile()?,
                100,
                10.0,
            )),
            Err(ProgressObserveError::IdentityMismatch)
        );
        assert_eq!(value.evidence(), before);

        let wrong_profile = DrainProgressProfile::try_new(
            ProgressConfig {
                confidence: 0.8,
                max_step_bound: 25.0,
                stall_threshold: 3,
                min_observations: 3,
                epsilon: 1e-12,
            },
            20,
        )?;
        assert_eq!(
            value.observe(SequencedPotential::new(
                identity()?,
                wrong_profile,
                100,
                10.0,
            )),
            Err(ProgressObserveError::ProfileMismatch)
        );
        assert_eq!(value.evidence(), before);

        assert_eq!(
            value.observe(input(101, 10.0)?),
            Err(ProgressObserveError::UnexpectedSequence {
                expected: 100,
                actual: 101,
            })
        );
        assert_eq!(value.evidence(), before);
        assert_eq!(
            value.observe(input(100, f64::INFINITY)?),
            Err(ProgressObserveError::NonFinitePotential {
                bits: f64::INFINITY.to_bits(),
            })
        );
        assert_eq!(value.evidence(), before);
        assert_eq!(
            value.observe(input(100, -1.0)?),
            Err(ProgressObserveError::NegativePotential {
                bits: (-1.0_f64).to_bits(),
            })
        );
        assert_eq!(value.evidence(), before);
        Ok(())
    }

    #[test]
    fn descending_potential_selects_candidate() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = monitor()?;
        let mut evidence = value.evidence();
        for (offset, potential) in [100.0, 75.0, 50.0, 25.0, 0.0].into_iter().enumerate() {
            let sequence = 100_u64 + u64::try_from(offset)?;
            evidence = value.observe(input(sequence, potential)?)?;
        }
        assert_eq!(evidence.phase(), DrainProgressPhase::Quiescent);
        assert!(evidence.has_sufficient_observations());
        assert!(evidence.step_bound_respected());
        assert!(evidence.statistics_valid());
        assert!(evidence.candidate_eligible());
        assert_eq!(
            evidence.selection(),
            DrainProgressSelection::CandidateDecision
        );
        assert_eq!(evidence.selected_policy_oid(), oid(3));
        Ok(())
    }

    #[test]
    fn stalled_potential_keeps_pinned_fallback() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = monitor()?;
        let mut evidence = value.evidence();
        for sequence in 100..106 {
            evidence = value.observe(input(sequence, 50.0)?)?;
        }
        assert!(evidence.stall_detected());
        assert_eq!(evidence.phase(), DrainProgressPhase::Stalled);
        assert!(!evidence.candidate_eligible());
        assert_eq!(evidence.selection(), DrainProgressSelection::PinnedFallback);
        assert_eq!(evidence.selected_policy_oid(), oid(4));
        Ok(())
    }

    #[test]
    fn insufficient_quiescence_keeps_pinned_fallback() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = monitor()?;
        let first = value.observe(input(100, 0.0)?)?;
        let second = value.observe(input(101, 0.0)?)?;

        assert_eq!(first.phase(), DrainProgressPhase::Warmup);
        assert_eq!(second.phase(), DrainProgressPhase::Warmup);
        assert!(!second.has_sufficient_observations());
        assert!(!second.candidate_eligible());
        assert_eq!(second.selection(), DrainProgressSelection::PinnedFallback);
        Ok(())
    }

    #[test]
    fn exceeded_step_bound_blocks_candidate_even_at_quiescence()
    -> Result<(), Box<dyn std::error::Error>> {
        let bounded_identity =
            DrainProgressIdentity::try_new(oid(1), oid(2), 200, 202, 7, oid(3), oid(4))?;
        let bounded_profile = DrainProgressProfile::try_new(
            ProgressConfig {
                confidence: 0.9,
                max_step_bound: 5.0,
                stall_threshold: 3,
                min_observations: 3,
                epsilon: 1e-12,
            },
            3,
        )?;
        let mut value = DrainProgressMonitor::try_new(bounded_identity, bounded_profile.clone())?;
        let mut evidence = value.evidence();
        for (offset, potential) in [20.0, 10.0, 0.0].into_iter().enumerate() {
            evidence = value.observe(SequencedPotential::new(
                bounded_identity,
                bounded_profile.clone(),
                200 + u64::try_from(offset)?,
                potential,
            ))?;
        }

        assert_eq!(evidence.phase(), DrainProgressPhase::Quiescent);
        assert!(evidence.has_sufficient_observations());
        assert!(!evidence.step_bound_respected());
        assert!(evidence.statistics_valid());
        assert!(!evidence.candidate_eligible());
        assert_eq!(evidence.selection(), DrainProgressSelection::PinnedFallback);
        Ok(())
    }

    #[test]
    fn identical_inputs_replay_to_identical_evidence() -> Result<(), Box<dyn std::error::Error>> {
        let mut left = monitor()?;
        let mut right = monitor()?;
        for (offset, potential) in [80.0, 55.0, 34.0, 21.0, 13.0].into_iter().enumerate() {
            let sequence = 100_u64 + u64::try_from(offset)?;
            let left_evidence = left.observe(input(sequence, potential)?)?;
            let right_evidence = right.observe(input(sequence, potential)?)?;
            assert_eq!(left_evidence, right_evidence);
        }
        assert_eq!(left.evidence(), right.evidence());
        Ok(())
    }

    #[test]
    fn hard_observation_limit_is_enforced() -> Result<(), Box<dyn std::error::Error>> {
        let bounded_profile = DrainProgressProfile::try_new(
            ProgressConfig {
                min_observations: 2,
                ..ProgressConfig::default()
            },
            2,
        )?;
        let bounded_identity =
            DrainProgressIdentity::try_new(oid(1), oid(2), 10, 12, 0, oid(3), oid(4))?;
        let mut value = DrainProgressMonitor::try_new(bounded_identity, bounded_profile.clone())?;
        let make = |sequence, potential| {
            SequencedPotential::new(
                bounded_identity,
                bounded_profile.clone(),
                sequence,
                potential,
            )
        };
        value.observe(make(10, 2.0))?;
        value.observe(make(11, 1.0))?;
        let before = value.evidence();
        assert_eq!(
            value.observe(make(12, 0.0)),
            Err(ProgressObserveError::ObservationLimitReached { maximum: 2 })
        );
        assert_eq!(value.evidence(), before);
        Ok(())
    }

    #[test]
    fn floating_projection_canonicalizes_zero_and_nan() {
        assert_eq!(
            canonical_float_bits(-0.0),
            canonical_float_bits(0.0),
            "signed zero must not split deterministic evidence identity"
        );
        let payload_nan = f64::from_bits(0x7ff8_0000_0000_0042);
        assert_eq!(
            canonical_float_bits(payload_nan),
            f64::NAN.to_bits(),
            "NaN payloads must collapse to one canonical evidence value"
        );
    }
}
