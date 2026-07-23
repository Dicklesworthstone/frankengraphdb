//! Canonical statistical calibration log.
//!
//! Records in this module are statistical evidence by construction. There is
//! no claim-class field a caller can change, no invariant constructor, and no
//! conversion to an invariant claim. Each record binds a closed monitor and
//! statistic vocabulary to immutable evidence, stream, regime, candidate, and
//! fallback identities. The bounded log accepts a deterministic total record
//! order and enforces strictly ordered, nonoverlapping batches independently
//! for each monitor family.

use core::{cmp::Ordering, fmt};

use fgdb_claim::RegistryClaimClass;
use fgdb_types::ObjectId;

use crate::{
    exploration::ExplorationBudgetEvidence, ope::OpeEvidence, progress::DrainProgressEvidence,
    regime::RegimeSignalEvidence,
};

/// Canonical record encoding version.
pub const STATISTICAL_LOG_RECORD_VERSION: u16 = 1;

/// Canonical bounded-log encoding version.
pub const STATISTICAL_LOG_VERSION: u16 = 1;

/// Absolute record-count ceiling for one in-memory log.
pub const MAX_STATISTICAL_LOG_RECORDS: usize = 1_048_576;

const RECORD_MAGIC: [u8; 8] = *b"FGDBSLR1";
const LOG_MAGIC: [u8; 8] = *b"FGDBSLL1";
const RECORD_TAG: u8 = 1;
const STATISTICAL_CLAIM_TAG: u8 = 1;
const RECORD_RESERVED: u16 = 0;
const LOG_RESERVED: u16 = 0;
const RECORD_FIXED_BYTES: usize = 234;
const LOG_HEADER_BYTES: usize = 20;
const MAX_STATISTIC_PAYLOAD_BYTES: usize = 96;
const MAX_CANONICAL_RECORD_BYTES: usize = RECORD_FIXED_BYTES + MAX_STATISTIC_PAYLOAD_BYTES;

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
            _ => Err(StatisticalLogCodecError::UnknownMonitorKind { tag }),
        }
    }
}

/// Exact statistic payload emitted by one closed monitor family.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StatisticalStatistic {
    /// E-process value and rejection boundary.
    EProcess {
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
        /// Candidate estimate numerator.
        candidate_numerator: i128,
        /// Fallback estimate numerator.
        fallback_numerator: i128,
        /// Positive common estimate denominator.
        common_denominator: u128,
        /// Candidate effective-sample-size numerator.
        candidate_ess_numerator: u128,
        /// Candidate effective-sample-size denominator, or zero with a zero
        /// numerator.
        candidate_ess_denominator: u128,
        /// Logged decisions.
        observations: u64,
        /// Explicit zero-support exclusions.
        zero_support_exclusions: u64,
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
        }
    }

    const fn payload_len(self) -> usize {
        match self {
            Self::EProcess { .. } | Self::RegimeChange { .. } => 32,
            Self::ConformalCoverage { .. } => 40,
            Self::ExplorationBudget { .. } => 48,
            Self::DrainProgress { .. } => 25,
            Self::OffPolicyEvaluation { .. } => 96,
        }
    }

    fn validate(self) -> Result<(), StatisticalLogRecordError> {
        match self {
            Self::EProcess {
                e_value_bits,
                rejection_threshold_bits,
                observations,
                one_observations,
            } => {
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
                threshold_bits,
                nonconformity_score_bits,
                coverage_target_bits,
                assessments,
                covered,
            } => {
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
                residual_rate_bits,
                upper_bound_bits,
                target_rate_bits,
                total_runs,
                discoveries,
                ..
            } => {
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
                common_denominator,
                candidate_ess_numerator,
                candidate_ess_denominator,
                observations,
                zero_support_exclusions,
                ..
            } => {
                if common_denominator == 0 {
                    return Err(StatisticalLogRecordError::ZeroEstimateDenominator);
                }
                if candidate_ess_denominator == 0 && candidate_ess_numerator != 0 {
                    return Err(
                        StatisticalLogRecordError::EffectiveSampleSizeDenominatorMismatch {
                            numerator: candidate_ess_numerator,
                            denominator: candidate_ess_denominator,
                        },
                    );
                }
                if let Some(maximum_exclusions) = observations.checked_mul(2) {
                    validate_subcount(
                        StatisticField::ZeroSupportExclusions,
                        zero_support_exclusions,
                        maximum_exclusions,
                    )?;
                }
                Ok(())
            }
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

/// Whether a record selected the candidate or its pinned fallback.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StatisticalPolicySelection {
    /// Candidate decision.
    Candidate = 1,
    /// Pinned deterministic fallback.
    PinnedFallback = 2,
}

/// Immutable statistical monitor record.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct StatisticalLogRecord {
    monitor_kind: StatisticalMonitorKind,
    monitor_oid: ObjectId,
    evidence_oid: ObjectId,
    filtration_or_window_oid: ObjectId,
    batch: StatisticalBatchRange,
    regime_epoch: u64,
    candidate_decision_oid: ObjectId,
    pinned_fallback_oid: ObjectId,
    selected_policy_oid: ObjectId,
    statistic: StatisticalStatistic,
}

impl StatisticalLogRecord {
    /// Constructs a record from identity-bound exploration-budget evidence.
    ///
    /// The batch is the singleton source sequence newly consumed by this
    /// evidence snapshot; statistic counters remain cumulative.
    pub fn try_from_exploration(
        evidence_oid: ObjectId,
        evidence: &ExplorationBudgetEvidence,
    ) -> Result<Self, StatisticalLogRecordError> {
        let identity = evidence.identity();
        let batch = singleton_batch_from_cumulative_evidence(
            StatisticalMonitorKind::ExplorationBudget,
            identity.first_sequence(),
            identity.last_sequence(),
            evidence.through_sequence(),
            evidence.total_runs(),
        )?;
        Self::try_from_parts(
            StatisticalMonitorKind::ExplorationBudget,
            identity.budget_oid(),
            evidence_oid,
            identity.window_oid(),
            batch,
            identity.regime_epoch(),
            identity.candidate_decision_oid(),
            identity.pinned_fallback_oid(),
            evidence.selected_policy_oid(),
            StatisticalStatistic::ExplorationBudget {
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
        evidence_oid: ObjectId,
        evidence: &DrainProgressEvidence,
    ) -> Result<Self, StatisticalLogRecordError> {
        let identity = evidence.identity();
        let batch = singleton_batch_from_cumulative_evidence(
            StatisticalMonitorKind::DrainProgress,
            identity.first_sequence(),
            identity.last_sequence(),
            evidence.through_sequence(),
            evidence.total_observations(),
        )?;
        Self::try_from_parts(
            StatisticalMonitorKind::DrainProgress,
            identity.monitor_oid(),
            evidence_oid,
            identity.filtration_oid(),
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
        evidence_oid: ObjectId,
        evidence: &RegimeSignalEvidence,
    ) -> Result<Self, StatisticalLogRecordError> {
        let identity = evidence.identity();
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
        Self::try_from_parts(
            StatisticalMonitorKind::RegimeChange,
            identity.signal_oid(),
            evidence_oid,
            identity.metric_stream_oid(),
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
        evidence_oid: ObjectId,
        evidence: &OpeEvidence,
    ) -> Result<Self, StatisticalLogRecordError> {
        let identity = evidence.identity();
        let batch = singleton_batch_from_observation_count(
            StatisticalMonitorKind::OffPolicyEvaluation,
            identity.window().first_sequence(),
            identity.window().last_sequence(),
            evidence.observations(),
        )?;
        let candidate = evidence.candidate_estimate();
        let fallback = evidence.fallback_estimate();
        if candidate.denominator() != fallback.denominator() {
            return Err(
                StatisticalLogRecordError::EvidenceEstimateDenominatorMismatch {
                    candidate: candidate.denominator(),
                    fallback: fallback.denominator(),
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
        Self::try_from_parts(
            StatisticalMonitorKind::OffPolicyEvaluation,
            identity.estimator_oid(),
            evidence_oid,
            identity.selection_oid(),
            batch,
            identity.regime_epoch(),
            identity.candidate_policy_oid(),
            identity.fallback_policy_oid(),
            evidence.selected_policy_oid(),
            StatisticalStatistic::OffPolicyEvaluation {
                candidate_numerator: candidate.numerator(),
                fallback_numerator: fallback.numerator(),
                common_denominator: candidate.denominator(),
                candidate_ess_numerator: candidate_ess.numerator(),
                candidate_ess_denominator: candidate_ess.denominator(),
                observations: evidence.observations(),
                zero_support_exclusions,
            },
        )
    }

    /// Constructs a validated record from already-decoded canonical fields.
    ///
    /// Kept private so live records originate only from typed monitor evidence.
    #[allow(clippy::too_many_arguments)]
    fn try_from_parts(
        monitor_kind: StatisticalMonitorKind,
        monitor_oid: ObjectId,
        evidence_oid: ObjectId,
        filtration_or_window_oid: ObjectId,
        batch: StatisticalBatchRange,
        regime_epoch: u64,
        candidate_decision_oid: ObjectId,
        pinned_fallback_oid: ObjectId,
        selected_policy_oid: ObjectId,
        statistic: StatisticalStatistic,
    ) -> Result<Self, StatisticalLogRecordError> {
        if candidate_decision_oid == pinned_fallback_oid {
            return Err(StatisticalLogRecordError::CandidateEqualsFallback);
        }
        if selected_policy_oid != candidate_decision_oid
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

        Ok(Self {
            monitor_kind,
            monitor_oid,
            evidence_oid,
            filtration_or_window_oid,
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

    /// Immutable evidence identity.
    #[must_use]
    pub const fn evidence_oid(self) -> ObjectId {
        self.evidence_oid
    }

    /// Filtration or observation-window identity.
    #[must_use]
    pub const fn filtration_or_window_oid(self) -> ObjectId {
        self.filtration_or_window_oid
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

    /// Candidate-or-fallback selection class.
    #[must_use]
    pub fn selection(self) -> StatisticalPolicySelection {
        if self.selected_policy_oid == self.candidate_decision_oid {
            StatisticalPolicySelection::Candidate
        } else {
            StatisticalPolicySelection::PinnedFallback
        }
    }

    /// Exact typed statistic.
    #[must_use]
    pub const fn statistic(self) -> StatisticalStatistic {
        self.statistic
    }

    /// Encodes this record under the strict version-1 canonical format.
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
        push_oid(&mut bytes, self.monitor_oid);
        push_oid(&mut bytes, self.evidence_oid);
        push_oid(&mut bytes, self.filtration_or_window_oid);
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

    /// Decodes exactly one strict version-1 canonical record.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, StatisticalLogCodecError> {
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
        let monitor_oid = decoder.read_oid()?;
        let evidence_oid = decoder.read_oid()?;
        let filtration_or_window_oid = decoder.read_oid()?;
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
        let batch = StatisticalBatchRange::try_new(first, last)
            .map_err(StatisticalLogCodecError::InvalidRecord)?;
        Self::try_from_parts(
            monitor_kind,
            monitor_oid,
            evidence_oid,
            filtration_or_window_oid,
            batch,
            regime_epoch,
            candidate_decision_oid,
            pinned_fallback_oid,
            selected_policy_oid,
            statistic,
        )
        .map_err(StatisticalLogCodecError::InvalidRecord)
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
    last_by_monitor: [Option<StatisticalLogRecord>; 6],
}

impl StatisticalDecisionLog {
    /// Constructs an empty log with a finite hard record bound.
    pub const fn try_new(maximum_records: usize) -> Result<Self, StatisticalLogAppendError> {
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
            last_by_monitor: [None; 6],
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
        let slot = monitor_slot(record.monitor_kind);
        if let Some(previous) = self.last_by_monitor[slot] {
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
        self.records.push(record);
        self.last_by_monitor[slot] = Some(record);
        Ok(())
    }

    /// Encodes the bound and every record under the strict version-1 format.
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

    /// Decodes a complete strict version-1 log and replays its append checks.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, StatisticalLogCodecError> {
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
        if record_count > maximum_records {
            return Err(StatisticalLogCodecError::RecordCountExceedsLimit {
                count: record_count,
                maximum: maximum_records,
            });
        }
        let mut log =
            Self::try_new(maximum_records).map_err(StatisticalLogCodecError::InvalidLogBound)?;
        log.records.try_reserve_exact(record_count).map_err(|_| {
            StatisticalLogCodecError::AllocationFailed {
                requested: record_count,
            }
        })?;
        for index in 0..record_count {
            let record_len = usize::try_from(decoder.read_u32()?)
                .map_err(|_| StatisticalLogCodecError::LengthOverflow)?;
            if record_len > MAX_CANONICAL_RECORD_BYTES {
                return Err(StatisticalLogCodecError::RecordFrameTooLarge {
                    index,
                    actual: record_len,
                    maximum: MAX_CANONICAL_RECORD_BYTES,
                });
            }
            let record_bytes = decoder.read_bytes(record_len)?;
            let record = StatisticalLogRecord::decode_canonical(record_bytes)?;
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
    EValue,
    RejectionThreshold,
    OneObservations,
    ConformalThreshold,
    NonconformityScore,
    CoverageTarget,
    CoveredAssessments,
    ResidualRate,
    ExplorationUpperBound,
    TargetResidualRate,
    Discoveries,
    CurrentPotential,
    ConfidenceBound,
    Detections,
    ZeroSupportExclusions,
}

impl fmt::Display for StatisticField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EValue => "e-value",
            Self::RejectionThreshold => "rejection threshold",
            Self::OneObservations => "binary-one observations",
            Self::ConformalThreshold => "conformal threshold",
            Self::NonconformityScore => "nonconformity score",
            Self::CoverageTarget => "coverage target",
            Self::CoveredAssessments => "covered assessments",
            Self::ResidualRate => "residual rate",
            Self::ExplorationUpperBound => "exploration upper bound",
            Self::TargetResidualRate => "target residual rate",
            Self::Discoveries => "discoveries",
            Self::CurrentPotential => "current potential",
            Self::ConfidenceBound => "confidence bound",
            Self::Detections => "detections",
            Self::ZeroSupportExclusions => "zero-support exclusions",
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
    EvidenceCounterUnrepresentable {
        monitor: StatisticalMonitorKind,
        field: StatisticField,
    },
    EvidenceEstimateDenominatorMismatch {
        candidate: u128,
        fallback: u128,
    },
    MissingRegimeDetectorSnapshot,
    CandidateEqualsFallback,
    SelectedPolicyIsNeitherCandidateNorFallback {
        selected: ObjectId,
        candidate: ObjectId,
        fallback: ObjectId,
    },
    MonitorStatisticMismatch {
        monitor: StatisticalMonitorKind,
        statistic_tag: u8,
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
    EffectiveSampleSizeDenominatorMismatch {
        numerator: u128,
        denominator: u128,
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
            Self::EvidenceCounterUnrepresentable { monitor, field } => write!(
                formatter,
                "{monitor:?} evidence counter {field} is not representable as u64"
            ),
            Self::EvidenceEstimateDenominatorMismatch {
                candidate,
                fallback,
            } => write!(
                formatter,
                "candidate estimate denominator {candidate} differs from fallback denominator {fallback}"
            ),
            Self::MissingRegimeDetectorSnapshot => {
                formatter.write_str("regime evidence contains no detector snapshot")
            }
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
            Self::MonitorStatisticMismatch {
                monitor,
                statistic_tag,
            } => write!(
                formatter,
                "monitor {monitor:?} cannot carry statistic tag {statistic_tag}"
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
            Self::EffectiveSampleSizeDenominatorMismatch {
                numerator,
                denominator,
            } => write!(
                formatter,
                "ESS numerator {numerator} is nonzero with denominator {denominator}"
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
    InvalidRecord(StatisticalLogRecordError),
    InvalidLogBound(StatisticalLogAppendError),
    InvalidAppend(StatisticalLogAppendError),
    RecordCountExceedsLimit {
        count: usize,
        maximum: usize,
    },
    RecordFrameTooLarge {
        index: usize,
        actual: usize,
        maximum: usize,
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
            Self::RecordFrameTooLarge {
                index,
                actual,
                maximum,
            } => write!(
                formatter,
                "canonical record frame {index} length {actual} exceeds {maximum}"
            ),
            Self::LengthOverflow => formatter.write_str("canonical length arithmetic overflowed"),
            Self::AllocationFailed { requested } => {
                write!(formatter, "could not allocate {requested} canonical bytes")
            }
        }
    }
}

impl std::error::Error for StatisticalLogCodecError {}

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

const fn monitor_slot(monitor: StatisticalMonitorKind) -> usize {
    match monitor {
        StatisticalMonitorKind::EProcess => 0,
        StatisticalMonitorKind::ConformalThreshold => 1,
        StatisticalMonitorKind::ExplorationBudget => 2,
        StatisticalMonitorKind::DrainProgress => 3,
        StatisticalMonitorKind::RegimeChange => 4,
        StatisticalMonitorKind::OffPolicyEvaluation => 5,
    }
}

fn canonical_record_order(left: &StatisticalLogRecord, right: &StatisticalLogRecord) -> Ordering {
    left.batch
        .first
        .cmp(&right.batch.first)
        .then_with(|| left.batch.last.cmp(&right.batch.last))
        .then_with(|| {
            left.monitor_kind
                .canonical_tag()
                .cmp(&right.monitor_kind.canonical_tag())
        })
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
            e_value_bits,
            rejection_threshold_bits,
            observations,
            one_observations,
        } => {
            push_u64(bytes, e_value_bits);
            push_u64(bytes, rejection_threshold_bits);
            push_u64(bytes, observations);
            push_u64(bytes, one_observations);
        }
        StatisticalStatistic::ConformalCoverage {
            threshold_bits,
            nonconformity_score_bits,
            coverage_target_bits,
            assessments,
            covered,
        } => {
            push_u64(bytes, threshold_bits);
            push_u64(bytes, nonconformity_score_bits);
            push_u64(bytes, coverage_target_bits);
            push_u64(bytes, assessments);
            push_u64(bytes, covered);
        }
        StatisticalStatistic::ExplorationBudget {
            residual_rate_bits,
            upper_bound_bits,
            target_rate_bits,
            total_runs,
            discoveries,
            recommended_additional_runs,
        } => {
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
            candidate_numerator,
            fallback_numerator,
            common_denominator,
            candidate_ess_numerator,
            candidate_ess_denominator,
            observations,
            zero_support_exclusions,
        } => {
            push_i128(bytes, candidate_numerator);
            push_i128(bytes, fallback_numerator);
            push_u128(bytes, common_denominator);
            push_u128(bytes, candidate_ess_numerator);
            push_u128(bytes, candidate_ess_denominator);
            push_u64(bytes, observations);
            push_u64(bytes, zero_support_exclusions);
        }
    }
}

fn decode_statistic(
    tag: u8,
    payload: &[u8],
) -> Result<StatisticalStatistic, StatisticalLogCodecError> {
    let expected = match tag {
        1 | 5 => 32,
        2 => 40,
        3 => 48,
        4 => 25,
        6 => 96,
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
            e_value_bits: decoder.read_u64()?,
            rejection_threshold_bits: decoder.read_u64()?,
            observations: decoder.read_u64()?,
            one_observations: decoder.read_u64()?,
        },
        2 => StatisticalStatistic::ConformalCoverage {
            threshold_bits: decoder.read_u64()?,
            nonconformity_score_bits: decoder.read_u64()?,
            coverage_target_bits: decoder.read_u64()?,
            assessments: decoder.read_u64()?,
            covered: decoder.read_u64()?,
        },
        3 => StatisticalStatistic::ExplorationBudget {
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
            candidate_numerator: decoder.read_i128()?,
            fallback_numerator: decoder.read_i128()?,
            common_denominator: decoder.read_u128()?,
            candidate_ess_numerator: decoder.read_u128()?,
            candidate_ess_denominator: decoder.read_u128()?,
            observations: decoder.read_u64()?,
            zero_support_exclusions: decoder.read_u64()?,
        },
        _ => return Err(StatisticalLogCodecError::UnknownStatisticKind { tag }),
    };
    decoder.finish()?;
    Ok(statistic)
}

fn push_oid(bytes: &mut Vec<u8>, oid: ObjectId) {
    bytes.extend_from_slice(oid.as_bytes());
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_i64(bytes: &mut Vec<u8>, value: i64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_u128(bytes: &mut Vec<u8>, value: u128) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn push_i128(bytes: &mut Vec<u8>, value: i128) {
    bytes.extend_from_slice(&value.to_be_bytes());
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
        Ok(u16::from_be_bytes(self.read_array::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32, StatisticalLogCodecError> {
        Ok(u32::from_be_bytes(self.read_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64, StatisticalLogCodecError> {
        Ok(u64::from_be_bytes(self.read_array::<8>()?))
    }

    fn read_i64(&mut self) -> Result<i64, StatisticalLogCodecError> {
        Ok(i64::from_be_bytes(self.read_array::<8>()?))
    }

    fn read_u128(&mut self) -> Result<u128, StatisticalLogCodecError> {
        Ok(u128::from_be_bytes(self.read_array::<16>()?))
    }

    fn read_i128(&mut self) -> Result<i128, StatisticalLogCodecError> {
        Ok(i128::from_be_bytes(self.read_array::<16>()?))
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

    use crate::progress::{
        DrainProgressIdentity, DrainProgressMonitor, DrainProgressProfile, SequencedPotential,
    };

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn oid(fill: u8) -> ObjectId {
        ObjectId([fill; 32])
    }

    fn eprocess_record(
        first: u64,
        last: u64,
    ) -> Result<StatisticalLogRecord, StatisticalLogRecordError> {
        StatisticalLogRecord::try_from_parts(
            StatisticalMonitorKind::EProcess,
            oid(1),
            oid(2),
            oid(3),
            StatisticalBatchRange::try_new(first, last)?,
            7,
            oid(4),
            oid(5),
            oid(4),
            StatisticalStatistic::EProcess {
                e_value_bits: 12.5_f64.to_bits(),
                rejection_threshold_bits: 20.0_f64.to_bits(),
                observations: 10,
                one_observations: 8,
            },
        )
    }

    fn conformal_record(
        first: u64,
        last: u64,
    ) -> Result<StatisticalLogRecord, StatisticalLogRecordError> {
        StatisticalLogRecord::try_from_parts(
            StatisticalMonitorKind::ConformalThreshold,
            oid(11),
            oid(12),
            oid(13),
            StatisticalBatchRange::try_new(first, last)?,
            7,
            oid(14),
            oid(15),
            oid(15),
            StatisticalStatistic::ConformalCoverage {
                threshold_bits: 2.0_f64.to_bits(),
                nonconformity_score_bits: 1.0_f64.to_bits(),
                coverage_target_bits: 0.8_f64.to_bits(),
                assessments: 10,
                covered: 8,
            },
        )
    }

    fn records_for_every_monitor() -> Result<Vec<StatisticalLogRecord>, StatisticalLogRecordError> {
        Ok(vec![
            eprocess_record(10, 19)?,
            StatisticalLogRecord::try_from_parts(
                StatisticalMonitorKind::ConformalThreshold,
                oid(11),
                oid(12),
                oid(13),
                StatisticalBatchRange::try_new(20, 29)?,
                7,
                oid(14),
                oid(15),
                oid(15),
                StatisticalStatistic::ConformalCoverage {
                    threshold_bits: f64::INFINITY.to_bits(),
                    nonconformity_score_bits: 3.0_f64.to_bits(),
                    coverage_target_bits: 0.8_f64.to_bits(),
                    assessments: 10,
                    covered: 8,
                },
            )?,
            StatisticalLogRecord::try_from_parts(
                StatisticalMonitorKind::ExplorationBudget,
                oid(21),
                oid(22),
                oid(23),
                StatisticalBatchRange::try_new(30, 39)?,
                8,
                oid(24),
                oid(25),
                oid(24),
                StatisticalStatistic::ExplorationBudget {
                    residual_rate_bits: 0.1_f64.to_bits(),
                    upper_bound_bits: 0.2_f64.to_bits(),
                    target_rate_bits: 0.2_f64.to_bits(),
                    total_runs: 10,
                    discoveries: 1,
                    recommended_additional_runs: 0,
                },
            )?,
            StatisticalLogRecord::try_from_parts(
                StatisticalMonitorKind::DrainProgress,
                oid(31),
                oid(32),
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
                StatisticalMonitorKind::RegimeChange,
                oid(41),
                oid(42),
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
            StatisticalLogRecord::try_from_parts(
                StatisticalMonitorKind::OffPolicyEvaluation,
                oid(51),
                oid(52),
                oid(53),
                StatisticalBatchRange::try_new(60, 69)?,
                9,
                oid(54),
                oid(55),
                oid(54),
                StatisticalStatistic::OffPolicyEvaluation {
                    candidate_numerator: 90,
                    fallback_numerator: 70,
                    common_denominator: 100,
                    candidate_ess_numerator: 400,
                    candidate_ess_denominator: 100,
                    observations: 10,
                    zero_support_exclusions: 0,
                },
            )?,
        ])
    }

    #[test]
    fn record_is_statistical_and_policy_consistent() -> TestResult {
        let candidate = eprocess_record(1, 2)?;
        assert_eq!(candidate.claim_class(), RegistryClaimClass::Statistical);
        assert_eq!(candidate.selection(), StatisticalPolicySelection::Candidate);

        let fallback = StatisticalLogRecord::try_from_parts(
            StatisticalMonitorKind::EProcess,
            oid(1),
            oid(2),
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
            StatisticalLogRecord::try_from_progress(oid(65), &monitor.evidence()),
            Err(StatisticalLogRecordError::EvidenceHasNoObservations {
                monitor: StatisticalMonitorKind::DrainProgress,
            })
        ));

        let evidence = monitor.observe(SequencedPotential::new(identity, profile, 100, 10.0))?;
        let record = StatisticalLogRecord::try_from_progress(oid(65), &evidence)?;
        assert_eq!(record.monitor_kind(), StatisticalMonitorKind::DrainProgress);
        assert_eq!(record.monitor_oid(), oid(61));
        assert_eq!(record.evidence_oid(), oid(65));
        assert_eq!(record.filtration_or_window_oid(), oid(62));
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
    fn invalid_identity_and_statistic_combinations_are_rejected() -> TestResult {
        let batch = StatisticalBatchRange::try_new(1, 1)?;
        let statistic = eprocess_record(1, 1)?.statistic();
        assert_eq!(
            StatisticalLogRecord::try_from_parts(
                StatisticalMonitorKind::EProcess,
                oid(1),
                oid(2),
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
                StatisticalMonitorKind::EProcess,
                oid(1),
                oid(2),
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
                StatisticalMonitorKind::DrainProgress,
                oid(1),
                oid(2),
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
        let negative_zero = StatisticalStatistic::EProcess {
            e_value_bits: (-0.0_f64).to_bits(),
            rejection_threshold_bits: 2.0_f64.to_bits(),
            observations: 1,
            one_observations: 0,
        };
        assert!(matches!(
            StatisticalLogRecord::try_from_parts(
                StatisticalMonitorKind::EProcess,
                oid(1),
                oid(2),
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

        let invalid_counts = StatisticalStatistic::ConformalCoverage {
            threshold_bits: 1.0_f64.to_bits(),
            nonconformity_score_bits: 0.0_f64.to_bits(),
            coverage_target_bits: 0.8_f64.to_bits(),
            assessments: 2,
            covered: 3,
        };
        assert!(matches!(
            StatisticalLogRecord::try_from_parts(
                StatisticalMonitorKind::ConformalThreshold,
                oid(1),
                oid(2),
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
            StatisticalMonitorKind::ConformalThreshold,
            oid(1),
            oid(2),
            oid(3),
            StatisticalBatchRange::try_new(1, 1)?,
            0,
            oid(4),
            oid(5),
            oid(5),
            StatisticalStatistic::ConformalCoverage {
                threshold_bits: (-2.0_f64).to_bits(),
                nonconformity_score_bits: (-1.0_f64).to_bits(),
                coverage_target_bits: 0.8_f64.to_bits(),
                assessments: 1,
                covered: 1,
            },
        )?;
        assert_eq!(
            signed_conformal.monitor_kind(),
            StatisticalMonitorKind::ConformalThreshold
        );

        let invalid_infinity = StatisticalStatistic::ConformalCoverage {
            threshold_bits: f64::NEG_INFINITY.to_bits(),
            nonconformity_score_bits: 0.0_f64.to_bits(),
            coverage_target_bits: 0.8_f64.to_bits(),
            assessments: 1,
            covered: 1,
        };
        assert!(matches!(
            StatisticalLogRecord::try_from_parts(
                StatisticalMonitorKind::ConformalThreshold,
                oid(1),
                oid(2),
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
            let decoded = StatisticalLogRecord::decode_canonical(&encoded)?;
            assert_eq!(decoded, record);
            assert_eq!(decoded.encode_canonical()?, encoded);
        }
        Ok(())
    }

    #[test]
    fn ordered_append_rejections_are_atomic() -> TestResult {
        let mut log = StatisticalDecisionLog::try_new(3)?;
        log.append(eprocess_record(10, 19)?)?;
        let before = log.encode_canonical()?;
        let overlap = log.append(eprocess_record(19, 25)?);
        assert!(matches!(
            overlap,
            Err(StatisticalLogAppendError::BatchNotStrictlyAfterMonitor { .. })
        ));
        assert_eq!(log.encode_canonical()?, before);

        let out_of_order = log.append(eprocess_record(1, 9)?);
        assert!(matches!(
            out_of_order,
            Err(StatisticalLogAppendError::BatchNotStrictlyAfterMonitor { .. })
        ));
        assert_eq!(log.encode_canonical()?, before);
        assert_eq!(log.len(), 1);
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
        assert_eq!(StatisticalDecisionLog::decode_canonical(&canonical)?, log);

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

        let decoded = StatisticalDecisionLog::decode_canonical(&left_bytes)?;
        assert_eq!(decoded, left);
        assert_eq!(decoded.maximum_records(), 16);
        assert_eq!(decoded.encode_canonical()?, left_bytes);
        Ok(())
    }

    #[test]
    fn strict_decoder_rejects_trailing_and_nonstatistical_bytes() -> TestResult {
        let record = eprocess_record(1, 1)?;
        let mut trailing = record.encode_canonical()?;
        trailing.push(0);
        assert!(matches!(
            StatisticalLogRecord::decode_canonical(&trailing),
            Err(StatisticalLogCodecError::TrailingBytes { .. })
        ));

        let mut wrong_claim = record.encode_canonical()?;
        let claim_offset = 11;
        let claim = wrong_claim
            .get_mut(claim_offset)
            .ok_or_else(|| std::io::Error::other("encoded record did not contain its claim tag"))?;
        *claim = 0xff;
        assert_eq!(
            StatisticalLogRecord::decode_canonical(&wrong_claim),
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
            StatisticalDecisionLog::decode_canonical(truncated),
            Err(StatisticalLogCodecError::Truncated { .. })
        ));

        let mut wrong_version = encoded;
        let version_low = wrong_version
            .get_mut(9)
            .ok_or_else(|| std::io::Error::other("encoded log did not contain its version"))?;
        *version_low = 2;
        assert!(matches!(
            StatisticalDecisionLog::decode_canonical(&wrong_version),
            Err(StatisticalLogCodecError::UnsupportedVersion {
                domain: CanonicalDomain::Log,
                ..
            })
        ));
        Ok(())
    }
}
