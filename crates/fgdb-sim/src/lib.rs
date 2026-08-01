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

use fgdb_chronicle::commit::{CommitCoordinator, CommitError};
use fgdb_chronicle::identity::IdentifiedObject;
use fgdb_chronicle::marker::{CommitMarker, EffectSource};
use fgdb_crypto::Digest;
use fgdb_delta_types::{
    CanonicalError, CommittedMarker, IndexError, LocalDeltaBatchIndex, LogicalDeltaBatch,
    LogicalDeltaTemplate,
};
use fgdb_reference::{ApplyError, ReferenceDatabase};
use fgdb_types::MarkerRef;
use fgdb_types::context::CommitCx;
use fgdb_types::ids::{DatabaseId, DatabaseSecurityNamespaceId, ObjectId};

/// Object kind for a committed effect capsule. `0x0274` is the Appendix A
/// reservation for `CommittedEffectCapsule`; it is spelled here as a constant
/// rather than a typed kind because that kind is `reserved`, not `active`, so
/// naming it in the type system would not compile (see the subset note on
/// `CoordinateEntry`).
pub const CAPSULE_OBJECT_KIND: u16 = 0x0274;

/// Domain for the verification-layer stand-in for `RootSlot.database_id`.
const REFERENCE_DATABASE_ID_DOMAIN: &[u8] = b"fgdb.reference.replay-database-id.v1";

/// A template prepared for commit: its canonical bytes, the identity those
/// bytes have, and the digest the marker will declare.
///
/// Built in one place so the three can never disagree. A caller that computed
/// the oid from one byte string and the digest from another would produce a
/// commit that passes every check at write time and fails to recover.
#[derive(Debug, Clone)]
pub struct PreparedCapsule {
    pub bytes: Vec<u8>,
    pub object_id: ObjectId,
    pub template_digest: Digest,
}

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

/// The digest a marker declares for its template — a plain hash of the exact
/// canonical bytes the capsule holds.
pub fn template_digest(bytes: &[u8]) -> Digest {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(TEMPLATE_DIGEST_DOMAIN);
    hasher.update(bytes);
    hasher.finalize()
}

/// Domain separator, so a template digest can never collide with any other
/// digest in the system by hashing the same bytes under a different meaning.
pub const TEMPLATE_DIGEST_DOMAIN: &[u8] = b"fgdb:logical-delta-template:v1";

/// Prepare a template for commit: encode it, identify it, digest it.
pub fn prepare_capsule(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    template: &LogicalDeltaTemplate,
) -> Result<PreparedCapsule, CanonicalError> {
    let bytes = template.canonical_bytes()?;
    // The §5.1 keyed identity over the same bytes the capsule will hold. The
    // header is empty because the canonical bytes ARE the whole object — the
    // transcript concatenates header and payload, so passing the bytes as the
    // payload reproduces exactly the intended stream.
    let identified = IdentifiedObject::new(k_oid, namespace, CAPSULE_OBJECT_KIND, &[], &bytes);
    Ok(PreparedCapsule {
        object_id: identified.object_id(),
        template_digest: template_digest(&bytes),
        bytes,
    })
}

/// Build the marker for a prepared capsule at an allocated sequence.
///
/// The marker's `capsule_ref` and `logical_delta_template_digest` both come
/// from the same `PreparedCapsule`, so the write-time cross-check and the
/// recovery-time cross-check are asking about the same object by construction.
pub fn marker_for_capsule(
    commit_seq: u64,
    capsule_oid: ObjectId,
    capsule: &PreparedCapsule,
    head_updates: Vec<fgdb_chronicle::marker::HeadUpdate>,
) -> CommitMarker {
    CommitMarker {
        logical_command_seq: commit_seq,
        commit_seq,
        effect_source: EffectSource::Local {
            capsule_ref: capsule_oid,
            logical_delta_template_digest: capsule.template_digest,
        },
        prev_global: None,
        head_updates,
        merge_record_oid: None,
        coordinate_schema_transition_digest: Digest([0u8; 32]),
        topology_epoch: 1,
        policy_epoch: 1,
        revocation_index: 0,
        txn_token: [0u8; 16],
        commit_hlc: commit_seq,
        final_effect_digest: capsule.template_digest,
        authorization_decision_digest: Digest([0u8; 32]),
        resource_effect_digest: Digest([0u8; 32]),
        payload_availability_certificate_oid: None,
        flags: 0,
    }
}

/// Commit a prepared capsule.
pub fn commit_capsule(
    coordinator: &mut CommitCoordinator,
    cx: &CommitCx,
    capsule: &PreparedCapsule,
    head_updates: Vec<fgdb_chronicle::marker::HeadUpdate>,
) -> Result<MarkerRef, CommitError> {
    // The coordinator DERIVES the identity from the bytes it seals, so the
    // marker names whatever the store actually wrote rather than whatever the
    // caller believed. `PreparedCapsule::object_id` is the same value computed
    // independently, and the e2e suite asserts the two agree — a cross-check
    // that would be lost if this simply trusted one of them.
    coordinator.commit(cx, &capsule.bytes, |seq, oid| {
        marker_for_capsule(seq, oid, capsule, head_updates)
    })
}

/// **Materialize the durable commit stream into graph state.**
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
}

/// Materialize the durable commit stream into graph state, discarding the
/// index. Kept for callers that only want the graph.
pub fn materialize(
    cx: &CommitCx,
    coordinator: &CommitCoordinator,
) -> Result<ReferenceDatabase, ReplayError> {
    replay(cx, coordinator).map(|replayed| replayed.database)
}

/// Replay the durable stream into graph state and the delta window.
///
/// Takes `&CommitCx` because attesting a marker as committed is a
/// capability-gated act: `CommittedMarker::attest` demands commit authority
/// precisely so a bare marker identity cannot be promoted outside the commit
/// lane. Recovery is inside that lane — every marker it walks came out of the
/// recovered chain, which holds only entries that reached D2 — so it can make
/// the attestation honestly rather than needing a back door.
pub fn replay(cx: &CommitCx, coordinator: &CommitCoordinator) -> Result<Replayed, ReplayError> {
    let mut database = ReferenceDatabase::with_database_id(reference_database_id(cx, coordinator)?);
    let mut index = LocalDeltaBatchIndex::new();
    for entry in coordinator.chain().entries() {
        let commit_seq = entry.marker.commit_seq;
        let EffectSource::Local {
            capsule_ref,
            logical_delta_template_digest,
        } = &entry.marker.effect_source;

        if !coordinator.capsule_exists(cx, *capsule_ref) {
            return Err(ReplayError::MissingCapsule {
                commit_seq,
                capsule_oid: *capsule_ref,
            });
        }
        let bytes = coordinator.read_capsule(cx, *capsule_ref)?;

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
    Ok(Replayed { database, index })
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
fn reference_database_id(
    cx: &CommitCx,
    coordinator: &CommitCoordinator,
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
