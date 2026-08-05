//! The fail-closed redaction contract (plan §15.1 line 1136, bead fgdb-verif-sim-q97e).
//!
//! Two laws with different strengths, and the tests keep them apart:
//!
//! * **fail-closed** — a class nobody opted into is withheld. Tested by
//!   `an_unconfigured_class_is_withheld_not_retained`.
//! * **never recorded** — crypto entropy cannot be opted into *at all*. Tested
//!   by `crypto_entropy_cannot_be_retained_by_any_policy`.
//!
//! If the second were merely the first with a different default, the fix for a
//! debugging session would be "retain it", and someone would. So the test
//! asserts the refusal, not the default.
//!
//! The control is `a_retainable_class_can_actually_be_retained`: without it,
//! a `retain` that refused *everything* would satisfy both laws above.

use fgdb_sim::completeness::{Recording, ReplayCompleteness, grade};
use fgdb_sim::redaction::{Disposition, RecordClass, RedactionPolicy};

/// THE LAW. Not a default — a refusal.
#[test]
fn crypto_entropy_cannot_be_retained_by_any_policy() {
    let error = RedactionPolicy::fail_closed()
        .retain(RecordClass::CryptoEntropy)
        .expect_err("crypto entropy must never be retainable");
    assert_eq!(error.class, RecordClass::CryptoEntropy);
    assert!(
        error.to_string().contains("never be recorded"),
        "the refusal must say it is a law, not a setting: {error}"
    );

    // And it stays withheld even after every other class is retained, so it
    // cannot be smuggled in by a maximally permissive policy.
    let mut policy = RedactionPolicy::fail_closed();
    for class in RecordClass::ALL {
        if !class.is_never_recordable() {
            policy = policy.retain(*class).expect("retainable class");
        }
    }
    assert!(
        matches!(
            policy.disposition(RecordClass::CryptoEntropy),
            Disposition::Redacted { .. }
        ),
        "a maximally permissive policy retained crypto entropy"
    );
}

#[test]
fn an_unconfigured_class_is_withheld_not_retained() {
    let policy = RedactionPolicy::fail_closed();
    for class in RecordClass::ALL {
        assert!(
            matches!(policy.disposition(*class), Disposition::Redacted { .. }),
            "{} is retained by a fail-closed policy",
            class.name()
        );
    }
}

#[test]
fn every_redaction_states_a_reason() {
    let policy = RedactionPolicy::fail_closed()
        .retain(RecordClass::FaultInjection)
        .expect("retainable");
    for class in RecordClass::ALL {
        if let Disposition::Redacted { because } = policy.disposition(*class) {
            assert!(
                !because.trim().is_empty(),
                "{} is withheld with no reason; the bundle cannot be audited",
                class.name()
            );
        }
    }
}

/// THE CONTROL. Both laws above would hold for a `retain` that refused
/// everything, which would be a contract nobody could use.
#[test]
fn a_retainable_class_can_actually_be_retained() {
    let policy = RedactionPolicy::fail_closed()
        .retain(RecordClass::FaultInjection)
        .expect("fault injection is recordable");
    assert_eq!(
        policy.disposition(RecordClass::FaultInjection),
        Disposition::Retained
    );
    assert!(
        !policy
            .withheld_classes()
            .contains(&"fault-injection".to_string()),
        "a retained class still appears in the withheld list"
    );
}

#[test]
fn class_names_are_unique() {
    let mut names: Vec<&str> = RecordClass::ALL.iter().map(|class| class.name()).collect();
    let before = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(
        before,
        names.len(),
        "two record classes share a name; a withheld list would be ambiguous"
    );
}

/// THE INTERLOCK. A redacted bundle must be structurally barred from claiming
/// byte identity — the two halves of §15.1's graded replay wired together
/// rather than merely adjacent.
#[test]
fn a_redacted_bundle_cannot_grade_as_replayable() {
    let policy = RedactionPolicy::fail_closed();
    let withheld = policy.withheld_classes();
    assert!(
        !withheld.is_empty(),
        "premise: a fail-closed policy withholds something, or this proves nothing"
    );

    // An otherwise perfect replay: identical (empty) fault logs, same outcome.
    // Only the withholding can downgrade it.
    let recording = Recording {
        events: Vec::new(),
        failure: None,
        withheld_classes: withheld.clone(),
    };
    let replayed = fgdb_sim::artifact::RunOutcome {
        failure: None,
        events: Vec::new(),
        artifact: None,
    };

    let awarded = grade(&recording, &replayed);
    assert!(
        !awarded.claims_byte_identity(),
        "a redacted bundle claimed byte identity: {awarded:?}"
    );
    assert_eq!(
        awarded,
        ReplayCompleteness::AuditOnly {
            missing_or_redacted_classes: withheld,
        },
        "a bundle that reproduced nothing and withheld everything is audit-only"
    );
}
