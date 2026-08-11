//! `ReplayCompleteness` grading (plan §15.1, bead fgdb-verif-sim-q97e).
//!
//! The plan's sentence is "customer-safe bundles never overclaim byte
//! identity". Transcribing the four-variant enum does not satisfy it — a
//! grader returning `Replayable` unconditionally matches the vocabulary
//! perfectly and is exactly the failure the sentence forbids.
//!
//! So most of these tests assert that `Replayable` is NOT returned, and the
//! one that matters most is the opposite: `a_faithful_replay_can_reach_the_top
//! _grade` proves the grader can reach it at all. Without that control every
//! "must not overclaim" assertion below is vacuous — they would all pass
//! against a grader hard-wired to `AuditOnly`.

use fgdb_sim::artifact::{Replay, Scenario};
use fgdb_sim::completeness::{Recording, ReplayCompleteness, grade};
use fgdb_sim::vfs::{FaultPlan, Trigger};
use std::path::PathBuf;

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-sim-grade-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn lying_replay() -> Replay {
    Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0020,
            fsync_lie: Trigger::Always,
            ..FaultPlan::faultless()
        },
    }
}

/// THE CONTROL. Every other test here asserts a grade is *not* `Replayable`;
/// this one proves the grader can return it, so those assertions mean
/// something.
#[test]
fn a_faithful_replay_can_reach_the_top_grade() {
    let dir = scratch_dir("top-grade");
    let replay = lying_replay();
    let recorded_run = replay.run(&dir);
    assert!(
        !recorded_run.events.is_empty(),
        "premise: the recording must contain faults, or identity is trivial"
    );

    let recording = Recording {
        events: recorded_run.events.clone(),
        failure: recorded_run.failure.clone(),
        withheld_classes: Vec::new(),
    };
    let replayed = replay.run(&dir);

    let awarded = grade(&recording, &replayed);
    assert_eq!(awarded, ReplayCompleteness::Replayable);
    assert!(awarded.claims_byte_identity());
}

#[test]
fn a_diverging_replay_is_downgraded_to_structural() {
    let dir = scratch_dir("structural");
    let recorded_run = lying_replay().run(&dir);
    let recording = Recording {
        events: recorded_run.events.clone(),
        failure: recorded_run.failure.clone(),
        withheld_classes: Vec::new(),
    };

    // A different plan: the tear fires instead of the lie, so the fault log
    // differs from the recording's.
    let other = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0021,
            torn_write: Trigger::Always,
            ..FaultPlan::faultless()
        },
    }
    .run(&dir);
    assert_ne!(
        recorded_run.events, other.events,
        "premise: the two runs must actually differ"
    );

    let awarded = grade(&recording, &other);
    assert!(
        !awarded.claims_byte_identity(),
        "a diverging replay claimed byte identity: {awarded:?}"
    );
    assert!(
        matches!(awarded, ReplayCompleteness::StructuralReplay { .. }),
        "expected StructuralReplay, got {awarded:?}"
    );
    // `if let` with no else rather than a match with a panic arm: the assert
    // above already decided the shape, so the arm would only be spending a
    // panic-class token on a destructure (see tests/lab_vfs.rs).
    if let ReplayCompleteness::StructuralReplay {
        reproduced_classes,
        omitted_classes,
    } = &awarded
    {
        assert!(
            !reproduced_classes.is_empty() || !omitted_classes.is_empty(),
            "a structural grade that names no classes is a label, not a diagnostic"
        );
    }
}

#[test]
fn a_withheld_class_forbids_the_top_grade_even_on_an_identical_replay() {
    let dir = scratch_dir("withheld");
    let replay = lying_replay();
    let recorded_run = replay.run(&dir);

    // Byte-identical replay — but the bundle admits it withheld something.
    // §15.1: crypto entropy is never recorded, so this is the ordinary case
    // for a real bundle, not an exotic one.
    let recording = Recording {
        events: recorded_run.events.clone(),
        failure: recorded_run.failure.clone(),
        withheld_classes: vec!["crypto-entropy".to_string()],
    };
    let replayed = replay.run(&dir);
    assert_eq!(
        recorded_run.events, replayed.events,
        "premise: the replay IS byte-identical, so only the withholding can downgrade it"
    );

    let awarded = grade(&recording, &replayed);
    assert!(
        !awarded.claims_byte_identity(),
        "byte identity was claimed over a withheld class: {awarded:?}"
    );
    assert_eq!(
        awarded,
        ReplayCompleteness::VerifiableIfArtifactsSupplied {
            missing_classes: vec!["crypto-entropy".to_string()],
        }
    );
}

#[test]
fn a_bundle_that_reproduces_nothing_is_audit_only() {
    let dir = scratch_dir("audit-only");
    let recorded_run = lying_replay().run(&dir);
    let recording = Recording {
        events: recorded_run.events,
        failure: recorded_run.failure,
        withheld_classes: vec!["crypto-entropy".to_string(), "fsync-lie".to_string()],
    };

    // A faultless run: nothing was injected, so nothing came back.
    let nothing = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan::faultless(),
    }
    .run(&dir);
    assert!(
        nothing.events.is_empty(),
        "premise: this replay must reproduce no faults at all"
    );

    let awarded = grade(&recording, &nothing);
    assert!(!awarded.claims_byte_identity());
    assert!(
        matches!(awarded, ReplayCompleteness::AuditOnly { .. }),
        "expected AuditOnly, got {awarded:?}"
    );
    if let ReplayCompleteness::AuditOnly {
        missing_or_redacted_classes,
    } = &awarded
    {
        assert_eq!(missing_or_redacted_classes.len(), 2);
        assert!(missing_or_redacted_classes.contains(&"crypto-entropy".to_string()));
    }
}

#[test]
fn exactly_one_grade_claims_byte_identity() {
    // The whole contract in one assertion: a caller deciding what a
    // customer-facing bundle may say must not be able to find a second grade
    // that also claims identity.
    let grades = [
        ReplayCompleteness::Replayable,
        ReplayCompleteness::StructuralReplay {
            reproduced_classes: vec!["fsync-lie".to_string()],
            omitted_classes: vec![],
        },
        ReplayCompleteness::VerifiableIfArtifactsSupplied {
            missing_classes: vec!["crypto-entropy".to_string()],
        },
        ReplayCompleteness::AuditOnly {
            missing_or_redacted_classes: vec!["crypto-entropy".to_string()],
        },
    ];
    let claiming = grades
        .iter()
        .filter(|awarded| awarded.claims_byte_identity())
        .count();
    assert_eq!(claiming, 1, "exactly one grade may claim byte identity");
}

/// Aggregate entrypoint for the governed replay-completeness claim. Each arm
/// is produced by the real grader from an executable fixture; merely listing
/// the enum variants would not prove that the grading policy can reach them.
#[test]
fn all_four_replay_completeness_grades_are_reached_by_executable_fixtures() {
    let dir = scratch_dir("all-four-grades");
    let replay = lying_replay();
    let recorded_run = replay.run(&dir);
    assert!(!recorded_run.events.is_empty());

    let complete = Recording {
        events: recorded_run.events.clone(),
        failure: recorded_run.failure.clone(),
        withheld_classes: Vec::new(),
    };
    let faithful = replay.run(&dir);
    let replayable = grade(&complete, &faithful);

    let divergent = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0022,
            torn_write: Trigger::Always,
            ..FaultPlan::faultless()
        },
    }
    .run(&dir);
    let structural = grade(&complete, &divergent);

    let withheld = Recording {
        events: recorded_run.events,
        failure: recorded_run.failure,
        withheld_classes: vec!["crypto-entropy".to_string()],
    };
    let verifiable = grade(&withheld, &faithful);

    let withheld_everything = Recording {
        events: withheld.events,
        failure: withheld.failure,
        withheld_classes: vec!["crypto-entropy".to_string(), "fsync-lie".to_string()],
    };
    let faultless = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan::faultless(),
    }
    .run(&dir);
    let audit_only = grade(&withheld_everything, &faultless);

    assert_eq!(replayable, ReplayCompleteness::Replayable);
    assert!(matches!(
        structural,
        ReplayCompleteness::StructuralReplay { .. }
    ));
    assert_eq!(
        verifiable,
        ReplayCompleteness::VerifiableIfArtifactsSupplied {
            missing_classes: vec!["crypto-entropy".to_string()],
        }
    );
    assert!(matches!(audit_only, ReplayCompleteness::AuditOnly { .. }));
    assert_eq!(
        [&replayable, &structural, &verifiable, &audit_only]
            .into_iter()
            .filter(|grade| grade.claims_byte_identity())
            .count(),
        1,
        "only the executable Replayable fixture may claim byte identity"
    );
}
