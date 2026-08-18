//! The registered validation interface for commit-protocol steps 2–3.
//!
//! Plan §5.2 validates a transaction between admission and capsule
//! publication: the WriteCoordinator orders a group, then FCW/SSI validation,
//! the merge ladder, constraint checking, and the authoritative durable-quota
//! ledger decide whether the draft's effects may become durable (steps 2–3).
//! None of those validators lives in this crate — they land in the w4
//! workstream (coordinator, SSI witnesses, merge ladder, constraints) — but
//! the seam they implement against must exist here, on the commit path
//! itself; a validation step bolted on beside the two-fsync protocol instead
//! of inside it would validate a different protocol.
//!
//! [`CommitValidator`] is that seam. The [`CommitCoordinator`] consults its
//! installed validator once per commit, after the draft is structurally
//! complete (capsule sealed, marker chained) and **before the first durable
//! byte**: a rejection aborts the commit with no durable trace and does not
//! consume the sequence.
//!
//! DETERMINISM BY CONSTRUCTION. `validate` receives no `Cx`. This is the
//! doctrine's sharpest instance applied at the seam: the plan gives the merge
//! ladder's intent-replay evaluator a context with no clock, entropy,
//! network, or filesystem capability — here, no ambient capability at all. A
//! validator is a function of the draft and its own explicitly constructed
//! state, so there is nothing to swap under the lab because there is nothing
//! ambient to begin with. Validators that need the committed basis capture it
//! at construction, where the capture itself is visible and testable.
//!
//! [`CommitCoordinator`]: crate::commit::CommitCoordinator

use crate::marker::CommitMarker;
use fgdb_types::CommitSeq;
use fgdb_types::ids::ObjectId;

/// Everything the coordinator knows about a commit at the instant validation
/// runs — borrowed, not owned. The draft is not yet a commit, and a validator
/// that could retain pieces of one draft to approve a later one would be a
/// mechanism for blessing an old capsule, which §5.2 step 2 forbids: a rebase
/// produces new effects, it never blesses an old capsule.
pub struct CommitDraft<'a> {
    /// The sequence this commit will occupy if it survives validation. Not
    /// consumed by a rejection: the chain only moves on a durable marker.
    pub commit_seq: CommitSeq,
    /// The identity derived from the sealed capsule bytes. Derived, never
    /// caller-supplied, so the validator judges the object that would be
    /// published and no other.
    pub capsule_oid: ObjectId,
    /// The canonical effect-capsule plaintext this commit would make durable.
    pub capsule_plaintext: &'a [u8],
    /// The chained marker that would name the capsule.
    pub marker: &'a CommitMarker,
}

impl core::fmt::Debug for CommitDraft<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CommitDraft")
            .field("commit_seq", &self.commit_seq)
            .field("capsule_oid", &self.capsule_oid)
            .field("capsule_plaintext_len", &self.capsule_plaintext.len())
            .field("capsule_plaintext", &"[REDACTED]")
            .field("marker", &"[REDACTED]")
            .finish()
    }
}

/// Why validation refused a draft.
///
/// A rejection is a verdict, not an error condition: the coordinator stays
/// healthy, the sequence stays free, and the caller may repair and resubmit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationRejection {
    /// The stable registry ID (`FG-INV-…` / `FG-LAW-…`) of the rule this
    /// rejection enforces. A rejection that cannot name its law is not a
    /// validation outcome, it is an opinion.
    pub law: &'static str,
    /// Human-facing diagnostic detail. Never parsed, never durable.
    pub detail: String,
}

impl core::fmt::Display for ValidationRejection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "draft rejected under {}: {}", self.law, self.detail)
    }
}

/// The commit-protocol steps 2–3 validation seam.
///
/// Implementations decide whether a structurally complete draft may become
/// durable. `&mut self` is deliberate: step 2 evaluates each accepted
/// transaction against the committed basis *plus earlier accepted effects in
/// that group*, so a real validator is stateful across a group by design.
///
/// The contract the coordinator holds every implementation to:
/// - consulted exactly once per commit attempt, before any durable write;
/// - `Ok(())` licenses publication of exactly this draft;
/// - `Err` aborts the attempt with no durable trace, no consumed sequence,
///   and no coordinator poisoning.
///
/// `Debug` is a supertrait because the installed validator is a field of
/// [`CommitCoordinator`], which derives `Debug`: a validator whose identity
/// cannot appear in the coordinator's debug output would be invisible state.
pub trait CommitValidator: core::fmt::Debug {
    /// Judge one draft. Must be a deterministic function of the draft and
    /// state this validator was explicitly constructed with.
    fn validate(&mut self, draft: &CommitDraft<'_>) -> Result<(), ValidationRejection>;
}

/// The deterministic pass-through fixture validator: accepts every draft.
///
/// This is the w2 delivery boundary made executable. The commit path consults
/// a real validator from its first commit; the w4 validators replace this
/// *instance*, never the seam. Pass-through is honest for the substrate that
/// exists today — a single-writer stream with no concurrent group has no
/// FCW/SSI conflict to detect and no merge to attempt — and it is the
/// identity element the w4 stack composes over, not a stub of it.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassThroughValidator;

impl CommitValidator for PassThroughValidator {
    fn validate(&mut self, _draft: &CommitDraft<'_>) -> Result<(), ValidationRejection> {
        Ok(())
    }
}
