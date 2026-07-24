//! Canonical statistical calibration log.
//!
//! Records in this module are statistical evidence by construction. There is
//! no claim-class field a caller can change, no invariant constructor, and no
//! conversion to an invariant claim. Each record binds a closed monitor and
//! statistic vocabulary to immutable evidence, stream, regime, candidate,
//! fallback, and registered terminal-action identities. The bounded log
//! accepts a deterministic total record order and enforces strictly ordered,
//! nonoverlapping batches independently for each monitor family.

use core::{cmp::Ordering, fmt};

use fgdb_claim::RegistryClaimClass;
use fgdb_types::ObjectId;

use crate::{
    ann_recall::{
        AnnRecallAction, AnnRecallActionReason, AnnRecallAssumptions, AnnRecallEvidence,
        AnnRecallProfile, QuerySampleDesign, RECALL_SCALE,
    },
    conformal::{
        AssessmentEvidence as ConformalEvidence, MetricThresholdMode,
        PolicySelection as ConformalPolicySelection,
    },
    eprocess::{EProcessConfig, EvidenceRecord as EProcessEvidence},
    exploration::ExplorationBudgetEvidence,
    ope::{
        EvaluatedPolicy, OUTCOME_SCALE, OpeEstimator, OpeEvidence, OpeFailureBehavior, OpeProfile,
        OpeSelectionReason, PROBABILITY_SCALE,
    },
    progress::DrainProgressEvidence,
    regime::RegimeSignalEvidence,
};

/// Canonical record encoding version.
pub const STATISTICAL_LOG_RECORD_VERSION: u16 = 4;

/// Canonical bounded-log encoding version.
pub const STATISTICAL_LOG_VERSION: u16 = 4;

/// Absolute record-count ceiling for one in-memory log.
pub const MAX_STATISTICAL_LOG_RECORDS: usize = 1_048_576;

/// Caller-owned admission bounds for a canonical statistical log.
///
/// These bounds are independent of the encoded log profile so input bytes
/// cannot authorize their own allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatisticalLogDecodeLimits {
    /// Largest encoded profile or concrete record count accepted.
    pub max_records: usize,
    /// Largest complete canonical log accepted.
    pub max_encoded_bytes: usize,
}

impl StatisticalLogDecodeLimits {
    /// Creates an explicit decode admission policy.
    #[must_use]
    pub const fn new(max_records: usize, max_encoded_bytes: usize) -> Self {
        Self {
            max_records,
            max_encoded_bytes,
        }
    }
}

const RECORD_MAGIC: [u8; 8] = *b"FGDBSLR4";
const LOG_MAGIC: [u8; 8] = *b"FGDBSLL4";
const MONITOR_IDENTITY_BODY_DOMAIN: &[u8] = b"fgdb:statistical-log-record:monitor-identity:v4";
const EVIDENCE_BODY_DOMAIN: &[u8] = b"fgdb:statistical-log-record:evidence-body:v4";
const OPE_SUPPORT_EXCLUSIONS_DOMAIN: &[u8] =
    b"fgdb:statistical-log-record:ope-support-exclusions:v1";
const RECORD_TAG: u8 = 1;
const STATISTICAL_CLAIM_TAG: u8 = 1;
const RECORD_RESERVED: u16 = 0;
const LOG_RESERVED: u16 = 0;
const RECORD_FIXED_BYTES: usize = 314;
const LOG_HEADER_BYTES: usize = 20;
const MIN_STATISTIC_PAYLOAD_BYTES: usize = 25;
const MAX_STATISTIC_PAYLOAD_BYTES: usize = 402;
const MIN_CANONICAL_RECORD_BYTES: usize = RECORD_FIXED_BYTES + MIN_STATISTIC_PAYLOAD_BYTES;
const MAX_CANONICAL_RECORD_BYTES: usize = RECORD_FIXED_BYTES + MAX_STATISTIC_PAYLOAD_BYTES;

/// Unkeyed digest of the canonical statistical-evidence body.
///
/// This digest detects transcript drift and lets an external identity
/// authority verify exactly which bytes an evidence OID names. It is
/// deliberately a distinct type: it is **not** a FrankenGraphDB [`ObjectId`]
/// and must never be reinterpreted as one. Production OID issuance and
/// verification require the database namespace and its keyed identity
/// authority.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StatisticalEvidenceDigest([u8; 32]);

impl StatisticalEvidenceDigest {
    /// Exact digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Failure reported by a namespace-aware evidence identity authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatisticalEvidenceIdentityError {
    /// The namespace key or identity service was unavailable.
    Unavailable,
    /// The supplied OID did not authenticate the canonical evidence body.
    Rejected,
}

impl fmt::Display for StatisticalEvidenceIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "statistical evidence identity authority is unavailable",
            Self::Rejected => {
                "statistical evidence OID does not authenticate its canonical evidence body"
            }
        })
    }
}

impl std::error::Error for StatisticalEvidenceIdentityError {}

/// Namespace-aware issuer for a canonical statistical-evidence body.
///
/// Production implementations must use the database's `K_oid` and
/// `DatabaseSecurityNamespaceId`; an unkeyed transcript digest is not an
/// implementation of this contract.
pub trait StatisticalEvidenceIdentityIssuer {
    /// Issues the authoritative ObjectId for one domain-separated, stable
    /// monitor identity.
    ///
    /// The default deliberately routes through the same namespace-aware
    /// authority while retaining a separate semantic entry point. The
    /// canonical bytes carry their own monitor-identity domain separator.
    fn issue_statistical_monitor_oid(
        &self,
        canonical_monitor_identity: &[u8],
    ) -> Result<ObjectId, StatisticalEvidenceIdentityError> {
        self.issue_statistical_evidence_oid(canonical_monitor_identity)
    }

    /// Issues the authoritative ObjectId for exactly `canonical_evidence_body`.
    fn issue_statistical_evidence_oid(
        &self,
        canonical_evidence_body: &[u8],
    ) -> Result<ObjectId, StatisticalEvidenceIdentityError>;
}

/// Namespace-aware verifier for a canonical statistical-evidence body.
///
/// Canonical decoding requires this verifier and therefore cannot silently
/// accept an unverified caller-supplied ObjectId.
pub trait StatisticalEvidenceIdentityVerifier {
    /// Authenticates `monitor_oid` against one domain-separated, stable
    /// monitor identity.
    fn verify_statistical_monitor_oid(
        &self,
        canonical_monitor_identity: &[u8],
        monitor_oid: ObjectId,
    ) -> Result<(), StatisticalEvidenceIdentityError> {
        self.verify_statistical_evidence_oid(canonical_monitor_identity, monitor_oid)
    }

    /// Authenticates `evidence_oid` against exactly
    /// `canonical_evidence_body`.
    fn verify_statistical_evidence_oid(
        &self,
        canonical_evidence_body: &[u8],
        evidence_oid: ObjectId,
    ) -> Result<(), StatisticalEvidenceIdentityError>;
}

/// Closed conformal threshold direction retained by canonical evidence.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StatisticalConformalMode {
    /// Only values above the threshold are nonconforming.
    Upper = 1,
    /// Deviations on either side of the calibration median are nonconforming.
    TwoSided = 2,
}

impl StatisticalConformalMode {
    const fn from_runtime(mode: MetricThresholdMode) -> Self {
        match mode {
            MetricThresholdMode::Upper => Self::Upper,
            MetricThresholdMode::TwoSided => Self::TwoSided,
        }
    }

    fn try_from_tag(tag: u8) -> Result<Self, StatisticalLogCodecError> {
        match tag {
            1 => Ok(Self::Upper),
            2 => Ok(Self::TwoSided),
            _ => Err(StatisticalLogCodecError::UnknownConformalMode { tag }),
        }
    }
}

/// Closed vocabulary of statistical monitors supported by the calibration
/// plane.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StatisticalMonitorKind {
    /// Anytime-valid e-process trial.
    EProcess = 1,
    /// Split-conformal threshold assessment.
    ConformalThreshold = 2,
    /// Finite-sample exploration-budget estimate.
    ExplorationBudget = 3,
    /// Drain-progress concentration evidence.
    DrainProgress = 4,
    /// Combined change-point regime signal.
    RegimeChange = 5,
    /// Off-policy value comparison.
    OffPolicyEvaluation = 6,
    /// Terminal approximate-nearest-neighbor recall assessment.
    AnnRecall = 7,
}

impl StatisticalMonitorKind {
    const fn canonical_tag(self) -> u8 {
        self as u8
    }

    fn try_from_tag(tag: u8) -> Result<Self, StatisticalLogCodecError> {
        match tag {
            1 => Ok(Self::EProcess),
            2 => Ok(Self::ConformalThreshold),
            3 => Ok(Self::ExplorationBudget),
            4 => Ok(Self::DrainProgress),
            5 => Ok(Self::RegimeChange),
            6 => Ok(Self::OffPolicyEvaluation),
            7 => Ok(Self::AnnRecall),
            _ => Err(StatisticalLogCodecError::UnknownMonitorKind { tag }),
        }
    }
}

/// Exact statistic payload emitted by one closed monitor family.
///
/// The OPE and ANN variants deliberately remain inline: records are immutable,
/// allocation-free values copied atomically by the bounded log, and boxing
/// would add a fallible hidden allocation to append and replay.
#[allow(
    clippy::large_enum_variant,
    reason = "durable statistical records keep complete OPE and ANN evidence inline"
)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StatisticalStatistic {
    /// E-process value and rejection boundary.
    EProcess {
        /// Registered identity of the complete e-process configuration.
        profile_oid: ObjectId,
        /// Exact canonical IEEE-754 null-probability bits.
        p0_bits: u64,
        /// Exact canonical IEEE-754 betting-factor bits.
        lambda_bits: u64,
        /// Exact canonical IEEE-754 significance-level bits.
        alpha_bits: u64,
        /// Exact canonical IEEE-754 saturation-bound bits.
        max_evalue_bits: u64,
        /// Exact IEEE-754 e-value bits.
        e_value_bits: u64,
        /// Exact IEEE-754 rejection-threshold bits.
        rejection_threshold_bits: u64,
        /// Accepted binary observations.
        observations: u64,
        /// Binary-one observations. Betting direction determines whether one
        /// or zero grows the e-process.
        one_observations: u64,
    },
    /// Split-conformal assessment and realized coverage.
    ConformalCoverage {
        /// Registered identity of the complete conformal configuration.
        profile_oid: ObjectId,
        /// Population over which the metric is interpreted.
        population_oid: ObjectId,
        /// Selection rule that formed the calibration stream.
        selection_oid: ObjectId,
        /// Exact canonical IEEE-754 miscoverage level.
        alpha_bits: u64,
        /// Threshold direction.
        mode: StatisticalConformalMode,
        /// Minimum calibration observations required for readiness.
        minimum_calibration_samples: u64,
        /// Maximum calibration observations admitted by the profile.
        maximum_calibration_samples: u64,
        /// Exact IEEE-754 threshold bits; positive infinity is a valid
        /// vacuous threshold.
        threshold_bits: u64,
        /// Exact IEEE-754 nonconformity-score bits.
        nonconformity_score_bits: u64,
        /// Exact IEEE-754 target-coverage bits.
        coverage_target_bits: u64,
        /// Ready held-out assessments.
        assessments: u64,
        /// Conforming ready held-out assessments.
        covered: u64,
    },
    /// Exploration residual-novelty estimate.
    ExplorationBudget {
        /// Exact canonical IEEE-754 target-miscoverage bits.
        alpha_bits: u64,
        /// Exact IEEE-754 empirical residual-rate bits.
        residual_rate_bits: u64,
        /// Exact IEEE-754 finite-sample upper-bound bits.
        upper_bound_bits: u64,
        /// Exact IEEE-754 target residual-rate bits.
        target_rate_bits: u64,
        /// Observed runs.
        total_runs: u64,
        /// Runs discovering a new class.
        discoveries: u64,
        /// Recommended additional existing-class runs.
        recommended_additional_runs: u64,
    },
    /// Drain-progress concentration projection.
    DrainProgress {
        /// Exact IEEE-754 current-potential bits.
        current_potential_bits: u64,
        /// Exact IEEE-754 confidence-bound bits.
        confidence_bound_bits: u64,
        /// Accepted potential observations.
        observations: u64,
        /// Whether the foundation verdict detected a stall.
        stall_detected: bool,
    },
    /// Combined regime-change receipt summary.
    RegimeChange {
        /// Exact signed detector statistic.
        statistic: i64,
        /// Exact positive detector threshold.
        threshold: i64,
        /// Accepted metric observations.
        observations: u64,
        /// Foundation-surfaced combined receipts.
        detections: u64,
    },
    /// Exact rational off-policy values and effective sample size.
    OffPolicyEvaluation {
        /// Population from which the logged trial window was selected.
        population_oid: ObjectId,
        /// Strata definition used by every logged decision.
        strata_oid: ObjectId,
        /// Complete action-space identity.
        action_space_oid: ObjectId,
        /// Decision-policy epoch under which the trial was logged.
        policy_epoch_oid: ObjectId,
        /// Registered estimator implementation identity.
        estimator_oid: ObjectId,
        /// Closed estimator family.
        estimator: OpeEstimator,
        /// Conservative behavior used when any promotion gate fails.
        failure_behavior: OpeFailureBehavior,
        /// Importance-weight clipping numerator.
        clipping_weight_units: u64,
        /// Exact minimum ESS required for both evaluated policies.
        minimum_effective_sample_size: u64,
        /// Maximum observations admitted by the immutable profile.
        maximum_observations: u64,
        /// Maximum action rows admitted per observation.
        maximum_actions_per_observation: u64,
        /// Maximum action rows admitted across the complete trial.
        maximum_total_action_rows: u64,
        /// Candidate estimate numerator.
        candidate_numerator: i128,
        /// Fallback estimate numerator.
        fallback_numerator: i128,
        /// Candidate-minus-fallback estimate numerator.
        advantage_numerator: i128,
        /// Positive common estimate denominator.
        common_denominator: u128,
        /// Candidate effective-sample-size numerator.
        candidate_ess_numerator: u128,
        /// Candidate effective-sample-size denominator, or zero with a zero
        /// numerator.
        candidate_ess_denominator: u128,
        /// Fallback effective-sample-size numerator.
        fallback_ess_numerator: u128,
        /// Fallback effective-sample-size denominator, or zero with a zero
        /// numerator.
        fallback_ess_denominator: u128,
        /// Logged decisions.
        observations: u64,
        /// Logged action rows.
        action_rows: u64,
        /// Whether the complete declared window was consumed.
        complete: bool,
        /// Exact candidate ESS gate result.
        candidate_ess_gate_passed: bool,
        /// Exact fallback ESS gate result.
        fallback_ess_gate_passed: bool,
        /// Explicit zero-support exclusions.
        zero_support_exclusions: u64,
        /// Canonical commitment to every typed support exclusion.
        zero_support_exclusions_digest: StatisticalEvidenceDigest,
        /// Deterministic reason for the emitted selection.
        selection_reason: OpeSelectionReason,
    },
    /// Complete terminal `FG-CAL-03` ANN-recall evidence.
    AnnRecall {
        /// Registered identity of the complete recall profile.
        profile_oid: ObjectId,
        /// Authorized population from which query samples were drawn.
        authorized_population_oid: ObjectId,
        /// Immutable graph and ANN-index snapshot under evaluation.
        snapshot_oid: ObjectId,
        /// Authority domain fixed for the complete trial.
        authority_domain_oid: ObjectId,
        /// Identity of the keyed sample, never raw key material.
        sample_key_oid: ObjectId,
        /// Registered query-sampling design identity.
        sample_design_oid: ObjectId,
        /// Registered exact-baseline implementation and profile.
        exact_baseline_oid: ObjectId,
        /// Pinned rebuild action identity.
        rebuild_policy_oid: ObjectId,
        /// Exact result-list length for every measured query.
        top_k: u64,
        /// Maximum query observations admitted by the profile.
        maximum_queries: u64,
        /// Maximum retained baseline plus candidate result identities.
        maximum_total_result_ids: u64,
        /// `q` in the interval failure-probability bound `2^-q`.
        confidence_exponent: u8,
        /// Candidate lower-confidence-bound gate over [`RECALL_SCALE`].
        candidate_recall_threshold_units: u64,
        /// Rebuild upper-confidence-bound gate over [`RECALL_SCALE`].
        rebuild_recall_threshold_units: u64,
        /// Declared query-sampling design.
        sample_design: QuerySampleDesign,
        /// Whether every exact-baseline result list was complete.
        exact_baseline_complete: bool,
        /// Whether population, snapshot, and authority remained fixed.
        authorization_domain_fixed: bool,
        /// Whether the evaluated candidate policy remained fixed.
        candidate_policy_fixed: bool,
        /// Accepted keyed-query observations.
        query_observations: u64,
        /// Exact-baseline result identities compared.
        exact_baseline_results: u64,
        /// Candidate result identities compared.
        candidate_results: u64,
        /// Candidate identities found in their corresponding baseline.
        intersection_hits: u64,
        /// Whether the fixed trial window completed.
        complete: bool,
        /// Exact-recall numerator.
        exact_recall_intersection_hits: u64,
        /// Exact-recall denominator.
        exact_recall_baseline_results: u64,
        /// Fixed confidence-interval denominator.
        interval_scale: u64,
        /// Floor-rounded empirical-recall numerator.
        interval_point_estimate_units: u64,
        /// Conservative interval lower endpoint.
        interval_lower_units: u64,
        /// Conservative interval upper endpoint.
        interval_upper_units: u64,
        /// Outward-rounded interval radius.
        interval_radius_units: u64,
        /// Interval failure-probability exponent.
        interval_confidence_exponent: u8,
        /// Per-query observations represented by the interval.
        interval_query_observations: u64,
        /// Whether all disclosed interval assumptions are supported.
        assumptions_supported: bool,
        /// Terminal candidate, fallback, or rebuild action.
        action: AnnRecallAction,
        /// Deterministic explanation for the terminal action.
        action_reason: AnnRecallActionReason,
    },
}

impl StatisticalStatistic {
    const fn canonical_tag(self) -> u8 {
        match self {
            Self::EProcess { .. } => 1,
            Self::ConformalCoverage { .. } => 2,
            Self::ExplorationBudget { .. } => 3,
            Self::DrainProgress { .. } => 4,
            Self::RegimeChange { .. } => 5,
            Self::OffPolicyEvaluation { .. } => 6,
            Self::AnnRecall { .. } => 7,
        }
    }

    const fn payload_len(self) -> usize {
        match self {
            Self::EProcess { .. } => 96,
            Self::ConformalCoverage { .. } => 161,
            Self::ExplorationBudget { .. } => 56,
            Self::DrainProgress { .. } => 25,
            Self::RegimeChange { .. } => 32,
            Self::OffPolicyEvaluation { .. } => 390,
            Self::AnnRecall { .. } => 402,
        }
    }

    fn validate(self) -> Result<(), StatisticalLogRecordError> {
        match self {
            Self::EProcess {
                p0_bits,
                lambda_bits,
                alpha_bits,
                max_evalue_bits,
                e_value_bits,
                rejection_threshold_bits,
                observations,
                one_observations,
                ..
            } => {
                let p0 = validate_canonical_float(StatisticField::EProcessP0, p0_bits)?;
                let lambda = validate_canonical_float(StatisticField::EProcessLambda, lambda_bits)?;
                let alpha = validate_canonical_float(StatisticField::EProcessAlpha, alpha_bits)?;
                let max_evalue =
                    validate_canonical_float(StatisticField::EProcessMaximum, max_evalue_bits)?;
                let config = EProcessConfig {
                    p0,
                    lambda,
                    alpha,
                    max_evalue,
                };
                if config.validate().is_err() {
                    return Err(StatisticalLogRecordError::InvalidEProcessConfiguration);
                }
                validate_nonnegative_or_positive_infinity(StatisticField::EValue, e_value_bits)?;
                validate_positive_finite(
                    StatisticField::RejectionThreshold,
                    rejection_threshold_bits,
                )?;
                validate_subcount(
                    StatisticField::OneObservations,
                    one_observations,
                    observations,
                )
            }
            Self::ConformalCoverage {
                alpha_bits,
                minimum_calibration_samples,
                maximum_calibration_samples,
                threshold_bits,
                nonconformity_score_bits,
                coverage_target_bits,
                assessments,
                covered,
                ..
            } => {
                let alpha = validate_canonical_float(StatisticField::ConformalAlpha, alpha_bits)?;
                if !alpha.is_finite() || !(0.0..1.0).contains(&alpha) {
                    return Err(StatisticalLogRecordError::InvalidConformalAlpha {
                        bits: alpha_bits,
                    });
                }
                if minimum_calibration_samples == 0
                    || maximum_calibration_samples < minimum_calibration_samples
                {
                    return Err(
                        StatisticalLogRecordError::InvalidConformalCalibrationBounds {
                            minimum: minimum_calibration_samples,
                            maximum: maximum_calibration_samples,
                        },
                    );
                }
                validate_finite_or_positive_infinity(
                    StatisticField::ConformalThreshold,
                    threshold_bits,
                )?;
                validate_finite_or_positive_infinity(
                    StatisticField::NonconformityScore,
                    nonconformity_score_bits,
                )?;
                validate_unit_interval(StatisticField::CoverageTarget, coverage_target_bits)?;
                validate_subcount(StatisticField::CoveredAssessments, covered, assessments)
            }
            Self::ExplorationBudget {
                alpha_bits,
                residual_rate_bits,
                upper_bound_bits,
                target_rate_bits,
                total_runs,
                discoveries,
                ..
            } => {
                let alpha = validate_canonical_float(StatisticField::ExplorationAlpha, alpha_bits)?;
                if !alpha.is_finite() || !(0.0..1.0).contains(&alpha) {
                    return Err(StatisticalLogRecordError::InvalidExplorationAlpha {
                        bits: alpha_bits,
                    });
                }
                validate_unit_interval(StatisticField::ResidualRate, residual_rate_bits)?;
                validate_unit_interval(StatisticField::ExplorationUpperBound, upper_bound_bits)?;
                validate_unit_interval(StatisticField::TargetResidualRate, target_rate_bits)?;
                validate_subcount(StatisticField::Discoveries, discoveries, total_runs)
            }
            Self::DrainProgress {
                current_potential_bits,
                confidence_bound_bits,
                ..
            } => {
                validate_nonnegative_finite(
                    StatisticField::CurrentPotential,
                    current_potential_bits,
                )?;
                validate_unit_interval(StatisticField::ConfidenceBound, confidence_bound_bits)
            }
            Self::RegimeChange {
                threshold,
                observations,
                detections,
                ..
            } => {
                if threshold <= 0 {
                    return Err(StatisticalLogRecordError::NonPositiveRegimeThreshold {
                        threshold,
                    });
                }
                validate_subcount(StatisticField::Detections, detections, observations)
            }
            Self::OffPolicyEvaluation {
                clipping_weight_units,
                minimum_effective_sample_size,
                maximum_observations,
                maximum_actions_per_observation,
                maximum_total_action_rows,
                candidate_numerator,
                fallback_numerator,
                advantage_numerator,
                common_denominator,
                candidate_ess_numerator,
                candidate_ess_denominator,
                fallback_ess_numerator,
                fallback_ess_denominator,
                observations,
                action_rows,
                candidate_ess_gate_passed,
                fallback_ess_gate_passed,
                zero_support_exclusions,
                zero_support_exclusions_digest,
                ..
            } => {
                if common_denominator == 0 {
                    return Err(StatisticalLogRecordError::ZeroEstimateDenominator);
                }
                let expected_advantage = candidate_numerator
                    .checked_sub(fallback_numerator)
                    .ok_or(StatisticalLogRecordError::OpeArithmeticOverflow)?;
                if advantage_numerator != expected_advantage {
                    return Err(StatisticalLogRecordError::OpeAdvantageMismatch {
                        candidate: candidate_numerator,
                        fallback: fallback_numerator,
                        actual: advantage_numerator,
                    });
                }
                let expected_denominator = u128::from(observations)
                    .checked_mul(u128::from(PROBABILITY_SCALE))
                    .and_then(|value| {
                        u128::try_from(OUTCOME_SCALE)
                            .ok()
                            .and_then(|scale| value.checked_mul(scale))
                    })
                    .ok_or(StatisticalLogRecordError::OpeArithmeticOverflow)?;
                if common_denominator != expected_denominator {
                    return Err(StatisticalLogRecordError::OpeEstimateDenominatorMismatch {
                        expected: expected_denominator,
                        actual: common_denominator,
                    });
                }
                let maximum_observations_usize = usize::try_from(maximum_observations)
                    .map_err(|_| StatisticalLogRecordError::InvalidOpeProfile)?;
                let maximum_actions_per_observation_usize =
                    usize::try_from(maximum_actions_per_observation)
                        .map_err(|_| StatisticalLogRecordError::InvalidOpeProfile)?;
                let maximum_total_action_rows_usize = usize::try_from(maximum_total_action_rows)
                    .map_err(|_| StatisticalLogRecordError::InvalidOpeProfile)?;
                if OpeProfile::try_new(
                    clipping_weight_units,
                    minimum_effective_sample_size,
                    maximum_observations_usize,
                    maximum_actions_per_observation_usize,
                    maximum_total_action_rows_usize,
                )
                .is_err()
                {
                    return Err(StatisticalLogRecordError::InvalidOpeProfile);
                }
                validate_ope_ess(
                    EvaluatedPolicy::Candidate,
                    candidate_ess_numerator,
                    candidate_ess_denominator,
                    observations,
                    minimum_effective_sample_size,
                    candidate_ess_gate_passed,
                )?;
                validate_ope_ess(
                    EvaluatedPolicy::Fallback,
                    fallback_ess_numerator,
                    fallback_ess_denominator,
                    observations,
                    minimum_effective_sample_size,
                    fallback_ess_gate_passed,
                )?;
                if observations == 0 || observations > maximum_observations {
                    return Err(StatisticalLogRecordError::OpeObservationCountOutOfBounds {
                        observations,
                        maximum: maximum_observations,
                    });
                }
                if action_rows < observations || action_rows > maximum_total_action_rows {
                    return Err(StatisticalLogRecordError::OpeActionRowCountOutOfBounds {
                        action_rows,
                        observations,
                        maximum: maximum_total_action_rows,
                    });
                }
                let maximum_exclusions = action_rows
                    .checked_mul(2)
                    .ok_or(StatisticalLogRecordError::OpeArithmeticOverflow)?;
                validate_subcount(
                    StatisticField::ZeroSupportExclusions,
                    zero_support_exclusions,
                    maximum_exclusions,
                )?;
                if zero_support_exclusions == 0
                    && !evidence_digests_match(
                        zero_support_exclusions_digest,
                        empty_ope_support_exclusions_digest(),
                    )
                {
                    return Err(StatisticalLogRecordError::EmptyOpeSupportExclusionsDigestMismatch);
                }
                Ok(())
            }
            ann_recall @ Self::AnnRecall { .. } => validate_ann_recall_statistic(ann_recall),
        }
    }
}

/// Exact inclusive sequence range covered by one observation batch.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StatisticalBatchRange {
    first: u64,
    last: u64,
}

impl StatisticalBatchRange {
    /// Constructs a non-empty inclusive range.
    pub const fn try_new(first: u64, last: u64) -> Result<Self, StatisticalLogRecordError> {
        if first > last {
            return Err(StatisticalLogRecordError::ReversedBatchRange { first, last });
        }
        Ok(Self { first, last })
    }

    /// Inclusive first sequence.
    #[must_use]
    pub const fn first(self) -> u64 {
        self.first
    }

    /// Inclusive last sequence.
    #[must_use]
    pub const fn last(self) -> u64 {
        self.last
    }
}

/// Closed policy-selection class retained by a validated record.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StatisticalPolicySelection {
    /// Candidate decision.
    Candidate = 1,
    /// Pinned deterministic fallback.
    PinnedFallback = 2,
    /// Pinned ANN-index rebuild policy.
    Rebuild = 3,
}

/// Immutable statistical monitor record.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct StatisticalLogRecord {
    monitor_kind: StatisticalMonitorKind,
    source_monitor_oid: ObjectId,
    monitor_oid: ObjectId,
    evidence_oid: ObjectId,
    evidence_digest: StatisticalEvidenceDigest,
    filtration_or_window_oid: ObjectId,
    identity_window: StatisticalBatchRange,
    batch: StatisticalBatchRange,
    regime_epoch: u64,
    candidate_decision_oid: ObjectId,
    pinned_fallback_oid: ObjectId,
    selected_policy_oid: ObjectId,
    statistic: StatisticalStatistic,
}

impl StatisticalLogRecord {
    /// Constructs a record from identity-bound e-process evidence.
    ///
    /// The batch is the singleton source sequence newly consumed by this
    /// evidence snapshot; statistic counters remain cumulative.
    pub fn try_from_eprocess(
        identity_issuer: &impl StatisticalEvidenceIdentityIssuer,
        evidence: &EProcessEvidence,
    ) -> Result<Self, StatisticalLogRecordError> {
        let identity = *evidence.identity();
        let identity_window =
            StatisticalBatchRange::try_new(identity.window().first(), identity.window().last())?;
        let batch = singleton_batch_from_cumulative_evidence(
            StatisticalMonitorKind::EProcess,
            identity.window().first(),
            identity.window().last(),
            evidence.through_sequence(),
            evidence.observations(),
        )?;
        Self::try_from_bound_parts(
            identity_issuer,
            StatisticalMonitorKind::EProcess,
            evidence.monitor_oid(),
            evidence.filtration_oid(),
            identity_window,
            batch,
            identity.regime_epoch(),
            evidence.candidate_decision_oid(),
            evidence.pinned_fallback_oid(),
            evidence.selected_policy_oid(),
            StatisticalStatistic::EProcess {
                profile_oid: evidence.profile_oid(),
                p0_bits: evidence.profile().p0_bits(),
                lambda_bits: evidence.profile().lambda_bits(),
                alpha_bits: evidence.profile().alpha_bits(),
                max_evalue_bits: evidence.profile().max_evalue_bits(),
                e_value_bits: evidence.e_value_bits(),
                rejection_threshold_bits: evidence.rejection_threshold_bits(),
                observations: evidence.observations(),
                one_observations: evidence.one_observations(),
            },
        )
    }

    /// Constructs a record from a ready, identity-bound conformal assessment.
    ///
    /// An assessment without a complete foundation statistic remains useful as
    /// an in-memory fallback receipt, but it cannot become a canonical
    /// conformal-statistic log record.
    pub fn try_from_conformal(
        identity_issuer: &impl StatisticalEvidenceIdentityIssuer,
        evidence: &ConformalEvidence,
    ) -> Result<Self, StatisticalLogRecordError> {
        let identity = *evidence.identity();
        let profile = *evidence.profile();
        let identity_window =
            StatisticalBatchRange::try_new(identity.window().first(), identity.window().last())?;
        let batch =
            StatisticalBatchRange::try_new(evidence.stream_sequence(), evidence.stream_sequence())?;
        let threshold_bits = evidence.threshold_bits().ok_or(
            StatisticalLogRecordError::EvidenceStatisticUnavailable {
                monitor: StatisticalMonitorKind::ConformalThreshold,
                field: StatisticField::ConformalThreshold,
            },
        )?;
        let nonconformity_score_bits = evidence.nonconformity_score_bits().ok_or(
            StatisticalLogRecordError::EvidenceStatisticUnavailable {
                monitor: StatisticalMonitorKind::ConformalThreshold,
                field: StatisticField::NonconformityScore,
            },
        )?;
        let coverage_target_bits = evidence.coverage_target_bits().ok_or(
            StatisticalLogRecordError::EvidenceStatisticUnavailable {
                monitor: StatisticalMonitorKind::ConformalThreshold,
                field: StatisticField::CoverageTarget,
            },
        )?;
        let minimum_calibration_samples = u64::try_from(profile.minimum_calibration_samples())
            .map_err(
                |_| StatisticalLogRecordError::EvidenceCounterUnrepresentable {
                    monitor: StatisticalMonitorKind::ConformalThreshold,
                    field: StatisticField::MinimumCalibrationSamples,
                },
            )?;
        let maximum_calibration_samples = u64::try_from(profile.maximum_calibration_samples())
            .map_err(
                |_| StatisticalLogRecordError::EvidenceCounterUnrepresentable {
                    monitor: StatisticalMonitorKind::ConformalThreshold,
                    field: StatisticField::MaximumCalibrationSamples,
                },
            )?;
        let selected_policy_oid = match evidence.selection() {
            ConformalPolicySelection::CandidateDecision => identity.candidate_decision_oid(),
            ConformalPolicySelection::PinnedFallback => identity.pinned_fallback_oid(),
        };

        Self::try_from_bound_parts(
            identity_issuer,
            StatisticalMonitorKind::ConformalThreshold,
            identity.metric_oid(),
            identity.window_oid(),
            identity_window,
            batch,
            identity.regime_epoch(),
            identity.candidate_decision_oid(),
            identity.pinned_fallback_oid(),
            selected_policy_oid,
            StatisticalStatistic::ConformalCoverage {
                profile_oid: profile.profile_oid(),
                population_oid: identity.population_oid(),
                selection_oid: identity.selection_oid(),
                alpha_bits: profile.alpha_bits(),
                mode: StatisticalConformalMode::from_runtime(profile.mode()),
                minimum_calibration_samples,
                maximum_calibration_samples,
                threshold_bits,
                nonconformity_score_bits,
                coverage_target_bits,
                assessments: evidence.ready_assessments(),
                covered: evidence.covered_ready_assessments(),
            },
        )
    }

    /// Constructs a record from identity-bound exploration-budget evidence.
    ///
    /// The batch is the singleton source sequence newly consumed by this
    /// evidence snapshot; statistic counters remain cumulative.
    pub fn try_from_exploration(
        identity_issuer: &impl StatisticalEvidenceIdentityIssuer,
        evidence: &ExplorationBudgetEvidence,
    ) -> Result<Self, StatisticalLogRecordError> {
        let identity = evidence.identity();
        let identity_window =
            StatisticalBatchRange::try_new(identity.first_sequence(), identity.last_sequence())?;
        let batch = singleton_batch_from_cumulative_evidence(
            StatisticalMonitorKind::ExplorationBudget,
            identity.first_sequence(),
            identity.last_sequence(),
            evidence.through_sequence(),
            evidence.total_runs(),
        )?;
        Self::try_from_bound_parts(
            identity_issuer,
            StatisticalMonitorKind::ExplorationBudget,
            identity.budget_oid(),
            identity.window_oid(),
            identity_window,
            batch,
            identity.regime_epoch(),
            identity.candidate_decision_oid(),
            identity.pinned_fallback_oid(),
            evidence.selected_policy_oid(),
            StatisticalStatistic::ExplorationBudget {
                alpha_bits: evidence.profile().alpha_bits(),
                residual_rate_bits: evidence.residual_discovery_rate_bits(),
                upper_bound_bits: evidence.conformal_upper_bound_bits(),
                target_rate_bits: evidence.target_residual_rate_bits(),
                total_runs: evidence.total_runs(),
                discoveries: evidence.discoveries(),
                recommended_additional_runs: evidence.recommended_additional_runs(),
            },
        )
    }

    /// Constructs a record from identity-bound drain-progress evidence.
    ///
    /// The batch is the singleton source sequence newly consumed by this
    /// evidence snapshot; statistic counters remain cumulative.
    pub fn try_from_progress(
        identity_issuer: &impl StatisticalEvidenceIdentityIssuer,
        evidence: &DrainProgressEvidence,
    ) -> Result<Self, StatisticalLogRecordError> {
        let identity = evidence.identity();
        let identity_window =
            StatisticalBatchRange::try_new(identity.first_sequence(), identity.last_sequence())?;
        let batch = singleton_batch_from_cumulative_evidence(
            StatisticalMonitorKind::DrainProgress,
            identity.first_sequence(),
            identity.last_sequence(),
            evidence.through_sequence(),
            evidence.total_observations(),
        )?;
        Self::try_from_bound_parts(
            identity_issuer,
            StatisticalMonitorKind::DrainProgress,
            identity.monitor_oid(),
            identity.filtration_oid(),
            identity_window,
            batch,
            identity.regime_epoch(),
            identity.decision_oid(),
            identity.fallback_oid(),
            evidence.selected_policy_oid(),
            StatisticalStatistic::DrainProgress {
                current_potential_bits: evidence.current_potential_bits(),
                confidence_bound_bits: evidence.confidence_bound_bits(),
                observations: evidence.total_observations(),
                stall_detected: evidence.stall_detected(),
            },
        )
    }

    /// Constructs a record from identity-bound combined regime evidence.
    ///
    /// The batch is the singleton source sequence newly consumed by this
    /// evidence snapshot; statistic counters remain cumulative.
    pub fn try_from_regime(
        identity_issuer: &impl StatisticalEvidenceIdentityIssuer,
        evidence: &RegimeSignalEvidence,
    ) -> Result<Self, StatisticalLogRecordError> {
        let identity = evidence.identity();
        let identity_window =
            StatisticalBatchRange::try_new(identity.window().first(), identity.window().last())?;
        let batch = singleton_batch_from_cumulative_evidence(
            StatisticalMonitorKind::RegimeChange,
            identity.window().first(),
            identity.window().last(),
            evidence.through_sequence(),
            evidence.observation_count(),
        )?;
        let (statistic, threshold) = if let Some(receipt) = evidence.retained_receipts().last() {
            (receipt.statistic(), receipt.threshold())
        } else {
            let snapshot = evidence
                .detector_snapshots()
                .first()
                .ok_or(StatisticalLogRecordError::MissingRegimeDetectorSnapshot)?;
            (snapshot.statistic(), snapshot.threshold())
        };
        Self::try_from_bound_parts(
            identity_issuer,
            StatisticalMonitorKind::RegimeChange,
            identity.signal_oid(),
            identity.metric_stream_oid(),
            identity_window,
            batch,
            identity.regime_epoch(),
            identity.candidate_decision_oid(),
            identity.pinned_fallback_oid(),
            evidence.selected_policy_oid(),
            StatisticalStatistic::RegimeChange {
                statistic,
                threshold,
                observations: evidence.observation_count(),
                detections: evidence.detection_count(),
            },
        )
    }

    /// Constructs a record from identity-bound off-policy evidence.
    ///
    /// The batch is the singleton final source sequence represented by this
    /// evidence prefix; statistic counters remain cumulative.
    pub fn try_from_ope(
        identity_issuer: &impl StatisticalEvidenceIdentityIssuer,
        evidence: &OpeEvidence,
    ) -> Result<Self, StatisticalLogRecordError> {
        let identity = evidence.identity();
        let identity_window = StatisticalBatchRange::try_new(
            identity.window().first_sequence(),
            identity.window().last_sequence(),
        )?;
        let batch = singleton_batch_from_observation_count(
            StatisticalMonitorKind::OffPolicyEvaluation,
            identity.window().first_sequence(),
            identity.window().last_sequence(),
            evidence.observations(),
        )?;
        let candidate = evidence.candidate_estimate();
        let fallback = evidence.fallback_estimate();
        let advantage = evidence.advantage_estimate();
        if candidate.denominator() != fallback.denominator()
            || candidate.denominator() != advantage.denominator()
        {
            return Err(
                StatisticalLogRecordError::EvidenceEstimateDenominatorMismatch {
                    candidate: candidate.denominator(),
                    fallback: fallback.denominator(),
                    advantage: advantage.denominator(),
                },
            );
        }
        let zero_support_exclusions = u64::try_from(evidence.zero_support_exclusions().len())
            .map_err(
                |_| StatisticalLogRecordError::EvidenceCounterUnrepresentable {
                    monitor: StatisticalMonitorKind::OffPolicyEvaluation,
                    field: StatisticField::ZeroSupportExclusions,
                },
            )?;
        let candidate_ess = evidence.candidate_effective_sample_size();
        let fallback_ess = evidence.fallback_effective_sample_size();
        let profile = evidence.profile();
        let maximum_observations = u64::try_from(profile.maximum_observations()).map_err(|_| {
            StatisticalLogRecordError::EvidenceCounterUnrepresentable {
                monitor: StatisticalMonitorKind::OffPolicyEvaluation,
                field: StatisticField::MaximumOpeObservations,
            }
        })?;
        let maximum_actions_per_observation =
            u64::try_from(profile.maximum_actions_per_observation()).map_err(|_| {
                StatisticalLogRecordError::EvidenceCounterUnrepresentable {
                    monitor: StatisticalMonitorKind::OffPolicyEvaluation,
                    field: StatisticField::MaximumOpeActionsPerObservation,
                }
            })?;
        let maximum_total_action_rows = u64::try_from(profile.maximum_total_action_rows())
            .map_err(
                |_| StatisticalLogRecordError::EvidenceCounterUnrepresentable {
                    monitor: StatisticalMonitorKind::OffPolicyEvaluation,
                    field: StatisticField::MaximumOpeActionRows,
                },
            )?;
        let zero_support_exclusions_digest =
            digest_ope_support_exclusions(evidence.zero_support_exclusions())?;
        Self::try_from_bound_parts(
            identity_issuer,
            StatisticalMonitorKind::OffPolicyEvaluation,
            identity.estimator_oid(),
            identity.selection_oid(),
            identity_window,
            batch,
            identity.regime_epoch(),
            identity.candidate_policy_oid(),
            identity.fallback_policy_oid(),
            evidence.selected_policy_oid(),
            StatisticalStatistic::OffPolicyEvaluation {
                population_oid: identity.population_oid(),
                strata_oid: identity.strata_oid(),
                action_space_oid: identity.action_space_oid(),
                policy_epoch_oid: identity.policy_epoch_oid(),
                estimator_oid: identity.estimator_oid(),
                estimator: identity.estimator(),
                failure_behavior: identity.failure_behavior(),
                clipping_weight_units: profile.clipping_weight_units(),
                minimum_effective_sample_size: profile.minimum_effective_sample_size(),
                maximum_observations,
                maximum_actions_per_observation,
                maximum_total_action_rows,
                candidate_numerator: candidate.numerator(),
                fallback_numerator: fallback.numerator(),
                advantage_numerator: advantage.numerator(),
                common_denominator: candidate.denominator(),
                candidate_ess_numerator: candidate_ess.numerator(),
                candidate_ess_denominator: candidate_ess.denominator(),
                fallback_ess_numerator: fallback_ess.numerator(),
                fallback_ess_denominator: fallback_ess.denominator(),
                observations: evidence.observations(),
                action_rows: evidence.action_rows(),
                complete: evidence.complete(),
                candidate_ess_gate_passed: evidence.candidate_ess_gate_passed(),
                fallback_ess_gate_passed: evidence.fallback_ess_gate_passed(),
                zero_support_exclusions,
                zero_support_exclusions_digest,
                selection_reason: evidence.selection_reason(),
            },
        )
    }

    /// Constructs one terminal record from a completed `FG-CAL-03`
    /// ANN-recall trial.
    ///
    /// Prefix evidence intentionally cannot enter the durable decision log:
    /// only a complete fixed window with a terminal candidate, fallback, or
    /// rebuild action is a governed decision.
    pub fn try_from_ann_recall(
        identity_issuer: &impl StatisticalEvidenceIdentityIssuer,
        evidence: &AnnRecallEvidence,
    ) -> Result<Self, StatisticalLogRecordError> {
        if !evidence.complete() {
            return Err(StatisticalLogRecordError::AnnRecallEvidenceIncomplete);
        }
        let action = evidence
            .action()
            .ok_or(StatisticalLogRecordError::AnnRecallActionUnavailable)?;
        let selected_policy_oid = evidence
            .selected_policy_oid()
            .ok_or(StatisticalLogRecordError::AnnRecallActionUnavailable)?;
        let identity = evidence.identity();
        let profile = evidence.profile();
        if profile.profile_oid() != identity.profile_oid() {
            return Err(
                StatisticalLogRecordError::AnnRecallProfileIdentityMismatch {
                    expected: identity.profile_oid(),
                    actual: profile.profile_oid(),
                },
            );
        }
        let identity_window = StatisticalBatchRange::try_new(
            identity.window().first_sequence(),
            identity.window().last_sequence(),
        )?;
        let top_k = u64::try_from(profile.top_k()).map_err(|_| {
            StatisticalLogRecordError::EvidenceCounterUnrepresentable {
                monitor: StatisticalMonitorKind::AnnRecall,
                field: StatisticField::AnnTopK,
            }
        })?;
        let maximum_queries = u64::try_from(profile.maximum_queries()).map_err(|_| {
            StatisticalLogRecordError::EvidenceCounterUnrepresentable {
                monitor: StatisticalMonitorKind::AnnRecall,
                field: StatisticField::MaximumAnnQueries,
            }
        })?;
        let maximum_total_result_ids =
            u64::try_from(profile.maximum_total_result_ids()).map_err(|_| {
                StatisticalLogRecordError::EvidenceCounterUnrepresentable {
                    monitor: StatisticalMonitorKind::AnnRecall,
                    field: StatisticField::MaximumAnnResultIds,
                }
            })?;
        let assumptions = evidence.assumptions();
        let exact_recall = evidence.exact_recall();
        let interval = evidence.confidence_interval();

        Self::try_from_bound_parts(
            identity_issuer,
            StatisticalMonitorKind::AnnRecall,
            identity.monitor_oid(),
            identity.sample_key_oid(),
            identity_window,
            identity_window,
            identity.regime_epoch(),
            identity.candidate_policy_oid(),
            identity.fallback_policy_oid(),
            selected_policy_oid,
            StatisticalStatistic::AnnRecall {
                profile_oid: profile.profile_oid(),
                authorized_population_oid: identity.authorized_population_oid(),
                snapshot_oid: identity.snapshot_oid(),
                authority_domain_oid: identity.authority_domain_oid(),
                sample_key_oid: identity.sample_key_oid(),
                sample_design_oid: identity.sample_design_oid(),
                exact_baseline_oid: identity.exact_baseline_oid(),
                rebuild_policy_oid: identity.rebuild_policy_oid(),
                top_k,
                maximum_queries,
                maximum_total_result_ids,
                confidence_exponent: profile.confidence_exponent(),
                candidate_recall_threshold_units: profile.candidate_recall_threshold_units(),
                rebuild_recall_threshold_units: profile.rebuild_recall_threshold_units(),
                sample_design: assumptions.sample_design(),
                exact_baseline_complete: assumptions.exact_baseline_complete(),
                authorization_domain_fixed: assumptions.authorization_domain_fixed(),
                candidate_policy_fixed: assumptions.candidate_policy_fixed(),
                query_observations: evidence.query_observations(),
                exact_baseline_results: evidence.exact_baseline_results(),
                candidate_results: evidence.candidate_results(),
                intersection_hits: evidence.intersection_hits(),
                complete: evidence.complete(),
                exact_recall_intersection_hits: exact_recall.intersection_hits(),
                exact_recall_baseline_results: exact_recall.baseline_results(),
                interval_scale: interval.scale(),
                interval_point_estimate_units: interval.point_estimate_units(),
                interval_lower_units: interval.lower_units(),
                interval_upper_units: interval.upper_units(),
                interval_radius_units: interval.radius_units(),
                interval_confidence_exponent: interval.failure_probability_power_of_two_exponent(),
                interval_query_observations: interval.query_observations(),
                assumptions_supported: evidence.assumptions_supported(),
                action,
                action_reason: evidence.action_reason(),
            },
        )
    }

    /// Constructs a validated record from already-decoded canonical fields.
    ///
    /// Kept private so live records originate only from typed monitor evidence.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn try_from_parts(
        identity_issuer: &impl StatisticalEvidenceIdentityIssuer,
        monitor_kind: StatisticalMonitorKind,
        monitor_oid: ObjectId,
        filtration_or_window_oid: ObjectId,
        batch: StatisticalBatchRange,
        regime_epoch: u64,
        candidate_decision_oid: ObjectId,
        pinned_fallback_oid: ObjectId,
        selected_policy_oid: ObjectId,
        statistic: StatisticalStatistic,
    ) -> Result<Self, StatisticalLogRecordError> {
        Self::try_from_bound_parts(
            identity_issuer,
            monitor_kind,
            monitor_oid,
            filtration_or_window_oid,
            batch,
            batch,
            regime_epoch,
            candidate_decision_oid,
            pinned_fallback_oid,
            selected_policy_oid,
            statistic,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn try_from_bound_parts(
        identity_issuer: &impl StatisticalEvidenceIdentityIssuer,
        monitor_kind: StatisticalMonitorKind,
        source_monitor_oid: ObjectId,
        filtration_or_window_oid: ObjectId,
        identity_window: StatisticalBatchRange,
        batch: StatisticalBatchRange,
        regime_epoch: u64,
        candidate_decision_oid: ObjectId,
        pinned_fallback_oid: ObjectId,
        selected_policy_oid: ObjectId,
        statistic: StatisticalStatistic,
    ) -> Result<Self, StatisticalLogRecordError> {
        validate_record_parts(
            monitor_kind,
            identity_window,
            batch,
            candidate_decision_oid,
            pinned_fallback_oid,
            selected_policy_oid,
            statistic,
        )?;
        let canonical_monitor_identity = encode_monitor_identity_body(
            monitor_kind,
            source_monitor_oid,
            filtration_or_window_oid,
            identity_window,
            regime_epoch,
            candidate_decision_oid,
            pinned_fallback_oid,
            statistic,
        )?;
        let monitor_oid = identity_issuer
            .issue_statistical_monitor_oid(&canonical_monitor_identity)
            .map_err(
                |source| StatisticalLogRecordError::MonitorIdentityIssuanceFailed { source },
            )?;
        let canonical_evidence_body = encode_evidence_body(
            monitor_kind,
            source_monitor_oid,
            monitor_oid,
            filtration_or_window_oid,
            identity_window,
            batch,
            regime_epoch,
            candidate_decision_oid,
            pinned_fallback_oid,
            selected_policy_oid,
            statistic,
        )?;
        let evidence_digest = digest_evidence_body(&canonical_evidence_body);
        let evidence_oid = identity_issuer
            .issue_statistical_evidence_oid(&canonical_evidence_body)
            .map_err(
                |source| StatisticalLogRecordError::EvidenceIdentityIssuanceFailed { source },
            )?;

        Ok(Self {
            monitor_kind,
            source_monitor_oid,
            monitor_oid,
            evidence_oid,
            evidence_digest,
            filtration_or_window_oid,
            identity_window,
            batch,
            regime_epoch,
            candidate_decision_oid,
            pinned_fallback_oid,
            selected_policy_oid,
            statistic,
        })
    }

    /// This type can represent only the statistical registry claim class.
    #[must_use]
    pub const fn claim_class(self) -> RegistryClaimClass {
        RegistryClaimClass::Statistical
    }

    /// Closed monitor family.
    #[must_use]
    pub const fn monitor_kind(self) -> StatisticalMonitorKind {
        self.monitor_kind
    }

    /// Immutable monitor identity.
    #[must_use]
    pub const fn monitor_oid(self) -> ObjectId {
        self.monitor_oid
    }

    /// Monitor-family identity supplied by the typed source evidence.
    ///
    /// [`Self::monitor_oid`] is the authority-issued identity of the complete
    /// stable trial; this source identity remains available for provenance.
    #[must_use]
    pub const fn source_monitor_oid(self) -> ObjectId {
        self.source_monitor_oid
    }

    /// Immutable evidence identity.
    #[must_use]
    pub const fn evidence_oid(self) -> ObjectId {
        self.evidence_oid
    }

    /// Unkeyed digest of the exact canonical evidence body authenticated by
    /// [`Self::evidence_oid`].
    #[must_use]
    pub const fn evidence_digest(self) -> StatisticalEvidenceDigest {
        self.evidence_digest
    }

    /// Filtration or observation-window identity.
    #[must_use]
    pub const fn filtration_or_window_oid(self) -> ObjectId {
        self.filtration_or_window_oid
    }

    /// Complete immutable source window declared by the monitor identity.
    #[must_use]
    pub const fn identity_window(self) -> StatisticalBatchRange {
        self.identity_window
    }

    /// Exact inclusive batch range.
    #[must_use]
    pub const fn batch(self) -> StatisticalBatchRange {
        self.batch
    }

    /// Regime epoch under which the statistic was computed.
    #[must_use]
    pub const fn regime_epoch(self) -> u64 {
        self.regime_epoch
    }

    /// Candidate decision identity.
    #[must_use]
    pub const fn candidate_decision_oid(self) -> ObjectId {
        self.candidate_decision_oid
    }

    /// Pinned deterministic fallback identity.
    #[must_use]
    pub const fn pinned_fallback_oid(self) -> ObjectId {
        self.pinned_fallback_oid
    }

    /// Selected policy identity.
    #[must_use]
    pub const fn selected_policy_oid(self) -> ObjectId {
        self.selected_policy_oid
    }

    /// Candidate, fallback, or ANN-rebuild selection class.
    #[must_use]
    pub fn selection(self) -> StatisticalPolicySelection {
        if self.selected_policy_oid == self.candidate_decision_oid {
            StatisticalPolicySelection::Candidate
        } else if self.selected_policy_oid == self.pinned_fallback_oid {
            StatisticalPolicySelection::PinnedFallback
        } else {
            StatisticalPolicySelection::Rebuild
        }
    }

    /// Exact typed statistic.
    #[must_use]
    pub const fn statistic(self) -> StatisticalStatistic {
        self.statistic
    }

    /// Encodes this record under the strict version-4 canonical format.
    pub fn encode_canonical(self) -> Result<Vec<u8>, StatisticalLogCodecError> {
        let capacity = RECORD_FIXED_BYTES
            .checked_add(self.statistic.payload_len())
            .ok_or(StatisticalLogCodecError::LengthOverflow)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(capacity).map_err(|_| {
            StatisticalLogCodecError::AllocationFailed {
                requested: capacity,
            }
        })?;
        bytes.extend_from_slice(&RECORD_MAGIC);
        push_u16(&mut bytes, STATISTICAL_LOG_RECORD_VERSION);
        bytes.push(RECORD_TAG);
        bytes.push(STATISTICAL_CLAIM_TAG);
        bytes.push(self.monitor_kind.canonical_tag());
        bytes.push(self.statistic.canonical_tag());
        push_u16(&mut bytes, RECORD_RESERVED);
        push_oid(&mut bytes, self.source_monitor_oid);
        push_oid(&mut bytes, self.monitor_oid);
        push_oid(&mut bytes, self.evidence_oid);
        bytes.extend_from_slice(self.evidence_digest.as_bytes());
        push_oid(&mut bytes, self.filtration_or_window_oid);
        push_u64(&mut bytes, self.identity_window.first);
        push_u64(&mut bytes, self.identity_window.last);
        push_u64(&mut bytes, self.batch.first);
        push_u64(&mut bytes, self.batch.last);
        push_u64(&mut bytes, self.regime_epoch);
        push_oid(&mut bytes, self.candidate_decision_oid);
        push_oid(&mut bytes, self.pinned_fallback_oid);
        push_oid(&mut bytes, self.selected_policy_oid);
        let payload_len = u16::try_from(self.statistic.payload_len())
            .map_err(|_| StatisticalLogCodecError::LengthOverflow)?;
        push_u16(&mut bytes, payload_len);
        encode_statistic(&mut bytes, self.statistic);
        Ok(bytes)
    }

    /// Decodes exactly one strict version-4 canonical record.
    pub fn decode_canonical(
        bytes: &[u8],
        identity_verifier: &impl StatisticalEvidenceIdentityVerifier,
    ) -> Result<Self, StatisticalLogCodecError> {
        let mut decoder = Decoder::new(bytes);
        let magic = decoder.read_array::<8>()?;
        if magic != RECORD_MAGIC {
            return Err(StatisticalLogCodecError::MagicMismatch {
                expected: CanonicalDomain::Record,
            });
        }
        let version = decoder.read_u16()?;
        if version != STATISTICAL_LOG_RECORD_VERSION {
            return Err(StatisticalLogCodecError::UnsupportedVersion {
                domain: CanonicalDomain::Record,
                actual: version,
                supported: STATISTICAL_LOG_RECORD_VERSION,
            });
        }
        let record_tag = decoder.read_u8()?;
        if record_tag != RECORD_TAG {
            return Err(StatisticalLogCodecError::InvalidRecordTag { actual: record_tag });
        }
        let claim_tag = decoder.read_u8()?;
        if claim_tag != STATISTICAL_CLAIM_TAG {
            return Err(StatisticalLogCodecError::NonStatisticalClaimTag { actual: claim_tag });
        }
        let monitor_kind = StatisticalMonitorKind::try_from_tag(decoder.read_u8()?)?;
        let statistic_tag = decoder.read_u8()?;
        let reserved = decoder.read_u16()?;
        if reserved != RECORD_RESERVED {
            return Err(StatisticalLogCodecError::NonZeroReserved {
                domain: CanonicalDomain::Record,
                actual: reserved,
            });
        }
        let source_monitor_oid = decoder.read_oid()?;
        let monitor_oid = decoder.read_oid()?;
        let encoded_evidence_oid = decoder.read_oid()?;
        let encoded_evidence_digest = StatisticalEvidenceDigest(decoder.read_array::<32>()?);
        let filtration_or_window_oid = decoder.read_oid()?;
        let identity_first = decoder.read_u64()?;
        let identity_last = decoder.read_u64()?;
        let first = decoder.read_u64()?;
        let last = decoder.read_u64()?;
        let regime_epoch = decoder.read_u64()?;
        let candidate_decision_oid = decoder.read_oid()?;
        let pinned_fallback_oid = decoder.read_oid()?;
        let selected_policy_oid = decoder.read_oid()?;
        let payload_len = usize::from(decoder.read_u16()?);
        let payload = decoder.read_bytes(payload_len)?;
        decoder.finish()?;
        let statistic = decode_statistic(statistic_tag, payload)?;
        let identity_window = StatisticalBatchRange::try_new(identity_first, identity_last)
            .map_err(StatisticalLogCodecError::InvalidRecord)?;
        let batch = StatisticalBatchRange::try_new(first, last)
            .map_err(StatisticalLogCodecError::InvalidRecord)?;
        validate_record_parts(
            monitor_kind,
            identity_window,
            batch,
            candidate_decision_oid,
            pinned_fallback_oid,
            selected_policy_oid,
            statistic,
        )
        .map_err(StatisticalLogCodecError::InvalidRecord)?;
        let canonical_monitor_identity = encode_monitor_identity_body(
            monitor_kind,
            source_monitor_oid,
            filtration_or_window_oid,
            identity_window,
            regime_epoch,
            candidate_decision_oid,
            pinned_fallback_oid,
            statistic,
        )
        .map_err(StatisticalLogCodecError::InvalidRecord)?;
        identity_verifier
            .verify_statistical_monitor_oid(&canonical_monitor_identity, monitor_oid)
            .map_err(
                |source| StatisticalLogCodecError::MonitorIdentityVerificationFailed {
                    monitor_oid,
                    source,
                },
            )?;
        let canonical_evidence_body = encode_evidence_body(
            monitor_kind,
            source_monitor_oid,
            monitor_oid,
            filtration_or_window_oid,
            identity_window,
            batch,
            regime_epoch,
            candidate_decision_oid,
            pinned_fallback_oid,
            selected_policy_oid,
            statistic,
        )
        .map_err(StatisticalLogCodecError::InvalidRecord)?;
        let evidence_digest = digest_evidence_body(&canonical_evidence_body);
        if !evidence_digests_match(evidence_digest, encoded_evidence_digest) {
            return Err(StatisticalLogCodecError::EvidenceDigestMismatch {
                expected: evidence_digest,
                actual: encoded_evidence_digest,
            });
        }
        identity_verifier
            .verify_statistical_evidence_oid(&canonical_evidence_body, encoded_evidence_oid)
            .map_err(
                |source| StatisticalLogCodecError::EvidenceIdentityVerificationFailed {
                    evidence_oid: encoded_evidence_oid,
                    source,
                },
            )?;
        Ok(Self {
            monitor_kind,
            source_monitor_oid,
            monitor_oid,
            evidence_oid: encoded_evidence_oid,
            evidence_digest,
            filtration_or_window_oid,
            identity_window,
            batch,
            regime_epoch,
            candidate_decision_oid,
            pinned_fallback_oid,
            selected_policy_oid,
            statistic,
        })
    }
}

/// Bounded ordered statistical log.
///
/// Deliberately non-`Clone`: one mutable log cannot fork into divergent
/// suffixes under the same ordered batch history.
#[derive(Debug, Eq, PartialEq)]
pub struct StatisticalDecisionLog {
    maximum_records: usize,
    records: Vec<StatisticalLogRecord>,
    last_by_monitor: Vec<StatisticalLogRecord>,
}

impl StatisticalDecisionLog {
    /// Constructs an empty log with a finite hard record bound.
    pub fn try_new(maximum_records: usize) -> Result<Self, StatisticalLogAppendError> {
        if maximum_records == 0 {
            return Err(StatisticalLogAppendError::ZeroRecordLimit);
        }
        if maximum_records > MAX_STATISTICAL_LOG_RECORDS {
            return Err(StatisticalLogAppendError::RecordLimitTooLarge {
                actual: maximum_records,
                maximum: MAX_STATISTICAL_LOG_RECORDS,
            });
        }
        Ok(Self {
            maximum_records,
            records: Vec::new(),
            last_by_monitor: Vec::new(),
        })
    }

    /// Hard record limit.
    #[must_use]
    pub const fn maximum_records(&self) -> usize {
        self.maximum_records
    }

    /// Accepted immutable records.
    #[must_use]
    pub fn records(&self) -> &[StatisticalLogRecord] {
        &self.records
    }

    /// Number of accepted records.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the log has no records.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Appends one canonically later record whose batch is strictly later for
    /// its monitor family.
    ///
    /// Every semantic and allocation check completes before the record vector
    /// changes, so all typed failures are atomic.
    pub fn append(
        &mut self,
        record: StatisticalLogRecord,
    ) -> Result<(), StatisticalLogAppendError> {
        if self.records.len() >= self.maximum_records {
            return Err(StatisticalLogAppendError::RecordLimitReached {
                maximum: self.maximum_records,
            });
        }
        let monitor_position = self
            .last_by_monitor
            .binary_search_by(|previous| canonical_monitor_identity_order(previous, &record));
        if let Ok(position) = monitor_position {
            let previous = self.last_by_monitor[position];
            if previous == record {
                return Err(StatisticalLogAppendError::DuplicateRecord {
                    monitor: record.monitor_kind,
                    batch: record.batch,
                    evidence_oid: record.evidence_oid,
                });
            }
            if record.batch.first <= previous.batch.last {
                return Err(StatisticalLogAppendError::BatchNotStrictlyAfterMonitor {
                    monitor: record.monitor_kind,
                    previous: previous.batch,
                    incoming: record.batch,
                });
            }
        }
        if let Some(previous) = self.records.last()
            && canonical_record_order(previous, &record) != Ordering::Less
        {
            return Err(StatisticalLogAppendError::RecordNotInCanonicalOrder {
                previous_monitor: previous.monitor_kind,
                previous_batch: previous.batch,
                incoming_monitor: record.monitor_kind,
                incoming_batch: record.batch,
            });
        }
        self.records
            .try_reserve(1)
            .map_err(|_| StatisticalLogAppendError::AllocationFailed)?;
        if monitor_position.is_err() {
            self.last_by_monitor
                .try_reserve(1)
                .map_err(|_| StatisticalLogAppendError::AllocationFailed)?;
        }
        self.records.push(record);
        match monitor_position {
            Ok(position) => self.last_by_monitor[position] = record,
            Err(position) => self.last_by_monitor.insert(position, record),
        }
        Ok(())
    }

    /// Encodes the bound and every record under the strict version-4 format.
    pub fn encode_canonical(&self) -> Result<Vec<u8>, StatisticalLogCodecError> {
        let mut encoded_records = Vec::new();
        encoded_records
            .try_reserve_exact(self.records.len())
            .map_err(|_| StatisticalLogCodecError::AllocationFailed {
                requested: self.records.len(),
            })?;
        let mut total_len = LOG_HEADER_BYTES;
        for record in &self.records {
            let encoded = record.encode_canonical()?;
            let framed_len = 4_usize
                .checked_add(encoded.len())
                .ok_or(StatisticalLogCodecError::LengthOverflow)?;
            total_len = total_len
                .checked_add(framed_len)
                .ok_or(StatisticalLogCodecError::LengthOverflow)?;
            encoded_records.push(encoded);
        }

        let mut bytes = Vec::new();
        bytes.try_reserve_exact(total_len).map_err(|_| {
            StatisticalLogCodecError::AllocationFailed {
                requested: total_len,
            }
        })?;
        bytes.extend_from_slice(&LOG_MAGIC);
        push_u16(&mut bytes, STATISTICAL_LOG_VERSION);
        push_u16(&mut bytes, LOG_RESERVED);
        let maximum_records = u32::try_from(self.maximum_records)
            .map_err(|_| StatisticalLogCodecError::LengthOverflow)?;
        let record_count = u32::try_from(self.records.len())
            .map_err(|_| StatisticalLogCodecError::LengthOverflow)?;
        push_u32(&mut bytes, maximum_records);
        push_u32(&mut bytes, record_count);
        for encoded in encoded_records {
            let length = u32::try_from(encoded.len())
                .map_err(|_| StatisticalLogCodecError::LengthOverflow)?;
            push_u32(&mut bytes, length);
            bytes.extend_from_slice(&encoded);
        }
        Ok(bytes)
    }

    /// Decodes a complete strict version-4 log and replays its append checks.
    pub fn decode_canonical(
        bytes: &[u8],
        expected_maximum_records: usize,
        limits: StatisticalLogDecodeLimits,
        identity_verifier: &impl StatisticalEvidenceIdentityVerifier,
    ) -> Result<Self, StatisticalLogCodecError> {
        if bytes.len() > limits.max_encoded_bytes {
            return Err(StatisticalLogCodecError::EncodedByteLimitExceeded {
                actual: bytes.len(),
                maximum: limits.max_encoded_bytes,
            });
        }
        let mut decoder = Decoder::new(bytes);
        let magic = decoder.read_array::<8>()?;
        if magic != LOG_MAGIC {
            return Err(StatisticalLogCodecError::MagicMismatch {
                expected: CanonicalDomain::Log,
            });
        }
        let version = decoder.read_u16()?;
        if version != STATISTICAL_LOG_VERSION {
            return Err(StatisticalLogCodecError::UnsupportedVersion {
                domain: CanonicalDomain::Log,
                actual: version,
                supported: STATISTICAL_LOG_VERSION,
            });
        }
        let reserved = decoder.read_u16()?;
        if reserved != LOG_RESERVED {
            return Err(StatisticalLogCodecError::NonZeroReserved {
                domain: CanonicalDomain::Log,
                actual: reserved,
            });
        }
        let maximum_records = usize::try_from(decoder.read_u32()?)
            .map_err(|_| StatisticalLogCodecError::LengthOverflow)?;
        let record_count = usize::try_from(decoder.read_u32()?)
            .map_err(|_| StatisticalLogCodecError::LengthOverflow)?;
        if maximum_records != expected_maximum_records {
            return Err(StatisticalLogCodecError::LogBoundMismatch {
                expected: expected_maximum_records,
                actual: maximum_records,
            });
        }
        if maximum_records > limits.max_records {
            return Err(StatisticalLogCodecError::DecodeRecordLimitExceeded {
                actual: maximum_records,
                maximum: limits.max_records,
            });
        }
        if record_count > limits.max_records {
            return Err(StatisticalLogCodecError::DecodeRecordLimitExceeded {
                actual: record_count,
                maximum: limits.max_records,
            });
        }
        if record_count > maximum_records {
            return Err(StatisticalLogCodecError::RecordCountExceedsLimit {
                count: record_count,
                maximum: maximum_records,
            });
        }

        let records_offset = decoder.offset;
        for index in 0..record_count {
            let record_len = usize::try_from(decoder.read_u32()?)
                .map_err(|_| StatisticalLogCodecError::LengthOverflow)?;
            let record_bytes = decoder.read_bytes(record_len)?;
            preflight_record_frame(index, record_bytes)?;
        }
        decoder.finish()?;

        let mut log =
            Self::try_new(maximum_records).map_err(StatisticalLogCodecError::InvalidLogBound)?;
        log.records.try_reserve_exact(record_count).map_err(|_| {
            StatisticalLogCodecError::AllocationFailed {
                requested: record_count,
            }
        })?;
        log.last_by_monitor
            .try_reserve_exact(record_count)
            .map_err(|_| StatisticalLogCodecError::AllocationFailed {
                requested: record_count,
            })?;
        let mut decoder = Decoder {
            bytes,
            offset: records_offset,
        };
        for index in 0..record_count {
            let record_len = usize::try_from(decoder.read_u32()?)
                .map_err(|_| StatisticalLogCodecError::LengthOverflow)?;
            let record_bytes = decoder.read_bytes(record_len)?;
            preflight_record_frame(index, record_bytes)?;
            let record = StatisticalLogRecord::decode_canonical(record_bytes, identity_verifier)?;
            log.append(record)
                .map_err(StatisticalLogCodecError::InvalidAppend)?;
        }
        decoder.finish()?;
        Ok(log)
    }
}

/// Statistic field named by a validation failure.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StatisticField {
    EProcessP0,
    EProcessLambda,
    EProcessAlpha,
    EProcessMaximum,
    EValue,
    RejectionThreshold,
    OneObservations,
    ConformalAlpha,
    ConformalThreshold,
    NonconformityScore,
    CoverageTarget,
    MinimumCalibrationSamples,
    MaximumCalibrationSamples,
    CoveredAssessments,
    ExplorationAlpha,
    ResidualRate,
    ExplorationUpperBound,
    TargetResidualRate,
    Discoveries,
    CurrentPotential,
    ConfidenceBound,
    Detections,
    ZeroSupportExclusions,
    MaximumOpeObservations,
    MaximumOpeActionsPerObservation,
    MaximumOpeActionRows,
    AnnTopK,
    MaximumAnnQueries,
    MaximumAnnResultIds,
    AnnExactBaselineResults,
    AnnCandidateResults,
    AnnIntersectionHits,
    AnnExactRecallIntersectionHits,
    AnnExactRecallBaselineResults,
    AnnIntervalScale,
    AnnIntervalPointEstimate,
    AnnIntervalLower,
    AnnIntervalUpper,
    AnnIntervalRadius,
    AnnIntervalConfidenceExponent,
    AnnIntervalQueryObservations,
}

impl fmt::Display for StatisticField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EProcessP0 => "e-process null probability",
            Self::EProcessLambda => "e-process betting factor",
            Self::EProcessAlpha => "e-process alpha",
            Self::EProcessMaximum => "e-process saturation bound",
            Self::EValue => "e-value",
            Self::RejectionThreshold => "rejection threshold",
            Self::OneObservations => "binary-one observations",
            Self::ConformalAlpha => "conformal alpha",
            Self::ConformalThreshold => "conformal threshold",
            Self::NonconformityScore => "nonconformity score",
            Self::CoverageTarget => "coverage target",
            Self::MinimumCalibrationSamples => "minimum calibration samples",
            Self::MaximumCalibrationSamples => "maximum calibration samples",
            Self::CoveredAssessments => "covered assessments",
            Self::ExplorationAlpha => "exploration alpha",
            Self::ResidualRate => "residual rate",
            Self::ExplorationUpperBound => "exploration upper bound",
            Self::TargetResidualRate => "target residual rate",
            Self::Discoveries => "discoveries",
            Self::CurrentPotential => "current potential",
            Self::ConfidenceBound => "confidence bound",
            Self::Detections => "detections",
            Self::ZeroSupportExclusions => "zero-support exclusions",
            Self::MaximumOpeObservations => "maximum OPE observations",
            Self::MaximumOpeActionsPerObservation => "maximum OPE actions per observation",
            Self::MaximumOpeActionRows => "maximum OPE action rows",
            Self::AnnTopK => "ANN top-k",
            Self::MaximumAnnQueries => "maximum ANN queries",
            Self::MaximumAnnResultIds => "maximum ANN result identities",
            Self::AnnExactBaselineResults => "ANN exact-baseline results",
            Self::AnnCandidateResults => "ANN candidate results",
            Self::AnnIntersectionHits => "ANN intersection hits",
            Self::AnnExactRecallIntersectionHits => "ANN exact-recall intersection hits",
            Self::AnnExactRecallBaselineResults => "ANN exact-recall baseline results",
            Self::AnnIntervalScale => "ANN interval scale",
            Self::AnnIntervalPointEstimate => "ANN interval point estimate",
            Self::AnnIntervalLower => "ANN interval lower endpoint",
            Self::AnnIntervalUpper => "ANN interval upper endpoint",
            Self::AnnIntervalRadius => "ANN interval radius",
            Self::AnnIntervalConfidenceExponent => "ANN interval confidence exponent",
            Self::AnnIntervalQueryObservations => "ANN interval query observations",
        })
    }
}

/// Immutable-record construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatisticalLogRecordError {
    ReversedBatchRange {
        first: u64,
        last: u64,
    },
    EvidenceHasNoObservations {
        monitor: StatisticalMonitorKind,
    },
    EvidenceBatchCountMismatch {
        monitor: StatisticalMonitorKind,
        first: u64,
        last: u64,
        observations: u64,
    },
    EvidenceBatchOutsideIdentityWindow {
        monitor: StatisticalMonitorKind,
        first: u64,
        last: u64,
        identity_last: u64,
    },
    BatchOutsideIdentityWindow {
        batch: StatisticalBatchRange,
        identity_window: StatisticalBatchRange,
    },
    EvidenceStatisticUnavailable {
        monitor: StatisticalMonitorKind,
        field: StatisticField,
    },
    EvidenceCounterUnrepresentable {
        monitor: StatisticalMonitorKind,
        field: StatisticField,
    },
    EvidenceEstimateDenominatorMismatch {
        candidate: u128,
        fallback: u128,
        advantage: u128,
    },
    MissingRegimeDetectorSnapshot,
    EvidenceIdentityLengthOverflow,
    EvidenceIdentityAllocationFailed {
        requested: usize,
    },
    EvidenceIdentityIssuanceFailed {
        source: StatisticalEvidenceIdentityError,
    },
    MonitorIdentityIssuanceFailed {
        source: StatisticalEvidenceIdentityError,
    },
    AnnRecallEvidenceIncomplete,
    AnnRecallActionUnavailable,
    AnnRecallProfileIdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    CandidateEqualsFallback,
    SelectedPolicyIsNeitherCandidateNorFallback {
        selected: ObjectId,
        candidate: ObjectId,
        fallback: ObjectId,
    },
    AnnRecallRebuildPolicyCollision {
        rebuild: ObjectId,
        candidate: ObjectId,
        fallback: ObjectId,
    },
    SelectedAnnRecallPolicyUnregistered {
        selected: ObjectId,
    },
    MonitorStatisticMismatch {
        monitor: StatisticalMonitorKind,
        statistic_tag: u8,
    },
    InvalidEProcessConfiguration,
    InvalidConformalAlpha {
        bits: u64,
    },
    InvalidConformalCalibrationBounds {
        minimum: u64,
        maximum: u64,
    },
    InvalidExplorationAlpha {
        bits: u64,
    },
    NonCanonicalFloatBits {
        field: StatisticField,
        bits: u64,
    },
    StatisticOutsideUnitInterval {
        field: StatisticField,
        bits: u64,
    },
    StatisticMustBeNonNegative {
        field: StatisticField,
        bits: u64,
    },
    StatisticMustBePositive {
        field: StatisticField,
        bits: u64,
    },
    StatisticMustBeFiniteOrPositiveInfinity {
        field: StatisticField,
        bits: u64,
    },
    SubcountExceedsTotal {
        field: StatisticField,
        subcount: u64,
        total: u64,
    },
    NonPositiveRegimeThreshold {
        threshold: i64,
    },
    ZeroEstimateDenominator,
    InvalidOpeProfile,
    OpeArithmeticOverflow,
    OpeAdvantageMismatch {
        candidate: i128,
        fallback: i128,
        actual: i128,
    },
    OpeEstimateDenominatorMismatch {
        expected: u128,
        actual: u128,
    },
    EffectiveSampleSizeDenominatorMismatch {
        policy: EvaluatedPolicy,
        numerator: u128,
        denominator: u128,
    },
    EffectiveSampleSizeExceedsObservations {
        policy: EvaluatedPolicy,
        numerator: u128,
        denominator: u128,
        observations: u64,
    },
    OpeEssGateMismatch {
        policy: EvaluatedPolicy,
        expected: bool,
        actual: bool,
    },
    OpeObservationCountOutOfBounds {
        observations: u64,
        maximum: u64,
    },
    OpeActionRowCountOutOfBounds {
        action_rows: u64,
        observations: u64,
        maximum: u64,
    },
    EmptyOpeSupportExclusionsDigestMismatch,
    OpeProfileCannotContainIdentityWindow {
        maximum_observations: u64,
        window_capacity: u64,
    },
    OpeCompletenessMismatch {
        complete: bool,
        observations: u64,
        window_capacity: u64,
    },
    OpeBatchSequenceMismatch {
        batch: StatisticalBatchRange,
        expected: u64,
    },
    OpeActionRowsExceedPerObservationProfile {
        action_rows: u64,
        maximum: u64,
    },
    OpeSelectionReasonMismatch {
        expected: OpeSelectionReason,
        actual: OpeSelectionReason,
    },
    OpeSelectionMismatch {
        reason: OpeSelectionReason,
        selected: ObjectId,
        expected: ObjectId,
    },
    InvalidAnnRecallProfile,
    AnnRecallArithmeticOverflow,
    AnnRecallDerivedFieldMismatch {
        field: StatisticField,
        expected: u64,
        actual: u64,
    },
    AnnRecallResultInventoryExceedsProfile {
        actual: u64,
        maximum: u64,
    },
    AnnRecallEvidenceMustBeTerminal,
    AnnRecallAssumptionSupportMismatch {
        expected: bool,
        actual: bool,
    },
    AnnRecallActionReasonMismatch {
        expected_action: AnnRecallAction,
        actual_action: AnnRecallAction,
        expected_reason: AnnRecallActionReason,
        actual_reason: AnnRecallActionReason,
    },
    AnnRecallObservationWindowMismatch {
        observations: u64,
        window_capacity: u64,
    },
    AnnRecallProfileCannotContainIdentityWindow {
        maximum_queries: u64,
        window_capacity: u64,
    },
    AnnRecallBatchMustEqualWindow {
        batch: StatisticalBatchRange,
        identity_window: StatisticalBatchRange,
    },
    AnnRecallSelectionMismatch {
        action: AnnRecallAction,
        reason: AnnRecallActionReason,
        selected: ObjectId,
        expected: ObjectId,
    },
}

impl fmt::Display for StatisticalLogRecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReversedBatchRange { first, last } => {
                write!(formatter, "statistical batch {first}..={last} is reversed")
            }
            Self::EvidenceHasNoObservations { monitor } => {
                write!(formatter, "{monitor:?} evidence has no observation batch")
            }
            Self::EvidenceBatchCountMismatch {
                monitor,
                first,
                last,
                observations,
            } => write!(
                formatter,
                "{monitor:?} evidence batch {first}..={last} does not contain {observations} observations"
            ),
            Self::EvidenceBatchOutsideIdentityWindow {
                monitor,
                first,
                last,
                identity_last,
            } => write!(
                formatter,
                "{monitor:?} evidence batch {first}..={last} exceeds identity end {identity_last}"
            ),
            Self::BatchOutsideIdentityWindow {
                batch,
                identity_window,
            } => write!(
                formatter,
                "statistical batch {}..={} is outside identity window {}..={}",
                batch.first, batch.last, identity_window.first, identity_window.last
            ),
            Self::EvidenceStatisticUnavailable { monitor, field } => {
                write!(formatter, "{monitor:?} evidence has no {field}")
            }
            Self::EvidenceCounterUnrepresentable { monitor, field } => write!(
                formatter,
                "{monitor:?} evidence counter {field} is not representable as u64"
            ),
            Self::EvidenceEstimateDenominatorMismatch {
                candidate,
                fallback,
                advantage,
            } => write!(
                formatter,
                "OPE estimate denominators differ: candidate {candidate}, fallback {fallback}, advantage {advantage}"
            ),
            Self::MissingRegimeDetectorSnapshot => {
                formatter.write_str("regime evidence contains no detector snapshot")
            }
            Self::EvidenceIdentityLengthOverflow => {
                formatter.write_str("canonical evidence identity length overflowed")
            }
            Self::EvidenceIdentityAllocationFailed { requested } => write!(
                formatter,
                "could not allocate {requested} bytes for canonical evidence identity"
            ),
            Self::EvidenceIdentityIssuanceFailed { source } => {
                write!(
                    formatter,
                    "could not issue statistical evidence identity: {source}"
                )
            }
            Self::MonitorIdentityIssuanceFailed { source } => {
                write!(
                    formatter,
                    "could not issue stable statistical monitor identity: {source}"
                )
            }
            Self::AnnRecallEvidenceIncomplete => {
                formatter.write_str("ANN-recall evidence is incomplete and has no terminal record")
            }
            Self::AnnRecallActionUnavailable => {
                formatter.write_str("ANN-recall evidence has no terminal action")
            }
            Self::AnnRecallProfileIdentityMismatch { expected, actual } => write!(
                formatter,
                "ANN-recall profile identity {actual:?} does not match trial identity {expected:?}"
            ),
            Self::CandidateEqualsFallback => {
                formatter.write_str("candidate and pinned fallback identities must differ")
            }
            Self::SelectedPolicyIsNeitherCandidateNorFallback {
                selected,
                candidate,
                fallback,
            } => write!(
                formatter,
                "selected policy {selected:?} is neither candidate {candidate:?} nor fallback {fallback:?}"
            ),
            Self::AnnRecallRebuildPolicyCollision {
                rebuild,
                candidate,
                fallback,
            } => write!(
                formatter,
                "ANN rebuild policy {rebuild:?} collides with candidate {candidate:?} or fallback {fallback:?}"
            ),
            Self::SelectedAnnRecallPolicyUnregistered { selected } => write!(
                formatter,
                "selected ANN policy {selected:?} is not one of the trial's registered candidate, fallback, or rebuild policies"
            ),
            Self::MonitorStatisticMismatch {
                monitor,
                statistic_tag,
            } => write!(
                formatter,
                "monitor {monitor:?} cannot carry statistic tag {statistic_tag}"
            ),
            Self::InvalidEProcessConfiguration => {
                formatter.write_str("canonical e-process configuration is invalid")
            }
            Self::InvalidConformalAlpha { bits } => write!(
                formatter,
                "conformal alpha bits 0x{bits:016x} are not finite and inside (0, 1)"
            ),
            Self::InvalidConformalCalibrationBounds { minimum, maximum } => write!(
                formatter,
                "conformal calibration bounds {minimum}..={maximum} are invalid"
            ),
            Self::InvalidExplorationAlpha { bits } => write!(
                formatter,
                "exploration alpha bits 0x{bits:016x} are not finite and inside (0, 1)"
            ),
            Self::NonCanonicalFloatBits { field, bits } => write!(
                formatter,
                "{field} bits 0x{bits:016x} are NaN or negative zero"
            ),
            Self::StatisticOutsideUnitInterval { field, bits } => write!(
                formatter,
                "{field} bits 0x{bits:016x} are outside the closed unit interval"
            ),
            Self::StatisticMustBeNonNegative { field, bits } => write!(
                formatter,
                "{field} bits 0x{bits:016x} are negative or non-finite"
            ),
            Self::StatisticMustBePositive { field, bits } => write!(
                formatter,
                "{field} bits 0x{bits:016x} are not finite and positive"
            ),
            Self::StatisticMustBeFiniteOrPositiveInfinity { field, bits } => write!(
                formatter,
                "{field} bits 0x{bits:016x} are neither finite nor positive infinity"
            ),
            Self::SubcountExceedsTotal {
                field,
                subcount,
                total,
            } => write!(formatter, "{field} count {subcount} exceeds total {total}"),
            Self::NonPositiveRegimeThreshold { threshold } => {
                write!(formatter, "regime threshold {threshold} is not positive")
            }
            Self::ZeroEstimateDenominator => {
                formatter.write_str("off-policy estimate denominator must be positive")
            }
            Self::InvalidOpeProfile => {
                formatter.write_str("canonical off-policy profile is invalid")
            }
            Self::OpeArithmeticOverflow => {
                formatter.write_str("off-policy cross-field arithmetic overflowed")
            }
            Self::OpeAdvantageMismatch {
                candidate,
                fallback,
                actual,
            } => write!(
                formatter,
                "OPE advantage {actual} does not equal candidate {candidate} minus fallback {fallback}"
            ),
            Self::OpeEstimateDenominatorMismatch { expected, actual } => write!(
                formatter,
                "OPE estimate denominator {actual} does not equal canonical denominator {expected}"
            ),
            Self::EffectiveSampleSizeDenominatorMismatch {
                policy,
                numerator,
                denominator,
            } => write!(
                formatter,
                "{policy:?} ESS numerator {numerator} is nonzero with denominator {denominator}"
            ),
            Self::EffectiveSampleSizeExceedsObservations {
                policy,
                numerator,
                denominator,
                observations,
            } => write!(
                formatter,
                "{policy:?} ESS {numerator}/{denominator} exceeds {observations} observations"
            ),
            Self::OpeEssGateMismatch {
                policy,
                expected,
                actual,
            } => write!(
                formatter,
                "{policy:?} ESS gate {actual} does not equal recomputed gate {expected}"
            ),
            Self::OpeObservationCountOutOfBounds {
                observations,
                maximum,
            } => write!(
                formatter,
                "OPE observation count {observations} is outside 1..={maximum}"
            ),
            Self::OpeActionRowCountOutOfBounds {
                action_rows,
                observations,
                maximum,
            } => write!(
                formatter,
                "OPE action-row count {action_rows} is outside {observations}..={maximum}"
            ),
            Self::EmptyOpeSupportExclusionsDigestMismatch => formatter.write_str(
                "zero OPE support exclusions do not carry the canonical empty commitment",
            ),
            Self::OpeProfileCannotContainIdentityWindow {
                maximum_observations,
                window_capacity,
            } => write!(
                formatter,
                "OPE profile admits {maximum_observations} observations but identity window needs {window_capacity}"
            ),
            Self::OpeCompletenessMismatch {
                complete,
                observations,
                window_capacity,
            } => write!(
                formatter,
                "OPE completeness {complete} disagrees with {observations}/{window_capacity} observations"
            ),
            Self::OpeBatchSequenceMismatch { batch, expected } => write!(
                formatter,
                "OPE batch {}..={} does not identify expected prefix sequence {expected}",
                batch.first, batch.last
            ),
            Self::OpeActionRowsExceedPerObservationProfile {
                action_rows,
                maximum,
            } => write!(
                formatter,
                "OPE action-row count {action_rows} exceeds per-observation envelope {maximum}"
            ),
            Self::OpeSelectionReasonMismatch { expected, actual } => write!(
                formatter,
                "OPE selection reason {actual:?} does not equal recomputed reason {expected:?}"
            ),
            Self::OpeSelectionMismatch {
                reason,
                selected,
                expected,
            } => write!(
                formatter,
                "OPE reason {reason:?} requires policy {expected:?}, not {selected:?}"
            ),
            Self::InvalidAnnRecallProfile => {
                formatter.write_str("canonical ANN-recall profile is invalid")
            }
            Self::AnnRecallArithmeticOverflow => {
                formatter.write_str("ANN-recall cross-field arithmetic overflowed")
            }
            Self::AnnRecallDerivedFieldMismatch {
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "{field} value {actual} does not equal recomputed value {expected}"
            ),
            Self::AnnRecallResultInventoryExceedsProfile { actual, maximum } => write!(
                formatter,
                "ANN result inventory {actual} exceeds profile maximum {maximum}"
            ),
            Self::AnnRecallEvidenceMustBeTerminal => {
                formatter.write_str("canonical ANN-recall evidence must be complete and terminal")
            }
            Self::AnnRecallAssumptionSupportMismatch { expected, actual } => write!(
                formatter,
                "ANN assumption-support flag {actual} does not equal recomputed value {expected}"
            ),
            Self::AnnRecallActionReasonMismatch {
                expected_action,
                actual_action,
                expected_reason,
                actual_reason,
            } => write!(
                formatter,
                "ANN action {actual_action:?}/{actual_reason:?} does not equal recomputed {expected_action:?}/{expected_reason:?}"
            ),
            Self::AnnRecallObservationWindowMismatch {
                observations,
                window_capacity,
            } => write!(
                formatter,
                "ANN observation count {observations} does not equal identity-window capacity {window_capacity}"
            ),
            Self::AnnRecallProfileCannotContainIdentityWindow {
                maximum_queries,
                window_capacity,
            } => write!(
                formatter,
                "ANN profile admits {maximum_queries} queries but identity window needs {window_capacity}"
            ),
            Self::AnnRecallBatchMustEqualWindow {
                batch,
                identity_window,
            } => write!(
                formatter,
                "terminal ANN batch {}..={} does not equal identity window {}..={}",
                batch.first, batch.last, identity_window.first, identity_window.last
            ),
            Self::AnnRecallSelectionMismatch {
                action,
                reason,
                selected,
                expected,
            } => write!(
                formatter,
                "ANN action {action:?}/{reason:?} requires policy {expected:?}, not {selected:?}"
            ),
        }
    }
}

impl std::error::Error for StatisticalLogRecordError {}

/// Atomic append failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatisticalLogAppendError {
    ZeroRecordLimit,
    RecordLimitTooLarge {
        actual: usize,
        maximum: usize,
    },
    RecordLimitReached {
        maximum: usize,
    },
    DuplicateRecord {
        monitor: StatisticalMonitorKind,
        batch: StatisticalBatchRange,
        evidence_oid: ObjectId,
    },
    BatchNotStrictlyAfterMonitor {
        monitor: StatisticalMonitorKind,
        previous: StatisticalBatchRange,
        incoming: StatisticalBatchRange,
    },
    RecordNotInCanonicalOrder {
        previous_monitor: StatisticalMonitorKind,
        previous_batch: StatisticalBatchRange,
        incoming_monitor: StatisticalMonitorKind,
        incoming_batch: StatisticalBatchRange,
    },
    AllocationFailed,
}

impl fmt::Display for StatisticalLogAppendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroRecordLimit => {
                formatter.write_str("statistical log record limit must be positive")
            }
            Self::RecordLimitTooLarge { actual, maximum } => write!(
                formatter,
                "statistical log record limit {actual} exceeds ceiling {maximum}"
            ),
            Self::RecordLimitReached { maximum } => {
                write!(formatter, "statistical log record limit {maximum} reached")
            }
            Self::DuplicateRecord {
                monitor,
                batch,
                evidence_oid,
            } => write!(
                formatter,
                "duplicate {monitor:?} record for batch {}..={} and evidence {evidence_oid:?}",
                batch.first, batch.last
            ),
            Self::BatchNotStrictlyAfterMonitor {
                monitor,
                previous,
                incoming,
            } => write!(
                formatter,
                "incoming {monitor:?} batch {}..={} is not strictly after {}..={}",
                incoming.first, incoming.last, previous.first, previous.last
            ),
            Self::RecordNotInCanonicalOrder {
                previous_monitor,
                previous_batch,
                incoming_monitor,
                incoming_batch,
            } => write!(
                formatter,
                "incoming {incoming_monitor:?} batch {}..={} does not sort after {previous_monitor:?} batch {}..={}",
                incoming_batch.first,
                incoming_batch.last,
                previous_batch.first,
                previous_batch.last
            ),
            Self::AllocationFailed => {
                formatter.write_str("could not reserve statistical log storage")
            }
        }
    }
}

impl std::error::Error for StatisticalLogAppendError {}

/// Canonical encoding domain named by a codec failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalDomain {
    Record,
    Log,
}

/// Strict canonical encode/decode failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatisticalLogCodecError {
    Truncated {
        offset: usize,
        needed: usize,
        remaining: usize,
    },
    TrailingBytes {
        offset: usize,
        remaining: usize,
    },
    MagicMismatch {
        expected: CanonicalDomain,
    },
    UnsupportedVersion {
        domain: CanonicalDomain,
        actual: u16,
        supported: u16,
    },
    InvalidRecordTag {
        actual: u8,
    },
    NonStatisticalClaimTag {
        actual: u8,
    },
    UnknownMonitorKind {
        tag: u8,
    },
    UnknownStatisticKind {
        tag: u8,
    },
    UnknownConformalMode {
        tag: u8,
    },
    UnknownOpeEstimator {
        tag: u8,
    },
    UnknownOpeFailureBehavior {
        tag: u8,
    },
    UnknownOpeSelectionReason {
        tag: u8,
    },
    UnknownAnnSampleDesign {
        tag: u8,
    },
    UnknownAnnAction {
        tag: u8,
    },
    UnknownAnnActionReason {
        tag: u8,
    },
    InvalidBooleanTag {
        statistic_tag: u8,
        field: &'static str,
        actual: u8,
    },
    NonZeroReserved {
        domain: CanonicalDomain,
        actual: u16,
    },
    StatisticPayloadLengthMismatch {
        statistic_tag: u8,
        actual: usize,
        expected: usize,
    },
    EvidenceDigestMismatch {
        expected: StatisticalEvidenceDigest,
        actual: StatisticalEvidenceDigest,
    },
    EvidenceIdentityVerificationFailed {
        evidence_oid: ObjectId,
        source: StatisticalEvidenceIdentityError,
    },
    MonitorIdentityVerificationFailed {
        monitor_oid: ObjectId,
        source: StatisticalEvidenceIdentityError,
    },
    InvalidRecord(StatisticalLogRecordError),
    InvalidLogBound(StatisticalLogAppendError),
    InvalidAppend(StatisticalLogAppendError),
    RecordCountExceedsLimit {
        count: usize,
        maximum: usize,
    },
    LogBoundMismatch {
        expected: usize,
        actual: usize,
    },
    DecodeRecordLimitExceeded {
        actual: usize,
        maximum: usize,
    },
    EncodedByteLimitExceeded {
        actual: usize,
        maximum: usize,
    },
    RecordFrameTooLarge {
        index: usize,
        actual: usize,
        maximum: usize,
    },
    RecordFrameTooSmall {
        index: usize,
        actual: usize,
        minimum: usize,
    },
    RecordFrameLengthMismatch {
        index: usize,
        actual: usize,
        expected: usize,
    },
    LengthOverflow,
    AllocationFailed {
        requested: usize,
    },
}

impl fmt::Display for StatisticalLogCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated {
                offset,
                needed,
                remaining,
            } => write!(
                formatter,
                "canonical input at {offset} needs {needed} bytes; {remaining} remain"
            ),
            Self::TrailingBytes { offset, remaining } => write!(
                formatter,
                "canonical input has {remaining} trailing bytes at offset {offset}"
            ),
            Self::MagicMismatch { expected } => {
                write!(formatter, "{expected:?} canonical magic does not match")
            }
            Self::UnsupportedVersion {
                domain,
                actual,
                supported,
            } => write!(
                formatter,
                "{domain:?} canonical version {actual} is unsupported; expected {supported}"
            ),
            Self::InvalidRecordTag { actual } => {
                write!(formatter, "canonical record tag {actual} is invalid")
            }
            Self::NonStatisticalClaimTag { actual } => write!(
                formatter,
                "canonical claim tag {actual} is not the statistical claim tag"
            ),
            Self::UnknownMonitorKind { tag } => {
                write!(formatter, "canonical monitor tag {tag} is unknown")
            }
            Self::UnknownStatisticKind { tag } => {
                write!(formatter, "canonical statistic tag {tag} is unknown")
            }
            Self::UnknownConformalMode { tag } => {
                write!(formatter, "canonical conformal mode tag {tag} is unknown")
            }
            Self::UnknownOpeEstimator { tag } => {
                write!(formatter, "canonical OPE estimator tag {tag} is unknown")
            }
            Self::UnknownOpeFailureBehavior { tag } => {
                write!(
                    formatter,
                    "canonical OPE failure-behavior tag {tag} is unknown"
                )
            }
            Self::UnknownOpeSelectionReason { tag } => {
                write!(
                    formatter,
                    "canonical OPE selection-reason tag {tag} is unknown"
                )
            }
            Self::UnknownAnnSampleDesign { tag } => {
                write!(
                    formatter,
                    "canonical ANN sample-design tag {tag} is unknown"
                )
            }
            Self::UnknownAnnAction { tag } => {
                write!(formatter, "canonical ANN action tag {tag} is unknown")
            }
            Self::UnknownAnnActionReason { tag } => {
                write!(
                    formatter,
                    "canonical ANN action-reason tag {tag} is unknown"
                )
            }
            Self::InvalidBooleanTag {
                statistic_tag,
                field,
                actual,
            } => write!(
                formatter,
                "statistic tag {statistic_tag} field {field} has invalid boolean tag {actual}"
            ),
            Self::NonZeroReserved { domain, actual } => {
                write!(formatter, "{domain:?} reserved bits are nonzero: {actual}")
            }
            Self::StatisticPayloadLengthMismatch {
                statistic_tag,
                actual,
                expected,
            } => write!(
                formatter,
                "statistic tag {statistic_tag} payload length {actual} does not equal {expected}"
            ),
            Self::EvidenceDigestMismatch { expected, actual } => write!(
                formatter,
                "canonical evidence digest {actual:?} does not match recomputed digest {expected:?}"
            ),
            Self::EvidenceIdentityVerificationFailed {
                evidence_oid,
                source,
            } => write!(
                formatter,
                "canonical evidence identity {evidence_oid:?} was not authenticated: {source}"
            ),
            Self::MonitorIdentityVerificationFailed {
                monitor_oid,
                source,
            } => write!(
                formatter,
                "canonical monitor identity {monitor_oid:?} was not authenticated: {source}"
            ),
            Self::InvalidRecord(error) => write!(formatter, "invalid statistical record: {error}"),
            Self::InvalidLogBound(error) => {
                write!(formatter, "invalid statistical log bound: {error}")
            }
            Self::InvalidAppend(error) => {
                write!(formatter, "invalid statistical log append: {error}")
            }
            Self::RecordCountExceedsLimit { count, maximum } => write!(
                formatter,
                "canonical record count {count} exceeds encoded limit {maximum}"
            ),
            Self::LogBoundMismatch { expected, actual } => write!(
                formatter,
                "canonical log bound {actual} does not match trusted profile {expected}"
            ),
            Self::DecodeRecordLimitExceeded { actual, maximum } => write!(
                formatter,
                "canonical log record bound or count {actual} exceeds decode limit {maximum}"
            ),
            Self::EncodedByteLimitExceeded { actual, maximum } => write!(
                formatter,
                "canonical log has {actual} bytes, decode limit is {maximum}"
            ),
            Self::RecordFrameTooLarge {
                index,
                actual,
                maximum,
            } => write!(
                formatter,
                "canonical record frame {index} length {actual} exceeds {maximum}"
            ),
            Self::RecordFrameTooSmall {
                index,
                actual,
                minimum,
            } => write!(
                formatter,
                "canonical record frame {index} length {actual} is below {minimum}"
            ),
            Self::RecordFrameLengthMismatch {
                index,
                actual,
                expected,
            } => write!(
                formatter,
                "canonical record frame {index} length {actual} does not match embedded length {expected}"
            ),
            Self::LengthOverflow => formatter.write_str("canonical length arithmetic overflowed"),
            Self::AllocationFailed { requested } => {
                write!(formatter, "could not allocate {requested} canonical bytes")
            }
        }
    }
}

impl std::error::Error for StatisticalLogCodecError {}

fn preflight_record_frame(
    index: usize,
    record_bytes: &[u8],
) -> Result<(), StatisticalLogCodecError> {
    if record_bytes.len() < MIN_CANONICAL_RECORD_BYTES {
        return Err(StatisticalLogCodecError::RecordFrameTooSmall {
            index,
            actual: record_bytes.len(),
            minimum: MIN_CANONICAL_RECORD_BYTES,
        });
    }
    if record_bytes.len() > MAX_CANONICAL_RECORD_BYTES {
        return Err(StatisticalLogCodecError::RecordFrameTooLarge {
            index,
            actual: record_bytes.len(),
            maximum: MAX_CANONICAL_RECORD_BYTES,
        });
    }
    let payload_length_offset = RECORD_FIXED_BYTES - 2;
    let payload_length_bytes = record_bytes
        .get(payload_length_offset..RECORD_FIXED_BYTES)
        .ok_or(StatisticalLogCodecError::RecordFrameTooSmall {
            index,
            actual: record_bytes.len(),
            minimum: RECORD_FIXED_BYTES,
        })?;
    let [high, low] = payload_length_bytes else {
        return Err(StatisticalLogCodecError::RecordFrameTooSmall {
            index,
            actual: record_bytes.len(),
            minimum: RECORD_FIXED_BYTES,
        });
    };
    let payload_length = usize::from(u16::from_le_bytes([*high, *low]));
    let expected = RECORD_FIXED_BYTES
        .checked_add(payload_length)
        .ok_or(StatisticalLogCodecError::LengthOverflow)?;
    if record_bytes.len() != expected {
        return Err(StatisticalLogCodecError::RecordFrameLengthMismatch {
            index,
            actual: record_bytes.len(),
            expected,
        });
    }
    Ok(())
}

fn singleton_batch_from_cumulative_evidence(
    monitor: StatisticalMonitorKind,
    identity_first: u64,
    identity_last: u64,
    through_sequence: Option<u64>,
    observations: u64,
) -> Result<StatisticalBatchRange, StatisticalLogRecordError> {
    let last =
        through_sequence.ok_or(StatisticalLogRecordError::EvidenceHasNoObservations { monitor })?;
    if last > identity_last {
        return Err(
            StatisticalLogRecordError::EvidenceBatchOutsideIdentityWindow {
                monitor,
                first: identity_first,
                last,
                identity_last,
            },
        );
    }
    let expected = last
        .checked_sub(identity_first)
        .and_then(|distance| distance.checked_add(1));
    if expected != Some(observations) {
        return Err(StatisticalLogRecordError::EvidenceBatchCountMismatch {
            monitor,
            first: identity_first,
            last,
            observations,
        });
    }
    StatisticalBatchRange::try_new(last, last)
}

fn singleton_batch_from_observation_count(
    monitor: StatisticalMonitorKind,
    identity_first: u64,
    identity_last: u64,
    observations: u64,
) -> Result<StatisticalBatchRange, StatisticalLogRecordError> {
    let offset = observations
        .checked_sub(1)
        .ok_or(StatisticalLogRecordError::EvidenceHasNoObservations { monitor })?;
    let last = identity_first.checked_add(offset).ok_or(
        StatisticalLogRecordError::EvidenceBatchOutsideIdentityWindow {
            monitor,
            first: identity_first,
            last: u64::MAX,
            identity_last,
        },
    )?;
    if last > identity_last {
        return Err(
            StatisticalLogRecordError::EvidenceBatchOutsideIdentityWindow {
                monitor,
                first: identity_first,
                last,
                identity_last,
            },
        );
    }
    StatisticalBatchRange::try_new(last, last)
}

fn canonical_monitor_identity_order(
    left: &StatisticalLogRecord,
    right: &StatisticalLogRecord,
) -> Ordering {
    left.monitor_kind
        .cmp(&right.monitor_kind)
        .then_with(|| left.monitor_oid.cmp(&right.monitor_oid))
}

fn canonical_record_order(left: &StatisticalLogRecord, right: &StatisticalLogRecord) -> Ordering {
    left.batch
        .cmp(&right.batch)
        .then_with(|| left.monitor_kind.cmp(&right.monitor_kind))
        .then_with(|| left.monitor_oid.cmp(&right.monitor_oid))
        .then_with(|| left.evidence_oid.cmp(&right.evidence_oid))
        .then_with(|| {
            left.filtration_or_window_oid
                .cmp(&right.filtration_or_window_oid)
        })
        .then_with(|| left.identity_window.cmp(&right.identity_window))
        .then_with(|| left.regime_epoch.cmp(&right.regime_epoch))
        .then_with(|| {
            left.candidate_decision_oid
                .cmp(&right.candidate_decision_oid)
        })
        .then_with(|| left.pinned_fallback_oid.cmp(&right.pinned_fallback_oid))
        .then_with(|| left.selected_policy_oid.cmp(&right.selected_policy_oid))
        .then_with(|| left.statistic.cmp(&right.statistic))
}

fn validate_record_parts(
    monitor_kind: StatisticalMonitorKind,
    identity_window: StatisticalBatchRange,
    batch: StatisticalBatchRange,
    candidate_decision_oid: ObjectId,
    pinned_fallback_oid: ObjectId,
    selected_policy_oid: ObjectId,
    statistic: StatisticalStatistic,
) -> Result<(), StatisticalLogRecordError> {
    if candidate_decision_oid == pinned_fallback_oid {
        return Err(StatisticalLogRecordError::CandidateEqualsFallback);
    }
    if let StatisticalStatistic::AnnRecall {
        rebuild_policy_oid, ..
    } = statistic
    {
        if rebuild_policy_oid == candidate_decision_oid || rebuild_policy_oid == pinned_fallback_oid
        {
            return Err(StatisticalLogRecordError::AnnRecallRebuildPolicyCollision {
                rebuild: rebuild_policy_oid,
                candidate: candidate_decision_oid,
                fallback: pinned_fallback_oid,
            });
        }
        if selected_policy_oid != candidate_decision_oid
            && selected_policy_oid != pinned_fallback_oid
            && selected_policy_oid != rebuild_policy_oid
        {
            return Err(
                StatisticalLogRecordError::SelectedAnnRecallPolicyUnregistered {
                    selected: selected_policy_oid,
                },
            );
        }
    } else if selected_policy_oid != candidate_decision_oid
        && selected_policy_oid != pinned_fallback_oid
    {
        return Err(
            StatisticalLogRecordError::SelectedPolicyIsNeitherCandidateNorFallback {
                selected: selected_policy_oid,
                candidate: candidate_decision_oid,
                fallback: pinned_fallback_oid,
            },
        );
    }
    if monitor_kind.canonical_tag() != statistic.canonical_tag() {
        return Err(StatisticalLogRecordError::MonitorStatisticMismatch {
            monitor: monitor_kind,
            statistic_tag: statistic.canonical_tag(),
        });
    }
    statistic.validate()?;
    if batch.first < identity_window.first || batch.last > identity_window.last {
        return Err(StatisticalLogRecordError::BatchOutsideIdentityWindow {
            batch,
            identity_window,
        });
    }
    if let StatisticalStatistic::OffPolicyEvaluation {
        maximum_observations,
        maximum_actions_per_observation,
        observations,
        action_rows,
        complete,
        candidate_ess_gate_passed,
        fallback_ess_gate_passed,
        zero_support_exclusions,
        selection_reason,
        advantage_numerator,
        failure_behavior,
        ..
    } = statistic
    {
        let window_capacity = identity_window
            .last
            .checked_sub(identity_window.first)
            .and_then(|distance| distance.checked_add(1))
            .ok_or(StatisticalLogRecordError::OpeArithmeticOverflow)?;
        if maximum_observations < window_capacity {
            return Err(
                StatisticalLogRecordError::OpeProfileCannotContainIdentityWindow {
                    maximum_observations,
                    window_capacity,
                },
            );
        }
        let expected_complete = observations == window_capacity;
        if complete != expected_complete {
            return Err(StatisticalLogRecordError::OpeCompletenessMismatch {
                complete,
                observations,
                window_capacity,
            });
        }
        let expected_batch_sequence = identity_window
            .first
            .checked_add(observations - 1)
            .ok_or(StatisticalLogRecordError::OpeArithmeticOverflow)?;
        if batch.first != expected_batch_sequence || batch.last != expected_batch_sequence {
            return Err(StatisticalLogRecordError::OpeBatchSequenceMismatch {
                batch,
                expected: expected_batch_sequence,
            });
        }
        let maximum_rows_for_observations = observations
            .checked_mul(maximum_actions_per_observation)
            .ok_or(StatisticalLogRecordError::OpeArithmeticOverflow)?;
        if action_rows > maximum_rows_for_observations {
            return Err(
                StatisticalLogRecordError::OpeActionRowsExceedPerObservationProfile {
                    action_rows,
                    maximum: maximum_rows_for_observations,
                },
            );
        }
        let expected_reason = if !complete {
            OpeSelectionReason::IncompleteWindow
        } else if zero_support_exclusions != 0 {
            OpeSelectionReason::ZeroSupport
        } else if !candidate_ess_gate_passed || !fallback_ess_gate_passed {
            OpeSelectionReason::InsufficientEffectiveSampleSize
        } else if advantage_numerator <= 0 {
            OpeSelectionReason::CandidateNotBetter
        } else {
            OpeSelectionReason::CandidateEstimatedBetter
        };
        if selection_reason != expected_reason {
            return Err(StatisticalLogRecordError::OpeSelectionReasonMismatch {
                expected: expected_reason,
                actual: selection_reason,
            });
        }
        let candidate_selected = matches!(
            selection_reason,
            OpeSelectionReason::CandidateEstimatedBetter
        );
        let expected_selected_policy_oid = if candidate_selected {
            candidate_decision_oid
        } else {
            match failure_behavior {
                OpeFailureBehavior::SelectPinnedFallback => pinned_fallback_oid,
            }
        };
        if selected_policy_oid != expected_selected_policy_oid {
            return Err(StatisticalLogRecordError::OpeSelectionMismatch {
                reason: selection_reason,
                selected: selected_policy_oid,
                expected: expected_selected_policy_oid,
            });
        }
    }
    if let StatisticalStatistic::AnnRecall {
        rebuild_policy_oid,
        query_observations,
        maximum_queries,
        action,
        action_reason,
        ..
    } = statistic
    {
        let window_capacity = identity_window
            .last
            .checked_sub(identity_window.first)
            .and_then(|distance| distance.checked_add(1))
            .ok_or(StatisticalLogRecordError::AnnRecallArithmeticOverflow)?;
        if query_observations != window_capacity {
            return Err(
                StatisticalLogRecordError::AnnRecallObservationWindowMismatch {
                    observations: query_observations,
                    window_capacity,
                },
            );
        }
        if maximum_queries < window_capacity {
            return Err(
                StatisticalLogRecordError::AnnRecallProfileCannotContainIdentityWindow {
                    maximum_queries,
                    window_capacity,
                },
            );
        }
        if batch != identity_window {
            return Err(StatisticalLogRecordError::AnnRecallBatchMustEqualWindow {
                batch,
                identity_window,
            });
        }
        let expected_selected_policy_oid = match action {
            AnnRecallAction::Candidate => candidate_decision_oid,
            AnnRecallAction::PinnedFallback => pinned_fallback_oid,
            AnnRecallAction::Rebuild => rebuild_policy_oid,
        };
        if selected_policy_oid != expected_selected_policy_oid {
            return Err(StatisticalLogRecordError::AnnRecallSelectionMismatch {
                action,
                reason: action_reason,
                selected: selected_policy_oid,
                expected: expected_selected_policy_oid,
            });
        }
    }
    Ok(())
}

fn validate_ann_recall_statistic(
    statistic: StatisticalStatistic,
) -> Result<(), StatisticalLogRecordError> {
    let StatisticalStatistic::AnnRecall {
        profile_oid,
        top_k,
        maximum_queries,
        maximum_total_result_ids,
        confidence_exponent,
        candidate_recall_threshold_units,
        rebuild_recall_threshold_units,
        sample_design,
        exact_baseline_complete,
        authorization_domain_fixed,
        candidate_policy_fixed,
        query_observations,
        exact_baseline_results,
        candidate_results,
        intersection_hits,
        complete,
        exact_recall_intersection_hits,
        exact_recall_baseline_results,
        interval_scale,
        interval_point_estimate_units,
        interval_lower_units,
        interval_upper_units,
        interval_radius_units,
        interval_confidence_exponent,
        interval_query_observations,
        assumptions_supported,
        action,
        action_reason,
        ..
    } = statistic
    else {
        return Ok(());
    };

    if !complete {
        return Err(StatisticalLogRecordError::AnnRecallEvidenceMustBeTerminal);
    }
    let top_k_usize =
        usize::try_from(top_k).map_err(|_| StatisticalLogRecordError::InvalidAnnRecallProfile)?;
    let maximum_queries_usize = usize::try_from(maximum_queries)
        .map_err(|_| StatisticalLogRecordError::InvalidAnnRecallProfile)?;
    let maximum_total_result_ids_usize = usize::try_from(maximum_total_result_ids)
        .map_err(|_| StatisticalLogRecordError::InvalidAnnRecallProfile)?;
    let assumptions = AnnRecallAssumptions::new(
        sample_design,
        exact_baseline_complete,
        authorization_domain_fixed,
        candidate_policy_fixed,
    );
    if AnnRecallProfile::try_new(
        profile_oid,
        top_k_usize,
        maximum_queries_usize,
        maximum_total_result_ids_usize,
        confidence_exponent,
        candidate_recall_threshold_units,
        rebuild_recall_threshold_units,
        assumptions,
    )
    .is_err()
    {
        return Err(StatisticalLogRecordError::InvalidAnnRecallProfile);
    }
    if query_observations == 0 {
        return Err(StatisticalLogRecordError::EvidenceHasNoObservations {
            monitor: StatisticalMonitorKind::AnnRecall,
        });
    }
    let expected_results = query_observations
        .checked_mul(top_k)
        .ok_or(StatisticalLogRecordError::AnnRecallArithmeticOverflow)?;
    validate_ann_derived_field(
        StatisticField::AnnExactBaselineResults,
        exact_baseline_results,
        expected_results,
    )?;
    validate_ann_derived_field(
        StatisticField::AnnCandidateResults,
        candidate_results,
        expected_results,
    )?;
    validate_ann_derived_field(
        StatisticField::AnnExactRecallIntersectionHits,
        exact_recall_intersection_hits,
        intersection_hits,
    )?;
    validate_ann_derived_field(
        StatisticField::AnnExactRecallBaselineResults,
        exact_recall_baseline_results,
        exact_baseline_results,
    )?;
    if intersection_hits > exact_baseline_results || intersection_hits > candidate_results {
        return Err(StatisticalLogRecordError::SubcountExceedsTotal {
            field: StatisticField::AnnIntersectionHits,
            subcount: intersection_hits,
            total: exact_baseline_results.min(candidate_results),
        });
    }
    let total_result_ids = exact_baseline_results
        .checked_add(candidate_results)
        .ok_or(StatisticalLogRecordError::AnnRecallArithmeticOverflow)?;
    if total_result_ids > maximum_total_result_ids {
        return Err(
            StatisticalLogRecordError::AnnRecallResultInventoryExceedsProfile {
                actual: total_result_ids,
                maximum: maximum_total_result_ids,
            },
        );
    }
    if query_observations > maximum_queries {
        return Err(
            StatisticalLogRecordError::AnnRecallProfileCannotContainIdentityWindow {
                maximum_queries,
                window_capacity: query_observations,
            },
        );
    }

    let expected_interval = recompute_ann_recall_interval(
        intersection_hits,
        exact_baseline_results,
        query_observations,
        confidence_exponent,
    )?;
    for (field, actual, expected) in [
        (
            StatisticField::AnnIntervalScale,
            interval_scale,
            expected_interval.scale,
        ),
        (
            StatisticField::AnnIntervalPointEstimate,
            interval_point_estimate_units,
            expected_interval.point_estimate_units,
        ),
        (
            StatisticField::AnnIntervalLower,
            interval_lower_units,
            expected_interval.lower_units,
        ),
        (
            StatisticField::AnnIntervalUpper,
            interval_upper_units,
            expected_interval.upper_units,
        ),
        (
            StatisticField::AnnIntervalRadius,
            interval_radius_units,
            expected_interval.radius_units,
        ),
        (
            StatisticField::AnnIntervalConfidenceExponent,
            u64::from(interval_confidence_exponent),
            u64::from(expected_interval.confidence_exponent),
        ),
        (
            StatisticField::AnnIntervalQueryObservations,
            interval_query_observations,
            expected_interval.query_observations,
        ),
    ] {
        validate_ann_derived_field(field, actual, expected)?;
    }

    let expected_assumptions_supported = assumptions.supported();
    if assumptions_supported ^ expected_assumptions_supported {
        return Err(
            StatisticalLogRecordError::AnnRecallAssumptionSupportMismatch {
                expected: expected_assumptions_supported,
                actual: assumptions_supported,
            },
        );
    }
    let (expected_action, expected_reason) = if !assumptions_supported {
        (
            AnnRecallAction::PinnedFallback,
            AnnRecallActionReason::UnsupportedAssumptions,
        )
    } else if interval_upper_units < rebuild_recall_threshold_units {
        (
            AnnRecallAction::Rebuild,
            AnnRecallActionReason::RecallDriftDetected,
        )
    } else if interval_lower_units >= candidate_recall_threshold_units {
        (
            AnnRecallAction::Candidate,
            AnnRecallActionReason::CandidateRecallSatisfied,
        )
    } else {
        (
            AnnRecallAction::PinnedFallback,
            AnnRecallActionReason::StatisticallyInconclusive,
        )
    };
    if action != expected_action || action_reason != expected_reason {
        return Err(StatisticalLogRecordError::AnnRecallActionReasonMismatch {
            expected_action,
            actual_action: action,
            expected_reason,
            actual_reason: action_reason,
        });
    }
    Ok(())
}

fn validate_ann_derived_field(
    field: StatisticField,
    actual: u64,
    expected: u64,
) -> Result<(), StatisticalLogRecordError> {
    if actual != expected {
        return Err(StatisticalLogRecordError::AnnRecallDerivedFieldMismatch {
            field,
            expected,
            actual,
        });
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct RecomputedAnnRecallInterval {
    scale: u64,
    point_estimate_units: u64,
    lower_units: u64,
    upper_units: u64,
    radius_units: u64,
    confidence_exponent: u8,
    query_observations: u64,
}

fn recompute_ann_recall_interval(
    hits: u64,
    baseline_results: u64,
    queries: u64,
    confidence_exponent: u8,
) -> Result<RecomputedAnnRecallInterval, StatisticalLogRecordError> {
    let scaled_hits = u128::from(hits)
        .checked_mul(u128::from(RECALL_SCALE))
        .ok_or(StatisticalLogRecordError::AnnRecallArithmeticOverflow)?;
    let denominator = u128::from(baseline_results);
    if denominator == 0 || queries == 0 {
        return Err(StatisticalLogRecordError::EvidenceHasNoObservations {
            monitor: StatisticalMonitorKind::AnnRecall,
        });
    }
    let point_floor = scaled_hits / denominator;
    let point_ceil = ann_ceil_div(scaled_hits, denominator)?;
    let radius_numerator = u128::from(confidence_exponent)
        .checked_add(1)
        .and_then(|factor| factor.checked_mul(u128::from(RECALL_SCALE)))
        .and_then(|value| value.checked_mul(u128::from(RECALL_SCALE)))
        .ok_or(StatisticalLogRecordError::AnnRecallArithmeticOverflow)?;
    let radius_denominator = u128::from(queries)
        .checked_mul(2)
        .ok_or(StatisticalLogRecordError::AnnRecallArithmeticOverflow)?;
    let squared_radius_ceiling = ann_ceil_div(radius_numerator, radius_denominator)?;
    let radius = ann_ceil_sqrt(squared_radius_ceiling).min(u128::from(RECALL_SCALE));
    let lower = point_floor.saturating_sub(radius);
    let upper = point_ceil
        .checked_add(radius)
        .ok_or(StatisticalLogRecordError::AnnRecallArithmeticOverflow)?
        .min(u128::from(RECALL_SCALE));

    Ok(RecomputedAnnRecallInterval {
        scale: RECALL_SCALE,
        point_estimate_units: u64::try_from(point_floor)
            .map_err(|_| StatisticalLogRecordError::AnnRecallArithmeticOverflow)?,
        lower_units: u64::try_from(lower)
            .map_err(|_| StatisticalLogRecordError::AnnRecallArithmeticOverflow)?,
        upper_units: u64::try_from(upper)
            .map_err(|_| StatisticalLogRecordError::AnnRecallArithmeticOverflow)?,
        radius_units: u64::try_from(radius)
            .map_err(|_| StatisticalLogRecordError::AnnRecallArithmeticOverflow)?,
        confidence_exponent,
        query_observations: queries,
    })
}

fn ann_ceil_div(numerator: u128, denominator: u128) -> Result<u128, StatisticalLogRecordError> {
    let adjusted = numerator
        .checked_add(
            denominator
                .checked_sub(1)
                .ok_or(StatisticalLogRecordError::AnnRecallArithmeticOverflow)?,
        )
        .ok_or(StatisticalLogRecordError::AnnRecallArithmeticOverflow)?;
    Ok(adjusted / denominator)
}

fn ann_ceil_sqrt(value: u128) -> u128 {
    if value <= 1 {
        return value;
    }
    let mut low = 1_u128;
    let mut high = value.min(u128::from(u64::MAX) + 1);
    while low < high {
        let midpoint = low + (high - low) / 2;
        if midpoint > value / midpoint {
            high = midpoint;
        } else if midpoint * midpoint == value {
            return midpoint;
        } else {
            low = midpoint + 1;
        }
    }
    low
}

#[allow(clippy::too_many_arguments)]
fn encode_monitor_identity_body(
    monitor_kind: StatisticalMonitorKind,
    source_monitor_oid: ObjectId,
    filtration_or_window_oid: ObjectId,
    identity_window: StatisticalBatchRange,
    regime_epoch: u64,
    candidate_decision_oid: ObjectId,
    pinned_fallback_oid: ObjectId,
    statistic: StatisticalStatistic,
) -> Result<Vec<u8>, StatisticalLogRecordError> {
    let stable_payload_len = stable_statistic_identity_len(statistic);
    let requested = MONITOR_IDENTITY_BODY_DOMAIN
        .len()
        .checked_add(2 + 2 + 3 + 64 + 16 + 8 + 64 + 2)
        .and_then(|fixed| fixed.checked_add(stable_payload_len))
        .ok_or(StatisticalLogRecordError::EvidenceIdentityLengthOverflow)?;
    let mut body = Vec::new();
    body.try_reserve_exact(requested)
        .map_err(|_| StatisticalLogRecordError::EvidenceIdentityAllocationFailed { requested })?;
    let domain_len = u16::try_from(MONITOR_IDENTITY_BODY_DOMAIN.len())
        .map_err(|_| StatisticalLogRecordError::EvidenceIdentityLengthOverflow)?;
    push_u16(&mut body, domain_len);
    body.extend_from_slice(MONITOR_IDENTITY_BODY_DOMAIN);
    push_u16(&mut body, STATISTICAL_LOG_RECORD_VERSION);
    body.push(STATISTICAL_CLAIM_TAG);
    body.push(monitor_kind.canonical_tag());
    body.push(statistic.canonical_tag());
    push_oid(&mut body, source_monitor_oid);
    push_oid(&mut body, filtration_or_window_oid);
    push_u64(&mut body, identity_window.first);
    push_u64(&mut body, identity_window.last);
    push_u64(&mut body, regime_epoch);
    push_oid(&mut body, candidate_decision_oid);
    push_oid(&mut body, pinned_fallback_oid);
    let stable_payload_len = u16::try_from(stable_payload_len)
        .map_err(|_| StatisticalLogRecordError::EvidenceIdentityLengthOverflow)?;
    push_u16(&mut body, stable_payload_len);
    encode_stable_statistic_identity(&mut body, statistic);
    Ok(body)
}

#[allow(clippy::too_many_arguments)]
fn encode_evidence_body(
    monitor_kind: StatisticalMonitorKind,
    source_monitor_oid: ObjectId,
    monitor_oid: ObjectId,
    filtration_or_window_oid: ObjectId,
    identity_window: StatisticalBatchRange,
    batch: StatisticalBatchRange,
    regime_epoch: u64,
    candidate_decision_oid: ObjectId,
    pinned_fallback_oid: ObjectId,
    selected_policy_oid: ObjectId,
    statistic: StatisticalStatistic,
) -> Result<Vec<u8>, StatisticalLogRecordError> {
    let requested = EVIDENCE_BODY_DOMAIN
        .len()
        .checked_add(2 + 2 + 3 + 96 + 16 + 16 + 8 + 32 + 32 + 32 + 2)
        .and_then(|fixed| fixed.checked_add(statistic.payload_len()))
        .ok_or(StatisticalLogRecordError::EvidenceIdentityLengthOverflow)?;
    let mut body = Vec::new();
    body.try_reserve_exact(requested)
        .map_err(|_| StatisticalLogRecordError::EvidenceIdentityAllocationFailed { requested })?;
    let domain_len = u16::try_from(EVIDENCE_BODY_DOMAIN.len())
        .map_err(|_| StatisticalLogRecordError::EvidenceIdentityLengthOverflow)?;
    push_u16(&mut body, domain_len);
    body.extend_from_slice(EVIDENCE_BODY_DOMAIN);
    push_u16(&mut body, STATISTICAL_LOG_RECORD_VERSION);
    body.push(STATISTICAL_CLAIM_TAG);
    body.push(monitor_kind.canonical_tag());
    body.push(statistic.canonical_tag());
    push_oid(&mut body, source_monitor_oid);
    push_oid(&mut body, monitor_oid);
    push_oid(&mut body, filtration_or_window_oid);
    push_u64(&mut body, identity_window.first);
    push_u64(&mut body, identity_window.last);
    push_u64(&mut body, batch.first);
    push_u64(&mut body, batch.last);
    push_u64(&mut body, regime_epoch);
    push_oid(&mut body, candidate_decision_oid);
    push_oid(&mut body, pinned_fallback_oid);
    push_oid(&mut body, selected_policy_oid);
    let payload_len = u16::try_from(statistic.payload_len())
        .map_err(|_| StatisticalLogRecordError::EvidenceIdentityLengthOverflow)?;
    push_u16(&mut body, payload_len);
    encode_statistic(&mut body, statistic);
    Ok(body)
}

const fn stable_statistic_identity_len(statistic: StatisticalStatistic) -> usize {
    match statistic {
        StatisticalStatistic::EProcess { .. } => 64,
        StatisticalStatistic::ConformalCoverage { .. } => 121,
        StatisticalStatistic::ExplorationBudget { .. } => 16,
        StatisticalStatistic::DrainProgress { .. } => 0,
        StatisticalStatistic::RegimeChange { .. } => 8,
        StatisticalStatistic::OffPolicyEvaluation { .. } => 202,
        StatisticalStatistic::AnnRecall { .. } => 301,
    }
}

fn encode_stable_statistic_identity(bytes: &mut Vec<u8>, statistic: StatisticalStatistic) {
    match statistic {
        StatisticalStatistic::EProcess {
            profile_oid,
            p0_bits,
            lambda_bits,
            alpha_bits,
            max_evalue_bits,
            ..
        } => {
            push_oid(bytes, profile_oid);
            push_u64(bytes, p0_bits);
            push_u64(bytes, lambda_bits);
            push_u64(bytes, alpha_bits);
            push_u64(bytes, max_evalue_bits);
        }
        StatisticalStatistic::ConformalCoverage {
            profile_oid,
            population_oid,
            selection_oid,
            alpha_bits,
            mode,
            minimum_calibration_samples,
            maximum_calibration_samples,
            ..
        } => {
            push_oid(bytes, profile_oid);
            push_oid(bytes, population_oid);
            push_oid(bytes, selection_oid);
            push_u64(bytes, alpha_bits);
            bytes.push(mode as u8);
            push_u64(bytes, minimum_calibration_samples);
            push_u64(bytes, maximum_calibration_samples);
        }
        StatisticalStatistic::ExplorationBudget {
            alpha_bits,
            target_rate_bits,
            ..
        } => {
            push_u64(bytes, alpha_bits);
            push_u64(bytes, target_rate_bits);
        }
        StatisticalStatistic::DrainProgress { .. } => {}
        StatisticalStatistic::RegimeChange { threshold, .. } => push_i64(bytes, threshold),
        StatisticalStatistic::OffPolicyEvaluation {
            population_oid,
            strata_oid,
            action_space_oid,
            policy_epoch_oid,
            estimator_oid,
            estimator,
            failure_behavior,
            clipping_weight_units,
            minimum_effective_sample_size,
            maximum_observations,
            maximum_actions_per_observation,
            maximum_total_action_rows,
            ..
        } => {
            push_oid(bytes, population_oid);
            push_oid(bytes, strata_oid);
            push_oid(bytes, action_space_oid);
            push_oid(bytes, policy_epoch_oid);
            push_oid(bytes, estimator_oid);
            bytes.push(estimator as u8);
            bytes.push(failure_behavior as u8);
            push_u64(bytes, clipping_weight_units);
            push_u64(bytes, minimum_effective_sample_size);
            push_u64(bytes, maximum_observations);
            push_u64(bytes, maximum_actions_per_observation);
            push_u64(bytes, maximum_total_action_rows);
        }
        StatisticalStatistic::AnnRecall {
            profile_oid,
            authorized_population_oid,
            snapshot_oid,
            authority_domain_oid,
            sample_key_oid,
            sample_design_oid,
            exact_baseline_oid,
            rebuild_policy_oid,
            top_k,
            maximum_queries,
            maximum_total_result_ids,
            confidence_exponent,
            candidate_recall_threshold_units,
            rebuild_recall_threshold_units,
            sample_design,
            exact_baseline_complete,
            authorization_domain_fixed,
            candidate_policy_fixed,
            ..
        } => {
            push_oid(bytes, profile_oid);
            push_oid(bytes, authorized_population_oid);
            push_oid(bytes, snapshot_oid);
            push_oid(bytes, authority_domain_oid);
            push_oid(bytes, sample_key_oid);
            push_oid(bytes, sample_design_oid);
            push_oid(bytes, exact_baseline_oid);
            push_oid(bytes, rebuild_policy_oid);
            push_u64(bytes, top_k);
            push_u64(bytes, maximum_queries);
            push_u64(bytes, maximum_total_result_ids);
            bytes.push(confidence_exponent);
            push_u64(bytes, candidate_recall_threshold_units);
            push_u64(bytes, rebuild_recall_threshold_units);
            bytes.push(sample_design as u8);
            bytes.push(u8::from(exact_baseline_complete));
            bytes.push(u8::from(authorization_domain_fixed));
            bytes.push(u8::from(candidate_policy_fixed));
        }
    }
}

fn digest_evidence_body(body: &[u8]) -> StatisticalEvidenceDigest {
    StatisticalEvidenceDigest(asupersync::atp::object::compute_hash(body))
}

fn empty_ope_support_exclusions_digest() -> StatisticalEvidenceDigest {
    StatisticalEvidenceDigest(asupersync::atp::object::compute_hash(
        OPE_SUPPORT_EXCLUSIONS_DOMAIN,
    ))
}

fn digest_ope_support_exclusions(
    exclusions: &[crate::ope::ZeroSupportExclusion],
) -> Result<StatisticalEvidenceDigest, StatisticalLogRecordError> {
    let mut digest = empty_ope_support_exclusions_digest();
    let requested = OPE_SUPPORT_EXCLUSIONS_DOMAIN
        .len()
        .checked_add(32 + 8 + 128 + 1 + 8)
        .ok_or(StatisticalLogRecordError::EvidenceIdentityLengthOverflow)?;
    let mut frame = Vec::new();
    frame
        .try_reserve_exact(requested)
        .map_err(|_| StatisticalLogRecordError::EvidenceIdentityAllocationFailed { requested })?;
    for exclusion in exclusions {
        frame.clear();
        frame.extend_from_slice(OPE_SUPPORT_EXCLUSIONS_DOMAIN);
        frame.extend_from_slice(digest.as_bytes());
        push_u64(&mut frame, exclusion.sequence());
        push_oid(&mut frame, exclusion.state_oid());
        push_oid(&mut frame, exclusion.features_oid());
        push_oid(&mut frame, exclusion.stratum_oid());
        push_oid(&mut frame, exclusion.action_oid());
        frame.push(match exclusion.affected_policy() {
            EvaluatedPolicy::Candidate => 1,
            EvaluatedPolicy::Fallback => 2,
        });
        push_u64(&mut frame, exclusion.unsupported_probability().numerator());
        digest = StatisticalEvidenceDigest(asupersync::atp::object::compute_hash(&frame));
    }
    Ok(digest)
}

fn evidence_digests_match(
    left: StatisticalEvidenceDigest,
    right: StatisticalEvidenceDigest,
) -> bool {
    let mut difference = 0_u8;
    for (left_byte, right_byte) in left.0.iter().zip(right.0.iter()) {
        difference |= left_byte ^ right_byte;
    }
    difference == 0
}

fn validate_ope_ess(
    policy: EvaluatedPolicy,
    numerator: u128,
    denominator: u128,
    observations: u64,
    minimum: u64,
    encoded_gate: bool,
) -> Result<(), StatisticalLogRecordError> {
    if denominator == 0 && numerator != 0 {
        return Err(
            StatisticalLogRecordError::EffectiveSampleSizeDenominatorMismatch {
                policy,
                numerator,
                denominator,
            },
        );
    }
    if denominator != 0 {
        let maximum_numerator = denominator
            .checked_mul(u128::from(observations))
            .ok_or(StatisticalLogRecordError::OpeArithmeticOverflow)?;
        if numerator > maximum_numerator {
            return Err(
                StatisticalLogRecordError::EffectiveSampleSizeExceedsObservations {
                    policy,
                    numerator,
                    denominator,
                    observations,
                },
            );
        }
    }
    let expected_gate = denominator != 0
        && denominator
            .checked_mul(u128::from(minimum))
            .is_some_and(|required| numerator >= required);
    if encoded_gate != expected_gate {
        return Err(StatisticalLogRecordError::OpeEssGateMismatch {
            policy,
            expected: expected_gate,
            actual: encoded_gate,
        });
    }
    Ok(())
}

fn validate_canonical_float(
    field: StatisticField,
    bits: u64,
) -> Result<f64, StatisticalLogRecordError> {
    let value = f64::from_bits(bits);
    if value.is_nan() || (value == 0.0 && bits != 0.0_f64.to_bits()) {
        return Err(StatisticalLogRecordError::NonCanonicalFloatBits { field, bits });
    }
    Ok(value)
}

fn validate_nonnegative_finite(
    field: StatisticField,
    bits: u64,
) -> Result<(), StatisticalLogRecordError> {
    let value = validate_canonical_float(field, bits)?;
    if !value.is_finite() || value < 0.0 {
        return Err(StatisticalLogRecordError::StatisticMustBeNonNegative { field, bits });
    }
    Ok(())
}

fn validate_positive_finite(
    field: StatisticField,
    bits: u64,
) -> Result<(), StatisticalLogRecordError> {
    let value = validate_canonical_float(field, bits)?;
    if !value.is_finite() || value <= 0.0 {
        return Err(StatisticalLogRecordError::StatisticMustBePositive { field, bits });
    }
    Ok(())
}

fn validate_nonnegative_or_positive_infinity(
    field: StatisticField,
    bits: u64,
) -> Result<(), StatisticalLogRecordError> {
    let value = validate_canonical_float(field, bits)?;
    if value < 0.0 {
        return Err(StatisticalLogRecordError::StatisticMustBeNonNegative { field, bits });
    }
    Ok(())
}

fn validate_finite_or_positive_infinity(
    field: StatisticField,
    bits: u64,
) -> Result<(), StatisticalLogRecordError> {
    let value = validate_canonical_float(field, bits)?;
    if !value.is_finite() && value != f64::INFINITY {
        return Err(
            StatisticalLogRecordError::StatisticMustBeFiniteOrPositiveInfinity { field, bits },
        );
    }
    Ok(())
}

fn validate_unit_interval(
    field: StatisticField,
    bits: u64,
) -> Result<(), StatisticalLogRecordError> {
    let value = validate_canonical_float(field, bits)?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(StatisticalLogRecordError::StatisticOutsideUnitInterval { field, bits });
    }
    Ok(())
}

fn validate_subcount(
    field: StatisticField,
    subcount: u64,
    total: u64,
) -> Result<(), StatisticalLogRecordError> {
    if subcount > total {
        return Err(StatisticalLogRecordError::SubcountExceedsTotal {
            field,
            subcount,
            total,
        });
    }
    Ok(())
}

fn encode_statistic(bytes: &mut Vec<u8>, statistic: StatisticalStatistic) {
    match statistic {
        StatisticalStatistic::EProcess {
            profile_oid,
            p0_bits,
            lambda_bits,
            alpha_bits,
            max_evalue_bits,
            e_value_bits,
            rejection_threshold_bits,
            observations,
            one_observations,
        } => {
            push_oid(bytes, profile_oid);
            push_u64(bytes, p0_bits);
            push_u64(bytes, lambda_bits);
            push_u64(bytes, alpha_bits);
            push_u64(bytes, max_evalue_bits);
            push_u64(bytes, e_value_bits);
            push_u64(bytes, rejection_threshold_bits);
            push_u64(bytes, observations);
            push_u64(bytes, one_observations);
        }
        StatisticalStatistic::ConformalCoverage {
            profile_oid,
            population_oid,
            selection_oid,
            alpha_bits,
            mode,
            minimum_calibration_samples,
            maximum_calibration_samples,
            threshold_bits,
            nonconformity_score_bits,
            coverage_target_bits,
            assessments,
            covered,
        } => {
            push_oid(bytes, profile_oid);
            push_oid(bytes, population_oid);
            push_oid(bytes, selection_oid);
            push_u64(bytes, alpha_bits);
            bytes.push(mode as u8);
            push_u64(bytes, minimum_calibration_samples);
            push_u64(bytes, maximum_calibration_samples);
            push_u64(bytes, threshold_bits);
            push_u64(bytes, nonconformity_score_bits);
            push_u64(bytes, coverage_target_bits);
            push_u64(bytes, assessments);
            push_u64(bytes, covered);
        }
        StatisticalStatistic::ExplorationBudget {
            alpha_bits,
            residual_rate_bits,
            upper_bound_bits,
            target_rate_bits,
            total_runs,
            discoveries,
            recommended_additional_runs,
        } => {
            push_u64(bytes, alpha_bits);
            push_u64(bytes, residual_rate_bits);
            push_u64(bytes, upper_bound_bits);
            push_u64(bytes, target_rate_bits);
            push_u64(bytes, total_runs);
            push_u64(bytes, discoveries);
            push_u64(bytes, recommended_additional_runs);
        }
        StatisticalStatistic::DrainProgress {
            current_potential_bits,
            confidence_bound_bits,
            observations,
            stall_detected,
        } => {
            push_u64(bytes, current_potential_bits);
            push_u64(bytes, confidence_bound_bits);
            push_u64(bytes, observations);
            bytes.push(u8::from(stall_detected));
        }
        StatisticalStatistic::RegimeChange {
            statistic,
            threshold,
            observations,
            detections,
        } => {
            push_i64(bytes, statistic);
            push_i64(bytes, threshold);
            push_u64(bytes, observations);
            push_u64(bytes, detections);
        }
        StatisticalStatistic::OffPolicyEvaluation {
            population_oid,
            strata_oid,
            action_space_oid,
            policy_epoch_oid,
            estimator_oid,
            estimator,
            failure_behavior,
            clipping_weight_units,
            minimum_effective_sample_size,
            maximum_observations,
            maximum_actions_per_observation,
            maximum_total_action_rows,
            candidate_numerator,
            fallback_numerator,
            advantage_numerator,
            common_denominator,
            candidate_ess_numerator,
            candidate_ess_denominator,
            fallback_ess_numerator,
            fallback_ess_denominator,
            observations,
            action_rows,
            complete,
            candidate_ess_gate_passed,
            fallback_ess_gate_passed,
            zero_support_exclusions,
            zero_support_exclusions_digest,
            selection_reason,
        } => {
            push_oid(bytes, population_oid);
            push_oid(bytes, strata_oid);
            push_oid(bytes, action_space_oid);
            push_oid(bytes, policy_epoch_oid);
            push_oid(bytes, estimator_oid);
            bytes.push(estimator as u8);
            bytes.push(failure_behavior as u8);
            push_u64(bytes, clipping_weight_units);
            push_u64(bytes, minimum_effective_sample_size);
            push_u64(bytes, maximum_observations);
            push_u64(bytes, maximum_actions_per_observation);
            push_u64(bytes, maximum_total_action_rows);
            push_i128(bytes, candidate_numerator);
            push_i128(bytes, fallback_numerator);
            push_i128(bytes, advantage_numerator);
            push_u128(bytes, common_denominator);
            push_u128(bytes, candidate_ess_numerator);
            push_u128(bytes, candidate_ess_denominator);
            push_u128(bytes, fallback_ess_numerator);
            push_u128(bytes, fallback_ess_denominator);
            push_u64(bytes, observations);
            push_u64(bytes, action_rows);
            bytes.push(u8::from(complete));
            bytes.push(u8::from(candidate_ess_gate_passed));
            bytes.push(u8::from(fallback_ess_gate_passed));
            push_u64(bytes, zero_support_exclusions);
            bytes.extend_from_slice(zero_support_exclusions_digest.as_bytes());
            bytes.push(selection_reason as u8);
        }
        StatisticalStatistic::AnnRecall {
            profile_oid,
            authorized_population_oid,
            snapshot_oid,
            authority_domain_oid,
            sample_key_oid,
            sample_design_oid,
            exact_baseline_oid,
            rebuild_policy_oid,
            top_k,
            maximum_queries,
            maximum_total_result_ids,
            confidence_exponent,
            candidate_recall_threshold_units,
            rebuild_recall_threshold_units,
            sample_design,
            exact_baseline_complete,
            authorization_domain_fixed,
            candidate_policy_fixed,
            query_observations,
            exact_baseline_results,
            candidate_results,
            intersection_hits,
            complete,
            exact_recall_intersection_hits,
            exact_recall_baseline_results,
            interval_scale,
            interval_point_estimate_units,
            interval_lower_units,
            interval_upper_units,
            interval_radius_units,
            interval_confidence_exponent,
            interval_query_observations,
            assumptions_supported,
            action,
            action_reason,
        } => {
            push_oid(bytes, profile_oid);
            push_oid(bytes, authorized_population_oid);
            push_oid(bytes, snapshot_oid);
            push_oid(bytes, authority_domain_oid);
            push_oid(bytes, sample_key_oid);
            push_oid(bytes, sample_design_oid);
            push_oid(bytes, exact_baseline_oid);
            push_oid(bytes, rebuild_policy_oid);
            push_u64(bytes, top_k);
            push_u64(bytes, maximum_queries);
            push_u64(bytes, maximum_total_result_ids);
            bytes.push(confidence_exponent);
            push_u64(bytes, candidate_recall_threshold_units);
            push_u64(bytes, rebuild_recall_threshold_units);
            bytes.push(sample_design as u8);
            bytes.push(u8::from(exact_baseline_complete));
            bytes.push(u8::from(authorization_domain_fixed));
            bytes.push(u8::from(candidate_policy_fixed));
            push_u64(bytes, query_observations);
            push_u64(bytes, exact_baseline_results);
            push_u64(bytes, candidate_results);
            push_u64(bytes, intersection_hits);
            bytes.push(u8::from(complete));
            push_u64(bytes, exact_recall_intersection_hits);
            push_u64(bytes, exact_recall_baseline_results);
            push_u64(bytes, interval_scale);
            push_u64(bytes, interval_point_estimate_units);
            push_u64(bytes, interval_lower_units);
            push_u64(bytes, interval_upper_units);
            push_u64(bytes, interval_radius_units);
            bytes.push(interval_confidence_exponent);
            push_u64(bytes, interval_query_observations);
            bytes.push(u8::from(assumptions_supported));
            bytes.push(action as u8);
            bytes.push(action_reason as u8);
        }
    }
}

fn decode_statistic(
    tag: u8,
    payload: &[u8],
) -> Result<StatisticalStatistic, StatisticalLogCodecError> {
    let expected = match tag {
        1 => 96,
        2 => 161,
        3 => 56,
        4 => 25,
        5 => 32,
        6 => 390,
        7 => 402,
        _ => return Err(StatisticalLogCodecError::UnknownStatisticKind { tag }),
    };
    if payload.len() != expected {
        return Err(StatisticalLogCodecError::StatisticPayloadLengthMismatch {
            statistic_tag: tag,
            actual: payload.len(),
            expected,
        });
    }
    let mut decoder = Decoder::new(payload);
    let statistic = match tag {
        1 => StatisticalStatistic::EProcess {
            profile_oid: decoder.read_oid()?,
            p0_bits: decoder.read_u64()?,
            lambda_bits: decoder.read_u64()?,
            alpha_bits: decoder.read_u64()?,
            max_evalue_bits: decoder.read_u64()?,
            e_value_bits: decoder.read_u64()?,
            rejection_threshold_bits: decoder.read_u64()?,
            observations: decoder.read_u64()?,
            one_observations: decoder.read_u64()?,
        },
        2 => StatisticalStatistic::ConformalCoverage {
            profile_oid: decoder.read_oid()?,
            population_oid: decoder.read_oid()?,
            selection_oid: decoder.read_oid()?,
            alpha_bits: decoder.read_u64()?,
            mode: StatisticalConformalMode::try_from_tag(decoder.read_u8()?)?,
            minimum_calibration_samples: decoder.read_u64()?,
            maximum_calibration_samples: decoder.read_u64()?,
            threshold_bits: decoder.read_u64()?,
            nonconformity_score_bits: decoder.read_u64()?,
            coverage_target_bits: decoder.read_u64()?,
            assessments: decoder.read_u64()?,
            covered: decoder.read_u64()?,
        },
        3 => StatisticalStatistic::ExplorationBudget {
            alpha_bits: decoder.read_u64()?,
            residual_rate_bits: decoder.read_u64()?,
            upper_bound_bits: decoder.read_u64()?,
            target_rate_bits: decoder.read_u64()?,
            total_runs: decoder.read_u64()?,
            discoveries: decoder.read_u64()?,
            recommended_additional_runs: decoder.read_u64()?,
        },
        4 => StatisticalStatistic::DrainProgress {
            current_potential_bits: decoder.read_u64()?,
            confidence_bound_bits: decoder.read_u64()?,
            observations: decoder.read_u64()?,
            stall_detected: decoder.read_bool(4, "stall_detected")?,
        },
        5 => StatisticalStatistic::RegimeChange {
            statistic: decoder.read_i64()?,
            threshold: decoder.read_i64()?,
            observations: decoder.read_u64()?,
            detections: decoder.read_u64()?,
        },
        6 => StatisticalStatistic::OffPolicyEvaluation {
            population_oid: decoder.read_oid()?,
            strata_oid: decoder.read_oid()?,
            action_space_oid: decoder.read_oid()?,
            policy_epoch_oid: decoder.read_oid()?,
            estimator_oid: decoder.read_oid()?,
            estimator: decode_ope_estimator(decoder.read_u8()?)?,
            failure_behavior: decode_ope_failure_behavior(decoder.read_u8()?)?,
            clipping_weight_units: decoder.read_u64()?,
            minimum_effective_sample_size: decoder.read_u64()?,
            maximum_observations: decoder.read_u64()?,
            maximum_actions_per_observation: decoder.read_u64()?,
            maximum_total_action_rows: decoder.read_u64()?,
            candidate_numerator: decoder.read_i128()?,
            fallback_numerator: decoder.read_i128()?,
            advantage_numerator: decoder.read_i128()?,
            common_denominator: decoder.read_u128()?,
            candidate_ess_numerator: decoder.read_u128()?,
            candidate_ess_denominator: decoder.read_u128()?,
            fallback_ess_numerator: decoder.read_u128()?,
            fallback_ess_denominator: decoder.read_u128()?,
            observations: decoder.read_u64()?,
            action_rows: decoder.read_u64()?,
            complete: decoder.read_bool(6, "complete")?,
            candidate_ess_gate_passed: decoder.read_bool(6, "candidate_ess_gate_passed")?,
            fallback_ess_gate_passed: decoder.read_bool(6, "fallback_ess_gate_passed")?,
            zero_support_exclusions: decoder.read_u64()?,
            zero_support_exclusions_digest: StatisticalEvidenceDigest(decoder.read_array::<32>()?),
            selection_reason: decode_ope_selection_reason(decoder.read_u8()?)?,
        },
        7 => StatisticalStatistic::AnnRecall {
            profile_oid: decoder.read_oid()?,
            authorized_population_oid: decoder.read_oid()?,
            snapshot_oid: decoder.read_oid()?,
            authority_domain_oid: decoder.read_oid()?,
            sample_key_oid: decoder.read_oid()?,
            sample_design_oid: decoder.read_oid()?,
            exact_baseline_oid: decoder.read_oid()?,
            rebuild_policy_oid: decoder.read_oid()?,
            top_k: decoder.read_u64()?,
            maximum_queries: decoder.read_u64()?,
            maximum_total_result_ids: decoder.read_u64()?,
            confidence_exponent: decoder.read_u8()?,
            candidate_recall_threshold_units: decoder.read_u64()?,
            rebuild_recall_threshold_units: decoder.read_u64()?,
            sample_design: decode_ann_sample_design(decoder.read_u8()?)?,
            exact_baseline_complete: decoder.read_bool(7, "exact_baseline_complete")?,
            authorization_domain_fixed: decoder.read_bool(7, "authorization_domain_fixed")?,
            candidate_policy_fixed: decoder.read_bool(7, "candidate_policy_fixed")?,
            query_observations: decoder.read_u64()?,
            exact_baseline_results: decoder.read_u64()?,
            candidate_results: decoder.read_u64()?,
            intersection_hits: decoder.read_u64()?,
            complete: decoder.read_bool(7, "complete")?,
            exact_recall_intersection_hits: decoder.read_u64()?,
            exact_recall_baseline_results: decoder.read_u64()?,
            interval_scale: decoder.read_u64()?,
            interval_point_estimate_units: decoder.read_u64()?,
            interval_lower_units: decoder.read_u64()?,
            interval_upper_units: decoder.read_u64()?,
            interval_radius_units: decoder.read_u64()?,
            interval_confidence_exponent: decoder.read_u8()?,
            interval_query_observations: decoder.read_u64()?,
            assumptions_supported: decoder.read_bool(7, "assumptions_supported")?,
            action: decode_ann_action(decoder.read_u8()?)?,
            action_reason: decode_ann_action_reason(decoder.read_u8()?)?,
        },
        _ => return Err(StatisticalLogCodecError::UnknownStatisticKind { tag }),
    };
    decoder.finish()?;
    Ok(statistic)
}

fn decode_ope_estimator(tag: u8) -> Result<OpeEstimator, StatisticalLogCodecError> {
    match tag {
        1 => Ok(OpeEstimator::Direct),
        2 => Ok(OpeEstimator::ImportanceWeighted),
        3 => Ok(OpeEstimator::DoublyRobust),
        _ => Err(StatisticalLogCodecError::UnknownOpeEstimator { tag }),
    }
}

fn decode_ope_failure_behavior(tag: u8) -> Result<OpeFailureBehavior, StatisticalLogCodecError> {
    match tag {
        1 => Ok(OpeFailureBehavior::SelectPinnedFallback),
        _ => Err(StatisticalLogCodecError::UnknownOpeFailureBehavior { tag }),
    }
}

fn decode_ope_selection_reason(tag: u8) -> Result<OpeSelectionReason, StatisticalLogCodecError> {
    match tag {
        1 => Ok(OpeSelectionReason::IncompleteWindow),
        2 => Ok(OpeSelectionReason::ZeroSupport),
        3 => Ok(OpeSelectionReason::InsufficientEffectiveSampleSize),
        4 => Ok(OpeSelectionReason::CandidateNotBetter),
        5 => Ok(OpeSelectionReason::CandidateEstimatedBetter),
        _ => Err(StatisticalLogCodecError::UnknownOpeSelectionReason { tag }),
    }
}

fn decode_ann_sample_design(tag: u8) -> Result<QuerySampleDesign, StatisticalLogCodecError> {
    match tag {
        1 => Ok(QuerySampleDesign::KeyedUniformWithoutReplacement),
        2 => Ok(QuerySampleDesign::KeyedIndependentWithReplacement),
        3 => Ok(QuerySampleDesign::UnspecifiedDependence),
        _ => Err(StatisticalLogCodecError::UnknownAnnSampleDesign { tag }),
    }
}

fn decode_ann_action(tag: u8) -> Result<AnnRecallAction, StatisticalLogCodecError> {
    match tag {
        1 => Ok(AnnRecallAction::Candidate),
        2 => Ok(AnnRecallAction::PinnedFallback),
        3 => Ok(AnnRecallAction::Rebuild),
        _ => Err(StatisticalLogCodecError::UnknownAnnAction { tag }),
    }
}

fn decode_ann_action_reason(tag: u8) -> Result<AnnRecallActionReason, StatisticalLogCodecError> {
    match tag {
        1 => Ok(AnnRecallActionReason::IncompleteWindow),
        2 => Ok(AnnRecallActionReason::UnsupportedAssumptions),
        3 => Ok(AnnRecallActionReason::StatisticallyInconclusive),
        4 => Ok(AnnRecallActionReason::CandidateRecallSatisfied),
        5 => Ok(AnnRecallActionReason::RecallDriftDetected),
        _ => Err(StatisticalLogCodecError::UnknownAnnActionReason { tag }),
    }
}

fn push_oid(bytes: &mut Vec<u8>, oid: ObjectId) {
    bytes.extend_from_slice(oid.as_bytes());
}

// Appendix A fixes every durable integer to little-endian byte order. Keep
// the entire statistical-log family on these helpers so record bodies,
// identity transcripts, and outer framing cannot drift independently.
fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_i64(bytes: &mut Vec<u8>, value: i64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u128(bytes: &mut Vec<u8>, value: u128) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_i128(bytes: &mut Vec<u8>, value: i128) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], StatisticalLogCodecError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(StatisticalLogCodecError::LengthOverflow)?;
        let Some(value) = self.bytes.get(self.offset..end) else {
            return Err(StatisticalLogCodecError::Truncated {
                offset: self.offset,
                needed: length,
                remaining: self.bytes.len().saturating_sub(self.offset),
            });
        };
        self.offset = end;
        Ok(value)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], StatisticalLogCodecError> {
        let source = self.read_bytes(N)?;
        let mut value = [0_u8; N];
        for (destination, byte) in value.iter_mut().zip(source.iter().copied()) {
            *destination = byte;
        }
        Ok(value)
    }

    fn read_u8(&mut self) -> Result<u8, StatisticalLogCodecError> {
        let [value] = self.read_array::<1>()?;
        Ok(value)
    }

    fn read_bool(
        &mut self,
        statistic_tag: u8,
        field: &'static str,
    ) -> Result<bool, StatisticalLogCodecError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            actual => Err(StatisticalLogCodecError::InvalidBooleanTag {
                statistic_tag,
                field,
                actual,
            }),
        }
    }

    fn read_u16(&mut self) -> Result<u16, StatisticalLogCodecError> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32, StatisticalLogCodecError> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64, StatisticalLogCodecError> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    fn read_i64(&mut self) -> Result<i64, StatisticalLogCodecError> {
        Ok(i64::from_le_bytes(self.read_array::<8>()?))
    }

    fn read_u128(&mut self) -> Result<u128, StatisticalLogCodecError> {
        Ok(u128::from_le_bytes(self.read_array::<16>()?))
    }

    fn read_i128(&mut self) -> Result<i128, StatisticalLogCodecError> {
        Ok(i128::from_le_bytes(self.read_array::<16>()?))
    }

    fn read_oid(&mut self) -> Result<ObjectId, StatisticalLogCodecError> {
        Ok(ObjectId(self.read_array::<32>()?))
    }

    fn finish(self) -> Result<(), StatisticalLogCodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(StatisticalLogCodecError::TrailingBytes {
                offset: self.offset,
                remaining: self.bytes.len() - self.offset,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::cancel::progress_certificate::ProgressConfig;
    use std::io;

    use crate::ann_recall::{
        AnnRecallBinding, AnnRecallIdentity, AnnRecallLedger, AnnRecallObservation, AnnRecallWindow,
    };
    use crate::ope::{
        LoggedAction, LoggedDecision, OpeIdentity, OpeLedger, OpeWindow, Outcome, Probability,
        WEIGHT_SCALE,
    };
    use crate::progress::{
        DrainProgressIdentity, DrainProgressMonitor, DrainProgressProfile, SequencedPotential,
    };

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn oid(fill: u8) -> ObjectId {
        ObjectId([fill; 32])
    }

    #[test]
    fn fixed_width_integer_codec_is_normative_little_endian() -> TestResult {
        let mut bytes = Vec::new();
        push_u16(&mut bytes, 0x1234);
        push_u32(&mut bytes, 0x1234_5678);
        push_u64(&mut bytes, 0x0123_4567_89ab_cdef);
        push_i64(&mut bytes, -0x0123_4567_89ab_cdef);
        push_u128(&mut bytes, 0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);
        push_i128(&mut bytes, -0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);

        assert_eq!(&bytes[..2], &0x1234_u16.to_le_bytes());
        assert_eq!(&bytes[2..6], &0x1234_5678_u32.to_le_bytes());
        assert_eq!(&bytes[6..14], &0x0123_4567_89ab_cdef_u64.to_le_bytes());

        let mut decoder = Decoder::new(&bytes);
        assert_eq!(decoder.read_u16()?, 0x1234);
        assert_eq!(decoder.read_u32()?, 0x1234_5678);
        assert_eq!(decoder.read_u64()?, 0x0123_4567_89ab_cdef);
        assert_eq!(decoder.read_i64()?, -0x0123_4567_89ab_cdef);
        assert_eq!(
            decoder.read_u128()?,
            0x0011_2233_4455_6677_8899_aabb_ccdd_eeff
        );
        assert_eq!(
            decoder.read_i128()?,
            -0x0011_2233_4455_6677_8899_aabb_ccdd_eeff
        );
        decoder.finish()?;
        Ok(())
    }

    /// Test-only deterministic stand-in for the still-external keyed
    /// FrankenGraphDB identity authority. The byte transform intentionally
    /// differs from production ObjectId derivation and is never exported.
    #[derive(Clone, Copy)]
    struct TestOnlyIdentityAuthority;

    const TEST_IDENTITY_AUTHORITY: TestOnlyIdentityAuthority = TestOnlyIdentityAuthority;

    impl TestOnlyIdentityAuthority {
        fn fixture_oid(canonical_evidence_body: &[u8]) -> ObjectId {
            let mut bytes = asupersync::atp::object::compute_hash(canonical_evidence_body);
            for byte in &mut bytes {
                *byte ^= 0xa5;
            }
            ObjectId(bytes)
        }
    }

    impl StatisticalEvidenceIdentityIssuer for TestOnlyIdentityAuthority {
        fn issue_statistical_evidence_oid(
            &self,
            canonical_evidence_body: &[u8],
        ) -> Result<ObjectId, StatisticalEvidenceIdentityError> {
            Ok(Self::fixture_oid(canonical_evidence_body))
        }
    }

    impl StatisticalEvidenceIdentityVerifier for TestOnlyIdentityAuthority {
        fn verify_statistical_evidence_oid(
            &self,
            canonical_evidence_body: &[u8],
            evidence_oid: ObjectId,
        ) -> Result<(), StatisticalEvidenceIdentityError> {
            if evidence_oid == Self::fixture_oid(canonical_evidence_body) {
                Ok(())
            } else {
                Err(StatisticalEvidenceIdentityError::Rejected)
            }
        }
    }

    const TEST_LOG_DECODE_LIMITS: StatisticalLogDecodeLimits =
        StatisticalLogDecodeLimits::new(32, 1 << 20);

    fn read_log(
        bytes: &[u8],
        expected_maximum_records: usize,
    ) -> Result<StatisticalDecisionLog, StatisticalLogCodecError> {
        StatisticalDecisionLog::decode_canonical(
            bytes,
            expected_maximum_records,
            TEST_LOG_DECODE_LIMITS,
            &TEST_IDENTITY_AUTHORITY,
        )
    }

    fn eprocess_statistic(profile_oid: ObjectId, e_value_bits: u64) -> StatisticalStatistic {
        StatisticalStatistic::EProcess {
            profile_oid,
            p0_bits: 0.2_f64.to_bits(),
            lambda_bits: 1.0_f64.to_bits(),
            alpha_bits: 0.05_f64.to_bits(),
            max_evalue_bits: 1_000.0_f64.to_bits(),
            e_value_bits,
            rejection_threshold_bits: 20.0_f64.to_bits(),
            observations: 10,
            one_observations: 8,
        }
    }

    fn conformal_statistic(
        threshold_bits: u64,
        nonconformity_score_bits: u64,
        assessments: u64,
        covered: u64,
    ) -> StatisticalStatistic {
        conformal_statistic_for_population(
            oid(17),
            threshold_bits,
            nonconformity_score_bits,
            assessments,
            covered,
        )
    }

    fn conformal_statistic_for_population(
        population_oid: ObjectId,
        threshold_bits: u64,
        nonconformity_score_bits: u64,
        assessments: u64,
        covered: u64,
    ) -> StatisticalStatistic {
        StatisticalStatistic::ConformalCoverage {
            profile_oid: oid(16),
            population_oid,
            selection_oid: oid(18),
            alpha_bits: 0.2_f64.to_bits(),
            mode: StatisticalConformalMode::Upper,
            minimum_calibration_samples: 5,
            maximum_calibration_samples: 10,
            threshold_bits,
            nonconformity_score_bits,
            coverage_target_bits: 0.8_f64.to_bits(),
            assessments,
            covered,
        }
    }

    fn ope_statistic_for_population(population_oid: ObjectId) -> StatisticalStatistic {
        StatisticalStatistic::OffPolicyEvaluation {
            population_oid,
            strata_oid: oid(57),
            action_space_oid: oid(58),
            policy_epoch_oid: oid(59),
            estimator_oid: oid(51),
            estimator: OpeEstimator::DoublyRobust,
            failure_behavior: OpeFailureBehavior::SelectPinnedFallback,
            clipping_weight_units: 1_000_000,
            minimum_effective_sample_size: 3,
            maximum_observations: 10,
            maximum_actions_per_observation: 2,
            maximum_total_action_rows: 20,
            candidate_numerator: 90,
            fallback_numerator: 70,
            advantage_numerator: 20,
            common_denominator: 10_000_000_000_000_000,
            candidate_ess_numerator: 400,
            candidate_ess_denominator: 100,
            fallback_ess_numerator: 900,
            fallback_ess_denominator: 100,
            observations: 10,
            action_rows: 20,
            complete: true,
            candidate_ess_gate_passed: true,
            fallback_ess_gate_passed: true,
            zero_support_exclusions: 0,
            zero_support_exclusions_digest: empty_ope_support_exclusions_digest(),
            selection_reason: OpeSelectionReason::CandidateEstimatedBetter,
        }
    }

    fn eprocess_record(
        first: u64,
        last: u64,
    ) -> Result<StatisticalLogRecord, StatisticalLogRecordError> {
        eprocess_record_for_monitor(oid(1), first, last)
    }

    fn eprocess_record_for_monitor(
        monitor_oid: ObjectId,
        first: u64,
        last: u64,
    ) -> Result<StatisticalLogRecord, StatisticalLogRecordError> {
        StatisticalLogRecord::try_from_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::EProcess,
            monitor_oid,
            oid(3),
            StatisticalBatchRange::try_new(first, last)?,
            7,
            oid(4),
            oid(5),
            oid(4),
            eprocess_statistic(oid(6), 12.5_f64.to_bits()),
        )
    }

    fn eprocess_record_in_window(
        identity_window: StatisticalBatchRange,
        batch: StatisticalBatchRange,
    ) -> Result<StatisticalLogRecord, StatisticalLogRecordError> {
        StatisticalLogRecord::try_from_bound_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::EProcess,
            oid(1),
            oid(3),
            identity_window,
            batch,
            7,
            oid(4),
            oid(5),
            oid(4),
            eprocess_statistic(oid(6), 12.5_f64.to_bits()),
        )
    }

    fn conformal_record(
        first: u64,
        last: u64,
    ) -> Result<StatisticalLogRecord, StatisticalLogRecordError> {
        StatisticalLogRecord::try_from_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::ConformalThreshold,
            oid(11),
            oid(13),
            StatisticalBatchRange::try_new(first, last)?,
            7,
            oid(14),
            oid(15),
            oid(15),
            conformal_statistic(2.0_f64.to_bits(), 1.0_f64.to_bits(), 10, 8),
        )
    }

    fn ope_record(
        population_oid: ObjectId,
    ) -> Result<StatisticalLogRecord, StatisticalLogRecordError> {
        StatisticalLogRecord::try_from_bound_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::OffPolicyEvaluation,
            oid(51),
            oid(53),
            StatisticalBatchRange::try_new(60, 69)?,
            StatisticalBatchRange::try_new(69, 69)?,
            9,
            oid(54),
            oid(55),
            oid(54),
            ope_statistic_for_population(population_oid),
        )
    }

    fn terminal_ann_recall_evidence() -> TestResult<AnnRecallEvidence> {
        terminal_ann_recall_evidence_for_snapshot(oid(93))
    }

    fn terminal_ann_recall_evidence_for_snapshot(
        snapshot_oid: ObjectId,
    ) -> TestResult<AnnRecallEvidence> {
        let mut ledger = empty_ann_recall_ledger(snapshot_oid)?;
        let identity = ledger.identity();
        let binding = AnnRecallBinding::new(
            identity.monitor_oid(),
            identity.profile_oid(),
            identity.authorized_population_oid(),
            identity.snapshot_oid(),
            identity.authority_domain_oid(),
            identity.sample_key_oid(),
            identity.sample_design_oid(),
            identity.exact_baseline_oid(),
            identity.candidate_policy_oid(),
            identity.regime_epoch(),
        );
        for offset in 0_u8..100 {
            ledger.record(AnnRecallObservation::try_new(
                1_000 + u64::from(offset),
                ObjectId([offset; 32]),
                ObjectId([offset.wrapping_add(101); 32]),
                binding,
                vec![oid(200)],
                vec![oid(201)],
            )?)?;
        }
        Ok(ledger.evidence()?)
    }

    fn empty_ann_recall_ledger(snapshot_oid: ObjectId) -> TestResult<AnnRecallLedger> {
        let window = AnnRecallWindow::try_new(1_000, 1_099)?;
        let identity = AnnRecallIdentity::try_new(
            oid(90),
            oid(91),
            oid(92),
            snapshot_oid,
            oid(94),
            oid(95),
            oid(96),
            oid(97),
            oid(98),
            oid(99),
            oid(100),
            window,
            13,
        )?;
        let assumptions = AnnRecallAssumptions::new(
            QuerySampleDesign::KeyedUniformWithoutReplacement,
            true,
            true,
            true,
        );
        let profile = AnnRecallProfile::try_new(
            identity.profile_oid(),
            1,
            100,
            200,
            1,
            900_000_000,
            200_000_000,
            assumptions,
        )?;
        Ok(AnnRecallLedger::try_new(identity, profile)?)
    }

    fn ann_recall_record() -> TestResult<StatisticalLogRecord> {
        let evidence = terminal_ann_recall_evidence()?;
        Ok(StatisticalLogRecord::try_from_ann_recall(
            &TEST_IDENTITY_AUTHORITY,
            &evidence,
        )?)
    }

    fn records_for_every_monitor() -> TestResult<Vec<StatisticalLogRecord>> {
        let mut records = vec![
            eprocess_record(10, 19)?,
            StatisticalLogRecord::try_from_parts(
                &TEST_IDENTITY_AUTHORITY,
                StatisticalMonitorKind::ConformalThreshold,
                oid(11),
                oid(13),
                StatisticalBatchRange::try_new(20, 29)?,
                7,
                oid(14),
                oid(15),
                oid(15),
                conformal_statistic(f64::INFINITY.to_bits(), 3.0_f64.to_bits(), 10, 8),
            )?,
            StatisticalLogRecord::try_from_parts(
                &TEST_IDENTITY_AUTHORITY,
                StatisticalMonitorKind::ExplorationBudget,
                oid(21),
                oid(23),
                StatisticalBatchRange::try_new(30, 39)?,
                8,
                oid(24),
                oid(25),
                oid(24),
                StatisticalStatistic::ExplorationBudget {
                    alpha_bits: 0.5_f64.to_bits(),
                    residual_rate_bits: 0.1_f64.to_bits(),
                    upper_bound_bits: 0.2_f64.to_bits(),
                    target_rate_bits: 0.2_f64.to_bits(),
                    total_runs: 10,
                    discoveries: 1,
                    recommended_additional_runs: 0,
                },
            )?,
            StatisticalLogRecord::try_from_parts(
                &TEST_IDENTITY_AUTHORITY,
                StatisticalMonitorKind::DrainProgress,
                oid(31),
                oid(33),
                StatisticalBatchRange::try_new(40, 49)?,
                8,
                oid(34),
                oid(35),
                oid(34),
                StatisticalStatistic::DrainProgress {
                    current_potential_bits: 1.0_f64.to_bits(),
                    confidence_bound_bits: 0.2_f64.to_bits(),
                    observations: 10,
                    stall_detected: false,
                },
            )?,
            StatisticalLogRecord::try_from_parts(
                &TEST_IDENTITY_AUTHORITY,
                StatisticalMonitorKind::RegimeChange,
                oid(41),
                oid(43),
                StatisticalBatchRange::try_new(50, 59)?,
                9,
                oid(44),
                oid(45),
                oid(45),
                StatisticalStatistic::RegimeChange {
                    statistic: 120,
                    threshold: 100,
                    observations: 10,
                    detections: 1,
                },
            )?,
            ope_record(oid(56))?,
        ];
        records.push(ann_recall_record()?);
        Ok(records)
    }

    #[test]
    fn record_is_statistical_and_policy_consistent() -> TestResult {
        let candidate = eprocess_record(1, 2)?;
        assert_eq!(candidate.claim_class(), RegistryClaimClass::Statistical);
        assert_eq!(candidate.selection(), StatisticalPolicySelection::Candidate);

        let fallback = StatisticalLogRecord::try_from_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::EProcess,
            oid(1),
            oid(3),
            StatisticalBatchRange::try_new(3, 4)?,
            7,
            oid(4),
            oid(5),
            oid(5),
            candidate.statistic(),
        )?;
        assert_eq!(
            fallback.selection(),
            StatisticalPolicySelection::PinnedFallback
        );
        Ok(())
    }

    #[test]
    fn progress_constructor_derives_every_semantic_field() -> TestResult {
        let identity =
            DrainProgressIdentity::try_new(oid(61), oid(62), 100, 102, 9, oid(63), oid(64))?;
        let profile = DrainProgressProfile::try_new(
            ProgressConfig {
                confidence: 0.9,
                max_step_bound: 20.0,
                stall_threshold: 2,
                min_observations: 2,
                epsilon: 1e-12,
            },
            3,
        )?;
        let mut monitor = DrainProgressMonitor::try_new(identity, profile.clone())?;
        assert!(matches!(
            StatisticalLogRecord::try_from_progress(&TEST_IDENTITY_AUTHORITY, &monitor.evidence()),
            Err(StatisticalLogRecordError::EvidenceHasNoObservations {
                monitor: StatisticalMonitorKind::DrainProgress,
            })
        ));

        let evidence = monitor.observe(SequencedPotential::new(identity, profile, 100, 10.0))?;
        let record = StatisticalLogRecord::try_from_progress(&TEST_IDENTITY_AUTHORITY, &evidence)?;
        assert_eq!(record.monitor_kind(), StatisticalMonitorKind::DrainProgress);
        assert_eq!(record.source_monitor_oid(), oid(61));
        assert_eq!(
            StatisticalLogRecord::try_from_progress(&TEST_IDENTITY_AUTHORITY, &evidence)?
                .evidence_oid(),
            record.evidence_oid()
        );
        assert_ne!(record.evidence_oid(), record.monitor_oid());
        assert_ne!(record.monitor_oid(), record.source_monitor_oid());
        assert_eq!(record.filtration_or_window_oid(), oid(62));
        assert_eq!(
            record.identity_window(),
            StatisticalBatchRange::try_new(100, 102)?
        );
        assert_eq!(record.batch(), StatisticalBatchRange::try_new(100, 100)?);
        assert_eq!(record.regime_epoch(), 9);
        assert_eq!(record.candidate_decision_oid(), oid(63));
        assert_eq!(record.pinned_fallback_oid(), oid(64));
        assert_eq!(record.selected_policy_oid(), evidence.selected_policy_oid());
        assert_eq!(
            record.statistic(),
            StatisticalStatistic::DrainProgress {
                current_potential_bits: evidence.current_potential_bits(),
                confidence_bound_bits: evidence.confidence_bound_bits(),
                observations: evidence.total_observations(),
                stall_detected: evidence.stall_detected(),
            }
        );
        Ok(())
    }

    #[test]
    fn ope_constructor_retains_complete_trial_profile_and_decision_basis() -> TestResult {
        let identity = OpeIdentity::try_new(
            oid(71),
            OpeWindow::try_new(100, 100)?,
            oid(72),
            oid(73),
            oid(74),
            oid(75),
            11,
            oid(76),
            oid(77),
            oid(78),
            OpeEstimator::Direct,
        )?;
        let profile = OpeProfile::try_new(10 * WEIGHT_SCALE, 1, 1, 2, 2)?;
        let mut ledger = OpeLedger::try_new(identity, profile)?;
        let actions = vec![
            LoggedAction::new(
                oid(80),
                Probability::try_from_numerator(PROBABILITY_SCALE / 2)?,
                Probability::try_from_numerator(3 * PROBABILITY_SCALE / 4)?,
                Probability::try_from_numerator(PROBABILITY_SCALE / 4)?,
                Some(Outcome::try_from_scaled(2 * OUTCOME_SCALE)?),
            ),
            LoggedAction::new(
                oid(81),
                Probability::try_from_numerator(PROBABILITY_SCALE / 2)?,
                Probability::try_from_numerator(PROBABILITY_SCALE / 4)?,
                Probability::try_from_numerator(3 * PROBABILITY_SCALE / 4)?,
                Some(Outcome::try_from_scaled(0)?),
            ),
        ];
        ledger.record(LoggedDecision::try_new(
            100,
            oid(82),
            oid(83),
            oid(84),
            oid(80),
            Outcome::try_from_scaled(2 * OUTCOME_SCALE)?,
            actions,
        )?)?;
        let evidence = ledger.evidence()?;
        let record = StatisticalLogRecord::try_from_ope(&TEST_IDENTITY_AUTHORITY, &evidence)?;

        assert_eq!(record.source_monitor_oid(), identity.estimator_oid());
        assert_eq!(record.filtration_or_window_oid(), identity.selection_oid());
        assert_eq!(record.selected_policy_oid(), evidence.selected_policy_oid());
        let StatisticalStatistic::OffPolicyEvaluation {
            population_oid,
            strata_oid,
            action_space_oid,
            policy_epoch_oid,
            estimator_oid,
            estimator,
            failure_behavior,
            clipping_weight_units,
            minimum_effective_sample_size,
            maximum_observations,
            maximum_actions_per_observation,
            maximum_total_action_rows,
            candidate_numerator,
            fallback_numerator,
            advantage_numerator,
            common_denominator,
            candidate_ess_numerator,
            candidate_ess_denominator,
            fallback_ess_numerator,
            fallback_ess_denominator,
            observations,
            action_rows,
            complete,
            candidate_ess_gate_passed,
            fallback_ess_gate_passed,
            zero_support_exclusions,
            zero_support_exclusions_digest,
            selection_reason,
        } = record.statistic()
        else {
            return Err(io::Error::other("OPE constructor emitted the wrong statistic").into());
        };
        assert_eq!(population_oid, identity.population_oid());
        assert_eq!(strata_oid, identity.strata_oid());
        assert_eq!(action_space_oid, identity.action_space_oid());
        assert_eq!(policy_epoch_oid, identity.policy_epoch_oid());
        assert_eq!(estimator_oid, identity.estimator_oid());
        assert_eq!(estimator, identity.estimator());
        assert_eq!(failure_behavior, identity.failure_behavior());
        assert_eq!(clipping_weight_units, profile.clipping_weight_units());
        assert_eq!(
            minimum_effective_sample_size,
            profile.minimum_effective_sample_size()
        );
        assert_eq!(maximum_observations, 1);
        assert_eq!(maximum_actions_per_observation, 2);
        assert_eq!(maximum_total_action_rows, 2);
        assert_eq!(
            candidate_numerator,
            evidence.candidate_estimate().numerator()
        );
        assert_eq!(fallback_numerator, evidence.fallback_estimate().numerator());
        assert_eq!(
            advantage_numerator,
            evidence.advantage_estimate().numerator()
        );
        assert_eq!(
            common_denominator,
            evidence.candidate_estimate().denominator()
        );
        assert_eq!(
            (candidate_ess_numerator, candidate_ess_denominator),
            (
                evidence.candidate_effective_sample_size().numerator(),
                evidence.candidate_effective_sample_size().denominator(),
            )
        );
        assert_eq!(
            (fallback_ess_numerator, fallback_ess_denominator),
            (
                evidence.fallback_effective_sample_size().numerator(),
                evidence.fallback_effective_sample_size().denominator(),
            )
        );
        assert_eq!(observations, evidence.observations());
        assert_eq!(action_rows, evidence.action_rows());
        assert_eq!(complete, evidence.complete());
        assert_eq!(
            candidate_ess_gate_passed,
            evidence.candidate_ess_gate_passed()
        );
        assert_eq!(
            fallback_ess_gate_passed,
            evidence.fallback_ess_gate_passed()
        );
        assert_eq!(zero_support_exclusions, 0);
        assert_eq!(
            zero_support_exclusions_digest,
            empty_ope_support_exclusions_digest()
        );
        assert_eq!(selection_reason, evidence.selection_reason());
        Ok(())
    }

    #[test]
    fn ann_recall_constructor_retains_complete_terminal_trial() -> TestResult {
        let evidence = terminal_ann_recall_evidence()?;
        assert_eq!(evidence.action(), Some(AnnRecallAction::Rebuild));
        let identity = evidence.identity();
        let profile = evidence.profile();
        let assumptions = evidence.assumptions();
        let exact_recall = evidence.exact_recall();
        let interval = evidence.confidence_interval();
        let record =
            StatisticalLogRecord::try_from_ann_recall(&TEST_IDENTITY_AUTHORITY, &evidence)?;

        assert_eq!(record.monitor_kind(), StatisticalMonitorKind::AnnRecall);
        assert_eq!(record.source_monitor_oid(), identity.monitor_oid());
        assert_eq!(record.filtration_or_window_oid(), identity.sample_key_oid());
        assert_eq!(
            record.identity_window(),
            StatisticalBatchRange::try_new(
                identity.window().first_sequence(),
                identity.window().last_sequence(),
            )?
        );
        assert_eq!(record.batch(), record.identity_window());
        assert_eq!(record.selection(), StatisticalPolicySelection::Rebuild);
        assert_eq!(
            record.statistic(),
            StatisticalStatistic::AnnRecall {
                profile_oid: profile.profile_oid(),
                authorized_population_oid: identity.authorized_population_oid(),
                snapshot_oid: identity.snapshot_oid(),
                authority_domain_oid: identity.authority_domain_oid(),
                sample_key_oid: identity.sample_key_oid(),
                sample_design_oid: identity.sample_design_oid(),
                exact_baseline_oid: identity.exact_baseline_oid(),
                rebuild_policy_oid: identity.rebuild_policy_oid(),
                top_k: u64::try_from(profile.top_k())?,
                maximum_queries: u64::try_from(profile.maximum_queries())?,
                maximum_total_result_ids: u64::try_from(profile.maximum_total_result_ids())?,
                confidence_exponent: profile.confidence_exponent(),
                candidate_recall_threshold_units: profile.candidate_recall_threshold_units(),
                rebuild_recall_threshold_units: profile.rebuild_recall_threshold_units(),
                sample_design: assumptions.sample_design(),
                exact_baseline_complete: assumptions.exact_baseline_complete(),
                authorization_domain_fixed: assumptions.authorization_domain_fixed(),
                candidate_policy_fixed: assumptions.candidate_policy_fixed(),
                query_observations: evidence.query_observations(),
                exact_baseline_results: evidence.exact_baseline_results(),
                candidate_results: evidence.candidate_results(),
                intersection_hits: evidence.intersection_hits(),
                complete: true,
                exact_recall_intersection_hits: exact_recall.intersection_hits(),
                exact_recall_baseline_results: exact_recall.baseline_results(),
                interval_scale: interval.scale(),
                interval_point_estimate_units: interval.point_estimate_units(),
                interval_lower_units: interval.lower_units(),
                interval_upper_units: interval.upper_units(),
                interval_radius_units: interval.radius_units(),
                interval_confidence_exponent: interval.failure_probability_power_of_two_exponent(),
                interval_query_observations: interval.query_observations(),
                assumptions_supported: evidence.assumptions_supported(),
                action: AnnRecallAction::Rebuild,
                action_reason: AnnRecallActionReason::RecallDriftDetected,
            }
        );

        let encoded = record.encode_canonical()?;
        assert_eq!(encoded.len(), MAX_CANONICAL_RECORD_BYTES);
        let decoded = StatisticalLogRecord::decode_canonical(&encoded, &TEST_IDENTITY_AUTHORITY)?;
        assert_eq!(decoded, record);
        assert_eq!(decoded.encode_canonical()?, encoded);
        Ok(())
    }

    #[test]
    fn ann_recall_log_rejects_prefix_evidence() -> TestResult {
        let ledger = empty_ann_recall_ledger(oid(93))?;
        let prefix = ledger.evidence()?;
        assert!(!prefix.complete());
        assert_eq!(prefix.action(), None);
        assert_eq!(
            StatisticalLogRecord::try_from_ann_recall(&TEST_IDENTITY_AUTHORITY, &prefix),
            Err(StatisticalLogRecordError::AnnRecallEvidenceIncomplete)
        );
        Ok(())
    }

    #[test]
    fn ann_recall_monitor_identity_binds_trial_snapshot() -> TestResult {
        let left = StatisticalLogRecord::try_from_ann_recall(
            &TEST_IDENTITY_AUTHORITY,
            &terminal_ann_recall_evidence_for_snapshot(oid(93))?,
        )?;
        let right = StatisticalLogRecord::try_from_ann_recall(
            &TEST_IDENTITY_AUTHORITY,
            &terminal_ann_recall_evidence_for_snapshot(oid(110))?,
        )?;
        assert_eq!(left.source_monitor_oid(), right.source_monitor_oid());
        assert_ne!(left.monitor_oid(), right.monitor_oid());
        assert_ne!(left.evidence_oid(), right.evidence_oid());
        Ok(())
    }

    #[test]
    fn ann_recall_monitor_authority_authenticates_every_stable_trial_field() -> TestResult {
        let record = ann_recall_record()?;
        let statistic = record.statistic();
        let body = encode_monitor_identity_body(
            record.monitor_kind(),
            record.source_monitor_oid(),
            record.filtration_or_window_oid(),
            record.identity_window(),
            record.regime_epoch(),
            record.candidate_decision_oid(),
            record.pinned_fallback_oid(),
            statistic,
        )?;
        let stable_start = body
            .len()
            .checked_sub(stable_statistic_identity_len(statistic))
            .ok_or_else(|| io::Error::other("monitor identity omitted ANN stable payload"))?;
        let StatisticalStatistic::AnnRecall {
            profile_oid,
            authorized_population_oid,
            snapshot_oid,
            authority_domain_oid,
            sample_key_oid,
            sample_design_oid,
            exact_baseline_oid,
            rebuild_policy_oid,
            top_k,
            maximum_queries,
            maximum_total_result_ids,
            confidence_exponent,
            candidate_recall_threshold_units,
            rebuild_recall_threshold_units,
            sample_design,
            exact_baseline_complete,
            authorization_domain_fixed,
            candidate_policy_fixed,
            ..
        } = statistic
        else {
            return Err(io::Error::other("ANN record emitted another statistic").into());
        };
        let mut expected_stable = Vec::new();
        push_oid(&mut expected_stable, profile_oid);
        push_oid(&mut expected_stable, authorized_population_oid);
        push_oid(&mut expected_stable, snapshot_oid);
        push_oid(&mut expected_stable, authority_domain_oid);
        push_oid(&mut expected_stable, sample_key_oid);
        push_oid(&mut expected_stable, sample_design_oid);
        push_oid(&mut expected_stable, exact_baseline_oid);
        push_oid(&mut expected_stable, rebuild_policy_oid);
        push_u64(&mut expected_stable, top_k);
        push_u64(&mut expected_stable, maximum_queries);
        push_u64(&mut expected_stable, maximum_total_result_ids);
        expected_stable.push(confidence_exponent);
        push_u64(&mut expected_stable, candidate_recall_threshold_units);
        push_u64(&mut expected_stable, rebuild_recall_threshold_units);
        expected_stable.push(sample_design as u8);
        expected_stable.push(u8::from(exact_baseline_complete));
        expected_stable.push(u8::from(authorization_domain_fixed));
        expected_stable.push(u8::from(candidate_policy_fixed));
        assert_eq!(expected_stable.len(), 301);
        assert_eq!(&body[stable_start..], expected_stable);

        let base_oid = TEST_IDENTITY_AUTHORITY.issue_statistical_monitor_oid(&body)?;
        for field_offset in [
            0_usize, 32, 64, 96, 128, 160, 192, 224, 256, 264, 272, 280, 281, 289, 297, 298, 299,
            300,
        ] {
            let mut changed = body.clone();
            changed[stable_start + field_offset] ^= 1;
            assert_ne!(
                TEST_IDENTITY_AUTHORITY.issue_statistical_monitor_oid(&changed)?,
                base_oid
            );
        }

        for changed in [
            encode_monitor_identity_body(
                record.monitor_kind(),
                oid(111),
                record.filtration_or_window_oid(),
                record.identity_window(),
                record.regime_epoch(),
                record.candidate_decision_oid(),
                record.pinned_fallback_oid(),
                statistic,
            )?,
            encode_monitor_identity_body(
                record.monitor_kind(),
                record.source_monitor_oid(),
                oid(112),
                record.identity_window(),
                record.regime_epoch(),
                record.candidate_decision_oid(),
                record.pinned_fallback_oid(),
                statistic,
            )?,
            encode_monitor_identity_body(
                record.monitor_kind(),
                record.source_monitor_oid(),
                record.filtration_or_window_oid(),
                StatisticalBatchRange::try_new(999, 1_099)?,
                record.regime_epoch(),
                record.candidate_decision_oid(),
                record.pinned_fallback_oid(),
                statistic,
            )?,
            encode_monitor_identity_body(
                record.monitor_kind(),
                record.source_monitor_oid(),
                record.filtration_or_window_oid(),
                record.identity_window(),
                record.regime_epoch() + 1,
                record.candidate_decision_oid(),
                record.pinned_fallback_oid(),
                statistic,
            )?,
            encode_monitor_identity_body(
                record.monitor_kind(),
                record.source_monitor_oid(),
                record.filtration_or_window_oid(),
                record.identity_window(),
                record.regime_epoch(),
                oid(113),
                record.pinned_fallback_oid(),
                statistic,
            )?,
            encode_monitor_identity_body(
                record.monitor_kind(),
                record.source_monitor_oid(),
                record.filtration_or_window_oid(),
                record.identity_window(),
                record.regime_epoch(),
                record.candidate_decision_oid(),
                oid(114),
                statistic,
            )?,
        ] {
            assert_ne!(
                TEST_IDENTITY_AUTHORITY.issue_statistical_monitor_oid(&changed)?,
                base_oid
            );
        }
        Ok(())
    }

    #[test]
    fn ann_recall_decoder_recomputes_terminal_fields_before_identity_acceptance() -> TestResult {
        const ANN_PAYLOAD_OFFSET: usize = RECORD_FIXED_BYTES;
        const COMPLETE_OFFSET: usize = ANN_PAYLOAD_OFFSET + 333;
        const EXACT_RECALL_HITS_OFFSET: usize = ANN_PAYLOAD_OFFSET + 334;
        const ACTION_OFFSET: usize = ANN_PAYLOAD_OFFSET + 400;
        const CANDIDATE_OID_OFFSET: usize = 216;
        const SELECTED_OID_OFFSET: usize = 280;

        let encoded = ann_recall_record()?.encode_canonical()?;

        let mut incomplete = encoded.clone();
        incomplete[COMPLETE_OFFSET] = 0;
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(&incomplete, &TEST_IDENTITY_AUTHORITY),
            Err(StatisticalLogCodecError::InvalidRecord(
                StatisticalLogRecordError::AnnRecallEvidenceMustBeTerminal
            ))
        ));

        let mut inconsistent_recall = encoded.clone();
        inconsistent_recall[EXACT_RECALL_HITS_OFFSET + 7] ^= 1;
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(&inconsistent_recall, &TEST_IDENTITY_AUTHORITY),
            Err(StatisticalLogCodecError::InvalidRecord(
                StatisticalLogRecordError::AnnRecallDerivedFieldMismatch {
                    field: StatisticField::AnnExactRecallIntersectionHits,
                    ..
                }
            ))
        ));

        let mut unknown_action = encoded;
        unknown_action[ACTION_OFFSET] = 0;
        assert_eq!(
            StatisticalLogRecord::decode_canonical(&unknown_action, &TEST_IDENTITY_AUTHORITY),
            Err(StatisticalLogCodecError::UnknownAnnAction { tag: 0 })
        );

        let mut wrong_registered_selection = ann_recall_record()?.encode_canonical()?;
        let candidate_oid: [u8; 32] = wrong_registered_selection
            [CANDIDATE_OID_OFFSET..CANDIDATE_OID_OFFSET + 32]
            .try_into()?;
        wrong_registered_selection[SELECTED_OID_OFFSET..SELECTED_OID_OFFSET + 32]
            .copy_from_slice(&candidate_oid);
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(
                &wrong_registered_selection,
                &TEST_IDENTITY_AUTHORITY
            ),
            Err(StatisticalLogCodecError::InvalidRecord(
                StatisticalLogRecordError::AnnRecallSelectionMismatch { .. }
            ))
        ));
        Ok(())
    }

    #[test]
    fn invalid_identity_and_statistic_combinations_are_rejected() -> TestResult {
        let batch = StatisticalBatchRange::try_new(1, 1)?;
        let statistic = eprocess_record(1, 1)?.statistic();
        assert_eq!(
            StatisticalLogRecord::try_from_parts(
                &TEST_IDENTITY_AUTHORITY,
                StatisticalMonitorKind::EProcess,
                oid(1),
                oid(3),
                batch,
                0,
                oid(4),
                oid(4),
                oid(4),
                statistic,
            ),
            Err(StatisticalLogRecordError::CandidateEqualsFallback)
        );
        assert!(matches!(
            StatisticalLogRecord::try_from_parts(
                &TEST_IDENTITY_AUTHORITY,
                StatisticalMonitorKind::EProcess,
                oid(1),
                oid(3),
                batch,
                0,
                oid(4),
                oid(5),
                oid(9),
                statistic,
            ),
            Err(StatisticalLogRecordError::SelectedPolicyIsNeitherCandidateNorFallback { .. })
        ));
        assert!(matches!(
            StatisticalLogRecord::try_from_parts(
                &TEST_IDENTITY_AUTHORITY,
                StatisticalMonitorKind::DrainProgress,
                oid(1),
                oid(3),
                batch,
                0,
                oid(4),
                oid(5),
                oid(4),
                statistic,
            ),
            Err(StatisticalLogRecordError::MonitorStatisticMismatch { .. })
        ));
        Ok(())
    }

    #[test]
    fn statistic_validation_rejects_noncanonical_values_and_counts() -> TestResult {
        let negative_zero = eprocess_statistic(oid(6), (-0.0_f64).to_bits());
        assert!(matches!(
            StatisticalLogRecord::try_from_parts(
                &TEST_IDENTITY_AUTHORITY,
                StatisticalMonitorKind::EProcess,
                oid(1),
                oid(3),
                StatisticalBatchRange::try_new(1, 1)?,
                0,
                oid(4),
                oid(5),
                oid(5),
                negative_zero,
            ),
            Err(StatisticalLogRecordError::NonCanonicalFloatBits { .. })
        ));

        let invalid_counts = conformal_statistic(1.0_f64.to_bits(), 0.0_f64.to_bits(), 2, 3);
        assert!(matches!(
            StatisticalLogRecord::try_from_parts(
                &TEST_IDENTITY_AUTHORITY,
                StatisticalMonitorKind::ConformalThreshold,
                oid(1),
                oid(3),
                StatisticalBatchRange::try_new(1, 1)?,
                0,
                oid(4),
                oid(5),
                oid(5),
                invalid_counts,
            ),
            Err(StatisticalLogRecordError::SubcountExceedsTotal { .. })
        ));

        let signed_conformal = StatisticalLogRecord::try_from_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::ConformalThreshold,
            oid(1),
            oid(3),
            StatisticalBatchRange::try_new(1, 1)?,
            0,
            oid(4),
            oid(5),
            oid(5),
            conformal_statistic((-2.0_f64).to_bits(), (-1.0_f64).to_bits(), 1, 1),
        )?;
        assert_eq!(
            signed_conformal.monitor_kind(),
            StatisticalMonitorKind::ConformalThreshold
        );

        let invalid_infinity =
            conformal_statistic(f64::NEG_INFINITY.to_bits(), 0.0_f64.to_bits(), 1, 1);
        assert!(matches!(
            StatisticalLogRecord::try_from_parts(
                &TEST_IDENTITY_AUTHORITY,
                StatisticalMonitorKind::ConformalThreshold,
                oid(1),
                oid(3),
                StatisticalBatchRange::try_new(1, 1)?,
                0,
                oid(4),
                oid(5),
                oid(5),
                invalid_infinity,
            ),
            Err(StatisticalLogRecordError::StatisticMustBeFiniteOrPositiveInfinity { .. })
        ));
        Ok(())
    }

    #[test]
    fn record_round_trip_covers_every_closed_statistic_variant() -> TestResult {
        for record in records_for_every_monitor()? {
            let encoded = record.encode_canonical()?;
            let decoded =
                StatisticalLogRecord::decode_canonical(&encoded, &TEST_IDENTITY_AUTHORITY)?;
            assert_eq!(decoded, record);
            assert_eq!(decoded.encode_canonical()?, encoded);
        }
        Ok(())
    }

    #[test]
    fn eprocess_evidence_identity_covers_profile_config_and_complete_window() -> TestResult {
        let batch = StatisticalBatchRange::try_new(12, 12)?;
        let base = StatisticalLogRecord::try_from_bound_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::EProcess,
            oid(1),
            oid(3),
            StatisticalBatchRange::try_new(10, 19)?,
            batch,
            7,
            oid(4),
            oid(5),
            oid(4),
            eprocess_statistic(oid(6), 12.5_f64.to_bits()),
        )?;
        let changed_profile = StatisticalLogRecord::try_from_bound_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::EProcess,
            oid(1),
            oid(3),
            StatisticalBatchRange::try_new(10, 19)?,
            batch,
            7,
            oid(4),
            oid(5),
            oid(4),
            eprocess_statistic(oid(7), 12.5_f64.to_bits()),
        )?;
        let changed_config = StatisticalLogRecord::try_from_bound_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::EProcess,
            oid(1),
            oid(3),
            StatisticalBatchRange::try_new(10, 19)?,
            batch,
            7,
            oid(4),
            oid(5),
            oid(4),
            StatisticalStatistic::EProcess {
                profile_oid: oid(6),
                p0_bits: 0.3_f64.to_bits(),
                lambda_bits: 1.0_f64.to_bits(),
                alpha_bits: 0.05_f64.to_bits(),
                max_evalue_bits: 1_000.0_f64.to_bits(),
                e_value_bits: 12.5_f64.to_bits(),
                rejection_threshold_bits: 20.0_f64.to_bits(),
                observations: 10,
                one_observations: 8,
            },
        )?;
        let changed_window = StatisticalLogRecord::try_from_bound_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::EProcess,
            oid(1),
            oid(3),
            StatisticalBatchRange::try_new(9, 19)?,
            batch,
            7,
            oid(4),
            oid(5),
            oid(4),
            eprocess_statistic(oid(6), 12.5_f64.to_bits()),
        )?;

        for changed in [changed_profile, changed_config, changed_window] {
            assert_ne!(changed.monitor_oid(), base.monitor_oid());
            assert_ne!(changed.evidence_digest(), base.evidence_digest());
            assert_ne!(changed.evidence_oid(), base.evidence_oid());
            assert_ne!(canonical_record_order(&base, &changed), Ordering::Equal);
        }
        assert_eq!(canonical_record_order(&base, &base), Ordering::Equal);
        Ok(())
    }

    #[test]
    fn stable_monitor_identity_separates_trials_sharing_metric_or_estimator() -> TestResult {
        let conformal_left = StatisticalLogRecord::try_from_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::ConformalThreshold,
            oid(11),
            oid(13),
            StatisticalBatchRange::try_new(20, 29)?,
            7,
            oid(14),
            oid(15),
            oid(15),
            conformal_statistic_for_population(
                oid(17),
                2.0_f64.to_bits(),
                1.0_f64.to_bits(),
                10,
                8,
            ),
        )?;
        let conformal_right = StatisticalLogRecord::try_from_parts(
            &TEST_IDENTITY_AUTHORITY,
            StatisticalMonitorKind::ConformalThreshold,
            oid(11),
            oid(13),
            StatisticalBatchRange::try_new(20, 29)?,
            7,
            oid(14),
            oid(15),
            oid(15),
            conformal_statistic_for_population(
                oid(19),
                2.0_f64.to_bits(),
                1.0_f64.to_bits(),
                10,
                8,
            ),
        )?;
        assert_eq!(
            conformal_left.source_monitor_oid(),
            conformal_right.source_monitor_oid()
        );
        assert_ne!(conformal_left.monitor_oid(), conformal_right.monitor_oid());

        let ope_left = ope_record(oid(56))?;
        let ope_right = ope_record(oid(60))?;
        assert_eq!(
            ope_left.source_monitor_oid(),
            ope_right.source_monitor_oid()
        );
        assert_ne!(ope_left.monitor_oid(), ope_right.monitor_oid());
        Ok(())
    }

    #[test]
    fn canonical_decode_checks_transcript_digest_and_authoritative_oid() -> TestResult {
        let record = eprocess_record(1, 1)?;
        let canonical = record.encode_canonical()?;

        let mut changed_digest = canonical.clone();
        let digest_byte = changed_digest
            .get_mut(112)
            .ok_or_else(|| io::Error::other("record omitted evidence digest"))?;
        *digest_byte ^= 1;
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(&changed_digest, &TEST_IDENTITY_AUTHORITY),
            Err(StatisticalLogCodecError::EvidenceDigestMismatch { .. })
        ));

        let mut changed_oid = canonical;
        let oid_byte = changed_oid
            .get_mut(80)
            .ok_or_else(|| io::Error::other("record omitted evidence OID"))?;
        *oid_byte ^= 1;
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(&changed_oid, &TEST_IDENTITY_AUTHORITY),
            Err(
                StatisticalLogCodecError::EvidenceIdentityVerificationFailed {
                    source: StatisticalEvidenceIdentityError::Rejected,
                    ..
                }
            )
        ));

        let mut changed_monitor_oid = record.encode_canonical()?;
        let monitor_oid_byte = changed_monitor_oid
            .get_mut(48)
            .ok_or_else(|| io::Error::other("record omitted stable monitor OID"))?;
        *monitor_oid_byte ^= 1;
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(&changed_monitor_oid, &TEST_IDENTITY_AUTHORITY),
            Err(
                StatisticalLogCodecError::MonitorIdentityVerificationFailed {
                    source: StatisticalEvidenceIdentityError::Rejected,
                    ..
                }
            )
        ));
        Ok(())
    }

    #[test]
    fn ope_decode_recomputes_every_decision_gate_before_identity_acceptance() -> TestResult {
        const OPE_PAYLOAD_OFFSET: usize = RECORD_FIXED_BYTES;
        const ADVANTAGE_OFFSET: usize = OPE_PAYLOAD_OFFSET + 234;
        const COMPLETE_OFFSET: usize = OPE_PAYLOAD_OFFSET + 346;
        const CANDIDATE_GATE_OFFSET: usize = OPE_PAYLOAD_OFFSET + 347;
        const SELECTION_REASON_OFFSET: usize = OPE_PAYLOAD_OFFSET + 389;

        let encoded = ope_record(oid(56))?.encode_canonical()?;

        let mut changed_advantage = encoded.clone();
        *changed_advantage
            .get_mut(ADVANTAGE_OFFSET + 15)
            .ok_or_else(|| io::Error::other("OPE payload omitted advantage"))? ^= 1;
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(&changed_advantage, &TEST_IDENTITY_AUTHORITY),
            Err(StatisticalLogCodecError::InvalidRecord(
                StatisticalLogRecordError::OpeAdvantageMismatch { .. }
            ))
        ));

        let mut changed_complete = encoded.clone();
        *changed_complete
            .get_mut(COMPLETE_OFFSET)
            .ok_or_else(|| io::Error::other("OPE payload omitted completeness"))? = 0;
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(&changed_complete, &TEST_IDENTITY_AUTHORITY),
            Err(StatisticalLogCodecError::InvalidRecord(
                StatisticalLogRecordError::OpeCompletenessMismatch { .. }
            ))
        ));

        let mut changed_gate = encoded.clone();
        *changed_gate
            .get_mut(CANDIDATE_GATE_OFFSET)
            .ok_or_else(|| io::Error::other("OPE payload omitted candidate ESS gate"))? = 0;
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(&changed_gate, &TEST_IDENTITY_AUTHORITY),
            Err(StatisticalLogCodecError::InvalidRecord(
                StatisticalLogRecordError::OpeEssGateMismatch { .. }
            ))
        ));

        let mut changed_reason = encoded;
        *changed_reason
            .get_mut(SELECTION_REASON_OFFSET)
            .ok_or_else(|| io::Error::other("OPE payload omitted selection reason"))? =
            OpeSelectionReason::CandidateNotBetter as u8;
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(&changed_reason, &TEST_IDENTITY_AUTHORITY),
            Err(StatisticalLogCodecError::InvalidRecord(
                StatisticalLogRecordError::OpeSelectionReasonMismatch { .. }
            ))
        ));
        Ok(())
    }

    #[test]
    fn ordered_append_rejections_are_atomic() -> TestResult {
        let mut log = StatisticalDecisionLog::try_new(3)?;
        let identity_window = StatisticalBatchRange::try_new(1, 30)?;
        log.append(eprocess_record_in_window(
            identity_window,
            StatisticalBatchRange::try_new(10, 19)?,
        )?)?;
        let before = log.encode_canonical()?;
        let overlap = log.append(eprocess_record_in_window(
            identity_window,
            StatisticalBatchRange::try_new(19, 25)?,
        )?);
        assert!(matches!(
            overlap,
            Err(StatisticalLogAppendError::BatchNotStrictlyAfterMonitor { .. })
        ));
        assert_eq!(log.encode_canonical()?, before);

        let out_of_order = log.append(eprocess_record_in_window(
            identity_window,
            StatisticalBatchRange::try_new(1, 9)?,
        )?);
        assert!(matches!(
            out_of_order,
            Err(StatisticalLogAppendError::BatchNotStrictlyAfterMonitor { .. })
        ));
        assert_eq!(log.encode_canonical()?, before);
        assert_eq!(log.len(), 1);
        Ok(())
    }

    #[test]
    fn same_kind_monitor_oids_share_a_batch_without_aliasing() -> TestResult {
        let left = eprocess_record_for_monitor(oid(1), 10, 19)?;
        let right = eprocess_record_for_monitor(oid(2), 10, 19)?;
        assert_ne!(left.evidence_oid(), right.evidence_oid());
        let (lower, higher) = if canonical_record_order(&left, &right) == Ordering::Less {
            (left, right)
        } else {
            (right, left)
        };

        let mut canonical = StatisticalDecisionLog::try_new(2)?;
        canonical.append(lower)?;
        canonical.append(higher)?;
        assert_eq!(canonical.records(), &[lower, higher]);

        let mut reversed = StatisticalDecisionLog::try_new(2)?;
        reversed.append(higher)?;
        assert!(matches!(
            reversed.append(lower),
            Err(StatisticalLogAppendError::RecordNotInCanonicalOrder {
                previous_monitor: StatisticalMonitorKind::EProcess,
                incoming_monitor: StatisticalMonitorKind::EProcess,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn distinct_monitors_share_a_batch_in_canonical_order() -> TestResult {
        let eprocess = eprocess_record(10, 19)?;
        let conformal = conformal_record(10, 19)?;
        let mut log = StatisticalDecisionLog::try_new(4)?;
        log.append(eprocess)?;
        log.append(conformal)?;
        assert_eq!(log.records(), &[eprocess, conformal]);
        let canonical = log.encode_canonical()?;
        assert_eq!(read_log(&canonical, 4)?, log);

        let before_duplicate = log.encode_canonical()?;
        assert!(matches!(
            log.append(conformal),
            Err(StatisticalLogAppendError::DuplicateRecord {
                monitor: StatisticalMonitorKind::ConformalThreshold,
                ..
            })
        ));
        assert_eq!(log.encode_canonical()?, before_duplicate);

        let mut reversed = StatisticalDecisionLog::try_new(2)?;
        reversed.append(conformal)?;
        let before_out_of_order = reversed.encode_canonical()?;
        assert!(matches!(
            reversed.append(eprocess),
            Err(StatisticalLogAppendError::RecordNotInCanonicalOrder {
                previous_monitor: StatisticalMonitorKind::ConformalThreshold,
                incoming_monitor: StatisticalMonitorKind::EProcess,
                ..
            })
        ));
        assert_eq!(reversed.encode_canonical()?, before_out_of_order);
        Ok(())
    }

    #[test]
    fn record_limit_rejection_is_atomic() -> TestResult {
        let mut log = StatisticalDecisionLog::try_new(1)?;
        log.append(eprocess_record(1, 1)?)?;
        let before = log.encode_canonical()?;
        assert_eq!(
            log.append(eprocess_record(2, 2)?),
            Err(StatisticalLogAppendError::RecordLimitReached { maximum: 1 })
        );
        assert_eq!(log.encode_canonical()?, before);
        Ok(())
    }

    #[test]
    fn log_replay_is_byte_identical() -> TestResult {
        fn build() -> TestResult<StatisticalDecisionLog> {
            let records = records_for_every_monitor()?;
            let mut log = StatisticalDecisionLog::try_new(16)?;
            for record in records {
                log.append(record)?;
            }
            Ok(log)
        }

        let left = build()?;
        let right = build()?;
        let left_bytes = left.encode_canonical()?;
        let right_bytes = right.encode_canonical()?;
        assert_eq!(left_bytes, right_bytes);
        assert_eq!(&left_bytes[8..10], &STATISTICAL_LOG_VERSION.to_le_bytes());
        assert_eq!(&left_bytes[10..12], &LOG_RESERVED.to_le_bytes());
        assert_eq!(&left_bytes[12..16], &16_u32.to_le_bytes());
        assert_eq!(
            &left_bytes[16..20],
            &u32::try_from(left.len())?.to_le_bytes()
        );

        let decoded = read_log(&left_bytes, 16)?;
        assert_eq!(decoded, left);
        assert_eq!(decoded.maximum_records(), 16);
        assert_eq!(decoded.encode_canonical()?, left_bytes);
        Ok(())
    }

    #[test]
    fn log_reader_enforces_trusted_profile_and_admission_before_reserving() -> TestResult {
        let mut log = StatisticalDecisionLog::try_new(2)?;
        log.append(eprocess_record(1, 1)?)?;
        let encoded = log.encode_canonical()?;

        assert_eq!(
            read_log(&encoded, 3),
            Err(StatisticalLogCodecError::LogBoundMismatch {
                expected: 3,
                actual: 2,
            })
        );
        assert_eq!(
            StatisticalDecisionLog::decode_canonical(
                &encoded,
                2,
                StatisticalLogDecodeLimits::new(1, encoded.len()),
                &TEST_IDENTITY_AUTHORITY,
            ),
            Err(StatisticalLogCodecError::DecodeRecordLimitExceeded {
                actual: 2,
                maximum: 1,
            })
        );
        assert_eq!(
            StatisticalDecisionLog::decode_canonical(
                &encoded,
                2,
                StatisticalLogDecodeLimits::new(2, encoded.len() - 1),
                &TEST_IDENTITY_AUTHORITY,
            ),
            Err(StatisticalLogCodecError::EncodedByteLimitExceeded {
                actual: encoded.len(),
                maximum: encoded.len() - 1,
            })
        );

        let mut missing_frame = encoded;
        missing_frame[16..20].copy_from_slice(&2_u32.to_le_bytes());
        assert!(matches!(
            read_log(&missing_frame, 2),
            Err(StatisticalLogCodecError::Truncated { .. })
        ));

        let mut undersized_frame = log.encode_canonical()?;
        undersized_frame[20..24].copy_from_slice(&0_u32.to_le_bytes());
        assert_eq!(
            read_log(&undersized_frame, 2),
            Err(StatisticalLogCodecError::RecordFrameTooSmall {
                index: 0,
                actual: 0,
                minimum: MIN_CANONICAL_RECORD_BYTES,
            })
        );

        let mut inconsistent_frame = log.encode_canonical()?;
        let payload_length_offset = 24 + RECORD_FIXED_BYTES - 2;
        let short_payload_length = u16::try_from(MIN_STATISTIC_PAYLOAD_BYTES)?;
        inconsistent_frame[payload_length_offset..payload_length_offset + 2]
            .copy_from_slice(&short_payload_length.to_le_bytes());
        assert!(matches!(
            read_log(&inconsistent_frame, 2),
            Err(StatisticalLogCodecError::RecordFrameLengthMismatch { index: 0, .. })
        ));
        Ok(())
    }

    #[test]
    fn strict_decoder_rejects_trailing_and_nonstatistical_bytes() -> TestResult {
        let record = eprocess_record(1, 1)?;
        let mut trailing = record.encode_canonical()?;
        trailing.push(0);
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(&trailing, &TEST_IDENTITY_AUTHORITY),
            Err(StatisticalLogCodecError::TrailingBytes { .. })
        ));

        let mut wrong_claim = record.encode_canonical()?;
        let claim_offset = 11;
        let claim = wrong_claim
            .get_mut(claim_offset)
            .ok_or_else(|| std::io::Error::other("encoded record did not contain its claim tag"))?;
        *claim = 0xff;
        assert_eq!(
            StatisticalLogRecord::decode_canonical(&wrong_claim, &TEST_IDENTITY_AUTHORITY),
            Err(StatisticalLogCodecError::NonStatisticalClaimTag { actual: 0xff })
        );
        Ok(())
    }

    #[test]
    fn strict_log_decoder_rejects_version_and_truncation() -> TestResult {
        let mut log = StatisticalDecisionLog::try_new(2)?;
        log.append(eprocess_record(1, 1)?)?;
        let encoded = log.encode_canonical()?;
        let truncated_length = encoded.len().saturating_sub(1);
        let truncated = encoded
            .get(..truncated_length)
            .ok_or_else(|| std::io::Error::other("encoded log could not be truncated"))?;
        assert!(matches!(
            read_log(truncated, 2),
            Err(StatisticalLogCodecError::Truncated { .. })
        ));

        let mut wrong_version = encoded;
        let version_low = wrong_version
            .get_mut(9)
            .ok_or_else(|| std::io::Error::other("encoded log did not contain its version"))?;
        *version_low = 5;
        assert!(matches!(
            read_log(&wrong_version, 2),
            Err(StatisticalLogCodecError::UnsupportedVersion {
                domain: CanonicalDomain::Log,
                ..
            })
        ));
        Ok(())
    }
}
