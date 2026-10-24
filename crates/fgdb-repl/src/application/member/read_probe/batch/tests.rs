//! Batch admission through the production applied-member driver, with the
//! existing memory application model. No disk, transport or Warden proof.

use super::*;
use crate::application::member::{PinnedApplication, ReadApplication, ReadState};
use crate::application::tests::{
    Command, MemoryApplication, Mode, genesis, immediate, memory, oid,
};
use crate::application::{
    ApplicationBatch, ApplicationError, ApplicationProgress, AppliedPosition,
};
use crate::replica::{Replica, ReplicaError};
use fgdb_order::{Configuration, Domain, Envelope, Event, Limits, MemberId, PersistentState};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{Future, pending, ready};
use std::rc::Rc;
use std::task::{Context, Waker};

#[derive(Clone, Copy, PartialEq, Eq)]
enum PinFault {
    Healthy,
    Error,
    Pending,
}

struct Pins {
    calls: usize,
    active: Rc<Cell<usize>>,
    fault: PinFault,
    consensus: Option<PersistentState<Command>>,
    fail_publication: bool,
}

struct Backend {
    app: MemoryApplication,
    pins: Rc<RefCell<Pins>>,
}

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
        self.app.load()
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
        let mut pins = self.pins.borrow_mut();
        pins.consensus = Some(state.clone());
        ready(if pins.fail_publication {
            Err("Raft root publication outcome unknown")
        } else {
            Ok(())
        })
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
        let store = self.app.0.borrow();
        assert_eq!(store.progress, *basis);
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
        let mut pins = self.pins.borrow_mut();
        pins.calls += 1;
        pins.active.set(pins.active.get() + 1);
        let fault = pins.fault;
        let pinned = PinnedApplication {
            basis: *basis,
            at,
            state_root,
            view: View {
                values: selected.into_iter().map(|(_, value)| value).collect(),
                active: Rc::clone(&pins.active),
            },
        };
        drop(store);
        drop(pins);
        async move {
            if fault == PinFault::Pending {
                pending::<()>().await;
            }
            if fault == PinFault::Error {
                Err("pin refused")
            } else {
                Ok(pinned)
            }
        }
    }
}

type Member = AppliedReplica<Command, Backend>;

fn cluster(count: u128, application_limit: usize, replica_limit: usize) -> Vec<Member> {
    let configuration =
        Configuration::stable(Domain([1; 32]), [2; 32], (1..=count).map(MemberId), []).unwrap();
    (1..=count)
        .map(|id| {
            let replica = Replica::new(
                MemberId(id),
                configuration.clone(),
                Limits::default(),
                replica_limit,
            )
            .unwrap();
            let (app, _) = memory(genesis());
            let backend = Backend {
                app,
                pins: Rc::new(RefCell::new(Pins {
                    calls: 0,
                    active: Rc::new(Cell::new(0)),
                    fault: PinFault::Healthy,
                    consensus: None,
                    fail_publication: false,
                })),
            };
            immediate(Member::recover(replica, backend, 1, application_limit)).unwrap()
        })
        .collect()
}

fn pump(members: &mut [Member], messages: Vec<Envelope<Command>>) -> Vec<ReadIndexId> {
    let mut queue: VecDeque<_> = messages.into();
    let mut lost = Vec::new();
    let mut count = 0;
    while let Some(message) = queue.pop_front() {
        count += 1;
        assert!(count < 4096, "replication must quiesce");
        let output =
            immediate(members[message.to.0 as usize - 1].step(Event::Receive(message))).unwrap();
        queue.extend(output.consensus.messages);
        lost.extend(output.leadership_lost);
    }
    lost
}

fn elect(members: &mut [Member]) {
    let output = immediate(members[0].step(Event::ElectionTimeout)).unwrap();
    assert!(pump(members, output.consensus.messages).is_empty());
    let output = immediate(members[0].step(Event::Heartbeat)).unwrap();
    assert!(pump(members, output.consensus.messages).is_empty());
}

#[test]
fn quorum_ready_batch_keeps_every_application_slot_until_pin_or_cancel() {
    let mut members = cluster(1, 3, 8);
    elect(&mut members);
    let member = &mut members[0];
    let (ids, output) = immediate(member.read_index_batch(3)).unwrap();
    assert!(output.consensus.messages.is_empty());
    assert_eq!(member.replica.pending_reads(), 0);
    assert_eq!(member.pending_reads(), 3);
    for id in &ids {
        assert!(matches!(
            immediate(member.try_read(id)),
            Ok(ReadState::PendingApplication {
                required: 1,
                applied: 0
            })
        ));
        assert!(member.read_index_probes(id).unwrap().is_empty());
    }
    assert!(matches!(
        immediate(member.read_index_batch(1)),
        Err(MemberError::State(ApplicationStateError::ReadBackpressure))
    ));
    immediate(member.apply_next()).unwrap().unwrap();
    let ReadState::Ready(read) = immediate(member.try_read(&ids[0])).unwrap() else {
        panic!("application is visible");
    };
    assert_eq!(read.id(), &ids[0]);
    assert_eq!(member.pending_reads(), 2);
    assert!(matches!(
        immediate(member.read_index_batch(2)),
        Err(MemberError::State(ApplicationStateError::ReadBackpressure))
    ));
    assert!(member.cancel_read(&ids[1]));
    let (replacement, _) = immediate(member.read_index_batch(2)).unwrap();
    assert_eq!(replacement.len(), 2);
    assert_eq!(member.pending_reads(), 3);
    assert!(matches!(
        immediate(member.try_read(&ids[0])),
        Err(ApplicationError::State(ApplicationStateError::UnknownRead))
    ));
    drop(read);
}

#[test]
fn whole_batch_waits_for_audit_and_never_pins_the_later_hidden_suffix() {
    let mut members = cluster(1, 3, 8);
    elect(&mut members);
    let member = &mut members[0];
    immediate(member.apply_next()).unwrap().unwrap();
    immediate(member.step(Event::Propose(Command::Put(7)))).unwrap();
    immediate(member.apply_next()).unwrap().unwrap();
    let (ids, _) = immediate(member.read_index_batch(3)).unwrap();
    for id in &ids {
        assert!(matches!(
            immediate(member.try_read(id)),
            Ok(ReadState::PendingAudit {
                required: 2,
                visible: 1
            })
        ));
    }
    assert_eq!(member.application.application.pins.borrow().calls, 0);
    immediate(member.step(Event::Propose(Command::ReleaseThrough(2)))).unwrap();
    immediate(member.apply_next()).unwrap().unwrap();
    immediate(member.step(Event::Propose(Command::Put(9)))).unwrap();
    immediate(member.apply_next()).unwrap().unwrap();
    assert_eq!(member.progress().unwrap().applied.index, 4);
    assert_eq!(member.progress().unwrap().visible_index, 3);
    for id in &ids {
        let ReadState::Ready(read) = immediate(member.try_read(id)).unwrap() else {
            panic!("original floor has become visible");
        };
        assert_eq!(read.id(), id);
        assert_eq!(read.required_index(), 2);
        assert_eq!(read.at().index, 3);
        assert_eq!(read.view().values, [7]);
        assert_eq!(read.state_root(), oid(1002));
    }
    assert_eq!(member.pending_reads(), 0);
    assert_eq!(member.application.application.pins.borrow().active.get(), 0);
}

#[test]
fn pin_failure_and_cancelled_pin_leave_other_batch_members_usable() {
    let mut members = cluster(1, 3, 8);
    elect(&mut members);
    let member = &mut members[0];
    immediate(member.apply_next()).unwrap().unwrap();
    let (ids, _) = immediate(member.read_index_batch(3)).unwrap();
    let pins = Rc::clone(&member.application.application.pins);
    pins.borrow_mut().fault = PinFault::Error;
    assert!(matches!(
        immediate(member.try_read(&ids[0])),
        Err(ApplicationError::Backend(_))
    ));
    assert_eq!(member.pending_reads(), 3);
    assert_eq!(pins.borrow().active.get(), 0);
    pins.borrow_mut().fault = PinFault::Pending;
    let mut waiting = Box::pin(member.try_read(&ids[0]));
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(pins.borrow().active.get(), 1);
    drop(waiting);
    assert_eq!(pins.borrow().active.get(), 0);
    assert_eq!(member.pending_reads(), 3);
    pins.borrow_mut().fault = PinFault::Healthy;
    assert!(member.cancel_read(&ids[1]));
    for id in [&ids[0], &ids[2]] {
        let ReadState::Ready(read) = immediate(member.try_read(id)).unwrap() else {
            panic!("a sibling's cancellation is not a quorum loss");
        };
        assert_eq!(read.id(), id);
    }
    assert_eq!(member.pending_reads(), 0);
    assert_eq!(pins.borrow().active.get(), 0);
}

#[test]
fn inner_replica_capacity_failure_cannot_create_orphan_application_waiters() {
    let mut members = cluster(3, 4, 2);
    elect(&mut members);
    let member = &mut members[0];
    assert!(matches!(
        immediate(member.read_index_batch(3)),
        Err(MemberError::Replica(ReplicaError::ReadBackpressure))
    ));
    assert_eq!(member.pending_reads(), 0);
    assert_eq!(member.replica.pending_reads(), 0);
    assert!(matches!(
        immediate(member.read_index_batch(0)),
        Err(MemberError::State(ApplicationStateError::InvalidLimits))
    ));
    let (ids, _) = immediate(member.read_index_batch(2)).unwrap();
    assert_eq!(member.pending_reads(), 2);
    assert_eq!(member.replica.pending_reads(), 2);
    for id in &ids {
        assert!(member.cancel_read(id));
    }
    assert_eq!(member.pending_reads(), 0);
    assert_eq!(member.replica.pending_reads(), 0);
}

#[test]
fn stepdown_retires_all_quorum_ready_batch_requests_before_they_can_pin() {
    let mut members = cluster(3, 4, 8);
    elect(&mut members);
    let (ids, output) = immediate(members[0].read_index_batch(3)).unwrap();
    assert_eq!(output.consensus.messages.len(), 2);
    assert!(pump(&mut members, output.consensus.messages).is_empty());
    assert_eq!(members[0].replica.pending_reads(), 0);
    assert_eq!(members[0].pending_reads(), 3);
    let _ = immediate(members[0].step(Event::LivenessTimeout)).unwrap();
    let output = immediate(members[0].step(Event::LivenessTimeout)).unwrap();
    assert_eq!(output.leadership_lost, ids);
    assert_eq!(members[0].pending_reads(), 0);
    for id in &ids {
        assert!(matches!(
            immediate(members[0].try_read(id)),
            Err(ApplicationError::State(ApplicationStateError::UnknownRead))
        ));
    }
    assert_eq!(members[0].application.application.pins.borrow().calls, 0);
}

#[test]
fn application_publication_failure_fences_the_entire_admitted_batch() {
    let mut members = cluster(1, 3, 8);
    elect(&mut members);
    let member = &mut members[0];
    let (ids, _) = immediate(member.read_index_batch(2)).unwrap();
    member.application.application.app.0.borrow_mut().mode = Mode::FailAfter;
    assert!(immediate(member.apply_next()).is_err());
    for id in &ids {
        assert!(matches!(
            immediate(member.try_read(id)),
            Err(ApplicationError::State(
                ApplicationStateError::RecoveryRequired
            ))
        ));
        assert!(member.read_index_probes(id).is_err());
    }
    assert!(matches!(
        immediate(member.read_index_batch(1)),
        Err(MemberError::State(ApplicationStateError::RecoveryRequired))
    ));
    assert_eq!(member.application.application.pins.borrow().calls, 0);
}

#[test]
fn uncertain_raft_publication_fences_quorum_ready_batch_before_application() {
    let mut members = cluster(1, 3, 8);
    elect(&mut members);
    let member = &mut members[0];
    let (ids, _) = immediate(member.read_index_batch(3)).unwrap();
    assert_eq!(member.replica.pending_reads(), 0);
    assert_eq!(member.pending_reads(), 3);
    let pins = Rc::clone(&member.application.application.pins);
    pins.borrow_mut().fail_publication = true;
    assert!(immediate(member.step(Event::Propose(Command::Put(23)))).is_err());
    // The model persisted the new committed suffix BEFORE reporting failure.
    // The driver must not serve the older ready batch from this uncertain root.
    let published = pins.borrow().consensus.clone().unwrap();
    assert_eq!(published.commit_index(), 2);
    assert_eq!(
        published.entries().last().unwrap().command,
        Some(Command::Put(23))
    );
    for id in &ids {
        assert!(matches!(
            immediate(member.try_read(id)),
            Err(ApplicationError::State(ApplicationStateError::Raft(
                fgdb_order::Error::RecoveryRequired
            )))
        ));
        assert!(member.read_index_probes(id).is_err());
    }
    assert!(matches!(
        immediate(member.read_index_batch(1)),
        Err(MemberError::State(ApplicationStateError::Raft(
            fgdb_order::Error::RecoveryRequired
        )))
    ));
    assert_eq!(pins.borrow().calls, 0);
    assert_eq!(
        member
            .application
            .application
            .app
            .0
            .borrow()
            .progress
            .applied
            .index,
        0
    );
}
