//! `fgdb-sim` — the database under asupersync's lab runtime (plan §15).
//!
//! This is where the two halves of the system are made to meet. Chronicle can
//! make a mutation durable and recover it across a crash at any instant of the
//! two-fsync protocol. `fgdb-reference` can turn delta rows into a graph. Until
//! they are joined, each is only *plausible*: Chronicle's tests prove bytes
//! survive without asking what they mean, and the oracle's tests prove rows
//! materialize without asking whether those rows were ever durable.
//!
//! THE LAW THIS CRATE EXISTS FOR: **the graph you get after a crash is exactly
//! the graph implied by the commits that reached D2.** Not a superset (an
//! orphan capsule must contribute nothing, even though its bytes are on disk
//! and readable), and not a subset (a commit that was acknowledged must be
//! there). That single sentence is B1's "one version universe" and B5's
//! determinism made checkable, and neither half can state it alone.
//!
//! WHY THIS CRATE AND NOT ONE OF THE OTHER TWO. `fgdb-reference` carries a
//! registered dependency allowlist (§15.2) that names `fgdb-chronicle` as a
//! CI-rejected import, precisely so the differential cannot be gutted by code
//! sharing; and Chronicle's layer may not depend on verification. The
//! verification layer is the only place both are visible, which is not an
//! accident of the map — it is the map enforcing that the oracle and the engine
//! stay independent implementations that can disagree.

#![forbid(unsafe_code)]

pub mod artifact;
pub mod campaign;
pub mod completeness;
pub mod dual_run;
pub mod fixture;
pub mod ldfi;
pub mod redaction;
pub mod shrink;
pub mod vfs;

use fgdb_chronicle::commit::{CommitCoordinator, CommitError};
use fgdb_chronicle::identity::CryptoVerificationEvent;
use fgdb_chronicle::marker::EffectSource;
use fgdb_crypto::Digest;
use fgdb_delta_types::{
    CanonicalError, CommittedMarker, IndexError, LocalDeltaBatchIndex, LogicalDeltaBatch,
    LogicalDeltaTemplate,
};
use fgdb_reference::{ApplyError, ReferenceDatabase};
use fgdb_types::MarkerRef;
use fgdb_types::context::CommitCx;
use fgdb_types::ids::{DatabaseId, ObjectId};

/// **The capsule-commit vocabulary lives in `fgdb`, not here** (`fgdb-khec`).
///
/// These six items were defined in BOTH crates for a while: `fgdb` needed them
/// for the spine's real commit path, and this crate had defined them first. Two
/// definitions of "what bytes is a capsule, and what digest does its marker
/// declare" is exactly the disagreement content-addressing exists to prevent —
/// one of them would eventually drift and the failure would look like corruption.
///
/// `fgdb` is the right home rather than `fgdb-chronicle`: `prepare_capsule` takes
/// a `LogicalDeltaTemplate`, and Chronicle does not depend on `fgdb-delta-types`.
/// The glue bridges the delta vocabulary and the durability substrate, which is
/// the composition layer's job. Re-exported here so the five test files that
/// drive them keep one import path, and because this crate's own `replay` needs
/// `template_digest` to verify what a marker committed to.
pub use fgdb::{
    CAPSULE_OBJECT_KIND, PreparedCapsule, TEMPLATE_DIGEST_DOMAIN, marker_for_capsule,
    prepare_capsule, template_digest,
};

/// Domain for the verification-layer stand-in for `RootSlot.database_id`.
const REFERENCE_DATABASE_ID_DOMAIN: &[u8] = b"fgdb.reference.replay-database-id.v1";

/// Why replaying a durable commit stream into graph state failed.
#[derive(Debug)]
pub enum ReplayError {
    Commit(CommitError),
    /// A committed marker names a capsule whose bytes are not on disk. This is
    /// unrecoverable rather than an orphan: the marker IS the commit, so its
    /// capsule was durable before the marker was written, and its absence means
    /// something deleted bytes the commit stream still references.
    MissingCapsule {
        commit_seq: u64,
        capsule_oid: ObjectId,
    },
    /// The capsule's bytes do not hash to the digest its marker declared.
    ///
    /// FG-INV-09's shape: identities recompute from their registered bytes. A
    /// reader that skipped this would materialize whatever it found, so silent
    /// corruption would become silently different graph state — the failure a
    /// content-addressed store exists to make impossible.
    TemplateDigestMismatch {
        commit_seq: u64,
        declared: Digest,
        recomputed: Digest,
    },
    /// The capsule's bytes are not a decodable template.
    Decode {
        commit_seq: u64,
        error: CanonicalError,
    },
    /// The template decoded but does not apply to the state the prior commits
    /// produced.
    Apply {
        commit_seq: u64,
        error: ApplyError,
    },
    /// The batch derived from a committed template was refused by the delta
    /// index. plan:397 names the modes: "a missing, duplicate, gapped,
    /// wrong-marker, or wrong-frontier insertion fails apply."
    Index {
        commit_seq: u64,
        error: IndexError,
    },
}

impl core::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Commit(error) => write!(f, "commit stream: {error}"),
            Self::MissingCapsule {
                commit_seq,
                capsule_oid,
            } => write!(
                f,
                "commit {commit_seq} names capsule {capsule_oid:?}, which is not on disk"
            ),
            Self::TemplateDigestMismatch { commit_seq, .. } => write!(
                f,
                "commit {commit_seq}: capsule bytes do not hash to the declared template digest"
            ),
            Self::Decode { commit_seq, error } => {
                write!(f, "commit {commit_seq}: capsule does not decode: {error}")
            }
            Self::Apply { commit_seq, error } => {
                write!(f, "commit {commit_seq}: template does not apply: {error}")
            }
            Self::Index { commit_seq, error } => {
                write!(
                    f,
                    "commit {commit_seq}: delta index refused the batch: {error}"
                )
            }
        }
    }
}

impl core::error::Error for ReplayError {}

impl From<CommitError> for ReplayError {
    fn from(error: CommitError) -> Self {
        Self::Commit(error)
    }
}

/// Commit a prepared capsule.
///
/// Stays here rather than moving to `fgdb` with the rest: the spine calls
/// `CommitCoordinator::commit_with_crash` directly because it must thread its
/// own crash point through, so this wrapper has exactly one consumer — the
/// verification suites — and moving it would put an unused function in the
/// engine's public surface.
pub async fn commit_capsule<V: asupersync::fs::Vfs>(
    coordinator: &mut CommitCoordinator<V>,
    cx: &CommitCx,
    capsule: &PreparedCapsule,
    head_updates: Vec<fgdb_chronicle::marker::HeadUpdate>,
) -> Result<MarkerRef, CommitError> {
    // The coordinator DERIVES the identity from the bytes it seals, so the
    // marker names whatever the store actually wrote rather than whatever the
    // caller believed. `PreparedCapsule::object_id` is the same value computed
    // independently, and the e2e suite asserts the two agree — a cross-check
    // that would be lost if this simply trusted one of them.
    coordinator
        .commit(cx, &capsule.bytes, |seq, oid| {
            marker_for_capsule(seq, oid, capsule, head_updates)
        })
        .await
}

/// What a replay produces: the materialized graph AND the delta index that
/// tracks it.
///
/// Returned together because plan:397 requires apply to insert the batch and
/// advance the frontier in the SAME transition as the commit. A replay that
/// produced one without the other would model a state the plan forbids.
#[derive(Debug, Clone, PartialEq)]
pub struct Replayed {
    pub database: ReferenceDatabase,
    pub index: LocalDeltaBatchIndex,
    /// Secret-free crypto/identity outcomes observed while replay read the
    /// durable capsules. This makes replay forensics retain the verification
    /// evidence rather than satisfying the mandatory sink with a throwaway.
    pub crypto_verification_events: Vec<CryptoVerificationEvent>,
}

/// Materialize the durable commit stream into graph state, discarding the
/// index. Kept for callers that only want the graph.
pub async fn materialize<V: asupersync::fs::Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
) -> Result<ReferenceDatabase, ReplayError> {
    replay(cx, coordinator)
        .await
        .map(|replayed| replayed.database)
}

/// **Replay the durable commit stream into graph state and the delta window.**
///
/// Walks the recovered marker chain in commit order and, for each marker,
/// reads its capsule, proves the bytes are the ones the marker committed to,
/// decodes the template, and applies it. Every step fails closed and names the
/// sequence, because "the database will not open" is only actionable if it also
/// says which commit is the problem.
///
/// Only markers reach this loop, so an orphan capsule — bytes on disk that no
/// marker names — contributes nothing without needing to be excluded: it was
/// never in the stream to begin with. That is the marker-is-the-commit rule
/// paying off at the semantic layer rather than being restated there.
///
/// Takes `&CommitCx` because attesting a marker as committed is a
/// capability-gated act: `CommittedMarker::attest` demands commit authority
/// precisely so a bare marker identity cannot be promoted outside the commit
/// lane. Recovery is inside that lane — every marker it walks came out of the
/// recovered chain, which holds only entries that reached D2 — so it can make
/// the attestation honestly rather than needing a back door.
pub async fn replay<V: asupersync::fs::Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
) -> Result<Replayed, ReplayError> {
    replay_through(cx, coordinator, fgdb_types::CommitSeq(u64::MAX)).await
}

/// [`replay`], stopping after the last marker at or below `through` — the
/// oracle-at-an-epoch a system-time differential compares against
/// (fgdb-90jx). The chain is walked in commit order, so everything applied
/// is exactly the stream's prefix through that sequence.
pub async fn replay_through<V: asupersync::fs::Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
    through: fgdb_types::CommitSeq,
) -> Result<Replayed, ReplayError> {
    let mut database = ReferenceDatabase::with_database_id(reference_database_id(cx, coordinator)?);
    let mut index = LocalDeltaBatchIndex::new();
    let mut crypto_verification_events = Vec::new();
    for entry in coordinator.chain().entries() {
        let commit_seq = entry.marker.commit_seq;
        if commit_seq > through.0 {
            break;
        }
        let EffectSource::Local {
            capsule_ref,
            logical_delta_template_digest,
        } = &entry.marker.effect_source;

        if !coordinator.capsule_exists(cx, *capsule_ref).await {
            return Err(ReplayError::MissingCapsule {
                commit_seq,
                capsule_oid: *capsule_ref,
            });
        }
        let bytes = coordinator
            .read_capsule(cx, *capsule_ref, &mut crypto_verification_events)
            .await?;

        let recomputed = template_digest(&bytes);
        // ubs:ignore — canonical logical-effect integrity is public, not authentication material.
        if recomputed != *logical_delta_template_digest {
            return Err(ReplayError::TemplateDigestMismatch {
                commit_seq,
                declared: *logical_delta_template_digest,
                recomputed,
            });
        }

        let template = LogicalDeltaTemplate::decode_canonical(&bytes)
            .map_err(|error| ReplayError::Decode { commit_seq, error })?;
        database
            .apply_template(
                &template,
                fgdb_types::CommitSeq(commit_seq),
                fgdb_types::LogicalCommandSeq(entry.marker.logical_command_seq),
            )
            .map_err(|error| ReplayError::Apply { commit_seq, error })?;

        // The batch enters the index in the SAME step that applied it — the
        // structural point of plan:397. `insert` advances the frontier as part
        // of the same call, so there is no instant at which this commit's
        // effects are in the graph and its batch is not in the window.
        let batch = LogicalDeltaBatch::order(
            &template,
            logical_delta_template_digest.0,
            // The marker is committed by construction here: it came out of the
            // recovered chain, which only holds entries that reached D2.
            CommittedMarker::attest(
                fgdb_types::MarkerRef {
                    marker_oid: entry.marker_oid,
                    commit_seq: fgdb_types::CommitSeq(commit_seq),
                },
                cx,
            ),
        );
        index
            .insert(batch)
            .map_err(|error| ReplayError::Index { commit_seq, error })?;
    }
    Ok(Replayed {
        database,
        index,
        crypto_verification_events,
    })
}

/// Bind every replay of one durable coordinator directory to the same reference
/// database authority.
///
/// The current Chronicle slice has not yet threaded Appendix A's persisted
/// `database_id` through `CommitCoordinator`, so the verification layer derives a
/// deterministic stand-in from the canonical directory plus its complete capsule
/// identity domain. The directory is included because two independent databases
/// may deliberately use the same key/namespace profile. Once the root stack owns
/// `DatabaseId`, this derivation is replaced by that field without changing
/// `ReferenceDatabase`'s contract.
fn reference_database_id<V: asupersync::fs::Vfs>(
    cx: &CommitCx,
    coordinator: &CommitCoordinator<V>,
) -> Result<DatabaseId, ReplayError> {
    let canonical_dir = cx
        .with_restriction(|| std::fs::canonicalize(coordinator.database_dir()))
        .map_err(CommitError::from)?;
    let keys = coordinator.keys();
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(REFERENCE_DATABASE_ID_DOMAIN);
    hasher.update(&keys.k_oid);
    hasher.update(&keys.namespace.0);
    hasher.update(&keys.object_kind.to_le_bytes());
    hasher.update(canonical_dir.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let mut database_id = [0u8; 16];
    database_id.copy_from_slice(&digest.0[..16]);
    Ok(DatabaseId(database_id))
}
