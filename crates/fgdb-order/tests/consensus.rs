//! Deterministic crash/partition histories for the production transition kernel.
//! The disk map changes BEFORE any output is released. It is a test driver,
//! not a substitute production storage implementation.
use fgdb_order::{
    Configuration, Domain, Entry, Envelope, Error, Event, Limits, MemberId, Message, Output,
    PersistentState, Raft, Role,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn config(voters: u128, learners: &[u128]) -> Configuration {
    Configuration::stable(
        Domain([1; 32]),
        [2; 32],
        (1..=voters).map(MemberId),
        learners.iter().copied().map(MemberId),
    )
    .unwrap()
}

fn publish(node: &mut Raft<u64>, event: Event<u64>) -> Output<u64> {
    let id = node.step(event).unwrap().id();
    node.persisted(id).unwrap()
}

fn message(from: u128, to: u128, message: Message<u64>) -> Event<u64> {
    Event::Receive(Envelope {
        domain: Domain([1; 32]),
        configuration: [2; 32],
        from: MemberId(from),
        to: MemberId(to),
        message,
    })
}

struct Cluster {
    nodes: BTreeMap<MemberId, Raft<u64>>,
    disk: BTreeMap<MemberId, PersistentState<u64>>,
    queue: VecDeque<Envelope<u64>>,
    isolated: BTreeSet<MemberId>,
    limits: Limits,
}

impl Cluster {
    fn new(voters: u128, learners: &[u128], batch: usize) -> Self {
        let configuration = config(voters, learners);
        let limits = Limits {
            max_log_entries: 256,
            max_append_entries: batch,
        };
        let mut cluster = Self {
            nodes: BTreeMap::new(),
            disk: BTreeMap::new(),
            queue: VecDeque::new(),
            isolated: BTreeSet::new(),
            limits,
        };
        for member in configuration.voters().union(configuration.learners()) {
            let node = Raft::new(*member, configuration.clone(), limits).unwrap();
            cluster.nodes.insert(*member, node);
            cluster.step(*member, Event::Heartbeat);
        }
        cluster
    }

    fn step(&mut self, member: MemberId, event: Event<u64>) {
        let node = self.nodes.get_mut(&member).unwrap();
        let pending = node.step(event).unwrap();
        let id = pending.id();
        if pending.requires_write() {
            self.disk.insert(member, pending.state().clone());
        } else {
            assert_eq!(self.disk.get(&member), Some(pending.state()));
        }
        let output = node.persisted(id).unwrap();
        for committed in &output.committed {
            assert!(committed.index <= self.disk[&member].commit_index());
            assert_eq!(
                &committed.entry,
                &self.disk[&member].entries()[committed.index as usize - 1]
            );
        }
        self.queue.extend(output.messages);
        self.check_committed_prefixes();
    }

    fn check_committed_prefixes(&self) {
        // Independent pairwise safety oracle, not the production quorum helper.
        for left in self.disk.values() {
            for right in self.disk.values() {
                let common = left.commit_index().min(right.commit_index()) as usize;
                assert_eq!(&left.entries()[..common], &right.entries()[..common]);
            }
        }
    }

    fn drain(&mut self) {
        let mut remaining = 100_000;
        while let Some(envelope) = self.queue.pop_front() {
            assert!(remaining > 0, "protocol did not quiesce");
            remaining -= 1;
            if !self.isolated.contains(&envelope.from) && !self.isolated.contains(&envelope.to) {
                self.step(envelope.to, Event::Receive(envelope));
            }
        }
    }

    fn elect(&mut self, id: u128) {
        self.step(MemberId(id), Event::ElectionTimeout);
        self.drain();
        assert_eq!(self.nodes[&MemberId(id)].role().unwrap(), Role::Leader);
    }

    fn restart(&mut self, id: MemberId) {
        let node = Raft::recover(id, self.disk[&id].clone(), self.limits).unwrap();
        self.nodes.insert(id, node);
    }
}

#[test]
fn one_three_and_five_voters_use_the_same_commit_path() {
    for voters in [1, 3, 5] {
        let mut cluster = Cluster::new(voters, &[], 2);
        cluster.elect(1);
        for command in 10..30 {
            cluster.step(MemberId(1), Event::Propose(command));
            cluster.drain();
        }
        for state in cluster.disk.values() {
            assert_eq!(state.commit_index(), 21);
            assert_eq!(state.entries().len(), 21);
            assert_eq!(state.entries()[0].command, None);
            assert_eq!(state.entries()[20].command, Some(29));
        }
    }
}

#[test]
fn minority_cannot_commit_and_new_leader_replaces_only_uncommitted_suffix() {
    let mut cluster = Cluster::new(3, &[], 1);
    cluster.elect(1);
    cluster.isolated.insert(MemberId(1));
    cluster.step(MemberId(1), Event::Propose(111));
    cluster.drain();
    assert_eq!(cluster.disk[&MemberId(1)].commit_index(), 1);
    cluster.elect(2);
    cluster.step(MemberId(2), Event::Propose(222));
    cluster.drain();
    cluster.isolated.clear();
    cluster.step(MemberId(2), Event::Heartbeat);
    cluster.drain();
    for state in cluster.disk.values() {
        assert_eq!(state.commit_index(), 3);
        assert!(!state.entries().iter().any(|entry| entry.command == Some(111)));
        assert_eq!(state.entries()[2].command, Some(222));
    }
    assert_eq!(cluster.nodes[&MemberId(1)].role().unwrap(), Role::Follower);
}

#[test]
fn learner_replication_never_counts_as_a_vote() {
    let mut cluster = Cluster::new(3, &[4], 2);
    cluster.elect(1);
    cluster.isolated.extend([MemberId(2), MemberId(3)]);
    cluster.step(MemberId(1), Event::Propose(50));
    cluster.drain();
    assert_eq!(cluster.disk[&MemberId(4)].entries().len(), 2);
    assert_eq!(cluster.disk[&MemberId(1)].commit_index(), 1);
    assert_eq!(
        cluster.nodes.get_mut(&MemberId(4)).unwrap().step(Event::ElectionTimeout).err(),
        Some(Error::NotVoter)
    );
}

#[test]
fn cancelled_and_failed_publication_release_no_messages_or_commands() {
    let mut node = Raft::<u64>::new(MemberId(1), config(1, &[]), Limits::default()).unwrap();
    let pending = node.step(Event::ElectionTimeout).unwrap();
    assert!(pending.requires_write());
    assert_eq!(pending.state().commit_index(), 1);
    let id = pending.id();
    assert_eq!(node.role(), Err(Error::AwaitingDurability));
    assert_eq!(node.committed_after(0), Err(Error::AwaitingDurability));
    assert_eq!(node.step(Event::Propose(7)).err(), Some(Error::AwaitingDurability));
    node.publication_failed();
    assert_eq!(node.persisted(id), Err(Error::RecoveryRequired));
    assert_eq!(node.durable_state().err(), Some(Error::RecoveryRequired));
}

#[test]
fn persistence_tokens_cannot_cross_nodes_restarts_or_generations() {
    let mut first = Raft::<u64>::new(MemberId(1), config(3, &[]), Limits::default()).unwrap();
    let mut second = Raft::<u64>::new(MemberId(2), config(3, &[]), Limits::default()).unwrap();
    let a = first.step(Event::Heartbeat).unwrap().id();
    let b = second.step(Event::Heartbeat).unwrap().id();
    assert_eq!(second.persisted(a.clone()), Err(Error::StalePersistence));
    second.persisted(b).unwrap();
    first.persisted(a.clone()).unwrap();
    let state = first.durable_state().unwrap().clone();
    let next = first.step(Event::Heartbeat).unwrap().id();
    assert_eq!(first.persisted(a.clone()), Err(Error::StalePersistence));
    first.persisted(next).unwrap();
    let mut recovered = Raft::recover(MemberId(1), state, Limits::default()).unwrap();
    let token = recovered.step(Event::Heartbeat).unwrap().id();
    assert_eq!(recovered.persisted(a), Err(Error::StalePersistence));
    recovered.persisted(token).unwrap();
}

#[test]
fn granted_vote_survives_restart_and_duplicate_requires_no_write() {
    let mut node = Raft::new(MemberId(1), config(3, &[]), Limits::default()).unwrap();
    let request = Message::RequestVote {
        term: 3,
        last_index: 0,
        last_term: 0,
    };
    let output = publish(&mut node, message(2, 1, request.clone()));
    assert!(matches!(output.messages[0].message, Message::Vote { granted: true, .. }));
    let state = node.durable_state().unwrap().clone();
    let mut node = Raft::recover(MemberId(1), state, Limits::default()).unwrap();
    let pending = node.step(message(2, 1, request.clone())).unwrap();
    assert!(!pending.requires_write());
    let id = pending.id();
    node.persisted(id).unwrap();
    let output = publish(&mut node, message(3, 1, request));
    assert!(matches!(output.messages[0].message, Message::Vote { granted: false, .. }));
    assert!(!output.reset_election_timer);
}

#[test]
fn duplicate_votes_and_responses_never_form_a_quorum() {
    let mut node = Raft::new(MemberId(1), config(5, &[]), Limits::default()).unwrap();
    publish(&mut node, Event::ElectionTimeout);
    for _ in 0..10 {
        publish(&mut node, message(2, 1, Message::Vote { term: 1, granted: true }));
    }
    assert_eq!(node.role().unwrap(), Role::Candidate);
    let output = publish(&mut node, message(3, 1, Message::Vote { term: 1, granted: true }));
    let request = output.messages.iter().find_map(|envelope| match &envelope.message {
        Message::Append { request, .. } if envelope.to == MemberId(2) => Some(*request),
        _ => None,
    }).unwrap();
    for _ in 0..10 {
        publish(&mut node, message(2, 1, Message::Appended {
            term: 1,
            request,
            success: true,
            conflict_next: u64::MAX,
        }));
    }
    assert_eq!(node.durable_state().unwrap().commit_index(), 0);
}

#[test]
fn heartbeat_commits_only_the_prefix_it_actually_matches() {
    let state = PersistentState::from_authenticated_parts(config(3, &[]), 3, None, 1, vec![
        Entry { term: 1, command: Some(1) },
        Entry { term: 2, command: Some(999) },
    ]);
    let mut node = Raft::recover(MemberId(2), state, Limits::default()).unwrap();
    publish(&mut node, message(1, 2, Message::Append {
        term: 3,
        request: 1,
        prev_index: 1,
        prev_term: 1,
        entries: vec![],
        leader_commit: 2,
    }));
    assert_eq!(node.durable_state().unwrap().commit_index(), 1);
}

#[test]
fn wrong_domain_configuration_and_peer_cannot_advance_term() {
    let mut node = Raft::<u64>::new(MemberId(1), config(3, &[]), Limits::default()).unwrap();
    publish(&mut node, Event::Heartbeat);
    for (domain, configuration, from, error) in [
        (Domain([9; 32]), [2; 32], MemberId(2), Error::WrongDomain),
        (Domain([1; 32]), [9; 32], MemberId(2), Error::WrongConfiguration),
        (Domain([1; 32]), [2; 32], MemberId(9), Error::UnknownMember),
    ] {
        let event = Event::Receive(Envelope {
            domain,
            configuration,
            from,
            to: MemberId(1),
            message: Message::RequestVote { term: 99, last_index: 0, last_term: 0 },
        });
        assert_eq!(node.step(event).err(), Some(error));
        assert_eq!(node.durable_state().unwrap().term(), 0);
    }
}

#[test]
fn log_freshness_is_lexicographic_term_then_index() {
    let state = PersistentState::from_authenticated_parts(config(3, &[]), 5, None, 0, vec![
        Entry { term: 4, command: Some(1) },
    ]);
    let mut node = Raft::recover(MemberId(1), state, Limits::default()).unwrap();
    let output = publish(&mut node, message(2, 1, Message::RequestVote {
        term: 6, last_index: 200, last_term: 3,
    }));
    assert!(matches!(output.messages[0].message, Message::Vote { granted: false, .. }));
    let output = publish(&mut node, message(3, 1, Message::RequestVote {
        term: 6, last_index: 1, last_term: 5,
    }));
    assert!(matches!(output.messages[0].message, Message::Vote { granted: true, .. }));
}

#[test]
fn old_term_entries_wait_for_a_current_term_quorum() {
    let mut cluster = Cluster::new(3, &[], 1);
    for id in 1..=3 {
        let id = MemberId(id);
        let state = PersistentState::from_authenticated_parts(config(3, &[]), 1, None, 0, vec![
            Entry { term: 1, command: Some(42) },
        ]);
        cluster.disk.insert(id, state);
        cluster.restart(id);
    }
    cluster.elect(1);
    for state in cluster.disk.values() {
        assert_eq!(state.commit_index(), 2);
        assert_eq!(state.entries()[1], Entry { term: 2, command: None });
    }
}

#[test]
fn malformed_append_and_committed_conflict_leave_state_unchanged() {
    let state = PersistentState::from_authenticated_parts(config(3, &[]), 2, None, 1, vec![
        Entry { term: 1, command: Some(1) },
    ]);
    let mut node = Raft::recover(MemberId(2), state.clone(), Limits::default()).unwrap();
    let event = message(1, 2, Message::Append {
        term: 3, request: 1, prev_index: 0, prev_term: 0,
        entries: vec![Entry { term: 2, command: Some(2) }], leader_commit: 1,
    });
    assert_eq!(node.step(event).err(), Some(Error::CommittedConflict));
    assert_eq!(node.durable_state().unwrap(), &state);
    let event = message(1, 2, Message::Append {
        term: 3, request: 1, prev_index: u64::MAX, prev_term: 1,
        entries: vec![Entry { term: 2, command: Some(2) }], leader_commit: 1,
    });
    assert_eq!(node.step(event).err(), Some(Error::InvalidMessage));
    assert_eq!(node.durable_state().unwrap(), &state);
}

#[test]
fn seeded_reordering_duplicates_loss_and_restarts_preserve_committed_prefixes() {
    for seed in 1..=12_u64 {
        let mut cluster = Cluster::new(5, &[], 2);
        cluster.elect(1);
        let mut random = seed;
        for command in 0..20 {
            cluster.step(MemberId(1), Event::Propose(command));
            for _ in 0..50 {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                if let Some(envelope) = cluster.queue.pop_front() {
                    match random % 5 {
                        0 => {} // loss; heartbeat retransmits the pending range
                        1 => cluster.queue.push_back(envelope), // reorder
                        2 => {
                            cluster.queue.push_back(envelope.clone());
                            cluster.step(envelope.to, Event::Receive(envelope));
                        }
                        _ => cluster.step(envelope.to, Event::Receive(envelope)),
                    }
                }
            }
            cluster.restart(MemberId(2 + random as u128 % 4));
            cluster.step(MemberId(1), Event::Heartbeat);
            cluster.drain();
        }
        cluster.step(MemberId(1), Event::Heartbeat);
        cluster.drain();
        for state in cluster.disk.values() {
            assert_eq!(state.commit_index(), 21, "seed {seed}");
        }
    }
}
