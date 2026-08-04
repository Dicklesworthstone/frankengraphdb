//! Grading how faithfully a replay reproduced its recording (plan §15.1).
//!
//! > "Production incident replay is graded. … Each bundle carries the explicit
//! > `ReplayCompleteness` grade `Replayable |
//! > StructuralReplay{reproduced_classes[],omitted_classes[]} |
//! > VerifiableIfArtifactsSupplied{missing_classes[]} |
//! > AuditOnly{missing_or_redacted_classes[]}`. Crypto entropy is never
//! > recorded. Full internal forensic bundles may be exact when policy
//! > permits; **customer-safe bundles never overclaim byte identity**."
//!
//! MEASURED before writing this: `ReplayCompleteness` had zero occurrences in
//! `crates/`. The vocabulary existed only in the plan.
//!
//! # The law is the last sentence, not the enum
//!
//! Transcribing four variants is not the work — a grader that returns
//! `Replayable` unconditionally satisfies the enum perfectly and is exactly
//! the failure the sentence forbids. So [`grade`] can only reach
//! [`ReplayCompleteness::Replayable`] when all three of these hold:
//!
//! 1. nothing was withheld from the recording (redaction or policy),
//! 2. the replayed fault log is **identical** to the recorded one — checked
//!    with [`crate::shrink::diverge`], the same comparison the determinism
//!    tests use, so the two cannot drift apart,
//! 3. the replay reached the same failure (or the same success).
//!
//! Everything else grades **down**. The grades are ordered by how much they
//! claim, and the ordering is the point: a bundle that cannot prove byte
//! identity must say so in its own type rather than in a footnote.
//!
//! # What the grades mean here
//!
//! * `Replayable` — byte-identical, nothing withheld. The only grade that
//!   claims byte identity, and the only one gated on all three conditions.
//! * `StructuralReplay` — the run reproduced, but not byte-identically:
//!   some fault classes came back and some did not. Both lists are reported
//!   because "which ones" is the whole diagnostic value.
//! * `VerifiableIfArtifactsSupplied` — classes are missing from the
//!   *recording*, not from the replay: the replay could be checked if someone
//!   supplied them. Names what to go and fetch.
//! * `AuditOnly` — nothing reproducible survives; the bundle can be read but
//!   not re-executed. The floor, and the honest answer for a heavily redacted
//!   customer bundle.

use crate::artifact::{Failure, RunOutcome};
use crate::shrink::diverge;
use crate::vfs::FaultEvent;
use std::collections::BTreeSet;

/// How much a replay is entitled to claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayCompleteness {
    /// Byte-identical reproduction with nothing withheld.
    Replayable,
    /// Reproduced, but not byte-identically.
    StructuralReplay {
        /// Fault classes that came back.
        reproduced_classes: Vec<String>,
        /// Fault classes that did not.
        omitted_classes: Vec<String>,
    },
    /// The recording is missing classes the replay would need.
    VerifiableIfArtifactsSupplied {
        /// What has to be supplied before this can be checked.
        missing_classes: Vec<String>,
    },
    /// Readable, not re-executable.
    AuditOnly {
        /// What is missing or was redacted away.
        missing_or_redacted_classes: Vec<String>,
    },
}

impl ReplayCompleteness {
    /// Whether this grade asserts byte identity.
    ///
    /// Exactly one variant may, which is the whole contract; a caller
    /// deciding what a customer-facing bundle is allowed to say asks this
    /// rather than matching the variants itself and getting it subtly wrong.
    #[must_use]
    pub const fn claims_byte_identity(&self) -> bool {
        matches!(self, Self::Replayable)
    }
}

/// A recorded run, plus whatever the retention contract kept out of it.
#[derive(Clone, Debug)]
pub struct Recording {
    /// The fault log as recorded.
    pub events: Vec<FaultEvent>,
    /// The failure the recorded run reached, if any.
    pub failure: Option<Failure>,
    /// Classes the bundle deliberately does not carry — redacted, or never
    /// retained. Plan §15.1: crypto entropy is never recorded, so it is
    /// always a member of this set for any run that used it.
    pub withheld_classes: Vec<String>,
}

fn classes_of(events: &[FaultEvent]) -> BTreeSet<String> {
    events
        .iter()
        .map(|event| event.kind.class().to_string())
        .collect()
}

/// Grades `replayed` against `recorded`.
///
/// The grade is the strongest one the evidence supports and never stronger —
/// see the module docs for the three conditions `Replayable` requires.
#[must_use]
pub fn grade(recorded: &Recording, replayed: &RunOutcome) -> ReplayCompleteness {
    let recorded_classes = classes_of(&recorded.events);
    let replayed_classes = classes_of(&replayed.events);

    let withheld: Vec<String> = {
        let mut sorted: Vec<String> = recorded.withheld_classes.clone();
        sorted.sort();
        sorted.dedup();
        sorted
    };

    // Nothing came back at all, and something was withheld: the bundle can be
    // read, not re-run.
    if !withheld.is_empty() && replayed.events.is_empty() {
        return ReplayCompleteness::AuditOnly {
            missing_or_redacted_classes: withheld,
        };
    }

    // Something was withheld but the replay still produced faults: it becomes
    // checkable once the withheld classes are supplied.
    if !withheld.is_empty() {
        return ReplayCompleteness::VerifiableIfArtifactsSupplied {
            missing_classes: withheld,
        };
    }

    // Nothing withheld. Byte identity is now the only remaining question, and
    // it is asked with the same comparison the determinism tests use.
    let identical = diverge(&recorded.events, &replayed.events).is_none();
    let same_outcome = recorded.failure.as_ref().map(|failure| failure.kind)
        == replayed.failure.as_ref().map(|failure| failure.kind);

    if identical && same_outcome {
        return ReplayCompleteness::Replayable;
    }

    // Reproduced, but not identically. Report both sides: "which classes came
    // back" is the diagnostic, and a grade without it is just a label.
    let reproduced: Vec<String> = recorded_classes
        .intersection(&replayed_classes)
        .cloned()
        .collect();
    let omitted: Vec<String> = recorded_classes
        .difference(&replayed_classes)
        .cloned()
        .collect();
    ReplayCompleteness::StructuralReplay {
        reproduced_classes: reproduced,
        omitted_classes: omitted,
    }
}
