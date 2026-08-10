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
//! from it* by [`Replay::encode`]. The contract test asserts three things
//! together, and it is the conjunction that closes the hole:
//!
//! 1. the encoded string decodes back to an equal [`Replay`] (the string cannot
//!    drift from the value);
//! 2. running that decoded value reproduces a **byte-identical fault event
//!    log** and the same failure (the value genuinely replays);
//! 3. a scenario that does not fail emits **no artifact at all** (the control —
//!    without it, an emitter that always emitted would pass 1 and 2).
//!
//! # What is honestly not covered yet
//!
//! `Replay::encode` produces the arguments of a replay, and
//! [`ARTIFACT_REPLAY_ENV`] is the variable a runner reads them from. Executing
//! it as a *subprocess* — the literal reading of "execute the replay command" —
//! is not done here; the contract test executes the replay **in process**.
//! That is strictly stronger than the placebo (there is a consumer, and it
//! reproduces the failure) and strictly weaker than the plan's sentence. Stated
//! rather than blurred.

use crate::vfs::{FaultEvent, FaultPlan, FaultVfs, Trigger};
use asupersync::fs::{OpenOptions, Vfs, VfsFile};
use asupersync::io::AsyncWrite;
use asupersync::{Budget, runtime::RuntimeBuilder};
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
use std::path::Path;
use std::pin::Pin;

/// The environment variable a replay runner reads. Named here so the emitter
/// and the consumer cannot disagree about it — the exact disagreement that
/// makes fgdb-4bxh's upstream replay command inert.
pub const ARTIFACT_REPLAY_ENV: &str = "FGDB_SIM_REPLAY";

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
/// A closed enum rather than a dynamic registry: a replay must be executable
/// from a decoded string in a *fresh process*, and a registry populated at
/// runtime cannot promise that the id in an artifact still resolves. When the
/// campaign registry (fgdb-verif-sim-q97e) lands, it owns the general case;
/// this is the part that has to work now.
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
    pub const COUNT: usize = 11;

    /// A dense index per variant, `0..COUNT`.
    ///
    /// Exists so completeness is checkable by arithmetic rather than by
    /// someone remembering to update a list.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::DurableAppend => 0,
            Self::LostAppend => 1,
            Self::PostD2Recovery(stage) => {
                2 + match stage {
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
            Self::DurableAppend | Self::LostAppend => None,
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

/// The scenario registry.
///
/// **Why a const table and not a registration API.** A replay must resolve its
/// scenario id in a *fresh process* — that is the whole point of
/// [`Replay::encode`] producing a string a human can paste. A registry
/// populated at runtime cannot promise that: whether an id resolves would
/// depend on which registration calls that particular binary happened to make
/// before the lookup, so the same artifact would replay in one process and
/// fail in another. Every entry being a compile-time constant makes
/// resolution a property of the binary rather than of its startup path.
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

fn encode_trigger(trigger: Trigger) -> String {
    match trigger {
        Trigger::Never => "never".to_string(),
        Trigger::Always => "always".to_string(),
        Trigger::Nth(n) => format!("nth{n}"),
        Trigger::At(n) => format!("at{n}"),
        Trigger::PerMille(p) => format!("pm{p}"),
    }
}

fn decode_trigger(text: &str) -> Result<Trigger, String> {
    if text == "never" {
        return Ok(Trigger::Never);
    }
    if text == "always" {
        return Ok(Trigger::Always);
    }
    if let Some(rest) = text.strip_prefix("nth") {
        return rest
            .parse()
            .map(Trigger::Nth)
            .map_err(|_| format!("bad Nth trigger {text:?}"));
    }
    if let Some(rest) = text.strip_prefix("at") {
        return rest
            .parse()
            .map(Trigger::At)
            .map_err(|_| format!("bad At trigger {text:?}"));
    }
    if let Some(rest) = text.strip_prefix("pm") {
        return rest
            .parse()
            .map(Trigger::PerMille)
            .map_err(|_| format!("bad PerMille trigger {text:?}"));
    }
    Err(format!("unknown trigger {text:?}"))
}

impl Replay {
    /// The replay's arguments, as one field-ordered string.
    ///
    /// Total over the plan: every field that changes behaviour is encoded, so
    /// a decoded `Replay` is equal to the original. A partial encoding would
    /// produce a command that runs *a* scenario rather than *the* failure.
    #[must_use]
    pub fn encode(&self) -> String {
        let budget = match self.plan.space_budget {
            Some(bytes) => bytes.to_string(),
            None => "none".to_string(),
        };
        format!(
            "{}:{:#x}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            self.scenario.id(),
            self.plan.seed,
            self.plan.sector_bytes,
            encode_trigger(self.plan.fsync_lie),
            encode_trigger(self.plan.torn_write),
            encode_trigger(self.plan.bit_flip),
            encode_trigger(self.plan.dirent_lie),
            encode_trigger(self.plan.dirent_loss),
            encode_trigger(self.plan.latency),
            self.plan.latency_micros,
            budget,
        )
    }

    /// Parses [`Replay::encode`]'s output.
    ///
    /// # Errors
    ///
    /// Returns a message naming the first field that did not parse.
    //
    // Not a JWT decode. This parses our own replay descriptor —
    // "scenario:seed:sector:lie:torn:flip:dirent-lie:dirent-loss:latency:
    // latency-micros:budget", eleven colon-separated fields — and there is
    // no token, signature, key, claim set or expiry
    // anywhere in it. MEASURED: zero occurrences of `jsonwebtoken` in any
    // manifest in this workspace, and doctrine 1's closed dependency universe
    // forbids adding one, so a JWT finding here is a false positive BY
    // CONSTRUCTION rather than by inspection. The name stays `decode` because
    // it is the counterpart of `encode`; renaming a correct API to satisfy a
    // scanner's substring match would cost more than the waiver.
    // ubs:ignore
    pub fn decode(text: &str) -> Result<Self, String> {
        let parts: Vec<&str> = text.split(':').collect();
        let [
            scenario,
            seed,
            sector,
            lie,
            torn,
            flip,
            dirent_lie,
            dirent_loss,
            latency,
            latency_micros,
            budget,
        ] = parts.as_slice()
        else {
            return Err(format!(
                "expected 11 colon-separated fields, got {}",
                parts.len()
            ));
        };
        let seed = seed
            .strip_prefix("0x")
            .ok_or_else(|| format!("seed {seed:?} is not 0x-prefixed"))
            .and_then(|hex| {
                u64::from_str_radix(hex, 16).map_err(|_| format!("bad seed {seed:?}"))
            })?;
        Ok(Self {
            scenario: Scenario::parse(scenario)?,
            plan: FaultPlan {
                seed,
                sector_bytes: sector
                    .parse()
                    .map_err(|_| format!("bad sector_bytes {sector:?}"))?,
                fsync_lie: decode_trigger(lie)?,
                torn_write: decode_trigger(torn)?,
                bit_flip: decode_trigger(flip)?,
                dirent_lie: decode_trigger(dirent_lie)?,
                dirent_loss: decode_trigger(dirent_loss)?,
                latency: decode_trigger(latency)?,
                latency_micros: latency_micros
                    .parse()
                    .map_err(|_| format!("bad latency_micros {latency_micros:?}"))?,
                space_budget: if *budget == "none" {
                    None
                } else {
                    Some(
                        budget
                            .parse()
                            .map_err(|_| format!("bad space_budget {budget:?}"))?,
                    )
                },
            },
        })
    }

    /// The human-facing command, whose arguments are exactly [`Replay::encode`].
    #[must_use]
    pub fn command(&self) -> String {
        format!(
            "{ARTIFACT_REPLAY_ENV}={} cargo test -p fgdb-sim --test sim_artifact -- --ignored replay_from_env",
            self.encode()
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
        let runtime = RuntimeBuilder::new()
            .build()
            .expect("the replay runtime builds");
        let root = runtime.request_cx_with_budget(Budget::INFINITE);
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let vfs = FaultVfs::unix_with_clock(self.plan, root);
        let scenario = self.scenario;
        let failure = runtime.block_on(async {
            match scenario {
                Scenario::DurableAppend => durable_append(&vfs, dir, true).await,
                Scenario::LostAppend => durable_append(&vfs, dir, false).await,
                Scenario::PostD2Recovery(stage) => {
                    post_d2_recovery(&commit_cx, &vfs, dir, stage).await
                }
            }
        });
        let events = vfs.events();
        let artifact = failure
            .as_ref()
            .err()
            .map(|failure| FailureArtifact::for_failure(*self, failure, &events));
        RunOutcome {
            failure: failure.err(),
            events,
            artifact,
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
}

impl Failure {
    fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            recovery: None,
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
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}

/// What one scenario run produced.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    /// `Some` when the scenario's expectation did not hold.
    pub failure: Option<Failure>,
    /// Every fault injected, in injection order.
    pub events: Vec<FaultEvent>,
    /// Emitted **iff** `failure` is `Some` — line 1138 binds the artifact to a
    /// *failing* run, so a passing run producing one would be a false record.
    pub artifact: Option<FailureArtifact>,
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
                Failure::new(FailureKind::IoFailed, format!("write failed: {error}"))
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
}

/// The absence every field owes to a subsystem that does not exist yet.
fn not_yet(subsystem: &'static str, bead: &'static str) -> Field {
    Field::Absent(Absence::NotYetBuilt { subsystem, bead })
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
            if failure.recovery.is_some() {
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
        set("topology", not_yet("W12 topology", "fgdb-verif-sim-q97e"));
        set("incarnation", not_yet("restore incarnations", "fgdb-1xtp"));
        set(
            "service_visibility_epoch",
            not_yet("service visibility", "fgdb-verif-sim-q97e"),
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
            set("raft_position", not_yet("Raft replication", "fgdb-1xtp"));
            set(
                "audit_visible_position",
                not_yet("audit visibility", "fgdb-verif-sim-q97e"),
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
                    not_yet(
                        "the commit/Raft position vector; this scenario drives the VFS directly, below Chronicle",
                        "fgdb-1xtp",
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
                not_yet("the transaction lifecycle", "fgdb-verif-sim-q97e"),
            );
        }
        for identity in [
            "object_identity",
            "spec_identity",
            "result_identity",
            "certificate_identity",
            "grant_identity",
            "floor_identity",
        ] {
            set(identity, not_yet("durable object identities", "fgdb-1xtp"));
        }

        set(
            "expected",
            Field::Present(replay.scenario.entry().asserts.to_string()),
        );
        // The kind leads, because that is what decides whether two runs
        // failed the same way; the prose follows it.
        set("actual", Field::Present(failure.to_string()));
        set("replay_command", Field::Present(replay.command()));

        Self { fields, replay }
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
