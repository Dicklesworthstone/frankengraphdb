//! Payload-gated proposal submission through the existing Raft publication path.
//!
//! Calculation, authority and consensus are three separate gates. The first is
//! implemented by `assess_systematic`; the second is a runtime capability that
//! authenticates the complete canonical command/certificate/receipt closure; the
//! third remains Replica's ordinary durable append and quorum machinery. No new
//! command, wire format, receipt, certificate or durability barrier is introduced.
//!
//! The capability returned by the authority IS the Raft publisher. Its custody
//! spans the actual append publication, rather than ending at a Boolean check
//! before an await. It must retain the writer fence and payload pins, and check
//! proposal freshness again immediately before the irreversible publication.

use std::future::Future;

use fgdb_order::{Domain, Error as RaftError, Event, MemberId, Role};

use crate::driver::RaftPublisher;
use crate::replica::{Replica, ReplicaError, ReplicaOutput};

use super::{
    AvailabilityError, AvailabilityInput, AvailabilityLimits, StorageSets, SystematicAssessment,
    assess_systematic,
};

/// Internal order-attempt coordinates, never a public transaction outcome.
/// They name this exact leader's next entry under the current configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProposalPosition {
    pub member: MemberId,
    pub domain: Domain,
    pub configuration: [u8; 32],
    pub term: u64,
    pub index: u64,
}

/// The runtime's canonical payload-authority verifier and fenced publisher.
///
/// `acquire` MUST authenticate the complete already-built command and its strong
/// availability-certificate edge, not only a caller-supplied digest. It must
/// byte-match every assessment input against that authoritative closure: exact
/// inventory completeness, required predicate/rule, storage sets and correlated
/// failure map, each receipt identity and full signed fence-bound header, stable
/// prepared-owner/base closure, placement/source coverage, and usable key wraps.
/// Reject an unsupported predicate rather than weakening it to systematic cover.
/// A valid signature on unreachable staging does not prove durable ownership.
///
/// Each call additionally checks the current leader/configuration/writer fence
/// and current-process `TimeValidationEvidence::Usable` for `FormNewProposal`.
/// Historical receipts remain ownership promises after freshness expires, but
/// cannot authorize this new proposal. The returned permit retains the closure
/// and fence, and rechecks freshness/authority at the irreversible publication.
/// It implements RaftPublisher over the SAME canonical store and leased inode
/// as this replica, preserving newer prepared/protocol roots. An unrelated
/// publisher or a one-time pre-await freshness check violates this contract.
///
/// Acquisition is read/verify plus ephemeral reservation only: it MUST NOT
/// publish a root, mint a receipt/certificate, alter Raft state, or release a
/// durable prepared promise. Preparation and certificate publication precede
/// this API. This keeps cancellation before the append a non-mutating refusal.
/// Dropping a permit releases local reservations, NEVER durable ownership.
/// Implementations retain their ReplCx/CommitCx and owned region/fence throughout
/// borrowed I/O; they must not detach tasks or replace them with ambient access.
///
/// This trait is an integration boundary, not an implementation of those codecs,
/// signatures, time authority or writer-fence checks. A permissive implementation
/// is only a test model and cannot enable production clustering.
pub trait PayloadProposalAuthority<C> {
    type Error;
    type Permit<'a>: RaftPublisher<C, Error = Self::Error>
    where
        Self: 'a;

    fn acquire<'a>(
        &'a mut self,
        command: &C,
        position: ProposalPosition,
        assessment: &SystematicAssessment<'_>,
    ) -> impl Future<Output = Result<Self::Permit<'a>, Self::Error>>;
}

#[derive(Debug)]
pub enum ProposalError<A, I> {
    Raft(RaftError),
    WrongBasis,
    WrongStorageForm,
    Availability(AvailabilityError<I>),
    Authority(A),
    Interrupted(I),
    Replica(ReplicaError<A>),
}

impl<A: core::fmt::Debug, I: core::fmt::Debug> core::fmt::Display for ProposalError<A, I> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis payload-gated proposal: {self:?}")
    }
}
impl<A: core::fmt::Debug, I: core::fmt::Debug> core::error::Error for ProposalError<A, I> {}

/// Local durable append plus native replication work, NOT a commit receipt.
/// Dispatch messages independently to peers and continue ordinary Raft. Client
/// success still requires quorum commitment, durable apply, audit visibility and
/// the canonical outcome/delivery protocol. Leader loss remains Unknown, never
/// an inferred abort; cancellation does not release receipted payload ownership.
#[derive(Debug)]
pub struct ProposalOutput<C> {
    pub position: ProposalPosition,
    pub replica: ReplicaOutput<C>,
}

/// Calculate recoverability, acquire current authority, and append exactly once.
///
/// The exclusive replica borrow prevents local term/configuration/log changes
/// during authority acquisition. Expensive calculation precedes freshness use;
/// a final cancellation checkpoint runs after acquisition but before Raft. The
/// command is moved, not cloned, into the existing proposal path. Every call
/// reacquires authority; a prior assessment or permit is not a reusable token.
///
/// An acquisition refusal/cancellation never starts a Raft transition. Once the
/// ordinary publication starts, its existing guard fences any uncertain error,
/// panic or dropped future and no output escapes. Reopen the authenticated root
/// before further consensus work. This gate covers NEW local proposals only:
/// followers/recovery validate already-ordered payload ownership under their
/// separate contracts, not a new-proposal freshness rule. The low-level
/// Replica::step interface remains a trusted kernel/composition boundary.
pub async fn propose<C, A, I, F>(
    replica: &mut Replica<C>,
    command: C,
    input: &AvailabilityInput,
    limits: AvailabilityLimits,
    authority: &mut A,
    checkpoint: &mut F,
) -> Result<ProposalOutput<C>, ProposalError<A::Error, I>>
where
    C: Clone + Eq,
    A: PayloadProposalAuthority<C>,
    F: FnMut() -> Result<(), I>,
{
    if replica.role().map_err(ProposalError::Raft)? != Role::Leader {
        return Err(ProposalError::Raft(RaftError::NotLeader));
    }
    let state = replica.durable_state().map_err(ProposalError::Raft)?;
    let configuration = state.configuration();
    if input.policy.basis.domain != configuration.domain()
        || input.policy.basis.configuration != configuration.identity()
    {
        return Err(ProposalError::WrongBasis);
    }
    if matches!(&input.policy.storage_sets, StorageSets::Joint { .. })
        != configuration.joint_voters().is_some()
    {
        return Err(ProposalError::WrongStorageForm);
    }
    let base = state.snapshot().map_or(0, |snapshot| snapshot.index());
    let suffix = u64::try_from(state.entries().len())
        .map_err(|_| ProposalError::Raft(RaftError::CounterExhausted))?;
    let index = base
        .checked_add(suffix)
        .and_then(|last| last.checked_add(1))
        .filter(|index| *index < u64::MAX)
        .ok_or(ProposalError::Raft(RaftError::CounterExhausted))?;
    let position = ProposalPosition {
        member: replica.id(),
        domain: configuration.domain(),
        configuration: configuration.identity(),
        term: state.term(),
        index,
    };
    let assessment =
        assess_systematic(input, limits, checkpoint).map_err(ProposalError::Availability)?;
    let mut permit = authority
        .acquire(&command, position, &assessment)
        .await
        .map_err(ProposalError::Authority)?;
    checkpoint().map_err(ProposalError::Interrupted)?;
    let output = replica
        .step(&mut permit, Event::Propose(command))
        .await
        .map_err(ProposalError::Replica)?;
    Ok(ProposalOutput {
        position,
        replica: output,
    })
}

#[cfg(test)]
mod tests;
