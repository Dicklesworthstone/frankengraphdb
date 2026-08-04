//! The shrinker's correctness law (plan §15.1, bead fgdb-verif-sim-q97e).
//!
//! §15.1 wants failing runs to "shrink themselves … to a minimal reproducer
//! before filing". The tempting acceptance test is "the shrunk plan is smaller
//! and still fails", and that test passes against a shrinker that has silently
//! minimised one bug into a different one — which is worse than not shrinking,
//! because the filed reproducer now describes something nobody observed.
//!
//! So the suite is built around the one property that catches it:
//! `a_reduction_that_changes_the_failure_kind_is_rejected` constructs a plan
//! whose obvious reduction fails a *different* way and requires the shrinker to
//! refuse it. Without that test the rest would pass against the broken version.

use fgdb_sim::artifact::{FailureKind, Replay, Scenario};
use fgdb_sim::shrink::shrink;
use fgdb_sim::vfs::{DEFAULT_SECTOR_BYTES, FaultPlan, Trigger};
use std::path::PathBuf;

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-sim-shrink-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Three fault classes at full strength, only one of which is needed: the lie
/// alone loses every acknowledged byte.
fn overspecified_replay() -> Replay {
    Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0002,
            fsync_lie: Trigger::Always,
            torn_write: Trigger::Always,
            bit_flip: Trigger::Always,
            ..FaultPlan::faultless()
        },
    }
}

#[test]
fn a_failing_replay_shrinks_to_fewer_accused_fault_classes() {
    let shrunk = shrink(overspecified_replay(), &scratch_dir("classes"))
        .expect("the overspecified replay fails, so it shrinks");

    assert!(
        !shrunk.steps.is_empty(),
        "nothing was reduced from a three-class plan; steps: {:?}",
        shrunk.steps
    );

    // Blame actually went down: count the classes the reproducer still accuses.
    let accused = [
        shrunk.replay.plan.fsync_lie,
        shrunk.replay.plan.torn_write,
        shrunk.replay.plan.bit_flip,
    ]
    .into_iter()
    .filter(|trigger| *trigger != Trigger::Never)
    .count();
    assert!(
        accused < 3,
        "the reproducer still accuses every class: {:?}",
        shrunk.replay.plan
    );
}

#[test]
fn the_shrunk_reproducer_still_fails_the_same_way() {
    let dir = scratch_dir("same-way");
    let original = overspecified_replay()
        .run(&dir)
        .failure
        .expect("the input must fail");
    let shrunk = shrink(overspecified_replay(), &dir).expect("it shrinks");

    assert_eq!(
        shrunk.failure.kind, original.kind,
        "the shrinker changed the failure kind"
    );

    // And the reproducer reproduces when run on its own, which is the only
    // claim a filed bug report actually makes.
    let rerun = shrunk
        .replay
        .run(&dir)
        .failure
        .expect("reproducer reproduces");
    assert_eq!(rerun.kind, original.kind);
}

/// THE TEST THAT CATCHES THE BROKEN SHRINKER.
///
/// The construction rests on the order of `flush_through`: the tear is applied
/// **before** the space budget is charged, so tearing a sector out of the flush
/// makes it *fit*. The scenario writes four sectors (2048 bytes); a tear drops
/// one interior sector, leaving 1536. With the budget set to exactly 1536:
///
/// * **with** the torn write, the flush fits, lands three sectors, and the run
///   fails `AcknowledgedBytesLost` — a lost-write bug;
/// * **without** it, the flush needs 2048, exceeds the budget, and the run
///   fails `SyncRefused` — a disk-full bug.
///
/// So "drop the torn-write class" is a smaller plan that still fails, and a
/// shrinker testing only for redness accepts it and files a disk-full
/// reproducer for a lost-write bug. The kind guard is the only thing that
/// stops it.
///
/// (The first version of this test used the fsync lie instead and asserted the
/// wrong premise: the lie returns before the budget is ever consulted, so that
/// plan could not fail `SyncRefused` at all. The premise assertion below is
/// what caught it, which is the argument for writing premise assertions.)
#[test]
fn a_reduction_that_changes_the_failure_kind_is_rejected() {
    let dir = scratch_dir("kind-guard");
    let budget = 3 * DEFAULT_SECTOR_BYTES; // exactly what survives one tear
    let replay = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0003,
            torn_write: Trigger::Always,
            space_budget: Some(budget),
            ..FaultPlan::faultless()
        },
    };

    // Premise, in both directions — without this the test is vacuous and would
    // pass against a shrinker with no guard at all.
    let original = replay.run(&dir).failure.expect("the input fails");
    assert_eq!(
        original.kind,
        FailureKind::AcknowledgedBytesLost,
        "premise: with the tear the flush fits and loses a sector"
    );
    let without_tear = Replay {
        plan: FaultPlan {
            torn_write: Trigger::Never,
            ..replay.plan
        },
        ..replay
    };
    let other = without_tear
        .run(&dir)
        .failure
        .expect("the reduced plan also fails");
    assert_eq!(
        other.kind,
        FailureKind::SyncRefused,
        "premise: without the tear the flush exceeds the budget. If this stops \
         holding, the reduction no longer changes the kind and this test proves nothing"
    );

    // The law itself: a smaller, still-red candidate that fails DIFFERENTLY
    // must be refused.
    let shrunk = shrink(replay, &dir).expect("it shrinks");
    assert_eq!(
        shrunk.failure.kind,
        FailureKind::AcknowledgedBytesLost,
        "the shrinker minimised a lost-write failure into a disk-full one"
    );
    assert_ne!(
        shrunk.replay.plan.torn_write,
        Trigger::Never,
        "the torn write IS the bug here; dropping it changed which bug is filed"
    );
    assert!(
        shrunk.rejected > 0,
        "no candidate was rejected, so the kind guard never ran"
    );
}

#[test]
fn a_passing_replay_does_not_shrink() {
    let passing = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan::faultless(),
    };
    assert!(
        passing
            .run(&scratch_dir("control-premise"))
            .failure
            .is_none(),
        "premise: the faultless durable append must pass"
    );
    assert!(
        shrink(passing, &scratch_dir("control")).is_none(),
        "a passing run produced a 'minimal reproducer', which would be fabricated"
    );
}

#[test]
fn an_already_minimal_reproducer_reports_no_steps_but_still_tried() {
    let dir = scratch_dir("minimal");
    // A single class, already at its weakest useful strength.
    let minimal = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0004,
            fsync_lie: Trigger::Nth(1),
            ..FaultPlan::faultless()
        },
    };
    let shrunk = shrink(minimal, &dir).expect("it fails, so it shrinks");
    assert!(
        shrunk.steps.is_empty(),
        "an already-minimal plan was reduced further: {:?}",
        shrunk.steps
    );
    // "Nothing was reducible" and "nothing was attempted" must not look alike.
    assert!(
        shrunk.rejected > 0,
        "no candidate was even tried; empty steps would then mean nothing"
    );
    assert_eq!(shrunk.replay.plan, minimal.plan);
}
