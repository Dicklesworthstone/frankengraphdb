#![cfg(not(target_arch = "wasm32"))]

#[path = "live_download/audit.rs"]
mod audit;

// Real Raft, Chronicle object crypto/FEC and seed gates. Storage below records
// publication images; it is NOT a disk/fsync or production authority verifier.
#[path = "../../fgdb-chronicle/tests/support/bonded.rs"]
mod support;

use std::future::{Future, pending};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use fgdb_chronicle::seed::{
    ObjectPublication, SeedAnchor, SeedError, SeedLimits, SeedObjectSpec, SeedPlan,
    SeedPublicationId,
};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_chronicle::transfer::VerifiedObject;
use fgdb_order::{
    Configuration, Domain, Entry, Envelope, Error, Event, Limits, MemberId, Message, Output,
    PersistentState, Raft, Role, SnapshotCut, SnapshotTransfer,
};
use fgdb_repl::download::SnapshotDownload;
use fgdb_repl::driver::{SeedDriveError, SeedObjectSource, SeedPublisher};
use fgdb_repl::{CatchupError, CatchupPhase, SnapshotPublication};
use support::{Fixture, KIND};

fn poll<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
fn immediate<F: Future>(future: F) -> F::Output {
    match poll(pin!(future)) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("unexpected suspension"),
    }
}
fn config() -> Configuration {
    Configuration::stable(
        Domain([5; 32]),
        [6; 32],
        [MemberId(1), MemberId(2), MemberId(3)],
        [],
    )
    .unwrap()
}
fn anchor(objects: &[Fixture]) -> SeedAnchor {
    SeedAnchor {
        namespace: support::namespace(),
        consensus_domain: [5; 32],
        configuration: [6; 32],
        snapshot_manifest: objects[0].encoding.object_id(),
        state_root: objects[1].encoding.object_id(),
        retention_floor: objects[2].encoding.object_id(),
        publication_root: objects[3].encoding.object_id(),
        publication_generation: 17,
        raft_index: 93,
        raft_term: 11,
        logical_command_seq: 61,
        commit_seq: 49,
    }
}
fn plan(anchor: SeedAnchor, objects: &[Fixture]) -> SeedPlan {
    SeedPlan::from_authenticated_inventory(
        anchor,
        objects.iter().map(|o| SeedObjectSpec {
            object_id: o.encoding.object_id(),
            object_kind: KIND,
            compressed_len: o.plaintext.len() as u64,
        }),
        SeedLimits::default(),
    )
    .unwrap()
}
fn envelope(to: MemberId, message: Message<u64>) -> Envelope<u64> {
    Envelope {
        domain: Domain([5; 32]),
        configuration: [6; 32],
        from: MemberId(1),
        to,
        message,
    }
}
fn step(raft: &mut Raft<u64>, event: Event<u64>) -> Output<u64> {
    let id = raft.step(event).unwrap().id();
    raft.persisted(id).unwrap()
}
fn offer(raft: &mut Raft<u64>, anchor: &SeedAnchor, request: u64) -> SnapshotTransfer {
    let snapshot = SnapshotCut::from_authenticated_parts(
        &config(),
        anchor.snapshot_manifest.0,
        anchor.state_root.0,
        anchor.retention_floor.0,
        anchor.raft_index,
        anchor.raft_term,
    )
    .unwrap();
    let to = raft.id();
    step(
        raft,
        Event::Receive(envelope(
            to,
            Message::InstallSnapshot {
                term: 12,
                request,
                snapshot,
            },
        )),
    )
    .snapshot_transfers
    .remove(0)
}
fn setup() -> (Vec<Fixture>, Raft<u64>, SnapshotDownload) {
    let objects: Vec<_> = (51..55).map(Fixture::new).collect();
    let a = anchor(&objects);
    let mut raft = Raft::new(MemberId(2), config(), Limits::default()).unwrap();
    let transfer = offer(&mut raft, &a, 71);
    let download =
        SnapshotDownload::begin(&raft, support::namespace(), transfer, plan(a, &objects)).unwrap();
    (objects, raft, download)
}
fn ready(download: &mut SnapshotDownload, objects: &[Fixture]) {
    for object in objects {
        let id = download.stage(object.verified()).unwrap().id();
        download.object_published(id).unwrap();
    }
}
struct Source<'a> {
    objects: &'a [Fixture],
    calls: usize,
    suspend: bool,
    fail: bool,
}
impl SeedObjectSource for Source<'_> {
    type Error = &'static str;
    async fn recover(&mut self, spec: SeedObjectSpec) -> Result<VerifiedObject, Self::Error> {
        self.calls += 1;
        if self.suspend {
            pending::<()>().await;
        }
        if self.fail {
            return Err("source unavailable");
        }
        Ok(self
            .objects
            .iter()
            .find(|o| o.encoding.object_id() == spec.object_id)
            .unwrap()
            .verified())
    }
}
#[derive(Default)]
struct Storage {
    objects: Vec<SeedPublicationId>,
    state: Option<PersistentState<u64>>,
    fail_object: bool,
    suspend_object: bool,
    // 1/2: fail before/after write; 3: suspend after write; 4: panic; 5: bad evidence.
    root_fault: u8,
}
impl SeedPublisher<u64> for Storage {
    type Error = &'static str;
    async fn publish_object(&mut self, object: ObjectPublication<'_>) -> Result<(), Self::Error> {
        self.objects.push(object.id());
        if self.suspend_object {
            pending::<()>().await;
        }
        if self.fail_object {
            return Err("object outcome unknown");
        }
        Ok(())
    }
    fn publish_snapshot(
        &mut self,
        snapshot: SnapshotPublication<'_, u64>,
    ) -> impl Future<Output = Result<RootPublicationEvidence, Self::Error>> {
        assert!(snapshot.seed().incoming_encodings().count() >= 4);
        let a = snapshot.seed().plan().anchor();
        let mut evidence = RootPublicationEvidence {
            written_index: 1,
            slot_generation: a.publication_generation,
            root_manifest_oid: a.publication_root.0,
        };
        assert_ne!(self.root_fault, 4, "synchronous publication panic");
        if self.root_fault != 1 {
            self.state = Some(snapshot.consensus().clone());
        }
        if self.root_fault == 5 {
            evidence.slot_generation += 1;
        }
        let fault = self.root_fault;
        async move {
            if fault == 3 {
                pending::<()>().await;
            }
            if fault == 1 || fault == 2 {
                Err("root outcome unknown")
            } else {
                Ok(evidence)
            }
        }
    }
}
fn heartbeat(raft: &mut Raft<u64>) -> Output<u64> {
    let to = raft.id();
    step(
        raft,
        Event::Receive(envelope(
            to,
            Message::Append {
                term: 12,
                request: 200,
                prev_index: 0,
                prev_term: 0,
                entries: Vec::new(),
                leader_commit: 0,
            },
        )),
    )
}

#[test]
fn suspended_source_does_not_borrow_or_stop_the_voter() {
    let (objects, mut raft, mut download) = setup();
    let before = raft.durable_state().unwrap().clone();
    let mut source = Source {
        objects: &objects,
        calls: 0,
        suspend: true,
        fail: false,
    };
    {
        let mut work = pin!(download.recover_next(&mut source));
        assert!(poll(work.as_mut()).is_pending());
        // This must compile with work STILL alive: network I/O has no Raft borrow.
        let output = heartbeat(&mut raft);
        assert!(output.reset_election_timer);
        assert!(output.cancelled_snapshot_transfers.is_empty());
        assert!(matches!(
            output.messages[0].message,
            Message::Appended { success: true, .. }
        ));
        assert_eq!(raft.durable_state().unwrap(), &before);
    }
    assert_eq!(download.published_count(), 0);
    source.suspend = false;
    assert!(
        immediate(download.recover_next(&mut source))
            .unwrap()
            .is_some()
    );
    let mut storage = Storage::default();
    immediate(download.publish_next::<u64, _>(&mut storage)).unwrap();
    while immediate(download.recover_next(&mut source))
        .unwrap()
        .is_some()
    {
        heartbeat(&mut raft);
        immediate(download.publish_next::<u64, _>(&mut storage)).unwrap();
    }
    let output = immediate(download.install(&mut raft, &mut storage)).unwrap();
    assert!(matches!(
        output.messages[0].message,
        Message::SnapshotInstalled { request: 71, .. }
    ));
    assert_eq!(raft.durable_state().unwrap().commit_index(), 93);
    assert_eq!(source.calls, 5); // one cancelled call plus exactly four objects
}

#[test]
fn source_failure_and_staged_retry_preserve_progress_without_refetch() {
    let (objects, mut raft, mut download) = setup();
    let mut source = Source {
        objects: &objects,
        calls: 0,
        suspend: false,
        fail: true,
    };
    assert!(matches!(
        immediate(download.recover_next(&mut source)),
        Err(SeedDriveError::Source(_))
    ));
    source.fail = false;
    let oid = immediate(download.recover_next(&mut source))
        .unwrap()
        .unwrap();
    let id = download.pending_object().unwrap().id();
    let mut storage = Storage {
        fail_object: true,
        ..Storage::default()
    };
    assert!(immediate(download.publish_next::<u64, _>(&mut storage)).is_err());
    heartbeat(&mut raft);
    assert_eq!(download.published_count(), 0);
    assert_eq!(download.pending_object().unwrap().id(), id);
    assert_eq!(
        immediate(download.recover_next(&mut source)).unwrap(),
        Some(oid)
    );
    assert_eq!(source.calls, 2);
    storage.fail_object = false;
    assert_eq!(
        immediate(download.publish_next::<u64, _>(&mut storage)).unwrap(),
        oid
    );
    assert_eq!(storage.objects, vec![id.clone(), id]);
    assert_eq!(download.published_count(), 1);
}

#[test]
fn cancelled_object_publication_keeps_exact_pending_identity_and_bytes() {
    let (objects, mut raft, mut download) = setup();
    let id = download.stage(objects[0].verified()).unwrap().id();
    let mut storage = Storage {
        suspend_object: true,
        ..Storage::default()
    };
    {
        let mut work = pin!(download.publish_next::<u64, _>(&mut storage));
        assert!(poll(work.as_mut()).is_pending());
        heartbeat(&mut raft);
    }
    assert_eq!(download.pending_object().unwrap().id(), id);
    assert_eq!(
        download.pending_object().unwrap().object().plaintext(),
        objects[0].plaintext
    );
    storage.suspend_object = false;
    immediate(download.publish_next::<u64, _>(&mut storage)).unwrap();
    assert_eq!(storage.objects, vec![id.clone(), id]);
}

#[test]
fn newer_offer_invalidates_old_download_without_poisoning_new_transfer() {
    let (objects, mut raft, mut download) = setup();
    ready(&mut download, &objects);
    let newer = offer(&mut raft, &anchor(&objects), 72);
    assert_ne!(newer.id(), download.transfer_id());
    assert!(matches!(
        download.prepare(&mut raft),
        Err(CatchupError::Raft(Error::StaleSnapshotTransfer))
    ));
    assert_eq!(raft.role(), Ok(Role::Follower));
    let mut next = SnapshotDownload::begin(
        &raft,
        support::namespace(),
        newer,
        plan(anchor(&objects), &objects),
    )
    .unwrap();
    ready(&mut next, &objects);
    assert!(immediate(next.install(&mut raft, &mut Storage::default())).is_ok());
}

#[test]
fn higher_term_and_local_election_cancel_download_without_rollback() {
    for campaign in [false, true] {
        let (objects, mut raft, mut download) = setup();
        ready(&mut download, &objects);
        let event = if campaign {
            Event::ElectionTimeout
        } else {
            Event::Receive(envelope(
                MemberId(2),
                Message::RequestVote {
                    term: 13,
                    last_index: 0,
                    last_term: 0,
                },
            ))
        };
        let output = step(&mut raft, event);
        assert!(
            output
                .cancelled_snapshot_transfers
                .contains(&download.transfer_id())
        );
        let before = raft.durable_state().unwrap().clone();
        assert!(matches!(
            download.prepare(&mut raft),
            Err(CatchupError::Raft(Error::StaleSnapshotTransfer))
        ));
        assert_eq!(raft.durable_state().unwrap(), &before);
        assert_eq!(before.term(), 13);
    }
}

#[test]
fn a_recovered_or_other_member_cannot_consume_a_download_capability() {
    for other_member in [false, true] {
        let (objects, raft, mut download) = setup();
        ready(&mut download, &objects);
        let mut target = Raft::recover(
            if other_member {
                MemberId(3)
            } else {
                MemberId(2)
            },
            raft.durable_state().unwrap().clone(),
            Limits::default(),
        )
        .unwrap();
        let _new_offer = offer(&mut target, &anchor(&objects), 71);
        assert!(matches!(
            download.prepare(&mut target),
            Err(CatchupError::Raft(Error::StaleSnapshotTransfer))
        ));
        assert_eq!(target.role(), Ok(Role::Follower));
        assert_eq!(target.durable_state().unwrap().commit_index(), 0);
    }
}

#[test]
fn log_catchup_during_download_prevents_rollback_to_the_offered_cut() {
    let (objects, mut raft, mut download) = setup();
    ready(&mut download, &objects);
    let output = step(
        &mut raft,
        Event::Receive(envelope(
            MemberId(2),
            Message::Append {
                term: 12,
                request: 73,
                prev_index: 0,
                prev_term: 0,
                entries: vec![
                    Entry {
                        term: 11,
                        command: Some(1)
                    };
                    94
                ],
                leader_commit: 94,
            },
        )),
    );
    assert!(
        output
            .cancelled_snapshot_transfers
            .contains(&download.transfer_id())
    );
    let before = raft.durable_state().unwrap().clone();
    assert!(matches!(
        download.prepare(&mut raft),
        Err(CatchupError::StaleSnapshot)
    ));
    assert_eq!(raft.durable_state().unwrap(), &before);
}

#[test]
fn atomic_install_uses_the_current_matching_suffix_not_a_download_time_copy() {
    let (objects, mut raft, mut download) = setup();
    ready(&mut download, &objects);
    let mut entries = vec![
        Entry {
            term: 11,
            command: Some(1)
        };
        93
    ];
    entries.extend([
        Entry {
            term: 12,
            command: Some(800),
        },
        Entry {
            term: 12,
            command: Some(900),
        },
    ]);
    step(
        &mut raft,
        Event::Receive(envelope(
            MemberId(2),
            Message::Append {
                term: 12,
                request: 73,
                prev_index: 0,
                prev_term: 0,
                entries,
                leader_commit: 7,
            },
        )),
    );
    let mut storage = Storage::default();
    let output = immediate(download.install(&mut raft, &mut storage)).unwrap();
    assert!(output.committed.is_empty());
    let state = storage.state.unwrap();
    assert_eq!(state.commit_index(), 93);
    assert_eq!(
        state.entries(),
        &[
            Entry {
                term: 12,
                command: Some(800)
            },
            Entry {
                term: 12,
                command: Some(900)
            }
        ]
    );
    assert_eq!(raft.durable_state().unwrap(), &state);
}

#[test]
fn incomplete_and_unacknowledged_downloads_cannot_start_installation() {
    for staged in [false, true] {
        let (objects, mut raft, mut download) = setup();
        if staged {
            let _ = download.stage(objects[0].verified()).unwrap();
        }
        let before = raft.durable_state().unwrap().clone();
        assert!(matches!(
            download.prepare(&mut raft),
            Err(CatchupError::Seed(
                SeedError::MissingObjects { .. } | SeedError::AwaitingObjectPublication
            ))
        ));
        assert_eq!(raft.durable_state().unwrap(), &before);
    }
}

#[test]
fn dropping_prepared_install_fences_but_dropping_download_does_not() {
    let (objects, mut raft, mut download) = setup();
    ready(&mut download, &objects);
    let guard = download.prepare(&mut raft).unwrap();
    assert_eq!(guard.phase(), CatchupPhase::Publishing);
    drop(guard);
    assert_eq!(raft.role(), Err(Error::RecoveryRequired));
    let (_objects, mut raft, download) = setup();
    drop(download);
    assert_eq!(raft.role(), Ok(Role::Follower));
    heartbeat(&mut raft);
}

#[test]
fn all_uncertain_atomic_publications_fence_without_releasing_a_reply() {
    for fault in [1, 2, 3, 4, 5] {
        let (objects, mut raft, mut download) = setup();
        ready(&mut download, &objects);
        let mut storage = Storage {
            root_fault: fault,
            ..Storage::default()
        };
        if fault == 3 {
            let mut future = Box::pin(download.install(&mut raft, &mut storage));
            assert!(poll(future.as_mut()).is_pending());
            drop(future);
        } else if fault == 4 {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    immediate(download.install(&mut raft, &mut storage))
                }))
                .is_err()
            );
        } else {
            assert!(immediate(download.install(&mut raft, &mut storage)).is_err());
        }
        assert_eq!(raft.role(), Err(Error::RecoveryRequired));
        if let Some(state) = storage.state {
            let recovered = Raft::recover(MemberId(2), state, Limits::default()).unwrap();
            assert_eq!(recovered.durable_state().unwrap().commit_index(), 93);
        }
    }
}

#[test]
fn wrong_namespace_or_plan_binding_is_rejected_before_staging() {
    let (objects, raft, download) = setup();
    let mut wrong = anchor(&objects);
    wrong.raft_index += 1;
    // A second output handle is obtained from the model root, not constructed.
    let mut other = Raft::recover(
        MemberId(2),
        raft.durable_state().unwrap().clone(),
        Limits::default(),
    )
    .unwrap();
    let transfer = offer(&mut other, &anchor(&objects), 71);
    assert!(matches!(
        SnapshotDownload::begin(
            &other,
            support::namespace(),
            transfer.clone(),
            plan(wrong, &objects)
        ),
        Err(CatchupError::SnapshotBindingMismatch)
    ));
    assert!(matches!(
        SnapshotDownload::begin(
            &other,
            fgdb_types::DatabaseSecurityNamespaceId([99; 32]),
            transfer,
            plan(anchor(&objects), &objects)
        ),
        Err(CatchupError::WrongNamespace)
    ));
    drop(download);
}

#[test]
fn a_different_valid_inventory_object_cannot_satisfy_the_requested_object() {
    struct Substitution(Option<VerifiedObject>);
    impl SeedObjectSource for Substitution {
        type Error = ();
        async fn recover(&mut self, _: SeedObjectSpec) -> Result<VerifiedObject, ()> {
            Ok(self.0.take().unwrap())
        }
    }
    let (objects, raft, mut download) = setup();
    let requested = download.missing_objects().next().unwrap().object_id;
    let different = objects
        .iter()
        .find(|o| o.encoding.object_id() != requested)
        .unwrap();
    let mut source = Substitution(Some(different.verified()));
    assert!(matches!(
        immediate(download.recover_next(&mut source)),
        Err(SeedDriveError::Catchup(CatchupError::Seed(
            SeedError::UnexpectedObject
        )))
    ));
    assert!(download.pending_object().is_err());
    assert_eq!(download.published_count(), 0);
    assert_eq!(raft.role(), Ok(Role::Follower));
}

#[test]
fn command_clone_panic_during_preparation_fences_the_affected_voter() {
    #[derive(Debug, PartialEq, Eq)]
    struct Bomb;
    impl Clone for Bomb {
        fn clone(&self) -> Self {
            panic!("command clone")
        }
    }
    let objects: Vec<_> = (51..55).map(Fixture::new).collect();
    let a = anchor(&objects);
    let mut entries: Vec<_> = (0..93)
        .map(|_| Entry::<Bomb> {
            term: 11,
            command: None,
        })
        .collect();
    entries.push(Entry {
        term: 12,
        command: Some(Bomb),
    });
    let mut raft = Raft::recover(
        MemberId(2),
        PersistentState::from_authenticated_parts(config(), 12, None, 1, entries),
        Limits::default(),
    )
    .unwrap();
    let cut = SnapshotCut::from_authenticated_parts(
        &config(),
        a.snapshot_manifest.0,
        a.state_root.0,
        a.retention_floor.0,
        a.raft_index,
        a.raft_term,
    )
    .unwrap();
    let id = raft
        .step(Event::Receive(Envelope {
            domain: config().domain(),
            configuration: config().identity(),
            from: MemberId(1),
            to: MemberId(2),
            message: Message::InstallSnapshot {
                term: 12,
                request: 71,
                snapshot: cut,
            },
        }))
        .unwrap()
        .id();
    let transfer = raft.persisted(id).unwrap().snapshot_transfers.remove(0);
    let mut download =
        SnapshotDownload::begin(&raft, support::namespace(), transfer, plan(a, &objects)).unwrap();
    ready(&mut download, &objects);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = download.prepare(&mut raft);
        }))
        .is_err()
    );
    assert_eq!(raft.role(), Err(Error::RecoveryRequired));
}

#[path = "live_download/member.rs"]
mod member;

#[path = "live_download/refresh.rs"]
mod refresh;
