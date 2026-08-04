//! Campaign claim typing (plan §15.1 lines 1128/1140, bead fgdb-verif-sim-q97e).
//!
//! Line 1140 requires reports "structurally incapable of asserting 'verified
//! fault-free'". A test that merely checks today's three variants do not say
//! it would pass forever while someone adds a fourth that does — so the guard
//! here runs over *every* outcome and its rendering, and the interesting case
//! is `bounded_exhaustion_still_does_not_claim_absence`: the one outcome
//! strong enough to be mistaken for a proof.

use fgdb_sim::campaign::{CampaignOutcome, ClaimClass, FORBIDDEN_CLAIMS};

/// Every outcome the type can express. Extended deliberately when a variant is
/// added — the guards below are only as total as this list.
fn every_outcome() -> Vec<CampaignOutcome> {
    vec![
        CampaignOutcome::Falsified {
            replay: "durable-append:0x1:512:always:never:never:none".to_string(),
            failure_kind: "AcknowledgedBytesLost".to_string(),
        },
        CampaignOutcome::NotFalsified {
            sampling_model: "uniform-over-declared-faults".to_string(),
            explored: 10_000,
        },
        CampaignOutcome::BoundedExhausted {
            model: "two-writer-one-crash".to_string(),
            states: 4_096,
        },
    ]
}

#[test]
fn no_outcome_renders_a_forbidden_claim() {
    for outcome in every_outcome() {
        let rendered = outcome.to_string().to_ascii_lowercase();
        for forbidden in FORBIDDEN_CLAIMS {
            assert!(
                !rendered.contains(forbidden),
                "{outcome:?} rendered a forbidden claim {forbidden:?}: {rendered}"
            );
        }
        assert!(
            !rendered.is_empty(),
            "an outcome that renders nothing cannot be audited for what it claims"
        );
    }
}

#[test]
fn no_claim_class_licence_promises_absence() {
    // The licence strings are what a report header quotes, so they are as
    // capable of overclaiming as the outcomes themselves.
    for class in [
        ClaimClass::Falsification,
        ClaimClass::Statistical,
        ClaimClass::BoundedFormal,
    ] {
        let licence = class.licence().to_ascii_lowercase();
        for forbidden in FORBIDDEN_CLAIMS {
            assert!(
                !licence.contains(forbidden),
                "{class:?} licence claims {forbidden:?}: {licence}"
            );
        }
    }
}

/// THE CASE THAT MATTERS. Bounded exhaustion is the outcome most easily read
/// as "we proved it clean" — it really did exhaust its model. Line 1128 says
/// that is still not absence of bugs, because the bound and the independence
/// relation are assumptions the campaign cannot discharge about itself.
#[test]
fn bounded_exhaustion_still_does_not_claim_absence() {
    let outcome = CampaignOutcome::BoundedExhausted {
        model: "two-writer-one-crash".to_string(),
        states: 4_096,
    };
    assert_eq!(outcome.claim_class(), ClaimClass::BoundedFormal);
    assert!(
        !outcome.found_counterexample(),
        "premise: this outcome found nothing, which is exactly why it is temptingly readable as a proof"
    );

    // It must name its bound in the rendering, or a reader sees "exhausted"
    // with nothing qualifying it.
    let rendered = outcome.to_string();
    assert!(
        rendered.contains("two-writer-one-crash"),
        "bounded exhaustion must name the model it exhausted: {rendered}"
    );
    assert!(
        rendered.contains("nothing is claimed"),
        "bounded exhaustion must state the limit of its claim: {rendered}"
    );
}

#[test]
fn finding_nothing_is_not_reported_as_the_same_claim_as_exhausting_a_model() {
    // §15.1: "Deterministic bounded-state completion is reported separately
    // from statistical/heuristic stopping." Same observation — no
    // counterexample — must not collapse into one claim class.
    let sampled = CampaignOutcome::NotFalsified {
        sampling_model: "uniform".to_string(),
        explored: 1,
    };
    let exhausted = CampaignOutcome::BoundedExhausted {
        model: "m".to_string(),
        states: 1,
    };
    assert_eq!(
        sampled.found_counterexample(),
        exhausted.found_counterexample()
    );
    assert_ne!(
        sampled.claim_class(),
        exhausted.claim_class(),
        "two different claims collapsed into one class"
    );
}

#[test]
fn only_falsification_asserts_anything_unconditionally() {
    let counts = every_outcome()
        .iter()
        .filter(|outcome| outcome.claim_class() == ClaimClass::Falsification)
        .count();
    assert_eq!(
        counts, 1,
        "exactly one outcome carries an unconditional claim"
    );

    // And it is the one that found a bug — the asymmetry the module is built
    // around. This is the control: without it, a `found_counterexample` that
    // always returned false would satisfy every other test here.
    for outcome in every_outcome() {
        assert_eq!(
            outcome.found_counterexample(),
            outcome.claim_class() == ClaimClass::Falsification,
            "{outcome:?}: counterexample and claim class disagree"
        );
    }
    assert!(
        every_outcome()
            .iter()
            .any(CampaignOutcome::found_counterexample),
        "no outcome reports a counterexample; every assertion above would then be vacuous"
    );
}
