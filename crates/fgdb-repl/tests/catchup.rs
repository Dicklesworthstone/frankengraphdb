#![cfg(not(target_arch = "wasm32"))]

//! Actual crypto/FEC, seed and Raft kernels; publication evidence models a
//! completed storage barrier, not a substitute for RootStore's crash matrix.

#[path = "../../fgdb-chronicle/tests/support/bonded.rs"]
mod support;

use fgdb_chronicle::seed::{SeedAnchor, SeedError, SeedLimits, SeedObjectSpec, SeedPlan};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_chronicle::transfer::{BondedPull, DonorId, PullLimits, VerifiedObject};
use fgdb_order::{Configuration, Domain, Entry, Envelope, Error as RaftError, Event,
    Limits, MemberId, Message, Raft, SnapshotCut, SnapshotTransfer};
use fgdb_repl::{CatchupError, CatchupPhase, SnapshotCatchup};
use fgdb_types::DatabaseSecurityNamespaceId;
use support::{Fixture, KIND};

fn fixtures() -> Vec<Fixture> {
    (41..45).map(Fixture::new).collect()
}

fn anchor(objects: &[Fixture]) -> SeedAnchor {
    SeedAnchor {
        namespace: support::namespace(), consensus_domain: [5; 32], configuration: [6; 32],
        snapshot_manifest: objects[0].encoding.object_id(),
        state_root: objects[1].encoding.object_id(),
        retention_floor: objects[2].encoding.object_id(),
        publication_root: objects[3].encoding.object_id(),
        publication_generation: 17, raft_index: 93, raft_term: 11,
        logical_command_seq: 61, commit_seq: 49,
    }
}

fn plan(anchor: SeedAnchor, objects: &[Fixture]) -> SeedPlan {
    SeedPlan::from_authenticated_inventory(anchor, objects.iter().map(|object| SeedObjectSpec {
        object_id: object.encoding.object_id(), object_kind: KIND,
        compressed_len: object.plaintext.len() as u64,
    }), SeedLimits::default()).unwrap()
}

fn configuration() -> Configuration {
    Configuration::stable(Domain([5; 32]), [6; 32],
        [MemberId(1), MemberId(2), MemberId(3)], []).unwrap()
}

fn evidence(anchor: &SeedAnchor) -> RootPublicationEvidence {
    RootPublicationEvidence { written_index: 1, slot_generation: anchor.publication_generation,
        root_manifest_oid: anchor.publication_root.0 }
}

fn offered_node(id: u128, anchor: &SeedAnchor) -> (Raft<u64>, SnapshotTransfer) {
    let configuration = configuration();
    let cut = SnapshotCut::from_authenticated_parts(&configuration, anchor.snapshot_manifest.0,
        anchor.state_root.0, anchor.retention_floor.0, anchor.raft_index, anchor.raft_term).unwrap();
    let mut node = Raft::new(MemberId(id), configuration, Limits::default()).unwrap();
    let publication = node.step(Event::Receive(Envelope {
        domain: Domain(anchor.consensus_domain), configuration: anchor.configuration,
        from: MemberId(1), to: MemberId(id),
        message: Message::InstallSnapshot { term: 12, request: 71, snapshot: cut },
    })).unwrap();
    assert!(publication.requires_write()); // term first, not an installation
    assert!(publication.state().snapshot().is_none());
    let token = publication.id();
    let mut output = node.persisted(token).unwrap();
    assert!(output.messages.is_empty());
    assert!(output.installed_snapshot.is_none());
    assert_eq!(output.snapshot_transfers.len(), 1);
    (node, output.snapshot_transfers.remove(0))
}

fn recover_with_failed_donor(fixture: &Fixture) -> VerifiedObject {
    let limits = PullLimits { max_in_flight: 12, ..PullLimits::default() };
    let mut pull = BondedPull::new(&fixture.encoding, fixture.target(), &support::DEK,
        &[DonorId(1), DonorId(2), DonorId(3)], limits).unwrap();
    let first = pull.schedule(12).unwrap();
    pull.donor_failed(DonorId(2)).unwrap();
    for request in first.into_iter().rev().filter(|request| request.donor != DonorId(2)) {
        pull.accept(request.donor, &fixture.records[request.esi as usize], &mut Vec::new()).unwrap();
    }
    for _ in 0..8 {
        if let Some(object) = pull.try_recover(&mut Vec::new()).unwrap() {
            assert_eq!(object.plaintext(), fixture.plaintext);
            return object;
        }
        let requests = pull.schedule(8).unwrap();
        assert!(!requests.is_empty());
        for request in requests.into_iter().rev() {
            assert_ne!(request.donor, DonorId(2));
            pull.accept(request.donor, &fixture.records[request.esi as usize], &mut Vec::new()).unwrap();
        }
    }
    panic!("surviving donor streams did not recover the fixture");
}

fn publish_objects(session: &mut SnapshotCatchup<'_, u64>, objects: &[Fixture]) {
    for object in objects.iter().rev() {
        let token = session.stage(object.verified()).unwrap().id();
        session.object_published(token).unwrap();
    }
}

#[test]
fn donor_loss_to_atomic_snapshot_to_suffix_replay_is_one_composed_path() {
    let objects = fixtures();
    let anchor = anchor(&objects);
    let (mut raft, transfer) = offered_node(2, &anchor);
    let expected_cut = transfer.snapshot().clone();
    let mut catchup = SnapshotCatchup::begin(&mut raft, support::namespace(),
        transfer, plan(anchor.clone(), &objects)).unwrap();
    assert!(matches!(catchup.begin_publication(),
        Err(CatchupError::Seed(SeedError::MissingObjects { remaining: 4 }))));
    for (offset, object) in objects.iter().enumerate() {
        let token = catchup.stage(recover_with_failed_donor(object)).unwrap().id();
        assert_eq!(catchup.published_count(), offset);
        assert_eq!(catchup.pending_object().unwrap().id(), token);
        assert!(matches!(catchup.begin_publication(),
            Err(CatchupError::Seed(SeedError::AwaitingObjectPublication))));
        catchup.object_published(token).unwrap();
    }
    assert_eq!(catchup.missing_objects().count(), 0);
    let publication = catchup.begin_publication().unwrap();
    let token = publication.id();
    let disk = publication.consensus().clone();
    assert_eq!(publication.seed().plan().anchor(), &anchor);
    assert_eq!(publication.seed().incoming_encodings().count(), 4);
    assert_eq!(disk.snapshot(), Some(&expected_cut));
    assert_eq!(disk.commit_index(), anchor.raft_index);
    assert_eq!(catchup.phase(), CatchupPhase::Publishing);
    let again = catchup.begin_publication().unwrap();
    assert_eq!(again.id(), token);
    assert_eq!(again.consensus(), &disk);
    let output = catchup.published(token, &evidence(&anchor)).unwrap();
    assert_eq!(output.installed_snapshot, Some(expected_cut));
    assert!(output.committed.is_empty());
    assert!(matches!(output.messages[0].message,
        Message::SnapshotInstalled { term: 12, request: 71 }));
    assert_eq!(catchup.phase(), CatchupPhase::Complete);
    drop(catchup);
    assert_eq!(raft.durable_state().unwrap(), &disk);
    let mut resumed = Raft::recover(MemberId(2), disk, Limits::default()).unwrap();
    let publication = resumed.step(Event::Receive(Envelope {
        domain: Domain([5; 32]), configuration: [6; 32], from: MemberId(1), to: MemberId(2),
        message: Message::Append { term: 12, request: 72, prev_index: 93,
            prev_term: 11, entries: vec![Entry { term: 12, command: Some(777) }], leader_commit: 94 },
    })).unwrap();
    let token = publication.id();
    let output = resumed.persisted(token).unwrap();
    assert_eq!(output.committed[0].index, 94);
    assert_eq!(output.committed[0].entry.command, Some(777));
}

#[test]
fn every_independent_seed_binding_is_checked_before_bulk_work() {
    let objects = fixtures();
    let original = anchor(&objects);
    for mutation in 0..8 {
        let mut changed = original.clone();
        match mutation {
            0 => changed.namespace = DatabaseSecurityNamespaceId(core::array::from_fn(|_| 0xab)),
            1 => changed.consensus_domain[0] ^= 1,
            2 => changed.configuration[0] ^= 1,
            3 => changed.snapshot_manifest = changed.state_root,
            4 => changed.state_root = changed.snapshot_manifest,
            5 => changed.retention_floor = changed.state_root,
            6 => changed.raft_index += 1,
            _ => changed.raft_term += 1,
        }
        let (mut raft, transfer) = offered_node(2, &original);
        let before = raft.durable_state().unwrap().clone();
        assert_eq!(SnapshotCatchup::begin(&mut raft, support::namespace(), transfer,
            plan(changed, &objects)).err(), Some(if mutation == 0 { CatchupError::WrongNamespace }
                else { CatchupError::SnapshotBindingMismatch }));
        assert_eq!(raft.durable_state().unwrap(), &before);
    }
}

#[test]
fn wrong_root_generation_and_slot_evidence_never_release_either_gate() {
    let objects = fixtures();
    let anchor = anchor(&objects);
    let (mut raft, transfer) = offered_node(2, &anchor);
    let mut catchup = SnapshotCatchup::begin(&mut raft, support::namespace(), transfer,
        plan(anchor.clone(), &objects)).unwrap();
    publish_objects(&mut catchup, &objects);
    let token = catchup.begin_publication().unwrap().id();
    for mutation in 0..3 {
        let mut wrong = evidence(&anchor);
        match mutation {
            0 => wrong.slot_generation += 1,
            1 => wrong.root_manifest_oid[0] ^= 1,
            _ => wrong.written_index = 2,
        }
        assert!(matches!(catchup.published(token.clone(), &wrong),
            Err(CatchupError::Seed(SeedError::RootEvidenceMismatch))));
        assert_eq!(catchup.phase(), CatchupPhase::Publishing);
        assert_eq!(catchup.begin_publication().unwrap().id(), token);
    }
    assert!(catchup.published(token, &evidence(&anchor)).unwrap().installed_snapshot.is_some());
}

#[test]
fn a_foreign_joint_publication_token_cannot_release_an_ack() {
    let objects = fixtures();
    let anchor = anchor(&objects);
    let (mut left, first) = offered_node(2, &anchor);
    let (mut right, second) = offered_node(3, &anchor);
    let mut a = SnapshotCatchup::begin(&mut left, support::namespace(), first,
        plan(anchor.clone(), &objects)).unwrap();
    let mut b = SnapshotCatchup::begin(&mut right, support::namespace(), second,
        plan(anchor.clone(), &objects)).unwrap();
    publish_objects(&mut a, &objects);
    publish_objects(&mut b, &objects);
    let a_id = a.begin_publication().unwrap().id();
    let b_id = b.begin_publication().unwrap().id();
    assert_eq!(b.published(a_id.clone(), &evidence(&anchor)).err(), Some(CatchupError::StalePublication));
    assert!(a.published(a_id, &evidence(&anchor)).unwrap().installed_snapshot.is_some());
    assert!(b.published(b_id, &evidence(&anchor)).unwrap().installed_snapshot.is_some());
}

#[test]
fn cross_node_and_cancelled_transfer_capabilities_fail_before_root_publication() {
    let objects = fixtures();
    let anchor = anchor(&objects);
    for cancelled in [false, true] {
        let (mut left, transfer) = offered_node(2, &anchor);
        let (mut right, _) = offered_node(3, &anchor);
        let target = if cancelled {
            let token = left.step(Event::SnapshotFailed(transfer.id())).unwrap().id();
            left.persisted(token).unwrap();
            &mut left
        } else { &mut right };
        let before = target.durable_state().unwrap().clone();
        {
            let mut catchup = SnapshotCatchup::begin(target, support::namespace(), transfer,
                plan(anchor.clone(), &objects)).unwrap();
            publish_objects(&mut catchup, &objects);
            assert!(matches!(catchup.begin_publication(),
                Err(CatchupError::Raft(RaftError::StaleSnapshotTransfer))));
            assert_eq!(catchup.phase(), CatchupPhase::Failed);
        }
        // No root-publication view was ever returned; restart from the old root.
        let recovered = Raft::recover(target.id(), before, Limits::default()).unwrap();
        assert!(recovered.durable_state().unwrap().snapshot().is_none());
    }
}

#[test]
fn cancellation_before_root_preparation_keeps_the_old_durable_raft_state() {
    let objects = fixtures();
    let anchor = anchor(&objects);
    let (mut raft, transfer) = offered_node(2, &anchor);
    let before = raft.durable_state().unwrap().clone();
    {
        let mut catchup = SnapshotCatchup::begin(&mut raft, support::namespace(), transfer,
            plan(anchor, &objects)).unwrap();
        let token = catchup.stage(objects[0].verified()).unwrap().id();
        catchup.object_published(token).unwrap();
        assert!(matches!(catchup.begin_publication(),
            Err(CatchupError::Seed(SeedError::MissingObjects { remaining: 3 }))));
    }
    assert_eq!(raft.durable_state().unwrap(), &before);
}

#[test]
fn cancelled_or_failed_atomic_publication_requires_recovery() {
    let objects = fixtures();
    let anchor = anchor(&objects);
    for failed in [false, true] {
        let (mut raft, transfer) = offered_node(2, &anchor);
        let candidate;
        {
            let mut catchup = SnapshotCatchup::begin(&mut raft, support::namespace(), transfer,
                plan(anchor.clone(), &objects)).unwrap();
            publish_objects(&mut catchup, &objects);
            let publication = catchup.begin_publication().unwrap();
            candidate = publication.consensus().clone();
            let token = publication.id();
            if failed {
                catchup.publication_failed();
                assert_eq!(catchup.published(token, &evidence(&anchor)).err(), Some(CatchupError::WrongPhase));
            }
        }
        assert_eq!(raft.step(Event::ElectionTimeout).err(), Some(RaftError::RecoveryRequired));
        // If the atomic root did make it to disk, recovery has BOTH the new cut
        // and the new consensus state, even if no acknowledgement was sent.
        let recovered = Raft::recover(MemberId(2), candidate, Limits::default()).unwrap();
        assert_eq!(recovered.durable_state().unwrap().snapshot().unwrap().index(), 93);
        assert_eq!(recovered.durable_state().unwrap().commit_index(), 93);
    }
}

#[test]
fn completed_cut_cannot_start_a_second_snapshot_installation() {
    let objects = fixtures();
    let anchor = anchor(&objects);
    let (mut raft, transfer) = offered_node(2, &anchor);
    let again = transfer.clone();
    {
        let mut catchup = SnapshotCatchup::begin(&mut raft, support::namespace(), transfer,
            plan(anchor.clone(), &objects)).unwrap();
        publish_objects(&mut catchup, &objects);
        let token = catchup.begin_publication().unwrap().id();
        catchup.published(token.clone(), &evidence(&anchor)).unwrap();
        assert_eq!(catchup.published(token, &evidence(&anchor)).err(), Some(CatchupError::WrongPhase));
        catchup.publication_failed(); // cannot undo a completed durable operation
        assert_eq!(catchup.phase(), CatchupPhase::Complete);
    }
    assert_eq!(SnapshotCatchup::begin(&mut raft, support::namespace(), again,
        plan(anchor, &objects)).err(), Some(CatchupError::StaleSnapshot));
}
