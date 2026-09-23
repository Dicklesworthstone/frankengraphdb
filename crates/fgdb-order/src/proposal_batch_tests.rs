//! The actual transition machine; persisted() below models completed publication,
//! not a claim that a disk barrier ran. No alternate batch evaluator is used.

use super::*;

fn config(count: u128) -> Configuration {
    Configuration::stable(Domain([1; 32]), [2; 32], (1..=count).map(MemberId), []).unwrap()
}

fn step<C: Clone + Eq>(node: &mut Raft<C>, event: Event<C>) -> Output<C> {
    let id = node.step(event).unwrap().id();
    node.persisted(id).unwrap()
}

fn single(limits: Limits) -> Raft<u64> {
    let mut node = Raft::new(MemberId(1), config(1), limits).unwrap();
    assert_eq!(step(&mut node, Event::ElectionTimeout).role, Role::Leader);
    node
}

struct Cluster {
    nodes: BTreeMap<MemberId, Raft<u64>>,
}

impl Cluster {
    fn new(configuration: Configuration, limits: Limits, window: usize) -> Self {
        let nodes = configuration.members().map(|id| {
            let mut node = Raft::new(id, configuration.clone(), limits).unwrap();
            node.configure_append_pipeline(window).unwrap();
            (id, node)
        }).collect();
        let mut cluster = Self { nodes };
        let output = step(cluster.leader(), Event::ElectionTimeout);
        cluster.deliver(output.messages, |_| true);
        assert_eq!(cluster.leader().role(), Ok(Role::Leader));
        assert_eq!(cluster.leader().state.commit_index, 1);
        cluster
    }

    fn leader(&mut self) -> &mut Raft<u64> {
        self.nodes.get_mut(&MemberId(1)).unwrap()
    }

    fn deliver(&mut self, messages: Vec<Envelope<u64>>, allow: impl Fn(MemberId) -> bool) {
        let mut queue: VecDeque<_> = messages.into();
        let mut work = 0;
        while let Some(message) = queue.pop_front() {
            if !allow(message.to) { continue; }
            work += 1;
            assert!(work < 1000, "finite reliable delivery did not quiesce");
            let output = step(self.nodes.get_mut(&message.to).unwrap(), Event::Receive(message));
            queue.extend(output.messages);
        }
    }
}

#[test]
fn one_publication_releases_separate_contiguous_commands_once() {
    let mut node = single(Limits::default());
    let before = node.generation;
    let stale = step(&mut node, Event::Heartbeat);
    assert!(stale.committed.is_empty());
    let pending = node.step(Event::ProposeBatch(vec![10, 20, 20, 30])).unwrap();
    assert!(pending.requires_write());
    let id = pending.id();
    assert_eq!(id.generation, before + 2); // heartbeat plus ONE proposal transition
    assert_eq!(pending.state().entries().len(), 5);
    assert_eq!(pending.state().commit_index(), 5);
    assert_eq!(node.role(), Err(Error::AwaitingDurability));
    assert!(matches!(node.step(Event::Heartbeat), Err(Error::AwaitingDurability)));
    let output = node.persisted(id.clone()).unwrap();
    assert_eq!(output.committed.iter().map(|entry| (entry.index, entry.entry.command)).collect::<Vec<_>>(),
        [(2, Some(10)), (3, Some(20)), (4, Some(20)), (5, Some(30))]);
    assert!(output.messages.is_empty());
    assert!(matches!(node.persisted(id), Err(Error::StalePersistence)));
    assert_eq!(node.state.term, 1);
}

#[test]
fn scalar_and_batch_paths_have_identical_ordered_states() {
    for count in 1..=32 {
        let commands: Vec<_> = (0..count).map(|index| index % 5).collect();
        let mut batched = single(Limits::default());
        let mut scalar = single(Limits::default());
        let generation = batched.generation;
        let batch = step(&mut batched, Event::ProposeBatch(commands.clone()));
        let mut individually = Vec::new();
        for command in commands {
            individually.extend(step(&mut scalar, Event::Propose(command)).committed);
        }
        assert_eq!(batch.committed, individually);
        assert_eq!(batched.durable_state().unwrap(), scalar.durable_state().unwrap());
        assert_eq!(batched.generation, generation + 1);
        assert_eq!(scalar.generation, generation + count);
    }
}

#[test]
fn empty_oversized_and_full_log_batches_reject_without_a_prefix() {
    let mut node = single(Limits { max_log_entries: 5, max_append_entries: 4 });
    for (commands, expected) in [(vec![], Error::InvalidMessage), (vec![1; 5], Error::AppendTooLarge)] {
        let before = node.state.clone();
        let counters = (node.generation, node.request);
        assert!(matches!(node.step(Event::ProposeBatch(commands)), Err(error) if error == expected));
        assert_eq!(node.durable_state().unwrap(), &before);
        assert_eq!((node.generation, node.request), counters);
    }
    step(&mut node, Event::ProposeBatch(vec![1, 2, 3]));
    let before = node.state.clone();
    let generation = node.generation;
    assert!(matches!(node.step(Event::ProposeBatch(vec![4, 5])), Err(Error::LogFull)));
    assert_eq!(node.durable_state().unwrap(), &before);
    assert_eq!(node.generation, generation);
    let output = step(&mut node, Event::ProposeBatch(vec![4]));
    assert_eq!(output.committed[0].index, 5);
    assert!(matches!(node.step(Event::Propose(5)), Err(Error::LogFull)));
}

#[test]
fn absolute_index_and_generation_exhaustion_reject_the_whole_range() {
    let configuration = config(1);
    let base = u64::MAX - 4;
    let cut = SnapshotCut::from_authenticated_parts(&configuration, [3; 32], [4; 32], [5; 32], base, 1).unwrap();
    let mut node = Raft::<u64>::recover(MemberId(1), PersistentState::from_authenticated_snapshot(
        configuration, 1, None, base, cut, Vec::new(),
    ), Limits::default()).unwrap();
    step(&mut node, Event::ElectionTimeout); // no-op occupies MAX-3
    let before = node.state.clone();
    let generation = node.generation;
    assert!(matches!(node.step(Event::ProposeBatch(vec![1, 2, 3])), Err(Error::CounterExhausted)));
    assert_eq!(node.durable_state().unwrap(), &before);
    assert_eq!(node.generation, generation);
    let output = step(&mut node, Event::ProposeBatch(vec![1, 2]));
    assert_eq!(output.committed.iter().map(|entry| entry.index).collect::<Vec<_>>(), [u64::MAX - 2, u64::MAX - 1]);
    assert!(matches!(node.step(Event::ProposeBatch(vec![3])), Err(Error::CounterExhausted)));

    let mut node = single(Limits::default());
    node.generation = u64::MAX;
    let before = node.state.clone();
    assert!(matches!(node.step(Event::ProposeBatch(vec![1, 2])), Err(Error::CounterExhausted)));
    assert_eq!(node.durable_state().unwrap(), &before);
}

#[test]
fn nonleaders_and_learners_cannot_admit_a_batch() {
    for phase in 0..3 {
        let mut node = Raft::<u64>::new(MemberId(1), config(3), Limits::default()).unwrap();
        if phase == 1 { step(&mut node, Event::ElectionTimeout); }
        if phase == 2 { step(&mut node, Event::LivenessTimeout); }
        let before = node.state.clone();
        assert!(matches!(node.step(Event::ProposeBatch(vec![1, 2])), Err(Error::NotLeader)));
        assert_eq!(node.durable_state().unwrap(), &before);
    }
    let configuration = Configuration::stable(Domain([1; 32]), [2; 32], [MemberId(1)], [MemberId(2)]).unwrap();
    let mut learner = Raft::<u64>::new(MemberId(2), configuration, Limits::default()).unwrap();
    assert!(matches!(learner.step(Event::ProposeBatch(vec![1])), Err(Error::NotLeader)));
    assert_eq!(learner.durable_state().unwrap().commit_index(), 0);
}

#[test]
fn actual_follower_persists_before_reply_and_commit_remains_separate() {
    let mut cluster = Cluster::new(config(3), Limits::default(), 4);
    let output = step(cluster.leader(), Event::ProposeBatch(vec![10, 20, 30]));
    assert!(output.committed.is_empty());
    assert_eq!(cluster.leader().state.commit_index, 1);
    let request = output.messages.into_iter().find(|message| message.to == MemberId(2)).unwrap();
    let follower = cluster.nodes.get_mut(&MemberId(2)).unwrap();
    let pending = follower.step(Event::Receive(request)).unwrap();
    assert!(pending.requires_write());
    assert_eq!(pending.state().entries().len(), 4);
    assert_eq!(pending.state().commit_index(), 1);
    let id = pending.id();
    assert_eq!(follower.role(), Err(Error::AwaitingDurability));
    let reply = follower.persisted(id).unwrap();
    assert!(reply.committed.is_empty());
    let reply = reply.messages[0].clone();
    let pending = cluster.leader().step(Event::Receive(reply.clone())).unwrap();
    assert!(pending.requires_write()); // leader's commit publication
    let id = pending.id();
    let committed = cluster.leader().persisted(id).unwrap();
    assert_eq!(committed.committed.iter().map(|entry| entry.entry.command).collect::<Vec<_>>(), [Some(10), Some(20), Some(30)]);
    cluster.deliver(committed.messages, |member| member != MemberId(3));
    assert_eq!(cluster.nodes[&MemberId(2)].state.commit_index, 4);
    assert!(step(cluster.leader(), Event::Receive(reply)).committed.is_empty());
}

#[test]
fn joint_batch_needs_each_majority_and_learners_do_not_count() {
    let configuration = Configuration::joint(Domain([1; 32]), [2; 32],
        [MemberId(1), MemberId(2), MemberId(3)], [MemberId(3), MemberId(4), MemberId(5)], [MemberId(6)]).unwrap();
    let mut cluster = Cluster::new(configuration, Limits::default(), 4);
    let output = step(cluster.leader(), Event::ProposeBatch(vec![10, 20]));
    for (peer, expected) in [(2, 1), (6, 1), (3, 1), (4, 3)] {
        let request = output.messages.iter().find(|message| message.to == MemberId(peer)).unwrap().clone();
        let response = step(cluster.nodes.get_mut(&MemberId(peer)).unwrap(), Event::Receive(request));
        for response in response.messages {
            let _ = step(cluster.leader(), Event::Receive(response));
        }
        assert_eq!(cluster.leader().state.commit_index, expected, "peer {peer}");
    }
}

#[test]
fn reordered_pipelined_batches_converge_with_a_silent_minority() {
    for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
        let mut cluster = Cluster::new(config(3), Limits { max_log_entries: 32, max_append_entries: 2 }, 4);
        let mut batches = Vec::new();
        for first in [10, 20, 30] {
            let output = step(cluster.leader(), Event::ProposeBatch(vec![first, first + 1]));
            assert!(output.committed.is_empty());
            batches.push(output.messages.into_iter().find(|message| message.to == MemberId(2)).unwrap());
        }
        assert_eq!(cluster.leader().progress[&MemberId(2)].in_flight.len(), 3);
        cluster.deliver(order.into_iter().map(|index| batches[index].clone()).collect(), |member| member != MemberId(3));
        assert_eq!(cluster.leader().state.commit_index, 7, "{order:?}");
        let expected = vec![Some(10), Some(11), Some(20), Some(21), Some(30), Some(31)];
        assert_eq!(cluster.leader().state.entries[1..].iter().map(|entry| entry.command).collect::<Vec<_>>(), expected);
        assert_eq!(cluster.nodes[&MemberId(2)].state.entries, cluster.nodes[&MemberId(1)].state.entries);
        assert_eq!(cluster.nodes[&MemberId(2)].state.commit_index, 7);
    }
}

#[test]
fn acknowledgement_of_a_prefix_does_not_commit_the_later_batch() {
    let mut cluster = Cluster::new(config(3), Limits { max_log_entries: 32, max_append_entries: 2 }, 4);
    let first = step(cluster.leader(), Event::ProposeBatch(vec![10, 11]));
    let second = step(cluster.leader(), Event::ProposeBatch(vec![20, 21]));
    let request = first.messages.into_iter().find(|message| message.to == MemberId(2)).unwrap();
    let response = step(cluster.nodes.get_mut(&MemberId(2)).unwrap(), Event::Receive(request));
    let output = step(cluster.leader(), Event::Receive(response.messages[0].clone()));
    assert_eq!(output.committed.iter().map(|entry| entry.index).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(cluster.leader().state.commit_index, 3);
    cluster.deliver(second.messages, |member| member != MemberId(3));
    assert_eq!(cluster.leader().state.commit_index, 5);
}

#[test]
fn internal_request_exhaustion_fences_the_complete_unpublished_batch() {
    let mut cluster = Cluster::new(config(3), Limits::default(), 4);
    let node = cluster.leader();
    node.request = u64::MAX;
    assert!(matches!(node.step(Event::ProposeBatch(vec![10, 20])), Err(Error::CounterExhausted)));
    assert_eq!(node.role(), Err(Error::RecoveryRequired));
    assert_eq!(node.state.entries.len(), 3); // all commands moved, no output escaped
    assert!(node.pending.is_none());
}

#[test]
fn panic_while_building_committed_output_fences_every_entry() {
    #[derive(Debug, PartialEq, Eq)]
    struct Bomb(u64);
    impl Clone for Bomb {
        fn clone(&self) -> Self {
            assert_ne!(self.0, 20, "panic while cloning a committed command");
            Self(self.0)
        }
    }
    let mut node = Raft::new(MemberId(1), config(1), Limits::default()).unwrap();
    step(&mut node, Event::ElectionTimeout);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = node.step(Event::ProposeBatch(vec![Bomb(10), Bomb(20), Bomb(30)]));
    }));
    assert!(result.is_err());
    assert_eq!(node.role(), Err(Error::RecoveryRequired));
    assert_eq!(node.state.entries.len(), 4);
    assert!(node.pending.is_none());
}

#[test]
fn late_batch_rejection_preserves_pending_replication_identities() {
    let mut cluster = Cluster::new(config(3), Limits { max_log_entries: 5, max_append_entries: 4 }, 4);
    let output = step(cluster.leader(), Event::ProposeBatch(vec![10, 20, 30]));
    let node = cluster.leader();
    let before = node.state.clone();
    let counters = (node.generation, node.request);
    let requests: Vec<_> = node.progress[&MemberId(2)].in_flight.iter().cloned().collect();
    assert!(matches!(node.step(Event::ProposeBatch(vec![40, 50])), Err(Error::LogFull)));
    assert_eq!(node.durable_state().unwrap(), &before);
    assert_eq!((node.generation, node.request), counters);
    for request in requests {
        if let InFlight::Append { request, .. } = request {
            assert!(node.pending_append_reply(MemberId(2), before.term(), request).unwrap());
        }
    }
    cluster.deliver(output.messages, |_| true);
    assert_eq!(cluster.leader().state.commit_index, 4);
}

fn minority_behind() -> Cluster {
    let configuration = config(3);
    let limits = Limits { max_log_entries: 32, max_append_entries: 3 };
    let nodes = configuration.members().map(|id| {
        (id, Raft::new(id, configuration.clone(), limits).unwrap())
    }).collect();
    let mut cluster = Cluster { nodes };
    let output = step(cluster.leader(), Event::ElectionTimeout);
    cluster.deliver(output.messages, |member| member != MemberId(3));
    let output = step(cluster.leader(), Event::Propose(21));
    cluster.deliver(output.messages, |member| member != MemberId(3));
    assert_eq!(cluster.leader().state.commit_index, 2);
    assert!(cluster.nodes[&MemberId(3)].state.entries.is_empty());
    cluster
}

#[test]
fn catchup_can_commit_a_prefix_inside_one_local_batch() {
    let mut cluster = minority_behind();
    let _ = step(cluster.leader(), Event::ProposeBatch(vec![30, 31, 32]));
    // Peer 2 is now silent. Peer 3 first receives its old no-op request; the
    // subsequent bounded repair RPC spans index 2 plus only TWO batch entries.
    let retry = step(cluster.leader(), Event::Heartbeat);
    let first = retry.messages.into_iter().find(|message| message.to == MemberId(3)).unwrap();
    let response = step(cluster.nodes.get_mut(&MemberId(3)).unwrap(), Event::Receive(first));
    let next = step(cluster.leader(), Event::Receive(response.messages[0].clone()));
    let request = next.messages.into_iter().find(|message| message.to == MemberId(3)).unwrap();
    assert!(matches!(&request.message, Message::Append { prev_index: 1, entries, .. } if entries.len() == 3));
    let response = step(cluster.nodes.get_mut(&MemberId(3)).unwrap(), Event::Receive(request));
    let next = step(cluster.leader(), Event::Receive(response.messages[0].clone()));
    assert_eq!(next.committed.iter().map(|entry| (entry.index, entry.entry.command)).collect::<Vec<_>>(),
        [(3, Some(30)), (4, Some(31))]);
    assert_eq!(cluster.leader().state.commit_index, 4);
    cluster.deliver(next.messages, |member| member != MemberId(2));
    assert_eq!(cluster.leader().state.commit_index, 5);
}

#[test]
fn batches_preserve_the_snapshot_lane_of_a_lagging_member() {
    let mut cluster = minority_behind();
    let cut = SnapshotCut::from_authenticated_parts(&config(3), [3; 32], [4; 32], [5; 32], 2, 1).unwrap();
    step(cluster.leader(), Event::Compact(cut));
    let output = step(cluster.leader(), Event::ProposeBatch(vec![30, 31]));
    let snapshot = output.messages.iter().find(|message| message.to == MemberId(3)).unwrap();
    let Message::InstallSnapshot { request, .. } = &snapshot.message else { panic!("lagging peer needs the snapshot") };
    let expected = *request;
    let later = step(cluster.leader(), Event::ProposeBatch(vec![40, 41]));
    assert!(!later.messages.iter().any(|message| message.to == MemberId(3)));
    assert!(matches!(cluster.leader().progress[&MemberId(3)].in_flight.front(),
        Some(InFlight::Snapshot { request, .. }) if *request == expected));
    cluster.deliver(output.messages, |member| member != MemberId(3));
    assert_eq!(cluster.leader().state.commit_index, 6);
    assert_eq!(cluster.nodes[&MemberId(3)].state.commit_index, 0);
}
