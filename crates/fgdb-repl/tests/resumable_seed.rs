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

use fgdb_chronicle::seed::SeedObjectSpec;
use fgdb_chronicle::transfer::{DonorId, PullError, PullLimits, PullRequest};
use fgdb_repl::driver::bonded::{PullDriveError, PullReply, PullTransport, ReplyOutcome};
use fgdb_repl::driver::{
    BondedSeedSource, SeedAcquireError, SeedCatalog, SeedObjectSource, SeedRecovery,
};
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
        let fixture = self.fixtures.iter()
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
            Ok(requests.iter().rev().map(|request| {
                let outcome = match self.mode.get() {
                    Mode::Unavailable => ReplyOutcome::Unavailable,
                    Mode::Healthy => {
                        let fixture = self.catalog.fixtures.iter()
                            .find(|fixture| fixture.encoding.object_id() == request.object_id)
                            .unwrap();
                        ReplyOutcome::Record(fixture.records[request.esi as usize].clone())
                    }
                };
                PullReply { request: *request, outcome }
            }).collect())
        }
    }
}

fn assert_unique_coordinates(windows: &Windows) {
    let windows = windows.borrow();
    let mut coordinates = BTreeSet::new();
    for request in windows.iter().flatten() {
        assert!(coordinates.insert((request.object_id, request.esi)),
            "a retry reset an ESI stream and requested the same equation twice");
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
        support::namespace(), &catalog, &mut transport, &mut verification, 8,
    );
    cancel_pending(source.recover(catalog.spec(0)));
    assert_eq!(windows.borrow().len(), 2);
    let object = immediate(source.recover(catalog.spec(0))).unwrap();
    assert_eq!(object.plaintext(), catalog.fixtures[0].plaintext);
    assert_eq!(catalog.lookups.get(), 1, "retries must retain the catalog binding");
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
        support::namespace(), &catalog, &mut transport, &mut verification, 8,
    );
    assert!(matches!(immediate(source.recover(catalog.spec(0))),
        Err(SeedAcquireError::Pull(PullDriveError::Transport(_)))));
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
        support::namespace(), &catalog, &mut transport, &mut verification, 8,
    );
    cancel_pending(source.recover(catalog.spec(0)));
    for _ in 0..2 {
        assert!(matches!(immediate(source.recover(catalog.spec(0))),
            Err(SeedAcquireError::Pull(PullDriveError::Pull(PullError::RequestBudget)))));
    }
    assert_eq!(windows.borrow().iter().map(Vec::len).sum::<usize>(), catalog.fixtures[0].sources);
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
        support::namespace(), &catalog, &mut transport, &mut verification, 8,
    );
    assert!(matches!(immediate(source.recover(catalog.spec(0))),
        Err(SeedAcquireError::Pull(PullDriveError::Pull(PullError::NoAvailableDonor)))));
    mode.set(Mode::Healthy);
    assert!(matches!(immediate(source.recover(catalog.spec(0))),
        Err(SeedAcquireError::Pull(PullDriveError::Pull(PullError::NoAvailableDonor)))));
    assert_eq!(windows.borrow().len(), 1);
    source.donor_available(DonorId(1)).unwrap();
    let object = immediate(source.recover(catalog.spec(0))).unwrap();
    assert_eq!(object.plaintext(), catalog.fixtures[0].plaintext);
    assert!(windows.borrow().iter().skip(1).flatten()
        .all(|request| request.donor == DonorId(1)));
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
        support::namespace(), &catalog, &mut transport, &mut verification, 8,
    );
    let original = catalog.spec(0);
    cancel_pending(source.recover(original));
    let mut wrong_kind = original;
    wrong_kind.object_kind += 1;
    let mut wrong_length = original;
    wrong_length.compressed_len += 1;
    for replacement in [catalog.spec(1), wrong_kind, wrong_length] {
        assert!(matches!(immediate(source.recover(replacement)),
            Err(SeedAcquireError::RecoveryInProgress)));
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
        support::namespace(), &catalog, &mut transport, &mut verification, 8,
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
        support::namespace(), &catalog, &mut transport, &mut verification, 0,
    );
    assert!(matches!(immediate(source.recover(catalog.spec(0))),
        Err(SeedAcquireError::Pull(PullDriveError::InvalidWindow))));
    assert_eq!(catalog.lookups.get(), 0);
    assert!(windows.borrow().is_empty());
}
