//! Public owning-driver composition. Roots and audit release are explicit
//! memory models, not canonical object digests, disk or Warden implementations.
use super::*;
use std::cell::RefCell;
use std::rc::Rc;
use fgdb_repl::application::{
    Application, ApplicationBatch, ApplicationError, ApplicationProgress,
    ApplicationStateError, AppliedPosition,
};
use fgdb_repl::application::member::{
    AppliedReplica, PinnedApplication, ReadApplication, ReadState,
};
use fgdb_types::ObjectId;

const HIDDEN_WRITE: u64 = 100;
const RELEASE_AUDIT: u64 = 200;
fn state_root(index: u64) -> ObjectId {
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&index.to_le_bytes());
    ObjectId(bytes)
}
struct Store {
    consensus: Option<PersistentState<u64>>,
    progress: ApplicationProgress,
    generation: u64,
    hidden: bool,
    values: Vec<u64>,
    history: BTreeMap<u64, Arc<Vec<u64>>>,
    suspend_pin: bool,
    fail_publication: bool,
}
struct Backend(Rc<RefCell<Store>>);
impl RaftPublisher<u64> for Backend {
    type Error = &'static str;
    async fn publish(&mut self, state: &PersistentState<u64>) -> Result<(), Self::Error> {
        let mut store = self.0.borrow_mut();
        store.consensus = Some(state.clone());
        store.generation += 1;
        if store.fail_publication { Err("uncertain root") } else { Ok(()) }
    }
}
impl Application<u64> for Backend {
    type Error = &'static str;
    async fn load(&mut self) -> Result<ApplicationProgress, Self::Error> {
        Ok(self.0.borrow().progress)
    }
    async fn apply(&mut self, batch: ApplicationBatch<'_, u64>) -> Result<ApplicationProgress, Self::Error> {
        let mut store = self.0.borrow_mut();
        assert_eq!(&store.progress, batch.basis());
        assert_eq!(store.consensus.as_ref(), Some(batch.consensus()));
        for (position, entry) in batch.indexed_entries() {
            match entry.command {
                Some(RELEASE_AUDIT) => store.hidden = false,
                Some(value) => {
                    store.values.push(value);
                    if value == HIDDEN_WRITE { store.hidden = true; }
                }
                None => {}
            }
            let snapshot = Arc::new(store.values.clone());
            store.history.insert(position.index, snapshot);
            if !store.hidden { store.progress.visible_index = position.index; }
        }
        store.generation += 1;
        store.progress = ApplicationProgress {
            applied: batch.last(),
            state_root: state_root(batch.last().index),
            publication_root: state_root(store.generation),
            publication_generation: store.generation,
            ..store.progress
        };
        Ok(store.progress)
    }
}
impl ReadApplication<u64> for Backend {
    type View = Arc<Vec<u64>>;
    async fn pin_visible(&mut self, basis: &ApplicationProgress, at: AppliedPosition)
        -> Result<PinnedApplication<Self::View>, Self::Error>
    {
        let (view, suspend) = {
            let store = self.0.borrow();
            assert_eq!(&store.progress, basis);
            assert!(at.index <= basis.visible_index);
            let view = Arc::clone(store.history.get(&at.index).ok_or("unretained history")?);
            (view, store.suspend_pin)
        };
        if suspend { pending::<()>().await; }
        Ok(PinnedApplication { basis: *basis, at, state_root: state_root(at.index), view })
    }
}
struct Host {
    member: AppliedReplica<u64, Backend>,
    peers: Cluster,
    store: Rc<RefCell<Store>>,
}
impl Host {
    fn new() -> Self {
        let configuration = stable(3, &[]);
        let store = Rc::new(RefCell::new(Store {
            consensus: None, generation: 1, hidden: false,
            values: Vec::new(), history: BTreeMap::new(),
            suspend_pin: false, fail_publication: false,
            progress: ApplicationProgress {
                domain: configuration.domain(), configuration: configuration.identity(),
                applied: AppliedPosition { index: 0, term: 0 }, visible_index: 0,
                state_root: state_root(0), publication_root: state_root(1), publication_generation: 1,
            },
        }));
        let replica = Replica::new(MemberId(1), configuration.clone(), Limits {
            max_log_entries: 128, max_append_entries: 1,
        }, 8).unwrap();
        let mut member = immediate(AppliedReplica::recover(replica,
            Backend(Rc::clone(&store)), 1, 8)).unwrap();
        member.configure_append_pipeline(4).unwrap();
        let peers = nodes(&configuration, 4).into_iter()
            .filter(|(id, _)| *id != MemberId(1)).collect();
        let mut host = Self { member, peers, store };
        let output = immediate(host.member.step(Event::ElectionTimeout)).unwrap();
        host.pump(output.consensus.messages);
        let output = immediate(host.member.step(Event::Heartbeat)).unwrap();
        host.pump(output.consensus.messages);
        assert_eq!(immediate(host.member.apply_next()).unwrap().unwrap().visible_index, 1);
        host
    }
    fn pump(&mut self, messages: Vec<Envelope<u64>>) {
        let mut queue: VecDeque<_> = messages.into();
        let mut steps = 0;
        while let Some(message) = queue.pop_front() {
            steps += 1;
            assert!(steps < 20_000);
            let messages = if message.to == MemberId(1) {
                let out = immediate(self.member.step(Event::Receive(message))).unwrap();
                assert!(out.leadership_lost.is_empty());
                out.consensus.messages
            } else {
                let out = self.peers.get_mut(&message.to).unwrap().step(Event::Receive(message));
                assert!(out.reads.is_empty());
                out.consensus.messages
            };
            queue.extend(messages);
        }
    }
    fn propose(&mut self, values: &[u64]) {
        let mut messages = Vec::new();
        for value in values {
            messages.extend(immediate(self.member.step(Event::Propose(*value))).unwrap().consensus.messages);
        }
        self.pump(messages);
    }
    fn read(&mut self) -> ReadIndexId {
        let (id, out) = immediate(self.member.read_index()).unwrap();
        self.pump(out.consensus.messages);
        id
    }
}

#[test]
fn pipelined_commit_to_apply_to_audit_release_to_pin_uses_one_backend_and_original_read() {
    let mut host = Host::new();
    host.propose(&[10, 11]);
    assert_eq!(host.member.durable_state().unwrap().commit_index(), 3);
    let read = host.read();
    assert!(matches!(immediate(host.member.try_read(&read)).unwrap(),
        ReadState::PendingApplication { required: 3, applied: 1 }));
    immediate(host.member.apply_next()).unwrap().unwrap();
    assert!(matches!(immediate(host.member.try_read(&read)).unwrap(),
        ReadState::PendingApplication { required: 3, applied: 2 }));
    immediate(host.member.apply_next()).unwrap().unwrap();
    let ReadState::Ready(visible) = immediate(host.member.try_read(&read)).unwrap() else { panic!() };
    assert_eq!(visible.id(), &read);
    assert_eq!(visible.required_index(), 3);
    assert_eq!(visible.view().as_ref(), &[10, 11]);
    assert!(matches!(immediate(host.member.try_read(&read)),
        Err(ApplicationError::State(ApplicationStateError::UnknownRead))));

    host.propose(&[HIDDEN_WRITE]);
    let pending_audit = host.read();
    immediate(host.member.apply_next()).unwrap().unwrap();
    assert!(matches!(immediate(host.member.try_read(&pending_audit)).unwrap(),
        ReadState::PendingAudit { required: 4, visible: 3 }));
    host.propose(&[RELEASE_AUDIT]); // audit wait cannot block consensus
    assert!(matches!(immediate(host.member.try_read(&pending_audit)).unwrap(),
        ReadState::PendingAudit { required: 4, visible: 3 }));
    immediate(host.member.apply_next()).unwrap().unwrap();
    let ReadState::Ready(released) = immediate(host.member.try_read(&pending_audit)).unwrap() else { panic!() };
    assert_eq!(released.id(), &pending_audit);
    assert_eq!(released.required_index(), 4);
    assert_eq!(released.at().index, 5);
    assert_eq!(released.view().as_ref(), &[10, 11, HIDDEN_WRITE]);
    assert_eq!(visible.view().as_ref(), &[10, 11]); // old pin remains immutable
    assert_eq!(host.member.pending_reads(), 0);
}

#[test]
fn earlier_floor_pins_history_not_hidden_tail_and_cancelled_pin_retains_the_read() {
    let mut host = Host::new();
    host.propose(&[10, 11]);
    immediate(host.member.apply_next()).unwrap();
    immediate(host.member.apply_next()).unwrap();
    let read = host.read(); // floor 3, do not pin yet
    host.propose(&[HIDDEN_WRITE]);
    immediate(host.member.apply_next()).unwrap();
    assert_eq!(host.member.progress().unwrap().applied.index, 4);
    assert_eq!(host.member.progress().unwrap().visible_index, 3);
    let references = Arc::strong_count(&host.store.borrow().history[&3]);
    host.store.borrow_mut().suspend_pin = true;
    {
        let mut work = Box::pin(host.member.try_read(&read));
        assert!(poll(work.as_mut()).is_pending());
        assert_eq!(Arc::strong_count(&host.store.borrow().history[&3]), references + 1);
    }
    assert_eq!(Arc::strong_count(&host.store.borrow().history[&3]), references);
    assert_eq!(host.member.pending_reads(), 1);
    host.store.borrow_mut().suspend_pin = false;
    let ReadState::Ready(visible) = immediate(host.member.try_read(&read)).unwrap() else { panic!() };
    assert_eq!(visible.required_index(), 3);
    assert_eq!(visible.at(), AppliedPosition { index: 3, term: 1 });
    assert_eq!(visible.state_root(), state_root(3));
    assert_eq!(visible.view().as_ref(), &[10, 11]);
    assert_eq!(host.store.borrow().values, vec![10, 11, HIDDEN_WRITE]);
    assert_eq!(host.member.pending_reads(), 0);
}

#[test]
fn owning_application_pipeline_configuration_preserves_both_publication_fences() {
    let mut host = Host::new();
    assert_eq!(host.member.configure_append_pipeline(2),
        Err(ApplicationStateError::Raft(RaftError::PipelineConfigurationBusy)));
    let before = host.member.progress().unwrap();
    host.store.borrow_mut().fail_publication = true;
    assert!(immediate(host.member.step(Event::Propose(10))).is_err());
    assert_eq!(host.member.configure_append_pipeline(2),
        Err(ApplicationStateError::Raft(RaftError::RecoveryRequired)));
    assert!(immediate(host.member.apply_next()).is_err());
    assert_eq!(host.store.borrow().progress, before);
}
