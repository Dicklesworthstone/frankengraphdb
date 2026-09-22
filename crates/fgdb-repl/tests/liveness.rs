#![cfg(not(target_arch = "wasm32"))]

//! Liveness/ReadIndex composition through the real owning Replica and driver.
//! The publisher models root atomicity only; it is not disk-durability evidence.
use std::collections::VecDeque;
use std::future::{Future, pending};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use fgdb_order::{
    Configuration, Domain, Envelope, Error, Event, Limits, MemberId, Message, PersistentState, Role,
};
use fgdb_repl::driver::{RaftPublisher, SequenceError};
use fgdb_repl::replica::{ReadIndexId, ReadResolution, Replica, ReplicaError, ReplicaOutput};

struct NoopWake;
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn immediate<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    match pin!(future).as_mut().poll(&mut Context::from_waker(&waker)) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("publication unexpectedly suspended"),
    }
}

#[derive(Default)]
struct Publisher {
    state: Option<PersistentState<u64>>,
    writes: usize,
    fail: bool,
    suspend: bool,
}
impl RaftPublisher<u64> for Publisher {
    type Error = &'static str;
    fn publish(
        &mut self,
        state: &PersistentState<u64>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.writes += 1;
        self.state = Some(state.clone());
        let (fail, suspend) = (self.fail, self.suspend);
        async move {
            if suspend {
                pending::<()>().await;
            }
            if fail {
                Err("ambiguous root publication")
            } else {
                Ok(())
            }
        }
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
    fn read(&mut self) -> (ReadIndexId, ReplicaOutput<u64>) {
        immediate(self.replica.read_index(&mut self.publisher)).unwrap()
    }
}

fn configuration() -> Configuration {
    Configuration::stable(Domain([1; 32]), [2; 32], [1, 2, 3].map(MemberId), []).unwrap()
}
fn nodes() -> Vec<Node> {
    (1..=3)
        .map(|id| Node {
            replica: Replica::new(MemberId(id), configuration(), Limits::default(), 8).unwrap(),
            publisher: Publisher::default(),
        })
        .collect()
}
fn pump(
    nodes: &mut [Node],
    messages: Vec<Envelope<u64>>,
    reachable: &[u128],
) -> Vec<ReadResolution> {
    let mut queue: VecDeque<_> = messages.into();
    let mut resolutions = Vec::new();
    let mut delivered = 0;
    while let Some(message) = queue.pop_front() {
        delivered += 1;
        assert!(delivered < 4096);
        if reachable.contains(&message.from.0) && reachable.contains(&message.to.0) {
            let node = nodes
                .iter_mut()
                .find(|node| node.replica.id() == message.to)
                .unwrap();
            let output = node.step(Event::Receive(message));
            queue.extend(output.consensus.messages);
            resolutions.extend(output.reads);
        }
    }
    resolutions
}
fn elect(nodes: &mut [Node]) {
    let output = nodes[0].step(Event::LivenessTimeout);
    assert!(pump(nodes, output.consensus.messages, &[1, 2, 3]).is_empty());
    let heartbeat = nodes[0].step(Event::Heartbeat);
    assert!(pump(nodes, heartbeat.consensus.messages, &[1, 2, 3]).is_empty());
    assert_eq!(nodes[0].replica.role(), Ok(Role::Leader));
}

#[test]
fn check_quorum_replies_never_finish_read_index_but_real_append_confirmation_does() {
    let mut nodes = nodes();
    elect(&mut nodes);
    let (id, read) = nodes[0].read();
    assert!(read.reads.is_empty());
    let writes = nodes[0].publisher.writes;
    let check = nodes[0].step(Event::LivenessTimeout);
    assert!(
        check
            .consensus
            .messages
            .iter()
            .all(|m| matches!(m.message, Message::QuorumProbe { .. }))
    );
    assert!(pump(&mut nodes, check.consensus.messages, &[1, 2, 3]).is_empty());
    assert_eq!(nodes[0].replica.pending_reads(), 1);
    assert_eq!(nodes[0].publisher.writes, writes);
    let resolutions = pump(&mut nodes, read.consensus.messages, &[1, 2, 3]);
    assert_eq!(resolutions.len(), 1);
    assert!(matches!(&resolutions[0], ReadResolution::Ready(ready) if ready.id() == &id));
    assert_eq!(nodes[0].replica.pending_reads(), 0);
}

#[test]
fn quorum_loss_cancels_pending_reads_and_delayed_replies_cannot_revive_them() {
    let mut nodes = nodes();
    elect(&mut nodes);
    let check = nodes[0].step(Event::LivenessTimeout);
    // Start the current check interval but withhold every reply.
    let probe = check
        .consensus
        .messages
        .into_iter()
        .find(|m| m.to == MemberId(2))
        .unwrap();
    let delayed_probe = nodes[1]
        .step(Event::Receive(probe))
        .consensus
        .messages
        .remove(0);
    let (id, output) = nodes[0].read();
    let request = output
        .consensus
        .messages
        .into_iter()
        .find(|m| m.to == MemberId(2) && matches!(m.message, Message::Append { .. }))
        .unwrap();
    let delayed_append = nodes[1]
        .step(Event::Receive(request))
        .consensus
        .messages
        .remove(0);
    let before = nodes[0].replica.durable_state().unwrap().clone();
    let output = nodes[0].step(Event::LivenessTimeout);
    assert_eq!(output.consensus.role, Role::Follower);
    assert!(matches!(&output.reads[..], [ReadResolution::LeadershipLost(lost)] if lost == &id));
    assert_eq!(nodes[0].replica.pending_reads(), 0);
    assert_eq!(nodes[0].replica.durable_state().unwrap(), &before);
    assert!(
        nodes[0]
            .step(Event::Receive(delayed_probe))
            .reads
            .is_empty()
    );
    assert!(
        nodes[0]
            .step(Event::Receive(delayed_append))
            .reads
            .is_empty()
    );
    assert!(matches!(
        immediate(nodes[0].replica.read_index(&mut Publisher::default())),
        Err(ReplicaError::Raft(Error::NotLeader))
    ));
    assert!(!nodes[0].replica.cancel_read(&id));
}

#[test]
fn preview_winner_cannot_send_actual_votes_after_failed_or_cancelled_publication() {
    for suspend in [false, true] {
        let mut nodes = nodes();
        let preview = nodes[0].step(Event::LivenessTimeout);
        let request = preview
            .consensus
            .messages
            .into_iter()
            .find(|m| m.to == MemberId(2))
            .unwrap();
        let reply = nodes[1]
            .step(Event::Receive(request))
            .consensus
            .messages
            .remove(0);
        let node = &mut nodes[0];
        node.publisher.fail = !suspend;
        node.publisher.suspend = suspend;
        if suspend {
            {
                let mut future = pin!(
                    node.replica
                        .step(&mut node.publisher, Event::Receive(reply))
                );
                let waker = Waker::from(Arc::new(NoopWake));
                assert!(
                    future
                        .as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending()
                );
            }
        } else {
            assert!(matches!(
                immediate(
                    node.replica
                        .step(&mut node.publisher, Event::Receive(reply))
                ),
                Err(ReplicaError::Sequence(SequenceError::Publication(_)))
            ));
        }
        assert_eq!(node.replica.role(), Err(Error::RecoveryRequired));
        let actual = node.publisher.state.take().unwrap();
        assert_eq!(actual.term(), 1);
        assert_eq!(actual.voted_for(), Some(MemberId(1)));
        let reopened = Replica::recover(MemberId(1), actual, Limits::default(), 8).unwrap();
        assert_eq!(reopened.role(), Ok(Role::Follower));
        assert_eq!(reopened.pending_reads(), 0);
    }
}

#[test]
fn higher_term_quorum_probe_cannot_reply_from_uncertain_storage() {
    let mut node = nodes().remove(1);
    node.publisher.fail = true;
    let configuration = configuration();
    let message = Envelope {
        domain: configuration.domain(),
        configuration: configuration.identity(),
        from: MemberId(1),
        to: MemberId(2),
        message: Message::QuorumProbe { term: 10, round: 1 },
    };
    assert!(matches!(
        immediate(
            node.replica
                .step(&mut node.publisher, Event::Receive(message))
        ),
        Err(ReplicaError::Sequence(SequenceError::Publication(_)))
    ));
    assert_eq!(node.publisher.state.as_ref().unwrap().term(), 10);
    assert_eq!(node.replica.role(), Err(Error::RecoveryRequired));
}
