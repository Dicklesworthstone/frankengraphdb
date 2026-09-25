#![cfg(not(target_arch = "wasm32"))]

//! Existing owning replica/application drivers with a memory publication model.
//! A batch is prevalidated host input, NOT a client or payload-authority API.
//! Publication counts below are calls to that model, not measured disk fsyncs.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::{Future, pending, ready};
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use fgdb_order::{
    Configuration, Domain, Envelope, Error as RaftError, Event, Limits, MemberId, Message,
    PersistentState, Role,
};
use fgdb_repl::application::member::{
    AppliedReplica, AppliedReplicaOutput, PinnedApplication, ReadApplication, ReadState,
};
use fgdb_repl::application::{
    Application, ApplicationBatch, ApplicationError, ApplicationProgress, ApplicationStateError,
    AppliedPosition,
};
use fgdb_repl::driver::RaftPublisher;
use fgdb_repl::replica::Replica;
use fgdb_types::ObjectId;

const RELEASE: u64 = 100_000;
type Member = AppliedReplica<u64, Backend>;

fn poll<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
fn immediate<F: Future>(future: F) -> F::Output {
    match poll(pin!(future)) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("unexpected suspension"),
    }
}
fn oid(value: u64) -> ObjectId {
    let mut bytes = [0x71; 32];
    bytes[..8].copy_from_slice(&value.to_le_bytes());
    ObjectId(bytes)
}
fn config(count: u128) -> Configuration {
    Configuration::stable(Domain([1; 32]), [2; 32], (1..=count).map(MemberId), []).unwrap()
}
fn incoming(from: MemberId, message: Message<u64>) -> Event<u64> {
    Event::Receive(Envelope {
        domain: Domain([1; 32]),
        configuration: [2; 32],
        from,
        to: MemberId(1),
        message,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    FailBefore,
    FailAfter,
    SuspendAfter,
    PanicAfter,
}

struct Image {
    state: Option<PersistentState<u64>>,
    progress: ApplicationProgress,
    values: Vec<(u64, u64)>,
    applied: Vec<u64>,
    publishes: usize,
    applies: usize,
    pins: usize,
    mode: Mode,
}
struct Backend(Rc<RefCell<Image>>);

impl RaftPublisher<u64> for Backend {
    type Error = &'static str;
    fn publish(
        &mut self,
        state: &PersistentState<u64>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let mut image = self.0.borrow_mut();
        image.publishes += 1;
        let mode = image.mode;
        if mode != Mode::FailBefore {
            image.state = Some(state.clone());
        }
        if mode == Mode::PanicAfter {
            panic!("publisher constructor panicked after changing the model root");
        }
        drop(image);
        async move {
            if mode == Mode::SuspendAfter {
                pending::<()>().await;
            }
            if matches!(mode, Mode::FailBefore | Mode::FailAfter) {
                Err("publication outcome unknown")
            } else {
                Ok(())
            }
        }
    }
}
impl Application<u64> for Backend {
    type Error = &'static str;
    fn load(&mut self) -> impl Future<Output = Result<ApplicationProgress, Self::Error>> {
        ready(Ok(self.0.borrow().progress))
    }
    fn apply(
        &mut self,
        batch: ApplicationBatch<'_, u64>,
    ) -> impl Future<Output = Result<ApplicationProgress, Self::Error>> {
        let mut image = self.0.borrow_mut();
        assert_eq!(&image.progress, batch.basis());
        assert_eq!(image.state.as_ref().unwrap(), batch.consensus());
        let mut progress = image.progress;
        for (position, entry) in batch.indexed_entries() {
            image.applied.push(position.index);
            match entry.command {
                Some(RELEASE) => progress.visible_index = position.index,
                Some(value) => image.values.push((position.index, value)),
                None if progress.visible_index + 1 == position.index => {
                    progress.visible_index = position.index
                }
                None => {}
            }
        }
        progress.applied = batch.last();
        progress.state_root = oid(1000 + progress.applied.index);
        progress.publication_generation += 1;
        progress.publication_root = oid(2000 + progress.publication_generation);
        image.progress = progress;
        image.applies += 1;
        ready(Ok(progress))
    }
}
impl ReadApplication<u64> for Backend {
    type View = Vec<u64>;
    fn pin_visible(
        &mut self,
        basis: &ApplicationProgress,
        at: AppliedPosition,
    ) -> impl Future<Output = Result<PinnedApplication<Self::View>, Self::Error>> {
        let mut image = self.0.borrow_mut();
        assert_eq!(basis, &image.progress);
        assert_eq!(at.index, basis.visible_index);
        let view = image
            .values
            .iter()
            .filter(|(index, _)| *index <= at.index)
            .map(|(_, value)| *value)
            .collect();
        image.pins += 1;
        ready(Ok(PinnedApplication {
            basis: *basis,
            at,
            state_root: oid(1000 + at.index),
            view,
        }))
    }
}

// Explicit peer-ack model: only acknowledge the exact requests released to it.
// Kernel tests separately route the requests through real follower machines.
fn acknowledge(member: &mut Member, message: &Envelope<u64>) -> AppliedReplicaOutput<u64> {
    let Message::Append { term, request, .. } = &message.message else {
        panic!("not an append")
    };
    immediate(member.step(incoming(
        message.to,
        Message::Appended {
            term: *term,
            request: *request,
            success: true,
            conflict_next: 0,
        },
    )))
    .unwrap()
}
fn drain(member: &mut Member, messages: Vec<Envelope<u64>>) {
    let mut messages: VecDeque<_> = messages.into();
    let mut calls = 0;
    while let Some(message) = messages.pop_front() {
        if message.to != MemberId(2) || !matches!(&message.message, Message::Append { .. }) {
            continue;
        }
        calls += 1;
        assert!(calls < 100, "finite acknowledgements must quiesce");
        messages.extend(acknowledge(member, &message).consensus.messages);
    }
}
fn setup(count: u128, limits: Limits, apply_batch: usize) -> (Member, Rc<RefCell<Image>>) {
    let image = Rc::new(RefCell::new(Image {
        state: None,
        progress: ApplicationProgress {
            domain: Domain([1; 32]),
            configuration: [2; 32],
            applied: AppliedPosition { index: 0, term: 0 },
            visible_index: 0,
            state_root: oid(10),
            publication_root: oid(11),
            publication_generation: 1,
        },
        values: Vec::new(),
        applied: Vec::new(),
        publishes: 0,
        applies: 0,
        pins: 0,
        mode: Mode::Normal,
    }));
    let replica = Replica::new(MemberId(1), config(count), limits, 16).unwrap();
    let mut member = immediate(Member::recover(
        replica,
        Backend(Rc::clone(&image)),
        apply_batch,
        16,
    ))
    .unwrap();
    member.configure_append_pipeline(4).unwrap();
    let mut output = immediate(member.step(Event::ElectionTimeout)).unwrap();
    if count > 1 {
        output = immediate(member.step(incoming(
            MemberId(2),
            Message::Vote {
                term: 1,
                granted: true,
            },
        )))
        .unwrap();
    }
    assert_eq!(output.consensus.role, Role::Leader);
    drain(&mut member, output.consensus.messages);
    immediate(member.apply_next()).unwrap().unwrap();
    assert_eq!(member.progress().unwrap().applied.index, 1);
    (member, image)
}

#[test]
fn a_batch_uses_one_root_publication_instead_of_one_per_command() {
    for count in 1..=16 {
        let (mut batched, batch_image) = setup(1, Limits::default(), 1);
        let (mut scalar, scalar_image) = setup(1, Limits::default(), 1);
        let batch_before = batch_image.borrow().publishes;
        let scalar_before = scalar_image.borrow().publishes;
        let output =
            immediate(batched.step(Event::ProposeBatch((10..10 + count).collect()))).unwrap();
        for command in 10..10 + count {
            immediate(scalar.step(Event::Propose(command))).unwrap();
        }
        assert_eq!(batch_image.borrow().publishes - batch_before, 1);
        assert_eq!(
            scalar_image.borrow().publishes - scalar_before,
            count as usize
        );
        assert_eq!(
            batched.durable_state().unwrap(),
            scalar.durable_state().unwrap()
        );
        assert_eq!(output.consensus.committed.len(), count as usize);
        assert_eq!(batched.progress().unwrap().applied.index, 1); // append is not apply
        assert_eq!(batch_image.borrow().applies, 1);
    }
}

#[test]
fn batched_commit_still_waits_for_bounded_apply_and_audit_visibility() {
    let (mut member, image) = setup(1, Limits::default(), 1);
    immediate(member.step(Event::ProposeBatch(vec![10, 20, 30]))).unwrap();
    let (read, _) = immediate(member.read_index()).unwrap();
    assert!(matches!(
        immediate(member.try_read(&read)).unwrap(),
        ReadState::PendingApplication {
            required: 4,
            applied: 1
        }
    ));
    for index in 2..=4 {
        let applied = immediate(member.apply_next()).unwrap().unwrap();
        assert_eq!(applied.applied.index, index);
        assert_eq!(applied.visible_index, 1);
    }
    assert!(matches!(
        immediate(member.try_read(&read)).unwrap(),
        ReadState::PendingAudit {
            required: 4,
            visible: 1
        }
    ));
    immediate(member.step(Event::ProposeBatch(vec![RELEASE, 99]))).unwrap();
    immediate(member.apply_next()).unwrap();
    immediate(member.apply_next()).unwrap();
    let ReadState::Ready(view) = immediate(member.try_read(&read)).unwrap() else {
        panic!("read did not become visible")
    };
    assert_eq!(view.at().index, 5);
    assert_eq!(view.view(), &[10, 20, 30]); // 99 is applied, but still hidden
    assert_eq!(image.borrow().applied, [1, 2, 3, 4, 5, 6]);
    assert_eq!(image.borrow().pins, 1);
    assert!(immediate(member.try_read(&read)).is_err());
}

#[test]
fn a_multi_member_local_batch_is_not_a_quorum_commit() {
    let (mut member, image) = setup(3, Limits::default(), 1);
    let before = image.borrow().publishes;
    let output = immediate(member.step(Event::ProposeBatch(vec![10, 20]))).unwrap();
    assert!(output.consensus.committed.is_empty());
    assert_eq!(member.durable_state().unwrap().commit_index(), 1);
    assert!(immediate(member.apply_next()).unwrap().is_none());
    assert_eq!(image.borrow().publishes, before + 1);
    let request = output
        .consensus
        .messages
        .iter()
        .find(|message| message.to == MemberId(2))
        .unwrap();
    let committed = acknowledge(&mut member, request);
    assert_eq!(committed.consensus.committed.len(), 2);
    assert_eq!(image.borrow().publishes, before + 2); // separate commit publication
    assert_eq!(member.progress().unwrap().applied.index, 1);
    immediate(member.apply_next()).unwrap();
    assert_eq!(member.progress().unwrap().applied.index, 2);
    assert_eq!(member.progress().unwrap().visible_index, 1);
}

#[test]
fn invalid_batches_never_call_the_publisher_or_poison_the_member() {
    let (mut member, image) = setup(
        1,
        Limits {
            max_log_entries: 5,
            max_append_entries: 4,
        },
        1,
    );
    let before = image.borrow().publishes;
    let state = member.durable_state().unwrap().clone();
    for batch in [Vec::new(), vec![0; 5]] {
        assert!(immediate(member.step(Event::ProposeBatch(batch))).is_err());
        assert_eq!(member.durable_state().unwrap(), &state);
        assert_eq!(image.borrow().publishes, before);
    }
    immediate(member.step(Event::ProposeBatch(vec![1, 2, 3]))).unwrap();
    let state = member.durable_state().unwrap().clone();
    assert!(immediate(member.step(Event::ProposeBatch(vec![4, 5]))).is_err());
    assert_eq!(member.durable_state().unwrap(), &state);
    assert_eq!(image.borrow().publishes, before + 1);
}

#[test]
fn failed_cancelled_and_panicking_publication_recovers_all_or_no_new_entries() {
    for mode in [
        Mode::FailBefore,
        Mode::FailAfter,
        Mode::SuspendAfter,
        Mode::PanicAfter,
    ] {
        let (mut member, image) = setup(1, Limits::default(), 1);
        image.borrow_mut().mode = mode;
        if mode == Mode::SuspendAfter {
            let mut work = Box::pin(member.step(Event::ProposeBatch(vec![10, 20])));
            assert!(poll(work.as_mut()).is_pending());
            drop(work);
        } else if mode == Mode::PanicAfter {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    immediate(member.step(Event::ProposeBatch(vec![10, 20])))
                }))
                .is_err()
            );
        } else {
            assert!(immediate(member.step(Event::ProposeBatch(vec![10, 20]))).is_err());
        }
        assert!(matches!(
            member.progress(),
            Err(ApplicationStateError::Raft(RaftError::RecoveryRequired))
        ));
        assert!(immediate(member.step(Event::Heartbeat)).is_err());
        let state = image.borrow().state.clone().unwrap();
        let expected = if mode == Mode::FailBefore { 1 } else { 3 };
        assert_eq!(state.commit_index(), expected);
        assert_eq!(state.entries().len(), expected as usize);
        drop(member);
        image.borrow_mut().mode = Mode::Normal;
        let replica = Replica::recover(MemberId(1), state, Limits::default(), 16).unwrap();
        let mut member =
            immediate(Member::recover(replica, Backend(Rc::clone(&image)), 1, 16)).unwrap();
        while immediate(member.apply_next()).unwrap().is_some() {}
        if expected == 1 {
            assert!(image.borrow().values.is_empty());
        } else {
            assert_eq!(image.borrow().values, [(2, 10), (3, 20)]);
        }
        assert_eq!(image.borrow().applied.len(), expected as usize);
    }
}

#[test]
fn dropping_an_unpolled_batch_has_no_proposal_or_publication_effect() {
    let (mut member, image) = setup(1, Limits::default(), 1);
    let state = member.durable_state().unwrap().clone();
    let before = image.borrow().publishes;
    drop(member.step(Event::ProposeBatch(vec![10, 20])));
    assert_eq!(member.durable_state().unwrap(), &state);
    assert_eq!(image.borrow().publishes, before);
    immediate(member.step(Event::ProposeBatch(vec![10, 20]))).unwrap();
    assert_eq!(image.borrow().publishes, before + 1);
}

#[test]
fn a_pre_read_batch_reply_cannot_become_a_fresh_read_confirmation() {
    let (mut member, _) = setup(3, Limits::default(), 1);
    let batch = immediate(member.step(Event::ProposeBatch(vec![10, 20]))).unwrap();
    let old = batch
        .consensus
        .messages
        .iter()
        .find(|message| message.to == MemberId(2))
        .unwrap();
    let (read, retries) = immediate(member.read_index()).unwrap();
    assert!(
        retries
            .consensus
            .messages
            .iter()
            .any(|message| message == old)
    );
    assert!(matches!(
        immediate(member.try_read(&read)).unwrap(),
        ReadState::PendingQuorum
    ));
    let committed = acknowledge(&mut member, old);
    assert_eq!(member.durable_state().unwrap().commit_index(), 3);
    assert!(matches!(
        immediate(member.try_read(&read)).unwrap(),
        ReadState::PendingQuorum
    ));
    drain(&mut member, committed.consensus.messages);
    let ReadState::Ready(view) = immediate(member.try_read(&read)).unwrap() else {
        panic!("fresh confirmation missing")
    };
    assert_eq!(view.required_index(), 1); // captured before the batch committed
    assert!(view.view().is_empty());
    let (after_commit, output) = immediate(member.read_index()).unwrap();
    drain(&mut member, output.consensus.messages);
    assert!(matches!(
        immediate(member.try_read(&after_commit)).unwrap(),
        ReadState::PendingApplication {
            required: 3,
            applied: 1
        }
    ));
}

#[test]
fn leader_loss_cancels_a_read_waiting_on_a_batch_without_guessing_its_outcome() {
    let (mut member, _) = setup(3, Limits::default(), 1);
    let batch = immediate(member.step(Event::ProposeBatch(vec![10, 20]))).unwrap();
    let old = batch
        .consensus
        .messages
        .iter()
        .find(|message| message.to == MemberId(2))
        .unwrap();
    let (read, _) = immediate(member.read_index()).unwrap();
    let output = immediate(member.step(incoming(
        MemberId(2),
        Message::RequestVote {
            term: 2,
            last_index: 0,
            last_term: 0,
        },
    )))
    .unwrap();
    assert_eq!(output.leadership_lost, vec![read.clone()]);
    assert_eq!(output.consensus.role, Role::Follower);
    let late = acknowledge(&mut member, old);
    assert!(late.consensus.committed.is_empty());
    assert!(matches!(
        immediate(member.try_read(&read)),
        Err(ApplicationError::State(ApplicationStateError::UnknownRead))
    ));
    assert_eq!(member.durable_state().unwrap().entries().len(), 3); // unknown, not aborted
}
