//! Voluntary leadership handoff inside one fixed authenticated configuration.
//!
//! A leader freezes new proposals, catches up one voter through the ordinary
//! append/snapshot pipeline, then asks it to run a NORMAL next-term election.
//! Neither the request nor a target's match position transfers voting authority.
//! No membership, payload-ownership, audit-visibility or retirement rule changes.

use std::sync::Arc;

use super::{Error, Event, MemberId, Message, Output, PersistenceId, Raft, Role};

/// Local cancellation/deadline identity. It is not a wire token or a receipt.
/// An old deadline cannot cancel a replacement attempt or a recovered member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeadershipTransferId(PersistenceId);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeadershipTransferPhase {
    CatchingUp,
    /// The target was asked to campaign. Election success is NOT established.
    ElectionRequested,
}

/// Diagnostic projection of one volatile attempt, never durable authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeadershipTransfer {
    id: LeadershipTransferId,
    target: MemberId,
    term: u64,
    last_index: u64,
    last_term: u64,
    phase: LeadershipTransferPhase,
}

impl LeadershipTransfer {
    pub fn id(&self) -> LeadershipTransferId {
        self.id.clone()
    }

    pub fn target(&self) -> MemberId {
        self.target
    }

    pub fn term(&self) -> u64 {
        self.term
    }

    /// Exact log endpoint frozen at admission, not a commit or application cut.
    pub fn last_index(&self) -> u64 {
        self.last_index
    }

    pub fn last_term(&self) -> u64 {
        self.last_term
    }

    pub fn phase(&self) -> LeadershipTransferPhase {
        self.phase
    }
}

pub(super) fn validate_event<C: Clone + Eq>(raft: &Raft<C>, event: &Event<C>) -> Result<(), Error> {
    match event {
        Event::TransferLeadership(target) => {
            if raft.role != Role::Leader {
                return Err(Error::NotLeader);
            }
            if *target == raft.id || !raft.state.configuration.voters.contains(target) {
                return Err(Error::InvalidLeadershipTarget);
            }
            if raft.state.term == u64::MAX {
                return Err(Error::CounterExhausted);
            }
            if raft
                .handoff
                .as_ref()
                .is_some_and(|pending| pending.target != *target)
            {
                return Err(Error::LeadershipTransferInProgress);
            }
        }
        Event::AbortLeadershipTransfer(id) => {
            if !raft
                .handoff
                .as_ref()
                .is_some_and(|pending| &pending.id == id)
            {
                return Err(Error::StaleLeadershipTransfer);
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn validate_message<C>(message: &Message<C>) -> Result<(), Error> {
    if let Message::TimeoutNow {
        term,
        round,
        last_index,
        last_term,
    } = message
    {
        if *term == 0
            || *term == u64::MAX
            || *round == 0
            || *last_index == u64::MAX
            || (*last_index == 0) != (*last_term == 0)
            || last_term > term
        {
            return Err(Error::InvalidMessage);
        }
    }
    Ok(())
}

impl<C: Clone + Eq> Raft<C> {
    /// Inspect only after the preceding transition has completed publication.
    /// None means no active attempt; it does NOT mean a transfer succeeded.
    pub fn leadership_transfer(&self) -> Result<Option<&LeadershipTransfer>, Error> {
        self.available()?;
        Ok(self.handoff.as_ref())
    }

    pub(super) fn start_handoff(
        &mut self,
        target: MemberId,
        output: &mut Output<C>,
    ) -> Result<(), Error> {
        if self.handoff.is_none() {
            self.handoff = Some(LeadershipTransfer {
                id: LeadershipTransferId(PersistenceId {
                    incarnation: Arc::clone(&self.incarnation),
                    generation: self.generation,
                }),
                target,
                term: self.state.term,
                last_index: self.last_index(),
                last_term: self.last_term(),
                phase: LeadershipTransferPhase::CatchingUp,
            });
            // The next periodic LivenessTimeout bounds this attempt. Do not
            // renew the leader's quorum-check deadline: handoff is not contact.
            // The host may also use this ID for a shorter explicit deadline.
        }
        // A snapshot offer is still only an offer. send_append retains the
        // existing peer window and cannot turn issued bytes into match evidence.
        if self
            .progress
            .get(&target)
            .is_some_and(|progress| progress.matched < self.last_index())
        {
            self.send_append(target, output, true)?;
        }
        Ok(())
    }

    pub(super) fn drive_handoff(&mut self, output: &mut Output<C>, retry: bool) {
        let Some(pending) = self.handoff.as_ref() else {
            return;
        };
        if self.role != Role::Leader || self.state.term != pending.term {
            return;
        }
        let caught_up = self
            .progress
            .get(&pending.target)
            .is_some_and(|progress| progress.matched >= pending.last_index);
        if !caught_up || (!retry && pending.phase == LeadershipTransferPhase::ElectionRequested) {
            return;
        }
        let target = pending.target;
        let message = Message::TimeoutNow {
            term: pending.term,
            round: pending.id.0.generation,
            last_index: pending.last_index,
            last_term: pending.last_term,
        };
        // One request per transition, retried only on heartbeat or explicit
        // same-target retry. Other peers keep their independent append windows.
        self.emit(target, message, output);
        if let Some(pending) = &mut self.handoff {
            pending.phase = LeadershipTransferPhase::ElectionRequested;
        }
    }

    pub(super) fn timeout_now(
        &mut self,
        from: MemberId,
        term: u64,
        last_index: u64,
        last_term: u64,
        output: &mut Output<C>,
    ) -> Result<(), Error> {
        // Full domain/configuration/recipient/sender checks ran before receive.
        // A higher-term request has already cleared the old leader, so cannot
        // manufacture an immediate campaign. A delayed duplicate after campaign
        // is stale by term; recent traffic from a DIFFERENT leader cannot help.
        if self.role != Role::Follower
            || term != self.state.term
            || self.leader != Some(from)
            || !self.state.configuration.voters.contains(&self.id)
            || self.incoming_snapshot.is_some()
            || (self.last_index(), self.last_term()) != (last_index, last_term)
        {
            return Ok(());
        }
        // Bypass pre-vote ONLY for this authenticated current-leader request.
        // campaign durably votes for self in the next term before RequestVote
        // escapes. Ordinary stable/joint quorums and log freshness still apply.
        self.campaign(output)
    }
}

#[cfg(test)]
mod tests;
