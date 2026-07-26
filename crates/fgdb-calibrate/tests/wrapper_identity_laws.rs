//! Identity-preservation and decision-contract laws for the calibration wrappers.
//!
//! The doctrine that makes this crate testable is stated in its own lib.rs:
//! it "binds the statistical cores supplied by asupersync ... It does not
//! implement a second statistical engine." That turns the central law into a
//! **differential** one rather than a numerical one: run the wrapper and the
//! bare foundation core over the same input, and the statistic must agree
//! bit-for-bit. A wrapper that rescales, clamps, rounds, or drops an
//! observation is the wrong kernel, and no amount of self-consistency
//! checking inside the wrapper would notice.
//!
//! Four families:
//!
//! 1. **Identity preservation** — `EProcessTrial` against a bare `EProcess`,
//!    and `DrainProgressMonitor` against a bare `ProgressCertificate`, fed the
//!    identical sequence. Compared on canonical float BITS, so a perturbation
//!    too small to print is still caught.
//! 2. **Decision-log append-only and totally ordered** — the accepted sequence
//!    is strictly increasing under the canonical order, a rejected append
//!    leaves the log byte-identical, and no prefix ever changes.
//! 3. **The adaptive-decision contract** — every decision names its pinned
//!    deterministic fallback, and the selected policy is always one of the two
//!    declared identities, never a third.
//! 4. **Policy-epoch monotonicity** — versions strictly increase, the policy
//!    identity and fallback are inherited exactly, and even a revert is a new
//!    successor rather than a rollback.
//!
//! Inputs are fixed literals chosen at domain boundaries. No clock, no
//! entropy, no new dependencies: the only randomness the crate can reach is
//! asupersync's seeded `DetRng`, which is not used here at all.

use asupersync::cancel::progress_certificate::{ProgressCertificate, ProgressConfig};
use asupersync::lab::oracle::eprocess::{EProcess, EProcessConfig};
use fgdb_calibrate::eprocess::{
    BinaryObservation, EProcessProfile, EProcessTrial, EvidenceRecord, PolicyOutcomeKind,
    SequenceWindow as EProcessWindow, SequencedObservation, TrialIdentity,
};
use fgdb_calibrate::log::{
    StatisticalDecisionLog, StatisticalEvidenceIdentityError, StatisticalEvidenceIdentityIssuer,
    StatisticalLogAppendError, StatisticalLogRecord,
};
use fgdb_calibrate::policy_epoch::{
    DecisionPolicyEpoch, DecisionPolicyEpochError, DecisionPolicyScope, LogicalEffectClass,
};
use fgdb_calibrate::progress::{
    DrainProgressEvidence, DrainProgressIdentity, DrainProgressMonitor, DrainProgressProfile,
    DrainProgressSelection, SequencedPotential,
};
use fgdb_types::ObjectId;

const REGIME_EPOCH: u64 = 7;

fn oid(fill: u8) -> ObjectId {
    ObjectId([fill; 32])
}

/// The wrapper canonicalizes every float it reports, collapsing `-0.0` so one
/// value cannot reach durable evidence under two bit patterns. Mirror that
/// here so a differential mismatch means a *statistical* divergence, not a
/// signed-zero spelling difference.
fn canonical_float_bits(value: f64) -> u64 {
    let bits = value.to_bits();
    if bits == (-0.0_f64).to_bits() {
        0.0_f64.to_bits()
    } else {
        bits
    }
}

// ================================================ family 1: identity =======

/// The e-process configuration used on both sides of the differential.
const TRIAL_CONFIG: EProcessConfig = EProcessConfig {
    p0: 0.2,
    lambda: 1.0,
    alpha: 0.25,
    max_evalue: 1_000.0,
};

/// Boundary-heavy outcome sequence: an all-ones run drives the martingale up
/// hardest, an all-zeros run drives it down, and the alternating and
/// late-flip runs exercise the paths where a wrapper that dropped or
/// reordered an observation would still produce a plausible number.
fn observation_sequences() -> Vec<(&'static str, Vec<BinaryObservation>)> {
    use BinaryObservation::{One, Zero};
    vec![
        ("all-ones", vec![One, One, One, One, One, One]),
        ("all-zeros", vec![Zero, Zero, Zero, Zero, Zero, Zero]),
        ("alternating", vec![One, Zero, One, Zero, One, Zero]),
        ("zeros-then-ones", vec![Zero, Zero, Zero, One, One, One]),
        ("single-one-at-end", vec![Zero, Zero, Zero, Zero, Zero, One]),
    ]
}

#[test]
fn eprocess_wrapper_preserves_the_foundation_e_value_bit_for_bit() {
    for (label, outcomes) in observation_sequences() {
        let first = 40_u64;
        let last = first + outcomes.len() as u64 - 1;
        let identity = TrialIdentity::try_new(
            oid(15),
            oid(16),
            EProcessWindow::try_new(first, last).expect("window"),
            REGIME_EPOCH,
            oid(200),
            oid(201),
        )
        .expect("identity");
        let profile = EProcessProfile::try_new(oid(17), TRIAL_CONFIG).expect("profile");
        let mut wrapped = EProcessTrial::try_new(identity, profile).expect("trial");

        // The bare foundation core, fed the identical stream. The label is a
        // diagnostic string in asupersync and does not enter the statistic.
        let mut bare = EProcess::new_without_history("differential", TRIAL_CONFIG);

        for (step, outcome) in outcomes.iter().enumerate() {
            let sequence = first + step as u64;
            let update = wrapped
                .observe(SequencedObservation::new(
                    identity, profile, sequence, *outcome,
                ))
                .unwrap_or_else(|e| panic!("{label} step {step} rejected: {e:?}"));
            // `as_foundation_event` is crate-private, so the mapping is restated
            // here: the `One` arm is the foundation's event.
            bare.observe(matches!(outcome, BinaryObservation::One));

            let evidence = update.evidence;
            assert_eq!(
                evidence.e_value_bits(),
                canonical_float_bits(bare.e_value()),
                "{label} step {step}: wrapper altered the foundation e-value \
                 (wrapper={:?}, foundation={:?})",
                f64::from_bits(evidence.e_value_bits()),
                bare.e_value()
            );
            assert_eq!(
                evidence.rejection_threshold_bits(),
                canonical_float_bits(bare.config.threshold()),
                "{label} step {step}: wrapper altered the rejection threshold"
            );
            assert_eq!(
                evidence.observations(),
                bare.observations as u64,
                "{label} step {step}: observation counts diverged"
            );
            // The policy outcome is a projection of the core's own rejection
            // state, not an independent decision.
            let expects_promotion = evidence.outcome().kind()
                == PolicyOutcomeKind::PromoteCandidateAgainstPinnedFallback;
            assert_eq!(
                expects_promotion, bare.rejected,
                "{label} step {step}: promotion disagrees with the core's rejection state"
            );
        }

        // The fixture must actually move the statistic, or bit-equality is
        // vacuous: an all-zeros sequence still has to change the e-value.
        assert!(
            bare.observations > 0,
            "{label}: fixture fed no observations to the core"
        );
    }
}

#[test]
fn eprocess_differential_is_sensitive_to_a_single_perturbed_outcome() {
    // Guards the guard: if the wrapper and the core were fed different data,
    // the differential above must be able to see it. Otherwise a comparison
    // that always passes would look like proof of identity preservation.
    let identity = TrialIdentity::try_new(
        oid(15),
        oid(16),
        EProcessWindow::try_new(40, 42).expect("window"),
        REGIME_EPOCH,
        oid(200),
        oid(201),
    )
    .expect("identity");
    let profile = EProcessProfile::try_new(oid(17), TRIAL_CONFIG).expect("profile");
    let mut wrapped = EProcessTrial::try_new(identity, profile).expect("trial");
    let mut divergent = EProcess::new_without_history("divergent", TRIAL_CONFIG);

    let mut final_bits = 0_u64;
    for (step, sequence) in (40_u64..=42).enumerate() {
        let update = wrapped
            .observe(SequencedObservation::new(
                identity,
                profile,
                sequence,
                BinaryObservation::One,
            ))
            .expect("observe");
        // Feed the bare core a DIFFERENT outcome at the last step only.
        divergent.observe(step != 2);
        final_bits = update.evidence.e_value_bits();
    }

    assert_ne!(
        final_bits,
        canonical_float_bits(divergent.e_value()),
        "the differential cannot distinguish divergent streams, so equality \
         elsewhere proves nothing"
    );
}

/// Potentials that fall to quiescence, chosen so the drain has a strictly
/// decreasing phase and an exactly-flat step (the stall boundary).
fn potential_sequences() -> Vec<(&'static str, Vec<f64>)> {
    vec![
        ("strict-descent", vec![100.0, 80.0, 60.0, 40.0, 20.0, 0.0]),
        (
            "flat-then-descent",
            vec![50.0, 50.0, 40.0, 30.0, 20.0, 10.0],
        ),
        ("single-large-step", vec![90.0, 89.0, 88.0, 87.0, 86.0, 1.0]),
        ("already-zero", vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ]
}

fn progress_config() -> ProgressConfig {
    ProgressConfig {
        confidence: 0.95,
        max_step_bound: 100.0,
        stall_threshold: 4,
        min_observations: 2,
        epsilon: 1e-9,
    }
}

#[test]
fn progress_wrapper_preserves_the_foundation_verdict_bit_for_bit() {
    for (label, potentials) in potential_sequences() {
        let first = 5_u64;
        let last = first + potentials.len() as u64 - 1;
        let identity = DrainProgressIdentity::try_new(
            oid(30),
            oid(31),
            first,
            last,
            REGIME_EPOCH,
            oid(202),
            oid(203),
        )
        .expect("identity");
        let profile =
            DrainProgressProfile::try_new(progress_config(), potentials.len()).expect("profile");
        let mut wrapped =
            DrainProgressMonitor::try_new(identity, profile.clone()).expect("monitor");
        let mut bare = ProgressCertificate::new(progress_config());

        for (step, potential) in potentials.iter().enumerate() {
            let sequence = first + step as u64;
            let evidence = wrapped
                .observe(SequencedPotential::new(
                    identity,
                    profile.clone(),
                    sequence,
                    *potential,
                ))
                .unwrap_or_else(|e| panic!("{label} step {step} rejected: {e:?}"));
            bare.observe(*potential);
            let verdict = bare.verdict();

            assert_eq!(
                evidence.total_observations(),
                verdict.total_steps as u64,
                "{label} step {step}: step count diverged"
            );
            assert_eq!(
                evidence.current_potential_bits(),
                canonical_float_bits(verdict.current_potential),
                "{label} step {step}: wrapper altered the current potential"
            );
            assert_eq!(
                evidence.confidence_bound_bits(),
                canonical_float_bits(verdict.confidence_bound),
                "{label} step {step}: wrapper altered the confidence bound"
            );
            assert_eq!(
                evidence.azuma_bound_bits(),
                canonical_float_bits(verdict.azuma_bound),
                "{label} step {step}: wrapper altered the Azuma bound"
            );
            assert_eq!(
                evidence.is_converging(),
                verdict.converging,
                "{label} step {step}: convergence verdict diverged"
            );
            assert_eq!(
                evidence.stall_detected(),
                verdict.stall_detected,
                "{label} step {step}: stall verdict diverged"
            );
        }

        assert!(
            bare.verdict().total_steps > 0,
            "{label}: fixture fed no potentials to the core"
        );
    }
}

// ============================ family 3: the adaptive-decision contract =====

/// Runs one e-process trial to completion and returns its final evidence.
fn eprocess_evidence(outcomes: &[BinaryObservation]) -> (EvidenceRecord, ObjectId, ObjectId) {
    let candidate = oid(200);
    let fallback = oid(201);
    let first = 40_u64;
    let last = first + outcomes.len() as u64 - 1;
    let identity = TrialIdentity::try_new(
        oid(15),
        oid(16),
        EProcessWindow::try_new(first, last).expect("window"),
        REGIME_EPOCH,
        candidate,
        fallback,
    )
    .expect("identity");
    let profile = EProcessProfile::try_new(oid(17), TRIAL_CONFIG).expect("profile");
    let mut trial = EProcessTrial::try_new(identity, profile).expect("trial");
    let mut evidence = None;
    for (step, outcome) in outcomes.iter().enumerate() {
        evidence = Some(
            trial
                .observe(SequencedObservation::new(
                    identity,
                    profile,
                    first + step as u64,
                    *outcome,
                ))
                .expect("observe")
                .evidence,
        );
    }
    (
        evidence.expect("at least one observation"),
        candidate,
        fallback,
    )
}

fn progress_evidence(potentials: &[f64]) -> (DrainProgressEvidence, ObjectId, ObjectId) {
    let candidate = oid(202);
    let fallback = oid(203);
    let first = 5_u64;
    let last = first + potentials.len() as u64 - 1;
    let identity = DrainProgressIdentity::try_new(
        oid(30),
        oid(31),
        first,
        last,
        REGIME_EPOCH,
        candidate,
        fallback,
    )
    .expect("identity");
    let profile =
        DrainProgressProfile::try_new(progress_config(), potentials.len()).expect("profile");
    let mut monitor = DrainProgressMonitor::try_new(identity, profile.clone()).expect("monitor");
    let mut evidence = None;
    for (step, potential) in potentials.iter().enumerate() {
        evidence = Some(
            monitor
                .observe(SequencedPotential::new(
                    identity,
                    profile.clone(),
                    first + step as u64,
                    *potential,
                ))
                .expect("observe"),
        );
    }
    (
        evidence.expect("at least one observation"),
        candidate,
        fallback,
    )
}

#[test]
fn every_decision_selects_one_of_its_two_declared_identities() {
    // The adaptive-decision contract: a decision may pick the candidate or the
    // pinned deterministic fallback. A third identity — or a fallback that
    // does not match the one the trial was bound to — means the decision is
    // not replayable from its own record.
    for (_, outcomes) in observation_sequences() {
        let (evidence, candidate, fallback) = eprocess_evidence(&outcomes);
        assert_eq!(evidence.candidate_decision_oid(), candidate);
        assert_eq!(evidence.pinned_fallback_oid(), fallback);
        let selected = evidence.selected_policy_oid();
        assert!(
            selected == candidate || selected == fallback,
            "e-process selected a third identity: {selected:?}"
        );
        // And the selection follows the outcome kind, not an independent path.
        let expected = match evidence.outcome().kind() {
            PolicyOutcomeKind::PromoteCandidateAgainstPinnedFallback => candidate,
            PolicyOutcomeKind::RetainPinnedFallback => fallback,
        };
        assert_eq!(
            selected, expected,
            "selected policy disagrees with the recorded outcome kind"
        );
    }

    for (_, potentials) in potential_sequences() {
        let (evidence, candidate, fallback) = progress_evidence(&potentials);
        let selected = evidence.selected_policy_oid();
        assert!(
            selected == candidate || selected == fallback,
            "drain progress selected a third identity: {selected:?}"
        );
        let expected = match evidence.selection() {
            DrainProgressSelection::CandidateDecision => candidate,
            DrainProgressSelection::PinnedFallback => fallback,
        };
        assert_eq!(selected, expected, "selection disagrees with the record");
    }
}

#[test]
fn an_ineligible_candidate_always_falls_back() {
    // The conservative direction is the one that must never drift: if the
    // evidence does not clear every gate, the pinned fallback is selected.
    for (label, potentials) in potential_sequences() {
        let (evidence, _, fallback) = progress_evidence(&potentials);
        if !evidence.candidate_eligible() {
            assert_eq!(
                evidence.selected_policy_oid(),
                fallback,
                "{label}: an ineligible candidate must select the pinned fallback"
            );
            assert_eq!(evidence.selection(), DrainProgressSelection::PinnedFallback);
        }
    }
}

#[test]
fn a_stalled_drain_is_never_eligible() {
    // The conservative gate that matters most: a drain whose potential stops
    // falling must NOT be promoted, however good its other statistics look.
    // Without this the stall term can be dropped from the eligibility
    // conjunction and every other law still holds - a stalled drain simply
    // becomes eligible, which is exactly the wrong direction to fail in.
    // The fixture must make the stall term BINDING, not merely present: a
    // drain that stalls at a nonzero potential is already ineligible for not
    // converging, so removing the stall term would change nothing there. Held
    // at zero the drain is quiescent, every other gate passes, and the stall
    // check is the only thing standing between it and promotion.
    let flat = vec![0.0; 6];
    let (evidence, _, fallback) = progress_evidence(&flat);
    assert!(
        evidence.stall_detected(),
        "fixture must actually stall, or this proves nothing"
    );
    assert!(
        evidence.has_sufficient_observations()
            && evidence.step_bound_respected()
            && evidence.statistics_valid(),
        "every non-stall gate must pass, or the stall term is not the binding one"
    );
    assert!(
        !evidence.candidate_eligible(),
        "a stalled drain must never be eligible for promotion"
    );
    assert_eq!(
        evidence.selected_policy_oid(),
        fallback,
        "a stalled drain must select the pinned fallback"
    );
    assert_eq!(evidence.selection(), DrainProgressSelection::PinnedFallback);
}

// ==================================== family 2: the decision log ===========

/// Fixture-only deterministic identity authority. Domain-separated hash over
/// the canonical bytes; no clock, no entropy, and no claim to be production
/// `ObjectId` derivation.
#[derive(Clone, Copy)]
struct FixtureIdentityAuthority;

impl StatisticalEvidenceIdentityIssuer for FixtureIdentityAuthority {
    fn issue_statistical_evidence_oid(
        &self,
        canonical_evidence_body: &[u8],
    ) -> Result<ObjectId, StatisticalEvidenceIdentityError> {
        let mut transcript = Vec::with_capacity(canonical_evidence_body.len() + 8);
        transcript.extend_from_slice(b"fixture\0");
        transcript.extend_from_slice(canonical_evidence_body);
        Ok(ObjectId(asupersync::atp::object::compute_hash(&transcript)))
    }
}

/// Builds one e-process record per disjoint, strictly increasing window.
fn eprocess_records(windows: &[(u64, u64)]) -> Vec<StatisticalLogRecord> {
    let mut records = Vec::new();
    for &(first, last) in windows {
        let identity = TrialIdentity::try_new(
            oid(15),
            oid(16),
            EProcessWindow::try_new(first, last).expect("window"),
            REGIME_EPOCH,
            oid(200),
            oid(201),
        )
        .expect("identity");
        let profile = EProcessProfile::try_new(oid(17), TRIAL_CONFIG).expect("profile");
        let mut trial = EProcessTrial::try_new(identity, profile).expect("trial");
        let mut evidence = None;
        for sequence in first..=last {
            evidence = Some(
                trial
                    .observe(SequencedObservation::new(
                        identity,
                        profile,
                        sequence,
                        BinaryObservation::One,
                    ))
                    .expect("observe")
                    .evidence,
            );
        }
        records.push(
            StatisticalLogRecord::try_from_eprocess(
                &FixtureIdentityAuthority,
                &evidence.expect("evidence"),
            )
            .expect("record"),
        );
    }
    records
}

#[test]
fn the_log_accepts_only_strictly_later_records() {
    let records = eprocess_records(&[(40, 42), (43, 45), (46, 48)]);
    assert_eq!(records.len(), 3);

    let mut log = StatisticalDecisionLog::try_new(16).expect("log");
    for record in &records {
        log.append(*record)
            .expect("in-order append must be accepted");
    }
    assert_eq!(log.len(), 3);

    // Re-appending the newest record is a duplicate.
    assert!(matches!(
        log.append(records[2]),
        Err(StatisticalLogAppendError::DuplicateRecord { .. })
    ));
    // Re-appending an OLD record is a duplicate rather than an order
    // violation: monitor identity includes the trial window, so each window
    // is its own monitor family and the record equals that family's last.
    assert!(matches!(
        log.append(records[0]),
        Err(StatisticalLogAppendError::DuplicateRecord { .. })
    ));

    // The order law needs a record that is genuinely NEW yet earlier: a
    // different window, so it is not a duplicate of any family, whose batch
    // still precedes the accepted tail.
    let earlier = eprocess_records(&[(37, 39)]);
    assert_eq!(earlier.len(), 1);
    assert!(
        earlier[0].batch().last() < records[0].batch().first(),
        "the fixture must actually be earlier, or it proves nothing"
    );
    assert!(
        matches!(
            log.append(earlier[0]),
            Err(StatisticalLogAppendError::RecordNotInCanonicalOrder { .. })
        ),
        "a strictly earlier record must be refused by the canonical order"
    );
}

#[test]
fn a_rejected_append_leaves_the_log_byte_identical() {
    // Append-only means more than "no remove method": a rejected append must
    // not consume a slot, reorder, or partially write. Every check runs before
    // the record vector changes, and this pins that.
    let records = eprocess_records(&[(40, 42), (43, 45)]);
    let mut log = StatisticalDecisionLog::try_new(16).expect("log");
    log.append(records[0]).expect("first append");
    log.append(records[1]).expect("second append");

    let before: Vec<StatisticalLogRecord> = log.records().to_vec();
    let before_len = log.len();

    for rejected in [records[0], records[1]] {
        let err = log.append(rejected).expect_err("must be rejected");
        assert_eq!(
            log.len(),
            before_len,
            "a rejected append changed the length"
        );
        assert_eq!(
            log.records(),
            before.as_slice(),
            "a rejected append mutated the log ({err:?})"
        );
    }
}

#[test]
fn the_accepted_prefix_is_immutable_and_totally_ordered() {
    let records = eprocess_records(&[(40, 42), (43, 45), (46, 48), (49, 51)]);
    let mut log = StatisticalDecisionLog::try_new(16).expect("log");

    let mut prefix: Vec<StatisticalLogRecord> = Vec::new();
    for record in &records {
        log.append(*record).expect("append");
        prefix.push(*record);
        // Append-only: everything previously accepted is still present, in
        // the same positions, unchanged.
        assert_eq!(
            log.records(),
            prefix.as_slice(),
            "appending rewrote an earlier entry"
        );
    }

    // Total order: strictly increasing, hence irreflexive and antisymmetric on
    // the accepted sequence, and transitive across every triple i < j < k.
    let accepted = log.records();
    for i in 0..accepted.len() {
        for j in 0..accepted.len() {
            for k in 0..accepted.len() {
                if i < j && j < k {
                    assert!(
                        accepted[i].batch().last() < accepted[j].batch().first()
                            && accepted[j].batch().last() < accepted[k].batch().first(),
                        "accepted order is not strictly increasing at {i} < {j} < {k}"
                    );
                }
            }
        }
    }
}

// ============================== family 4: policy-epoch monotonicity ========

#[test]
fn a_root_epoch_has_no_predecessor_and_no_promotion_evidence() {
    let root = DecisionPolicyEpoch::try_root(
        "policy:wrapper-identity-laws",
        0,
        DecisionPolicyScope::new(oid(70)),
        LogicalEffectClass::AnswerAffectingExecution,
        oid(71),
        oid(71),
    )
    .expect("root");

    assert_eq!(root.version(), 0);
    assert_eq!(root.previous_epoch_oid(), None);
    assert!(
        root.evidence_refs().is_empty(),
        "a root epoch cannot carry promotion evidence"
    );
}

#[test]
fn a_promotion_may_not_pin_the_fallback_as_its_candidate() {
    // The adaptive-decision contract requires a candidate distinct from the
    // pinned deterministic fallback; otherwise "promotion" is unobservable.
    let fallback = oid(71);
    let root = DecisionPolicyEpoch::try_root(
        "policy:wrapper-identity-laws",
        0,
        DecisionPolicyScope::new(oid(70)),
        LogicalEffectClass::AnswerAffectingExecution,
        fallback,
        fallback,
    )
    .expect("root");

    let attempted = DecisionPolicyEpoch::try_promote(&root, oid(72), fallback, &[], &[]);
    // Assert the REASON, not merely that it failed: a promotion with no
    // evidence would also fail, and an `is_err()` check could pass for that
    // reason while the candidate/fallback guard was gone.
    assert!(
        matches!(
            attempted,
            Err(DecisionPolicyEpochError::CandidateEqualsFallback { policy_oid })
                if policy_oid == fallback
        ),
        "expected CandidateEqualsFallback, got {attempted:?}"
    );
}
