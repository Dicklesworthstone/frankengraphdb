//! Volatile election discovery. None of these fields belong in RaftHardState.
//!
//! A pre-vote only licenses attempting the ordinary, durability-gated election.
//! It is not a vote, read lease, membership change or payload-availability proof.

use std::collections::BTreeSet;

use crate::{Error, MemberId, Message, Output, Raft, Role};

#[derive(Default)]
pub(super) struct LivenessState {
    pre_vote: Option<PreVoteRound>,
}

struct PreVoteRound {
    term: u64,
    round: u64,
    voters: BTreeSet<MemberId>,
}

impl LivenessState {
    pub(super) fn reset(&mut self) {
        self.pre_vote = None;
    }
}

pub(super) fn validate<C>(message: &Message<C>) -> Result<(), Error> {
    match message {
        Message::PreVoteRequest { prospective_term, round, last_index, last_term } => {
            if *prospective_term == 0 || *round == 0 || *last_index == u64::MAX
                || (*last_index == 0) != (*last_term == 0) || last_term >= prospective_term
            {
                return Err(Error::InvalidMessage);
            }
        }
        Message::PreVoteReply { term, prospective_term, round, granted } => {
            if *prospective_term == 0 || *round == 0 || (*granted && term >= prospective_term) {
                return Err(Error::InvalidMessage);
            }
        }
        _ => {}
    }
    Ok(())
}

impl<C: Clone + Eq> Raft<C> {
    pub(super) fn pre_campaign(&mut self, output: &mut Output<C>) -> Result<(), Error> {
        if self.role == Role::Leader {
            return Ok(());
        }
        let term = self.state.term.checked_add(1).ok_or(Error::CounterExhausted)?;
        // Preserve the actual term AND its durable vote. Clearing a vote here
        // would let repeated pre-votes grant two actual votes in one term.
        self.follow(self.state.term);
        self.incoming_snapshot = None;
        self.role = Role::PreCandidate;
        let voters = BTreeSet::from([self.id]);
        output.reset_election_timer = true;
        if self.state.configuration.quorum(&voters) {
            return self.campaign(output);
        }
        let round = self.generation;
        self.liveness.pre_vote = Some(PreVoteRound { term, round, voters });
        for member in &self.state.configuration.voters {
            if *member != self.id {
                self.emit(*member, Message::PreVoteRequest {
                    prospective_term: term, round,
                    last_index: self.last_index(), last_term: self.last_term(),
                }, output);
            }
        }
        Ok(())
    }

    pub(super) fn pre_vote_request(
        &self,
        from: MemberId,
        prospective_term: u64,
        round: u64,
        last_index: u64,
        last_term: u64,
        output: &mut Output<C>,
    ) {
        // A known leader is recent until the runtime delivers the locally
        // expired deadline. Incoming pre-votes cannot expire or renew it.
        let granted = self.state.configuration.voters.contains(&self.id)
            && self.leader.is_none()
            && prospective_term > self.state.term
            && (last_term, last_index) >= (self.last_term(), self.last_index());
        self.emit(from, Message::PreVoteReply {
            term: self.state.term, prospective_term, round, granted,
        }, output);
    }

    pub(super) fn pre_vote_reply(
        &mut self,
        from: MemberId,
        prospective_term: u64,
        round: u64,
        granted: bool,
        output: &mut Output<C>,
    ) -> Result<(), Error> {
        if self.role != Role::PreCandidate || !granted {
            return Ok(());
        }
        let Some(pending) = &mut self.liveness.pre_vote else { return Ok(()) };
        if pending.round != round || pending.term != prospective_term {
            return Ok(());
        }
        pending.voters.insert(from);
        if self.state.configuration.quorum(&pending.voters) {
            // This is the ONLY term-changing transition in the pre-vote path.
            // It also persists the self-vote before releasing RequestVote.
            self.campaign(output)?;
        }
        Ok(())
    }
}
