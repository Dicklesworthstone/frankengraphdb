//! The two-runs-one-seed determinism gate and the lab-vs-live dual-run driver
//! (plan §15.1 force multiplier 4; q97e items 4 and 6; bead `fgdb-qd2s`).
//!
//! Both consume the exported fixture from [`crate::fixture`]:
//!
//! - [`determinism_gate`] runs the fixture N times under the LAB runtime at
//!   one seed and demands **byte-identical** serialized traces plus equal
//!   Foata trace fingerprints and schedule-certificate hashes. A failure names
//!   the first diverging byte and event — the bead's "determinism-gate
//!   failures log the first diverging trace offset" requirement is a field on
//!   the verdict, not a prose promise.
//! - [`dual_run_fixture`] wires the same fixture into asupersync's own
//!   [`DualRunHarness`] — the foundation's structured lab-vs-live comparison —
//!   running once under the lab (virtual clock, deterministic scheduler) and
//!   once under the live runtime (wall clock, real scheduler), then comparing
//!   the clock-free [`crate::fixture::FixtureSemantics`] projections. We
//!   deliberately do NOT hand-roll trace diffing here: Doctrine #1 says the
//!   foundation is consumed as-is, and the harness already owns normalization,
//!   invariant checks, and mismatch classification.
//!
//! # Why the two sides may differ, and what must not
//!
//! Under live, timestamps are wall-clock and the producer/consumer interleave
//! at the scheduler's whim, so trace BYTES are not comparable across runtimes.
//! What must survive the runtime swap is meaning: the chain digests, the
//! record counts, the durable byte count. That is exactly the semantic
//! projection the harness compares, and the counters ride
//! [`ResourceSurfaceRecord`] with exact tolerance.
//!
//! # Every verdict is reconstructable from its record
//!
//! Both entry points return structs whose `log_lines` carry the seed, per-run
//! fingerprints, schedule hashes, and side-by-side semantic digests, so a
//! campaign verdict can be re-read without re-running (q97e's logging
//! acceptance).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use asupersync::Budget;
use asupersync::lab::{
    CancellationRecord, DualRunHarness, DualRunResult, LabConfig, LabRuntime, LoserDrainRecord,
    NormalizedSemantics, ObligationBalanceRecord, RegionCloseRecord, ResourceSurfaceRecord,
    TerminalOutcome, capture_obligation_balance, capture_region_close, normalize_lab_report,
};
use asupersync::runtime::RuntimeBuilder;
use fgdb_crypto::Hasher;

use crate::fixture::{FixtureConfig, FixtureSemantics, first_divergence, fixture_futures};
use crate::vfs::FaultEvent;

/// Scope name for the fixture's semantic counters.
const SURFACE_SCOPE: &str = "fgdb.sim.fixture";

/// Stable scenario identity shared by the fixture's lab and live executions.
const FIXTURE_SCENARIO_ID: &str = "fgdb.sim.fixture.producer_consumer";

/// Runtime posture that produced a fixture receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureRuntime {
    /// Deterministic asupersync lab runtime with a virtual clock.
    Lab,
    /// Live asupersync runtime with a wall clock and ordinary scheduler.
    Live,
}

impl FixtureRuntime {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Lab => "lab",
            Self::Live => "live",
        }
    }
}

/// Immutable, reconstructable facts emitted by one fixture execution.
///
/// This is deliberately separate from [`crate::artifact::RunReceipt`]: that
/// receipt is closed over the built-in fault-replay scenarios, while this one
/// records the exported producer/consumer fixture that runs under both lab and
/// live. A live execution has no virtual-clock epoch and says so explicitly;
/// it must never relabel a wall-clock observation as virtual time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureRunReceipt {
    scenario_id: &'static str,
    runtime: FixtureRuntime,
    seed: u64,
    virtual_clock_epoch_nanos: Option<u64>,
    trace_digest: String,
    injected_faults: Vec<FaultEvent>,
    artifact_fields_asserted: Vec<&'static str>,
    shrink_iterations: usize,
    final_reproducer_path: Option<PathBuf>,
}

impl FixtureRunReceipt {
    fn new(
        runtime: FixtureRuntime,
        seed: u64,
        virtual_clock_epoch_nanos: Option<u64>,
        trace_bytes: &[u8],
        injected_faults: Vec<FaultEvent>,
    ) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(b"fgdb.sim.fixture.trace.v1");
        hasher.update(trace_bytes);
        Self {
            scenario_id: FIXTURE_SCENARIO_ID,
            runtime,
            seed,
            virtual_clock_epoch_nanos,
            trace_digest: hasher.finalize().to_hex(),
            injected_faults,
            // The exported fixture is a passing substrate run. It neither
            // asserts a failure-artifact schema nor claims to be pre-shrunk.
            artifact_fields_asserted: Vec::new(),
            shrink_iterations: 0,
            final_reproducer_path: None,
        }
    }

    #[must_use]
    pub const fn scenario_id(&self) -> &'static str {
        self.scenario_id
    }

    #[must_use]
    pub const fn runtime(&self) -> FixtureRuntime {
        self.runtime
    }

    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    #[must_use]
    pub const fn virtual_clock_epoch_nanos(&self) -> Option<u64> {
        self.virtual_clock_epoch_nanos
    }

    #[must_use]
    pub fn trace_digest(&self) -> &str {
        &self.trace_digest
    }

    #[must_use]
    pub fn injected_faults(&self) -> &[FaultEvent] {
        &self.injected_faults
    }

    #[must_use]
    pub fn artifact_fields_asserted(&self) -> &[&'static str] {
        &self.artifact_fields_asserted
    }

    #[must_use]
    pub const fn shrink_iterations(&self) -> usize {
        self.shrink_iterations
    }

    #[must_use]
    pub fn final_reproducer_path(&self) -> Option<&Path> {
        self.final_reproducer_path.as_deref()
    }

    /// Complete structured log for this execution without rerunning it.
    #[must_use]
    pub fn log_lines(&self) -> Vec<String> {
        let virtual_epoch = self.virtual_clock_epoch_nanos.map_or_else(
            || "not-applicable-live".to_string(),
            |epoch| epoch.to_string(),
        );
        let mut lines = vec![format!(
            "fixture-run scenario_id={} runtime={} seed={:#x} virtual_clock_epoch_nanos={} trace_digest={}",
            self.scenario_id,
            self.runtime.as_str(),
            self.seed,
            virtual_epoch,
            self.trace_digest,
        )];
        for event in &self.injected_faults {
            lines.push(format!(
                "fixture-run runtime={} injected_fault seq={} class={} path={} detail={:?}",
                self.runtime.as_str(),
                event.seq,
                event.kind.class(),
                event.path.display(),
                event.kind,
            ));
        }
        lines.push(format!(
            "fixture-run runtime={} artifact_fields_asserted={}",
            self.runtime.as_str(),
            self.artifact_fields_asserted.join(",")
        ));
        lines.push(format!(
            "fixture-run runtime={} shrink_iterations={} final_reproducer_path={}",
            self.runtime.as_str(),
            self.shrink_iterations,
            self.final_reproducer_path
                .as_deref()
                .map_or_else(|| "none".to_string(), |path| path.display().to_string())
        ));
        lines
    }
}

/// One completed lab execution of the fixture.
pub struct LabFixtureRun {
    /// Canonical serialized trace (see [`crate::fixture::TraceHandle::to_bytes`]).
    pub trace_bytes: Vec<u8>,
    /// Foata/Mazurkiewicz fingerprint from the lab's own trace buffer.
    pub trace_fingerprint: u64,
    /// Schedule-certificate hash: the dispatch decisions themselves.
    pub schedule_hash: u64,
    /// Virtual nanoseconds the run consumed — proof the clock was virtual.
    pub virtual_elapsed_nanos: u64,
    /// Terminal virtual-clock epoch reported by the lab runtime.
    pub virtual_clock_epoch_nanos: u64,
    /// Clock-free semantic projection.
    pub semantics: FixtureSemantics,
    /// Region-close evidence observed from the lab runtime report.
    pub region_close: RegionCloseRecord,
    /// Obligation-leak evidence observed by the lab runtime oracles.
    pub obligation_balance: ObligationBalanceRecord,
    /// Immutable facts retained from this exact execution.
    pub receipt: FixtureRunReceipt,
}

/// One completed live execution of the fixture.
pub struct LiveFixtureRun {
    /// Clock-free semantic projection.
    pub semantics: FixtureSemantics,
    /// Region-close evidence derived from joining both fixture futures.
    pub region_close: RegionCloseRecord,
    /// Explicit live-adapter counters for the fixture's zero-obligation scope.
    pub obligation_balance: ObligationBalanceRecord,
    /// Immutable facts retained from this exact execution.
    pub receipt: FixtureRunReceipt,
}

/// Runs the fixture once under the lab runtime with auto-advancing virtual
/// time, both components as separate tasks under the deterministic scheduler.
///
/// # Panics
///
/// Panics if the run fails to reach quiescence or violates a runtime
/// invariant — a broken harness must never return a comparable-looking value.
#[must_use]
pub fn run_fixture_under_lab(
    cfg: &FixtureConfig,
    scratch_dir: &Path,
    mut lab_config: LabConfig,
) -> LabFixtureRun {
    lab_config.auto_advance_time = true;
    let mut lab = LabRuntime::new(lab_config);
    let root = lab.state.create_root_region(Budget::INFINITE);
    let (producer_fut, consumer_fut, trace) = fixture_futures(cfg, scratch_dir);
    let (producer_task, mut producer_handle) = lab
        .state
        .create_task(root, Budget::INFINITE, producer_fut)
        .expect("create producer task");
    let (consumer_task, mut consumer_handle) = lab
        .state
        .create_task(root, Budget::INFINITE, consumer_fut)
        .expect("create consumer task");
    lab.scheduler.lock().schedule(producer_task, 0);
    lab.scheduler.lock().schedule(consumer_task, 0);
    let virtual_report = lab.run_with_auto_advance();
    let report = lab.report();
    assert!(
        matches!(producer_handle.try_join(), Ok(Some(()))),
        "fixture producer did not complete successfully"
    );
    assert!(
        matches!(consumer_handle.try_join(), Ok(Some(()))),
        "fixture consumer did not complete successfully"
    );
    assert!(
        report.quiescent,
        "fixture lab run did not reach quiescence: {report:?}"
    );
    assert!(
        report.invariant_violations.is_empty(),
        "fixture lab run violated invariants: {:?}",
        report.invariant_violations
    );
    let (runtime_semantics, _capture_manifest) = normalize_lab_report(&report, SURFACE_SCOPE);
    let trace_bytes = trace.to_bytes();
    let semantics = trace.semantics();
    let receipt = FixtureRunReceipt::new(
        FixtureRuntime::Lab,
        cfg.seed,
        Some(report.now_nanos),
        &trace_bytes,
        trace.fault_events(),
    );
    LabFixtureRun {
        trace_bytes,
        trace_fingerprint: report.trace_fingerprint,
        schedule_hash: report.trace_certificate.schedule_hash,
        virtual_elapsed_nanos: virtual_report.virtual_elapsed_nanos,
        virtual_clock_epoch_nanos: report.now_nanos,
        semantics,
        region_close: runtime_semantics.region_close,
        obligation_balance: runtime_semantics.obligation_balance,
        receipt,
    }
}

/// Runs the fixture once under the LIVE runtime: real clock, real scheduler,
/// ambient `Cx` installed by `block_on`. The two component futures are polled
/// jointly inside one task — the live side's concurrency shape is allowed to
/// differ from the lab's; only semantics must survive.
#[must_use]
pub fn run_fixture_live(cfg: &FixtureConfig, scratch_dir: &Path) -> LiveFixtureRun {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("live runtime builds");
    let (producer_fut, consumer_fut, trace) = fixture_futures(cfg, scratch_dir);
    runtime.block_on(async move {
        join2(producer_fut, consumer_fut).await;
    });
    let trace_bytes = trace.to_bytes();
    let semantics = trace.semantics();
    let receipt = FixtureRunReceipt::new(
        FixtureRuntime::Live,
        cfg.seed,
        None,
        &trace_bytes,
        trace.fault_events(),
    );
    LiveFixtureRun {
        semantics,
        // Returning from `join2` is the live adapter's direct witness that
        // both fixture children completed. The fixture creates no finalizers.
        region_close: capture_region_close(true, true),
        // This fixture creates no asupersync obligations. Keep the explicit
        // counters here so a future obligation-producing fixture must change
        // the witness rather than inheriting an unexplained constant.
        obligation_balance: capture_obligation_balance(0, 0, 0),
        receipt,
    }
}

/// Polls two independent futures to completion within one task. Local and
/// dependency-free on purpose: the foundation's `Join` combinator is a
/// region-spawning builder, which is more machinery than "run both halves of
/// the fixture in this task" needs.
async fn join2(
    a: impl std::future::Future<Output = ()> + Send,
    b: impl std::future::Future<Output = ()> + Send,
) {
    let mut a = Box::pin(a);
    let mut b = Box::pin(b);
    let mut a_done = false;
    let mut b_done = false;
    std::future::poll_fn(|cx| {
        if !a_done && a.as_mut().poll(cx).is_ready() {
            a_done = true;
        }
        if !b_done && b.as_mut().poll(cx).is_ready() {
            b_done = true;
        }
        if a_done && b_done {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
}

/// Where and how two same-seed runs first disagreed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DivergencePoint {
    /// Index of the run that diverged from run 0.
    pub run_index: usize,
    /// First differing byte in the serialized trace.
    pub byte_offset: usize,
    /// Event that byte falls inside, when it is in the event region.
    pub event_index: Option<usize>,
}

/// The two-runs-at-one-seed gate's complete, self-describing verdict.
#[derive(Debug)]
pub struct DeterminismVerdict {
    /// The seed every run used.
    pub seed: u64,
    /// How many runs were compared.
    pub runs: usize,
    /// Whether every run was byte-identical with matching fingerprints.
    pub passed: bool,
    /// First divergence against run 0, if any.
    pub first_divergence: Option<DivergencePoint>,
    /// Per-run Foata fingerprints.
    pub trace_fingerprints: Vec<u64>,
    /// Per-run schedule-certificate hashes.
    pub schedule_hashes: Vec<u64>,
    /// Per-run virtual elapsed nanoseconds.
    pub virtual_elapsed_nanos: Vec<u64>,
    /// Immutable receipt from every execution compared by this verdict.
    pub receipts: Vec<FixtureRunReceipt>,
    /// The verdict, reconstructable without re-running.
    pub log_lines: Vec<String>,
}

/// Runs the fixture `runs` times at one seed under the lab and compares every
/// run against run 0: serialized trace bytes, lab trace fingerprint, and
/// schedule hash all must match. Each run gets its own subdirectory of
/// `scratch_root` so the durable leg cannot cross-contaminate runs.
#[must_use]
pub fn determinism_gate(
    cfg: &FixtureConfig,
    scratch_root: &Path,
    runs: usize,
) -> DeterminismVerdict {
    assert!(runs >= 2, "a determinism gate needs at least two runs");
    let mut executions = Vec::with_capacity(runs);
    for run in 0..runs {
        let dir = scratch_root.join(format!("run-{run}"));
        executions.push(run_fixture_under_lab(cfg, &dir, LabConfig::new(cfg.seed)));
    }

    let mut log_lines = Vec::new();
    for (i, run) in executions.iter().enumerate() {
        log_lines.extend(run.receipt.log_lines());
        log_lines.push(format!(
            "determinism-gate seed={:#x} run={} trace_bytes={} fingerprint={:#x} schedule_hash={:#x} virtual_elapsed_ns={}",
            cfg.seed,
            i,
            run.trace_bytes.len(),
            run.trace_fingerprint,
            run.schedule_hash,
            run.virtual_elapsed_nanos,
        ));
    }

    let mut first = None;
    for (i, run) in executions.iter().enumerate().skip(1) {
        if let Some((byte_offset, event_index)) =
            first_divergence(&executions[0].trace_bytes, &run.trace_bytes)
        {
            first = Some(DivergencePoint {
                run_index: i,
                byte_offset,
                event_index,
            });
            log_lines.push(format!(
                "determinism-gate FAILED seed={:#x}: first diverging trace offset byte={byte_offset} event={event_index:?} (run 0 vs run {i})",
                cfg.seed,
            ));
            break;
        }
    }
    let fingerprints_agree = executions
        .iter()
        .all(|r| r.trace_fingerprint == executions[0].trace_fingerprint);
    let schedules_agree = executions
        .iter()
        .all(|r| r.schedule_hash == executions[0].schedule_hash);
    if first.is_none() && fingerprints_agree && schedules_agree {
        log_lines.push(format!(
            "determinism-gate PASSED seed={:#x}: {} runs byte-identical",
            cfg.seed, runs
        ));
    } else if first.is_none() {
        log_lines.push(format!(
            "determinism-gate FAILED seed={:#x}: traces byte-identical but lab reports disagree (fingerprints_agree={fingerprints_agree} schedules_agree={schedules_agree})",
            cfg.seed,
        ));
    }

    DeterminismVerdict {
        seed: cfg.seed,
        runs,
        passed: first.is_none() && fingerprints_agree && schedules_agree,
        first_divergence: first,
        trace_fingerprints: executions.iter().map(|r| r.trace_fingerprint).collect(),
        schedule_hashes: executions.iter().map(|r| r.schedule_hash).collect(),
        virtual_elapsed_nanos: executions.iter().map(|r| r.virtual_elapsed_nanos).collect(),
        receipts: executions.iter().map(|r| r.receipt.clone()).collect(),
        log_lines,
    }
}

/// Projects the fixture's semantics into the harness's normalized vocabulary.
/// The consumer digest rides `surface_result`; the counts ride exact-tolerance
/// counters; `chain_intact` is a counter so a broken network leg is a compared
/// value, not a silent assumption.
fn to_normalized(
    semantics: &FixtureSemantics,
    region_close: RegionCloseRecord,
    obligation_balance: ObligationBalanceRecord,
) -> NormalizedSemantics {
    let mut terminal = TerminalOutcome::ok();
    terminal.surface_result = Some(semantics.final_digest_hex.clone());
    NormalizedSemantics {
        terminal_outcome: terminal,
        cancellation: CancellationRecord::none(),
        loser_drain: LoserDrainRecord::not_applicable(),
        region_close,
        obligation_balance,
        resource_surface: ResourceSurfaceRecord::empty(SURFACE_SCOPE)
            .with_counter("records_produced", semantics.produced)
            .with_counter("records_consumed", semantics.consumed)
            .with_counter("durable_bytes", semantics.durable_bytes)
            .with_counter("injected_faults", semantics.injected_faults)
            .with_counter(
                "network_backpressure_events",
                semantics.network_backpressure_events,
            )
            .with_counter("chain_intact", i64::from(semantics.chain_intact)),
    }
}

/// The dual run's result plus its reconstructable log.
pub struct DualRunOutcome {
    /// The foundation harness's structured comparison.
    pub result: DualRunResult,
    /// Exact execution receipt produced by the lab side.
    pub lab_receipt: FixtureRunReceipt,
    /// Exact execution receipt produced by the live side.
    pub live_receipt: FixtureRunReceipt,
    /// Side-by-side digests and counters, reconstructable without re-running.
    pub log_lines: Vec<String>,
}

/// Runs the fixture under the lab AND under the live runtime at one seed via
/// asupersync's [`DualRunHarness`], comparing the clock-free semantics.
///
/// `live_seed_override` deliberately runs the live side at a different seed —
/// the mutation control proving the comparison can fail. Pass `None` for the
/// honest dual run.
#[must_use]
pub fn dual_run_fixture(
    cfg: &FixtureConfig,
    scratch_root: &Path,
    live_seed_override: Option<u64>,
) -> DualRunOutcome {
    let lab_cfg = cfg.clone();
    let lab_dir = scratch_root.join("lab");
    let live_cfg = cfg.clone();
    let live_dir = scratch_root.join("live");
    let lab_receipt_slot = Arc::new(Mutex::new(None));
    let live_receipt_slot = Arc::new(Mutex::new(None));
    let lab_receipt_capture = Arc::clone(&lab_receipt_slot);
    let live_receipt_capture = Arc::clone(&live_receipt_slot);

    let mut harness = DualRunHarness::phase1(
        "fgdb.sim.fixture.producer_consumer",
        SURFACE_SCOPE,
        "v1",
        "producer/consumer fixture over virtual time, lab VFS, and virtual TCP",
        cfg.seed,
    )
    .lab(move |config| {
        let run = run_fixture_under_lab(&lab_cfg, &lab_dir, config);
        *lab_receipt_capture
            .lock()
            .expect("dual-run lab receipt slot") = Some(run.receipt.clone());
        to_normalized(&run.semantics, run.region_close, run.obligation_balance)
    });
    if let Some(seed) = live_seed_override {
        harness = harness.live(move |_seed, _entropy| {
            let mut mutated = live_cfg.clone();
            mutated.seed = seed;
            let run = run_fixture_live(&mutated, &live_dir);
            *live_receipt_capture
                .lock()
                .expect("dual-run live receipt slot") = Some(run.receipt.clone());
            to_normalized(&run.semantics, run.region_close, run.obligation_balance)
        });
    } else {
        harness = harness.live(move |seed, _entropy| {
            let mut effective = live_cfg.clone();
            effective.seed = seed;
            let run = run_fixture_live(&effective, &live_dir);
            *live_receipt_capture
                .lock()
                .expect("dual-run live receipt slot") = Some(run.receipt.clone());
            to_normalized(&run.semantics, run.region_close, run.obligation_balance)
        });
    }
    let result = harness.run();
    let lab_receipt = lab_receipt_slot
        .lock()
        .expect("dual-run lab receipt slot")
        .take()
        .expect("dual-run harness executed its lab side");
    let live_receipt = live_receipt_slot
        .lock()
        .expect("dual-run live receipt slot")
        .take()
        .expect("dual-run harness executed its live side");

    let lab_digest = result
        .lab
        .semantics
        .terminal_outcome
        .surface_result
        .clone()
        .unwrap_or_default();
    let live_digest = result
        .live
        .semantics
        .terminal_outcome
        .surface_result
        .clone()
        .unwrap_or_default();
    let mut log_lines = lab_receipt.log_lines();
    log_lines.extend(live_receipt.log_lines());
    log_lines.extend([
        format!(
            "dual-run seed={:#x} lab_digest={lab_digest} live_digest={live_digest}",
            cfg.seed
        ),
        format!(
            "dual-run seed={:#x} lab_trace_digest={} live_trace_digest={}",
            cfg.seed,
            lab_receipt.trace_digest(),
            live_receipt.trace_digest(),
        ),
        format!(
            "dual-run seed={:#x} lab_counters={:?} live_counters={:?}",
            cfg.seed,
            result.lab.semantics.resource_surface.counters,
            result.live.semantics.resource_surface.counters
        ),
        format!(
            "dual-run seed={:#x} passed={} mismatches={}",
            cfg.seed,
            result.passed(),
            result.verdict.mismatches.len()
        ),
    ]);
    DualRunOutcome {
        result,
        lab_receipt,
        live_receipt,
        log_lines,
    }
}
