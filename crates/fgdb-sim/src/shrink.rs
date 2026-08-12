//! Shrinking a failing replay to a minimal reproducer (plan §15.1).
//!
//! > "Failing runs shrink themselves. Hierarchical delta debugging + replay
//! > minimization reduce every failing seed (schedule + workload trace) to a
//! > minimal reproducer before filing, and divergence diagnostics explain
//! > exactly where a replay departed from its recording. Crashpacks arrive
//! > pre-shrunk; the bug report writes itself."
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

use crate::artifact::{Failure, Replay, RunOutcome};
use crate::vfs::{FaultEvent, FaultPlan, Trigger};
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
    let Some(original) = isolated_run(replay, dir, &mut ordinal)?.failure else {
        return Ok(None);
    };
    let target = original.kind;

    let mut best = replay;
    let mut failure = original;
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
    Ok(Some(Shrunk {
        replay: best,
        failure,
        steps,
        rejected,
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
