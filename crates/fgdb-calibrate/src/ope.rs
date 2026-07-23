//! Deterministic, identity-bound off-policy evaluation.
//!
//! This module turns a finite stream of fully logged action distributions into
//! replayable `FG-EVID-01` evidence. Probabilities, outcomes, clipping, value
//! estimates, and effective-sample-size checks use fixed-point integers and
//! checked arithmetic. A candidate policy is selected only after the complete
//! declared window has been observed, every candidate and fallback action has
//! logging-policy support, both effective-sample-size gates pass, and the
//! candidate's estimated value is strictly greater than the pinned fallback.

use core::fmt;

use fgdb_types::ObjectId;

/// Exact denominator shared by every logged probability.
pub const PROBABILITY_SCALE: u64 = 1_000_000_000;

/// Exact denominator shared by outcomes and direct-model predictions.
pub const OUTCOME_SCALE: i64 = 1_000_000;

/// Exact denominator shared by importance weights and clipping limits.
pub const WEIGHT_SCALE: u64 = 1_000_000;

/// Converts a weight numerator into the probability-scale denominator used by
/// every exact estimate.
const WEIGHT_TO_PROBABILITY_SCALE: u64 = PROBABILITY_SCALE / WEIGHT_SCALE;

/// Absolute ceiling for one outcome or direct-model prediction.
pub const MAX_ABS_OUTCOME_UNITS: u64 = 1_000_000_000_000;

/// Absolute observation-count ceiling for one ledger.
pub const MAX_OBSERVATIONS: usize = 1_048_576;

/// Absolute action-count ceiling for one logged decision.
pub const MAX_ACTIONS_PER_OBSERVATION: usize = 65_536;

/// Absolute action-row ceiling for one ledger.
pub const MAX_TOTAL_ACTION_ROWS: usize = 4_194_304;

/// Absolute clipping ceiling, equal to a weight of 1,000.
pub const MAX_CLIPPING_WEIGHT_UNITS: u64 = 1_000 * WEIGHT_SCALE;

/// A probability represented exactly as a numerator over
/// [`PROBABILITY_SCALE`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Probability {
    numerator: u64,
}

impl Probability {
    /// Constructs an exact probability in the closed interval `[0, 1]`.
    pub const fn try_from_numerator(numerator: u64) -> Result<Self, OpeError> {
        if numerator > PROBABILITY_SCALE {
            return Err(OpeError::ProbabilityAboveOne { numerator });
        }
        Ok(Self { numerator })
    }

    /// Zero probability.
    #[must_use]
    pub const fn zero() -> Self {
        Self { numerator: 0 }
    }

    /// Unit probability.
    #[must_use]
    pub const fn one() -> Self {
        Self {
            numerator: PROBABILITY_SCALE,
        }
    }

    /// Returns the exact numerator.
    #[must_use]
    pub const fn numerator(self) -> u64 {
        self.numerator
    }

    /// Returns whether this probability is exactly zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.numerator == 0
    }
}

/// An outcome represented exactly as a numerator over [`OUTCOME_SCALE`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Outcome {
    scaled: i64,
}

impl Outcome {
    /// Constructs a bounded fixed-point outcome.
    pub const fn try_from_scaled(scaled: i64) -> Result<Self, OpeError> {
        if scaled.unsigned_abs() > MAX_ABS_OUTCOME_UNITS {
            return Err(OpeError::OutcomeOutOfRange { scaled });
        }
        Ok(Self { scaled })
    }

    /// Returns the exact signed numerator.
    #[must_use]
    pub const fn scaled(self) -> i64 {
        self.scaled
    }
}

/// The estimator applied to the complete logged window.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OpeEstimator {
    /// Average the direct model's policy-specific expected outcomes.
    Direct = 1,
    /// Average clipped inverse-propensity-weighted observed outcomes.
    ImportanceWeighted = 2,
    /// Add a clipped importance-weighted residual to each direct estimate.
    DoublyRobust = 3,
}

impl OpeEstimator {
    const fn requires_direct_model(self) -> bool {
        matches!(self, Self::Direct | Self::DoublyRobust)
    }
}

/// Failure behavior fixed into every OPE identity.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OpeFailureBehavior {
    /// Preserve the registered fallback whenever any promotion gate fails.
    SelectPinnedFallback = 1,
}

/// Immutable inclusive stream window.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OpeWindow {
    first_sequence: u64,
    last_sequence: u64,
    observation_capacity: u64,
}

impl OpeWindow {
    /// Constructs a non-empty inclusive sequence window.
    pub const fn try_new(first_sequence: u64, last_sequence: u64) -> Result<Self, OpeError> {
        let Some(distance) = last_sequence.checked_sub(first_sequence) else {
            return Err(OpeError::ReversedWindow {
                first: first_sequence,
                last: last_sequence,
            });
        };
        let Some(observation_capacity) = distance.checked_add(1) else {
            return Err(OpeError::WindowLengthOverflow {
                first: first_sequence,
                last: last_sequence,
            });
        };
        Ok(Self {
            first_sequence,
            last_sequence,
            observation_capacity,
        })
    }

    /// Inclusive first source sequence.
    #[must_use]
    pub const fn first_sequence(self) -> u64 {
        self.first_sequence
    }

    /// Inclusive last source sequence.
    #[must_use]
    pub const fn last_sequence(self) -> u64 {
        self.last_sequence
    }

    /// Number of observations in the complete window.
    #[must_use]
    pub const fn observation_capacity(self) -> u64 {
        self.observation_capacity
    }
}

/// Complete immutable identity of one off-policy evaluation trial.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OpeIdentity {
    population_oid: ObjectId,
    window: OpeWindow,
    selection_oid: ObjectId,
    strata_oid: ObjectId,
    action_space_oid: ObjectId,
    policy_epoch_oid: ObjectId,
    regime_epoch: u64,
    candidate_policy_oid: ObjectId,
    fallback_policy_oid: ObjectId,
    estimator_oid: ObjectId,
    estimator: OpeEstimator,
    failure_behavior: OpeFailureBehavior,
}

impl OpeIdentity {
    /// Constructs the immutable population, sampling, policy, and estimator
    /// identity for a trial.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        population_oid: ObjectId,
        window: OpeWindow,
        selection_oid: ObjectId,
        strata_oid: ObjectId,
        action_space_oid: ObjectId,
        policy_epoch_oid: ObjectId,
        regime_epoch: u64,
        candidate_policy_oid: ObjectId,
        fallback_policy_oid: ObjectId,
        estimator_oid: ObjectId,
        estimator: OpeEstimator,
    ) -> Result<Self, OpeError> {
        if candidate_policy_oid.0 == fallback_policy_oid.0 {
            return Err(OpeError::CandidateEqualsFallback);
        }
        Ok(Self {
            population_oid,
            window,
            selection_oid,
            strata_oid,
            action_space_oid,
            policy_epoch_oid,
            regime_epoch,
            candidate_policy_oid,
            fallback_policy_oid,
            estimator_oid,
            estimator,
            failure_behavior: OpeFailureBehavior::SelectPinnedFallback,
        })
    }

    /// Population from which the logged window was selected.
    #[must_use]
    pub const fn population_oid(self) -> ObjectId {
        self.population_oid
    }

    /// Complete finite sequence window.
    #[must_use]
    pub const fn window(self) -> OpeWindow {
        self.window
    }

    /// Selection-policy identity.
    #[must_use]
    pub const fn selection_oid(self) -> ObjectId {
        self.selection_oid
    }

    /// Strata-definition identity.
    #[must_use]
    pub const fn strata_oid(self) -> ObjectId {
        self.strata_oid
    }

    /// Action-space identity.
    #[must_use]
    pub const fn action_space_oid(self) -> ObjectId {
        self.action_space_oid
    }

    /// Stream-sequenced decision-policy epoch.
    #[must_use]
    pub const fn policy_epoch_oid(self) -> ObjectId {
        self.policy_epoch_oid
    }

    /// Regime epoch under which the logged window is evaluated.
    #[must_use]
    pub const fn regime_epoch(self) -> u64 {
        self.regime_epoch
    }

    /// Candidate policy being evaluated.
    #[must_use]
    pub const fn candidate_policy_oid(self) -> ObjectId {
        self.candidate_policy_oid
    }

    /// Deterministic fallback policy.
    #[must_use]
    pub const fn fallback_policy_oid(self) -> ObjectId {
        self.fallback_policy_oid
    }

    /// Registered estimator implementation identity.
    #[must_use]
    pub const fn estimator_oid(self) -> ObjectId {
        self.estimator_oid
    }

    /// Estimator family bound by the identity.
    #[must_use]
    pub const fn estimator(self) -> OpeEstimator {
        self.estimator
    }

    /// Conservative behavior applied whenever a promotion gate fails.
    #[must_use]
    pub const fn failure_behavior(self) -> OpeFailureBehavior {
        self.failure_behavior
    }
}

/// Immutable resource and promotion gates for one ledger.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OpeProfile {
    clipping_weight_units: u64,
    minimum_effective_sample_size: u64,
    maximum_observations: usize,
    maximum_actions_per_observation: usize,
    maximum_total_action_rows: usize,
}

impl OpeProfile {
    /// Constructs explicit clipping, ESS, and history bounds.
    pub fn try_new(
        clipping_weight_units: u64,
        minimum_effective_sample_size: u64,
        maximum_observations: usize,
        maximum_actions_per_observation: usize,
        maximum_total_action_rows: usize,
    ) -> Result<Self, OpeError> {
        if clipping_weight_units == 0 {
            return Err(OpeError::ZeroClippingLimit);
        }
        if clipping_weight_units > MAX_CLIPPING_WEIGHT_UNITS {
            return Err(OpeError::ClippingLimitTooLarge {
                actual: clipping_weight_units,
                maximum: MAX_CLIPPING_WEIGHT_UNITS,
            });
        }
        if minimum_effective_sample_size == 0 {
            return Err(OpeError::ZeroMinimumEffectiveSampleSize);
        }
        if maximum_observations == 0 {
            return Err(OpeError::ZeroObservationLimit);
        }
        if maximum_observations > MAX_OBSERVATIONS {
            return Err(OpeError::ObservationLimitTooLarge {
                actual: maximum_observations,
                maximum: MAX_OBSERVATIONS,
            });
        }
        if maximum_actions_per_observation == 0 {
            return Err(OpeError::ZeroActionLimit);
        }
        if maximum_actions_per_observation > MAX_ACTIONS_PER_OBSERVATION {
            return Err(OpeError::ActionLimitTooLarge {
                actual: maximum_actions_per_observation,
                maximum: MAX_ACTIONS_PER_OBSERVATION,
            });
        }
        if maximum_total_action_rows == 0 {
            return Err(OpeError::ZeroTotalActionRowLimit);
        }
        if maximum_total_action_rows > MAX_TOTAL_ACTION_ROWS {
            return Err(OpeError::TotalActionRowLimitTooLarge {
                actual: maximum_total_action_rows,
                maximum: MAX_TOTAL_ACTION_ROWS,
            });
        }
        if maximum_total_action_rows < maximum_actions_per_observation {
            return Err(OpeError::TotalActionRowLimitBelowPerObservationLimit {
                total: maximum_total_action_rows,
                per_observation: maximum_actions_per_observation,
            });
        }
        let Ok(maximum_observations_u64) = u64::try_from(maximum_observations) else {
            return Err(OpeError::ObservationLimitUnrepresentable {
                actual: maximum_observations,
            });
        };
        if minimum_effective_sample_size > maximum_observations_u64 {
            return Err(
                OpeError::MinimumEffectiveSampleSizeExceedsObservationLimit {
                    minimum: minimum_effective_sample_size,
                    maximum_observations,
                },
            );
        }
        validate_arithmetic_envelope(
            clipping_weight_units,
            minimum_effective_sample_size,
            maximum_observations,
        )?;
        Ok(Self {
            clipping_weight_units,
            minimum_effective_sample_size,
            maximum_observations,
            maximum_actions_per_observation,
            maximum_total_action_rows,
        })
    }

    /// Importance-weight clipping numerator over [`WEIGHT_SCALE`].
    #[must_use]
    pub const fn clipping_weight_units(self) -> u64 {
        self.clipping_weight_units
    }

    /// Minimum exact ESS required for both evaluated policies.
    #[must_use]
    pub const fn minimum_effective_sample_size(self) -> u64 {
        self.minimum_effective_sample_size
    }

    /// Maximum observations retained by the ledger.
    #[must_use]
    pub const fn maximum_observations(self) -> usize {
        self.maximum_observations
    }

    /// Maximum actions in one fully logged distribution.
    #[must_use]
    pub const fn maximum_actions_per_observation(self) -> usize {
        self.maximum_actions_per_observation
    }

    /// Maximum action rows retained across the ledger.
    #[must_use]
    pub const fn maximum_total_action_rows(self) -> usize {
        self.maximum_total_action_rows
    }
}

/// One action row in a fully logged decision.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LoggedAction {
    action_oid: ObjectId,
    behavior_probability: Probability,
    candidate_probability: Probability,
    fallback_probability: Probability,
    direct_outcome: Option<Outcome>,
}

impl LoggedAction {
    /// Constructs one exact action-probability row.
    #[must_use]
    pub const fn new(
        action_oid: ObjectId,
        behavior_probability: Probability,
        candidate_probability: Probability,
        fallback_probability: Probability,
        direct_outcome: Option<Outcome>,
    ) -> Self {
        Self {
            action_oid,
            behavior_probability,
            candidate_probability,
            fallback_probability,
            direct_outcome,
        }
    }

    /// Stable action identity.
    #[must_use]
    pub const fn action_oid(self) -> ObjectId {
        self.action_oid
    }

    /// Probability assigned by the logging behavior.
    #[must_use]
    pub const fn behavior_probability(self) -> Probability {
        self.behavior_probability
    }

    /// Probability assigned by the candidate policy.
    #[must_use]
    pub const fn candidate_probability(self) -> Probability {
        self.candidate_probability
    }

    /// Probability assigned by the pinned fallback policy.
    #[must_use]
    pub const fn fallback_probability(self) -> Probability {
        self.fallback_probability
    }

    /// Optional direct-model prediction for this action and state.
    #[must_use]
    pub const fn direct_outcome(self) -> Option<Outcome> {
        self.direct_outcome
    }
}

/// One exact, fully logged action selection and observed outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggedDecision {
    sequence: u64,
    state_oid: ObjectId,
    features_oid: ObjectId,
    stratum_oid: ObjectId,
    selected_action_oid: ObjectId,
    observed_outcome: Outcome,
    actions: Vec<LoggedAction>,
}

impl LoggedDecision {
    /// Validates canonical action order, uniqueness, three complete
    /// probability distributions, and positive logging probability for the
    /// selected action.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        sequence: u64,
        state_oid: ObjectId,
        features_oid: ObjectId,
        stratum_oid: ObjectId,
        selected_action_oid: ObjectId,
        observed_outcome: Outcome,
        actions: Vec<LoggedAction>,
    ) -> Result<Self, OpeError> {
        if actions.is_empty() {
            return Err(OpeError::EmptyActionTable { sequence });
        }

        let mut behavior_sum = 0_u64;
        let mut candidate_sum = 0_u64;
        let mut fallback_sum = 0_u64;
        let mut previous_action = None;
        let mut selected_behavior_probability = None;

        for (index, action) in actions.iter().copied().enumerate() {
            if let Some(previous) = previous_action {
                if action.action_oid == previous {
                    return Err(OpeError::DuplicateAction {
                        sequence,
                        index,
                        action_oid: action.action_oid,
                    });
                }
                if action.action_oid < previous {
                    return Err(OpeError::ActionsOutOfOrder {
                        sequence,
                        index,
                        previous,
                        current: action.action_oid,
                    });
                }
            }
            previous_action = Some(action.action_oid);
            behavior_sum = behavior_sum
                .checked_add(action.behavior_probability.numerator)
                .ok_or(OpeError::ArithmeticOverflow)?;
            candidate_sum = candidate_sum
                .checked_add(action.candidate_probability.numerator)
                .ok_or(OpeError::ArithmeticOverflow)?;
            fallback_sum = fallback_sum
                .checked_add(action.fallback_probability.numerator)
                .ok_or(OpeError::ArithmeticOverflow)?;
            if action.action_oid == selected_action_oid {
                selected_behavior_probability = Some(action.behavior_probability);
            }
        }

        validate_distribution(sequence, PolicyDistribution::Behavior, behavior_sum)?;
        validate_distribution(sequence, PolicyDistribution::Candidate, candidate_sum)?;
        validate_distribution(sequence, PolicyDistribution::Fallback, fallback_sum)?;

        let selected_behavior_probability =
            selected_behavior_probability.ok_or(OpeError::SelectedActionMissing {
                sequence,
                selected_action_oid,
            })?;
        if selected_behavior_probability.is_zero() {
            return Err(OpeError::SelectedActionHasZeroBehaviorProbability {
                sequence,
                selected_action_oid,
            });
        }

        Ok(Self {
            sequence,
            state_oid,
            features_oid,
            stratum_oid,
            selected_action_oid,
            observed_outcome,
            actions,
        })
    }

    /// Exact source sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Logged state identity.
    #[must_use]
    pub const fn state_oid(&self) -> ObjectId {
        self.state_oid
    }

    /// Logged feature-vector identity.
    #[must_use]
    pub const fn features_oid(&self) -> ObjectId {
        self.features_oid
    }

    /// Logged stratum identity under the trial's immutable strata definition.
    #[must_use]
    pub const fn stratum_oid(&self) -> ObjectId {
        self.stratum_oid
    }

    /// Action selected by the logging behavior.
    #[must_use]
    pub const fn selected_action_oid(&self) -> ObjectId {
        self.selected_action_oid
    }

    /// Observed outcome.
    #[must_use]
    pub const fn observed_outcome(&self) -> Outcome {
        self.observed_outcome
    }

    /// Canonically ordered full action table.
    #[must_use]
    pub fn actions(&self) -> &[LoggedAction] {
        &self.actions
    }
}

/// Which evaluated policy assigned positive probability outside logging
/// support.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EvaluatedPolicy {
    /// Candidate policy.
    Candidate = 1,
    /// Pinned fallback policy.
    Fallback = 2,
}

/// Typed record of candidate or fallback mass outside behavior support.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ZeroSupportExclusion {
    sequence: u64,
    state_oid: ObjectId,
    features_oid: ObjectId,
    stratum_oid: ObjectId,
    action_oid: ObjectId,
    affected_policy: EvaluatedPolicy,
    unsupported_probability: Probability,
}

impl ZeroSupportExclusion {
    /// Source sequence at which support was absent.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    /// State containing the unsupported action.
    #[must_use]
    pub const fn state_oid(self) -> ObjectId {
        self.state_oid
    }

    /// Feature-vector identity for the decision.
    #[must_use]
    pub const fn features_oid(self) -> ObjectId {
        self.features_oid
    }

    /// Stratum containing the decision.
    #[must_use]
    pub const fn stratum_oid(self) -> ObjectId {
        self.stratum_oid
    }

    /// Action with zero behavior support.
    #[must_use]
    pub const fn action_oid(self) -> ObjectId {
        self.action_oid
    }

    /// Evaluated policy carrying unsupported mass.
    #[must_use]
    pub const fn affected_policy(self) -> EvaluatedPolicy {
        self.affected_policy
    }

    /// Exact unsupported probability assigned by that policy.
    #[must_use]
    pub const fn unsupported_probability(self) -> Probability {
        self.unsupported_probability
    }
}

/// Exact fixed-point mean emitted by an OPE report.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExactEstimate {
    numerator: i128,
    denominator: u128,
}

impl ExactEstimate {
    /// Exact signed numerator.
    #[must_use]
    pub const fn numerator(self) -> i128 {
        self.numerator
    }

    /// Exact positive denominator, or zero before the first observation.
    #[must_use]
    pub const fn denominator(self) -> u128 {
        self.denominator
    }

    /// Whether at least one observation contributes to this estimate.
    #[must_use]
    pub const fn is_available(self) -> bool {
        self.denominator != 0
    }
}

/// Exact effective sample size `(sum weights)^2 / sum squared weights`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExactEffectiveSampleSize {
    numerator: u128,
    denominator: u128,
}

impl ExactEffectiveSampleSize {
    /// Exact numerator.
    #[must_use]
    pub const fn numerator(self) -> u128 {
        self.numerator
    }

    /// Exact denominator, or zero when all weights are zero.
    #[must_use]
    pub const fn denominator(self) -> u128 {
        self.denominator
    }

    /// Checks an integer ESS threshold without floating-point rounding.
    #[must_use]
    pub fn meets(self, minimum: u64) -> bool {
        if self.denominator == 0 {
            return false;
        }
        let Some(required) = self.denominator.checked_mul(u128::from(minimum)) else {
            return false;
        };
        self.numerator >= required
    }
}

/// Conservative selection emitted by the completed evidence calculation.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OpeSelection {
    /// Continue using the immutable pinned fallback.
    PinnedFallback = 1,
    /// Select the candidate against the pinned fallback.
    Candidate = 2,
}

/// Deterministic reason for the emitted policy selection.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OpeSelectionReason {
    /// The declared stream window is incomplete.
    IncompleteWindow = 1,
    /// At least one evaluated action lacks behavior-policy support.
    ZeroSupport = 2,
    /// Candidate or fallback effective sample size is below the gate.
    InsufficientEffectiveSampleSize = 3,
    /// The candidate's exact estimate does not strictly exceed the fallback.
    CandidateNotBetter = 4,
    /// Every gate passed and the candidate estimate is strictly greater.
    CandidateEstimatedBetter = 5,
}

/// Replayable `FG-EVID-01` report for the current ledger prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpeEvidence {
    identity: OpeIdentity,
    profile: OpeProfile,
    observations: u64,
    action_rows: u64,
    complete: bool,
    candidate_estimate: ExactEstimate,
    fallback_estimate: ExactEstimate,
    advantage_estimate: ExactEstimate,
    candidate_effective_sample_size: ExactEffectiveSampleSize,
    fallback_effective_sample_size: ExactEffectiveSampleSize,
    candidate_ess_gate_passed: bool,
    fallback_ess_gate_passed: bool,
    zero_support_exclusions: Vec<ZeroSupportExclusion>,
    selection: OpeSelection,
    selection_reason: OpeSelectionReason,
}

impl OpeEvidence {
    /// Complete immutable evaluation identity.
    #[must_use]
    pub const fn identity(&self) -> OpeIdentity {
        self.identity
    }

    /// Explicit clipping, ESS, and resource profile.
    #[must_use]
    pub const fn profile(&self) -> OpeProfile {
        self.profile
    }

    /// Number of exact logged decisions included.
    #[must_use]
    pub const fn observations(&self) -> u64 {
        self.observations
    }

    /// Number of exact logged action rows included.
    #[must_use]
    pub const fn action_rows(&self) -> u64 {
        self.action_rows
    }

    /// Whether the declared inclusive sequence window is complete.
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.complete
    }

    /// Candidate value estimate.
    #[must_use]
    pub const fn candidate_estimate(&self) -> ExactEstimate {
        self.candidate_estimate
    }

    /// Pinned-fallback value estimate.
    #[must_use]
    pub const fn fallback_estimate(&self) -> ExactEstimate {
        self.fallback_estimate
    }

    /// Candidate-minus-fallback value estimate.
    #[must_use]
    pub const fn advantage_estimate(&self) -> ExactEstimate {
        self.advantage_estimate
    }

    /// Candidate-policy effective sample size.
    #[must_use]
    pub const fn candidate_effective_sample_size(&self) -> ExactEffectiveSampleSize {
        self.candidate_effective_sample_size
    }

    /// Fallback-policy effective sample size.
    #[must_use]
    pub const fn fallback_effective_sample_size(&self) -> ExactEffectiveSampleSize {
        self.fallback_effective_sample_size
    }

    /// Whether candidate ESS meets the exact configured threshold.
    #[must_use]
    pub const fn candidate_ess_gate_passed(&self) -> bool {
        self.candidate_ess_gate_passed
    }

    /// Whether fallback ESS meets the exact configured threshold.
    #[must_use]
    pub const fn fallback_ess_gate_passed(&self) -> bool {
        self.fallback_ess_gate_passed
    }

    /// Every typed zero-support exclusion in sequence/action/policy order.
    #[must_use]
    pub fn zero_support_exclusions(&self) -> &[ZeroSupportExclusion] {
        &self.zero_support_exclusions
    }

    /// Conservative policy selection.
    #[must_use]
    pub const fn selection(&self) -> OpeSelection {
        self.selection
    }

    /// Deterministic explanation of the selection.
    #[must_use]
    pub const fn selection_reason(&self) -> OpeSelectionReason {
        self.selection_reason
    }

    /// Policy selected by this report.
    #[must_use]
    pub const fn selected_policy_oid(&self) -> ObjectId {
        match self.selection {
            OpeSelection::PinnedFallback => self.identity.fallback_policy_oid,
            OpeSelection::Candidate => self.identity.candidate_policy_oid,
        }
    }
}

/// A bounded OPE ledger and its checked running statistics.
#[derive(Debug)]
pub struct OpeLedger {
    identity: OpeIdentity,
    profile: OpeProfile,
    observations: Vec<LoggedDecision>,
    zero_support_exclusions: Vec<ZeroSupportExclusion>,
    total_action_rows: usize,
    candidate_estimate_sum: i128,
    fallback_estimate_sum: i128,
    candidate_weight_sum: u128,
    candidate_weight_square_sum: u128,
    fallback_weight_sum: u128,
    fallback_weight_square_sum: u128,
}

impl OpeLedger {
    /// Constructs an empty ledger after checking that its declared complete
    /// window is representable under the profile.
    pub fn try_new(identity: OpeIdentity, profile: OpeProfile) -> Result<Self, OpeError> {
        let window_observations =
            usize::try_from(identity.window.observation_capacity).map_err(|_| {
                OpeError::WindowObservationCountUnrepresentable {
                    actual: identity.window.observation_capacity,
                }
            })?;
        if window_observations > profile.maximum_observations {
            return Err(OpeError::WindowExceedsObservationLimit {
                window_observations,
                maximum: profile.maximum_observations,
            });
        }
        if profile.minimum_effective_sample_size > identity.window.observation_capacity {
            return Err(OpeError::MinimumEffectiveSampleSizeExceedsWindow {
                minimum: profile.minimum_effective_sample_size,
                window_observations: identity.window.observation_capacity,
            });
        }
        if profile.maximum_total_action_rows < window_observations {
            return Err(OpeError::TotalActionRowLimitCannotCoverWindow {
                total: profile.maximum_total_action_rows,
                window_observations,
            });
        }
        Ok(Self {
            identity,
            profile,
            observations: Vec::new(),
            zero_support_exclusions: Vec::new(),
            total_action_rows: 0,
            candidate_estimate_sum: 0,
            fallback_estimate_sum: 0,
            candidate_weight_sum: 0,
            candidate_weight_square_sum: 0,
            fallback_weight_sum: 0,
            fallback_weight_square_sum: 0,
        })
    }

    /// Complete immutable evaluation identity.
    #[must_use]
    pub const fn identity(&self) -> OpeIdentity {
        self.identity
    }

    /// Explicit clipping, ESS, and resource profile.
    #[must_use]
    pub const fn profile(&self) -> OpeProfile {
        self.profile
    }

    /// Accepted logged decisions.
    #[must_use]
    pub fn observations(&self) -> &[LoggedDecision] {
        &self.observations
    }

    /// Accepted typed zero-support exclusions.
    #[must_use]
    pub fn zero_support_exclusions(&self) -> &[ZeroSupportExclusion] {
        &self.zero_support_exclusions
    }

    /// Returns whether the entire declared sequence window has been accepted.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        u64::try_from(self.observations.len())
            .is_ok_and(|count| count == self.identity.window.observation_capacity)
    }

    /// Accepts exactly the next logged decision.
    ///
    /// Every failure is atomic: sequence, model, resource, support, and
    /// arithmetic checks complete before any retained state is changed.
    pub fn record(&mut self, decision: LoggedDecision) -> Result<(), OpeError> {
        if self.is_complete() {
            return Err(OpeError::WindowAlreadyComplete);
        }
        let observation_count =
            u64::try_from(self.observations.len()).map_err(|_| OpeError::ArithmeticOverflow)?;
        let expected_sequence = self
            .identity
            .window
            .first_sequence
            .checked_add(observation_count)
            .ok_or(OpeError::ArithmeticOverflow)?;
        if decision.sequence != expected_sequence {
            return Err(OpeError::UnexpectedSequence {
                expected: expected_sequence,
                actual: decision.sequence,
            });
        }
        if decision.sequence > self.identity.window.last_sequence {
            return Err(OpeError::SequenceOutsideWindow {
                first: self.identity.window.first_sequence,
                last: self.identity.window.last_sequence,
                actual: decision.sequence,
            });
        }

        let action_count = decision.actions.len();
        if action_count > self.profile.maximum_actions_per_observation {
            return Err(OpeError::TooManyActions {
                sequence: decision.sequence,
                actual: action_count,
                maximum: self.profile.maximum_actions_per_observation,
            });
        }
        let new_total_action_rows = self
            .total_action_rows
            .checked_add(action_count)
            .ok_or(OpeError::ArithmeticOverflow)?;
        if new_total_action_rows > self.profile.maximum_total_action_rows {
            return Err(OpeError::TotalActionRowLimitExceeded {
                current: self.total_action_rows,
                incoming: action_count,
                maximum: self.profile.maximum_total_action_rows,
            });
        }

        let evaluated = evaluate_decision(&decision, self.identity.estimator, self.profile)?;
        let new_candidate_estimate_sum = self
            .candidate_estimate_sum
            .checked_add(evaluated.candidate_estimate)
            .ok_or(OpeError::ArithmeticOverflow)?;
        let new_fallback_estimate_sum = self
            .fallback_estimate_sum
            .checked_add(evaluated.fallback_estimate)
            .ok_or(OpeError::ArithmeticOverflow)?;
        let candidate_weight = u128::from(evaluated.candidate_weight);
        let fallback_weight = u128::from(evaluated.fallback_weight);
        let new_candidate_weight_sum = self
            .candidate_weight_sum
            .checked_add(candidate_weight)
            .ok_or(OpeError::ArithmeticOverflow)?;
        let new_candidate_weight_square_sum = self
            .candidate_weight_square_sum
            .checked_add(
                candidate_weight
                    .checked_mul(candidate_weight)
                    .ok_or(OpeError::ArithmeticOverflow)?,
            )
            .ok_or(OpeError::ArithmeticOverflow)?;
        let new_fallback_weight_sum = self
            .fallback_weight_sum
            .checked_add(fallback_weight)
            .ok_or(OpeError::ArithmeticOverflow)?;
        let new_fallback_weight_square_sum = self
            .fallback_weight_square_sum
            .checked_add(
                fallback_weight
                    .checked_mul(fallback_weight)
                    .ok_or(OpeError::ArithmeticOverflow)?,
            )
            .ok_or(OpeError::ArithmeticOverflow)?;

        self.observations
            .try_reserve(1)
            .map_err(|_| OpeError::HistoryAllocationFailed)?;
        self.zero_support_exclusions
            .try_reserve(evaluated.zero_support_exclusions.len())
            .map_err(|_| OpeError::ExclusionAllocationFailed)?;

        self.total_action_rows = new_total_action_rows;
        self.candidate_estimate_sum = new_candidate_estimate_sum;
        self.fallback_estimate_sum = new_fallback_estimate_sum;
        self.candidate_weight_sum = new_candidate_weight_sum;
        self.candidate_weight_square_sum = new_candidate_weight_square_sum;
        self.fallback_weight_sum = new_fallback_weight_sum;
        self.fallback_weight_square_sum = new_fallback_weight_square_sum;
        self.zero_support_exclusions
            .extend(evaluated.zero_support_exclusions);
        self.observations.push(decision);
        Ok(())
    }

    /// Produces a replayable report for the accepted prefix.
    pub fn evidence(&self) -> Result<OpeEvidence, OpeError> {
        let observations =
            u64::try_from(self.observations.len()).map_err(|_| OpeError::ArithmeticOverflow)?;
        let action_rows =
            u64::try_from(self.total_action_rows).map_err(|_| OpeError::ArithmeticOverflow)?;
        let outcome_scale =
            u128::try_from(OUTCOME_SCALE).map_err(|_| OpeError::ArithmeticOverflow)?;
        let estimate_denominator = u128::from(observations)
            .checked_mul(u128::from(PROBABILITY_SCALE))
            .and_then(|denominator| denominator.checked_mul(outcome_scale))
            .ok_or(OpeError::ArithmeticOverflow)?;
        let advantage_numerator = self
            .candidate_estimate_sum
            .checked_sub(self.fallback_estimate_sum)
            .ok_or(OpeError::ArithmeticOverflow)?;
        let candidate_effective_sample_size =
            exact_ess(self.candidate_weight_sum, self.candidate_weight_square_sum)?;
        let fallback_effective_sample_size =
            exact_ess(self.fallback_weight_sum, self.fallback_weight_square_sum)?;
        let candidate_ess_gate_passed =
            candidate_effective_sample_size.meets(self.profile.minimum_effective_sample_size);
        let fallback_ess_gate_passed =
            fallback_effective_sample_size.meets(self.profile.minimum_effective_sample_size);
        let complete = self.is_complete();
        let failure_selection = match self.identity.failure_behavior {
            OpeFailureBehavior::SelectPinnedFallback => OpeSelection::PinnedFallback,
        };

        let (selection, selection_reason) = if !complete {
            (failure_selection, OpeSelectionReason::IncompleteWindow)
        } else if !self.zero_support_exclusions.is_empty() {
            (failure_selection, OpeSelectionReason::ZeroSupport)
        } else if !candidate_ess_gate_passed || !fallback_ess_gate_passed {
            (
                failure_selection,
                OpeSelectionReason::InsufficientEffectiveSampleSize,
            )
        } else if advantage_numerator <= 0 {
            (failure_selection, OpeSelectionReason::CandidateNotBetter)
        } else {
            (
                OpeSelection::Candidate,
                OpeSelectionReason::CandidateEstimatedBetter,
            )
        };

        let mut zero_support_exclusions = Vec::new();
        zero_support_exclusions
            .try_reserve_exact(self.zero_support_exclusions.len())
            .map_err(|_| OpeError::ExclusionAllocationFailed)?;
        zero_support_exclusions.extend_from_slice(&self.zero_support_exclusions);

        Ok(OpeEvidence {
            identity: self.identity,
            profile: self.profile,
            observations,
            action_rows,
            complete,
            candidate_estimate: ExactEstimate {
                numerator: self.candidate_estimate_sum,
                denominator: estimate_denominator,
            },
            fallback_estimate: ExactEstimate {
                numerator: self.fallback_estimate_sum,
                denominator: estimate_denominator,
            },
            advantage_estimate: ExactEstimate {
                numerator: advantage_numerator,
                denominator: estimate_denominator,
            },
            candidate_effective_sample_size,
            fallback_effective_sample_size,
            candidate_ess_gate_passed,
            fallback_ess_gate_passed,
            zero_support_exclusions,
            selection,
            selection_reason,
        })
    }
}

/// One of the three complete action distributions in a logged decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyDistribution {
    /// Logging behavior that selected the observed action.
    Behavior,
    /// Candidate policy under evaluation.
    Candidate,
    /// Pinned deterministic fallback policy.
    Fallback,
}

#[derive(Debug)]
struct EvaluatedDecision {
    candidate_estimate: i128,
    fallback_estimate: i128,
    candidate_weight: u64,
    fallback_weight: u64,
    zero_support_exclusions: Vec<ZeroSupportExclusion>,
}

fn validate_distribution(
    sequence: u64,
    distribution: PolicyDistribution,
    actual_sum: u64,
) -> Result<(), OpeError> {
    if actual_sum != PROBABILITY_SCALE {
        return Err(OpeError::ProbabilityDistributionDoesNotSumToOne {
            sequence,
            distribution,
            actual_sum,
        });
    }
    Ok(())
}

fn evaluate_decision(
    decision: &LoggedDecision,
    estimator: OpeEstimator,
    profile: OpeProfile,
) -> Result<EvaluatedDecision, OpeError> {
    let mut selected_action = None;
    let mut candidate_direct_sum = 0_i128;
    let mut fallback_direct_sum = 0_i128;
    let mut exclusions = Vec::new();
    exclusions
        .try_reserve(decision.actions.len().saturating_mul(2))
        .map_err(|_| OpeError::ExclusionAllocationFailed)?;

    for action in decision.actions.iter().copied() {
        if action.behavior_probability.is_zero() {
            if !action.candidate_probability.is_zero() {
                exclusions.push(ZeroSupportExclusion {
                    sequence: decision.sequence,
                    state_oid: decision.state_oid,
                    features_oid: decision.features_oid,
                    stratum_oid: decision.stratum_oid,
                    action_oid: action.action_oid,
                    affected_policy: EvaluatedPolicy::Candidate,
                    unsupported_probability: action.candidate_probability,
                });
            }
            if !action.fallback_probability.is_zero() {
                exclusions.push(ZeroSupportExclusion {
                    sequence: decision.sequence,
                    state_oid: decision.state_oid,
                    features_oid: decision.features_oid,
                    stratum_oid: decision.stratum_oid,
                    action_oid: action.action_oid,
                    affected_policy: EvaluatedPolicy::Fallback,
                    unsupported_probability: action.fallback_probability,
                });
            }
        }

        if estimator.requires_direct_model() {
            let direct_outcome = action
                .direct_outcome
                .ok_or(OpeError::MissingDirectOutcome {
                    sequence: decision.sequence,
                    action_oid: action.action_oid,
                    estimator,
                })?;
            candidate_direct_sum = candidate_direct_sum
                .checked_add(checked_probability_outcome_product(
                    action.candidate_probability,
                    direct_outcome,
                )?)
                .ok_or(OpeError::ArithmeticOverflow)?;
            fallback_direct_sum = fallback_direct_sum
                .checked_add(checked_probability_outcome_product(
                    action.fallback_probability,
                    direct_outcome,
                )?)
                .ok_or(OpeError::ArithmeticOverflow)?;
        }

        if action.action_oid == decision.selected_action_oid {
            selected_action = Some(action);
        }
    }

    let selected_action = selected_action.ok_or(OpeError::SelectedActionMissing {
        sequence: decision.sequence,
        selected_action_oid: decision.selected_action_oid,
    })?;
    let candidate_weight = clipped_weight(
        selected_action.candidate_probability,
        selected_action.behavior_probability,
        profile.clipping_weight_units,
    )?;
    let fallback_weight = clipped_weight(
        selected_action.fallback_probability,
        selected_action.behavior_probability,
        profile.clipping_weight_units,
    )?;

    let observed = i128::from(decision.observed_outcome.scaled);

    let candidate_estimate = match estimator {
        OpeEstimator::Direct => candidate_direct_sum,
        OpeEstimator::ImportanceWeighted => {
            checked_weighted_outcome_numerator(candidate_weight, observed)?
        }
        OpeEstimator::DoublyRobust => {
            let selected_direct = i128::from(
                selected_action
                    .direct_outcome
                    .ok_or(OpeError::MissingDirectOutcome {
                        sequence: decision.sequence,
                        action_oid: selected_action.action_oid,
                        estimator,
                    })?
                    .scaled,
            );
            candidate_direct_sum
                .checked_add(checked_weighted_outcome_numerator(
                    candidate_weight,
                    observed
                        .checked_sub(selected_direct)
                        .ok_or(OpeError::ArithmeticOverflow)?,
                )?)
                .ok_or(OpeError::ArithmeticOverflow)?
        }
    };
    let fallback_estimate = match estimator {
        OpeEstimator::Direct => fallback_direct_sum,
        OpeEstimator::ImportanceWeighted => {
            checked_weighted_outcome_numerator(fallback_weight, observed)?
        }
        OpeEstimator::DoublyRobust => {
            let selected_direct = i128::from(
                selected_action
                    .direct_outcome
                    .ok_or(OpeError::MissingDirectOutcome {
                        sequence: decision.sequence,
                        action_oid: selected_action.action_oid,
                        estimator,
                    })?
                    .scaled,
            );
            fallback_direct_sum
                .checked_add(checked_weighted_outcome_numerator(
                    fallback_weight,
                    observed
                        .checked_sub(selected_direct)
                        .ok_or(OpeError::ArithmeticOverflow)?,
                )?)
                .ok_or(OpeError::ArithmeticOverflow)?
        }
    };

    Ok(EvaluatedDecision {
        candidate_estimate,
        fallback_estimate,
        candidate_weight,
        fallback_weight,
        zero_support_exclusions: exclusions,
    })
}

fn checked_probability_outcome_product(
    probability: Probability,
    outcome: Outcome,
) -> Result<i128, OpeError> {
    i128::from(probability.numerator)
        .checked_mul(i128::from(outcome.scaled))
        .ok_or(OpeError::ArithmeticOverflow)
}

fn checked_weighted_outcome_numerator(weight: u64, outcome: i128) -> Result<i128, OpeError> {
    i128::from(weight)
        .checked_mul(outcome)
        .and_then(|product| product.checked_mul(i128::from(WEIGHT_TO_PROBABILITY_SCALE)))
        .ok_or(OpeError::ArithmeticOverflow)
}

fn clipped_weight(
    evaluated_probability: Probability,
    behavior_probability: Probability,
    clipping_weight_units: u64,
) -> Result<u64, OpeError> {
    if evaluated_probability.is_zero() {
        return Ok(0);
    }
    if behavior_probability.is_zero() {
        return Ok(clipping_weight_units);
    }
    let scaled_numerator = u128::from(evaluated_probability.numerator)
        .checked_mul(u128::from(WEIGHT_SCALE))
        .ok_or(OpeError::ArithmeticOverflow)?;
    let raw_weight = scaled_numerator
        .checked_div(u128::from(behavior_probability.numerator))
        .ok_or(OpeError::ArithmeticOverflow)?;
    let raw_weight = u64::try_from(raw_weight).map_err(|_| OpeError::ArithmeticOverflow)?;
    Ok(raw_weight.min(clipping_weight_units))
}

fn exact_ess(
    weight_sum: u128,
    weight_square_sum: u128,
) -> Result<ExactEffectiveSampleSize, OpeError> {
    let numerator = weight_sum
        .checked_mul(weight_sum)
        .ok_or(OpeError::ArithmeticOverflow)?;
    Ok(ExactEffectiveSampleSize {
        numerator,
        denominator: weight_square_sum,
    })
}

fn validate_arithmetic_envelope(
    clipping_weight_units: u64,
    minimum_effective_sample_size: u64,
    maximum_observations: usize,
) -> Result<(), OpeError> {
    if !PROBABILITY_SCALE.is_multiple_of(WEIGHT_SCALE) {
        return Err(OpeError::ArithmeticEnvelopeUnrepresentable);
    }

    let observations = u128::try_from(maximum_observations)
        .map_err(|_| OpeError::ArithmeticEnvelopeUnrepresentable)?;
    let maximum_outcome = u128::from(MAX_ABS_OUTCOME_UNITS);
    let maximum_direct_numerator = u128::from(PROBABILITY_SCALE)
        .checked_mul(maximum_outcome)
        .ok_or(OpeError::ArithmeticEnvelopeUnrepresentable)?;
    let maximum_residual = maximum_outcome
        .checked_mul(2)
        .ok_or(OpeError::ArithmeticEnvelopeUnrepresentable)?;
    let maximum_weighted_residual_numerator = u128::from(clipping_weight_units)
        .checked_mul(maximum_residual)
        .and_then(|value| value.checked_mul(u128::from(WEIGHT_TO_PROBABILITY_SCALE)))
        .ok_or(OpeError::ArithmeticEnvelopeUnrepresentable)?;
    let maximum_estimate_sum = maximum_direct_numerator
        .checked_add(maximum_weighted_residual_numerator)
        .and_then(|value| value.checked_mul(observations))
        .ok_or(OpeError::ArithmeticEnvelopeUnrepresentable)?;
    let maximum_advantage_numerator = maximum_estimate_sum
        .checked_mul(2)
        .ok_or(OpeError::ArithmeticEnvelopeUnrepresentable)?;
    if maximum_advantage_numerator > i128::MAX as u128 {
        return Err(OpeError::ArithmeticEnvelopeUnrepresentable);
    }

    let maximum_weight_sum = u128::from(clipping_weight_units)
        .checked_mul(observations)
        .ok_or(OpeError::ArithmeticEnvelopeUnrepresentable)?;
    let _ = maximum_weight_sum
        .checked_mul(maximum_weight_sum)
        .ok_or(OpeError::ArithmeticEnvelopeUnrepresentable)?;
    let maximum_weight_square_sum = u128::from(clipping_weight_units)
        .checked_mul(u128::from(clipping_weight_units))
        .and_then(|value| value.checked_mul(observations))
        .ok_or(OpeError::ArithmeticEnvelopeUnrepresentable)?;
    let _ = maximum_weight_square_sum
        .checked_mul(u128::from(minimum_effective_sample_size))
        .ok_or(OpeError::ArithmeticEnvelopeUnrepresentable)?;

    let outcome_scale =
        u128::try_from(OUTCOME_SCALE).map_err(|_| OpeError::ArithmeticEnvelopeUnrepresentable)?;
    let _ = observations
        .checked_mul(u128::from(PROBABILITY_SCALE))
        .and_then(|value| value.checked_mul(outcome_scale))
        .ok_or(OpeError::ArithmeticEnvelopeUnrepresentable)?;
    Ok(())
}

/// Construction, ingestion, resource, or checked-arithmetic failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpeError {
    /// A probability numerator exceeded [`PROBABILITY_SCALE`].
    ProbabilityAboveOne {
        /// Rejected numerator.
        numerator: u64,
    },
    /// A fixed-point outcome exceeded the supported absolute bound.
    OutcomeOutOfRange {
        /// Rejected signed numerator.
        scaled: i64,
    },
    /// Candidate and fallback policy identities were equal.
    CandidateEqualsFallback,
    /// An inclusive sequence window was reversed.
    ReversedWindow {
        /// First sequence.
        first: u64,
        /// Last sequence.
        last: u64,
    },
    /// An inclusive sequence-window length overflowed.
    WindowLengthOverflow {
        /// First sequence.
        first: u64,
        /// Last sequence.
        last: u64,
    },
    /// Clipping limit was zero.
    ZeroClippingLimit,
    /// Clipping limit exceeded the implementation ceiling.
    ClippingLimitTooLarge {
        /// Requested fixed-point weight.
        actual: u64,
        /// Maximum fixed-point weight.
        maximum: u64,
    },
    /// Minimum ESS was zero.
    ZeroMinimumEffectiveSampleSize,
    /// Observation limit was zero.
    ZeroObservationLimit,
    /// Observation limit exceeded the implementation ceiling.
    ObservationLimitTooLarge {
        /// Requested limit.
        actual: usize,
        /// Maximum supported limit.
        maximum: usize,
    },
    /// Per-observation action limit was zero.
    ZeroActionLimit,
    /// Per-observation action limit exceeded the implementation ceiling.
    ActionLimitTooLarge {
        /// Requested limit.
        actual: usize,
        /// Maximum supported limit.
        maximum: usize,
    },
    /// Total action-row limit was zero.
    ZeroTotalActionRowLimit,
    /// Total action-row limit exceeded the implementation ceiling.
    TotalActionRowLimitTooLarge {
        /// Requested limit.
        actual: usize,
        /// Maximum supported limit.
        maximum: usize,
    },
    /// Total row limit was smaller than the per-observation row limit.
    TotalActionRowLimitBelowPerObservationLimit {
        /// Total limit.
        total: usize,
        /// Per-observation limit.
        per_observation: usize,
    },
    /// Observation limit could not be represented in the evidence domain.
    ObservationLimitUnrepresentable {
        /// Requested limit.
        actual: usize,
    },
    /// Minimum ESS exceeded the maximum observation count.
    MinimumEffectiveSampleSizeExceedsObservationLimit {
        /// Requested minimum.
        minimum: u64,
        /// Observation limit.
        maximum_observations: usize,
    },
    /// Trial-window length could not be represented by the host.
    WindowObservationCountUnrepresentable {
        /// Declared window length.
        actual: u64,
    },
    /// Trial window exceeded the observation bound.
    WindowExceedsObservationLimit {
        /// Declared trial length.
        window_observations: usize,
        /// Profile limit.
        maximum: usize,
    },
    /// Minimum ESS exceeded the complete trial window.
    MinimumEffectiveSampleSizeExceedsWindow {
        /// Requested minimum.
        minimum: u64,
        /// Complete window length.
        window_observations: u64,
    },
    /// Even one action per observation could not fit the total row limit.
    TotalActionRowLimitCannotCoverWindow {
        /// Total action-row limit.
        total: usize,
        /// Complete window length.
        window_observations: usize,
    },
    /// A logged decision had no actions.
    EmptyActionTable {
        /// Source sequence.
        sequence: u64,
    },
    /// Action table was not in strict ObjectId order.
    ActionsOutOfOrder {
        /// Source sequence.
        sequence: u64,
        /// Index of the current row.
        index: usize,
        /// Previous action.
        previous: ObjectId,
        /// Current action.
        current: ObjectId,
    },
    /// Action table repeated an action.
    DuplicateAction {
        /// Source sequence.
        sequence: u64,
        /// Index of the duplicate.
        index: usize,
        /// Duplicated action.
        action_oid: ObjectId,
    },
    /// One logged distribution did not sum exactly to one.
    ProbabilityDistributionDoesNotSumToOne {
        /// Source sequence.
        sequence: u64,
        /// Invalid distribution.
        distribution: PolicyDistribution,
        /// Exact observed numerator sum.
        actual_sum: u64,
    },
    /// Selected action did not occur in the full action table.
    SelectedActionMissing {
        /// Source sequence.
        sequence: u64,
        /// Missing selected action.
        selected_action_oid: ObjectId,
    },
    /// Selected action claimed zero behavior probability.
    SelectedActionHasZeroBehaviorProbability {
        /// Source sequence.
        sequence: u64,
        /// Invalid selected action.
        selected_action_oid: ObjectId,
    },
    /// Direct or doubly robust evaluation lacked an action prediction.
    MissingDirectOutcome {
        /// Source sequence.
        sequence: u64,
        /// Action without a prediction.
        action_oid: ObjectId,
        /// Estimator requiring the prediction.
        estimator: OpeEstimator,
    },
    /// A record did not have the exact next sequence.
    UnexpectedSequence {
        /// Required next sequence.
        expected: u64,
        /// Supplied sequence.
        actual: u64,
    },
    /// A supplied sequence lay outside the immutable trial window.
    SequenceOutsideWindow {
        /// Inclusive first sequence.
        first: u64,
        /// Inclusive last sequence.
        last: u64,
        /// Supplied sequence.
        actual: u64,
    },
    /// The complete trial window had already been consumed.
    WindowAlreadyComplete,
    /// One decision exceeded the profile's action bound.
    TooManyActions {
        /// Source sequence.
        sequence: u64,
        /// Supplied action count.
        actual: usize,
        /// Profile limit.
        maximum: usize,
    },
    /// Retaining one decision would exceed the total action-row bound.
    TotalActionRowLimitExceeded {
        /// Rows already retained.
        current: usize,
        /// Incoming rows.
        incoming: usize,
        /// Profile limit.
        maximum: usize,
    },
    /// Checked integer arithmetic failed.
    ArithmeticOverflow,
    /// Configured fixed-point bounds could exceed an accumulator.
    ArithmeticEnvelopeUnrepresentable,
    /// Space for the decision history could not be reserved.
    HistoryAllocationFailed,
    /// Space for explicit support exclusions could not be reserved.
    ExclusionAllocationFailed,
}

impl fmt::Display for OpeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProbabilityAboveOne { numerator } => write!(
                formatter,
                "probability numerator {numerator} exceeds scale {PROBABILITY_SCALE}"
            ),
            Self::OutcomeOutOfRange { scaled } => write!(
                formatter,
                "outcome numerator {scaled} exceeds absolute limit {MAX_ABS_OUTCOME_UNITS}"
            ),
            Self::CandidateEqualsFallback => {
                formatter.write_str("candidate and pinned fallback policies must differ")
            }
            Self::ReversedWindow { first, last } => {
                write!(formatter, "sequence window {first}..={last} is reversed")
            }
            Self::WindowLengthOverflow { first, last } => write!(
                formatter,
                "sequence window {first}..={last} has an unrepresentable length"
            ),
            Self::ZeroClippingLimit => formatter.write_str("clipping limit must be positive"),
            Self::ClippingLimitTooLarge { actual, maximum } => write!(
                formatter,
                "clipping limit {actual} exceeds implementation ceiling {maximum}"
            ),
            Self::ZeroMinimumEffectiveSampleSize => {
                formatter.write_str("minimum effective sample size must be positive")
            }
            Self::ZeroObservationLimit => {
                formatter.write_str("maximum observations must be positive")
            }
            Self::ObservationLimitTooLarge { actual, maximum } => write!(
                formatter,
                "observation limit {actual} exceeds implementation ceiling {maximum}"
            ),
            Self::ZeroActionLimit => {
                formatter.write_str("maximum actions per observation must be positive")
            }
            Self::ActionLimitTooLarge { actual, maximum } => write!(
                formatter,
                "action limit {actual} exceeds implementation ceiling {maximum}"
            ),
            Self::ZeroTotalActionRowLimit => {
                formatter.write_str("maximum total action rows must be positive")
            }
            Self::TotalActionRowLimitTooLarge { actual, maximum } => write!(
                formatter,
                "total action-row limit {actual} exceeds implementation ceiling {maximum}"
            ),
            Self::TotalActionRowLimitBelowPerObservationLimit {
                total,
                per_observation,
            } => write!(
                formatter,
                "total action-row limit {total} is below per-observation limit {per_observation}"
            ),
            Self::ObservationLimitUnrepresentable { actual } => write!(
                formatter,
                "observation limit {actual} is not representable in the evidence domain"
            ),
            Self::MinimumEffectiveSampleSizeExceedsObservationLimit {
                minimum,
                maximum_observations,
            } => write!(
                formatter,
                "minimum effective sample size {minimum} exceeds observation limit {maximum_observations}"
            ),
            Self::WindowObservationCountUnrepresentable { actual } => write!(
                formatter,
                "window observation count {actual} is not representable on this host"
            ),
            Self::WindowExceedsObservationLimit {
                window_observations,
                maximum,
            } => write!(
                formatter,
                "window has {window_observations} observations; profile maximum is {maximum}"
            ),
            Self::MinimumEffectiveSampleSizeExceedsWindow {
                minimum,
                window_observations,
            } => write!(
                formatter,
                "minimum effective sample size {minimum} exceeds window length {window_observations}"
            ),
            Self::TotalActionRowLimitCannotCoverWindow {
                total,
                window_observations,
            } => write!(
                formatter,
                "total action-row limit {total} cannot cover {window_observations} observations"
            ),
            Self::EmptyActionTable { sequence } => {
                write!(formatter, "decision at sequence {sequence} has no actions")
            }
            Self::ActionsOutOfOrder {
                sequence,
                index,
                previous,
                current,
            } => write!(
                formatter,
                "decision {sequence} action {index} ({current:?}) sorts before {previous:?}"
            ),
            Self::DuplicateAction {
                sequence,
                index,
                action_oid,
            } => write!(
                formatter,
                "decision {sequence} action {index} duplicates {action_oid:?}"
            ),
            Self::ProbabilityDistributionDoesNotSumToOne {
                sequence,
                distribution,
                actual_sum,
            } => write!(
                formatter,
                "decision {sequence} {distribution:?} probability sum is {actual_sum}, expected {PROBABILITY_SCALE}"
            ),
            Self::SelectedActionMissing {
                sequence,
                selected_action_oid,
            } => write!(
                formatter,
                "decision {sequence} selected action {selected_action_oid:?} is absent"
            ),
            Self::SelectedActionHasZeroBehaviorProbability {
                sequence,
                selected_action_oid,
            } => write!(
                formatter,
                "decision {sequence} selected action {selected_action_oid:?} has zero behavior probability"
            ),
            Self::MissingDirectOutcome {
                sequence,
                action_oid,
                estimator,
            } => write!(
                formatter,
                "decision {sequence} action {action_oid:?} lacks the direct outcome required by {estimator:?}"
            ),
            Self::UnexpectedSequence { expected, actual } => write!(
                formatter,
                "expected source sequence {expected}, received {actual}"
            ),
            Self::SequenceOutsideWindow {
                first,
                last,
                actual,
            } => write!(
                formatter,
                "source sequence {actual} lies outside {first}..={last}"
            ),
            Self::WindowAlreadyComplete => {
                formatter.write_str("the complete trial window is already recorded")
            }
            Self::TooManyActions {
                sequence,
                actual,
                maximum,
            } => write!(
                formatter,
                "decision {sequence} has {actual} actions; maximum is {maximum}"
            ),
            Self::TotalActionRowLimitExceeded {
                current,
                incoming,
                maximum,
            } => write!(
                formatter,
                "{current} retained action rows plus {incoming} incoming rows exceed maximum {maximum}"
            ),
            Self::ArithmeticOverflow => formatter.write_str("checked OPE arithmetic overflowed"),
            Self::ArithmeticEnvelopeUnrepresentable => {
                formatter.write_str("configured OPE arithmetic envelope is not representable")
            }
            Self::HistoryAllocationFailed => {
                formatter.write_str("could not reserve OPE history storage")
            }
            Self::ExclusionAllocationFailed => {
                formatter.write_str("could not reserve zero-support exclusion storage")
            }
        }
    }
}

impl std::error::Error for OpeError {}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const HALF: Probability = Probability {
        numerator: PROBABILITY_SCALE / 2,
    };

    fn oid(fill: u8) -> ObjectId {
        ObjectId([fill; 32])
    }

    fn outcome(scaled: i64) -> Result<Outcome, OpeError> {
        Outcome::try_from_scaled(scaled)
    }

    fn identity(estimator: OpeEstimator, first: u64, last: u64) -> Result<OpeIdentity, OpeError> {
        OpeIdentity::try_new(
            oid(1),
            OpeWindow::try_new(first, last)?,
            oid(2),
            oid(3),
            oid(4),
            oid(5),
            9,
            oid(6),
            oid(7),
            oid(8),
            estimator,
        )
    }

    fn profile(
        minimum_ess: u64,
        maximum_observations: usize,
        maximum_actions: usize,
        maximum_rows: usize,
    ) -> Result<OpeProfile, OpeError> {
        OpeProfile::try_new(
            10 * WEIGHT_SCALE,
            minimum_ess,
            maximum_observations,
            maximum_actions,
            maximum_rows,
        )
    }

    fn binary_decision(
        sequence: u64,
        selected_a: bool,
        behavior_a: Probability,
        candidate_a: Probability,
        fallback_a: Probability,
        outcome_a: Outcome,
        outcome_b: Outcome,
    ) -> Result<LoggedDecision, OpeError> {
        let selected_action = if selected_a { oid(20) } else { oid(21) };
        let observed_outcome = if selected_a { outcome_a } else { outcome_b };
        let behavior_b =
            Probability::try_from_numerator(PROBABILITY_SCALE - behavior_a.numerator())?;
        let candidate_b =
            Probability::try_from_numerator(PROBABILITY_SCALE - candidate_a.numerator())?;
        let fallback_b =
            Probability::try_from_numerator(PROBABILITY_SCALE - fallback_a.numerator())?;
        LoggedDecision::try_new(
            sequence,
            oid(30),
            oid(40),
            oid(50),
            selected_action,
            observed_outcome,
            vec![
                LoggedAction::new(
                    oid(20),
                    behavior_a,
                    candidate_a,
                    fallback_a,
                    Some(outcome_a),
                ),
                LoggedAction::new(
                    oid(21),
                    behavior_b,
                    candidate_b,
                    fallback_b,
                    Some(outcome_b),
                ),
            ],
        )
    }

    #[test]
    fn identity_binds_regime_and_conservative_failure_behavior() -> TestResult {
        let value = identity(OpeEstimator::DoublyRobust, 1, 1)?;
        assert_eq!(value.regime_epoch(), 9);
        assert_eq!(
            value.failure_behavior(),
            OpeFailureBehavior::SelectPinnedFallback
        );
        Ok(())
    }

    #[test]
    fn maximum_profile_has_a_representable_fixed_point_envelope() -> TestResult {
        let value = OpeProfile::try_new(
            MAX_CLIPPING_WEIGHT_UNITS,
            u64::try_from(MAX_OBSERVATIONS)?,
            MAX_OBSERVATIONS,
            MAX_ACTIONS_PER_OBSERVATION,
            MAX_TOTAL_ACTION_ROWS,
        )?;
        assert_eq!(value.maximum_observations(), MAX_OBSERVATIONS);
        Ok(())
    }

    #[test]
    fn all_three_logged_distributions_must_sum_exactly_to_one() -> TestResult {
        let short_half = Probability::try_from_numerator(HALF.numerator() - 1)?;
        let make_actions = |behavior_b, candidate_b, fallback_b| {
            vec![
                LoggedAction::new(oid(20), HALF, HALF, HALF, Some(Outcome { scaled: 0 })),
                LoggedAction::new(
                    oid(21),
                    behavior_b,
                    candidate_b,
                    fallback_b,
                    Some(Outcome { scaled: 0 }),
                ),
            ]
        };
        let construct = |actions| {
            LoggedDecision::try_new(
                1,
                oid(30),
                oid(40),
                oid(50),
                oid(20),
                Outcome { scaled: 0 },
                actions,
            )
        };

        assert_eq!(
            construct(make_actions(short_half, HALF, HALF)),
            Err(OpeError::ProbabilityDistributionDoesNotSumToOne {
                sequence: 1,
                distribution: PolicyDistribution::Behavior,
                actual_sum: PROBABILITY_SCALE - 1,
            })
        );
        assert_eq!(
            construct(make_actions(HALF, short_half, HALF)),
            Err(OpeError::ProbabilityDistributionDoesNotSumToOne {
                sequence: 1,
                distribution: PolicyDistribution::Candidate,
                actual_sum: PROBABILITY_SCALE - 1,
            })
        );
        assert_eq!(
            construct(make_actions(HALF, HALF, short_half)),
            Err(OpeError::ProbabilityDistributionDoesNotSumToOne {
                sequence: 1,
                distribution: PolicyDistribution::Fallback,
                actual_sum: PROBABILITY_SCALE - 1,
            })
        );
        Ok(())
    }

    #[test]
    fn direct_estimate_preserves_sub_outcome_unit_information() -> TestResult {
        let mut ledger =
            OpeLedger::try_new(identity(OpeEstimator::Direct, 1, 1)?, profile(1, 1, 2, 2)?)?;
        ledger.record(binary_decision(
            1,
            true,
            HALF,
            HALF,
            Probability::one(),
            outcome(0)?,
            outcome(1)?,
        )?)?;
        let evidence = ledger.evidence()?;

        assert_eq!(
            evidence.candidate_estimate(),
            ExactEstimate {
                numerator: i128::from(PROBABILITY_SCALE / 2),
                denominator: u128::from(PROBABILITY_SCALE) * u128::try_from(OUTCOME_SCALE)?,
            }
        );
        assert_eq!(evidence.fallback_estimate().numerator(), 0);
        assert_eq!(evidence.selection(), OpeSelection::Candidate);
        Ok(())
    }

    #[test]
    fn clipping_and_ess_thresholds_use_exact_fixed_point_inequalities() -> TestResult {
        let quarter = Probability::try_from_numerator(PROBABILITY_SCALE / 4)?;
        let three_quarters = Probability::try_from_numerator(3 * PROBABILITY_SCALE / 4)?;
        assert_eq!(
            clipped_weight(three_quarters, HALF, WEIGHT_SCALE + WEIGHT_SCALE / 4,)?,
            WEIGHT_SCALE + WEIGHT_SCALE / 4
        );
        assert_eq!(
            clipped_weight(quarter, HALF, 10 * WEIGHT_SCALE)?,
            WEIGHT_SCALE / 2
        );
        assert_eq!(
            clipped_weight(Probability::zero(), HALF, 10 * WEIGHT_SCALE)?,
            0
        );

        let fractional = ExactEffectiveSampleSize {
            numerator: 9,
            denominator: 5,
        };
        assert!(fractional.meets(1));
        assert!(!fractional.meets(2));
        let exact_boundary = ExactEffectiveSampleSize {
            numerator: 10,
            denominator: 5,
        };
        assert!(exact_boundary.meets(2));
        Ok(())
    }

    #[test]
    fn known_ground_truth_fixture_selects_candidate_for_all_estimators() -> TestResult {
        for estimator in [
            OpeEstimator::Direct,
            OpeEstimator::ImportanceWeighted,
            OpeEstimator::DoublyRobust,
        ] {
            let mut ledger =
                OpeLedger::try_new(identity(estimator, 100, 103)?, profile(2, 4, 2, 8)?)?;
            let one = outcome(OUTCOME_SCALE)?;
            let zero = outcome(0)?;
            for sequence in 100..=103 {
                ledger.record(binary_decision(
                    sequence,
                    sequence % 2 == 0,
                    HALF,
                    Probability::one(),
                    Probability::zero(),
                    one,
                    zero,
                )?)?;
            }
            let evidence = ledger.evidence()?;
            assert!(evidence.complete());
            assert_eq!(evidence.selection(), OpeSelection::Candidate);
            assert_eq!(
                evidence.selection_reason(),
                OpeSelectionReason::CandidateEstimatedBetter
            );
            assert_eq!(
                evidence.candidate_estimate().numerator(),
                4 * i128::from(PROBABILITY_SCALE) * i128::from(OUTCOME_SCALE)
            );
            assert_eq!(
                evidence.candidate_estimate().denominator(),
                4 * u128::from(PROBABILITY_SCALE) * u128::try_from(OUTCOME_SCALE)?
            );
            assert_eq!(evidence.fallback_estimate().numerator(), 0);
            assert_eq!(
                evidence.candidate_effective_sample_size(),
                ExactEffectiveSampleSize {
                    numerator: 16 * u128::from(WEIGHT_SCALE) * u128::from(WEIGHT_SCALE),
                    denominator: 8 * u128::from(WEIGHT_SCALE) * u128::from(WEIGHT_SCALE),
                }
            );
            assert!(evidence.zero_support_exclusions().is_empty());
        }
        Ok(())
    }

    #[test]
    fn positivity_violation_is_typed_and_forces_fallback() -> TestResult {
        let mut ledger =
            OpeLedger::try_new(identity(OpeEstimator::Direct, 1, 2)?, profile(1, 2, 2, 4)?)?;
        for sequence in 1..=2 {
            ledger.record(binary_decision(
                sequence,
                true,
                Probability::one(),
                Probability::zero(),
                Probability::one(),
                outcome(0)?,
                outcome(OUTCOME_SCALE)?,
            )?)?;
        }
        let evidence = ledger.evidence()?;
        assert_eq!(evidence.selection(), OpeSelection::PinnedFallback);
        assert_eq!(evidence.selection_reason(), OpeSelectionReason::ZeroSupport);
        assert_eq!(evidence.zero_support_exclusions().len(), 2);
        for exclusion in evidence.zero_support_exclusions() {
            assert_eq!(exclusion.affected_policy(), EvaluatedPolicy::Candidate);
            assert_eq!(exclusion.action_oid(), oid(21));
            assert_eq!(exclusion.unsupported_probability(), Probability::one());
        }
        Ok(())
    }

    #[test]
    fn support_exclusions_preserve_action_then_policy_order() -> TestResult {
        let quarter = Probability::try_from_numerator(PROBABILITY_SCALE / 4)?;
        let three_quarters = Probability::try_from_numerator(3 * PROBABILITY_SCALE / 4)?;
        let decision = LoggedDecision::try_new(
            1,
            oid(30),
            oid(40),
            oid(50),
            oid(20),
            outcome(0)?,
            vec![
                LoggedAction::new(
                    oid(20),
                    Probability::one(),
                    Probability::zero(),
                    Probability::zero(),
                    Some(outcome(0)?),
                ),
                LoggedAction::new(
                    oid(21),
                    Probability::zero(),
                    quarter,
                    three_quarters,
                    Some(outcome(0)?),
                ),
                LoggedAction::new(
                    oid(22),
                    Probability::zero(),
                    three_quarters,
                    quarter,
                    Some(outcome(0)?),
                ),
            ],
        )?;
        let mut ledger =
            OpeLedger::try_new(identity(OpeEstimator::Direct, 1, 1)?, profile(1, 1, 3, 3)?)?;
        ledger.record(decision)?;
        let evidence = ledger.evidence()?;
        let exclusions = evidence.zero_support_exclusions();

        assert_eq!(exclusions.len(), 4);
        assert_eq!(exclusions[0].action_oid(), oid(21));
        assert_eq!(exclusions[0].affected_policy(), EvaluatedPolicy::Candidate);
        assert_eq!(exclusions[0].unsupported_probability(), quarter);
        assert_eq!(exclusions[1].action_oid(), oid(21));
        assert_eq!(exclusions[1].affected_policy(), EvaluatedPolicy::Fallback);
        assert_eq!(exclusions[1].unsupported_probability(), three_quarters);
        assert_eq!(exclusions[2].action_oid(), oid(22));
        assert_eq!(exclusions[2].affected_policy(), EvaluatedPolicy::Candidate);
        assert_eq!(exclusions[2].unsupported_probability(), three_quarters);
        assert_eq!(exclusions[3].action_oid(), oid(22));
        assert_eq!(exclusions[3].affected_policy(), EvaluatedPolicy::Fallback);
        assert_eq!(exclusions[3].unsupported_probability(), quarter);
        assert_eq!(evidence.selection(), OpeSelection::PinnedFallback);
        assert_eq!(evidence.selection_reason(), OpeSelectionReason::ZeroSupport);
        Ok(())
    }

    #[test]
    fn low_effective_sample_size_forces_fallback() -> TestResult {
        let mut ledger = OpeLedger::try_new(
            identity(OpeEstimator::Direct, 10, 13)?,
            profile(3, 4, 2, 8)?,
        )?;
        let zero = outcome(0)?;
        let one = outcome(OUTCOME_SCALE)?;
        for sequence in 10..=12 {
            ledger.record(binary_decision(
                sequence, true, HALF, HALF, HALF, zero, one,
            )?)?;
        }
        let ninety_nine_percent = Probability::try_from_numerator(PROBABILITY_SCALE * 99 / 100)?;
        ledger.record(binary_decision(
            13,
            false,
            ninety_nine_percent,
            HALF,
            ninety_nine_percent,
            zero,
            one,
        )?)?;

        let evidence = ledger.evidence()?;
        assert!(evidence.zero_support_exclusions().is_empty());
        assert!(!evidence.candidate_ess_gate_passed());
        assert!(evidence.fallback_ess_gate_passed());
        assert!(evidence.advantage_estimate().numerator() > 0);
        assert_eq!(evidence.selection(), OpeSelection::PinnedFallback);
        assert_eq!(
            evidence.selection_reason(),
            OpeSelectionReason::InsufficientEffectiveSampleSize
        );
        Ok(())
    }

    #[test]
    fn identical_logs_produce_identical_evidence() -> TestResult {
        let identity = identity(OpeEstimator::DoublyRobust, 50, 53)?;
        let profile = profile(2, 4, 2, 8)?;
        let records = [
            binary_decision(
                50,
                true,
                HALF,
                Probability::one(),
                Probability::zero(),
                outcome(OUTCOME_SCALE)?,
                outcome(0)?,
            )?,
            binary_decision(
                51,
                false,
                HALF,
                Probability::one(),
                Probability::zero(),
                outcome(OUTCOME_SCALE)?,
                outcome(0)?,
            )?,
            binary_decision(
                52,
                true,
                HALF,
                Probability::one(),
                Probability::zero(),
                outcome(OUTCOME_SCALE)?,
                outcome(0)?,
            )?,
            binary_decision(
                53,
                false,
                HALF,
                Probability::one(),
                Probability::zero(),
                outcome(OUTCOME_SCALE)?,
                outcome(0)?,
            )?,
        ];
        let mut left = OpeLedger::try_new(identity, profile)?;
        let mut right = OpeLedger::try_new(identity, profile)?;
        for record in records {
            left.record(record.clone())?;
            right.record(record)?;
        }
        assert_eq!(left.evidence()?, right.evidence()?);
        Ok(())
    }

    #[test]
    fn rejected_sequence_is_atomic() -> TestResult {
        let mut ledger = OpeLedger::try_new(
            identity(OpeEstimator::ImportanceWeighted, 5, 6)?,
            profile(1, 2, 2, 4)?,
        )?;
        let first = binary_decision(
            5,
            true,
            HALF,
            HALF,
            HALF,
            outcome(OUTCOME_SCALE)?,
            outcome(0)?,
        )?;
        ledger.record(first)?;
        let before = ledger.evidence()?;
        let error = ledger.record(binary_decision(
            5,
            false,
            HALF,
            HALF,
            HALF,
            outcome(OUTCOME_SCALE)?,
            outcome(0)?,
        )?);
        assert!(matches!(
            error,
            Err(OpeError::UnexpectedSequence {
                expected: 6,
                actual: 5
            })
        ));
        assert_eq!(ledger.evidence()?, before);
        Ok(())
    }

    #[test]
    fn total_action_row_cap_rejects_atomically() -> TestResult {
        let mut ledger = OpeLedger::try_new(
            identity(OpeEstimator::Direct, 20, 21)?,
            profile(1, 2, 2, 3)?,
        )?;
        ledger.record(binary_decision(
            20,
            true,
            HALF,
            HALF,
            HALF,
            outcome(OUTCOME_SCALE)?,
            outcome(0)?,
        )?)?;
        let before = ledger.evidence()?;
        let error = ledger.record(binary_decision(
            21,
            false,
            HALF,
            HALF,
            HALF,
            outcome(OUTCOME_SCALE)?,
            outcome(0)?,
        )?);
        assert!(matches!(
            error,
            Err(OpeError::TotalActionRowLimitExceeded {
                current: 2,
                incoming: 2,
                maximum: 3
            })
        ));
        assert_eq!(ledger.evidence()?, before);
        Ok(())
    }

    #[test]
    fn incomplete_window_never_selects_candidate() -> TestResult {
        let mut ledger = OpeLedger::try_new(
            identity(OpeEstimator::Direct, 30, 31)?,
            profile(1, 2, 2, 4)?,
        )?;
        ledger.record(binary_decision(
            30,
            true,
            HALF,
            Probability::one(),
            Probability::zero(),
            outcome(OUTCOME_SCALE)?,
            outcome(0)?,
        )?)?;
        let evidence = ledger.evidence()?;
        assert_eq!(evidence.selection(), OpeSelection::PinnedFallback);
        assert_eq!(
            evidence.selection_reason(),
            OpeSelectionReason::IncompleteWindow
        );
        Ok(())
    }

    #[test]
    fn missing_direct_prediction_is_an_atomic_rejection() -> TestResult {
        let mut ledger = OpeLedger::try_new(
            identity(OpeEstimator::DoublyRobust, 40, 40)?,
            profile(1, 1, 2, 2)?,
        )?;
        let decision = LoggedDecision::try_new(
            40,
            oid(30),
            oid(31),
            oid(32),
            oid(20),
            outcome(OUTCOME_SCALE)?,
            vec![
                LoggedAction::new(oid(20), HALF, HALF, HALF, None),
                LoggedAction::new(oid(21), HALF, HALF, HALF, Some(outcome(0)?)),
            ],
        )?;
        let error = ledger.record(decision);
        assert!(matches!(
            error,
            Err(OpeError::MissingDirectOutcome {
                sequence: 40,
                action_oid,
                estimator: OpeEstimator::DoublyRobust
            }) if action_oid == oid(20)
        ));
        assert!(ledger.observations().is_empty());
        assert!(ledger.zero_support_exclusions().is_empty());
        Ok(())
    }
}
