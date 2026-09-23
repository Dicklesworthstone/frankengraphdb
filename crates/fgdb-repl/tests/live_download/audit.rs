//! Actual application/seed/Raft composition with an explicit memory audit queue.
//! This model is NOT a canonical snapshot codec, signature verifier or disk VFS.

use super::*;
use std::cell::RefCell;
use std::rc::Rc;

use fgdb_chronicle::seed::{ReplicaSeed, SeedAuditCut};
use fgdb_repl::application::{Application, ApplicationBatch, ApplicationError, ApplicationProgress, AppliedPosition, ApplicationStateError};
use fgdb_repl::application::member::{AppliedReplica, MemberSeedError, PinnedApplication, ReadApplication, ReadState};
use fgdb_repl::application::snapshot::RestoredSnapshot;
use fgdb_repl::driver::RaftPublisher;
use fgdb_repl::replica::Replica;
use fgdb_types::ObjectId;

const RELEASE: u64 = 900_000;
type Member = AppliedReplica<u64, Backend>;

fn fixtures() -> Vec<Fixture> { (61..67).map(Fixture::new).collect() }
fn audit(objects: &[Fixture]) -> SeedAuditCut {
    SeedAuditCut {
        visible_index: 1, visible_term: 11,
        visible_state_root: objects[4].encoding.object_id(),
        audit_state_root: objects[5].encoding.object_id(),
    }
}
fn audited_plan(objects: &[Fixture]) -> SeedPlan {
    plan(anchor(objects), objects).with_authenticated_audit_cut(audit(objects)).unwrap()
}
fn snapshot(objects: &[Fixture]) -> SnapshotCut {
    let a = anchor(objects);
    SnapshotCut::from_authenticated_parts(&config(), a.snapshot_manifest.0, a.state_root.0,
        a.retention_floor.0, a.raft_index, a.raft_term).unwrap()
}
fn image(objects: &[Fixture]) -> RestoredSnapshot {
    RestoredSnapshot::from_authenticated_parts(snapshot(objects), audit(objects)).unwrap()
}
fn genesis() -> ApplicationProgress {
    ApplicationProgress {
        domain: config().domain(), configuration: config().identity(),
        applied: AppliedPosition { index: 0, term: 0 }, visible_index: 0,
        state_root: ObjectId([90; 32]), publication_root: ObjectId([91; 32]),
        publication_generation: 1,
    }
}

struct Disk {
    progress: ApplicationProgress,
    raft: Option<PersistentState<u64>>,
    restored: Option<RestoredSnapshot>,
    visible: Vec<u64>,
    pending: Vec<(u64, u64)>,
    applied: Vec<u64>,
    loads: usize,
    installations: usize,
    pins: usize,
    fault: u8,
    next_image: RestoredSnapshot,
}
struct Backend {
    disk: Rc<RefCell<Disk>>,
    restored: Option<RestoredSnapshot>,
    visible: Vec<u64>,
    pending: Vec<(u64, u64)>,
}
impl Backend {
    fn new(disk: &Rc<RefCell<Disk>>) -> Self {
        Self { disk: Rc::clone(disk), restored: None, visible: Vec::new(), pending: Vec::new() }
    }
}
impl Application<u64> for Backend {
    type Error = &'static str;
    fn load(&mut self) -> impl Future<Output = Result<ApplicationProgress, Self::Error>> {
        let mut disk = self.disk.borrow_mut();
        disk.loads += 1;
        // Load the actual modeled queue and visible history, not just positions.
        self.restored = disk.restored.clone();
        self.visible = disk.visible.clone();
        self.pending = disk.pending.clone();
        let mut progress = disk.progress;
        let fault = if disk.installations == 0 { 0 } else { disk.fault };
        match fault {
            1 => self.restored = None,
            2 => progress.visible_index = progress.applied.index,
            3 => progress.visible_index += 1,
            4 => {
                let old = self.restored.as_ref().unwrap();
                let mut audit = old.audit_cut();
                audit.audit_state_root = ObjectId([201; 32]);
                self.restored = Some(RestoredSnapshot::from_authenticated_parts(old.snapshot().clone(), audit).unwrap());
            }
            5 => {
                let old = self.restored.as_ref().unwrap();
                let mut audit = old.audit_cut();
                audit.visible_state_root = ObjectId([202; 32]);
                self.restored = Some(RestoredSnapshot::from_authenticated_parts(old.snapshot().clone(), audit).unwrap());
            }
            6 => {
                let old = self.restored.as_ref().unwrap();
                let a = old.snapshot();
                let other = SnapshotCut::from_authenticated_parts(&config(), [203; 32], a.state_root(),
                    a.retention_floor(), a.index(), a.term()).unwrap();
                self.restored = Some(RestoredSnapshot::from_authenticated_parts(other, old.audit_cut()).unwrap());
            }
            7 => { self.restored = None; progress.visible_index = progress.applied.index; }
            10 => panic!("load panicked after restoring part of the model"),
            11 => {
                let old = self.restored.as_ref().unwrap();
                let mut audit = old.audit_cut();
                audit.visible_term -= 1;
                self.restored = Some(RestoredSnapshot::from_authenticated_parts(old.snapshot().clone(), audit).unwrap());
            }
            _ => {}
        }
        drop(disk);
        async move {
            if fault == 8 { pending::<()>().await; }
            if fault == 9 { Err("load outcome unavailable") } else { Ok(progress) }
        }
    }
    fn restored_snapshot(&self) -> Option<RestoredSnapshot> {
        assert_ne!(self.disk.borrow().fault, 12, "restoration projection panicked");
        self.restored.clone()
    }
    async fn apply(&mut self, batch: ApplicationBatch<'_, u64>) -> Result<ApplicationProgress, Self::Error> {
        let mut disk = self.disk.borrow_mut();
        assert_eq!(&disk.progress, batch.basis());
        assert_eq!(disk.raft.as_ref().unwrap(), batch.consensus());
        let mut visible_index = disk.progress.visible_index;
        for (position, entry) in batch.indexed_entries() {
            disk.applied.push(position.index);
            match entry.command {
                Some(control) if control >= RELEASE => {
                    let through = control - RELEASE;
                    assert!(through < position.index);
                    let visible = &mut self.visible;
                    self.pending.retain(|(index, value)| {
                        if *index <= through { visible.push(*value); false } else { true }
                    });
                    visible_index = visible_index.max(through);
                    if self.pending.is_empty() { visible_index = position.index; }
                }
                Some(value) => self.pending.push((position.index, value)),
                None if self.pending.is_empty() => visible_index = position.index,
                None => {}
            }
        }
        let generation = disk.progress.publication_generation + 1;
        let root = ObjectId([generation as u8; 32]);
        disk.progress = ApplicationProgress {
            applied: batch.last(), visible_index, state_root: root,
            publication_root: root, publication_generation: generation, ..disk.progress
        };
        disk.visible = self.visible.clone();
        disk.pending = self.pending.clone();
        Ok(disk.progress)
    }
}
impl RaftPublisher<u64> for Backend {
    type Error = &'static str;
    async fn publish(&mut self, state: &PersistentState<u64>) -> Result<(), Self::Error> {
        self.disk.borrow_mut().raft = Some(state.clone());
        Ok(())
    }
}
impl SeedPublisher<u64> for Backend {
    type Error = &'static str;
    async fn publish_object(&mut self, _: ObjectPublication<'_>) -> Result<(), Self::Error> { Ok(()) }
    async fn publish_snapshot(&mut self, publication: SnapshotPublication<'_, u64>) -> Result<RootPublicationEvidence, Self::Error> {
        let mut disk = self.disk.borrow_mut();
        let plan = publication.seed().plan();
        let a = plan.anchor();
        assert_eq!(plan.audit_cut(), Some(disk.next_image.audit_cut()));
        assert_eq!(publication.consensus().snapshot(), Some(disk.next_image.snapshot()));
        assert_eq!(publication.seed().incoming_encodings().count(), 6);
        disk.installations += 1;
        disk.raft = Some(publication.consensus().clone());
        disk.restored = Some(disk.next_image.clone());
        disk.progress = ApplicationProgress {
            domain: Domain(a.consensus_domain), configuration: a.configuration,
            applied: AppliedPosition { index: a.raft_index, term: a.raft_term },
            visible_index: disk.next_image.audit_cut().visible_index,
            state_root: a.state_root, publication_root: a.publication_root,
            publication_generation: a.publication_generation,
        };
        disk.visible = vec![111];
        disk.pending = vec![(50, 222), (93, 333)];
        Ok(RootPublicationEvidence { written_index: 1, slot_generation: a.publication_generation,
            root_manifest_oid: a.publication_root.0 })
    }
}
impl ReadApplication<u64> for Backend {
    type View = Vec<u64>;
    async fn pin_visible(&mut self, basis: &ApplicationProgress, at: AppliedPosition) -> Result<PinnedApplication<Self::View>, Self::Error> {
        let mut disk = self.disk.borrow_mut();
        assert_eq!(basis, &disk.progress);
        assert_eq!(at.index, basis.visible_index);
        assert!(self.pending.iter().all(|(index, _)| *index > at.index));
        disk.pins += 1;
        Ok(PinnedApplication { basis: *basis, at, state_root: ObjectId([at.index as u8; 32]), view: self.visible.clone() })
    }
}
fn setup_member(objects: &[Fixture]) -> (Member, Rc<RefCell<Disk>>, SnapshotTransfer) {
    let disk = Rc::new(RefCell::new(Disk {
        progress: genesis(), raft: None, restored: None, visible: Vec::new(), pending: Vec::new(),
        applied: Vec::new(), loads: 0, installations: 0, pins: 0, fault: 0, next_image: image(objects),
    }));
    let replica = Replica::new(MemberId(2), config(), Limits::default(), 8).unwrap();
    let mut member = immediate(Member::recover(replica, Backend::new(&disk), 1, 8)).unwrap();
    let transfer = immediate(member.step(Event::Receive(envelope(MemberId(2), Message::InstallSnapshot {
        term: 12, request: 71, snapshot: snapshot(objects),
    })))).unwrap().consensus.snapshot_transfers.remove(0);
    (member, disk, transfer)
}
fn download(member: &Member, transfer: SnapshotTransfer, objects: &[Fixture]) -> SnapshotDownload {
    member.begin_snapshot_download(support::namespace(), transfer, audited_plan(objects)).unwrap()
}
fn acquire(member: &mut Member, download: &mut SnapshotDownload, objects: &[Fixture]) {
    let mut source = Source { objects, calls: 0, suspend: false, fail: false };
    while immediate(download.recover_next(&mut source)).unwrap().is_some() {
        immediate(member.publish_download_object(download)).unwrap();
    }
    assert_eq!(source.calls, 6);
}
// A modeled authenticated peer acknowledges only request IDs just emitted to it.
// This is quorum-driver evidence, not a claim that a real peer performed fsync.
fn ack(member: &mut Member, messages: Vec<Envelope<u64>>) {
    let mut messages = messages;
    for _ in 0..16 {
        let Some((term, request)) = messages.iter().find_map(|message| {
            if message.to != MemberId(1) { return None; }
            if let Message::Append { term, request, .. } = &message.message { Some((*term, *request)) } else { None }
        }) else { return };
        messages = immediate(member.step(Event::Receive(envelope(MemberId(2), Message::Appended {
            term, request, success: true, conflict_next: 0,
        })))).unwrap().consensus.messages;
    }
    panic!("modeled peer must quiesce");
}
fn elect(member: &mut Member) {
    immediate(member.step(Event::ElectionTimeout)).unwrap();
    let term = member.durable_state().unwrap().term();
    let output = immediate(member.step(Event::Receive(envelope(MemberId(2), Message::Vote { term, granted: true })))).unwrap();
    assert_eq!(output.consensus.role, Role::Leader);
    ack(member, output.consensus.messages);
}
fn commit(member: &mut Member, command: u64) {
    let output = immediate(member.step(Event::Propose(command))).unwrap();
    ack(member, output.consensus.messages);
    immediate(member.apply_next()).unwrap().unwrap();
}
fn noop_prefix(member: &mut Member, index: u64) {
    immediate(member.step(Event::Receive(envelope(MemberId(2), Message::Append {
        term: 12, request: 200 + index, prev_index: index - 1, prev_term: if index == 1 { 0 } else { 11 },
        entries: vec![Entry { term: 11, command: None }], leader_commit: index,
    })))).unwrap();
    immediate(member.apply_next()).unwrap().unwrap();
}

#[test]
fn hidden_snapshot_installs_then_ordered_controls_release_the_restored_queue() {
    let objects = fixtures();
    let (mut member, disk, transfer) = setup_member(&objects);
    let mut download = download(&member, transfer, &objects);
    acquire(&mut member, &mut download, &objects);
    let output = immediate(member.install_download(download)).unwrap();
    assert!(matches!(output.consensus.messages[0].message, Message::SnapshotInstalled { request: 71, .. }));
    assert_eq!(member.progress().unwrap().applied.index, 93);
    assert_eq!(member.progress().unwrap().visible_index, 1);
    assert!(immediate(member.apply_next()).unwrap().is_none());
    assert_eq!(disk.borrow().pending, [(50, 222), (93, 333)]);

    elect(&mut member); // current-term no-op is committed at 94, not yet applied
    let (read, probe) = immediate(member.read_index()).unwrap();
    ack(&mut member, probe.consensus.messages);
    assert!(matches!(immediate(member.try_read(&read)).unwrap(), ReadState::PendingApplication { required: 94, applied: 93 }));
    immediate(member.apply_next()).unwrap();
    assert!(matches!(immediate(member.try_read(&read)).unwrap(), ReadState::PendingAudit { required: 94, visible: 1 }));
    commit(&mut member, RELEASE + 50);
    assert_eq!(member.progress().unwrap().visible_index, 50); // earlier than retained Raft base
    assert!(matches!(immediate(member.try_read(&read)).unwrap(), ReadState::PendingAudit { visible: 50, .. }));
    assert_eq!(disk.borrow().pins, 0);
    commit(&mut member, RELEASE + 94);
    commit(&mut member, 444); // later hidden effect, not part of this visible view
    assert_eq!(member.progress().unwrap().applied.index, 97);
    assert_eq!(member.progress().unwrap().visible_index, 96);
    let ReadState::Ready(view) = immediate(member.try_read(&read)).unwrap() else { panic!("read did not become visible") };
    assert_eq!(view.at().index, 96);
    assert_eq!(view.view(), &[111, 222, 333]);
    assert_eq!(disk.borrow().pending, [(97, 444)]);
    assert_eq!(disk.borrow().applied, [94, 95, 96, 97]); // snapshot-covered effects not replayed
}

#[test]
fn bound_install_and_live_install_restore_the_same_hidden_cut() {
    let objects = fixtures();
    let (mut member, disk, transfer) = setup_member(&objects);
    let mut source = Source { objects: &objects, calls: 0, suspend: false, fail: false };
    let output = immediate(member.install_snapshot(support::namespace(), transfer, audited_plan(&objects), &mut source)).unwrap();
    assert!(output.consensus.installed_snapshot.is_some());
    assert_eq!(member.progress().unwrap().visible_index, 1);
    assert_eq!(disk.borrow().pending, [(50, 222), (93, 333)]);
    assert_eq!(source.calls, 6);
}

#[test]
fn mismatched_or_incomplete_restoration_never_releases_an_install_reply() {
    let objects = fixtures();
    for fault in 1..=12 {
        let (mut member, disk, transfer) = setup_member(&objects);
        let mut download = download(&member, transfer, &objects);
        acquire(&mut member, &mut download, &objects);
        disk.borrow_mut().fault = fault;
        if fault == 8 {
            let mut work = Box::pin(member.install_download(download));
            assert!(poll(work.as_mut()).is_pending());
            drop(work);
        } else if fault == 10 || fault == 12 {
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                immediate(member.install_download(download))
            })).is_err());
        } else {
            assert!(immediate(member.install_download(download)).is_err(), "fault {fault}");
        }
        assert_eq!(disk.borrow().installations, 1);
        assert_eq!(member.progress(), Err(ApplicationStateError::RecoveryRequired));
        assert!(immediate(member.step(Event::Heartbeat)).is_err());
        assert_eq!(disk.borrow().pins, 0);
    }
}

#[test]
fn recovered_snapshot_preserves_the_pending_queue_without_replaying_its_prefix() {
    let objects = fixtures();
    let (mut member, disk, transfer) = setup_member(&objects);
    let mut download = download(&member, transfer, &objects);
    acquire(&mut member, &mut download, &objects);
    immediate(member.install_download(download)).unwrap();
    drop(member);
    let state = disk.borrow().raft.clone().unwrap();
    let replica = Replica::recover(MemberId(2), state, Limits::default(), 8).unwrap();
    let mut recovered = immediate(Member::recover(replica, Backend::new(&disk), 1, 8)).unwrap();
    assert_eq!(recovered.progress().unwrap().visible_index, 1);
    elect(&mut recovered);
    immediate(recovered.apply_next()).unwrap();
    commit(&mut recovered, RELEASE + 94);
    assert_eq!(disk.borrow().visible, [111, 222, 333]);
    assert!(disk.borrow().pending.is_empty());
    assert_eq!(disk.borrow().applied, [94, 95]);
}

#[test]
fn visibility_cannot_regress_at_admission_or_after_a_live_download() {
    let objects = fixtures();
    for before_admission in [false, true] {
        let (mut member, disk, transfer) = setup_member(&objects);
        let mut downloaded = None;
        if !before_admission {
            let mut d = download(&member, transfer.clone(), &objects);
            acquire(&mut member, &mut d, &objects);
            downloaded = Some(d);
        }
        noop_prefix(&mut member, 1);
        noop_prefix(&mut member, 2);
        let previous = member.progress().unwrap();
        assert_eq!(previous.visible_index, 2);
        if before_admission {
            assert!(matches!(member.begin_snapshot_download(support::namespace(), transfer, audited_plan(&objects)),
                Err(MemberSeedError::Application(ApplicationError::State(ApplicationStateError::VisibilityRegression)))));
        } else {
            assert!(matches!(immediate(member.install_download(downloaded.unwrap())),
                Err(MemberSeedError::Application(ApplicationError::State(ApplicationStateError::VisibilityRegression)))));
        }
        assert_eq!(member.progress().unwrap(), previous);
        assert_eq!(disk.borrow().installations, 0);
    }
}

#[test]
fn audit_cut_requires_both_roots_and_exact_position_relationships() {
    let objects = fixtures();
    let valid = audit(&objects);
    for missing in [4, 5] {
        let inventory = objects.iter().enumerate().filter(|(index, _)| *index != missing).map(|(_, object)| SeedObjectSpec {
            object_id: object.encoding.object_id(), object_kind: KIND, compressed_len: object.plaintext.len() as u64,
        });
        let p = SeedPlan::from_authenticated_inventory(anchor(&objects), inventory, SeedLimits::default()).unwrap();
        assert!(matches!(p.with_authenticated_audit_cut(valid), Err(SeedError::MissingRoot)));
    }
    for field in 0..5 {
        let mut changed = valid;
        match field {
            0 => changed.visible_index = 94,
            1 => changed.visible_term = 12,
            2 => changed.visible_term = 0,
            3 => changed.visible_index = 0,
            _ => changed.visible_index = 93, // equal cut but a different root
        }
        assert!(plan(anchor(&objects), &objects).with_authenticated_audit_cut(changed).is_err());
        assert!(RestoredSnapshot::from_authenticated_parts(snapshot(&objects), changed).is_err());
    }
    let origin = SeedAuditCut { visible_index: 0, visible_term: 0, ..valid };
    assert!(plan(anchor(&objects), &objects).with_authenticated_audit_cut(origin).is_ok());
    let complete = SeedAuditCut { visible_index: 93, visible_term: 11,
        visible_state_root: anchor(&objects).state_root, ..valid };
    assert!(plan(anchor(&objects), &objects).with_authenticated_audit_cut(complete).is_ok());
}

#[test]
fn destination_refresh_cannot_drop_or_substitute_the_snapshotted_audit_state() {
    let objects = fixtures();
    let original = audited_plan(&objects);
    let mut seed = ReplicaSeed::new(original.clone());
    for object in &objects {
        let id = seed.stage(object.verified()).unwrap().id();
        seed.object_published(id).unwrap();
    }
    for field in 0..5 {
        let mut a = anchor(&objects);
        a.publication_generation += 1;
        let mut p = plan(a, &objects);
        if field != 0 {
            let mut changed = audit(&objects);
            match field {
                1 => changed.visible_index = 2,
                2 => changed.visible_term = 10,
                3 => changed.visible_state_root = objects[0].encoding.object_id(),
                _ => changed.audit_state_root = objects[1].encoding.object_id(),
            }
            p = p.with_authenticated_audit_cut(changed).unwrap();
        }
        assert!(matches!(seed.refresh_plan(p), Err(SeedError::InvalidAnchor)));
        assert_eq!(seed.plan().audit_cut(), original.audit_cut());
        assert_eq!(seed.published_count(), 6);
    }
    let mut a = anchor(&objects);
    a.publication_generation += 1;
    let next = plan(a, &objects).with_authenticated_audit_cut(audit(&objects)).unwrap();
    seed.refresh_plan(next.clone()).unwrap();
    seed.refresh_plan(next).unwrap();
    assert_eq!(seed.published_count(), 6);
    assert_eq!(seed.plan().audit_cut(), original.audit_cut());
}

#[test]
fn recovery_refuses_hidden_progress_without_the_exact_restored_snapshot() {
    let objects = fixtures();
    let (mut member, disk, transfer) = setup_member(&objects);
    let mut download = download(&member, transfer, &objects);
    acquire(&mut member, &mut download, &objects);
    immediate(member.install_download(download)).unwrap();
    let state = disk.borrow().raft.clone().unwrap();
    let valid = disk.borrow().restored.clone().unwrap();
    drop(member);
    for mutation in 0..3 {
        {
            let mut d = disk.borrow_mut();
            d.restored = Some(valid.clone());
            d.progress.visible_index = 1;
            match mutation {
                0 => d.restored = None,
                1 => {
                    let cut = valid.snapshot();
                    let wrong = SnapshotCut::from_authenticated_parts(&config(), [213; 32], cut.state_root(),
                        cut.retention_floor(), cut.index(), cut.term()).unwrap();
                    d.restored = Some(RestoredSnapshot::from_authenticated_parts(wrong, valid.audit_cut()).unwrap());
                }
                _ => d.progress.visible_index = 93,
            }
        }
        let replica = Replica::recover(MemberId(2), state.clone(), Limits::default(), 8).unwrap();
        assert!(immediate(Member::recover(replica, Backend::new(&disk), 1, 8)).is_err());
    }
}

#[test]
fn restart_after_partial_release_preserves_later_pending_candidates() {
    let objects = fixtures();
    let (mut member, disk, transfer) = setup_member(&objects);
    let mut download = download(&member, transfer, &objects);
    acquire(&mut member, &mut download, &objects);
    immediate(member.install_download(download)).unwrap();
    elect(&mut member);
    immediate(member.apply_next()).unwrap();
    commit(&mut member, RELEASE + 50);
    assert_eq!(member.progress().unwrap().visible_index, 50);
    assert_eq!(disk.borrow().pending, [(93, 333)]);
    let state = disk.borrow().raft.clone().unwrap();
    drop(member);
    let replica = Replica::recover(MemberId(2), state, Limits::default(), 8).unwrap();
    let mut member = immediate(Member::recover(replica, Backend::new(&disk), 1, 8)).unwrap();
    assert_eq!(member.progress().unwrap().visible_index, 50);
    elect(&mut member);
    immediate(member.apply_next()).unwrap();
    assert_eq!(member.progress().unwrap().visible_index, 50); // no-op cannot open hidden state
    commit(&mut member, RELEASE + 96);
    assert_eq!(disk.borrow().visible, [111, 222, 333]);
    assert!(disk.borrow().pending.is_empty());
    assert_eq!(disk.borrow().applied, [94, 95, 96, 97]);
}

#[test]
fn later_visible_compaction_does_not_reuse_the_old_hidden_cut_projection() {
    let objects = fixtures();
    let (mut member, disk, transfer) = setup_member(&objects);
    let mut download = download(&member, transfer, &objects);
    acquire(&mut member, &mut download, &objects);
    immediate(member.install_download(download)).unwrap();
    // Local hidden-prefix compaction still requires the separate full-floor
    // verifier integration; this increment deliberately does not relax it.
    assert!(immediate(member.step(Event::Compact(snapshot(&objects)))).is_err());
    elect(&mut member);
    immediate(member.apply_next()).unwrap();
    commit(&mut member, RELEASE + 94);
    let p = member.progress().unwrap();
    let cut = SnapshotCut::from_authenticated_parts(&config(), [214; 32], p.state_root.0,
        [215; 32], p.applied.index, p.applied.term).unwrap();
    immediate(member.step(Event::Compact(cut))).unwrap();
    commit(&mut member, 444);
    assert_eq!(member.progress().unwrap().visible_index, p.applied.index);
    assert_eq!(disk.borrow().pending, [(96, 444)]);
}
