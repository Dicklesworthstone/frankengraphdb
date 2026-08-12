//! Witnesses for `fgdb-qd2s`: the exported fixture boots under the lab with
//! virtual time, disk, and network; two runs at one seed are byte-identical;
//! and the lab-vs-live dual run compares real executions of the same program.
//!
//! Every positive claim here travels with the control that can refute it: the
//! determinism gate is proven able to FIRE (injected process-global entropy),
//! and the dual run is proven able to DETECT a semantic mutation (live side
//! forced to a different seed). A gate whose red path has never been observed
//! is not a gate (see `a-marker-test-cannot-see-an-append`,
//! `sourcing-bash-gate-functions-into-zsh-fakes-green` — same law, Rust shape).

use std::path::PathBuf;

use fgdb_sim::dual_run::{determinism_gate, dual_run_fixture, run_fixture_under_lab};
use fgdb_sim::fixture::{FixtureConfig, MAX_FIXTURE_PAYLOAD_BYTES};
use fgdb_sim::vfs::Trigger;

use asupersync::lab::LabConfig;

/// Per-test scratch root, pid-suffixed so concurrent panes cannot collide
/// (the `neighbour-test-run` hazard), never deleted (AGENTS.md Rule 1).
fn scratch_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-sim-dual-run-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch root");
    dir
}

// ---------------------------------------------------------------------------
// Item 6: the two-runs-at-one-seed determinism gate
// ---------------------------------------------------------------------------

#[test]
fn two_lab_runs_at_one_seed_are_byte_identical_across_seeds() {
    for &seed in &[0x1u64, 0x5EED, 0xDEAD_BEEF] {
        let cfg = FixtureConfig::new(seed);
        let root = scratch_root(&format!("gate-{seed:x}"));
        let verdict = determinism_gate(&cfg, &root, 2);
        assert!(
            verdict.passed,
            "seed {seed:#x} diverged: {:?}\n{}",
            verdict.first_divergence,
            verdict.log_lines.join("\n")
        );
        assert!(verdict.first_divergence.is_none());
        assert_eq!(verdict.trace_fingerprints[0], verdict.trace_fingerprints[1]);
        assert_eq!(verdict.schedule_hashes[0], verdict.schedule_hashes[1]);
        assert!(
            verdict.log_lines.iter().any(|l| l.contains("PASSED")),
            "verdict must be reconstructable from its log: {:?}",
            verdict.log_lines
        );
    }
}

#[test]
fn the_determinism_gate_fires_on_injected_process_global_entropy() {
    let mut cfg = FixtureConfig::new(0xBAD_5EED);
    cfg.entropy_probe = true;
    let root = scratch_root("gate-control");
    let verdict = determinism_gate(&cfg, &root, 2);
    assert!(
        !verdict.passed,
        "the control must fire: a gate that passes an entropy-injected pair \
         has measured nothing\n{}",
        verdict.log_lines.join("\n")
    );
    let point = verdict
        .first_divergence
        .expect("an entropy divergence must be located, not merely declared");
    assert!(
        point.event_index.is_some(),
        "the divergence must land inside the event region, naming the event"
    );
    assert!(
        verdict
            .log_lines
            .iter()
            .any(|l| l.contains("first diverging trace offset")),
        "the failure must log the first diverging trace offset: {:?}",
        verdict.log_lines
    );
}

// ---------------------------------------------------------------------------
// The fixture itself: virtual time, disk, and network all exercised
// ---------------------------------------------------------------------------

#[test]
fn the_fixture_advances_virtual_time_and_moves_every_record_through_disk_and_network() {
    let cfg = FixtureConfig::new(0xF1D0);
    let root = scratch_root("fixture-legs");
    let run = run_fixture_under_lab(&cfg, &root, LabConfig::new(cfg.seed));

    let expected_sleep_nanos =
        u64::from(cfg.rounds) * u64::try_from(cfg.tick.as_nanos()).expect("tick fits u64");
    assert!(
        run.virtual_elapsed_nanos >= expected_sleep_nanos,
        "virtual clock must have advanced through every pacing sleep: \
         elapsed {} < expected {expected_sleep_nanos}",
        run.virtual_elapsed_nanos
    );

    assert_eq!(run.semantics.produced, i64::from(cfg.rounds));
    assert_eq!(run.semantics.consumed, i64::from(cfg.rounds));
    assert_eq!(
        run.semantics.durable_bytes,
        i64::from(cfg.rounds) * i64::try_from(cfg.payload_bytes).expect("payload fits i64")
    );
    assert!(run.region_close.quiescent);
    assert!(run.region_close.close_completed);
    assert!(run.obligation_balance.balanced);
    assert_eq!(run.obligation_balance.leaked, 0);
    assert!(
        run.semantics.chain_intact,
        "consumer chain must equal producer chain: every payload crossed the \
         virtual network intact"
    );
    assert!(!run.semantics.final_digest_hex.is_empty());
}

#[test]
fn deterministic_latency_and_large_payload_pressure_cross_every_fixture_leg() {
    let mut cfg = FixtureConfig::new(0xC4A0_5EED);
    cfg.rounds = 16;
    cfg.payload_bytes = 65_536;
    cfg.fault_plan.latency = Trigger::Always;
    cfg.fault_plan.latency_micros = 750;

    let root = scratch_root("latency-pressure");
    let verdict = determinism_gate(&cfg, &root, 2);
    assert!(
        verdict.passed,
        "the same injected latency and pressure campaign must replay byte-for-byte:\n{}",
        verdict.log_lines.join("\n")
    );

    let run = run_fixture_under_lab(&cfg, &root.join("evidence"), LabConfig::new(cfg.seed));
    assert_eq!(run.semantics.injected_faults, i64::from(cfg.rounds));
    assert_eq!(
        run.semantics.durable_bytes,
        i64::from(cfg.rounds) * i64::try_from(cfg.payload_bytes).expect("payload fits i64")
    );
    let expected_latency_nanos = u64::from(cfg.rounds) * cfg.fault_plan.latency_micros * 1_000;
    let expected_tick_nanos =
        u64::from(cfg.rounds) * u64::try_from(cfg.tick.as_nanos()).expect("tick fits u64");
    assert!(
        run.virtual_elapsed_nanos >= expected_tick_nanos + expected_latency_nanos,
        "virtual time must include every pacing interval and injected VFS delay"
    );
    assert!(run.semantics.chain_intact);
    assert!(
        run.semantics.network_backpressure_events > 0,
        "a payload spanning the finite virtual-TCP window must observe backpressure"
    );
    assert!(run.region_close.quiescent);
    assert!(run.obligation_balance.balanced);
}

#[test]
fn seeded_residual_random_chaos_is_replayable_and_nonvacuous() {
    let mut cfg = FixtureConfig::new(0xB11D_5A07);
    cfg.rounds = 64;
    cfg.fault_plan.latency = Trigger::PerMille(500);
    cfg.fault_plan.latency_micros = 125;
    let root = scratch_root("residual-random-chaos");

    let verdict = determinism_gate(&cfg, &root, 2);
    assert!(
        verdict.passed,
        "seeded residual chaos must replay byte-for-byte:\n{}",
        verdict.log_lines.join("\n")
    );
    let run = run_fixture_under_lab(&cfg, &root.join("evidence"), LabConfig::new(cfg.seed));
    assert!(
        0 < run.semantics.injected_faults && run.semantics.injected_faults < i64::from(cfg.rounds),
        "the probabilistic trigger must both fire and decline at this pinned seed: {} of {}",
        run.semantics.injected_faults,
        cfg.rounds
    );
    assert!(run.semantics.chain_intact);
    assert!(run.region_close.quiescent);
    assert!(run.obligation_balance.balanced);
}

#[test]
fn lab_runtime_integration_covers_time_disk_network_chaos_pressure_and_lifecycle() {
    let mut cfg = FixtureConfig::new(0x1AB0_5EED);
    cfg.rounds = 32;
    cfg.payload_bytes = 65_536;
    cfg.fault_plan.latency = Trigger::PerMille(500);
    cfg.fault_plan.latency_micros = 250;
    let root = scratch_root("governed-lab-runtime-integration");

    let verdict = determinism_gate(&cfg, &root, 2);
    assert!(verdict.passed, "{}", verdict.log_lines.join("\n"));
    let run = run_fixture_under_lab(&cfg, &root.join("evidence"), LabConfig::new(cfg.seed));
    assert_eq!(run.semantics.produced, i64::from(cfg.rounds));
    assert_eq!(run.semantics.consumed, i64::from(cfg.rounds));
    assert_eq!(
        run.semantics.durable_bytes,
        i64::from(cfg.rounds) * i64::try_from(cfg.payload_bytes).expect("payload fits i64")
    );
    assert!(run.semantics.injected_faults > 0);
    assert!(run.semantics.network_backpressure_events > 0);
    assert!(run.semantics.chain_intact);
    assert!(run.virtual_elapsed_nanos > 0);
    assert!(run.region_close.quiescent && run.region_close.close_completed);
    assert!(run.obligation_balance.balanced);
    assert_eq!(run.obligation_balance.leaked, 0);

    // Keep both refusal controls inside the registered aggregate. Otherwise a
    // regression that drops task failures or the allocation bound could leave
    // the positive lab-runtime selector green.
    lab_task_failure_and_unbounded_payload_cannot_return_successful_semantics();
}

#[test]
fn lab_task_failure_and_unbounded_payload_cannot_return_successful_semantics() {
    let mut faulting = FixtureConfig::new(0x00FA_11ED);
    faulting.fault_plan.write_enospc = Trigger::Always;
    let faulting_root = scratch_root("task-failure-control");
    let failed = std::panic::catch_unwind(|| {
        run_fixture_under_lab(&faulting, &faulting_root, LabConfig::new(faulting.seed))
    });
    assert!(
        failed.is_err(),
        "a producer task that panics on the injected write refusal was reported successful"
    );

    let mut oversized = FixtureConfig::new(0xB0_0D);
    oversized.payload_bytes = MAX_FIXTURE_PAYLOAD_BYTES + 1;
    let oversized_root = scratch_root("payload-bound-control");
    let refused = std::panic::catch_unwind(|| {
        run_fixture_under_lab(&oversized, &oversized_root, LabConfig::new(oversized.seed))
    });
    assert!(
        refused.is_err(),
        "an unbounded allocation request reached the fixture task"
    );
}

// ---------------------------------------------------------------------------
// Item 4: the lab-vs-live dual run
// ---------------------------------------------------------------------------

#[test]
fn the_dual_run_matches_lab_and_live_at_one_seed() {
    let cfg = FixtureConfig::new(0xD0A1);
    let root = scratch_root("dual-honest");
    let outcome = dual_run_fixture(&cfg, &root, None);
    assert!(
        outcome.result.passed(),
        "lab and live must agree on semantics at one seed:\n{}\n{}",
        outcome.result.summary(),
        outcome.log_lines.join("\n")
    );
    let digest_line = outcome
        .log_lines
        .iter()
        .find(|l| l.contains("lab_digest=") && l.contains("live_digest="))
        .expect("the dual run must log both trace digests side by side");
    let lab_digest = digest_line
        .split("lab_digest=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .expect("lab digest present");
    assert!(
        !lab_digest.is_empty(),
        "digest lines must carry real digests"
    );
    assert!(
        digest_line.contains(&format!("live_digest={lab_digest}")),
        "at one seed the two digests must be the same digest: {digest_line}"
    );
}

#[test]
fn the_dual_run_detects_a_semantic_mutation() {
    let cfg = FixtureConfig::new(0xD0A2);
    let root = scratch_root("dual-control");
    let outcome = dual_run_fixture(&cfg, &root, Some(cfg.seed ^ 1));
    assert!(
        !outcome.result.passed(),
        "the control must fire: a live run at a different seed produces \
         different payloads, and a comparison that accepts it compares \
         nothing\n{}",
        outcome.result.summary()
    );
    assert!(
        !outcome.result.verdict.mismatches.is_empty(),
        "the mismatch must be named, not merely counted"
    );
}
