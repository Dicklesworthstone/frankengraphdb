//! Request-scoped, quorum-confirmed read barriers over published Raft state.
//!
//! This is the consensus part of plan §14's ReadIndexRead, not a database read
//! API, an authorization lease, or an alternate durable/wire format. A trusted
//! runtime owns authentication, transport, deadlines and bounds on outstanding
//! rounds. It must retain its writer fence while using these methods, just as
//! it does while driving `step`/`persisted`.
//!
//! A round starts only after a current-term entry is durably committed. It pins
//! that commit index, obtains fresh acknowledgements from the exact voter
//! configuration, and yields one barrier. Liveness probes, append responses,
//! payload receipts and acknowledgements for another round are not read votes.
//! The application must then wait for application AND audit visibility through
//! the barrier, pin its authorized snapshot and validate its security binding.
//! Raft indices are internal positions, never public RYW or delivery tokens.

use super::{Configuration, Domain, Error, MemberId, Raft, Role};
use std::collections::BTreeSet;
use std::sync::Arc;

/// One authenticated, configuration-bound read probe. The runtime must bind
/// every field, including the claimed sender, to its authenticated transport.
/// These public fields are an in-process adapter boundary, not wire tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadIndexProbe {
    pub domain: Domain,
    pub configuration: [u8; 32],
    pub from: MemberId,
    pub to: MemberId,
    pub term: u64,
    pub round: u64,
}

/// A response to one exact ReadIndexProbe. The transport must authenticate the
/// responder and distinguish this type from ordinary liveness/append replies.
/// It proves term/leader recognition, NOT application or payload availability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadIndexAck {
    pub domain: Domain,
    pub configuration: [u8; 32],
    pub from: MemberId,
    pub to: MemberId,
    pub term: u64,
    pub round: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadIndexError {
    Raft(Error),
    /// The leader has not yet durably committed an entry from its own term.
    CurrentTermNotCommitted,
    /// Ordinary Raft traffic must establish the leader before a probe can ACK.
    LeaderNotConfirmed,
    WrongTerm,
    WrongRound,
    /// An object from another node or recovered machine incarnation was used.
    ForeignRound,
    /// This round has already yielded its single read barrier.
    Completed,
}

impl From<Error> for ReadIndexError {
    fn from(error: Error) -> Self {
        Self::Raft(error)
    }
}

impl core::fmt::Display for ReadIndexError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis ReadIndex: {self:?}")
    }
}

impl core::error::Error for ReadIndexError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Raft(error) => Some(error),
            _ => None,
        }
    }
}

/// Exact local owner and the authority scope shared by a round and its result.
/// Holding the Arc prevents allocator address reuse from reviving stale tokens.
#[derive(Debug)]
struct ReadOwner {
    incarnation: Arc<()>,
    member: MemberId,
    domain: Domain,
    configuration: [u8; 32],
    term: u64,
    round: u64,
}

impl ReadOwner {
    fn validate<C: Clone + Eq>(&self, raft: &Raft<C>) -> Result<(), ReadIndexError> {
        if !Arc::ptr_eq(&self.incarnation, &raft.incarnation) || self.member != raft.id {
            return Err(ReadIndexError::ForeignRound);
        }
        raft.available()?;
        if self.domain != raft.state.configuration.domain {
            return Err(Error::WrongDomain.into());
        }
        if self.configuration != raft.state.configuration.identity {
            return Err(Error::WrongConfiguration.into());
        }
        if self.term != raft.state.term {
            return Err(ReadIndexError::WrongTerm);
        }
        if raft.role != Role::Leader {
            return Err(Error::NotLeader.into());
        }
        if raft.handoff.is_some() {
            return Err(Error::LeadershipTransferInProgress.into());
        }
        Ok(())
    }
}

/// One non-cloneable read round. The host bounds its outstanding round count
/// and owns its deadline/cancellation region; dropping this value cancels only
/// this read. Its tally is bounded by the configuration's 1024-member limit.
/// No registration, log entry, snapshot, fsync, or background task is created.
#[derive(Debug)]
#[must_use = "send the round's probes and collect acknowledgements, or drop it to cancel"]
pub struct ReadIndexRound {
    owner: Option<ReadOwner>,
    configuration: Configuration,
    index: u64,
    acknowledged: BTreeSet<MemberId>,
}

/// A single read's consensus fence, not a reusable leader lease. It intentionally
/// has neither a public constructor nor Clone/Copy. The host must not reuse it
/// for requests that started after its round, including retries of completed
/// client operations.
///
/// Before observing application state, wait until the role's application and
/// visible-protocol cursors cover `index()`, obtain the complete authorized
/// snapshot, and call `validate` against the same live Raft owner. This type
/// cannot certify those application, retention, authorization or audit facts.
/// Do not read an older snapshot merely because Raft has committed this index.
#[derive(Debug)]
#[must_use = "a consensus fence still requires application and audit-visible snapshot admission"]
pub struct ReadIndex {
    owner: ReadOwner,
    index: u64,
}

impl ReadIndex {
    /// Internal Raft index captured BEFORE sending this round's probes.
    pub fn index(&self) -> u64 {
        self.index
    }

    pub fn term(&self) -> u64 {
        self.owner.term
    }

    /// Recheck the owner after waiting for application/visibility. This checks
    /// only local consensus validity, not the application's applied position.
    /// Publication failure, step-down, a new term or recovery invalidates it.
    pub fn validate<C: Clone + Eq>(&self, raft: &Raft<C>) -> Result<(), ReadIndexError> {
        self.owner.validate(raft)
    }
}

impl<C: Clone + Eq> Raft<C> {
    /// Start a fresh read barrier AFTER the application has received its read
    /// request. No prior heartbeat/quorum result is reused. Commit a current-
    /// term entry through the ordinary persistence path before retrying a
    /// CurrentTermNotCommitted refusal; old-term inherited entries do not count.
    ///
    /// This changes only the checked in-process request allocator. Requests
    /// share its non-reusing namespace with append/snapshot RPCs, and recovery
    /// cannot create a leader without a new term. No durable state changes, so
    /// no Persistence acknowledgement is needed for this operation itself.
    pub fn begin_read_index(&mut self) -> Result<ReadIndexRound, ReadIndexError> {
        self.available()?;
        if self.role != Role::Leader {
            return Err(Error::NotLeader.into());
        }
        if self.handoff.is_some() {
            return Err(Error::LeadershipTransferInProgress.into());
        }
        if self.state.term == 0 || self.term_at(self.state.commit_index) != Some(self.state.term) {
            return Err(ReadIndexError::CurrentTermNotCommitted);
        }
        let round = self.request.checked_add(1).ok_or(Error::CounterExhausted)?;
        let result = ReadIndexRound {
            owner: Some(ReadOwner {
                incarnation: Arc::clone(&self.incarnation),
                member: self.id,
                domain: self.state.configuration.domain,
                configuration: self.state.configuration.identity,
                term: self.state.term,
                round,
            }),
            configuration: self.state.configuration.clone(),
            index: self.state.commit_index,
            acknowledged: BTreeSet::from([self.id]),
        };
        self.request = round;
        Ok(result)
    }

    /// Acknowledge a read probe using only already-published consensus state.
    /// This is deliberately NOT a leader-discovery or term-transition input:
    /// a lagging node must process normal Raft traffic and publish that state
    /// before it retries. A rejected probe changes neither term nor timer.
    /// The runtime must keep driving ordinary heartbeats and liveness deadlines.
    ///
    /// No bytes may escape from speculative state: even a publication with no
    /// required disk write must have passed `persisted` before this call.
    pub fn acknowledge_read_index(
        &self,
        probe: ReadIndexProbe,
    ) -> Result<ReadIndexAck, ReadIndexError> {
        self.available()?;
        if probe.domain != self.state.configuration.domain {
            return Err(Error::WrongDomain.into());
        }
        if probe.configuration != self.state.configuration.identity {
            return Err(Error::WrongConfiguration.into());
        }
        if probe.to != self.id || probe.from == self.id {
            return Err(Error::WrongRecipient.into());
        }
        if !self.state.configuration.contains(probe.from) {
            return Err(Error::UnknownMember.into());
        }
        if !self.state.configuration.voters.contains(&probe.from)
            || !self.state.configuration.voters.contains(&self.id)
        {
            return Err(Error::NotVoter.into());
        }
        if probe.round == 0 || probe.term == 0 {
            return Err(Error::InvalidMessage.into());
        }
        if probe.term != self.state.term {
            return Err(ReadIndexError::WrongTerm);
        }
        if self.role != Role::Follower || self.leader != Some(probe.from) {
            return Err(ReadIndexError::LeaderNotConfirmed);
        }
        Ok(ReadIndexAck {
            domain: probe.domain,
            configuration: probe.configuration,
            from: self.id,
            to: probe.from,
            term: probe.term,
            round: probe.round,
        })
    }
}

impl ReadIndexRound {
    fn active_owner<C: Clone + Eq>(&self, raft: &Raft<C>) -> Result<&ReadOwner, ReadIndexError> {
        let owner = self.owner.as_ref().ok_or(ReadIndexError::Completed)?;
        owner.validate(raft)?;
        Ok(owner)
    }

    /// Probe the remaining voters in canonical member order. Retrying uses the
    /// same round; acknowledged voters and learners are never targeted. A new
    /// read request must begin its own round, not reuse this retry operation.
    pub fn probes<C: Clone + Eq>(
        &self,
        raft: &Raft<C>,
    ) -> Result<Vec<ReadIndexProbe>, ReadIndexError> {
        let owner = self.active_owner(raft)?;
        Ok(self
            .configuration
            .voters
            .difference(&self.acknowledged)
            .map(|member| ReadIndexProbe {
                domain: owner.domain,
                configuration: owner.configuration,
                from: owner.member,
                to: *member,
                term: owner.term,
                round: owner.round,
            })
            .collect())
    }

    /// Record one exact authenticated voter response. Duplicate responses are
    /// idempotent and count once. Any refusal leaves the tally untouched, so an
    /// unrelated, malformed or delayed response cannot destroy a valid round.
    pub fn acknowledge<C: Clone + Eq>(
        &mut self,
        raft: &Raft<C>,
        ack: ReadIndexAck,
    ) -> Result<(), ReadIndexError> {
        let owner = self.active_owner(raft)?;
        if ack.domain != owner.domain {
            return Err(Error::WrongDomain.into());
        }
        if ack.configuration != owner.configuration {
            return Err(Error::WrongConfiguration.into());
        }
        if ack.to != owner.member || ack.from == owner.member {
            return Err(Error::WrongRecipient.into());
        }
        if !self.configuration.contains(ack.from) {
            return Err(Error::UnknownMember.into());
        }
        if !self.configuration.voters.contains(&ack.from) {
            return Err(Error::NotVoter.into());
        }
        if ack.term != owner.term {
            return Err(ReadIndexError::WrongTerm);
        }
        if ack.round != owner.round {
            return Err(ReadIndexError::WrongRound);
        }
        self.acknowledged.insert(ack.from);
        Ok(())
    }

    /// Yield the round's single barrier once the exact stable/joint quorum is
    /// satisfied. None means more voter responses are needed. Joint membership
    /// uses independent old/new majorities, never a pooled union majority.
    ///
    /// The captured index cannot move when later writes commit: quorum evidence
    /// for this round cannot be upgraded into a barrier for a later request.
    pub fn try_complete<C: Clone + Eq>(
        &mut self,
        raft: &Raft<C>,
    ) -> Result<Option<ReadIndex>, ReadIndexError> {
        self.active_owner(raft)?;
        if !self.configuration.quorum(&self.acknowledged) {
            return Ok(None);
        }
        let owner = self.owner.take().ok_or(ReadIndexError::Completed)?;
        self.acknowledged.clear();
        Ok(Some(ReadIndex {
            owner,
            index: self.index,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, Limits};

    #[test]
    fn exhausted_request_counter_does_not_mutate_or_poison_the_leader() {
        let configuration = Configuration::stable(Domain([1; 32]), [2; 32], [MemberId(1)], [])
            .expect("singleton configuration");
        let mut raft = Raft::<u64>::new(MemberId(1), configuration, Limits::default()).unwrap();
        let id = raft.step(Event::ElectionTimeout).unwrap().id();
        raft.persisted(id).unwrap();
        let before = raft.durable_state().unwrap().clone();
        raft.request = u64::MAX;
        assert!(matches!(
            raft.begin_read_index(),
            Err(ReadIndexError::Raft(Error::CounterExhausted))
        ));
        assert_eq!(raft.request, u64::MAX);
        assert_eq!(raft.durable_state().unwrap(), &before);
        assert_eq!(raft.role().unwrap(), Role::Leader);
    }
}
