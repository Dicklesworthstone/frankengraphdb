//! Protocol tests over the real Raft transitions. The disk image below models
//! only the publication contract; it is not filesystem or network evidence.
use fgdb_order::{
    Configuration, Domain, Entry, Envelope, Error, Event, Limits, MemberId, Message, Output,
    PersistentState, Raft, Role, SnapshotCut,
};
use std::collections::{BTreeSet, VecDeque};

fn config() -> Configuration {
    Configuration::stable(
        Domain([1; 32]),
        [2; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [],
    )
    .unwrap()
}

struct Node {
    raft: Raft<u64>,
    disk: PersistentState<u64>,
    writes: usize,
}

impl Node {
    fn recovered(id: u128, configuration: Configuration) -> Self {
        let disk = PersistentState::from_authenticated_parts(configuration, 0, None, 0, vec![]);
        let raft = Raft::recover(MemberId(id), disk.clone(), Limits::default()).unwrap();
        Self {
            raft,
            disk,
            writes: 0,
        }
    }
    fn step(&mut self, event: Event<u64>) -> Output<u64> {
        let pending = self.raft.step(event).unwrap();
        if pending.requires_write() {
            self.disk = pending.state().clone();
            self.writes += 1;
        }
        let id = pending.id();
        self.raft.persisted(id).unwrap()
    }
}

fn envelope(
    configuration: &Configuration,
    from: u128,
    to: u128,
    message: Message<u64>,
) -> Envelope<u64> {
    Envelope {
        domain: configuration.domain(),
        configuration: configuration.identity(),
        from: MemberId(from),
        to: MemberId(to),
        message,
    }
}

fn preview(output: &Output<u64>) -> (u64, u64) {
    output
        .messages
        .iter()
        .find_map(|m| match m.message {
            Message::PreVoteRequest {
                prospective_term,
                round,
                ..
            } => Some((prospective_term, round)),
            _ => None,
        })
        .unwrap()
}

fn approval(
    configuration: &Configuration,
    from: u128,
    to: u128,
    term: u64,
    round: u64,
) -> Event<u64> {
    Event::Receive(envelope(
        configuration,
        from,
        to,
        Message::PreVoteReply {
            term: term - 1,
            prospective_term: term,
            round,
            granted: true,
        },
    ))
}

fn pump(nodes: &mut [Node], messages: Vec<Envelope<u64>>, reachable: &[u128]) {
    let mut queue: VecDeque<_> = messages.into();
    let mut deliveries = 0;
    while let Some(message) = queue.pop_front() {
        deliveries += 1;
        assert!(deliveries < 4096, "message dispatch did not quiesce");
        if reachable.contains(&message.from.0) && reachable.contains(&message.to.0) {
            let index = nodes
                .iter()
                .position(|node| node.raft.id() == message.to)
                .unwrap();
            queue.extend(nodes[index].step(Event::Receive(message)).messages);
        }
    }
}

#[test]
fn isolated_timeouts_never_raise_a_term_or_erase_a_durable_vote() {
    let configuration = config();
    let disk = PersistentState::from_authenticated_parts(
        configuration.clone(),
        7,
        Some(MemberId(2)),
        0,
        vec![],
    );
    let mut node = Node {
        raft: Raft::recover(MemberId(1), disk.clone(), Limits::default()).unwrap(),
        disk: disk.clone(),
        writes: 0,
    };
    let mut previous_round = 0;
    for _ in 0..100 {
        let output = node.step(Event::LivenessTimeout);
        let (term, round) = preview(&output);
        assert_eq!(term, 8);
        assert!(round > previous_round);
        previous_round = round;
        assert_eq!(output.role, Role::PreCandidate);
        assert_eq!(node.raft.durable_state().unwrap(), &disk);
    }
    assert_eq!(node.writes, 0);
}

#[test]
fn prospective_term_is_not_actual_term_and_pre_votes_do_not_reset_timers() {
    let configuration = config();
    let mut node = Node::recovered(2, configuration.clone());
    let before = node.disk.clone();
    let output = node.step(Event::Receive(envelope(
        &configuration,
        1,
        2,
        Message::PreVoteRequest {
            prospective_term: 100,
            round: 9,
            last_index: 0,
            last_term: 0,
        },
    )));
    assert_eq!(node.disk, before);
    assert_eq!(node.writes, 0);
    assert!(!output.reset_election_timer);
    assert_eq!(
        output.messages[0].message,
        Message::PreVoteReply {
            term: 0,
            prospective_term: 100,
            round: 9,
            granted: true,
        }
    );
}

#[test]
fn pre_quorum_only_starts_the_durability_gated_actual_election() {
    let configuration = config();
    let mut node = Node::recovered(1, configuration.clone());
    let output = node.step(Event::LivenessTimeout);
    let (term, round) = preview(&output);
    assert_eq!(node.writes, 0);
    let pending = node
        .raft
        .step(approval(&configuration, 2, 1, term, round))
        .unwrap();
    assert!(pending.requires_write());
    assert_eq!(pending.state().term(), 1);
    assert_eq!(pending.state().voted_for(), Some(MemberId(1)));
    assert_eq!(pending.state().commit_index(), 0);
    assert!(pending.state().entries().is_empty());
    node.disk = pending.state().clone();
    let id = pending.id();
    assert_eq!(node.raft.role(), Err(Error::AwaitingDurability));
    let output = node.raft.persisted(id).unwrap();
    assert_eq!(output.role, Role::Candidate);
    assert!(output.committed.is_empty());
    assert!(
        output
            .messages
            .iter()
            .all(|m| matches!(m.message, Message::RequestVote { term: 1, .. }))
    );
}

#[test]
fn surviving_majority_elects_and_commits_without_the_third_member() {
    let configuration = config();
    let mut nodes: Vec<_> = (1..=3)
        .map(|id| Node::recovered(id, configuration.clone()))
        .collect();
    let output = nodes[0].step(Event::LivenessTimeout);
    pump(&mut nodes, output.messages, &[1, 2]);
    assert_eq!(nodes[0].raft.role(), Ok(Role::Leader));
    let output = nodes[0].step(Event::Propose(42));
    pump(&mut nodes, output.messages, &[1, 2]);
    let output = nodes[0].step(Event::Heartbeat);
    pump(&mut nodes, output.messages, &[1, 2]);
    for node in &nodes[..2] {
        assert!(
            node.raft
                .committed_after(0)
                .unwrap()
                .iter()
                .any(|e| e.entry.command == Some(42))
        );
        assert_eq!(node.disk.term(), 1);
    }
    assert_eq!(nodes[2].disk.term(), 0);
}

#[test]
fn returning_isolated_member_cannot_disrupt_a_healthy_leader() {
    let configuration = config();
    let mut nodes: Vec<_> = (1..=3)
        .map(|id| Node::recovered(id, configuration.clone()))
        .collect();
    let election = nodes[0].step(Event::LivenessTimeout);
    pump(&mut nodes, election.messages, &[1, 2]);
    for _ in 0..50 {
        nodes[2].step(Event::LivenessTimeout);
    }
    assert_eq!(nodes[2].disk.term(), 0);
    let retry = nodes[2].step(Event::LivenessTimeout);
    pump(&mut nodes, retry.messages, &[1, 2, 3]);
    assert_eq!(nodes[0].raft.role(), Ok(Role::Leader));
    assert_eq!(nodes[0].disk.term(), 1);
    assert_eq!(nodes[1].disk.term(), 1);
    // An actual higher responder term is learned without disrupting its leader.
    assert_eq!(nodes[2].disk.term(), 1);
    let retry = nodes[2].step(Event::LivenessTimeout);
    pump(&mut nodes, retry.messages, &[1, 2, 3]);
    assert_eq!(nodes[2].disk.term(), 1);
    assert_ne!(nodes[2].raft.role(), Ok(Role::Leader));
}

#[test]
fn stale_round_duplicate_and_wrong_prospective_replies_do_not_elect() {
    let configuration =
        Configuration::stable(Domain([1; 32]), [2; 32], (1..=5).map(MemberId), []).unwrap();
    let mut node = Node::recovered(1, configuration.clone());
    let old = preview(&node.step(Event::LivenessTimeout));
    let current = preview(&node.step(Event::LivenessTimeout));
    for donor in 2..=5 {
        node.step(approval(&configuration, donor, 1, old.0, old.1));
    }
    assert_eq!(node.raft.role(), Ok(Role::PreCandidate));
    for _ in 0..8 {
        node.step(approval(&configuration, 2, 1, current.0, current.1));
    }
    assert_eq!(node.raft.role(), Ok(Role::PreCandidate));
    // Keep the actual term at zero; a future prospective term is not a term update.
    node.step(Event::Receive(envelope(
        &configuration,
        3,
        1,
        Message::PreVoteReply {
            term: 0,
            prospective_term: 2,
            round: current.1,
            granted: true,
        },
    )));
    assert_eq!(node.disk.term(), 0);
    node.step(approval(&configuration, 3, 1, current.0, current.1));
    assert_eq!(node.raft.role(), Ok(Role::Candidate));
    assert_eq!(node.disk.term(), 1);
}

#[test]
fn both_joint_majorities_are_required_in_the_pre_election_too() {
    let configuration = Configuration::joint(
        Domain([1; 32]),
        [2; 32],
        [1, 2, 3].map(MemberId),
        [3, 4, 5].map(MemberId),
        [],
    )
    .unwrap();
    let mut node = Node::recovered(1, configuration.clone());
    let (term, round) = preview(&node.step(Event::LivenessTimeout));
    for donor in [2, 3] {
        node.step(approval(&configuration, donor, 1, term, round));
    }
    assert_eq!(
        node.disk.term(),
        0,
        "pooled union must not substitute for two majorities"
    );
    node.step(approval(&configuration, 4, 1, term, round));
    assert_eq!(node.disk.term(), 1);
    assert_eq!(node.raft.role(), Ok(Role::Candidate));
}

#[test]
fn actual_higher_term_rejection_is_persisted_and_invalidates_pre_election() {
    let configuration = config();
    let mut node = Node::recovered(1, configuration.clone());
    let (term, round) = preview(&node.step(Event::LivenessTimeout));
    let pending = node
        .raft
        .step(Event::Receive(envelope(
            &configuration,
            2,
            1,
            Message::PreVoteReply {
                term: 8,
                prospective_term: term,
                round,
                granted: false,
            },
        )))
        .unwrap();
    assert!(pending.requires_write());
    assert_eq!(pending.state().term(), 8);
    let id = pending.id();
    let output = node.raft.persisted(id).unwrap();
    assert_eq!(output.role, Role::Follower);
    assert!(output.messages.is_empty());
    assert!(!output.reset_election_timer);
    node.step(approval(&configuration, 3, 1, term, round));
    assert_eq!(node.raft.durable_state().unwrap().term(), 8);
    assert_eq!(node.raft.role(), Ok(Role::Follower));
}

#[test]
fn malformed_cross_domain_and_learner_messages_fail_before_mutation() {
    let configuration = Configuration::stable(
        Domain([1; 32]),
        [2; 32],
        [1, 2, 3].map(MemberId),
        [MemberId(4)],
    )
    .unwrap();
    let mut node = Node::recovered(1, configuration.clone());
    let valid = Message::PreVoteRequest {
        prospective_term: 1,
        round: 1,
        last_index: 0,
        last_term: 0,
    };
    let mut foreign = envelope(&configuration, 2, 1, valid.clone());
    foreign.domain = Domain([3; 32]);
    let mut stale_config = envelope(&configuration, 2, 1, valid.clone());
    stale_config.configuration = [4; 32];
    let cases = [
        (foreign, Error::WrongDomain),
        (stale_config, Error::WrongConfiguration),
        (envelope(&configuration, 4, 1, valid), Error::NotVoter),
        (
            envelope(
                &configuration,
                2,
                1,
                Message::PreVoteRequest {
                    prospective_term: 0,
                    round: 1,
                    last_index: 0,
                    last_term: 0,
                },
            ),
            Error::InvalidMessage,
        ),
        (
            envelope(
                &configuration,
                2,
                1,
                Message::PreVoteRequest {
                    prospective_term: 1,
                    round: 0,
                    last_index: 0,
                    last_term: 0,
                },
            ),
            Error::InvalidMessage,
        ),
        (
            envelope(
                &configuration,
                2,
                1,
                Message::PreVoteRequest {
                    prospective_term: 1,
                    round: 1,
                    last_index: 1,
                    last_term: 0,
                },
            ),
            Error::InvalidMessage,
        ),
        (
            envelope(
                &configuration,
                2,
                1,
                Message::PreVoteRequest {
                    prospective_term: 1,
                    round: 1,
                    last_index: 1,
                    last_term: 1,
                },
            ),
            Error::InvalidMessage,
        ),
        (
            envelope(
                &configuration,
                2,
                1,
                Message::PreVoteReply {
                    term: 1,
                    prospective_term: 1,
                    round: 1,
                    granted: true,
                },
            ),
            Error::InvalidMessage,
        ),
    ];
    for (message, expected) in cases {
        assert_eq!(
            node.raft.step(Event::Receive(message)).unwrap_err(),
            expected
        );
        assert_eq!(node.raft.durable_state().unwrap(), &node.disk);
    }
    let mut learner = Node::recovered(4, configuration);
    assert_eq!(
        learner.raft.step(Event::LivenessTimeout).unwrap_err(),
        Error::NotVoter
    );
}

#[test]
fn compacted_log_freshness_uses_absolute_index_and_term_not_suffix_length() {
    let configuration = config();
    let cut = SnapshotCut::from_authenticated_parts(
        &configuration,
        [3; 32],
        [4; 32],
        [5; 32],
        1_000_000,
        8,
    )
    .unwrap();
    let disk = PersistentState::from_authenticated_snapshot(
        configuration.clone(),
        9,
        None,
        1_000_000,
        cut,
        vec![Entry {
            term: 9,
            command: Some(42),
        }],
    );
    let mut node = Node {
        raft: Raft::recover(MemberId(2), disk.clone(), Limits::default()).unwrap(),
        disk,
        writes: 0,
    };
    for (last_index, last_term, granted) in [
        (999_999, 8, false),
        (1_000_000, 9, false),
        (1_000_001, 9, true),
        (u64::MAX - 1, 8, false),
    ] {
        let output = node.step(Event::Receive(envelope(
            &configuration,
            1,
            2,
            Message::PreVoteRequest {
                prospective_term: 10,
                round: 1,
                last_index,
                last_term,
            },
        )));
        assert_eq!(
            output.messages[0].message,
            Message::PreVoteReply {
                term: 9,
                prospective_term: 10,
                round: 1,
                granted,
            }
        );
    }
    let output = node.step(Event::LivenessTimeout);
    assert!(output.messages.iter().all(|m| matches!(
        m.message,
        Message::PreVoteRequest {
            prospective_term: 10,
            last_index: 1_000_001,
            last_term: 9,
            ..
        }
    )));
    assert_eq!(node.writes, 0);
}

#[test]
fn pre_election_cancels_old_snapshot_capability_without_abandoning_bytes() {
    let configuration = config();
    let mut node = Node::recovered(2, configuration.clone());
    let cut =
        SnapshotCut::from_authenticated_parts(&configuration, [3; 32], [4; 32], [5; 32], 12, 3)
            .unwrap();
    let offered = node.step(Event::Receive(envelope(
        &configuration,
        1,
        2,
        Message::InstallSnapshot {
            term: 3,
            request: 1,
            snapshot: cut,
        },
    )));
    let transfer = offered.snapshot_transfers[0].id();
    let output = node.step(Event::LivenessTimeout);
    assert_eq!(output.cancelled_snapshot_transfers, vec![transfer.clone()]);
    assert_eq!(
        node.raft.step(Event::SnapshotReady(transfer)).unwrap_err(),
        Error::StaleSnapshotTransfer
    );
    assert_eq!(node.disk.commit_index(), 0);
    assert!(node.disk.snapshot().is_none());
}

#[test]
fn failed_actual_vote_publication_requires_recovery_and_preserves_the_vote() {
    let configuration = config();
    let mut node = Node::recovered(1, configuration.clone());
    let (term, round) = preview(&node.step(Event::LivenessTimeout));
    let pending = node
        .raft
        .step(approval(&configuration, 2, 1, term, round))
        .unwrap();
    node.disk = pending.state().clone(); // failure after the root may have changed
    let id = pending.id();
    node.raft.publication_failed();
    assert_eq!(
        node.raft.persisted(id).unwrap_err(),
        Error::RecoveryRequired
    );
    let recovered = Raft::recover(MemberId(1), node.disk, Limits::default()).unwrap();
    assert_eq!(
        recovered.durable_state().unwrap().voted_for(),
        Some(MemberId(1))
    );
    assert_eq!(recovered.role(), Ok(Role::Follower));
}

#[test]
fn single_member_pre_election_is_the_ordinary_durable_specialization() {
    let configuration = Configuration::stable(Domain([1; 32]), [2; 32], [MemberId(1)], []).unwrap();
    let mut node = Node::recovered(1, configuration);
    let output = node.step(Event::LivenessTimeout);
    assert_eq!(output.role, Role::Leader);
    assert!(output.messages.is_empty());
    assert_eq!(node.disk.term(), 1);
    assert_eq!(node.disk.commit_index(), 1);
    assert_eq!(node.writes, 1);
    assert_eq!(node.disk.entries()[0].command, None);
}

#[test]
fn term_exhaustion_refuses_before_changing_durable_or_volatile_state() {
    let configuration = config();
    let disk =
        PersistentState::<u64>::from_authenticated_parts(configuration, u64::MAX, None, 0, vec![]);
    let mut raft = Raft::recover(MemberId(1), disk.clone(), Limits::default()).unwrap();
    assert_eq!(
        raft.step(Event::LivenessTimeout).unwrap_err(),
        Error::CounterExhausted
    );
    assert_eq!(raft.durable_state().unwrap(), &disk);
    assert_eq!(raft.role(), Ok(Role::Follower));
}

#[test]
fn exhaustive_stable_and_joint_pre_votes_match_independent_majority_counting() {
    let profiles = [
        (vec![1, 2, 3, 4, 5], None),
        (vec![1, 2, 3], Some(vec![3, 4, 5])),
        (vec![1, 2], Some(vec![2, 3, 4, 5])),
    ];
    for (old, new) in profiles {
        let configuration = match &new {
            Some(new) => Configuration::joint(
                Domain([1; 32]),
                [2; 32],
                old.iter().copied().map(MemberId),
                new.iter().copied().map(MemberId),
                [],
            )
            .unwrap(),
            None => Configuration::stable(
                Domain([1; 32]),
                [2; 32],
                old.iter().copied().map(MemberId),
                [],
            )
            .unwrap(),
        };
        for mask in 0..16 {
            for reverse in [false, true] {
                let mut node = Node::recovered(1, configuration.clone());
                let (term, round) = preview(&node.step(Event::LivenessTimeout));
                let mut donors: Vec<_> = (2..=5).filter(|id| mask & (1 << (id - 2)) != 0).collect();
                if reverse {
                    donors.reverse();
                }
                let mut seen = BTreeSet::from([1]);
                for donor in donors {
                    seen.insert(donor);
                    node.step(approval(&configuration, donor, 1, term, round));
                    node.step(approval(&configuration, donor, 1, term, round));
                    let majority = |voters: &[u128]| {
                        voters.iter().filter(|id| seen.contains(*id)).count() * 2 > voters.len()
                    };
                    let won = majority(&old) && new.as_ref().is_none_or(|new| majority(new));
                    assert_eq!(
                        node.disk.term() == 1,
                        won,
                        "mask={mask} reverse={reverse} seen={seen:?}"
                    );
                }
            }
        }
    }
}

fn probes(output: &Output<u64>) -> Vec<Envelope<u64>> {
    output
        .messages
        .iter()
        .filter(|m| matches!(m.message, Message::QuorumProbe { .. }))
        .cloned()
        .collect()
}

fn protected_leader(configuration: &Configuration) -> (Vec<Node>, Output<u64>) {
    let mut nodes: Vec<_> = configuration
        .voters()
        .iter()
        .chain(configuration.learners())
        .map(|id| Node::recovered(id.0, configuration.clone()))
        .collect();
    let output = nodes[0].step(Event::LivenessTimeout);
    // Explicitly deliver the pre-election and actual votes, retaining the
    // initial leader output so each test controls every post-election response.
    let mut queue: VecDeque<_> = output.messages.into();
    while let Some(message) = queue.pop_front() {
        let index = nodes
            .iter()
            .position(|node| node.raft.id() == message.to)
            .unwrap();
        let output = nodes[index].step(Event::Receive(message));
        if output.role == Role::Leader {
            return (nodes, output);
        }
        queue.extend(output.messages);
    }
    panic!("configuration did not elect the test leader");
}

#[test]
fn absent_fresh_quorum_steps_down_without_mutating_log_term_or_vote() {
    let configuration = config();
    let (mut nodes, election) = protected_leader(&configuration);
    assert_eq!(probes(&election).len(), 2);
    let pending = nodes[0].step(Event::Propose(77));
    assert!(pending.committed.is_empty());
    let before = nodes[0].disk.clone();
    let writes = nodes[0].writes;
    let output = nodes[0].step(Event::LivenessTimeout);
    assert_eq!(output.role, Role::Follower);
    assert_eq!(output.leader, None);
    assert!(output.messages.is_empty());
    assert!(output.reset_election_timer);
    assert_eq!(nodes[0].disk, before);
    assert_eq!(nodes[0].writes, writes);
    assert_eq!(
        nodes[0].raft.step(Event::Propose(88)).unwrap_err(),
        Error::NotLeader
    );
    assert!(
        nodes[0]
            .disk
            .entries()
            .iter()
            .any(|entry| entry.command == Some(77)),
        "timeout cannot decide or abandon a proposed payload"
    );
}

#[test]
fn healthy_quorum_remains_leader_with_no_periodic_root_writes() {
    let configuration = config();
    let (mut nodes, output) = protected_leader(&configuration);
    pump(&mut nodes, output.messages, &[1, 2]);
    let committed = nodes[0].disk.commit_index();
    let writes: Vec<_> = nodes.iter().map(|node| node.writes).collect();
    let mut rounds = BTreeSet::new();
    for _ in 0..20 {
        let output = nodes[0].step(Event::LivenessTimeout);
        assert_eq!(output.role, Role::Leader);
        assert!(output.reset_election_timer);
        for message in probes(&output) {
            let Message::QuorumProbe { round, .. } = message.message else {
                unreachable!()
            };
            rounds.insert(round);
        }
        pump(&mut nodes, output.messages, &[1, 2]);
        assert_eq!(nodes[0].disk.commit_index(), committed);
    }
    assert_eq!(rounds.len(), 20);
    assert_eq!(
        nodes.iter().map(|node| node.writes).collect::<Vec<_>>(),
        writes
    );
}

#[test]
fn old_round_and_duplicate_replies_cannot_keep_an_isolated_leader_alive() {
    let configuration = config();
    let (mut nodes, output) = protected_leader(&configuration);
    let probe = probes(&output)
        .into_iter()
        .find(|message| message.to == MemberId(2))
        .unwrap();
    let reply = nodes[1].step(Event::Receive(probe)).messages.remove(0);
    nodes[0].step(Event::Receive(reply.clone()));
    assert_eq!(nodes[0].step(Event::LivenessTimeout).role, Role::Leader);
    for _ in 0..100 {
        nodes[0].step(Event::Receive(reply.clone()));
    }
    let output = nodes[0].step(Event::LivenessTimeout);
    assert_eq!(output.role, Role::Follower);
    assert_eq!(nodes[0].disk.term(), 1);
}

#[test]
fn heartbeat_retries_only_unconfirmed_voters_without_extending_the_interval() {
    let configuration = config();
    let (mut nodes, output) = protected_leader(&configuration);
    let original = probes(&output);
    let heartbeat = nodes[0].step(Event::Heartbeat);
    assert_eq!(probes(&heartbeat), original);
    assert!(!heartbeat.reset_election_timer);
    let reply = nodes[1]
        .step(Event::Receive(original[0].clone()))
        .messages
        .remove(0);
    nodes[0].step(Event::Receive(reply));
    let heartbeat = nodes[0].step(Event::Heartbeat);
    assert_eq!(probes(&heartbeat), vec![original[1].clone()]);
    assert!(!heartbeat.reset_election_timer);
}

#[test]
fn liveness_quorum_is_not_append_commit_or_snapshot_evidence() {
    let configuration = config();
    let (mut nodes, output) = protected_leader(&configuration);
    // Only liveness probes are delivered: all no-op and user append messages
    // are withheld. Even a unanimous liveness quorum cannot commit either.
    pump(&mut nodes, probes(&output), &[1, 2, 3]);
    nodes[0].step(Event::Propose(42));
    for _ in 0..4 {
        let output = nodes[0].step(Event::LivenessTimeout);
        assert_eq!(output.role, Role::Leader);
        assert_eq!(nodes[0].disk.commit_index(), 0);
        assert!(output.committed.is_empty());
        pump(&mut nodes, probes(&output), &[1, 2, 3]);
    }
    assert_eq!(nodes[1].disk.entries().len(), 0);
    assert_eq!(nodes[2].disk.entries().len(), 0);
}

#[test]
fn joint_quorum_checks_both_sides_and_rejects_learner_liveness() {
    let configuration = Configuration::joint(
        Domain([1; 32]),
        [2; 32],
        [1, 2, 3].map(MemberId),
        [3, 4, 5].map(MemberId),
        [MemberId(6)],
    )
    .unwrap();
    for (reachable, expected) in [
        (vec![1, 2, 3], Role::Follower),
        (vec![1, 3, 4], Role::Leader),
    ] {
        let (mut nodes, output) = protected_leader(&configuration);
        assert!(
            probes(&output)
                .iter()
                .all(|message| message.to != MemberId(6))
        );
        let Message::QuorumProbe { term, round } = probes(&output)[0].message else {
            unreachable!()
        };
        let before = nodes[0].disk.clone();
        let error = nodes[0]
            .raft
            .step(Event::Receive(envelope(
                &configuration,
                6,
                1,
                Message::QuorumReply { term, round },
            )))
            .unwrap_err();
        assert_eq!(error, Error::NotVoter);
        assert_eq!(nodes[0].raft.durable_state().unwrap(), &before);
        pump(&mut nodes, probes(&output), &reachable);
        assert_eq!(nodes[0].step(Event::LivenessTimeout).role, expected);
    }
}

#[test]
fn actual_higher_term_probe_is_published_before_reply_and_leader_reset() {
    let configuration = config();
    let mut node = Node::recovered(2, configuration.clone());
    let pending = node
        .raft
        .step(Event::Receive(envelope(
            &configuration,
            1,
            2,
            Message::QuorumProbe { term: 9, round: 81 },
        )))
        .unwrap();
    assert!(pending.requires_write());
    assert_eq!(pending.state().term(), 9);
    assert_eq!(pending.state().commit_index(), 0);
    node.disk = pending.state().clone();
    let id = pending.id();
    assert_eq!(node.raft.role(), Err(Error::AwaitingDurability));
    let output = node.raft.persisted(id).unwrap();
    assert_eq!(output.leader, Some(MemberId(1)));
    assert!(output.reset_election_timer);
    assert_eq!(
        output.messages[0].message,
        Message::QuorumReply { term: 9, round: 81 }
    );
    let stale = node.step(Event::Receive(envelope(
        &configuration,
        3,
        2,
        Message::QuorumProbe { term: 8, round: 1 },
    )));
    assert!(!stale.reset_election_timer);
    assert_eq!(stale.leader, Some(MemberId(1)));
    assert_eq!(
        stale.messages[0].message,
        Message::QuorumReply { term: 9, round: 1 }
    );
}

#[test]
fn malformed_or_foreign_liveness_traffic_cannot_change_the_term() {
    let configuration = config();
    let mut node = Node::recovered(2, configuration.clone());
    for message in [
        Message::QuorumProbe { term: 9, round: 0 },
        Message::QuorumReply { term: 9, round: 0 },
        Message::QuorumProbe { term: 0, round: 1 },
        Message::QuorumReply { term: 0, round: 1 },
    ] {
        assert_eq!(
            node.raft
                .step(Event::Receive(envelope(&configuration, 1, 2, message)))
                .unwrap_err(),
            Error::InvalidMessage
        );
        assert_eq!(node.raft.durable_state().unwrap(), &node.disk);
    }
    let mut foreign = envelope(
        &configuration,
        1,
        2,
        Message::QuorumProbe { term: 9, round: 1 },
    );
    foreign.configuration = [9; 32];
    assert_eq!(
        node.raft.step(Event::Receive(foreign)).unwrap_err(),
        Error::WrongConfiguration
    );
    assert_eq!(node.raft.durable_state().unwrap(), &node.disk);
}

#[test]
fn probes_keep_snapshot_transfer_alive_without_acknowledging_its_installation() {
    let configuration = config();
    let mut node = Node::recovered(2, configuration.clone());
    let cut =
        SnapshotCut::from_authenticated_parts(&configuration, [3; 32], [4; 32], [5; 32], 12, 3)
            .unwrap();
    let offered = node.step(Event::Receive(envelope(
        &configuration,
        1,
        2,
        Message::InstallSnapshot {
            term: 3,
            request: 1,
            snapshot: cut,
        },
    )));
    let transfer = offered.snapshot_transfers[0].id();
    let writes = node.writes;
    for round in 1..=5 {
        let output = node.step(Event::Receive(envelope(
            &configuration,
            1,
            2,
            Message::QuorumProbe { term: 3, round },
        )));
        assert!(output.reset_election_timer);
        assert!(output.cancelled_snapshot_transfers.is_empty());
        assert!(output.installed_snapshot.is_none());
        assert!(output.committed.is_empty());
        assert!(
            output
                .messages
                .iter()
                .all(|m| matches!(m.message, Message::QuorumReply { .. }))
        );
    }
    assert_eq!(node.writes, writes);
    let installed = node.step(Event::SnapshotReady(transfer));
    assert!(installed.installed_snapshot.is_some());
}

#[test]
fn direct_election_can_enable_checks_but_gets_one_complete_probe_interval() {
    let configuration = config();
    let mut nodes: Vec<_> = (1..=3)
        .map(|id| Node::recovered(id, configuration.clone()))
        .collect();
    let output = nodes[0].step(Event::ElectionTimeout);
    pump(&mut nodes, output.messages, &[1, 2, 3]);
    let initial = nodes[0].step(Event::LivenessTimeout);
    assert_eq!(initial.role, Role::Leader);
    assert_eq!(probes(&initial).len(), 2);
    assert_eq!(nodes[0].step(Event::LivenessTimeout).role, Role::Follower);
}

#[test]
fn quorum_one_periodic_checks_do_not_add_log_entries_or_syncs() {
    let configuration = Configuration::stable(Domain([1; 32]), [2; 32], [MemberId(1)], []).unwrap();
    let mut node = Node::recovered(1, configuration);
    node.step(Event::LivenessTimeout);
    let before = node.disk.clone();
    for _ in 0..20 {
        let output = node.step(Event::LivenessTimeout);
        assert_eq!(output.role, Role::Leader);
        assert!(output.messages.is_empty());
        assert!(output.committed.is_empty());
        assert_eq!(node.disk, before);
    }
    assert_eq!(node.writes, 1);
}

#[test]
fn failover_after_quorum_loss_preserves_commits_and_stale_probes_do_not_revive_leader() {
    let configuration = config();
    let (mut nodes, election) = protected_leader(&configuration);
    pump(&mut nodes, election.messages, &[1, 2, 3]);
    let proposal = nodes[0].step(Event::Propose(101));
    pump(&mut nodes, proposal.messages, &[1, 2, 3]);
    let heartbeat = nodes[0].step(Event::Heartbeat);
    pump(&mut nodes, heartbeat.messages, &[1, 2, 3]);
    let pending = nodes[0].step(Event::LivenessTimeout);
    let old_probe = probes(&pending)[0].clone();
    // Partition 1 away. Both followers first observe their expired leader
    // deadline; the second can obtain a pre-vote from the leader-free first.
    nodes[1].step(Event::LivenessTimeout);
    let election = nodes[2].step(Event::LivenessTimeout);
    pump(&mut nodes, election.messages, &[2, 3]);
    assert_eq!(nodes[2].raft.role(), Ok(Role::Leader));
    assert_eq!(nodes[0].step(Event::LivenessTimeout).role, Role::Follower);
    let proposal = nodes[2].step(Event::Propose(202));
    pump(&mut nodes, proposal.messages, &[2, 3]);
    let heartbeat = nodes[2].step(Event::Heartbeat);
    pump(&mut nodes, heartbeat.messages, &[2, 3]);
    for node in &nodes[1..] {
        let commands: Vec<_> = node
            .raft
            .committed_after(0)
            .unwrap()
            .into_iter()
            .filter_map(|e| e.entry.command)
            .collect();
        assert_eq!(commands, vec![101, 202]);
    }
    let output = nodes[1].step(Event::Receive(old_probe));
    assert!(!output.reset_election_timer);
    assert_eq!(output.leader, Some(MemberId(3)));
    pump(&mut nodes, output.messages, &[1, 2, 3]);
    assert_eq!(nodes[0].raft.role(), Ok(Role::Follower));
    assert_eq!(nodes[0].disk.term(), nodes[2].disk.term());
    assert!(nodes[0].raft.step(Event::Propose(303)).is_err());
}

#[test]
fn granting_an_actual_vote_cancels_same_term_preview_without_double_voting() {
    let configuration = config();
    let disk = PersistentState::from_authenticated_parts(configuration.clone(), 7, None, 0, vec![]);
    let mut node = Node {
        raft: Raft::recover(MemberId(1), disk.clone(), Limits::default()).unwrap(),
        disk,
        writes: 0,
    };
    let (term, round) = preview(&node.step(Event::LivenessTimeout));
    let voted = node.step(Event::Receive(envelope(
        &configuration,
        2,
        1,
        Message::RequestVote {
            term: 7,
            last_index: 0,
            last_term: 0,
        },
    )));
    assert_eq!(voted.role, Role::Follower);
    assert!(matches!(
        voted.messages[0].message,
        Message::Vote {
            term: 7,
            granted: true
        }
    ));
    assert_eq!(node.disk.voted_for(), Some(MemberId(2)));
    node.step(approval(&configuration, 3, 1, term, round));
    assert_eq!(node.disk.term(), 7);
    node.step(Event::LivenessTimeout);
    let rejected = node.step(Event::Receive(envelope(
        &configuration,
        3,
        1,
        Message::RequestVote {
            term: 7,
            last_index: 0,
            last_term: 0,
        },
    )));
    assert!(matches!(
        rejected.messages[0].message,
        Message::Vote {
            term: 7,
            granted: false
        }
    ));
    assert_eq!(node.disk.voted_for(), Some(MemberId(2)));
}
