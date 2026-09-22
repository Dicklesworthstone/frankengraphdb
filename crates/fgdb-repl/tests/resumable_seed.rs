#![cfg(not(target_arch = "wasm32"))]

// Exercise the real authentication and erasure decoder, not a forged
// VerifiedObject or a second encoding implementation.
#[allow(dead_code)]
#[path = "../../fgdb-chronicle/tests/support/bonded.rs"]
mod support;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::future::{Future, pending};
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use fgdb_chronicle::seed::{
    ObjectPublication, SeedAnchor, SeedLimits, SeedObjectSpec, SeedPlan, SeedPublicationId,
};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_chronicle::transfer::{BondedPull, DonorId, PullError, PullLimits, PullRequest};
use fgdb_order::{
    Configuration, Domain, Envelope, Error as RaftError, Event, Limits, MemberId, Message,
    PersistentState, Raft, SnapshotCut, SnapshotTransfer,
};
use fgdb_repl::driver::bonded::{PullDriveError, PullReply, PullTransport, ReplyOutcome};
use fgdb_repl::driver::{
    BondedSeedSource, SeedAcquireError, SeedCatalog, SeedDriveError, SeedObjectSource,
    SeedPublisher, SeedRecovery, resume_snapshot,
};
use fgdb_repl::{CatchupError, CatchupPhase, SnapshotCatchup, SnapshotPublication};
use fgdb_types::ObjectId;
use support::{DEK, Fixture};

const DONORS: [DonorId; 3] = [DonorId(1), DonorId(2), DonorId(3)];

struct NoopWake;
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn immediate<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    match pin!(future).as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test operation unexpectedly suspended"),
    }
}

fn cancel_pending<F: Future>(future: F) {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    assert!(future.as_mut().poll(&mut cx).is_pending());
    // Dropping the future must release its window without discarding its owner.
}

struct Catalog {
    fixtures: Vec<Fixture>,
    limits: PullLimits,
    lookups: Cell<usize>,
}

impl Catalog {
    fn new() -> Self {
        Self {
            fixtures: vec![Fixture::new(111), Fixture::new(112)],
            limits: PullLimits::default(),
            lookups: Cell::new(0),
        }
    }

    fn snapshot() -> Self {
        let mut catalog = Self::new();
        catalog
            .fixtures
            .extend([Fixture::new(113), Fixture::new(114)]);
        catalog
    }

    fn spec(&self, index: usize) -> SeedObjectSpec {
        let fixture = &self.fixtures[index];
        SeedObjectSpec {
            object_id: fixture.encoding.object_id(),
            object_kind: support::KIND,
            compressed_len: fixture.plaintext.len() as u64,
        }
    }
}

impl SeedCatalog for Catalog {
    type Error = &'static str;

    fn recovery(&self, object: SeedObjectSpec) -> Result<SeedRecovery<'_>, Self::Error> {
        self.lookups.set(self.lookups.get() + 1);
        let fixture = self
            .fixtures
            .iter()
            .find(|fixture| fixture.encoding.object_id() == object.object_id)
            .ok_or("object is not in the authenticated catalog")?;
        Ok(SeedRecovery {
            encoding: &fixture.encoding,
            target: fixture.target(),
            dek: &DEK,
            donors: &DONORS,
            limits: self.limits,
        })
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Healthy,
    Unavailable,
}

type Windows = Rc<RefCell<Vec<Vec<PullRequest>>>>;

struct Transport<'a> {
    catalog: &'a Catalog,
    windows: Windows,
    mode: Rc<Cell<Mode>>,
    suspend_on: Option<usize>,
    fail_on: Option<usize>,
}

impl<'a> Transport<'a> {
    fn new(catalog: &'a Catalog) -> Self {
        Self {
            catalog,
            windows: Rc::new(RefCell::new(Vec::new())),
            mode: Rc::new(Cell::new(Mode::Healthy)),
            suspend_on: None,
            fail_on: None,
        }
    }
}

impl PullTransport for Transport<'_> {
    type Error = &'static str;

    fn exchange(
        &mut self,
        requests: &[PullRequest],
    ) -> impl Future<Output = Result<Vec<PullReply>, Self::Error>> {
        self.windows.borrow_mut().push(requests.to_vec());
        let call = self.windows.borrow().len();
        async move {
            if self.suspend_on == Some(call) {
                pending::<()>().await;
            }
            if self.fail_on == Some(call) {
                return Err("temporary transport interruption");
            }
            Ok(requests
                .iter()
                .rev()
                .map(|request| {
                    let outcome = match self.mode.get() {
                        Mode::Unavailable => ReplyOutcome::Unavailable,
                        Mode::Healthy => {
                            let fixture = self
                                .catalog
                                .fixtures
                                .iter()
                                .find(|fixture| fixture.encoding.object_id() == request.object_id)
                                .unwrap();
                            ReplyOutcome::Record(fixture.records[request.esi as usize].clone())
                        }
                    };
                    PullReply {
                        request: *request,
                        outcome,
                    }
                })
                .collect())
        }
    }
}

fn assert_unique_coordinates(windows: &Windows) {
    let windows = windows.borrow();
    let mut coordinates = BTreeSet::new();
    for request in windows.iter().flatten() {
        assert!(
            coordinates.insert((request.object_id, request.esi)),
            "a retry reset an ESI stream and requested the same equation twice"
        );
    }
}

#[test]
fn cancelled_seed_recovery_resumes_the_pinned_pull() {
    let catalog = Catalog::new();
    let mut transport = Transport::new(&catalog);
    transport.suspend_on = Some(2);
    let windows = Rc::clone(&transport.windows);
    let mut verification = Vec::new();
    let mut source = BondedSeedSource::new(
        support::namespace(),
        &catalog,
        &mut transport,
        &mut verification,
        8,
    );
    cancel_pending(source.recover(catalog.spec(0)));
    assert_eq!(windows.borrow().len(), 2);
    let object = immediate(source.recover(catalog.spec(0))).unwrap();
    assert_eq!(object.plaintext(), catalog.fixtures[0].plaintext);
    assert_eq!(
        catalog.lookups.get(),
        1,
        "retries must retain the catalog binding"
    );
    assert_unique_coordinates(&windows);
}

#[test]
fn transport_failure_preserves_authenticated_contributions() {
    let catalog = Catalog::new();
    let mut transport = Transport::new(&catalog);
    transport.fail_on = Some(2);
    let windows = Rc::clone(&transport.windows);
    let mut verification = Vec::new();
    let mut source = BondedSeedSource::new(
        support::namespace(),
        &catalog,
        &mut transport,
        &mut verification,
        8,
    );
    assert!(matches!(
        immediate(source.recover(catalog.spec(0))),
        Err(SeedAcquireError::Pull(PullDriveError::Transport(_)))
    ));
    let object = immediate(source.recover(catalog.spec(0))).unwrap();
    assert_eq!(object.plaintext(), catalog.fixtures[0].plaintext);
    assert_eq!(catalog.lookups.get(), 1);
    assert_unique_coordinates(&windows);
}

#[test]
fn retry_cannot_reset_the_request_budget() {
    let mut catalog = Catalog::new();
    catalog.limits.max_requests = catalog.fixtures[0].sources as u64;
    let mut transport = Transport::new(&catalog);
    transport.suspend_on = Some(2);
    let windows = Rc::clone(&transport.windows);
    let mut verification = Vec::new();
    let mut source = BondedSeedSource::new(
        support::namespace(),
        &catalog,
        &mut transport,
        &mut verification,
        8,
    );
    cancel_pending(source.recover(catalog.spec(0)));
    for _ in 0..2 {
        assert!(matches!(
            immediate(source.recover(catalog.spec(0))),
            Err(SeedAcquireError::Pull(PullDriveError::Pull(
                PullError::RequestBudget
            )))
        ));
    }
    assert_eq!(
        windows.borrow().iter().map(Vec::len).sum::<usize>(),
        catalog.fixtures[0].sources
    );
    assert_eq!(catalog.lookups.get(), 1);
    assert_unique_coordinates(&windows);
}

#[test]
fn failed_donors_stay_quarantined_until_authenticated_reconnection() {
    let catalog = Catalog::new();
    let mut transport = Transport::new(&catalog);
    transport.mode.set(Mode::Unavailable);
    let mode = Rc::clone(&transport.mode);
    let windows = Rc::clone(&transport.windows);
    let mut verification = Vec::new();
    let mut source = BondedSeedSource::new(
        support::namespace(),
        &catalog,
        &mut transport,
        &mut verification,
        8,
    );
    assert!(matches!(
        immediate(source.recover(catalog.spec(0))),
        Err(SeedAcquireError::Pull(PullDriveError::Pull(
            PullError::NoAvailableDonor
        )))
    ));
    mode.set(Mode::Healthy);
    assert!(matches!(
        immediate(source.recover(catalog.spec(0))),
        Err(SeedAcquireError::Pull(PullDriveError::Pull(
            PullError::NoAvailableDonor
        )))
    ));
    assert_eq!(windows.borrow().len(), 1);
    source.donor_available(DonorId(1)).unwrap();
    let object = immediate(source.recover(catalog.spec(0))).unwrap();
    assert_eq!(object.plaintext(), catalog.fixtures[0].plaintext);
    assert!(
        windows
            .borrow()
            .iter()
            .skip(1)
            .flatten()
            .all(|request| request.donor == DonorId(1))
    );
    assert_eq!(catalog.lookups.get(), 1);
    assert_unique_coordinates(&windows);
}

#[test]
fn active_target_cannot_be_replaced_without_explicit_abandonment() {
    let catalog = Catalog::new();
    let mut transport = Transport::new(&catalog);
    transport.suspend_on = Some(2);
    let windows = Rc::clone(&transport.windows);
    let mut verification = Vec::new();
    let mut source = BondedSeedSource::new(
        support::namespace(),
        &catalog,
        &mut transport,
        &mut verification,
        8,
    );
    let original = catalog.spec(0);
    cancel_pending(source.recover(original));
    let mut wrong_kind = original;
    wrong_kind.object_kind += 1;
    let mut wrong_length = original;
    wrong_length.compressed_len += 1;
    for replacement in [catalog.spec(1), wrong_kind, wrong_length] {
        assert!(matches!(
            immediate(source.recover(replacement)),
            Err(SeedAcquireError::RecoveryInProgress)
        ));
    }
    assert_eq!(windows.borrow().len(), 2);
    assert_eq!(catalog.lookups.get(), 1);
    source.abandon_recovery();
    let object = immediate(source.recover(catalog.spec(1))).unwrap();
    assert_eq!(object.plaintext(), catalog.fixtures[1].plaintext);
    assert_eq!(catalog.lookups.get(), 2);
}

#[test]
fn successful_recovery_releases_the_slot_for_the_next_inventory_object() {
    let catalog = Catalog::new();
    let mut transport = Transport::new(&catalog);
    let mut verification = Vec::new();
    let mut source = BondedSeedSource::new(
        support::namespace(),
        &catalog,
        &mut transport,
        &mut verification,
        8,
    );
    for index in 0..catalog.fixtures.len() {
        let object = immediate(source.recover(catalog.spec(index))).unwrap();
        assert_eq!(object.plaintext(), catalog.fixtures[index].plaintext);
    }
    assert_eq!(catalog.lookups.get(), 2);
}

#[test]
fn invalid_window_is_rejected_before_catalog_lookup_or_transport() {
    let catalog = Catalog::new();
    let mut transport = Transport::new(&catalog);
    let windows = Rc::clone(&transport.windows);
    let mut verification = Vec::new();
    let mut source = BondedSeedSource::new(
        support::namespace(),
        &catalog,
        &mut transport,
        &mut verification,
        0,
    );
    assert!(matches!(
        immediate(source.recover(catalog.spec(0))),
        Err(SeedAcquireError::Pull(PullDriveError::InvalidWindow))
    ));
    assert_eq!(catalog.lookups.get(), 0);
    assert!(windows.borrow().is_empty());
}

#[test]
fn oversized_window_succeeds_with_exactly_enough_authentication_budget() {
    let mut catalog = Catalog::new();
    catalog.limits.max_verifications = catalog.fixtures[0].sources as u64;
    let mut transport = Transport::new(&catalog);
    let windows = Rc::clone(&transport.windows);
    let mut verification = Vec::new();
    let mut source = BondedSeedSource::new(
        support::namespace(),
        &catalog,
        &mut transport,
        &mut verification,
        64,
    );
    let object = immediate(source.recover(catalog.spec(0))).unwrap();
    assert_eq!(object.plaintext(), catalog.fixtures[0].plaintext);
    assert_eq!(windows.borrow().len(), 1);
    assert_eq!(windows.borrow()[0].len(), catalog.fixtures[0].sources);
}

#[test]
fn overlapping_windows_reserve_and_release_verification_capacity() {
    let fixture = Fixture::new(115);
    let limits = PullLimits {
        max_verifications: fixture.sources as u64,
        ..PullLimits::default()
    };
    let mut pull =
        BondedPull::new(&fixture.encoding, fixture.target(), &DEK, &DONORS, limits).unwrap();
    let first = pull.schedule(8).unwrap();
    let second = pull.schedule(64).unwrap();
    assert_eq!(first.len() + second.len(), fixture.sources);
    assert!(pull.schedule(64).unwrap().is_empty());
    pull.expire(first[0]).unwrap();
    let replacement = pull.schedule(64).unwrap();
    assert_eq!(replacement.len(), 1);
    assert!(!first.contains(&replacement[0]));
    assert!(!second.contains(&replacement[0]));
    assert_eq!(pull.pending_count(), fixture.sources);
}

// Memory-only publication adapter for exercising transition boundaries. It is
// deliberately not a canonical manifest verifier or a production RootStore.
#[derive(Default)]
struct Publisher {
    attempts: Vec<(SeedPublicationId, ObjectId)>,
    durable: BTreeSet<ObjectId>,
    consensus: Option<PersistentState<u64>>,
    suspend_object_on: Option<usize>,
    fail_object_on: Option<usize>,
    suspend_root: bool,
    fail_root: bool,
}

impl SeedPublisher<u64> for Publisher {
    type Error = &'static str;

    fn publish_object(
        &mut self,
        object: ObjectPublication<'_>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let object_id = object.object().object_id();
        self.attempts.push((object.id(), object_id));
        let attempt = self.attempts.len();
        async move {
            if self.suspend_object_on == Some(attempt) {
                pending::<()>().await;
            }
            if self.fail_object_on == Some(attempt) {
                return Err("immutable object publication interrupted");
            }
            self.durable.insert(object_id);
            Ok(())
        }
    }

    fn publish_snapshot(
        &mut self,
        snapshot: SnapshotPublication<'_, u64>,
    ) -> impl Future<Output = Result<RootPublicationEvidence, Self::Error>> {
        for object in snapshot.seed().plan().objects() {
            assert!(self.durable.contains(&object.object_id));
        }
        let anchor = snapshot.seed().plan().anchor();
        assert_eq!(
            snapshot.consensus().snapshot().unwrap().index(),
            anchor.raft_index
        );
        assert_eq!(
            snapshot.consensus().snapshot().unwrap().state_root(),
            anchor.state_root.0
        );
        let evidence = RootPublicationEvidence {
            written_index: 1,
            slot_generation: anchor.publication_generation,
            root_manifest_oid: anchor.publication_root.0,
        };
        // Model an uncertain storage result: the new root can become visible
        // before its fsync/reread future errors or is cancelled.
        self.consensus = Some(snapshot.consensus().clone());
        async move {
            if self.suspend_root {
                pending::<()>().await;
            }
            if self.fail_root {
                Err("atomic root outcome unknown")
            } else {
                Ok(evidence)
            }
        }
    }
}

fn snapshot_offer(catalog: &Catalog) -> (Raft<u64>, SnapshotTransfer, SeedPlan) {
    let config = Configuration::stable(
        Domain([21; 32]),
        [22; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [],
    )
    .unwrap();
    let roots: Vec<_> = catalog
        .fixtures
        .iter()
        .map(|fixture| fixture.encoding.object_id())
        .collect();
    let cut =
        SnapshotCut::from_authenticated_parts(&config, roots[0].0, roots[1].0, roots[2].0, 12, 3)
            .unwrap();
    let anchor = SeedAnchor {
        namespace: support::namespace(),
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
    };
    // Synthetic authenticated inventory for this state-machine test only.
    let plan = SeedPlan::from_authenticated_inventory(
        anchor,
        (0..catalog.fixtures.len()).map(|index| catalog.spec(index)),
        SeedLimits::default(),
    )
    .unwrap();
    let mut raft = Raft::new(MemberId(2), config.clone(), Limits::default()).unwrap();
    let pending = raft
        .step(Event::Receive(Envelope {
            domain: config.domain(),
            configuration: config.identity(),
            from: MemberId(1),
            to: MemberId(2),
            message: Message::InstallSnapshot {
                term: 3,
                request: 1,
                snapshot: cut,
            },
        }))
        .unwrap();
    let id = pending.id();
    // The offer's durability acknowledgement is modeled in memory here.
    let output = raft.persisted(id).unwrap();
    (
        raft,
        output.snapshot_transfers.into_iter().next().unwrap(),
        plan,
    )
}

#[test]
fn interrupted_object_publication_resumes_staging_and_keeps_the_durable_prefix() {
    for cancel in [false, true] {
        let catalog = Catalog::snapshot();
        let (mut raft, transfer, plan) = snapshot_offer(&catalog);
        let mut catchup =
            SnapshotCatchup::begin(&mut raft, support::namespace(), transfer, plan).unwrap();
        let mut transport = Transport::new(&catalog);
        let mut verification = Vec::new();
        let mut source = BondedSeedSource::new(
            support::namespace(),
            &catalog,
            &mut transport,
            &mut verification,
            8,
        );
        let mut publisher = Publisher {
            suspend_object_on: cancel.then_some(2),
            fail_object_on: (!cancel).then_some(2),
            ..Publisher::default()
        };
        if cancel {
            cancel_pending(resume_snapshot(&mut catchup, &mut source, &mut publisher));
        } else {
            assert!(matches!(
                immediate(resume_snapshot(&mut catchup, &mut source, &mut publisher)),
                Err(SeedDriveError::Publication(_))
            ));
        }
        assert_eq!(catchup.phase(), CatchupPhase::Transferring);
        assert_eq!(catchup.published_count(), 1);
        assert_eq!(catalog.lookups.get(), 2);
        let staged = catchup.pending_object().unwrap().id();
        assert_eq!(publisher.attempts[1].0, staged);
        let output = immediate(resume_snapshot(&mut catchup, &mut source, &mut publisher)).unwrap();
        assert_eq!(catchup.phase(), CatchupPhase::Complete);
        assert_eq!(catchup.published_count(), 4);
        assert_eq!(
            catalog.lookups.get(),
            4,
            "staged objects must not be fetched again"
        );
        assert_eq!(publisher.attempts.len(), 5);
        assert_eq!(
            publisher.attempts[2].0, staged,
            "retry must use the same publication identity"
        );
        assert_eq!(publisher.durable.len(), 4);
        assert_eq!(output.installed_snapshot.unwrap().index(), 12);
        drop(catchup);
        assert_eq!(raft.durable_state().unwrap().commit_index(), 12);
    }
}

#[test]
fn interrupted_root_publication_fences_even_a_caller_owned_session() {
    for cancel in [false, true] {
        let catalog = Catalog::snapshot();
        let (mut raft, transfer, plan) = snapshot_offer(&catalog);
        let mut catchup =
            SnapshotCatchup::begin(&mut raft, support::namespace(), transfer, plan).unwrap();
        let mut transport = Transport::new(&catalog);
        let mut verification = Vec::new();
        let mut source = BondedSeedSource::new(
            support::namespace(),
            &catalog,
            &mut transport,
            &mut verification,
            8,
        );
        let mut publisher = Publisher {
            suspend_root: cancel,
            fail_root: !cancel,
            ..Publisher::default()
        };
        if cancel {
            cancel_pending(resume_snapshot(&mut catchup, &mut source, &mut publisher));
        } else {
            assert!(matches!(
                immediate(resume_snapshot(&mut catchup, &mut source, &mut publisher)),
                Err(SeedDriveError::Publication(_))
            ));
        }
        assert_eq!(catchup.phase(), CatchupPhase::Failed);
        assert!(publisher.consensus.is_some());
        assert!(matches!(
            immediate(resume_snapshot(&mut catchup, &mut source, &mut publisher)),
            Err(SeedDriveError::Catchup(CatchupError::WrongPhase))
        ));
        assert_eq!(catalog.lookups.get(), 4);
        drop(catchup);
        assert_eq!(raft.role(), Err(RaftError::RecoveryRequired));
    }
}
