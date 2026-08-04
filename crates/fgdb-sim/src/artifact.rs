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
use asupersync::runtime::RuntimeBuilder;
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
}

impl Scenario {
    /// The stable wire name, used in [`Replay::encode`].
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::DurableAppend => "durable-append",
            Self::LostAppend => "lost-append",
        }
    }

    fn parse(text: &str) -> Result<Self, String> {
        match text {
            "durable-append" => Ok(Self::DurableAppend),
            "lost-append" => Ok(Self::LostAppend),
            other => Err(format!("unknown scenario {other:?}")),
        }
    }
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
            "{}:{:#x}:{}:{}:{}:{}:{}",
            self.scenario.id(),
            self.plan.seed,
            self.plan.sector_bytes,
            encode_trigger(self.plan.fsync_lie),
            encode_trigger(self.plan.torn_write),
            encode_trigger(self.plan.bit_flip),
            budget,
        )
    }

    /// Parses [`Replay::encode`]'s output.
    ///
    /// # Errors
    ///
    /// Returns a message naming the first field that did not parse.
    pub fn decode(text: &str) -> Result<Self, String> {
        let parts: Vec<&str> = text.split(':').collect();
        let [scenario, seed, sector, lie, torn, flip, budget] = parts.as_slice() else {
            return Err(format!(
                "expected 7 colon-separated fields, got {}",
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
    #[must_use]
    pub fn run(&self, dir: &Path) -> RunOutcome {
        let vfs = FaultVfs::unix(self.plan);
        let scenario = self.scenario;
        let runtime = RuntimeBuilder::new()
            .build()
            .expect("the lab runtime builds");
        let failure = runtime.block_on(async {
            match scenario {
                Scenario::DurableAppend => durable_append(&vfs, dir, true).await,
                Scenario::LostAppend => durable_append(&vfs, dir, false).await,
            }
        });
        let events = vfs.events();
        let artifact = failure
            .as_ref()
            .err()
            .map(|reason| FailureArtifact::for_failure(*self, reason, &events));
        RunOutcome {
            failure: failure.err(),
            events,
            artifact,
        }
    }
}

/// What one scenario run produced.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    /// `Some` when the scenario's expectation did not hold.
    pub failure: Option<String>,
    /// Every fault injected, in injection order.
    pub events: Vec<FaultEvent>,
    /// Emitted **iff** `failure` is `Some` — line 1138 binds the artifact to a
    /// *failing* run, so a passing run producing one would be a false record.
    pub artifact: Option<FailureArtifact>,
}

/// Writes four sectors through one handle, syncs, crashes, and checks the
/// durable bytes against `expect_durable`.
async fn durable_append(vfs: &FaultVfs, dir: &Path, expect_durable: bool) -> Result<(), String> {
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
        .map_err(|error| format!("open failed: {error}"))?;
    let mut done = 0usize;
    while done < written.len() {
        let n = poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &written[done..]))
            .await
            .map_err(|error| format!("write failed: {error}"))?;
        if n == 0 {
            return Err("write made no progress".to_string());
        }
        done += n;
    }
    // A refused sync is a legitimate outcome, not a harness error: ENOSPC is
    // one of the fault classes under test.
    if let Err(error) = VfsFile::sync_all(&file).await {
        return Err(format!("sync failed: {error}"));
    }
    vfs.crash();

    let durable = vfs
        .read(&path)
        .await
        .map_err(|error| format!("read after crash failed: {error}"))?;
    if expect_durable {
        if durable == written {
            Ok(())
        } else {
            Err(format!(
                "acknowledged {} bytes, {} survived the crash",
                written.len(),
                durable.len()
            ))
        }
    } else if durable.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "expected nothing durable, {} bytes survived",
            durable.len()
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
    pub fn for_failure(replay: Replay, reason: &str, events: &[FaultEvent]) -> Self {
        let mut fields: BTreeMap<&'static str, Field> = BTreeMap::new();
        let mut set = |name: &'static str, field: Field| {
            fields.insert(name, field);
        };

        set("seed", Field::Present(format!("{:#x}", replay.plan.seed)));
        // The lab VFS's "schedule" is its injected-fault sequence: the ordered
        // decisions that made this run differ from a faultless one.
        let mut schedule = String::new();
        for event in events {
            let _ = write!(schedule, "[{} {:?}]", event.seq, event.kind);
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
            Field::Absent(Absence::NotApplicable {
                because: "the lab VFS is role-agnostic; it sits below the role split",
            }),
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
            Field::Present(match replay.scenario {
                Scenario::DurableAppend => "every acknowledged byte survives the crash".to_string(),
                Scenario::LostAppend => "nothing survives the crash".to_string(),
            }),
        );
        set("actual", Field::Present(reason.to_string()));
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
