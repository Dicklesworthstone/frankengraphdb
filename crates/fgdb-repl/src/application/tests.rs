use super::*;
use std::cell::RefCell;
use std::future::{pending, ready};
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::task::{Context, Poll, Wake, Waker};

use fgdb_order::{Configuration, Limits, MemberId, SnapshotCut};

pub(super) struct NoopWake;
impl Wake for NoopWake { fn wake(self: Arc<Self>) {} }

pub(super) fn immediate<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    match pin!(future).as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test operation unexpectedly suspended"),
    }
}

pub(super) fn oid(value: u64) -> ObjectId {
    let mut bytes = [0x5a; 32];
    bytes[..8].copy_from_slice(&value.to_le_bytes());
    ObjectId(bytes)
}

pub(super) fn config() -> Configuration {
    Configuration::stable(Domain([1; 32]), [2; 32], [MemberId(1)], []).unwrap()
}

pub(super) fn genesis() -> ApplicationProgress {
    ApplicationProgress {
        domain: config().domain(), configuration: config().identity(),
        applied: AppliedPosition { index: 0, term: 0 }, visible_index: 0,
        state_root: oid(10), publication_root: oid(11), publication_generation: 1,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Command { Put(u64), ReleaseThrough(u64) }

pub(super) fn entry(term: u64, command: Option<Command>) -> Entry<Command> {
    Entry { term, command }
}

pub(super) fn replica(entries: Vec<Entry<Command>>, commit: u64) -> Replica<Command> {
    let term = entries.last().map_or(1, |entry| entry.term);
    Replica::recover(MemberId(1), PersistentState::from_authenticated_parts(
        config(), term, None, commit, entries,
    ), Limits::default(), 16).unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Normal, FailBefore, FailAfter, SuspendAfter, PanicAfter,
    WrongPosition, WrongTerm, WrongDomain, WrongConfiguration,
    FutureVisibility, RegressVisibility, SameGeneration, SameRoot,
}

/// An explicitly memory-only publication model, NOT Chronicle disk evidence.
/// Mutating before failure models an unknown result of an atomic publication.
pub(super) struct MemoryState {
    pub(super) progress: ApplicationProgress,
    pub(super) values: Vec<(u64, u64)>,
    pub(super) batches: Vec<(u64, u64)>,
    pub(super) calls: usize,
    pub(super) loads: usize,
    pub(super) mode: Mode,
}

pub(super) struct MemoryApplication(pub(super) Rc<RefCell<MemoryState>>);

pub(super) fn memory(progress: ApplicationProgress) -> (MemoryApplication, Rc<RefCell<MemoryState>>) {
    let store = Rc::new(RefCell::new(MemoryState {
        progress, values: Vec::new(), batches: Vec::new(), calls: 0, loads: 0, mode: Mode::Normal,
    }));
    (MemoryApplication(Rc::clone(&store)), store)
}

impl Application<Command> for MemoryApplication {
    type Error = &'static str;

    fn load(&mut self) -> impl Future<Output = Result<ApplicationProgress, Self::Error>> {
        let mut store = self.0.borrow_mut();
        store.loads += 1;
        ready(Ok(store.progress))
    }

    fn apply(&mut self, batch: ApplicationBatch<'_, Command>)
        -> impl Future<Output = Result<ApplicationProgress, Self::Error>>
    {
        let mut store = self.0.borrow_mut();
        store.calls += 1;
        assert_eq!(&store.progress, batch.basis());
        assert!(!batch.entries().is_empty());
        assert!(batch.last().index <= batch.consensus().commit_index());
        let mode = store.mode;
        let mut result = store.progress;
        if mode != Mode::FailBefore {
            for (position, entry) in batch.indexed_entries() {
                match &entry.command {
                    Some(Command::Put(value)) => {
                        store.values.push((position.index, *value));
                        store.progress.state_root = oid(1000 + position.index);
                    }
                    Some(Command::ReleaseThrough(cut)) => {
                        assert!(*cut < position.index, "a test audit control cannot release the future");
                        store.progress.visible_index = store.progress.visible_index.max(*cut);
                        if *cut == position.index - 1 { store.progress.visible_index = position.index; }
                    }
                    None if store.progress.visible_index == position.index - 1 => {
                        store.progress.visible_index = position.index;
                    }
                    None => {}
                }
            }
            store.batches.push((batch.first_index(), batch.last().index));
            store.progress.applied = batch.last();
            store.progress.publication_generation += 1;
            store.progress.publication_root = oid(10 + store.progress.publication_generation);
            result = store.progress;
        }
        match mode {
            Mode::WrongPosition => result.applied = batch.basis().applied,
            Mode::WrongTerm => result.applied.term += 1,
            Mode::WrongDomain => result.domain = Domain([9; 32]),
            Mode::WrongConfiguration => result.configuration = [9; 32],
            Mode::FutureVisibility => result.visible_index = result.applied.index + 1,
            Mode::RegressVisibility => result.visible_index = batch.basis().visible_index.saturating_sub(1),
            Mode::SameGeneration => result.publication_generation = batch.basis().publication_generation,
            Mode::SameRoot => result.publication_root = batch.basis().publication_root,
            Mode::PanicAfter => panic!("publication panicked after changing the durable model"),
            _ => {}
        }
        // No borrow of the test store survives suspension.
        drop(store);
        async move {
            if mode == Mode::SuspendAfter { pending::<()>().await; }
            if matches!(mode, Mode::FailBefore | Mode::FailAfter) { Err("publication outcome unknown") }
            else { Ok(result) }
        }
    }
}

#[test]
fn bounded_replay_excludes_uncommitted_tail_and_never_reapplies() {
    let replica = replica(vec![
        entry(1, None), entry(1, Some(Command::Put(10))),
        entry(2, Some(Command::Put(20))), entry(2, Some(Command::ReleaseThrough(3))),
        entry(2, Some(Command::Put(99))),
    ], 4);
    let (backend, store) = memory(genesis());
    let mut app = immediate(ApplicationDriver::recover(backend, &replica, 2)).unwrap();
    let first = immediate(app.apply_next(&replica)).unwrap().unwrap();
    assert_eq!(first.applied, AppliedPosition { index: 2, term: 1 });
    assert_eq!(first.visible_index, 1);
    let second = immediate(app.apply_next(&replica)).unwrap().unwrap();
    assert_eq!(second.applied, AppliedPosition { index: 4, term: 2 });
    assert_eq!(second.visible_index, 4);
    assert_eq!(immediate(app.apply_next(&replica)).unwrap(), None);
    let store = store.borrow();
    assert_eq!(store.batches, [(1, 2), (3, 4)]);
    assert_eq!(store.values, [(2, 10), (3, 20)]);
    assert_eq!(store.calls, 2);
}

#[test]
fn noops_publish_the_cursor_without_becoming_semantic_commands() {
    let replica = replica(vec![entry(1, None), entry(1, None), entry(2, None)], 3);
    let (backend, store) = memory(genesis());
    let mut app = immediate(ApplicationDriver::recover(backend, &replica, 8)).unwrap();
    let progress = immediate(app.apply_next(&replica)).unwrap().unwrap();
    assert_eq!(progress.applied, AppliedPosition { index: 3, term: 2 });
    assert_eq!(progress.visible_index, 3);
    assert_eq!(progress.state_root, genesis().state_root);
    assert_ne!(progress.publication_root, genesis().publication_root);
    assert!(store.borrow().values.is_empty());
}

#[test]
fn hidden_commands_and_following_noops_need_an_ordered_visibility_control() {
    let replica = replica(vec![entry(1, Some(Command::Put(7))), entry(1, None),
        entry(1, Some(Command::ReleaseThrough(2)))], 3);
    let (backend, _) = memory(genesis());
    let mut app = immediate(ApplicationDriver::recover(backend, &replica, 1)).unwrap();
    for _ in 0..2 {
        assert_eq!(immediate(app.apply_next(&replica)).unwrap().unwrap().visible_index, 0);
    }
    assert_eq!(immediate(app.apply_next(&replica)).unwrap().unwrap().visible_index, 3);
}

#[test]
fn recovery_rejects_wrong_domains_cuts_and_impossible_visibility() {
    let replica = replica(vec![entry(1, None), entry(1, None), entry(2, None)], 3);
    let mut valid = genesis();
    valid.applied = AppliedPosition { index: 2, term: 1 };
    valid.visible_index = 1;
    for (change, expected) in [
        (0, ApplicationStateError::WrongDomain),
        (1, ApplicationStateError::WrongConfiguration),
        (2, ApplicationStateError::InvalidPosition),
        (3, ApplicationStateError::InvalidPosition),
        (4, ApplicationStateError::AppliedAheadOfCommit),
        (5, ApplicationStateError::InvalidPosition),
        (6, ApplicationStateError::InvalidPosition),
    ] {
        let mut bad = valid;
        match change {
            0 => bad.domain = Domain([9; 32]),
            1 => bad.configuration = [9; 32],
            2 => bad.publication_generation = 0,
            3 => bad.visible_index = 3,
            4 => bad.applied.index = 4,
            5 => bad.applied = AppliedPosition { index: 0, term: 1 },
            6 => bad.applied.term = 2,
            _ => unreachable!(),
        }
        let (backend, _) = memory(bad);
        assert!(matches!(immediate(ApplicationDriver::recover(backend, &replica, 2)),
            Err(ApplicationError::State(error)) if error == expected));
    }
}

#[test]
fn installed_snapshot_requires_exact_state_and_audit_safe_base() {
    let configuration = config();
    let cut = SnapshotCut::from_authenticated_parts(&configuration, oid(40).0, oid(50).0,
        oid(60).0, 5, 2).unwrap();
    let replica = Replica::recover(MemberId(1), PersistentState::from_authenticated_snapshot(
        configuration, 2, None, 6, cut, vec![entry(2, Some(Command::Put(70)))],
    ), Limits::default(), 16).unwrap();
    let mut progress = genesis();
    progress.applied = AppliedPosition { index: 5, term: 2 };
    progress.visible_index = 5;
    progress.state_root = oid(50);
    for change in 0..3 {
        let mut bad = progress;
        match change {
            0 => bad.applied.index = 4,
            1 => bad.visible_index = 4,
            _ => bad.state_root = oid(51),
        }
        let (backend, _) = memory(bad);
        assert!(immediate(ApplicationDriver::recover(backend, &replica, 2)).is_err());
    }
    let (backend, store) = memory(progress);
    let mut app = immediate(ApplicationDriver::recover(backend, &replica, 2)).unwrap();
    let applied = immediate(app.apply_next(&replica)).unwrap().unwrap();
    assert_eq!(applied.applied.index, 6);
    assert_eq!(applied.visible_index, 5);
    assert_eq!(store.borrow().values, [(6, 70)]);
}

#[test]
fn every_bad_or_uncertain_publication_permanently_fences_cached_state() {
    let replica = replica(vec![entry(1, Some(Command::Put(10)))], 1);
    for mode in [Mode::FailBefore, Mode::FailAfter, Mode::WrongPosition, Mode::WrongTerm,
        Mode::WrongDomain, Mode::WrongConfiguration, Mode::FutureVisibility,
        Mode::SameGeneration, Mode::SameRoot]
    {
        let (backend, store) = memory(genesis());
        store.borrow_mut().mode = mode;
        let mut app = immediate(ApplicationDriver::recover(backend, &replica, 2)).unwrap();
        assert!(immediate(app.apply_next(&replica)).is_err(), "{mode:?}");
        assert_eq!(app.progress(), Err(ApplicationStateError::RecoveryRequired));
        assert!(matches!(immediate(app.apply_next(&replica)),
            Err(ApplicationError::State(ApplicationStateError::RecoveryRequired))));
        assert_eq!(store.borrow().calls, 1);
    }
}

#[test]
fn visibility_must_never_regress_after_a_successful_apply() {
    let replica = replica(vec![entry(1, None), entry(1, None)], 2);
    let (backend, store) = memory(genesis());
    let mut app = immediate(ApplicationDriver::recover(backend, &replica, 1)).unwrap();
    immediate(app.apply_next(&replica)).unwrap();
    store.borrow_mut().mode = Mode::RegressVisibility;
    assert!(matches!(immediate(app.apply_next(&replica)),
        Err(ApplicationError::State(ApplicationStateError::VisibilityRegression))));
    assert_eq!(app.progress(), Err(ApplicationStateError::RecoveryRequired));
}

#[test]
fn cancellation_after_atomic_publish_recovers_without_duplicate_effects() {
    let replica = replica(vec![entry(1, Some(Command::Put(10))),
        entry(1, Some(Command::Put(20)))], 2);
    let (backend, store) = memory(genesis());
    store.borrow_mut().mode = Mode::SuspendAfter;
    let mut app = immediate(ApplicationDriver::recover(backend, &replica, 1)).unwrap();
    {
        let mut future = pin!(app.apply_next(&replica));
        let waker = Waker::from(Arc::new(NoopWake));
        assert!(future.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
    }
    assert_eq!(app.progress(), Err(ApplicationStateError::RecoveryRequired));
    assert_eq!(store.borrow().progress.applied.index, 1);
    drop(app);
    store.borrow_mut().mode = Mode::Normal;
    let mut recovered = immediate(ApplicationDriver::recover(
        MemoryApplication(Rc::clone(&store)), &replica, 1,
    )).unwrap();
    immediate(recovered.apply_next(&replica)).unwrap();
    assert_eq!(store.borrow().values, [(1, 10), (2, 20)]);
    assert_eq!(store.borrow().batches, [(1, 1), (2, 2)]);
}

#[test]
fn failure_before_publication_recovers_the_old_cursor_without_skipping() {
    let replica = replica(vec![entry(1, Some(Command::Put(10)))], 1);
    let (backend, store) = memory(genesis());
    store.borrow_mut().mode = Mode::FailBefore;
    let mut app = immediate(ApplicationDriver::recover(backend, &replica, 1)).unwrap();
    assert!(immediate(app.apply_next(&replica)).is_err());
    assert_eq!(store.borrow().progress.applied.index, 0);
    drop(app);
    store.borrow_mut().mode = Mode::Normal;
    let mut recovered = immediate(ApplicationDriver::recover(
        MemoryApplication(Rc::clone(&store)), &replica, 1,
    )).unwrap();
    immediate(recovered.apply_next(&replica)).unwrap();
    assert_eq!(store.borrow().values, [(1, 10)]);
}

#[test]
fn synchronous_backend_panic_is_fenced_before_future_creation() {
    let replica = replica(vec![entry(1, Some(Command::Put(10)))], 1);
    let (backend, store) = memory(genesis());
    store.borrow_mut().mode = Mode::PanicAfter;
    let mut app = immediate(ApplicationDriver::recover(backend, &replica, 1)).unwrap();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = immediate(app.apply_next(&replica));
    })).is_err());
    assert_eq!(app.progress(), Err(ApplicationStateError::RecoveryRequired));
    assert_eq!(store.borrow().values, [(1, 10)]);
}

#[test]
fn invalid_limits_do_not_load_or_mutate_the_backend() {
    let replica = replica(Vec::new(), 0);
    for limit in [0, 65_537, usize::MAX] {
        let (backend, store) = memory(genesis());
        assert!(matches!(immediate(ApplicationDriver::recover(backend, &replica, limit)),
            Err(ApplicationError::State(ApplicationStateError::InvalidLimits))));
        assert_eq!(store.borrow().loads, 0);
    }
}

#[test]
fn replay_borrows_commands_instead_of_cloning_the_retained_log() {
    #[derive(Debug)]
    struct Counted(Arc<AtomicUsize>);
    impl Clone for Counted {
        fn clone(&self) -> Self { self.0.fetch_add(1, Ordering::SeqCst); Self(Arc::clone(&self.0)) }
    }
    impl PartialEq for Counted { fn eq(&self, _: &Self) -> bool { true } }
    impl Eq for Counted {}
    struct CounterBackend(ApplicationProgress);
    impl Application<Counted> for CounterBackend {
        type Error = ();
        fn load(&mut self) -> impl Future<Output = Result<ApplicationProgress, ()>> { ready(Ok(self.0)) }
        fn apply(&mut self, batch: ApplicationBatch<'_, Counted>)
            -> impl Future<Output = Result<ApplicationProgress, ()>>
        {
            assert_eq!(batch.commands().count(), 2);
            self.0.applied = batch.last();
            self.0.publication_generation += 1;
            self.0.publication_root = oid(12);
            ready(Ok(self.0))
        }
    }
    let clones = Arc::new(AtomicUsize::new(0));
    let entries = (0..1000).map(|_| Entry { term: 1, command: Some(Counted(Arc::clone(&clones))) }).collect();
    let replica = Replica::recover(MemberId(1), PersistentState::from_authenticated_parts(
        config(), 1, None, 1000, entries,
    ), Limits::default(), 16).unwrap();
    let mut app = immediate(ApplicationDriver::recover(CounterBackend(genesis()), &replica, 2)).unwrap();
    immediate(app.apply_next(&replica)).unwrap();
    assert_eq!(clones.load(Ordering::SeqCst), 0);
    assert_eq!(app.progress().unwrap().applied.index, 2);
}
