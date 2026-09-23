use super::*;
use fgdb_chronicle::seed::ReplicaSeed;

fn refreshed(objects: &[Fixture], extra: &Fixture, generation: u64) -> SeedPlan {
    let mut a = anchor(objects);
    a.publication_generation = generation;
    a.publication_root = extra.encoding.object_id();
    SeedPlan::from_authenticated_inventory(a, objects[..3].iter().chain(std::iter::once(extra)).map(|o| SeedObjectSpec {
        object_id: o.encoding.object_id(), object_kind: KIND, compressed_len: o.plaintext.len() as u64,
    }), SeedLimits::default()).unwrap()
}
fn seed(objects: &[Fixture]) -> ReplicaSeed {
    let mut seed = ReplicaSeed::new(plan(anchor(objects), objects));
    for o in objects {
        let id = seed.stage(o.verified()).unwrap().id();
        seed.object_published(id).unwrap();
    }
    seed
}

#[test]
fn refreshed_destination_reuses_shared_objects_and_requires_the_new_root() {
    let (objects, mut raft, mut download) = setup();
    let extra = Fixture::new(90);
    ready(&mut download, &objects);
    let transfer = download.transfer_id();
    download.refresh_plan(refreshed(&objects, &extra, 30)).unwrap();
    assert_eq!(download.transfer_id(), transfer);
    assert_eq!(download.published_count(), 3);
    assert_eq!(download.missing_objects().copied().collect::<Vec<_>>(), vec![SeedObjectSpec {
        object_id: extra.encoding.object_id(), object_kind: KIND, compressed_len: extra.plaintext.len() as u64,
    }]);
    // An identical explicit retry does not discard state or allocate new IDs.
    download.refresh_plan(refreshed(&objects, &extra, 30)).unwrap();
    let id = download.stage(extra.verified()).unwrap().id();
    download.object_published(id).unwrap();
    let mut storage = Storage::default();
    immediate(download.install(&mut raft, &mut storage)).unwrap();
    assert_eq!(raft.durable_state().unwrap().commit_index(), 93);
}

#[test]
fn refreshed_plan_cannot_change_any_immutable_source_coordinate() {
    let (objects, _, _) = setup();
    for field in 0..10 {
        let mut s = seed(&objects);
        let old = s.plan().anchor().clone();
        let mut a = old.clone();
        a.publication_generation += 1;
        match field {
            0 => a.namespace.0[0] ^= 1,
            1 => a.consensus_domain[0] ^= 1,
            2 => a.configuration[0] ^= 1,
            3 => a.snapshot_manifest = objects[1].encoding.object_id(),
            4 => a.state_root = objects[2].encoding.object_id(),
            5 => a.retention_floor = objects[0].encoding.object_id(),
            6 => a.raft_index += 1,
            7 => a.raft_term += 1,
            8 => a.logical_command_seq += 1,
            9 => a.commit_seq += 1,
            _ => unreachable!(),
        }
        assert_eq!(s.refresh_plan(plan(a, &objects)), Err(SeedError::InvalidAnchor), "field {field}");
        assert_eq!(s.plan().anchor(), &old);
        assert_eq!(s.published_count(), 4);
    }
}

#[test]
fn changed_shared_object_facts_fail_without_partial_owner_mutation() {
    let (objects, _, _) = setup();
    for change_kind in [false, true] {
        let mut s = seed(&objects);
        let old = s.plan().anchor().clone();
        let mut a = old.clone();
        a.publication_generation += 1;
        let specs = objects.iter().enumerate().map(|(i, o)| SeedObjectSpec {
            object_id: o.encoding.object_id(),
            object_kind: KIND + u16::from(i == 2 && change_kind),
            compressed_len: o.plaintext.len() as u64 + u64::from(i == 2 && !change_kind),
        });
        let candidate = SeedPlan::from_authenticated_inventory(a, specs, SeedLimits::default()).unwrap();
        assert_eq!(s.refresh_plan(candidate), Err(if change_kind { SeedError::KindMismatch } else { SeedError::LengthMismatch }));
        assert_eq!(s.plan().anchor(), &old);
        assert_eq!(s.published_count(), 4);
        assert_eq!(s.missing_objects().count(), 0);
    }
}

#[test]
fn unfinished_or_uncertain_publication_cannot_be_reset_by_replanning() {
    let (objects, _, _) = setup();
    let extra = Fixture::new(90);
    let old_plan = plan(anchor(&objects), &objects);
    let mut s = ReplicaSeed::new(old_plan.clone());
    assert!(matches!(s.refresh_plan(refreshed(&objects, &extra, 30)), Err(SeedError::MissingObjects { remaining: 4 })));
    let id = s.stage(objects[0].verified()).unwrap().id();
    assert_eq!(s.refresh_plan(refreshed(&objects, &extra, 30)), Err(SeedError::AwaitingObjectPublication));
    s.refresh_plan(old_plan).unwrap();
    assert_eq!(s.pending_publication().unwrap().id(), id);
    s.publication_failed();
    assert_eq!(s.refresh_plan(refreshed(&objects, &extra, 30)), Err(SeedError::RecoveryRequired));

    let mut s = seed(&objects);
    let install = s.begin_install().unwrap().id();
    assert_eq!(s.refresh_plan(refreshed(&objects, &extra, 30)), Err(SeedError::InstallPending));
    let a = anchor(&objects);
    s.finish_install(install, &RootPublicationEvidence {
        written_index: 0, slot_generation: a.publication_generation, root_manifest_oid: a.publication_root.0,
    }).unwrap();
    assert_eq!(s.refresh_plan(refreshed(&objects, &extra, 30)), Err(SeedError::Closed));
}

#[test]
fn publication_serials_and_exact_root_evidence_survive_refresh() {
    let (objects, _, _) = setup();
    let extra = Fixture::new(90);
    let mut s = ReplicaSeed::new(plan(anchor(&objects), &objects));
    let mut old_id = None;
    for o in &objects {
        let id = s.stage(o.verified()).unwrap().id();
        old_id = Some(id.clone());
        s.object_published(id).unwrap();
    }
    s.refresh_plan(refreshed(&objects, &extra, 30)).unwrap();
    let new_id = s.stage(extra.verified()).unwrap().id();
    let old_id = old_id.unwrap();
    assert_ne!(new_id, old_id);
    assert_eq!(s.object_published(old_id), Err(SeedError::StalePublication));
    assert_eq!(s.pending_publication().unwrap().id(), new_id);
    s.object_published(new_id).unwrap();
    let install = s.begin_install().unwrap().id();
    assert_eq!(s.finish_install(install.clone(), &RootPublicationEvidence {
        written_index: 0, slot_generation: 17, root_manifest_oid: objects[3].encoding.object_id().0,
    }), Err(SeedError::RootEvidenceMismatch));
    let result = s.finish_install(install, &RootPublicationEvidence {
        written_index: 1, slot_generation: 30, root_manifest_oid: extra.encoding.object_id().0,
    }).unwrap();
    assert_eq!(result.object_count(), 4);
    assert_eq!(result.anchor().publication_generation, 30);
}

#[test]
fn replanning_does_not_renew_a_cancelled_raft_offer() {
    let (objects, mut raft, mut download) = setup();
    let extra = Fixture::new(90);
    ready(&mut download, &objects);
    let transfer = download.transfer_id();
    step(&mut raft, Event::SnapshotFailed(transfer.clone()));
    download.refresh_plan(refreshed(&objects, &extra, 30)).unwrap();
    let id = download.stage(extra.verified()).unwrap().id();
    download.object_published(id).unwrap();
    assert_eq!(download.transfer_id(), transfer);
    assert!(matches!(download.prepare(&mut raft), Err(CatchupError::Raft(Error::StaleSnapshotTransfer))));
    assert_eq!(raft.role(), Ok(Role::Follower));
}

#[test]
fn a_different_plan_requires_a_strictly_newer_destination_generation() {
    let (objects, _, _) = setup();
    let extra = Fixture::new(90);
    for generation in [1, 16, 17] {
        let mut s = seed(&objects);
        assert_eq!(s.refresh_plan(refreshed(&objects, &extra, generation)), Err(SeedError::InvalidAnchor));
        assert_eq!(s.published_count(), 4);
        assert_eq!(s.plan().anchor(), &anchor(&objects));
    }
}
