#![cfg(not(target_arch = "wasm32"))]

//! These are publication-gate histories, not a substitute filesystem proof.
//! Payloads pass the real Chronicle crypto/FEC path; root evidence below is
//! explicitly the test model's post-barrier input. RootStore's separate crash
//! matrix owns the truth of that barrier in a real storage implementation.

#[path = "support/bonded.rs"]
mod support;

use fgdb_chronicle::seed::{
    ReplicaSeed, SeedAnchor, SeedError, SeedLimits, SeedObjectSpec, SeedPlan,
};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId};
use support::{Fixture, KIND};

fn fixtures() -> Vec<Fixture> {
    (41..45).map(Fixture::new).collect()
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

fn specs(objects: &[Fixture]) -> Vec<SeedObjectSpec> {
    objects
        .iter()
        .map(|fixture| SeedObjectSpec {
            object_id: fixture.encoding.object_id(),
            object_kind: KIND,
            compressed_len: fixture.plaintext.len() as u64,
        })
        .collect()
}

fn plan(objects: &[Fixture]) -> SeedPlan {
    SeedPlan::from_authenticated_inventory(anchor(objects), specs(objects), SeedLimits::default())
        .unwrap()
}

fn publish_all(seed: &mut ReplicaSeed, objects: &[Fixture]) {
    for object in objects.iter().rev() {
        let pending = seed.stage(object.verified()).unwrap();
        let id = pending.id();
        assert_eq!(pending.object().plaintext(), object.plaintext);
        seed.object_published(id).unwrap();
    }
}

fn evidence(anchor: &SeedAnchor) -> RootPublicationEvidence {
    RootPublicationEvidence {
        written_index: 1,
        slot_generation: anchor.publication_generation,
        root_manifest_oid: anchor.publication_root.0,
    }
}

#[test]
fn every_object_barrier_and_exact_root_barrier_precede_completion() {
    let objects = fixtures();
    let expected_anchor = anchor(&objects);
    let mut seed = ReplicaSeed::new(plan(&objects));
    assert!(matches!(
        seed.begin_install(),
        Err(SeedError::MissingObjects { remaining: 4 })
    ));
    for (offset, object) in objects.iter().enumerate() {
        let id = seed.stage(object.verified()).unwrap().id();
        assert_eq!(seed.published_count(), offset);
        assert_eq!(seed.missing_objects().count(), 4 - offset);
        assert!(seed.completion().is_none());
        assert!(matches!(
            seed.begin_install(),
            Err(SeedError::AwaitingObjectPublication)
        ));
        assert_eq!(
            seed.object_published(id).unwrap(),
            object.encoding.object_id()
        );
    }
    assert!(seed.completion().is_none());
    let installation = seed.begin_install().unwrap();
    assert_eq!(installation.plan().anchor(), &expected_anchor);
    assert_eq!(installation.incoming_encodings().count(), 4);
    let id = installation.id();
    let complete = seed
        .finish_install(id, &evidence(&expected_anchor))
        .unwrap();
    assert_eq!(complete.anchor(), &expected_anchor);
    assert_eq!(complete.object_count(), 4);
    assert_eq!(complete.bytes(), 4 * 4096);
    assert!(matches!(seed.begin_install(), Err(SeedError::Closed)));
}

#[test]
fn partial_closure_never_produces_installation_even_when_state_root_exists() {
    let objects = fixtures();
    let mut seed = ReplicaSeed::new(plan(&objects));
    let id = seed.stage(objects[1].verified()).unwrap().id();
    seed.object_published(id).unwrap();
    assert!(matches!(
        seed.begin_install(),
        Err(SeedError::MissingObjects { remaining: 3 })
    ));
    assert!(
        seed.missing_objects()
            .any(|object| object.object_id == objects[2].encoding.object_id())
    );
    assert!(seed.completion().is_none());
}

#[test]
fn cross_session_stale_and_wrong_kind_tokens_do_not_release_publication() {
    let objects = fixtures();
    let mut first = ReplicaSeed::new(plan(&objects));
    let mut second = ReplicaSeed::new(plan(&objects));
    let a = first.stage(objects[0].verified()).unwrap().id();
    let b = second.stage(objects[0].verified()).unwrap().id();
    assert!(matches!(
        second.object_published(a.clone()),
        Err(SeedError::StalePublication)
    ));
    first.object_published(a.clone()).unwrap();
    second.object_published(b).unwrap();
    let next = first.stage(objects[1].verified()).unwrap().id();
    assert!(matches!(
        first.object_published(a.clone()),
        Err(SeedError::StalePublication)
    ));
    first.object_published(next).unwrap();
    for object in &objects[2..] {
        let id = first.stage(object.verified()).unwrap().id();
        first.object_published(id).unwrap();
    }
    let install = first.begin_install().unwrap().id();
    assert!(matches!(
        first.finish_install(a, &evidence(&anchor(&objects))),
        Err(SeedError::StalePublication)
    ));
    first
        .finish_install(install, &evidence(&anchor(&objects)))
        .unwrap();
}

#[test]
fn cancelled_object_publication_remains_pending_and_failure_requires_recovery() {
    let objects = fixtures();
    let mut seed = ReplicaSeed::new(plan(&objects));
    let id = seed.stage(objects[0].verified()).unwrap().id();
    assert_eq!(seed.pending_publication().unwrap().id(), id);
    assert_eq!(seed.published_count(), 0);
    assert!(matches!(
        seed.stage(objects[1].verified()),
        Err(SeedError::AwaitingObjectPublication)
    ));
    seed.publication_failed();
    assert!(matches!(
        seed.object_published(id),
        Err(SeedError::RecoveryRequired)
    ));
    assert!(matches!(
        seed.pending_publication(),
        Err(SeedError::RecoveryRequired)
    ));
    assert!(matches!(
        seed.begin_install(),
        Err(SeedError::RecoveryRequired)
    ));
    assert!(seed.completion().is_none());
}

#[test]
fn wrong_namespace_kind_length_and_unlisted_objects_never_become_durable() {
    let objects = fixtures();
    let mut wrong_anchor = anchor(&objects);
    wrong_anchor.namespace = DatabaseSecurityNamespaceId(core::array::from_fn(|_| 0xab));
    let wrong_plan = SeedPlan::from_authenticated_inventory(
        wrong_anchor,
        specs(&objects),
        SeedLimits::default(),
    )
    .unwrap();
    let mut seed = ReplicaSeed::new(wrong_plan);
    assert!(matches!(
        seed.stage(objects[0].verified()),
        Err(SeedError::WrongNamespace)
    ));
    assert_eq!(seed.published_count(), 0);

    let mut incorrect = specs(&objects);
    incorrect[0].object_kind ^= 1;
    let mut seed = ReplicaSeed::new(
        SeedPlan::from_authenticated_inventory(anchor(&objects), incorrect, SeedLimits::default())
            .unwrap(),
    );
    assert!(matches!(
        seed.stage(objects[0].verified()),
        Err(SeedError::KindMismatch)
    ));

    let mut incorrect = specs(&objects);
    incorrect[0].compressed_len += 1;
    let mut seed = ReplicaSeed::new(
        SeedPlan::from_authenticated_inventory(anchor(&objects), incorrect, SeedLimits::default())
            .unwrap(),
    );
    assert!(matches!(
        seed.stage(objects[0].verified()),
        Err(SeedError::LengthMismatch)
    ));
    assert!(matches!(
        seed.stage(Fixture::new(80).verified()),
        Err(SeedError::UnexpectedObject)
    ));
    assert_eq!(seed.published_count(), 0);
}

#[test]
fn duplicate_verified_delivery_does_not_change_inventory_or_progress() {
    let objects = fixtures();
    let mut seed = ReplicaSeed::new(plan(&objects));
    let id = seed.stage(objects[0].verified()).unwrap().id();
    seed.object_published(id).unwrap();
    assert!(matches!(
        seed.stage(objects[0].verified()),
        Err(SeedError::AlreadyPublished)
    ));
    assert_eq!(seed.published_count(), 1);
    assert_eq!(seed.missing_objects().count(), 3);
}

#[test]
fn installation_retry_keeps_one_cut_and_rejects_wrong_root_evidence() {
    let objects = fixtures();
    let expected = anchor(&objects);
    let mut seed = ReplicaSeed::new(plan(&objects));
    publish_all(&mut seed, &objects);
    let id = seed.begin_install().unwrap().id();
    assert_eq!(seed.begin_install().unwrap().id(), id);
    assert!(matches!(
        seed.stage(objects[0].verified()),
        Err(SeedError::InstallPending)
    ));
    let mut wrong = evidence(&expected);
    wrong.root_manifest_oid[0] ^= 1;
    assert!(matches!(
        seed.finish_install(id.clone(), &wrong),
        Err(SeedError::RootEvidenceMismatch)
    ));
    wrong = evidence(&expected);
    wrong.slot_generation += 1;
    assert!(matches!(
        seed.finish_install(id.clone(), &wrong),
        Err(SeedError::RootEvidenceMismatch)
    ));
    wrong = evidence(&expected);
    wrong.written_index = 2;
    assert!(matches!(
        seed.finish_install(id.clone(), &wrong),
        Err(SeedError::RootEvidenceMismatch)
    ));
    assert!(seed.completion().is_none());
    seed.finish_install(id, &evidence(&expected)).unwrap();
    assert_eq!(seed.completion().unwrap().anchor(), &expected);
}

#[test]
fn failed_final_publication_never_reports_an_installed_replica() {
    let objects = fixtures();
    let mut seed = ReplicaSeed::new(plan(&objects));
    publish_all(&mut seed, &objects);
    let id = seed.begin_install().unwrap().id();
    seed.publication_failed();
    assert!(matches!(
        seed.finish_install(id, &evidence(&anchor(&objects))),
        Err(SeedError::RecoveryRequired)
    ));
    assert!(seed.completion().is_none());
    assert_eq!(seed.published_count(), 4);
}

#[test]
fn inventory_admission_rejects_missing_roots_duplicates_and_budget_overflow() {
    let objects = fixtures();
    let mut duplicate = specs(&objects);
    duplicate.push(duplicate[0]);
    assert!(matches!(
        SeedPlan::from_authenticated_inventory(anchor(&objects), duplicate, SeedLimits::default()),
        Err(SeedError::DuplicateObject)
    ));
    assert!(matches!(
        SeedPlan::from_authenticated_inventory(
            anchor(&objects),
            specs(&objects)[..3].iter().copied(),
            SeedLimits::default()
        ),
        Err(SeedError::MissingRoot)
    ));
    assert!(matches!(
        SeedPlan::from_authenticated_inventory(
            anchor(&objects),
            specs(&objects),
            SeedLimits {
                max_objects: 3,
                ..SeedLimits::default()
            }
        ),
        Err(SeedError::ObjectCountBudget)
    ));
    assert!(matches!(
        SeedPlan::from_authenticated_inventory(
            anchor(&objects),
            specs(&objects),
            SeedLimits {
                max_object_bytes: 4095,
                ..SeedLimits::default()
            }
        ),
        Err(SeedError::ObjectSizeBudget)
    ));
    let huge = [
        SeedObjectSpec {
            object_id: ObjectId([1; 32]),
            object_kind: KIND,
            compressed_len: u64::MAX,
        },
        SeedObjectSpec {
            object_id: ObjectId([2; 32]),
            object_kind: KIND,
            compressed_len: 1,
        },
    ];
    assert!(matches!(
        SeedPlan::from_authenticated_inventory(
            anchor(&objects),
            huge,
            SeedLimits {
                max_objects: 4,
                max_object_bytes: u64::MAX,
                max_total_bytes: u64::MAX,
            }
        ),
        Err(SeedError::TotalSizeBudget)
    ));
}

#[test]
fn snapshot_cut_requires_a_term_and_destination_generation() {
    let objects = fixtures();
    let mut bad = anchor(&objects);
    bad.raft_term = 0;
    assert!(matches!(
        SeedPlan::from_authenticated_inventory(bad, specs(&objects), SeedLimits::default()),
        Err(SeedError::InvalidAnchor)
    ));
    let mut bad = anchor(&objects);
    bad.publication_generation = 0;
    assert!(matches!(
        SeedPlan::from_authenticated_inventory(bad, specs(&objects), SeedLimits::default()),
        Err(SeedError::InvalidAnchor)
    ));
}
