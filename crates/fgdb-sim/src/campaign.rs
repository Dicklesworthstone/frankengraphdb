//! What a campaign is allowed to conclude (plan §15.1, lines 1128 and 1140).
//!
//! > "DPOR is exhaustive only within the declared bounded scenario/state model
//! > and the soundness of its independence relation; broader campaigns remain
//! > falsification, not proof of bug absence." (line 1128)
//!
//! > "its reports are claim-typed falsification-only — **structurally
//! > incapable of asserting 'verified fault-free'**" (line 1140)
//!
//! MEASURED before writing this: `CampaignSummary` and `falsification` had
//! zero occurrences across `crates/`.
//!
//! # "Structurally incapable" is the whole specification
//!
//! A doc comment saying "do not claim fault-free" is not what line 1140 asks
//! for — it asks that the claim be *unrepresentable*. So [`CampaignOutcome`]
//! has no variant meaning "clean", and there is deliberately no
//! `is_bug_free()` or `passed()` for a caller to reach for. The closest a
//! campaign can come is [`CampaignOutcome::NotFalsified`], whose name is the
//! claim: nothing was found, under a named model, within a stated budget.
//!
//! The three outcomes are not three flavours of the same thing — they carry
//! **different claim classes**, and the plan requires them reported
//! separately:
//!
//! * [`ClaimClass::Falsification`] — a counterexample exists. The only
//!   outcome that proves anything unconditionally, and it proves a bug, never
//!   its absence.
//! * [`ClaimClass::Statistical`] — exploration stopped under a sampling
//!   policy. Says nothing about what was not explored.
//! * [`ClaimClass::BoundedFormal`] — the declared bounded state model was
//!   exhausted. **Still not "fault-free"**: it is exhaustive within the model
//!   and the soundness of its independence relation, and both are assumptions
//!   the campaign cannot discharge about itself.
//!
//! Reporting the third as if it were the absence of bugs is the specific
//! error line 1128 exists to forbid, which is why `BoundedExhausted` carries
//! the model it exhausted and renders it in every message.

/// What kind of claim an outcome supports. Never "verified".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimClass {
    /// A counterexample was found. Unconditional, and about a bug.
    Falsification,
    /// Stopped under a named sampling policy. Silent about the unexplored.
    Statistical,
    /// A declared bounded model was exhausted. Bounded, and conditional on
    /// the model and its independence relation.
    BoundedFormal,
}

impl ClaimClass {
    /// How strong a claim this class licenses, in one line, for a report
    /// header. Kept beside the variants so a summary cannot be rendered with
    /// a stronger gloss than its class allows.
    #[must_use]
    pub const fn licence(self) -> &'static str {
        match self {
            Self::Falsification => "a counterexample was found",
            Self::Statistical => {
                "nothing found under a sampling policy; the unexplored space is not characterised"
            }
            Self::BoundedFormal => {
                "the declared bounded model was exhausted; outside that model nothing is claimed"
            }
        }
    }
}

/// The complete set of things a campaign may conclude.
///
/// There is no "clean" or "passed" variant, and adding one would be the
/// defect: line 1140 requires that the assertion be unrepresentable rather
/// than merely discouraged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CampaignOutcome {
    /// A counterexample was found, with the replay that reproduces it.
    Falsified {
        /// The failing replay's encoded descriptor — enough to re-run it.
        replay: String,
        /// What kind of failure it was.
        failure_kind: String,
    },
    /// Exploration stopped without finding anything, under a named policy.
    NotFalsified {
        /// The sampling model the stop was taken under. Mandatory: a stop
        /// without a named model is an opinion.
        sampling_model: String,
        /// How many cases were explored.
        explored: u64,
    },
    /// The declared bounded state model was exhausted without a
    /// counterexample. Reported separately from `NotFalsified` because it is
    /// a different claim class, not a stronger version of the same one.
    BoundedExhausted {
        /// The model that was exhausted, named so the bound is legible.
        model: String,
        /// States covered within it.
        states: u64,
    },
}

impl CampaignOutcome {
    /// The claim class this outcome carries.
    #[must_use]
    pub const fn claim_class(&self) -> ClaimClass {
        match self {
            Self::Falsified { .. } => ClaimClass::Falsification,
            Self::NotFalsified { .. } => ClaimClass::Statistical,
            Self::BoundedExhausted { .. } => ClaimClass::BoundedFormal,
        }
    }

    /// Whether a counterexample was found.
    ///
    /// Note the asymmetry, which is deliberate: `true` is a fact about a bug.
    /// `false` is **not** a claim that none exists, and there is no method
    /// here that turns it into one.
    #[must_use]
    pub const fn found_counterexample(&self) -> bool {
        matches!(self, Self::Falsified { .. })
    }
}

impl std::fmt::Display for CampaignOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Falsified {
                replay,
                failure_kind,
            } => write!(f, "falsified: {failure_kind} — reproduce with {replay}"),
            Self::NotFalsified {
                sampling_model,
                explored,
            } => write!(
                f,
                "not falsified in {explored} cases under sampling model {sampling_model:?}; \
                 the unexplored space is not characterised"
            ),
            Self::BoundedExhausted { model, states } => write!(
                f,
                "bounded model {model:?} exhausted over {states} states; \
                 outside that model nothing is claimed"
            ),
        }
    }
}

/// Phrases a campaign report may never contain.
///
/// Exported so the guard is a shared artifact rather than a private habit of
/// one test: any future report surface can assert against the same list
/// instead of inventing its own and missing a phrase.
pub const FORBIDDEN_CLAIMS: &[&str] = &[
    "verified fault-free",
    "fault-free",
    "no bugs",
    "bug-free",
    "proven correct",
    "proves correctness",
    "guaranteed correct",
];

// ---------------------------------------------------------------------------
// Transaction-lifecycle campaign coverage (plan §15.1)
// ---------------------------------------------------------------------------

/// First gate that requires the complete Local lifecycle campaign matrix.
pub const LIFECYCLE_FIRST_REQUIRED_GATE: &str = "fgdb-gate-genesis-lce";

/// Consumers that may not complete while any lifecycle row remains pending.
pub const EXPECTED_LIFECYCLE_CONSUMERS: &[&str] =
    &["fgdb-gate-genesis-lce", "fgdb-verif-torture-ddcl"];

/// The only Beads allowed to activate lifecycle rows in this registry.
pub const EXPECTED_LIFECYCLE_OWNER_BEADS: &[&str] = &[
    "fgdb-w2-txn-lifecycle-mhae",
    "fgdb-w2-prepare-terminal-uhkw",
    "fgdb-w2-outcome-tokens-v1w1",
    "fgdb-w2-compaction-zmkv",
];

/// The fixed §15.1 lifecycle campaign inventory in plan order.
///
/// This list is independent of [`LIFECYCLE_COVERAGE_ROWS`]. Whole-registry
/// validation compares the two, so removing a pending row cannot silently
/// shrink the denominator.
pub const EXPECTED_LIFECYCLE_COVERAGE_IDS: &[&str] = &[
    "lost-begin-accepted",
    "duplicate-begin-key",
    "conflicting-begin-key",
    "denial-before-registration",
    "abandonment-before-registration",
    "workspace-zero-recovery",
    "successor-registered-outcome-rooting",
    "cancel-with-prior-results",
    "cancel-with-prior-workspace",
    "cancel-with-prior-grants",
    "terminal-ack-release-race",
    "autocommit-ack-release-race",
    "terminal-pending-missing-postcondition-combinations",
    "status-before-compaction",
    "status-during-compaction",
    "status-after-compaction",
    "status-after-detail-reclamation",
];

/// Whether a lifecycle campaign row is future work, executable evidence, or
/// intentionally unavailable under a selected product posture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleCoverageState {
    Pending,
    Live,
    Disabled,
}

impl LifecycleCoverageState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Live => "live",
            Self::Disabled => "disabled",
        }
    }
}

/// One machine-readable lifecycle campaign obligation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleCoverageRow {
    pub id: &'static str,
    pub source_phrase: &'static str,
    pub owner_bead: &'static str,
    pub required_owner_beads: &'static [&'static str],
    pub first_required_gate: &'static str,
    pub implementation_enabled: bool,
    pub row_state: LifecycleCoverageState,
    pub coverage_evidence_ref: Option<&'static str>,
}

const fn pending_lifecycle(
    id: &'static str,
    source_phrase: &'static str,
    owner_bead: &'static str,
    required_owner_beads: &'static [&'static str],
) -> LifecycleCoverageRow {
    LifecycleCoverageRow {
        id,
        source_phrase,
        owner_bead,
        required_owner_beads,
        first_required_gate: LIFECYCLE_FIRST_REQUIRED_GATE,
        implementation_enabled: false,
        row_state: LifecycleCoverageState::Pending,
        coverage_evidence_ref: None,
    }
}

const TXN_LIFECYCLE_OWNER: &str = "fgdb-w2-txn-lifecycle-mhae";
const PREPARE_TERMINAL_OWNER: &str = "fgdb-w2-prepare-terminal-uhkw";
const OUTCOME_TOKENS_OWNER: &str = "fgdb-w2-outcome-tokens-v1w1";
const COMPACTION_OWNER: &str = "fgdb-w2-compaction-zmkv";
const TXN_ONLY: &[&str] = &[TXN_LIFECYCLE_OWNER];
const PREPARE_ONLY: &[&str] = &[PREPARE_TERMINAL_OWNER];
const TERMINAL_ACK_SEAM: &[&str] = &[PREPARE_TERMINAL_OWNER, OUTCOME_TOKENS_OWNER];
const AUTOCOMMIT_ACK_SEAM: &[&str] = &[
    TXN_LIFECYCLE_OWNER,
    PREPARE_TERMINAL_OWNER,
    OUTCOME_TOKENS_OWNER,
];
const STATUS_COMPACTION_SEAM: &[&str] = &[OUTCOME_TOKENS_OWNER, COMPACTION_OWNER];

/// The complete lifecycle campaign matrix required by plan §15.1.
///
/// Every row is pending because none of the four product owners is complete at
/// this HEAD. Pending is data, not a skip: [`validate_lifecycle_owner_completion`]
/// makes owner completion illegal until each owned row is live and evidenced.
pub const LIFECYCLE_COVERAGE_ROWS: &[LifecycleCoverageRow] = &[
    pending_lifecycle(
        "lost-begin-accepted",
        "lost `BEGIN_ACCEPTED`",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "duplicate-begin-key",
        "duplicate/conflicting begin keys",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "conflicting-begin-key",
        "duplicate/conflicting begin keys",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "denial-before-registration",
        "denial/abandonment before registration",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "abandonment-before-registration",
        "denial/abandonment before registration",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "workspace-zero-recovery",
        "workspace-zero recovery",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "successor-registered-outcome-rooting",
        "successor Registered-outcome rooting",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "cancel-with-prior-results",
        "cancel with prior results/workspace/grants",
        PREPARE_TERMINAL_OWNER,
        PREPARE_ONLY,
    ),
    pending_lifecycle(
        "cancel-with-prior-workspace",
        "cancel with prior results/workspace/grants",
        PREPARE_TERMINAL_OWNER,
        PREPARE_ONLY,
    ),
    pending_lifecycle(
        "cancel-with-prior-grants",
        "cancel with prior results/workspace/grants",
        PREPARE_TERMINAL_OWNER,
        PREPARE_ONLY,
    ),
    pending_lifecycle(
        "terminal-ack-release-race",
        "terminal/autocommit ACK/release races",
        OUTCOME_TOKENS_OWNER,
        TERMINAL_ACK_SEAM,
    ),
    pending_lifecycle(
        "autocommit-ack-release-race",
        "terminal/autocommit ACK/release races",
        OUTCOME_TOKENS_OWNER,
        AUTOCOMMIT_ACK_SEAM,
    ),
    pending_lifecycle(
        "terminal-pending-missing-postcondition-combinations",
        "every TerminalPending missing-postcondition combination",
        PREPARE_TERMINAL_OWNER,
        PREPARE_ONLY,
    ),
    pending_lifecycle(
        "status-before-compaction",
        "status before/during/after compaction and detail reclamation",
        OUTCOME_TOKENS_OWNER,
        STATUS_COMPACTION_SEAM,
    ),
    pending_lifecycle(
        "status-during-compaction",
        "status before/during/after compaction and detail reclamation",
        COMPACTION_OWNER,
        STATUS_COMPACTION_SEAM,
    ),
    pending_lifecycle(
        "status-after-compaction",
        "status before/during/after compaction and detail reclamation",
        COMPACTION_OWNER,
        STATUS_COMPACTION_SEAM,
    ),
    pending_lifecycle(
        "status-after-detail-reclamation",
        "status before/during/after compaction and detail reclamation",
        COMPACTION_OWNER,
        STATUS_COMPACTION_SEAM,
    ),
];

fn expected_lifecycle_owner(id: &str) -> Option<&'static str> {
    match id {
        "lost-begin-accepted"
        | "duplicate-begin-key"
        | "conflicting-begin-key"
        | "denial-before-registration"
        | "abandonment-before-registration"
        | "workspace-zero-recovery"
        | "successor-registered-outcome-rooting" => Some(TXN_LIFECYCLE_OWNER),
        "cancel-with-prior-results"
        | "cancel-with-prior-workspace"
        | "cancel-with-prior-grants"
        | "terminal-pending-missing-postcondition-combinations" => Some(PREPARE_TERMINAL_OWNER),
        "terminal-ack-release-race"
        | "autocommit-ack-release-race"
        | "status-before-compaction" => Some(OUTCOME_TOKENS_OWNER),
        "status-during-compaction"
        | "status-after-compaction"
        | "status-after-detail-reclamation" => Some(COMPACTION_OWNER),
        _ => None,
    }
}

fn expected_lifecycle_required_owners(id: &str) -> Option<&'static [&'static str]> {
    match id {
        "lost-begin-accepted"
        | "duplicate-begin-key"
        | "conflicting-begin-key"
        | "denial-before-registration"
        | "abandonment-before-registration"
        | "workspace-zero-recovery"
        | "successor-registered-outcome-rooting" => Some(TXN_ONLY),
        "cancel-with-prior-results"
        | "cancel-with-prior-workspace"
        | "cancel-with-prior-grants"
        | "terminal-pending-missing-postcondition-combinations" => Some(PREPARE_ONLY),
        "terminal-ack-release-race" => Some(TERMINAL_ACK_SEAM),
        "autocommit-ack-release-race" => Some(AUTOCOMMIT_ACK_SEAM),
        "status-before-compaction"
        | "status-during-compaction"
        | "status-after-compaction"
        | "status-after-detail-reclamation" => Some(STATUS_COMPACTION_SEAM),
        _ => None,
    }
}

/// Exact evidence identity registered for a live lifecycle row.
///
/// No row is live yet. Adding a live row requires adding its exact
/// `path::test_selector` here in the same change; arbitrary non-empty strings
/// cannot activate coverage or satisfy an owner/consumer completion tripwire.
fn expected_lifecycle_evidence_ref(_id: &str) -> Option<&'static str> {
    None
}

/// Why lifecycle coverage metadata is not authoritative enough to consume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleRegistryError {
    InventoryLength {
        expected: usize,
        actual: usize,
    },
    InventoryId {
        index: usize,
    },
    DuplicateId {
        id: &'static str,
    },
    UnknownBoundary {
        id: &'static str,
    },
    UnknownRequestedId,
    WrongOwner {
        id: &'static str,
    },
    WrongRequiredOwners {
        id: &'static str,
    },
    WrongGate {
        id: &'static str,
    },
    PendingImplementationEnabled {
        id: &'static str,
    },
    PendingCarriesEvidence {
        id: &'static str,
    },
    LiveImplementationDisabled {
        id: &'static str,
    },
    LiveMissingEvidence {
        id: &'static str,
    },
    LiveEvidenceUnregistered {
        id: &'static str,
    },
    LiveEvidenceMismatch {
        id: &'static str,
    },
    DisabledImplementationEnabled {
        id: &'static str,
    },
    DisabledCarriesEvidence {
        id: &'static str,
    },
    OwnerInventoryLength {
        expected: usize,
        actual: usize,
    },
    OwnerInventoryId {
        index: usize,
    },
    CompletedOwnerMissingCampaign {
        owner_bead: &'static str,
        row_id: &'static str,
    },
    ConsumerInventoryLength {
        expected: usize,
        actual: usize,
    },
    ConsumerInventoryId {
        index: usize,
    },
    CompletedConsumerMissingCampaign {
        consumer_id: &'static str,
        row_id: &'static str,
    },
}

impl std::fmt::Display for LifecycleRegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid lifecycle campaign registry: {self:?}")
    }
}

impl std::error::Error for LifecycleRegistryError {}

/// Validate one complete lifecycle matrix without consulting tracker state.
pub fn validate_lifecycle_coverage_rows(
    rows: &[LifecycleCoverageRow],
) -> Result<(), LifecycleRegistryError> {
    if rows.len() != EXPECTED_LIFECYCLE_COVERAGE_IDS.len() {
        return Err(LifecycleRegistryError::InventoryLength {
            expected: EXPECTED_LIFECYCLE_COVERAGE_IDS.len(),
            actual: rows.len(),
        });
    }
    let mut seen = std::collections::BTreeSet::new();
    for (index, (row, expected_id)) in rows.iter().zip(EXPECTED_LIFECYCLE_COVERAGE_IDS).enumerate()
    {
        if !seen.insert(row.id) {
            return Err(LifecycleRegistryError::DuplicateId { id: row.id });
        }
        if row.id != *expected_id {
            return Err(LifecycleRegistryError::InventoryId { index });
        }
        let Some(expected_owner) = expected_lifecycle_owner(row.id) else {
            return Err(LifecycleRegistryError::UnknownBoundary { id: row.id });
        };
        if row.owner_bead != expected_owner {
            return Err(LifecycleRegistryError::WrongOwner { id: row.id });
        }
        let Some(expected_required_owners) = expected_lifecycle_required_owners(row.id) else {
            return Err(LifecycleRegistryError::UnknownBoundary { id: row.id });
        };
        if row.required_owner_beads != expected_required_owners {
            return Err(LifecycleRegistryError::WrongRequiredOwners { id: row.id });
        }
        if row.first_required_gate != LIFECYCLE_FIRST_REQUIRED_GATE {
            return Err(LifecycleRegistryError::WrongGate { id: row.id });
        }
        match row.row_state {
            LifecycleCoverageState::Pending => {
                if row.implementation_enabled {
                    return Err(LifecycleRegistryError::PendingImplementationEnabled {
                        id: row.id,
                    });
                }
                if row.coverage_evidence_ref.is_some() {
                    return Err(LifecycleRegistryError::PendingCarriesEvidence { id: row.id });
                }
            }
            LifecycleCoverageState::Live => {
                if !row.implementation_enabled {
                    return Err(LifecycleRegistryError::LiveImplementationDisabled { id: row.id });
                }
                let Some(actual_evidence) =
                    row.coverage_evidence_ref.filter(|value| !value.is_empty())
                else {
                    return Err(LifecycleRegistryError::LiveMissingEvidence { id: row.id });
                };
                let Some(expected_evidence) = expected_lifecycle_evidence_ref(row.id) else {
                    return Err(LifecycleRegistryError::LiveEvidenceUnregistered { id: row.id });
                };
                if actual_evidence != expected_evidence {
                    return Err(LifecycleRegistryError::LiveEvidenceMismatch { id: row.id });
                }
            }
            LifecycleCoverageState::Disabled => {
                if row.implementation_enabled {
                    return Err(LifecycleRegistryError::DisabledImplementationEnabled {
                        id: row.id,
                    });
                }
                if row.coverage_evidence_ref.is_some() {
                    return Err(LifecycleRegistryError::DisabledCarriesEvidence { id: row.id });
                }
            }
        }
    }
    Ok(())
}

/// Tracker completion state supplied by the CI adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleOwnerCompletion {
    pub owner_bead: &'static str,
    pub complete: bool,
}

/// Completion state for a gate or verification consumer of the whole matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleConsumerCompletion {
    pub consumer_id: &'static str,
    pub complete: bool,
}

/// Enforce the owner-completion tripwire required by q97e.
///
/// The owner list is an exact ordered inventory, not a caller-selected subset.
/// Once an owner is complete, every row it owns must be live and carry an
/// evidence reference. Before completion, pending rows remain visible and
/// legal but never count as coverage.
pub fn validate_lifecycle_owner_completion(
    rows: &[LifecycleCoverageRow],
    owners: &[LifecycleOwnerCompletion],
) -> Result<(), LifecycleRegistryError> {
    validate_lifecycle_coverage_rows(rows)?;
    if owners.len() != EXPECTED_LIFECYCLE_OWNER_BEADS.len() {
        return Err(LifecycleRegistryError::OwnerInventoryLength {
            expected: EXPECTED_LIFECYCLE_OWNER_BEADS.len(),
            actual: owners.len(),
        });
    }
    for (index, (owner, expected)) in owners
        .iter()
        .zip(EXPECTED_LIFECYCLE_OWNER_BEADS)
        .enumerate()
    {
        if owner.owner_bead != *expected {
            return Err(LifecycleRegistryError::OwnerInventoryId { index });
        }
        if !owner.complete {
            continue;
        }
        if let Some(row) = rows.iter().find(|row| {
            row.required_owner_beads.contains(&owner.owner_bead)
                && (row.row_state != LifecycleCoverageState::Live
                    || row.coverage_evidence_ref.is_none_or(str::is_empty))
        }) {
            return Err(LifecycleRegistryError::CompletedOwnerMissingCampaign {
                owner_bead: owner.owner_bead,
                row_id: row.id,
            });
        }
    }
    Ok(())
}

/// Prevent Genesis or the fault-torture owner from completing over a partial
/// lifecycle matrix.
pub fn validate_lifecycle_consumer_completion(
    rows: &[LifecycleCoverageRow],
    consumers: &[LifecycleConsumerCompletion],
) -> Result<(), LifecycleRegistryError> {
    validate_lifecycle_coverage_rows(rows)?;
    if consumers.len() != EXPECTED_LIFECYCLE_CONSUMERS.len() {
        return Err(LifecycleRegistryError::ConsumerInventoryLength {
            expected: EXPECTED_LIFECYCLE_CONSUMERS.len(),
            actual: consumers.len(),
        });
    }
    for (index, (consumer, expected)) in consumers
        .iter()
        .zip(EXPECTED_LIFECYCLE_CONSUMERS)
        .enumerate()
    {
        if consumer.consumer_id != *expected {
            return Err(LifecycleRegistryError::ConsumerInventoryId { index });
        }
        if !consumer.complete {
            continue;
        }
        if let Some(row) = rows
            .iter()
            .find(|row| row.row_state != LifecycleCoverageState::Live)
        {
            return Err(LifecycleRegistryError::CompletedConsumerMissingCampaign {
                consumer_id: consumer.consumer_id,
                row_id: row.id,
            });
        }
    }
    Ok(())
}

/// Base-harness routing result for a lifecycle campaign row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleCampaignEntrypoint {
    Covered {
        coverage_evidence_ref: &'static str,
    },
    Delegated {
        owner_bead: &'static str,
        required_owner_beads: &'static [&'static str],
        first_required_gate: &'static str,
        row_state: LifecycleCoverageState,
    },
}

/// Resolve a lifecycle row without turning delegation into base-harness proof.
pub fn lifecycle_campaign_entrypoint(
    id: &str,
) -> Result<LifecycleCampaignEntrypoint, LifecycleRegistryError> {
    validate_lifecycle_coverage_rows(LIFECYCLE_COVERAGE_ROWS)?;
    let Some(row) = LIFECYCLE_COVERAGE_ROWS.iter().find(|row| row.id == id) else {
        return Err(LifecycleRegistryError::UnknownRequestedId);
    };
    if row.row_state == LifecycleCoverageState::Live {
        let evidence = row
            .coverage_evidence_ref
            .ok_or(LifecycleRegistryError::LiveMissingEvidence { id: row.id })?;
        Ok(LifecycleCampaignEntrypoint::Covered {
            coverage_evidence_ref: evidence,
        })
    } else {
        Ok(LifecycleCampaignEntrypoint::Delegated {
            owner_bead: row.owner_bead,
            required_owner_beads: row.required_owner_beads,
            first_required_gate: row.first_required_gate,
            row_state: row.row_state,
        })
    }
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{:04x}", ch as u32);
            }
            ch => output.push(ch),
        }
    }
    output.push('"');
}

/// Serialize the complete validated matrix as one JSON object per line.
pub fn lifecycle_coverage_jsonl() -> Result<String, LifecycleRegistryError> {
    validate_lifecycle_coverage_rows(LIFECYCLE_COVERAGE_ROWS)?;
    let mut output = String::new();
    for row in LIFECYCLE_COVERAGE_ROWS {
        output.push_str("{\"id\":");
        push_json_string(&mut output, row.id);
        output.push_str(",\"source_phrase\":");
        push_json_string(&mut output, row.source_phrase);
        output.push_str(",\"owner_bead\":");
        push_json_string(&mut output, row.owner_bead);
        output.push_str(",\"required_owner_beads\":[");
        for (index, owner) in row.required_owner_beads.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            push_json_string(&mut output, owner);
        }
        output.push(']');
        output.push_str(",\"first_required_gate\":");
        push_json_string(&mut output, row.first_required_gate);
        output.push_str(",\"implementation_enabled\":");
        output.push_str(if row.implementation_enabled {
            "true"
        } else {
            "false"
        });
        output.push_str(",\"row_state\":");
        push_json_string(&mut output, row.row_state.as_str());
        output.push_str(",\"coverage_evidence_ref\":");
        match row.coverage_evidence_ref {
            Some(reference) => push_json_string(&mut output, reference),
            None => output.push_str("null"),
        }
        output.push_str("}\n");
    }
    Ok(output)
}
