//! Stable/joint sequencing histories, not authorization to change membership.
//! The disk map changes before messages escape. These fixtures do not replace
//! Chronicle's root publisher or the configuration/payload-floor verifier.
use fgdb_order::{
    Configuration, Domain, Entry, Envelope, Error, Event, Limits, MemberId,
    Message, Output, PersistentState, Raft, Role, SnapshotCut,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn joint(old: &[u128], new: &[u128], learners: &[u128]) -> Result<Configuration, Error> {
    Configuration::joint(Domain([1; 32]), [2; 32],
        old.iter().copied().map(MemberId), new.iter().copied().map(MemberId),
        learners.iter().copied().map(MemberId))
}

fn publish(node: &mut Raft<u64>, event: Event<u64>) -> Output<u64> {
    let id = node.step(event).unwrap().id();
    node.persisted(id).unwrap()
}

fn vote(from: u128, to: u128, term: u64) -> Event<u64> {
    Event::Receive(Envelope {
        domain: Domain([1; 32]), configuration: [2; 32],
        from: MemberId(from), to: MemberId(to), message: Message::Vote { term, granted: true },
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
    fn new(configuration: Configuration, entries: Vec<Entry<u64>>, term: u64, limits: Limits) -> Self {
        let mut nodes = BTreeMap::new();
        let mut disk = BTreeMap::new();
        for member in configuration.voters().union(configuration.learners()) {
            let state = PersistentState::from_authenticated_parts(configuration.clone(), term, None, 0, entries.clone());
            let node = Raft::recover(*member, state.clone(), limits).unwrap();
            nodes.insert(*member, node);
            disk.insert(*member, state);
        }
        Self { nodes, disk, queue: VecDeque::new(), isolated: BTreeSet::new(), limits }
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
        assert!(output.snapshot_transfers.is_empty());
        for applied in &output.committed {
            assert!(applied.index <= self.disk[&member].commit_index());
            assert_eq!(&applied.entry, &self.disk[&member].entries()[applied.index as usize - 1]);
        }
        self.queue.extend(output.messages);
        for left in self.disk.values() {
            for right in self.disk.values() {
                let common = left.commit_index().min(right.commit_index()) as usize;
                assert_eq!(&left.entries()[..common], &right.entries()[..common]);
            }
        }
    }

    fn drain(&mut self) {
        let mut remaining = 10_000;
        while let Some(message) = self.queue.pop_front() {
            assert!(remaining > 0, "joint history failed to quiesce");
            remaining -= 1;
            if !self.isolated.contains(&message.from) && !self.isolated.contains(&message.to) {
                self.step(message.to, Event::Receive(message));
            }
        }
    }

    fn elect(&mut self, member: u128) {
        self.step(MemberId(member), Event::ElectionTimeout);
        self.drain();
        assert_eq!(self.nodes[&MemberId(member)].role().unwrap(), Role::Leader);
    }

    fn heartbeat(&mut self, member: u128) {
        self.step(MemberId(member), Event::Heartbeat);
        self.drain();
    }

    fn restart(&mut self, member: u128) {
        let id = MemberId(member);
        self.nodes.insert(id, Raft::recover(id, self.disk[&id].clone(), self.limits).unwrap());
    }
}

#[test]
fn joint_admission_rejects_empty_duplicate_zero_and_learner_overlap() {
    for (old, new, learners) in [
        (vec![], vec![1], vec![]), (vec![1], vec![], vec![]),
        (vec![0], vec![1], vec![]), (vec![1], vec![0], vec![]),
        (vec![1, 1], vec![2], vec![]), (vec![1], vec![2, 2], vec![]),
        (vec![1], vec![2], vec![1]), (vec![1], vec![2], vec![2]),
        (vec![1], vec![2], vec![3, 3]), (vec![1], vec![2], vec![0]),
    ] {
        assert_eq!(joint(&old, &new, &learners), Err(Error::InvalidConfiguration));
    }
    let configuration = joint(&[1, 2, 3], &[3, 4, 5], &[6]).unwrap();
    assert_eq!(configuration.voters().len(), 5);
    assert_eq!(configuration.learners(), &BTreeSet::from([MemberId(6)]));
    let (old, new) = configuration.joint_voters().unwrap();
    assert_eq!(old, &BTreeSet::from([MemberId(1), MemberId(2), MemberId(3)]));
    assert_eq!(new, &BTreeSet::from([MemberId(3), MemberId(4), MemberId(5)]));
}

#[test]
fn membership_bound_counts_unique_union_not_twice_the_overlap() {
    let members: Vec<_> = (1..=1024).collect();
    assert_eq!(joint(&members, &members, &[]).unwrap().voters().len(), 1024);
    assert_eq!(joint(&members, &[1025], &[]), Err(Error::InvalidConfiguration));
    assert_eq!(joint(&members, &[1], &[1025]), Err(Error::InvalidConfiguration));
    assert_eq!(joint(&[1], &members, &[1025]), Err(Error::InvalidConfiguration));
}

#[test]
fn elections_require_both_groups_for_old_new_and_shared_candidates() {
    let configuration = joint(&[1, 2, 3], &[3, 4, 5], &[]).unwrap();
    for (candidate, first, second, decisive) in [(1, 2, 4, 5), (4, 5, 1, 2)] {
        let mut node = Raft::<u64>::new(MemberId(candidate), configuration.clone(), Limits::default()).unwrap();
        publish(&mut node, Event::ElectionTimeout);
        publish(&mut node, vote(first, candidate, 1));
        for _ in 0..4 {
            publish(&mut node, vote(second, candidate, 1));
            assert_eq!(node.role().unwrap(), Role::Candidate, "a union majority is not a joint quorum");
        }
        publish(&mut node, vote(decisive, candidate, 1));
        assert_eq!(node.role().unwrap(), Role::Leader);
        assert_eq!(node.durable_state().unwrap().configuration(), &configuration);
    }
    let mut node = Raft::<u64>::new(MemberId(3), configuration, Limits::default()).unwrap();
    publish(&mut node, Event::ElectionTimeout);
    publish(&mut node, vote(1, 3, 1));
    assert_eq!(node.role().unwrap(), Role::Candidate);
    publish(&mut node, vote(4, 3, 1));
    assert_eq!(node.role().unwrap(), Role::Leader, "overlapping voter counts once in each group");
}

#[test]
fn neither_one_sided_quorum_nor_learners_can_commit_during_partition() {
    for (leader, blocked, healing) in [(1, [3, 5], 5), (4, [2, 3], 2)] {
        let configuration = joint(&[1, 2, 3], &[3, 4, 5], &[6, 7]).unwrap();
        let mut cluster = Cluster::new(configuration, vec![], 0, Limits::default());
        cluster.elect(leader);
        cluster.isolated.extend(blocked.map(MemberId));
        cluster.step(MemberId(leader), Event::Propose(77));
        cluster.drain();
        assert_eq!(cluster.disk[&MemberId(leader)].commit_index(), 1);
        assert_eq!(cluster.disk[&MemberId(6)].entries()[1].command, Some(77));
        assert_eq!(cluster.disk[&MemberId(7)].commit_index(), 1);
        cluster.restart(6);
        cluster.heartbeat(leader);
        assert_eq!(cluster.disk[&MemberId(leader)].commit_index(), 1);
        cluster.isolated.remove(&MemberId(healing));
        cluster.heartbeat(leader);
        for (member, state) in &cluster.disk {
            if !cluster.isolated.contains(member) {
                assert_eq!(state.commit_index(), 2);
                assert_eq!(state.entries()[1].command, Some(77));
            }
        }
    }
}

#[test]
fn disjoint_singleton_group_is_not_outvoted_by_the_larger_group() {
    let configuration = joint(&[1, 2, 3], &[4], &[]).unwrap();
    let mut cluster = Cluster::new(configuration, vec![], 0, Limits::default());
    cluster.elect(1);
    cluster.isolated.insert(MemberId(4));
    cluster.step(MemberId(1), Event::Propose(88));
    cluster.drain();
    assert_eq!(cluster.disk[&MemberId(1)].commit_index(), 1);
    cluster.isolated.clear();
    cluster.heartbeat(1);
    for state in cluster.disk.values() {
        assert_eq!(state.commit_index(), 2);
        assert_eq!(state.entries()[1].command, Some(88));
    }
}

#[test]
fn satisfying_both_groups_still_cannot_commit_an_old_term_without_a_barrier() {
    let configuration = joint(&[1, 2, 3], &[3, 4, 5], &[]).unwrap();
    let mut cluster = Cluster::new(configuration, vec![Entry { term: 1, command: Some(99) }], 1,
        Limits { max_log_entries: 1, max_append_entries: 1 });
    cluster.elect(1);
    cluster.heartbeat(1);
    for state in cluster.disk.values() {
        assert_eq!(state.term(), 2);
        assert_eq!(state.commit_index(), 0, "old-term quorum counting is not a commit barrier");
        assert_eq!(state.entries().len(), 1);
    }
}

#[test]
fn snapshot_recovery_keeps_joint_voter_groups_and_configuration_identity() {
    let configuration = joint(&[1, 2, 3], &[3, 4, 5], &[]).unwrap();
    let snapshot = SnapshotCut::from_authenticated_parts(&configuration, [3; 32], [4; 32], [5; 32], 100, 3).unwrap();
    let state = PersistentState::<u64>::from_authenticated_snapshot(configuration.clone(), 3,
        Some(MemberId(4)), 100, snapshot.clone(), vec![]);
    let mut node = Raft::recover(MemberId(1), state, Limits::default()).unwrap();
    publish(&mut node, Event::ElectionTimeout);
    publish(&mut node, vote(2, 1, 4));
    publish(&mut node, vote(4, 1, 4));
    assert_eq!(node.role().unwrap(), Role::Candidate);
    publish(&mut node, vote(5, 1, 4));
    assert_eq!(node.role().unwrap(), Role::Leader);
    let saved = node.durable_state().unwrap().clone();
    assert_eq!(saved.configuration(), &configuration);
    assert_eq!(saved.snapshot(), Some(&snapshot));
    assert_eq!(saved.commit_index(), 100);
    assert_eq!(saved.entries(), &[Entry { term: 4, command: None }]);
    let event = Event::Receive(Envelope {
        domain: Domain([1; 32]), configuration: [9; 32], from: MemberId(4), to: MemberId(1),
        message: Message::RequestVote { term: 99, last_index: 100, last_term: 3 },
    });
    assert_eq!(node.step(event).err(), Some(Error::WrongConfiguration));
    assert_eq!(node.durable_state().unwrap(), &saved);
}

#[test]
fn identical_single_member_groups_preserve_the_one_member_commit_path() {
    let configuration = joint(&[1], &[1], &[]).unwrap();
    let mut node = Raft::new(MemberId(1), configuration, Limits::default()).unwrap();
    let campaign = publish(&mut node, Event::ElectionTimeout);
    assert_eq!(campaign.role, Role::Leader);
    assert_eq!(campaign.committed[0].entry.command, None);
    let output = publish(&mut node, Event::Propose(123));
    assert_eq!(output.committed[0].index, 2);
    assert_eq!(output.committed[0].entry.command, Some(123));
    assert!(output.messages.is_empty());
}
