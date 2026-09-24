#![cfg(not(target_arch = "wasm32"))]

#[allow(dead_code)]
#[path = "../../fgdb-chronicle/tests/support/multiblock.rs"]
mod support;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::future::{Future, pending, ready};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use fgdb_chronicle::seed::{ObjectPublication, SeedAnchor, SeedLimits, SeedObjectSpec, SeedPlan};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_chronicle::transfer::{BondedPull, DonorId, PullLimits, PullRequest};
use fgdb_order::{
    Configuration, Domain, Envelope, Error as RaftError, Event, Limits, MemberId, Message,
    PersistentState, Raft, SnapshotCut,
};
use fgdb_repl::driver::bonded::streaming::{self, StreamingSeedSource, SymbolTransport};
use fgdb_repl::driver::bonded::{self, PullReply, PullTransport, ReplyOutcome};
use fgdb_repl::driver::{
    RaftPublisher, SeedCatalog, SeedDriveError, SeedObjectSource, SeedPublisher, SeedRecovery,
    sequence,
};
use fgdb_repl::{SnapshotCatchup, SnapshotPublication};
use support::{DEK, Fixture};

struct NoopWake;
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

// Borrowed transports and publishers below complete immediately except for
// deliberately silent/cancelled requests and the driver's cooperative yield.
// Bounded polling is a test executor, not an ATP runtime or deadline model.
fn run_ready<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    for _ in 0..4096 {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
    }
    panic!("fixture failed to complete without waking its silent donors");
}

struct Transport<'a> {
    fixtures: &'a [Fixture],
    silent: Option<DonorId>,
    corrupt: Option<DonorId>,
    pause_after: Cell<Option<usize>>,
    delivered: Cell<usize>,
    issued: RefCell<Vec<PullRequest>>,
}

impl<'a> Transport<'a> {
    fn new(fixtures: &'a [Fixture]) -> Self {
        Self {
            fixtures,
            silent: None,
            corrupt: None,
            pause_after: Cell::new(None),
            delivered: Cell::new(0),
            issued: RefCell::new(Vec::new()),
        }
    }

    fn record(&self, request: PullRequest) -> Vec<u8> {
        let fixture = self
            .fixtures
            .iter()
            .find(|f| f.encoding.object_id() == request.object_id)
            .unwrap();
        assert_eq!(request.encoding_id, fixture.encoding.encoding_id());
        fixture.records[request.source_block as usize][request.esi as usize].clone()
    }
}

impl SymbolTransport for Transport<'_> {
    type Error = &'static str;
    fn request(
        &self,
        request: PullRequest,
    ) -> impl Future<Output = Result<ReplyOutcome, Self::Error>> {
        self.issued.borrow_mut().push(request);
        async move {
            if self.silent == Some(request.donor)
                || self
                    .pause_after
                    .get()
                    .is_some_and(|limit| self.delivered.get() >= limit)
            {
                pending::<()>().await;
            }
            let mut bytes = self.record(request);
            if self.corrupt == Some(request.donor) {
                *bytes.last_mut().unwrap() ^= 1;
            }
            self.delivered.set(self.delivered.get() + 1);
            Ok(ReplyOutcome::Record(bytes))
        }
    }
}

impl PullTransport for Transport<'_> {
    type Error = &'static str;
    fn exchange(
        &mut self,
        requests: &[PullRequest],
    ) -> impl Future<Output = Result<Vec<PullReply>, Self::Error>> {
        self.issued.borrow_mut().extend_from_slice(requests);
        let replies = requests
            .iter()
            .rev()
            .map(|request| PullReply {
                request: *request,
                outcome: ReplyOutcome::Record(self.record(*request)),
            })
            .collect();
        ready(Ok(replies))
    }
}

const DONORS: [DonorId; 3] = [DonorId(1), DonorId(2), DonorId(3)];

#[test]
fn reordered_batched_replies_with_equal_esis_across_blocks_recover() {
    let fixtures = [Fixture::new(3, 3, 64)];
    let f = &fixtures[0];
    let mut pull = BondedPull::new(
        &f.encoding,
        f.target(),
        &DEK,
        &DONORS,
        PullLimits::default(),
    )
    .unwrap();
    let mut transport = Transport::new(&fixtures);
    let object = run_ready(bonded::recover(
        &mut pull,
        &mut transport,
        &mut Vec::new(),
        9,
    ))
    .unwrap();
    assert_eq!(object.plaintext(), f.plaintext);
    assert_eq!(pull.pending_count(), 0);
    let issued = transport.issued.borrow();
    let zeros: BTreeSet<_> = issued
        .iter()
        .filter(|r| r.esi == 0)
        .map(|r| r.source_block)
        .collect();
    assert_eq!(zeros, BTreeSet::from([0, 1, 2]));
}

#[test]
fn streaming_recovers_all_blocks_with_one_silent_and_one_corrupt_donor() {
    let fixtures = [Fixture::new(3, 3, 128)];
    let f = &fixtures[0];
    let mut pull = BondedPull::new(
        &f.encoding,
        f.target(),
        &DEK,
        &DONORS,
        PullLimits::default(),
    )
    .unwrap();
    let mut transport = Transport::new(&fixtures);
    transport.silent = Some(DonorId(1));
    transport.corrupt = Some(DonorId(2));
    let object = run_ready(streaming::recover(
        &mut pull,
        &transport,
        &mut Vec::new(),
        9,
    ))
    .unwrap();
    assert_eq!(object.plaintext(), f.plaintext);
    assert_eq!(pull.pending_count(), 0);
    for block in 0..3 {
        assert!(pull.block_symbol_count(block).unwrap() >= f.sources[block as usize]);
    }
    let issued = transport.issued.borrow();
    assert!(issued.iter().any(|r| r.donor == DonorId(1)));
    assert!(issued.iter().any(|r| r.donor == DonorId(2)));
    assert!(
        issued
            .iter()
            .any(|r| r.donor == DonorId(3) && r.esi as usize >= f.sources[r.source_block as usize])
    );
}

#[test]
fn dropping_a_multiblock_stream_retires_only_requests_not_verified_equations() {
    let fixtures = [Fixture::new(3, 1, 128)];
    let f = &fixtures[0];
    let mut pull = BondedPull::new(
        &f.encoding,
        f.target(),
        &DEK,
        &DONORS,
        PullLimits::default(),
    )
    .unwrap();
    let transport = Transport::new(&fixtures);
    transport.pause_after.set(Some(6));
    let mut verification = Vec::new();
    {
        let mut future = pin!(streaming::recover(
            &mut pull,
            &transport,
            &mut verification,
            9
        ));
        let waker = Waker::from(Arc::new(NoopWake));
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }
    assert_eq!(pull.pending_count(), 0);
    assert_eq!(pull.symbol_count(), 6);
    let before: Vec<_> = (0..3)
        .map(|b| pull.block_symbol_count(b).unwrap())
        .collect();
    transport.pause_after.set(None);
    let object = run_ready(streaming::recover(
        &mut pull,
        &transport,
        &mut verification,
        9,
    ))
    .unwrap();
    assert_eq!(object.plaintext(), f.plaintext);
    for (block, count) in before.into_iter().enumerate() {
        assert!(pull.block_symbol_count(block as u32).unwrap() >= count);
    }
    let issued = transport.issued.borrow();
    let distinct: BTreeSet<_> = issued.iter().map(|r| (r.source_block, r.esi)).collect();
    assert_eq!(distinct.len(), issued.len());
}

struct Catalog {
    fixtures: Vec<Fixture>,
}
impl SeedCatalog for Catalog {
    type Error = &'static str;
    fn recovery(&self, object: SeedObjectSpec) -> Result<SeedRecovery<'_>, Self::Error> {
        let f = self
            .fixtures
            .iter()
            .find(|f| f.encoding.object_id() == object.object_id)
            .ok_or("unknown object")?;
        Ok(SeedRecovery {
            encoding: &f.encoding,
            target: f.target(),
            dek: &DEK,
            donors: &DONORS,
            limits: PullLimits::default(),
        })
    }
}

fn spec(f: &Fixture) -> SeedObjectSpec {
    SeedObjectSpec {
        object_id: f.encoding.object_id(),
        object_kind: support::KIND,
        compressed_len: f.plaintext.len() as u64,
    }
}

#[test]
fn streaming_seed_source_resumes_its_multiblock_object_after_cancellation() {
    let catalog = Catalog {
        fixtures: vec![Fixture::new(3, 3, 128)],
    };
    let f = &catalog.fixtures[0];
    let transport = Transport::new(&catalog.fixtures);
    transport.pause_after.set(Some(6));
    let mut verification = Vec::new();
    let mut source = StreamingSeedSource::new(
        support::namespace(),
        &catalog,
        &transport,
        &mut verification,
        9,
    );
    {
        let mut future = pin!(source.recover(spec(f)));
        let waker = Waker::from(Arc::new(NoopWake));
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }
    assert_eq!(source.retained_symbols(), 6);
    assert_eq!(source.active_object(), Some(spec(f)));
    transport.pause_after.set(None);
    let object = run_ready(source.recover(spec(f))).unwrap();
    assert_eq!(object.plaintext(), f.plaintext);
    assert_eq!(source.active_object(), None);
    let issued = transport.issued.borrow();
    assert_eq!(
        issued.len(),
        issued
            .iter()
            .map(|r| (r.source_block, r.esi))
            .collect::<BTreeSet<_>>()
            .len()
    );
}

// Explicit memory-only root/ownership model, not a canonical manifest encoder,
// authenticated catalog validator, filesystem publisher, or membership grant.
#[derive(Default)]
struct Root {
    objects: BTreeSet<[u8; 32]>,
    state: Option<PersistentState<u64>>,
    installs: usize,
    fail: bool,
}

impl RaftPublisher<u64> for Root {
    type Error = &'static str;
    fn publish(
        &mut self,
        state: &PersistentState<u64>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.state = Some(state.clone());
        ready(Ok(()))
    }
}

impl SeedPublisher<u64> for Root {
    type Error = &'static str;
    fn publish_object(
        &mut self,
        publication: ObjectPublication<'_>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        assert!(
            publication
                .object()
                .encoding()
                .descriptor()
                .source_block_count
                > 1
        );
        self.objects.insert(publication.object().object_id().0);
        ready(Ok(()))
    }
    fn publish_snapshot(
        &mut self,
        publication: SnapshotPublication<'_, u64>,
    ) -> impl Future<Output = Result<RootPublicationEvidence, Self::Error>> {
        for object in publication.seed().plan().objects() {
            assert!(self.objects.contains(&object.object_id.0));
        }
        let anchor = publication.seed().plan().anchor();
        assert_eq!(
            publication.consensus().snapshot().unwrap().index(),
            anchor.raft_index
        );
        self.state = Some(publication.consensus().clone());
        self.installs += 1;
        ready(if self.fail {
            Err("model root outcome unknown")
        } else {
            Ok(RootPublicationEvidence {
                written_index: 1,
                slot_generation: anchor.publication_generation,
                root_manifest_oid: anchor.publication_root.0,
            })
        })
    }
}

fn install(fail: bool) {
    let catalog = Catalog {
        fixtures: (0..4)
            .map(|i| Fixture::with_shape(40 + i, 4093, 256, u16::from(i) + 2, 3, 4, 128))
            .collect(),
    };
    let config = Configuration::stable(
        Domain([11; 32]),
        [12; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [],
    )
    .unwrap();
    let roots: Vec<_> = catalog
        .fixtures
        .iter()
        .map(|f| f.encoding.object_id())
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
    // Inventory is a synthetic verifier projection for this integration test.
    let plan = SeedPlan::from_authenticated_inventory(
        anchor,
        catalog.fixtures.iter().map(spec),
        SeedLimits::default(),
    )
    .unwrap();
    let mut raft = Raft::new(MemberId(2), config.clone(), Limits::default()).unwrap();
    let mut root = Root {
        fail,
        ..Root::default()
    };
    let offered = run_ready(sequence(
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
                snapshot: cut.clone(),
            },
        }),
    ))
    .unwrap();
    let transfer = offered.snapshot_transfers.into_iter().next().unwrap();
    assert!(offered.messages.is_empty());
    let catchup = SnapshotCatchup::begin(&mut raft, support::namespace(), transfer, plan).unwrap();
    let mut transport = Transport::new(&catalog.fixtures);
    transport.silent = Some(DonorId(1));
    let mut verification = Vec::new();
    let mut source = StreamingSeedSource::new(
        support::namespace(),
        &catalog,
        &transport,
        &mut verification,
        9,
    );
    let result = run_ready(catchup.install(&mut source, &mut root));
    assert_eq!(root.objects.len(), 4);
    assert_eq!(root.installs, 1);
    if fail {
        assert!(matches!(
            result,
            Err(SeedDriveError::Publication("model root outcome unknown"))
        ));
        assert_eq!(raft.role(), Err(RaftError::RecoveryRequired));
    } else {
        let output = result.unwrap();
        assert_eq!(output.installed_snapshot, Some(cut));
        assert!(output.messages.iter().any(|envelope| matches!(
            envelope.message,
            Message::SnapshotInstalled {
                term: 3,
                request: 1
            }
        )));
        let recovered = Raft::recover(MemberId(2), root.state.unwrap(), Limits::default()).unwrap();
        assert_eq!(recovered.durable_state().unwrap().commit_index(), 12);
    }
}

#[test]
fn multiblock_seed_closure_reaches_the_atomic_raft_installation_gate() {
    install(false);
}

#[test]
fn failed_multiblock_seed_root_publication_releases_no_install_ack() {
    install(true);
}
