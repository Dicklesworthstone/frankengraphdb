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
//! # What the registry is not
//!
//! The table is the target *inventory*, not proof that the injector covered a
//! row. The executable adapter below consumes successful-run trace points,
//! delegates causal-cone and minimal-hitting-set work to asupersync, and maps a
//! hypothesis back to an exact [`crate::vfs::FaultPlan`]. A reachable row still
//! means only that a witnessed injection point exists; campaign evidence is a
//! separate result.

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
///
/// fgdb-1xtp's rows were re-derived 2026-08-05 after its three steps landed
/// (async migration 9b80da3, FaultVfs bac511b, crash-matrix re-expression
/// 8876ea4): the file-level dual-root boundaries flipped to reachable with
/// witnesses in `root_store_durability.rs`. `directory-sync` followed the
/// same day when fgdb-3a3u landed the dirent model, retiring the
/// `DIRENT_MODEL` cluster, and the two remaining dual-root boundaries
/// (certificate, external-CAS) flipped when fgdb-1dgm landed the evidence
/// reread and continuity seam (45ea028), retiring `DUAL_ROOT_MACHINERY`;
/// the remaining unreachable rows name the bead that owns their gap.
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
        // FaultVfs models dirent durability (fgdb-3a3u): namespace operations
        // stay pending until their parent directory honestly syncs,
        // `FaultPlan::dirent_lie` can lie at exactly that barrier, and
        // `FaultPlan::dirent_loss` decides per pending operation whether a
        // crash rolls the name back. Witnessed in `tests/lab_vfs.rs` (a
        // lying directory sync loses a synced file's name across a crash;
        // the honest control keeps it).
        reachability: Reachability::Reachable,
    },
    // "every ordered, certificate, external-CAS, or physical side-effect
    //  boundary in dual-root publication"
    LdfiTarget {
        id: "dual-root-ordered-boundary",
        source_phrase: "ordered ... boundary in dual-root publication",
        // The write-inactive-slot-then-sync ordering of `RootStore` runs
        // through `RootStore::with_vfs`, so the fault model can lie at the
        // barrier that makes the ordering durable. Witnessed in
        // `tests/sim_ldfi.rs` (fsync lie loses the publish; recovery selects
        // the prior generation).
        reachability: Reachability::Reachable,
    },
    LdfiTarget {
        id: "dual-root-certificate-boundary",
        source_phrase: "certificate ... boundary in dual-root publication",
        // The certificate machinery exists since 45ea028:
        // `RootStore::publish_evidenced` mints `RootPublicationEvidence`
        // from a post-barrier reread, so a fault on the publish flush
        // (bit_flip) makes the reread refuse and no evidence exists —
        // witnessed in `tests/sim_ldfi.rs`
        // (`damaged_publish_bytes_mint_no_certificate_*`).
        reachability: Reachability::Reachable,
    },
    LdfiTarget {
        id: "dual-root-external-cas-boundary",
        source_phrase: "external-CAS ... boundary in dual-root publication",
        // `RootStore::publish_with_continuity` (45ea028) revalidates the
        // exact external head before the irreversible slot write, through
        // the `ContinuityAuthority` seam a lab CAS register implements —
        // stale, forked, and absent heads are injectable data, witnessed in
        // `tests/sim_ldfi.rs` (`a_stale_forked_or_absent_continuity_head_*`).
        reachability: Reachability::Reachable,
    },
    LdfiTarget {
        id: "dual-root-physical-side-effect-boundary",
        source_phrase: "physical side-effect boundary in dual-root publication",
        // The physical action of the landed protocol is the slot write and
        // its sync, which go through the Vfs and can be refused (ENOSPC) or
        // lied to. Witnessed beside the ordered-boundary witness.
        reachability: Reachability::Reachable,
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

// ---------------------------------------------------------------------------
// Successful trace -> minimal fault hypotheses -> executable FaultPlan
// ---------------------------------------------------------------------------

use crate::vfs::{FAULT_POINT_TRACE_PREFIX, FaultPlan, Trigger};
use asupersync::lab::ldfi::{
    FaultEventId, HittingSetBudget, HittingSetResult, LdfiExperimentBudget,
    LdfiExperimentObservation, LdfiExperimentReport, SupportGraph,
};
use asupersync::lab::ldfi_trace::{TraceLineageConfig, build_causal_lineage};
use asupersync::trace::{TraceData, TraceEvent};
use std::collections::{BTreeMap, BTreeSet};

/// One fault class that the current [`FaultPlan`] can target by eligible
/// operation ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InjectableFaultClass {
    /// A file sync acknowledges bytes it did not persist.
    FsyncLie,
    /// An interior sector is lost during a file sync.
    TornWrite,
    /// A durable byte is damaged after write-through.
    BitFlip,
    /// A directory sync acknowledges names it did not settle.
    DirentSyncLie,
    /// A pending namespace operation is lost at crash.
    DirentLoss,
    /// An eligible durability boundary is delayed through the lab clock.
    Latency,
}

impl InjectableFaultClass {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "fsync-lie" => Some(Self::FsyncLie),
            "torn-write" => Some(Self::TornWrite),
            "bit-flip" => Some(Self::BitFlip),
            "dirent-sync-lie" => Some(Self::DirentSyncLie),
            "dirent-loss" => Some(Self::DirentLoss),
            "latency" => Some(Self::Latency),
            _ => None,
        }
    }

    fn install(self, plan: &mut FaultPlan, trigger: Trigger, latency_micros: u64) {
        match self {
            Self::FsyncLie => plan.fsync_lie = trigger,
            Self::TornWrite => plan.torn_write = trigger,
            Self::BitFlip => plan.bit_flip = trigger,
            Self::DirentSyncLie => plan.dirent_lie = trigger,
            Self::DirentLoss => plan.dirent_loss = trigger,
            Self::Latency => {
                plan.latency = trigger;
                plan.latency_micros = latency_micros;
            }
        }
    }
}

/// A faultable event recovered from one successful asupersync trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TracedFaultPoint {
    /// The asupersync trace sequence number used by the LDFI core.
    pub event: FaultEventId,
    /// Which fault class was eligible.
    pub class: InjectableFaultClass,
    /// One-based ordinal within that class for this [`FaultVfs`](crate::vfs::FaultVfs).
    pub ordinal: u64,
}

/// One minimal hypothesis from asupersync, enriched with the VFS injection
/// coordinates needed to execute it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultHypothesis {
    /// The exact minimal event set produced by asupersync.
    pub events: BTreeSet<FaultEventId>,
    /// The corresponding VFS class/ordinal points, in trace order.
    pub points: Vec<TracedFaultPoint>,
}

/// Why a minimal event hypothesis cannot be represented exactly by today's
/// `FaultPlan` trigger vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanMappingError {
    /// One `FaultPlan` field can name only one exact ordinal, so two distinct
    /// ordinals of the same class cannot be encoded as exactly those two
    /// events. Executing a broader plan would no longer test the hypothesis as
    /// generated.
    RepeatedClass {
        /// The class that appeared more than once.
        class: InjectableFaultClass,
    },
    /// `Trigger::At` currently stores `u32`; this trace ran longer than that
    /// durable replay vocabulary can name.
    OrdinalOutOfRange {
        /// The unrepresentable trace point.
        point: TracedFaultPoint,
    },
}

impl std::fmt::Display for PlanMappingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepeatedClass { class } => write!(
                f,
                "minimal hypothesis contains multiple {class:?} ordinals; FaultPlan cannot target that exact set"
            ),
            Self::OrdinalOutOfRange { point } => write!(
                f,
                "fault point {:?} ordinal {} exceeds Trigger::At(u32)",
                point.class, point.ordinal
            ),
        }
    }
}

impl std::error::Error for PlanMappingError {}

impl FaultHypothesis {
    /// Translate this exact hypothesis into an executable plan.
    ///
    /// The mapping refuses any set the current trigger vocabulary would
    /// broaden. `latency_micros` supplies the deterministic delay for latency
    /// points; [`crate::artifact::Replay::run`] executes it on its runtime clock.
    pub fn to_plan(&self, seed: u64, latency_micros: u64) -> Result<FaultPlan, PlanMappingError> {
        let mut plan = FaultPlan {
            seed,
            ..FaultPlan::faultless()
        };
        let mut classes = BTreeSet::new();
        for point in &self.points {
            if !classes.insert(point.class) {
                return Err(PlanMappingError::RepeatedClass { class: point.class });
            }
            let ordinal = u32::try_from(point.ordinal)
                .map_err(|_| PlanMappingError::OrdinalOutOfRange { point: *point })?;
            point
                .class
                .install(&mut plan, Trigger::At(ordinal), latency_micros);
        }
        Ok(plan)
    }
}

/// The honestly scoped result of LDFI over one successful trace corpus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceLdfi {
    /// Upstream search result, including truncation and per-corpus coverage
    /// semantics.
    pub search: HittingSetResult,
    /// Minimal hypotheses enriched with executable VFS coordinates.
    pub hypotheses: Vec<FaultHypothesis>,
    /// Number of trace events supplied by the successful run.
    pub source_event_count: usize,
    /// Number of recognised VFS fault points in that trace.
    pub fault_point_count: usize,
    /// Number of events that independently asserted the requested outcome.
    pub outcome_count: usize,
}

impl TraceLdfi {
    /// Execute the generated hypotheses using asupersync's deterministic
    /// experiment-loop admission and stop policy.
    pub fn run_experiments<F>(
        &self,
        budget: LdfiExperimentBudget,
        mut experiment: F,
    ) -> LdfiExperimentReport
    where
        F: FnMut(&FaultHypothesis) -> LdfiExperimentObservation,
    {
        self.search.run_experiments(budget, |events| {
            let hypothesis = self
                .hypotheses
                .iter()
                .find(|hypothesis| &hypothesis.events == events)
                .expect("TraceLdfi hypotheses are a total enrichment of the upstream result");
            experiment(hypothesis)
        })
    }
}

/// Why a successful trace could not be admitted to the LDFI search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceLdfiError {
    /// The trace contained no event with the caller's exact outcome message.
    MissingOutcome {
        /// The requested stable outcome marker.
        message: String,
    },
    /// The trace contained no instrumented VFS fault point, so claiming a
    /// lineage-derived campaign would be vacuous.
    MissingFaultPoints,
    /// A versioned fault-point event was present but malformed. Ignoring it
    /// would silently shrink the search space.
    MalformedFaultPoint {
        /// Trace sequence number of the malformed event.
        event: u64,
        /// Recorded message.
        message: String,
    },
}

impl std::fmt::Display for TraceLdfiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingOutcome { message } => {
                write!(f, "successful trace has no outcome event {message:?}")
            }
            Self::MissingFaultPoints => f.write_str("successful trace has no VFS fault points"),
            Self::MalformedFaultPoint { event, message } => {
                write!(
                    f,
                    "trace event {event} has malformed fault point {message:?}"
                )
            }
        }
    }
}

impl std::error::Error for TraceLdfiError {}

fn trace_message(event: &TraceEvent) -> Option<&str> {
    match &event.data {
        TraceData::Message(message) => Some(message),
        _ => None,
    }
}

fn parse_fault_point(event: &TraceEvent) -> Result<Option<TracedFaultPoint>, TraceLdfiError> {
    let Some(message) = trace_message(event) else {
        return Ok(None);
    };
    let Some(encoded) = message.strip_prefix(FAULT_POINT_TRACE_PREFIX) else {
        return Ok(None);
    };
    let Some((class, ordinal)) = encoded.rsplit_once(':') else {
        return Err(TraceLdfiError::MalformedFaultPoint {
            event: event.seq,
            message: message.to_string(),
        });
    };
    let Some(class) = InjectableFaultClass::parse(class) else {
        return Err(TraceLdfiError::MalformedFaultPoint {
            event: event.seq,
            message: message.to_string(),
        });
    };
    let Ok(ordinal) = ordinal.parse::<u64>() else {
        return Err(TraceLdfiError::MalformedFaultPoint {
            event: event.seq,
            message: message.to_string(),
        });
    };
    if ordinal == 0 {
        return Err(TraceLdfiError::MalformedFaultPoint {
            event: event.seq,
            message: message.to_string(),
        });
    }
    Ok(Some(TracedFaultPoint {
        event: FaultEventId::new(event.seq),
        class,
        ordinal,
    }))
}

/// Derive minimal, executable fault hypotheses from one successful lab trace.
///
/// Only the versioned FrankenGraphDB VFS markers are faultable in the derived
/// graph. Other asupersync events still carry causality, but cannot accidentally
/// turn into a `FaultPlan` action with no adapter. Because asupersync correctly
/// refuses to infer happens-before from scalar Lamport counters and
/// `TraceData::Message` carries no task id, the adapter conservatively adds
/// every preceding VFS marker as a predecessor of the caller's explicit outcome
/// marker. This over-approximation can schedule extra experiments; it cannot
/// omit a prior faultable boundary. The result is per trace and per budget; it
/// is not a universal correctness certificate.
pub fn derive_fault_hypotheses(
    events: &[TraceEvent],
    outcome_message: &str,
    budget: HittingSetBudget,
) -> Result<TraceLdfi, TraceLdfiError> {
    let mut points = BTreeMap::new();
    for event in events {
        if let Some(point) = parse_fault_point(event)? {
            points.insert(point.event, point);
        }
    }
    if points.is_empty() {
        return Err(TraceLdfiError::MissingFaultPoints);
    }

    let outcomes: Vec<FaultEventId> = events
        .iter()
        .filter(|event| trace_message(event) == Some(outcome_message))
        .map(|event| FaultEventId::new(event.seq))
        .collect();
    if outcomes.is_empty() {
        return Err(TraceLdfiError::MissingOutcome {
            message: outcome_message.to_string(),
        });
    }

    let mut lineage = build_causal_lineage(events, TraceLineageConfig::default());
    // The upstream adapter has a useful general default faultability policy,
    // but FrankenGraphDB can execute only its own versioned VFS markers. Demote
    // everything first, then admit precisely the events with a total mapping.
    for event in events {
        lineage.add_event(FaultEventId::new(event.seq), false);
    }
    for event in points.keys() {
        lineage.mark_faultable(*event);
    }
    for outcome in &outcomes {
        for point in points.keys().filter(|point| point.get() < outcome.get()) {
            lineage.add_happens_before(*point, *outcome);
        }
    }

    let graph = SupportGraph::from_causal_cones(&lineage, outcomes.iter().copied());
    let search = graph.minimal_hitting_sets(budget);
    let hypotheses = search
        .hypotheses
        .iter()
        .map(|events| FaultHypothesis {
            events: events.clone(),
            points: events
                .iter()
                .map(|event| {
                    *points
                        .get(event)
                        .expect("only admitted fault points can appear in a hypothesis")
                })
                .collect(),
        })
        .collect();

    Ok(TraceLdfi {
        search,
        hypotheses,
        source_event_count: events.len(),
        fault_point_count: points.len(),
        outcome_count: outcomes.len(),
    })
}
