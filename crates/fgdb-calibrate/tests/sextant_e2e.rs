#![forbid(unsafe_code)]

use std::{
    error::Error,
    fs, io,
    sync::{Arc, Mutex},
};

use asupersync::{
    cancel::progress_certificate::ProgressConfig,
    lab::{LabConfig, LabRuntime},
    runtime::changepoint::ChangeDirection,
    types::Budget,
    util::DetRng,
};
use fgdb_calibrate::{
    ann_recall::{
        AnnRecallAction, AnnRecallActionReason, AnnRecallAssumptions, AnnRecallBinding,
        AnnRecallEvidence, AnnRecallIdentity, AnnRecallLedger, AnnRecallObservation,
        AnnRecallProfile, AnnRecallProfileIdentityVerifier, AnnRecallReplayDecodeLimits,
        AnnRecallWindow, QuerySampleDesign, RECALL_SCALE,
    },
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
    log::{
        StatisticalDecisionLog, StatisticalEvidenceIdentityError,
        StatisticalEvidenceIdentityIssuer, StatisticalEvidenceIdentityVerifier,
        StatisticalEvidenceRegistration, StatisticalLogDecodeLimits, StatisticalLogRecord,
        StatisticalMonitorKind, StatisticalStatistic,
    },
    no_regret::{
        NoRegretActionSpace, NoRegretAssumptions, NoRegretController, NoRegretDecisionReceipt,
        NoRegretDecisionSelection, NoRegretFeedbackReceipt, NoRegretIdentity,
        NoRegretNumericFingerprint, NoRegretProfile, NoRegretProfileIdentityVerifier,
        NoRegretRegime, NoRegretRegimeResetReceipt, NoRegretRegimeShift,
        NoRegretRegimeTransitionAuthority, NoRegretReplayEvent, NoRegretReplayLog,
        NoRegretReplayLogDecodeLimits, NoRegretReplaySummary, NoRegretReplayVerifier,
        NoRegretSelectionMode, NoRegretStateSpaceIdentityVerifier,
    },
    ope::{
        EvaluatedPolicy, LoggedAction, LoggedDecision, OUTCOME_SCALE, OpeEstimator, OpeEvidence,
        OpeIdentity, OpeLedger, OpeProfile, OpeSelection, OpeSelectionReason, OpeWindow, Outcome,
        PROBABILITY_SCALE, Probability, WEIGHT_SCALE,
    },
    policy_epoch::{
        DecisionPolicyEpoch, DecisionPolicyEpochError, DecisionPolicyScope, LogicalEffectClass,
    },
    progress::{
        DrainProgressEvidence, DrainProgressIdentity, DrainProgressMonitor, DrainProgressProfile,
        SequencedPotential,
    },
    regime::{
        COMBINED_REGIME_SIGNAL_ID, COMBINED_REGIME_SIGNAL_VERSION, CusumConfig, MetricSample,
        PageHinkleyConfig, RegimePolicySelection, RegimeSequenceWindow, RegimeSignalEvidence,
        RegimeSignalIdentity, RegimeSignalMonitor, RegimeSignalProfile, RegimeSignalStatus,
        RuntimeMetricSeries,
    },
};
use fgdb_claim::{EvidenceClaim, StatisticalErrorControl};
use fgdb_evidence::{
    EvidenceEnvelope, FallbackBehavior, PropensitySupportIdentity, StrataIdentity,
};
use fgdb_sketch::{
    count_min::{CountMinDecodeLimits, CountMinError, CountMinProfile, CountMinSketch},
    label_counts::{
        LabelCounts, LabelCountsDecodeLimits, LabelCountsDomain, LabelCountsKey, LabelCountsProfile,
    },
    maintenance_log::{
        SketchFamily, SketchMaintenanceLog, SketchMaintenanceLogDecodeLimits,
        SketchMaintenanceOutcome, SketchMaintenanceRecord, SketchStateDigest,
    },
};
use fgdb_types::ObjectId;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
type PromotionResult = (Vec<u8>, Vec<EvidenceEnvelope>, DecisionPolicyEpoch, Vec<u8>);

const REGIME_EPOCH: u64 = 7;
const FIXTURE_LOG_DECODE_LIMITS: StatisticalLogDecodeLimits =
    StatisticalLogDecodeLimits::new(32, 1 << 20);
const SKETCH_MAINTENANCE_MAX_RECORDS: usize = 3;
const SKETCH_MAINTENANCE_DECODE_LIMITS: SketchMaintenanceLogDecodeLimits =
    SketchMaintenanceLogDecodeLimits::new(SKETCH_MAINTENANCE_MAX_RECORDS, 1 << 10);
const ANN_RECALL_QUERY_COUNT: usize = 4;
const ANN_RECALL_TOP_K: usize = 3;
const ANN_RECALL_TOTAL_RESULT_IDS: usize = ANN_RECALL_QUERY_COUNT * ANN_RECALL_TOP_K * 2;
const ANN_RECALL_DECODE_LIMITS: AnnRecallReplayDecodeLimits = AnnRecallReplayDecodeLimits::new(
    1 << 14,
    ANN_RECALL_TOP_K,
    ANN_RECALL_QUERY_COUNT,
    ANN_RECALL_TOTAL_RESULT_IDS,
);
const NO_REGRET_FIRST_SEQUENCE: u64 = 80;
const NO_REGRET_DECISION_COUNT: usize = 3;
const NO_REGRET_REPLAY_SEED: u64 = 0xa11c_e5e5_7a17;

#[derive(Debug, PartialEq)]
struct SketchFixture {
    final_state_bytes: Vec<u8>,
    label_counts_final_state_bytes: Vec<u8>,
    maintenance_log: SketchMaintenanceLog,
    maintenance_log_bytes: Vec<u8>,
}

#[derive(Debug, PartialEq)]
struct AnnRecallFixture {
    evidence: AnnRecallEvidence,
    replay_bytes: Vec<u8>,
}

#[derive(Debug, PartialEq)]
struct NoRegretFixture {
    selections: Vec<NoRegretDecisionSelection>,
    decision_bytes: Vec<Vec<u8>>,
    event_bytes: Vec<Vec<u8>>,
    replay_log_bytes: Vec<u8>,
    replay_summary: NoRegretReplaySummary,
}

#[derive(Debug, PartialEq)]
struct FixtureRun {
    exploration: Vec<ExplorationBudgetEvidence>,
    calibration: Vec<CalibrationEvidence>,
    assessments: Vec<AssessmentEvidence>,
    assessment: AssessmentEvidence,
    sequential_evidence: Vec<EvidenceRecord>,
    ope_prefixes: Vec<OpeEvidence>,
    ope: OpeEvidence,
    progress_evidence: Vec<DrainProgressEvidence>,
    root_epoch_bytes: Vec<u8>,
    promotion_envelopes: Vec<EvidenceEnvelope>,
    promoted_epoch: DecisionPolicyEpoch,
    promoted_epoch_bytes: Vec<u8>,
    regime_evidence: Vec<RegimeSignalEvidence>,
    regime_evidence_oid: ObjectId,
    regime_evidence_bytes: Vec<u8>,
    fallback_envelope: EvidenceEnvelope,
    reverted_epoch: DecisionPolicyEpoch,
    reverted_epoch_bytes: Vec<u8>,
    statistical_log: StatisticalDecisionLog,
    statistical_log_bytes: Vec<u8>,
    sketch: SketchFixture,
    ann_recall: AnnRecallFixture,
    no_regret: NoRegretFixture,
}

fn oid(fill: u8) -> ObjectId {
    ObjectId([fill; 32])
}

/// Explicitly test-scoped identity authority.
///
/// Statistical sub-artifacts use a deterministic test mapping because this
/// fixture does not own database key management. Policy epochs themselves use
/// [`DecisionPolicyEpoch::try_object_id`] with a fixed test key and namespace,
/// so the end-to-end path exercises the production §5.1 identity transcript.
#[derive(Clone, Copy)]
struct FixtureOnlyIdentityAuthority;

const FIXTURE_IDENTITY_AUTHORITY: FixtureOnlyIdentityAuthority = FixtureOnlyIdentityAuthority;
const FIXTURE_K_OID: [u8; 32] = [0xa7; 32];
const FIXTURE_SECURITY_NAMESPACE: fgdb_types::DatabaseSecurityNamespaceId =
    fgdb_types::DatabaseSecurityNamespaceId([0xb8; 32]);

impl FixtureOnlyIdentityAuthority {
    fn issue_for_domain(domain: &[u8], canonical_bytes: &[u8]) -> ObjectId {
        let mut transcript = Vec::with_capacity(
            1_usize
                .saturating_add(domain.len())
                .saturating_add(canonical_bytes.len()),
        );
        transcript.extend_from_slice(domain);
        transcript.push(0);
        transcript.extend_from_slice(canonical_bytes);
        let mut bytes = asupersync::atp::object::compute_hash(&transcript);
        for byte in &mut bytes {
            *byte ^= 0x5a;
        }
        ObjectId(bytes)
    }

    fn statistical_evidence_oid(canonical_evidence_body: &[u8]) -> ObjectId {
        Self::issue_for_domain(
            b"fgdb:fixture-only:statistical-evidence-oid:v1",
            canonical_evidence_body,
        )
    }

    fn epoch_oid(epoch: &DecisionPolicyEpoch) -> TestResult<ObjectId> {
        Ok(epoch.try_object_id(&FIXTURE_K_OID, FIXTURE_SECURITY_NAMESPACE)?)
    }

    fn regime_evidence_oid(canonical_evidence: &[u8]) -> ObjectId {
        Self::issue_for_domain(
            b"fgdb:fixture-only:regime-signal-evidence-oid:v1",
            canonical_evidence,
        )
    }
}

impl StatisticalEvidenceIdentityIssuer for FixtureOnlyIdentityAuthority {
    fn issue_statistical_evidence_oid(
        &self,
        canonical_evidence_body: &[u8],
    ) -> Result<ObjectId, StatisticalEvidenceIdentityError> {
        Ok(Self::statistical_evidence_oid(canonical_evidence_body))
    }
}

impl StatisticalEvidenceIdentityVerifier for FixtureOnlyIdentityAuthority {
    fn verify_statistical_evidence_oid(
        &self,
        canonical_evidence_body: &[u8],
        evidence_oid: ObjectId,
    ) -> Result<(), StatisticalEvidenceIdentityError> {
        if evidence_oid == Self::statistical_evidence_oid(canonical_evidence_body) {
            Ok(())
        } else {
            Err(StatisticalEvidenceIdentityError::Rejected)
        }
    }
}

impl NoRegretStateSpaceIdentityVerifier for FixtureOnlyIdentityAuthority {
    fn verify_state_space_oid(&self, claimed_oid: ObjectId, bytes: &[u8]) -> bool {
        claimed_oid
            == Self::issue_for_domain(b"fgdb:fixture-only:no-regret-action-space-oid:v1", bytes)
    }
}

impl AnnRecallProfileIdentityVerifier for FixtureOnlyIdentityAuthority {
    fn verify_ann_recall_profile_oid(
        &self,
        claimed_oid: ObjectId,
        canonical_profile: &[u8],
    ) -> bool {
        claimed_oid
            == Self::issue_for_domain(
                b"fgdb:fixture-only:ann-recall-profile-oid:v1",
                canonical_profile,
            )
    }
}

impl NoRegretProfileIdentityVerifier for FixtureOnlyIdentityAuthority {
    fn verify_no_regret_profile_oid(
        &self,
        claimed_oid: ObjectId,
        canonical_profile: &[u8],
    ) -> bool {
        claimed_oid
            == Self::issue_for_domain(
                b"fgdb:fixture-only:no-regret-profile-oid:v1",
                canonical_profile,
            )
    }
}

impl NoRegretRegimeTransitionAuthority for FixtureOnlyIdentityAuthority {
    fn verify_regime_shift(&self, shift: NoRegretRegimeShift) -> bool {
        shift.next().regime_oid()
            == Self::issue_for_domain(
                b"fgdb:fixture-only:no-regret-regime-oid:v1",
                &shift.evidence_oid().0,
            )
    }
}

fn fixture_identity(domain: &[u8], descriptor: &[u8]) -> ObjectId {
    FixtureOnlyIdentityAuthority::issue_for_domain(domain, descriptor)
}

fn read_statistical_log(
    bytes: &[u8],
    expected_maximum_records: usize,
) -> Result<StatisticalDecisionLog, fgdb_calibrate::log::StatisticalLogCodecError> {
    StatisticalDecisionLog::decode_canonical(
        bytes,
        expected_maximum_records,
        FIXTURE_LOG_DECODE_LIMITS,
        &FIXTURE_IDENTITY_AUTHORITY,
    )
}

fn persist_regime_evidence(evidence: &RegimeSignalEvidence) -> TestResult<(ObjectId, Vec<u8>)> {
    let bytes = evidence.try_to_canonical_bytes()?;
    let evidence_oid = FixtureOnlyIdentityAuthority::regime_evidence_oid(&bytes);
    Ok((evidence_oid, bytes))
}

fn read_persisted_regime_evidence(
    bytes: &[u8],
    expected_oid: ObjectId,
) -> TestResult<RegimeSignalEvidence> {
    let actual_oid = FixtureOnlyIdentityAuthority::regime_evidence_oid(bytes);
    if actual_oid != expected_oid {
        return Err(io::Error::other(
            "persisted regime evidence failed fixture identity verification",
        )
        .into());
    }
    Ok(RegimeSignalEvidence::try_from_canonical_bytes(bytes)?)
}

fn canonical_count_min_delete(key: &[u8], weight: u64) -> TestResult<Vec<u8>> {
    const DELETE_MAGIC: [u8; 8] = *b"FGDBCMD1";
    const DELETE_VERSION: u16 = 1;
    const FIXED_BYTES: usize =
        DELETE_MAGIC.len() + core::mem::size_of::<u16>() + (2 * core::mem::size_of::<u64>());

    let key_len = u64::try_from(key.len())?;
    let encoded_len = FIXED_BYTES
        .checked_add(key.len())
        .ok_or_else(|| io::Error::other("count-min deletion input length overflowed"))?;
    let mut bytes = Vec::with_capacity(encoded_len);
    bytes.extend_from_slice(&DELETE_MAGIC);
    bytes.extend_from_slice(&DELETE_VERSION.to_be_bytes());
    bytes.extend_from_slice(&weight.to_be_bytes());
    bytes.extend_from_slice(&key_len.to_be_bytes());
    bytes.extend_from_slice(key);
    Ok(bytes)
}

/// Maintains the one sketch family whose deletions apply exactly.
///
/// This is the deliberate counterpart to the count-min phase below: there, a
/// deletion is refused with a typed rebuild requirement and the state must not
/// move; here the same workload applies exactly and the state must move. A
/// StatsSegment carries both kinds, so the E2E has to exercise both.
///
/// The deletion is applied *before* the merge on purpose. The maintenance log's
/// vocabulary is `Merged | RebuildRequired`, so an applied deletion has no
/// record of its own; ordering the merge last keeps the family's final logged
/// `after_digest` an accurate description of the final canonical state rather
/// than a stale one. Records carry no digest chaining, so this is an ordering
/// choice for honest evidence, not a codec requirement.
fn run_label_counts_maintenance(
    maintenance_log: &mut SketchMaintenanceLog,
    sequence: u64,
) -> TestResult<Vec<u8>> {
    const PERSON: &[u8] = b"Person";
    const CITY: &[u8] = b"City";
    const FOLLOWS: &[u8] = b"FOLLOWS";
    const VISITED: &[u8] = b"VISITED";

    let vertex = |name: &'static [u8]| LabelCountsKey::new(LabelCountsDomain::VertexLabel, name);
    let edge = |name: &'static [u8]| LabelCountsKey::new(LabelCountsDomain::EdgeType, name);

    let profile = LabelCountsProfile::new(16, 32, 1_000);
    let profile_state_bytes = LabelCounts::try_new(profile)?.try_to_canonical_bytes()?;
    let sketch_profile_oid = FixtureOnlyIdentityAuthority::issue_for_domain(
        b"fgdb:fixture-only:label-counts-profile-oid:v1",
        &profile_state_bytes,
    );

    let mut maintained = LabelCounts::try_new(profile)?;
    maintained.try_observe(vertex(PERSON), 5)?;
    maintained.try_observe(vertex(CITY), 2)?;
    maintained.try_observe(edge(FOLLOWS), 4)?;
    assert_eq!(maintained.total_count(), 11);
    assert_eq!(maintained.distinct_keys(), 3);
    let before_delete = maintained.try_to_canonical_bytes()?;

    // Exact deletion: it applies, and the state moves. Removing a key's last
    // observation drops the key rather than retaining a zero, which is what
    // keeps canonical bytes a function of logical state alone.
    maintained.try_remove(vertex(PERSON), 2)?;
    maintained.try_remove(vertex(CITY), 2)?;
    assert_eq!(maintained.count(vertex(PERSON)), 3);
    assert_eq!(maintained.count(vertex(CITY)), 0);
    assert_eq!(maintained.total_count(), 7);
    assert_eq!(maintained.distinct_keys(), 2);
    let after_delete = maintained.try_to_canonical_bytes()?;
    assert_ne!(after_delete, before_delete);

    let mut operand = LabelCounts::try_new(profile)?;
    operand.try_observe(vertex(PERSON), 1)?;
    operand.try_observe(edge(VISITED), 6)?;
    let operand_bytes = operand.try_to_canonical_bytes()?;
    let merge_operation_oid = FixtureOnlyIdentityAuthority::issue_for_domain(
        b"fgdb:fixture-only:label-counts-merge-oid:v1",
        &operand_bytes,
    );

    maintained.try_merge(&operand)?;
    assert_eq!(maintained.count(vertex(PERSON)), 4);
    assert_eq!(maintained.count(edge(FOLLOWS)), 4);
    assert_eq!(maintained.count(edge(VISITED)), 6);
    assert_eq!(maintained.total_count(), 14);
    assert_eq!(maintained.distinct_keys(), 3);
    let after_merge = maintained.try_to_canonical_bytes()?;
    assert_ne!(after_merge, after_delete);

    let decoded_state = LabelCounts::try_from_canonical_bytes(
        &after_merge,
        profile,
        LabelCountsDecodeLimits::conservative(),
    )?;
    assert_eq!(decoded_state, maintained);
    assert_eq!(decoded_state.try_to_canonical_bytes()?, after_merge);

    maintenance_log.append(SketchMaintenanceRecord::merged(
        sequence,
        SketchFamily::LabelCounts,
        sketch_profile_oid,
        merge_operation_oid,
        &after_delete,
        &operand_bytes,
        &after_merge,
    ))?;

    Ok(after_merge)
}

fn run_sketch_maintenance() -> TestResult<SketchFixture> {
    const PRIMARY_KEY: &[u8] = b"vertex-label:person";
    const SECONDARY_KEY: &[u8] = b"edge-type:follows";
    const DELETE_WEIGHT: u64 = 2;

    let profile = CountMinProfile::new(16, 4, 0x5e_87_a1_17, 1_000);
    let profile_state_bytes = CountMinSketch::try_new(profile)?.try_to_canonical_bytes()?;
    let sketch_profile_oid = FixtureOnlyIdentityAuthority::issue_for_domain(
        b"fgdb:fixture-only:count-min-profile-oid:v1",
        &profile_state_bytes,
    );

    let mut maintained = CountMinSketch::try_new(profile)?;
    maintained.try_observe(PRIMARY_KEY, 3)?;
    maintained.try_observe(SECONDARY_KEY, 1)?;
    let before_merge = maintained.try_to_canonical_bytes()?;

    let mut operand = CountMinSketch::try_new(profile)?;
    operand.try_observe(PRIMARY_KEY, 2)?;
    operand.try_observe(SECONDARY_KEY, 4)?;
    let operand_bytes = operand.try_to_canonical_bytes()?;
    let merge_operation_oid = FixtureOnlyIdentityAuthority::issue_for_domain(
        b"fgdb:fixture-only:count-min-merge-oid:v1",
        &operand_bytes,
    );

    maintained.try_merge(&operand)?;
    assert_eq!(maintained.total_weight(), 10);
    assert!(maintained.estimate(PRIMARY_KEY) >= 5);
    assert!(maintained.estimate(SECONDARY_KEY) >= 5);
    let after_merge = maintained.try_to_canonical_bytes()?;
    assert_ne!(after_merge, before_merge);

    let decoded_state = CountMinSketch::try_from_canonical_bytes(
        &after_merge,
        profile,
        CountMinDecodeLimits::conservative(),
    )?;
    assert_eq!(decoded_state, maintained);
    assert_eq!(decoded_state.try_to_canonical_bytes()?, after_merge);

    let mut maintenance_log = SketchMaintenanceLog::new(SKETCH_MAINTENANCE_MAX_RECORDS)?;
    maintenance_log.append(SketchMaintenanceRecord::merged(
        0,
        SketchFamily::CountMin,
        sketch_profile_oid,
        merge_operation_oid,
        &before_merge,
        &operand_bytes,
        &after_merge,
    ))?;

    let canonical_deletion = canonical_count_min_delete(PRIMARY_KEY, DELETE_WEIGHT)?;
    let delete_operation_oid = FixtureOnlyIdentityAuthority::issue_for_domain(
        b"fgdb:fixture-only:count-min-delete-oid:v1",
        &canonical_deletion,
    );
    let before_delete = maintained.try_to_canonical_bytes()?;
    match maintained.try_remove(PRIMARY_KEY, DELETE_WEIGHT) {
        Err(CountMinError::RebuildRequired { requested_weight })
            if requested_weight == DELETE_WEIGHT => {}
        outcome => {
            return Err(io::Error::other(format!(
                "count-min deletion did not return its typed rebuild requirement: {outcome:?}"
            ))
            .into());
        }
    }
    let after_delete = maintained.try_to_canonical_bytes()?;
    assert_eq!(after_delete, before_delete);
    maintenance_log.append(SketchMaintenanceRecord::rebuild_required(
        1,
        SketchFamily::CountMin,
        sketch_profile_oid,
        delete_operation_oid,
        &before_delete,
        &canonical_deletion,
    ))?;

    let label_counts_final_state_bytes = run_label_counts_maintenance(&mut maintenance_log, 2)?;

    let maintenance_log_bytes = maintenance_log.to_canonical_bytes()?;
    let decoded_log = SketchMaintenanceLog::from_canonical_bytes(
        &maintenance_log_bytes,
        SKETCH_MAINTENANCE_DECODE_LIMITS,
        SKETCH_MAINTENANCE_MAX_RECORDS,
    )?;
    assert_eq!(decoded_log, maintenance_log);
    assert_eq!(decoded_log.to_canonical_bytes()?, maintenance_log_bytes);

    Ok(SketchFixture {
        final_state_bytes: after_delete,
        label_counts_final_state_bytes,
        maintenance_log,
        maintenance_log_bytes,
    })
}

fn run_ann_recall() -> TestResult<AnnRecallFixture> {
    const FIRST_SEQUENCE: u64 = 70;

    let assumptions = AnnRecallAssumptions::new(
        QuerySampleDesign::KeyedUniformWithoutReplacement,
        true,
        true,
        true,
    );
    let profile_descriptor = AnnRecallProfile::try_canonical_descriptor_bytes(
        ANN_RECALL_TOP_K,
        ANN_RECALL_QUERY_COUNT,
        ANN_RECALL_TOTAL_RESULT_IDS,
        1,
        (RECALL_SCALE * 2) / 5,
        RECALL_SCALE / 4,
        assumptions,
    )?;
    let profile_oid = fixture_identity(
        b"fgdb:fixture-only:ann-recall-profile-oid:v1",
        &profile_descriptor,
    );
    let candidate_policy_oid = fixture_identity(
        b"fgdb:fixture-only:ann-policy-oid:v1",
        b"hnsw-search:ef-search=128",
    );
    let fallback_policy_oid = fixture_identity(
        b"fgdb:fixture-only:ann-policy-oid:v1",
        b"exact-vector-scan:v1",
    );
    let rebuild_policy_oid = fixture_identity(
        b"fgdb:fixture-only:ann-policy-oid:v1",
        b"rebuild-ann-index:v1",
    );
    let last_sequence = FIRST_SEQUENCE
        .checked_add(u64::try_from(ANN_RECALL_QUERY_COUNT - 1)?)
        .ok_or_else(|| io::Error::other("ANN recall fixture window overflowed"))?;
    let identity = AnnRecallIdentity::try_new(
        fixture_identity(
            b"fgdb:fixture-only:ann-monitor-oid:v1",
            b"sextant-fixed-window-recall",
        ),
        profile_oid,
        fixture_identity(
            b"fgdb:fixture-only:ann-population-oid:v1",
            b"authorized-query-population:sextant",
        ),
        fixture_identity(
            b"fgdb:fixture-only:snapshot-oid:v1",
            b"chronicle-sequence:sextant-fixture",
        ),
        fixture_identity(
            b"fgdb:fixture-only:authority-domain-oid:v1",
            b"tenant:sextant-fixture",
        ),
        fixture_identity(
            b"fgdb:fixture-only:sample-key-oid:v1",
            b"keyed-sample:sextant-v1",
        ),
        fixture_identity(
            b"fgdb:fixture-only:sample-design-oid:v1",
            b"keyed-uniform-without-replacement:v1",
        ),
        fixture_identity(
            b"fgdb:fixture-only:exact-baseline-oid:v1",
            b"complete-exact-vector-scan:v1",
        ),
        candidate_policy_oid,
        fallback_policy_oid,
        rebuild_policy_oid,
        AnnRecallWindow::try_new(FIRST_SEQUENCE, last_sequence)?,
        REGIME_EPOCH,
    )?;
    let profile = AnnRecallProfile::try_new_verified(
        profile_oid,
        ANN_RECALL_TOP_K,
        ANN_RECALL_QUERY_COUNT,
        ANN_RECALL_TOTAL_RESULT_IDS,
        1,
        (RECALL_SCALE * 2) / 5,
        RECALL_SCALE / 4,
        assumptions,
        &FIXTURE_IDENTITY_AUTHORITY,
    )?;
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
    let mut ledger =
        AnnRecallLedger::try_new_verified(identity, profile, &FIXTURE_IDENTITY_AUTHORITY)?;
    for offset in 0..ANN_RECALL_QUERY_COUNT {
        let sequence = FIRST_SEQUENCE
            .checked_add(u64::try_from(offset)?)
            .ok_or_else(|| io::Error::other("ANN recall fixture sequence overflowed"))?;
        let sample_descriptor = format!("sample-member:{offset}");
        let query_descriptor = format!("vector-query:{offset}");
        let mut exact_baseline_top_k = (0..ANN_RECALL_TOP_K)
            .map(|rank| {
                let descriptor = format!("query:{offset}:exact-rank:{rank}");
                fixture_identity(
                    b"fgdb:fixture-only:ann-result-object-oid:v1",
                    descriptor.as_bytes(),
                )
            })
            .collect::<Vec<_>>();
        exact_baseline_top_k.sort_unstable();
        let mut candidate_top_k = exact_baseline_top_k.clone();
        if offset == ANN_RECALL_QUERY_COUNT - 1 {
            let _ = candidate_top_k.pop();
            candidate_top_k.push(fixture_identity(
                b"fgdb:fixture-only:ann-result-object-oid:v1",
                b"query:3:candidate-false-positive",
            ));
            candidate_top_k.sort_unstable();
        }
        ledger.record(AnnRecallObservation::try_new(
            sequence,
            fixture_identity(
                b"fgdb:fixture-only:ann-sample-member-oid:v1",
                sample_descriptor.as_bytes(),
            ),
            fixture_identity(
                b"fgdb:fixture-only:ann-query-oid:v1",
                query_descriptor.as_bytes(),
            ),
            binding,
            exact_baseline_top_k,
            candidate_top_k,
        )?)?;
    }

    let evidence = ledger.evidence()?;
    assert!(evidence.complete());
    assert!(evidence.assumptions_supported());
    assert_eq!(evidence.query_observations(), 4);
    assert_eq!(evidence.exact_baseline_results(), 12);
    assert_eq!(evidence.candidate_results(), 12);
    assert_eq!(evidence.intersection_hits(), 11);
    assert_eq!(evidence.action(), Some(AnnRecallAction::Candidate));
    assert_eq!(
        evidence.action_reason(),
        AnnRecallActionReason::CandidateRecallSatisfied
    );
    assert_eq!(evidence.selected_policy_oid(), Some(candidate_policy_oid));

    let replay_bytes = ledger.to_canonical_replay_bytes()?;
    let replayed = AnnRecallLedger::from_canonical_replay_bytes(
        &replay_bytes,
        ANN_RECALL_DECODE_LIMITS,
        identity,
        profile,
        &FIXTURE_IDENTITY_AUTHORITY,
    )?;
    assert_eq!(replayed.observations(), ledger.observations());
    assert_eq!(replayed.evidence()?, evidence);
    assert_eq!(replayed.to_canonical_replay_bytes()?, replay_bytes);

    Ok(AnnRecallFixture {
        evidence,
        replay_bytes,
    })
}

fn no_regret_numeric_fingerprint() -> NoRegretNumericFingerprint {
    NoRegretNumericFingerprint::current(
        fixture_identity(
            b"fgdb:fixture-only:rust-toolchain-oid:v1",
            b"workspace-pinned-nightly-toolchain",
        ),
        fixture_identity(
            b"fgdb:fixture-only:asupersync-revision-oid:v1",
            b"3b34c9b0a14c55eb814cf4aaeceeba3be0c0a72e",
        ),
        fixture_identity(
            b"fgdb:fixture-only:math-abi-oid:v1",
            b"rust-f64-exp:current-target:v1",
        ),
    )
}

fn run_no_regret(
    policy_epoch_oid: ObjectId,
    regime_evidence_oid: ObjectId,
) -> TestResult<NoRegretFixture> {
    let pinned_fallback_oid = fixture_identity(
        b"fgdb:fixture-only:no-regret-policy-oid:v1",
        b"pinned-analytic-fallback:v1",
    );
    let mut policy_oids = vec![
        fixture_identity(
            b"fgdb:fixture-only:no-regret-policy-oid:v1",
            b"compaction-pacing:conservative",
        ),
        fixture_identity(
            b"fgdb:fixture-only:no-regret-policy-oid:v1",
            b"compaction-pacing:balanced",
        ),
        pinned_fallback_oid,
    ];
    policy_oids.sort_unstable();
    let canonical_action_space =
        NoRegretActionSpace::try_canonical_descriptor_bytes(&policy_oids, pinned_fallback_oid)?;
    let state_space_oid = fixture_identity(
        b"fgdb:fixture-only:no-regret-action-space-oid:v1",
        &canonical_action_space,
    );
    let action_space = NoRegretActionSpace::try_new(
        policy_oids.clone(),
        pinned_fallback_oid,
        state_space_oid,
        &FIXTURE_IDENTITY_AUTHORITY,
    )?;
    let retained_receipts = NO_REGRET_DECISION_COUNT + 1;
    let profile_descriptor = NoRegretProfile::try_canonical_descriptor_bytes(
        0.08,
        0.10,
        policy_oids.len(),
        NO_REGRET_DECISION_COUNT,
        2,
        retained_receipts,
    )?;
    let profile_oid = fixture_identity(
        b"fgdb:fixture-only:no-regret-profile-oid:v1",
        &profile_descriptor,
    );
    let profile = NoRegretProfile::try_new_verified(
        profile_oid,
        0.08,
        0.10,
        policy_oids.len(),
        NO_REGRET_DECISION_COUNT,
        2,
        retained_receipts,
        &FIXTURE_IDENTITY_AUTHORITY,
    )?;
    let fingerprint = no_regret_numeric_fingerprint();
    let last_sequence = NO_REGRET_FIRST_SEQUENCE
        .checked_add(u64::try_from(NO_REGRET_DECISION_COUNT - 1)?)
        .ok_or_else(|| io::Error::other("no-regret fixture window overflowed"))?;
    let initial_regime_oid = fixture_identity(
        b"fgdb:fixture-only:no-regret-regime-oid:v1",
        b"steady-regime:sextant",
    );
    let identity = NoRegretIdentity::try_new(
        fixture_identity(
            b"fgdb:fixture-only:no-regret-monitor-oid:v1",
            b"sextant-compaction-pacing-controller",
        ),
        profile_oid,
        state_space_oid,
        fixture_identity(
            b"fgdb:fixture-only:no-regret-features-oid:v1",
            b"normalized-compaction-losses:v1",
        ),
        policy_epoch_oid,
        initial_regime_oid,
        fixture_identity(
            b"fgdb:fixture-only:no-regret-window-oid:v1",
            b"sequences:80..=82",
        ),
        REGIME_EPOCH,
        NO_REGRET_FIRST_SEQUENCE,
        last_sequence,
        pinned_fallback_oid,
        NO_REGRET_REPLAY_SEED,
        fingerprint,
    )?;
    let assumptions = NoRegretAssumptions::fully_supported();
    let mut controller = NoRegretController::try_new(
        identity,
        profile,
        action_space.clone(),
        assumptions,
        fingerprint,
        &FIXTURE_IDENTITY_AUTHORITY,
    )?;
    let preferred_policy_oid = fixture_identity(
        b"fgdb:fixture-only:no-regret-policy-oid:v1",
        b"compaction-pacing:balanced",
    );
    let mut selections = Vec::with_capacity(NO_REGRET_DECISION_COUNT);
    let mut decision_bytes = Vec::with_capacity(NO_REGRET_DECISION_COUNT);
    let mut event_bytes = Vec::with_capacity(NO_REGRET_DECISION_COUNT + 1);
    let mut decoded_events = Vec::with_capacity(NO_REGRET_DECISION_COUNT + 1);

    for offset in 0..NO_REGRET_DECISION_COUNT {
        let sequence = NO_REGRET_FIRST_SEQUENCE
            .checked_add(u64::try_from(offset)?)
            .ok_or_else(|| io::Error::other("no-regret fixture sequence overflowed"))?;
        if offset == 1 {
            let next_regime_oid = fixture_identity(
                b"fgdb:fixture-only:no-regret-regime-oid:v1",
                &regime_evidence_oid.0,
            );
            let shift = NoRegretRegimeShift::try_new(
                controller.current_regime(),
                NoRegretRegime::new(next_regime_oid, REGIME_EPOCH + 1),
                sequence,
                regime_evidence_oid,
            )?;
            let reset = controller.apply_regime_shift(shift, &FIXTURE_IDENTITY_AUTHORITY)?;
            let encoded = reset.try_canonical_bytes()?;
            let decoded = NoRegretRegimeResetReceipt::try_from_canonical_bytes(
                &encoded,
                &FIXTURE_IDENTITY_AUTHORITY,
                fingerprint,
            )?;
            assert_eq!(decoded, reset);
            assert_eq!(decoded.try_canonical_bytes()?, encoded);
            event_bytes.push(encoded);
            decoded_events.push(NoRegretReplayEvent::RegimeReset(decoded));
        }

        let selection = controller.choose(sequence)?;
        let encoded_decision = controller
            .pending_decision()
            .ok_or_else(|| io::Error::other("no-regret choose omitted its decision receipt"))?
            .try_canonical_bytes()?;
        let decoded_decision = NoRegretDecisionReceipt::try_from_canonical_bytes(
            &encoded_decision,
            &FIXTURE_IDENTITY_AUTHORITY,
            fingerprint,
        )?;
        assert_eq!(
            &decoded_decision,
            controller
                .pending_decision()
                .ok_or_else(|| io::Error::other("no-regret pending receipt disappeared"))?
        );
        assert_eq!(decoded_decision.try_canonical_bytes()?, encoded_decision);
        if offset == 1 {
            assert_eq!(selection.mode(), NoRegretSelectionMode::RegimeResetFallback);
            assert_eq!(selection.selected_policy_oid(), pinned_fallback_oid);
        }
        decision_bytes.push(encoded_decision);

        let loss = if selection.selected_policy_oid() == preferred_policy_oid {
            0.125
        } else if selection.selected_policy_oid() == pinned_fallback_oid {
            0.625
        } else {
            0.375
        };
        controller.feedback(selection, loss)?;
        let receipt = controller
            .latest_receipt()
            .ok_or_else(|| io::Error::other("no-regret feedback omitted its receipt"))?;
        let encoded_feedback = receipt.try_canonical_bytes()?;
        let decoded_feedback = NoRegretFeedbackReceipt::try_from_canonical_bytes(
            &encoded_feedback,
            &FIXTURE_IDENTITY_AUTHORITY,
            fingerprint,
        )?;
        assert_eq!(&decoded_feedback, receipt);
        assert_eq!(decoded_feedback.try_canonical_bytes()?, encoded_feedback);
        selections.push(selection);
        event_bytes.push(encoded_feedback);
        decoded_events.push(NoRegretReplayEvent::Feedback(decoded_feedback));
    }

    let retained_events = controller.replay_history().cloned().collect::<Vec<_>>();
    assert_eq!(retained_events, decoded_events);
    assert!(!controller.replay_history_truncated());
    let replay_summary = NoRegretReplayVerifier::verify_complete(
        identity,
        profile,
        &action_space,
        assumptions,
        &FIXTURE_IDENTITY_AUTHORITY,
        fingerprint,
        false,
        decoded_events.iter(),
    )?;
    assert_eq!(
        replay_summary,
        controller.verify_replay_history(&FIXTURE_IDENTITY_AUTHORITY, fingerprint)?
    );
    assert_eq!(replay_summary.event_count(), NO_REGRET_DECISION_COUNT + 1);
    assert_eq!(replay_summary.completed_epochs(), NO_REGRET_DECISION_COUNT);
    assert_eq!(replay_summary.observed_regime_epochs(), 2);
    assert_eq!(replay_summary.next_sequence(), None);
    assert_eq!(
        replay_summary.final_weight_bits(),
        controller.try_weight_bits()?
    );
    let replay_log_bytes =
        controller.try_canonical_replay_log_bytes(&FIXTURE_IDENTITY_AUTHORITY)?;
    let replay_log = NoRegretReplayLog::try_from_canonical_bytes(
        &replay_log_bytes,
        NoRegretReplayLogDecodeLimits::new(
            1 << 16,
            policy_oids.len(),
            NO_REGRET_DECISION_COUNT + 1,
            1 << 14,
        ),
        identity,
        profile,
        &action_space,
        assumptions,
        &FIXTURE_IDENTITY_AUTHORITY,
        fingerprint,
    )?;
    assert_eq!(replay_log.events(), decoded_events);
    assert_eq!(
        replay_log.verify(&FIXTURE_IDENTITY_AUTHORITY, fingerprint)?,
        replay_summary
    );
    assert_eq!(replay_log.try_canonical_bytes()?, replay_log_bytes);

    Ok(NoRegretFixture {
        selections,
        decision_bytes,
        event_bytes,
        replay_log_bytes,
        replay_summary,
    })
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

fn run_conformal(
    candidate_oid: ObjectId,
    fallback_oid: ObjectId,
) -> TestResult<(Vec<CalibrationEvidence>, Vec<AssessmentEvidence>)> {
    let identity = GraphMetricIdentity::try_new(
        oid(10),
        oid(11),
        oid(12),
        oid(13),
        ConformalWindow::try_new(20, 30)?,
        REGIME_EPOCH,
        candidate_oid,
        fallback_oid,
    )?;
    let profile = ConformalProfile::try_new(oid(14), 0.2, MetricThresholdMode::Upper, 5, 10)?;
    let mut trial = GraphMetricConformal::try_new(identity, profile)?;
    let mut calibration = Vec::new();
    for (offset, value) in (1_u64..=5).enumerate() {
        let sequence = 20_u64
            .checked_add(u64::try_from(offset)?)
            .ok_or_else(|| io::Error::other("conformal fixture sequence arithmetic overflowed"))?;
        calibration.push(trial.calibrate(SequencedMetricValue::new(
            identity,
            profile,
            sequence,
            value as f64,
        ))?);
    }
    let mut assessments = Vec::new();
    for (sequence, value) in (25_u64..=30).zip([3.0, 4.0, 5.0, 2.0, 1.0, 5.0]) {
        assessments.push(trial.assess(SequencedMetricValue::new(
            identity, profile, sequence, value,
        ))?);
    }
    Ok((calibration, assessments))
}

fn run_eprocess(
    candidate_oid: ObjectId,
    fallback_oid: ObjectId,
) -> TestResult<Vec<EvidenceRecord>> {
    let identity = TrialIdentity::try_new(
        oid(15),
        oid(16),
        EProcessWindow::try_new(40, 42)?,
        REGIME_EPOCH,
        candidate_oid,
        fallback_oid,
    )?;
    let profile = EProcessProfile::try_new(
        oid(17),
        EProcessConfig {
            p0: 0.2,
            lambda: 1.0,
            alpha: 0.25,
            max_evalue: 1_000.0,
        },
    )?;
    let mut trial = EProcessTrial::try_new(identity, profile)?;
    let mut evidence = Vec::new();
    for sequence in 40..=42 {
        evidence.push(
            trial
                .observe(SequencedObservation::new(
                    identity,
                    profile,
                    sequence,
                    BinaryObservation::One,
                ))?
                .evidence,
        );
    }
    Ok(evidence)
}

#[test]
fn eprocess_type_i_control_holds_under_null_and_adversarial_stopping() -> TestResult {
    const ROOT_SEED: u64 = 0xe000_c355_7a17_0001;
    const NULL_TRIALS: u64 = 8_192;
    const HORIZON: u64 = 128;
    const ALPHA: f64 = 0.05;
    const P0: f64 = 0.25;
    const FAILURE_CONFIDENCE: f64 = 1.0e-6;
    const PLANTED_TRIALS: u64 = 256;

    let config = EProcessConfig {
        p0: P0,
        lambda: 1.0,
        alpha: ALPHA,
        max_evalue: 1.0e12,
    };
    let profile = EProcessProfile::try_new(oid(17), config.clone())?;
    let identity = TrialIdentity::try_new(
        oid(15),
        oid(16),
        EProcessWindow::try_new(0, HORIZON - 1)?,
        REGIME_EPOCH,
        oid(200),
        oid(201),
    )?;

    // Every null trial peeks after every prefix and stops at its first
    // rejection. This is the adversarial optional-stopping rule Ville's bound
    // is meant to survive; checking only a fixed terminal time would miss the
    // exact failure mode an e-process is selected to prevent.
    let mut false_rejections = 0_u64;
    for trial_index in 0..NULL_TRIALS {
        let mut rng = DetRng::new(ROOT_SEED ^ trial_index.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut trial = EProcessTrial::try_new(identity, profile)?;
        for sequence in 0..HORIZON {
            let observation = if rng.next_u64() & 3 == 0 {
                BinaryObservation::One
            } else {
                BinaryObservation::Zero
            };
            let update = trial.observe(SequencedObservation::new(
                identity,
                profile,
                sequence,
                observation,
            ))?;
            if update.evidence.first_rejection_sequence().is_some() {
                false_rejections += 1;
                break;
            }
        }
    }

    let empirical_type_i = false_rejections as f64 / NULL_TRIALS as f64;
    let hoeffding_margin = ((1.0 / FAILURE_CONFIDENCE).ln() / (2.0 * NULL_TRIALS as f64)).sqrt();
    assert!(
        empirical_type_i <= ALPHA + hoeffding_margin,
        "seed={ROOT_SEED:#018x} config={config:?} null_trials={NULL_TRIALS} \
         horizon={HORIZON} false_rejections={false_rejections} \
         empirical_type_i={empirical_type_i:.8} alpha={ALPHA:.8} \
         one_sided_hoeffding_margin={hoeffding_margin:.8} \
         failure_confidence={FAILURE_CONFIDENCE:.1e}"
    );

    // A planted alternative proves that the campaign is not a detector that
    // merely stays green. Three quarters of observations are violations while
    // the declared null rate remains one quarter.
    let mut planted_rejections = 0_u64;
    for trial_index in 0..PLANTED_TRIALS {
        let mut rng = DetRng::new(
            ROOT_SEED
                .rotate_left(29)
                .wrapping_add(trial_index.wrapping_mul(0xd1b5_4a32_d192_ed03)),
        );
        let mut trial = EProcessTrial::try_new(identity, profile)?;
        for sequence in 0..HORIZON {
            let observation = if rng.next_u64() & 3 != 0 {
                BinaryObservation::One
            } else {
                BinaryObservation::Zero
            };
            let update = trial.observe(SequencedObservation::new(
                identity,
                profile,
                sequence,
                observation,
            ))?;
            if update.evidence.first_rejection_sequence().is_some() {
                planted_rejections += 1;
                break;
            }
        }
    }
    assert!(
        planted_rejections * 100 >= PLANTED_TRIALS * 95,
        "seed={ROOT_SEED:#018x} config={config:?} planted_trials={PLANTED_TRIALS} \
         planted_rejections={planted_rejections}: the alternative-population \
         control did not prove detector liveness"
    );
    Ok(())
}

#[test]
fn conformal_marginal_coverage_holds_under_exchangeability_and_degrades_under_seeded_drift()
-> TestResult {
    const ROOT_SEED: u64 = 0xc0f0_4a1a_7a17_0001;
    const TRIALS: u64 = 8_192;
    const CALIBRATION_SAMPLES: u64 = 64;
    const ALPHA: f64 = 0.10;
    const FAILURE_CONFIDENCE: f64 = 1.0e-6;
    const DRIFT_OFFSET: f64 = 1_000_000.0;

    let profile = ConformalProfile::try_new(
        oid(14),
        ALPHA,
        MetricThresholdMode::Upper,
        32,
        CALIBRATION_SAMPLES as usize,
    )?;
    let identity = GraphMetricIdentity::try_new(
        oid(10),
        oid(11),
        oid(12),
        oid(13),
        ConformalWindow::try_new(0, CALIBRATION_SAMPLES + 1)?,
        REGIME_EPOCH,
        oid(200),
        oid(201),
    )?;
    let mut exchangeable_covered = 0_u64;
    let mut drift_covered = 0_u64;

    for trial_index in 0..TRIALS {
        let mut rng = DetRng::new(ROOT_SEED ^ trial_index.wrapping_mul(0xa076_1d64_78bd_642f));
        let mut trial = GraphMetricConformal::try_new(identity, profile)?;
        for sequence in 0..CALIBRATION_SAMPLES {
            let value = (rng.next_u64() % 1_000_003) as f64;
            trial.calibrate(SequencedMetricValue::new(
                identity, profile, sequence, value,
            ))?;
        }

        let held_out = (rng.next_u64() % 1_000_003) as f64;
        let exchangeable = trial.assess(SequencedMetricValue::new(
            identity,
            profile,
            CALIBRATION_SAMPLES,
            held_out,
        ))?;
        exchangeable_covered += u64::from(exchangeable.conforming() == Some(true));

        let shifted = trial.assess(SequencedMetricValue::new(
            identity,
            profile,
            CALIBRATION_SAMPLES + 1,
            held_out + DRIFT_OFFSET,
        ))?;
        drift_covered += u64::from(shifted.conforming() == Some(true));
    }

    let exchangeable_rate = exchangeable_covered as f64 / TRIALS as f64;
    let drift_rate = drift_covered as f64 / TRIALS as f64;
    let target_coverage = 1.0 - ALPHA;
    let hoeffding_margin = ((1.0 / FAILURE_CONFIDENCE).ln() / (2.0 * TRIALS as f64)).sqrt();
    assert!(
        exchangeable_rate + hoeffding_margin >= target_coverage,
        "seed={ROOT_SEED:#018x} trials={TRIALS} calibration={CALIBRATION_SAMPLES} \
         alpha={ALPHA:.8} exchangeable_covered={exchangeable_covered} \
         exchangeable_rate={exchangeable_rate:.8} target={target_coverage:.8} \
         one_sided_hoeffding_margin={hoeffding_margin:.8} \
         failure_confidence={FAILURE_CONFIDENCE:.1e}"
    );
    assert!(
        drift_rate <= 0.05 && exchangeable_rate - drift_rate >= 0.75,
        "seed={ROOT_SEED:#018x} trials={TRIALS} drift_offset={DRIFT_OFFSET} \
         exchangeable_rate={exchangeable_rate:.8} drift_covered={drift_covered} \
         drift_rate={drift_rate:.8}: planted drift did not materially degrade coverage"
    );
    Ok(())
}

fn logged_decision(sequence: u64, selected_a: bool) -> TestResult<LoggedDecision> {
    let half = Probability::try_from_numerator(PROBABILITY_SCALE / 2)?;
    let one = Outcome::try_from_scaled(OUTCOME_SCALE)?;
    let zero = Outcome::try_from_scaled(0)?;
    logged_binary_decision(
        sequence,
        selected_a,
        half,
        Probability::one(),
        Probability::zero(),
        one,
        zero,
    )
}

fn logged_binary_decision(
    sequence: u64,
    selected_a: bool,
    behavior_a: Probability,
    candidate_a: Probability,
    fallback_a: Probability,
    outcome_a: Outcome,
    outcome_b: Outcome,
) -> TestResult<LoggedDecision> {
    let observed_outcome = if selected_a { outcome_a } else { outcome_b };
    logged_binary_decision_with_observed(
        sequence,
        selected_a,
        behavior_a,
        candidate_a,
        fallback_a,
        outcome_a,
        outcome_b,
        observed_outcome,
    )
}

#[allow(clippy::too_many_arguments)]
fn logged_binary_decision_with_observed(
    sequence: u64,
    selected_a: bool,
    behavior_a: Probability,
    candidate_a: Probability,
    fallback_a: Probability,
    direct_outcome_a: Outcome,
    direct_outcome_b: Outcome,
    observed_outcome: Outcome,
) -> TestResult<LoggedDecision> {
    let behavior_b = Probability::try_from_numerator(PROBABILITY_SCALE - behavior_a.numerator())?;
    let candidate_b = Probability::try_from_numerator(PROBABILITY_SCALE - candidate_a.numerator())?;
    let fallback_b = Probability::try_from_numerator(PROBABILITY_SCALE - fallback_a.numerator())?;
    let action_a = oid(20);
    let action_b = oid(21);
    let selected_action = if selected_a { action_a } else { action_b };

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
                behavior_a,
                candidate_a,
                fallback_a,
                Some(direct_outcome_a),
            ),
            LoggedAction::new(
                action_b,
                behavior_b,
                candidate_b,
                fallback_b,
                Some(direct_outcome_b),
            ),
        ],
    )?)
}

fn ope_identity(
    estimator: OpeEstimator,
    first_sequence: u64,
    last_sequence: u64,
    candidate_oid: ObjectId,
    fallback_oid: ObjectId,
) -> TestResult<OpeIdentity> {
    Ok(OpeIdentity::try_new(
        oid(33),
        OpeWindow::try_new(first_sequence, last_sequence)?,
        oid(34),
        oid(35),
        oid(36),
        oid(37),
        REGIME_EPOCH,
        candidate_oid,
        fallback_oid,
        oid(38),
        estimator,
    )?)
}

fn run_ope(candidate_oid: ObjectId, fallback_oid: ObjectId) -> TestResult<Vec<OpeEvidence>> {
    let identity = ope_identity(
        OpeEstimator::DoublyRobust,
        50,
        53,
        candidate_oid,
        fallback_oid,
    )?;
    let profile = OpeProfile::try_new(10 * WEIGHT_SCALE, 2, 4, 2, 8)?;
    let mut ledger = OpeLedger::try_new(identity, profile)?;
    let mut prefixes = Vec::new();
    for sequence in 50..=53 {
        ledger.record(logged_decision(sequence, sequence % 2 == 0)?)?;
        prefixes.push(ledger.evidence()?);
    }
    Ok(prefixes)
}

#[test]
fn ope_support_firewall_promotes_only_complete_supported_effective_logs() -> TestResult {
    let candidate_oid = oid(40);
    let fallback_oid = oid(90);
    let positive_profile = OpeProfile::try_new(10 * WEIGHT_SCALE, 1, 1, 2, 2)?;
    let half = Probability::try_from_numerator(PROBABILITY_SCALE / 2)?;
    let quarter = Probability::try_from_numerator(PROBABILITY_SCALE / 4)?;
    let three_quarters = Probability::try_from_numerator(3 * PROBABILITY_SCALE / 4)?;
    let one = Outcome::try_from_scaled(OUTCOME_SCALE)?;
    let direct_a = Outcome::try_from_scaled(OUTCOME_SCALE * 4 / 5)?;
    let direct_b = Outcome::try_from_scaled(OUTCOME_SCALE / 5)?;
    let expected_estimates = [
        (
            OpeEstimator::Direct,
            650_000_000_000_000_i128,
            350_000_000_000_000_i128,
        ),
        (
            OpeEstimator::ImportanceWeighted,
            1_500_000_000_000_000_i128,
            500_000_000_000_000_i128,
        ),
        (
            OpeEstimator::DoublyRobust,
            950_000_000_000_000_i128,
            450_000_000_000_000_i128,
        ),
    ];

    for (estimator, expected_candidate, expected_fallback) in expected_estimates {
        let identity = ope_identity(estimator, 100, 100, candidate_oid, fallback_oid)?;
        let mut left = OpeLedger::try_new(identity, positive_profile)?;
        let mut right = OpeLedger::try_new(identity, positive_profile)?;
        let decision = logged_binary_decision_with_observed(
            100,
            true,
            half,
            three_quarters,
            quarter,
            direct_a,
            direct_b,
            one,
        )?;
        left.record(decision.clone())?;
        right.record(decision)?;
        let evidence = left.evidence()?;
        assert_eq!(evidence, right.evidence()?, "{estimator:?} replay drifted");
        assert!(evidence.complete());
        assert!(evidence.zero_support_exclusions().is_empty());
        assert!(evidence.candidate_ess_gate_passed());
        assert!(evidence.fallback_ess_gate_passed());
        assert_eq!(evidence.selection(), OpeSelection::Candidate);
        assert_eq!(
            evidence.selection_reason(),
            OpeSelectionReason::CandidateEstimatedBetter
        );
        assert_eq!(
            evidence.candidate_estimate().numerator(),
            expected_candidate
        );
        assert_eq!(evidence.fallback_estimate().numerator(), expected_fallback);
        assert_eq!(
            evidence.advantage_estimate().numerator(),
            expected_candidate - expected_fallback
        );
        assert_eq!(
            evidence.candidate_estimate().denominator(),
            u128::from(PROBABILITY_SCALE) * u128::try_from(OUTCOME_SCALE)?
        );

        let record = StatisticalLogRecord::try_from_ope(&FIXTURE_IDENTITY_AUTHORITY, &evidence)?;
        let registration = record.evidence_registration();
        assert_eq!(registration.registry_id(), "FG-EVID-01");
        assert_eq!(
            registration.required_disclosures(),
            &[
                "state/actions/features",
                "logged probabilities",
                "zero-support exclusions",
                "clipping/estimator",
                "policy epoch and fallback",
            ]
        );
        let envelope = record.try_to_evidence_envelope()?;
        let EvidenceClaim::StatisticalClaim { assumptions, .. } = envelope.claim() else {
            return Err(io::Error::other("OPE evidence was not statistically typed").into());
        };
        for disclosure in registration.required_disclosures() {
            assert!(
                assumptions
                    .iter()
                    .any(|value| value == &format!("registry-disclosure:{disclosure}")),
                "OPE envelope omitted registry disclosure {disclosure:?}"
            );
        }
        assert_eq!(
            envelope.propensity_support_identity(),
            PropensitySupportIdentity::Bound(record.evidence_oid())
        );
        assert_eq!(
            envelope.fallback(),
            FallbackBehavior::DeterministicPolicy {
                policy_oid: fallback_oid
            }
        );
        let (_, envelopes, promoted, _) = promote_epoch(candidate_oid, fallback_oid, &[record])?;
        assert_eq!(envelopes, vec![envelope]);
        assert_eq!(promoted.pinned_table_oid(), candidate_oid);
    }

    let zero = Outcome::try_from_scaled(0)?;
    let mut unsupported = OpeLedger::try_new(
        ope_identity(OpeEstimator::Direct, 200, 201, candidate_oid, fallback_oid)?,
        OpeProfile::try_new(WEIGHT_SCALE, 1, 2, 2, 4)?,
    )?;
    for sequence in 200..=201 {
        unsupported.record(logged_binary_decision(
            sequence,
            true,
            Probability::one(),
            Probability::zero(),
            Probability::one(),
            zero,
            one,
        )?)?;
    }
    let unsupported_evidence = unsupported.evidence()?;
    assert!(unsupported_evidence.complete());
    assert_eq!(unsupported_evidence.zero_support_exclusions().len(), 2);
    for exclusion in unsupported_evidence.zero_support_exclusions() {
        assert_eq!(exclusion.affected_policy(), EvaluatedPolicy::Candidate);
        assert_eq!(exclusion.action_oid(), oid(21));
        assert_eq!(exclusion.unsupported_probability(), Probability::one());
    }
    assert_eq!(
        unsupported_evidence.selection(),
        OpeSelection::PinnedFallback
    );
    assert_eq!(
        unsupported_evidence.selection_reason(),
        OpeSelectionReason::ZeroSupport
    );
    assert_ope_evidence_refuses_candidate_promotion(
        &unsupported_evidence,
        candidate_oid,
        fallback_oid,
    )?;

    let mut ineffective = OpeLedger::try_new(
        ope_identity(OpeEstimator::Direct, 300, 303, candidate_oid, fallback_oid)?,
        OpeProfile::try_new(10 * WEIGHT_SCALE, 3, 4, 2, 8)?,
    )?;
    for sequence in 300..=302 {
        ineffective.record(logged_binary_decision(
            sequence, true, half, half, half, zero, one,
        )?)?;
    }
    let ninety_nine_percent = Probability::try_from_numerator(PROBABILITY_SCALE * 99 / 100)?;
    ineffective.record(logged_binary_decision(
        303,
        false,
        ninety_nine_percent,
        half,
        ninety_nine_percent,
        zero,
        one,
    )?)?;
    let ineffective_evidence = ineffective.evidence()?;
    assert!(ineffective_evidence.complete());
    assert!(ineffective_evidence.zero_support_exclusions().is_empty());
    assert!(ineffective_evidence.advantage_estimate().numerator() > 0);
    assert!(!ineffective_evidence.candidate_ess_gate_passed());
    assert!(ineffective_evidence.fallback_ess_gate_passed());
    assert_eq!(
        ineffective_evidence.selection(),
        OpeSelection::PinnedFallback
    );
    assert_eq!(
        ineffective_evidence.selection_reason(),
        OpeSelectionReason::InsufficientEffectiveSampleSize
    );
    assert_ope_evidence_refuses_candidate_promotion(
        &ineffective_evidence,
        candidate_oid,
        fallback_oid,
    )?;

    let mut incomplete = OpeLedger::try_new(
        ope_identity(OpeEstimator::Direct, 400, 401, candidate_oid, fallback_oid)?,
        OpeProfile::try_new(WEIGHT_SCALE, 1, 2, 2, 4)?,
    )?;
    incomplete.record(logged_binary_decision(
        400, true, half, half, half, one, zero,
    )?)?;
    let incomplete_evidence = incomplete.evidence()?;
    assert!(!incomplete_evidence.complete());
    assert!(incomplete_evidence.zero_support_exclusions().is_empty());
    assert_eq!(
        incomplete_evidence.selection(),
        OpeSelection::PinnedFallback
    );
    assert_eq!(
        incomplete_evidence.selection_reason(),
        OpeSelectionReason::IncompleteWindow
    );
    assert_ope_evidence_refuses_candidate_promotion(
        &incomplete_evidence,
        candidate_oid,
        fallback_oid,
    )?;
    Ok(())
}

fn assert_ope_evidence_refuses_candidate_promotion(
    evidence: &OpeEvidence,
    candidate_oid: ObjectId,
    fallback_oid: ObjectId,
) -> TestResult {
    let record = StatisticalLogRecord::try_from_ope(&FIXTURE_IDENTITY_AUTHORITY, evidence)?;
    assert_eq!(record.evidence_registration().registry_id(), "FG-EVID-01");
    let envelope = record.try_to_evidence_envelope()?;
    assert_eq!(envelope.selection_policy_oid(), fallback_oid);
    let root = DecisionPolicyEpoch::try_root(
        "policy:ope-support-firewall",
        0,
        DecisionPolicyScope::new(oid(70)),
        LogicalEffectClass::AnswerAffectingExecution,
        fallback_oid,
        fallback_oid,
    )?;
    let root_oid = FixtureOnlyIdentityAuthority::epoch_oid(&root)?;
    assert!(matches!(
        DecisionPolicyEpoch::try_promote(
            &root,
            root_oid,
            candidate_oid,
            &[record.evidence_oid()],
            &[envelope],
        ),
        Err(DecisionPolicyEpochError::EvidenceSelectionPolicyMismatch { .. })
    ));
    Ok(())
}

fn evidence_envelope(record: StatisticalLogRecord) -> TestResult<EvidenceEnvelope> {
    Ok(record.try_to_evidence_envelope()?)
}

fn sorted_envelopes(
    records: &[StatisticalLogRecord],
    selected_policy_oid: ObjectId,
) -> TestResult<Vec<EvidenceEnvelope>> {
    let mut envelopes = Vec::with_capacity(records.len());
    for record in records {
        if record.selected_policy_oid() != selected_policy_oid {
            return Err(io::Error::other(
                "statistical record selected a policy inconsistent with its transition",
            )
            .into());
        }
        envelopes.push(evidence_envelope(*record)?);
    }
    envelopes.sort_by_key(EvidenceEnvelope::evidence_oid);
    Ok(envelopes)
}

fn promote_epoch(
    candidate_oid: ObjectId,
    fallback_oid: ObjectId,
    promotion_records: &[StatisticalLogRecord],
) -> TestResult<PromotionResult> {
    let root = DecisionPolicyEpoch::try_root(
        "policy:sextant-e2e",
        0,
        DecisionPolicyScope::new(oid(70)),
        LogicalEffectClass::AnswerAffectingExecution,
        fallback_oid,
        fallback_oid,
    )?;
    let root_bytes = root.try_to_canonical_bytes()?;
    let root_oid = FixtureOnlyIdentityAuthority::epoch_oid(&root)?;
    let envelopes = sorted_envelopes(promotion_records, candidate_oid)?;
    let evidence_refs: Vec<_> = envelopes
        .iter()
        .map(EvidenceEnvelope::evidence_oid)
        .collect();
    let promoted = DecisionPolicyEpoch::try_promote(
        &root,
        root_oid,
        candidate_oid,
        &evidence_refs,
        &envelopes,
    )?;
    let encoded = promoted.try_to_canonical_bytes()?;
    let decoded = DecisionPolicyEpoch::try_promoted_from_canonical_bytes(
        &encoded, &root, root_oid, &envelopes,
    )?;
    assert_eq!(decoded, promoted);
    Ok((root_bytes, envelopes, promoted, encoded))
}

fn revert_epoch(
    promoted_epoch: &DecisionPolicyEpoch,
    _promoted_epoch_bytes: &[u8],
    shifted: &RegimeSignalEvidence,
    regime_record: StatisticalLogRecord,
) -> TestResult<(EvidenceEnvelope, DecisionPolicyEpoch, Vec<u8>)> {
    let expected_regime_record =
        StatisticalLogRecord::try_from_regime(&FIXTURE_IDENTITY_AUTHORITY, shifted)?;
    if regime_record != expected_regime_record {
        return Err(io::Error::other(
            "persisted regime record does not match the typed regime receipt",
        )
        .into());
    }
    let evidence_oid = regime_record.evidence_oid();
    let envelope = evidence_envelope(regime_record)?;
    let predecessor_oid = FixtureOnlyIdentityAuthority::epoch_oid(promoted_epoch)?;
    let reverted = DecisionPolicyEpoch::try_revert_to_fallback(
        promoted_epoch,
        predecessor_oid,
        &[evidence_oid],
        std::slice::from_ref(&envelope),
        evidence_oid,
        shifted,
    )?;
    let encoded = reverted.try_to_canonical_bytes()?;
    let decoded = DecisionPolicyEpoch::try_fallback_from_canonical_bytes(
        &encoded,
        promoted_epoch,
        predecessor_oid,
        std::slice::from_ref(&envelope),
        evidence_oid,
        shifted,
    )?;
    assert_eq!(decoded, reverted);
    Ok((envelope, reverted, encoded))
}

fn build_statistical_log(
    exploration: &[ExplorationBudgetEvidence],
    assessments: &[AssessmentEvidence],
    sequential: &[EvidenceRecord],
    ope: &[OpeEvidence],
    progress: &[DrainProgressEvidence],
    regime: &[RegimeSignalEvidence],
    ann_recall: &AnnRecallEvidence,
) -> TestResult<(StatisticalDecisionLog, Vec<u8>)> {
    const EXPECTED_RECORDS: usize = 2 + 6 + 3 + 4 + 3 + 8 + 1;
    let mut log = StatisticalDecisionLog::try_new(EXPECTED_RECORDS)?;
    for evidence in exploration {
        log.append(StatisticalLogRecord::try_from_exploration(
            &FIXTURE_IDENTITY_AUTHORITY,
            evidence,
        )?)?;
    }
    for evidence in assessments {
        log.append(StatisticalLogRecord::try_from_conformal(
            &FIXTURE_IDENTITY_AUTHORITY,
            evidence,
        )?)?;
    }
    for evidence in sequential {
        log.append(StatisticalLogRecord::try_from_eprocess(
            &FIXTURE_IDENTITY_AUTHORITY,
            evidence,
        )?)?;
    }
    for evidence in ope {
        log.append(StatisticalLogRecord::try_from_ope(
            &FIXTURE_IDENTITY_AUTHORITY,
            evidence,
        )?)?;
    }
    for evidence in progress {
        log.append(StatisticalLogRecord::try_from_progress(
            &FIXTURE_IDENTITY_AUTHORITY,
            evidence,
        )?)?;
    }
    for evidence in regime {
        log.append(StatisticalLogRecord::try_from_regime(
            &FIXTURE_IDENTITY_AUTHORITY,
            evidence,
        )?)?;
    }
    log.append(StatisticalLogRecord::try_from_ann_recall(
        &FIXTURE_IDENTITY_AUTHORITY,
        ann_recall,
    )?)?;
    assert_eq!(log.len(), EXPECTED_RECORDS);
    let encoded = log.encode_canonical()?;
    let decoded = read_statistical_log(&encoded, EXPECTED_RECORDS)?;
    assert_eq!(decoded, log);
    Ok((log, encoded))
}

fn records_for_promotion(log: &StatisticalDecisionLog) -> Vec<StatisticalLogRecord> {
    [
        StatisticalMonitorKind::ExplorationBudget,
        StatisticalMonitorKind::ConformalThreshold,
        StatisticalMonitorKind::EProcess,
        StatisticalMonitorKind::OffPolicyEvaluation,
    ]
    .into_iter()
    .filter_map(|monitor| {
        log.records()
            .iter()
            .rev()
            .copied()
            .find(|record| record.monitor_kind() == monitor)
    })
    .collect()
}

fn regime_record(log: &StatisticalDecisionLog) -> TestResult<StatisticalLogRecord> {
    log.records()
        .iter()
        .copied()
        .find(|record| {
            record.monitor_kind() == StatisticalMonitorKind::RegimeChange
                && record.selected_policy_oid() == record.pinned_fallback_oid()
        })
        .ok_or_else(|| io::Error::other("statistical log omitted regime evidence").into())
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

fn run_progress_monitor(
    candidate_oid: ObjectId,
    fallback_oid: ObjectId,
) -> TestResult<Vec<DrainProgressEvidence>> {
    let identity = DrainProgressIdentity::try_new(
        oid(71),
        oid(72),
        55,
        57,
        REGIME_EPOCH,
        candidate_oid,
        fallback_oid,
    )?;
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
    let mut evidence = Vec::new();
    for (sequence, potential) in [(55, 10.0), (56, 8.0), (57, 5.0)] {
        evidence.push(monitor.observe(SequencedPotential::new(
            identity,
            profile.clone(),
            sequence,
            potential,
        ))?);
    }
    Ok(evidence)
}

fn run_fixture() -> TestResult<FixtureRun> {
    let candidate_oid = oid(40);
    let fallback_oid = oid(90);
    let exploration = run_exploration(candidate_oid, fallback_oid)?;
    let (calibration, assessments) = run_conformal(candidate_oid, fallback_oid)?;
    let assessment = assessments
        .last()
        .cloned()
        .ok_or_else(|| io::Error::other("conformal fixture produced no assessment evidence"))?;
    let sequential_evidence = run_eprocess(candidate_oid, fallback_oid)?;
    let ope_prefixes = run_ope(candidate_oid, fallback_oid)?;
    let ope = ope_prefixes
        .last()
        .cloned()
        .ok_or_else(|| io::Error::other("OPE fixture produced no evidence"))?;
    let progress_evidence = run_progress_monitor(candidate_oid, fallback_oid)?;
    let regime_evidence = run_regime_monitor(candidate_oid, fallback_oid)?;
    let shifted = regime_evidence
        .get(5)
        .ok_or_else(|| io::Error::other("regime fixture omitted its shift sample"))?;
    let (regime_evidence_oid, regime_evidence_bytes) = persist_regime_evidence(shifted)?;
    let persisted_shifted =
        read_persisted_regime_evidence(&regime_evidence_bytes, regime_evidence_oid)?;
    let ann_recall = run_ann_recall()?;
    let (statistical_log, statistical_log_bytes) = build_statistical_log(
        &exploration,
        &assessments,
        &sequential_evidence,
        &ope_prefixes,
        &progress_evidence,
        &regime_evidence,
        &ann_recall.evidence,
    )?;
    let promotion_records = records_for_promotion(&statistical_log);
    if promotion_records.len() != 4 {
        return Err(
            io::Error::other("statistical log did not retain all four promotion monitors").into(),
        );
    }
    let (root_epoch_bytes, promotion_envelopes, promoted_epoch, promoted_epoch_bytes) =
        promote_epoch(candidate_oid, fallback_oid, &promotion_records)?;
    let (fallback_envelope, reverted_epoch, reverted_epoch_bytes) = revert_epoch(
        &promoted_epoch,
        &promoted_epoch_bytes,
        &persisted_shifted,
        regime_record(&statistical_log)?,
    )?;
    let sketch = run_sketch_maintenance()?;
    let no_regret = run_no_regret(
        FixtureOnlyIdentityAuthority::epoch_oid(&promoted_epoch)?,
        regime_evidence_oid,
    )?;

    Ok(FixtureRun {
        exploration,
        calibration,
        assessments,
        assessment,
        sequential_evidence,
        ope_prefixes,
        ope,
        progress_evidence,
        root_epoch_bytes,
        promotion_envelopes,
        promoted_epoch,
        promoted_epoch_bytes,
        regime_evidence,
        regime_evidence_oid,
        regime_evidence_bytes,
        fallback_envelope,
        reverted_epoch,
        reverted_epoch_bytes,
        statistical_log,
        statistical_log_bytes,
        sketch,
        ann_recall,
        no_regret,
    })
}

fn reconstruct_epochs_from_persisted_records(fixture: &FixtureRun) -> TestResult {
    let decoded_log = read_statistical_log(
        &fixture.statistical_log_bytes,
        fixture.statistical_log.maximum_records(),
    )?;
    let shifted = read_persisted_regime_evidence(
        &fixture.regime_evidence_bytes,
        fixture.regime_evidence_oid,
    )?;
    let root = DecisionPolicyEpoch::try_root_from_canonical_bytes(&fixture.root_epoch_bytes)?;
    let root_oid = FixtureOnlyIdentityAuthority::epoch_oid(&root)?;
    let promotion_records = records_for_promotion(&decoded_log);
    let promotion_envelopes = sorted_envelopes(&promotion_records, oid(40))?;
    let promoted = DecisionPolicyEpoch::try_promoted_from_canonical_bytes(
        &fixture.promoted_epoch_bytes,
        &root,
        root_oid,
        &promotion_envelopes,
    )?;
    assert_eq!(promoted, fixture.promoted_epoch);
    assert_eq!(promotion_envelopes, fixture.promotion_envelopes);

    let decoded_regime_record = regime_record(&decoded_log)?;
    let expected_regime_record =
        StatisticalLogRecord::try_from_regime(&FIXTURE_IDENTITY_AUTHORITY, &shifted)?;
    assert_eq!(decoded_regime_record, expected_regime_record);
    let fallback_envelope = evidence_envelope(decoded_regime_record)?;
    let promoted_oid = FixtureOnlyIdentityAuthority::epoch_oid(&promoted)?;
    let reverted = DecisionPolicyEpoch::try_fallback_from_canonical_bytes(
        &fixture.reverted_epoch_bytes,
        &promoted,
        promoted_oid,
        std::slice::from_ref(&fallback_envelope),
        decoded_regime_record.evidence_oid(),
        &shifted,
    )?;
    assert_eq!(fallback_envelope, fixture.fallback_envelope);
    assert_eq!(reverted, fixture.reverted_epoch);
    Ok(())
}

fn run_fixture_in_lab(seed: u64) -> TestResult<FixtureRun> {
    let output: Arc<Mutex<Option<Result<FixtureRun, String>>>> = Arc::new(Mutex::new(None));
    let task_output = Arc::clone(&output);
    let mut runtime = LabRuntime::new(LabConfig::new(seed).max_steps(1_000));
    let root_region = runtime.state.create_root_region(Budget::INFINITE);
    let (task_id, task_handle) =
        runtime
            .state
            .create_task(root_region, Budget::INFINITE, async move {
                let result = run_fixture().map_err(|error| error.to_string());
                if let Ok(mut slot) = task_output.lock() {
                    *slot = Some(result);
                }
            })?;
    runtime.scheduler.lock().schedule(task_id, 0);
    runtime.run_until_quiescent();
    if !runtime.is_quiescent() || !task_handle.is_finished() {
        return Err(io::Error::other("seeded lab fixture did not quiesce").into());
    }
    output
        .lock()
        .map_err(|_| io::Error::other("seeded lab fixture output lock was poisoned"))?
        .take()
        .ok_or_else(|| io::Error::other("seeded lab fixture produced no result"))?
        .map_err(|message| io::Error::other(message).into())
}

fn unique_field_payload_offset(
    encoded: &[u8],
    field_tag: u8,
    payload_len: u32,
) -> TestResult<usize> {
    let mut header = [0_u8; 5];
    header[0] = field_tag;
    header[1..].copy_from_slice(&payload_len.to_le_bytes());
    let offsets: Vec<_> = encoded
        .windows(header.len())
        .enumerate()
        .filter_map(|(offset, window)| (window == header).then_some(offset + header.len()))
        .collect();
    if offsets.len() != 1 {
        return Err(io::Error::other(format!(
            "canonical field tag {field_tag:#04x} length {payload_len} occurred {} times",
            offsets.len()
        ))
        .into());
    }
    Ok(offsets[0])
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedEvidenceRegistryRow {
    qualified_claim: String,
    required_disclosures: Vec<String>,
    binds_to: Vec<String>,
}

fn evidence_registry_row_source(registry_id: &str) -> TestResult<String> {
    const ROW_HEADER: &str = "[[evidence]]";
    let source_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../registries/evidence.toml");
    let source = fs::read_to_string(&source_path).map_err(|error| {
        io::Error::other(format!("failed to read {}: {error}", source_path.display()))
    })?;
    let marker = format!("id = \"{registry_id}\"");
    let marker_offset = source
        .find(&marker)
        .ok_or_else(|| io::Error::other(format!("evidence registry omitted row {registry_id}")))?;
    let row_start = source[..marker_offset].rfind(ROW_HEADER).ok_or_else(|| {
        io::Error::other(format!("evidence registry row {registry_id} has no header"))
    })?;
    let row_tail = &source[row_start..];
    let after_header = &row_tail[ROW_HEADER.len()..];
    let row_end = after_header
        .find(ROW_HEADER)
        .map_or(row_tail.len(), |offset| ROW_HEADER.len() + offset);
    Ok(row_tail[..row_end].to_owned())
}

fn evidence_registry_scalar(row: &str, key: &str) -> TestResult<String> {
    let assignment = format!("{key} = ");
    let line = row
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(&assignment))
        .ok_or_else(|| io::Error::other(format!("evidence row omitted {key}")))?;
    let value = line[assignment.len()..]
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| io::Error::other(format!("evidence row {key} is not a string")))?;
    Ok(value.to_owned())
}

fn evidence_registry_array(row: &str, key: &str) -> TestResult<Vec<String>> {
    let assignment = format!("{key} = ");
    let assignment_offset = row
        .find(&assignment)
        .ok_or_else(|| io::Error::other(format!("evidence row omitted {key}")))?;
    let after_assignment = &row[assignment_offset + assignment.len()..];
    let array_start = after_assignment
        .find('[')
        .ok_or_else(|| io::Error::other(format!("evidence row {key} is not an array")))?;
    let after_open = &after_assignment[array_start + 1..];
    let array_end = after_open
        .find(']')
        .ok_or_else(|| io::Error::other(format!("evidence row {key} array is unterminated")))?;
    after_open[..array_end]
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .map(str::to_owned)
                .ok_or_else(|| {
                    io::Error::other(format!("evidence row {key} contains a non-string value"))
                        .into()
                })
        })
        .collect()
}

fn parsed_evidence_registry_row(registry_id: &str) -> TestResult<ParsedEvidenceRegistryRow> {
    let row = evidence_registry_row_source(registry_id)?;
    Ok(ParsedEvidenceRegistryRow {
        qualified_claim: evidence_registry_scalar(&row, "qualified_claim")?,
        required_disclosures: evidence_registry_array(&row, "required_disclosures")?,
        binds_to: evidence_registry_array(&row, "binds_to")?,
    })
}

fn registration_matches_registry(
    registration: StatisticalEvidenceRegistration,
    registry: &ParsedEvidenceRegistryRow,
) -> bool {
    registry.qualified_claim == registration.qualified_claim()
        && registry
            .required_disclosures
            .iter()
            .map(String::as_str)
            .eq(registration.required_disclosures().iter().copied())
        && registry
            .binds_to
            .iter()
            .map(String::as_str)
            .eq(registration.binds_to().iter().copied())
}

fn object_id_hex_for_test(object_id: ObjectId) -> String {
    object_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn expected_substantive_assumptions(record: StatisticalLogRecord) -> TestResult<Vec<String>> {
    let monitor_identity = format!(
        "monitor_oid={}",
        object_id_hex_for_test(record.monitor_oid())
    );
    let mut assumptions = match record.monitor_kind() {
        StatisticalMonitorKind::ExplorationBudget => vec![
            "alpha is the registered exploration-profile alpha only".to_owned(),
            "binary novelty runs are exchangeable under the named window".to_owned(),
        ],
        StatisticalMonitorKind::ConformalThreshold => vec![
            "alpha is the registered conformal-profile alpha only".to_owned(),
            "calibration and assessment observations are exchangeable under the named population and selection".to_owned(),
        ],
        StatisticalMonitorKind::EProcess => vec![
            "alpha is the registered e-process-profile alpha only".to_owned(),
            "the declared null and filtration make the e-value a nonnegative supermartingale"
                .to_owned(),
        ],
        StatisticalMonitorKind::DrainProgress => vec![
            "alpha does not apply to this bounded progress certificate".to_owned(),
            "the registered potential and maximum-step bound cover the complete prefix".to_owned(),
        ],
        StatisticalMonitorKind::RegimeChange => vec![
            "alpha does not apply to this versioned change-detector receipt".to_owned(),
            "the runtime metric series and detector profile are registered identities".to_owned(),
        ],
        StatisticalMonitorKind::OffPolicyEvaluation => vec![
            "alpha does not apply to this exact-rational off-policy evaluation".to_owned(),
            "every logged action carries exact realized support and propensity evidence".to_owned(),
        ],
        StatisticalMonitorKind::AnnRecall => {
            let StatisticalStatistic::AnnRecall {
                confidence_exponent,
                assumptions_supported,
                ..
            } = record.statistic()
            else {
                return Err(io::Error::other("ANN monitor carried a non-ANN statistic").into());
            };
            vec![
                format!("failure probability is exactly 2^-{confidence_exponent}"),
                format!("all interval assumptions supported={assumptions_supported}"),
                "exact-baseline and candidate result lists are complete".to_owned(),
            ]
        }
    };
    assumptions.push(monitor_identity);
    Ok(assumptions)
}

#[test]
fn sextant_assumption_registration_is_complete_and_fail_closed() -> TestResult {
    let fixture = run_fixture()?;
    assert_eq!(fixture.statistical_log.len(), 27);
    let mut registrations = Vec::<StatisticalEvidenceRegistration>::new();
    let mut monitor_registrations = Vec::<(StatisticalMonitorKind, &'static str)>::new();
    for record in fixture.statistical_log.records() {
        let registration = record.evidence_registration();
        let expected_registry_id = match record.monitor_kind() {
            StatisticalMonitorKind::EProcess
            | StatisticalMonitorKind::ExplorationBudget
            | StatisticalMonitorKind::DrainProgress
            | StatisticalMonitorKind::RegimeChange => "FG-CAL-01",
            StatisticalMonitorKind::ConformalThreshold => "FG-CAL-02",
            StatisticalMonitorKind::OffPolicyEvaluation => "FG-EVID-01",
            StatisticalMonitorKind::AnnRecall => "FG-CAL-03",
        };
        assert_eq!(registration.registry_id(), expected_registry_id);
        if !monitor_registrations
            .iter()
            .any(|(monitor, _)| *monitor == record.monitor_kind())
        {
            monitor_registrations.push((record.monitor_kind(), registration.registry_id()));
        }
        if !registrations
            .iter()
            .any(|registered| registered.registry_id() == registration.registry_id())
        {
            registrations.push(registration);
        }
        let envelope = record.try_to_evidence_envelope()?;
        assert_eq!(envelope.evidence_oid(), record.evidence_oid());
        assert_eq!(
            envelope.selection_policy_oid(),
            record.selected_policy_oid()
        );
        assert_eq!(envelope.regime_epoch(), record.regime_epoch());
        assert_eq!(
            envelope.fallback(),
            FallbackBehavior::DeterministicPolicy {
                policy_oid: record.pinned_fallback_oid()
            }
        );
        let window = envelope
            .calibration_window()
            .ok_or_else(|| io::Error::other("statistical envelope omitted its source window"))?;
        assert_eq!(window.start_seq(), record.identity_window().first());
        assert_eq!(window.end_seq(), record.batch().last() + 1);
        match envelope.claim() {
            EvidenceClaim::StatisticalClaim {
                population,
                sampling_rule,
                power_or_effective_sample_size,
                assumptions,
                ..
            } => {
                assert!(!population.is_empty());
                assert!(!sampling_rule.is_empty());
                assert!(!power_or_effective_sample_size.is_empty());
                let substantive_assumptions = assumptions
                    .iter()
                    .filter(|assumption| !assumption.starts_with("registry-disclosure:"))
                    .cloned()
                    .collect::<Vec<_>>();
                assert_eq!(
                    substantive_assumptions,
                    expected_substantive_assumptions(*record)?,
                    "monitor {:?} changed or omitted a substantive model assumption",
                    record.monitor_kind()
                );
                let registered_disclosures = assumptions
                    .iter()
                    .filter_map(|assumption| assumption.strip_prefix("registry-disclosure:"))
                    .collect::<Vec<_>>();
                assert_eq!(
                    registered_disclosures,
                    registration.required_disclosures(),
                    "monitor {:?} did not carry its exact registry disclosures",
                    record.monitor_kind()
                );
            }
            other => {
                return Err(io::Error::other(format!(
                    "statistical monitor projected a stronger claim: {other:?}"
                ))
                .into());
            }
        }
    }

    registrations.sort_by_key(|registration| registration.registry_id());
    monitor_registrations.sort_unstable();
    assert_eq!(
        monitor_registrations,
        [
            (StatisticalMonitorKind::EProcess, "FG-CAL-01"),
            (StatisticalMonitorKind::ConformalThreshold, "FG-CAL-02"),
            (StatisticalMonitorKind::ExplorationBudget, "FG-CAL-01"),
            (StatisticalMonitorKind::DrainProgress, "FG-CAL-01"),
            (StatisticalMonitorKind::RegimeChange, "FG-CAL-01"),
            (StatisticalMonitorKind::OffPolicyEvaluation, "FG-EVID-01"),
            (StatisticalMonitorKind::AnnRecall, "FG-CAL-03"),
        ]
    );
    assert_eq!(
        registrations
            .iter()
            .map(|registration| registration.registry_id())
            .collect::<Vec<_>>(),
        ["FG-CAL-01", "FG-CAL-02", "FG-CAL-03", "FG-EVID-01"]
    );
    for registration in registrations {
        let registry = parsed_evidence_registry_row(registration.registry_id())?;
        assert!(registration_matches_registry(registration, &registry));
        assert!(
            !registration.binds_to().is_empty(),
            "{} remained dead instrumentation",
            registration.registry_id()
        );

        let mut missing = registry.clone();
        missing.required_disclosures.pop();
        assert!(!registration_matches_registry(registration, &missing));
        let mut substituted = registry.clone();
        substituted.required_disclosures[0] = "arbitrary substitute prose".to_owned();
        assert!(!registration_matches_registry(registration, &substituted));
    }

    let mut tampered = fixture.statistical_log_bytes;
    let final_byte = tampered
        .last_mut()
        .ok_or_else(|| io::Error::other("canonical statistical log was empty"))?;
    *final_byte ^= 1;
    assert!(
        read_statistical_log(&tampered, 27).is_err(),
        "a post-registration evidence mutation bypassed identity verification"
    );
    Ok(())
}

#[test]
fn adaptive_decision_classification_is_closed_and_substitution_fails() -> TestResult {
    for class in [
        LogicalEffectClass::AnswerPreservingPhysical,
        LogicalEffectClass::AnswerAffectingExecution,
        LogicalEffectClass::CanonicalStateAffecting,
    ] {
        let root = DecisionPolicyEpoch::try_root(
            "policy:classification",
            0,
            DecisionPolicyScope::new(oid(70)),
            class,
            oid(90),
            oid(90),
        )?;
        let encoded = root.try_to_canonical_bytes()?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&encoded)?,
            root
        );
    }

    let fixture = run_fixture()?;
    let root = DecisionPolicyEpoch::try_root_from_canonical_bytes(&fixture.root_epoch_bytes)?;
    let root_oid = FixtureOnlyIdentityAuthority::epoch_oid(&root)?;
    let mut substituted_class = fixture.promoted_epoch_bytes.clone();
    let effect_offset = unique_field_payload_offset(&substituted_class, 0x04, 1)?;
    substituted_class[effect_offset] = LogicalEffectClass::AnswerPreservingPhysical as u8;
    assert_eq!(
        DecisionPolicyEpoch::try_promoted_from_canonical_bytes(
            &substituted_class,
            &root,
            root_oid,
            &fixture.promotion_envelopes,
        ),
        Err(DecisionPolicyEpochError::LogicalEffectClassChanged)
    );

    let root_bytes = root.try_to_canonical_bytes()?;
    let effect_offset = unique_field_payload_offset(&root_bytes, 0x04, 1)?;
    for tag in 0..=u8::MAX {
        let mut candidate = root_bytes.clone();
        candidate[effect_offset] = tag;
        let decoded = DecisionPolicyEpoch::try_root_from_canonical_bytes(&candidate);
        match tag {
            0x01 => assert_eq!(
                decoded?.logical_effect_class(),
                LogicalEffectClass::AnswerPreservingPhysical
            ),
            0x02 => assert_eq!(
                decoded?.logical_effect_class(),
                LogicalEffectClass::AnswerAffectingExecution
            ),
            0x03 => assert_eq!(
                decoded?.logical_effect_class(),
                LogicalEffectClass::CanonicalStateAffecting
            ),
            actual => assert_eq!(
                decoded,
                Err(DecisionPolicyEpochError::InvalidLogicalEffectClassTag { actual }),
                "unassigned logical-effect tag {actual:#04x} was accepted"
            ),
        }
    }

    let mut substituted_scope = fixture.promoted_epoch_bytes;
    let scope_offset = unique_field_payload_offset(&substituted_scope, 0x03, 32)?;
    substituted_scope[scope_offset] ^= 1;
    assert_eq!(
        DecisionPolicyEpoch::try_promoted_from_canonical_bytes(
            &substituted_scope,
            &root,
            root_oid,
            &fixture.promotion_envelopes,
        ),
        Err(DecisionPolicyEpochError::ScopeChanged)
    );
    Ok(())
}

#[test]
fn sextant_evidence_promotes_then_stickily_reverts_on_regime_shift() -> TestResult {
    const LAB_SEED: u64 = 0x05e5_7a17;
    let first = run_fixture_in_lab(LAB_SEED)?;

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
    assert_eq!(sequential.outcome().selected_policy_oid(), oid(40));

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
    let decoded_root = DecisionPolicyEpoch::try_root_from_canonical_bytes(&first.root_epoch_bytes)?;
    assert_eq!(
        first.promoted_epoch.previous_epoch_oid(),
        Some(FixtureOnlyIdentityAuthority::epoch_oid(&decoded_root)?)
    );
    let mut expected_promotion_refs: Vec<_> = records_for_promotion(&first.statistical_log)
        .iter()
        .map(|record| record.evidence_oid())
        .collect();
    expected_promotion_refs.sort();
    assert_eq!(
        first.promoted_epoch.evidence_refs(),
        expected_promotion_refs
    );
    assert_eq!(first.promotion_envelopes.len(), 4);
    for record in records_for_promotion(&first.statistical_log) {
        let envelope = first
            .promotion_envelopes
            .iter()
            .find(|envelope| envelope.evidence_oid() == record.evidence_oid())
            .ok_or_else(|| io::Error::other("promotion envelope omitted a log record"))?;
        assert_eq!(envelope.selection_policy_oid(), oid(40));
        match record.statistic() {
            StatisticalStatistic::OffPolicyEvaluation { strata_oid, .. } => {
                assert_eq!(
                    envelope.strata_identity(),
                    StrataIdentity::Bound(strata_oid)
                );
                assert_eq!(
                    envelope.propensity_support_identity(),
                    PropensitySupportIdentity::Bound(record.evidence_oid())
                );
            }
            _ => {
                assert_eq!(envelope.strata_identity(), StrataIdentity::NotApplicable);
                assert_eq!(
                    envelope.propensity_support_identity(),
                    PropensitySupportIdentity::NotApplicable
                );
            }
        }
        assert_eq!(
            envelope.fallback(),
            FallbackBehavior::DeterministicPolicy {
                policy_oid: oid(90)
            }
        );
        let error_control = match envelope.claim() {
            EvidenceClaim::StatisticalClaim { error_control, .. } => *error_control,
            _ => {
                return Err(io::Error::other("promotion envelope was not statistical").into());
            }
        };
        let expected_alpha: Option<f64> = match record.monitor_kind() {
            StatisticalMonitorKind::ExplorationBudget => Some(0.5),
            StatisticalMonitorKind::ConformalThreshold => Some(0.2),
            StatisticalMonitorKind::EProcess => Some(0.25),
            StatisticalMonitorKind::OffPolicyEvaluation => None,
            StatisticalMonitorKind::DrainProgress
            | StatisticalMonitorKind::RegimeChange
            | StatisticalMonitorKind::AnnRecall => {
                return Err(
                    io::Error::other("unexpected monitor in promotion envelope inventory").into(),
                );
            }
        };
        match (error_control, expected_alpha) {
            (StatisticalErrorControl::Alpha(alpha), Some(expected)) => {
                assert_eq!(alpha.get().to_bits(), expected.to_bits());
            }
            (StatisticalErrorControl::NotApplicable, None) => {}
            _ => {
                return Err(
                    io::Error::other("promotion envelope used the wrong error control").into(),
                );
            }
        }
        if record.monitor_kind() == StatisticalMonitorKind::OffPolicyEvaluation {
            match envelope.claim() {
                EvidenceClaim::StatisticalClaim {
                    error_control: StatisticalErrorControl::NotApplicable,
                    power_or_effective_sample_size,
                    assumptions,
                    ..
                } => {
                    assert!(
                        power_or_effective_sample_size.contains("error_control=not-applicable")
                    );
                    assert!(
                        assumptions
                            .iter()
                            .any(|assumption| assumption.contains("alpha does not apply"))
                    );
                }
                _ => {
                    return Err(io::Error::other("OPE envelope was not statistical").into());
                }
            }
        }
    }

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
    assert_eq!(
        read_persisted_regime_evidence(&first.regime_evidence_bytes, first.regime_evidence_oid,)?,
        *shifted
    );

    let sticky = first
        .regime_evidence
        .last()
        .ok_or_else(|| io::Error::other("regime fixture produced no evidence"))?;
    assert_eq!(sticky.selection(), RegimePolicySelection::PinnedFallback);
    assert_eq!(sticky.selected_policy_oid(), oid(90));
    assert_eq!(sticky.fallback_sequence(), Some(65));

    assert_eq!(first.reverted_epoch.version(), 2);
    assert_eq!(first.reverted_epoch.pinned_table_oid(), oid(90));
    assert_eq!(first.reverted_epoch.fallback_oid(), oid(90));
    assert_eq!(
        first.reverted_epoch.previous_epoch_oid(),
        Some(FixtureOnlyIdentityAuthority::epoch_oid(
            &first.promoted_epoch
        )?)
    );
    let persisted_regime_record = regime_record(&first.statistical_log)?;
    assert_eq!(
        first.reverted_epoch.evidence_refs(),
        &[persisted_regime_record.evidence_oid()]
    );
    assert_eq!(first.fallback_envelope.selection_policy_oid(), oid(90));
    match first.fallback_envelope.claim() {
        EvidenceClaim::StatisticalClaim {
            error_control: StatisticalErrorControl::NotApplicable,
            power_or_effective_sample_size,
            assumptions,
            ..
        } => {
            assert!(power_or_effective_sample_size.contains("error_control=not-applicable"));
            assert!(
                assumptions
                    .iter()
                    .any(|assumption| assumption.contains("alpha does not apply"))
            );
        }
        _ => {
            return Err(io::Error::other("fallback envelope was not statistical").into());
        }
    }
    assert_eq!(first.statistical_log.len(), 27);
    for (monitor, expected) in [
        (StatisticalMonitorKind::ExplorationBudget, 2),
        (StatisticalMonitorKind::ConformalThreshold, 6),
        (StatisticalMonitorKind::EProcess, 3),
        (StatisticalMonitorKind::OffPolicyEvaluation, 4),
        (StatisticalMonitorKind::DrainProgress, 3),
        (StatisticalMonitorKind::RegimeChange, 8),
        (StatisticalMonitorKind::AnnRecall, 1),
    ] {
        assert_eq!(
            first
                .statistical_log
                .records()
                .iter()
                .filter(|record| record.monitor_kind() == monitor)
                .count(),
            expected,
            "monitor {monitor:?} did not persist every decision-bearing observation batch"
        );
    }
    assert!(
        first
            .statistical_log
            .records()
            .iter()
            .any(|record| record.monitor_kind() == StatisticalMonitorKind::ConformalThreshold)
    );
    assert!(
        first
            .statistical_log
            .records()
            .iter()
            .any(|record| record.monitor_kind() == StatisticalMonitorKind::AnnRecall)
    );
    let logged_ann_recall = first
        .statistical_log
        .records()
        .iter()
        .rev()
        .copied()
        .find(|record| record.monitor_kind() == StatisticalMonitorKind::AnnRecall)
        .ok_or_else(|| io::Error::other("statistical log omitted ANN recall evidence"))?;
    assert_eq!(
        logged_ann_recall.selected_policy_oid(),
        first
            .ann_recall
            .evidence
            .selected_policy_oid()
            .ok_or_else(|| {
                io::Error::other("terminal ANN recall evidence omitted its selected policy")
            })?
    );
    match logged_ann_recall.statistic() {
        StatisticalStatistic::AnnRecall {
            intersection_hits,
            complete,
            action,
            action_reason,
            ..
        } => {
            assert_eq!(intersection_hits, 11);
            assert!(complete);
            assert_eq!(action, AnnRecallAction::Candidate);
            assert_eq!(
                action_reason,
                AnnRecallActionReason::CandidateRecallSatisfied
            );
        }
        _ => {
            return Err(io::Error::other(
                "ANN recall monitor carried the wrong statistical payload",
            )
            .into());
        }
    }
    reconstruct_epochs_from_persisted_records(&first)?;

    let sketch_records = first.sketch.maintenance_log.records();
    assert_eq!(sketch_records.len(), SKETCH_MAINTENANCE_MAX_RECORDS);
    assert_eq!(sketch_records[0].family, SketchFamily::CountMin);
    assert_eq!(sketch_records[0].outcome, SketchMaintenanceOutcome::Merged);
    assert_ne!(
        sketch_records[0].before_digest,
        sketch_records[0].after_digest
    );
    assert_eq!(sketch_records[1].family, SketchFamily::CountMin);
    assert_eq!(
        sketch_records[1].outcome,
        SketchMaintenanceOutcome::RebuildRequired
    );
    assert_eq!(
        sketch_records[1].before_digest,
        sketch_records[1].after_digest
    );
    assert_eq!(
        sketch_records[1].after_digest,
        SketchStateDigest::from_canonical_state(&first.sketch.final_state_bytes)
    );
    // The exact-deletion family: its merge moved the state, and because the
    // merge is ordered last its logged after-digest still names the family's
    // final canonical state.
    assert_eq!(sketch_records[2].family, SketchFamily::LabelCounts);
    assert_eq!(sketch_records[2].outcome, SketchMaintenanceOutcome::Merged);
    assert_ne!(
        sketch_records[2].before_digest,
        sketch_records[2].after_digest
    );
    assert_eq!(
        sketch_records[2].after_digest,
        SketchStateDigest::from_canonical_state(&first.sketch.label_counts_final_state_bytes)
    );
    let replayed_sketch_log = SketchMaintenanceLog::from_canonical_bytes(
        &first.sketch.maintenance_log_bytes,
        SKETCH_MAINTENANCE_DECODE_LIMITS,
        SKETCH_MAINTENANCE_MAX_RECORDS,
    )?;
    assert_eq!(replayed_sketch_log, first.sketch.maintenance_log);
    assert_eq!(
        replayed_sketch_log.to_canonical_bytes()?,
        first.sketch.maintenance_log_bytes
    );

    assert!(first.ann_recall.evidence.complete());
    assert_eq!(
        first.ann_recall.evidence.action(),
        Some(AnnRecallAction::Candidate)
    );
    assert_eq!(
        first.ann_recall.evidence.action_reason(),
        AnnRecallActionReason::CandidateRecallSatisfied
    );
    assert_eq!(first.no_regret.selections.len(), NO_REGRET_DECISION_COUNT);
    assert_eq!(
        first.no_regret.selections[1].mode(),
        NoRegretSelectionMode::RegimeResetFallback
    );
    assert_eq!(
        first.no_regret.replay_summary.event_count(),
        NO_REGRET_DECISION_COUNT + 1
    );
    assert_eq!(
        first.no_regret.replay_summary.completed_epochs(),
        NO_REGRET_DECISION_COUNT
    );

    let mut tampered_regime_evidence = first.regime_evidence_bytes.clone();
    let last = tampered_regime_evidence
        .last_mut()
        .ok_or_else(|| io::Error::other("persisted regime evidence was empty"))?;
    *last ^= 1;
    assert!(
        read_persisted_regime_evidence(&tampered_regime_evidence, first.regime_evidence_oid,)
            .is_err()
    );

    let replay = run_fixture_in_lab(LAB_SEED)?;
    assert_eq!(first.exploration, replay.exploration);
    assert_eq!(first.calibration, replay.calibration);
    assert_eq!(first.assessments, replay.assessments);
    assert_eq!(first.assessment, replay.assessment);
    assert_eq!(first.sequential_evidence, replay.sequential_evidence);
    assert_eq!(first.ope_prefixes, replay.ope_prefixes);
    assert_eq!(first.ope, replay.ope);
    assert_eq!(first.root_epoch_bytes, replay.root_epoch_bytes);
    assert_eq!(first.promotion_envelopes, replay.promotion_envelopes);
    assert_eq!(first.promoted_epoch, replay.promoted_epoch);
    assert_eq!(first.promoted_epoch_bytes, replay.promoted_epoch_bytes);
    assert_eq!(first.regime_evidence, replay.regime_evidence);
    assert_eq!(first.regime_evidence_oid, replay.regime_evidence_oid);
    assert_eq!(first.regime_evidence_bytes, replay.regime_evidence_bytes);
    assert_eq!(first.fallback_envelope, replay.fallback_envelope);
    assert_eq!(first.reverted_epoch, replay.reverted_epoch);
    assert_eq!(first.reverted_epoch_bytes, replay.reverted_epoch_bytes);
    assert_eq!(first.statistical_log, replay.statistical_log);
    assert_eq!(first.statistical_log_bytes, replay.statistical_log_bytes);
    assert_eq!(first.sketch, replay.sketch);
    assert_eq!(first.ann_recall, replay.ann_recall);
    assert_eq!(first.no_regret, replay.no_regret);
    Ok(())
}
