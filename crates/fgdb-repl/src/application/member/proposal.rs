//! Payload-authorized submission through the owning application/replica pair.
//!
//! The backend that owns application and ordinary Raft publication also owns
//! proposal authority. No external publisher can be substituted for that store.
//! This closes the composition gap without making the assessment itself a
//! certificate or treating a locally appended command as a completed write.

use super::{AppliedReplica, AppliedReplicaOutput, Application, ApplicationStateError};
use crate::availability::proposal::{
    PayloadProposalAuthority, ProposalError, ProposalPosition, propose,
};
use crate::availability::{AvailabilityInput, AvailabilityLimits};
use crate::driver::RaftPublisher;

pub mod batch;

#[derive(Debug)]
pub enum MemberProposalError<A, I> {
    State(ApplicationStateError),
    Proposal(ProposalError<A, I>),
}

impl<A: core::fmt::Debug, I: core::fmt::Debug> core::fmt::Display
    for MemberProposalError<A, I>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis applied proposal: {self:?}")
    }
}

impl<A: core::fmt::Debug, I: core::fmt::Debug> core::error::Error
    for MemberProposalError<A, I>
{
}

/// A local durable append, with the owning member's complete output history.
/// The position is internal. Neither it nor this value is a public write result,
/// an availability certificate, or permission to acknowledge client success.
#[derive(Debug)]
pub struct MemberProposalOutput<C> {
    pub position: ProposalPosition,
    pub member: AppliedReplicaOutput<C>,
}

impl<C, A> AppliedReplica<C, A>
where
    C: Clone + Eq,
    A: Application<C> + RaftPublisher<C> + PayloadProposalAuthority<C>,
{
    /// Submit using the SAME owned backend's current payload authority.
    ///
    /// All canonical closure, receipt, prepared-owner, key, freshness and writer
    /// fence checks remain the PayloadProposalAuthority contract. Its scoped
    /// permit spans the actual publication. There is no separately supplied
    /// authority/store and no mutable escape hatch into the replica.
    ///
    /// This does not wait for quorum or apply: dispatch the returned consensus
    /// messages promptly and continue ordinary step/apply work, including audit
    /// controls that can release hidden commands. Every output is absorbed by
    /// the same member that owns outstanding ReadIndex requests.
    ///
    /// Refusal before append leaves the member available. Error, panic or
    /// cancellation after publication starts uses the ordinary recovery fence;
    /// it is NOT an abort result and never releases durable prepared ownership.
    /// The low-level step API remains a privileged composition interface.
    pub async fn propose_available<I, F>(
        &mut self,
        command: C,
        input: &AvailabilityInput,
        limits: AvailabilityLimits,
        checkpoint: &mut F,
    ) -> Result<MemberProposalOutput<C>, MemberProposalError<<A as PayloadProposalAuthority<C>>::Error, I>>
    where
        F: FnMut() -> Result<(), I>,
    {
        self.available().map_err(MemberProposalError::State)?;
        // Do not spend payload-assessment work or acquire a publication permit
        // for a proposal the frozen handoff endpoint cannot admit. The kernel
        // also enforces this for privileged direct scalar/batch callers.
        if self.leadership_transfer().map_err(MemberProposalError::State)?.is_some() {
            return Err(MemberProposalError::Proposal(ProposalError::Raft(
                fgdb_order::Error::LeadershipTransferInProgress,
            )));
        }
        let output = propose(
            &mut self.replica,
            command,
            input,
            limits,
            &mut self.application.application,
            checkpoint,
        )
        .await
        .map_err(MemberProposalError::Proposal)?;
        let member = self.absorb(output.replica).map_err(MemberProposalError::State)?;
        Ok(MemberProposalOutput { position: output.position, member })
    }
}