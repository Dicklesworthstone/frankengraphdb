//! Shrinking a failing replay to a minimal reproducer (plan §15.1).
//!
//! > "Failing runs shrink themselves. Hierarchical delta debugging + replay
//! > minimization reduce every failing seed (schedule + workload trace) to a
//! > minimal reproducer before filing, and divergence diagnostics explain
//! > exactly where a replay departed from its recording. Crashpacks arrive
//! > pre-shrunk; the bug report writes itself."
//!
//! That quotation is the target-state contract. The current reusable filing
//! pipeline minimizes a typed [`FaultPlan`] through [`shrink`]. The separate
//! [`shrink_schedule_and_workload`] primitive proves typed two-axis reduction.
//! [`shrink_fixture_workload_under_lab`] consumes the fixture's real typed LAB
//! verdict and minimizes canonical workload actions. The stricter
//! [`shrink_fixture_schedule_and_workload_under_lab`] derives deletion-only
//! authorities from the exact failed execution and executes both reduced axes
//! through asupersync's bounded forced-schedule candidate path. Its
//! [`FixtureScheduleWorkloadArtifact`] freezes the complete source authorities,
//! reconstructs both private candidates under caller limits, replays the exact
//! source and minimized executions, and publishes immutable bytes for a fresh
//! process. This is real two-axis minimization for the exported fixture, not a
//! universal scheduler shrinker or a production scheduler certificate.
//!
//! # The one law, and why it is a type and not a string comparison
//!
//! A shrinker searches for a smaller input that "still fails". Taken literally
//! that is wrong, and wrong in a way that is very hard to see afterwards: a
//! plan that loses acknowledged bytes can be minimised into one that merely
//! runs out of space, and the filed reproducer then describes a different bug
//! than the one that was found. The reduction looks like a success — it is
//! smaller, and it is red.
//!
//! So [`shrink`] accepts a candidate only when it fails with the **same
//! [`crate::artifact::FailureKind`]** as the original. The kind is carried on
//! [`Failure`] as a value precisely so this check cannot degrade into matching
//! on a message whose text contains a byte count.
//!
//! # What "smaller" means here
//!
//! The search space is the [`FaultPlan`], and the ordering is by *blame*: a
//! reproducer is better when it accuses fewer things. In descending order of
//! what a reader has to rule out:
//!
//! 1. **fault classes** — a plan naming three classes leaves a reader asking
//!    which one did it. Removing a class is the largest single reduction, so
//!    classes are dropped first, one at a time, greedily.
//! 2. **trigger strength** — `Always` fires on every eligible operation and
//!    `Nth(k)` fires periodically. Both can be narrowed to `At(k)`, which fires
//!    only at one exact eligible boundary. A reproducer that needs the fault
//!    once is sharper than one that needs it repeatedly.
//! 3. **space budget** — dropped when the failure does not need it.
//!
//! Each step is accepted only if the reduced plan still fails the same way, so
//! the result is *1-minimal*: no single further reduction in this lattice
//! preserves the failure. That is a real and checkable property, and it is
//! weaker than "globally minimal" — which delta debugging does not promise
//! either. Stated rather than implied.

use crate::artifact::{Failure, FailureKind, Replay, RunOutcome};
use crate::dual_run::{
    FIXTURE_FORCED_SCHEDULE_CANDIDATE_LIMITS, FIXTURE_FORCED_SCHEDULE_CAPTURE_LIMITS,
    FixtureFailureEvidence, FixtureFailureKind, FixtureReplay, FixtureReplayError, FixtureRunError,
    FixtureScheduleCandidate, FixtureScheduleCandidateRun, derive_fixture_schedule_candidate,
    run_fixture_schedule_workload_candidate, run_fixture_workload_under_forced_schedule,
    run_fixture_workload_under_lab,
};
use crate::fixture::{
    FixtureTaskStage, FixtureWorkload, FixtureWorkloadAction, FixtureWorkloadCandidate,
    FixtureWorkloadDecodeLimits, FixtureWorkloadError,
};
use crate::vfs::{FaultEvent, FaultPlan, Trigger};
use asupersync::lab::LabConfig;
use asupersync::lab::runtime::{
    ForcedSchedule, ForcedScheduleCandidateLimits, ForcedScheduleDecodeLimits, ForcedScheduleError,
};
use fgdb_crypto::Hasher;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn isolated_run(replay: Replay, root: &Path, ordinal: &mut usize) -> std::io::Result<RunOutcome> {
    let dir = root.join(format!("shrink-attempt-{ordinal:04}"));
    *ordinal += 1;
    std::fs::create_dir_all(&dir)?;
    Ok(replay.run(&dir))
}

/// One accepted reduction, in the order it was applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShrinkStep {
    /// What was reduced, for the filed report.
    pub what: &'static str,
    /// The plan after the reduction was accepted.
    pub plan: FaultPlan,
}

/// The result of minimising a failing replay.
#[derive(Clone, Debug)]
pub struct Shrunk {
    /// Exact replay whose failure initiated the shrink search.
    pub original_replay: Replay,
    /// Canonical digest of the sealed execution that initiated this search.
    /// This binds filing to the exact observed detail/events/epoch/artifact,
    /// not merely to a replay and coarse failure class.
    pub original_execution_digest: String,
    /// Exact typed failure produced by the initiating replay.
    pub original_failure: Failure,
    /// Normalized fault-event log produced by the initiating replay.
    pub original_events: Vec<FaultEvent>,
    /// The minimal replay that still fails the original way.
    pub replay: Replay,
    /// The failure it reproduces. Its [`crate::artifact::FailureKind`] equals the original's —
    /// that is the postcondition, asserted by [`shrink`] before returning.
    pub failure: Failure,
    /// Reductions accepted, in order. Empty means the input was already
    /// 1-minimal, which is a result, not a no-op.
    pub steps: Vec<ShrinkStep>,
    /// Candidates tried and rejected. Reported so a reader can tell "nothing
    /// was reducible" from "nothing was attempted" — the difference between a
    /// minimal reproducer and a broken shrinker.
    pub rejected: usize,
    /// Seal over the shrinker's complete returned provenance. Callers may
    /// inspect the public evidence, but filing refuses it after mutation.
    provenance_digest: String,
}

impl Shrunk {
    /// Whether every inspectable provenance field still matches the result
    /// emitted by [`shrink`].
    #[must_use]
    pub fn provenance_is_valid(&self) -> bool {
        self.provenance_digest
            == shrink_provenance_digest(
                OriginalExecution {
                    replay: self.original_replay,
                    digest: &self.original_execution_digest,
                    failure: &self.original_failure,
                    events: &self.original_events,
                },
                self.replay,
                &self.failure,
                &self.steps,
                self.rejected,
            )
    }
}

#[derive(Clone, Copy)]
struct OriginalExecution<'a> {
    replay: Replay,
    digest: &'a str,
    failure: &'a Failure,
    events: &'a [FaultEvent],
}

fn shrink_provenance_digest(
    original: OriginalExecution<'_>,
    replay: Replay,
    failure: &Failure,
    steps: &[ShrinkStep],
    rejected: usize,
) -> String {
    let mut hasher = Hasher::new();
    hasher.update(b"fgdb.sim.shrink.provenance.v1");
    hasher.update(original.replay.encode().as_bytes());
    hasher.update(original.digest.as_bytes());
    hasher.update(format!("{:?}", original.failure).as_bytes());
    for event in original.events {
        hasher.update(format!("{event:?}").as_bytes());
    }
    hasher.update(replay.encode().as_bytes());
    hasher.update(format!("{failure:?}").as_bytes());
    for step in steps {
        hasher.update(format!("{step:?}").as_bytes());
    }
    hasher.update(&rejected.to_le_bytes());
    hasher.finalize().to_hex()
}

/// Every candidate reduction of `plan`, strongest first.
///
/// Order matters: dropping a whole fault class removes more blame than
/// weakening a trigger, so it is offered first and the greedy loop takes it.
fn candidates(plan: FaultPlan) -> Vec<(&'static str, FaultPlan)> {
    let mut out = Vec::new();

    // 1. Drop a fault class outright.
    if plan.fsync_lie != Trigger::Never {
        out.push((
            "dropped the fsync-lie class",
            FaultPlan {
                fsync_lie: Trigger::Never,
                ..plan
            },
        ));
    }
    if plan.write_enospc != Trigger::Never {
        out.push((
            "dropped the write-ENOSPC class",
            FaultPlan {
                write_enospc: Trigger::Never,
                ..plan
            },
        ));
    }
    if plan.torn_write != Trigger::Never {
        out.push((
            "dropped the torn-write class",
            FaultPlan {
                torn_write: Trigger::Never,
                ..plan
            },
        ));
    }
    if plan.bit_flip != Trigger::Never {
        out.push((
            "dropped the bit-flip class",
            FaultPlan {
                bit_flip: Trigger::Never,
                ..plan
            },
        ));
    }
    if plan.dirent_lie != Trigger::Never {
        out.push((
            "dropped the dirent-lie class",
            FaultPlan {
                dirent_lie: Trigger::Never,
                ..plan
            },
        ));
    }
    if plan.dirent_loss != Trigger::Never {
        out.push((
            "dropped the dirent-loss class",
            FaultPlan {
                dirent_loss: Trigger::Never,
                ..plan
            },
        ));
    }
    if plan.latency != Trigger::Never {
        out.push((
            "dropped the latency class",
            FaultPlan {
                latency: Trigger::Never,
                latency_micros: 0,
                ..plan
            },
        ));
    }

    // 2. Drop the space budget.
    if plan.space_budget.is_some() {
        out.push((
            "dropped the space budget",
            FaultPlan {
                space_budget: None,
                ..plan
            },
        ));
    }

    // 3. Weaken a surviving repeated trigger to one exact firing. `Nth(1)` is
    // behaviorally the same as `Always`, so treating it as "once" would record
    // a cosmetic reduction while leaving every later boundary faulted.
    if let Some(trigger) = weaken_to_single_firing(plan.fsync_lie) {
        out.push((
            "weakened the fsync lie to fire once",
            FaultPlan {
                fsync_lie: trigger,
                ..plan
            },
        ));
    }
    if let Some(trigger) = weaken_to_single_firing(plan.write_enospc) {
        out.push((
            "weakened write ENOSPC to fire once",
            FaultPlan {
                write_enospc: trigger,
                ..plan
            },
        ));
    }
    if let Some(trigger) = weaken_to_single_firing(plan.torn_write) {
        out.push((
            "weakened the torn write to fire once",
            FaultPlan {
                torn_write: trigger,
                ..plan
            },
        ));
    }
    if let Some(trigger) = weaken_to_single_firing(plan.bit_flip) {
        out.push((
            "weakened the bit flip to fire once",
            FaultPlan {
                bit_flip: trigger,
                ..plan
            },
        ));
    }
    if let Some(trigger) = weaken_to_single_firing(plan.dirent_lie) {
        out.push((
            "weakened the dirent lie to fire once",
            FaultPlan {
                dirent_lie: trigger,
                ..plan
            },
        ));
    }
    if let Some(trigger) = weaken_to_single_firing(plan.dirent_loss) {
        out.push((
            "weakened the dirent loss to fire once",
            FaultPlan {
                dirent_loss: trigger,
                ..plan
            },
        ));
    }
    if let Some(trigger) = weaken_to_single_firing(plan.latency) {
        out.push((
            "weakened the latency to fire once",
            FaultPlan {
                latency: trigger,
                ..plan
            },
        ));
    }

    out
}

fn weaken_to_single_firing(trigger: Trigger) -> Option<Trigger> {
    match trigger {
        Trigger::Always => Some(Trigger::At(1)),
        Trigger::Nth(n) if n != 0 => Some(Trigger::At(n)),
        Trigger::Never | Trigger::Nth(_) | Trigger::At(_) | Trigger::PerMille(_) => None,
    }
}

/// Minimises `replay` to a 1-minimal reproducer of the *same* failure kind.
///
/// Returns `Ok(None)` when `replay` does not fail at all — there is nothing to
/// shrink, and returning a "minimal" reproducer of a passing run would be a
/// fabricated report.
///
/// Every scenario run is deterministic ([`Replay::run`]), so this search is
/// reproducible: the same input yields the same `Shrunk`.
///
/// # Errors
///
/// Returns the filesystem error when an isolated attempt directory cannot be
/// created. A workspace failure is not reported as either a passing replay or
/// a minimal reproducer.
pub fn shrink(replay: Replay, dir: &Path) -> std::io::Result<Option<Shrunk>> {
    let mut ordinal = 0usize;
    let original_run = isolated_run(replay, dir, &mut ordinal)?;
    shrink_observed_from_ordinal(original_run, dir, ordinal)
}

/// Minimize the exact sealed execution already observed by a campaign.
///
/// Unlike [`shrink`], this entrypoint does not rerun the source replay before
/// reduction. The supplied execution is the shrink lineage root, so a valid
/// same-replay/same-kind execution with different events, epoch, or artifact
/// cannot be silently replaced by a later rerun.
pub fn shrink_observed(original_run: RunOutcome, dir: &Path) -> std::io::Result<Option<Shrunk>> {
    shrink_observed_from_ordinal(original_run, dir, 0)
}

fn shrink_observed_from_ordinal(
    original_run: RunOutcome,
    dir: &Path,
    mut ordinal: usize,
) -> std::io::Result<Option<Shrunk>> {
    let Some(original_execution_digest) = original_run.replay_completeness_digest() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "shrink source execution seal is invalid",
        ));
    };
    let replay = original_run.replay();
    let Some(original) = original_run.failure else {
        return Ok(None);
    };
    let original_events = original_run.events;
    let target = original.kind;

    let mut best = replay;
    let mut failure = original.clone();
    let mut steps = Vec::new();
    let mut rejected = 0usize;

    // Greedy descent: re-offer the full candidate set after every accepted
    // reduction, because dropping one class can make another newly droppable.
    // Terminates because every candidate is strictly smaller in the lattice
    // above and the lattice is finite.
    'descent: loop {
        for (what, plan) in candidates(best.plan) {
            let candidate = Replay { plan, ..best };
            match isolated_run(candidate, dir, &mut ordinal)?.failure {
                // THE LAW: same kind, or it is a different bug and the
                // reduction is rejected however much smaller it looks.
                Some(next) if next.kind == target => {
                    best = candidate;
                    failure = next;
                    steps.push(ShrinkStep { what, plan });
                    continue 'descent;
                }
                _ => rejected += 1,
            }
        }
        break;
    }

    debug_assert_eq!(
        failure.kind, target,
        "shrink returned a different failure kind than it was given"
    );
    let provenance_digest = shrink_provenance_digest(
        OriginalExecution {
            replay,
            digest: &original_execution_digest,
            failure: &original,
            events: &original_events,
        },
        best,
        &failure,
        &steps,
        rejected,
    );
    Ok(Some(Shrunk {
        original_replay: replay,
        original_execution_digest,
        original_failure: original,
        original_events,
        replay: best,
        failure,
        steps,
        rejected,
        provenance_digest,
    }))
}

// ---------------------------------------------------------------------------
// Divergence diagnostics
// ---------------------------------------------------------------------------

/// Where a replay stopped matching its recording.
///
/// §15.1 asks for the *second* half of the replay-minimisation bullet:
/// "divergence diagnostics explain exactly where a replay departed from its
/// recording". An equality assertion says only *that* two runs differ, which
/// is the least useful moment to be told nothing — a determinism failure is
/// precisely the case where a reader cannot guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    /// Index of the first event that does not match, 0-based.
    pub index: usize,
    /// What the recording had there. `None` means the recording ended first.
    pub recorded: Option<FaultEvent>,
    /// What the replay produced there. `None` means the replay ended first.
    pub replayed: Option<FaultEvent>,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "fault log diverged at index {}: ", self.index)?;
        match (&self.recorded, &self.replayed) {
            (Some(recorded), Some(replayed)) => write!(
                f,
                "recording had {:?}, replay produced {:?}",
                recorded.kind, replayed.kind
            ),
            (Some(recorded), None) => write!(
                f,
                "replay ended early; recording still had {:?}",
                recorded.kind
            ),
            (None, Some(replayed)) => write!(
                f,
                "replay ran past the recording and produced {:?}",
                replayed.kind
            ),
            // Unreachable by construction: `diverge` returns None when both
            // sides are exhausted. Rendered rather than panicked, because a
            // diagnostic that aborts while explaining a failure is worse than
            // one that prints something odd.
            (None, None) => write!(f, "both logs ended (no divergence)"),
        }
    }
}

/// The first index at which `replayed` departs from `recorded`, if any.
///
/// A length difference is a divergence at the first missing index, not a
/// separate kind of result: "the replay stopped after three faults" and "the
/// replay produced a different third fault" are the same question asked of
/// index 3, and a caller should not have to handle them differently.
#[must_use]
pub fn diverge(recorded: &[FaultEvent], replayed: &[FaultEvent]) -> Option<Divergence> {
    for index in 0..recorded.len().max(replayed.len()) {
        let left = recorded.get(index);
        let right = replayed.get(index);
        if left != right {
            return Some(Divergence {
                index,
                recorded: left.cloned(),
                replayed: right.cloned(),
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Hierarchical schedule + workload minimization
// ---------------------------------------------------------------------------

/// Result of minimizing both the scheduled decisions and workload actions of
/// a failing replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HierarchicalShrunk<S, W> {
    /// Minimal retained schedule under the supplied failure predicate.
    pub schedule: Vec<S>,
    /// Minimal retained workload under the supplied failure predicate.
    pub workload: Vec<W>,
    /// Candidate replays executed, including the initial premise check.
    pub attempts: usize,
    /// Reductions accepted across both axes.
    pub accepted: usize,
    /// Smaller candidates that stayed red but changed the typed failure.
    pub rejected_different_failure: usize,
}

/// Typed verdict for one hierarchical replay candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShrinkTrial<K = FailureKind> {
    /// The candidate reproduced the exact target failure.
    Reproduced,
    /// The candidate no longer failed.
    DidNotReproduce,
    /// The candidate remained red, but it is a different bug.
    DifferentFailure(K),
}

fn reduce_axis<S, W, K, F>(
    schedule: &mut Vec<S>,
    workload: &mut Vec<W>,
    reduce_schedule: bool,
    attempts: &mut usize,
    accepted: &mut usize,
    rejected_different_failure: &mut usize,
    reproduces: &mut F,
) -> bool
where
    S: Clone,
    W: Clone,
    F: FnMut(&[S], &[W]) -> ShrinkTrial<K>,
{
    let mut changed = false;
    let mut granularity = 2usize;
    loop {
        let length = if reduce_schedule {
            schedule.len()
        } else {
            workload.len()
        };
        if length == 0 {
            break;
        }
        let chunk = length.div_ceil(granularity);
        let mut removed = false;
        for start in (0..length).step_by(chunk) {
            let end = (start + chunk).min(length);
            let mut candidate_schedule = schedule.clone();
            let mut candidate_workload = workload.clone();
            if reduce_schedule {
                candidate_schedule.drain(start..end);
            } else {
                candidate_workload.drain(start..end);
            }
            *attempts += 1;
            match reproduces(&candidate_schedule, &candidate_workload) {
                ShrinkTrial::Reproduced => {
                    *schedule = candidate_schedule;
                    *workload = candidate_workload;
                    *accepted += 1;
                    changed = true;
                    removed = true;
                    granularity = granularity.saturating_sub(1).max(2);
                    break;
                }
                ShrinkTrial::DifferentFailure(_) => *rejected_different_failure += 1,
                ShrinkTrial::DidNotReproduce => {}
            }
        }
        if removed {
            continue;
        }
        if granularity >= length {
            break;
        }
        granularity = (granularity * 2).min(length);
    }
    changed
}

/// Delta-debug a failing replay across its schedule and workload axes.
///
/// The caller supplies a real typed replay verdict. `None` means the input
/// itself did not reproduce the target failure, so no minimized artifact may
/// be filed. A smaller input that fails differently is counted and rejected;
/// it can never be mistaken for progress. The two axes are revisited until
/// neither can shrink: reducing a workload can expose a schedule reduction
/// and vice versa.
#[must_use]
pub fn shrink_schedule_and_workload<S, W, K, F>(
    schedule: Vec<S>,
    workload: Vec<W>,
    mut reproduces: F,
) -> Option<HierarchicalShrunk<S, W>>
where
    S: Clone,
    W: Clone,
    F: FnMut(&[S], &[W]) -> ShrinkTrial<K>,
{
    let mut attempts = 1usize;
    if !matches!(reproduces(&schedule, &workload), ShrinkTrial::Reproduced) {
        return None;
    }
    let mut schedule = schedule;
    let mut workload = workload;
    let mut accepted = 0usize;
    let mut rejected_different_failure = 0usize;
    loop {
        let schedule_changed = reduce_axis(
            &mut schedule,
            &mut workload,
            true,
            &mut attempts,
            &mut accepted,
            &mut rejected_different_failure,
            &mut reproduces,
        );
        let workload_changed = reduce_axis(
            &mut schedule,
            &mut workload,
            false,
            &mut attempts,
            &mut accepted,
            &mut rejected_different_failure,
            &mut reproduces,
        );
        if !schedule_changed && !workload_changed {
            break;
        }
    }
    Some(HierarchicalShrunk {
        schedule,
        workload,
        attempts,
        accepted,
        rejected_different_failure,
    })
}

/// A real LAB fixture workload reduced to one-minimal actions for the same
/// typed component failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureWorkloadShrunk {
    original_workload_digest: String,
    original_execution_digest: String,
    replay: FixtureReplay,
    minimal_evidence: FixtureFailureEvidence,
    failure: FixtureFailureKind,
    attempts: usize,
    accepted: usize,
    rejected_different_failure: usize,
}

impl FixtureWorkloadShrunk {
    /// Digest of the complete workload that first reproduced the failure.
    #[must_use]
    pub fn original_workload_digest(&self) -> &str {
        &self.original_workload_digest
    }

    /// Execution-root seal of the complete workload's observed failure.
    #[must_use]
    pub fn original_execution_digest(&self) -> &str {
        &self.original_execution_digest
    }

    /// Canonical one-minimal workload.
    #[must_use]
    pub const fn workload(&self) -> &FixtureWorkload {
        self.replay.workload()
    }

    /// Self-contained replay value for the retained one-minimal workload.
    #[must_use]
    pub const fn replay(&self) -> &FixtureReplay {
        &self.replay
    }

    /// Execution-root seal of the retained one-minimal failure.
    #[must_use]
    pub fn minimal_execution_digest(&self) -> &str {
        self.minimal_evidence.execution_digest()
    }

    /// Immutable LAB evidence emitted by the retained one-minimal failure.
    #[must_use]
    pub const fn minimal_evidence(&self) -> &FixtureFailureEvidence {
        &self.minimal_evidence
    }

    /// Real fresh-process replay command bound to the minimized execution.
    ///
    /// # Errors
    ///
    /// Refuses if any replay/evidence identity diverged after construction.
    pub fn replay_command(&self) -> Result<String, FixtureReplayError> {
        self.replay.command_for(&self.minimal_evidence)
    }

    /// Exact component/operation/I/O category retained by every reduction.
    #[must_use]
    pub const fn failure(&self) -> FixtureFailureKind {
        self.failure
    }

    /// Candidate executions, including the original premise run.
    #[must_use]
    pub const fn attempts(&self) -> usize {
        self.attempts
    }

    /// Workload reductions accepted.
    #[must_use]
    pub const fn accepted(&self) -> usize {
        self.accepted
    }

    /// Smaller workloads refused because they failed differently.
    #[must_use]
    pub const fn rejected_different_failure(&self) -> usize {
        self.rejected_different_failure
    }
}

/// Infrastructure error that prevents an honest fixture shrink verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixtureWorkloadShrinkError {
    /// An isolated attempt directory could not be created.
    AttemptIo(io::ErrorKind),
    /// Rebuilding a retained canonical action sequence failed.
    Workload(FixtureWorkloadError),
    /// The runtime failed without a component I/O identity suitable for
    /// same-bug comparison.
    Harness(FixtureRunError),
    /// The reducer reported a reproduction without retaining the typed
    /// component failure identity.
    MissingFailureIdentity,
}

impl core::fmt::Display for FixtureWorkloadShrinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AttemptIo(kind) => write!(f, "fixture shrink attempt I/O failed: {kind:?}"),
            Self::Workload(error) => write!(f, "fixture shrink workload refused: {error}"),
            Self::Harness(error) => write!(f, "fixture shrink harness failed: {error}"),
            Self::MissingFailureIdentity => {
                f.write_str("fixture shrink reproduced without a typed failure identity")
            }
        }
    }
}

impl std::error::Error for FixtureWorkloadShrinkError {}

/// Minimize the canonical action sequence of one actually failing LAB fixture.
///
/// Each candidate executes in a fresh directory through
/// [`run_fixture_workload_under_lab`]. A reduction is accepted only if the
/// producer/consumer operation and stable `io::ErrorKind` equal the original
/// failure. Passing candidates and different failures are never filed as the
/// reproducer.
///
/// The LAB runtime chooses a deterministic schedule for every candidate. This
/// function does **not** edit or force the retained `TaskScheduled` stream; it
/// closes the workload axis only.
pub fn shrink_fixture_workload_under_lab(
    cfg: &crate::fixture::FixtureConfig,
    workload: &FixtureWorkload,
    scratch_root: &Path,
    scheduler_seed: u64,
) -> Result<Option<FixtureWorkloadShrunk>, FixtureWorkloadShrinkError> {
    let original_workload_digest = workload.canonical_digest_hex();
    let mut target = None;
    let mut original_execution_digest = None;
    let mut minimal_evidence = None;
    let mut fatal = None;
    let mut ordinal = 0usize;
    let shrunk = {
        let mut reproduces = |_: &[()], retained: &[FixtureWorkloadAction]| {
            if fatal.is_some() {
                return ShrinkTrial::DidNotReproduce;
            }
            let candidate = match FixtureWorkload::try_from_retained_actions(cfg.seed, retained) {
                Ok(candidate) => candidate,
                Err(error) => {
                    fatal = Some(FixtureWorkloadShrinkError::Workload(error));
                    return ShrinkTrial::DidNotReproduce;
                }
            };
            let attempt = scratch_root.join(format!("fixture-workload-attempt-{ordinal:04}"));
            ordinal += 1;
            if let Err(error) = std::fs::create_dir_all(&attempt) {
                fatal = Some(FixtureWorkloadShrinkError::AttemptIo(error.kind()));
                return ShrinkTrial::DidNotReproduce;
            }
            match run_fixture_workload_under_lab(
                cfg,
                &candidate,
                &attempt,
                asupersync::lab::LabConfig::new(scheduler_seed),
            ) {
                Ok(_) => ShrinkTrial::DidNotReproduce,
                Err(error) => {
                    let evidence = error.failure_evidence().cloned();
                    match error.failure_kind() {
                        Some(kind) if target.is_none() || target == Some(kind) => {
                            let Some(evidence) = evidence else {
                                fatal = Some(FixtureWorkloadShrinkError::Harness(error));
                                return ShrinkTrial::DidNotReproduce;
                            };
                            if original_execution_digest.is_none() {
                                original_execution_digest =
                                    Some(evidence.execution_digest().to_string());
                            }
                            minimal_evidence = Some(evidence);
                            target.get_or_insert(kind);
                            ShrinkTrial::Reproduced
                        }
                        Some(kind) => ShrinkTrial::DifferentFailure(kind),
                        None => {
                            fatal = Some(FixtureWorkloadShrinkError::Harness(error));
                            ShrinkTrial::DidNotReproduce
                        }
                    }
                }
            }
        };
        shrink_schedule_and_workload(
            Vec::<()>::new(),
            workload.actions().to_vec(),
            &mut reproduces,
        )
    };
    if let Some(error) = fatal {
        return Err(error);
    }
    let Some(shrunk) = shrunk else {
        return Ok(None);
    };
    let Some(failure) = target else {
        return Err(FixtureWorkloadShrinkError::MissingFailureIdentity);
    };
    let (Some(original_execution_digest), Some(minimal_evidence)) =
        (original_execution_digest, minimal_evidence)
    else {
        return Err(FixtureWorkloadShrinkError::MissingFailureIdentity);
    };
    let workload = FixtureWorkload::try_from_retained_actions(cfg.seed, &shrunk.workload)
        .map_err(FixtureWorkloadShrinkError::Workload)?;
    Ok(Some(FixtureWorkloadShrunk {
        original_workload_digest,
        original_execution_digest,
        replay: FixtureReplay::new(workload, cfg.fault_plan, scheduler_seed),
        minimal_evidence,
        failure,
        attempts: shrunk.attempts,
        accepted: shrunk.accepted,
        rejected_different_failure: shrunk.rejected_different_failure,
    }))
}

/// One real exported-fixture failure minimized across both its recorded LAB
/// dispatch schedule and canonical workload actions.
#[derive(Debug)]
pub struct FixtureScheduleWorkloadShrunk {
    original_evidence: FixtureFailureEvidence,
    schedule_candidate: FixtureScheduleCandidate,
    workload_candidate: FixtureWorkloadCandidate,
    minimal_run: FixtureScheduleCandidateRun,
    failure: FixtureFailureKind,
    attempts: usize,
    accepted: usize,
    rejected_different_failure: usize,
}

impl FixtureScheduleWorkloadShrunk {
    /// Execution-root seal of the complete source failure.
    #[must_use]
    pub fn original_execution_digest(&self) -> &str {
        self.original_evidence.execution_digest()
    }

    /// Immutable evidence captured from the complete failing source run.
    #[must_use]
    pub const fn original_evidence(&self) -> &FixtureFailureEvidence {
        &self.original_evidence
    }

    /// One-minimal deletion-only scheduler authority.
    #[must_use]
    pub const fn schedule_candidate(&self) -> &FixtureScheduleCandidate {
        &self.schedule_candidate
    }

    /// One-minimal deletion-only workload authority.
    #[must_use]
    pub const fn workload_candidate(&self) -> &FixtureWorkloadCandidate {
        &self.workload_candidate
    }

    /// Exact observed boundary of the minimized candidate execution.
    #[must_use]
    pub const fn minimal_run(&self) -> &FixtureScheduleCandidateRun {
        &self.minimal_run
    }

    /// Seal over the minimized authorities and observed execution boundary.
    #[must_use]
    pub fn minimal_execution_digest(&self) -> &str {
        self.minimal_run.execution_digest()
    }

    /// Exact component/operation/I/O category retained by every reduction.
    #[must_use]
    pub const fn failure(&self) -> FixtureFailureKind {
        self.failure
    }

    /// Runtime executions, including source capture and the premise candidate.
    #[must_use]
    pub const fn attempts(&self) -> usize {
        self.attempts
    }

    /// Reductions accepted across both axes.
    #[must_use]
    pub const fn accepted(&self) -> usize {
        self.accepted
    }

    /// Smaller candidates rejected because they failed differently.
    #[must_use]
    pub const fn rejected_different_failure(&self) -> usize {
        self.rejected_different_failure
    }
}

const FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_MAGIC: &[u8; 8] = b"FGDBFSW\0";
const FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_VERSION: u32 = 1;
const FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_CHECKSUM_BYTES: usize = 32;
const FIXTURE_SCHEDULE_WORKLOAD_DIGEST_HEX_BYTES: usize = 64;
const FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_DOMAIN: &[u8] =
    b"fgdb.sim.fixture.schedule-workload-artifact.v1";
static FIXTURE_SCHEDULE_WORKLOAD_PUBLICATION_ORDINAL: AtomicU64 = AtomicU64::new(0);

/// Path to a strict two-axis artifact consumed by the ignored fresh-process
/// replay entrypoint in `tests/sim_dual_run.rs`.
pub const FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_ENV: &str =
    "FGDB_SIM_FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT";
/// Expected source-execution root for fresh-process replay.
pub const FIXTURE_SCHEDULE_WORKLOAD_SOURCE_DIGEST_ENV: &str =
    "FGDB_SIM_FIXTURE_SCHEDULE_WORKLOAD_SOURCE_DIGEST";
/// Expected minimized-execution root for fresh-process replay.
pub const FIXTURE_SCHEDULE_WORKLOAD_MINIMAL_DIGEST_ENV: &str =
    "FGDB_SIM_FIXTURE_SCHEDULE_WORKLOAD_MINIMAL_DIGEST";

/// Caller-owned admission for one persisted two-axis replay artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureScheduleWorkloadArtifactLimits {
    /// Maximum complete artifact bytes admitted from storage.
    pub max_encoded_bytes: usize,
    /// Foundation-owned schedule decoder limits.
    pub schedule: ForcedScheduleDecodeLimits,
    /// Canonical fixture-workload decoder limits.
    pub workload: FixtureWorkloadDecodeLimits,
    /// Candidate derivation and execution work limits.
    pub candidate: ForcedScheduleCandidateLimits,
    /// Maximum retained scheduler indices.
    pub max_schedule_indices: usize,
    /// Maximum retained workload indices.
    pub max_workload_indices: usize,
}

impl Default for FixtureScheduleWorkloadArtifactLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 2 * 1024 * 1024,
            schedule: ForcedScheduleDecodeLimits::new(1024 * 1024, 4_096, 4_096 * 64),
            workload: FixtureWorkloadDecodeLimits::default(),
            candidate: FIXTURE_FORCED_SCHEDULE_CANDIDATE_LIMITS,
            max_schedule_indices: 4_096,
            max_workload_indices: 4_096,
        }
    }
}

/// Why a strict two-axis replay artifact was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixtureScheduleWorkloadArtifactError {
    /// Source shrink evidence was internally inconsistent.
    SourceInvariant(&'static str),
    /// The complete encoded artifact exceeded caller admission.
    EncodedBytesExceeded { actual: usize, limit: usize },
    /// The artifact prefix or codec version is unsupported.
    WrongMagicOrVersion,
    /// A declared field extended beyond the supplied bytes.
    Truncated,
    /// Bytes remained after the canonical artifact boundary.
    TrailingBytes,
    /// Untrusted length/count arithmetic could not be represented.
    IntegerOverflow,
    /// A bounded vector could not reserve its admitted capacity.
    AllocationRefused,
    /// The artifact checksum did not bind the supplied body.
    ChecksumMismatch,
    /// One execution/candidate root was not canonical lowercase hex.
    InvalidDigest,
    /// One stable failure vocabulary tag was not recognized.
    InvalidFailureTag,
    /// A fault plan was malformed or noncanonical.
    InvalidFaultPlan,
    /// The embedded foundation schedule was refused.
    Schedule(ForcedScheduleError),
    /// The embedded workload or one candidate index set was refused.
    Workload(FixtureWorkloadError),
    /// Decoded authorities did not reproduce a bound identity.
    AuthorityMismatch(&'static str),
    /// Parsed fields did not reproduce the exact supplied bytes.
    NonCanonical,
    /// The real two-axis executor refused the reconstructed authorities.
    Harness(FixtureRunError),
    /// Replay completed without reproducing the exact filed verdict.
    ReplayDiverged(&'static str),
    /// Artifact storage or publication failed.
    Io(io::ErrorKind),
    /// Exact immutable bytes were already published.
    AlreadyPublished,
    /// A publication namespace or content object held different bytes.
    PublicationConflict,
}

impl core::fmt::Display for FixtureScheduleWorkloadArtifactError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SourceInvariant(field) => {
                write!(f, "two-axis shrink source invariant failed: {field}")
            }
            Self::EncodedBytesExceeded { actual, limit } => {
                write!(f, "two-axis artifact bytes {actual} exceed limit {limit}")
            }
            Self::WrongMagicOrVersion => f.write_str("wrong two-axis artifact magic or version"),
            Self::Truncated => f.write_str("truncated two-axis artifact"),
            Self::TrailingBytes => f.write_str("trailing two-axis artifact bytes"),
            Self::IntegerOverflow => f.write_str("two-axis artifact integer overflow"),
            Self::AllocationRefused => f.write_str("two-axis artifact allocation refused"),
            Self::ChecksumMismatch => f.write_str("two-axis artifact checksum mismatch"),
            Self::InvalidDigest => f.write_str("two-axis artifact digest is not canonical hex"),
            Self::InvalidFailureTag => f.write_str("two-axis artifact failure tag is invalid"),
            Self::InvalidFaultPlan => f.write_str("two-axis artifact fault plan is invalid"),
            Self::Schedule(error) => write!(f, "two-axis schedule refused: {error}"),
            Self::Workload(error) => write!(f, "two-axis workload refused: {error}"),
            Self::AuthorityMismatch(field) => {
                write!(f, "two-axis artifact authority mismatch: {field}")
            }
            Self::NonCanonical => f.write_str("two-axis artifact is not canonical"),
            Self::Harness(error) => write!(f, "two-axis artifact replay failed: {error}"),
            Self::ReplayDiverged(field) => {
                write!(f, "two-axis artifact replay diverged at {field}")
            }
            Self::Io(kind) => write!(f, "two-axis artifact I/O failed: {kind:?}"),
            Self::AlreadyPublished => f.write_str("two-axis artifact is already published"),
            Self::PublicationConflict => {
                f.write_str("two-axis artifact publication path holds different bytes")
            }
        }
    }
}

impl std::error::Error for FixtureScheduleWorkloadArtifactError {}

/// Strict, self-contained authority for one minimized fixture failure.
///
/// This is deliberately fixture-scoped. It embeds a complete canonical
/// foundation schedule and workload, but only deletion indices may create the
/// executable candidates. It is not a production scheduler certificate, an
/// arbitrary `TaskId` replay format, or evidence for FG-INV-16.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureScheduleWorkloadArtifact {
    source_execution_digest: String,
    minimal_execution_digest: String,
    source_trace_digest: String,
    source_schedule_digest: String,
    source_workload_digest: String,
    schedule_candidate_digest: String,
    workload_candidate_digest: String,
    fault_plan: FaultPlan,
    failure: FixtureFailureKind,
    source_schedule: ForcedSchedule,
    source_workload: FixtureWorkload,
    schedule_indices: Vec<usize>,
    workload_indices: Vec<usize>,
}

impl FixtureScheduleWorkloadArtifact {
    /// Freezes one already-executed two-axis shrink result.
    pub fn from_shrunk(
        shrunk: &FixtureScheduleWorkloadShrunk,
    ) -> Result<Self, FixtureScheduleWorkloadArtifactError> {
        let source_schedule = shrunk
            .original_evidence()
            .forced_schedule()
            .cloned()
            .ok_or(FixtureScheduleWorkloadArtifactError::SourceInvariant(
                "forced-schedule",
            ))?;
        let source_workload = FixtureWorkload::try_from_canonical_bytes(
            shrunk.workload_candidate().source_workload_bytes(),
            FixtureWorkloadDecodeLimits::default(),
        )
        .map_err(FixtureScheduleWorkloadArtifactError::Workload)?;
        if !shrunk
            .original_evidence()
            .matches_workload(&source_workload)
            || shrunk.original_evidence().scheduler_seed() != Some(source_schedule.seed())
            || shrunk.schedule_candidate().source_trace_digest()
                != shrunk.original_evidence().trace_digest()
            || !shrunk
                .minimal_run()
                .consumed_schedule_candidate(shrunk.schedule_candidate())
            || !shrunk
                .minimal_run()
                .matches_workload_candidate(shrunk.workload_candidate())
            || shrunk.minimal_run().failure_kind() != Some(shrunk.failure())
        {
            return Err(FixtureScheduleWorkloadArtifactError::SourceInvariant(
                "shrink-lineage",
            ));
        }
        let artifact = Self {
            source_execution_digest: shrunk.original_execution_digest().to_string(),
            minimal_execution_digest: shrunk.minimal_execution_digest().to_string(),
            source_trace_digest: shrunk.original_evidence().trace_digest().to_string(),
            source_schedule_digest: shrunk
                .schedule_candidate()
                .source_schedule_digest()
                .to_string(),
            source_workload_digest: shrunk
                .workload_candidate()
                .source_workload_digest()
                .to_string(),
            schedule_candidate_digest: shrunk.schedule_candidate().candidate_digest().to_string(),
            workload_candidate_digest: shrunk.workload_candidate().candidate_digest().to_string(),
            fault_plan: shrunk.original_evidence().fault_plan(),
            failure: shrunk.failure(),
            source_schedule,
            source_workload,
            schedule_indices: shrunk
                .schedule_candidate()
                .retained_source_indices()
                .collect(),
            workload_indices: shrunk
                .workload_candidate()
                .retained_source_indices()
                .collect(),
        };
        artifact.validate_authorities(FixtureScheduleWorkloadArtifactLimits::default())?;
        Ok(artifact)
    }

    /// Source execution-root identity captured before shrinking.
    #[must_use]
    pub fn source_execution_digest(&self) -> &str {
        &self.source_execution_digest
    }

    /// Exact execution-root identity the minimized candidate must reproduce.
    #[must_use]
    pub fn minimal_execution_digest(&self) -> &str {
        &self.minimal_execution_digest
    }

    /// Stable typed failure the minimized execution must reproduce.
    #[must_use]
    pub const fn failure(&self) -> FixtureFailureKind {
        self.failure
    }

    /// Complete source schedule retained by this artifact.
    #[must_use]
    pub const fn source_schedule(&self) -> &ForcedSchedule {
        &self.source_schedule
    }

    /// Complete source workload retained by this artifact.
    #[must_use]
    pub const fn source_workload(&self) -> &FixtureWorkload {
        &self.source_workload
    }

    /// Exact retained scheduler indices.
    #[must_use]
    pub fn schedule_indices(&self) -> &[usize] {
        &self.schedule_indices
    }

    /// Exact retained workload indices.
    #[must_use]
    pub fn workload_indices(&self) -> &[usize] {
        &self.workload_indices
    }

    /// Strict version-1 canonical bytes including an integrity checksum.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, FixtureScheduleWorkloadArtifactError> {
        let schedule_bytes = self
            .source_schedule
            .to_canonical_bytes()
            .map_err(FixtureScheduleWorkloadArtifactError::Schedule)?;
        let workload_bytes = self.source_workload.to_canonical_bytes();
        let fault_plan = self.fault_plan.encode_replay_fields();
        let (component, stage, error_kind) = encode_fixture_failure(self.failure)?;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_MAGIC);
        bytes.extend_from_slice(&FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_VERSION.to_le_bytes());
        for digest in [
            &self.source_execution_digest,
            &self.minimal_execution_digest,
            &self.source_trace_digest,
            &self.source_schedule_digest,
            &self.source_workload_digest,
            &self.schedule_candidate_digest,
            &self.workload_candidate_digest,
        ] {
            if !canonical_digest_hex(digest) {
                return Err(FixtureScheduleWorkloadArtifactError::InvalidDigest);
            }
            bytes.extend_from_slice(digest.as_bytes());
        }
        bytes.extend_from_slice(
            &u32::try_from(fault_plan.len())
                .map_err(|_| FixtureScheduleWorkloadArtifactError::IntegerOverflow)?
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&[component, stage, error_kind, 0]);
        for length in [
            schedule_bytes.len(),
            workload_bytes.len(),
            self.schedule_indices.len(),
            self.workload_indices.len(),
        ] {
            bytes.extend_from_slice(
                &u64::try_from(length)
                    .map_err(|_| FixtureScheduleWorkloadArtifactError::IntegerOverflow)?
                    .to_le_bytes(),
            );
        }
        bytes.extend_from_slice(fault_plan.as_bytes());
        bytes.extend_from_slice(&schedule_bytes);
        bytes.extend_from_slice(&workload_bytes);
        for &index in &self.schedule_indices {
            bytes.extend_from_slice(
                &u64::try_from(index)
                    .map_err(|_| FixtureScheduleWorkloadArtifactError::IntegerOverflow)?
                    .to_le_bytes(),
            );
        }
        for &index in &self.workload_indices {
            bytes.extend_from_slice(
                &u64::try_from(index)
                    .map_err(|_| FixtureScheduleWorkloadArtifactError::IntegerOverflow)?
                    .to_le_bytes(),
            );
        }
        let checksum = fixture_schedule_workload_artifact_checksum(&bytes);
        bytes.extend_from_slice(&checksum);
        Ok(bytes)
    }

    /// Strictly decodes one artifact under caller-owned byte/count/work bounds.
    pub fn try_from_canonical_bytes(
        bytes: &[u8],
        limits: FixtureScheduleWorkloadArtifactLimits,
    ) -> Result<Self, FixtureScheduleWorkloadArtifactError> {
        if bytes.len() > limits.max_encoded_bytes {
            return Err(FixtureScheduleWorkloadArtifactError::EncodedBytesExceeded {
                actual: bytes.len(),
                limit: limits.max_encoded_bytes,
            });
        }
        let mut cursor = 0usize;
        if take_artifact_bytes(bytes, &mut cursor, 8)? != FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_MAGIC
            || read_artifact_u32(bytes, &mut cursor)? != FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_VERSION
        {
            return Err(FixtureScheduleWorkloadArtifactError::WrongMagicOrVersion);
        }
        let source_execution_digest = read_artifact_digest(bytes, &mut cursor)?;
        let minimal_execution_digest = read_artifact_digest(bytes, &mut cursor)?;
        let source_trace_digest = read_artifact_digest(bytes, &mut cursor)?;
        let source_schedule_digest = read_artifact_digest(bytes, &mut cursor)?;
        let source_workload_digest = read_artifact_digest(bytes, &mut cursor)?;
        let schedule_candidate_digest = read_artifact_digest(bytes, &mut cursor)?;
        let workload_candidate_digest = read_artifact_digest(bytes, &mut cursor)?;
        let fault_plan_len = usize::try_from(read_artifact_u32(bytes, &mut cursor)?)
            .map_err(|_| FixtureScheduleWorkloadArtifactError::IntegerOverflow)?;
        let failure_component = read_artifact_u8(bytes, &mut cursor)?;
        let failure_stage = read_artifact_u8(bytes, &mut cursor)?;
        let failure_kind = read_artifact_u8(bytes, &mut cursor)?;
        if read_artifact_u8(bytes, &mut cursor)? != 0 {
            return Err(FixtureScheduleWorkloadArtifactError::NonCanonical);
        }
        let schedule_len = read_artifact_usize(bytes, &mut cursor)?;
        let workload_len = read_artifact_usize(bytes, &mut cursor)?;
        let schedule_count = read_artifact_usize(bytes, &mut cursor)?;
        let workload_count = read_artifact_usize(bytes, &mut cursor)?;
        if schedule_count > limits.max_schedule_indices {
            return Err(FixtureScheduleWorkloadArtifactError::AuthorityMismatch(
                "schedule-index-limit",
            ));
        }
        if workload_count > limits.max_workload_indices {
            return Err(FixtureScheduleWorkloadArtifactError::AuthorityMismatch(
                "workload-index-limit",
            ));
        }
        let index_bytes = schedule_count
            .checked_add(workload_count)
            .and_then(|count| count.checked_mul(8))
            .ok_or(FixtureScheduleWorkloadArtifactError::IntegerOverflow)?;
        let expected = cursor
            .checked_add(fault_plan_len)
            .and_then(|length| length.checked_add(schedule_len))
            .and_then(|length| length.checked_add(workload_len))
            .and_then(|length| length.checked_add(index_bytes))
            .and_then(|length| {
                length.checked_add(FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_CHECKSUM_BYTES)
            })
            .ok_or(FixtureScheduleWorkloadArtifactError::IntegerOverflow)?;
        if bytes.len() < expected {
            return Err(FixtureScheduleWorkloadArtifactError::Truncated);
        }
        if bytes.len() > expected {
            return Err(FixtureScheduleWorkloadArtifactError::TrailingBytes);
        }
        let checksum_start = expected - FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_CHECKSUM_BYTES;
        if bytes[checksum_start..]
            != fixture_schedule_workload_artifact_checksum(&bytes[..checksum_start])
        {
            return Err(FixtureScheduleWorkloadArtifactError::ChecksumMismatch);
        }
        let fault_plan_bytes = take_artifact_bytes(bytes, &mut cursor, fault_plan_len)?;
        let fault_plan_text = std::str::from_utf8(fault_plan_bytes)
            .map_err(|_| FixtureScheduleWorkloadArtifactError::InvalidFaultPlan)?;
        let fault_plan = FaultPlan::decode_replay_fields(fault_plan_text)
            .map_err(|_| FixtureScheduleWorkloadArtifactError::InvalidFaultPlan)?;
        if fault_plan.encode_replay_fields() != fault_plan_text {
            return Err(FixtureScheduleWorkloadArtifactError::InvalidFaultPlan);
        }
        let source_schedule = ForcedSchedule::try_from_canonical_bytes(
            take_artifact_bytes(bytes, &mut cursor, schedule_len)?,
            limits.schedule,
        )
        .map_err(FixtureScheduleWorkloadArtifactError::Schedule)?;
        let source_workload = FixtureWorkload::try_from_canonical_bytes(
            take_artifact_bytes(bytes, &mut cursor, workload_len)?,
            limits.workload,
        )
        .map_err(FixtureScheduleWorkloadArtifactError::Workload)?;
        let schedule_indices = read_artifact_indices(bytes, &mut cursor, schedule_count)?;
        let workload_indices = read_artifact_indices(bytes, &mut cursor, workload_count)?;
        cursor = cursor
            .checked_add(FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_CHECKSUM_BYTES)
            .ok_or(FixtureScheduleWorkloadArtifactError::IntegerOverflow)?;
        if cursor != bytes.len() {
            return Err(FixtureScheduleWorkloadArtifactError::NonCanonical);
        }
        let artifact = Self {
            source_execution_digest,
            minimal_execution_digest,
            source_trace_digest,
            source_schedule_digest,
            source_workload_digest,
            schedule_candidate_digest,
            workload_candidate_digest,
            fault_plan,
            failure: decode_fixture_failure(failure_component, failure_stage, failure_kind)?,
            source_schedule,
            source_workload,
            schedule_indices,
            workload_indices,
        };
        artifact.validate_authorities(limits)?;
        if artifact.to_canonical_bytes()?.as_slice() != bytes {
            return Err(FixtureScheduleWorkloadArtifactError::NonCanonical);
        }
        Ok(artifact)
    }

    /// Reads a bounded artifact without trusting file metadata for allocation.
    pub fn read_from_path(
        path: &Path,
        limits: FixtureScheduleWorkloadArtifactLimits,
    ) -> Result<Self, FixtureScheduleWorkloadArtifactError> {
        use std::io::Read as _;
        let file = std::fs::File::open(path)
            .map_err(|error| FixtureScheduleWorkloadArtifactError::Io(error.kind()))?;
        let admitted = limits
            .max_encoded_bytes
            .checked_add(1)
            .ok_or(FixtureScheduleWorkloadArtifactError::IntegerOverflow)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(admitted.min(64 * 1024))
            .map_err(|_| FixtureScheduleWorkloadArtifactError::AllocationRefused)?;
        file.take(
            u64::try_from(admitted)
                .map_err(|_| FixtureScheduleWorkloadArtifactError::IntegerOverflow)?,
        )
        .read_to_end(&mut bytes)
        .map_err(|error| FixtureScheduleWorkloadArtifactError::Io(error.kind()))?;
        Self::try_from_canonical_bytes(&bytes, limits)
    }

    /// Reconstructs both private candidates and executes the real fixture.
    pub fn replay(
        &self,
        scratch_root: &Path,
        limits: FixtureScheduleWorkloadArtifactLimits,
    ) -> Result<FixtureScheduleCandidateRun, FixtureScheduleWorkloadArtifactError> {
        let (schedule_candidate, workload_candidate) = self.validate_authorities(limits)?;
        let mut cfg = crate::fixture::FixtureConfig::new(self.source_workload.seed());
        cfg.fault_plan = self.fault_plan;
        let source_error = match run_fixture_workload_under_forced_schedule(
            &cfg,
            &self.source_workload,
            &scratch_root.join("source"),
            LabConfig::new(self.source_schedule.seed()),
            &self.source_schedule,
            FIXTURE_FORCED_SCHEDULE_CAPTURE_LIMITS,
        ) {
            Ok(_) => {
                return Err(FixtureScheduleWorkloadArtifactError::ReplayDiverged(
                    "source-failure",
                ));
            }
            Err(error) => error,
        };
        let source_evidence = source_error.failure_evidence().ok_or(
            FixtureScheduleWorkloadArtifactError::ReplayDiverged("source-evidence"),
        )?;
        if source_error.failure_kind() != Some(self.failure)
            || source_evidence.execution_digest() != self.source_execution_digest
            || source_evidence.trace_digest() != self.source_trace_digest
        {
            return Err(FixtureScheduleWorkloadArtifactError::ReplayDiverged(
                "source-execution-digest",
            ));
        }
        let run = run_fixture_schedule_workload_candidate(
            &cfg,
            &workload_candidate,
            &scratch_root.join("minimal"),
            LabConfig::new(self.source_schedule.seed()),
            &schedule_candidate,
            limits.candidate,
        )
        .map_err(FixtureScheduleWorkloadArtifactError::Harness)?;
        if run.failure_kind() != Some(self.failure) {
            return Err(FixtureScheduleWorkloadArtifactError::ReplayDiverged(
                "failure-kind",
            ));
        }
        if !run.consumed_schedule_candidate(&schedule_candidate)
            || !run.matches_workload_candidate(&workload_candidate)
        {
            return Err(FixtureScheduleWorkloadArtifactError::ReplayDiverged(
                "candidate-consumption",
            ));
        }
        if run.execution_digest() != self.minimal_execution_digest {
            return Err(FixtureScheduleWorkloadArtifactError::ReplayDiverged(
                "minimal-execution-digest",
            ));
        }
        Ok(run)
    }

    /// Replays before publishing immutable, no-overwrite canonical bytes.
    pub fn replay_and_publish(
        &self,
        output_root: &Path,
        replay_root: &Path,
        limits: FixtureScheduleWorkloadArtifactLimits,
    ) -> Result<PathBuf, FixtureScheduleWorkloadArtifactError> {
        use std::io::Write as _;
        let _ = self.replay(replay_root, limits)?;
        let bytes = self.to_canonical_bytes()?;
        if bytes.len() > limits.max_encoded_bytes {
            return Err(FixtureScheduleWorkloadArtifactError::EncodedBytesExceeded {
                actual: bytes.len(),
                limit: limits.max_encoded_bytes,
            });
        }
        std::fs::create_dir_all(output_root)
            .map_err(|error| FixtureScheduleWorkloadArtifactError::Io(error.kind()))?;
        let final_path = output_root.join(format!(
            "{}.fixture-schedule-workload.fgsw",
            self.source_execution_digest
        ));
        if final_path
            .try_exists()
            .map_err(|error| FixtureScheduleWorkloadArtifactError::Io(error.kind()))?
        {
            let existing = read_bounded_artifact_bytes(&final_path, limits.max_encoded_bytes)?;
            return if existing == bytes {
                Err(FixtureScheduleWorkloadArtifactError::AlreadyPublished)
            } else {
                Err(FixtureScheduleWorkloadArtifactError::PublicationConflict)
            };
        }
        let ordinal = FIXTURE_SCHEDULE_WORKLOAD_PUBLICATION_ORDINAL.fetch_add(1, Ordering::Relaxed);
        let staging_path = output_root.join(format!(
            ".fixture-schedule-workload-{}-{}-{ordinal}.fgsw",
            std::process::id(),
            fixture_schedule_workload_artifact_digest(&bytes),
        ));
        let mut staging = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging_path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    FixtureScheduleWorkloadArtifactError::PublicationConflict
                } else {
                    FixtureScheduleWorkloadArtifactError::Io(error.kind())
                }
            })?;
        staging
            .write_all(&bytes)
            .and_then(|()| staging.sync_all())
            .map_err(|error| FixtureScheduleWorkloadArtifactError::Io(error.kind()))?;
        if let Err(error) = std::fs::hard_link(&staging_path, &final_path) {
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(FixtureScheduleWorkloadArtifactError::Io(error.kind()));
            }
            let existing = read_bounded_artifact_bytes(&final_path, limits.max_encoded_bytes)?;
            return if existing == bytes {
                Err(FixtureScheduleWorkloadArtifactError::AlreadyPublished)
            } else {
                Err(FixtureScheduleWorkloadArtifactError::PublicationConflict)
            };
        }
        std::fs::File::open(output_root)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| FixtureScheduleWorkloadArtifactError::Io(error.kind()))?;
        Ok(final_path)
    }

    /// Builds the ignored-test command that validates a fresh-process replay.
    pub fn command_for(
        &self,
        artifact_path: &Path,
    ) -> Result<std::process::Command, FixtureScheduleWorkloadArtifactError> {
        let executable = std::env::current_exe()
            .map_err(|error| FixtureScheduleWorkloadArtifactError::Io(error.kind()))?;
        let mut command = std::process::Command::new(executable);
        command
            .arg("--ignored")
            .arg("--exact")
            .arg("fixture_schedule_workload_artifact_from_env")
            .arg("--test-threads=1")
            .env(FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_ENV, artifact_path)
            .env(
                FIXTURE_SCHEDULE_WORKLOAD_SOURCE_DIGEST_ENV,
                &self.source_execution_digest,
            )
            .env(
                FIXTURE_SCHEDULE_WORKLOAD_MINIMAL_DIGEST_ENV,
                &self.minimal_execution_digest,
            );
        Ok(command)
    }

    fn validate_authorities(
        &self,
        limits: FixtureScheduleWorkloadArtifactLimits,
    ) -> Result<
        (FixtureScheduleCandidate, FixtureWorkloadCandidate),
        FixtureScheduleWorkloadArtifactError,
    > {
        for digest in [
            &self.source_execution_digest,
            &self.minimal_execution_digest,
            &self.source_trace_digest,
            &self.source_schedule_digest,
            &self.source_workload_digest,
            &self.schedule_candidate_digest,
            &self.workload_candidate_digest,
        ] {
            if !canonical_digest_hex(digest) {
                return Err(FixtureScheduleWorkloadArtifactError::InvalidDigest);
            }
        }
        if self.source_schedule.seed() != self.source_workload.seed() {
            return Err(FixtureScheduleWorkloadArtifactError::AuthorityMismatch(
                "source-seed",
            ));
        }
        if self.schedule_indices.len() > limits.max_schedule_indices
            || self.workload_indices.len() > limits.max_workload_indices
        {
            return Err(FixtureScheduleWorkloadArtifactError::AuthorityMismatch(
                "candidate-index-limit",
            ));
        }
        let schedule_candidate = derive_fixture_schedule_candidate(
            &self.source_workload,
            &self.source_schedule,
            &self.source_trace_digest,
            self.fault_plan,
            &self.schedule_indices,
            limits.candidate,
        )
        .map_err(FixtureScheduleWorkloadArtifactError::Harness)?;
        let workload_candidate = self
            .source_workload
            .derive_candidate(&self.workload_indices)
            .map_err(FixtureScheduleWorkloadArtifactError::Workload)?;
        for (matches, field) in [
            (
                schedule_candidate.source_schedule_digest() == self.source_schedule_digest,
                "source-schedule-digest",
            ),
            (
                workload_candidate.source_workload_digest() == self.source_workload_digest,
                "source-workload-digest",
            ),
            (
                schedule_candidate.candidate_digest() == self.schedule_candidate_digest,
                "schedule-candidate-digest",
            ),
            (
                workload_candidate.candidate_digest() == self.workload_candidate_digest,
                "workload-candidate-digest",
            ),
        ] {
            if !matches {
                return Err(FixtureScheduleWorkloadArtifactError::AuthorityMismatch(
                    field,
                ));
            }
        }
        Ok((schedule_candidate, workload_candidate))
    }
}

fn canonical_digest_hex(digest: &str) -> bool {
    digest.len() == FIXTURE_SCHEDULE_WORKLOAD_DIGEST_HEX_BYTES
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn encode_fixture_failure(
    failure: FixtureFailureKind,
) -> Result<(u8, u8, u8), FixtureScheduleWorkloadArtifactError> {
    let (component, stage, kind) = match failure {
        FixtureFailureKind::Producer { stage, kind } => (1, stage, kind),
        FixtureFailureKind::Consumer { stage, kind } => (2, stage, kind),
    };
    let stage = match stage {
        FixtureTaskStage::DurableWrite => 1,
        FixtureTaskStage::FrameHeaderWrite => 2,
        FixtureTaskStage::FrameBodyWrite => 3,
        FixtureTaskStage::TerminatorWrite => 4,
        FixtureTaskStage::FrameHeaderRead => 5,
        FixtureTaskStage::FrameBodyRead => 6,
    };
    let kind = match kind {
        io::ErrorKind::NotFound => 1,
        io::ErrorKind::AlreadyExists => 2,
        io::ErrorKind::InvalidInput => 3,
        io::ErrorKind::InvalidData => 4,
        io::ErrorKind::UnexpectedEof => 5,
        io::ErrorKind::WriteZero => 6,
        io::ErrorKind::StorageFull => 7,
        _ => return Err(FixtureScheduleWorkloadArtifactError::InvalidFailureTag),
    };
    Ok((component, stage, kind))
}

fn decode_fixture_failure(
    component: u8,
    stage: u8,
    kind: u8,
) -> Result<FixtureFailureKind, FixtureScheduleWorkloadArtifactError> {
    let stage = match stage {
        1 => FixtureTaskStage::DurableWrite,
        2 => FixtureTaskStage::FrameHeaderWrite,
        3 => FixtureTaskStage::FrameBodyWrite,
        4 => FixtureTaskStage::TerminatorWrite,
        5 => FixtureTaskStage::FrameHeaderRead,
        6 => FixtureTaskStage::FrameBodyRead,
        _ => return Err(FixtureScheduleWorkloadArtifactError::InvalidFailureTag),
    };
    let kind = match kind {
        1 => io::ErrorKind::NotFound,
        2 => io::ErrorKind::AlreadyExists,
        3 => io::ErrorKind::InvalidInput,
        4 => io::ErrorKind::InvalidData,
        5 => io::ErrorKind::UnexpectedEof,
        6 => io::ErrorKind::WriteZero,
        7 => io::ErrorKind::StorageFull,
        _ => return Err(FixtureScheduleWorkloadArtifactError::InvalidFailureTag),
    };
    match component {
        1 => Ok(FixtureFailureKind::Producer { stage, kind }),
        2 => Ok(FixtureFailureKind::Consumer { stage, kind }),
        _ => Err(FixtureScheduleWorkloadArtifactError::InvalidFailureTag),
    }
}

fn fixture_schedule_workload_artifact_checksum(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(FIXTURE_SCHEDULE_WORKLOAD_ARTIFACT_DOMAIN);
    hasher.update(bytes);
    hasher.finalize().0
}

fn fixture_schedule_workload_artifact_digest(bytes: &[u8]) -> String {
    let mut hasher = Hasher::new();
    hasher.update(b"fgdb.sim.fixture.schedule-workload-artifact-object.v1");
    hasher.update(bytes);
    hasher.finalize().to_hex()
}

fn take_artifact_bytes<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], FixtureScheduleWorkloadArtifactError> {
    let end = cursor
        .checked_add(length)
        .ok_or(FixtureScheduleWorkloadArtifactError::IntegerOverflow)?;
    let value = bytes
        .get(*cursor..end)
        .ok_or(FixtureScheduleWorkloadArtifactError::Truncated)?;
    *cursor = end;
    Ok(value)
}

fn read_artifact_u8(
    bytes: &[u8],
    cursor: &mut usize,
) -> Result<u8, FixtureScheduleWorkloadArtifactError> {
    Ok(take_artifact_bytes(bytes, cursor, 1)?[0])
}

fn read_artifact_u32(
    bytes: &[u8],
    cursor: &mut usize,
) -> Result<u32, FixtureScheduleWorkloadArtifactError> {
    let raw: [u8; 4] = take_artifact_bytes(bytes, cursor, 4)?
        .try_into()
        .map_err(|_| FixtureScheduleWorkloadArtifactError::Truncated)?;
    Ok(u32::from_le_bytes(raw))
}

fn read_artifact_usize(
    bytes: &[u8],
    cursor: &mut usize,
) -> Result<usize, FixtureScheduleWorkloadArtifactError> {
    let raw: [u8; 8] = take_artifact_bytes(bytes, cursor, 8)?
        .try_into()
        .map_err(|_| FixtureScheduleWorkloadArtifactError::Truncated)?;
    usize::try_from(u64::from_le_bytes(raw))
        .map_err(|_| FixtureScheduleWorkloadArtifactError::IntegerOverflow)
}

fn read_artifact_digest(
    bytes: &[u8],
    cursor: &mut usize,
) -> Result<String, FixtureScheduleWorkloadArtifactError> {
    let raw = take_artifact_bytes(bytes, cursor, FIXTURE_SCHEDULE_WORKLOAD_DIGEST_HEX_BYTES)?;
    let digest = std::str::from_utf8(raw)
        .map_err(|_| FixtureScheduleWorkloadArtifactError::InvalidDigest)?;
    if !canonical_digest_hex(digest) {
        return Err(FixtureScheduleWorkloadArtifactError::InvalidDigest);
    }
    Ok(digest.to_string())
}

fn read_artifact_indices(
    bytes: &[u8],
    cursor: &mut usize,
    count: usize,
) -> Result<Vec<usize>, FixtureScheduleWorkloadArtifactError> {
    let mut indices = Vec::new();
    indices
        .try_reserve_exact(count)
        .map_err(|_| FixtureScheduleWorkloadArtifactError::AllocationRefused)?;
    for _ in 0..count {
        indices.push(read_artifact_usize(bytes, cursor)?);
    }
    Ok(indices)
}

fn read_bounded_artifact_bytes(
    path: &Path,
    limit: usize,
) -> Result<Vec<u8>, FixtureScheduleWorkloadArtifactError> {
    use std::io::Read as _;
    let admitted = limit
        .checked_add(1)
        .ok_or(FixtureScheduleWorkloadArtifactError::IntegerOverflow)?;
    let file = std::fs::File::open(path)
        .map_err(|error| FixtureScheduleWorkloadArtifactError::Io(error.kind()))?;
    let mut bytes = Vec::new();
    file.take(
        u64::try_from(admitted)
            .map_err(|_| FixtureScheduleWorkloadArtifactError::IntegerOverflow)?,
    )
    .read_to_end(&mut bytes)
    .map_err(|error| FixtureScheduleWorkloadArtifactError::Io(error.kind()))?;
    if bytes.len() > limit {
        return Err(FixtureScheduleWorkloadArtifactError::EncodedBytesExceeded {
            actual: bytes.len(),
            limit,
        });
    }
    Ok(bytes)
}

/// Infrastructure error that prevents an honest two-axis fixture shrink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixtureScheduleWorkloadShrinkError {
    /// The source run or a candidate failed outside the typed fixture model.
    Harness(FixtureRunError),
    /// Deriving a deletion-only workload candidate failed.
    Workload(FixtureWorkloadError),
    /// A source failure did not carry LAB forced-schedule authority.
    MissingScheduleEvidence,
    /// The source failed, but its complete candidate did not reproduce.
    CandidatePremiseDidNotReproduce,
    /// The reducer returned axes that did not match its last accepted run.
    MinimalCandidateMismatch,
    /// Caller configured no candidate execution budget.
    ZeroCandidateExecutionLimit,
    /// Candidate execution budget was exhausted before a verdict was known.
    CandidateExecutionLimitExceeded { limit: usize },
}

impl core::fmt::Display for FixtureScheduleWorkloadShrinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Harness(error) => write!(f, "two-axis fixture shrink harness failed: {error}"),
            Self::Workload(error) => {
                write!(f, "two-axis fixture workload candidate refused: {error}")
            }
            Self::MissingScheduleEvidence => {
                f.write_str("two-axis fixture shrink source lacks forced-schedule evidence")
            }
            Self::CandidatePremiseDidNotReproduce => f.write_str(
                "complete schedule/workload candidate did not reproduce its source failure",
            ),
            Self::MinimalCandidateMismatch => {
                f.write_str("two-axis fixture shrink returned mismatched minimal authorities")
            }
            Self::ZeroCandidateExecutionLimit => {
                f.write_str("two-axis fixture shrink candidate limit must be nonzero")
            }
            Self::CandidateExecutionLimitExceeded { limit } => write!(
                f,
                "two-axis fixture shrink exhausted its {limit} candidate executions"
            ),
        }
    }
}

impl std::error::Error for FixtureScheduleWorkloadShrinkError {}

/// Caller-owned work admission for one two-axis fixture shrink campaign.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureScheduleWorkloadShrinkLimits {
    /// Per-candidate foundation admission.
    pub candidate: ForcedScheduleCandidateLimits,
    /// Maximum prefix probes plus delta-debug candidate executions.
    pub max_candidate_executions: usize,
}

impl FixtureScheduleWorkloadShrinkLimits {
    #[must_use]
    pub const fn new(
        candidate: ForcedScheduleCandidateLimits,
        max_candidate_executions: usize,
    ) -> Self {
        Self {
            candidate,
            max_candidate_executions,
        }
    }
}

/// Default bounded admission for the exported fixture's two-axis shrinker.
pub const FIXTURE_SCHEDULE_WORKLOAD_SHRINK_LIMITS: FixtureScheduleWorkloadShrinkLimits =
    FixtureScheduleWorkloadShrinkLimits::new(FIXTURE_FORCED_SCHEDULE_CANDIDATE_LIMITS, 32_768);

/// Minimize one real failing LAB fixture across schedule and workload axes.
///
/// The source execution is captured once. Every scheduler candidate derives
/// from that failure's exact [`asupersync::lab::runtime::ForcedSchedule`], and
/// every workload candidate derives from the exact canonical source actions.
/// A reduction is accepted only when the candidate executor observes the same
/// [`FixtureFailureKind`] and consumes both immutable authorities.
pub fn shrink_fixture_schedule_and_workload_under_lab(
    cfg: &crate::fixture::FixtureConfig,
    workload: &FixtureWorkload,
    scratch_root: &Path,
    scheduler_seed: u64,
    limits: FixtureScheduleWorkloadShrinkLimits,
) -> Result<Option<FixtureScheduleWorkloadShrunk>, FixtureScheduleWorkloadShrinkError> {
    if limits.max_candidate_executions == 0 {
        return Err(FixtureScheduleWorkloadShrinkError::ZeroCandidateExecutionLimit);
    }
    let source_result = run_fixture_workload_under_lab(
        cfg,
        workload,
        &scratch_root.join("fixture-two-axis-source"),
        asupersync::lab::LabConfig::new(scheduler_seed),
    );
    let source_error = match source_result {
        Ok(_) => return Ok(None),
        Err(error) => error,
    };
    let target = source_error
        .failure_kind()
        .ok_or_else(|| FixtureScheduleWorkloadShrinkError::Harness(source_error.clone()))?;
    let original_evidence = source_error
        .failure_evidence()
        .cloned()
        .ok_or_else(|| FixtureScheduleWorkloadShrinkError::Harness(source_error.clone()))?;
    let source_schedule = original_evidence
        .forced_schedule()
        .ok_or(FixtureScheduleWorkloadShrinkError::MissingScheduleEvidence)?;
    let mut schedule_axis = (0..source_schedule.dispatches().len()).collect::<Vec<_>>();
    let workload_axis = (0..workload.actions().len()).collect::<Vec<_>>();

    // A failed source run may record cleanup dispatches after the component
    // returned its error. Exact candidate execution correctly refuses those
    // now-unavailable tasks. Trim only that observed suffix, then require the
    // remaining complete prefix to reproduce before delta debugging begins.
    let full_workload_candidate = workload
        .derive_candidate(&workload_axis)
        .map_err(FixtureScheduleWorkloadShrinkError::Workload)?;
    let mut prefix_probes = 0usize;
    loop {
        let prefix_candidate = original_evidence
            .derive_schedule_candidate(workload, &schedule_axis, limits.candidate)
            .map_err(FixtureScheduleWorkloadShrinkError::Harness)?;
        let prefix_root =
            scratch_root.join(format!("fixture-two-axis-prefix-probe-{prefix_probes:04}"));
        if prefix_probes >= limits.max_candidate_executions {
            return Err(
                FixtureScheduleWorkloadShrinkError::CandidateExecutionLimitExceeded {
                    limit: limits.max_candidate_executions,
                },
            );
        }
        prefix_probes += 1;
        match run_fixture_schedule_workload_candidate(
            cfg,
            &full_workload_candidate,
            &prefix_root,
            asupersync::lab::LabConfig::new(scheduler_seed),
            &prefix_candidate,
            limits.candidate,
        ) {
            Ok(run) if run.failure_kind() == Some(target) => break,
            Err(FixtureRunError::ForcedSchedule(
                asupersync::lab::runtime::ForcedScheduleError::TaskUnavailable { index, .. },
            )) if index > 0 && index < schedule_axis.len() => {
                schedule_axis.truncate(index);
            }
            Ok(_) | Err(FixtureRunError::ForcedSchedule(_)) => {
                return Err(FixtureScheduleWorkloadShrinkError::CandidatePremiseDidNotReproduce);
            }
            Err(error) => return Err(FixtureScheduleWorkloadShrinkError::Harness(error)),
        }
    }

    let mut fatal = None;
    let mut ordinal = 0usize;
    let mut last_reproduced = None;
    let shrunk = {
        let mut reproduces = |schedule_indices: &[usize], workload_indices: &[usize]| {
            if fatal.is_some() {
                return ShrinkTrial::DidNotReproduce;
            }
            let workload_candidate = match workload.derive_candidate(workload_indices) {
                Ok(candidate) => candidate,
                Err(error) => {
                    fatal = Some(FixtureScheduleWorkloadShrinkError::Workload(error));
                    return ShrinkTrial::DidNotReproduce;
                }
            };
            let schedule_candidate = match original_evidence.derive_schedule_candidate(
                workload,
                schedule_indices,
                limits.candidate,
            ) {
                Ok(candidate) => candidate,
                Err(error) => {
                    fatal = Some(FixtureScheduleWorkloadShrinkError::Harness(error));
                    return ShrinkTrial::DidNotReproduce;
                }
            };
            let attempt = scratch_root.join(format!("fixture-two-axis-attempt-{ordinal:04}"));
            if prefix_probes.saturating_add(ordinal) >= limits.max_candidate_executions {
                fatal = Some(
                    FixtureScheduleWorkloadShrinkError::CandidateExecutionLimitExceeded {
                        limit: limits.max_candidate_executions,
                    },
                );
                return ShrinkTrial::DidNotReproduce;
            }
            ordinal += 1;
            match run_fixture_schedule_workload_candidate(
                cfg,
                &workload_candidate,
                &attempt,
                asupersync::lab::LabConfig::new(scheduler_seed),
                &schedule_candidate,
                limits.candidate,
            ) {
                Ok(run) if run.failure_kind() == Some(target) => {
                    if !run.consumed_schedule_candidate(&schedule_candidate)
                        || !run.matches_workload_candidate(&workload_candidate)
                    {
                        fatal = Some(FixtureScheduleWorkloadShrinkError::MinimalCandidateMismatch);
                        return ShrinkTrial::DidNotReproduce;
                    }
                    last_reproduced = Some((
                        schedule_indices.to_vec(),
                        workload_indices.to_vec(),
                        schedule_candidate,
                        workload_candidate,
                        run,
                    ));
                    ShrinkTrial::Reproduced
                }
                Ok(run) => run
                    .failure_kind()
                    .map_or(ShrinkTrial::DidNotReproduce, ShrinkTrial::DifferentFailure),
                Err(FixtureRunError::ForcedSchedule(
                    asupersync::lab::runtime::ForcedScheduleError::TaskUnavailable { .. },
                )) => ShrinkTrial::DidNotReproduce,
                Err(error) => {
                    fatal = Some(FixtureScheduleWorkloadShrinkError::Harness(error));
                    ShrinkTrial::DidNotReproduce
                }
            }
        };
        shrink_schedule_and_workload(schedule_axis, workload_axis, &mut reproduces)
    };
    if let Some(error) = fatal {
        return Err(error);
    }
    let Some(shrunk) = shrunk else {
        return Err(FixtureScheduleWorkloadShrinkError::CandidatePremiseDidNotReproduce);
    };
    let Some((
        minimal_schedule_indices,
        minimal_workload_indices,
        schedule_candidate,
        workload_candidate,
        minimal_run,
    )) = last_reproduced
    else {
        return Err(FixtureScheduleWorkloadShrinkError::CandidatePremiseDidNotReproduce);
    };
    if minimal_schedule_indices != shrunk.schedule
        || minimal_workload_indices != shrunk.workload
        || minimal_run.failure_kind() != Some(target)
    {
        return Err(FixtureScheduleWorkloadShrinkError::MinimalCandidateMismatch);
    }
    Ok(Some(FixtureScheduleWorkloadShrunk {
        original_evidence,
        schedule_candidate,
        workload_candidate,
        minimal_run,
        failure: target,
        attempts: shrunk
            .attempts
            .saturating_add(prefix_probes)
            .saturating_add(1),
        accepted: shrunk.accepted,
        rejected_different_failure: shrunk.rejected_different_failure,
    }))
}
