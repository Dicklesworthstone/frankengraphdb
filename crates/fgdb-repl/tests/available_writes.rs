#![cfg(not(target_arch = "wasm32"))]

// Actual Raft/availability/application drivers; the shared backend below models
// root ordering, authority custody and audit visibility, NOT disk or signatures.
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::{Future, pending, ready};
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use fgdb_order::{Configuration, Domain, Envelope, Event, Limits, MemberId, PersistentState, Role};
use fgdb_repl::application::member::{AppliedReplica, PinnedApplication, ReadApplication, ReadState};
use fgdb_repl::application::member::proposal::{MemberProposalError, MemberProposalOutput};
use fgdb_repl::application::{Application, ApplicationBatch, ApplicationProgress, AppliedPosition};
use fgdb_repl::availability::proposal::{PayloadProposalAuthority, ProposalError, ProposalPosition};
use fgdb_repl::availability::{
    AvailabilityInput, AvailabilityLimits, AvailabilityPolicy, EncodingRequirement, FailureDomain,
    PayloadBasis, ReceiptCoverage, SourceCoverage, StorageLocation, StorageSets, SystematicAssessment,
};
use fgdb_repl::driver::RaftPublisher;
use fgdb_repl::replica::Replica;
use fgdb_types::ObjectId;

#[path = "available_writes/tracked.rs"]
mod tracked;

#[path = "available_writes/leadership.rs"]
mod leadership;

#[path = "available_writes/batches.rs"]
mod batches;

struct NoopWake;
impl Wake for NoopWake { fn wake(self: Arc<Self>) {} }
fn immediate<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    match pin!(future).as_mut().poll(&mut Context::from_waker(&waker)) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("unexpected suspension"),
    }
}
fn cancel<F: Future>(future: F) {
    let waker = Waker::from(Arc::new(NoopWake));
    assert!(pin!(future).as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
}
fn configuration(count: u128) -> Configuration {
    Configuration::stable(Domain([1; 32]), [2; 32], (1..=count).map(MemberId), []).unwrap()
}
fn availability(config: &Configuration) -> AvailabilityInput {
    let basis = PayloadBasis { domain: config.domain(), configuration: config.identity(),
        predicate_digest: [3; 32], base_closure_digest: [4; 32] };
    let members: Vec<_> = config.voters().iter().copied().collect();
    let storage_sets = match config.joint_voters() {
        Some((old, new)) => StorageSets::Joint { old: old.iter().copied().collect(), new: new.iter().copied().collect() },
        None => StorageSets::Stable(members.clone()),
    };
    AvailabilityInput {
        policy: AvailabilityPolicy {
            basis: basis.clone(), storage_sets, tolerated_domain_failures: 0,
            locations: members.iter().map(|member| StorageLocation { member: *member,
                placement_id: [member.0 as u8; 32], failure_domains: vec![FailureDomain(member.0)] }).collect(),
        },
        requirements: vec![EncodingRequirement { object_id: ObjectId([30; 32]), encoding_id: [31; 32], source_symbols: vec![3] }],
        receipts: members.iter().map(|member| ReceiptCoverage {
            receipt_id: [member.0 as u8 + 10; 32], basis: basis.clone(), storage_member: *member,
            prepared_ownership_id: [member.0 as u8 + 20; 32],
            coverage: vec![SourceCoverage { object_id: ObjectId([30; 32]), encoding_id: [31; 32],
                placement_id: [member.0 as u8; 32], source_block: 0, first_esi: 0, end_esi: 3 }],
        }).collect(),
    }
}
fn oid(generation: u64, tag: u8) -> ObjectId {
    let mut bytes = [tag; 32];
    bytes[..8].copy_from_slice(&generation.to_le_bytes());
    ObjectId(bytes)
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode { Ready, Refuse, Suspend, FailAfter, SuspendAfter, Panic }
struct State {
    root: Option<PersistentState<u64>>,
    progress: ApplicationProgress,
    generation: u64,
    expected: AvailabilityInput,
    batch_commands: Vec<u64>,
    acquire: Mode,
    publish: Mode,
    apply: Mode,
    active_permits: usize,
    acquires: usize,
    trace: Vec<&'static str>,
    effects: Vec<(u64, u64)>,
    hide: bool,
}
struct Backend(Rc<RefCell<State>>);
impl Backend {
    fn store(&mut self, state: &PersistentState<u64>) {
        let mut s = self.0.borrow_mut();
        s.generation += 1;
        s.root = Some(state.clone());
    }
}
impl RaftPublisher<u64> for Backend {
    type Error = &'static str;
    fn publish(&mut self, state: &PersistentState<u64>) -> impl Future<Output = Result<(), Self::Error>> {
        self.0.borrow_mut().trace.push("raft");
        self.store(state);
        ready(Ok(()))
    }
}
struct Permit<'a> { backend: &'a mut Backend, position: ProposalPosition, command: u64 }
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut s = self.backend.0.borrow_mut();
        s.active_permits -= 1;
        s.trace.push("permit-drop");
    }
}
impl PayloadProposalAuthority<u64> for Backend {
    type Error = &'static str;
    type Permit<'a> = Permit<'a> where Self: 'a;
    fn acquire<'a>(&'a mut self, command: &u64, position: ProposalPosition, assessment: &SystematicAssessment<'_>)
        -> impl Future<Output = Result<Self::Permit<'a>, Self::Error>> {
        let command = *command;
        let matches = assessment.input() == &self.0.borrow().expected;
        async move {
            let mode = {
                let mut s = self.0.borrow_mut();
                s.acquires += 1;
                s.trace.push("acquire");
                s.acquire
            };
            if mode == Mode::Suspend { pending::<()>().await; }
            if mode == Mode::Refuse || !matches { return Err("authority refused"); }
            if mode == Mode::Panic { panic!("authority panic"); }
            self.0.borrow_mut().active_permits += 1;
            Ok(Permit { backend: self, position, command })
        }
    }
}
impl RaftPublisher<u64> for Permit<'_> {
    type Error = &'static str;
    fn publish(&mut self, state: &PersistentState<u64>) -> impl Future<Output = Result<(), Self::Error>> {
        assert_eq!(self.backend.0.borrow().active_permits, 1);
        assert_eq!(state.term(), self.position.term);
        assert_eq!(state.configuration().domain(), self.position.domain);
        assert_eq!(state.configuration().identity(), self.position.configuration);
        assert_eq!(state.snapshot().map_or(0, |cut| cut.index()) + state.entries().len() as u64, self.position.index);
        assert_eq!(state.entries().last().unwrap().command, Some(self.command));
        self.backend.0.borrow_mut().trace.push("authorized-raft");
        let mode = self.backend.0.borrow().publish;
        if mode == Mode::Panic { panic!("publisher panic"); }
        if mode != Mode::Refuse { self.backend.store(state); }
        async move {
            if mode == Mode::SuspendAfter { pending::<()>().await; }
            if matches!(mode, Mode::Refuse | Mode::FailAfter) { Err("uncertain publication") } else { Ok(()) }
        }
    }
}
impl Application<u64> for Backend {
    type Error = &'static str;
    fn load(&mut self) -> impl Future<Output = Result<ApplicationProgress, Self::Error>> { ready(Ok(self.0.borrow().progress)) }
    fn apply(&mut self, batch: ApplicationBatch<'_, u64>) -> impl Future<Output = Result<ApplicationProgress, Self::Error>> {
        let mode = self.0.borrow().apply;
        async move {
            if mode == Mode::Suspend { pending::<()>().await; }
            if mode == Mode::Refuse { return Err("application publication refused"); }
            let mut s = self.0.borrow_mut();
            assert_eq!(batch.basis(), &s.progress);
            assert_eq!(Some(batch.consensus()), s.root.as_ref());
            s.trace.push("apply");
            for (index, command) in batch.commands() {
                assert!(!s.effects.iter().any(|(previous, _)| *previous == index));
                s.effects.push((index, *command));
                if *command == 0 { s.hide = false; } // Explicit audit-release MODEL.
            }
            s.generation += 1;
            s.progress.applied = batch.last();
            if !s.hide { s.progress.visible_index = batch.last().index; }
            s.progress.publication_generation = s.generation;
            s.progress.publication_root = oid(s.generation, 41);
            s.progress.state_root = oid(s.generation, 42);
            let progress = s.progress;
            drop(s);
            if mode == Mode::Panic { panic!("application publication panic"); }
            if mode == Mode::SuspendAfter { pending::<()>().await; }
            if mode == Mode::FailAfter { return Err("application publication uncertain"); }
            Ok(progress)
        }
    }
}
impl ReadApplication<u64> for Backend {
    type View = Vec<u64>;
    fn pin_visible(&mut self, basis: &ApplicationProgress, at: AppliedPosition)
        -> impl Future<Output = Result<PinnedApplication<Self::View>, Self::Error>> {
        let s = self.0.borrow();
        assert_eq!(basis, &s.progress);
        let view = s.effects.iter().filter(|(index, _)| *index <= at.index).map(|(_, value)| *value).collect();
        ready(Ok(PinnedApplication { basis: *basis, at, state_root: basis.state_root, view }))
    }
}
struct Node { member: AppliedReplica<u64, Backend>, state: Rc<RefCell<State>> }
fn nodes(config: &Configuration) -> Vec<Node> {
    config.voters().iter().map(|id| {
        let state = Rc::new(RefCell::new(State {
            root: None,
            progress: ApplicationProgress { domain: config.domain(), configuration: config.identity(),
                applied: AppliedPosition { index: 0, term: 0 }, visible_index: 0,
                state_root: oid(1, 42), publication_root: oid(1, 41), publication_generation: 1 },
            generation: 1, expected: availability(config), batch_commands: Vec::new(), acquire: Mode::Ready, publish: Mode::Ready,
            apply: Mode::Ready, active_permits: 0, acquires: 0, trace: Vec::new(), effects: Vec::new(), hide: false,
        }));
        let replica = Replica::new(*id, config.clone(), Limits::default(), 16).unwrap();
        let member = immediate(AppliedReplica::recover(replica, Backend(Rc::clone(&state)), 1, 16)).unwrap();
        Node { member, state }
    }).collect()
}
fn pump(nodes: &mut [Node], messages: Vec<Envelope<u64>>, reachable: &[u128]) {
    let mut messages: VecDeque<_> = messages.into();
    let mut iterations = 0;
    while let Some(message) = messages.pop_front() {
        iterations += 1;
        assert!(iterations < 4096);
        if !reachable.contains(&message.from.0) || !reachable.contains(&message.to.0) { continue; }
        let node = nodes.iter_mut().find(|node| node.member.id() == message.to).unwrap();
        let output = immediate(node.member.step(Event::Receive(message))).unwrap();
        messages.extend(output.consensus.messages);
    }
}
fn apply_all(node: &mut Node) { while immediate(node.member.apply_next()).unwrap().is_some() {} }
fn elect(nodes: &mut [Node], reachable: &[u128]) {
    let out = immediate(nodes[0].member.step(Event::ElectionTimeout)).unwrap();
    pump(nodes, out.consensus.messages, reachable);
    let out = immediate(nodes[0].member.step(Event::Heartbeat)).unwrap();
    pump(nodes, out.consensus.messages, reachable);
    for node in nodes { apply_all(node); }
}
fn submit(node: &mut Node, command: u64) -> MemberProposalOutput<u64> {
    let input = node.state.borrow().expected.clone();
    immediate(node.member.propose_available(command, &input, AvailabilityLimits::default(), &mut || Ok::<_, ()>(()))).unwrap()
}

#[test]
fn owned_authority_publishes_before_apply_and_never_reports_client_success() {
    let mut nodes = nodes(&configuration(1));
    elect(&mut nodes, &[1]);
    let node = &mut nodes[0];
    let before = node.member.progress().unwrap();
    node.state.borrow_mut().trace.clear();
    let out = submit(node, 77);
    assert_eq!(out.member.consensus.role, Role::Leader);
    assert_eq!(out.position.index, 2);
    assert_eq!(out.member.consensus.committed[0].entry.command, Some(77));
    assert_eq!(node.member.progress().unwrap(), before);
    assert_eq!(node.state.borrow().trace, ["acquire", "authorized-raft", "permit-drop"]);
    assert_eq!(node.state.borrow().active_permits, 0);
    assert_eq!(node.state.borrow().root.as_ref().unwrap().entries()[1].command, Some(77));
    apply_all(node);
    assert_eq!(node.member.progress().unwrap().visible_index, 2);
    assert_eq!(node.state.borrow().effects, [(2, 77)]);
}

#[test]
fn basis_and_coverage_fail_before_owned_authority_and_refusal_leaves_no_append() {
    let mut nodes = nodes(&configuration(1));
    elect(&mut nodes, &[1]);
    let node = &mut nodes[0];
    let original = node.state.borrow().expected.clone();
    let before = node.member.durable_state().unwrap().clone();
    for mutation in 0..3 {
        let mut input = original.clone();
        match mutation { 0 => input.policy.basis.configuration[0] ^= 1,
            1 => input.receipts.clear(), _ => node.state.borrow_mut().acquire = Mode::Refuse }
        assert!(immediate(node.member.propose_available(9, &input, AvailabilityLimits::default(), &mut || Ok::<_, ()>(()))).is_err());
        assert_eq!(node.member.durable_state().unwrap(), &before);
        assert_eq!(node.state.borrow().acquires, usize::from(mutation == 2));
        assert_eq!(node.state.borrow().active_permits, 0);
    }
    node.state.borrow_mut().acquire = Mode::Ready;
    assert_eq!(submit(node, 9).position.index, 2);
}

#[test]
fn authority_cancellation_and_post_acquire_checkpoint_release_only_ephemeral_custody() {
    let mut nodes = nodes(&configuration(1));
    elect(&mut nodes, &[1]);
    let node = &mut nodes[0];
    let input = node.state.borrow().expected.clone();
    let before = node.member.durable_state().unwrap().clone();
    node.state.borrow_mut().acquire = Mode::Suspend;
    cancel(node.member.propose_available(9, &input, AvailabilityLimits::default(), &mut || Ok::<_, ()>(()))) ;
    assert_eq!(node.member.durable_state().unwrap(), &before);
    node.state.borrow_mut().acquire = Mode::Ready;
    let state = Rc::clone(&node.state);
    let result = immediate(node.member.propose_available(9, &input, AvailabilityLimits::default(), &mut || {
        if state.borrow().active_permits == 1 { Err("cancel before append") } else { Ok(()) }
    }));
    assert!(matches!(result, Err(MemberProposalError::Proposal(ProposalError::Interrupted(_)))));
    assert_eq!(node.member.durable_state().unwrap(), &before);
    assert_eq!(node.state.borrow().active_permits, 0);
}

#[test]
fn every_uncertain_authorized_publication_fences_the_whole_member() {
    for mode in [Mode::Refuse, Mode::FailAfter, Mode::SuspendAfter, Mode::Panic] {
        let mut nodes = nodes(&configuration(1));
        elect(&mut nodes, &[1]);
        let node = &mut nodes[0];
        let input = node.state.borrow().expected.clone();
        node.state.borrow_mut().publish = mode;
        if mode == Mode::SuspendAfter {
            cancel(node.member.propose_available(9, &input, AvailabilityLimits::default(), &mut || Ok::<_, ()>(()))) ;
        } else if mode == Mode::Panic {
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| { submit(node, 9); })).is_err());
        } else {
            assert!(immediate(node.member.propose_available(9, &input, AvailabilityLimits::default(), &mut || Ok::<_, ()>(()))).is_err());
        }
        assert!(node.member.progress().is_err());
        assert!(immediate(node.member.step(Event::Heartbeat)).is_err());
        assert!(immediate(node.member.apply_next()).is_err());
        assert_eq!(node.state.borrow().active_permits, 0);
        let retained = node.state.borrow().root.as_ref().unwrap().entries().len();
        assert_eq!(retained, if matches!(mode, Mode::FailAfter | Mode::SuspendAfter) { 2 } else { 1 });
    }
}

#[test]
fn availability_is_not_a_voting_quorum_and_apply_is_not_audit_visibility() {
    let mut nodes = nodes(&configuration(3));
    elect(&mut nodes, &[1, 2, 3]);
    nodes[0].state.borrow_mut().hide = true;
    let out = submit(&mut nodes[0], 77);
    assert!(out.member.consensus.committed.is_empty());
    assert!(immediate(nodes[0].member.apply_next()).unwrap().is_none());
    pump(&mut nodes, out.member.consensus.messages, &[1, 2]);
    apply_all(&mut nodes[0]);
    assert_eq!(nodes[0].member.progress().unwrap().applied.index, 2);
    assert_eq!(nodes[0].member.progress().unwrap().visible_index, 1);
    let (id, out) = immediate(nodes[0].member.read_index()).unwrap();
    pump(&mut nodes, out.consensus.messages, &[1, 2]);
    // A preceding in-flight append can require another fresh read probe.
    let out = immediate(nodes[0].member.step(Event::Heartbeat)).unwrap();
    pump(&mut nodes, out.consensus.messages, &[1, 2]);
    assert!(matches!(immediate(nodes[0].member.try_read(&id)).unwrap(), ReadState::PendingAudit { .. }));
    let out = submit(&mut nodes[0], 0);
    pump(&mut nodes, out.member.consensus.messages, &[1, 2]);
    apply_all(&mut nodes[0]);
    match immediate(nodes[0].member.try_read(&id)).unwrap() {
        ReadState::Ready(read) => assert_eq!(read.view(), &[77, 0]),
        _ => panic!("ordered audit-release model must release the pending read"),
    }
}

#[test]
fn no_member_can_acquire_proposal_authority_after_application_fencing_or_as_follower() {
    let mut nodes = nodes(&configuration(3));
    let input = availability(&configuration(3));
    assert!(immediate(nodes[1].member.propose_available(1, &input, AvailabilityLimits::default(), &mut || Ok::<_, ()>(()))).is_err());
    assert_eq!(nodes[1].state.borrow().acquires, 0);
    elect(&mut nodes, &[1, 2, 3]);
    let out = submit(&mut nodes[0], 1);
    pump(&mut nodes, out.member.consensus.messages, &[1, 2, 3]);
    nodes[0].state.borrow_mut().apply = Mode::Refuse;
    assert!(immediate(nodes[0].member.apply_next()).is_err());
    let acquired = nodes[0].state.borrow().acquires;
    assert!(immediate(nodes[0].member.propose_available(2, &input, AvailabilityLimits::default(), &mut || Ok::<_, ()>(()))).is_err());
    assert_eq!(nodes[0].state.borrow().acquires, acquired);
}
