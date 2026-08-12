//! The registered validation-interface seam of the §5.2 commit protocol.
//!
//! Steps 2–3 of the six-step protocol — FCW/SSI validation, the merge
//! ladder, constraint checking, and the durable-quota ledger — are consumed
//! by the commit path through ONE trait, [`ValidationInterface`]. This module
//! ships the trait and a deterministic pass-through fixture implementation;
//! the real validators land in the w4 workstream and implement against this
//! seam rather than against the coordinator's internals.
//!
//! Two laws are load-bearing here, and both are enforced by construction
//! rather than by review:
//!
//! * **Validation precedes durability.** A proposal the validator refuses
//!   writes NOTHING — no capsule bytes, no marker, no sequence consumed.
//!   [`validate_and_commit`] runs the validator strictly before the
//!   coordinator seals anything, so a rejection cannot leave residue that
//!   recovery would have to explain.
//! * **The durable unit is the validator's output.** What gets sealed is the
//!   canonical effect payload the validator RETURNED, never the caller's
//!   proposal — the §5.2 rule that the durable unit is the post-rebase,
//!   post-constraint canonical effect capsule, not the user's unresolved
//!   intent log. The pass-through fixture makes those bytes equal; that
//!   equality is a property of the fixture, not of the seam.

use crate::commit::{CommitCoordinator, CommitError};
use crate::marker::{CommitMarker, MarkerChain};
use asupersync::fs::Vfs;
use fgdb_types::MarkerRef;
use fgdb_types::context::CommitCx;
use fgdb_types::ids::ObjectId;

/// The committed basis a proposal is validated against.
///
/// For now the basis is the marker chain itself: every fact the landed
/// coordinator can speak for (heads, next sequence, chain value) is derivable
/// from it. The w4 validators additionally need snapshot state, witness
/// stores, and the quota ledger; those arrive as new fields when their types
/// exist. `#[non_exhaustive]` keeps construction inside this crate so growing
/// the basis is not a breaking change for implementors.
#[derive(Debug)]
#[non_exhaustive]
pub struct ValidationBasis<'a> {
    /// The committed marker chain — the durable history this proposal would
    /// extend. Validators read it; only the coordinator advances it.
    pub chain: &'a MarkerChain,
}

/// One transaction proposal presented for validation.
///
/// The landed protocol carries the proposal as the plaintext the capsule
/// would seal. The full `TxnProposal` record (permit, predicate witnesses,
/// reservations, txn token) is w2's nine-state scope; its fields join this
/// struct when their types exist, which is why construction stays
/// `#[non_exhaustive]`-guarded and callers go through [`CommitProposal::new`].
#[derive(Debug)]
#[non_exhaustive]
pub struct CommitProposal<'a> {
    /// The proposed intent payload, unresolved and unvalidated.
    pub intent_plaintext: &'a [u8],
}

impl<'a> CommitProposal<'a> {
    #[must_use]
    pub fn new(intent_plaintext: &'a [u8]) -> Self {
        Self { intent_plaintext }
    }
}

/// A proposal the validator accepted, carrying the canonical effect payload
/// the capsule will seal.
///
/// This type is the seam's answer to "what is durable": not the proposal,
/// but what validation MADE of it. A rebase produces new effects; a pass-through
/// fixture produces the input unchanged; either way the coordinator seals
/// exactly these bytes.
#[derive(Debug)]
#[non_exhaustive]
pub struct ValidatedCommit {
    /// The post-validation canonical effect payload.
    pub canonical_effect_plaintext: Vec<u8>,
}

impl ValidatedCommit {
    #[must_use]
    pub fn new(canonical_effect_plaintext: Vec<u8>) -> Self {
        Self {
            canonical_effect_plaintext,
        }
    }
}

/// The registered validation-interface trait (§5.2 steps 2–3).
///
/// One implementor speaks for admission of a proposal into the durable
/// stream: FCW/SSI validation, merge-ladder rebase, constraint checking, and
/// the durable-quota ledger all sit behind this signature. The rejection
/// vocabulary is an associated type because the real rejection taxonomy
/// (permit scope, epoch drift, dangerous structures, quota) belongs to the
/// w4 crates that own those types — legislating it here, before those types
/// exist, would freeze a guess.
///
/// Implementations must be deterministic over `(basis, proposal)`: the merge
/// ladder's evaluator runs under a context with no clock, entropy, network,
/// or filesystem capability (FG-INV-17), and this seam is where that
/// guarantee is consumed. The `cx` parameter carries the commit-path
/// capability context so a validator that DOES need a legal effect (an
/// obligation reservation, a storage read) performs it under restriction,
/// visible to the lab runtime.
pub trait ValidationInterface {
    /// How this validator says no.
    type Rejection: core::error::Error + Send + 'static;

    /// Validate one proposal against the committed basis.
    ///
    /// `Ok` carries the canonical effect payload to seal; `Err` means the
    /// proposal must leave no durable trace. Implementations never write.
    fn validate(
        &self,
        cx: &CommitCx,
        basis: &ValidationBasis<'_>,
        proposal: &CommitProposal<'_>,
    ) -> Result<ValidatedCommit, Self::Rejection>;
}

/// The deterministic pass-through fixture validator.
///
/// It admits every proposal and its canonical effect payload is the proposal
/// bytes unchanged — the identity function on the payload, with no state, no
/// clock, and no way to say no: the rejection type is [`core::convert::Infallible`],
/// so "the fixture never rejects" is a fact the compiler checks rather than a
/// comment. This is a FIXTURE in the doctrine-7 sense: a subset of the final
/// abstraction (real validators rebase, refuse, and charge quota), never a
/// substitute for it — nothing outside tests and pre-w4 wiring should reach
/// for it once the real ladder lands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassThroughValidator;

impl ValidationInterface for PassThroughValidator {
    type Rejection = core::convert::Infallible;

    fn validate(
        &self,
        _cx: &CommitCx,
        _basis: &ValidationBasis<'_>,
        proposal: &CommitProposal<'_>,
    ) -> Result<ValidatedCommit, Self::Rejection> {
        Ok(ValidatedCommit::new(proposal.intent_plaintext.to_vec()))
    }
}

/// Why a validated commit did not land.
///
/// The two arms are different WORLDS, not different severities: a rejection
/// means nothing durable changed and the caller may revise and re-propose; a
/// commit error means the durability machinery itself failed and the
/// coordinator's own poisoning/recovery rules govern what happens next.
#[derive(Debug)]
pub enum ValidatedCommitError<R> {
    /// The validator refused the proposal before anything was written.
    Rejected(R),
    /// Validation passed, but the two-fsync protocol failed.
    Commit(CommitError),
}

impl<R: core::fmt::Display> core::fmt::Display for ValidatedCommitError<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Rejected(rejection) => write!(f, "proposal rejected: {rejection}"),
            Self::Commit(error) => write!(f, "validated commit failed: {error}"),
        }
    }
}

impl<R: core::error::Error + 'static> core::error::Error for ValidatedCommitError<R> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Rejected(rejection) => Some(rejection),
            Self::Commit(error) => Some(error),
        }
    }
}

/// Validate a proposal and, if admitted, commit its canonical effects.
///
/// This is the consumption seam: the commit path calls the validator HERE,
/// strictly before the coordinator seals or writes anything, and what it
/// commits is the validator's output. The function composes over
/// [`CommitCoordinator`]'s public API on purpose — the coordinator stays a
/// pure durability actor with no knowledge of validation, and the w4 ladder
/// replaces the validator without touching the two-fsync protocol.
///
/// `marker_for` receives the allocated sequence and the CANONICAL payload's
/// derived capsule identity — the identity of what validation admitted, which
/// under any non-pass-through validator differs from the proposal's.
pub async fn validate_and_commit<V: Vfs, I: ValidationInterface>(
    coordinator: &mut CommitCoordinator<V>,
    cx: &CommitCx,
    validator: &I,
    proposal: CommitProposal<'_>,
    marker_for: impl FnOnce(u64, ObjectId) -> CommitMarker,
) -> Result<MarkerRef, ValidatedCommitError<I::Rejection>> {
    let basis = ValidationBasis {
        chain: coordinator.chain(),
    };
    let validated = validator
        .validate(cx, &basis, &proposal)
        .map_err(ValidatedCommitError::Rejected)?;
    coordinator
        .commit(cx, &validated.canonical_effect_plaintext, marker_for)
        .await
        .map_err(ValidatedCommitError::Commit)
}
