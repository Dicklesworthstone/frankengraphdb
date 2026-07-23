//! Identity-bound exploration-budget evidence.
//!
//! Asupersync owns the finite-sample estimator. This module binds that
//! estimator to immutable FrankenGraphDB identities, an exactly sequenced
//! observation window, explicit assumption attestations, bounded evaluation
//! work, bit-exact evidence, and a deterministic pinned fallback.

use core::fmt;

pub use asupersync::lab::exploration_budget::ExplorationBudgetConfig;
use asupersync::lab::exploration_budget::{ExplorationBudget, ExplorationBudgetAssumptions};
use fgdb_types::ObjectId;

/// Absolute number of observed plus projected runs accepted by one estimate.
///
/// The foundation materializes projection vectors, so a caller-provided work
/// budget alone is not a sufficient allocation bound.
pub const MAX_EXPLORATION_PROJECTED_RUNS: usize = 1_048_576;

/// Absolute implementation ceiling for one foundation estimate.
pub const MAX_EXPLORATION_ESTIMATION_WORK: u128 = 100_000_000;

/// Immutable identity of one exploration-budget decision window.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExplorationBudgetIdentity {
    budget_oid: ObjectId,
    window_oid: ObjectId,
    regime_oid: ObjectId,
    regime_epoch: u64,
    first_sequence: u64,
    last_sequence: u64,
    observation_capacity: u64,
    candidate_decision_oid: ObjectId,
    pinned_fallback_oid: ObjectId,
}

impl ExplorationBudgetIdentity {
    /// Constructs a complete identity and its finite inclusive stream window.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        budget_oid: ObjectId,
        window_oid: ObjectId,
        regime_oid: ObjectId,
        regime_epoch: u64,
        first_sequence: u64,
        last_sequence: u64,
        candidate_decision_oid: ObjectId,
        pinned_fallback_oid: ObjectId,
    ) -> Result<Self, ExplorationBuildError> {
        let distance = last_sequence.checked_sub(first_sequence).ok_or(
            ExplorationBuildError::ReversedWindow {
                first: first_sequence,
                last: last_sequence,
            },
        )?;
        let observation_capacity =
            distance
                .checked_add(1)
                .ok_or(ExplorationBuildError::WindowLengthOverflow {
                    first: first_sequence,
                    last: last_sequence,
                })?;
        if candidate_decision_oid == pinned_fallback_oid {
            return Err(ExplorationBuildError::DecisionEqualsFallback);
        }

        Ok(Self {
            budget_oid,
            window_oid,
            regime_oid,
            regime_epoch,
            first_sequence,
            last_sequence,
            observation_capacity,
            candidate_decision_oid,
            pinned_fallback_oid,
        })
    }

    /// Stable identity of the registered budget definition.
    #[must_use]
    pub const fn budget_oid(self) -> ObjectId {
        self.budget_oid
    }

    /// Stable identity of the observed schedule window.
    #[must_use]
    pub const fn window_oid(self) -> ObjectId {
        self.window_oid
    }

    /// Stable identity of the operating regime.
    #[must_use]
    pub const fn regime_oid(self) -> ObjectId {
        self.regime_oid
    }

    /// Monotonic epoch of the operating regime.
    #[must_use]
    pub const fn regime_epoch(self) -> u64 {
        self.regime_epoch
    }

    /// Inclusive first source-stream sequence.
    #[must_use]
    pub const fn first_sequence(self) -> u64 {
        self.first_sequence
    }

    /// Inclusive last source-stream sequence.
    #[must_use]
    pub const fn last_sequence(self) -> u64 {
        self.last_sequence
    }

    /// Number of positions in the fixed sequence window.
    #[must_use]
    pub const fn observation_capacity(self) -> u64 {
        self.observation_capacity
    }

    /// Candidate decision selected only by sufficient evidence.
    #[must_use]
    pub const fn candidate_decision_oid(self) -> ObjectId {
        self.candidate_decision_oid
    }

    /// Pinned deterministic fallback.
    #[must_use]
    pub const fn pinned_fallback_oid(self) -> ObjectId {
        self.pinned_fallback_oid
    }
}

/// Explicit caller attestations for the estimator's scoped assumptions.
///
/// These values describe the input window; they do not weaken or replace the
/// assumptions reported by asupersync. Any unsupported required assumption
/// forces the pinned fallback.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExplorationAssumptionAttestation {
    exchangeable_runs: bool,
    binary_novelty_score: bool,
    additional_runs_hit_existing_classes: bool,
}

impl ExplorationAssumptionAttestation {
    /// Constructs an explicit attestation set.
    #[must_use]
    pub const fn new(
        exchangeable_runs: bool,
        binary_novelty_score: bool,
        additional_runs_hit_existing_classes: bool,
    ) -> Self {
        Self {
            exchangeable_runs,
            binary_novelty_score,
            additional_runs_hit_existing_classes,
        }
    }

    /// Constructs the fully supported attestation set.
    #[must_use]
    pub const fn fully_supported() -> Self {
        Self::new(true, true, true)
    }

    /// Whether run order is attested exchangeable for this window.
    #[must_use]
    pub const fn exchangeable_runs(self) -> bool {
        self.exchangeable_runs
    }

    /// Whether the observation contract is attested binary.
    #[must_use]
    pub const fn binary_novelty_score(self) -> bool {
        self.binary_novelty_score
    }

    /// Whether projected added runs are attested to hit existing classes.
    #[must_use]
    pub const fn additional_runs_hit_existing_classes(self) -> bool {
        self.additional_runs_hit_existing_classes
    }

    fn supports(self, assumptions: ExplorationBudgetAssumptions) -> bool {
        (!assumptions.exchangeable_runs || self.exchangeable_runs)
            && (!assumptions.binary_novelty_score || self.binary_novelty_score)
            && (!assumptions.additional_runs_assume_existing_classes
                || self.additional_runs_hit_existing_classes)
    }
}

/// Resource-bounded, bit-canonical profile for asupersync's estimator.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ExplorationBudgetProfile {
    alpha_bits: u64,
    target_coverage_bits: u64,
    min_samples: usize,
    max_additional_runs: usize,
    max_observations: usize,
    max_estimation_work: u128,
    required_estimation_work: u128,
}

impl ExplorationBudgetProfile {
    /// Validates a foundation config and an explicit worst-case work budget.
    ///
    /// `max_estimation_work` is measured in conservatively estimated
    /// element-visits by the foundation implementation. Construction rejects
    /// a profile whose observation and projection caps cannot fit inside it.
    pub fn try_new(
        config: ExplorationBudgetConfig,
        max_observations: usize,
        max_estimation_work: u128,
    ) -> Result<Self, ExplorationBuildError> {
        validate_probability(
            ProbabilityField::Alpha,
            config.alpha,
            config.alpha.to_bits(),
        )?;
        validate_probability(
            ProbabilityField::TargetCoverage,
            config.target_coverage,
            config.target_coverage.to_bits(),
        )?;
        if config.min_samples == 0 {
            return Err(ExplorationBuildError::ZeroMinimumSamples);
        }
        if max_observations == 0 {
            return Err(ExplorationBuildError::ZeroObservationLimit);
        }
        if max_observations < config.min_samples {
            return Err(ExplorationBuildError::ObservationLimitBelowMinimum {
                maximum: max_observations,
                minimum: config.min_samples,
            });
        }
        if max_observations > MAX_EXPLORATION_PROJECTED_RUNS {
            return Err(ExplorationBuildError::ObservationLimitTooLarge {
                actual: max_observations,
                maximum: MAX_EXPLORATION_PROJECTED_RUNS,
            });
        }
        if config.max_additional_runs > MAX_EXPLORATION_PROJECTED_RUNS {
            return Err(ExplorationBuildError::AdditionalRunLimitTooLarge {
                actual: config.max_additional_runs,
                maximum: MAX_EXPLORATION_PROJECTED_RUNS,
            });
        }
        let _ = u64::try_from(max_observations).map_err(|_| {
            ExplorationBuildError::ObservationLimitUnrepresentable {
                maximum: max_observations,
            }
        })?;
        let _ = u64::try_from(config.max_additional_runs).map_err(|_| {
            ExplorationBuildError::AdditionalRunLimitUnrepresentable {
                maximum: config.max_additional_runs,
            }
        })?;
        let projected_runs = max_observations
            .checked_add(config.max_additional_runs)
            .ok_or(ExplorationBuildError::ProjectedRunCountOverflow {
                observations: max_observations,
                additional: config.max_additional_runs,
            })?;
        let maximum_vector_length = MAX_EXPLORATION_PROJECTED_RUNS.min(isize::MAX as usize);
        if projected_runs > maximum_vector_length {
            return Err(ExplorationBuildError::ProjectedRunLimitTooLarge {
                projected: projected_runs,
                maximum: maximum_vector_length,
            });
        }
        let required_estimation_work =
            estimation_work_ceiling(max_observations, config.max_additional_runs)
                .ok_or(ExplorationBuildError::EstimationWorkOverflow)?;
        if required_estimation_work > MAX_EXPLORATION_ESTIMATION_WORK {
            return Err(
                ExplorationBuildError::EstimationWorkExceedsImplementationLimit {
                    required: required_estimation_work,
                    maximum: MAX_EXPLORATION_ESTIMATION_WORK,
                },
            );
        }
        if required_estimation_work > max_estimation_work {
            return Err(ExplorationBuildError::EstimationWorkBudgetTooSmall {
                required: required_estimation_work,
                available: max_estimation_work,
            });
        }

        Ok(Self {
            alpha_bits: canonical_float_bits(config.alpha),
            target_coverage_bits: canonical_float_bits(config.target_coverage),
            min_samples: config.min_samples,
            max_additional_runs: config.max_additional_runs,
            max_observations,
            max_estimation_work,
            required_estimation_work,
        })
    }

    /// Exact target-miscoverage bits.
    #[must_use]
    pub const fn alpha_bits(&self) -> u64 {
        self.alpha_bits
    }

    /// Exact target-coverage bits.
    #[must_use]
    pub const fn target_coverage_bits(&self) -> u64 {
        self.target_coverage_bits
    }

    /// Minimum sample count accepted by the estimator.
    #[must_use]
    pub const fn min_samples(&self) -> usize {
        self.min_samples
    }

    /// Maximum number of projected additional runs.
    #[must_use]
    pub const fn max_additional_runs(&self) -> usize {
        self.max_additional_runs
    }

    /// Maximum number of accepted observations.
    #[must_use]
    pub const fn max_observations(&self) -> usize {
        self.max_observations
    }

    /// Caller-provided ceiling for one estimate.
    #[must_use]
    pub const fn max_estimation_work(&self) -> u128 {
        self.max_estimation_work
    }

    /// Conservative worst-case work required by this profile.
    #[must_use]
    pub const fn required_estimation_work(&self) -> u128 {
        self.required_estimation_work
    }

    fn foundation_config(&self) -> ExplorationBudgetConfig {
        ExplorationBudgetConfig {
            alpha: f64::from_bits(self.alpha_bits),
            target_coverage: f64::from_bits(self.target_coverage_bits),
            min_samples: self.min_samples,
            max_additional_runs: self.max_additional_runs,
        }
    }
}

/// Probability field rejected during profile construction.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProbabilityField {
    Alpha,
    TargetCoverage,
}

impl fmt::Display for ProbabilityField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Alpha => "alpha",
            Self::TargetCoverage => "target coverage",
        })
    }
}

/// Identity or profile construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExplorationBuildError {
    /// The inclusive stream window is reversed.
    ReversedWindow { first: u64, last: u64 },
    /// The inclusive window length cannot be represented.
    WindowLengthOverflow { first: u64, last: u64 },
    /// Candidate and fallback identities must be distinct.
    DecisionEqualsFallback,
    /// A probability was non-finite or outside the open unit interval.
    InvalidProbability { field: ProbabilityField, bits: u64 },
    /// The foundation requires a positive minimum sample count.
    ZeroMinimumSamples,
    /// A monitor must accept at least one observation.
    ZeroObservationLimit,
    /// The hard observation cap cannot reach the estimator's minimum.
    ObservationLimitBelowMinimum { maximum: usize, minimum: usize },
    /// The observation cap exceeded the implementation allocation ceiling.
    ObservationLimitTooLarge { actual: usize, maximum: usize },
    /// The additional-run cap exceeded the implementation allocation ceiling.
    AdditionalRunLimitTooLarge { actual: usize, maximum: usize },
    /// The hard observation cap cannot be represented canonically.
    ObservationLimitUnrepresentable { maximum: usize },
    /// The projection cap cannot be represented canonically.
    AdditionalRunLimitUnrepresentable { maximum: usize },
    /// Observation and projection caps overflow the platform count domain.
    ProjectedRunCountOverflow {
        observations: usize,
        additional: usize,
    },
    /// A projected foundation vector would exceed Rust's maximum length.
    ProjectedRunLimitTooLarge { projected: usize, maximum: usize },
    /// Arithmetic for the conservative work ceiling overflowed.
    EstimationWorkOverflow,
    /// The configured worst case exceeded the implementation work ceiling.
    EstimationWorkExceedsImplementationLimit { required: u128, maximum: u128 },
    /// The explicit work budget cannot cover the configured worst case.
    EstimationWorkBudgetTooSmall { required: u128, available: u128 },
    /// The observation cap is larger than the identity's sequence window.
    ObservationLimitExceedsWindow { maximum: usize, window: u64 },
}

impl fmt::Display for ExplorationBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ReversedWindow { first, last } => {
                write!(formatter, "exploration window {first}..={last} is reversed")
            }
            Self::WindowLengthOverflow { first, last } => write!(
                formatter,
                "exploration window {first}..={last} has an unrepresentable length"
            ),
            Self::DecisionEqualsFallback => {
                formatter.write_str("exploration decision and fallback identities must differ")
            }
            Self::InvalidProbability { field, bits } => write!(
                formatter,
                "exploration {field} 0x{bits:016x} is not finite and inside (0, 1)"
            ),
            Self::ZeroMinimumSamples => {
                formatter.write_str("exploration minimum sample count must be positive")
            }
            Self::ZeroObservationLimit => {
                formatter.write_str("exploration observation limit must be positive")
            }
            Self::ObservationLimitBelowMinimum { maximum, minimum } => write!(
                formatter,
                "exploration observation limit {maximum} is below minimum {minimum}"
            ),
            Self::ObservationLimitTooLarge { actual, maximum } => write!(
                formatter,
                "exploration observation limit {actual} exceeds implementation maximum {maximum}"
            ),
            Self::AdditionalRunLimitTooLarge { actual, maximum } => write!(
                formatter,
                "exploration additional-run limit {actual} exceeds implementation maximum {maximum}"
            ),
            Self::ObservationLimitUnrepresentable { maximum } => write!(
                formatter,
                "exploration observation limit {maximum} is not representable"
            ),
            Self::AdditionalRunLimitUnrepresentable { maximum } => write!(
                formatter,
                "exploration additional-run limit {maximum} is not representable"
            ),
            Self::ProjectedRunCountOverflow {
                observations,
                additional,
            } => write!(
                formatter,
                "exploration caps {observations} + {additional} overflow the run-count domain"
            ),
            Self::ProjectedRunLimitTooLarge { projected, maximum } => write!(
                formatter,
                "exploration projection length {projected} exceeds platform maximum {maximum}"
            ),
            Self::EstimationWorkOverflow => {
                formatter.write_str("exploration work-ceiling arithmetic overflowed")
            }
            Self::EstimationWorkExceedsImplementationLimit { required, maximum } => write!(
                formatter,
                "exploration estimate requires {required} work units; implementation maximum is {maximum}"
            ),
            Self::EstimationWorkBudgetTooSmall {
                required,
                available,
            } => write!(
                formatter,
                "exploration estimate requires {required} work units but only {available} are allowed"
            ),
            Self::ObservationLimitExceedsWindow { maximum, window } => write!(
                formatter,
                "exploration observation limit {maximum} exceeds window length {window}"
            ),
        }
    }
}

impl std::error::Error for ExplorationBuildError {}

/// Identity- and profile-bound novelty observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SequencedNovelty {
    identity: ExplorationBudgetIdentity,
    profile: ExplorationBudgetProfile,
    stream_sequence: u64,
    discovered_new_class: bool,
}

impl SequencedNovelty {
    /// Creates an input envelope without accepting it into a monitor.
    #[must_use]
    pub fn new(
        identity: ExplorationBudgetIdentity,
        profile: ExplorationBudgetProfile,
        stream_sequence: u64,
        discovered_new_class: bool,
    ) -> Self {
        Self {
            identity,
            profile,
            stream_sequence,
            discovered_new_class,
        }
    }

    /// Complete immutable budget identity.
    #[must_use]
    pub const fn identity(&self) -> ExplorationBudgetIdentity {
        self.identity
    }

    /// Complete immutable budget profile.
    #[must_use]
    pub const fn profile(&self) -> &ExplorationBudgetProfile {
        &self.profile
    }

    /// Exact source-stream sequence.
    #[must_use]
    pub const fn stream_sequence(&self) -> u64 {
        self.stream_sequence
    }

    /// Whether this run discovered a new equivalence class.
    #[must_use]
    pub const fn discovered_new_class(&self) -> bool {
        self.discovered_new_class
    }
}

/// Policy selected by the current exploration-budget evidence.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExplorationSelection {
    CandidateDecision,
    PinnedFallback,
}

/// Why the current policy selection was made.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExplorationDisposition {
    CandidateSupported,
    AssumptionsUnsupported,
    InsufficientSamples,
    RecommendationExhausted,
    TargetNotMet,
}

/// Deterministic, immutable projection of one foundation estimate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExplorationBudgetEvidence {
    identity: ExplorationBudgetIdentity,
    profile: ExplorationBudgetProfile,
    assumption_attestation: ExplorationAssumptionAttestation,
    required_assumptions: ExplorationBudgetAssumptions,
    through_sequence: Option<u64>,
    total_runs: u64,
    discoveries: u64,
    residual_discovery_rate_bits: u64,
    conformal_upper_bound_bits: u64,
    target_residual_rate_bits: u64,
    target_coverage_bits: u64,
    calibration_samples: u64,
    recommended_additional_runs: u64,
    target_met: bool,
    exhausted_recommendation: bool,
    assumptions_supported: bool,
    selection: ExplorationSelection,
    disposition: ExplorationDisposition,
}

impl ExplorationBudgetEvidence {
    /// Complete identity of this estimate.
    #[must_use]
    pub const fn identity(&self) -> ExplorationBudgetIdentity {
        self.identity
    }

    /// Complete profile used by the estimator.
    #[must_use]
    pub const fn profile(&self) -> &ExplorationBudgetProfile {
        &self.profile
    }

    /// Caller attestations evaluated against the foundation assumptions.
    #[must_use]
    pub const fn assumption_attestation(&self) -> ExplorationAssumptionAttestation {
        self.assumption_attestation
    }

    /// Foundation assumptions that the caller attestations were checked
    /// against.
    #[must_use]
    pub const fn required_assumptions(&self) -> ExplorationBudgetAssumptions {
        self.required_assumptions
    }

    /// Last accepted source-stream sequence, if any.
    #[must_use]
    pub const fn through_sequence(&self) -> Option<u64> {
        self.through_sequence
    }

    /// Total observations included in the estimate.
    #[must_use]
    pub const fn total_runs(&self) -> u64 {
        self.total_runs
    }

    /// Count of observations that discovered a new class.
    #[must_use]
    pub const fn discoveries(&self) -> u64 {
        self.discoveries
    }

    /// Exact residual-discovery-rate bits.
    #[must_use]
    pub const fn residual_discovery_rate_bits(&self) -> u64 {
        self.residual_discovery_rate_bits
    }

    /// Exact finite-sample upper-bound bits.
    #[must_use]
    pub const fn conformal_upper_bound_bits(&self) -> u64 {
        self.conformal_upper_bound_bits
    }

    /// Exact target-residual-rate bits.
    #[must_use]
    pub const fn target_residual_rate_bits(&self) -> u64 {
        self.target_residual_rate_bits
    }

    /// Exact target-coverage bits.
    #[must_use]
    pub const fn target_coverage_bits(&self) -> u64 {
        self.target_coverage_bits
    }

    /// Samples consumed by the foundation estimate.
    #[must_use]
    pub const fn calibration_samples(&self) -> u64 {
        self.calibration_samples
    }

    /// Recommended additional existing-class runs.
    #[must_use]
    pub const fn recommended_additional_runs(&self) -> u64 {
        self.recommended_additional_runs
    }

    /// Whether the foundation target was met.
    #[must_use]
    pub const fn target_met(&self) -> bool {
        self.target_met
    }

    /// Whether the projection cap was exhausted before meeting the target.
    #[must_use]
    pub const fn exhausted_recommendation(&self) -> bool {
        self.exhausted_recommendation
    }

    /// Whether every foundation assumption has a matching attestation.
    #[must_use]
    pub const fn assumptions_supported(&self) -> bool {
        self.assumptions_supported
    }

    /// Candidate or pinned-fallback selection.
    #[must_use]
    pub const fn selection(&self) -> ExplorationSelection {
        self.selection
    }

    /// Stable explanation of the current selection.
    #[must_use]
    pub const fn disposition(&self) -> ExplorationDisposition {
        self.disposition
    }

    /// Exact identity of the selected policy.
    #[must_use]
    pub const fn selected_policy_oid(&self) -> ObjectId {
        match self.selection {
            ExplorationSelection::CandidateDecision => self.identity.candidate_decision_oid,
            ExplorationSelection::PinnedFallback => self.identity.pinned_fallback_oid,
        }
    }
}

/// Non-mutating observation rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExplorationObserveError {
    IdentityMismatch,
    ProfileMismatch,
    UnexpectedSequence { expected: u64, actual: u64 },
    WindowComplete { last: u64 },
    ObservationLimitReached { maximum: usize },
    CounterExhausted,
}

impl fmt::Display for ExplorationObserveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::IdentityMismatch => formatter.write_str("exploration identity does not match"),
            Self::ProfileMismatch => formatter.write_str("exploration profile does not match"),
            Self::UnexpectedSequence { expected, actual } => {
                write!(
                    formatter,
                    "expected exploration sequence {expected}, got {actual}"
                )
            }
            Self::WindowComplete { last } => {
                write!(formatter, "exploration window ended at {last}")
            }
            Self::ObservationLimitReached { maximum } => {
                write!(formatter, "exploration observation limit {maximum} reached")
            }
            Self::CounterExhausted => formatter.write_str("exploration counter is exhausted"),
        }
    }
}

impl std::error::Error for ExplorationObserveError {}

/// Bounded, exactly sequenced wrapper around [`ExplorationBudget`].
#[derive(Debug, Eq, PartialEq)]
pub struct ExplorationBudgetMonitor {
    identity: ExplorationBudgetIdentity,
    profile: ExplorationBudgetProfile,
    assumption_attestation: ExplorationAssumptionAttestation,
    next_sequence: Option<u64>,
    through_sequence: Option<u64>,
    total_runs: usize,
    discoveries: usize,
}

impl ExplorationBudgetMonitor {
    /// Constructs a monitor after validating the profile against its window.
    pub fn try_new(
        identity: ExplorationBudgetIdentity,
        profile: ExplorationBudgetProfile,
        assumption_attestation: ExplorationAssumptionAttestation,
    ) -> Result<Self, ExplorationBuildError> {
        let maximum = u64::try_from(profile.max_observations).map_err(|_| {
            ExplorationBuildError::ObservationLimitUnrepresentable {
                maximum: profile.max_observations,
            }
        })?;
        if maximum > identity.observation_capacity {
            return Err(ExplorationBuildError::ObservationLimitExceedsWindow {
                maximum: profile.max_observations,
                window: identity.observation_capacity,
            });
        }
        let first_sequence = identity.first_sequence;

        Ok(Self {
            identity,
            profile,
            assumption_attestation,
            next_sequence: Some(first_sequence),
            through_sequence: None,
            total_runs: 0,
            discoveries: 0,
        })
    }

    /// Complete immutable budget identity.
    #[must_use]
    pub const fn identity(&self) -> ExplorationBudgetIdentity {
        self.identity
    }

    /// Complete immutable budget profile.
    #[must_use]
    pub const fn profile(&self) -> &ExplorationBudgetProfile {
        &self.profile
    }

    /// Assumptions attested for this monitor.
    #[must_use]
    pub const fn assumption_attestation(&self) -> ExplorationAssumptionAttestation {
        self.assumption_attestation
    }

    /// Next exact sequence accepted by the monitor.
    #[must_use]
    pub fn next_sequence(&self) -> Option<u64> {
        if self.total_runs >= self.profile.max_observations {
            None
        } else {
            self.next_sequence
        }
    }

    /// Number of accepted observations.
    #[must_use]
    pub const fn total_runs(&self) -> usize {
        self.total_runs
    }

    /// Current evidence without changing the monitor.
    #[must_use]
    pub fn evidence(&self) -> ExplorationBudgetEvidence {
        self.project_estimate(self.total_runs, self.discoveries, self.through_sequence)
    }

    /// Accepts one exact sequence position after preflighting every rejection.
    pub fn observe(
        &mut self,
        input: SequencedNovelty,
    ) -> Result<ExplorationBudgetEvidence, ExplorationObserveError> {
        if input.identity != self.identity {
            return Err(ExplorationObserveError::IdentityMismatch);
        }
        if input.profile != self.profile {
            return Err(ExplorationObserveError::ProfileMismatch);
        }
        let Some(expected) = self.next_sequence else {
            return Err(ExplorationObserveError::WindowComplete {
                last: self.identity.last_sequence,
            });
        };
        if self.total_runs >= self.profile.max_observations {
            return Err(ExplorationObserveError::ObservationLimitReached {
                maximum: self.profile.max_observations,
            });
        }
        if input.stream_sequence != expected {
            return Err(ExplorationObserveError::UnexpectedSequence {
                expected,
                actual: input.stream_sequence,
            });
        }

        let next_total = self
            .total_runs
            .checked_add(1)
            .ok_or(ExplorationObserveError::CounterExhausted)?;
        let discovery_increment = if input.discovered_new_class { 1 } else { 0 };
        let next_discoveries = self
            .discoveries
            .checked_add(discovery_increment)
            .ok_or(ExplorationObserveError::CounterExhausted)?;
        let next_sequence = if input.stream_sequence == self.identity.last_sequence {
            None
        } else {
            Some(
                input
                    .stream_sequence
                    .checked_add(1)
                    .ok_or(ExplorationObserveError::CounterExhausted)?,
            )
        };
        let evidence =
            self.project_estimate(next_total, next_discoveries, Some(input.stream_sequence));

        self.total_runs = next_total;
        self.discoveries = next_discoveries;
        self.through_sequence = Some(input.stream_sequence);
        self.next_sequence = next_sequence;
        Ok(evidence)
    }

    fn project_estimate(
        &self,
        total_runs: usize,
        discoveries: usize,
        through_sequence: Option<u64>,
    ) -> ExplorationBudgetEvidence {
        let estimate = ExplorationBudget::estimate_from_counts(
            total_runs,
            discoveries,
            self.profile.foundation_config(),
        );
        let assumptions_supported = self.assumption_attestation.supports(estimate.assumptions);
        let disposition = select_disposition(
            assumptions_supported,
            estimate.total_runs,
            self.profile.min_samples,
            estimate.target_met,
            estimate.exhausted_recommendation,
        );
        let selection = if disposition == ExplorationDisposition::CandidateSupported {
            ExplorationSelection::CandidateDecision
        } else {
            ExplorationSelection::PinnedFallback
        };

        ExplorationBudgetEvidence {
            identity: self.identity,
            profile: self.profile.clone(),
            assumption_attestation: self.assumption_attestation,
            required_assumptions: estimate.assumptions,
            through_sequence,
            total_runs: canonical_count(estimate.total_runs),
            discoveries: canonical_count(estimate.discoveries),
            residual_discovery_rate_bits: canonical_float_bits(estimate.residual_discovery_rate),
            conformal_upper_bound_bits: canonical_float_bits(estimate.conformal_upper_bound),
            target_residual_rate_bits: canonical_float_bits(estimate.target_residual_rate),
            target_coverage_bits: canonical_float_bits(estimate.target_coverage),
            calibration_samples: canonical_count(estimate.calibration_samples),
            recommended_additional_runs: canonical_count(estimate.recommended_additional_runs),
            target_met: estimate.target_met,
            exhausted_recommendation: estimate.exhausted_recommendation,
            assumptions_supported,
            selection,
            disposition,
        }
    }
}

const fn select_disposition(
    assumptions_supported: bool,
    total_runs: usize,
    min_samples: usize,
    target_met: bool,
    exhausted_recommendation: bool,
) -> ExplorationDisposition {
    if !assumptions_supported {
        ExplorationDisposition::AssumptionsUnsupported
    } else if total_runs < min_samples {
        ExplorationDisposition::InsufficientSamples
    } else if exhausted_recommendation {
        ExplorationDisposition::RecommendationExhausted
    } else if !target_met {
        ExplorationDisposition::TargetNotMet
    } else {
        ExplorationDisposition::CandidateSupported
    }
}

fn validate_probability(
    field: ProbabilityField,
    value: f64,
    bits: u64,
) -> Result<(), ExplorationBuildError> {
    if value.is_finite() && value > 0.0 && value < 1.0 {
        Ok(())
    } else {
        Err(ExplorationBuildError::InvalidProbability { field, bits })
    }
}

fn estimation_work_ceiling(max_observations: usize, max_additional: usize) -> Option<u128> {
    let observations = max_observations as u128;
    let additional = max_additional as u128;
    let terms = additional.checked_add(1)?;
    let rectangular = terms.checked_mul(observations)?;
    let triangular = additional.checked_mul(terms)?.checked_div(2)?;
    let projected = rectangular.checked_add(triangular)?;

    observations
        .checked_mul(5)?
        .checked_add(projected.checked_mul(2)?)?
        .checked_add(additional.checked_mul(2)?)
}

fn canonical_float_bits(value: f64) -> u64 {
    if value == 0.0 {
        0.0_f64.to_bits()
    } else {
        value.to_bits()
    }
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

    fn identity_with_window(
        first: u64,
        last: u64,
    ) -> Result<ExplorationBudgetIdentity, ExplorationBuildError> {
        ExplorationBudgetIdentity::try_new(oid(1), oid(2), oid(3), 7, first, last, oid(4), oid(5))
    }

    fn identity() -> Result<ExplorationBudgetIdentity, ExplorationBuildError> {
        identity_with_window(100, 109)
    }

    fn config(min_samples: usize, max_additional_runs: usize) -> ExplorationBudgetConfig {
        ExplorationBudgetConfig {
            alpha: 0.5,
            target_coverage: 0.5,
            min_samples,
            max_additional_runs,
        }
    }

    fn profile() -> Result<ExplorationBudgetProfile, ExplorationBuildError> {
        ExplorationBudgetProfile::try_new(config(2, 4), 10, 10_000)
    }

    fn monitor(
        assumptions: ExplorationAssumptionAttestation,
    ) -> Result<ExplorationBudgetMonitor, ExplorationBuildError> {
        ExplorationBudgetMonitor::try_new(identity()?, profile()?, assumptions)
    }

    fn input(
        sequence: u64,
        discovered_new_class: bool,
    ) -> Result<SequencedNovelty, ExplorationBuildError> {
        Ok(SequencedNovelty::new(
            identity()?,
            profile()?,
            sequence,
            discovered_new_class,
        ))
    }

    #[test]
    fn construction_rejects_invalid_identity_and_profile_inputs() {
        assert_eq!(
            identity_with_window(9, 8),
            Err(ExplorationBuildError::ReversedWindow { first: 9, last: 8 })
        );
        assert_eq!(
            identity_with_window(0, u64::MAX),
            Err(ExplorationBuildError::WindowLengthOverflow {
                first: 0,
                last: u64::MAX,
            })
        );
        assert_eq!(
            ExplorationBudgetIdentity::try_new(oid(1), oid(2), oid(3), 0, 1, 2, oid(4), oid(4),),
            Err(ExplorationBuildError::DecisionEqualsFallback)
        );

        let invalid_alpha = ExplorationBudgetConfig {
            alpha: f64::NAN,
            ..config(2, 1)
        };
        assert_eq!(
            ExplorationBudgetProfile::try_new(invalid_alpha, 2, 1_000),
            Err(ExplorationBuildError::InvalidProbability {
                field: ProbabilityField::Alpha,
                bits: f64::NAN.to_bits(),
            })
        );
        let invalid_coverage = ExplorationBudgetConfig {
            target_coverage: 1.0,
            ..config(2, 1)
        };
        assert_eq!(
            ExplorationBudgetProfile::try_new(invalid_coverage, 2, 1_000),
            Err(ExplorationBuildError::InvalidProbability {
                field: ProbabilityField::TargetCoverage,
                bits: 1.0_f64.to_bits(),
            })
        );
        assert_eq!(
            ExplorationBudgetProfile::try_new(config(0, 1), 2, 1_000),
            Err(ExplorationBuildError::ZeroMinimumSamples)
        );
        assert_eq!(
            ExplorationBudgetProfile::try_new(config(2, 1), 0, 1_000),
            Err(ExplorationBuildError::ZeroObservationLimit)
        );
        assert_eq!(
            ExplorationBudgetProfile::try_new(config(3, 1), 2, 1_000),
            Err(ExplorationBuildError::ObservationLimitBelowMinimum {
                maximum: 2,
                minimum: 3,
            })
        );
        assert_eq!(
            ExplorationBudgetProfile::try_new(config(1, usize::MAX), 1, u128::MAX),
            Err(ExplorationBuildError::AdditionalRunLimitTooLarge {
                actual: usize::MAX,
                maximum: MAX_EXPLORATION_PROJECTED_RUNS,
            })
        );
        assert_eq!(
            ExplorationBudgetProfile::try_new(
                config(1, MAX_EXPLORATION_PROJECTED_RUNS),
                1,
                u128::MAX,
            ),
            Err(ExplorationBuildError::ProjectedRunLimitTooLarge {
                projected: MAX_EXPLORATION_PROJECTED_RUNS + 1,
                maximum: MAX_EXPLORATION_PROJECTED_RUNS,
            })
        );
        assert_eq!(
            ExplorationBudgetProfile::try_new(
                config(1, 0),
                MAX_EXPLORATION_PROJECTED_RUNS + 1,
                u128::MAX,
            ),
            Err(ExplorationBuildError::ObservationLimitTooLarge {
                actual: MAX_EXPLORATION_PROJECTED_RUNS + 1,
                maximum: MAX_EXPLORATION_PROJECTED_RUNS,
            })
        );
        let excessive_work = estimation_work_ceiling(10_000, 5_000).unwrap_or(u128::MAX);
        assert!(excessive_work > MAX_EXPLORATION_ESTIMATION_WORK);
        assert_eq!(
            ExplorationBudgetProfile::try_new(config(1, 5_000), 10_000, u128::MAX),
            Err(
                ExplorationBuildError::EstimationWorkExceedsImplementationLimit {
                    required: excessive_work,
                    maximum: MAX_EXPLORATION_ESTIMATION_WORK,
                }
            )
        );
    }

    #[test]
    fn explicit_work_and_window_limits_are_enforced() -> Result<(), Box<dyn std::error::Error>> {
        let required =
            estimation_work_ceiling(10, 4).ok_or(ExplorationBuildError::EstimationWorkOverflow)?;
        assert_eq!(
            ExplorationBudgetProfile::try_new(config(2, 4), 10, required - 1),
            Err(ExplorationBuildError::EstimationWorkBudgetTooSmall {
                required,
                available: required - 1,
            })
        );
        let exact = ExplorationBudgetProfile::try_new(config(2, 4), 10, required)?;
        assert_eq!(exact.required_estimation_work(), required);
        assert_eq!(exact.max_estimation_work(), required);

        let short_identity = identity_with_window(100, 101)?;
        assert_eq!(
            ExplorationBudgetMonitor::try_new(
                short_identity,
                profile()?,
                ExplorationAssumptionAttestation::fully_supported(),
            ),
            Err(ExplorationBuildError::ObservationLimitExceedsWindow {
                maximum: 10,
                window: 2,
            })
        );
        Ok(())
    }

    #[test]
    fn sequence_and_envelope_rejections_are_atomic() -> Result<(), Box<dyn std::error::Error>> {
        let assumptions = ExplorationAssumptionAttestation::fully_supported();
        let mut value = monitor(assumptions)?;
        let before = value.evidence();

        assert_eq!(
            value.observe(input(101, false)?),
            Err(ExplorationObserveError::UnexpectedSequence {
                expected: 100,
                actual: 101,
            })
        );
        assert_eq!(value.evidence(), before);

        let other_identity = ExplorationBudgetIdentity::try_new(
            oid(9),
            oid(2),
            oid(3),
            7,
            100,
            109,
            oid(4),
            oid(5),
        )?;
        assert_eq!(
            value.observe(SequencedNovelty::new(
                other_identity,
                profile()?,
                100,
                false,
            )),
            Err(ExplorationObserveError::IdentityMismatch)
        );
        assert_eq!(value.evidence(), before);

        let other_profile = ExplorationBudgetProfile::try_new(config(3, 4), 10, 10_000)?;
        assert_eq!(
            value.observe(SequencedNovelty::new(
                identity()?,
                other_profile,
                100,
                false,
            )),
            Err(ExplorationObserveError::ProfileMismatch)
        );
        assert_eq!(value.evidence(), before);
        assert_eq!(value.next_sequence(), Some(100));
        Ok(())
    }

    #[test]
    fn supported_target_selects_candidate_after_minimum_samples()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut value = monitor(ExplorationAssumptionAttestation::fully_supported())?;
        let initial = value.evidence();
        assert_eq!(
            initial.disposition(),
            ExplorationDisposition::InsufficientSamples
        );
        assert_eq!(
            initial.selected_policy_oid(),
            identity()?.pinned_fallback_oid()
        );

        let first = value.observe(input(100, false)?)?;
        assert_eq!(
            first.disposition(),
            ExplorationDisposition::InsufficientSamples
        );
        let second = value.observe(input(101, false)?)?;
        assert!(second.target_met());
        assert!(second.assumptions_supported());
        assert_eq!(
            second.disposition(),
            ExplorationDisposition::CandidateSupported
        );
        assert_eq!(second.selection(), ExplorationSelection::CandidateDecision);
        assert_eq!(
            second.selected_policy_oid(),
            identity()?.candidate_decision_oid()
        );
        assert_eq!(second.through_sequence(), Some(101));
        Ok(())
    }

    #[test]
    fn every_unsupported_required_assumption_pins_fallback()
    -> Result<(), Box<dyn std::error::Error>> {
        let attestations = [
            ExplorationAssumptionAttestation::new(false, true, true),
            ExplorationAssumptionAttestation::new(true, false, true),
            ExplorationAssumptionAttestation::new(true, true, false),
        ];

        for attestation in attestations {
            let mut value = monitor(attestation)?;
            let _ = value.observe(input(100, false)?)?;
            let evidence = value.observe(input(101, false)?)?;
            assert!(evidence.target_met());
            assert!(!evidence.assumptions_supported());
            assert_eq!(
                evidence.disposition(),
                ExplorationDisposition::AssumptionsUnsupported
            );
            assert_eq!(evidence.selection(), ExplorationSelection::PinnedFallback);
            assert_eq!(
                evidence.selected_policy_oid(),
                identity()?.pinned_fallback_oid()
            );
        }
        Ok(())
    }

    #[test]
    fn unmet_and_exhausted_budget_conditions_pin_fallback() -> Result<(), Box<dyn std::error::Error>>
    {
        let strict_config = ExplorationBudgetConfig {
            alpha: 0.2,
            target_coverage: 0.95,
            min_samples: 2,
            max_additional_runs: 1,
        };
        let strict_profile = ExplorationBudgetProfile::try_new(strict_config, 2, 1_000)?;
        let strict_identity = identity_with_window(10, 11)?;
        let mut value = ExplorationBudgetMonitor::try_new(
            strict_identity,
            strict_profile.clone(),
            ExplorationAssumptionAttestation::fully_supported(),
        )?;
        let _ = value.observe(SequencedNovelty::new(
            strict_identity,
            strict_profile.clone(),
            10,
            false,
        ))?;
        let evidence = value.observe(SequencedNovelty::new(
            strict_identity,
            strict_profile,
            11,
            false,
        ))?;

        assert!(!evidence.target_met());
        assert!(evidence.exhausted_recommendation());
        assert_eq!(evidence.recommended_additional_runs(), 1);
        assert_eq!(
            evidence.disposition(),
            ExplorationDisposition::RecommendationExhausted
        );
        assert_eq!(evidence.selection(), ExplorationSelection::PinnedFallback);
        Ok(())
    }

    #[test]
    fn pending_recommendation_pins_fallback_without_marking_exhaustion()
    -> Result<(), Box<dyn std::error::Error>> {
        let pending_config = ExplorationBudgetConfig {
            alpha: 0.2,
            target_coverage: 0.8,
            min_samples: 2,
            max_additional_runs: 30,
        };
        let pending_profile = ExplorationBudgetProfile::try_new(pending_config, 2, 10_000)?;
        let pending_identity = identity_with_window(20, 21)?;
        let mut value = ExplorationBudgetMonitor::try_new(
            pending_identity,
            pending_profile.clone(),
            ExplorationAssumptionAttestation::fully_supported(),
        )?;
        let _ = value.observe(SequencedNovelty::new(
            pending_identity,
            pending_profile.clone(),
            20,
            false,
        ))?;
        let evidence = value.observe(SequencedNovelty::new(
            pending_identity,
            pending_profile,
            21,
            false,
        ))?;

        assert!(!evidence.target_met());
        assert!(!evidence.exhausted_recommendation());
        assert!(evidence.recommended_additional_runs() > 0);
        assert_eq!(evidence.disposition(), ExplorationDisposition::TargetNotMet);
        assert_eq!(evidence.selection(), ExplorationSelection::PinnedFallback);
        Ok(())
    }

    #[test]
    fn window_completion_and_observation_cap_are_distinct() -> Result<(), Box<dyn std::error::Error>>
    {
        let window_identity = identity_with_window(10, 11)?;
        let window_profile = ExplorationBudgetProfile::try_new(config(2, 1), 2, 1_000)?;
        let mut complete = ExplorationBudgetMonitor::try_new(
            window_identity,
            window_profile.clone(),
            ExplorationAssumptionAttestation::fully_supported(),
        )?;
        let _ = complete.observe(SequencedNovelty::new(
            window_identity,
            window_profile.clone(),
            10,
            false,
        ))?;
        let _ = complete.observe(SequencedNovelty::new(
            window_identity,
            window_profile.clone(),
            11,
            false,
        ))?;
        assert_eq!(complete.next_sequence(), None);
        assert_eq!(
            complete.observe(SequencedNovelty::new(
                window_identity,
                window_profile,
                11,
                false,
            )),
            Err(ExplorationObserveError::WindowComplete { last: 11 })
        );

        let capped_identity = identity_with_window(20, 24)?;
        let capped_profile = ExplorationBudgetProfile::try_new(config(2, 1), 2, 1_000)?;
        let mut capped = ExplorationBudgetMonitor::try_new(
            capped_identity,
            capped_profile.clone(),
            ExplorationAssumptionAttestation::fully_supported(),
        )?;
        let _ = capped.observe(SequencedNovelty::new(
            capped_identity,
            capped_profile.clone(),
            20,
            false,
        ))?;
        let _ = capped.observe(SequencedNovelty::new(
            capped_identity,
            capped_profile.clone(),
            21,
            false,
        ))?;
        assert_eq!(capped.next_sequence(), None);
        assert_eq!(
            capped.observe(SequencedNovelty::new(
                capped_identity,
                capped_profile,
                22,
                false,
            )),
            Err(ExplorationObserveError::ObservationLimitReached { maximum: 2 })
        );
        Ok(())
    }

    #[test]
    fn evidence_is_bit_exact_projection_of_foundation() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = monitor(ExplorationAssumptionAttestation::fully_supported())?;
        let _ = value.observe(input(100, true)?)?;
        let evidence = value.observe(input(101, false)?)?;
        let foundation =
            ExplorationBudget::estimate_from_counts(2, 1, profile()?.foundation_config());

        assert_eq!(
            evidence.total_runs(),
            canonical_count(foundation.total_runs)
        );
        assert_eq!(
            evidence.discoveries(),
            canonical_count(foundation.discoveries)
        );
        assert_eq!(
            evidence.residual_discovery_rate_bits(),
            canonical_float_bits(foundation.residual_discovery_rate)
        );
        assert_eq!(
            evidence.conformal_upper_bound_bits(),
            canonical_float_bits(foundation.conformal_upper_bound)
        );
        assert_eq!(
            evidence.target_residual_rate_bits(),
            canonical_float_bits(foundation.target_residual_rate)
        );
        assert_eq!(
            evidence.target_coverage_bits(),
            canonical_float_bits(foundation.target_coverage)
        );
        assert_eq!(
            evidence.calibration_samples(),
            canonical_count(foundation.calibration_samples)
        );
        assert_eq!(
            evidence.recommended_additional_runs(),
            canonical_count(foundation.recommended_additional_runs)
        );
        assert_eq!(evidence.required_assumptions(), foundation.assumptions);
        Ok(())
    }

    #[test]
    fn replay_of_same_identity_profile_and_stream_is_identical()
    -> Result<(), Box<dyn std::error::Error>> {
        let assumptions = ExplorationAssumptionAttestation::fully_supported();
        let mut left = monitor(assumptions)?;
        let mut right = monitor(assumptions)?;
        let observations = [(100, true), (101, false), (102, false), (103, false)];

        for (sequence, discovered) in observations {
            let left_evidence = left.observe(input(sequence, discovered)?)?;
            let right_evidence = right.observe(input(sequence, discovered)?)?;
            assert_eq!(left_evidence, right_evidence);
        }
        assert_eq!(left.evidence(), right.evidence());
        Ok(())
    }
}
