#![cfg(not(target_arch = "wasm32"))]

//! Real Raft/Replica transitions with explicitly modeled root publication.
//! These tests do not implement authenticated transport, disk durability or
//! application authorization. Every acknowledgement goes through a follower.
use std::collections::{BTreeMap, VecDeque};
use std::future::{Future, pending};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use fgdb_order::{
    Configuration, Domain, Envelope, Error as RaftError, Event, Limits, MemberId, Message,
    PersistentState, Role, SnapshotCut,
};
use fgdb_repl::driver::{RaftPublisher, SequenceError};
use fgdb_repl::replica::{
    ReadIndexId, ReadIndexReady, ReadResolution, Replica, ReplicaError, ReplicaOutput,
};

fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
fn immediate<F: Future>(future: F) -> F::Output {
    match poll(pin!(future)) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("memory publication unexpectedly suspended"),
    }
}

#[derive(Default)]
struct MemoryRoot {
    states: Vec<PersistentState<u64>>,
    fail: bool,
    suspend: bool,
}
impl RaftPublisher<u64> for MemoryRoot {
    type Error = &'static str;
    fn publish(
        &mut self,
        state: &PersistentState<u64>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        // Model even an error/drop AFTER the exact new root reached storage.
        self.states.push(state.clone());
        let (fail, suspend) = (self.fail, self.suspend);
        async move {
            if suspend {
                pending::<()>().await;
            }
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
    root: MemoryRoot,
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
fn stable(voters: u128, learners: &[u128]) -> Configuration {
    Configuration::stable(
        Domain([51; 32]),
        [52; 32],
        (1..=voters).map(MemberId),
        learners.iter().copied().map(MemberId),
    )
    .unwrap()
}
fn nodes(configuration: &Configuration, window: usize) -> Cluster {
    configuration
        .voters()
        .union(configuration.learners())
        .map(|id| {
            let mut replica = Replica::new(
                *id,
                configuration.clone(),
                Limits {
                    max_log_entries: 128,
                    max_append_entries: 1,
                },
                8,
            )
            .unwrap();
            replica.configure_append_pipeline(window).unwrap();
            (
                *id,
                Node {
                    replica,
                    root: MemoryRoot::default(),
                },
            )
        })
        .collect()
}
fn leader(nodes: &mut Cluster) -> &mut Node {
    nodes.get_mut(&MemberId(1)).unwrap()
}
fn pump(
    nodes: &mut Cluster,
    messages: Vec<Envelope<u64>>,
    reachable: &[u128],
) -> Vec<ReadResolution> {
    let mut queue: VecDeque<_> = messages.into();
    let mut reads = Vec::new();
    let mut steps = 0;
    while let Some(message) = queue.pop_front() {
        steps += 1;
        assert!(steps < 20_000, "replication must quiesce");
        if !reachable.contains(&message.from.0) || !reachable.contains(&message.to.0) {
            continue;
        }
        let output = nodes
            .get_mut(&message.to)
            .unwrap()
            .step(Event::Receive(message));
        queue.extend(output.consensus.messages);
        reads.extend(output.reads);
    }
    reads
}
fn elect(configuration: &Configuration, window: usize) -> Cluster {
    let mut nodes = nodes(configuration, window);
    let reachable: Vec<_> = nodes.keys().map(|id| id.0).collect();
    let output = leader(&mut nodes).step(Event::ElectionTimeout);
    assert!(pump(&mut nodes, output.consensus.messages, &reachable).is_empty());
    let output = leader(&mut nodes).step(Event::Heartbeat);
    assert!(pump(&mut nodes, output.consensus.messages, &reachable).is_empty());
    assert_eq!(leader(&mut nodes).replica.role(), Ok(Role::Leader));
    nodes
}
fn requests(output: &ReplicaOutput<u64>, peer: u128) -> Vec<Envelope<u64>> {
    output
        .consensus
        .messages
        .iter()
        .filter(|m| m.to == MemberId(peer) && matches!(m.message, Message::Append { .. }))
        .cloned()
        .collect()
}
fn serial(request: &Envelope<u64>) -> u64 {
    let Message::Append { request, .. } = &request.message else {
        panic!("expected append")
    };
    *request
}
fn reply(nodes: &mut Cluster, request: Envelope<u64>) -> Envelope<u64> {
    let output = nodes
        .get_mut(&request.to)
        .unwrap()
        .step(Event::Receive(request));
    assert!(output.reads.is_empty());
    assert_eq!(output.consensus.messages.len(), 1);
    output.consensus.messages.into_iter().next().unwrap()
}
fn only_ready(resolutions: Vec<ReadResolution>) -> ReadIndexReady {
    assert_eq!(resolutions.len(), 1);
    match resolutions.into_iter().next().unwrap() {
        ReadResolution::Ready(ready) => ready,
        ReadResolution::LeadershipLost(_) => panic!("unexpected leadership loss"),
    }
}

#[test]
fn earlier_fresh_reply_completes_read_while_later_append_requests_remain_in_flight() {
    let mut nodes = elect(&stable(3, &[]), 4);
    let before = leader(&mut nodes).replica.durable_state().unwrap().clone();
    let (id, probe) = leader(&mut nodes).read();
    let probe = requests(&probe, 2).remove(0);
    let first = leader(&mut nodes).step(Event::Propose(10));
    let second = leader(&mut nodes).step(Event::Propose(11));
    assert!(serial(&requests(&first, 2)[0]) > serial(&probe));
    assert!(serial(&requests(&second, 2)[0]) > serial(&requests(&first, 2)[0]));
    let reply = reply(&mut nodes, probe);
    let writes = leader(&mut nodes).root.states.len();
    let ready = only_ready(leader(&mut nodes).step(Event::Receive(reply)).reads);
    assert_eq!(ready.id(), &id);
    assert_eq!(ready.index(), before.commit_index());
    assert_eq!(leader(&mut nodes).root.states.len(), writes); // read does not publish
    assert_eq!(leader(&mut nodes).replica.pending_reads(), 0);
    assert_eq!(
        leader(&mut nodes)
            .replica
            .durable_state()
            .unwrap()
            .commit_index(),
        1
    );
}

#[test]
fn retransmitted_pre_read_request_is_stale_but_a_later_pipelined_append_is_fresh() {
    let mut nodes = elect(&stable(3, &[]), 4);
    let old = leader(&mut nodes).step(Event::Heartbeat);
    let old = requests(&old, 2).remove(0);
    let delayed = reply(&mut nodes, old.clone());
    let (id, probe) = leader(&mut nodes).read();
    assert_eq!(requests(&probe, 2), vec![old]);
    let data = leader(&mut nodes).step(Event::Propose(10));
    assert!(
        leader(&mut nodes)
            .step(Event::Receive(delayed))
            .reads
            .is_empty()
    );
    assert_eq!(leader(&mut nodes).replica.pending_reads(), 1);
    let response = reply(&mut nodes, requests(&data, 2).remove(0));
    let output = leader(&mut nodes).step(Event::Receive(response));
    let ready = only_ready(output.reads);
    assert_eq!(ready.id(), &id);
    assert_eq!(ready.index(), 1); // floor captured before the later write
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
fn concurrent_reads_keep_distinct_watermarks_with_multiple_requests_in_each_window() {
    let mut nodes = elect(&stable(3, &[]), 4);
    let (first, round) = leader(&mut nodes).read();
    let probe = requests(&round, 2).remove(0);
    let data = leader(&mut nodes).step(Event::Propose(10));
    let (second, _) = leader(&mut nodes).read(); // both probe and data already issued
    let newer = leader(&mut nodes).step(Event::Propose(11));
    let response = reply(&mut nodes, probe);
    assert_eq!(
        only_ready(leader(&mut nodes).step(Event::Receive(response)).reads).id(),
        &first
    );
    let response = reply(&mut nodes, requests(&data, 2).remove(0));
    assert!(
        leader(&mut nodes)
            .step(Event::Receive(response))
            .reads
            .is_empty()
    );
    assert_eq!(leader(&mut nodes).replica.pending_reads(), 1);
    let response = reply(&mut nodes, requests(&newer, 2).remove(0));
    assert_eq!(
        only_ready(leader(&mut nodes).step(Event::Receive(response)).reads).id(),
        &second
    );
}

#[test]
fn rejection_invalidates_even_post_read_requests_before_their_delayed_successes_arrive() {
    let mut nodes = elect(&stable(3, &[]), 4);
    let (id, output) = leader(&mut nodes).read();
    let old_probe = requests(&output, 2).remove(0);
    let first = requests(&leader(&mut nodes).step(Event::Propose(10)), 2).remove(0);
    let second = requests(&leader(&mut nodes).step(Event::Propose(11)), 2).remove(0);
    let rejection = reply(&mut nodes, second.clone()); // predecessor not present
    assert!(matches!(
        rejection.message,
        Message::Appended { success: false, .. }
    ));
    let output = leader(&mut nodes).step(Event::Receive(rejection));
    assert!(output.reads.is_empty());
    let replacement = requests(&output, 2).remove(0);
    assert!(serial(&replacement) > serial(&second));
    for old in [old_probe, first, second] {
        let response = reply(&mut nodes, old);
        assert!(matches!(
            response.message,
            Message::Appended { success: true, .. }
        ));
        assert!(
            leader(&mut nodes)
                .step(Event::Receive(response))
                .reads
                .is_empty()
        );
    }
    assert_eq!(leader(&mut nodes).replica.pending_reads(), 1);
    let response = reply(&mut nodes, replacement);
    assert_eq!(
        only_ready(leader(&mut nodes).step(Event::Receive(response)).reads).id(),
        &id
    );
}

#[test]
fn compaction_retires_every_old_window_identity_before_read_confirmation() {
    let configuration = stable(3, &[]);
    let mut nodes = elect(&configuration, 4);
    let (id, output) = leader(&mut nodes).read();
    let old_probe = requests(&output, 2).remove(0);
    let data = requests(&leader(&mut nodes).step(Event::Propose(10)), 2).remove(0);
    let delayed_probe = reply(&mut nodes, old_probe);
    let delayed_data = reply(&mut nodes, data);
    let cut =
        SnapshotCut::from_authenticated_parts(&configuration, [31; 32], [32; 32], [33; 32], 1, 1)
            .unwrap();
    assert!(
        leader(&mut nodes)
            .step(Event::Compact(cut))
            .reads
            .is_empty()
    );
    for response in [delayed_probe, delayed_data] {
        assert!(
            leader(&mut nodes)
                .step(Event::Receive(response))
                .reads
                .is_empty()
        );
    }
    let retry = leader(&mut nodes).step(Event::Heartbeat);
    let response = reply(&mut nodes, requests(&retry, 2).remove(0));
    assert_eq!(
        only_ready(leader(&mut nodes).step(Event::Receive(response)).reads).id(),
        &id
    );
}

#[test]
fn matching_serial_never_bypasses_domain_configuration_recipient_or_member_validation() {
    for mutation in 0..4 {
        let mut nodes = elect(&stable(3, &[]), 4);
        let (id, output) = leader(&mut nodes).read();
        let response = reply(&mut nodes, requests(&output, 2).remove(0));
        let mut invalid = response.clone();
        match mutation {
            0 => invalid.domain = Domain([99; 32]),
            1 => invalid.configuration = [99; 32],
            2 => invalid.to = MemberId(3),
            _ => invalid.from = MemberId(9),
        }
        let node = leader(&mut nodes);
        assert!(immediate(node.replica.step(&mut node.root, Event::Receive(invalid))).is_err());
        assert_eq!(node.replica.pending_reads(), 1);
        assert_eq!(
            only_ready(node.step(Event::Receive(response)).reads).id(),
            &id
        );
    }
}

#[test]
fn several_fresh_replies_from_one_voter_and_all_learners_do_not_form_a_joint_quorum() {
    let configuration = Configuration::joint(
        Domain([51; 32]),
        [52; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [MemberId(3), MemberId(4), MemberId(5)],
        [MemberId(6)],
    )
    .unwrap();
    let mut nodes = elect(&configuration, 4);
    let (id, round) = leader(&mut nodes).read();
    let first = leader(&mut nodes).step(Event::Propose(10));
    let second = leader(&mut nodes).step(Event::Propose(11));
    for peer in [2, 4, 6] {
        for output in [&round, &first, &second] {
            let response = reply(&mut nodes, requests(output, peer).remove(0));
            assert!(
                leader(&mut nodes)
                    .step(Event::Receive(response.clone()))
                    .reads
                    .is_empty()
            );
            assert!(
                leader(&mut nodes)
                    .step(Event::Receive(response))
                    .reads
                    .is_empty()
            );
        }
    }
    assert_eq!(leader(&mut nodes).replica.pending_reads(), 1);
    let response = reply(&mut nodes, requests(&round, 3).remove(0));
    assert_eq!(
        only_ready(leader(&mut nodes).step(Event::Receive(response)).reads).id(),
        &id
    );
}

#[test]
fn failed_or_cancelled_commit_publication_releases_no_pipeline_read_evidence() {
    for suspend in [false, true] {
        let mut nodes = elect(&stable(3, &[]), 4);
        let old = leader(&mut nodes).step(Event::Heartbeat);
        let old = reply(&mut nodes, requests(&old, 2).remove(0));
        let (id, _) = leader(&mut nodes).read();
        let data = leader(&mut nodes).step(Event::Propose(10));
        assert!(
            leader(&mut nodes)
                .step(Event::Receive(old))
                .reads
                .is_empty()
        );
        let response = reply(&mut nodes, requests(&data, 2).remove(0));
        let node = leader(&mut nodes);
        node.root.suspend = suspend;
        node.root.fail = !suspend;
        if suspend {
            let mut work = Box::pin(node.replica.step(&mut node.root, Event::Receive(response)));
            assert!(poll(work.as_mut()).is_pending());
            drop(work);
        } else {
            assert!(matches!(
                immediate(node.replica.step(&mut node.root, Event::Receive(response))),
                Err(ReplicaError::Sequence(SequenceError::Publication(_)))
            ));
        }
        assert_eq!(node.replica.role(), Err(RaftError::RecoveryRequired));
        assert!(immediate(node.replica.read_index(&mut node.root)).is_err());
        let recovered_root = node.root.states.last().unwrap().clone();
        assert_eq!(recovered_root.commit_index(), 2); // error need not mean rollback
        let mut recovered =
            Replica::recover(MemberId(1), recovered_root, Limits::default(), 8).unwrap();
        assert_eq!(recovered.pending_reads(), 0);
        assert!(!recovered.cancel_read(&id));
        assert_eq!(recovered.append_pipeline_window(), 1);
    }
}

#[test]
fn leader_loss_cancels_reads_with_full_windows_and_delayed_replies_cannot_revive_them() {
    let mut nodes = elect(&stable(3, &[]), 4);
    let (id, round) = leader(&mut nodes).read();
    let mut requests_to_two = requests(&round, 2);
    for value in 10..13 {
        requests_to_two.extend(requests(&leader(&mut nodes).step(Event::Propose(value)), 2));
    }
    assert_eq!(requests_to_two.len(), 4);
    let replies: Vec<_> = requests_to_two
        .into_iter()
        .map(|r| reply(&mut nodes, r))
        .collect();
    leader(&mut nodes).step(Event::LivenessTimeout); // initial fresh-quorum interval
    let output = leader(&mut nodes).step(Event::LivenessTimeout); // no probe replies
    assert_eq!(output.consensus.role, Role::Follower);
    assert_eq!(output.reads.len(), 1);
    assert!(matches!(&output.reads[0], ReadResolution::LeadershipLost(lost) if lost == &id));
    for response in replies {
        assert!(
            leader(&mut nodes)
                .step(Event::Receive(response))
                .reads
                .is_empty()
        );
    }
    assert_eq!(leader(&mut nodes).replica.pending_reads(), 0);
}

#[test]
fn pipeline_setting_is_follower_only_and_cannot_bypass_replica_publication_fences() {
    let mut node = Node {
        replica: Replica::new(MemberId(1), stable(1, &[]), Limits::default(), 8).unwrap(),
        root: MemoryRoot::default(),
    };
    assert_eq!(
        node.replica.configure_append_pipeline(65),
        Err(RaftError::InvalidLimits)
    );
    node.replica.configure_append_pipeline(4).unwrap();
    assert_eq!(node.replica.append_pipeline_window(), 4);
    node.step(Event::ElectionTimeout);
    assert_eq!(
        node.replica.configure_append_pipeline(2),
        Err(RaftError::PipelineConfigurationBusy)
    );
    let (id, output) = node.read();
    assert_eq!(only_ready(output.reads).id(), &id); // quorum one needs no peer flight
    node.root.fail = true;
    assert!(immediate(node.replica.step(&mut node.root, Event::Propose(9))).is_err());
    assert_eq!(
        node.replica.configure_append_pipeline(2),
        Err(RaftError::RecoveryRequired)
    );
}

#[path = "pipelined_reads/application.rs"]
mod application;
