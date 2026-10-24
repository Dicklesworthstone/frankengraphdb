//! Exercise the real owned member using the existing memory publication model.
//! The fixture models historical pins and cancellation; it proves neither
//! Chronicle disk durability nor authenticated transport or Warden authority.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{Future, pending, ready};
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Waker};

use fgdb_order::{Configuration, Domain, Envelope, Event, Limits, MemberId, PersistentState};

use super::*;
use crate::application::member::{PinnedApplication, ReadApplication, ReadState};
use crate::application::tests::{
    Command, MemoryApplication, MemoryState, Mode, genesis, immediate, memory, oid,
};
use crate::application::{
    ApplicationBatch, ApplicationError, ApplicationProgress, AppliedPosition,
};
use crate::replica::Replica;

#[derive(Clone, Copy, PartialEq, Eq)]
enum PinFault {
    Healthy,
    Error,
    Pending,
    WrongCut,
}

struct Controls {
    publications: Cell<usize>,
    pin_calls: Cell<usize>,
    active_pins: Rc<Cell<usize>>,
    pin_fault: Cell<PinFault>,
}

struct Backend {
    app: MemoryApplication,
    controls: Rc<Controls>,
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
        _state: &PersistentState<Command>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.controls
            .publications
            .set(self.controls.publications.get() + 1);
        ready(Ok(()))
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
        self.controls
            .pin_calls
            .set(self.controls.pin_calls.get() + 1);
        let active = Rc::clone(&self.controls.active_pins);
        active.set(active.get() + 1);
        let fault = self.controls.pin_fault.get();
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
        if fault == PinFault::WrongCut {
            pinned.at.index += 1;
        }
        drop(store);
        async move {
            if fault == PinFault::Pending {
                pending::<()>().await;
            }
            if fault == PinFault::Error {
                Err("modeled pin unavailable")
            } else {
                Ok(pinned)
            }
        }
    }
}

type Member = AppliedReplica<Command, Backend>;
type Model = (Rc<RefCell<MemoryState>>, Rc<Controls>);

fn pump(members: &mut [Member], messages: Vec<Envelope<Command>>) {
    let mut queue: VecDeque<_> = messages.into();
    let mut count = 0;
    while let Some(message) = queue.pop_front() {
        count += 1;
        assert!(count < 10_000, "consensus dispatch must quiesce");
        let output =
            immediate(members[message.to.0 as usize - 1].step(Event::Receive(message))).unwrap();
        assert!(output.leadership_lost.is_empty());
        queue.extend(output.consensus.messages);
    }
}

fn elected(maximum_reads: usize) -> (Vec<Member>, Vec<Model>) {
    let configuration = Configuration::stable(
        Domain([1; 32]),
        [2; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [],
    )
    .unwrap();
    let mut members = Vec::new();
    let mut models = Vec::new();
    for id in 1..=3 {
        let replica =
            Replica::new(MemberId(id), configuration.clone(), Limits::default(), 8).unwrap();
        let (app, state) = memory(genesis());
        let controls = Rc::new(Controls {
            publications: Cell::new(0),
            pin_calls: Cell::new(0),
            active_pins: Rc::new(Cell::new(0)),
            pin_fault: Cell::new(PinFault::Healthy),
        });
        let backend = Backend {
            app,
            controls: Rc::clone(&controls),
        };
        members.push(immediate(Member::recover(replica, backend, 1, maximum_reads)).unwrap());
        models.push((state, controls));
    }
    let output = immediate(members[0].step(Event::ElectionTimeout)).unwrap();
    pump(&mut members, output.consensus.messages);
    let output = immediate(members[0].step(Event::Heartbeat)).unwrap();
    pump(&mut members, output.consensus.messages);
    (members, models)
}

fn response(members: &[Member], id: &ReadIndexId) -> ReadIndexAck {
    let probe = members[0]
        .read_index_probes(id)
        .unwrap()
        .into_iter()
        .find(|probe| probe.to == MemberId(2))
        .unwrap();
    members[1].acknowledge_read_index(probe).unwrap()
}

fn commit(members: &mut [Member], command: Command) {
    let output = immediate(members[0].step(Event::Propose(command))).unwrap();
    pump(members, output.consensus.messages);
}

#[test]
fn probe_quorum_waits_for_apply_and_audit_then_pins_only_the_visible_prefix() {
    let (mut members, models) = elected(1);
    commit(&mut members, Command::Put(10));
    let (id, admission) = immediate(members[0].read_index()).unwrap();
    let ack = response(&members, &id);
    let writes = models[0].1.publications.get();
    assert!(members[0].confirm_read_index(&id, ack).unwrap());
    assert_eq!(models[0].1.publications.get(), writes);
    assert!(members[0].read_index_probes(&id).unwrap().is_empty());
    assert!(matches!(
        immediate(members[0].try_read(&id)).unwrap(),
        ReadState::PendingApplication {
            required: 2,
            applied: 0
        }
    ));
    immediate(members[0].apply_next()).unwrap(); // election no-op
    immediate(members[0].apply_next()).unwrap(); // hidden Put(10)
    assert!(matches!(
        immediate(members[0].try_read(&id)).unwrap(),
        ReadState::PendingAudit {
            required: 2,
            visible: 1
        }
    ));
    assert_eq!(models[0].1.pin_calls.get(), 0);
    assert_eq!(members[0].pending_reads(), 1);
    assert!(matches!(
        immediate(members[0].read_index()),
        Err(MemberError::State(ApplicationStateError::ReadBackpressure))
    ));
    // Late heartbeat acknowledgements must not duplicate the stored quorum or
    // remove the application wait. They also retire the append window before
    // the subsequent audit control is sent through normal consensus.
    pump(&mut members, admission.consensus.messages);
    assert_eq!(members[0].pending_reads(), 1);
    commit(&mut members, Command::ReleaseThrough(2));
    immediate(members[0].apply_next()).unwrap();
    commit(&mut members, Command::Put(99));
    immediate(members[0].apply_next()).unwrap();
    let progress = members[0].progress().unwrap();
    assert_eq!(progress.applied.index, 4);
    assert_eq!(progress.visible_index, 3);
    let ReadState::Ready(read) = immediate(members[0].try_read(&id)).unwrap() else {
        panic!("visible prefix did not satisfy the admitted read")
    };
    assert_eq!(read.id(), &id);
    assert_eq!(read.required_index(), 2);
    assert_eq!(read.at(), AppliedPosition { index: 3, term: 1 });
    assert_eq!(read.state_root(), oid(1002));
    assert_eq!(read.view().values, [10]); // applied-but-hidden 99 never escapes
    assert_eq!(members[0].pending_reads(), 0);
    assert_eq!(models[0].1.active_pins.get(), 1);
    drop(read);
    assert_eq!(models[0].1.active_pins.get(), 0);
}

#[test]
fn failed_and_cancelled_pins_retain_one_quorum_and_release_unreturned_views() {
    let (mut members, models) = elected(1);
    immediate(members[0].apply_next()).unwrap();
    let (id, _) = immediate(members[0].read_index()).unwrap();
    let ack = response(&members, &id);
    assert!(members[0].confirm_read_index(&id, ack).unwrap());
    models[0].1.pin_fault.set(PinFault::Error);
    assert!(matches!(
        immediate(members[0].try_read(&id)),
        Err(ApplicationError::Backend("modeled pin unavailable"))
    ));
    assert_eq!(members[0].pending_reads(), 1);
    assert_eq!(models[0].1.active_pins.get(), 0);
    models[0].1.pin_fault.set(PinFault::Pending);
    {
        let mut future = pin!(members[0].try_read(&id));
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(models[0].1.active_pins.get(), 1);
    }
    assert_eq!(models[0].1.active_pins.get(), 0);
    assert_eq!(members[0].pending_reads(), 1);
    assert!(members[0].read_index_probes(&id).unwrap().is_empty());
    assert!(matches!(
        members[0].confirm_read_index(&id, ack),
        Err(MemberError::Replica(ReplicaError::ReadIndex(
            ReadIndexError::Completed
        )))
    ));
    models[0].1.pin_fault.set(PinFault::Healthy);
    let ReadState::Ready(read) = immediate(members[0].try_read(&id)).unwrap() else {
        panic!("original quorum was lost during pin retry")
    };
    assert_eq!(read.required_index(), 1);
    assert_eq!(models[0].1.pin_calls.get(), 3);
    drop(read);
    assert_eq!(models[0].1.active_pins.get(), 0);
    assert!(matches!(
        immediate(members[0].try_read(&id)),
        Err(ApplicationError::State(ApplicationStateError::UnknownRead))
    ));
    assert!(matches!(
        members[0].confirm_read_index(&id, ack),
        Err(MemberError::State(ApplicationStateError::UnknownRead))
    ));
}

#[test]
fn cancellation_stale_replies_and_stepdown_preserve_the_owned_read_identity() {
    let (mut members, models) = elected(1);
    let (cancelled, _) = immediate(members[0].read_index()).unwrap();
    let stale = response(&members, &cancelled);
    assert!(members[0].cancel_read(&cancelled));
    let (id, _) = immediate(members[0].read_index()).unwrap();
    assert!(matches!(
        members[0].confirm_read_index(&id, stale),
        Err(MemberError::Replica(ReplicaError::ReadIndex(
            ReadIndexError::WrongRound
        )))
    ));
    assert!(matches!(
        immediate(members[0].try_read(&id)).unwrap(),
        ReadState::PendingQuorum
    ));
    assert!(matches!(
        members[0].confirm_read_index(&cancelled, stale),
        Err(MemberError::State(ApplicationStateError::UnknownRead))
    ));
    let ack = response(&members, &id);
    assert!(matches!(
        members[1].confirm_read_index(&id, ack),
        Err(MemberError::State(ApplicationStateError::UnknownRead))
    ));
    assert!(members[0].confirm_read_index(&id, ack).unwrap());
    immediate(members[0].step(Event::LivenessTimeout)).unwrap();
    let output = immediate(members[0].step(Event::LivenessTimeout)).unwrap();
    assert_eq!(output.leadership_lost, std::slice::from_ref(&id));
    assert_eq!(members[0].pending_reads(), 0);
    assert!(matches!(
        members[0].read_index_probes(&id),
        Err(MemberError::State(ApplicationStateError::UnknownRead))
    ));
    assert!(matches!(
        immediate(members[0].try_read(&id)),
        Err(ApplicationError::State(ApplicationStateError::UnknownRead))
    ));
    assert_eq!(models[0].1.pin_calls.get(), 0);
}

#[test]
fn uncertain_application_publication_fences_both_probe_directions() {
    let (mut members, models) = elected(1);
    let (id, _) = immediate(members[0].read_index()).unwrap();
    let probe = members[0]
        .read_index_probes(&id)
        .unwrap()
        .into_iter()
        .find(|probe| probe.to == MemberId(2))
        .unwrap();
    let ack = members[1].acknowledge_read_index(probe).unwrap();
    models[1].0.borrow_mut().mode = Mode::FailAfter;
    assert!(immediate(members[1].apply_next()).is_err());
    assert!(matches!(
        members[1].acknowledge_read_index(probe),
        Err(MemberError::State(ApplicationStateError::RecoveryRequired))
    ));
    models[0].0.borrow_mut().mode = Mode::FailAfter;
    assert!(immediate(members[0].apply_next()).is_err());
    assert!(matches!(
        members[0].read_index_probes(&id),
        Err(MemberError::State(ApplicationStateError::RecoveryRequired))
    ));
    assert!(matches!(
        members[0].confirm_read_index(&id, ack),
        Err(MemberError::State(ApplicationStateError::RecoveryRequired))
    ));
    assert_eq!(models[0].1.pin_calls.get(), 0);
}

#[test]
fn malformed_snapshot_pin_cannot_turn_a_probe_quorum_into_a_read() {
    let (mut members, models) = elected(1);
    immediate(members[0].apply_next()).unwrap();
    let (id, _) = immediate(members[0].read_index()).unwrap();
    let ack = response(&members, &id);
    members[0].confirm_read_index(&id, ack).unwrap();
    models[0].1.pin_fault.set(PinFault::WrongCut);
    assert!(matches!(
        immediate(members[0].try_read(&id)),
        Err(ApplicationError::State(
            ApplicationStateError::InvalidReadSnapshot
        ))
    ));
    assert_eq!(models[0].1.active_pins.get(), 0);
    assert!(matches!(
        members[0].read_index_probes(&id),
        Err(MemberError::State(ApplicationStateError::RecoveryRequired))
    ));
}
