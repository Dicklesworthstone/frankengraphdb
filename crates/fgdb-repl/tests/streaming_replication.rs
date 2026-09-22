#![cfg(not(target_arch = "wasm32"))]

// Exercise real Chronicle symbol MACs, RaptorQ, AEAD and logical identity.
#[allow(dead_code)]
#[path = "../../fgdb-chronicle/tests/support/bonded.rs"]
mod support;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{Future, pending, ready};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use fgdb_chronicle::seed::{ObjectPublication, SeedAnchor, SeedLimits, SeedObjectSpec, SeedPlan};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_chronicle::transfer::{BondedPull, DonorId, PullError, PullLimits, PullRequest};
use fgdb_order::{
    Configuration, Domain, Envelope, Error as RaftError, Event, Limits, MemberId, Message,
    PersistentState, Raft, SnapshotCut, SnapshotTransfer,
};
use fgdb_repl::driver::bonded::streaming::{StreamingSeedSource, SymbolTransport, recover};
use fgdb_repl::driver::bonded::{PullDriveError, ReplyOutcome};
use fgdb_repl::driver::{
    RaftPublisher, SeedAcquireError, SeedCatalog, SeedObjectSource, SeedPublisher, SeedRecovery,
    sequence,
};
use fgdb_repl::{SnapshotCatchup, SnapshotPublication};
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
        Poll::Pending => panic!("healthy donors should complete without waiting for silent peers"),
    }
}

#[derive(Default)]
struct Counters {
    started: Cell<usize>,
    live: Cell<usize>,
    peak: Cell<usize>,
    requests: RefCell<Vec<PullRequest>>,
}

struct RequestGuard<'a>(&'a Counters);

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.0.live.set(self.0.live.get() - 1);
    }
}

struct Transport<'a> {
    fixture: &'a Fixture,
    counters: Counters,
    silent: Option<DonorId>,
    corrupt: Option<DonorId>,
    unavailable: Option<DonorId>,
    misroute: Option<DonorId>,
    fatal: Option<DonorId>,
    suspend_after: Cell<Option<usize>>,
    panic_on: Option<usize>,
    timeouts_only: bool,
}

impl<'a> Transport<'a> {
    fn healthy(fixture: &'a Fixture) -> Self {
        Self {
            fixture,
            counters: Counters::default(),
            silent: None,
            corrupt: None,
            unavailable: None,
            misroute: None,
            fatal: None,
            suspend_after: Cell::new(None),
            panic_on: None,
            timeouts_only: false,
        }
    }
}

impl SymbolTransport for Transport<'_> {
    type Error = &'static str;

    fn request(
        &self,
        request: PullRequest,
    ) -> impl Future<Output = Result<ReplyOutcome, Self::Error>> {
        let call = self.counters.started.get() + 1;
        self.counters.started.set(call);
        assert_ne!(
            self.panic_on,
            Some(call),
            "injected adapter constructor panic"
        );
        self.counters.requests.borrow_mut().push(request);
        let live = self.counters.live.get() + 1;
        self.counters.live.set(live);
        self.counters.peak.set(self.counters.peak.get().max(live));
        let guard = RequestGuard(&self.counters);
        async move {
            let _guard = guard;
            if self.silent == Some(request.donor)
                || self.suspend_after.get().is_some_and(|limit| call > limit)
            {
                // Deliberately stronger than a normal finite-deadline failure:
                // another donor must still be able to finish the object.
                pending::<()>().await;
            }
            if self.fatal == Some(request.donor) {
                return Err("authority revoked");
            }
            if self.unavailable == Some(request.donor) {
                return Ok(ReplyOutcome::Unavailable);
            }
            if self.timeouts_only {
                return Ok(ReplyOutcome::TimedOut);
            }
            let esi = if self.misroute == Some(request.donor) {
                request.esi + DONORS.len() as u32
            } else {
                request.esi
            };
            let mut record = self.fixture.records[esi as usize].clone();
            if self.corrupt == Some(request.donor) {
                *record.last_mut().unwrap() ^= 1;
            }
            Ok(ReplyOutcome::Record(record))
        }
    }
}

#[test]
fn healthy_donors_finish_while_another_donor_never_replies() {
    let fixture = Fixture::new(121);
    let mut transport = Transport::healthy(&fixture);
    transport.silent = Some(DonorId(1));
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &DONORS,
        PullLimits::default(),
    )
    .unwrap();
    let object = immediate(recover(&mut pull, &transport, &mut Vec::new(), 6)).unwrap();
    assert_eq!(object.plaintext(), fixture.plaintext);
    assert_eq!(pull.pending_count(), 0);
    assert_eq!(transport.counters.live.get(), 0);
    assert!(transport.counters.peak.get() <= 6);
    assert_eq!(
        transport
            .counters
            .requests
            .borrow()
            .iter()
            .filter(|request| request.donor == DonorId(1))
            .count(),
        2
    );
}

#[test]
fn corrupt_and_disconnected_donors_do_not_cancel_healthy_recovery() {
    let fixture = Fixture::new(122);
    let mut transport = Transport::healthy(&fixture);
    transport.unavailable = Some(DonorId(1));
    transport.corrupt = Some(DonorId(2));
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &DONORS,
        PullLimits::default(),
    )
    .unwrap();
    let object = immediate(recover(&mut pull, &transport, &mut Vec::new(), 9)).unwrap();
    assert_eq!(object.plaintext(), fixture.plaintext);
    assert_eq!(pull.pending_count(), 0);
    assert_eq!(transport.counters.live.get(), 0);
}

#[test]
fn authenticated_wrong_coordinate_is_quarantined_not_miscredited() {
    let fixture = Fixture::new(123);
    let mut transport = Transport::healthy(&fixture);
    transport.misroute = Some(DonorId(1));
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &DONORS,
        PullLimits::default(),
    )
    .unwrap();
    let object = immediate(recover(&mut pull, &transport, &mut Vec::new(), 6)).unwrap();
    assert_eq!(object.plaintext(), fixture.plaintext);
    assert_eq!(transport.counters.live.get(), 0);
    assert_eq!(
        transport
            .counters
            .requests
            .borrow()
            .iter()
            .filter(|request| request.donor == DonorId(1))
            .count(),
        2
    );
}

#[test]
fn cancellation_preserves_verified_symbols_and_releases_every_credit() {
    let fixture = Fixture::new(124);
    let transport = Transport::healthy(&fixture);
    transport.suspend_after.set(Some(8));
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &DONORS,
        PullLimits::default(),
    )
    .unwrap();
    let mut verification = Vec::new();
    {
        let mut future = pin!(recover(&mut pull, &transport, &mut verification, 8));
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(pull.symbol_count(), 8);
    assert_eq!(pull.pending_count(), 0);
    assert_eq!(transport.counters.live.get(), 0);
    let resumed = Transport::healthy(&fixture);
    let object = immediate(recover(&mut pull, &resumed, &mut verification, 8)).unwrap();
    assert_eq!(object.plaintext(), fixture.plaintext);
    assert_eq!(resumed.counters.live.get(), 0);
}

#[test]
fn issuance_budget_exhaustion_drains_already_requested_useful_symbols() {
    let fixture = Fixture::new(125);
    let limits = PullLimits {
        max_requests: fixture.sources as u64,
        ..PullLimits::default()
    };
    let transport = Transport::healthy(&fixture);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        limits,
    )
    .unwrap();
    let object = immediate(recover(&mut pull, &transport, &mut Vec::new(), 6)).unwrap();
    assert_eq!(object.plaintext(), fixture.plaintext);
    assert_eq!(transport.counters.started.get(), fixture.sources);
    assert_eq!(pull.pending_count(), 0);
}

#[test]
fn esi_exhaustion_does_not_discard_in_flight_systematic_symbols() {
    let fixture = Fixture::new(126);
    let limits = PullLimits {
        max_esi: fixture.sources as u32 - 1,
        ..PullLimits::default()
    };
    let transport = Transport::healthy(&fixture);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        limits,
    )
    .unwrap();
    let object = immediate(recover(&mut pull, &transport, &mut Vec::new(), 6)).unwrap();
    assert_eq!(object.plaintext(), fixture.plaintext);
    assert_eq!(pull.pending_count(), 0);
}

#[test]
fn fatal_error_and_constructor_panic_retire_the_entire_owned_batch() {
    for panic_on in [None, Some(3)] {
        let fixture = Fixture::new(127);
        let mut transport = Transport::healthy(&fixture);
        transport.panic_on = panic_on;
        transport.fatal = Some(DonorId(1));
        let mut pull = BondedPull::new(
            &fixture.encoding,
            fixture.target(),
            &DEK,
            &DONORS,
            PullLimits::default(),
        )
        .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            immediate(recover(&mut pull, &transport, &mut Vec::new(), 6))
        }));
        if panic_on.is_some() {
            assert!(result.is_err());
        } else {
            assert!(matches!(
                result.unwrap(),
                Err(PullDriveError::Transport("authority revoked"))
            ));
        }
        assert_eq!(pull.pending_count(), 0);
        assert_eq!(transport.counters.live.get(), 0);
    }
}

#[test]
fn instant_timeouts_cooperate_instead_of_monopolizing_the_executor() {
    let fixture = Fixture::new(128);
    let mut transport = Transport::healthy(&fixture);
    transport.timeouts_only = true;
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &DONORS,
        PullLimits::default(),
    )
    .unwrap();
    let mut verification = Vec::new();
    {
        let mut future = pin!(recover(&mut pull, &transport, &mut verification, 6));
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    assert!(transport.counters.started.get() <= 64 + 6);
    assert_eq!(pull.pending_count(), 0);
    assert_eq!(transport.counters.live.get(), 0);
}

#[test]
fn a_driver_does_not_steal_another_owners_reservations() {
    let fixture = Fixture::new(129);
    let transport = Transport::healthy(&fixture);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &DONORS,
        PullLimits::default(),
    )
    .unwrap();
    let request = pull.schedule(1).unwrap()[0];
    assert!(matches!(
        immediate(recover(&mut pull, &transport, &mut Vec::new(), 6)),
        Err(PullDriveError::OutstandingRequests)
    ));
    assert_eq!(pull.pending_count(), 1);
    assert_eq!(transport.counters.started.get(), 0);
    pull.expire(request).unwrap();
}

#[test]
fn repeated_timeouts_preserve_the_original_request_budget() {
    let fixture = Fixture::new(130);
    let mut transport = Transport::healthy(&fixture);
    transport.timeouts_only = true;
    let limits = PullLimits {
        max_requests: 20,
        ..PullLimits::default()
    };
    let mut pull =
        BondedPull::new(&fixture.encoding, fixture.target(), &DEK, &DONORS, limits).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            immediate(recover(&mut pull, &transport, &mut Vec::new(), 6)),
            Err(PullDriveError::Pull(PullError::RequestBudget))
        ));
        assert_eq!(pull.pending_count(), 0);
    }
    assert_eq!(transport.counters.started.get(), 20);
}

struct Catalog {
    fixtures: Vec<Fixture>,
    lookups: Cell<usize>,
    limits: PullLimits,
    wrong_identity_key: bool,
}

impl Catalog {
    fn new(salts: &[u8]) -> Self {
        Self {
            fixtures: salts.iter().copied().map(Fixture::new).collect(),
            lookups: Cell::new(0),
            limits: PullLimits::default(),
            wrong_identity_key: false,
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
        let fixture = self
            .fixtures
            .iter()
            .find(|fixture| fixture.encoding.object_id() == object.object_id)
            .ok_or("object absent from pinned catalog")?;
        let mut target = fixture.target();
        if self.wrong_identity_key {
            target.k_oid = &[0; 32];
        }
        Ok(SeedRecovery {
            encoding: &fixture.encoding,
            target,
            dek: &DEK,
            donors: &DONORS,
            limits: self.limits,
        })
    }
}

#[test]
fn seed_inventory_and_namespace_mismatches_fail_before_network_requests() {
    let catalog = Catalog::new(&[131]);
    let transport = Transport::healthy(&catalog.fixtures[0]);
    let mut verification = Vec::new();
    let spec = catalog.spec(0);
    let mut source = StreamingSeedSource::new(
        support::namespace(),
        &catalog,
        &transport,
        &mut verification,
        6,
    );
    let mut wrong = spec;
    wrong.object_kind += 1;
    assert!(matches!(
        immediate(source.recover(wrong)),
        Err(SeedAcquireError::InvalidObject)
    ));
    wrong = spec;
    wrong.compressed_len += 1;
    assert!(matches!(
        immediate(source.recover(wrong)),
        Err(SeedAcquireError::InvalidObject)
    ));
    assert!(source.active_object().is_none());
    assert_eq!(transport.counters.started.get(), 0);
    drop(source);

    let mut namespace = support::namespace();
    namespace.0[0] ^= 1;
    let mut source =
        StreamingSeedSource::new(namespace, &catalog, &transport, &mut verification, 6);
    assert!(matches!(
        immediate(source.recover(spec)),
        Err(SeedAcquireError::WrongNamespace)
    ));
    assert_eq!(transport.counters.started.get(), 0);
}

#[test]
fn seed_cancellation_resumes_the_same_pinned_object_without_resetting_progress() {
    let catalog = Catalog::new(&[132, 133]);
    let transport = Transport::healthy(&catalog.fixtures[0]);
    transport.suspend_after.set(Some(8));
    let mut verification = Vec::new();
    let spec = catalog.spec(0);
    let mut source = StreamingSeedSource::new(
        support::namespace(),
        &catalog,
        &transport,
        &mut verification,
        8,
    );
    {
        let mut future = pin!(source.recover(spec));
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(source.active_object(), Some(spec));
    assert_eq!(source.retained_symbols(), 8);
    assert_eq!(transport.counters.live.get(), 0);
    let requests = transport.counters.started.get();
    assert!(matches!(
        immediate(source.recover(catalog.spec(1))),
        Err(SeedAcquireError::RecoveryInProgress)
    ));
    assert_eq!(transport.counters.started.get(), requests);
    assert_eq!(source.retained_symbols(), 8);
    // Recovering a different object cannot silently replace the pinned closure.
    assert_eq!(catalog.lookups.get(), 1);
    transport.suspend_after.set(None);
    let recovered = immediate(source.recover(spec)).unwrap();
    assert_eq!(recovered.plaintext(), catalog.fixtures[0].plaintext);
    assert_eq!(catalog.lookups.get(), 1);
    assert!(source.active_object().is_none());
    assert_eq!(transport.counters.live.get(), 0);
}

#[test]
fn seed_retry_does_not_reset_an_exhausted_request_budget() {
    let mut catalog = Catalog::new(&[134]);
    catalog.limits.max_requests = 20;
    let mut transport = Transport::healthy(&catalog.fixtures[0]);
    transport.timeouts_only = true;
    let mut verification = Vec::new();
    let mut source = StreamingSeedSource::new(
        support::namespace(),
        &catalog,
        &transport,
        &mut verification,
        6,
    );
    for _ in 0..2 {
        assert!(matches!(
            immediate(source.recover(catalog.spec(0))),
            Err(SeedAcquireError::Pull(PullDriveError::Pull(
                PullError::RequestBudget
            )))
        ));
        assert_eq!(source.active_object(), Some(catalog.spec(0)));
        assert_eq!(transport.counters.live.get(), 0);
    }
    assert_eq!(catalog.lookups.get(), 1);
    assert_eq!(transport.counters.started.get(), 20);
}

struct CatalogTransport<'a> {
    objects: Vec<Transport<'a>>,
}

impl<'a> CatalogTransport<'a> {
    fn with_silent_donor(catalog: &'a Catalog, donor: DonorId) -> Self {
        Self {
            objects: catalog
                .fixtures
                .iter()
                .map(|fixture| {
                    let mut transport = Transport::healthy(fixture);
                    transport.silent = Some(donor);
                    transport
                })
                .collect(),
        }
    }
}

impl SymbolTransport for CatalogTransport<'_> {
    type Error = &'static str;

    fn request(
        &self,
        request: PullRequest,
    ) -> impl Future<Output = Result<ReplyOutcome, Self::Error>> {
        async move {
            let transport = self
                .objects
                .iter()
                .find(|transport| transport.fixture.encoding.object_id() == request.object_id)
                .ok_or("transport has no authorized object route")?;
            transport.request(request).await
        }
    }
}

#[derive(Default)]
struct MemoryRaftRoot(Vec<PersistentState<u64>>);

impl RaftPublisher<u64> for MemoryRaftRoot {
    type Error = &'static str;

    fn publish(
        &mut self,
        state: &PersistentState<u64>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.0.push(state.clone());
        ready(Ok(()))
    }
}

fn seed_offer(catalog: &Catalog) -> (Raft<u64>, SnapshotTransfer, SeedPlan) {
    let config = Configuration::stable(
        Domain([5; 32]),
        [6; 32],
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
    // Synthetic verifier output for a composition test. This is not a canonical
    // manifest parser, a signed availability certificate, or production storage.
    let plan = SeedPlan::from_authenticated_inventory(
        anchor,
        (0..catalog.fixtures.len()).map(|index| catalog.spec(index)),
        SeedLimits::default(),
    )
    .unwrap();
    let mut raft = Raft::new(MemberId(2), config.clone(), Limits::default()).unwrap();
    let mut root = MemoryRaftRoot::default();
    let output = immediate(sequence(
        &mut raft,
        &mut root,
        Event::Receive(Envelope {
            domain: config.domain(),
            configuration: config.identity(),
            from: MemberId(1),
            to: MemberId(2),
            message: Message::InstallSnapshot {
                term: 3,
                request: 1,
                snapshot: cut,
            },
        }),
    ))
    .unwrap();
    assert!(output.installed_snapshot.is_none());
    assert!(output.messages.is_empty());
    (
        raft,
        output.snapshot_transfers.into_iter().next().unwrap(),
        plan,
    )
}

#[derive(Default)]
struct MemorySeedRoot {
    objects: Vec<[u8; 32]>,
    consensus: Option<PersistentState<u64>>,
    snapshot_calls: usize,
    suspend: bool,
    fail: bool,
    wrong_root: bool,
}

impl SeedPublisher<u64> for MemorySeedRoot {
    type Error = &'static str;

    fn publish_object(
        &mut self,
        object: ObjectPublication<'_>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.objects.push(object.object().object_id().0);
        ready(Ok(()))
    }

    fn publish_snapshot(
        &mut self,
        snapshot: SnapshotPublication<'_, u64>,
    ) -> impl Future<Output = Result<RootPublicationEvidence, Self::Error>> {
        self.snapshot_calls += 1;
        for object in snapshot.seed().plan().objects() {
            assert!(self.objects.contains(&object.object_id.0));
        }
        let anchor = snapshot.seed().plan().anchor();
        let cut = snapshot.consensus().snapshot().unwrap();
        assert_eq!(cut.index(), anchor.raft_index);
        assert_eq!(cut.term(), anchor.raft_term);
        assert_eq!(cut.state_root(), anchor.state_root.0);
        assert_eq!(cut.retention_floor(), anchor.retention_floor.0);
        self.consensus = Some(snapshot.consensus().clone());
        // Only a memory test double can synthesize this evidence. Production
        // publishers must obtain it from RootStore's actual post-sync reread.
        let mut evidence = RootPublicationEvidence {
            written_index: 1,
            slot_generation: anchor.publication_generation,
            root_manifest_oid: anchor.publication_root.0,
        };
        if self.wrong_root {
            evidence.root_manifest_oid[0] ^= 1;
        }
        let (suspend, fail) = (self.suspend, self.fail);
        async move {
            if suspend {
                pending::<()>().await;
            }
            if fail {
                Err("root publication outcome unknown")
            } else {
                Ok(evidence)
            }
        }
    }
}

#[test]
fn streaming_seed_installs_exact_cut_without_waiting_for_silent_donor() {
    let catalog = Catalog::new(&[135, 136, 137, 138]);
    let transport = CatalogTransport::with_silent_donor(&catalog, DonorId(1));
    let mut verification = Vec::new();
    let mut source = StreamingSeedSource::new(
        support::namespace(),
        &catalog,
        &transport,
        &mut verification,
        6,
    );
    let (mut raft, transfer, plan) = seed_offer(&catalog);
    let expected_cut = transfer.snapshot().clone();
    let catchup = SnapshotCatchup::begin(&mut raft, support::namespace(), transfer, plan).unwrap();
    let mut root = MemorySeedRoot::default();
    let output = immediate(catchup.install(&mut source, &mut root)).unwrap();
    assert_eq!(root.snapshot_calls, 1);
    assert_eq!(root.objects.len(), 4);
    assert_eq!(output.installed_snapshot, Some(expected_cut.clone()));
    assert_eq!(
        raft.durable_state().unwrap().snapshot(),
        Some(&expected_cut)
    );
    assert_eq!(output.messages.len(), 1);
    assert!(matches!(
        output.messages[0].message,
        Message::SnapshotInstalled {
            term: 3,
            request: 1
        }
    ));
    for object in &transport.objects {
        assert_eq!(object.counters.live.get(), 0);
        assert!(object.counters.peak.get() <= 6);
        assert!(object.counters.started.get() >= object.fixture.sources);
    }
}

#[test]
fn streaming_seed_root_failure_or_wrong_evidence_never_releases_install_ack() {
    for wrong_root in [false, true] {
        let catalog = Catalog::new(&[139, 140, 141, 142]);
        let transport = CatalogTransport::with_silent_donor(&catalog, DonorId(1));
        let mut verification = Vec::new();
        let mut source = StreamingSeedSource::new(
            support::namespace(),
            &catalog,
            &transport,
            &mut verification,
            6,
        );
        let (mut raft, transfer, plan) = seed_offer(&catalog);
        let catchup =
            SnapshotCatchup::begin(&mut raft, support::namespace(), transfer, plan).unwrap();
        let mut root = MemorySeedRoot {
            fail: !wrong_root,
            wrong_root,
            ..MemorySeedRoot::default()
        };
        assert!(immediate(catchup.install(&mut source, &mut root)).is_err());
        assert_eq!(root.objects.len(), 4);
        assert_eq!(root.snapshot_calls, 1);
        assert_eq!(raft.role(), Err(RaftError::RecoveryRequired));
        assert!(
            transport
                .objects
                .iter()
                .all(|object| object.counters.live.get() == 0)
        );
    }
}

#[test]
fn cancelling_streaming_seed_during_atomic_publication_fences_the_member() {
    let catalog = Catalog::new(&[143, 144, 145, 146]);
    let transport = CatalogTransport::with_silent_donor(&catalog, DonorId(1));
    let mut verification = Vec::new();
    let mut source = StreamingSeedSource::new(
        support::namespace(),
        &catalog,
        &transport,
        &mut verification,
        6,
    );
    let (mut raft, transfer, plan) = seed_offer(&catalog);
    let catchup = SnapshotCatchup::begin(&mut raft, support::namespace(), transfer, plan).unwrap();
    let mut root = MemorySeedRoot {
        suspend: true,
        ..MemorySeedRoot::default()
    };
    {
        let mut future = pin!(catchup.install(&mut source, &mut root));
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(root.objects.len(), 4);
    assert_eq!(root.snapshot_calls, 1);
    assert!(root.consensus.is_some()); // Publication may have escaped before drop.
    assert_eq!(raft.role(), Err(RaftError::RecoveryRequired));
    assert!(
        transport
            .objects
            .iter()
            .all(|object| object.counters.live.get() == 0)
    );
}

#[test]
fn failed_final_object_authentication_cannot_be_reset_by_retrying_seed_recovery() {
    let mut catalog = Catalog::new(&[147]);
    catalog.wrong_identity_key = true;
    let transport = Transport::healthy(&catalog.fixtures[0]);
    let mut verification = Vec::new();
    let mut source = StreamingSeedSource::new(
        support::namespace(),
        &catalog,
        &transport,
        &mut verification,
        6,
    );
    let spec = catalog.spec(0);
    assert!(matches!(
        immediate(source.recover(spec)),
        Err(SeedAcquireError::Pull(PullDriveError::Pull(
            PullError::Recovery(_)
        )))
    ));
    let requests = transport.counters.started.get();
    assert_eq!(transport.counters.live.get(), 0);
    assert!(matches!(
        immediate(source.recover(spec)),
        Err(SeedAcquireError::Pull(PullDriveError::Pull(
            PullError::Closed
        )))
    ));
    assert_eq!(transport.counters.started.get(), requests);
    assert_eq!(catalog.lookups.get(), 1);
}

struct PairMember {
    raft: Raft<u64>,
    root: MemoryRaftRoot,
}

fn pump_pair(members: &mut [PairMember; 2], messages: Vec<Envelope<u64>>) {
    let mut queue: VecDeque<_> = messages.into();
    let mut delivered = 0;
    while let Some(envelope) = queue.pop_front() {
        // Three configured voters, but only two are reachable throughout.
        if envelope.to == MemberId(3) {
            continue;
        }
        assert!(envelope.to == MemberId(1) || envelope.to == MemberId(2));
        delivered += 1;
        assert!(delivered < 4096, "replication must quiesce");
        let member = &mut members[envelope.to.0 as usize - 1];
        let output = immediate(sequence(
            &mut member.raft,
            &mut member.root,
            Event::Receive(envelope),
        ))
        .unwrap();
        assert!(output.snapshot_transfers.is_empty());
        queue.extend(output.messages);
    }
}

#[test]
fn seeded_replica_rejoins_multi_member_sequencing_with_distinct_append_and_commit_roots() {
    let catalog = Catalog::new(&[148, 149, 150, 151]);
    let transport = CatalogTransport::with_silent_donor(&catalog, DonorId(1));
    let mut verification = Vec::new();
    let mut source = StreamingSeedSource::new(
        support::namespace(),
        &catalog,
        &transport,
        &mut verification,
        6,
    );
    let (mut follower, transfer, plan) = seed_offer(&catalog);
    let catchup =
        SnapshotCatchup::begin(&mut follower, support::namespace(), transfer, plan).unwrap();
    let mut snapshot_root = MemorySeedRoot::default();
    immediate(catchup.install(&mut source, &mut snapshot_root)).unwrap();

    // Model both members recovering the same verified application snapshot.
    // This is an in-process protocol campaign, NOT an on-disk crash test.
    let initial = snapshot_root.consensus.clone().unwrap();
    let leader = Raft::recover(MemberId(1), initial, Limits::default()).unwrap();
    let mut members = [
        PairMember {
            raft: leader,
            root: MemoryRaftRoot::default(),
        },
        PairMember {
            raft: follower,
            root: MemoryRaftRoot::default(),
        },
    ];
    let node = &mut members[0];
    let election = immediate(sequence(
        &mut node.raft,
        &mut node.root,
        Event::ElectionTimeout,
    ))
    .unwrap();
    pump_pair(&mut members, election.messages);
    let node = &mut members[0];
    let proposal = immediate(sequence(&mut node.raft, &mut node.root, Event::Propose(42))).unwrap();
    assert!(
        !proposal
            .committed
            .iter()
            .any(|entry| entry.entry.command == Some(42))
    );
    pump_pair(&mut members, proposal.messages);

    let first = members[0].raft.committed_after(12).unwrap();
    let second = members[1].raft.committed_after(12).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].entry.command, None); // Current-term leader no-op.
    assert_eq!(first[1].entry.command, Some(42));
    assert_eq!(first[1].index, 14);

    // The follower must first persist the append with commit=13, then publish
    // commit=14 separately. Neither a payload decode nor an append reply is a
    // license to fuse these two independently observable multi-member roots.
    let states = &members[1].root.0;
    let appended = states
        .iter()
        .position(|state| state.commit_index() == 13 && state.entries().len() == 2)
        .unwrap();
    let committed = states
        .iter()
        .position(|state| state.commit_index() == 14)
        .unwrap();
    assert!(appended < committed);
    let reopened = Raft::recover(
        MemberId(2),
        states.last().unwrap().clone(),
        Limits::default(),
    )
    .unwrap();
    assert_eq!(reopened.committed_after(12).unwrap(), first);
}

#[test]
fn streaming_respects_upstream_authentication_reservations_and_drains_the_last_reply() {
    let fixture = Fixture::new(152);
    let limits = PullLimits {
        max_verifications: fixture.sources as u64,
        ..PullLimits::default()
    };
    let transport = Transport::healthy(&fixture);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        limits,
    )
    .unwrap();
    let object = immediate(recover(&mut pull, &transport, &mut Vec::new(), 6)).unwrap();
    assert_eq!(object.plaintext(), fixture.plaintext);
    assert_eq!(transport.counters.started.get(), fixture.sources);
    assert_eq!(transport.counters.live.get(), 0);
    assert_eq!(pull.pending_count(), 0);
}

struct RecoveringTransport<'a> {
    inner: Transport<'a>,
    offline: Cell<bool>,
    issued: RefCell<Vec<PullRequest>>,
}

impl SymbolTransport for RecoveringTransport<'_> {
    type Error = &'static str;

    fn request(
        &self,
        request: PullRequest,
    ) -> impl Future<Output = Result<ReplyOutcome, Self::Error>> {
        self.issued.borrow_mut().push(request);
        async move {
            if self.offline.get() {
                Ok(ReplyOutcome::Unavailable)
            } else {
                self.inner.request(request).await
            }
        }
    }
}

#[test]
fn an_authenticated_returning_donor_resumes_without_resetting_residues_or_catalog() {
    let catalog = Catalog::new(&[153]);
    let transport = RecoveringTransport {
        inner: Transport::healthy(&catalog.fixtures[0]),
        offline: Cell::new(true),
        issued: RefCell::new(Vec::new()),
    };
    let mut verification = Vec::new();
    let mut source = StreamingSeedSource::new(
        support::namespace(),
        &catalog,
        &transport,
        &mut verification,
        6,
    );
    let spec = catalog.spec(0);
    assert!(matches!(
        immediate(source.recover(spec)),
        Err(SeedAcquireError::Pull(PullDriveError::Pull(
            PullError::NoAvailableDonor
        )))
    ));
    assert_eq!(source.active_object(), Some(spec));
    assert_eq!(catalog.lookups.get(), 1);
    let issued = transport.issued.borrow().len();
    assert!(issued >= 6);
    let previous_last = transport
        .issued
        .borrow()
        .iter()
        .filter(|request| request.donor == DonorId(3))
        .map(|request| request.esi)
        .max()
        .unwrap();
    transport.offline.set(false);
    // The test models the caller's fresh authenticated donor-recovery decision.
    // A production caller must obtain that authority, not trust a gossip flag.
    source.donor_available(DonorId(3)).unwrap();
    let object = immediate(source.recover(spec)).unwrap();
    assert_eq!(object.plaintext(), catalog.fixtures[0].plaintext);
    assert_eq!(catalog.lookups.get(), 1);
    assert!(
        transport.issued.borrow()[issued..]
            .iter()
            .all(|request| request.donor == DonorId(3)
                && request.esi > previous_last
                && request.esi % 3 == 2)
    );
    assert_eq!(transport.inner.counters.live.get(), 0);
    assert!(source.active_object().is_none());
    assert!(matches!(
        source.donor_available(DonorId(3)),
        Err(SeedAcquireError::InvalidObject)
    ));
}

#[test]
fn abandoning_seed_recovery_is_explicit_and_does_not_touch_durable_publication() {
    let catalog = Catalog::new(&[154]);
    let transport = Transport::healthy(&catalog.fixtures[0]);
    transport.suspend_after.set(Some(8));
    let mut verification = Vec::new();
    let mut source = StreamingSeedSource::new(
        support::namespace(),
        &catalog,
        &transport,
        &mut verification,
        8,
    );
    let spec = catalog.spec(0);
    {
        let mut future = pin!(source.recover(spec));
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(source.retained_symbols(), 8);
    assert_eq!(transport.counters.live.get(), 0);
    source.abandon_recovery();
    assert_eq!(source.retained_symbols(), 0);
    assert!(source.active_object().is_none());
    transport.suspend_after.set(None);
    let object = immediate(source.recover(spec)).unwrap();
    assert_eq!(object.plaintext(), catalog.fixtures[0].plaintext);
    assert_eq!(catalog.lookups.get(), 2); // Deliberate new attempt, not a retry.
}
