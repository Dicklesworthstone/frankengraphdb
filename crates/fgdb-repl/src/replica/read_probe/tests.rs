//! The real kernel/replica with an explicitly modeled root publisher. These
//! laws do not claim disk, authenticated transport or application visibility.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use fgdb_order::{
    Configuration, Domain, Envelope, Error as RaftError, Event, Limits, MemberId, Message,
    PersistentState, ReadIndexAck, ReadIndexError, ReadIndexProbe, Role,
};

use crate::driver::RaftPublisher;
use crate::replica::{ReadIndexId, Replica, ReplicaError, ReplicaOutput};

const LEADER: MemberId = MemberId(1);

fn immediate<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("modeled root publication suspended"),
    }
}

#[derive(Default)]
struct Root {
    publications: usize,
    fail: bool,
}

impl RaftPublisher<u64> for Root {
    type Error = &'static str;

    fn publish(
        &mut self,
        _state: &PersistentState<u64>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.publications += 1;
        let fail = self.fail;
        async move {
            if fail {
                Err("uncertain publication")
            } else {
                Ok(())
            }
        }
    }
}

struct Node {
    replica: Replica<u64>,
    root: Root,
}

impl Node {
    fn step(&mut self, event: Event<u64>) -> ReplicaOutput<u64> {
        immediate(self.replica.step(&mut self.root, event)).unwrap()
    }

    fn read(&mut self) -> (ReadIndexId, ReplicaOutput<u64>) {
        immediate(self.replica.read_index(&mut self.root)).unwrap()
    }
}

type Cluster = BTreeMap<MemberId, Node>;

fn stable(count: u128, learners: &[u128]) -> Configuration {
    Configuration::stable(
        Domain([61; 32]),
        [62; 32],
        (1..=count).map(MemberId),
        learners.iter().copied().map(MemberId),
    )
    .unwrap()
}

fn leader(nodes: &mut Cluster) -> &mut Node {
    nodes.get_mut(&LEADER).unwrap()
}

fn pump(nodes: &mut Cluster, messages: Vec<Envelope<u64>>) {
    let mut queue: VecDeque<_> = messages.into();
    let mut delivered = 0;
    while let Some(message) = queue.pop_front() {
        delivered += 1;
        assert!(delivered < 20_000, "replication did not quiesce");
        let output = nodes
            .get_mut(&message.to)
            .unwrap()
            .step(Event::Receive(message));
        assert!(output.reads.is_empty(), "unexpected second read completion");
        queue.extend(output.consensus.messages);
    }
}

fn elected(configuration: Configuration) -> Cluster {
    let mut nodes: Cluster = configuration
        .voters()
        .union(configuration.learners())
        .map(|member| {
            (
                *member,
                Node {
                    replica: Replica::new(
                        *member,
                        configuration.clone(),
                        Limits {
                            max_log_entries: 128,
                            max_append_entries: 1,
                        },
                        8,
                    )
                    .unwrap(),
                    root: Root::default(),
                },
            )
        })
        .collect();
    let output = leader(&mut nodes).step(Event::ElectionTimeout);
    pump(&mut nodes, output.consensus.messages);
    let output = leader(&mut nodes).step(Event::Heartbeat);
    pump(&mut nodes, output.consensus.messages);
    assert_eq!(leader(&mut nodes).replica.role(), Ok(Role::Leader));
    nodes
}

fn probe(nodes: &Cluster, id: &ReadIndexId, voter: u128) -> ReadIndexProbe {
    nodes[&LEADER]
        .replica
        .read_index_probes(id)
        .unwrap()
        .into_iter()
        .find(|probe| probe.to == MemberId(voter))
        .unwrap()
}

fn ack(nodes: &Cluster, id: &ReadIndexId, voter: u128) -> ReadIndexAck {
    nodes[&MemberId(voter)]
        .replica
        .acknowledge_read_index(probe(nodes, id, voter))
        .unwrap()
}

#[test]
fn a_full_append_window_does_not_block_a_read_or_fake_a_data_ack() {
    let mut nodes = elected(stable(3, &[]));
    let stalled = leader(&mut nodes).step(Event::Propose(10));
    let before = leader(&mut nodes).replica.durable_state().unwrap().clone();
    let writes = leader(&mut nodes).root.publications;
    let (id, retry) = leader(&mut nodes).read();
    assert_eq!(retry.consensus.messages, stalled.consensus.messages);
    assert!(retry.reads.is_empty());
    let response = ack(&nodes, &id, 2);
    let ready = leader(&mut nodes)
        .replica
        .confirm_read_index(&id, response)
        .unwrap()
        .unwrap();
    assert_eq!(ready.id(), &id);
    assert_eq!(ready.index(), 1);
    assert_eq!(leader(&mut nodes).replica.durable_state().unwrap(), &before);
    assert_eq!(leader(&mut nodes).root.publications, writes);
    assert_eq!(leader(&mut nodes).replica.pending_reads(), 0);
    assert!(matches!(
        leader(&mut nodes).replica.confirm_read_index(&id, response),
        Err(ReadIndexError::ForeignRound)
    ));
    // The delayed real data acknowledgements may commit the write, but cannot
    // release the already-consumed read a second time.
    pump(&mut nodes, stalled.consensus.messages);
    assert_eq!(
        leader(&mut nodes)
            .replica
            .durable_state()
            .unwrap()
            .commit_index(),
        2
    );
}

#[test]
fn append_and_probe_votes_share_a_joint_quorum_without_double_counting() {
    let configuration = Configuration::joint(
        Domain([61; 32]),
        [62; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [MemberId(3), MemberId(4), MemberId(5)],
        [MemberId(6)],
    )
    .unwrap();
    let mut nodes = elected(configuration);
    let (id, output) = leader(&mut nodes).read();
    let append = output
        .consensus
        .messages
        .into_iter()
        .find(|message| message.to == MemberId(2))
        .unwrap();
    assert!(matches!(append.message, Message::Append { .. }));
    let response = nodes
        .get_mut(&MemberId(2))
        .unwrap()
        .step(Event::Receive(append));
    let output = leader(&mut nodes).step(Event::Receive(
        response.consensus.messages.into_iter().next().unwrap(),
    ));
    assert!(output.reads.is_empty());
    assert!(
        leader(&mut nodes)
            .replica
            .read_index_probes(&id)
            .unwrap()
            .iter()
            .all(|probe| probe.to != MemberId(2) && probe.to != MemberId(6))
    );
    let response = ack(&nodes, &id, 4);
    for _ in 0..4 {
        assert!(
            leader(&mut nodes)
                .replica
                .confirm_read_index(&id, response)
                .unwrap()
                .is_none()
        );
    }
    // {1,2,4} is a union majority, but has only one new-configuration vote.
    let response = ack(&nodes, &id, 5);
    let ready = leader(&mut nodes)
        .replica
        .confirm_read_index(&id, response)
        .unwrap()
        .unwrap();
    assert_eq!(ready.id(), &id);
    assert_eq!(ready.index(), 1);
}

#[test]
fn cancelled_rounds_and_invalid_bindings_cannot_pollute_the_next_read() {
    let mut nodes = elected(stable(3, &[4]));
    let (old, _) = leader(&mut nodes).read();
    let delayed = ack(&nodes, &old, 2);
    assert!(leader(&mut nodes).replica.cancel_read(&old));
    let (id, _) = leader(&mut nodes).read();
    assert!(matches!(
        leader(&mut nodes).replica.confirm_read_index(&id, delayed),
        Err(ReadIndexError::WrongRound)
    ));
    assert!(matches!(
        leader(&mut nodes).replica.confirm_read_index(&old, delayed),
        Err(ReadIndexError::ForeignRound)
    ));
    let good = ack(&nodes, &id, 2);
    let cases = [
        (
            ReadIndexAck {
                domain: Domain([9; 32]),
                ..good
            },
            ReadIndexError::Raft(RaftError::WrongDomain),
        ),
        (
            ReadIndexAck {
                configuration: [9; 32],
                ..good
            },
            ReadIndexError::Raft(RaftError::WrongConfiguration),
        ),
        (
            ReadIndexAck {
                from: MemberId(4),
                ..good
            },
            ReadIndexError::Raft(RaftError::NotVoter),
        ),
        (
            ReadIndexAck {
                to: MemberId(3),
                ..good
            },
            ReadIndexError::Raft(RaftError::WrongRecipient),
        ),
        (
            ReadIndexAck {
                term: good.term + 1,
                ..good
            },
            ReadIndexError::WrongTerm,
        ),
    ];
    for (bad, expected) in cases {
        let actual = leader(&mut nodes).replica.confirm_read_index(&id, bad);
        assert!(matches!(actual, Err(error) if error == expected));
        assert_eq!(
            leader(&mut nodes)
                .replica
                .read_index_probes(&id)
                .unwrap()
                .len(),
            2
        );
    }
    assert!(
        leader(&mut nodes)
            .replica
            .confirm_read_index(&id, good)
            .unwrap()
            .is_some()
    );
}

#[test]
fn later_commits_do_not_raise_the_probe_rounds_captured_floor() {
    let mut nodes = elected(stable(3, &[]));
    let write = leader(&mut nodes).step(Event::Propose(42));
    let (id, _) = leader(&mut nodes).read();
    let delayed = ack(&nodes, &id, 2);
    let before = leader(&mut nodes)
        .replica
        .durable_state()
        .unwrap()
        .commit_index();
    // This append was issued BEFORE read admission. Its delayed success can
    // commit the write, but is deliberately too old to confirm the read.
    let request = write
        .consensus
        .messages
        .into_iter()
        .find(|message| message.to == MemberId(2))
        .unwrap();
    let reply = nodes
        .get_mut(&MemberId(2))
        .unwrap()
        .step(Event::Receive(request));
    let output = leader(&mut nodes).step(Event::Receive(
        reply.consensus.messages.into_iter().next().unwrap(),
    ));
    assert!(output.reads.is_empty());
    assert_eq!(
        leader(&mut nodes)
            .replica
            .durable_state()
            .unwrap()
            .commit_index(),
        before + 1
    );
    let ready = leader(&mut nodes)
        .replica
        .confirm_read_index(&id, delayed)
        .unwrap()
        .unwrap();
    assert_eq!(ready.index(), before);
}

#[test]
fn publication_failure_fences_probes_and_acknowledgements() {
    let mut nodes = elected(stable(3, &[]));
    let (id, _) = leader(&mut nodes).read();
    let request = probe(&nodes, &id, 2);
    let response = ack(&nodes, &id, 2);
    let follower = nodes.get_mut(&MemberId(2)).unwrap();
    follower.root.fail = true;
    assert!(
        immediate(
            follower
                .replica
                .step(&mut follower.root, Event::ElectionTimeout)
        )
        .is_err()
    );
    assert_eq!(
        follower.replica.acknowledge_read_index(request),
        Err(ReadIndexError::Raft(RaftError::RecoveryRequired))
    );
    let node = leader(&mut nodes);
    node.root.fail = true;
    assert!(immediate(node.replica.step(&mut node.root, Event::Propose(9))).is_err());
    assert_eq!(
        node.replica.read_index_probes(&id),
        Err(ReadIndexError::Raft(RaftError::RecoveryRequired))
    );
    assert!(matches!(
        node.replica.confirm_read_index(&id, response),
        Err(ReadIndexError::Raft(RaftError::RecoveryRequired))
    ));
}

#[test]
fn recovery_and_leadership_transfer_cannot_reuse_or_admit_probe_rounds() {
    let mut nodes = elected(stable(3, &[]));
    let (id, _) = leader(&mut nodes).read();
    let response = ack(&nodes, &id, 2);
    let mut recovered = Replica::recover(
        LEADER,
        leader(&mut nodes).replica.durable_state().unwrap().clone(),
        Limits::default(),
        8,
    )
    .unwrap();
    assert_eq!(
        recovered.read_index_probes(&id),
        Err(ReadIndexError::ForeignRound)
    );
    assert!(matches!(
        recovered.confirm_read_index(&id, response),
        Err(ReadIndexError::ForeignRound)
    ));
    leader(&mut nodes).step(Event::TransferLeadership(MemberId(2)));
    let node = leader(&mut nodes);
    assert!(matches!(
        immediate(node.replica.read_index(&mut node.root)),
        Err(ReplicaError::Raft(RaftError::LeadershipTransferInProgress))
    ));
    assert_eq!(
        node.replica.read_index_probes(&id),
        Err(ReadIndexError::Raft(
            RaftError::LeadershipTransferInProgress
        ))
    );
    assert_eq!(node.replica.pending_reads(), 1);
}
