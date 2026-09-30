use fgdb_order::{
    Configuration, Domain, Envelope, Error, Event, Limits, MemberId, Output, Raft,
    ReadIndexError, ReadIndexRound, Role,
};
use std::collections::{BTreeMap, VecDeque};

const LEADER: MemberId = MemberId(1);

fn configuration() -> Configuration {
    Configuration::stable(
        Domain([1; 32]),
        [2; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [],
    )
    .unwrap()
}

fn publish(node: &mut Raft<u64>, event: Event<u64>) -> Output<u64> {
    let id = node.step(event).unwrap().id();
    node.persisted(id).unwrap()
}

struct Cluster {
    nodes: BTreeMap<MemberId, Raft<u64>>,
    messages: VecDeque<Envelope<u64>>,
}

impl Cluster {
    fn new() -> Self {
        let config = configuration();
        let nodes = config
            .voters()
            .iter()
            .map(|id| (*id, Raft::new(*id, config.clone(), Limits::default()).unwrap()))
            .collect();
        Self { nodes, messages: VecDeque::new() }
    }

    fn node(&self, id: MemberId) -> &Raft<u64> {
        self.nodes.get(&id).unwrap()
    }

    fn node_mut(&mut self, id: MemberId) -> &mut Raft<u64> {
        self.nodes.get_mut(&id).unwrap()
    }

    fn step(&mut self, id: MemberId, event: Event<u64>) {
        let output = publish(self.node_mut(id), event);
        self.messages.extend(output.messages);
    }

    fn drain(&mut self) {
        let mut delivered = 0;
        while let Some(message) = self.messages.pop_front() {
            delivered += 1;
            assert!(delivered < 10_000);
            self.step(message.to, Event::Receive(message));
        }
    }

    fn elect(&mut self) {
        self.step(LEADER, Event::ElectionTimeout);
        self.drain();
        assert_eq!(self.node(LEADER).role().unwrap(), Role::Leader);
    }

    fn ack_from(&self, round: &ReadIndexRound, voter: MemberId) -> fgdb_order::ReadIndexAck {
        let probe = round
            .probes(self.node(LEADER))
            .unwrap()
            .into_iter()
            .find(|probe| probe.to == voter)
            .unwrap();
        self.node(voter).acknowledge_read_index(probe).unwrap()
    }
}

#[test]
fn fresh_voter_quorum_is_required_for_read_index() {
    let mut cluster = Cluster::new();
    cluster.elect();

    let mut round = cluster.node_mut(LEADER).begin_read_index().unwrap();
    assert!(round.try_complete(cluster.node(LEADER)).unwrap().is_none());

    let ack = cluster.ack_from(&round, MemberId(2));
    round.acknowledge(cluster.node(LEADER), ack).unwrap();

    let barrier = round.try_complete(cluster.node(LEADER)).unwrap().unwrap();
    assert_eq!(
        barrier.index(),
        cluster.node(LEADER).durable_state().unwrap().commit_index()
    );
}

#[test]
fn acknowledgements_are_bound_to_one_round() {
    let mut cluster = Cluster::new();
    cluster.elect();

    let first = cluster.node_mut(LEADER).begin_read_index().unwrap();
    let stale = cluster.ack_from(&first, MemberId(2));
    drop(first);

    let mut second = cluster.node_mut(LEADER).begin_read_index().unwrap();
    assert_eq!(
        second.acknowledge(cluster.node(LEADER), stale),
        Err(ReadIndexError::WrongRound)
    );
    assert!(second.try_complete(cluster.node(LEADER)).unwrap().is_none());
}

#[test]
fn ordinary_raft_success_does_not_complete_an_unacknowledged_read() {
    let mut cluster = Cluster::new();
    cluster.elect();

    let mut round = cluster.node_mut(LEADER).begin_read_index().unwrap();
    cluster.step(LEADER, Event::Propose(42));
    cluster.drain();
    cluster.step(LEADER, Event::Heartbeat);
    cluster.drain();

    assert!(round.try_complete(cluster.node(LEADER)).unwrap().is_none());
    let ack = cluster.ack_from(&round, MemberId(2));
    round.acknowledge(cluster.node(LEADER), ack).unwrap();
    assert!(round.try_complete(cluster.node(LEADER)).unwrap().is_some());
}

#[test]
fn leadership_loss_invalidates_completed_barrier() {
    let mut cluster = Cluster::new();
    cluster.elect();

    let mut round = cluster.node_mut(LEADER).begin_read_index().unwrap();
    let ack = cluster.ack_from(&round, MemberId(2));
    round.acknowledge(cluster.node(LEADER), ack).unwrap();
    let barrier = round.try_complete(cluster.node(LEADER)).unwrap().unwrap();

    cluster.step(LEADER, Event::LivenessTimeout);
    cluster.messages.clear();
    cluster.step(LEADER, Event::LivenessTimeout);
    cluster.messages.clear();

    assert_eq!(cluster.node(LEADER).role().unwrap(), Role::Follower);
    assert_eq!(
        barrier.validate(cluster.node(LEADER)),
        Err(ReadIndexError::Raft(Error::NotLeader))
    );
}
