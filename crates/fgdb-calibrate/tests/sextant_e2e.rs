#![forbid(unsafe_code)]

use std::{error::Error, io};

use asupersync::runtime::changepoint::ChangeDirection;
use fgdb_calibrate::{
    conformal::{
        AssessmentDisposition, AssessmentEvidence, CalibrationEvidence, ConformalProfile,
        GraphMetricConformal, GraphMetricIdentity, MetricThresholdMode,
        PolicySelection as ConformalSelection, SequenceWindow as ConformalWindow,
        SequencedMetricValue,
    },
    eprocess::{
        BinaryObservation, EProcessConfig, EProcessProfile, EProcessTrial, EvidenceRecord,
        PolicyOutcomeKind, SequenceWindow as EProcessWindow, SequencedObservation, TrialIdentity,
    },
    exploration::{
        ExplorationAssumptionAttestation, ExplorationBudgetConfig, ExplorationBudgetEvidence,
        ExplorationBudgetIdentity, ExplorationBudgetMonitor, ExplorationBudgetProfile,
        ExplorationDisposition, ExplorationSelection, SequencedNovelty,
    },
    ope::{
        LoggedAction, LoggedDecision, OUTCOME_SCALE, OpeEstimator, OpeEvidence, OpeIdentity,
        OpeLedger, OpeProfile, OpeSelection, OpeSelectionReason, OpeWindow, Outcome,
        PROBABILITY_SCALE, Probability, WEIGHT_SCALE,
    },
    policy_epoch::{DecisionPolicyEpoch, DecisionPolicyScope, LogicalEffectClass},
    regime::{
        COMBINED_REGIME_SIGNAL_ID, COMBINED_REGIME_SIGNAL_VERSION, CusumConfig, MetricSample,
        PageHinkleyConfig, RegimePolicySelection, RegimeSequenceWindow, RegimeSignalEvidence,
        RegimeSignalIdentity, RegimeSignalMonitor, RegimeSignalProfile, RegimeSignalStatus,
        RuntimeMetricSeries,
    },
};
use fgdb_claim::EvidenceClaim;
use fgdb_evidence::{CalibrationWindow, EvidenceEnvelope, FallbackBehavior};
use fgdb_types::ObjectId;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const REGIME_EPOCH: u64 = 7;
const CANDIDATE_ID: &str = "policy:candidate-v2";
const FALLBACK_ID: &str = "policy:fallback-v1";

#[derive(Debug, PartialEq)]
struct FixtureRun {
    exploration: Vec<ExplorationBudgetEvidence>,
    calibration: Vec<CalibrationEvidence>,
    assessment: AssessmentEvidence,
    sequential_evidence: Vec<EvidenceRecord>,
    ope: OpeEvidence,
    envelope: EvidenceEnvelope,
    promoted_epoch: DecisionPolicyEpoch,
    promoted_epoch_bytes: Vec<u8>,
    regime_evidence: Vec<RegimeSignalEvidence>,
}

fn oid(fill: u8) -> ObjectId {
    ObjectId([fill; 32])
}

fn run_exploration(
    candidate_oid: ObjectId,
    fallback_oid: ObjectId,
) -> TestResult<Vec<ExplorationBudgetEvidence>> {
    let identity = ExplorationBudgetIdentity::try_new(
        oid(1),
        oid(2),
        oid(3),
        REGIME_EPOCH,
        10,
        11,
        candidate_oid,
        fallback_oid,
    )?;
    let profile = ExplorationBudgetProfile::try_new(
        ExplorationBudgetConfig {
            alpha: 0.5,
            target_coverage: 0.5,
            min_samples: 2,
            max_additional_runs: 4,
        },
        2,
        1_000,
    )?;
    let mut monitor = ExplorationBudgetMonitor::try_new(
        identity,
        profile.clone(),
        ExplorationAssumptionAttestation::fully_supported(),
    )?;
    let mut evidence = Vec::new();
    for sequence in 10..=11 {
        evidence.push(monitor.observe(SequencedNovelty::new(
            identity,
            profile.clone(),
            sequence,
            false,
        ))?);
    }
    Ok(evidence)
}

fn run_conformal() -> TestResult<(Vec<CalibrationEvidence>, AssessmentEvidence)> {
    let identity = GraphMetricIdentity::try_new(
        "metric:sextant-latency",
        "population:fixed-e2e-stream",
        "selection:complete-window-v1",
        ConformalWindow::try_new(20, 30)?,
        REGIME_EPOCH,
        CANDIDATE_ID,
        FALLBACK_ID,
    )?;
    let profile = ConformalProfile::try_new(0.2, MetricThresholdMode::Upper, 5, 10)?;
    let mut trial = GraphMetricConformal::try_new(identity.clone(), profile.clone())?;
    let mut calibration = Vec::new();
    for (offset, value) in (1_u64..=10).enumerate() {
        let sequence = 20_u64
            .checked_add(u64::try_from(offset)?)
            .ok_or_else(|| io::Error::other("conformal fixture sequence arithmetic overflowed"))?;
        calibration.push(trial.calibrate(SequencedMetricValue::new(
            identity.clone(),
            profile.clone(),
            sequence,
            value as f64,
        ))?);
    }
    let assessment = trial.assess(SequencedMetricValue::new(identity, profile, 30, 5.0))?;
    Ok((calibration, assessment))
}

fn run_eprocess() -> TestResult<Vec<EvidenceRecord>> {
    let identity = TrialIdentity::try_new(
        "monitor:sextant-promotion",
        "filtration:fixed-commit-stream",
        EProcessWindow::try_new(40, 42)?,
        REGIME_EPOCH,
        CANDIDATE_ID,
        FALLBACK_ID,
    )?;
    let profile = EProcessProfile::try_new(
        "profile:sextant-eprocess-v1",
        EProcessConfig {
            p0: 0.2,
            lambda: 1.0,
            alpha: 0.25,
            max_evalue: 1_000.0,
        },
    )?;
    let mut trial = EProcessTrial::try_new(identity.clone(), profile.clone())?;
    let mut evidence = Vec::new();
    for sequence in 40..=42 {
        evidence.push(
            trial
                .observe(SequencedObservation::new(
                    identity.clone(),
                    profile.clone(),
                    sequence,
                    BinaryObservation::One,
                ))?
                .evidence,
        );
    }
    Ok(evidence)
}

fn logged_decision(sequence: u64, selected_a: bool) -> TestResult<LoggedDecision> {
    let half = Probability::try_from_numerator(PROBABILITY_SCALE / 2)?;
    let one = Outcome::try_from_scaled(OUTCOME_SCALE)?;
    let zero = Outcome::try_from_scaled(0)?;
    let action_a = oid(20);
    let action_b = oid(21);
    let selected_action = if selected_a { action_a } else { action_b };
    let observed_outcome = if selected_a { one } else { zero };

    Ok(LoggedDecision::try_new(
        sequence,
        oid(30),
        oid(31),
        oid(32),
        selected_action,
        observed_outcome,
        vec![
            LoggedAction::new(
                action_a,
                half,
                Probability::one(),
                Probability::zero(),
                Some(one),
            ),
            LoggedAction::new(
                action_b,
                half,
                Probability::zero(),
                Probability::one(),
                Some(zero),
            ),
        ],
    )?)
}

fn run_ope(candidate_oid: ObjectId, fallback_oid: ObjectId) -> TestResult<OpeEvidence> {
    let identity = OpeIdentity::try_new(
        oid(33),
        OpeWindow::try_new(50, 53)?,
        oid(34),
        oid(35),
        oid(36),
        oid(37),
        REGIME_EPOCH,
        candidate_oid,
        fallback_oid,
        oid(38),
        OpeEstimator::DoublyRobust,
    )?;
    let profile = OpeProfile::try_new(10 * WEIGHT_SCALE, 2, 4, 2, 8)?;
    let mut ledger = OpeLedger::try_new(identity, profile)?;
    for sequence in 50..=53 {
        ledger.record(logged_decision(sequence, sequence % 2 == 0)?)?;
    }
    Ok(ledger.evidence()?)
}

fn promote_epoch(
    candidate_oid: ObjectId,
    fallback_oid: ObjectId,
) -> TestResult<(EvidenceEnvelope, DecisionPolicyEpoch, Vec<u8>)> {
    let evidence_oid = oid(80);
    let envelope = EvidenceEnvelope::new(
        EvidenceClaim::StatisticalClaim {
            population: "fixed-sextant-e2e-stream".into(),
            sampling_rule: "complete-sequenced-fixture-v1".into(),
            alpha: 0.25,
            power_or_effective_sample_size: "eprocess-crossed;ope-n_eff>=2".into(),
            assumptions: vec![
                "registered-null-and-filtration".into(),
                "exchangeable-exploration-runs".into(),
                "logged-action-support".into(),
            ],
        },
        evidence_oid,
        candidate_oid,
        Some(CalibrationWindow::new(10, 54)?),
        REGIME_EPOCH,
        FallbackBehavior::DeterministicPolicy {
            policy_oid: fallback_oid,
        },
    );
    let predecessor = DecisionPolicyEpoch::try_root(
        "policy:sextant-e2e",
        0,
        DecisionPolicyScope::new(oid(70)),
        LogicalEffectClass::AnswerAffectingExecution,
        fallback_oid,
        fallback_oid,
    )?;
    let predecessor_oid = oid(71);
    let promoted = DecisionPolicyEpoch::try_promote(
        &predecessor,
        predecessor_oid,
        candidate_oid,
        &[evidence_oid],
        std::slice::from_ref(&envelope),
    )?;
    let encoded = promoted.try_to_canonical_bytes()?;
    let decoded = DecisionPolicyEpoch::try_promoted_from_canonical_bytes(
        &encoded,
        &predecessor,
        predecessor_oid,
        std::slice::from_ref(&envelope),
    )?;
    assert_eq!(decoded, promoted);
    Ok((envelope, promoted, encoded))
}

fn run_regime_monitor(
    candidate_oid: ObjectId,
    fallback_oid: ObjectId,
) -> TestResult<Vec<RegimeSignalEvidence>> {
    let profile_oid = oid(61);
    let profile = RegimeSignalProfile::try_new(
        profile_oid,
        RuntimeMetricSeries::Custom(17),
        PageHinkleyConfig {
            tolerance: MetricSample::from_micro_units(0),
            threshold: 10 * MetricSample::SCALE,
            reset_after_detection: true,
        },
        CusumConfig {
            baseline: MetricSample::from_units(10),
            drift: MetricSample::from_micro_units(0),
            threshold: 100 * MetricSample::SCALE,
            direction: ChangeDirection::Increase,
            reset_after_detection: true,
        },
        CusumConfig {
            baseline: MetricSample::from_units(10),
            drift: MetricSample::from_micro_units(0),
            threshold: 100 * MetricSample::SCALE,
            direction: ChangeDirection::Decrease,
            reset_after_detection: true,
        },
        8,
        4,
    )?;
    let identity = RegimeSignalIdentity::try_new(
        oid(60),
        oid(62),
        profile_oid,
        COMBINED_REGIME_SIGNAL_ID,
        COMBINED_REGIME_SIGNAL_VERSION,
        RegimeSequenceWindow::try_new(60, 67)?,
        REGIME_EPOCH,
        candidate_oid,
        fallback_oid,
    )?;
    let mut monitor = RegimeSignalMonitor::try_new(identity.clone(), profile.clone())?;
    let mut evidence = Vec::new();
    for (offset, units) in [10, 10, 10, 10, 10, 30, 10, 10].into_iter().enumerate() {
        let sequence = 60_u64
            .checked_add(u64::try_from(offset)?)
            .ok_or_else(|| io::Error::other("regime fixture sequence arithmetic overflowed"))?;
        evidence.push(
            monitor
                .observe(fgdb_calibrate::regime::SequencedRegimeSample::new(
                    identity.clone(),
                    profile.clone(),
                    sequence,
                    MetricSample::from_units(units),
                ))?
                .evidence,
        );
    }
    Ok(evidence)
}

fn run_fixture() -> TestResult<FixtureRun> {
    let candidate_oid = oid(40);
    let fallback_oid = oid(90);
    let exploration = run_exploration(candidate_oid, fallback_oid)?;
    let (calibration, assessment) = run_conformal()?;
    let sequential_evidence = run_eprocess()?;
    let ope = run_ope(candidate_oid, fallback_oid)?;
    let (envelope, promoted_epoch, promoted_epoch_bytes) =
        promote_epoch(candidate_oid, fallback_oid)?;
    let regime_evidence = run_regime_monitor(candidate_oid, fallback_oid)?;

    Ok(FixtureRun {
        exploration,
        calibration,
        assessment,
        sequential_evidence,
        ope,
        envelope,
        promoted_epoch,
        promoted_epoch_bytes,
        regime_evidence,
    })
}

#[test]
fn sextant_evidence_promotes_then_stickily_reverts_on_regime_shift() -> TestResult {
    let first = run_fixture()?;

    let exploration = first
        .exploration
        .last()
        .ok_or_else(|| io::Error::other("exploration fixture produced no evidence"))?;
    assert!(exploration.target_met());
    assert_eq!(
        exploration.disposition(),
        ExplorationDisposition::CandidateSupported
    );
    assert_eq!(
        exploration.selection(),
        ExplorationSelection::CandidateDecision
    );

    let calibration = first
        .calibration
        .last()
        .ok_or_else(|| io::Error::other("conformal fixture produced no calibration evidence"))?;
    assert!(calibration.is_ready());
    assert_eq!(
        first.assessment.disposition(),
        AssessmentDisposition::CandidateConforming
    );
    assert_eq!(
        first.assessment.selection(),
        ConformalSelection::CandidateDecision
    );

    let sequential = first
        .sequential_evidence
        .last()
        .ok_or_else(|| io::Error::other("e-process fixture produced no evidence"))?;
    assert_eq!(
        sequential.outcome().kind(),
        PolicyOutcomeKind::PromoteCandidateAgainstPinnedFallback
    );
    assert_eq!(sequential.outcome().selected_policy_id(), CANDIDATE_ID);

    assert!(first.ope.complete());
    assert_eq!(first.ope.selection(), OpeSelection::Candidate);
    assert_eq!(
        first.ope.selection_reason(),
        OpeSelectionReason::CandidateEstimatedBetter
    );
    assert!(first.ope.candidate_ess_gate_passed());
    assert!(first.ope.fallback_ess_gate_passed());

    assert_eq!(first.promoted_epoch.version(), 1);
    assert_eq!(first.promoted_epoch.pinned_table_oid(), oid(40));
    assert_eq!(first.promoted_epoch.fallback_oid(), oid(90));
    assert_eq!(first.promoted_epoch.evidence_refs(), &[oid(80)]);
    assert_eq!(first.envelope.selection_policy_oid(), oid(40));
    assert_eq!(
        first.envelope.fallback(),
        FallbackBehavior::DeterministicPolicy {
            policy_oid: oid(90)
        }
    );

    let stable = first
        .regime_evidence
        .get(4)
        .ok_or_else(|| io::Error::other("regime fixture omitted its stable prefix"))?;
    assert_eq!(stable.status(), RegimeSignalStatus::NoChangeDetected);
    assert_eq!(stable.selection(), RegimePolicySelection::CandidateDecision);

    let shifted = first
        .regime_evidence
        .get(5)
        .ok_or_else(|| io::Error::other("regime fixture omitted its shift sample"))?;
    assert_eq!(shifted.status(), RegimeSignalStatus::ChangeDetected);
    assert_eq!(shifted.selection(), RegimePolicySelection::PinnedFallback);
    assert_eq!(shifted.fallback_sequence(), Some(65));

    let sticky = first
        .regime_evidence
        .last()
        .ok_or_else(|| io::Error::other("regime fixture produced no evidence"))?;
    assert_eq!(sticky.selection(), RegimePolicySelection::PinnedFallback);
    assert_eq!(sticky.selected_policy_oid(), oid(90));
    assert_eq!(sticky.fallback_sequence(), Some(65));

    let replay = run_fixture()?;
    assert_eq!(first.exploration, replay.exploration);
    assert_eq!(first.calibration, replay.calibration);
    assert_eq!(first.assessment, replay.assessment);
    assert_eq!(first.sequential_evidence, replay.sequential_evidence);
    assert_eq!(first.ope, replay.ope);
    assert_eq!(first.envelope, replay.envelope);
    assert_eq!(first.promoted_epoch, replay.promoted_epoch);
    assert_eq!(first.promoted_epoch_bytes, replay.promoted_epoch_bytes);
    assert_eq!(first.regime_evidence, replay.regime_evidence);
    Ok(())
}
