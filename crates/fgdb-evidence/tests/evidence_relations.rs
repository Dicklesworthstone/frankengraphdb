//! Mutation-oriented relations for evidence bindings, deterministic fallback,
//! and the closed replay vocabulary.
//!
//! The corpus is deterministic and boundary-heavy. It uses a fixed SplitMix64
//! seed, explicit zero/maximum integer edges, empty and 255/256-byte strings,
//! embedded NULs, and differently allocated but logically equal values. There
//! is no clock, entropy, or added dependency.
//!
//! `relation_hash` is deliberately only a deterministic test hash. The
//! production relation under test is the canonical transcript passed to the
//! caller-supplied project content hash by
//! `EvidenceEnvelope::binding_address_with`.
//!
//! MUTATION EVIDENCE. Each mutation was applied to the production kernel,
//! observed RED, reverted with a source patch, and observed GREEN:
//!
//! - `EB1 subset-only binding`: encode only `evidence_oid`.
//!   `envelope_address_changes_for_every_bound_component` and
//!   `seeded_boundary_corpus_is_stable_and_address_distinct` failed (2/6).
//! - `EB2 allocation-shaped serialization`: append `String::capacity()` to the
//!   encoded population. `identical_content_has_one_address_independent_of_allocation_shape`
//!   failed (1/6).
//! - `EB3 omitted strata identity`: remove field tag 4 from the production
//!   transcript. `envelope_address_changes_for_every_bound_component` failed
//!   on `strata_identity.variant` (1/6).
//! - `EB4 omitted propensity-support identity`: remove field tag 5 from the
//!   production transcript. `envelope_address_changes_for_every_bound_component`
//!   failed on `propensity_support_identity.variant` (1/6).
//! - `FB1 advisory wins`: return `selection_policy_oid` from `fallback()`.
//!   `mandatory_fallback_is_preserved_for_every_claim_kind` and
//!   `analytic_fallback_does_not_alias_the_advisory_selection` failed (2/6).
//! - `RC1 merged variant`: encode `WallClock` with the
//!   `vm_jit_compiler_artifacts` spelling.
//!   `replay_class_canonical_form_reaches_every_distinct_variant` failed (1/6).
//!
//! Thus all six integration relations are mutation-proven; none ships on
//! coverage or reasoning alone. Separate `E0061` and `E0308` compile-fail
//! doctests pin the mandatory fallback and the distinct strata/support roles.

use std::collections::BTreeSet;

use fgdb_claim::{EvidenceClaim, RefinementStatus, StatisticalErrorControl};
use fgdb_evidence::{
    CalibrationWindow, EVIDENCE_ENVELOPE_BINDING_VERSION, EvidenceEnvelope, FallbackBehavior,
    PropensitySupportIdentity, REPLAY_CLASS_VOCABULARY_VERSION, ReplayClass, StrataIdentity,
};
use fgdb_types::ObjectId;

fn oid(fill: u8) -> ObjectId {
    ObjectId([fill; 32])
}

/// Four independently seeded FNV/mix lanes make accidental collisions in this
/// bounded relation corpus implausible without pretending to be the production
/// cryptographic content hash.
fn relation_hash(bytes: &[u8]) -> [u8; 32] {
    const SEEDS: [u64; 4] = [
        0x243F_6A88_85A3_08D3,
        0x1319_8A2E_0370_7344,
        0xA409_3822_299F_31D0,
        0x082E_FA98_EC4E_6C89,
    ];
    let mut result = [0_u8; 32];
    for (lane, seed) in SEEDS.into_iter().enumerate() {
        let mut state = seed ^ u64::try_from(bytes.len()).expect("Rust slice lengths fit in u64");
        for &byte in bytes {
            state ^= u64::from(byte);
            state = state.wrapping_mul(0x0000_0100_0000_01B3);
            state ^= state >> 29;
        }
        state ^= state >> 30;
        state = state.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        state ^= state >> 27;
        state = state.wrapping_mul(0x94D0_49BB_1331_11EB);
        state ^= state >> 31;
        result[lane * 8..(lane + 1) * 8].copy_from_slice(&state.to_le_bytes());
    }
    result
}

fn address(envelope: &EvidenceEnvelope) -> ObjectId {
    envelope.binding_address_with(relation_hash)
}

#[derive(Clone)]
struct EnvelopeParts {
    claim: EvidenceClaim,
    evidence_oid: ObjectId,
    selection_policy_oid: ObjectId,
    strata_identity: StrataIdentity,
    propensity_support_identity: PropensitySupportIdentity,
    calibration_window: Option<CalibrationWindow>,
    regime_epoch: u64,
    fallback: FallbackBehavior,
}

impl EnvelopeParts {
    fn build(&self) -> EvidenceEnvelope {
        EvidenceEnvelope::new(
            self.claim.clone(),
            self.evidence_oid,
            self.selection_policy_oid,
            self.strata_identity,
            self.propensity_support_identity,
            self.calibration_window,
            self.regime_epoch,
            self.fallback,
        )
    }
}

fn statistical_claim() -> EvidenceClaim {
    EvidenceClaim::StatisticalClaim {
        population: "fixture population\0A".into(),
        sampling_rule: "every admitted operation".into(),
        error_control: StatisticalErrorControl::try_alpha(0.01).expect("valid alpha"),
        power_or_effective_sample_size: "n_eff=65536".into(),
        assumptions: vec!["exchangeable within epoch".into(), "bounded loss".into()],
    }
}

fn baseline_parts() -> EnvelopeParts {
    EnvelopeParts {
        claim: statistical_claim(),
        evidence_oid: oid(0x11),
        selection_policy_oid: oid(0x22),
        strata_identity: StrataIdentity::Bound(oid(0x44)),
        propensity_support_identity: PropensitySupportIdentity::Bound(oid(0x55)),
        calibration_window: Some(CalibrationWindow::new(0, u64::MAX).expect("nonempty window")),
        regime_epoch: u64::MAX,
        fallback: FallbackBehavior::DeterministicPolicy {
            policy_oid: oid(0x33),
        },
    }
}

fn assert_binding_changed(label: &str, left: &EvidenceEnvelope, right: &EvidenceEnvelope) {
    assert_ne!(
        left.to_canonical_binding_bytes(),
        right.to_canonical_binding_bytes(),
        "{label}: different bound content produced the same canonical transcript"
    );
    assert_ne!(
        address(left),
        address(right),
        "{label}: different bound content produced the same address"
    );
}

fn claim_field_pairs() -> Vec<(&'static str, EvidenceClaim, EvidenceClaim)> {
    vec![
        (
            "safety.invariant_id",
            EvidenceClaim::SafetyInvariant {
                invariant_id: "FG-INV-01".into(),
            },
            EvidenceClaim::SafetyInvariant {
                invariant_id: "FG-INV-02".into(),
            },
        ),
        (
            "formal.model_name",
            EvidenceClaim::FormalModelClaim {
                model_name: "model-a".into(),
                abstraction_boundary: "boundary".into(),
                checked_bounds: Some("actors=3".into()),
                refinement_status: RefinementStatus::ModelOnly,
            },
            EvidenceClaim::FormalModelClaim {
                model_name: "model-b".into(),
                abstraction_boundary: "boundary".into(),
                checked_bounds: Some("actors=3".into()),
                refinement_status: RefinementStatus::ModelOnly,
            },
        ),
        (
            "formal.abstraction_boundary",
            EvidenceClaim::FormalModelClaim {
                model_name: "model".into(),
                abstraction_boundary: "boundary-a".into(),
                checked_bounds: Some("actors=3".into()),
                refinement_status: RefinementStatus::ModelOnly,
            },
            EvidenceClaim::FormalModelClaim {
                model_name: "model".into(),
                abstraction_boundary: "boundary-b".into(),
                checked_bounds: Some("actors=3".into()),
                refinement_status: RefinementStatus::ModelOnly,
            },
        ),
        (
            "formal.checked_bounds",
            EvidenceClaim::FormalModelClaim {
                model_name: "model".into(),
                abstraction_boundary: "boundary".into(),
                checked_bounds: None,
                refinement_status: RefinementStatus::ModelOnly,
            },
            EvidenceClaim::FormalModelClaim {
                model_name: "model".into(),
                abstraction_boundary: "boundary".into(),
                checked_bounds: Some(String::new()),
                refinement_status: RefinementStatus::ModelOnly,
            },
        ),
        (
            "formal.refinement_status",
            EvidenceClaim::FormalModelClaim {
                model_name: "model".into(),
                abstraction_boundary: "boundary".into(),
                checked_bounds: None,
                refinement_status: RefinementStatus::ModelOnly,
            },
            EvidenceClaim::FormalModelClaim {
                model_name: "model".into(),
                abstraction_boundary: "boundary".into(),
                checked_bounds: None,
                refinement_status: RefinementStatus::RefinedToImplementation,
            },
        ),
        (
            "statistical.population",
            EvidenceClaim::StatisticalClaim {
                population: "a".into(),
                sampling_rule: "sampling".into(),
                error_control: StatisticalErrorControl::NotApplicable,
                power_or_effective_sample_size: "n".into(),
                assumptions: vec![],
            },
            EvidenceClaim::StatisticalClaim {
                population: "ab".into(),
                sampling_rule: "sampling".into(),
                error_control: StatisticalErrorControl::NotApplicable,
                power_or_effective_sample_size: "n".into(),
                assumptions: vec![],
            },
        ),
        (
            "statistical.sampling_rule",
            EvidenceClaim::StatisticalClaim {
                population: "population".into(),
                sampling_rule: "a".into(),
                error_control: StatisticalErrorControl::NotApplicable,
                power_or_effective_sample_size: "n".into(),
                assumptions: vec![],
            },
            EvidenceClaim::StatisticalClaim {
                population: "population".into(),
                sampling_rule: "ab".into(),
                error_control: StatisticalErrorControl::NotApplicable,
                power_or_effective_sample_size: "n".into(),
                assumptions: vec![],
            },
        ),
        (
            "statistical.field_boundaries",
            EvidenceClaim::StatisticalClaim {
                population: "a".into(),
                sampling_rule: "bc".into(),
                error_control: StatisticalErrorControl::NotApplicable,
                power_or_effective_sample_size: "n".into(),
                assumptions: vec![],
            },
            EvidenceClaim::StatisticalClaim {
                population: "ab".into(),
                sampling_rule: "c".into(),
                error_control: StatisticalErrorControl::NotApplicable,
                power_or_effective_sample_size: "n".into(),
                assumptions: vec![],
            },
        ),
        (
            "statistical.error_control",
            EvidenceClaim::StatisticalClaim {
                population: "p".into(),
                sampling_rule: "s".into(),
                error_control: StatisticalErrorControl::NotApplicable,
                power_or_effective_sample_size: "n".into(),
                assumptions: vec![],
            },
            EvidenceClaim::StatisticalClaim {
                population: "p".into(),
                sampling_rule: "s".into(),
                error_control: StatisticalErrorControl::try_alpha(f64::from_bits(
                    0x3FB9_9999_9999_999A,
                ))
                .expect("0.1 is valid"),
                power_or_effective_sample_size: "n".into(),
                assumptions: vec![],
            },
        ),
        (
            "statistical.power_or_effective_sample_size",
            statistical_claim(),
            EvidenceClaim::StatisticalClaim {
                population: "fixture population\0A".into(),
                sampling_rule: "every admitted operation".into(),
                error_control: StatisticalErrorControl::try_alpha(0.01).expect("valid alpha"),
                power_or_effective_sample_size: "n_eff=65537".into(),
                assumptions: vec!["exchangeable within epoch".into(), "bounded loss".into()],
            },
        ),
        (
            "statistical.assumptions",
            statistical_claim(),
            EvidenceClaim::StatisticalClaim {
                population: "fixture population\0A".into(),
                sampling_rule: "every admitted operation".into(),
                error_control: StatisticalErrorControl::try_alpha(0.01).expect("valid alpha"),
                power_or_effective_sample_size: "n_eff=65536".into(),
                assumptions: vec!["exchangeable within epoch".into(), "unbounded loss".into()],
            },
        ),
        (
            "configuration.model_version",
            EvidenceClaim::ConfigurationModelClaim {
                model_version: "v1".into(),
                fitted_inputs: vec!["a".into()],
                sensitivity: "bounded".into(),
                validity_domain: "epoch".into(),
            },
            EvidenceClaim::ConfigurationModelClaim {
                model_version: "v2".into(),
                fitted_inputs: vec!["a".into()],
                sensitivity: "bounded".into(),
                validity_domain: "epoch".into(),
            },
        ),
        (
            "configuration.fitted_inputs",
            EvidenceClaim::ConfigurationModelClaim {
                model_version: "v1".into(),
                fitted_inputs: vec!["a".into()],
                sensitivity: "bounded".into(),
                validity_domain: "epoch".into(),
            },
            EvidenceClaim::ConfigurationModelClaim {
                model_version: "v1".into(),
                fitted_inputs: vec!["a".into(), String::new()],
                sensitivity: "bounded".into(),
                validity_domain: "epoch".into(),
            },
        ),
        (
            "configuration.sensitivity",
            EvidenceClaim::ConfigurationModelClaim {
                model_version: "v1".into(),
                fitted_inputs: vec![],
                sensitivity: "low".into(),
                validity_domain: "epoch".into(),
            },
            EvidenceClaim::ConfigurationModelClaim {
                model_version: "v1".into(),
                fitted_inputs: vec![],
                sensitivity: "high".into(),
                validity_domain: "epoch".into(),
            },
        ),
        (
            "configuration.validity_domain",
            EvidenceClaim::ConfigurationModelClaim {
                model_version: "v1".into(),
                fitted_inputs: vec![],
                sensitivity: "bounded".into(),
                validity_domain: "epoch-a".into(),
            },
            EvidenceClaim::ConfigurationModelClaim {
                model_version: "v1".into(),
                fitted_inputs: vec![],
                sensitivity: "bounded".into(),
                validity_domain: "epoch-b".into(),
            },
        ),
        (
            "empirical.fixture",
            EvidenceClaim::EmpiricalGate {
                fixture: "a".into(),
                machine_profile: "machine".into(),
                sample_count: 0,
                variance_budget: "0".into(),
                comparison_rule: "equal".into(),
            },
            EvidenceClaim::EmpiricalGate {
                fixture: "ab".into(),
                machine_profile: "machine".into(),
                sample_count: 0,
                variance_budget: "0".into(),
                comparison_rule: "equal".into(),
            },
        ),
        (
            "empirical.machine_profile",
            EvidenceClaim::EmpiricalGate {
                fixture: "fixture".into(),
                machine_profile: "a".into(),
                sample_count: 0,
                variance_budget: "0".into(),
                comparison_rule: "equal".into(),
            },
            EvidenceClaim::EmpiricalGate {
                fixture: "fixture".into(),
                machine_profile: "ab".into(),
                sample_count: 0,
                variance_budget: "0".into(),
                comparison_rule: "equal".into(),
            },
        ),
        (
            "empirical.sample_count",
            EvidenceClaim::EmpiricalGate {
                fixture: "f".into(),
                machine_profile: "m".into(),
                sample_count: 0,
                variance_budget: "0".into(),
                comparison_rule: "equal".into(),
            },
            EvidenceClaim::EmpiricalGate {
                fixture: "f".into(),
                machine_profile: "m".into(),
                sample_count: u64::MAX,
                variance_budget: "0".into(),
                comparison_rule: "equal".into(),
            },
        ),
        (
            "empirical.variance_budget",
            EvidenceClaim::EmpiricalGate {
                fixture: "f".into(),
                machine_profile: "m".into(),
                sample_count: 1,
                variance_budget: "1%".into(),
                comparison_rule: "equal".into(),
            },
            EvidenceClaim::EmpiricalGate {
                fixture: "f".into(),
                machine_profile: "m".into(),
                sample_count: 1,
                variance_budget: "2%".into(),
                comparison_rule: "equal".into(),
            },
        ),
        (
            "empirical.comparison_rule",
            EvidenceClaim::EmpiricalGate {
                fixture: "f".into(),
                machine_profile: "m".into(),
                sample_count: 1,
                variance_budget: "1%".into(),
                comparison_rule: "equal".into(),
            },
            EvidenceClaim::EmpiricalGate {
                fixture: "f".into(),
                machine_profile: "m".into(),
                sample_count: 1,
                variance_budget: "1%".into(),
                comparison_rule: "not slower".into(),
            },
        ),
    ]
}

#[test]
fn envelope_address_changes_for_every_bound_component() {
    assert_eq!(EVIDENCE_ENVELOPE_BINDING_VERSION, 2);
    let base = baseline_parts();

    for (label, left_claim, right_claim) in claim_field_pairs() {
        let mut left = base.clone();
        left.claim = left_claim;
        let mut right = base.clone();
        right.claim = right_claim;
        assert_binding_changed(label, &left.build(), &right.build());
    }

    let mut changes: Vec<(&str, EnvelopeParts)> = Vec::new();
    let mut changed = base.clone();
    changed.evidence_oid = oid(0x12);
    changes.push(("evidence_oid", changed));
    let mut changed = base.clone();
    changed.selection_policy_oid = oid(0x23);
    changes.push(("selection_policy_oid", changed));
    let mut changed = base.clone();
    changed.strata_identity = StrataIdentity::NotApplicable;
    changes.push(("strata_identity.variant", changed));
    let mut changed = base.clone();
    changed.strata_identity = StrataIdentity::Bound(oid(0x45));
    changes.push(("strata_identity.oid", changed));
    let mut changed = base.clone();
    changed.propensity_support_identity = PropensitySupportIdentity::NotApplicable;
    changes.push(("propensity_support_identity.variant", changed));
    let mut changed = base.clone();
    changed.propensity_support_identity = PropensitySupportIdentity::Bound(oid(0x56));
    changes.push(("propensity_support_identity.oid", changed));
    let mut changed = base.clone();
    changed.calibration_window = None;
    changes.push(("calibration_window.presence", changed));
    let mut changed = base.clone();
    changed.calibration_window = Some(CalibrationWindow::new(1, u64::MAX).expect("valid"));
    changes.push(("calibration_window.start_seq", changed));
    let mut changed = base.clone();
    changed.calibration_window = Some(CalibrationWindow::new(0, u64::MAX - 1).expect("valid"));
    changes.push(("calibration_window.end_seq", changed));
    let mut changed = base.clone();
    changed.regime_epoch = 0;
    changes.push(("regime_epoch", changed));
    let mut changed = base.clone();
    changed.fallback = FallbackBehavior::FailClosed;
    changes.push(("fallback.variant", changed));
    let mut changed = base.clone();
    changed.fallback = FallbackBehavior::DeterministicPolicy {
        policy_oid: oid(0x34),
    };
    changes.push(("fallback.policy_oid", changed));

    let baseline = base.build();
    for (label, changed) in changes {
        assert_binding_changed(label, &baseline, &changed.build());
    }
}

fn allocated_string(value: &str, spare: usize) -> String {
    let mut result = String::with_capacity(value.len() + spare);
    result.push_str(value);
    result
}

fn differently_allocated_claim(spare: usize) -> EvidenceClaim {
    let mut assumptions = Vec::with_capacity(2 + spare);
    assumptions.push(allocated_string("exchangeable", spare));
    assumptions.push(allocated_string("bounded\0loss", spare * 2));
    EvidenceClaim::StatisticalClaim {
        population: allocated_string("population", spare),
        sampling_rule: allocated_string("sampling", spare * 3),
        error_control: StatisticalErrorControl::try_alpha(0.125).expect("valid alpha"),
        power_or_effective_sample_size: allocated_string("n_eff=256", spare * 4),
        assumptions,
    }
}

#[test]
fn identical_content_has_one_address_independent_of_allocation_shape() {
    let left = EvidenceEnvelope::new(
        differently_allocated_claim(0),
        oid(1),
        oid(2),
        StrataIdentity::Bound(oid(4)),
        PropensitySupportIdentity::Bound(oid(5)),
        Some(CalibrationWindow::new(63, 65).expect("valid")),
        64,
        FallbackBehavior::DeterministicPolicy { policy_oid: oid(3) },
    );
    let right = EvidenceEnvelope::new(
        differently_allocated_claim(257),
        oid(1),
        oid(2),
        StrataIdentity::Bound(oid(4)),
        PropensitySupportIdentity::Bound(oid(5)),
        Some(CalibrationWindow::new(63, 65).expect("valid")),
        64,
        FallbackBehavior::DeterministicPolicy { policy_oid: oid(3) },
    );

    assert_eq!(left, right, "fixtures must have identical logical content");
    assert_eq!(
        left.to_canonical_binding_bytes(),
        right.to_canonical_binding_bytes(),
        "allocation capacity is not canonical content"
    );
    assert_eq!(address(&left), address(&right));
    assert_eq!(
        address(&left),
        address(&left),
        "addressing is deterministic"
    );
}

/// Dependency-free deterministic generator. Fixed seed, never clocked.
struct Sweep(u64);

impl Sweep {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }
}

fn generated_ascii(sweep: &mut Sweep, len: usize) -> String {
    let mut result = String::with_capacity(len);
    while result.len() < len {
        for byte in sweep.next().to_le_bytes() {
            if result.len() == len {
                break;
            }
            result.push(char::from(b'a' + byte % 26));
        }
    }
    result
}

fn generated_oid(sweep: &mut Sweep) -> ObjectId {
    let mut bytes = [0_u8; 32];
    for lane in 0..4 {
        let start = lane * 8;
        bytes[start..start + 8].copy_from_slice(&sweep.next().to_le_bytes());
    }
    ObjectId(bytes)
}

#[test]
fn seeded_boundary_corpus_is_stable_and_address_distinct() {
    const LENGTHS: [usize; 8] = [0, 1, 31, 32, 63, 64, 255, 256];
    const EPOCHS: [u64; 8] = [0, 1, 63, 64, 65, u32::MAX as u64, u64::MAX - 1, u64::MAX];

    let mut sweep = Sweep(0xE71D_EACE_B17D_0001);
    let mut addresses = BTreeSet::new();
    for index in 0..64 {
        let text = generated_ascii(&mut sweep, LENGTHS[index % LENGTHS.len()]);
        let claim = EvidenceClaim::EmpiricalGate {
            fixture: text,
            machine_profile: format!("machine\0{index}"),
            sample_count: EPOCHS[index % EPOCHS.len()],
            variance_budget: format!("{}ppm", sweep.next()),
            comparison_rule: "candidate <= analytic fallback".into(),
        };
        let envelope = EvidenceEnvelope::new(
            claim,
            // Intentionally fixed: a wrong subset kernel that hashes only the
            // evidence body OID aliases this entire corpus.
            oid(0xA5),
            generated_oid(&mut sweep),
            if index % 3 == 0 {
                StrataIdentity::NotApplicable
            } else {
                StrataIdentity::Bound(generated_oid(&mut sweep))
            },
            if index % 5 == 0 {
                PropensitySupportIdentity::NotApplicable
            } else {
                PropensitySupportIdentity::Bound(generated_oid(&mut sweep))
            },
            Some(
                CalibrationWindow::new(0, EPOCHS[index % EPOCHS.len()].max(1))
                    .expect("end is positive"),
            ),
            EPOCHS[(index + 1) % EPOCHS.len()],
            if index % 2 == 0 {
                FallbackBehavior::FailClosed
            } else {
                FallbackBehavior::DeterministicPolicy {
                    policy_oid: generated_oid(&mut sweep),
                }
            },
        );

        let first = address(&envelope);
        let second = address(&envelope);
        assert_eq!(first, second, "index={index}: address changed on replay");
        assert!(
            addresses.insert(first),
            "index={index}: distinct bound content aliased an earlier address"
        );
    }
}

fn every_claim_kind() -> Vec<EvidenceClaim> {
    vec![
        EvidenceClaim::SafetyInvariant {
            invariant_id: "FG-INV-01".into(),
        },
        EvidenceClaim::FormalModelClaim {
            model_name: "model".into(),
            abstraction_boundary: "boundary".into(),
            checked_bounds: None,
            refinement_status: RefinementStatus::ModelOnly,
        },
        statistical_claim(),
        EvidenceClaim::ConfigurationModelClaim {
            model_version: "v1".into(),
            fitted_inputs: vec![],
            sensitivity: "bounded".into(),
            validity_domain: "epoch".into(),
        },
        EvidenceClaim::EmpiricalGate {
            fixture: "fixture".into(),
            machine_profile: "machine".into(),
            sample_count: 1,
            variance_budget: "zero".into(),
            comparison_rule: "equal".into(),
        },
    ]
}

#[test]
fn mandatory_fallback_is_preserved_for_every_claim_kind() {
    let fallbacks = [
        FallbackBehavior::FailClosed,
        FallbackBehavior::DeterministicPolicy {
            policy_oid: oid(0xD0),
        },
    ];
    for claim in every_claim_kind() {
        for fallback in fallbacks {
            let envelope = EvidenceEnvelope::new(
                claim.clone(),
                oid(1),
                oid(2),
                StrataIdentity::NotApplicable,
                PropensitySupportIdentity::NotApplicable,
                None,
                0,
                fallback,
            );
            assert_eq!(
                envelope.fallback(),
                fallback,
                "the required fallback changed at construction"
            );
        }
    }
}

#[test]
fn analytic_fallback_does_not_alias_the_advisory_selection() {
    let advisory_selection_oid = oid(0xA0);
    let analytic_fallback_oid = oid(0xF0);
    let with_advisory_evidence = EvidenceEnvelope::new(
        statistical_claim(),
        oid(1),
        advisory_selection_oid,
        StrataIdentity::NotApplicable,
        PropensitySupportIdentity::NotApplicable,
        None,
        1,
        FallbackBehavior::DeterministicPolicy {
            policy_oid: analytic_fallback_oid,
        },
    );
    assert_eq!(
        with_advisory_evidence.fallback(),
        FallbackBehavior::DeterministicPolicy {
            policy_oid: analytic_fallback_oid,
        }
    );
    assert_ne!(
        with_advisory_evidence.fallback(),
        FallbackBehavior::DeterministicPolicy {
            policy_oid: advisory_selection_oid,
        },
        "advisory selection must not replace the analytic fallback"
    );

    let without_statistical_advisory = EvidenceEnvelope::new(
        EvidenceClaim::SafetyInvariant {
            invariant_id: "FG-INV-01".into(),
        },
        oid(1),
        advisory_selection_oid,
        StrataIdentity::NotApplicable,
        PropensitySupportIdentity::NotApplicable,
        None,
        1,
        FallbackBehavior::DeterministicPolicy {
            policy_oid: analytic_fallback_oid,
        },
    );
    assert_eq!(
        without_statistical_advisory.fallback(),
        with_advisory_evidence.fallback(),
        "the deterministic fallback must remain available without an estimator"
    );
}

#[test]
fn replay_class_canonical_form_reaches_every_distinct_variant() {
    const EXPECTED: [&str; 36] = [
        "bound_query",
        "compilation_target",
        "cpu_feature_contract",
        "crypto_entropy",
        "decision_cards",
        "derived_generation_snapshots",
        "differential_privacy_seed",
        "evidence",
        "executable_binary",
        "execution_schedule",
        "execution_seed",
        "kernel_profile_registry",
        "key_material",
        "language_profile",
        "logical_state",
        "mediated_external_inputs",
        "mediated_nondeterminism",
        "normalized_query",
        "numeric_profile",
        "platform_abi",
        "policies",
        "replay_authority_snapshot",
        "reproducible_build_closure",
        "role_bearing_binding_set",
        "runtime_allocator_configuration",
        "rust_toolchain",
        "scalar_profile",
        "semantic_profile",
        "source_tree",
        "structural_control_flow",
        "structural_data_shape",
        "structural_replay_projection",
        "typed_parameters",
        "udf_module_set",
        "vm_jit_compiler_artifacts",
        "wall_clock",
    ];

    assert_eq!(REPLAY_CLASS_VOCABULARY_VERSION, 1);
    assert_eq!(ReplayClass::ALL.len(), EXPECTED.len());
    let mut names = BTreeSet::new();
    let mut variants = BTreeSet::new();
    let mut order: Vec<usize> = (0..ReplayClass::ALL.len()).collect();
    let mut sweep = Sweep(0x5EED_C1A5_5000_0001);
    for index in (1..order.len()).rev() {
        let upper = u64::try_from(index + 1).expect("vocabulary length fits in u64");
        let swap = usize::try_from(sweep.next() % upper).expect("index fits in usize");
        order.swap(index, swap);
    }

    for index in order {
        let class = ReplayClass::ALL[index];
        let canonical = class.as_str();
        assert_eq!(canonical, EXPECTED[index]);
        assert_eq!(class.to_string(), canonical);
        assert_eq!(
            ReplayClass::from_canonical_str(canonical),
            Some(class),
            "canonical round trip merged or lost {class:?}"
        );
        assert!(
            names.insert(canonical),
            "canonical spelling {canonical:?} is aliased"
        );
        assert!(variants.insert(class), "variant {class:?} is duplicated");
        assert_eq!(
            class as usize, index,
            "stable discriminant does not match canonical order"
        );
    }
    assert_eq!(names.len(), EXPECTED.len());
    assert_eq!(variants.len(), EXPECTED.len());
}
