//! The LDFI target registry (plan §15.1 line 1132).
//!
//! > "lineage-driven fault injection derives minimal fault hypotheses from
//! > successful-run dependencies. It targets every file/directory action in
//! > D1/D2 and every ordered, certificate, external-CAS, or physical
//! > side-effect boundary in dual-root publication; attempt generation/ticket
//! > claim/statement-workspace publication and delivery; checkpoint
//! > install/provisional-cut activation; prepared ownership and Raft; remote
//! > release; key stage/activate/zero/destroy/physical completion; GC
//! > preflight/authorization/quarantine/member completion; backup
//! > pin/copy/reopen/publish/release; restore
//! > reservation/transform/reconciliation/hidden activation/visibility/service
//! > preparation/continuity-plus-catalog receipt/finalize/open/reopen/
//! > completion; and Local-to-W12 seal/activation/authority-transfer/
//! > retirement."
//!
//! MEASURED before writing this: `ldfi` had zero occurrences across `crates/`.
//!
//! # What this registry is, and the specific dishonesty it prevents
//!
//! The plan calls that a **fixed target list**. Almost none of those targets
//! exist yet — there is no Raft, no GC, no backup, no restore, no W12. The
//! tempting move is to register the handful that do exist and let the campaign
//! report coverage over *those*, which yields a healthy-looking percentage of
//! a denominator quietly redefined to mean "what we built".
//!
//! So every target in line 1132 gets a row **now**, and each row carries a
//! [`Reachability`] saying whether an injection point exists at this HEAD.
//! Coverage is then reported against the plan's denominator, and the gap is a
//! number ([`unreachable_count`]) rather than an omission. A registry that
//! only listed reachable targets could not express "we cover 4 of 41".
//!
//! # What this is not
//!
//! It is the target *inventory*, not the injector. Deriving minimal fault
//! hypotheses from successful-run lineage is the actual LDFI algorithm and is
//! not here — [`Reachability::Reachable`] currently means "the lab VFS can
//! fault this", which is exactly the four filesystem classes from
//! [`crate::vfs`] and nothing else. Claiming otherwise would make this
//! registry the very overclaim it exists to prevent.

/// Whether a target can actually be faulted at this HEAD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reachability {
    /// An injection point exists and the harness can fault it today.
    Reachable,
    /// The subsystem does not exist yet. Names the bead that will make it
    /// reachable, so the gap has an owner rather than being a silent zero.
    NotYetBuilt {
        /// The bead that will make this target reachable.
        bead: &'static str,
    },
}

impl Reachability {
    /// Whether the harness can fault this target today.
    #[must_use]
    pub const fn is_reachable(self) -> bool {
        matches!(self, Self::Reachable)
    }
}

/// One declared fault-injection target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LdfiTarget {
    /// Stable id, kebab-case.
    pub id: &'static str,
    /// The phrase in plan line 1132 this row comes from. Every row must quote
    /// its source, so a row nobody can find in the plan is visible as invented.
    pub source_phrase: &'static str,
    /// Whether it can be faulted today.
    pub reachability: Reachability,
}

/// The bead that owns each not-yet-built cluster, named once.
const W2: &str = "fgdb-1xtp";
const W12: &str = "fgdb-verif-sim-q97e";

const fn later(bead: &'static str) -> Reachability {
    Reachability::NotYetBuilt { bead }
}

/// The fixed target list of plan line 1132, in the order the line spells it.
///
/// Reachable rows are exactly the filesystem faults [`crate::vfs`] can inject.
/// Everything else is declared and unreachable — deliberately present, so the
/// denominator is the plan's and not ours.
pub static TARGETS: &[LdfiTarget] = &[
    // "every file/directory action in D1/D2"
    LdfiTarget {
        id: "d1-file-write",
        source_phrase: "every file/directory action in D1/D2",
        reachability: Reachability::Reachable,
    },
    LdfiTarget {
        id: "d1-file-sync",
        source_phrase: "every file/directory action in D1/D2",
        reachability: Reachability::Reachable,
    },
    LdfiTarget {
        id: "d2-file-write",
        source_phrase: "every file/directory action in D1/D2",
        reachability: Reachability::Reachable,
    },
    LdfiTarget {
        id: "d2-file-sync",
        source_phrase: "every file/directory action in D1/D2",
        reachability: Reachability::Reachable,
    },
    LdfiTarget {
        id: "directory-sync",
        source_phrase: "every file/directory action in D1/D2",
        // Chronicle syncs the directory through std::fs, not through a Vfs,
        // so there is no seam to inject at until step 1 of fgdb-1xtp lands.
        reachability: later(W2),
    },
    // "every ordered, certificate, external-CAS, or physical side-effect
    //  boundary in dual-root publication"
    LdfiTarget {
        id: "dual-root-ordered-boundary",
        source_phrase: "ordered ... boundary in dual-root publication",
        reachability: later(W2),
    },
    LdfiTarget {
        id: "dual-root-certificate-boundary",
        source_phrase: "certificate ... boundary in dual-root publication",
        reachability: later(W2),
    },
    LdfiTarget {
        id: "dual-root-external-cas-boundary",
        source_phrase: "external-CAS ... boundary in dual-root publication",
        reachability: later(W2),
    },
    LdfiTarget {
        id: "dual-root-physical-side-effect-boundary",
        source_phrase: "physical side-effect boundary in dual-root publication",
        reachability: later(W2),
    },
    // "attempt generation/ticket claim/statement-workspace publication and
    //  delivery"
    LdfiTarget {
        id: "attempt-generation",
        source_phrase: "attempt generation",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "ticket-claim",
        source_phrase: "ticket claim",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "statement-workspace-publication",
        source_phrase: "statement-workspace publication",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "statement-workspace-delivery",
        source_phrase: "statement-workspace ... delivery",
        reachability: later(W12),
    },
    // "checkpoint install/provisional-cut activation"
    LdfiTarget {
        id: "checkpoint-install",
        source_phrase: "checkpoint install",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "provisional-cut-activation",
        source_phrase: "provisional-cut activation",
        reachability: later(W12),
    },
    // "prepared ownership and Raft"
    LdfiTarget {
        id: "prepared-ownership",
        source_phrase: "prepared ownership",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "raft",
        source_phrase: "prepared ownership and Raft",
        reachability: later(W12),
    },
    // "remote release"
    LdfiTarget {
        id: "remote-release",
        source_phrase: "remote release",
        reachability: later(W12),
    },
    // "key stage/activate/zero/destroy/physical completion"
    LdfiTarget {
        id: "key-stage",
        source_phrase: "key stage",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "key-activate",
        source_phrase: "key ... activate",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "key-zero",
        source_phrase: "key ... zero",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "key-destroy",
        source_phrase: "key ... destroy",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "key-physical-completion",
        source_phrase: "key ... physical completion",
        reachability: later(W12),
    },
    // "GC preflight/authorization/quarantine/member completion"
    LdfiTarget {
        id: "gc-preflight",
        source_phrase: "GC preflight",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "gc-authorization",
        source_phrase: "GC ... authorization",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "gc-quarantine",
        source_phrase: "GC ... quarantine",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "gc-member-completion",
        source_phrase: "GC ... member completion",
        reachability: later(W12),
    },
    // "backup pin/copy/reopen/publish/release"
    LdfiTarget {
        id: "backup-pin",
        source_phrase: "backup pin",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "backup-copy",
        source_phrase: "backup ... copy",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "backup-reopen",
        source_phrase: "backup ... reopen",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "backup-publish",
        source_phrase: "backup ... publish",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "backup-release",
        source_phrase: "backup ... release",
        reachability: later(W12),
    },
    // "restore reservation/transform/reconciliation/hidden activation/
    //  visibility/service preparation/continuity-plus-catalog receipt/
    //  finalize/open/reopen/completion"
    LdfiTarget {
        id: "restore-reservation",
        source_phrase: "restore reservation",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "restore-transform",
        source_phrase: "restore ... transform",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "restore-reconciliation",
        source_phrase: "restore ... reconciliation",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "restore-hidden-activation",
        source_phrase: "restore ... hidden activation",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "restore-visibility",
        source_phrase: "restore ... visibility",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "restore-service-preparation",
        source_phrase: "restore ... service preparation",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "restore-continuity-plus-catalog-receipt",
        source_phrase: "restore ... continuity-plus-catalog receipt",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "restore-finalize",
        source_phrase: "restore ... finalize",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "restore-open",
        source_phrase: "restore ... open",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "restore-reopen",
        source_phrase: "restore ... reopen",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "restore-completion",
        source_phrase: "restore ... completion",
        reachability: later(W12),
    },
    // "Local-to-W12 seal/activation/authority-transfer/retirement"
    LdfiTarget {
        id: "local-to-w12-seal",
        source_phrase: "Local-to-W12 seal",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "local-to-w12-activation",
        source_phrase: "Local-to-W12 ... activation",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "local-to-w12-authority-transfer",
        source_phrase: "Local-to-W12 ... authority-transfer",
        reachability: later(W12),
    },
    LdfiTarget {
        id: "local-to-w12-retirement",
        source_phrase: "Local-to-W12 ... retirement",
        reachability: later(W12),
    },
];

/// How many declared targets the harness can fault today.
#[must_use]
pub fn reachable_count() -> usize {
    TARGETS
        .iter()
        .filter(|target| target.reachability.is_reachable())
        .count()
}

/// How many declared targets have no injection point yet.
///
/// This is the honest coverage gap. It is a function rather than a constant so
/// it cannot drift from [`TARGETS`], and it is public because a campaign
/// summary that omits it is reporting coverage over a denominator it chose.
#[must_use]
pub fn unreachable_count() -> usize {
    TARGETS.len() - reachable_count()
}

/// Coverage over the **plan's** denominator, as a sentence for a report.
///
/// Deliberately not a bare percentage: the interesting quantity is the gap and
/// who owns it, and a lone "9%" invites rounding into "we have LDFI".
#[must_use]
pub fn coverage_statement() -> String {
    format!(
        "{} of {} declared LDFI targets are reachable at this HEAD; {} have no injection point yet",
        reachable_count(),
        TARGETS.len(),
        unreachable_count()
    )
}
