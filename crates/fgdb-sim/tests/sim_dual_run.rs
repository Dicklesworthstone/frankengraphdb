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

use fgdb_crypto::Hasher;
use fgdb_sim::dual_run::{
    DualRunOutcome, FixtureFailureKind, FixtureRunError, FixtureRunReceipt, FixtureRuntime,
    determinism_gate, dual_run_fixture, dual_run_verdict_log_lines, run_fixture_under_lab,
    run_fixture_workload_live, run_fixture_workload_under_lab,
};
use fgdb_sim::fixture::{
    FixtureConfig, FixtureTaskStage, FixtureWorkload, FixtureWorkloadDecodeLimits,
    FixtureWorkloadError, MAX_FIXTURE_PAYLOAD_BYTES,
};
use fgdb_sim::shrink::shrink_fixture_workload_under_lab;
use fgdb_sim::vfs::Trigger;

use asupersync::lab::LabConfig;
use asupersync::trace::RecorderConfig;
use asupersync::trace::replay::ReplayEvent;

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
        assert_eq!(verdict.receipts.len(), 2);
        assert_eq!(
            verdict.receipts[0].trace_digest(),
            verdict.receipts[1].trace_digest(),
            "byte-identical runs must retain the same exact trace digest"
        );
        assert!(verdict.receipts.iter().all(|receipt| {
            receipt.runtime() == FixtureRuntime::Lab
                && receipt.seed() == seed
                && receipt.virtual_clock_epoch_nanos().is_some()
        }));
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
    assert_eq!(run.receipt.runtime(), FixtureRuntime::Lab);
    assert_eq!(run.receipt.seed(), cfg.seed);
    assert_eq!(
        run.receipt.virtual_clock_epoch_nanos(),
        Some(run.virtual_clock_epoch_nanos)
    );
    assert_eq!(
        run.receipt.injected_faults().len(),
        usize::try_from(run.semantics.injected_faults).expect("fault count is nonnegative")
    );
    assert!(run.receipt.matches_lab_replay_trace(run.replay_trace()));
    assert!(
        !run.receipt
            .task_dispatches()
            .expect("lab receipt carries dispatches")
            .is_empty(),
        "the replay recorder must observe actual task dispatches"
    );
    assert!(
        run.receipt
            .injected_faults()
            .iter()
            .all(|event| event.path.is_relative()),
        "fixture receipts must not retain caller scratch prefixes"
    );
    assert_non_failure_receipt(&run.receipt);
    let receipt_log = run.receipt.log_lines().join("\n");
    for required in [
        "scenario_id=",
        "seed=",
        "virtual_clock_epoch_nanos=",
        "lab_replay_trace_digest=",
        "task_dispatch_count=",
        "task_dispatch index=",
        "injected_fault",
        "artifact_fields_asserted=",
        "shrink_iterations=0",
        "final_reproducer_path=none",
    ] {
        assert!(
            receipt_log.contains(required),
            "fixture receipt omitted {required:?}:\n{receipt_log}"
        );
    }

    // Keep both refusal controls inside the registered aggregate. Otherwise a
    // regression that drops task failures or the allocation bound could leave
    // the positive lab-runtime selector green.
    lab_task_failure_and_unbounded_payload_cannot_return_successful_semantics();
    raw_fixture_and_dual_run_receipts_are_execution_bound_and_reconstructable();
}

fn assert_non_failure_receipt(receipt: &FixtureRunReceipt) {
    assert_eq!(receipt.scenario_id(), "fgdb.sim.fixture.producer_consumer");
    assert!(!receipt.trace_digest().is_empty());
    assert!(receipt.artifact_fields_asserted().is_empty());
    assert_eq!(receipt.shrink_iterations(), 0);
    assert!(receipt.final_reproducer_path().is_none());
    match receipt.runtime() {
        FixtureRuntime::Lab => {
            assert!(receipt.lab_replay_trace_digest().is_some());
            assert!(
                !receipt
                    .task_dispatches()
                    .expect("lab receipt carries task dispatches")
                    .is_empty()
            );
        }
        FixtureRuntime::Live => {
            assert_eq!(receipt.lab_replay_trace_digest(), None);
            assert_eq!(receipt.task_dispatches(), None);
        }
    }
}

fn assert_failed_dual_run_log_is_lossless(outcome: &DualRunOutcome) {
    let detail_lines = dual_run_verdict_log_lines(&outcome.result);
    assert_eq!(
        detail_lines.len(),
        outcome.result.verdict.mismatches.len(),
        "this control has semantic mismatches and no invariant violations"
    );
    for (index, mismatch) in outcome.result.verdict.mismatches.iter().enumerate() {
        let expected = format!(
            "dual-run mismatch index={index} field={:?} description={:?} lab_value={:?} live_value={:?}",
            mismatch.field, mismatch.description, mismatch.lab_value, mismatch.live_value,
        );
        assert_eq!(detail_lines[index], expected);
        assert!(
            outcome.log_lines.contains(&expected),
            "the retained verdict log omitted mismatch {index}: {expected}"
        );
    }

    let mut invariant_control = outcome.result.clone();
    invariant_control
        .lab_invariant_violations
        .push("planted lab invariant detail".to_string());
    invariant_control
        .live_invariant_violations
        .push("planted live invariant detail".to_string());
    let invariant_lines = dual_run_verdict_log_lines(&invariant_control);
    assert!(invariant_lines.contains(
        &"dual-run invariant_violation runtime=lab index=0 detail=\"planted lab invariant detail\""
            .to_string()
    ));
    assert!(invariant_lines.contains(
        &"dual-run invariant_violation runtime=live index=0 detail=\"planted live invariant detail\""
            .to_string()
    ));
}

#[test]
fn lab_task_failure_and_unbounded_payload_cannot_return_successful_semantics() {
    let mut faulting = FixtureConfig::new(0x00FA_11ED);
    faulting.fault_plan.write_enospc = Trigger::Always;
    let faulting_workload =
        FixtureWorkload::try_from_config(&faulting).expect("faulting workload materializes");
    let faulting_root = scratch_root("task-failure-control");
    let lab_result = run_fixture_workload_under_lab(
        &faulting,
        &faulting_workload,
        &faulting_root.join("typed-lab"),
        LabConfig::new(faulting.seed),
    );
    assert!(
        lab_result.is_err(),
        "injected LAB write refusal must be typed"
    );
    let lab_failure = if let Err(error) = lab_result {
        error
    } else {
        return;
    };
    let lab_evidence = lab_failure
        .failure_evidence()
        .expect("component failure carries execution evidence");
    assert_eq!(lab_evidence.runtime(), FixtureRuntime::Lab);
    assert_eq!(lab_evidence.seed(), faulting.seed);
    assert!(lab_evidence.virtual_clock_epoch_nanos().is_some());
    assert!(lab_evidence.matches_workload(&faulting_workload));
    assert!(!lab_evidence.trace_digest().is_empty());
    assert!(lab_evidence.lab_replay_trace_digest().is_some());
    assert!(
        !lab_evidence
            .task_dispatches()
            .expect("LAB failure carries dispatches")
            .is_empty()
    );
    assert_eq!(
        lab_evidence.task_error().stage(),
        FixtureTaskStage::DurableWrite
    );
    assert_eq!(lab_evidence.task_error().action(), Some(0));
    assert_eq!(
        lab_evidence.task_error().kind(),
        std::io::ErrorKind::StorageFull
    );
    assert_eq!(lab_evidence.injected_faults().len(), 1);
    assert!(matches!(
        lab_evidence.injected_faults()[0].kind,
        fgdb_sim::vfs::FaultKind::WriteEnospc { requested } if requested > 0
    ));
    assert_eq!(
        lab_evidence.injected_faults()[0].path.to_string_lossy(),
        "record-0000.bin"
    );
    assert!(!lab_evidence.execution_digest().is_empty());
    let live_result = run_fixture_workload_live(
        &faulting,
        &faulting_workload,
        &faulting_root.join("typed-live"),
    );
    assert!(
        live_result.is_err(),
        "injected live write refusal must be typed"
    );
    let live_failure = if let Err(error) = live_result {
        error
    } else {
        return;
    };
    assert_eq!(
        live_failure.failure_kind(),
        Some(FixtureFailureKind::Producer {
            stage: FixtureTaskStage::DurableWrite,
            kind: std::io::ErrorKind::StorageFull,
        })
    );
    let live_evidence = live_failure
        .failure_evidence()
        .expect("live component failure carries execution evidence");
    assert_eq!(live_evidence.runtime(), FixtureRuntime::Live);
    assert_eq!(live_evidence.virtual_clock_epoch_nanos(), None);
    assert!(live_evidence.lab_replay_trace_digest().is_none());
    assert!(live_evidence.task_dispatches().is_none());
    assert!(live_evidence.matches_workload(&faulting_workload));
    assert_eq!(live_evidence.injected_faults().len(), 1);
    assert_ne!(
        live_evidence.execution_digest(),
        lab_evidence.execution_digest(),
        "runtime posture is part of the failure execution seal"
    );

    let shrunk = shrink_fixture_workload_under_lab(
        &faulting,
        &faulting_workload,
        &faulting_root.join("shrink"),
        LabConfig::new(faulting.seed),
    )
    .expect("fixture shrink infrastructure succeeds")
    .expect("the injected producer failure reproduces");
    assert_eq!(
        shrunk.original_workload_digest(),
        faulting_workload.canonical_digest_hex()
    );
    assert_eq!(
        shrunk.original_execution_digest(),
        lab_evidence.execution_digest()
    );
    assert_eq!(shrunk.workload().actions().len(), 1);
    assert_eq!(shrunk.workload().actions()[0].ordinal(), 0);
    assert_eq!(
        shrunk.failure(),
        FixtureFailureKind::Producer {
            stage: FixtureTaskStage::DurableWrite,
            kind: std::io::ErrorKind::StorageFull,
        }
    );
    assert!(shrunk.attempts() > 1);
    assert!(shrunk.accepted() > 0);
    assert_eq!(shrunk.rejected_different_failure(), 0);
    let minimal_result = run_fixture_workload_under_lab(
        &faulting,
        shrunk.workload(),
        &faulting_root.join("minimal-replay"),
        LabConfig::new(faulting.seed),
    );
    assert!(matches!(
        &minimal_result,
        Err(error) if error.failure_kind() == Some(shrunk.failure())
    ));
    let minimal_evidence = minimal_result
        .as_ref()
        .err()
        .and_then(FixtureRunError::failure_evidence)
        .expect("minimal failure carries execution evidence");
    assert_eq!(
        minimal_evidence.execution_digest(),
        shrunk.minimal_execution_digest()
    );
    assert_ne!(
        shrunk.original_execution_digest(),
        shrunk.minimal_execution_digest(),
        "removing actions must change the sealed execution"
    );

    let blocked_scratch_root = scratch_root("typed-scratch-io-control");
    let blocking_file = blocked_scratch_root.join("ordinary-file");
    let blocked_child = blocking_file.join("child");
    std::fs::write(&blocking_file, b"not a directory").expect("plant blocking file");
    assert!(matches!(
        run_fixture_workload_under_lab(
            &faulting,
            shrunk.workload(),
            &blocked_child,
            LabConfig::new(faulting.seed),
        ),
        Err(FixtureRunError::ScratchIo(_))
    ));
    assert!(!blocked_child.exists());

    // One-minimality is executable, not inferred from the shrinker's counts:
    // deleting the sole retained action yields a canonical empty workload and
    // a passing LAB run under the identical fault plan.
    let mut empty_bytes = shrunk.workload().to_canonical_bytes();
    empty_bytes.truncate(8 + 8 + 4);
    empty_bytes[16..20].copy_from_slice(&0u32.to_le_bytes());
    let empty = FixtureWorkload::try_from_canonical_bytes(
        &empty_bytes,
        FixtureWorkloadDecodeLimits::default(),
    )
    .expect("empty retained workload is canonical");
    run_fixture_workload_under_lab(
        &faulting,
        &empty,
        &faulting_root.join("minimal-minus-action"),
        LabConfig::new(faulting.seed),
    )
    .expect("removing the sole causal action must clear the failure");

    let failed = std::panic::catch_unwind(|| {
        run_fixture_under_lab(&faulting, &faulting_root, LabConfig::new(faulting.seed))
    });
    assert!(
        failed.is_err(),
        "the infallible convenience wrapper must not launder a typed task failure"
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
    assert_eq!(outcome.lab_receipt.runtime(), FixtureRuntime::Lab);
    assert_eq!(outcome.live_receipt.runtime(), FixtureRuntime::Live);
    assert_eq!(outcome.lab_receipt.seed(), cfg.seed);
    assert_eq!(outcome.live_receipt.seed(), cfg.seed);
    assert!(outcome.lab_receipt.virtual_clock_epoch_nanos().is_some());
    assert_eq!(outcome.live_receipt.virtual_clock_epoch_nanos(), None);
    assert_non_failure_receipt(&outcome.lab_receipt);
    assert_non_failure_receipt(&outcome.live_receipt);
    let trace_line = outcome
        .log_lines
        .iter()
        .find(|line| line.contains("lab_trace_digest=") && line.contains("live_trace_digest="))
        .expect("the dual run retains both exact execution-trace digests");
    assert!(trace_line.contains(outcome.lab_receipt.trace_digest()));
    assert!(trace_line.contains(outcome.live_receipt.trace_digest()));
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
    assert_eq!(outcome.lab_receipt.seed(), cfg.seed);
    assert_eq!(outcome.live_receipt.seed(), cfg.seed ^ 1);
    assert_ne!(
        outcome.lab_receipt.trace_digest(),
        outcome.live_receipt.trace_digest(),
        "the live receipt must bind the mutated execution rather than the caller's base seed"
    );

    assert_failed_dual_run_log_is_lossless(&outcome);
}

#[test]
fn raw_fixture_and_dual_run_receipts_are_execution_bound_and_reconstructable() {
    let cfg = FixtureConfig::new(0xD0A3);
    let lab_root = scratch_root("raw-receipt-trace-binding");
    let raw = run_fixture_under_lab(&cfg, &lab_root, LabConfig::new(cfg.seed));
    let expected_workload =
        FixtureWorkload::try_from_config(&cfg).expect("fixture config materializes");
    let workload_bytes = expected_workload.to_canonical_bytes();
    let mut workload_hasher = Hasher::new();
    workload_hasher.update(b"fgdb.sim.fixture.workload.v1");
    workload_hasher.update(&workload_bytes);
    assert_eq!(raw.workload(), &expected_workload);
    assert_eq!(raw.receipt.workload_bytes(), workload_bytes);
    assert_eq!(
        raw.receipt.workload_digest(),
        workload_hasher.finalize().to_hex(),
        "the receipt must bind the exact versioned workload bytes"
    );
    assert_eq!(
        raw.receipt.workload_action_count(),
        usize::try_from(cfg.rounds).expect("rounds fit usize")
    );
    assert!(raw.receipt.matches_workload(&expected_workload));
    assert_eq!(
        expected_workload
            .actions()
            .iter()
            .map(|action| action.ordinal())
            .collect::<Vec<_>>(),
        (0..cfg.rounds).collect::<Vec<_>>()
    );
    assert!(expected_workload.actions().iter().all(|action| {
        action.delay_nanos() == u64::try_from(cfg.tick.as_nanos()).expect("fixture tick fits u64")
            && action.payload().len() == cfg.payload_bytes
    }));

    let decoded = FixtureWorkload::try_from_canonical_bytes(
        &workload_bytes,
        FixtureWorkloadDecodeLimits::default(),
    )
    .expect("canonical workload decodes");
    assert_eq!(decoded, expected_workload);
    assert_eq!(decoded.to_canonical_bytes(), workload_bytes);
    let decoded_run = run_fixture_workload_under_lab(
        &cfg,
        &decoded,
        &lab_root.join("decoded-workload"),
        LabConfig::new(cfg.seed),
    )
    .expect("decoded workload executes");
    assert_eq!(decoded_run.trace_bytes, raw.trace_bytes);
    assert!(decoded_run.receipt.matches_workload(&decoded));

    // Version-1 layout: magic(8), seed(8), count(4), then the first action's
    // ordinal(4), delay(8), payload length(4), and payload. Mutating the
    // payload remains a valid workload but must change the actual execution.
    let first_action_offset = 8 + 8 + 4;
    let first_delay_offset = first_action_offset + 4;
    let first_payload_offset = first_action_offset + 4 + 8 + 4;
    let mut substituted_bytes = workload_bytes.clone();
    substituted_bytes[first_payload_offset] ^= 0x80;
    let substituted = FixtureWorkload::try_from_canonical_bytes(
        &substituted_bytes,
        FixtureWorkloadDecodeLimits::default(),
    )
    .expect("payload substitution remains structurally valid");
    assert!(!raw.receipt.matches_workload(&substituted));
    let substituted_run = run_fixture_workload_under_lab(
        &cfg,
        &substituted,
        &lab_root.join("substituted-workload"),
        LabConfig::new(cfg.seed),
    )
    .expect("substituted workload executes");
    assert_ne!(
        substituted_run.semantics.producer_digest_hex, raw.semantics.producer_digest_hex,
        "the explicit runner must execute supplied payloads rather than regenerate config inputs"
    );
    assert!(substituted_run.receipt.matches_workload(&substituted));

    let delay_delta = 1_000_000u64;
    let mut delayed_bytes = workload_bytes.clone();
    let delayed_first = expected_workload.actions()[0]
        .delay_nanos()
        .checked_add(delay_delta)
        .expect("small delay mutation fits");
    delayed_bytes[first_delay_offset..first_delay_offset + 8]
        .copy_from_slice(&delayed_first.to_le_bytes());
    let delayed = FixtureWorkload::try_from_canonical_bytes(
        &delayed_bytes,
        FixtureWorkloadDecodeLimits::default(),
    )
    .expect("bounded delay substitution remains structurally valid");
    let delayed_run = run_fixture_workload_under_lab(
        &cfg,
        &delayed,
        &lab_root.join("delayed-workload"),
        LabConfig::new(cfg.seed),
    )
    .expect("bounded delayed workload executes");
    assert_eq!(
        delayed_run.virtual_elapsed_nanos,
        raw.virtual_elapsed_nanos + delay_delta,
        "the explicit runner must execute the supplied per-action delay"
    );
    assert_eq!(
        delayed_run.semantics.producer_digest_hex, raw.semantics.producer_digest_hex,
        "a timing-only workload mutation must preserve payload semantics"
    );
    assert!(!raw.receipt.matches_workload(&delayed));

    let mut wrong_magic = workload_bytes.clone();
    wrong_magic[0] ^= 1;
    assert_eq!(
        FixtureWorkload::try_from_canonical_bytes(
            &wrong_magic,
            FixtureWorkloadDecodeLimits::default()
        ),
        Err(FixtureWorkloadError::WrongMagic)
    );
    let mut wrong_ordinal = workload_bytes.clone();
    wrong_ordinal[first_action_offset..first_action_offset + 4]
        .copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(
        FixtureWorkload::try_from_canonical_bytes(
            &wrong_ordinal,
            FixtureWorkloadDecodeLimits::default()
        ),
        Err(FixtureWorkloadError::NonContiguousAction {
            expected: 0,
            actual: 1,
        })
    );
    assert!(matches!(
        FixtureWorkload::try_from_canonical_bytes(
            &workload_bytes[..workload_bytes.len() - 1],
            FixtureWorkloadDecodeLimits::default()
        ),
        Err(FixtureWorkloadError::Truncated)
    ));
    let mut trailing = workload_bytes.clone();
    trailing.push(0);
    assert_eq!(
        FixtureWorkload::try_from_canonical_bytes(
            &trailing,
            FixtureWorkloadDecodeLimits::default()
        ),
        Err(FixtureWorkloadError::TrailingBytes)
    );
    let action_limited = FixtureWorkloadDecodeLimits {
        max_actions: expected_workload.actions().len() - 1,
        ..FixtureWorkloadDecodeLimits::default()
    };
    assert!(matches!(
        FixtureWorkload::try_from_canonical_bytes(&workload_bytes, action_limited),
        Err(FixtureWorkloadError::ActionCountExceeded { .. })
    ));
    let total_payload_bytes = expected_workload
        .actions()
        .iter()
        .map(|action| action.payload().len())
        .sum::<usize>();
    let payload_limited = FixtureWorkloadDecodeLimits {
        max_payload_bytes: total_payload_bytes - 1,
        ..FixtureWorkloadDecodeLimits::default()
    };
    assert!(matches!(
        FixtureWorkload::try_from_canonical_bytes(&workload_bytes, payload_limited),
        Err(FixtureWorkloadError::PayloadBytesExceeded { .. })
    ));
    let action_delay_limited = FixtureWorkloadDecodeLimits {
        max_action_delay_nanos: expected_workload.actions()[0].delay_nanos() - 1,
        ..FixtureWorkloadDecodeLimits::default()
    };
    assert!(matches!(
        FixtureWorkload::try_from_canonical_bytes(&workload_bytes, action_delay_limited),
        Err(FixtureWorkloadError::ActionDelayExceeded { action: 0, .. })
    ));
    let total_delay = expected_workload
        .actions()
        .iter()
        .map(|action| action.delay_nanos())
        .sum::<u64>();
    let total_delay_limited = FixtureWorkloadDecodeLimits {
        max_total_delay_nanos: total_delay - 1,
        ..FixtureWorkloadDecodeLimits::default()
    };
    assert!(matches!(
        FixtureWorkload::try_from_canonical_bytes(&workload_bytes, total_delay_limited),
        Err(FixtureWorkloadError::TotalDelayExceeded { .. })
    ));
    let mut centuries_delay = workload_bytes.clone();
    centuries_delay[first_delay_offset..first_delay_offset + 8]
        .copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(matches!(
        FixtureWorkload::try_from_canonical_bytes(
            &centuries_delay,
            FixtureWorkloadDecodeLimits::default()
        ),
        Err(FixtureWorkloadError::ActionDelayExceeded { action: 0, .. })
    ));
    let byte_limited = FixtureWorkloadDecodeLimits {
        max_encoded_bytes: workload_bytes.len() - 1,
        ..FixtureWorkloadDecodeLimits::default()
    };
    assert!(matches!(
        FixtureWorkload::try_from_canonical_bytes(&workload_bytes, byte_limited),
        Err(FixtureWorkloadError::EncodedBytesExceeded { .. })
    ));
    let mut wrong_seed_cfg = cfg.clone();
    wrong_seed_cfg.seed ^= 1;
    let wrong_seed_root = lab_root.join("wrong-seed-refusal");
    assert!(matches!(
        run_fixture_workload_under_lab(
            &wrong_seed_cfg,
            &expected_workload,
            &wrong_seed_root,
            LabConfig::new(wrong_seed_cfg.seed)
        ),
        Err(FixtureRunError::Workload(
            FixtureWorkloadError::SeedMismatch { .. }
        ))
    ));
    assert!(
        !wrong_seed_root.exists(),
        "seed mismatch must refuse before fixture filesystem side effects"
    );

    let mut trace_hasher = Hasher::new();
    trace_hasher.update(b"fgdb.sim.fixture.trace.v1");
    trace_hasher.update(&raw.trace_bytes);
    assert_eq!(
        raw.receipt.trace_digest(),
        trace_hasher.finalize().to_hex(),
        "the receipt digest must bind the completed execution bytes"
    );
    let replay_bytes = raw
        .replay_trace()
        .to_bytes()
        .expect("completed replay trace serializes");
    let mut replay_hasher = Hasher::new();
    replay_hasher.update(b"fgdb.sim.fixture.lab-replay-trace.v1");
    replay_hasher.update(&replay_bytes);
    assert_eq!(
        raw.receipt
            .lab_replay_trace_digest()
            .expect("lab receipt carries replay digest"),
        replay_hasher.finalize().to_hex(),
        "the receipt digest must bind the complete foundation replay trace"
    );
    let expected_dispatches: Vec<(u64, u64)> = raw
        .replay_trace()
        .events
        .iter()
        .filter_map(|event| match event {
            ReplayEvent::TaskScheduled { task, at_tick } => Some((task.0, *at_tick)),
            _ => None,
        })
        .collect();
    let retained_dispatches: Vec<(u64, u64)> = raw
        .receipt
        .task_dispatches()
        .expect("lab receipt carries task dispatches")
        .iter()
        .map(|step| (step.task_id(), step.at_tick()))
        .collect();
    assert!(!expected_dispatches.is_empty());
    let mut dispatched_task_ids: Vec<u64> = expected_dispatches
        .iter()
        .map(|(task_id, _)| *task_id)
        .collect();
    dispatched_task_ids.sort_unstable();
    dispatched_task_ids.dedup();
    assert_eq!(
        dispatched_task_ids.len(),
        2,
        "the producer and consumer must both appear in the real schedule"
    );
    for task_id in dispatched_task_ids {
        let last_dispatch = raw
            .replay_trace()
            .events
            .iter()
            .rposition(|event| {
                matches!(event, ReplayEvent::TaskScheduled { task, .. } if task.0 == task_id)
            })
            .expect("scheduled task has a dispatch");
        assert!(
            raw.replay_trace()
                .events
                .iter()
                .skip(last_dispatch + 1)
                .any(|event| {
                    matches!(
                        event,
                        ReplayEvent::TaskCompleted { task, outcome: 0 } if task.0 == task_id
                    )
                }),
            "task {task_id} must complete successfully after its last dispatch"
        );
    }
    assert_eq!(retained_dispatches, expected_dispatches);
    assert_eq!(raw.replay_trace().metadata.seed, cfg.seed);
    assert!(raw.receipt.matches_lab_replay_trace(raw.replay_trace()));

    let caller_truncated = run_fixture_under_lab(
        &cfg,
        &lab_root.join("caller-truncated-recorder"),
        LabConfig::new(cfg.seed)
            .with_replay_recording(RecorderConfig::enabled().with_max_events(Some(1))),
    );
    assert!(
        caller_truncated
            .receipt
            .matches_lab_replay_trace(caller_truncated.replay_trace()),
        "the fixture adapter must replace a caller-truncated recorder with its complete evidence recorder"
    );
    assert!(
        caller_truncated
            .receipt
            .task_dispatches()
            .expect("lab receipt carries task dispatches")
            .len()
            > 1,
        "a one-event caller limit must not truncate fixture evidence"
    );

    let mut mutated_replay = raw.replay_trace().clone();
    let mutated_tick = mutated_replay
        .events
        .iter_mut()
        .find_map(|event| match event {
            ReplayEvent::TaskScheduled { at_tick, .. } => Some(at_tick),
            _ => None,
        })
        .expect("fixture replay contains a dispatch decision");
    *mutated_tick = mutated_tick
        .checked_add(1)
        .expect("fixture tick increments");
    assert!(
        !raw.receipt.matches_lab_replay_trace(&mutated_replay),
        "a substituted dispatch decision must not match the retained execution receipt"
    );

    let repeated = run_fixture_under_lab(
        &cfg,
        &lab_root.join("same-seed-same-workload"),
        LabConfig::new(cfg.seed),
    );
    assert_eq!(
        raw.replay_trace()
            .to_bytes()
            .expect("first replay trace serializes"),
        repeated
            .replay_trace()
            .to_bytes()
            .expect("repeated replay trace serializes"),
        "one seed and workload must reproduce the exact foundation replay trace"
    );
    assert_eq!(
        raw.receipt.task_dispatches(),
        repeated.receipt.task_dispatches()
    );

    let mut changed_workload = cfg.clone();
    changed_workload.rounds += 1;
    let changed = run_fixture_under_lab(
        &changed_workload,
        &lab_root.join("same-seed-changed-workload"),
        LabConfig::new(changed_workload.seed),
    );
    assert_eq!(raw.receipt.seed(), changed.receipt.seed());
    assert_ne!(
        raw.receipt.workload_digest(),
        changed.receipt.workload_digest(),
        "a same-seed workload mutation must change canonical workload identity"
    );
    assert!(!raw.receipt.matches_workload(changed.workload()));
    assert_ne!(
        raw.receipt.trace_digest(),
        changed.receipt.trace_digest(),
        "a same-seed workload mutation must change the execution-bound digest"
    );
    assert_ne!(
        raw.receipt.lab_replay_trace_digest(),
        changed.receipt.lab_replay_trace_digest(),
        "a same-seed workload mutation must change the captured foundation replay trace"
    );
    assert_ne!(
        raw.receipt.task_dispatches(),
        changed.receipt.task_dispatches(),
        "a same-seed workload mutation must change the actual task-dispatch schedule"
    );
    assert!(
        !raw.receipt.matches_lab_replay_trace(changed.replay_trace()),
        "a trace from another workload must not validate this execution receipt"
    );

    let honest_root = scratch_root("dual-receipt-contract");
    let honest = dual_run_fixture(&cfg, &honest_root, None);
    assert!(honest.result.passed(), "{}", honest.result.summary());
    assert_non_failure_receipt(&honest.lab_receipt);
    assert_non_failure_receipt(&honest.live_receipt);
    assert_eq!(honest.lab_receipt.seed(), cfg.seed);
    assert_eq!(honest.live_receipt.seed(), cfg.seed);
    assert_eq!(
        honest.lab_receipt.workload_bytes(),
        honest.live_receipt.workload_bytes(),
        "dual-run runtimes must consume one exact canonical workload"
    );
    assert!(honest.lab_receipt.virtual_clock_epoch_nanos().is_some());
    assert_eq!(honest.live_receipt.virtual_clock_epoch_nanos(), None);

    let log = honest.log_lines.join("\n");
    for required in [
        "runtime=lab",
        "runtime=live",
        "virtual_clock_epoch_nanos=",
        "virtual_clock_epoch_nanos=not-applicable-live",
        "workload_digest=",
        "workload_action_count=",
        "lab_trace_digest=",
        "live_trace_digest=",
        "artifact_fields_asserted=",
        "shrink_iterations=0",
        "final_reproducer_path=none",
    ] {
        assert!(
            log.contains(required),
            "dual-run receipt log omitted {required:?}:\n{log}"
        );
    }

    let live_seed = cfg.seed ^ 1;
    let mutated_root = scratch_root("dual-receipt-mutation");
    let mutated = dual_run_fixture(&cfg, &mutated_root, Some(live_seed));
    assert!(!mutated.result.passed());
    assert_eq!(mutated.lab_receipt.seed(), cfg.seed);
    assert_eq!(mutated.live_receipt.seed(), live_seed);
    assert_ne!(
        mutated.lab_receipt.trace_digest(),
        mutated.live_receipt.trace_digest()
    );
    assert!(
        mutated
            .live_receipt
            .log_lines()
            .iter()
            .any(|line| line.contains(&format!("seed={live_seed:#x}"))),
        "the receipt must name the execution that actually ran, not the base request"
    );
    assert_failed_dual_run_log_is_lossless(&mutated);
}
