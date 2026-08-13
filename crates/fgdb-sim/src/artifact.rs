//! The structured failure artifact (plan line 1138).
//!
//! > "Every failing sim, live differential, integration, or E2E run emits one
//! > secret-redacted structured artifact containing `{seed, schedule,
//! > crashpoint, role, group, configuration, topology, incarnation,
//! > service_visibility_epoch, logical/commit/Raft/applied/visible/audit-visible
//! > positions, attempt/generation/statement/workspace/backup/restore/GC/key
//! > identifiers, object/spec/result/certificate/grant/floor identities,
//! > expected, actual, replay_command}`. **Contract tests require every
//! > applicable field and execute the replay command**; human prose is
//! > supplemental."
//!
//! MEASURED 2026-08-04 before writing this: `replay_command`, `FailureArtifact`
//! and `crashpack` had **zero** hits across `crates/`. The artifact was
//! entirely unbuilt, so §15.1's "mandatory" artifact was mandatory over nothing.
//!
//! # The two ways this contract fails open, and what stops each
//!
//! **"Every applicable field" can be satisfied by shipping one field.** Most of
//! the fields above name subsystems that do not exist at this HEAD — there is
//! no Raft, no topology, no backup. A struct that simply omits them reads as
//! complete. So the field set is a **closed list** ([`CONTRACT_FIELDS`], one
//! entry per name in line 1138) and an artifact is a *total map over it*: every
//! field is either `Present` or `Absent` **with a stated reason**
//! ([`Absence`]). A field cannot leave the artifact by being forgotten — only
//! by someone writing down why it is not there.
//!
//! **A `replay_command` is the easiest placebo in the repository.** We already
//! own one: fgdb-4bxh records that asupersync's crashpack emits a replay
//! command whose environment variables nothing reads, and that libtest has no
//! `--seed` flag to honour anyway. A string that no consumer parses is worse
//! than no string, because it makes the contract test look satisfied.
//!
//! So the artifact's replay is a [`Replay`] — a value that **actually re-runs
//! the failure** through [`Replay::run`] — and `replay_command` is *derived
//! from it* by [`Replay::encode`]. The contract test asserts four things
//! together, and it is the conjunction that closes the hole:
//!
//! 1. the encoded string decodes back to an equal [`Replay`] (the string cannot
//!    drift from the value);
//! 2. running that decoded value reproduces a **byte-identical fault event
//!    log** and the same failure (the value genuinely replays);
//! 3. the rendered command's exact environment assignments and frozen consumer
//!    selector are executed through the already-built test executable in a
//!    fresh subprocess, where the consumer reproduces the failure successfully
//!    (the check does not make correctness depend on a nested Cargo rebuild);
//! 4. a scenario that does not fail emits **no artifact at all** (the control —
//!    without it, an emitter that always emitted would pass 1 through 3).
//!
//! This module currently discharges that command contract for built-in
//! [`Replay`] scenarios. [`ScenarioCatalog`] gives later owners a deterministic
//! in-process compile-time registration seam and binds their identity/state
//! model to the returned evidence, but it cannot synthesize a fresh-process
//! command or decoder for an arbitrary downstream function pointer. Those
//! owners must provide and test that executable boundary themselves.

use crate::vfs::{FaultEvent, FaultPlan, FaultVfs};
use asupersync::fs::{OpenOptions, Vfs, VfsFile};
use asupersync::io::AsyncWrite;
use asupersync::lab::{LabConfig, LabRuntime};
use asupersync::{Budget, Cx};
use fgdb::{
    CAPSULE_OBJECT_KIND, Database, DatabaseKeys, DatabaseState, DerivedPublicationStage, ReadError,
    RebuildError, RecoveryRequired, WriteBatch, WriteError,
};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CommitSeq, GraphId, VId};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::pin::Pin;

/// The environment variable a replay runner reads. Named here so the emitter
/// and the consumer cannot disagree about it — the exact disagreement that
/// makes fgdb-4bxh's upstream replay command inert.
pub const ARTIFACT_REPLAY_ENV: &str = "FGDB_SIM_REPLAY";

/// Expected canonical failure/event digest consumed by the fresh-process
/// replay entrypoint. The emitted command sets this beside
/// [`ARTIFACT_REPLAY_ENV`], so a child that reaches a different failure cannot
/// report success merely because it failed somehow.
pub const ARTIFACT_REPLAY_EXPECTED_DIGEST_ENV: &str = "FGDB_SIM_EXPECTED_REPLAY_DIGEST";

/// Plan line 1138's field list, one entry per name, in the order the line
/// spells them. Closed on purpose: [`FailureArtifact`] is a total map over
/// this, so a field can only be missing if someone stated why.
pub const CONTRACT_FIELDS: &[&str] = &[
    "seed",
    "schedule",
    "crashpoint",
    "role",
    "group",
    "configuration",
    "topology",
    "incarnation",
    "service_visibility_epoch",
    "logical_position",
    "commit_position",
    "raft_position",
    "applied_position",
    "visible_position",
    "audit_visible_position",
    "attempt_identifier",
    "generation_identifier",
    "statement_identifier",
    "workspace_identifier",
    "backup_identifier",
    "restore_identifier",
    "gc_identifier",
    "key_identifier",
    "object_identity",
    "spec_identity",
    "result_identity",
    "certificate_identity",
    "grant_identity",
    "floor_identity",
    "expected",
    "actual",
    "replay_command",
];

/// Why a contract field carries no value.
///
/// Absence is never bare. "The subsystem does not exist yet" and "this run had
/// no such value" are different claims with different lifetimes, and a reader
/// deciding whether an artifact is trustworthy needs to tell them apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Absence {
    /// Nothing at this HEAD can supply it. Names the subsystem and the bead
    /// that will make it applicable, so the absence has an owner.
    NotYetBuilt {
        /// The subsystem that would supply the value.
        subsystem: &'static str,
        /// The bead that makes it applicable.
        bead: &'static str,
    },
    /// The run's shape genuinely has no such value — a single-role local run
    /// has no group, whatever else exists.
    NotApplicable {
        /// Why this run cannot have one.
        because: &'static str,
    },
    /// Withheld by the secret-redaction contract. The field was applicable and
    /// had a value; that is itself information, so it is recorded.
    Redacted,
}

/// One contract field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Field {
    /// The value, rendered.
    Present(String),
    /// No value, and why.
    Absent(Absence),
}

impl Field {
    /// True when this field carries a value.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        matches!(self, Self::Present(_))
    }
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// The scenarios this harness can replay.
///
/// A closed enum for the base crate's fresh-process replay commands. A runtime
/// registry cannot promise that an artifact ID resolves in a new process;
/// downstream owners use [`ScenarioCatalog`] for deterministic in-process
/// registration and provide their own executable decoder when filing commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// Write four sectors through one handle, sync, crash, and require every
    /// byte to survive. Fails under any durability fault.
    DurableAppend,
    /// Write four sectors, sync, crash, and require the file to be *empty* —
    /// the inverse expectation, so a faultless run fails and a lying one
    /// passes. Exists to prove the artifact emitter is driven by the failure
    /// and not by the fault.
    LostAppend,
    /// One real embedded-database commit, process loss, and ordinary reopen.
    /// A pre-acknowledgement refusal is clean; an acknowledged commit whose
    /// frontier or vertex is absent after reopen is a durability failure.
    SpineDurability,
    /// The same real embedded workload as [`Scenario::SpineDurability`], then
    /// an explicit mutation of its recovered observation to a missing commit.
    /// This is the detector-liveness control; real campaigns never use it.
    PlantedSpineLoss,
    /// One real integrated database commit stopped at the named derived-
    /// publication stage after Chronicle D2. The run verifies the stale
    /// handle is fenced and that a fresh engine open and independent reference
    /// replay both recover the durable commit before emitting the structured
    /// recovery failure.
    PostD2Recovery(DerivedPublicationStage),
}

impl Scenario {
    /// How many scenarios exist.
    ///
    /// Load-bearing, and the middle link of a three-step chain that makes the
    /// registry impossible to leave incomplete without noticing:
    ///
    /// 1. adding a variant breaks [`Scenario::index`]'s exhaustive match — a
    ///    **compile error**, so the author cannot not-notice;
    /// 2. fixing that forces them to choose an index, which forces this count;
    /// 3. `scenario_registry_is_complete` then fails until [`SCENARIOS`] gains
    ///    the matching row.
    ///
    /// Doctrine #1 rules out the usual answer (a `linkme`/`inventory`-style
    /// distributed slice is an external crate), and a runtime registry is
    /// ruled out by the replay contract — see [`SCENARIOS`]. A const table
    /// plus a compile error is what remains, and it is not a lesser option:
    /// the compile error fires earlier than any registration call would.
    pub const COUNT: usize = 13;

    /// A dense index per variant, `0..COUNT`.
    ///
    /// Exists so completeness is checkable by arithmetic rather than by
    /// someone remembering to update a list.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::DurableAppend => 0,
            Self::LostAppend => 1,
            Self::SpineDurability => 2,
            Self::PlantedSpineLoss => 3,
            Self::PostD2Recovery(stage) => {
                4 + match stage {
                    DerivedPublicationStage::FoldCommittedTemplate => 0,
                    DerivedPublicationStage::SealPartition => 1,
                    DerivedPublicationStage::PublishEdgeBlocks => 2,
                    DerivedPublicationStage::PublishVertexPatches => 3,
                    DerivedPublicationStage::PublishPartitionRoot => 4,
                    DerivedPublicationStage::PublishManifest => 5,
                    DerivedPublicationStage::PublishRootSlot => 6,
                    DerivedPublicationStage::RefreshEdgeSnapshot => 7,
                    DerivedPublicationStage::RefreshVertexSnapshot => 8,
                }
            }
        }
    }

    /// The stable wire name, used in [`Replay::encode`].
    ///
    /// Stable in the durable sense: it appears in emitted replay commands and
    /// in filed artifacts, so renaming one silently invalidates every replay
    /// string already written down.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::DurableAppend => "durable-append",
            Self::LostAppend => "lost-append",
            Self::SpineDurability => "spine-durability",
            Self::PlantedSpineLoss => "planted-spine-loss",
            Self::PostD2Recovery(stage) => match stage {
                DerivedPublicationStage::FoldCommittedTemplate => "post-d2-fold-committed-template",
                DerivedPublicationStage::SealPartition => "post-d2-seal-partition",
                DerivedPublicationStage::PublishEdgeBlocks => "post-d2-publish-edge-blocks",
                DerivedPublicationStage::PublishVertexPatches => "post-d2-publish-vertex-patches",
                DerivedPublicationStage::PublishPartitionRoot => "post-d2-publish-partition-root",
                DerivedPublicationStage::PublishManifest => "post-d2-publish-manifest",
                DerivedPublicationStage::PublishRootSlot => "post-d2-publish-root-slot",
                DerivedPublicationStage::RefreshEdgeSnapshot => "post-d2-refresh-edge-snapshot",
                DerivedPublicationStage::RefreshVertexSnapshot => "post-d2-refresh-vertex-snapshot",
            },
        }
    }

    /// The injected derived-publication stage, when this is an integrated
    /// database recovery replay rather than a direct VFS scenario.
    #[must_use]
    pub const fn recovery_stage(self) -> Option<DerivedPublicationStage> {
        match self {
            Self::PostD2Recovery(stage) => Some(stage),
            Self::DurableAppend
            | Self::LostAppend
            | Self::SpineDurability
            | Self::PlantedSpineLoss => None,
        }
    }

    /// The registry row for this scenario.
    #[must_use]
    pub fn entry(self) -> &'static ScenarioEntry {
        &SCENARIOS[self.index()]
    }

    fn parse(text: &str) -> Result<Self, String> {
        resolve(text).map_err(|error| error.to_string())
    }
}

/// One registered scenario: what it is, and what bounded model it explores.
///
/// `state_model` is not decoration. A campaign that exhausts a scenario
/// reports [`crate::campaign::CampaignOutcome::BoundedExhausted`], which is
/// required to name the model it exhausted — so the name has to live
/// somewhere durable, next to the scenario rather than in the campaign that
/// happened to run it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScenarioEntry {
    /// The stable wire id. Must equal [`Scenario::id`].
    pub id: &'static str,
    /// The scenario itself.
    pub scenario: Scenario,
    /// What the scenario asserts, for a filed report.
    pub asserts: &'static str,
    /// The declared bounded state model this scenario explores.
    pub state_model: &'static str,
}

/// Compile-time registration row for a scenario supplied by this crate or a
/// later campaign owner.
///
/// Function pointers keep in-process registration deterministic: a binary
/// assembles an immutable catalog explicitly, with no constructor side effects
/// or process-global plugin order. This catalog does not itself encode those
/// pointers into a fresh-process replay command.
#[derive(Clone, Copy)]
pub struct ScenarioRegistration {
    pub id: &'static str,
    pub asserts: &'static str,
    pub state_model: &'static str,
    pub execute: fn(FaultPlan, &Path) -> RunOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScenarioRegistrationError {
    InvalidId,
    EmptyAssertion,
    EmptyStateModel,
    DuplicateId,
    UnknownId,
    ExecutionEvidenceMutated,
    PlanMismatch,
}

/// Validated immutable view over registrations selected by one binary.
pub struct ScenarioCatalog<'a> {
    rows: &'a [ScenarioRegistration],
}

/// One catalog execution with the registered identity and state-model contract
/// bound to its sealed outcome. This prevents a downstream scenario from
/// silently inheriting the built-in replay's name in campaign logs.
pub struct RegisteredRunOutcome<'a> {
    registration: &'a ScenarioRegistration,
    outcome: RunOutcome,
    evidence_digest: String,
}

impl RegisteredRunOutcome<'_> {
    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.registration.id
    }

    #[must_use]
    pub const fn assertion(&self) -> &'static str {
        self.registration.asserts
    }

    #[must_use]
    pub const fn state_model(&self) -> &'static str {
        self.registration.state_model
    }

    #[must_use]
    pub const fn outcome(&self) -> &RunOutcome {
        &self.outcome
    }

    /// Digest over the catalog identity/state model plus the callback's sealed
    /// execution. Later-owner artifact emitters can pin this in their own
    /// fresh-process command without editing the base scenario enum.
    #[must_use]
    pub fn evidence_digest(&self) -> &str {
        &self.evidence_digest
    }

    #[must_use]
    pub fn log_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "registered-sim-run id={} assertion={:?} state_model={:?} evidence_digest={}",
            self.id(),
            self.assertion(),
            self.state_model(),
            self.evidence_digest,
        )];
        lines.extend(self.outcome.receipt.log_lines());
        lines
    }
}

impl<'a> ScenarioCatalog<'a> {
    /// Validate identifiers and exact-set uniqueness for unambiguous catalog
    /// resolution within the assembled binary.
    pub fn try_new(rows: &'a [ScenarioRegistration]) -> Result<Self, ScenarioRegistrationError> {
        let mut ids = std::collections::BTreeSet::new();
        for row in rows {
            let valid_id = !row.id.is_empty()
                && row.id.len() <= 128
                && row.id.bytes().enumerate().all(|(index, byte)| {
                    byte.is_ascii_alphanumeric()
                        || (index > 0 && matches!(byte, b'.' | b'_' | b':' | b'/' | b'-'))
                });
            if !valid_id {
                return Err(ScenarioRegistrationError::InvalidId);
            }
            if row.asserts.trim().is_empty() {
                return Err(ScenarioRegistrationError::EmptyAssertion);
            }
            if row.state_model.trim().is_empty() {
                return Err(ScenarioRegistrationError::EmptyStateModel);
            }
            if !ids.insert(row.id) {
                return Err(ScenarioRegistrationError::DuplicateId);
            }
            if SCENARIOS.iter().any(|builtin| builtin.id == row.id) {
                return Err(ScenarioRegistrationError::DuplicateId);
            }
        }
        Ok(Self { rows })
    }

    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<&ScenarioRegistration> {
        self.rows.iter().find(|row| row.id == id)
    }

    /// Resolve and execute through the row's registered function pointer.
    pub fn execute(
        &'a self,
        id: &str,
        plan: FaultPlan,
        dir: &Path,
    ) -> Result<RegisteredRunOutcome<'a>, ScenarioRegistrationError> {
        let row = self
            .resolve(id)
            .ok_or(ScenarioRegistrationError::UnknownId)?;
        let outcome = (row.execute)(plan, dir);
        if outcome.replay().plan != plan {
            return Err(ScenarioRegistrationError::PlanMismatch);
        }
        let run_digest = outcome
            .replay_completeness_digest()
            .ok_or(ScenarioRegistrationError::ExecutionEvidenceMutated)?;
        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(b"fgdb.sim.registered-run.v1");
        hasher.update(row.id.as_bytes());
        hasher.update(row.asserts.as_bytes());
        hasher.update(row.state_model.as_bytes());
        hasher.update(run_digest.as_bytes());
        Ok(RegisteredRunOutcome {
            registration: row,
            outcome,
            evidence_digest: hasher.finalize().to_hex(),
        })
    }
}

/// The scenario registry.
///
/// Built-in artifact descriptors remain a const table because the base replay
/// command must resolve without any application assembly. Later campaign
/// crates use [`ScenarioRegistration`] and [`ScenarioCatalog`] to assemble
/// their own compile-time catalogs without editing this enum or relying on a
/// process-global dynamic registry.
///
/// Indexed by [`Scenario::index`]; `scenario_registry_is_complete` pins that.
const POST_D2_EXPECTATION: &str =
    "derived publication completes and the live handle advances to the durable frontier";

pub static SCENARIOS: [ScenarioEntry; Scenario::COUNT] = [
    ScenarioEntry {
        id: "durable-append",
        scenario: Scenario::DurableAppend,
        asserts: "every acknowledged byte survives the crash",
        state_model: "single-writer four-sector append, one flush, one crash",
    },
    ScenarioEntry {
        id: "lost-append",
        scenario: Scenario::LostAppend,
        asserts: "nothing survives the crash",
        state_model: "single-writer four-sector append, one flush, one crash",
    },
    ScenarioEntry {
        id: "spine-durability",
        scenario: Scenario::SpineDurability,
        asserts: "every acknowledged database commit survives process loss and ordinary reopen",
        state_model: "one embedded database, one vertex commit, one crash, one reopen",
    },
    ScenarioEntry {
        id: "planted-spine-loss",
        scenario: Scenario::PlantedSpineLoss,
        asserts: "the durability detector rejects an explicitly planted missing acknowledged commit",
        state_model: "one embedded database, one vertex commit, one crash, one reopen, one oracle mutation",
    },
    ScenarioEntry {
        id: "post-d2-fold-committed-template",
        scenario: Scenario::PostD2Recovery(DerivedPublicationStage::FoldCommittedTemplate),
        asserts: POST_D2_EXPECTATION,
        state_model: "one committed vertex, failure at FoldCommittedTemplate, one reopen",
    },
    ScenarioEntry {
        id: "post-d2-seal-partition",
        scenario: Scenario::PostD2Recovery(DerivedPublicationStage::SealPartition),
        asserts: POST_D2_EXPECTATION,
        state_model: "one committed vertex, failure at SealPartition, one reopen",
    },
    ScenarioEntry {
        id: "post-d2-publish-edge-blocks",
        scenario: Scenario::PostD2Recovery(DerivedPublicationStage::PublishEdgeBlocks),
        asserts: POST_D2_EXPECTATION,
        state_model: "one committed vertex, failure at PublishEdgeBlocks, one reopen",
    },
    ScenarioEntry {
        id: "post-d2-publish-vertex-patches",
        scenario: Scenario::PostD2Recovery(DerivedPublicationStage::PublishVertexPatches),
        asserts: POST_D2_EXPECTATION,
        state_model: "one committed vertex, failure at PublishVertexPatches, one reopen",
    },
    ScenarioEntry {
        id: "post-d2-publish-partition-root",
        scenario: Scenario::PostD2Recovery(DerivedPublicationStage::PublishPartitionRoot),
        asserts: POST_D2_EXPECTATION,
        state_model: "one committed vertex, failure at PublishPartitionRoot, one reopen",
    },
    ScenarioEntry {
        id: "post-d2-publish-manifest",
        scenario: Scenario::PostD2Recovery(DerivedPublicationStage::PublishManifest),
        asserts: POST_D2_EXPECTATION,
        state_model: "one committed vertex, failure at PublishManifest, one reopen",
    },
    ScenarioEntry {
        id: "post-d2-publish-root-slot",
        scenario: Scenario::PostD2Recovery(DerivedPublicationStage::PublishRootSlot),
        asserts: POST_D2_EXPECTATION,
        state_model: "one committed vertex, failure at PublishRootSlot, one reopen",
    },
    ScenarioEntry {
        id: "post-d2-refresh-edge-snapshot",
        scenario: Scenario::PostD2Recovery(DerivedPublicationStage::RefreshEdgeSnapshot),
        asserts: POST_D2_EXPECTATION,
        state_model: "one committed vertex, failure at RefreshEdgeSnapshot, one reopen",
    },
    ScenarioEntry {
        id: "post-d2-refresh-vertex-snapshot",
        scenario: Scenario::PostD2Recovery(DerivedPublicationStage::RefreshVertexSnapshot),
        asserts: POST_D2_EXPECTATION,
        state_model: "one committed vertex, failure at RefreshVertexSnapshot, one reopen",
    },
];

/// Why an id did not resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownScenario {
    /// The id that was asked for.
    pub asked: String,
}

impl std::fmt::Display for UnknownScenario {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The known set is listed because the caller is usually holding a
        // replay string from a filed artifact and needs to know whether the
        // id is stale or simply misspelled. "unknown scenario" alone forces
        // them into the source to find out.
        write!(f, "unknown scenario {:?}; registered ids are ", self.asked)?;
        for (position, entry) in SCENARIOS.iter().enumerate() {
            if position > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{:?}", entry.id)?;
        }
        Ok(())
    }
}

impl std::error::Error for UnknownScenario {}

/// Resolves a stable id to its scenario.
///
/// # Errors
///
/// Returns [`UnknownScenario`], which names the registered ids, when `id` is
/// not one of them.
pub fn resolve(id: &str) -> Result<Scenario, UnknownScenario> {
    SCENARIOS
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.scenario)
        .ok_or_else(|| UnknownScenario {
            asked: id.to_string(),
        })
}

/// Everything needed to re-run a failure: which scenario, under which seeded
/// fault plan.
///
/// This is the artifact's replay *value*. `replay_command` is a rendering of
/// it, never an independent string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Replay {
    /// Which scenario to run.
    pub scenario: Scenario,
    /// The fault plan, whose `seed` is the artifact's `seed` field.
    pub plan: FaultPlan,
}

impl Replay {
    /// The replay's arguments, as one field-ordered string.
    ///
    /// Total over the plan: every field that changes behaviour is encoded, so
    /// a decoded `Replay` is equal to the original. A partial encoding would
    /// produce a command that runs *a* scenario rather than *the* failure.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{}:{}",
            self.scenario.id(),
            self.plan.encode_replay_fields()
        )
    }

    /// Parses [`Replay::encode`]'s output.
    ///
    /// # Errors
    ///
    /// Returns a message naming the first field that did not parse.
    //
    // Not a JWT decode. This parses our own replay descriptor —
    // "scenario:seed:sector:lie:write-enospc:torn:flip:dirent-lie:
    // dirent-loss:latency:latency-micros:budget", twelve colon-separated
    // fields. The prior eleven-field descriptor remains readable with
    // `write_enospc = Never`, so already-emitted crashpacks do not become
    // unreplayable merely because the fault vocabulary grew. There is
    // no token, signature, key, claim set or expiry
    // anywhere in it. MEASURED: zero occurrences of `jsonwebtoken` in any
    // manifest in this workspace, and doctrine 1's closed dependency universe
    // forbids adding one, so a JWT finding here is a false positive BY
    // CONSTRUCTION rather than by inspection. The name stays `decode` because
    // it is the counterpart of `encode`; renaming a correct API to satisfy a
    // scanner's substring match would cost more than the waiver.
    // ubs:ignore
    pub fn decode(text: &str) -> Result<Self, String> {
        let (scenario, plan) = text
            .split_once(':')
            .ok_or_else(|| "replay descriptor is missing its fault plan".to_string())?;
        Ok(Self {
            scenario: Scenario::parse(scenario)?,
            plan: FaultPlan::decode_replay_fields(plan)?,
        })
    }

    /// The human-facing command, carrying the replay plus the exact failure
    /// and event-log digest the fresh process must reproduce.
    #[must_use]
    pub fn command_for(&self, failure: &Failure, events: &[FaultEvent]) -> String {
        format!(
            "{ARTIFACT_REPLAY_ENV}={} {ARTIFACT_REPLAY_EXPECTED_DIGEST_ENV}={} cargo test -p fgdb-sim --test sim_artifact -- --ignored replay_from_env",
            self.encode(),
            replay_evidence_digest(failure, events)
        )
    }

    /// Runs the scenario under this plan and reports what happened.
    ///
    /// Deterministic: the fault plan's seed drives every injection, so two
    /// calls with equal `Replay`s produce equal [`RunOutcome::events`].
    /// Latency plans use this runtime's clock-bearing `Cx`, so an encoded replay
    /// containing `latency != Never` is executable rather than panicking in the
    /// clockless VFS constructor.
    #[must_use]
    pub fn run(&self, dir: &Path) -> RunOutcome {
        let replay = *self;
        let scenario = self.scenario;
        let dir = dir.to_path_buf();
        let event_root = dir.clone();
        let mut lab_config = LabConfig::new(self.plan.seed);
        lab_config.auto_advance_time = true;
        let mut lab = LabRuntime::new(lab_config);
        let root_region = lab.state.create_root_region(Budget::INFINITE);
        let (task, mut handle) = lab
            .state
            .create_task(root_region, Budget::INFINITE, async move {
                let root = Cx::current().expect("lab replay task installs its Cx");
                let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
                let vfs = FaultVfs::unix_with_clock(replay.plan, root);
                let failure = match scenario {
                    Scenario::DurableAppend => durable_append(&vfs, &dir, true).await,
                    Scenario::LostAppend => durable_append(&vfs, &dir, false).await,
                    Scenario::SpineDurability => {
                        spine_durability(&commit_cx, &vfs, &dir, false).await
                    }
                    Scenario::PlantedSpineLoss => {
                        spine_durability(&commit_cx, &vfs, &dir, true).await
                    }
                    Scenario::PostD2Recovery(stage) => {
                        post_d2_recovery(&commit_cx, &vfs, &dir, stage).await
                    }
                };
                (failure, vfs.events())
            })
            .expect("create replay task");
        lab.scheduler.lock().schedule(task, 0);
        let _ = lab.run_with_auto_advance();
        let report = lab.report();
        let (failure, mut events) = handle
            .try_join()
            .expect("replay task join remains valid")
            .expect("replay task completed under the bounded lab run");
        assert!(
            report.quiescent,
            "replay did not reach lab quiescence: {report:?}"
        );
        assert!(
            report.invariant_violations.is_empty(),
            "replay violated lab runtime invariants: {:?}",
            report.invariant_violations
        );
        // An artifact is replayed in a fresh directory. Preserve the durable
        // object path within that directory, but do not make a caller's
        // scratch prefix part of the supposedly reproducible schedule.
        for event in &mut events {
            if let Ok(relative) = event.path.strip_prefix(&event_root) {
                event.path = relative.to_path_buf();
            }
        }
        let artifact = failure
            .as_ref()
            .err()
            .map(|failure| FailureArtifact::for_failure(*self, failure, &events));
        let failure = failure.err();
        let receipt = RunReceipt::new(
            *self,
            report.now_nanos,
            &events,
            artifact.is_some(),
            failure.as_ref().map(|_| event_root),
        );
        let execution_root_digest = canonical_run_digest(
            &receipt,
            report.now_nanos,
            failure.as_ref(),
            &events,
            artifact.as_ref(),
        );
        RunOutcome {
            failure,
            events,
            artifact,
            virtual_clock_epoch_nanos: report.now_nanos,
            receipt,
            execution_root_digest,
        }
    }
}

/// What kind of failure a scenario hit.
///
/// Coarse on purpose, and a *type* rather than a string prefix. A shrinker's
/// one real correctness law is that a minimised reproducer must still fail
/// **the same way**: "still errors" happily minimises a lost-write bug into an
/// unrelated I/O error and files the wrong report. Deciding sameness by
/// matching on a message would put that law at the mercy of a byte count in
/// the text, so the kind is carried explicitly and the detail is only prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// Bytes the scenario was told were durable did not survive the crash.
    AcknowledgedBytesLost,
    /// Bytes survived that the scenario required to be gone.
    UnexpectedSurvival,
    /// The sync itself was refused — ENOSPC is the fault class that does this.
    SyncRefused,
    /// A write accepted zero bytes and was refused with ENOSPC. This is a
    /// legal injected outcome, distinct from an unexpected I/O failure.
    WriteRefused,
    /// An open, write, or read failed outright.
    IoFailed,
    /// Chronicle reached D2 and a named derived-publication stage failed. The
    /// artifact carries the typed recovery evidence after verifying the stale
    /// handle fence and authoritative replay.
    CommittedNeedsRecovery,
    /// The recovery scenario did not produce, fence, or replay the exact state
    /// its contract requires. This is a harness-observed protocol regression,
    /// not the expected structured post-D2 failure.
    RecoveryProtocolDrift,
    /// The embedded API acknowledged a commit that ordinary reopen could not
    /// recover with both its frontier and its state intact.
    AcknowledgedCommitLost,
}

/// Structured observation attached to an acknowledged embedded commit that
/// did not survive the crash/reopen boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitDurabilityObservation {
    /// The commit sequence returned to the caller.
    pub acknowledged: CommitSeq,
    /// Whether the simulated process-loss rollback itself completed.
    pub crash_succeeded: bool,
    /// Whether the ordinary production opener recovered a database.
    pub reopen_succeeded: bool,
    /// The frontier the ordinary opener recovered, when readable.
    pub recovered_frontier: Option<CommitSeq>,
    /// Whether the committed vertex was visible after reopen.
    pub recovered_vertex: bool,
    /// True only for the explicit detector-liveness mutation scenario.
    pub planted: bool,
}

/// A failure: its kind, which decides sameness, and its prose, which does not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// What kind of failure this is.
    pub kind: FailureKind,
    /// Human-facing detail. Never load-bearing for a comparison.
    pub detail: String,
    /// Exact integrated recovery evidence when Chronicle reached D2. Direct
    /// VFS scenarios have no database frontier and therefore carry `None`.
    pub recovery: Option<RecoveryRequired>,
    /// Exact integrated durability evidence for an acknowledged commit loss.
    /// Other scenarios carry `None`.
    pub durability: Option<CommitDurabilityObservation>,
}

impl Failure {
    fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            recovery: None,
            durability: None,
        }
    }

    fn committed_needs_recovery(recovery: RecoveryRequired) -> Self {
        Self {
            kind: FailureKind::CommittedNeedsRecovery,
            detail: format!(
                "committed through {:?}; durable_frontier={}; prior_published_frontier={}; \
                 stale handle fenced; engine reopen and independent reference replay agreed",
                recovery.failed_stage, recovery.durable_frontier.0, recovery.published_frontier.0,
            ),
            recovery: Some(recovery),
            durability: None,
        }
    }

    fn acknowledged_commit_lost(observation: CommitDurabilityObservation) -> Self {
        Self {
            kind: FailureKind::AcknowledgedCommitLost,
            detail: format!(
                "acknowledged={}; crash_succeeded={}; reopen_succeeded={}; \
                 recovered_frontier={:?}; recovered_vertex={}; planted={}",
                observation.acknowledged.0,
                observation.crash_succeeded,
                observation.reopen_succeeded,
                observation.recovered_frontier.map(|seq| seq.0),
                observation.recovered_vertex,
                observation.planted,
            ),
            recovery: None,
            durability: Some(observation),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}

/// Canonical same-binary digest for a replay's typed failure and normalized
/// fault-event log.
///
/// The descriptor already binds scenario, seed, and plan. This digest binds
/// the child process to the exact remaining outcome evidence rather than the
/// much weaker predicate "some failure occurred".
#[must_use]
pub fn replay_evidence_digest(failure: &Failure, events: &[FaultEvent]) -> String {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(b"fgdb.sim.replay.evidence.v1");
    hasher.update(format!("{failure:?}").as_bytes());
    for event in events {
        hasher.update(format!("{event:?}").as_bytes());
    }
    hasher.finalize().to_hex()
}

/// Immutable receipt emitted at the execution root for every replay.
///
/// A passing run has no final reproducer and asserts no failure-artifact
/// fields; a failing unshrunk run names its exact run directory and reports
/// zero shrink iterations. [`crate::campaign::FalsificationCampaignRecord`]
/// supersedes that path/count only after an actual shrink has been filed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReceipt {
    replay: Replay,
    scenario_id: &'static str,
    seed: u64,
    virtual_clock_epoch_nanos: u64,
    injected_faults: Vec<FaultEvent>,
    artifact_fields_asserted: Vec<&'static str>,
    shrink_iterations: usize,
    final_reproducer_path: Option<PathBuf>,
}

impl RunReceipt {
    fn new(
        replay: Replay,
        virtual_clock_epoch_nanos: u64,
        injected_faults: &[FaultEvent],
        artifact_emitted: bool,
        final_reproducer_path: Option<PathBuf>,
    ) -> Self {
        Self {
            replay,
            scenario_id: replay.scenario.id(),
            seed: replay.plan.seed,
            virtual_clock_epoch_nanos,
            injected_faults: injected_faults.to_vec(),
            artifact_fields_asserted: if artifact_emitted {
                CONTRACT_FIELDS.to_vec()
            } else {
                Vec::new()
            },
            shrink_iterations: 0,
            final_reproducer_path,
        }
    }

    #[must_use]
    pub const fn scenario_id(&self) -> &'static str {
        self.scenario_id
    }

    /// Exact scenario, seed, and fault/workload plan installed at the
    /// execution root. This is deliberately not caller-supplied at grading
    /// time, so an unrelated replay cannot be spliced onto a real outcome.
    #[must_use]
    pub const fn replay(&self) -> Replay {
        self.replay
    }

    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    #[must_use]
    pub const fn virtual_clock_epoch_nanos(&self) -> u64 {
        self.virtual_clock_epoch_nanos
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

    /// Complete reconstructable log for this one execution.
    #[must_use]
    pub fn log_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "sim-run scenario_id={} seed={:#x} virtual_clock_epoch_nanos={}",
            self.scenario_id, self.seed, self.virtual_clock_epoch_nanos
        )];
        for event in &self.injected_faults {
            lines.push(format!(
                "sim-run injected_fault seq={} class={} path={} detail={:?}",
                event.seq,
                event.kind.class(),
                event.path.display(),
                event.kind,
            ));
        }
        lines.push(format!(
            "sim-run artifact_fields_asserted={}",
            self.artifact_fields_asserted.join(",")
        ));
        lines.push(format!(
            "sim-run shrink_iterations={} final_reproducer_path={}",
            self.shrink_iterations,
            self.final_reproducer_path
                .as_deref()
                .map_or_else(|| "none".to_string(), |path| path.display().to_string())
        ));
        lines
    }
}

/// What one scenario run produced.
#[derive(Debug)]
pub struct RunOutcome {
    /// `Some` when the scenario's expectation did not hold.
    pub failure: Option<Failure>,
    /// Every fault injected, in injection order.
    pub events: Vec<FaultEvent>,
    /// Emitted **iff** `failure` is `Some` — line 1138 binds the artifact to a
    /// *failing* run, so a passing run producing one would be a false record.
    pub artifact: Option<FailureArtifact>,
    /// Lab virtual-clock epoch when the replay reached its terminal report.
    pub virtual_clock_epoch_nanos: u64,
    /// Immutable structured facts logged for this execution.
    pub receipt: RunReceipt,
    /// Seal over the execution-root facts. Public evidence fields remain
    /// inspectable for existing oracle callers, but any post-run mutation is
    /// detected before recording or grading.
    execution_root_digest: String,
}

impl RunOutcome {
    /// Replay identity captured by the execution root.
    #[must_use]
    pub const fn replay(&self) -> Replay {
        self.receipt.replay()
    }

    /// Whether the inspectable fields still match the immutable execution
    /// seal produced by [`Replay::run`].
    #[must_use]
    pub fn execution_root_is_valid(&self) -> bool {
        self.execution_root_digest
            == canonical_run_digest(
                &self.receipt,
                self.virtual_clock_epoch_nanos,
                self.failure.as_ref(),
                &self.events,
                self.artifact.as_ref(),
            )
    }

    /// Canonical digest of every currently replayable run-level fact.
    ///
    /// This binds replay completeness to scenario/seed/plan, exact typed
    /// failure detail, normalized fault events, virtual epoch, and the total
    /// structured-artifact map. Comparing only fault classes and a coarse
    /// failure kind would let semantically different executions claim byte
    /// identity.
    #[must_use]
    pub fn replay_completeness_digest(&self) -> Option<String> {
        if !self.execution_root_is_valid() {
            return None;
        }
        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(b"fgdb.sim.replay.completeness.v1");
        hasher.update(self.receipt.replay.encode().as_bytes());
        hasher.update(&self.virtual_clock_epoch_nanos.to_le_bytes());
        hasher.update(format!("{:?}", self.failure).as_bytes());
        for event in &self.events {
            hasher.update(format!("{event:?}").as_bytes());
        }
        hasher.update(format!("{:?}", self.artifact).as_bytes());
        Some(hasher.finalize().to_hex())
    }
}

fn canonical_run_digest(
    receipt: &RunReceipt,
    virtual_clock_epoch_nanos: u64,
    failure: Option<&Failure>,
    events: &[FaultEvent],
    artifact: Option<&FailureArtifact>,
) -> String {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(b"fgdb.sim.execution-root.v1");
    hasher.update(format!("{receipt:?}").as_bytes());
    hasher.update(&virtual_clock_epoch_nanos.to_le_bytes());
    hasher.update(format!("{failure:?}").as_bytes());
    for event in events {
        hasher.update(format!("{event:?}").as_bytes());
    }
    hasher.update(format!("{artifact:?}").as_bytes());
    hasher.finalize().to_hex()
}

const RECOVERY_GRAPH: GraphId = GraphId(1);
const RECOVERY_BRANCH: BranchId = BranchId(1);
const RECOVERY_RELATION: RelationId = RelationId(1);
const RECOVERY_K_OID: [u8; 32] = [0x5a; 32];
const RECOVERY_NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const RECOVERY_DEK: [u8; 32] = [0x3c; 32];

fn recovery_keys() -> DatabaseKeys {
    DatabaseKeys {
        k_oid: RECOVERY_K_OID,
        namespace: RECOVERY_NAMESPACE,
        dek: RECOVERY_DEK,
    }
}

fn recovery_capsule_keys() -> CapsuleKeys {
    CapsuleKeys {
        k_oid: RECOVERY_K_OID,
        namespace: RECOVERY_NAMESPACE,
        dek: RECOVERY_DEK,
        object_kind: CAPSULE_OBJECT_KIND,
        profile: CapsuleProfile::balanced(),
    }
}

fn recovery_batch(vid: VId) -> WriteBatch {
    let mut batch = WriteBatch::new(RECOVERY_RELATION);
    batch.create_vertex(vid, vec![], vec![]);
    batch
}

fn recovery_drift(detail: impl Into<String>) -> Failure {
    Failure::new(FailureKind::RecoveryProtocolDrift, detail)
}

/// Drive the real embedded composition through one commit, simulated process
/// loss, and an ordinary production reopen. Refusals before acknowledgement
/// are clean outcomes: only a promise already returned to the caller can be
/// falsified by this scenario.
async fn spine_durability(
    cx: &CommitCx,
    vfs: &FaultVfs,
    dir: &Path,
    plant_loss: bool,
) -> Result<(), Failure> {
    drop(
        Database::create(cx, dir, recovery_keys())
            .await
            .map_err(|error| recovery_drift(format!("genesis create failed: {error}")))?,
    );
    let Ok(mut database) = Database::open_with_vfs(cx, vfs.clone(), dir, recovery_keys()).await
    else {
        return Ok(());
    };

    let Ok(acknowledged) = database.write(cx, recovery_batch(VId(1))).await else {
        return Ok(());
    };
    let crash_succeeded = vfs.crash().await.is_ok();
    drop(database);

    let reopened = Database::open(cx, dir, recovery_keys()).await;
    let reopen_succeeded = reopened.is_ok();
    let mut recovered_frontier = reopened
        .as_ref()
        .ok()
        .and_then(|database| database.frontier().ok());
    let mut recovered_vertex = reopened
        .as_ref()
        .ok()
        .and_then(|database| database.vertex(VId(1)).ok())
        .flatten()
        .is_some();
    if plant_loss {
        recovered_frontier = None;
        recovered_vertex = false;
    }
    let observation = CommitDurabilityObservation {
        acknowledged,
        crash_succeeded,
        reopen_succeeded,
        recovered_frontier,
        recovered_vertex,
        planted: plant_loss,
    };

    if crash_succeeded && recovered_frontier == Some(acknowledged) && recovered_vertex {
        Ok(())
    } else {
        Err(Failure::acknowledged_commit_lost(observation))
    }
}

/// Drive the real composition-layer database through one post-D2 failure,
/// then prove the emitted evidence names a genuinely recoverable commit.
///
/// This is deliberately not a second recovery implementation. It calls the
/// public fault-matrix surface, observes the same typed state callers see,
/// drops every engine handle, and asks the ordinary opener plus the independent
/// reference replay what the Chronicle bytes mean.
async fn post_d2_recovery(
    cx: &CommitCx,
    vfs: &FaultVfs,
    dir: &Path,
    stage: DerivedPublicationStage,
) -> Result<(), Failure> {
    drop(
        Database::create(cx, dir, recovery_keys())
            .await
            .map_err(|error| recovery_drift(format!("genesis create failed: {error}")))?,
    );
    let mut database = Database::open_with_vfs(cx, vfs.clone(), dir, recovery_keys())
        .await
        .map_err(|error| recovery_drift(format!("faultable open failed: {error}")))?;

    let error = database
        .write_with_publication_failure(cx, recovery_batch(VId(1)), stage)
        .await
        .map(|seq| {
            recovery_drift(format!(
                "{stage:?} returned success at {seq:?} instead of a post-D2 recovery error"
            ))
        })
        .unwrap_or_else(|error| match error {
            WriteError::CommittedNeedsRecovery { recovery, source }
                if recovery.durable_frontier == CommitSeq(1)
                    && recovery.published_frontier == CommitSeq(0)
                    && recovery.failed_stage == stage
                    && matches!(
                        *source,
                        RebuildError::InjectedPublicationFailure(found) if found == stage
                    ) =>
            {
                Failure::committed_needs_recovery(recovery)
            }
            other => recovery_drift(format!(
                "{stage:?} returned the wrong typed recovery outcome: {other:?}"
            )),
        });
    if error.kind != FailureKind::CommittedNeedsRecovery {
        return Err(error);
    }
    let Some(recovery) = error.recovery else {
        return Err(recovery_drift(format!(
            "{stage:?}: CommittedNeedsRecovery omitted its recovery evidence"
        )));
    };

    let observed_state = database.state();
    if !matches!(
        observed_state,
        DatabaseState::NeedsAuthoritativeRecovery(found) if found == recovery
    ) {
        return Err(recovery_drift(format!(
            "{stage:?}: handle state {:?} did not retain {recovery:?}",
            observed_state
        )));
    }
    if !matches!(
        database.vertex(VId(1)),
        Err(ReadError::RecoveryRequired(found)) if found == recovery
    ) {
        return Err(recovery_drift(format!(
            "{stage:?}: a state-bearing read escaped the recovery fence"
        )));
    }
    if !matches!(
        database.write(cx, recovery_batch(VId(2))).await,
        Err(WriteError::RecoveryRequired(found)) if found == recovery
    ) {
        return Err(recovery_drift(format!(
            "{stage:?}: a second write escaped the recovery fence"
        )));
    }
    drop(database);

    let reopened = Database::open(cx, dir, recovery_keys())
        .await
        .map_err(|open_error| {
            recovery_drift(format!(
                "{stage:?}: authoritative engine reopen failed: {open_error}"
            ))
        })?;
    let engine_vertices = reopened.vertices().map_err(|read_error| {
        recovery_drift(format!(
            "{stage:?}: reopened engine could not read: {read_error}"
        ))
    })?;
    let exactly_durable_vertex =
        matches!(engine_vertices.as_slice(), [vertex] if vertex.vid == VId(1));
    if !matches!(reopened.frontier(), Ok(CommitSeq(1))) || !exactly_durable_vertex {
        return Err(recovery_drift(format!(
            "{stage:?}: reopened engine did not expose exactly the durable vertex"
        )));
    }
    drop(reopened);

    let coordinator = CommitCoordinator::open(cx, dir, recovery_capsule_keys())
        .await
        .map_err(|commit_error| {
            recovery_drift(format!(
                "{stage:?}: independent Chronicle open failed: {commit_error}"
            ))
        })?;
    let replayed = crate::replay(cx, &coordinator)
        .await
        .map_err(|replay_error| {
            recovery_drift(format!(
                "{stage:?}: independent reference replay failed: {replay_error}"
            ))
        })?;
    let graph = replayed
        .database
        .graph(RECOVERY_GRAPH, RECOVERY_BRANCH)
        .ok_or_else(|| recovery_drift(format!("{stage:?}: reference graph is absent")))?;
    if graph.vertex_count() != 1 || graph.vertex(VId(1)).is_none() {
        return Err(recovery_drift(format!(
            "{stage:?}: reference replay did not include the durable vertex exactly once"
        )));
    }

    Err(error)
}

/// Writes four sectors through one handle, syncs, crashes, and checks the
/// durable bytes against `expect_durable`.
async fn durable_append(vfs: &FaultVfs, dir: &Path, expect_durable: bool) -> Result<(), Failure> {
    let path = dir.join("append.log");
    let mut written = Vec::new();
    for sector in 0u8..4 {
        written.extend(std::iter::repeat_n(sector + 1, 512));
    }

    let mut file = vfs
        .open(
            &path,
            &OpenOptions::new().write(true).create(true).truncate(true),
        )
        .await
        .map_err(|error| Failure::new(FailureKind::IoFailed, format!("open failed: {error}")))?;
    let mut done = 0usize;
    while done < written.len() {
        let n = poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &written[done..]))
            .await
            .map_err(|error| {
                let kind = if error.raw_os_error() == Some(28) {
                    FailureKind::WriteRefused
                } else {
                    FailureKind::IoFailed
                };
                Failure::new(kind, format!("write failed: {error}"))
            })?;
        if n == 0 {
            return Err(Failure::new(
                FailureKind::IoFailed,
                "write made no progress",
            ));
        }
        done += n;
    }
    // A refused sync is a legitimate outcome, not a harness error: ENOSPC is
    // one of the fault classes under test.
    if let Err(error) = VfsFile::sync_all(&file).await {
        return Err(Failure::new(
            FailureKind::SyncRefused,
            format!("sync failed: {error}"),
        ));
    }
    vfs.crash().await.map_err(|error| {
        Failure::new(
            FailureKind::IoFailed,
            format!("crash rollback failed: {error}"),
        )
    })?;

    let durable = vfs.read(&path).await.map_err(|error| {
        Failure::new(
            FailureKind::IoFailed,
            format!("read after crash failed: {error}"),
        )
    })?;
    if expect_durable {
        if durable == written {
            Ok(())
        } else {
            Err(Failure::new(
                FailureKind::AcknowledgedBytesLost,
                format!(
                    "acknowledged {} bytes, {} survived the crash",
                    written.len(),
                    durable.len()
                ),
            ))
        }
    } else if durable.is_empty() {
        Ok(())
    } else {
        Err(Failure::new(
            FailureKind::UnexpectedSurvival,
            format!("expected nothing durable, {} bytes survived", durable.len()),
        ))
    }
}

// ---------------------------------------------------------------------------
// The artifact
// ---------------------------------------------------------------------------

/// A total map over [`CONTRACT_FIELDS`].
#[derive(Clone, Debug)]
pub struct FailureArtifact {
    fields: BTreeMap<&'static str, Field>,
    replay: Replay,
    failure_kind: FailureKind,
}

/// The absence every field owes to a subsystem that does not exist yet.
fn not_yet(subsystem: &'static str, bead: &'static str) -> Field {
    Field::Absent(Absence::NotYetBuilt { subsystem, bead })
}

fn not_applicable(because: &'static str) -> Field {
    Field::Absent(Absence::NotApplicable { because })
}

impl FailureArtifact {
    /// Builds the artifact for a failing run.
    ///
    /// Every one of [`CONTRACT_FIELDS`] is written, including the ones with no
    /// referent at this HEAD — those carry [`Absence::NotYetBuilt`] naming the
    /// bead that will supply them. That is what makes the completeness test
    /// non-vacuous: this function cannot compile a field away, and cannot omit
    /// one without the test noticing.
    #[must_use]
    pub fn for_failure(replay: Replay, failure: &Failure, events: &[FaultEvent]) -> Self {
        let mut fields: BTreeMap<&'static str, Field> = BTreeMap::new();
        let mut set = |name: &'static str, field: Field| {
            fields.insert(name, field);
        };

        set("seed", Field::Present(format!("{:#x}", replay.plan.seed)));
        // The lab VFS's schedule is its injected-fault sequence. Integrated
        // post-D2 scenarios add the named publication boundary even under a
        // faultless VFS: that boundary is the deterministic decision that made
        // the run stop, and omitting it would leave a recovery artifact with an
        // unexplained empty schedule.
        let mut schedule = String::new();
        for event in events {
            let _ = write!(schedule, "[{} {:?}]", event.seq, event.kind);
        }
        if let Some(recovery) = failure.recovery {
            let _ = write!(schedule, "[post-d2 {:?}]", recovery.failed_stage);
        }
        set(
            "schedule",
            if schedule.is_empty() {
                Field::Absent(Absence::NotApplicable {
                    because: "no fault was injected; the run failed under a faultless plan",
                })
            } else {
                Field::Present(schedule)
            },
        );
        set(
            "crashpoint",
            Field::Present(replay.scenario.id().to_string()),
        );

        set(
            "role",
            if failure.recovery.is_some() || failure.durability.is_some() {
                Field::Present("commit".to_string())
            } else {
                Field::Absent(Absence::NotApplicable {
                    because: "the lab VFS is role-agnostic; it sits below the role split",
                })
            },
        );
        set(
            "group",
            Field::Absent(Absence::NotApplicable {
                because: "single-process lab run; there is no group",
            }),
        );
        set("configuration", Field::Present(replay.encode()));
        set(
            "topology",
            not_applicable("single-process fixture has no cluster topology"),
        );
        set(
            "incarnation",
            not_applicable("this fixture does not execute restore incarnation changes"),
        );
        set(
            "service_visibility_epoch",
            not_applicable("embedded fixture has no server service-visibility transition"),
        );

        if let Some(recovery) = failure.recovery {
            let durable = recovery.durable_frontier.0.to_string();
            set("logical_position", Field::Present(durable.clone()));
            set("commit_position", Field::Present(durable.clone()));
            // The scenario does not emit until ordinary engine open and the
            // independent oracle have both applied this exact durable prefix.
            set("applied_position", Field::Present(durable));
            // The prior snapshot frontier is the visibility claim the fenced
            // handle was forbidden to keep serving as current.
            set(
                "visible_position",
                Field::Present(recovery.published_frontier.0.to_string()),
            );
            set(
                "raft_position",
                not_applicable("single-member embedded fixture does not execute Raft"),
            );
            set(
                "audit_visible_position",
                not_applicable("fixture has no audit-visibility publication stage"),
            );
        } else if let Some(durability) = failure.durability {
            let acknowledged = durability.acknowledged.0.to_string();
            set("logical_position", Field::Present(acknowledged.clone()));
            set("commit_position", Field::Present(acknowledged));
            set(
                "raft_position",
                not_applicable("single-member embedded fixture does not execute Raft"),
            );
            for position in ["applied_position", "visible_position"] {
                set(
                    position,
                    durability.recovered_frontier.map_or_else(
                        || {
                            Field::Absent(Absence::NotApplicable {
                                because: "ordinary reopen recovered no readable frontier",
                            })
                        },
                        |frontier| Field::Present(frontier.0.to_string()),
                    ),
                );
            }
            set(
                "audit_visible_position",
                not_applicable("fixture has no audit-visibility publication stage"),
            );
        } else {
            for position in [
                "logical_position",
                "commit_position",
                "raft_position",
                "applied_position",
                "visible_position",
                "audit_visible_position",
            ] {
                set(
                    position,
                    not_applicable(
                        "direct VFS scenario runs below Chronicle and has no commit/Raft position",
                    ),
                );
            }
        }
        for identifier in [
            "attempt_identifier",
            "generation_identifier",
            "statement_identifier",
            "workspace_identifier",
            "backup_identifier",
            "restore_identifier",
            "gc_identifier",
            "key_identifier",
        ] {
            set(
                identifier,
                not_applicable(
                    "this fixture does not execute a session transaction-lifecycle protocol",
                ),
            );
        }
        set(
            "object_identity",
            if failure.recovery.is_some() || failure.durability.is_some() {
                not_yet(
                    "complete durable-object identity pipeline",
                    "fgdb-w2-object-identity-t0f",
                )
            } else {
                not_applicable("direct VFS scenario has no Chronicle object identity")
            },
        );
        for identity in [
            "spec_identity",
            "result_identity",
            "certificate_identity",
            "grant_identity",
            "floor_identity",
        ] {
            set(
                identity,
                not_applicable(
                    "this fixture does not execute the corresponding specification/result/grant surface",
                ),
            );
        }

        set(
            "expected",
            Field::Present(replay.scenario.entry().asserts.to_string()),
        );
        // The kind leads, because that is what decides whether two runs
        // failed the same way; the prose follows it.
        set("actual", Field::Present(failure.to_string()));
        set(
            "replay_command",
            Field::Present(replay.command_for(failure, events)),
        );

        Self {
            fields,
            replay,
            failure_kind: failure.kind,
        }
    }

    /// The field, if the name is one of [`CONTRACT_FIELDS`].
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.get(name)
    }

    /// The replay value this artifact was built from.
    #[must_use]
    pub const fn replay(&self) -> Replay {
        self.replay
    }

    /// Typed failure class this artifact records.
    ///
    /// Campaign evidence uses this to prove a shrunk reproducer still accuses
    /// the same defect as the artifact it is being filed for. Reading the
    /// human `actual` field back into a type would make that law depend on
    /// formatting.
    #[must_use]
    pub const fn failure_kind(&self) -> FailureKind {
        self.failure_kind
    }

    /// Contract-field names the artifact failed to account for.
    ///
    /// Empty is the contract. Non-empty names exactly which fields were
    /// dropped — the assertion line 1138 calls "require every applicable
    /// field", made total by treating inapplicability as a written answer
    /// rather than an omission.
    #[must_use]
    pub fn unaccounted_fields(&self) -> Vec<&'static str> {
        CONTRACT_FIELDS
            .iter()
            .filter(|name| !self.fields.contains_key(*name))
            .copied()
            .collect()
    }

    /// Field names present in the artifact that line 1138 does not spell.
    ///
    /// The other direction of the same closure: a field invented here would
    /// otherwise look like coverage.
    #[must_use]
    pub fn unregistered_fields(&self) -> Vec<&'static str> {
        self.fields
            .keys()
            .filter(|name| !CONTRACT_FIELDS.contains(*name))
            .copied()
            .collect()
    }
}
