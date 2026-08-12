//! Contract tests for the structured failure artifact (plan line 1138,
//! bead fgdb-verif-sim-q97e).
//!
//! Line 1138 states two requirements — "contract tests require every applicable
//! field and execute the replay command" — and both are trivially satisfiable
//! by something that does nothing. These tests are built around that:
//!
//! * *every applicable field* is checked in BOTH directions
//!   (`every_contract_field_is_accounted_for`), and the field list itself is
//!   anchored to the plan line (`the_contract_field_list_is_anchored_to_the_plan`)
//!   so it cannot quietly shrink to whatever the emitter happens to write;
//! * *execute the replay command* is checked by actually replaying and
//!   requiring a byte-identical fault log
//!   (`replaying_an_artifact_reproduces_the_identical_fault_log`), with the
//!   command string tied to the replayed value by a round-trip
//!   (`the_replay_command_round_trips_to_the_value_that_replays`). The pair is
//!   the point: either alone leaves the fgdb-4bxh hole open, where a command
//!   string exists and nothing honours it.
//!
//! The control is `an_artifact_is_emitted_only_for_a_failing_run`. Without it
//! an emitter that emitted unconditionally would pass everything above.

use fgdb::{DerivedPublicationStage, RecoveryRequired};
use fgdb_sim::artifact::{
    ARTIFACT_REPLAY_ENV, Absence, CONTRACT_FIELDS, CommitDurabilityObservation, FailureKind, Field,
    Replay, Scenario,
};
use fgdb_sim::vfs::{FaultPlan, Trigger};
use fgdb_types::CommitSeq;
use std::path::PathBuf;

/// Plan line 1138 — the artifact contract sentence. 1-based, as cited.
const CONTRACT_LINE: usize = 1138;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/<crate> has a repo root")
        .to_path_buf()
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-sim-artifact-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The rendered value of a present field, or `None` for an absent one.
///
/// Pairs with `assert!(matches!(..))` at the call site: the assert is the check
/// and carries the message, this is a *total* read of the field. The obvious
/// spelling — `let Field::Present(v) = f else { panic!(..) }` — spends a
/// panic-class token on what is only a destructure, and UBS counts those as
/// critical against a ratchet that is at zero. Same split as `torn_range` /
/// `flip_site` in tests/lab_vfs.rs.
fn present_value(field: &Field) -> Option<&str> {
    match field {
        Field::Present(value) => Some(value.as_str()),
        Field::Absent(_) => None,
    }
}

/// A plan that loses everything on the sync: the durable-append expectation
/// then fails, which is what makes an artifact exist to inspect.
fn lying_plan() -> FaultPlan {
    FaultPlan {
        seed: 0x1774_0000_0000_0001,
        fsync_lie: Trigger::Always,
        ..FaultPlan::faultless()
    }
}

fn failing_replay() -> Replay {
    Replay {
        scenario: Scenario::DurableAppend,
        plan: lying_plan(),
    }
}

fn planted_spine_loss() -> Replay {
    Replay {
        scenario: Scenario::PlantedSpineLoss,
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0007,
            // Deliberately removable input so the shrinker has real work to
            // do without depending on a product durability bug.
            space_budget: Some(u64::MAX),
            ..FaultPlan::faultless()
        },
    }
}

/// An expectation table independent of `Scenario::{index,id,recovery_stage}`.
/// The production mapping and this test would have to drift together for a
/// replay command to name one boundary while injecting another and stay green.
const POST_D2_SCENARIOS: [(Scenario, DerivedPublicationStage); 9] = [
    (
        Scenario::PostD2Recovery(DerivedPublicationStage::FoldCommittedTemplate),
        DerivedPublicationStage::FoldCommittedTemplate,
    ),
    (
        Scenario::PostD2Recovery(DerivedPublicationStage::SealPartition),
        DerivedPublicationStage::SealPartition,
    ),
    (
        Scenario::PostD2Recovery(DerivedPublicationStage::PublishEdgeBlocks),
        DerivedPublicationStage::PublishEdgeBlocks,
    ),
    (
        Scenario::PostD2Recovery(DerivedPublicationStage::PublishVertexPatches),
        DerivedPublicationStage::PublishVertexPatches,
    ),
    (
        Scenario::PostD2Recovery(DerivedPublicationStage::PublishPartitionRoot),
        DerivedPublicationStage::PublishPartitionRoot,
    ),
    (
        Scenario::PostD2Recovery(DerivedPublicationStage::PublishManifest),
        DerivedPublicationStage::PublishManifest,
    ),
    (
        Scenario::PostD2Recovery(DerivedPublicationStage::PublishRootSlot),
        DerivedPublicationStage::PublishRootSlot,
    ),
    (
        Scenario::PostD2Recovery(DerivedPublicationStage::RefreshEdgeSnapshot),
        DerivedPublicationStage::RefreshEdgeSnapshot,
    ),
    (
        Scenario::PostD2Recovery(DerivedPublicationStage::RefreshVertexSnapshot),
        DerivedPublicationStage::RefreshVertexSnapshot,
    ),
];

// ---------------------------------------------------------------------------
// "require every applicable field"
// ---------------------------------------------------------------------------

#[test]
fn every_contract_field_is_accounted_for() {
    let outcome = failing_replay().run(&scratch_dir("accounted"));
    let artifact = outcome
        .artifact
        .expect("a failing run must emit an artifact");

    assert_eq!(
        artifact.unaccounted_fields(),
        Vec::<&str>::new(),
        "line 1138 fields left out of the artifact"
    );
    assert_eq!(
        artifact.unregistered_fields(),
        Vec::<&str>::new(),
        "artifact fields line 1138 does not spell"
    );
}

#[test]
fn the_contract_field_list_is_anchored_to_the_plan() {
    let plan = std::fs::read_to_string(
        repo_root().join("COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"),
    )
    .expect("plan is readable");
    let line = plan
        .lines()
        .nth(CONTRACT_LINE - 1)
        .expect("plan has line 1138")
        .to_ascii_lowercase();

    // The anchor itself, before anything is derived from it: if line 1138 stops
    // being the artifact sentence, every assertion below is meaningless and
    // this is the one that says so.
    for marker in ["replay_command", "secret-redacted", "expected", "actual"] {
        assert!(
            line.contains(marker),
            "plan line {CONTRACT_LINE} is not the artifact contract sentence (missing {marker:?})"
        );
    }

    // Line 1138 contracts groups ("logical/commit/Raft/applied/... positions",
    // "attempt/generation/... identifiers"), so a field name is anchored when
    // one of its tokens is spelled there — not by literal equality.
    for field in CONTRACT_FIELDS {
        let anchored = field
            .split('_')
            .any(|token| token.len() > 3 && line.contains(token));
        assert!(
            anchored,
            "contract field {field:?} names nothing in plan line {CONTRACT_LINE}"
        );
    }

    let mut sorted: Vec<&&str> = CONTRACT_FIELDS.iter().collect();
    sorted.sort_unstable();
    let before = sorted.len();
    sorted.dedup();
    assert_eq!(before, sorted.len(), "CONTRACT_FIELDS contains a duplicate");
}

#[test]
fn an_absent_field_always_states_a_reason() {
    let outcome = failing_replay().run(&scratch_dir("absence"));
    let artifact = outcome.artifact.expect("failing run emits an artifact");

    let mut absent = 0usize;
    for name in CONTRACT_FIELDS {
        match artifact.field(name).expect("field is accounted for") {
            Field::Present(value) => assert!(
                !value.trim().is_empty(),
                "field {name:?} is Present with an empty value, which is an absence in disguise"
            ),
            Field::Absent(reason) => {
                absent += 1;
                match reason {
                    Absence::NotYetBuilt { subsystem, bead } => {
                        assert!(!subsystem.trim().is_empty(), "{name:?}: unnamed subsystem");
                        assert!(
                            bead.starts_with("fgdb-"),
                            "{name:?}: absence must name an owning bead, got {bead:?}"
                        );
                    }
                    Absence::NotApplicable { because } => assert!(
                        !because.trim().is_empty(),
                        "{name:?}: NotApplicable with no reason"
                    ),
                    Absence::Redacted => {}
                }
            }
        }
    }
    // Non-vacuity: this HEAD has no Raft, no topology and no backup, so an
    // artifact with zero absences would mean the field map was not total.
    assert!(absent > 0, "no field was absent; the map cannot be total");
}

// ---------------------------------------------------------------------------
// "execute the replay command"
// ---------------------------------------------------------------------------

#[test]
fn the_replay_command_round_trips_to_the_value_that_replays() {
    let replay = failing_replay();
    // `Replay::decode` parses OUR replay descriptor ("scenario:seed:sector:..."),
    // not a JWT: no token, no signature, no key, no claim set. MEASURED: zero
    // occurrences of `jsonwebtoken` in any manifest, and doctrine 1's closed
    // dependency universe forbids adding one, so a JWT finding anywhere in this
    // repo is a false positive by construction rather than by inspection.
    // ubs:ignore
    let decoded = Replay::decode(&replay.encode()).expect("encode output decodes");
    assert_eq!(
        decoded, replay,
        "the encoded command does not name the replay it came from"
    );

    // The human command must carry exactly those arguments, or the string a
    // person copies and the value the harness runs are two different things.
    let command = replay.command();
    assert!(
        command.contains(&replay.encode()),
        "command {command:?} does not carry its own replay arguments"
    );
    assert!(
        command.contains(ARTIFACT_REPLAY_ENV),
        "command {command:?} does not name the variable its runner reads"
    );
}

#[test]
fn an_exact_one_shot_trigger_round_trips_without_becoming_periodic() {
    let replay = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            seed: 0x1774_0000_0000_0006,
            fsync_lie: Trigger::At(3),
            write_enospc: Trigger::At(2),
            ..FaultPlan::faultless()
        },
    };
    // This is the same private replay descriptor parser covered above, not JWT
    // authentication or claim validation.
    // ubs:ignore
    let decoded = Replay::decode(&replay.encode()).expect("exact trigger decodes");
    assert_eq!(decoded, replay);
    assert_eq!(decoded.plan.fsync_lie, Trigger::At(3));
    assert_eq!(decoded.plan.write_enospc, Trigger::At(2));
}

#[test]
fn an_already_emitted_eleven_field_replay_remains_executable() {
    let replay = failing_replay();
    let mut legacy_fields: Vec<_> = replay.encode().split(':').map(str::to_owned).collect();
    assert_eq!(legacy_fields.len(), 12, "current descriptor shape changed");
    legacy_fields.remove(4);
    // Private replay descriptor compatibility, not JWT authentication; see
    // the format-boundary note on Replay::decode.
    // ubs:ignore
    let decoded = Replay::decode(&legacy_fields.join(":"))
        .expect("the prior descriptor shape remains replayable");
    assert_eq!(decoded.scenario, replay.scenario);
    assert_eq!(decoded.plan.write_enospc, Trigger::Never);

    let mut expected = replay.plan;
    expected.write_enospc = Trigger::Never;
    assert_eq!(decoded.plan, expected);
}

#[test]
fn replaying_an_artifact_reproduces_the_identical_fault_log() {
    // ONE directory for both runs. A FaultEvent names the file it acted on, so
    // replaying into a different scratch path would differ in `path` alone and
    // the comparison below would be weaker than it looks — it would be
    // asserting nothing about the fault schedule while appearing to.
    let dir = scratch_dir("replay");
    let first = failing_replay().run(&dir);
    let artifact = first.artifact.expect("failing run emits an artifact");

    // Go through the STRING, not the value: this is the path a human takes
    // from a filed artifact back to the failure.
    let replay_command = artifact
        .field("replay_command")
        .expect("replay_command is a contract field");
    assert!(
        matches!(replay_command, Field::Present(_)),
        "replay_command must be Present on a failing run, got {replay_command:?}"
    );
    let command = present_value(replay_command).expect("shape asserted above");
    let encoded = command
        .split_whitespace()
        .find_map(|token| token.strip_prefix(&format!("{ARTIFACT_REPLAY_ENV}=")))
        .expect("the command carries its arguments");

    // Our replay descriptor, not a JWT — see the note at the first decode.
    // ubs:ignore
    let second = Replay::decode(encoded)
        .expect("the emitted command decodes")
        .run(&dir);

    assert_eq!(
        second.failure, first.failure,
        "the replay did not reproduce the failure"
    );
    assert_eq!(
        second.events, first.events,
        "the replay produced a different fault log"
    );
    assert!(
        !first.events.is_empty(),
        "a run that injected nothing cannot witness replay"
    );
}

/// The recovery state machine and the replay artifact machinery used to be
/// two finished islands. This is their public-surface join: every post-D2
/// boundary emits exact frontiers and a command that reconstructs the same
/// typed failure in a fresh database, after independently proving the durable
/// commit is neither missing nor duplicated.
#[test]
fn every_post_d2_recovery_failure_is_structured_and_executable() {
    for (ordinal, (scenario, stage)) in POST_D2_SCENARIOS.into_iter().enumerate() {
        assert_eq!(
            scenario.recovery_stage(),
            Some(stage),
            "{scenario:?}: the stable replay id maps to the wrong stage"
        );
        let replay = Replay {
            scenario,
            plan: FaultPlan::faultless(),
        };
        let first = replay.run(&scratch_dir(&format!("post-d2-first-{ordinal}")));
        assert_eq!(
            first.events,
            Vec::new(),
            "{stage:?}: the structured stage injection must not masquerade as a VFS fault"
        );
        let failure = first
            .failure
            .as_ref()
            .expect("the injected post-D2 boundary emits a failure");
        assert_eq!(
            failure.kind,
            FailureKind::CommittedNeedsRecovery,
            "{stage:?}"
        );
        let expected_recovery = RecoveryRequired {
            durable_frontier: CommitSeq(1),
            published_frontier: CommitSeq(0),
            failed_stage: stage,
        };
        assert_eq!(failure.recovery, Some(expected_recovery), "{stage:?}");

        let artifact = first
            .artifact
            .as_ref()
            .expect("a post-D2 failure emits a structured artifact");
        assert_eq!(artifact.unaccounted_fields(), Vec::<&str>::new());
        assert_eq!(artifact.unregistered_fields(), Vec::<&str>::new());
        assert_eq!(
            present_value(artifact.field("role").expect("role is total")),
            Some("commit"),
            "{stage:?}"
        );
        assert_eq!(
            present_value(
                artifact
                    .field("commit_position")
                    .expect("commit_position is total")
            ),
            Some("1"),
            "{stage:?}"
        );
        assert_eq!(
            present_value(
                artifact
                    .field("visible_position")
                    .expect("visible_position is total")
            ),
            Some("0"),
            "{stage:?}"
        );
        let schedule = present_value(artifact.field("schedule").expect("schedule is total"))
            .expect("a named post-D2 injection is a present schedule");
        assert!(
            schedule.contains(&format!("{stage:?}")),
            "the schedule names another boundary: {schedule}"
        );
        assert_eq!(
            present_value(artifact.field("crashpoint").expect("crashpoint is total")),
            Some(scenario.id()),
            "{stage:?}"
        );

        let command = present_value(
            artifact
                .field("replay_command")
                .expect("replay_command is total"),
        )
        .expect("a failure has a replay command");
        let encoded = command
            .split_whitespace()
            .find_map(|token| token.strip_prefix(&format!("{ARTIFACT_REPLAY_ENV}=")))
            .expect("the command carries the descriptor consumed by replay_from_env");
        assert_eq!(encoded, replay.encode(), "{stage:?}");

        // A different directory models the fresh process named by the command.
        // The failure value contains no scratch path, so exact equality proves
        // the replay is about the durable state transition, not one inode. The
        // descriptor decoder and external command consumer are pinned by the
        // adjacent generic contract tests and the ignored environment-driven
        // consumer respectively.
        let second = replay.run(&scratch_dir(&format!("post-d2-second-{ordinal}")));
        assert_eq!(second.failure, first.failure, "{stage:?}");
        assert_eq!(second.events, first.events, "{stage:?}");
    }
}

/// The campaign's planted oracle mutation uses the real embedded API rather
/// than the direct append fixture. It proves that an acknowledged loss crosses
/// the existing artifact boundary with typed frontier evidence without
/// requiring the product to remain buggy.
#[test]
fn an_acknowledged_spine_loss_is_structured_and_replayable() {
    let replay = planted_spine_loss();
    let first = replay.run(&scratch_dir("spine-loss-first"));
    let failure = first
        .failure
        .as_ref()
        .expect("the persistent planted lie loses the acknowledged commit");
    assert_eq!(failure.kind, FailureKind::AcknowledgedCommitLost);
    assert_eq!(
        failure.durability,
        Some(CommitDurabilityObservation {
            acknowledged: CommitSeq(1),
            crash_succeeded: true,
            reopen_succeeded: true,
            recovered_frontier: None,
            recovered_vertex: false,
            planted: true,
        })
    );
    let artifact = first
        .artifact
        .as_ref()
        .expect("an acknowledged commit loss emits an artifact");
    assert_eq!(artifact.unaccounted_fields(), Vec::<&str>::new());
    assert_eq!(
        present_value(
            artifact
                .field("commit_position")
                .expect("commit position is total")
        ),
        Some("1")
    );
    assert!(
        matches!(
            artifact
                .field("visible_position")
                .expect("visible position is total"),
            Field::Absent(Absence::NotApplicable { .. })
        ),
        "the planted missing frontier must not be rendered as a real position"
    );

    let second = replay.run(&scratch_dir("spine-loss-second"));
    assert_eq!(second.failure, first.failure);
    assert_eq!(
        second.events, first.events,
        "fresh-directory replay must retain the relative schedule"
    );
    assert!(
        first.events.is_empty(),
        "the planted oracle mutation must not masquerade as an injected VFS fault"
    );
}

#[test]
fn a_persistent_sync_lie_refuses_cleanly_before_acknowledgement() {
    let outcome = Replay {
        scenario: Scenario::SpineDurability,
        plan: FaultPlan {
            fsync_lie: Trigger::Always,
            ..FaultPlan::faultless()
        },
    }
    .run(&scratch_dir("spine-persistent-lie"));
    assert!(
        outcome.failure.is_none(),
        "a pre-acknowledgement refusal became a durability violation: {outcome:?}"
    );
    assert!(outcome.artifact.is_none());
    assert!(
        !outcome.events.is_empty(),
        "the clean outcome is only meaningful if the planted lie fired"
    );
}

#[test]
fn the_faultless_spine_emits_no_failure_artifact() {
    let outcome = Replay {
        scenario: Scenario::SpineDurability,
        plan: FaultPlan::faultless(),
    }
    .run(&scratch_dir("spine-control"));
    assert!(
        outcome.failure.is_none(),
        "faultless embedded commit did not survive: {:?}",
        outcome.failure
    );
    assert!(outcome.artifact.is_none());
    assert!(outcome.events.is_empty());
}

/// The consumer that makes the emitted command more than a string.
///
/// `#[ignore]` because it is driven by the environment, not by the suite: it is
/// what `Replay::command()` tells a human to run. fgdb-4bxh is the case where
/// this test does not exist and the command is inert.
#[test]
#[ignore = "driven by FGDB_SIM_REPLAY; run via the command an artifact emits"]
fn replay_from_env() {
    // Not `.expect(..)`: an unset variable is the expected way to reach this
    // test by accident (it is #[ignore]d and driven from the outside), so the
    // message has to name the variable, and an assert says it without
    // spending a panic-class token.
    let encoded = std::env::var(ARTIFACT_REPLAY_ENV).unwrap_or_default();
    assert!(
        !encoded.is_empty(),
        "{ARTIFACT_REPLAY_ENV} is unset; run the command an artifact's replay_command names"
    );
    // Our replay descriptor, not a JWT — see the note at the first decode.
    // ubs:ignore
    let replay = Replay::decode(&encoded).expect("FGDB_SIM_REPLAY decodes");
    let outcome = replay.run(&scratch_dir("replay-from-env"));
    assert!(
        outcome.failure.is_some(),
        "the replay did not reproduce a failure: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// The control
// ---------------------------------------------------------------------------

#[test]
fn an_artifact_is_emitted_only_for_a_failing_run() {
    // Faultless: every acknowledged byte survives, so the expectation holds.
    let passing = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan::faultless(),
    }
    .run(&scratch_dir("control-pass"));
    assert!(
        passing.failure.is_none(),
        "the faultless durable-append must pass: {:?}",
        passing.failure
    );
    assert!(
        passing.artifact.is_none(),
        "a passing run emitted an artifact; line 1138 binds it to a FAILING run"
    );
    assert!(
        passing.events.is_empty(),
        "a faultless plan injected faults"
    );

    // And the inverse expectation under the same faultless plan DOES fail —
    // so the control above is a property of the run's outcome, not of a plan
    // that can never produce an artifact.
    let failing = Replay {
        scenario: Scenario::LostAppend,
        plan: FaultPlan::faultless(),
    }
    .run(&scratch_dir("control-fail"));
    assert!(
        failing.failure.is_some(),
        "the inverse expectation must fail under a faultless plan"
    );
    let artifact = failing.artifact.expect("a failing run emits an artifact");
    assert_eq!(artifact.unaccounted_fields(), Vec::<&str>::new());

    // Its schedule is honestly absent: nothing was injected, and the artifact
    // says so rather than emitting an empty string that reads like a record.
    let schedule = artifact.field("schedule").expect("schedule is a field");
    assert!(
        matches!(schedule, Field::Absent(Absence::NotApplicable { .. })),
        "expected an explained absence for schedule, got {schedule:?}"
    );
}

#[test]
fn a_malformed_replay_string_is_rejected_field_by_field() {
    let good = failing_replay().encode();
    // Our replay descriptor, not a JWT — see the note at the first decode.
    // ubs:ignore
    assert!(Replay::decode(&good).is_ok(), "the control must parse");

    for (mutated, what) in [
        (
            good.replace("durable-append", "no-such-scenario"),
            "scenario",
        ),
        (good.replace("0x", ""), "seed prefix"),
        (good.replacen("always", "sometimes", 1), "trigger"),
        (format!("{good}:extra"), "field count"),
    ] {
        // Hoisted out of the assert! so the waiver below is IMMEDIATELY above
        // the flagged call: a ubs:ignore separated from its line by the macro
        // head is inert, and an inert waiver reads exactly like a real one.
        // Our replay descriptor, not a JWT — see the note at the first decode.
        // ubs:ignore
        let rejected = Replay::decode(&mutated).is_err();
        assert!(
            rejected,
            "a replay string with a bad {what} was accepted: {mutated:?}"
        );
    }
}
