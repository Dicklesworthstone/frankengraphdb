//! Volatile election discovery and quorum checks. None of these fields belong
//! in RaftHardState. The runtime supplies deadlines; no clock is read here.
//!
//! A pre-vote only licenses attempting the ordinary, durability-gated election.
//! It is not a vote, read lease, membership change or payload-availability proof.

use std::collections::BTreeSet;

use crate::{Error, MemberId, Message, Output, Raft, Role};

#[derive(Default)]
pub(super) struct LivenessState {
    enabled: bool,
    pre_vote: Option<PreVoteRound>,
    quorum: Option<QuorumRound>,
}

struct PreVoteRound {
    term: u64,
    round: u64,
    voters: BTreeSet<MemberId>,
}

struct QuorumRound {
    round: u64,
    voters: BTreeSet<MemberId>,
}

impl LivenessState {
    pub(super) fn reset(&mut self) {
        self.pre_vote = None;
        self.quorum = None;
        // The host selected protected elections for this machine incarnation.
        // A term/role transition does not switch that choice back off.
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }
}

pub(super) fn validate<C>(message: &Message<C>) -> Result<(), Error> {
    match message {
        Message::PreVoteRequest {
            prospective_term,
            round,
            last_index,
            last_term,
        } => {
            if *prospective_term == 0
                || *round == 0
                || *last_index == u64::MAX
                || (*last_index == 0) != (*last_term == 0)
                || last_term >= prospective_term
            {
                return Err(Error::InvalidMessage);
            }
        }
        Message::PreVoteReply {
            term,
            prospective_term,
            round,
            granted,
        } => {
            if *prospective_term == 0 || *round == 0 || (*granted && term >= prospective_term) {
                return Err(Error::InvalidMessage);
            }
        }
        Message::QuorumProbe { round: 0, .. } | Message::QuorumReply { round: 0, .. } => {
            return Err(Error::InvalidMessage);
        }
        _ => {}
    }
    Ok(())
}

impl<C: Clone + Eq> Raft<C> {
    pub(super) fn liveness_timeout(&mut self, output: &mut Output<C>) -> Result<(), Error> {
        self.liveness.enabled = true;
        if self.role != Role::Leader {
            return self.pre_campaign(output);
        }
        if self
            .liveness
            .quorum
            .as_ref()
            .is_some_and(|quorum| !self.state.configuration.quorum(&quorum.voters))
        {
            // Stepdown changes neither term/vote nor log/commit ownership. In
            // particular, silence never decides or abandons a proposed write.
            self.follow(self.state.term);
            output.reset_election_timer = true;
            return Ok(());
        }
        self.start_quorum_probe(output);
        Ok(())
    }

    pub(super) fn start_quorum_probe(&mut self, output: &mut Output<C>) {
        self.liveness.quorum = Some(QuorumRound {
            round: self.generation,
            voters: BTreeSet::from([self.id]),
        });
        output.reset_election_timer = true;
        self.retry_quorum_probe(output);
    }

    pub(super) fn retry_quorum_probe(&self, output: &mut Output<C>) {
        let Some(quorum) = &self.liveness.quorum else {
            return;
        };
        for member in &self.state.configuration.voters {
            if !quorum.voters.contains(member) {
                self.emit(
                    *member,
                    Message::QuorumProbe {
                        term: self.state.term,
                        round: quorum.round,
                    },
                    output,
                );
            }
        }
    }

    pub(super) fn quorum_probe(
        &mut self,
        from: MemberId,
        term: u64,
        round: u64,
        output: &mut Output<C>,
    ) {
        if term == self.state.term {
            // Unlike pre-vote, this is contact from an actual leader in its
            // actual term. Preserve an in-progress same-leader snapshot, and
            // renew the follower election deadline even during bulk transfer.
            self.accept_leader(from, term, output);
        }
        if self.state.configuration.voters.contains(&self.id) {
            self.emit(
                from,
                Message::QuorumReply {
                    term: self.state.term,
                    round,
                },
                output,
            );
        }
    }

    pub(super) fn quorum_reply(&mut self, from: MemberId, term: u64, round: u64) {
        if self.role != Role::Leader || term != self.state.term {
            return;
        }
        if let Some(quorum) = &mut self.liveness.quorum {
            if round == quorum.round {
                // Membership, domain, recipient and nonzero fields were checked
                // before transition. This grants no match-index/read evidence.
                quorum.voters.insert(from);
            }
        }
    }

    pub(super) fn pre_campaign(&mut self, output: &mut Output<C>) -> Result<(), Error> {
        if self.role == Role::Leader {
            return Ok(());
        }
        let term = self
            .state
            .term
            .checked_add(1)
            .ok_or(Error::CounterExhausted)?;
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
        self.liveness.pre_vote = Some(PreVoteRound {
            term,
            round,
            voters,
        });
        for member in &self.state.configuration.voters {
            if *member != self.id {
                self.emit(
                    *member,
                    Message::PreVoteRequest {
                        prospective_term: term,
                        round,
                        last_index: self.last_index(),
                        last_term: self.last_term(),
                    },
                    output,
                );
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
        self.emit(
            from,
            Message::PreVoteReply {
                term: self.state.term,
                prospective_term,
                round,
                granted,
            },
            output,
        );
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
        let Some(pending) = &mut self.liveness.pre_vote else {
            return Ok(());
        };
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
