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
//! through asupersync's bounded forced-schedule candidate path. This is real
//! two-axis minimization for the exported fixture, not a universal scheduler
//! shrinker or a standalone persisted forced-schedule artifact.
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
    FIXTURE_FORCED_SCHEDULE_CANDIDATE_LIMITS, FixtureFailureEvidence, FixtureFailureKind,
    FixtureReplay, FixtureReplayError, FixtureRunError, FixtureScheduleCandidate,
    FixtureScheduleCandidateRun, run_fixture_schedule_workload_candidate,
    run_fixture_workload_under_lab,
};
use crate::fixture::{
    FixtureWorkload, FixtureWorkloadAction, FixtureWorkloadCandidate, FixtureWorkloadError,
};
use crate::vfs::{FaultEvent, FaultPlan, Trigger};
use asupersync::lab::runtime::ForcedScheduleCandidateLimits;
use fgdb_crypto::Hasher;
use std::io;
use std::path::Path;

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
