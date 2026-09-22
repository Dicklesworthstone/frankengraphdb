use super::super::tests::{
    Command, MemoryApplication, MemoryState, Mode, NoopWake, genesis, immediate, memory, oid,
};
use super::*;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::future::{pending, ready};
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Waker};

use crate::SnapshotPublication;
use crate::application::ApplicationBatch;
use fgdb_chronicle::seed::{ObjectPublication, SeedAnchor, SeedLimits, SeedObjectSpec};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_chronicle::transfer::VerifiedObject;
use fgdb_order::{Configuration, Domain, Envelope, Limits, Message, SnapshotCut};

#[allow(dead_code)]
#[path = "../../../fgdb-chronicle/tests/support/bonded.rs"]
mod crypto;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    Healthy,
    Error,
    Pending,
    WrongCut,
    WrongBasis,
    WrongRoot,
}

struct IoState {
    raft: Option<PersistentState<Command>>,
    raft_writes: usize,
    raft_fault: Fault,
    pin_fault: Fault,
    load_fault: Fault,
    reload_after_seed: Fault,
    objects: BTreeSet<[u8; 32]>,
    pin_calls: usize,
    active_pins: Rc<Cell<usize>>,
}

struct Backend {
    app: MemoryApplication,
    io: Rc<RefCell<IoState>>,
}

/// In-memory root and pin model. This does not certify filesystem durability,
/// canonical snapshot parsing, Warden authority or distributed audit signatures.
struct View {
    values: Vec<u64>,
    active: Rc<Cell<usize>>,
}
impl Drop for View {
    fn drop(&mut self) {
        self.active.set(self.active.get() - 1);
    }
}

impl Application<Command> for Backend {
    type Error = &'static str;
    fn load(&mut self) -> impl Future<Output = Result<ApplicationProgress, Self::Error>> {
        let fault = self.io.borrow().load_fault;
        let loaded = self.app.load();
        async move {
            let mut progress = loaded.await?;
            match fault {
                Fault::Error => return Err("root reload unavailable"),
                Fault::Pending => pending::<()>().await,
                Fault::WrongRoot => progress.state_root = oid(999),
                Fault::WrongBasis => progress.publication_root = oid(999),
                Fault::WrongCut => progress.applied.index += 1,
                Fault::Healthy => {}
            }
            Ok(progress)
        }
    }
    fn apply(
        &mut self,
        batch: ApplicationBatch<'_, Command>,
    ) -> impl Future<Output = Result<ApplicationProgress, Self::Error>> {
        self.app.apply(batch)
    }
}

impl RaftPublisher<Command> for Backend {
    type Error = &'static str;
    fn publish(
        &mut self,
        state: &PersistentState<Command>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let mut io = self.io.borrow_mut();
        io.raft = Some(state.clone());
        io.raft_writes += 1;
        let fault = io.raft_fault;
        drop(io);
        async move {
            if fault == Fault::Pending {
                pending::<()>().await;
            }
            if fault == Fault::Error {
                Err("Raft root publication unknown")
            } else {
                Ok(())
            }
        }
    }
}

impl ReadApplication<Command> for Backend {
    type View = View;
    fn pin_visible(
        &mut self,
        basis: &ApplicationProgress,
        at: AppliedPosition,
    ) -> impl Future<Output = Result<PinnedApplication<View>, <Self as Application<Command>>::Error>>
    {
        let mut io = self.io.borrow_mut();
        io.pin_calls += 1;
        io.active_pins.set(io.active_pins.get() + 1);
        let active = Rc::clone(&io.active_pins);
        let fault = io.pin_fault;
        let store = self.app.0.borrow();
        assert_eq!(&store.progress, basis);
        assert!(at.index <= basis.visible_index);
        let selected: Vec<_> = store
            .values
            .iter()
            .filter(|(index, _)| *index <= at.index)
            .copied()
            .collect();
        let state_root = if at == basis.applied {
            basis.state_root
        } else {
            selected
                .last()
                .map_or(oid(10), |(index, _)| oid(1000 + index))
        };
        let mut pinned = PinnedApplication {
            basis: *basis,
            at,
            state_root,
            view: View {
                values: selected.into_iter().map(|(_, value)| value).collect(),
                active,
            },
        };
        match fault {
            Fault::WrongCut => pinned.at.index += 1,
            Fault::WrongBasis => pinned.basis.publication_generation += 1,
            Fault::WrongRoot => pinned.state_root = oid(999),
            _ => {}
        }
        drop(store);
        drop(io);
        async move {
            if fault == Fault::Pending {
                pending::<()>().await;
            }
            if fault == Fault::Error {
                Err("pin unavailable")
            } else {
                Ok(pinned)
            }
        }
    }
}

impl SeedPublisher<Command> for Backend {
    type Error = &'static str;
    fn publish_object(
        &mut self,
        object: ObjectPublication<'_>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.io
            .borrow_mut()
            .objects
            .insert(object.object().object_id().0);
        ready(Ok(()))
    }
    fn publish_snapshot(
        &mut self,
        snapshot: SnapshotPublication<'_, Command>,
    ) -> impl Future<Output = Result<RootPublicationEvidence, Self::Error>> {
        let anchor = snapshot.seed().plan().anchor();
        let mut io = self.io.borrow_mut();
        for object in snapshot.seed().plan().objects() {
            assert!(io.objects.contains(&object.object_id.0));
        }
        // One model transition installs both sides. There is no await between
        // the writes; real implementations must use the one canonical root.
        io.raft = Some(snapshot.consensus().clone());
        io.raft_writes += 1;
        io.load_fault = io.reload_after_seed;
        let mut store = self.app.0.borrow_mut();
        store.progress = ApplicationProgress {
            domain: Domain(anchor.consensus_domain),
            configuration: anchor.configuration,
            applied: AppliedPosition {
                index: anchor.raft_index,
                term: anchor.raft_term,
            },
            visible_index: anchor.raft_index,
            state_root: anchor.state_root,
            publication_root: anchor.publication_root,
            publication_generation: anchor.publication_generation,
        };
        store.values.clear();
        ready(Ok(RootPublicationEvidence {
            written_index: 1,
            slot_generation: anchor.publication_generation,
            root_manifest_oid: anchor.publication_root.0,
        }))
    }
}

type Member = AppliedReplica<Command, Backend>;
type Model = (Rc<RefCell<MemoryState>>, Rc<RefCell<IoState>>);

fn backend(progress: ApplicationProgress) -> (Backend, Model) {
    let (app, store) = memory(progress);
    let io = Rc::new(RefCell::new(IoState {
        raft: None,
        raft_writes: 0,
        raft_fault: Fault::Healthy,
        pin_fault: Fault::Healthy,
        load_fault: Fault::Healthy,
        reload_after_seed: Fault::Healthy,
        objects: BTreeSet::new(),
        pin_calls: 0,
        active_pins: Rc::new(Cell::new(0)),
    }));
    (
        Backend {
            app,
            io: Rc::clone(&io),
        },
        (store, io),
    )
}

fn cluster(count: u128, max_reads: usize) -> (Vec<Member>, Vec<Model>) {
    let configuration =
        Configuration::stable(Domain([1; 32]), [2; 32], (1..=count).map(MemberId), []).unwrap();
    let mut members = Vec::new();
    let mut models = Vec::new();
    for id in 1..=count {
        let state = PersistentState::from_authenticated_parts(
            configuration.clone(),
            0,
            None,
            0,
            Vec::new(),
        );
        let replica = Replica::recover(MemberId(id), state.clone(), Limits::default(), 16).unwrap();
        let (backend, model) = backend(genesis());
        model.1.borrow_mut().raft = Some(state);
        members.push(immediate(Member::recover(replica, backend, 1, max_reads)).unwrap());
        models.push(model);
    }
    (members, models)
}

fn pump(members: &mut [Member], messages: Vec<Envelope<Command>>, reachable: &[u128]) {
    let mut queue: VecDeque<_> = messages.into();
    let mut count = 0;
    while let Some(envelope) = queue.pop_front() {
        count += 1;
        assert!(count < 4096, "network dispatch must quiesce");
        if reachable.contains(&envelope.from.0) && reachable.contains(&envelope.to.0) {
            let output =
                immediate(members[envelope.to.0 as usize - 1].step(Event::Receive(envelope)))
                    .unwrap();
            queue.extend(output.consensus.messages);
        }
    }
}

fn elect(members: &mut [Member]) {
    let all: Vec<_> = (1..=members.len() as u128).collect();
    let output = immediate(members[0].step(Event::ElectionTimeout)).unwrap();
    pump(members, output.consensus.messages, &all);
    assert_eq!(members[0].replica.role().unwrap(), Role::Leader);
}

fn propose(members: &mut [Member], command: Command) {
    let all: Vec<_> = (1..=members.len() as u128).collect();
    let output = immediate(members[0].step(Event::Propose(command))).unwrap();
    pump(members, output.consensus.messages, &all);
}

fn confirm_read(members: &mut [Member]) -> ReadIndexId {
    let all: Vec<_> = (1..=members.len() as u128).collect();
    let (id, output) = immediate(members[0].read_index()).unwrap();
    pump(members, output.consensus.messages, &all);
    // An earlier in-flight request can require another heartbeat before a fresh
    // append is issued. None of these rounds perform application work.
    for _ in 0..3 {
        let output = immediate(members[0].step(Event::Heartbeat)).unwrap();
        pump(members, output.consensus.messages, &all);
    }
    assert!(
        members[0]
            .waiting
            .iter()
            .find(|read| read.id == id)
            .unwrap()
            .quorum
            .is_some()
    );
    id
}

fn apply_all(member: &mut Member) {
    for _ in 0..64 {
        if immediate(member.apply_next()).unwrap().is_none() {
            return;
        }
    }
    panic!("bounded fixture did not finish applying");
}

#[test]
fn three_member_commit_apply_audit_and_pin_are_separate_gates() {
    let (mut nodes, models) = cluster(3, 16);
    elect(&mut nodes);
    propose(&mut nodes, Command::Put(10));
    for (member, model) in nodes.iter().zip(&models) {
        assert_eq!(member.durable_state().unwrap().commit_index(), 2);
        assert_eq!(
            model.0.borrow().calls,
            0,
            "append replies cannot wait for application"
        );
    }
    let read = confirm_read(&mut nodes);
    assert!(matches!(
        immediate(nodes[0].try_read(&read)).unwrap(),
        ReadState::PendingApplication {
            required: 2,
            applied: 0
        }
    ));
    apply_all(&mut nodes[0]);
    assert!(matches!(
        immediate(nodes[0].try_read(&read)).unwrap(),
        ReadState::PendingAudit {
            required: 2,
            visible: 1
        }
    ));
    assert_eq!(models[0].1.borrow().pin_calls, 0);
    propose(&mut nodes, Command::ReleaseThrough(2));
    apply_all(&mut nodes[0]);
    let ReadState::Ready(snapshot) = immediate(nodes[0].try_read(&read)).unwrap() else {
        panic!("not ready")
    };
    assert_eq!(snapshot.required_index(), 2);
    assert_eq!(snapshot.at().index, 3);
    assert_eq!(snapshot.view().values, [10]);
    assert!(matches!(
        immediate(nodes[0].try_read(&read)),
        Err(ApplicationError::State(ApplicationStateError::UnknownRead))
    ));
    assert_eq!(models[0].1.borrow().active_pins.get(), 1);
    drop(nodes);
    assert_eq!(
        snapshot.view().values,
        [10],
        "owned pin survives member destruction"
    );
    drop(snapshot);
    assert_eq!(models[0].1.borrow().active_pins.get(), 0);
}

#[test]
fn a_ready_read_pins_visible_history_not_the_hidden_applied_tail() {
    let (mut nodes, models) = cluster(3, 16);
    elect(&mut nodes);
    propose(&mut nodes, Command::Put(10));
    propose(&mut nodes, Command::ReleaseThrough(2));
    apply_all(&mut nodes[0]);
    let early = confirm_read(&mut nodes);
    propose(&mut nodes, Command::Put(20));
    apply_all(&mut nodes[0]);
    assert_eq!(nodes[0].progress().unwrap().applied.index, 4);
    assert_eq!(nodes[0].progress().unwrap().visible_index, 3);
    assert_eq!(models[0].0.borrow().values, [(2, 10), (4, 20)]);
    let ReadState::Ready(snapshot) = immediate(nodes[0].try_read(&early)).unwrap() else {
        panic!("not ready")
    };
    assert_eq!(snapshot.at().index, 3);
    assert_eq!(snapshot.view().values, [10], "hidden suffix must not leak");
    let late = confirm_read(&mut nodes);
    assert!(matches!(
        immediate(nodes[0].try_read(&late)).unwrap(),
        ReadState::PendingAudit { required: 4, .. }
    ));
    propose(&mut nodes, Command::ReleaseThrough(4));
    apply_all(&mut nodes[0]);
    let ReadState::Ready(later) = immediate(nodes[0].try_read(&late)).unwrap() else {
        panic!("not ready")
    };
    assert_eq!(later.view().values, [10, 20]);
    assert_eq!(snapshot.view().values, [10]);
}

#[test]
fn losing_leadership_cancels_even_already_confirmed_unpinned_reads() {
    let (mut nodes, _) = cluster(3, 16);
    elect(&mut nodes);
    let id = confirm_read(&mut nodes);
    let state = nodes[0].durable_state().unwrap();
    let term = state.term();
    let message = Envelope {
        domain: state.configuration().domain(),
        configuration: state.configuration().identity(),
        from: MemberId(2),
        to: MemberId(1),
        message: Message::RequestVote {
            term: term + 1,
            last_index: 1,
            last_term: term,
        },
    };
    let output = immediate(nodes[0].step(Event::Receive(message))).unwrap();
    assert_eq!(output.leadership_lost, [id.clone()]);
    assert_eq!(nodes[0].pending_reads(), 0);
    assert!(matches!(
        immediate(nodes[0].try_read(&id)),
        Err(ApplicationError::State(ApplicationStateError::UnknownRead))
    ));
}

#[test]
fn confirmed_audit_waiters_still_consume_the_admission_budget() {
    let (mut nodes, _) = cluster(1, 1);
    elect(&mut nodes);
    let (first, _) = immediate(nodes[0].read_index()).unwrap();
    assert_eq!(nodes[0].replica.pending_reads(), 0);
    assert_eq!(nodes[0].pending_reads(), 1);
    assert!(matches!(
        immediate(nodes[0].read_index()),
        Err(MemberError::State(ApplicationStateError::ReadBackpressure))
    ));
    assert!(nodes[0].cancel_read(&first));
    assert!(!nodes[0].cancel_read(&first));
    let (second, _) = immediate(nodes[0].read_index()).unwrap();
    assert_ne!(first, second);
}

#[test]
fn partitioned_leader_cannot_pin_a_read_without_a_new_quorum() {
    let (mut nodes, models) = cluster(3, 16);
    elect(&mut nodes);
    apply_all(&mut nodes[0]);
    let (id, output) = immediate(nodes[0].read_index()).unwrap();
    pump(&mut nodes, output.consensus.messages, &[1]);
    assert!(matches!(
        immediate(nodes[0].try_read(&id)).unwrap(),
        ReadState::PendingQuorum
    ));
    assert_eq!(models[0].1.borrow().pin_calls, 0);
}

#[test]
fn pin_cancellation_releases_the_guard_but_retains_the_original_read() {
    let (mut nodes, models) = cluster(1, 16);
    elect(&mut nodes);
    apply_all(&mut nodes[0]);
    let (id, _) = immediate(nodes[0].read_index()).unwrap();
    models[0].1.borrow_mut().pin_fault = Fault::Pending;
    let writes = models[0].1.borrow().raft_writes;
    {
        let mut future = pin!(nodes[0].try_read(&id));
        let waker = Waker::from(Arc::new(NoopWake));
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        assert_eq!(models[0].1.borrow().active_pins.get(), 1);
    }
    assert_eq!(models[0].1.borrow().active_pins.get(), 0);
    assert_eq!(nodes[0].pending_reads(), 1);
    models[0].1.borrow_mut().pin_fault = Fault::Healthy;
    let ReadState::Ready(view) = immediate(nodes[0].try_read(&id)).unwrap() else {
        panic!("not ready")
    };
    assert_eq!(
        models[0].1.borrow().raft_writes,
        writes,
        "retry does not invent another quorum round"
    );
    assert_eq!(nodes[0].pending_reads(), 0);
    drop(view);
    assert_eq!(models[0].1.borrow().active_pins.get(), 0);
}

#[test]
fn bad_snapshot_pins_fence_the_whole_member_and_release_resources() {
    for fault in [Fault::WrongCut, Fault::WrongBasis, Fault::WrongRoot] {
        let (mut nodes, models) = cluster(1, 16);
        elect(&mut nodes);
        apply_all(&mut nodes[0]);
        let (id, _) = immediate(nodes[0].read_index()).unwrap();
        models[0].1.borrow_mut().pin_fault = fault;
        assert!(matches!(
            immediate(nodes[0].try_read(&id)),
            Err(ApplicationError::State(
                ApplicationStateError::InvalidReadSnapshot
            ))
        ));
        assert_eq!(models[0].1.borrow().active_pins.get(), 0);
        assert_eq!(
            nodes[0].progress(),
            Err(ApplicationStateError::RecoveryRequired)
        );
        assert!(matches!(
            immediate(nodes[0].step(Event::Heartbeat)),
            Err(MemberError::State(ApplicationStateError::RecoveryRequired))
        ));
    }
}

#[test]
fn unavailable_pin_is_retryable_without_readmission_or_leaking_a_guard() {
    let (mut nodes, models) = cluster(1, 16);
    elect(&mut nodes);
    apply_all(&mut nodes[0]);
    let (id, _) = immediate(nodes[0].read_index()).unwrap();
    models[0].1.borrow_mut().pin_fault = Fault::Error;
    assert!(matches!(
        immediate(nodes[0].try_read(&id)),
        Err(ApplicationError::Backend(_))
    ));
    assert_eq!(models[0].1.borrow().active_pins.get(), 0);
    assert_eq!(nodes[0].pending_reads(), 1);
    let writes = models[0].1.borrow().raft_writes;
    models[0].1.borrow_mut().pin_fault = Fault::Healthy;
    let ReadState::Ready(view) = immediate(nodes[0].try_read(&id)).unwrap() else {
        panic!("not ready")
    };
    assert_eq!(models[0].1.borrow().raft_writes, writes);
    drop(view);
    assert_eq!(models[0].1.borrow().active_pins.get(), 0);
}

#[test]
fn foreign_read_ids_cannot_cancel_or_consume_a_local_request() {
    let (mut first, _) = cluster(1, 16);
    let (mut second, _) = cluster(1, 16);
    elect(&mut first);
    elect(&mut second);
    apply_all(&mut first[0]);
    apply_all(&mut second[0]);
    let (local, _) = immediate(first[0].read_index()).unwrap();
    let (foreign, _) = immediate(second[0].read_index()).unwrap();
    assert_ne!(local, foreign);
    assert!(!first[0].cancel_read(&foreign));
    assert!(matches!(
        immediate(first[0].try_read(&foreign)),
        Err(ApplicationError::State(ApplicationStateError::UnknownRead))
    ));
    assert_eq!(first[0].pending_reads(), 1);
    assert!(matches!(
        immediate(first[0].try_read(&local)).unwrap(),
        ReadState::Ready(_)
    ));
}

#[test]
fn application_cancellation_fences_consensus_and_reads_until_root_recovery() {
    let (mut nodes, models) = cluster(1, 16);
    elect(&mut nodes);
    propose(&mut nodes, Command::Put(10));
    let (read, _) = immediate(nodes[0].read_index()).unwrap();
    models[0].0.borrow_mut().mode = Mode::SuspendAfter;
    {
        let mut future = pin!(nodes[0].apply_next());
        let waker = Waker::from(Arc::new(NoopWake));
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }
    assert!(matches!(
        immediate(nodes[0].try_read(&read)),
        Err(ApplicationError::State(
            ApplicationStateError::RecoveryRequired
        ))
    ));
    assert!(matches!(
        immediate(nodes[0].step(Event::Heartbeat)),
        Err(MemberError::State(ApplicationStateError::RecoveryRequired))
    ));
    let durable_raft = models[0].1.borrow().raft.clone().unwrap();
    drop(nodes);
    models[0].0.borrow_mut().mode = Mode::Normal;
    let backend = Backend {
        app: MemoryApplication(Rc::clone(&models[0].0)),
        io: Rc::clone(&models[0].1),
    };
    let replica = Replica::recover(MemberId(1), durable_raft, Limits::default(), 16).unwrap();
    let mut recovered = immediate(Member::recover(replica, backend, 1, 16)).unwrap();
    apply_all(&mut recovered);
    assert_eq!(models[0].0.borrow().values, [(2, 10)]);
    assert!(!recovered.cancel_read(&read));
    assert!(matches!(
        immediate(recovered.try_read(&read)),
        Err(ApplicationError::State(ApplicationStateError::UnknownRead))
    ));
}

#[test]
fn an_uncertain_raft_publication_blocks_an_already_visible_read() {
    let (mut nodes, models) = cluster(1, 16);
    elect(&mut nodes);
    apply_all(&mut nodes[0]);
    let (id, _) = immediate(nodes[0].read_index()).unwrap();
    models[0].1.borrow_mut().raft_fault = Fault::Error;
    assert!(immediate(nodes[0].step(Event::Propose(Command::Put(10)))).is_err());
    assert!(immediate(nodes[0].try_read(&id)).is_err());
    assert_eq!(models[0].1.borrow().pin_calls, 0);
}

#[test]
fn compaction_cannot_discard_unapplied_or_audit_hidden_entries() {
    let (mut nodes, models) = cluster(1, 16);
    elect(&mut nodes);
    let cut = |index, state_root| {
        SnapshotCut::from_authenticated_parts(
            &super::super::tests::config(),
            oid(80 + index).0,
            state_root,
            oid(90 + index).0,
            index,
            1,
        )
        .unwrap()
    };
    assert!(matches!(
        immediate(nodes[0].step(Event::Compact(cut(1, oid(10).0)))),
        Err(MemberError::State(
            ApplicationStateError::CompactionNotVisible
        ))
    ));
    apply_all(&mut nodes[0]);
    immediate(nodes[0].step(Event::Compact(cut(1, oid(10).0)))).unwrap();
    propose(&mut nodes, Command::Put(10));
    apply_all(&mut nodes[0]);
    assert!(matches!(
        immediate(nodes[0].step(Event::Compact(cut(2, oid(1002).0)))),
        Err(MemberError::State(
            ApplicationStateError::CompactionNotVisible
        ))
    ));
    propose(&mut nodes, Command::ReleaseThrough(2));
    apply_all(&mut nodes[0]);
    let calls = models[0].1.borrow().raft_writes;
    assert!(matches!(
        immediate(nodes[0].step(Event::Compact(cut(3, oid(999).0)))),
        Err(MemberError::State(
            ApplicationStateError::SnapshotStateMismatch
        ))
    ));
    assert_eq!(models[0].1.borrow().raft_writes, calls);
    immediate(nodes[0].step(Event::Compact(cut(3, oid(1002).0)))).unwrap();
    assert_eq!(
        nodes[0]
            .durable_state()
            .unwrap()
            .snapshot()
            .unwrap()
            .index(),
        3
    );
    assert!(immediate(nodes[0].apply_next()).unwrap().is_none());
}

struct CryptoSource {
    fixtures: Vec<crypto::Fixture>,
    calls: usize,
}
impl SeedObjectSource for CryptoSource {
    type Error = &'static str;
    fn recover(
        &mut self,
        spec: SeedObjectSpec,
    ) -> impl Future<Output = Result<VerifiedObject, Self::Error>> {
        self.calls += 1;
        ready(
            self.fixtures
                .iter()
                .find(|fixture| fixture.encoding.object_id() == spec.object_id)
                .map(crypto::Fixture::verified)
                .ok_or("missing fixture"),
        )
    }
}

fn offer_seed(nodes: &mut [Member]) -> (SnapshotTransfer, SeedPlan, CryptoSource) {
    let fixtures: Vec<_> = (180..184).map(crypto::Fixture::new).collect();
    let objects: Vec<_> = fixtures
        .iter()
        .map(|fixture| SeedObjectSpec {
            object_id: fixture.encoding.object_id(),
            object_kind: crypto::KIND,
            compressed_len: fixture.plaintext.len() as u64,
        })
        .collect();
    let config = nodes[1].durable_state().unwrap().configuration().clone();
    let cut = SnapshotCut::from_authenticated_parts(
        &config,
        objects[0].object_id.0,
        objects[1].object_id.0,
        objects[2].object_id.0,
        8,
        3,
    )
    .unwrap();
    let anchor = SeedAnchor {
        namespace: crypto::namespace(),
        consensus_domain: config.domain().0,
        configuration: config.identity(),
        snapshot_manifest: objects[0].object_id,
        state_root: objects[1].object_id,
        retention_floor: objects[2].object_id,
        publication_root: objects[3].object_id,
        publication_generation: 20,
        raft_index: 8,
        raft_term: 3,
        logical_command_seq: 5,
        commit_seq: 4,
    };
    // Synthetic authenticated inventory exercises composition, not the absent
    // production canonical snapshot-manifest verifier or graph interpreter.
    let plan =
        SeedPlan::from_authenticated_inventory(anchor, objects, SeedLimits::default()).unwrap();
    let output = immediate(nodes[1].step(Event::Receive(Envelope {
        domain: config.domain(),
        configuration: config.identity(),
        from: MemberId(1),
        to: MemberId(2),
        message: Message::InstallSnapshot {
            term: 3,
            request: 1,
            snapshot: cut,
        },
    })))
    .unwrap();
    (
        output
            .consensus
            .snapshot_transfers
            .into_iter()
            .next()
            .unwrap(),
        plan,
        CryptoSource { fixtures, calls: 0 },
    )
}

#[test]
fn bonded_seed_reloads_the_same_atomic_application_cut_before_acknowledging() {
    let (mut nodes, models) = cluster(3, 16);
    let (transfer, plan, mut source) = offer_seed(&mut nodes);
    let root = plan.anchor().state_root;
    let output =
        immediate(nodes[1].install_snapshot(crypto::namespace(), transfer, plan, &mut source))
            .unwrap();
    assert_eq!(source.calls, 4);
    assert_eq!(
        nodes[1].progress().unwrap().applied,
        AppliedPosition { index: 8, term: 3 }
    );
    assert_eq!(nodes[1].progress().unwrap().visible_index, 8);
    assert_eq!(nodes[1].progress().unwrap().state_root, root);
    assert!(
        output
            .consensus
            .messages
            .iter()
            .any(|message| matches!(message.message, Message::SnapshotInstalled { .. }))
    );
    assert_eq!(
        models[1].0.borrow().loads,
        2,
        "reload is mandatory after root installation"
    );
    assert!(immediate(nodes[1].apply_next()).unwrap().is_none());
}

#[test]
fn failed_or_cancelled_post_seed_reload_never_reactivates_the_old_application() {
    for fault in [
        Fault::Error,
        Fault::Pending,
        Fault::WrongCut,
        Fault::WrongRoot,
        Fault::WrongBasis,
    ] {
        let (mut nodes, models) = cluster(3, 16);
        let (transfer, plan, mut source) = offer_seed(&mut nodes);
        models[1].1.borrow_mut().reload_after_seed = fault;
        if fault == Fault::Pending {
            let mut future =
                pin!(nodes[1].install_snapshot(crypto::namespace(), transfer, plan, &mut source));
            let waker = Waker::from(Arc::new(NoopWake));
            assert!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
        } else {
            assert!(
                immediate(nodes[1].install_snapshot(
                    crypto::namespace(),
                    transfer,
                    plan,
                    &mut source
                ))
                .is_err()
            );
        }
        assert_eq!(
            models[1]
                .1
                .borrow()
                .raft
                .as_ref()
                .unwrap()
                .snapshot()
                .unwrap()
                .index(),
            8
        );
        assert_eq!(
            nodes[1].progress(),
            Err(ApplicationStateError::RecoveryRequired)
        );
        assert!(matches!(
            immediate(nodes[1].step(Event::Heartbeat)),
            Err(MemberError::State(ApplicationStateError::RecoveryRequired))
        ));
    }
}
