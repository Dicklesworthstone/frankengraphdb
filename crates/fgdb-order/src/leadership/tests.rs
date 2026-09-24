//! Real transition kernels with explicit, in-memory publication acknowledgements.
//! These tests do not claim Chronicle disk, payload or transport authentication.

use super::*;
use crate::{Configuration, Domain, Entry, Envelope, Limits, PersistentState, SnapshotCut};
use std::collections::{BTreeMap, VecDeque};

fn stable() -> Configuration {
    Configuration::stable(Domain([11; 32]), [12; 32], [MemberId(1), MemberId(2), MemberId(3)], [MemberId(4)]).unwrap()
}

fn step(node: &mut Raft<u64>, event: Event<u64>) -> Output<u64> {
    let id = node.step(event).unwrap().id();
    node.persisted(id).unwrap()
}

fn message(from: u128, to: u128, message: Message<u64>) -> Envelope<u64> {
    Envelope { domain: stable().domain(), configuration: stable().identity(), from: MemberId(from), to: MemberId(to), message }
}

fn timeout(round: u64, last_index: u64, last_term: u64) -> Message<u64> {
    Message::TimeoutNow { term: 1, round, last_index, last_term }
}

fn is_timeout(message: &Envelope<u64>) -> bool {
    matches!(&message.message, Message::TimeoutNow { .. })
}

struct Cluster(BTreeMap<u128, Raft<u64>>);
impl Cluster {
    fn new(configuration: Configuration) -> Self {
        let nodes = configuration.members().map(|id| {
            let mut node = Raft::new(id, configuration.clone(), Limits { max_log_entries: 128, max_append_entries: 1 }).unwrap();
            node.configure_append_pipeline(4).unwrap();
            (id.0, node)
        }).collect();
        Self(nodes)
    }
    fn node(&mut self, id: u128) -> &mut Raft<u64> { self.0.get_mut(&id).unwrap() }
    fn event(&mut self, id: u128, event: Event<u64>) -> Output<u64> { step(self.node(id), event) }
    fn deliver(&mut self, message: Envelope<u64>) -> Output<u64> {
        self.event(message.to.0, Event::Receive(message))
    }
    fn settle(&mut self, messages: impl IntoIterator<Item = Envelope<u64>>) {
        let mut queue: VecDeque<_> = messages.into_iter().collect();
        let mut deliveries = 0;
        while let Some(message) = queue.pop_front() {
            deliveries += 1;
            assert!(deliveries < 4000, "bounded delivery did not quiesce");
            queue.extend(self.deliver(message).messages);
        }
    }
    fn elect(&mut self, protected: bool) {
        let event = if protected { Event::LivenessTimeout } else { Event::ElectionTimeout };
        let output = self.event(1, event);
        self.settle(output.messages);
        assert_eq!(self.node(1).role().unwrap(), Role::Leader);
    }
}

#[test]
fn caught_up_handoff_requires_a_persisted_next_term_vote_and_normal_quorum() {
    let mut cluster = Cluster::new(stable());
    cluster.elect(true);
    let writes = cluster.event(1, Event::Propose(77));
    cluster.settle(writes.messages);
    let original = cluster.node(1).durable_state().unwrap().clone();
    let output = cluster.event(1, Event::TransferLeadership(MemberId(2)));
    assert_eq!(output.role, Role::Leader, "request is not stepdown or election success");
    assert!(!output.reset_election_timer, "handoff must not renew quorum contact");
    assert!(output.committed.is_empty());
    assert_eq!(cluster.node(1).durable_state().unwrap(), &original);
    let request = output.messages.into_iter().find(is_timeout).unwrap();
    assert_eq!(request.to, MemberId(2));
    assert_eq!(cluster.node(1).leadership_transfer().unwrap().unwrap().phase(), LeadershipTransferPhase::ElectionRequested);
    let target = cluster.node(2);
    let publication = target.step(Event::Receive(request.clone())).unwrap();
    assert!(publication.requires_write());
    assert_eq!(publication.state().term(), 2);
    assert_eq!(publication.state().voted_for(), Some(MemberId(2)));
    let id = publication.id();
    assert_eq!(target.role(), Err(Error::AwaitingDurability));
    let campaign = target.persisted(id).unwrap();
    assert_eq!(campaign.role, Role::Candidate);
    assert!(campaign.messages.iter().all(|envelope| matches!(envelope.message, Message::RequestVote { term: 2, .. })));
    let duplicate = cluster.deliver(request);
    assert_eq!(duplicate.role, Role::Candidate);
    assert!(duplicate.messages.is_empty());
    cluster.settle(campaign.messages);
    assert_eq!(cluster.node(2).role().unwrap(), Role::Leader);
    assert_eq!(cluster.node(1).role().unwrap(), Role::Follower);
    assert!(cluster.node(1).leadership_transfer().unwrap().is_none());
    for node in cluster.0.values() {
        let state = node.durable_state().unwrap();
        assert_eq!(state.configuration(), original.configuration());
        assert_eq!(state.entries()[1].command, Some(77));
        assert!(state.commit_index() >= original.commit_index());
    }
}

#[test]
fn optimistic_pipeline_sends_do_not_authorize_handoff_but_later_prefix_ack_does() {
    let mut cluster = Cluster::new(stable());
    cluster.elect(false);
    let mut appends = Vec::new();
    for value in 1..=3 {
        appends.extend(cluster.event(1, Event::Propose(value)).messages.into_iter().filter(|m| m.to == MemberId(2)));
    }
    assert_eq!(appends.len(), 3);
    let admission = cluster.event(1, Event::TransferLeadership(MemberId(2)));
    assert!(!admission.messages.iter().any(is_timeout));
    assert_eq!(cluster.node(1).leadership_transfer().unwrap().unwrap().last_index(), 4);
    assert_eq!(cluster.node(1).leadership_transfer().unwrap().unwrap().phase(), LeadershipTransferPhase::CatchingUp);
    assert!(matches!(cluster.node(1).step(Event::Propose(99)), Err(Error::LeadershipTransferInProgress)));
    let mut replies = Vec::new();
    for append in appends { replies.extend(cluster.deliver(append).messages); }
    assert_eq!(replies.len(), 3);
    let pending = cluster.node(1).step(Event::Receive(replies[2].clone())).unwrap();
    assert!(pending.requires_write(), "newly advanced commit must publish before all output");
    assert_eq!(pending.state().commit_index(), 4);
    let id = pending.id();
    let released = cluster.node(1).persisted(id).unwrap();
    assert_eq!(released.messages.iter().filter(|m| is_timeout(m)).count(), 1);
    for old in &replies[..2] {
        assert!(!cluster.deliver(old.clone()).messages.iter().any(is_timeout));
    }
    assert_eq!(cluster.node(1).durable_state().unwrap().entries().len(), 4);
}

#[test]
fn snapshot_offer_and_installed_base_are_not_the_required_retained_suffix() {
    let configuration = stable();
    let cut = SnapshotCut::from_authenticated_parts(&configuration, [21; 32], [22; 32], [23; 32], 9, 3).unwrap();
    let mut cluster = Cluster::new(configuration.clone());
    for member in [1, 3] {
        let state = PersistentState::from_authenticated_snapshot(configuration.clone(), 3, None, 9, cut.clone(),
            vec![Entry { term: 3, command: Some(10) }]);
        cluster.0.insert(member, Raft::recover(MemberId(member), state, Limits { max_log_entries: 128, max_append_entries: 1 }).unwrap());
    }
    cluster.elect(false);
    let admission = cluster.event(1, Event::TransferLeadership(MemberId(2)));
    assert!(!admission.messages.iter().any(is_timeout));
    let target = cluster.node(2);
    let id = target.incoming_snapshot.as_ref().unwrap().transfer.id();
    let installed = step(target, Event::SnapshotReady(id));
    assert_eq!(installed.installed_snapshot, Some(cut));
    let mut messages = cluster.deliver(installed.messages.into_iter().next().unwrap()).messages;
    assert!(!messages.iter().any(is_timeout), "snapshot only covers index 9, not 10/11");
    let mut handoff = None;
    for _ in 0..8 {
        let mut next = Vec::new();
        for envelope in messages {
            if is_timeout(&envelope) { handoff = Some(envelope); continue; }
            if envelope.to == MemberId(2) || envelope.from == MemberId(2) {
                next.extend(cluster.deliver(envelope).messages);
            }
        }
        messages = next;
        if handoff.is_some() { break; }
    }
    assert!(handoff.is_some());
    assert_eq!(cluster.node(2).last_index(), 11);
    assert_eq!(cluster.node(2).entry_at(10).unwrap().command, Some(10));
    assert_eq!(cluster.deliver(handoff.unwrap()).role, Role::Candidate);
}

#[test]
fn retries_cannot_extend_deadline_and_abort_neither_loses_logs_nor_reuses_identity() {
    let mut cluster = Cluster::new(stable());
    cluster.elect(false);
    cluster.event(1, Event::Propose(88)); // target deliberately silent
    let first = cluster.event(1, Event::TransferLeadership(MemberId(2)));
    assert!(!first.reset_election_timer);
    let id = cluster.node(1).leadership_transfer().unwrap().unwrap().id();
    let state = cluster.node(1).durable_state().unwrap().clone();
    for _ in 0..10 {
        let again = cluster.event(1, Event::TransferLeadership(MemberId(2)));
        assert!(!again.reset_election_timer);
        assert_eq!(cluster.node(1).leadership_transfer().unwrap().unwrap().id(), id);
        assert!(!cluster.event(1, Event::Heartbeat).reset_election_timer);
    }
    assert!(matches!(cluster.node(1).step(Event::TransferLeadership(MemberId(3))), Err(Error::LeadershipTransferInProgress)));
    assert_eq!(cluster.event(1, Event::LivenessTimeout).role, Role::Leader);
    assert!(cluster.node(1).leadership_transfer().unwrap().is_none());
    assert_eq!(cluster.node(1).durable_state().unwrap(), &state);
    cluster.event(1, Event::TransferLeadership(MemberId(3)));
    let second = cluster.node(1).leadership_transfer().unwrap().unwrap().id();
    assert_ne!(id, second);
    assert!(matches!(cluster.node(1).step(Event::AbortLeadershipTransfer(id)), Err(Error::StaleLeadershipTransfer)));
    cluster.event(1, Event::AbortLeadershipTransfer(second));
    assert_eq!(cluster.node(1).durable_state().unwrap(), &state);
    cluster.event(1, Event::Propose(89));
    assert_eq!(cluster.node(1).state.entries.last().unwrap().command, Some(89));
}

#[test]
fn lost_quorum_still_steps_down_and_stale_timers_cannot_cross_recovery() {
    let mut cluster = Cluster::new(stable());
    cluster.elect(true);
    cluster.event(1, Event::LivenessTimeout); // begin a fresh unconfirmed interval
    cluster.event(1, Event::TransferLeadership(MemberId(2)));
    let id = cluster.node(1).leadership_transfer().unwrap().unwrap().id();
    let state = cluster.node(1).durable_state().unwrap().clone();
    let lost = cluster.event(1, Event::LivenessTimeout);
    assert_eq!(lost.role, Role::Follower);
    assert_eq!(cluster.node(1).durable_state().unwrap(), &state);
    let mut recovered = Raft::recover(MemberId(1), state, Limits::default()).unwrap();
    assert!(recovered.leadership_transfer().unwrap().is_none());
    assert!(matches!(recovered.step(Event::AbortLeadershipTransfer(id)), Err(Error::StaleLeadershipTransfer)));
}

#[test]
fn only_current_leader_exact_endpoint_and_voter_can_trigger_campaign() {
    let mut cluster = Cluster::new(stable());
    cluster.elect(false);
    for (from, to, body) in [
        (3, 2, timeout(9, 1, 1)),      // another voter is not the current leader
        (1, 2, timeout(9, 0, 0)),      // shorter endpoint
        (1, 2, timeout(9, 2, 1)),      // missing suffix
        (1, 4, timeout(9, 1, 1)),      // learner
    ] {
        let before = cluster.node(to).durable_state().unwrap().clone();
        let ignored = cluster.deliver(message(from, to, body));
        assert_eq!(ignored.role, Role::Follower);
        assert!(!ignored.reset_election_timer);
        assert!(ignored.messages.is_empty());
        assert_eq!(cluster.node(to).durable_state().unwrap(), &before);
    }
    let snapshot = SnapshotCut::from_authenticated_parts(&stable(), [31; 32], [32; 32], [33; 32], 20, 1).unwrap();
    let offer = cluster.deliver(message(1, 2, Message::InstallSnapshot { term: 1, request: 45, snapshot }));
    let transfer = offer.snapshot_transfers[0].id();
    let ignored = cluster.deliver(message(1, 2, timeout(10, 1, 1)));
    assert!(ignored.messages.is_empty());
    assert!(ignored.cancelled_snapshot_transfers.is_empty());
    assert_eq!(cluster.node(2).incoming_snapshot.as_ref().unwrap().transfer.id(), transfer);
}

#[test]
fn malformed_or_cross_domain_handoffs_do_not_mutate_live_state() {
    let mut cluster = Cluster::new(stable());
    cluster.elect(false);
    let good = message(1, 2, timeout(1, 1, 1));
    let before = cluster.node(2).durable_state().unwrap().clone();
    for case in 0..11 {
        let mut bad = good.clone();
        match case {
            0 => bad.domain = Domain([0; 32]),
            1 => bad.configuration = [0; 32],
            2 => bad.to = MemberId(3),
            3 => bad.from = MemberId(2),
            4 => bad.from = MemberId(99),
            5 => bad.from = MemberId(4),
            6 => bad.message = timeout(0, 1, 1),
            7 => bad.message = timeout(1, 0, 1),
            8 => bad.message = timeout(1, u64::MAX, 1),
            9 => bad.message = Message::TimeoutNow { term: u64::MAX, round: 1, last_index: 1, last_term: 1 },
            _ => bad.message = timeout(1, 1, 2),
        }
        assert!(cluster.node(2).step(Event::Receive(bad)).is_err(), "case {case}");
        assert_eq!(cluster.node(2).durable_state().unwrap(), &before);
    }
    for target in [1, 4, 99] {
        assert!(matches!(cluster.node(1).step(Event::TransferLeadership(MemberId(target))), Err(Error::InvalidLeadershipTarget)));
    }
    assert!(matches!(cluster.node(2).step(Event::TransferLeadership(MemberId(1))), Err(Error::NotLeader)));
    let higher = message(1, 2, Message::TimeoutNow { term: 2, round: 2, last_index: 1, last_term: 1 });
    let output = cluster.deliver(higher);
    assert_eq!(output.role, Role::Follower);
    assert!(output.messages.is_empty());
    assert_eq!(cluster.node(2).durable_state().unwrap().term(), 2);
    assert_eq!(cluster.node(2).durable_state().unwrap().voted_for(), None);
}

#[test]
fn handoff_election_requires_both_joint_majorities() {
    let configuration = Configuration::joint(Domain([11; 32]), [12; 32],
        [MemberId(1), MemberId(2), MemberId(3)], [MemberId(3), MemberId(4), MemberId(5)], [MemberId(6)]).unwrap();
    let mut cluster = Cluster::new(configuration);
    cluster.elect(false);
    let offer = cluster.event(1, Event::TransferLeadership(MemberId(3)));
    let campaign = cluster.deliver(offer.messages.into_iter().find(is_timeout).unwrap());
    assert_eq!(campaign.role, Role::Candidate);
    for voter in [1, 2] {
        let request = campaign.messages.iter().find(|m| m.to == MemberId(voter)).unwrap().clone();
        let reply = cluster.deliver(request).messages.into_iter().next().unwrap();
        assert_eq!(cluster.deliver(reply).role, Role::Candidate, "pooled quorum is insufficient");
    }
    let request = campaign.messages.into_iter().find(|m| m.to == MemberId(4)).unwrap();
    let reply = cluster.deliver(request).messages.into_iter().next().unwrap();
    assert_eq!(cluster.deliver(reply).role, Role::Leader);
}

#[test]
fn abort_cannot_recall_an_already_released_election_request() {
    let mut cluster = Cluster::new(stable());
    cluster.elect(false);
    let offered = cluster.event(1, Event::TransferLeadership(MemberId(2)));
    let id = cluster.node(1).leadership_transfer().unwrap().unwrap().id();
    cluster.event(1, Event::AbortLeadershipTransfer(id));
    cluster.event(1, Event::Propose(999)); // not delivered: remains an uncertain write
    let campaign = cluster.deliver(offered.messages.into_iter().find(is_timeout).unwrap());
    assert_eq!(campaign.role, Role::Candidate);
    let request = campaign.messages.into_iter().find(|m| m.to == MemberId(1)).unwrap();
    let response = cluster.deliver(request);
    assert_eq!(response.role, Role::Follower);
    assert!(matches!(response.messages[0].message, Message::Vote { granted: false, .. }));
    assert_eq!(cluster.node(1).state.entries.last().unwrap().command, Some(999));
}

#[test]
fn failed_target_vote_publication_cannot_release_campaign_messages() {
    let mut cluster = Cluster::new(stable());
    cluster.elect(false);
    let offered = cluster.event(1, Event::TransferLeadership(MemberId(2)));
    let request = offered.messages.into_iter().find(is_timeout).unwrap();
    let target = cluster.node(2);
    let pending = target.step(Event::Receive(request)).unwrap();
    assert!(pending.requires_write());
    let id = pending.id();
    target.publication_failed();
    assert_eq!(target.persisted(id), Err(Error::RecoveryRequired));
    assert!(matches!(target.step(Event::Heartbeat), Err(Error::RecoveryRequired)));
}

#[test]
fn compaction_preserves_the_frozen_endpoint_and_rejects_retired_acknowledgements() {
    let mut cluster = Cluster::new(stable());
    cluster.elect(false);
    let data = cluster.event(1, Event::Propose(40));
    cluster.settle(data.messages);
    let outgoing = cluster.event(1, Event::Propose(41));
    let old = outgoing.messages.into_iter().find(|m| m.to == MemberId(2)).unwrap();
    let reply = cluster.deliver(old).messages.into_iter().next().unwrap();
    cluster.event(1, Event::TransferLeadership(MemberId(2)));
    let token = cluster.node(1).leadership_transfer().unwrap().unwrap().id();
    let cut = SnapshotCut::from_authenticated_parts(&stable(), [41; 32], [42; 32], [43; 32], 2, 1).unwrap();
    cluster.event(1, Event::Compact(cut));
    assert!(!cluster.deliver(reply).messages.iter().any(is_timeout), "compaction retired that append ID");
    let transfer = cluster.node(1).leadership_transfer().unwrap().unwrap();
    assert_eq!(transfer.id(), token);
    assert_eq!(transfer.last_index(), 3);
    let heartbeat = cluster.event(1, Event::Heartbeat);
    let append = heartbeat.messages.into_iter().find(|m| m.to == MemberId(2)).unwrap();
    let reply = cluster.deliver(append).messages.into_iter().next().unwrap();
    assert!(cluster.deliver(reply).messages.iter().any(is_timeout));
}

#[test]
fn exhausted_term_and_pre_candidate_state_cannot_start_a_handoff_election() {
    let mut cluster = Cluster::new(stable());
    cluster.elect(false);
    let discovery = cluster.event(2, Event::LivenessTimeout);
    assert_eq!(discovery.role, Role::PreCandidate);
    let before = cluster.node(2).durable_state().unwrap().clone();
    let output = cluster.deliver(message(1, 2, timeout(11, 1, 1)));
    assert_eq!(output.role, Role::PreCandidate);
    assert!(output.messages.is_empty());
    assert_eq!(cluster.node(2).durable_state().unwrap(), &before);

    let one = Configuration::stable(Domain([11; 32]), [12; 32], [MemberId(1)], []).unwrap();
    let mut solo = Raft::new(MemberId(1), one, Limits::default()).unwrap();
    step(&mut solo, Event::ElectionTimeout);
    assert!(matches!(solo.step(Event::TransferLeadership(MemberId(1))), Err(Error::InvalidLeadershipTarget)));

    // An actual term at the integer frontier admits no next-term election.
    let max = PersistentState::from_authenticated_parts(stable(), u64::MAX - 1, None, 0, Vec::new());
    let mut node = Raft::recover(MemberId(1), max, Limits::default()).unwrap();
    step(&mut node, Event::ElectionTimeout);
    step(&mut node, Event::Receive(message(2, 1, Message::Vote { term: u64::MAX, granted: true })));
    assert_eq!(node.role().unwrap(), Role::Leader);
    let before = node.durable_state().unwrap().clone();
    assert!(matches!(node.step(Event::TransferLeadership(MemberId(2))), Err(Error::CounterExhausted)));
    assert_eq!(node.durable_state().unwrap(), &before);
    assert!(node.leadership_transfer().unwrap().is_none());
}

#[test]
fn handoff_freezes_batched_and_scalar_proposals_before_any_admission_mutation() {
    for caught_up in [false, true] {
        let mut cluster = Cluster::new(stable());
        for node in cluster.0.values_mut() {
            node.limits.max_append_entries = 4;
        }
        cluster.elect(false);
        let group = cluster.event(1, Event::ProposeBatch(vec![7, 8, 9]));
        if caught_up {
            cluster.settle(group.messages);
        }
        cluster.event(1, Event::TransferLeadership(MemberId(2)));
        let node = cluster.node(1);
        let transfer = node.leadership_transfer().unwrap().unwrap().clone();
        assert_eq!(transfer.last_index(), 4);
        assert_eq!(transfer.phase(), if caught_up {
            LeadershipTransferPhase::ElectionRequested
        } else {
            LeadershipTransferPhase::CatchingUp
        });
        let before = node.durable_state().unwrap().clone();
        let counters = (node.generation, node.request);
        // Include otherwise-valid groups: a test using only oversized groups
        // would pass even if the handoff guard were removed from batch admission.
        for commands in [vec![], vec![10], vec![10, 11, 12], vec![10; 5]] {
            assert!(matches!(node.step(Event::ProposeBatch(commands)),
                Err(Error::LeadershipTransferInProgress)));
            assert_eq!(node.durable_state().unwrap(), &before);
            assert_eq!((node.generation, node.request), counters);
            assert_eq!(node.leadership_transfer().unwrap(), Some(&transfer));
        }
        assert!(matches!(node.step(Event::Propose(10)), Err(Error::LeadershipTransferInProgress)));
        assert_eq!((node.generation, node.request), counters);
        step(node, Event::AbortLeadershipTransfer(transfer.id()));
        step(node, Event::ProposeBatch(vec![10, 11]));
        assert_eq!(node.last_index(), 6);
        assert_eq!(node.state.entries[4..].iter().map(|entry| entry.command).collect::<Vec<_>>(),
            [Some(10), Some(11)]);
    }
}

#[test]
fn identical_numeric_attempts_in_distinct_incarnations_cannot_cancel_each_other() {
    let mut left = Cluster::new(stable());
    let mut right = Cluster::new(stable());
    left.elect(false);
    right.elect(false);
    left.event(1, Event::TransferLeadership(MemberId(2)));
    right.event(1, Event::TransferLeadership(MemberId(2)));
    let a = left.node(1).leadership_transfer().unwrap().unwrap().id();
    let b = right.node(1).leadership_transfer().unwrap().unwrap().id();
    assert_eq!(a.0.generation, b.0.generation, "exercise the equal-scalar case");
    assert_ne!(a, b);
    assert!(matches!(right.node(1).step(Event::AbortLeadershipTransfer(a)), Err(Error::StaleLeadershipTransfer)));
    assert_eq!(right.node(1).leadership_transfer().unwrap().unwrap().id(), b);
}
