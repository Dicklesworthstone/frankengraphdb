#![cfg(not(target_arch = "wasm32"))]

// Deterministic, in-process protocol scenarios. MemoryRoot models publication
// ordering, not Chronicle serialization, fsync, auth, application or Warden.
use std::collections::VecDeque;
use std::future::{Future, pending};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use fgdb_order::{
    Configuration, Domain, Entry, Envelope, Error as RaftError, Event, Limits, MemberId, Message,
    PersistentState, Role, SnapshotCut,
};
use fgdb_repl::driver::{RaftPublisher, SequenceError};
use fgdb_repl::replica::{
    ReadIndexId, ReadIndexReady, ReadResolution, Replica, ReplicaError, ReplicaOutput,
};

fn immediate<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    match pin!(future).as_mut().poll(&mut context) {
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
        // An error/cancellation can follow a completed write. Never assume the
        // old root stayed authoritative solely because the caller saw an error.
        self.states.push(state.clone());
        let (fail, suspend) = (self.fail, self.suspend);
        async move {
            if suspend {
                pending::<()>().await;
            }
            if fail {
                Err("publication outcome unknown")
            } else {
                Ok(())
            }
        }
    }
}

struct Node {
    member: Replica<u64>,
    root: MemoryRoot,
}

impl Node {
    fn step(&mut self, event: Event<u64>) -> ReplicaOutput<u64> {
        immediate(self.member.step(&mut self.root, event)).unwrap()
    }
    fn read(&mut self) -> (ReadIndexId, ReplicaOutput<u64>) {
        immediate(self.member.read_index(&mut self.root)).unwrap()
    }
}

fn stable(voters: u128, learners: u128) -> Configuration {
    Configuration::stable(
        Domain([11; 32]),
        [12; 32],
        (1..=voters).map(MemberId),
        (voters + 1..=voters + learners).map(MemberId),
    )
    .unwrap()
}

fn nodes(configuration: &Configuration) -> Vec<Node> {
    configuration
        .voters()
        .union(configuration.learners())
        .map(|id| Node {
            member: Replica::new(*id, configuration.clone(), Limits::default(), 16).unwrap(),
            root: MemoryRoot::default(),
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
    let mut count = 0;
    while let Some(envelope) = queue.pop_front() {
        count += 1;
        assert!(count < 16_384, "message delivery must quiesce");
        if !reachable.contains(&envelope.from.0) || !reachable.contains(&envelope.to.0) {
            continue;
        }
        let destination = nodes
            .iter_mut()
            .find(|node| node.member.id() == envelope.to)
            .unwrap();
        let output = destination.step(Event::Receive(envelope));
        queue.extend(output.consensus.messages);
        resolutions.extend(output.reads);
    }
    resolutions
}

fn elect(nodes: &mut [Node], reachable: &[u128]) {
    let output = nodes[0].step(Event::ElectionTimeout);
    assert!(pump(nodes, output.consensus.messages, reachable).is_empty());
    assert_eq!(nodes[0].member.role(), Ok(Role::Leader));
    assert!(nodes[0].member.durable_state().unwrap().commit_index() > 0);
    // Drain any commit-notification heartbeat that was retained while an older
    // append was in flight, leaving every reachable peer quiescent.
    let output = nodes[0].step(Event::Heartbeat);
    assert!(pump(nodes, output.consensus.messages, reachable).is_empty());
}

fn request_to(output: &ReplicaOutput<u64>, to: u128) -> Envelope<u64> {
    output
        .consensus
        .messages
        .iter()
        .find(|envelope| envelope.to == MemberId(to))
        .unwrap()
        .clone()
}

fn reply(nodes: &mut [Node], request: Envelope<u64>) -> Envelope<u64> {
    let leader = request.from;
    let destination = nodes
        .iter_mut()
        .find(|node| node.member.id() == request.to)
        .unwrap();
    let output = destination.step(Event::Receive(request));
    assert!(output.reads.is_empty());
    output
        .consensus
        .messages
        .into_iter()
        .find(|envelope| envelope.to == leader)
        .unwrap()
}

fn only_ready(mut resolutions: Vec<ReadResolution>) -> ReadIndexReady {
    assert_eq!(resolutions.len(), 1);
    match resolutions.remove(0) {
        ReadResolution::Ready(ready) => ready,
        ReadResolution::LeadershipLost(_) => panic!("unexpected leader loss"),
    }
}

#[test]
fn quiescent_majority_confirms_one_read_without_a_log_entry_or_publication() {
    let config = stable(3, 0);
    let mut nodes = nodes(&config);
    elect(&mut nodes, &[1, 2]); // Member 3 remains partitioned.
    let before = nodes[0].member.durable_state().unwrap().clone();
    let writes: Vec<_> = nodes.iter().map(|node| node.root.states.len()).collect();
    let (id, probe) = nodes[0].read();
    assert!(probe.reads.is_empty());
    let ready = only_ready(pump(&mut nodes, probe.consensus.messages, &[1, 2]));
    assert_eq!(ready.id(), &id);
    assert_eq!(ready.domain(), config.domain());
    assert_eq!(ready.configuration(), config.identity());
    assert_eq!(ready.leader(), MemberId(1));
    assert_eq!(ready.term(), before.term());
    assert_eq!(ready.index(), before.commit_index());
    assert_eq!(nodes[0].member.durable_state().unwrap(), &before);
    assert_eq!(nodes[0].member.pending_reads(), 0);
    assert_eq!(
        nodes
            .iter()
            .map(|node| node.root.states.len())
            .collect::<Vec<_>>(),
        writes
    );
}

#[test]
fn delayed_pre_read_reply_cannot_authorize_a_retransmitted_probe() {
    let mut nodes = nodes(&stable(3, 0));
    elect(&mut nodes, &[1, 2]);
    let old = nodes[0].step(Event::Heartbeat);
    let old_request = request_to(&old, 2);
    let delayed = reply(&mut nodes, old_request.clone());
    let (id, probe) = nodes[0].read();
    assert_eq!(request_to(&probe, 2), old_request); // Same native in-flight RPC.
    let output = nodes[0].step(Event::Receive(delayed));
    assert!(output.reads.is_empty());
    assert_eq!(nodes[0].member.pending_reads(), 1);
    let fresh = nodes[0].step(Event::Heartbeat);
    let ready = only_ready(pump(&mut nodes, fresh.consensus.messages, &[1, 2]));
    assert_eq!(ready.id(), &id);
}

#[test]
fn later_reads_never_join_a_round_already_in_flight() {
    let mut nodes = nodes(&stable(3, 0));
    elect(&mut nodes, &[1, 2]);
    let (first, first_probe) = nodes[0].read();
    let request = request_to(&first_probe, 2);
    let delayed = reply(&mut nodes, request.clone());
    let (second, second_probe) = nodes[0].read();
    assert_eq!(request_to(&second_probe, 2), request);
    let ready = only_ready(nodes[0].step(Event::Receive(delayed)).reads);
    assert_eq!(ready.id(), &first);
    assert_ne!(ready.id(), &second);
    assert_eq!(nodes[0].member.pending_reads(), 1);
    let output = nodes[0].step(Event::Heartbeat);
    let ready = only_ready(pump(&mut nodes, output.consensus.messages, &[1, 2]));
    assert_eq!(ready.id(), &second);
}

#[test]
fn duplicates_and_learner_responses_do_not_form_a_voter_quorum() {
    let mut nodes = nodes(&stable(5, 1));
    elect(&mut nodes, &[1, 2, 3, 4, 5, 6]);
    let (id, output) = nodes[0].read();
    let learner = reply(&mut nodes, request_to(&output, 6));
    assert!(nodes[0].step(Event::Receive(learner)).reads.is_empty());
    let voter = reply(&mut nodes, request_to(&output, 2));
    assert!(
        nodes[0]
            .step(Event::Receive(voter.clone()))
            .reads
            .is_empty()
    );
    for _ in 0..8 {
        assert!(
            nodes[0]
                .step(Event::Receive(voter.clone()))
                .reads
                .is_empty()
        );
    }
    assert_eq!(nodes[0].member.pending_reads(), 1);
    let another = reply(&mut nodes, request_to(&output, 3));
    let ready = only_ready(nodes[0].step(Event::Receive(another)).reads);
    assert_eq!(ready.id(), &id);
    assert!(nodes[0].step(Event::Receive(voter)).reads.is_empty());
}

#[test]
fn joint_read_needs_independent_old_and_new_majorities() {
    let config = Configuration::joint(
        Domain([13; 32]),
        [14; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [MemberId(3), MemberId(4), MemberId(5)],
        [],
    )
    .unwrap();
    let mut nodes = nodes(&config);
    elect(&mut nodes, &[1, 2, 3, 4, 5]);
    let (id, output) = nodes[0].read();
    for voter in [2, 3] {
        let ack = reply(&mut nodes, request_to(&output, voter));
        assert!(nodes[0].step(Event::Receive(ack)).reads.is_empty());
    }
    // Three of five union members is still only one of the three new voters.
    assert_eq!(nodes[0].member.pending_reads(), 1);
    let ack = reply(&mut nodes, request_to(&output, 4));
    let ready = only_ready(nodes[0].step(Event::Receive(ack)).reads);
    assert_eq!(ready.id(), &id);
}

#[test]
fn follower_and_new_leader_without_current_term_commit_refuse_reads() {
    let mut nodes = nodes(&stable(3, 0));
    {
        let node = &mut nodes[0];
        assert!(matches!(
            immediate(node.member.read_index(&mut node.root)),
            Err(ReplicaError::Raft(RaftError::NotLeader))
        ));
    }
    let campaign = nodes[0].step(Event::ElectionTimeout);
    let vote = reply(&mut nodes, request_to(&campaign, 2));
    let leader = nodes[0].step(Event::Receive(vote));
    assert_eq!(leader.consensus.role, Role::Leader);
    assert_eq!(nodes[0].member.durable_state().unwrap().commit_index(), 0);
    {
        let node = &mut nodes[0];
        assert!(matches!(
            immediate(node.member.read_index(&mut node.root)),
            Err(ReplicaError::CurrentTermNotCommitted)
        ));
        assert_eq!(node.member.pending_reads(), 0);
    }
    assert!(pump(&mut nodes, leader.consensus.messages, &[1, 2]).is_empty());
    let (_, probe) = nodes[0].read();
    let mut results = pump(&mut nodes, probe.consensus.messages, &[1, 2]);
    if results.is_empty() {
        let fresh = nodes[0].step(Event::Heartbeat);
        results = pump(&mut nodes, fresh.consensus.messages, &[1, 2]);
    }
    assert!(only_ready(results).index() > 0);
}

#[test]
fn unknown_or_wrong_domain_replies_never_consume_the_real_probe() {
    let mut nodes = nodes(&stable(3, 0));
    elect(&mut nodes, &[1, 2]);
    let (id, output) = nodes[0].read();
    let ack = reply(&mut nodes, request_to(&output, 2));
    let mut unissued = ack.clone();
    if let Message::Appended { request, .. } = &mut unissued.message {
        *request += 1000;
    }
    assert!(nodes[0].step(Event::Receive(unissued)).reads.is_empty());
    for configuration in [false, true] {
        let mut foreign = ack.clone();
        if configuration {
            foreign.configuration[0] ^= 1;
        } else {
            foreign.domain.0[0] ^= 1;
        }
        let node = &mut nodes[0];
        let result = immediate(node.member.step(&mut node.root, Event::Receive(foreign)));
        assert!(matches!(
            result,
            Err(ReplicaError::Sequence(SequenceError::Raft(
                RaftError::WrongDomain | RaftError::WrongConfiguration
            )))
        ));
        assert_eq!(node.member.pending_reads(), 1);
    }
    let ready = only_ready(nodes[0].step(Event::Receive(ack)).reads);
    assert_eq!(ready.id(), &id);
}

#[test]
fn rejection_only_schedules_a_new_probe_and_never_confirms_the_read() {
    let mut nodes = nodes(&stable(3, 0));
    elect(&mut nodes, &[1, 2]);
    let (id, output) = nodes[0].read();
    let mut rejected = reply(&mut nodes, request_to(&output, 2));
    if let Message::Appended {
        success,
        conflict_next,
        ..
    } = &mut rejected.message
    {
        *success = false;
        *conflict_next = 1;
    }
    let retry = nodes[0].step(Event::Receive(rejected));
    assert!(retry.reads.is_empty());
    let ready = only_ready(pump(&mut nodes, retry.consensus.messages, &[1, 2]));
    assert_eq!(ready.id(), &id);
}

#[test]
fn observed_leader_loss_cancels_pending_reads_instead_of_releasing_stale_floors() {
    let config = stable(3, 0);
    let mut nodes = nodes(&config);
    elect(&mut nodes, &[1, 2, 3]);
    let (id, probe) = nodes[0].read();
    let stale = reply(&mut nodes, request_to(&probe, 2));
    // New term from the other voter invalidates all prior-term read barriers.
    let term = nodes[0].member.durable_state().unwrap().term() + 1;
    let output = nodes[0].step(Event::Receive(Envelope {
        domain: config.domain(),
        configuration: config.identity(),
        from: MemberId(3),
        to: MemberId(1),
        message: Message::RequestVote {
            term,
            last_index: 1,
            last_term: term - 1,
        },
    }));
    assert_eq!(output.consensus.role, Role::Follower);
    assert!(matches!(&output.reads[..], [ReadResolution::LeadershipLost(lost)] if lost == &id));
    assert_eq!(nodes[0].member.pending_reads(), 0);
    assert!(nodes[0].step(Event::Receive(stale)).reads.is_empty());
}

#[test]
fn partitioned_former_leader_never_completes_a_new_read_from_old_responses() {
    let mut nodes = nodes(&stable(3, 0));
    elect(&mut nodes, &[1, 2, 3]);
    let old = nodes[0].step(Event::Heartbeat);
    let delayed: Vec<_> = [2, 3]
        .into_iter()
        .map(|to| reply(&mut nodes, request_to(&old, to)))
        .collect();
    // The old leader is partitioned while voters 2 and 3 elect a successor.
    let election = nodes[1].step(Event::ElectionTimeout);
    assert!(pump(&mut nodes, election.consensus.messages, &[2, 3]).is_empty());
    assert_eq!(nodes[1].member.role(), Ok(Role::Leader));
    let write = nodes[1].step(Event::Propose(42));
    assert!(pump(&mut nodes, write.consensus.messages, &[2, 3]).is_empty());
    assert!(
        nodes[1]
            .member
            .committed_after(0)
            .unwrap()
            .iter()
            .any(|entry| entry.entry.command == Some(42))
    );
    let (id, _) = nodes[0].read(); // Still believes it leads, but must probe afresh.
    for ack in delayed {
        assert!(nodes[0].step(Event::Receive(ack)).reads.is_empty());
    }
    assert_eq!(nodes[0].member.pending_reads(), 1);
    // Healing delivers a current-term rejection, not a read confirmation.
    let probe = nodes[0].step(Event::Heartbeat);
    let results = pump(&mut nodes, probe.consensus.messages, &[1, 2, 3]);
    assert!(
        results
            .iter()
            .all(|result| matches!(result, ReadResolution::LeadershipLost(_)))
    );
    assert!(
        results
            .iter()
            .any(|result| matches!(result, ReadResolution::LeadershipLost(lost) if lost == &id))
    );
    assert_eq!(nodes[0].member.role(), Ok(Role::Follower));
}

#[test]
fn captured_floor_does_not_move_when_a_later_write_commits() {
    let mut nodes = nodes(&stable(3, 0));
    elect(&mut nodes, &[1, 2]);
    let floor = nodes[0].member.durable_state().unwrap().commit_index();
    let (id, probe) = nodes[0].read();
    let ack = reply(&mut nodes, request_to(&probe, 2));
    // A later proposal is not a reason to change an already-admitted read cut.
    let proposal = nodes[0].step(Event::Propose(77));
    assert!(proposal.reads.is_empty());
    let output = nodes[0].step(Event::Receive(ack));
    let ready = only_ready(output.reads);
    assert_eq!(ready.id(), &id);
    assert_eq!(ready.index(), floor);
    assert!(pump(&mut nodes, output.consensus.messages, &[1, 2]).is_empty());
    assert!(nodes[0].member.durable_state().unwrap().commit_index() > floor);
}

#[test]
fn compaction_invalidates_old_probe_identities_without_losing_pending_reads() {
    let config = stable(3, 0);
    let mut nodes = nodes(&config);
    elect(&mut nodes, &[1, 2]);
    let (id, output) = nodes[0].read();
    let delayed = reply(&mut nodes, request_to(&output, 2));
    let state = nodes[0].member.durable_state().unwrap();
    let cut = SnapshotCut::from_authenticated_parts(
        &config,
        [21; 32],
        [22; 32],
        [23; 32],
        state.commit_index(),
        state.term(),
    )
    .unwrap();
    // Synthetic verifier-approved visible/retained cut in this protocol fixture.
    assert!(nodes[0].step(Event::Compact(cut)).reads.is_empty());
    assert!(nodes[0].step(Event::Receive(delayed)).reads.is_empty());
    let fresh = nodes[0].step(Event::Heartbeat);
    let ready = only_ready(pump(&mut nodes, fresh.consensus.messages, &[1, 2]));
    assert_eq!(ready.id(), &id);
}

#[test]
fn bounded_admission_cancellation_and_recovery_do_not_reuse_read_identities() {
    let config = stable(3, 0);
    let mut nodes = nodes(&config);
    nodes[0].member = Replica::new(MemberId(1), config, Limits::default(), 1).unwrap();
    elect(&mut nodes, &[1, 2]);
    let (cancelled, first) = nodes[0].read();
    {
        let node = &mut nodes[0];
        assert!(matches!(
            immediate(node.member.read_index(&mut node.root)),
            Err(ReplicaError::ReadBackpressure)
        ));
    }
    assert!(!nodes[1].member.cancel_read(&cancelled));
    assert!(nodes[0].member.cancel_read(&cancelled));
    assert!(!nodes[0].member.cancel_read(&cancelled));
    assert!(pump(&mut nodes, first.consensus.messages, &[1, 2]).is_empty());
    let (before_recovery, _) = nodes[0].read();
    let persisted = nodes[0].member.durable_state().unwrap().clone();
    nodes[0].member = Replica::recover(MemberId(1), persisted, Limits::default(), 1).unwrap();
    assert_eq!(nodes[0].member.pending_reads(), 0);
    elect(&mut nodes, &[1, 2]);
    let (after_recovery, _) = nodes[0].read();
    assert_ne!(before_recovery, after_recovery);
    assert_ne!(cancelled, after_recovery); // Serial 1 is reused only in a NEW incarnation.
    assert!(!nodes[0].member.cancel_read(&cancelled));
    assert_eq!(nodes[0].member.pending_reads(), 1);
    assert!(nodes[0].member.cancel_read(&after_recovery));
}

#[test]
fn one_member_read_after_snapshot_and_noop_needs_no_extra_root_write() {
    let config = stable(1, 0);
    let cut = SnapshotCut::from_authenticated_parts(&config, [31; 32], [32; 32], [33; 32], 10, 2)
        .unwrap();
    let persisted = PersistentState::from_authenticated_snapshot(
        config,
        2,
        None,
        10,
        cut,
        Vec::<Entry<u64>>::new(),
    );
    let mut node = Node {
        member: Replica::recover(MemberId(1), persisted, Limits::default(), 16).unwrap(),
        root: MemoryRoot::default(),
    };
    node.step(Event::ElectionTimeout);
    let writes = node.root.states.len();
    let (id, output) = node.read();
    let ready = only_ready(output.reads);
    assert_eq!(ready.id(), &id);
    assert_eq!(ready.index(), 11);
    assert_eq!(ready.term(), 3);
    assert!(output.consensus.messages.is_empty());
    assert_eq!(node.root.states.len(), writes);
}

#[test]
fn failed_or_cancelled_publication_cannot_release_a_quorum_read() {
    for suspend in [false, true] {
        let mut nodes = nodes(&stable(3, 0));
        elect(&mut nodes, &[1, 2]);
        // Stage a write first, then admit a read. Its old append ack cannot
        // confirm the read and must independently publish the new commit index.
        let proposal = nodes[0].step(Event::Propose(88));
        let ack = reply(&mut nodes, request_to(&proposal, 2));
        let (_, output) = nodes[0].read();
        assert!(output.reads.is_empty());
        let node = &mut nodes[0];
        node.root.fail = !suspend;
        node.root.suspend = suspend;
        if suspend {
            let mut future = pin!(node.member.step(&mut node.root, Event::Receive(ack)));
            let waker = Waker::noop();
            let mut context = Context::from_waker(waker);
            assert!(future.as_mut().poll(&mut context).is_pending());
        } else {
            assert!(matches!(
                immediate(node.member.step(&mut node.root, Event::Receive(ack))),
                Err(ReplicaError::Sequence(SequenceError::Publication(_)))
            ));
        }
        assert_eq!(node.member.role(), Err(RaftError::RecoveryRequired));
        assert!(matches!(
            immediate(node.member.read_index(&mut node.root)),
            Err(ReplicaError::Raft(RaftError::RecoveryRequired))
        ));
        assert!(node.root.states.last().unwrap().commit_index() >= 2);
    }
}

#[test]
fn invalid_read_capacity_is_rejected_before_a_replica_can_be_used() {
    for capacity in [0, 1025, usize::MAX] {
        assert!(matches!(
            Replica::<u64>::new(MemberId(1), stable(1, 0), Limits::default(), capacity),
            Err(RaftError::InvalidLimits)
        ));
    }
}

#[test]
fn snapshot_install_ack_is_not_a_fresh_read_confirmation() {
    let config = stable(3, 0);
    let mut nodes = nodes(&config);
    elect(&mut nodes, &[1, 3]); // Voter 2 never learned the committed prefix.
    let state = nodes[0].member.durable_state().unwrap();
    let cut = SnapshotCut::from_authenticated_parts(
        &config,
        [41; 32],
        [42; 32],
        [43; 32],
        state.commit_index(),
        state.term(),
    )
    .unwrap();
    nodes[0].step(Event::Compact(cut));
    let (id, probe) = nodes[0].read();
    let offer = request_to(&probe, 2);
    let Message::InstallSnapshot { term, request, .. } = offer.message else {
        panic!("lagging voter needs snapshot catch-up");
    };
    // Even an exact successful install receipt cannot stand in for the separate
    // Append-based current-leader probe. The ordinary quorum still must respond.
    let output = nodes[0].step(Event::Receive(Envelope {
        domain: config.domain(),
        configuration: config.identity(),
        from: MemberId(2),
        to: MemberId(1),
        message: Message::SnapshotInstalled { term, request },
    }));
    assert!(output.reads.is_empty());
    let ack = reply(&mut nodes, request_to(&probe, 3));
    let ready = only_ready(nodes[0].step(Event::Receive(ack)).reads);
    assert_eq!(ready.id(), &id);
}

#[allow(dead_code)]
#[path = "../../fgdb-chronicle/tests/support/bonded.rs"]
mod seed_support;

use fgdb_chronicle::seed::{ObjectPublication, SeedAnchor, SeedLimits, SeedObjectSpec, SeedPlan};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_chronicle::transfer::{DonorId, PullLimits, PullRequest};
use fgdb_repl::SnapshotPublication;
use fgdb_repl::driver::bonded::ReplyOutcome;
use fgdb_repl::driver::bonded::streaming::{StreamingSeedSource, SymbolTransport};
use fgdb_repl::driver::{SeedCatalog, SeedPublisher, SeedRecovery};

struct SeedFixture(Vec<seed_support::Fixture>);

impl SeedFixture {
    fn spec(&self, i: usize) -> SeedObjectSpec {
        SeedObjectSpec {
            object_id: self.0[i].encoding.object_id(),
            object_kind: seed_support::KIND,
            compressed_len: self.0[i].plaintext.len() as u64,
        }
    }
}

impl SeedCatalog for SeedFixture {
    type Error = &'static str;
    fn recovery(&self, object: SeedObjectSpec) -> Result<SeedRecovery<'_>, Self::Error> {
        let fixture = self
            .0
            .iter()
            .find(|item| item.encoding.object_id() == object.object_id)
            .ok_or("unknown fixture object")?;
        Ok(SeedRecovery {
            encoding: &fixture.encoding,
            target: fixture.target(),
            dek: &seed_support::DEK,
            donors: &[DonorId(1), DonorId(2), DonorId(3)],
            limits: PullLimits::default(),
        })
    }
}

impl SymbolTransport for SeedFixture {
    type Error = &'static str;
    async fn request(&self, request: PullRequest) -> Result<ReplyOutcome, Self::Error> {
        if request.donor == DonorId(1) {
            return Ok(ReplyOutcome::Unavailable);
        }
        let fixture = self
            .0
            .iter()
            .find(|item| item.encoding.object_id() == request.object_id)
            .ok_or("unknown fixture route")?;
        Ok(ReplyOutcome::Record(
            fixture.records[request.esi as usize].clone(),
        ))
    }
}

#[derive(Default)]
struct SeedRoot {
    objects: Vec<[u8; 32]>,
    state: Option<PersistentState<u64>>,
}

impl SeedPublisher<u64> for SeedRoot {
    type Error = &'static str;
    fn publish_object(
        &mut self,
        object: ObjectPublication<'_>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.objects.push(object.object().object_id().0);
        std::future::ready(Ok(()))
    }
    fn publish_snapshot(
        &mut self,
        snapshot: SnapshotPublication<'_, u64>,
    ) -> impl Future<Output = Result<RootPublicationEvidence, Self::Error>> {
        for object in snapshot.seed().plan().objects() {
            assert!(self.objects.contains(&object.object_id.0));
        }
        self.state = Some(snapshot.consensus().clone());
        let anchor = snapshot.seed().plan().anchor();
        // Memory publication test double; production obtains post-sync reread
        // evidence from RootStore, never by copying coordinates from the plan.
        std::future::ready(Ok(RootPublicationEvidence {
            written_index: 1,
            slot_generation: anchor.publication_generation,
            root_manifest_oid: anchor.publication_root.0,
        }))
    }
}

#[test]
fn owned_replica_seeds_then_elects_and_confirms_reads_through_the_same_driver() {
    let config = stable(3, 0);
    let fixture = SeedFixture((161..165).map(seed_support::Fixture::new).collect());
    let roots: Vec<_> = (0..4).map(|i| fixture.spec(i).object_id).collect();
    let cut =
        SnapshotCut::from_authenticated_parts(&config, roots[0].0, roots[1].0, roots[2].0, 12, 3)
            .unwrap();
    let plan = SeedPlan::from_authenticated_inventory(
        SeedAnchor {
            namespace: seed_support::namespace(),
            consensus_domain: config.domain().0,
            configuration: config.identity(),
            snapshot_manifest: roots[0],
            state_root: roots[1],
            retention_floor: roots[2],
            publication_root: roots[3],
            publication_generation: 2,
            raft_index: 12,
            raft_term: 3,
            logical_command_seq: 8,
            commit_seq: 8,
        },
        (0..4).map(|i| fixture.spec(i)),
        SeedLimits::default(),
    )
    .unwrap();
    let mut follower = Node {
        member: Replica::new(MemberId(2), config.clone(), Limits::default(), 16).unwrap(),
        root: MemoryRoot::default(),
    };
    let offered = follower.step(Event::Receive(Envelope {
        domain: config.domain(),
        configuration: config.identity(),
        from: MemberId(1),
        to: MemberId(2),
        message: Message::InstallSnapshot {
            term: 3,
            request: 1,
            snapshot: cut.clone(),
        },
    }));
    assert!(offered.consensus.messages.is_empty());
    let transfer = offered
        .consensus
        .snapshot_transfers
        .into_iter()
        .next()
        .unwrap();
    let mut verification = Vec::new();
    let mut source = StreamingSeedSource::new(
        seed_support::namespace(),
        &fixture,
        &fixture,
        &mut verification,
        6,
    );
    let mut publisher = SeedRoot::default();
    let installed = immediate(follower.member.install_snapshot(
        seed_support::namespace(),
        transfer,
        plan,
        &mut source,
        &mut publisher,
    ))
    .unwrap();
    assert_eq!(installed.consensus.installed_snapshot, Some(cut));
    assert!(installed.reads.is_empty());
    assert_eq!(publisher.objects.len(), 4);
    assert_eq!(follower.member.role(), Ok(Role::Follower));
    assert!(matches!(
        immediate(follower.member.read_index(&mut follower.root)),
        Err(ReplicaError::Raft(RaftError::NotLeader))
    ));
    let mut nodes = vec![
        Node {
            member: Replica::recover(MemberId(1), publisher.state.unwrap(), Limits::default(), 16)
                .unwrap(),
            root: MemoryRoot::default(),
        },
        follower,
    ];
    elect(&mut nodes, &[1, 2]);
    let (id, probe) = nodes[0].read();
    let ready = only_ready(pump(&mut nodes, probe.consensus.messages, &[1, 2]));
    assert_eq!(ready.id(), &id);
    assert_eq!(ready.index(), 13); // Seed cut plus current-term nonsemantic no-op.
    assert_eq!(ready.term(), 4);
    assert_eq!(nodes[1].member.durable_state().unwrap().commit_index(), 13);
}
