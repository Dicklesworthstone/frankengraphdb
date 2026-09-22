#![cfg(not(target_arch = "wasm32"))]

// Reuse the real Chronicle crypto/FEC fixture rather than inventing a second
// encoding or mocking VerifiedObject's authentication boundary.
#[allow(dead_code)]
#[path = "../../fgdb-chronicle/tests/support/bonded.rs"]
mod support;

use std::collections::VecDeque;
use std::future::{Future, pending, ready};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use fgdb_chronicle::seed::{ObjectPublication, SeedAnchor, SeedLimits, SeedObjectSpec, SeedPlan};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_chronicle::transfer::{BondedPull, DonorId, PullLimits, PullRequest};
use fgdb_order::{
    Configuration, Domain, Envelope, Error as RaftError, Event, Limits, MemberId, Message, Output,
    PersistentState, Raft, Role, SnapshotCut, SnapshotTransfer,
};
use fgdb_repl::driver::bonded::{self, PullReply, PullTransport, ReplyOutcome};
use fgdb_repl::driver::{
    BondedSeedSource, RaftPublisher, SeedAcquireError, SeedCatalog, SeedObjectSource,
    SeedPublisher, SeedRecovery, SequenceError, sequence,
};
use fgdb_repl::{SnapshotCatchup, SnapshotPublication};
use support::{DEK, Fixture};

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

#[derive(Default)]
struct MemoryRoot {
    state: Option<PersistentState<u64>>,
}

impl RaftPublisher<u64> for MemoryRoot {
    type Error = &'static str;
    fn publish(
        &mut self,
        state: &PersistentState<u64>,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.state = Some(state.clone());
        ready(Ok(()))
    }
}

struct Member {
    raft: Raft<u64>,
    root: MemoryRoot,
}

fn members(config: &Configuration, count: u128) -> Vec<Member> {
    (1..=count)
        .map(|id| Member {
            raft: Raft::new(MemberId(id), config.clone(), Limits::default()).unwrap(),
            root: MemoryRoot::default(),
        })
        .collect()
}

fn step(members: &mut [Member], id: usize, event: Event<u64>) -> Output<u64> {
    let member = &mut members[id - 1];
    immediate(sequence(&mut member.raft, &mut member.root, event)).unwrap()
}

fn pump(members: &mut [Member], messages: Vec<Envelope<u64>>, reachable: &[u128]) {
    let mut queue: VecDeque<_> = messages.into();
    let mut deliveries = 0;
    while let Some(envelope) = queue.pop_front() {
        deliveries += 1;
        assert!(deliveries < 4096, "consensus dispatch must quiesce");
        if reachable.contains(&envelope.from.0) && reachable.contains(&envelope.to.0) {
            let output = step(members, envelope.to.0 as usize, Event::Receive(envelope));
            queue.extend(output.messages);
        }
    }
}

#[test]
fn minority_loss_and_learner_ack_do_not_release_a_commit() {
    let config = Configuration::stable(
        Domain([1; 32]),
        [2; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [MemberId(4)],
    )
    .unwrap();
    let mut nodes = members(&config, 4);
    let election = step(&mut nodes, 1, Event::ElectionTimeout);
    pump(&mut nodes, election.messages, &[1, 2, 4]);
    assert_eq!(nodes[0].raft.role(), Ok(Role::Leader));

    // Leader + learner is not a majority of voters, even if both persist.
    let proposal = step(&mut nodes, 1, Event::Propose(42));
    pump(&mut nodes, proposal.messages, &[1, 4]);
    assert!(
        !nodes[0]
            .raft
            .committed_after(0)
            .unwrap()
            .iter()
            .any(|entry| entry.entry.command == Some(42))
    );

    // Restoring one voter is sufficient; the still-unavailable voter cannot
    // block progress. A heartbeat retries the in-flight append identity.
    for _ in 0..4 {
        let heartbeat = step(&mut nodes, 1, Event::Heartbeat);
        pump(&mut nodes, heartbeat.messages, &[1, 2, 4]);
    }
    for index in [0, 1] {
        let committed = nodes[index].raft.committed_after(0).unwrap();
        assert!(
            committed
                .iter()
                .any(|entry| entry.entry.command == Some(42))
        );
        let durable = nodes[index].root.state.as_ref().unwrap();
        assert!(
            committed
                .iter()
                .all(|entry| entry.index <= durable.commit_index())
        );
    }
    let recovered = Raft::recover(
        MemberId(2),
        nodes[1].root.state.clone().unwrap(),
        Limits::default(),
    )
    .unwrap();
    assert!(
        recovered
            .committed_after(0)
            .unwrap()
            .iter()
            .any(|entry| entry.entry.command == Some(42))
    );
}

#[test]
fn joint_configuration_requires_both_majorities() {
    let config = Configuration::joint(
        Domain([3; 32]),
        [4; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [MemberId(3), MemberId(4), MemberId(5)],
        [],
    )
    .unwrap();
    let mut nodes = members(&config, 5);
    let election = step(&mut nodes, 1, Event::ElectionTimeout);
    pump(&mut nodes, election.messages, &[1, 2, 3]);
    // Three of five union members, but only one of the three new voters.
    assert_ne!(nodes[0].raft.role(), Ok(Role::Leader));
    let mut nodes = members(&config, 5);
    let election = step(&mut nodes, 1, Event::ElectionTimeout);
    pump(&mut nodes, election.messages, &[1, 3, 4]);
    assert_eq!(nodes[0].raft.role(), Ok(Role::Leader));
}

struct Catalog {
    fixtures: Vec<Fixture>,
}

const DONORS: [DonorId; 3] = [DonorId(1), DonorId(2), DonorId(3)];

impl SeedCatalog for Catalog {
    type Error = &'static str;
    fn recovery(&self, object: SeedObjectSpec) -> Result<SeedRecovery<'_>, Self::Error> {
        let fixture = self
            .fixtures
            .iter()
            .find(|fixture| fixture.encoding.object_id() == object.object_id)
            .ok_or("object absent from authenticated catalog")?;
        Ok(SeedRecovery {
            encoding: &fixture.encoding,
            target: fixture.target(),
            dek: &DEK,
            donors: &DONORS,
            limits: PullLimits::default(),
        })
    }
}

struct FixtureTransport<'a> {
    fixtures: &'a [Fixture],
    failed: Option<DonorId>,
    corrupt: Option<DonorId>,
    suspend_on: Option<usize>,
    calls: usize,
}

impl<'a> FixtureTransport<'a> {
    fn healthy(fixtures: &'a [Fixture]) -> Self {
        Self {
            fixtures,
            failed: None,
            corrupt: None,
            suspend_on: None,
            calls: 0,
        }
    }
}

impl PullTransport for FixtureTransport<'_> {
    type Error = &'static str;
    fn exchange(
        &mut self,
        requests: &[PullRequest],
    ) -> impl Future<Output = Result<Vec<PullReply>, Self::Error>> {
        self.calls += 1;
        let suspend = self.suspend_on == Some(self.calls);
        let replies: Vec<_> = requests
            .iter()
            .rev()
            .map(|request| {
                let fixture = self
                    .fixtures
                    .iter()
                    .find(|fixture| fixture.encoding.object_id() == request.object_id)
                    .unwrap();
                let outcome = if self.failed == Some(request.donor) {
                    ReplyOutcome::Unavailable
                } else {
                    let mut record = fixture.records[request.esi as usize].clone();
                    if self.corrupt == Some(request.donor) {
                        *record.last_mut().unwrap() ^= 1;
                    }
                    ReplyOutcome::Record(record)
                };
                PullReply {
                    request: *request,
                    outcome,
                }
            })
            .collect();
        async move {
            if suspend {
                pending::<()>().await;
            }
            Ok(replies)
        }
    }
}

#[test]
fn real_bonded_recovery_survives_disconnected_and_corrupt_donors() {
    let fixtures = [Fixture::new(91)];
    let fixture = &fixtures[0];
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &DONORS,
        PullLimits::default(),
    )
    .unwrap();
    let mut transport = FixtureTransport::healthy(&fixtures);
    transport.failed = Some(DonorId(1));
    transport.corrupt = Some(DonorId(2));
    let object = immediate(bonded::recover(
        &mut pull,
        &mut transport,
        &mut Vec::new(),
        8,
    ))
    .unwrap();
    assert_eq!(object.plaintext(), fixture.plaintext);
    assert_eq!(pull.pending_count(), 0);
    assert!(transport.calls > 1);
}

#[test]
fn cancelling_a_window_releases_credit_but_preserves_verified_symbols() {
    let fixtures = [Fixture::new(92)];
    let fixture = &fixtures[0];
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &DONORS,
        PullLimits::default(),
    )
    .unwrap();
    let mut transport = FixtureTransport::healthy(&fixtures);
    transport.suspend_on = Some(2);
    let mut verification = Vec::new();
    {
        let mut future = pin!(bonded::recover(
            &mut pull,
            &mut transport,
            &mut verification,
            8
        ));
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(pull.symbol_count(), 8);
    assert_eq!(pull.pending_count(), 0);
    transport.suspend_on = None;
    let object = immediate(bonded::recover(
        &mut pull,
        &mut transport,
        &mut verification,
        8,
    ))
    .unwrap();
    assert_eq!(object.plaintext(), fixture.plaintext);
}

#[test]
fn catalog_inventory_mismatch_is_rejected_before_network_io() {
    let catalog = Catalog {
        fixtures: vec![Fixture::new(93)],
    };
    let fixture = &catalog.fixtures[0];
    let mut transport = FixtureTransport::healthy(&catalog.fixtures);
    let mut verification = Vec::new();
    {
        let mut source = BondedSeedSource::new(
            support::namespace(),
            &catalog,
            &mut transport,
            &mut verification,
            8,
        );
        let wrong = SeedObjectSpec {
            object_id: fixture.encoding.object_id(),
            object_kind: support::KIND + 1,
            compressed_len: fixture.plaintext.len() as u64,
        };
        assert!(matches!(
            immediate(source.recover(wrong)),
            Err(SeedAcquireError::InvalidObject)
        ));
    }
    assert_eq!(transport.calls, 0);
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
    // Synthetic authenticated inventory for a transition test, NOT a production
    // canonical manifest verifier or signed snapshot certificate.
    let plan = SeedPlan::from_authenticated_inventory(
        anchor,
        catalog.fixtures.iter().map(|fixture| SeedObjectSpec {
            object_id: fixture.encoding.object_id(),
            object_kind: support::KIND,
            compressed_len: fixture.plaintext.len() as u64,
        }),
        SeedLimits::default(),
    )
    .unwrap();
    let mut raft = Raft::new(MemberId(2), config.clone(), Limits::default()).unwrap();
    let mut store = MemoryRoot::default();
    let output = immediate(sequence(
        &mut raft,
        &mut store,
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
    (
        raft,
        output.snapshot_transfers.into_iter().next().unwrap(),
        plan,
    )
}

#[derive(Default)]
struct SeedRoot {
    objects: Vec<[u8; 32]>,
    consensus: Option<PersistentState<u64>>,
    suspend: bool,
    fail: bool,
}

impl SeedPublisher<u64> for SeedRoot {
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
        for object in snapshot.seed().plan().objects() {
            assert!(self.objects.contains(&object.object_id.0));
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
        // Memory-only test double. Real publishers must mint this evidence by
        // RootStore's post-sync reread, not by copying the planned coordinates.
        let evidence = RootPublicationEvidence {
            written_index: 1,
            slot_generation: anchor.publication_generation,
            root_manifest_oid: anchor.publication_root.0,
        };
        self.consensus = Some(snapshot.consensus().clone());
        let (suspend, fail) = (self.suspend, self.fail);
        async move {
            if suspend {
                pending::<()>().await;
            }
            if fail {
                Err("root sync outcome unknown")
            } else {
                Ok(evidence)
            }
        }
    }
}

fn seed_catalog() -> Catalog {
    Catalog {
        fixtures: (101..105).map(Fixture::new).collect(),
    }
}

#[test]
fn seed_driver_installs_only_after_every_object_is_durable() {
    let catalog = seed_catalog();
    let (mut raft, transfer, plan) = seed_offer(&catalog);
    let catchup = SnapshotCatchup::begin(&mut raft, support::namespace(), transfer, plan).unwrap();
    let mut transport = FixtureTransport::healthy(&catalog.fixtures);
    transport.failed = Some(DonorId(1));
    let mut verification = Vec::new();
    let mut source = BondedSeedSource::new(
        support::namespace(),
        &catalog,
        &mut transport,
        &mut verification,
        8,
    );
    let mut root = SeedRoot::default();
    let output = immediate(catchup.install(&mut source, &mut root)).unwrap();
    assert_eq!(root.objects.len(), 4);
    assert_eq!(output.installed_snapshot.unwrap().index(), 12);
    assert_eq!(raft.durable_state().unwrap().commit_index(), 12);
    assert!(
        output
            .messages
            .iter()
            .any(|message| matches!(message.message, Message::SnapshotInstalled { .. }))
    );
}

#[test]
fn cancelled_atomic_seed_publication_fences_the_replica() {
    let catalog = seed_catalog();
    let (mut raft, transfer, plan) = seed_offer(&catalog);
    let catchup = SnapshotCatchup::begin(&mut raft, support::namespace(), transfer, plan).unwrap();
    let mut transport = FixtureTransport::healthy(&catalog.fixtures);
    let mut verification = Vec::new();
    let mut source = BondedSeedSource::new(
        support::namespace(),
        &catalog,
        &mut transport,
        &mut verification,
        8,
    );
    let mut root = SeedRoot {
        suspend: true,
        ..SeedRoot::default()
    };
    {
        let mut future = pin!(catchup.install(&mut source, &mut root));
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(root.objects.len(), 4);
    assert!(root.consensus.is_some());
    assert_eq!(raft.role(), Err(RaftError::RecoveryRequired));
}

#[test]
fn ordinary_sequencer_cannot_install_only_the_raft_half_of_a_snapshot() {
    let catalog = seed_catalog();
    let (mut raft, transfer, _) = seed_offer(&catalog);
    let mut root = MemoryRoot::default();
    assert!(matches!(
        immediate(sequence(
            &mut raft,
            &mut root,
            Event::SnapshotReady(transfer.id())
        )),
        Err(SequenceError::SnapshotNeedsAtomicPublication)
    ));
    assert!(root.state.is_none());
    assert_eq!(raft.durable_state().unwrap().commit_index(), 0);
}

#[test]
fn failed_atomic_seed_publication_emits_no_success_and_requires_recovery() {
    let catalog = seed_catalog();
    let (mut raft, transfer, plan) = seed_offer(&catalog);
    let catchup = SnapshotCatchup::begin(&mut raft, support::namespace(), transfer, plan).unwrap();
    let mut transport = FixtureTransport::healthy(&catalog.fixtures);
    let mut verification = Vec::new();
    let mut source = BondedSeedSource::new(
        support::namespace(),
        &catalog,
        &mut transport,
        &mut verification,
        8,
    );
    let mut root = SeedRoot {
        fail: true,
        ..SeedRoot::default()
    };
    assert!(immediate(catchup.install(&mut source, &mut root)).is_err());
    assert_eq!(raft.role(), Err(RaftError::RecoveryRequired));
}
