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

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use asupersync::Budget;
use asupersync::lab::{
    CancellationRecord, DualRunHarness, DualRunResult, LabConfig, LabRuntime, LoserDrainRecord,
    NormalizedSemantics, ObligationBalanceRecord, RegionCloseRecord, ResourceSurfaceRecord,
    TerminalOutcome, capture_obligation_balance, capture_region_close, normalize_lab_report,
};
use asupersync::runtime::RuntimeBuilder;
use asupersync::trace::replay::{CompactTaskId, ReplayEvent, ReplayTrace};
use fgdb_crypto::Hasher;

use crate::fixture::{
    FixtureConfig, FixtureSemantics, FixtureTaskError, FixtureTaskStage, FixtureWorkload,
    FixtureWorkloadDecodeLimits, FixtureWorkloadError, first_divergence,
    fixture_futures_for_workload,
};
use crate::vfs::{FaultEvent, FaultPlan};

/// Scope name for the fixture's semantic counters.
const SURFACE_SCOPE: &str = "fgdb.sim.fixture";

/// Stable scenario identity shared by the fixture's lab and live executions.
const FIXTURE_SCENARIO_ID: &str = "fgdb.sim.fixture.producer_consumer";

/// Environment variable carrying one strict exported-fixture replay value.
pub const FIXTURE_REPLAY_ENV: &str = "FGDB_SIM_FIXTURE_REPLAY";

/// Environment variable carrying the exact failure-execution seal expected
/// from [`FIXTURE_REPLAY_ENV`].
pub const FIXTURE_REPLAY_EXPECTED_DIGEST_ENV: &str = "FGDB_SIM_EXPECTED_FIXTURE_EXECUTION_DIGEST";

const FIXTURE_REPLAY_MAGIC: &str = "FGDBFIX1";
const MAX_FIXTURE_REPLAY_PLAN_BYTES: usize = 1_024;

/// Runtime posture that produced a fixture receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureRuntime {
    /// Deterministic asupersync lab runtime with a virtual clock.
    Lab,
    /// Live asupersync runtime with a wall clock and ordinary scheduler.
    Live,
}

/// Stable failure identity used by real fixture-workload minimization.
///
/// Action ordinals are deliberately excluded: deleting an irrelevant prefix
/// re-numbers canonical actions but does not turn a durable-write `StorageFull`
/// into a different bug. The complete [`FixtureTaskError`] remains available
/// on [`FixtureRunError`] for diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureFailureKind {
    /// Producer-side I/O failed at this operation/category.
    Producer {
        /// Operation being attempted.
        stage: FixtureTaskStage,
        /// Stable I/O category.
        kind: io::ErrorKind,
    },
    /// Consumer-side I/O failed at this operation/category.
    Consumer {
        /// Operation being attempted.
        stage: FixtureTaskStage,
        /// Stable I/O category.
        kind: io::ErrorKind,
    },
}

/// Immutable execution-root evidence for one typed fixture task failure.
///
/// A component error without this receipt is not a falsification: it would let
/// a stub return the expected `io::ErrorKind` without executing the workload,
/// fault plan, or LAB schedule. All fields are private and the digest binds the
/// exact completed evidence captured before the adapter returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureFailureEvidence {
    runtime: FixtureRuntime,
    seed: u64,
    scheduler_seed: Option<u64>,
    fault_plan: crate::vfs::FaultPlan,
    virtual_clock_epoch_nanos: Option<u64>,
    trace_digest: String,
    workload_digest: String,
    workload_bytes: Vec<u8>,
    lab_replay_trace_digest: Option<String>,
    task_dispatches: Option<Vec<TaskDispatchStep>>,
    injected_faults: Vec<FaultEvent>,
    task_error: FixtureTaskError,
    execution_digest: String,
}

/// A complete replay value for the exported producer/consumer fixture.
///
/// This replays the exact canonical workload under the exact fault plan and a
/// fresh [`LabConfig::new`] built from the recorded scheduler seed. It does
/// not claim to force the recorded task-dispatch schedule: the pinned
/// foundation exposes schedule recording but no public schedule-driving API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureReplay {
    workload: FixtureWorkload,
    fault_plan: FaultPlan,
    scheduler_seed: u64,
}

impl FixtureReplay {
    /// Constructs a replay from already-validated values.
    #[must_use]
    pub const fn new(
        workload: FixtureWorkload,
        fault_plan: FaultPlan,
        scheduler_seed: u64,
    ) -> Self {
        Self {
            workload,
            fault_plan,
            scheduler_seed,
        }
    }

    /// Exact canonical workload this replay executes.
    #[must_use]
    pub const fn workload(&self) -> &FixtureWorkload {
        &self.workload
    }

    /// Exact injected fault plan.
    #[must_use]
    pub const fn fault_plan(&self) -> FaultPlan {
        self.fault_plan
    }

    /// Seed supplied to the deterministic LAB scheduler.
    #[must_use]
    pub const fn scheduler_seed(&self) -> u64 {
        self.scheduler_seed
    }

    /// Strict, versioned, self-contained descriptor.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{FIXTURE_REPLAY_MAGIC}:{:#018x}:{}:{}",
            self.scheduler_seed,
            encode_hex(self.fault_plan.encode_replay_fields().as_bytes()),
            encode_hex(&self.workload.to_canonical_bytes()),
        )
    }

    /// Decodes one descriptor under caller-owned workload admission limits.
    ///
    /// # Errors
    ///
    /// Refuses malformed, non-canonical, oversized, or resource-unbounded
    /// descriptors before executing any fixture or touching its scratch path.
    pub fn parse(
        text: &str,
        limits: FixtureWorkloadDecodeLimits,
    ) -> Result<Self, FixtureReplayError> {
        let max_descriptor_bytes = limits
            .max_encoded_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(MAX_FIXTURE_REPLAY_PLAN_BYTES * 2 + 64))
            .ok_or(FixtureReplayError::DecodeLimitOverflow)?;
        if text.len() > max_descriptor_bytes {
            return Err(FixtureReplayError::DescriptorBytesExceeded {
                actual: text.len(),
                limit: max_descriptor_bytes,
            });
        }
        let mut fields = text.split(':');
        let magic = fields.next().ok_or(FixtureReplayError::WrongFieldCount)?;
        let scheduler_seed = fields.next().ok_or(FixtureReplayError::WrongFieldCount)?;
        let plan_hex = fields.next().ok_or(FixtureReplayError::WrongFieldCount)?;
        let workload_hex = fields.next().ok_or(FixtureReplayError::WrongFieldCount)?;
        if fields.next().is_some() {
            return Err(FixtureReplayError::WrongFieldCount);
        }
        if magic != FIXTURE_REPLAY_MAGIC {
            return Err(FixtureReplayError::WrongMagic);
        }
        let scheduler_seed = scheduler_seed
            .strip_prefix("0x")
            .ok_or(FixtureReplayError::InvalidSchedulerSeed)
            .and_then(|hex| {
                u64::from_str_radix(hex, 16).map_err(|_| FixtureReplayError::InvalidSchedulerSeed)
            })?;
        let plan_bytes = decode_hex(plan_hex, "fault plan", MAX_FIXTURE_REPLAY_PLAN_BYTES)?;
        let plan_text = core::str::from_utf8(&plan_bytes)
            .map_err(|_| FixtureReplayError::InvalidFaultPlanText)?;
        let fault_plan =
            FaultPlan::decode_replay_fields(plan_text).map_err(FixtureReplayError::FaultPlan)?;
        let workload_bytes = decode_hex(workload_hex, "workload", limits.max_encoded_bytes)?;
        let workload = FixtureWorkload::try_from_canonical_bytes(&workload_bytes, limits)
            .map_err(FixtureReplayError::Workload)?;
        let replay = Self::new(workload, fault_plan, scheduler_seed);
        if replay.encode() != text {
            return Err(FixtureReplayError::NonCanonical);
        }
        Ok(replay)
    }

    /// Executes this replay in a caller-selected isolated scratch directory.
    pub fn run(&self, scratch_dir: &Path) -> Result<LabFixtureRun, FixtureRunError> {
        let mut config = FixtureConfig::new(self.workload.seed());
        config.fault_plan = self.fault_plan;
        run_fixture_workload_under_lab(
            &config,
            &self.workload,
            scratch_dir,
            LabConfig::new(self.scheduler_seed),
        )
    }

    /// Renders the real fresh-process consumer command for this exact failure.
    ///
    /// # Errors
    ///
    /// Refuses an evidence receipt from a different runtime, workload, fault
    /// plan, or scheduler seed instead of emitting a plausible wrong command.
    pub fn command_for(
        &self,
        evidence: &FixtureFailureEvidence,
    ) -> Result<String, FixtureReplayError> {
        if evidence.runtime() != FixtureRuntime::Lab
            || evidence.scheduler_seed() != Some(self.scheduler_seed)
            || evidence.fault_plan() != self.fault_plan
            || !evidence.matches_workload(&self.workload)
        {
            return Err(FixtureReplayError::EvidenceMismatch);
        }
        Ok(format!(
            "{FIXTURE_REPLAY_ENV}={} {FIXTURE_REPLAY_EXPECTED_DIGEST_ENV}={} cargo test -p fgdb-sim --test sim_dual_run -- --ignored fixture_replay_from_env",
            self.encode(),
            evidence.execution_digest(),
        ))
    }
}

/// Why an exported-fixture replay descriptor was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixtureReplayError {
    WrongMagic,
    WrongFieldCount,
    InvalidSchedulerSeed,
    DecodeLimitOverflow,
    DescriptorBytesExceeded {
        actual: usize,
        limit: usize,
    },
    HexBytesExceeded {
        field: &'static str,
        actual: usize,
        limit: usize,
    },
    OddHexLength {
        field: &'static str,
    },
    InvalidHex {
        field: &'static str,
    },
    InvalidFaultPlanText,
    FaultPlan(String),
    Workload(FixtureWorkloadError),
    NonCanonical,
    EvidenceMismatch,
}

impl core::fmt::Display for FixtureReplayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::WrongMagic => f.write_str("wrong fixture replay magic/version"),
            Self::WrongFieldCount => f.write_str("wrong fixture replay field count"),
            Self::InvalidSchedulerSeed => f.write_str("invalid fixture replay scheduler seed"),
            Self::DecodeLimitOverflow => f.write_str("fixture replay decode limit overflow"),
            Self::DescriptorBytesExceeded { actual, limit } => {
                write!(f, "fixture replay bytes {actual} exceed limit {limit}")
            }
            Self::HexBytesExceeded {
                field,
                actual,
                limit,
            } => write!(
                f,
                "fixture replay {field} bytes {actual} exceed limit {limit}"
            ),
            Self::OddHexLength { field } => {
                write!(f, "fixture replay {field} has odd hex length")
            }
            Self::InvalidHex { field } => write!(f, "fixture replay {field} is not lowercase hex"),
            Self::InvalidFaultPlanText => f.write_str("fixture replay fault plan is not UTF-8"),
            Self::FaultPlan(error) => write!(f, "fixture replay fault plan refused: {error}"),
            Self::Workload(error) => write!(f, "fixture replay workload refused: {error}"),
            Self::NonCanonical => f.write_str("fixture replay descriptor is non-canonical"),
            Self::EvidenceMismatch => {
                f.write_str("fixture replay does not match the supplied failure evidence")
            }
        }
    }
}

impl std::error::Error for FixtureReplayError {}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_hex(
    text: &str,
    field: &'static str,
    max_bytes: usize,
) -> Result<Vec<u8>, FixtureReplayError> {
    if !text.len().is_multiple_of(2) {
        return Err(FixtureReplayError::OddHexLength { field });
    }
    let byte_count = text.len() / 2;
    if byte_count > max_bytes {
        return Err(FixtureReplayError::HexBytesExceeded {
            field,
            actual: byte_count,
            limit: max_bytes,
        });
    }
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(byte_count)
        .map_err(|_| FixtureReplayError::HexBytesExceeded {
            field,
            actual: byte_count,
            limit: max_bytes,
        })?;
    let (pairs, remainder) = text.as_bytes().as_chunks::<2>();
    debug_assert!(remainder.is_empty(), "odd hex was refused above");
    for pair in pairs {
        let high = decode_hex_digit(pair[0]).ok_or(FixtureReplayError::InvalidHex { field })?;
        let low = decode_hex_digit(pair[1]).ok_or(FixtureReplayError::InvalidHex { field })?;
        decoded.push((high << 4) | low);
    }
    Ok(decoded)
}

const fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

struct FixtureFailureCapture<'a> {
    runtime: FixtureRuntime,
    seed: u64,
    scheduler_seed: Option<u64>,
    fault_plan: crate::vfs::FaultPlan,
    virtual_clock_epoch_nanos: Option<u64>,
    trace_bytes: &'a [u8],
    workload: &'a FixtureWorkload,
    lab_replay_trace: Option<&'a ReplayTrace>,
    injected_faults: Vec<FaultEvent>,
}

impl FixtureFailureEvidence {
    fn new(capture: FixtureFailureCapture<'_>, task_error: FixtureTaskError) -> Self {
        let FixtureFailureCapture {
            runtime,
            seed,
            scheduler_seed,
            fault_plan,
            virtual_clock_epoch_nanos,
            trace_bytes,
            workload,
            lab_replay_trace,
            injected_faults,
        } = capture;
        let mut trace_hasher = Hasher::new();
        trace_hasher.update(b"fgdb.sim.fixture.trace.v1");
        trace_hasher.update(trace_bytes);
        let trace_digest = trace_hasher.finalize().to_hex();
        let workload_bytes = workload.to_canonical_bytes();
        let workload_digest = workload.canonical_digest_hex();
        let lab_replay_trace_digest = lab_replay_trace.map(replay_trace_digest);
        let task_dispatches = lab_replay_trace.map(task_dispatch_steps);
        let mut execution_hasher = Hasher::new();
        execution_hasher.update(b"fgdb.sim.fixture.failure-execution.v1");
        execution_hasher.update(runtime.as_str().as_bytes());
        execution_hasher.update(&seed.to_le_bytes());
        execution_hasher.update(&scheduler_seed.unwrap_or(u64::MAX).to_le_bytes());
        execution_hasher.update(fault_plan.encode_replay_fields().as_bytes());
        execution_hasher.update(&virtual_clock_epoch_nanos.unwrap_or(u64::MAX).to_le_bytes());
        execution_hasher.update(trace_digest.as_bytes());
        execution_hasher.update(&workload_bytes);
        if let Some(digest) = &lab_replay_trace_digest {
            execution_hasher.update(digest.as_bytes());
        }
        if let Some(dispatches) = &task_dispatches {
            for dispatch in dispatches {
                execution_hasher.update(&dispatch.task_id.to_le_bytes());
                execution_hasher.update(&dispatch.at_tick.to_le_bytes());
            }
        }
        for fault in &injected_faults {
            execution_hasher.update(format!("{fault:?}").as_bytes());
        }
        execution_hasher.update(format!("{task_error:?}").as_bytes());
        let execution_digest = execution_hasher.finalize().to_hex();
        Self {
            runtime,
            seed,
            scheduler_seed,
            fault_plan,
            virtual_clock_epoch_nanos,
            trace_digest,
            workload_digest,
            workload_bytes,
            lab_replay_trace_digest,
            task_dispatches,
            injected_faults,
            task_error,
            execution_digest,
        }
    }

    /// Runtime posture that observed the failure.
    #[must_use]
    pub const fn runtime(&self) -> FixtureRuntime {
        self.runtime
    }

    /// Seed bound by both the workload and runtime adapter.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// LAB scheduler seed, or `None` for a live execution.
    #[must_use]
    pub const fn scheduler_seed(&self) -> Option<u64> {
        self.scheduler_seed
    }

    /// Exact fault plan installed for this execution.
    #[must_use]
    pub const fn fault_plan(&self) -> crate::vfs::FaultPlan {
        self.fault_plan
    }

    /// Terminal LAB epoch, or `None` for a live execution.
    #[must_use]
    pub const fn virtual_clock_epoch_nanos(&self) -> Option<u64> {
        self.virtual_clock_epoch_nanos
    }

    /// Domain-separated digest of the component trace bytes.
    #[must_use]
    pub fn trace_digest(&self) -> &str {
        &self.trace_digest
    }

    /// Domain-separated digest of the exact workload bytes.
    #[must_use]
    pub fn workload_digest(&self) -> &str {
        &self.workload_digest
    }

    /// Whether the receipt binds exactly this canonical workload.
    #[must_use]
    pub fn matches_workload(&self, workload: &FixtureWorkload) -> bool {
        self.seed == workload.seed()
            && self.workload_digest == workload.canonical_digest_hex()
            && self.workload_bytes == workload.to_canonical_bytes()
    }

    /// LAB replay-trace digest, absent under the live runtime.
    #[must_use]
    pub fn lab_replay_trace_digest(&self) -> Option<&str> {
        self.lab_replay_trace_digest.as_deref()
    }

    /// Exact ordered LAB task dispatches, absent under the live runtime.
    #[must_use]
    pub fn task_dispatches(&self) -> Option<&[TaskDispatchStep]> {
        self.task_dispatches.as_deref()
    }

    /// Faults actually injected before the component returned its error.
    #[must_use]
    pub fn injected_faults(&self) -> &[FaultEvent] {
        &self.injected_faults
    }

    /// Typed component error bound by this receipt.
    #[must_use]
    pub const fn task_error(&self) -> FixtureTaskError {
        self.task_error
    }

    /// Seal over every retained execution-root field.
    #[must_use]
    pub fn execution_digest(&self) -> &str {
        &self.execution_digest
    }
}

/// Typed failure from one fixture adapter execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixtureRunError {
    /// The supplied workload was malformed or belonged to another seed.
    Workload(FixtureWorkloadError),
    /// The run's isolated scratch directory could not be created.
    ScratchIo(io::ErrorKind),
    /// The producer returned a typed I/O failure with exact execution evidence.
    Producer(Box<FixtureFailureEvidence>),
    /// The consumer returned a typed I/O failure with exact execution evidence.
    Consumer(Box<FixtureFailureEvidence>),
    /// A LAB task terminated without returning its typed output.
    LabTaskTerminated { component: &'static str },
    /// A LAB task could not be admitted to the runtime.
    LabTaskCreate { component: &'static str },
    /// A LAB task was still incomplete after the runtime stopped.
    LabTaskIncomplete { component: &'static str },
    /// The LAB runtime stopped before quiescence.
    LabNotQuiescent,
    /// The LAB runtime reported at least one invariant violation.
    LabInvariantViolation,
    /// The complete runtime replay trace was not available.
    MissingReplayTrace,
    /// The replay trace did not prove successful completion of both tasks.
    IncompleteReplayTrace,
    /// The replay trace scheduled a task outside the producer/consumer pair.
    UnexpectedTaskSet,
    /// The live runtime could not be constructed.
    LiveRuntimeBuild,
}

impl FixtureRunError {
    /// Failure identity suitable for same-bug shrink comparisons.
    #[must_use]
    pub const fn failure_kind(&self) -> Option<FixtureFailureKind> {
        match self {
            Self::Producer(evidence) => Some(FixtureFailureKind::Producer {
                stage: evidence.task_error().stage(),
                kind: evidence.task_error().kind(),
            }),
            Self::Consumer(evidence) => Some(FixtureFailureKind::Consumer {
                stage: evidence.task_error().stage(),
                kind: evidence.task_error().kind(),
            }),
            _ => None,
        }
    }

    /// Immutable execution receipt for a component failure.
    #[must_use]
    pub fn failure_evidence(&self) -> Option<&FixtureFailureEvidence> {
        match self {
            Self::Producer(evidence) | Self::Consumer(evidence) => Some(evidence.as_ref()),
            _ => None,
        }
    }
}

impl core::fmt::Display for FixtureRunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Workload(error) => write!(f, "fixture workload refused: {error}"),
            Self::ScratchIo(kind) => write!(f, "fixture scratch I/O failed: {kind:?}"),
            Self::Producer(evidence) => {
                write!(f, "fixture producer failed: {}", evidence.task_error())
            }
            Self::Consumer(evidence) => {
                write!(f, "fixture consumer failed: {}", evidence.task_error())
            }
            Self::LabTaskTerminated { component } => {
                write!(f, "fixture LAB {component} task terminated")
            }
            Self::LabTaskCreate { component } => {
                write!(f, "fixture LAB {component} task could not be created")
            }
            Self::LabTaskIncomplete { component } => {
                write!(f, "fixture LAB {component} task remained incomplete")
            }
            Self::LabNotQuiescent => f.write_str("fixture LAB run did not reach quiescence"),
            Self::LabInvariantViolation => {
                f.write_str("fixture LAB run reported an invariant violation")
            }
            Self::MissingReplayTrace => f.write_str("fixture LAB replay trace is missing"),
            Self::IncompleteReplayTrace => {
                f.write_str("fixture LAB replay trace lacks completed task dispatches")
            }
            Self::UnexpectedTaskSet => {
                f.write_str("fixture LAB replay trace contains an unexpected task set")
            }
            Self::LiveRuntimeBuild => f.write_str("fixture live runtime could not be built"),
        }
    }
}

impl std::error::Error for FixtureRunError {}

/// One task-dispatch decision retained from the lab runtime's replay trace.
///
/// This is evidence capture, not a schedule-control API. The pinned asupersync
/// runtime can record these decisions, but does not yet expose a public driver
/// that forces an edited decision stream back through [`LabRuntime`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskDispatchStep {
    task_id: u64,
    at_tick: u64,
}

impl TaskDispatchStep {
    #[must_use]
    pub const fn task_id(self) -> u64 {
        self.task_id
    }

    #[must_use]
    pub const fn at_tick(self) -> u64 {
        self.at_tick
    }
}

fn task_dispatch_steps(trace: &ReplayTrace) -> Vec<TaskDispatchStep> {
    trace
        .events
        .iter()
        .filter_map(|event| match event {
            ReplayEvent::TaskScheduled { task, at_tick } => Some(TaskDispatchStep {
                task_id: task.0,
                at_tick: *at_tick,
            }),
            _ => None,
        })
        .collect()
}

fn completed_task_dispatch_steps(
    trace: &ReplayTrace,
    expected_seed: u64,
) -> Option<Vec<TaskDispatchStep>> {
    if trace.metadata.seed != expected_seed {
        return None;
    }
    let dispatches = task_dispatch_steps(trace);
    if dispatches.is_empty() {
        return None;
    }
    let mut task_ids: Vec<u64> = dispatches.iter().map(|step| step.task_id).collect();
    task_ids.sort_unstable();
    task_ids.dedup();
    if task_ids.len() < 2 {
        return None;
    }
    let every_task_completed_after_its_last_dispatch = task_ids.into_iter().all(|task_id| {
        let last_dispatch = trace.events.iter().rposition(
            |event| matches!(event, ReplayEvent::TaskScheduled { task, .. } if task.0 == task_id),
        );
        last_dispatch.is_some_and(|last_dispatch| {
            trace.events.iter().skip(last_dispatch + 1).any(|event| {
                matches!(
                    event,
                    ReplayEvent::TaskCompleted { task, outcome: 0 } if task.0 == task_id
                )
            })
        })
    });
    every_task_completed_after_its_last_dispatch.then_some(dispatches)
}

fn replay_trace_digest(trace: &ReplayTrace) -> String {
    let bytes = trace
        .to_bytes()
        .expect("a completed in-memory ReplayTrace must serialize");
    let mut hasher = Hasher::new();
    hasher.update(b"fgdb.sim.fixture.lab-replay-trace.v1");
    hasher.update(&bytes);
    hasher.finalize().to_hex()
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
    scheduler_seed: Option<u64>,
    virtual_clock_epoch_nanos: Option<u64>,
    trace_digest: String,
    workload_digest: String,
    workload_bytes: Vec<u8>,
    workload_action_count: usize,
    lab_replay_trace_digest: Option<String>,
    task_dispatches: Option<Vec<TaskDispatchStep>>,
    injected_faults: Vec<FaultEvent>,
    artifact_fields_asserted: Vec<&'static str>,
    shrink_iterations: usize,
    final_reproducer_path: Option<PathBuf>,
}

impl FixtureRunReceipt {
    #[allow(clippy::too_many_arguments)]
    fn new(
        runtime: FixtureRuntime,
        seed: u64,
        scheduler_seed: Option<u64>,
        virtual_clock_epoch_nanos: Option<u64>,
        trace_bytes: &[u8],
        workload: &FixtureWorkload,
        lab_replay_trace: Option<&ReplayTrace>,
        injected_faults: Vec<FaultEvent>,
    ) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(b"fgdb.sim.fixture.trace.v1");
        hasher.update(trace_bytes);
        let workload_bytes = workload.to_canonical_bytes();
        Self {
            scenario_id: FIXTURE_SCENARIO_ID,
            runtime,
            seed,
            scheduler_seed,
            virtual_clock_epoch_nanos,
            trace_digest: hasher.finalize().to_hex(),
            workload_digest: workload.canonical_digest_hex(),
            workload_action_count: workload.actions().len(),
            workload_bytes,
            lab_replay_trace_digest: lab_replay_trace.map(replay_trace_digest),
            task_dispatches: lab_replay_trace.map(task_dispatch_steps),
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

    /// LAB scheduler seed recorded on the replay trace, or `None` for live.
    #[must_use]
    pub const fn scheduler_seed(&self) -> Option<u64> {
        self.scheduler_seed
    }

    #[must_use]
    pub const fn virtual_clock_epoch_nanos(&self) -> Option<u64> {
        self.virtual_clock_epoch_nanos
    }

    #[must_use]
    pub fn trace_digest(&self) -> &str {
        &self.trace_digest
    }

    /// Domain-separated digest of the exact canonical workload bytes.
    #[must_use]
    pub fn workload_digest(&self) -> &str {
        &self.workload_digest
    }

    /// Exact versioned workload bytes consumed by this execution.
    #[must_use]
    pub fn workload_bytes(&self) -> &[u8] {
        &self.workload_bytes
    }

    /// Number of explicit actions in the retained workload.
    #[must_use]
    pub const fn workload_action_count(&self) -> usize {
        self.workload_action_count
    }

    /// Verifies exact canonical workload identity, not only its seed.
    #[must_use]
    pub fn matches_workload(&self, workload: &FixtureWorkload) -> bool {
        let bytes = workload.to_canonical_bytes();
        self.seed == workload.seed()
            && self.workload_action_count == workload.actions().len()
            && self.workload_digest == workload.canonical_digest_hex()
            && self.workload_bytes == bytes
    }

    /// Domain-separated digest of the complete asupersync replay trace.
    ///
    /// Live runs return `None`: they have neither virtual time nor a lab
    /// scheduler trace and must not synthesize either.
    #[must_use]
    pub fn lab_replay_trace_digest(&self) -> Option<&str> {
        self.lab_replay_trace_digest.as_deref()
    }

    /// Exact ordered task-dispatch projection captured by the lab runtime.
    #[must_use]
    pub fn task_dispatches(&self) -> Option<&[TaskDispatchStep]> {
        self.task_dispatches.as_deref()
    }

    /// Verifies that this receipt was derived from exactly `trace`.
    ///
    /// This binds both the complete replay bytes and the human-readable
    /// dispatch projection. It deliberately does not claim the runtime can
    /// force-replay an edited trace.
    #[must_use]
    pub fn matches_lab_replay_trace(&self, trace: &ReplayTrace) -> bool {
        let digest = replay_trace_digest(trace);
        // asupersync records LabConfig.seed on the trace. That is the
        // scheduler seed, not the workload seed (fgdb-u95t).
        let Some(scheduler_seed) = self.scheduler_seed else {
            return false;
        };
        let Some(dispatches) = completed_task_dispatch_steps(trace, scheduler_seed) else {
            return false;
        };
        self.runtime == FixtureRuntime::Lab
            && self.lab_replay_trace_digest.as_deref() == Some(digest.as_str())
            && self.task_dispatches.as_deref() == Some(dispatches.as_slice())
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
            "fixture-run scenario_id={} runtime={} seed={:#x} virtual_clock_epoch_nanos={} trace_digest={} workload_digest={} workload_bytes={} workload_action_count={} lab_replay_trace_digest={} task_dispatch_count={}",
            self.scenario_id,
            self.runtime.as_str(),
            self.seed,
            virtual_epoch,
            self.trace_digest,
            self.workload_digest,
            self.workload_bytes.len(),
            self.workload_action_count,
            self.lab_replay_trace_digest
                .as_deref()
                .unwrap_or("not-applicable-live"),
            self.task_dispatches
                .as_deref()
                .map_or(0, |steps| steps.len()),
        )];
        if let Some(dispatches) = &self.task_dispatches {
            for (index, dispatch) in dispatches.iter().enumerate() {
                lines.push(format!(
                    "fixture-run runtime={} task_dispatch index={} task_id={} at_tick={}",
                    self.runtime.as_str(),
                    index,
                    dispatch.task_id,
                    dispatch.at_tick,
                ));
            }
        }
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
    /// Complete canonical asupersync record of runtime decisions and events.
    ///
    /// Capturing this is a prerequisite for future schedule minimization; the
    /// current adapter does not claim it can force an edited trace to replay.
    replay_trace: ReplayTrace,
    /// Exact versioned workload consumed by the producer.
    workload: FixtureWorkload,
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

impl LabFixtureRun {
    /// Exact complete replay trace captured from this lab execution.
    #[must_use]
    pub const fn replay_trace(&self) -> &ReplayTrace {
        &self.replay_trace
    }

    /// Exact immutable workload consumed by this execution.
    #[must_use]
    pub const fn workload(&self) -> &FixtureWorkload {
        &self.workload
    }
}

/// One completed live execution of the fixture.
pub struct LiveFixtureRun {
    /// Exact versioned workload consumed by the producer.
    workload: FixtureWorkload,
    /// Clock-free semantic projection.
    pub semantics: FixtureSemantics,
    /// Region-close evidence derived from joining both fixture futures.
    pub region_close: RegionCloseRecord,
    /// Explicit live-adapter counters for the fixture's zero-obligation scope.
    pub obligation_balance: ObligationBalanceRecord,
    /// Immutable facts retained from this exact execution.
    pub receipt: FixtureRunReceipt,
}

impl LiveFixtureRun {
    /// Exact immutable workload consumed by this execution.
    #[must_use]
    pub const fn workload(&self) -> &FixtureWorkload {
        &self.workload
    }
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
    lab_config: LabConfig,
) -> LabFixtureRun {
    let workload = FixtureWorkload::try_from_config(cfg)
        .expect("fixture configuration must materialize a bounded workload");
    run_fixture_workload_under_lab(cfg, &workload, scratch_dir, lab_config)
        .expect("generated fixture workload must match its configuration")
}

/// Runs one already-materialized workload under the lab runtime.
///
/// This is the execution seam future workload minimization can call. It does
/// not force an edited scheduler trace; schedule control remains a separate
/// foundation capability.
pub fn run_fixture_workload_under_lab(
    cfg: &FixtureConfig,
    workload: &FixtureWorkload,
    scratch_dir: &Path,
    mut lab_config: LabConfig,
) -> Result<LabFixtureRun, FixtureRunError> {
    let scheduler_seed = lab_config.seed;
    lab_config.auto_advance_time = true;
    // This adapter owns its evidence contract. A caller-supplied recorder may
    // filter or truncate events, so replace it with the foundation's complete,
    // unbounded default rather than accepting a plausible partial trace.
    lab_config = lab_config.with_default_replay_recording();
    let mut lab = LabRuntime::new(lab_config);
    let root = lab.state.create_root_region(Budget::INFINITE);
    let (producer_fut, consumer_fut, trace, workload) =
        fixture_futures_for_workload(cfg, workload.clone(), scratch_dir)
            .map_err(FixtureRunError::Workload)?;
    std::fs::create_dir_all(scratch_dir)
        .map_err(|error| FixtureRunError::ScratchIo(error.kind()))?;
    let (producer_task, mut producer_handle) = lab
        .state
        .create_task(root, Budget::INFINITE, producer_fut)
        .map_err(|_| FixtureRunError::LabTaskCreate {
            component: "producer",
        })?;
    let (consumer_task, mut consumer_handle) = lab
        .state
        .create_task(root, Budget::INFINITE, consumer_fut)
        .map_err(|_| FixtureRunError::LabTaskCreate {
            component: "consumer",
        })?;
    lab.scheduler.lock().schedule(producer_task, 0);
    lab.scheduler.lock().schedule(consumer_task, 0);
    let virtual_report = lab.run_with_auto_advance();
    let report = lab.report();
    let producer_result = producer_handle
        .try_join()
        .map_err(|_| FixtureRunError::LabTaskTerminated {
            component: "producer",
        })?
        .ok_or(FixtureRunError::LabTaskIncomplete {
            component: "producer",
        })?;
    let consumer_result = consumer_handle
        .try_join()
        .map_err(|_| FixtureRunError::LabTaskTerminated {
            component: "consumer",
        })?
        .ok_or(FixtureRunError::LabTaskIncomplete {
            component: "consumer",
        })?;
    if !report.quiescent {
        return Err(FixtureRunError::LabNotQuiescent);
    }
    if !report.invariant_violations.is_empty() {
        return Err(FixtureRunError::LabInvariantViolation);
    }
    let (runtime_semantics, _capture_manifest) = normalize_lab_report(&report, SURFACE_SCOPE);
    let replay_trace = lab
        .finish_replay_trace()
        .ok_or(FixtureRunError::MissingReplayTrace)?;
    let completed_dispatches = completed_task_dispatch_steps(&replay_trace, scheduler_seed)
        .ok_or(FixtureRunError::IncompleteReplayTrace)?;
    let mut observed_task_ids: Vec<u64> = completed_dispatches
        .iter()
        .map(|step| step.task_id)
        .collect();
    observed_task_ids.sort_unstable();
    observed_task_ids.dedup();
    let mut expected_task_ids = [
        CompactTaskId::from(producer_task).0,
        CompactTaskId::from(consumer_task).0,
    ];
    expected_task_ids.sort_unstable();
    if observed_task_ids != expected_task_ids {
        return Err(FixtureRunError::UnexpectedTaskSet);
    }
    let trace_bytes = trace.to_bytes();
    let injected_faults = trace.fault_events();
    if let Err(error) = producer_result {
        return Err(FixtureRunError::Producer(Box::new(
            FixtureFailureEvidence::new(
                FixtureFailureCapture {
                    runtime: FixtureRuntime::Lab,
                    seed: cfg.seed,
                    scheduler_seed: Some(scheduler_seed),
                    fault_plan: cfg.fault_plan,
                    virtual_clock_epoch_nanos: Some(report.now_nanos),
                    trace_bytes: &trace_bytes,
                    workload: &workload,
                    lab_replay_trace: Some(&replay_trace),
                    injected_faults,
                },
                error,
            ),
        )));
    }
    if let Err(error) = consumer_result {
        return Err(FixtureRunError::Consumer(Box::new(
            FixtureFailureEvidence::new(
                FixtureFailureCapture {
                    runtime: FixtureRuntime::Lab,
                    seed: cfg.seed,
                    scheduler_seed: Some(scheduler_seed),
                    fault_plan: cfg.fault_plan,
                    virtual_clock_epoch_nanos: Some(report.now_nanos),
                    trace_bytes: &trace_bytes,
                    workload: &workload,
                    lab_replay_trace: Some(&replay_trace),
                    injected_faults,
                },
                error,
            ),
        )));
    }
    let semantics = trace.semantics();
    let receipt = FixtureRunReceipt::new(
        FixtureRuntime::Lab,
        cfg.seed,
        Some(scheduler_seed),
        Some(report.now_nanos),
        &trace_bytes,
        &workload,
        Some(&replay_trace),
        injected_faults,
    );
    Ok(LabFixtureRun {
        trace_bytes,
        trace_fingerprint: report.trace_fingerprint,
        schedule_hash: report.trace_certificate.schedule_hash,
        replay_trace,
        workload,
        virtual_elapsed_nanos: virtual_report.virtual_elapsed_nanos,
        virtual_clock_epoch_nanos: report.now_nanos,
        semantics,
        region_close: runtime_semantics.region_close,
        obligation_balance: runtime_semantics.obligation_balance,
        receipt,
    })
}

/// Runs the fixture once under the LIVE runtime: real clock, real scheduler,
/// ambient `Cx` installed by `block_on`. The two component futures are polled
/// jointly inside one task — the live side's concurrency shape is allowed to
/// differ from the lab's; only semantics must survive.
#[must_use]
pub fn run_fixture_live(cfg: &FixtureConfig, scratch_dir: &Path) -> LiveFixtureRun {
    let workload = FixtureWorkload::try_from_config(cfg)
        .expect("fixture configuration must materialize a bounded workload");
    run_fixture_workload_live(cfg, &workload, scratch_dir)
        .expect("generated fixture workload must match its configuration")
}

/// Runs one already-materialized workload under the live runtime.
pub fn run_fixture_workload_live(
    cfg: &FixtureConfig,
    workload: &FixtureWorkload,
    scratch_dir: &Path,
) -> Result<LiveFixtureRun, FixtureRunError> {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .map_err(|_| FixtureRunError::LiveRuntimeBuild)?;
    let (producer_fut, consumer_fut, trace, workload) =
        fixture_futures_for_workload(cfg, workload.clone(), scratch_dir)
            .map_err(FixtureRunError::Workload)?;
    std::fs::create_dir_all(scratch_dir)
        .map_err(|error| FixtureRunError::ScratchIo(error.kind()))?;
    let (producer_result, consumer_result) =
        runtime.block_on(async move { join2(producer_fut, consumer_fut).await });
    let trace_bytes = trace.to_bytes();
    let injected_faults = trace.fault_events();
    if let Err(error) = producer_result {
        return Err(FixtureRunError::Producer(Box::new(
            FixtureFailureEvidence::new(
                FixtureFailureCapture {
                    runtime: FixtureRuntime::Live,
                    seed: cfg.seed,
                    scheduler_seed: None,
                    fault_plan: cfg.fault_plan,
                    virtual_clock_epoch_nanos: None,
                    trace_bytes: &trace_bytes,
                    workload: &workload,
                    lab_replay_trace: None,
                    injected_faults,
                },
                error,
            ),
        )));
    }
    if let Err(error) = consumer_result {
        return Err(FixtureRunError::Consumer(Box::new(
            FixtureFailureEvidence::new(
                FixtureFailureCapture {
                    runtime: FixtureRuntime::Live,
                    seed: cfg.seed,
                    scheduler_seed: None,
                    fault_plan: cfg.fault_plan,
                    virtual_clock_epoch_nanos: None,
                    trace_bytes: &trace_bytes,
                    workload: &workload,
                    lab_replay_trace: None,
                    injected_faults,
                },
                error,
            ),
        )));
    }
    let semantics = trace.semantics();
    let receipt = FixtureRunReceipt::new(
        FixtureRuntime::Live,
        cfg.seed,
        None,
        None,
        &trace_bytes,
        &workload,
        None,
        injected_faults,
    );
    Ok(LiveFixtureRun {
        workload,
        semantics,
        // Returning from `join2` is the live adapter's direct witness that
        // both fixture children completed. The fixture creates no finalizers.
        region_close: capture_region_close(true, true),
        // This fixture creates no asupersync obligations. Keep the explicit
        // counters here so a future obligation-producing fixture must change
        // the witness rather than inheriting an unexplained constant.
        obligation_balance: capture_obligation_balance(0, 0, 0),
        receipt,
    })
}

/// Polls two independent futures to completion within one task. Local and
/// dependency-free on purpose: the foundation's `Join` combinator is a
/// region-spawning builder, which is more machinery than "run both halves of
/// the fixture in this task" needs.
async fn join2(
    a: impl std::future::Future<Output = Result<(), FixtureTaskError>> + Send,
    b: impl std::future::Future<Output = Result<(), FixtureTaskError>> + Send,
) -> (Result<(), FixtureTaskError>, Result<(), FixtureTaskError>) {
    let mut a = Box::pin(a);
    let mut b = Box::pin(b);
    let mut a_output = None;
    let mut b_output = None;
    std::future::poll_fn(|cx| {
        if a_output.is_none()
            && let std::task::Poll::Ready(output) = a.as_mut().poll(cx)
        {
            a_output = Some(output);
        }
        if b_output.is_none()
            && let std::task::Poll::Ready(output) = b.as_mut().poll(cx)
        {
            b_output = Some(output);
        }
        match (a_output.take(), b_output.take()) {
            (Some(a), Some(b)) => std::task::Poll::Ready((a, b)),
            (a, b) => {
                a_output = a;
                b_output = b;
                std::task::Poll::Pending
            }
        }
    })
    .await
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

/// Project every failed comparison fact into structured log lines.
///
/// The foundation result remains the typed authority. This lossless textual
/// projection exists so a stored verdict can be reconstructed without
/// rerunning: a count alone cannot identify which value diverged.
#[must_use]
pub fn dual_run_verdict_log_lines(result: &DualRunResult) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, mismatch) in result.verdict.mismatches.iter().enumerate() {
        lines.push(format!(
            "dual-run mismatch index={index} field={:?} description={:?} lab_value={:?} live_value={:?}",
            mismatch.field, mismatch.description, mismatch.lab_value, mismatch.live_value,
        ));
    }
    for (runtime, violations) in [
        ("lab", &result.lab_invariant_violations),
        ("live", &result.live_invariant_violations),
    ] {
        for (index, violation) in violations.iter().enumerate() {
            lines.push(format!(
                "dual-run invariant_violation runtime={runtime} index={index} detail={violation:?}"
            ));
        }
    }
    lines
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
    log_lines.extend(dual_run_verdict_log_lines(&result));
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
