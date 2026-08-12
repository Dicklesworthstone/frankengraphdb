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
use fgdb_sim::shrink::{ShrinkTrial, diverge, shrink, shrink_schedule_and_workload};
use fgdb_sim::vfs::{DEFAULT_SECTOR_BYTES, FaultPlan, Trigger};
use std::path::PathBuf;

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-sim-shrink-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn schedule_and_workload_shrink_together_to_a_standalone_reproducer() {
    // Failure requires the commit decision and the conflicting write. Every
    // other decision/action is deliberately irrelevant blame.
    let reproduces = |schedule: &[u8], workload: &[u8]| {
        if schedule.contains(&3) && workload.contains(&12) {
            ShrinkTrial::Reproduced
        } else {
            ShrinkTrial::DidNotReproduce
        }
    };
    let shrunk = shrink_schedule_and_workload(
        vec![1u8, 2, 3, 4, 5],
        vec![10u8, 11, 12, 13, 14],
        reproduces,
    )
    .expect("the starting schedule/workload reproduces");
    assert_eq!(shrunk.schedule, vec![3]);
    assert_eq!(shrunk.workload, vec![12]);
    assert!(shrunk.accepted > 0);
    assert!(shrunk.attempts > shrunk.accepted);

    // 1-minimal across both axes: deleting either remaining item makes the
    // typed failure predicate false.
    assert_eq!(
        reproduces(&[], &shrunk.workload),
        ShrinkTrial::DidNotReproduce
    );
    assert_eq!(
        reproduces(&shrunk.schedule, &[]),
        ShrinkTrial::DidNotReproduce
    );
}

#[test]
fn a_nonreproducing_schedule_workload_pair_cannot_be_filed_as_minimal() {
    assert!(
        shrink_schedule_and_workload(vec![1u8], vec![2u8], |schedule, workload| {
            if schedule.contains(&9) && workload.contains(&9) {
                ShrinkTrial::Reproduced
            } else {
                ShrinkTrial::DidNotReproduce
            }
        })
        .is_none()
    );
}

#[test]
fn real_fixture_replays_shrink_fault_schedule_and_workload_without_changing_the_bug() {
    let root = scratch_dir("hierarchical-real-replay");
    let target = FailureKind::AcknowledgedBytesLost;
    let mut ordinal = 0usize;
    let mut replay_candidate = |schedule: &[Trigger], workload: &[Scenario]| {
        let fsync_lie = schedule
            .iter()
            .copied()
            .find(|trigger| *trigger != Trigger::Never)
            .unwrap_or(Trigger::Never);
        for scenario in workload {
            let attempt = root.join(format!("candidate-{ordinal:04}"));
            ordinal += 1;
            std::fs::create_dir_all(&attempt).expect("isolated hierarchical replay directory");
            let outcome = Replay {
                scenario: *scenario,
                plan: FaultPlan {
                    seed: 0x1774_0000_0000_0020,
                    fsync_lie,
                    ..FaultPlan::faultless()
                },
            }
            .run(&attempt);
            if let Some(failure) = outcome.failure {
                if failure.kind == target {
                    assert!(
                        outcome.artifact.is_some(),
                        "a reproduced fixture failure must carry its structured artifact"
                    );
                    return ShrinkTrial::Reproduced;
                }
                return ShrinkTrial::DifferentFailure(failure.kind);
            }
        }
        ShrinkTrial::DidNotReproduce
    };

    // `LostAppend` is a real passing workload under the injected fsync lie,
    // while `DurableAppend` produces the target acknowledged-loss failure.
    // Without the lie, `LostAppend` remains red as UnexpectedSurvival: this is
    // the repair-hostile candidate the typed shrinker must reject.
    let shrunk = shrink_schedule_and_workload(
        vec![Trigger::Never, Trigger::Always, Trigger::Never],
        vec![
            Scenario::LostAppend,
            Scenario::DurableAppend,
            Scenario::LostAppend,
        ],
        &mut replay_candidate,
    )
    .expect("the original real fixture workload reproduces");
    assert_eq!(shrunk.schedule, vec![Trigger::Always]);
    assert_eq!(shrunk.workload, vec![Scenario::DurableAppend]);
    assert!(shrunk.accepted >= 2);
    assert!(
        shrunk.rejected_different_failure > 0,
        "the wrong-failure red path was never exercised"
    );

    let final_dir = root.join("standalone-final");
    std::fs::create_dir_all(&final_dir).expect("standalone final replay directory");
    let final_outcome = Replay {
        scenario: shrunk.workload[0],
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0020,
            fsync_lie: shrunk.schedule[0],
            ..FaultPlan::faultless()
        },
    }
    .run(&final_dir);
    assert_eq!(
        final_outcome.failure.as_ref().map(|failure| failure.kind),
        Some(target)
    );
    assert!(final_outcome.artifact.is_some());
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
        .expect("isolated shrink attempts are created")
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
    assert!(
        [
            shrunk.replay.plan.fsync_lie,
            shrunk.replay.plan.torn_write,
            shrunk.replay.plan.bit_flip,
        ]
        .into_iter()
        .filter(|trigger| *trigger != Trigger::Never)
        .all(|trigger| trigger == Trigger::At(1)),
        "every remaining fault must fire once, not periodically: {:?}",
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
    let shrunk = shrink(overspecified_replay(), &dir)
        .expect("isolated shrink attempts are created")
        .expect("it shrinks");

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
    let shrunk = shrink(replay, &dir)
        .expect("isolated shrink attempts are created")
        .expect("it shrinks");
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
        shrink(passing, &scratch_dir("control"))
            .expect("isolated shrink attempts are created")
            .is_none(),
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
            fsync_lie: Trigger::At(1),
            ..FaultPlan::faultless()
        },
    };
    let shrunk = shrink(minimal, &dir)
        .expect("isolated shrink attempts are created")
        .expect("it fails, so it shrinks");
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

#[test]
fn nth_one_is_periodic_and_therefore_not_a_minimal_reproducer() {
    let dir = scratch_dir("periodic-is-not-once");
    let periodic = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0005,
            fsync_lie: Trigger::Nth(1),
            ..FaultPlan::faultless()
        },
    };
    let shrunk = shrink(periodic, &dir)
        .expect("isolated shrink attempts are created")
        .expect("the periodic lie fails, so it shrinks");
    assert_eq!(shrunk.replay.plan.fsync_lie, Trigger::At(1));
    assert!(
        shrunk
            .steps
            .iter()
            .any(|step| step.what == "weakened the fsync lie to fire once"),
        "the shrinker did not record the behavioral reduction: {:?}",
        shrunk.steps
    );
}

#[test]
fn an_integrated_spine_loss_shrinks_in_isolated_databases() {
    let replay = Replay {
        scenario: Scenario::PlantedSpineLoss,
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0008,
            space_budget: Some(u64::MAX),
            ..FaultPlan::faultless()
        },
    };
    let shrunk = shrink(replay, &scratch_dir("spine-isolated"))
        .expect("isolated database attempts are created")
        .expect("the planted acknowledged loss reproduces");
    assert_eq!(shrunk.failure.kind, FailureKind::AcknowledgedCommitLost);
    assert!(
        shrunk
            .steps
            .iter()
            .any(|step| step.what == "dropped the space budget"),
        "the integrated shrink did not remove the irrelevant budget: {:?}",
        shrunk.steps
    );

    let rerun = shrunk.replay.run(&scratch_dir("spine-isolated-rerun"));
    assert_eq!(
        rerun.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::AcknowledgedCommitLost),
        "the isolated result is not a standalone reproducer: {rerun:?}"
    );
    assert!(rerun.artifact.is_some());
}

// ---------------------------------------------------------------------------
// Divergence diagnostics (§15.1: "explain exactly where a replay departed")
// ---------------------------------------------------------------------------

#[test]
fn a_faithful_replay_reports_no_divergence() {
    let dir = scratch_dir("diverge-control");
    let replay = overspecified_replay();
    let first = replay.run(&dir);
    let second = replay.run(&dir);
    assert!(
        !first.events.is_empty(),
        "a run with no faults cannot witness anything about divergence"
    );
    assert_eq!(
        diverge(&first.events, &second.events),
        None,
        "a deterministic replay of the same value diverged from itself"
    );
}

#[test]
fn divergence_names_the_first_differing_index_and_both_sides() {
    let dir = scratch_dir("diverge-real");
    // Two different seeds under a coin-flip schedule: the fault logs differ,
    // and the diagnostic has to say WHERE rather than merely that they do.
    let plan = |seed| FaultPlan {
        seed,
        fsync_lie: Trigger::PerMille(500),
        torn_write: Trigger::PerMille(500),
        ..FaultPlan::faultless()
    };
    let a = Replay {
        scenario: Scenario::DurableAppend,
        plan: plan(0x1774_0000_0000_0010),
    }
    .run(&dir);
    let b = Replay {
        scenario: Scenario::DurableAppend,
        plan: plan(0x1774_0000_0000_0011),
    }
    .run(&dir);

    match diverge(&a.events, &b.events) {
        None => assert_eq!(
            a.events, b.events,
            "diverge() said the logs match while they differ"
        ),
        Some(divergence) => {
            // Everything before the reported index must actually agree, or the
            // diagnostic is pointing at the wrong place — which is worse than
            // no diagnostic, because it sends a reader to innocent code.
            assert_eq!(
                a.events[..divergence.index],
                b.events[..divergence.index],
                "events before the reported index already differ"
            );
            assert!(
                divergence.recorded.is_some() || divergence.replayed.is_some(),
                "a divergence with neither side present explains nothing"
            );
            assert!(
                !divergence.to_string().is_empty(),
                "the rendered diagnostic is what a reader actually sees"
            );
        }
    }
}

#[test]
fn a_truncated_replay_diverges_at_the_first_missing_event() {
    let dir = scratch_dir("diverge-truncated");
    // NOT overspecified_replay(): the fsync lie returns from flush_through
    // before the tear and the flip can fire, so that plan records exactly ONE
    // event and there is nothing to truncate. Tear + flip both land on the
    // same flush, giving two. (The premise assertion below is what caught
    // this — the same short-circuit that invalidated the first kind-guard.)
    let full = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0012,
            torn_write: Trigger::Always,
            bit_flip: Trigger::Always,
            ..FaultPlan::faultless()
        },
    }
    .run(&dir);
    assert!(
        full.events.len() >= 2,
        "need at least two events to truncate meaningfully, got {:?}",
        full.events
    );
    let truncated = &full.events[..full.events.len() - 1];

    let divergence =
        diverge(&full.events, truncated).expect("a shorter replay must diverge from its recording");
    assert_eq!(divergence.index, full.events.len() - 1);
    assert!(
        divergence.recorded.is_some(),
        "the recording had an event at the missing index"
    );
    assert_eq!(
        divergence.replayed, None,
        "the replay ended there, and the diagnostic must say so"
    );
}
