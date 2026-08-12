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
    /// The executable campaign does not exist yet. Names the phase bead that
    /// must activate it, so the gap has an owner rather than being a silent
    /// zero.
    NotYetBuilt {
        /// The phase bead that will make this target reachable.
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

/// Whether a declared target is currently campaign evidence, future work, or
/// deliberately unavailable under the selected feature/posture contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetRowState {
    /// The owning implementation or campaign has not activated the row yet.
    Pending,
    /// The injection point exists and has an executable coverage witness.
    Live,
    /// The target is intentionally unavailable under the active feature set.
    Disabled,
}

impl TargetRowState {
    /// Stable spelling used by the machine-readable registry stream.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Live => "live",
            Self::Disabled => "disabled",
        }
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
    /// Bead that alone may turn this row into executable campaign coverage.
    pub phase_owner_bead: &'static str,
    /// First gate at which the row is required to be live.
    pub first_required_gate: &'static str,
    /// Whether the owning product implementation exists at this HEAD.
    pub implementation_enabled: bool,
    /// Current registry state. Pending rows never count as coverage.
    pub row_state: TargetRowState,
    /// Exact executable witness for a live row; absent otherwise.
    pub coverage_evidence_ref: Option<&'static str>,
}

/// The four phase boundaries represented in the base registry.
///
/// q97e owns only the reusable harness and its current filesystem witnesses.
/// Local/G1, G3, and W12 fault-execution campaigns are delegated to their
/// dedicated torture beads; the base harness must never report them as covered.
pub const BASE_HARNESS_OWNER: &str = "fgdb-verif-sim-q97e";
pub const LOCAL_TORTURE_OWNER: &str = "fgdb-verif-torture-ddcl";
pub const G3_PHASE_OWNER: &str = "fgdb-g3-protocol-ha-torture-jni4";
pub const W12_PHASE_OWNER: &str = "fgdb-w12-formal-torture-ejx0";
pub const GENESIS_GATE: &str = "fgdb-gate-genesis-lce";
pub const G1_GATE: &str = "fgdb-gate-g1-6vc";
pub const G3_GATE: &str = "fgdb-gate-g3-30m";
pub const W12_GATE: &str = "fgdb-gate-w12-w2y";

/// Exact owner universe whose tracker completion is coupled to this registry.
pub const EXPECTED_LDFI_OWNER_BEADS: &[&str] = &[
    BASE_HARNESS_OWNER,
    LOCAL_TORTURE_OWNER,
    G3_PHASE_OWNER,
    W12_PHASE_OWNER,
];

const fn live(
    id: &'static str,
    source_phrase: &'static str,
    coverage_evidence_ref: &'static str,
) -> LdfiTarget {
    LdfiTarget {
        id,
        source_phrase,
        reachability: Reachability::Reachable,
        phase_owner_bead: BASE_HARNESS_OWNER,
        first_required_gate: GENESIS_GATE,
        implementation_enabled: true,
        row_state: TargetRowState::Live,
        coverage_evidence_ref: Some(coverage_evidence_ref),
    }
}

const fn delegated(
    id: &'static str,
    source_phrase: &'static str,
    phase_owner_bead: &'static str,
    first_required_gate: &'static str,
) -> LdfiTarget {
    LdfiTarget {
        id,
        source_phrase,
        reachability: Reachability::NotYetBuilt {
            bead: phase_owner_bead,
        },
        phase_owner_bead,
        first_required_gate,
        implementation_enabled: false,
        row_state: TargetRowState::Pending,
        coverage_evidence_ref: None,
    }
}

/// The complete normative LDFI target-id inventory, in plan order.
///
/// Whole-registry validation compares [`TARGETS`] against this independent
/// list. A missing pending row must therefore fail rather than silently shrink
/// the coverage denominator.
pub static EXPECTED_TARGET_IDS: &[&str] = &[
    "d1-file-write",
    "d1-file-sync",
    "d2-file-write",
    "d2-file-sync",
    "directory-sync",
    "dual-root-ordered-boundary",
    "dual-root-certificate-boundary",
    "dual-root-external-cas-boundary",
    "dual-root-physical-side-effect-boundary",
    "attempt-generation",
    "ticket-claim",
    "statement-workspace-publication",
    "statement-workspace-delivery",
    "checkpoint-install",
    "provisional-cut-activation",
    "prepared-ownership",
    "raft",
    "remote-release",
    "key-stage",
    "key-activate",
    "key-zero",
    "key-destroy",
    "key-physical-completion",
    "gc-preflight",
    "gc-authorization",
    "gc-quarantine",
    "gc-member-completion",
    "backup-pin",
    "backup-copy",
    "backup-reopen",
    "backup-publish",
    "backup-release",
    "restore-reservation",
    "restore-transform",
    "restore-reconciliation",
    "restore-hidden-activation",
    "restore-visibility",
    "restore-service-preparation",
    "restore-continuity-plus-catalog-receipt",
    "restore-finalize",
    "restore-open",
    "restore-reopen",
    "restore-completion",
    "local-to-w12-seal",
    "local-to-w12-activation",
    "local-to-w12-authority-transfer",
    "local-to-w12-retirement",
];

/// The fixed target list of plan line 1132, in the order the line spells it.
///
/// Reachable rows are exactly the filesystem faults [`crate::vfs`] can inject.
/// Everything else is declared and unreachable — deliberately present, so the
/// denominator is the plan's and not ours.
pub static TARGETS: &[LdfiTarget] = &[
    // "every file/directory action in D1/D2"
    live(
        "d1-file-write",
        "every file/directory action in D1/D2",
        "crates/fgdb-sim/tests/durability_semantics_e2e.rs::d1_file_write_enospc_refuses_before_marker_publication_and_recovers_prefix",
    ),
    live(
        "d1-file-sync",
        "every file/directory action in D1/D2",
        "crates/fgdb-sim/tests/durability_semantics_e2e.rs::a_one_shot_d1_fsync_lie_is_reinforced_before_marker_publication",
    ),
    live(
        "d2-file-write",
        "every file/directory action in D1/D2",
        "crates/fgdb-sim/tests/durability_semantics_e2e.rs::d2_file_write_enospc_refuses_before_acknowledgement_and_recovers_prefix",
    ),
    live(
        "d2-file-sync",
        "every file/directory action in D1/D2",
        "crates/fgdb-sim/tests/durability_semantics_e2e.rs::a_one_shot_d2_fsync_lie_is_reinforced_before_acknowledgement",
    ),
    live(
        "directory-sync",
        "every file/directory action in D1/D2",
        "crates/fgdb-sim/tests/lab_vfs.rs::a_lying_directory_sync_settles_nothing",
    ),
    // "every ordered, certificate, external-CAS, or physical side-effect
    //  boundary in dual-root publication"
    live(
        "dual-root-ordered-boundary",
        "ordered ... boundary in dual-root publication",
        "crates/fgdb-sim/tests/sim_ldfi.rs::a_lying_publish_sync_is_caught_by_the_reread_and_loses_cleanly",
    ),
    live(
        "dual-root-certificate-boundary",
        "certificate ... boundary in dual-root publication",
        "crates/fgdb-sim/tests/sim_ldfi.rs::damaged_publish_bytes_mint_no_certificate_and_the_prior_root_survives",
    ),
    live(
        "dual-root-external-cas-boundary",
        "external-CAS ... boundary in dual-root publication",
        "crates/fgdb-sim/tests/sim_ldfi.rs::a_stale_forked_or_absent_continuity_head_refuses_before_the_slot_write",
    ),
    live(
        "dual-root-physical-side-effect-boundary",
        "physical side-effect boundary in dual-root publication",
        "crates/fgdb-sim/tests/sim_ldfi.rs::enospc_refuses_the_publish_and_the_prior_root_survives",
    ),
    // "attempt generation/ticket claim/statement-workspace publication and
    //  delivery"
    delegated(
        "attempt-generation",
        "attempt generation",
        W12_PHASE_OWNER,
        W12_GATE,
    ),
    delegated("ticket-claim", "ticket claim", W12_PHASE_OWNER, W12_GATE),
    delegated(
        "statement-workspace-publication",
        "statement-workspace publication",
        W12_PHASE_OWNER,
        W12_GATE,
    ),
    delegated(
        "statement-workspace-delivery",
        "statement-workspace ... delivery",
        W12_PHASE_OWNER,
        W12_GATE,
    ),
    // "checkpoint install/provisional-cut activation"
    delegated(
        "checkpoint-install",
        "checkpoint install",
        LOCAL_TORTURE_OWNER,
        G1_GATE,
    ),
    delegated(
        "provisional-cut-activation",
        "provisional-cut activation",
        LOCAL_TORTURE_OWNER,
        G1_GATE,
    ),
    // "prepared ownership and Raft"
    delegated(
        "prepared-ownership",
        "prepared ownership",
        LOCAL_TORTURE_OWNER,
        G1_GATE,
    ),
    delegated(
        "raft",
        "prepared ownership and Raft",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    // "remote release"
    delegated(
        "remote-release",
        "remote release",
        W12_PHASE_OWNER,
        W12_GATE,
    ),
    // "key stage/activate/zero/destroy/physical completion"
    delegated("key-stage", "key stage", G3_PHASE_OWNER, G3_GATE),
    delegated("key-activate", "key ... activate", G3_PHASE_OWNER, G3_GATE),
    delegated("key-zero", "key ... zero", G3_PHASE_OWNER, G3_GATE),
    delegated("key-destroy", "key ... destroy", G3_PHASE_OWNER, G3_GATE),
    delegated(
        "key-physical-completion",
        "key ... physical completion",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    // "GC preflight/authorization/quarantine/member completion"
    delegated("gc-preflight", "GC preflight", G3_PHASE_OWNER, G3_GATE),
    delegated(
        "gc-authorization",
        "GC ... authorization",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "gc-quarantine",
        "GC ... quarantine",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "gc-member-completion",
        "GC ... member completion",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    // "backup pin/copy/reopen/publish/release"
    delegated("backup-pin", "backup pin", G3_PHASE_OWNER, G3_GATE),
    delegated("backup-copy", "backup ... copy", G3_PHASE_OWNER, G3_GATE),
    delegated(
        "backup-reopen",
        "backup ... reopen",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "backup-publish",
        "backup ... publish",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "backup-release",
        "backup ... release",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    // "restore reservation/transform/reconciliation/hidden activation/
    //  visibility/service preparation/continuity-plus-catalog receipt/
    //  finalize/open/reopen/completion"
    delegated(
        "restore-reservation",
        "restore reservation",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "restore-transform",
        "restore ... transform",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "restore-reconciliation",
        "restore ... reconciliation",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "restore-hidden-activation",
        "restore ... hidden activation",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "restore-visibility",
        "restore ... visibility",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "restore-service-preparation",
        "restore ... service preparation",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "restore-continuity-plus-catalog-receipt",
        "restore ... continuity-plus-catalog receipt",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "restore-finalize",
        "restore ... finalize",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated("restore-open", "restore ... open", G3_PHASE_OWNER, G3_GATE),
    delegated(
        "restore-reopen",
        "restore ... reopen",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    delegated(
        "restore-completion",
        "restore ... completion",
        G3_PHASE_OWNER,
        G3_GATE,
    ),
    // "Local-to-W12 seal/activation/authority-transfer/retirement"
    delegated(
        "local-to-w12-seal",
        "Local-to-W12 seal",
        W12_PHASE_OWNER,
        W12_GATE,
    ),
    delegated(
        "local-to-w12-activation",
        "Local-to-W12 ... activation",
        W12_PHASE_OWNER,
        W12_GATE,
    ),
    delegated(
        "local-to-w12-authority-transfer",
        "Local-to-W12 ... authority-transfer",
        W12_PHASE_OWNER,
        W12_GATE,
    ),
    delegated(
        "local-to-w12-retirement",
        "Local-to-W12 ... retirement",
        W12_PHASE_OWNER,
        W12_GATE,
    ),
];

/// Return the only owner/gate pair permitted for one normative target id.
///
/// This match is deliberately independent of the row metadata. It prevents a
/// row from validating merely because it carries *some* registered phase pair,
/// and it makes an invented target id fail closed before reporting or
/// activation can consume it.
#[must_use]
pub fn expected_phase_boundary(target_id: &str) -> Option<(&'static str, &'static str)> {
    match target_id {
        "d1-file-write"
        | "d1-file-sync"
        | "d2-file-write"
        | "d2-file-sync"
        | "directory-sync"
        | "dual-root-ordered-boundary"
        | "dual-root-certificate-boundary"
        | "dual-root-external-cas-boundary"
        | "dual-root-physical-side-effect-boundary" => Some((BASE_HARNESS_OWNER, GENESIS_GATE)),
        "checkpoint-install" | "provisional-cut-activation" | "prepared-ownership" => {
            Some((LOCAL_TORTURE_OWNER, G1_GATE))
        }
        "raft"
        | "key-stage"
        | "key-activate"
        | "key-zero"
        | "key-destroy"
        | "key-physical-completion"
        | "gc-preflight"
        | "gc-authorization"
        | "gc-quarantine"
        | "gc-member-completion"
        | "backup-pin"
        | "backup-copy"
        | "backup-reopen"
        | "backup-publish"
        | "backup-release"
        | "restore-reservation"
        | "restore-transform"
        | "restore-reconciliation"
        | "restore-hidden-activation"
        | "restore-visibility"
        | "restore-service-preparation"
        | "restore-continuity-plus-catalog-receipt"
        | "restore-finalize"
        | "restore-open"
        | "restore-reopen"
        | "restore-completion" => Some((G3_PHASE_OWNER, G3_GATE)),
        "attempt-generation"
        | "ticket-claim"
        | "statement-workspace-publication"
        | "statement-workspace-delivery"
        | "remote-release"
        | "local-to-w12-seal"
        | "local-to-w12-activation"
        | "local-to-w12-authority-transfer"
        | "local-to-w12-retirement" => Some((W12_PHASE_OWNER, W12_GATE)),
        _ => None,
    }
}

/// A structural inconsistency inside one target row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetMetadataError {
    /// The phase owner is not a Bead id.
    MalformedOwner,
    /// The first required gate is not a Bead id.
    MalformedGate,
    /// A live row says its implementation is unavailable.
    LiveImplementationDisabled,
    /// A live row has no reachable injection point.
    LiveTargetUnreachable,
    /// A live row has no executable evidence reference.
    LiveEvidenceMissing,
    /// A pending or disabled row is marked reachable.
    InactiveTargetReachable,
    /// A pending or disabled row says its implementation is enabled.
    InactiveImplementationEnabled,
    /// A pending or disabled row carries coverage evidence.
    InactiveEvidencePresent,
    /// The reachability owner and phase owner disagree.
    InactiveOwnerMismatch,
    /// The target id is not part of the fixed normative inventory.
    UnknownTargetId,
    /// The target carries a different owner/gate pair than its exact mapping.
    PhaseBoundaryMismatch,
}

/// Check the row-state/owner/evidence closure for one target.
///
/// This is public so downstream campaign registries can reject a malformed
/// base row instead of silently interpreting it.
pub fn validate_target_metadata(target: &LdfiTarget) -> Result<(), TargetMetadataError> {
    if !target.phase_owner_bead.starts_with("fgdb-") {
        return Err(TargetMetadataError::MalformedOwner);
    }
    if !target.first_required_gate.starts_with("fgdb-gate-") {
        return Err(TargetMetadataError::MalformedGate);
    }
    let expected =
        expected_phase_boundary(target.id).ok_or(TargetMetadataError::UnknownTargetId)?;
    if (target.phase_owner_bead, target.first_required_gate) != expected {
        return Err(TargetMetadataError::PhaseBoundaryMismatch);
    }

    match target.row_state {
        TargetRowState::Live => {
            if !target.implementation_enabled {
                return Err(TargetMetadataError::LiveImplementationDisabled);
            }
            if !matches!(target.reachability, Reachability::Reachable) {
                return Err(TargetMetadataError::LiveTargetUnreachable);
            }
            if target.coverage_evidence_ref.is_none_or(str::is_empty) {
                return Err(TargetMetadataError::LiveEvidenceMissing);
            }
        }
        TargetRowState::Pending | TargetRowState::Disabled => {
            let Reachability::NotYetBuilt { bead } = target.reachability else {
                return Err(TargetMetadataError::InactiveTargetReachable);
            };
            if target.implementation_enabled {
                return Err(TargetMetadataError::InactiveImplementationEnabled);
            }
            if target.coverage_evidence_ref.is_some() {
                return Err(TargetMetadataError::InactiveEvidencePresent);
            }
            if bead != target.phase_owner_bead {
                return Err(TargetMetadataError::InactiveOwnerMismatch);
            }
        }
    }
    Ok(())
}

/// Why the registry as a whole is not safe to report or activate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryValidationError {
    /// One row violates the owner/gate/state/evidence closure.
    InvalidRow {
        /// Stable target id of the invalid row.
        target_id: &'static str,
        /// Exact row-level violation.
        error: TargetMetadataError,
    },
    /// Two rows use the same stable target id.
    DuplicateTargetId {
        /// Repeated target id.
        target_id: &'static str,
    },
    /// The registry has fewer or more rows than the normative inventory.
    TargetInventoryLength {
        /// Normative row count.
        expected: usize,
        /// Observed row count.
        actual: usize,
    },
    /// A row does not occupy its normative plan-order position.
    TargetIdOrderMismatch {
        /// Zero-based position in the normative inventory.
        index: usize,
        /// Required stable target id.
        expected: &'static str,
        /// Observed stable target id.
        actual: &'static str,
    },
}

/// Tracker completion state supplied by the local CI adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LdfiOwnerCompletion {
    /// Exact Bead id from [`EXPECTED_LDFI_OWNER_BEADS`].
    pub owner_bead: &'static str,
    /// Whether the tracked Bead is closed.
    pub complete: bool,
}

/// Why the target registry and tracker owner state cannot coexist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LdfiOwnerCompletionError {
    /// Target-row structure is invalid before tracker state is considered.
    InvalidRegistry(RegistryValidationError),
    /// The adapter omitted or invented an owner.
    OwnerInventoryLength { expected: usize, actual: usize },
    /// The owner at one exact inventory position is wrong.
    OwnerInventoryId { index: usize },
    /// A closed owner still has a pending or unevidenced target row.
    CompletedOwnerMissingCampaign {
        owner_bead: &'static str,
        target_id: &'static str,
    },
}

/// Validate every row and the target-id uniqueness law before reporting any
/// portion of a registry.
pub fn validate_registry_rows(rows: &[LdfiTarget]) -> Result<(), RegistryValidationError> {
    let mut ids = std::collections::BTreeSet::new();
    for target in rows {
        validate_target_metadata(target).map_err(|error| RegistryValidationError::InvalidRow {
            target_id: target.id,
            error,
        })?;
        if !ids.insert(target.id) {
            return Err(RegistryValidationError::DuplicateTargetId {
                target_id: target.id,
            });
        }
    }
    if rows.len() != EXPECTED_TARGET_IDS.len() {
        return Err(RegistryValidationError::TargetInventoryLength {
            expected: EXPECTED_TARGET_IDS.len(),
            actual: rows.len(),
        });
    }
    for (index, (target, expected)) in rows.iter().zip(EXPECTED_TARGET_IDS).enumerate() {
        if target.id != *expected {
            return Err(RegistryValidationError::TargetIdOrderMismatch {
                index,
                expected,
                actual: target.id,
            });
        }
    }
    Ok(())
}

/// Refuse tracker completion over pending or unevidenced LDFI rows.
///
/// The owner list is an exact inventory so callers cannot omit a closed owner.
/// A future phase owner may remain open with explicit pending rows, but once
/// it closes every row assigned to it must be live and carry executable
/// evidence.
pub fn validate_ldfi_owner_completion(
    rows: &[LdfiTarget],
    owners: &[LdfiOwnerCompletion],
) -> Result<(), LdfiOwnerCompletionError> {
    validate_registry_rows(rows).map_err(LdfiOwnerCompletionError::InvalidRegistry)?;
    if owners.len() != EXPECTED_LDFI_OWNER_BEADS.len() {
        return Err(LdfiOwnerCompletionError::OwnerInventoryLength {
            expected: EXPECTED_LDFI_OWNER_BEADS.len(),
            actual: owners.len(),
        });
    }
    for (index, (owner, expected)) in owners.iter().zip(EXPECTED_LDFI_OWNER_BEADS).enumerate() {
        if owner.owner_bead != *expected {
            return Err(LdfiOwnerCompletionError::OwnerInventoryId { index });
        }
        if !owner.complete {
            continue;
        }
        if let Some(target) = rows.iter().find(|target| {
            target.phase_owner_bead == owner.owner_bead
                && (target.row_state != TargetRowState::Live
                    || target.coverage_evidence_ref.is_none_or(str::is_empty))
        }) {
            return Err(LdfiOwnerCompletionError::CompletedOwnerMissingCampaign {
                owner_bead: owner.owner_bead,
                target_id: target.id,
            });
        }
    }
    Ok(())
}

/// Why a request to activate campaign coverage was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationRejection {
    /// No declared target has the requested stable id.
    UnknownTarget,
    /// The caller is not the row's declared phase owner.
    WrongPhaseOwner,
    /// The caller is trying to activate the row at a different gate.
    WrongGate,
    /// The owning product implementation has not landed.
    ImplementationDisabled,
    /// The source row has not itself moved to the live state.
    RowNotLive,
    /// No coverage witness was supplied.
    MissingEvidence,
    /// The supplied witness does not byte-match the registered witness.
    EvidenceMismatch,
    /// The source row is internally inconsistent.
    InvalidMetadata(TargetMetadataError),
    /// Another row makes the complete registry unsafe to activate.
    InvalidRegistry(RegistryValidationError),
}

/// Validate an activation request against the authoritative base row.
///
/// Pending rows fail even if a caller supplies plausible-looking owner, gate,
/// and evidence strings: the owning implementation must first change the
/// source row to `implementation_enabled = true` and `row_state = live` in the
/// same reviewed change that adds its executable witness.
pub fn validate_activation(
    target_id: &str,
    phase_owner_bead: &str,
    first_required_gate: &str,
    coverage_evidence_ref: Option<&str>,
) -> Result<&'static LdfiTarget, ActivationRejection> {
    validate_registry_rows(TARGETS).map_err(ActivationRejection::InvalidRegistry)?;
    let target = TARGETS
        .iter()
        .find(|target| target.id == target_id)
        .ok_or(ActivationRejection::UnknownTarget)?;
    validate_target_metadata(target).map_err(ActivationRejection::InvalidMetadata)?;
    if phase_owner_bead != target.phase_owner_bead {
        return Err(ActivationRejection::WrongPhaseOwner);
    }
    if first_required_gate != target.first_required_gate {
        return Err(ActivationRejection::WrongGate);
    }
    if !target.implementation_enabled {
        return Err(ActivationRejection::ImplementationDisabled);
    }
    if target.row_state != TargetRowState::Live {
        return Err(ActivationRejection::RowNotLive);
    }
    let supplied = coverage_evidence_ref.ok_or(ActivationRejection::MissingEvidence)?;
    if target.coverage_evidence_ref != Some(supplied) {
        return Err(ActivationRejection::EvidenceMismatch);
    }
    Ok(target)
}

/// How the base harness routes one target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CampaignEntrypoint {
    /// q97e owns and executes this live witness.
    Covered {
        /// Exact executable evidence registered for the row.
        coverage_evidence_ref: &'static str,
    },
    /// Another phase owns this row; the base harness reports the delegation.
    Delegated {
        /// Dedicated campaign owner.
        phase_owner_bead: &'static str,
        /// Gate at which that owner must make the row live.
        first_required_gate: &'static str,
        /// Current state, which never counts as q97e coverage.
        row_state: TargetRowState,
    },
}

/// Resolve a stable target id without laundering delegated work into base
/// harness coverage.
pub fn campaign_entrypoint(target_id: &str) -> Result<CampaignEntrypoint, ActivationRejection> {
    validate_registry_rows(TARGETS).map_err(ActivationRejection::InvalidRegistry)?;
    let target = TARGETS
        .iter()
        .find(|target| target.id == target_id)
        .ok_or(ActivationRejection::UnknownTarget)?;
    validate_target_metadata(target).map_err(ActivationRejection::InvalidMetadata)?;
    if target.phase_owner_bead == BASE_HARNESS_OWNER && target.row_state == TargetRowState::Live {
        let coverage_evidence_ref = target
            .coverage_evidence_ref
            .ok_or(ActivationRejection::MissingEvidence)?;
        Ok(CampaignEntrypoint::Covered {
            coverage_evidence_ref,
        })
    } else {
        Ok(CampaignEntrypoint::Delegated {
            phase_owner_bead: target.phase_owner_bead,
            first_required_gate: target.first_required_gate,
            row_state: target.row_state,
        })
    }
}

fn phase_owned_campaign_entrypoint(
    target_id: &str,
    phase_owner_bead: &'static str,
    first_required_gate: &'static str,
) -> Result<&'static LdfiTarget, ActivationRejection> {
    validate_registry_rows(TARGETS).map_err(ActivationRejection::InvalidRegistry)?;
    let target = TARGETS
        .iter()
        .find(|target| target.id == target_id)
        .ok_or(ActivationRejection::UnknownTarget)?;
    if target.phase_owner_bead != phase_owner_bead {
        return Err(ActivationRejection::WrongPhaseOwner);
    }
    if target.first_required_gate != first_required_gate {
        return Err(ActivationRejection::WrongGate);
    }
    validate_activation(
        target_id,
        phase_owner_bead,
        first_required_gate,
        target.coverage_evidence_ref,
    )
}

/// Admission boundary for a G3-owned executable fault campaign.
///
/// Today every such row refuses with [`ActivationRejection::ImplementationDisabled`].
/// That refusal is load-bearing: q97e can route a row to G3 but cannot execute
/// or count it before the G3 torture owner lands the product machinery and an
/// exact witness in the same row.
pub fn g3_campaign_entrypoint(target_id: &str) -> Result<&'static LdfiTarget, ActivationRejection> {
    phase_owned_campaign_entrypoint(target_id, G3_PHASE_OWNER, G3_GATE)
}

/// Admission boundary for a W12-owned executable fault campaign.
///
/// Like [`g3_campaign_entrypoint`], this is a real fail-closed adapter, not a
/// placeholder success path: pending W12 rows cannot produce campaign proof.
pub fn w12_campaign_entrypoint(
    target_id: &str,
) -> Result<&'static LdfiTarget, ActivationRejection> {
    phase_owned_campaign_entrypoint(target_id, W12_PHASE_OWNER, W12_GATE)
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1f}' => {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

/// Render the target registry as versioned, line-oriented JSON.
///
/// The six phase-boundary fields are present on every row. This is a report of
/// registry state, not a claim that pending or delegated campaigns ran.
pub fn registry_jsonl() -> Result<String, RegistryValidationError> {
    validate_registry_rows(TARGETS)?;
    let mut output = String::new();
    for target in TARGETS {
        output.push_str("{\"event_version\":1,\"target_id\":");
        push_json_string(&mut output, target.id);
        output.push_str(",\"phase_owner_bead\":");
        push_json_string(&mut output, target.phase_owner_bead);
        output.push_str(",\"first_required_gate\":");
        push_json_string(&mut output, target.first_required_gate);
        output.push_str(",\"implementation_enabled\":");
        output.push_str(if target.implementation_enabled {
            "true"
        } else {
            "false"
        });
        output.push_str(",\"row_state\":");
        push_json_string(&mut output, target.row_state.as_str());
        output.push_str(",\"coverage_evidence_ref\":");
        match target.coverage_evidence_ref {
            Some(reference) => push_json_string(&mut output, reference),
            None => output.push_str("null"),
        }
        output.push_str("}\n");
    }
    Ok(output)
}

/// How many declared targets the harness can fault today.
pub fn reachable_count() -> Result<usize, RegistryValidationError> {
    validate_registry_rows(TARGETS)?;
    Ok(TARGETS
        .iter()
        .filter(|target| target.row_state == TargetRowState::Live)
        .count())
}

/// How many declared targets have no injection point yet.
///
/// This is the honest coverage gap. It is a function rather than a constant so
/// it cannot drift from [`TARGETS`], and it is public because a campaign
/// summary that omits it is reporting coverage over a denominator it chose.
pub fn unreachable_count() -> Result<usize, RegistryValidationError> {
    Ok(TARGETS.len() - reachable_count()?)
}

/// Coverage over the **plan's** denominator, as a sentence for a report.
///
/// Deliberately not a bare percentage: the interesting quantity is the gap and
/// who owns it, and a lone "9%" invites rounding into "we have LDFI".
pub fn coverage_statement() -> Result<String, RegistryValidationError> {
    let reachable = reachable_count()?;
    let unreachable = TARGETS.len() - reachable;
    Ok(format!(
        "{} of {} declared LDFI targets are reachable at this HEAD; {} have no injection point yet",
        reachable,
        TARGETS.len(),
        unreachable
    ))
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
    /// A non-empty file write is refused with ENOSPC before accepting bytes.
    WriteEnospc,
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
            "write-enospc" => Some(Self::WriteEnospc),
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
            Self::WriteEnospc => plan.write_enospc = trigger,
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
