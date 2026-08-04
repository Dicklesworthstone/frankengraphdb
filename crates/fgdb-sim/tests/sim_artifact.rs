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

use fgdb_sim::artifact::{ARTIFACT_REPLAY_ENV, Absence, CONTRACT_FIELDS, Field, Replay, Scenario};
use fgdb_sim::vfs::{FaultPlan, Trigger};
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
    let Field::Present(command) = artifact
        .field("replay_command")
        .expect("replay_command is a contract field")
    else {
        panic!("replay_command must be Present on a failing run");
    };
    let encoded = command
        .split_whitespace()
        .find_map(|token| token.strip_prefix(&format!("{ARTIFACT_REPLAY_ENV}=")))
        .expect("the command carries its arguments");

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

/// The consumer that makes the emitted command more than a string.
///
/// `#[ignore]` because it is driven by the environment, not by the suite: it is
/// what `Replay::command()` tells a human to run. fgdb-4bxh is the case where
/// this test does not exist and the command is inert.
#[test]
#[ignore = "driven by FGDB_SIM_REPLAY; run via the command an artifact emits"]
fn replay_from_env() {
    let encoded = std::env::var(ARTIFACT_REPLAY_ENV).unwrap_or_else(|_| {
        panic!("{ARTIFACT_REPLAY_ENV} is unset; run the command an artifact's replay_command names")
    });
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
    match artifact.field("schedule").expect("schedule is a field") {
        Field::Absent(Absence::NotApplicable { .. }) => {}
        other => panic!("expected an explained absence for schedule, got {other:?}"),
    }
}

#[test]
fn a_malformed_replay_string_is_rejected_field_by_field() {
    let good = failing_replay().encode();
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
        assert!(
            Replay::decode(&mutated).is_err(),
            "a replay string with a bad {what} was accepted: {mutated:?}"
        );
    }
}
