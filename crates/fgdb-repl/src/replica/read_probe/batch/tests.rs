//! Real kernel/replica transitions; publication is an explicit in-memory model.
//! These tests do not certify disk durability, transport or query authorization.

use super::*;
use crate::replica::ReadResolution;
use fgdb_order::{Configuration, Domain, Envelope, Limits, MemberId, PersistentState};
use std::collections::{BTreeMap, VecDeque};
use std::future::{Future, ready};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

fn immediate<F: Future>(future: F) -> F::Output {
    match pin!(future)
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("memory publication unexpectedly suspended"),
    }
}

#[derive(Default)]
struct Publisher {
    states: Vec<PersistentState<u64>>,
}

impl RaftPublisher<u64> for Publisher {
    type Error = &'static str;
    fn publish(
        &mut self,
        state: &PersistentState<u64>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.states.push(state.clone());
        ready(Ok(()))
    }
}

struct Node {
    replica: Replica<u64>,
    publisher: Publisher,
}

impl Node {
    fn step(&mut self, event: Event<u64>) -> ReplicaOutput<u64> {
        immediate(self.replica.step(&mut self.publisher, event)).unwrap()
    }
    fn batch(&mut self, count: usize) -> (Vec<ReadIndexId>, ReplicaOutput<u64>) {
        immediate(self.replica.read_index_batch(&mut self.publisher, count)).unwrap()
    }
}

type Cluster = BTreeMap<MemberId, Node>;

fn stable(voters: u128) -> Configuration {
    Configuration::stable(Domain([41; 32]), [42; 32], (1..=voters).map(MemberId), []).unwrap()
}

fn cluster(configuration: Configuration) -> Cluster {
    configuration
        .voters()
        .iter()
        .map(|id| {
            (
                *id,
                Node {
                    replica: Replica::new(*id, configuration.clone(), Limits::default(), 8)
                        .unwrap(),
                    publisher: Publisher::default(),
                },
            )
        })
        .collect()
}

fn leader(nodes: &mut Cluster) -> &mut Node {
    nodes.get_mut(&MemberId(1)).unwrap()
}

fn pump(nodes: &mut Cluster, messages: Vec<Envelope<u64>>) -> Vec<ReadResolution> {
    let mut queue: VecDeque<_> = messages.into();
    let mut reads = Vec::new();
    let mut count = 0;
    while let Some(message) = queue.pop_front() {
        count += 1;
        assert!(count < 4096, "network must quiesce");
        let output = nodes
            .get_mut(&message.to)
            .unwrap()
            .step(Event::Receive(message));
        queue.extend(output.consensus.messages);
        reads.extend(output.reads);
    }
    reads
}

fn elect(nodes: &mut Cluster) {
    let output = leader(nodes).step(Event::ElectionTimeout);
    assert!(pump(nodes, output.consensus.messages).is_empty());
    let output = leader(nodes).step(Event::Heartbeat);
    assert!(pump(nodes, output.consensus.messages).is_empty());
    assert_eq!(leader(nodes).replica.role(), Ok(Role::Leader));
}

#[test]
fn one_heartbeat_output_confirms_all_original_requests_without_log_writes() {
    let mut nodes = cluster(stable(3));
    elect(&mut nodes);
    let node = leader(&mut nodes);
    let before = node.replica.durable_state().unwrap().clone();
    let publications = node.publisher.states.len();
    let (ids, output) = node.batch(8);
    assert_eq!(ids.len(), 8);
    assert_eq!(node.replica.pending_reads(), 8);
    assert!(output.reads.is_empty());
    assert_eq!(output.consensus.messages.len(), 2); // one per peer, not per read
    let results = pump(&mut nodes, output.consensus.messages);
    assert_eq!(results.len(), ids.len());
    for (result, id) in results.iter().zip(&ids) {
        let ReadResolution::Ready(result) = result else {
            panic!("expected quorum");
        };
        assert_eq!(result.id(), id);
        assert_eq!(result.index(), before.commit_index());
    }
    let node = leader(&mut nodes);
    assert_eq!(node.replica.pending_reads(), 0);
    assert_eq!(node.replica.durable_state().unwrap(), &before);
    assert_eq!(node.publisher.states.len(), publications);
    for id in &ids {
        assert!(node.replica.read_index_probes(id).is_err());
    }
}

#[test]
fn singleton_batch_consumes_slots_and_releases_each_read_once() {
    let mut nodes = cluster(stable(1));
    elect(&mut nodes);
    let node = leader(&mut nodes);
    let (ids, output) = node.batch(8);
    assert!(output.consensus.messages.is_empty());
    assert_eq!(output.reads.len(), 8);
    assert_eq!(node.replica.pending_reads(), 0);
    for (resolution, id) in output.reads.into_iter().zip(&ids) {
        let ReadResolution::Ready(ready) = resolution else {
            panic!("expected ready");
        };
        assert_eq!(ready.id(), id);
        assert!(!node.replica.cancel_read(id));
    }
    assert!(node.step(Event::Heartbeat).reads.is_empty());
}

#[test]
fn cancellation_retires_only_selected_requests_and_keeps_admission_order() {
    let mut nodes = cluster(stable(3));
    elect(&mut nodes);
    let (ids, output) = leader(&mut nodes).batch(4);
    assert!(leader(&mut nodes).replica.cancel_read(&ids[1]));
    assert!(leader(&mut nodes).replica.cancel_read(&ids[3]));
    let results = pump(&mut nodes, output.consensus.messages);
    assert_eq!(results.len(), 2);
    for (result, expected) in results.iter().zip([&ids[0], &ids[2]]) {
        let ReadResolution::Ready(ready) = result else {
            panic!("expected ready");
        };
        assert_eq!(ready.id(), expected);
    }
    assert_eq!(leader(&mut nodes).replica.pending_reads(), 0);
}

#[test]
fn whole_batch_capacity_and_counter_refusals_leave_existing_reads_intact() {
    let mut nodes = cluster(stable(3));
    elect(&mut nodes);
    let node = leader(&mut nodes);
    let (old, _) = node.batch(2);
    let next = node.replica.next_read;
    for count in [7, usize::MAX] {
        assert!(matches!(
            immediate(node.replica.read_index_batch(&mut node.publisher, count)),
            Err(ReplicaError::ReadBackpressure)
        ));
        assert_eq!(node.replica.next_read, next);
        assert_eq!(node.replica.pending_reads(), 2);
    }
    assert!(matches!(
        immediate(node.replica.read_index_batch(&mut node.publisher, 0)),
        Err(ReplicaError::Raft(RaftError::InvalidLimits))
    ));
    assert_eq!(node.replica.next_read, next);
    node.replica.next_read = u64::MAX - 1;
    assert!(matches!(
        immediate(node.replica.read_index_batch(&mut node.publisher, 2)),
        Err(ReplicaError::ReadCounterExhausted)
    ));
    assert_eq!(node.replica.next_read, u64::MAX - 1);
    for id in &old {
        assert!(node.replica.read_index_probes(id).is_ok());
    }
}

#[test]
fn typed_probe_nonces_remain_independent_and_later_writes_cannot_raise_the_floor() {
    let mut nodes = cluster(stable(3));
    elect(&mut nodes);
    let (ids, _) = leader(&mut nodes).batch(2);
    let floor = leader(&mut nodes)
        .replica
        .durable_state()
        .unwrap()
        .commit_index();
    let probes: Vec<_> = ids
        .iter()
        .map(|id| {
            leader(&mut nodes)
                .replica
                .read_index_probes(id)
                .unwrap()
                .into_iter()
                .find(|probe| probe.to == MemberId(2))
                .unwrap()
        })
        .collect();
    assert_ne!(probes[0].round, probes[1].round);
    let first = nodes[&MemberId(2)]
        .replica
        .acknowledge_read_index(probes[0])
        .unwrap();
    assert!(matches!(
        leader(&mut nodes)
            .replica
            .confirm_read_index(&ids[1], first),
        Err(ReadIndexError::WrongRound)
    ));
    assert_eq!(leader(&mut nodes).replica.pending_reads(), 2);
    // A later data quorum may complete both through the ordinary observer,
    // but it must return their earlier floor, never the later commit index.
    let output = leader(&mut nodes).step(Event::Propose(99));
    let mut results = pump(&mut nodes, output.consensus.messages);
    let output = leader(&mut nodes).step(Event::Heartbeat);
    results.extend(pump(&mut nodes, output.consensus.messages));
    assert_eq!(results.len(), 2);
    for result in results {
        let ReadResolution::Ready(ready) = result else {
            panic!("expected ready");
        };
        assert_eq!(ready.index(), floor);
    }
    assert!(
        leader(&mut nodes)
            .replica
            .durable_state()
            .unwrap()
            .commit_index()
            > floor
    );
}

#[test]
fn joint_batches_require_both_majorities_for_every_request() {
    let config = Configuration::joint(
        Domain([41; 32]),
        [42; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [MemberId(3), MemberId(4), MemberId(5)],
        [],
    )
    .unwrap();
    let mut nodes = cluster(config);
    elect(&mut nodes);
    let (ids, _) = leader(&mut nodes).batch(3);
    for id in &ids {
        for (voter, completes) in [(2, false), (4, false), (3, true)] {
            let probe = leader(&mut nodes)
                .replica
                .read_index_probes(id)
                .unwrap()
                .into_iter()
                .find(|probe| probe.to == MemberId(voter))
                .unwrap();
            let ack = nodes[&MemberId(voter)]
                .replica
                .acknowledge_read_index(probe)
                .unwrap();
            let ready = leader(&mut nodes)
                .replica
                .confirm_read_index(id, ack)
                .unwrap();
            assert_eq!(ready.is_some(), completes);
            if let Some(ready) = ready {
                assert_eq!(ready.id(), id);
            }
        }
    }
    assert_eq!(leader(&mut nodes).replica.pending_reads(), 0);
}

#[test]
fn dropped_batch_reservation_removes_its_entire_range_but_not_earlier_reads() {
    let mut nodes = cluster(stable(3));
    elect(&mut nodes);
    let node = leader(&mut nodes);
    let (prior, _) = node.batch(1);
    let (batch, _) = node.batch(3);
    let first = batch[0].serial;
    let last = batch[2].serial;
    // Directly exercise the guard's unwind/drop boundary; the normal memory
    // heartbeat is disk-free and does not suspend in this fixture.
    drop(BatchAdmission {
        replica: &mut node.replica,
        first,
        last,
        armed: true,
    });
    assert_eq!(node.replica.pending_reads(), 1);
    assert!(node.replica.read_index_probes(&prior[0]).is_ok());
    for id in &batch {
        assert!(node.replica.read_index_probes(id).is_err());
    }
    let (fresh, _) = node.batch(1);
    assert!(fresh[0].serial > last);
}

#[test]
fn handoff_and_recovery_refuse_batches_without_leaking_read_slots() {
    let mut nodes = cluster(stable(3));
    elect(&mut nodes);
    let node = leader(&mut nodes);
    let _ = node.step(Event::TransferLeadership(MemberId(2)));
    assert!(matches!(
        immediate(node.replica.read_index_batch(&mut node.publisher, 3)),
        Err(ReplicaError::Raft(RaftError::LeadershipTransferInProgress))
    ));
    assert_eq!(node.replica.pending_reads(), 0);
    let state = node.replica.durable_state().unwrap().clone();
    let mut recovered = Replica::<u64>::recover(MemberId(1), state, Limits::default(), 8).unwrap();
    assert!(matches!(
        immediate(recovered.read_index_batch(&mut node.publisher, 3)),
        Err(ReplicaError::Raft(RaftError::NotLeader))
    ));
    assert_eq!(recovered.pending_reads(), 0);
}
