// Extend the real-kernel download scenarios through the public owning member.
// All root/application backends remain explicit memory publication models.
use super::*;
use fgdb_repl::application::member::AppliedReplica;
use fgdb_repl::application::{
    Application, ApplicationBatch, ApplicationProgress, ApplicationStateError, AppliedPosition,
};
use fgdb_repl::driver::RaftPublisher;
use fgdb_repl::replica::Replica;
use std::cell::RefCell;
use std::rc::Rc;

struct Shared {
    state: Option<PersistentState<u64>>,
    progress: ApplicationProgress,
    generation: u64,
    objects: usize,
    installations: usize,
    loads: usize,
    reload_fault: u8,
    refuse_root: bool,
}
#[derive(Clone)]
struct Backend(Rc<RefCell<Shared>>);
type Member = AppliedReplica<u64, Backend>;

impl Application<u64> for Backend {
    type Error = &'static str;
    fn load(&mut self) -> impl Future<Output = Result<ApplicationProgress, Self::Error>> {
        let (mut value, fault) = {
            let mut s = self.0.borrow_mut();
            s.loads += 1;
            (s.progress, if s.loads == 1 { 0 } else { s.reload_fault })
        };
        assert_ne!(fault, 3, "synchronous load panic");
        match fault {
            4 => value.publication_root = fgdb_types::ObjectId([200; 32]),
            5 => value.publication_generation += 1,
            6 => value.visible_index -= 1,
            7 => value.domain = Domain([200; 32]),
            8 => value.configuration = [200; 32],
            9 => value.state_root = fgdb_types::ObjectId([200; 32]),
            10 => value.applied.term += 1,
            _ => {}
        }
        async move {
            if fault == 2 {
                pending::<()>().await;
            }
            if fault == 1 {
                Err("load unavailable")
            } else {
                Ok(value)
            }
        }
    }
    async fn apply(
        &mut self,
        batch: ApplicationBatch<'_, u64>,
    ) -> Result<ApplicationProgress, Self::Error> {
        let mut s = self.0.borrow_mut();
        assert_eq!(&s.progress, batch.basis());
        assert_eq!(s.state.as_ref().unwrap(), batch.consensus());
        s.generation += 1;
        let root = fgdb_types::ObjectId([s.generation as u8; 32]);
        s.progress = ApplicationProgress {
            applied: batch.last(),
            visible_index: batch.last().index,
            state_root: root,
            publication_root: root,
            publication_generation: s.generation,
            ..s.progress
        };
        Ok(s.progress)
    }
}
impl RaftPublisher<u64> for Backend {
    type Error = &'static str;
    async fn publish(&mut self, state: &PersistentState<u64>) -> Result<(), Self::Error> {
        let mut s = self.0.borrow_mut();
        s.state = Some(state.clone());
        s.generation += 1;
        Ok(())
    }
}
impl SeedPublisher<u64> for Backend {
    type Error = &'static str;
    async fn publish_object(&mut self, _: ObjectPublication<'_>) -> Result<(), Self::Error> {
        let mut s = self.0.borrow_mut();
        s.objects += 1;
        s.generation += 1;
        Ok(())
    }
    async fn publish_snapshot(
        &mut self,
        publication: SnapshotPublication<'_, u64>,
    ) -> Result<RootPublicationEvidence, Self::Error> {
        let mut s = self.0.borrow_mut();
        let a = publication.seed().plan().anchor();
        // The real canonical publisher must validate exact root/closure/fence,
        // not only these modeled generation and cut coordinates.
        if s.refuse_root || a.publication_generation <= s.generation {
            return Err("stale or refused destination root");
        }
        s.installations += 1;
        s.generation = a.publication_generation;
        s.state = Some(publication.consensus().clone());
        s.progress = ApplicationProgress {
            domain: Domain(a.consensus_domain),
            configuration: a.configuration,
            applied: AppliedPosition {
                index: a.raft_index,
                term: a.raft_term,
            },
            visible_index: a.raft_index,
            state_root: a.state_root,
            publication_root: a.publication_root,
            publication_generation: a.publication_generation,
        };
        Ok(RootPublicationEvidence {
            written_index: 1,
            slot_generation: a.publication_generation,
            root_manifest_oid: a.publication_root.0,
        })
    }
}
fn member(objects: &[Fixture]) -> (Member, Rc<RefCell<Shared>>, SnapshotTransfer) {
    let shared = Rc::new(RefCell::new(Shared {
        state: None,
        generation: 1,
        objects: 0,
        installations: 0,
        loads: 0,
        reload_fault: 0,
        refuse_root: false,
        progress: ApplicationProgress {
            domain: config().domain(),
            configuration: config().identity(),
            applied: AppliedPosition { index: 0, term: 0 },
            visible_index: 0,
            state_root: fgdb_types::ObjectId([90; 32]),
            publication_root: fgdb_types::ObjectId([91; 32]),
            publication_generation: 1,
        },
    }));
    let replica = Replica::new(MemberId(2), config(), Limits::default(), 8).unwrap();
    let mut member = immediate(Member::recover(
        replica,
        Backend(Rc::clone(&shared)),
        128,
        8,
    ))
    .unwrap();
    let a = anchor(objects);
    let cut = SnapshotCut::from_authenticated_parts(
        &config(),
        a.snapshot_manifest.0,
        a.state_root.0,
        a.retention_floor.0,
        a.raft_index,
        a.raft_term,
    )
    .unwrap();
    let transfer = immediate(member.step(Event::Receive(envelope(
        MemberId(2),
        Message::InstallSnapshot {
            term: 12,
            request: 71,
            snapshot: cut,
        },
    ))))
    .unwrap()
    .consensus
    .snapshot_transfers
    .remove(0);
    (member, shared, transfer)
}
fn start(member: &Member, transfer: SnapshotTransfer, objects: &[Fixture]) -> SnapshotDownload {
    member
        .begin_snapshot_download(
            support::namespace(),
            transfer,
            plan(anchor(objects), objects),
        )
        .unwrap()
}
fn finish_objects(member: &mut Member, download: &mut SnapshotDownload, objects: &[Fixture]) {
    let mut source = Source {
        objects,
        calls: 0,
        suspend: false,
        fail: false,
    };
    while immediate(download.recover_next(&mut source))
        .unwrap()
        .is_some()
    {
        immediate(member.publish_download_object(download)).unwrap();
    }
}
fn append(member: &mut Member, index: u64) {
    immediate(member.step(Event::Receive(envelope(
        MemberId(2),
        Message::Append {
            term: 12,
            request: 200 + index,
            prev_index: index - 1,
            prev_term: if index == 1 { 0 } else { 11 },
            entries: vec![Entry {
                term: 11,
                command: Some(index),
            }],
            leader_commit: index,
        },
    ))))
    .unwrap();
}

#[test]
fn owned_member_applies_commits_while_snapshot_source_is_suspended() {
    let objects: Vec<_> = (51..55).map(Fixture::new).collect();
    let (mut member, shared, transfer) = member(&objects);
    let mut download = start(&member, transfer, &objects);
    let mut source = Source {
        objects: &objects,
        calls: 0,
        suspend: true,
        fail: false,
    };
    {
        let mut work = pin!(download.recover_next(&mut source));
        assert!(poll(work.as_mut()).is_pending());
        append(&mut member, 1);
        let progress = immediate(member.apply_next()).unwrap().unwrap();
        assert_eq!(progress.applied.index, 1);
        assert_eq!(shared.borrow().objects, 0);
    }
    finish_objects(&mut member, &mut download, &objects);
    let output = immediate(member.install_download(download)).unwrap();
    assert!(matches!(
        output.consensus.messages[0].message,
        Message::SnapshotInstalled { request: 71, .. }
    ));
    assert_eq!(member.progress().unwrap().applied.index, 93);
    assert_eq!(member.progress().unwrap().visible_index, 93);
    assert_eq!(shared.borrow().loads, 2);
    assert_eq!(shared.borrow().objects, 4);
    assert_eq!(shared.borrow().installations, 1);
    assert!(immediate(member.apply_next()).unwrap().is_none());
}

#[test]
fn higher_term_during_a_suspended_owned_download_keeps_the_member_usable() {
    let objects: Vec<_> = (51..55).map(Fixture::new).collect();
    let (mut member, shared, transfer) = member(&objects);
    let mut download = start(&member, transfer, &objects);
    let id = download.transfer_id();
    let mut source = Source {
        objects: &objects,
        calls: 0,
        suspend: true,
        fail: false,
    };
    {
        let mut work = pin!(download.recover_next(&mut source));
        assert!(poll(work.as_mut()).is_pending());
        let output = immediate(member.step(Event::Receive(envelope(
            MemberId(2),
            Message::RequestVote {
                term: 13,
                last_index: 0,
                last_term: 0,
            },
        ))))
        .unwrap();
        assert!(output.consensus.cancelled_snapshot_transfers.contains(&id));
    }
    // Model a host late to observe cancellation. Even complete stale bytes must
    // not activate or poison the current member. Immutable writes alone are not
    // installed state or membership authority.
    ready(&mut download, &objects);
    assert!(immediate(member.install_download(download)).is_err());
    assert_eq!(member.durable_state().unwrap().term(), 13);
    assert_eq!(member.progress().unwrap().applied.index, 0);
    assert_eq!(shared.borrow().installations, 0);
    assert_eq!(shared.borrow().loads, 1);
}

#[test]
fn owned_download_reload_failures_cannot_reactivate_the_old_application() {
    for fault in 1..=10 {
        let objects: Vec<_> = (51..55).map(Fixture::new).collect();
        let (mut member, shared, transfer) = member(&objects);
        let mut download = start(&member, transfer, &objects);
        finish_objects(&mut member, &mut download, &objects);
        shared.borrow_mut().reload_fault = fault;
        if fault == 2 {
            let mut work = Box::pin(member.install_download(download));
            assert!(poll(work.as_mut()).is_pending());
            drop(work);
        } else if fault == 3 {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    immediate(member.install_download(download))
                }))
                .is_err()
            );
        } else {
            assert!(
                immediate(member.install_download(download)).is_err(),
                "fault {fault}"
            );
        }
        assert_eq!(shared.borrow().installations, 1);
        assert_eq!(shared.borrow().progress.applied.index, 93);
        assert_eq!(
            member.progress(),
            Err(ApplicationStateError::RecoveryRequired)
        );
        assert!(immediate(member.step(Event::Heartbeat)).is_err());
        assert!(immediate(member.apply_next()).is_err());
    }
}

#[test]
fn newer_applied_publications_refuse_the_old_seed_generation_before_install() {
    let objects: Vec<_> = (51..55).map(Fixture::new).collect();
    let (mut member, shared, transfer) = member(&objects);
    let mut download = start(&member, transfer, &objects);
    finish_objects(&mut member, &mut download, &objects);
    for index in 1..=8 {
        append(&mut member, index);
        immediate(member.apply_next()).unwrap();
    }
    let before = member.progress().unwrap();
    assert!(before.publication_generation >= download.anchor().publication_generation);
    assert!(immediate(member.install_download(download)).is_err());
    assert_eq!(member.progress().unwrap(), before);
    assert_eq!(shared.borrow().installations, 0);
    assert_eq!(shared.borrow().loads, 1);
}

#[test]
fn canonical_publisher_refusal_fences_consensus_before_application_reload() {
    let objects: Vec<_> = (51..55).map(Fixture::new).collect();
    let (mut member, shared, transfer) = member(&objects);
    let mut download = start(&member, transfer, &objects);
    finish_objects(&mut member, &mut download, &objects);
    shared.borrow_mut().refuse_root = true;
    assert!(immediate(member.install_download(download)).is_err());
    assert_eq!(
        member.progress(),
        Err(ApplicationStateError::Raft(Error::RecoveryRequired))
    );
    assert_eq!(shared.borrow().loads, 1);
    assert_eq!(shared.borrow().installations, 0);
}

#[test]
fn bound_and_independent_installs_share_exact_application_activation_checks() {
    let objects: Vec<_> = (51..55).map(Fixture::new).collect();
    let mut results = Vec::new();
    for independent in [false, true] {
        let (mut member, shared, transfer) = member(&objects);
        let output = if independent {
            let mut download = start(&member, transfer, &objects);
            finish_objects(&mut member, &mut download, &objects);
            immediate(member.install_download(download)).unwrap()
        } else {
            let mut source = Source {
                objects: &objects,
                calls: 0,
                suspend: false,
                fail: false,
            };
            immediate(member.install_snapshot(
                support::namespace(),
                transfer,
                plan(anchor(&objects), &objects),
                &mut source,
            ))
            .unwrap()
        };
        assert!(output.leadership_lost.is_empty());
        assert_eq!(shared.borrow().loads, 2);
        results.push((
            member.progress().unwrap(),
            member.durable_state().unwrap().clone(),
            output.consensus.messages,
        ));
    }
    assert_eq!(results[0], results[1]);
}

#[test]
fn refresh_after_live_apply_reuses_objects_and_installs_the_current_plan() {
    let objects: Vec<_> = (51..55).map(Fixture::new).collect();
    let extra = Fixture::new(90);
    let (mut member, shared, transfer) = member(&objects);
    let mut download = start(&member, transfer, &objects);
    finish_objects(&mut member, &mut download, &objects);
    for index in 1..=8 {
        append(&mut member, index);
        immediate(member.apply_next()).unwrap();
    }
    let mut a = download.anchor().clone();
    assert!(member.progress().unwrap().publication_generation >= a.publication_generation);
    a.publication_generation = 30;
    a.publication_root = extra.encoding.object_id();
    let plan = SeedPlan::from_authenticated_inventory(
        a,
        objects[..3]
            .iter()
            .chain(std::iter::once(&extra))
            .map(|o| SeedObjectSpec {
                object_id: o.encoding.object_id(),
                object_kind: KIND,
                compressed_len: o.plaintext.len() as u64,
            }),
        SeedLimits::default(),
    )
    .unwrap();
    download.refresh_plan(plan).unwrap();
    assert_eq!(download.published_count(), 3);
    let mut source = Source {
        objects: std::slice::from_ref(&extra),
        calls: 0,
        suspend: false,
        fail: false,
    };
    assert!(
        immediate(download.recover_next(&mut source))
            .unwrap()
            .is_some()
    );
    immediate(member.publish_download_object(&mut download)).unwrap();
    assert!(
        immediate(download.recover_next(&mut source))
            .unwrap()
            .is_none()
    );
    immediate(member.install_download(download)).unwrap();
    assert_eq!(source.calls, 1); // No replay of the large shared snapshot closure.
    assert_eq!(shared.borrow().objects, 5); // four original plus one refreshed root
    assert_eq!(member.progress().unwrap().publication_generation, 30);
    assert_eq!(
        member.progress().unwrap().publication_root,
        extra.encoding.object_id()
    );
    assert_eq!(member.progress().unwrap().applied.index, 93);
}
