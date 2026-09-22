#![cfg(not(target_arch = "wasm32"))]

//! Real crypto/FEC transfers through the seed publication owner. Root evidence
//! is an explicit post-barrier test input; these tests do not claim filesystem,
//! authenticated-network or membership-activation coverage.
#[path = "support/bonded.rs"]
mod support;

use fgdb_chronicle::seed::{
    ReplicaSeed, SeedAnchor, SeedError, SeedLimits, SeedObjectPull, SeedObjectSpec,
    SeedPlan, SeedPublicationId, SeedPullError,
};
use fgdb_chronicle::store::RootPublicationEvidence;
use fgdb_chronicle::transfer::{DonorId, PullError, PullLimits, SymbolAdmission};
use fgdb_types::DatabaseSecurityNamespaceId;
use support::{DEK, Fixture, KIND};

fn fixtures() -> Vec<Fixture> {
    (51..55).map(Fixture::new).collect()
}

fn anchor(objects: &[Fixture]) -> SeedAnchor {
    SeedAnchor {
        namespace: support::namespace(), consensus_domain: [5; 32], configuration: [6; 32],
        snapshot_manifest: objects[0].encoding.object_id(),
        state_root: objects[1].encoding.object_id(),
        retention_floor: objects[2].encoding.object_id(),
        publication_root: objects[3].encoding.object_id(),
        publication_generation: 19, raft_index: 103, raft_term: 13,
        logical_command_seq: 73, commit_seq: 51,
    }
}

fn specs(objects: &[Fixture]) -> Vec<SeedObjectSpec> {
    objects.iter().map(|object| SeedObjectSpec {
        object_id: object.encoding.object_id(), object_kind: KIND,
        compressed_len: object.plaintext.len() as u64,
    }).collect()
}

fn plan(objects: &[Fixture]) -> SeedPlan {
    SeedPlan::from_authenticated_inventory(anchor(objects), specs(objects), SeedLimits::default()).unwrap()
}

fn stage(pull: &mut SeedObjectPull<'_, '_>, object: &Fixture) -> SeedPublicationId {
    for request in pull.schedule(object.sources).unwrap() {
        pull.accept(request.donor, &object.records[request.esi as usize], &mut Vec::new()).unwrap();
    }
    let publication = pull.try_stage(&mut Vec::new()).unwrap().unwrap();
    assert_eq!(publication.object().plaintext(), object.plaintext);
    publication.id()
}

#[test]
fn full_seed_bonds_three_donors_and_survives_one_failure_per_object() {
    let objects = fixtures();
    let mut seed = ReplicaSeed::new(plan(&objects));
    for (number, object) in objects.iter().enumerate() {
        let mut pull = seed.begin_pull(&object.encoding, object.target(), &DEK,
            &[DonorId(1), DonorId(2), DonorId(3)], PullLimits::default()).unwrap();
        let mut publication_id = None;
        for round in 0..8 {
            let requests = pull.schedule(16).unwrap();
            if round == 0 {
                // Preserve one authenticated contribution from the failed donor.
                let first = requests[0];
                assert_eq!(first.donor, DonorId(1));
                pull.accept(first.donor, &object.records[first.esi as usize], &mut Vec::new()).unwrap();
                pull.donor_failed(DonorId(1)).unwrap();
            }
            for request in requests {
                if request.donor != DonorId(1) {
                    pull.accept(request.donor, &object.records[request.esi as usize], &mut Vec::new()).unwrap();
                }
            }
            if let Some(publication) = pull.try_stage(&mut Vec::new()).unwrap() {
                assert_eq!(publication.object().plaintext(), object.plaintext);
                publication_id = Some(publication.id());
                break;
            }
        }
        let id = publication_id.expect("surviving streams must recover using repair equations");
        assert_eq!(pull.pending_count(), 0);
        assert_eq!(pull.try_stage(&mut Vec::new()).unwrap().unwrap().id(), id);
        assert!(matches!(pull.schedule(1), Err(SeedPullError::Seed(SeedError::AwaitingObjectPublication))));
        assert_eq!(pull.object_published(id).unwrap(), object.encoding.object_id());
        assert_eq!(seed.published_count(), number + 1);
        assert!(seed.completion().is_none());
    }
    let expected = anchor(&objects);
    let install = seed.begin_install().unwrap().id();
    assert!(seed.completion().is_none());
    let evidence = RootPublicationEvidence {
        written_index: 1, slot_generation: expected.publication_generation,
        root_manifest_oid: expected.publication_root.0,
    };
    let completion = seed.finish_install(install, &evidence).unwrap();
    assert_eq!(completion.anchor(), &expected);
    assert_eq!(completion.object_count(), 4);
    assert_eq!(completion.bytes(), 4 * 4096);
}

#[test]
fn target_and_inventory_mismatches_are_rejected_before_a_pull_exists() {
    let objects = fixtures();
    let object = &objects[0];
    let donors = [DonorId(1)];
    let mut seed = ReplicaSeed::new(plan(&objects));
    let mut target = object.target();
    target.namespace = DatabaseSecurityNamespaceId([0x99; 32]);
    assert!(matches!(seed.begin_pull(&object.encoding, target, &DEK, &donors, PullLimits::default()),
        Err(SeedPullError::Seed(SeedError::WrongNamespace))));
    let mut target = object.target();
    target.object_id = objects[1].encoding.object_id();
    assert!(matches!(seed.begin_pull(&object.encoding, target, &DEK, &donors, PullLimits::default()),
        Err(SeedPullError::Pull(PullError::InvalidTarget))));
    let unknown = Fixture::new(91);
    assert!(matches!(seed.begin_pull(&unknown.encoding, unknown.target(), &DEK, &donors, PullLimits::default()),
        Err(SeedPullError::Seed(SeedError::UnexpectedObject))));
    assert_eq!(seed.published_count(), 0);
    assert_eq!(seed.missing_objects().count(), 4);
    for wrong_kind in [true, false] {
        let mut inventory = specs(&objects);
        if wrong_kind {
            inventory[0].object_kind ^= 1;
        } else {
            inventory[0].compressed_len += 1;
        }
        let mut seed = ReplicaSeed::new(SeedPlan::from_authenticated_inventory(
            anchor(&objects), inventory, SeedLimits::default()).unwrap());
        let error = seed.begin_pull(&object.encoding, object.target(), &DEK, &donors, PullLimits::default()).err().unwrap();
        if wrong_kind {
            assert!(matches!(error, SeedPullError::Seed(SeedError::KindMismatch)));
        } else {
            assert!(matches!(error, SeedPullError::Seed(SeedError::LengthMismatch)));
        }
        assert_eq!(seed.published_count(), 0);
    }
}

#[test]
fn cancelled_publication_keeps_owned_bytes_and_the_same_publication_id() {
    let objects = fixtures();
    let object = &objects[0];
    let mut seed = ReplicaSeed::new(plan(&objects));
    let id = {
        let mut pull = seed.begin_pull(&object.encoding, object.target(), &DEK,
            &[DonorId(1)], PullLimits::default()).unwrap();
        let id = stage(&mut pull, object);
        let mut verification = Vec::new();
        assert_eq!(pull.try_stage(&mut verification).unwrap().unwrap().id(), id);
        assert!(verification.is_empty(), "reacquiring staged bytes must not decode again");
        id // Drop transfer owner without acknowledging publication.
    };
    assert_eq!(seed.published_count(), 0);
    assert_eq!(seed.pending_publication().unwrap().id(), id);
    assert_eq!(seed.pending_publication().unwrap().object().plaintext(), object.plaintext);
    assert!(matches!(seed.begin_pull(&objects[1].encoding, objects[1].target(), &DEK,
        &[DonorId(1)], PullLimits::default()), Err(SeedPullError::Seed(SeedError::AwaitingObjectPublication))));
    seed.object_published(id).unwrap();
    assert_eq!(seed.published_count(), 1);
    assert!(matches!(seed.begin_pull(&object.encoding, object.target(), &DEK,
        &[DonorId(1)], PullLimits::default()), Err(SeedPullError::Seed(SeedError::AlreadyPublished))));
}

#[test]
fn wrong_session_acknowledgment_leaves_staged_object_recoverable() {
    let objects = fixtures();
    let object = &objects[0];
    let mut seed = ReplicaSeed::new(plan(&objects));
    let mut other = ReplicaSeed::new(plan(&objects));
    let other_id = other.stage(object.verified()).unwrap().id();
    let mut pull = seed.begin_pull(&object.encoding, object.target(), &DEK,
        &[DonorId(1)], PullLimits::default()).unwrap();
    let id = stage(&mut pull, object);
    assert!(matches!(pull.object_published(other_id), Err(SeedPullError::Seed(SeedError::StalePublication))));
    assert_eq!(seed.published_count(), 0);
    assert_eq!(seed.pending_publication().unwrap().id(), id);
    seed.object_published(id).unwrap();
    assert_eq!(seed.published_count(), 1);
}

#[test]
fn failed_publication_poisons_seed_instead_of_only_resetting_the_pull() {
    let objects = fixtures();
    let object = &objects[0];
    let mut seed = ReplicaSeed::new(plan(&objects));
    let mut pull = seed.begin_pull(&object.encoding, object.target(), &DEK,
        &[DonorId(1)], PullLimits::default()).unwrap();
    let id = stage(&mut pull, object);
    pull.publication_failed();
    assert!(matches!(seed.object_published(id), Err(SeedError::RecoveryRequired)));
    assert!(matches!(seed.begin_pull(&object.encoding, object.target(), &DEK,
        &[DonorId(1)], PullLimits::default()), Err(SeedPullError::Seed(SeedError::RecoveryRequired))));
    assert!(seed.completion().is_none());
}

#[test]
fn bad_mac_does_not_consume_credit_and_expired_answers_cannot_reuse_it() {
    let objects = fixtures();
    let object = &objects[0];
    let mut seed = ReplicaSeed::new(plan(&objects));
    let mut pull = seed.begin_pull(&object.encoding, object.target(), &DEK,
        &[DonorId(1)], PullLimits::default()).unwrap();
    let requests = pull.schedule(2).unwrap();
    let mut corrupt = object.records[requests[0].esi as usize].clone();
    let end = corrupt.len() - 1;
    corrupt[end] ^= 1;
    assert!(matches!(pull.accept(DonorId(1), &corrupt, &mut Vec::new()), Err(SeedPullError::Pull(PullError::Symbol(_)))));
    assert_eq!(pull.pending_count(), 2);
    pull.expire(requests[0]).unwrap();
    assert!(matches!(pull.accept(DonorId(1), &object.records[requests[0].esi as usize], &mut Vec::new()),
        Err(SeedPullError::Pull(PullError::UnrequestedSymbol))));
    assert_eq!(pull.pending_count(), 1);
    assert_eq!(pull.accept(DonorId(1), &object.records[requests[1].esi as usize], &mut Vec::new()).unwrap(), SymbolAdmission::Added);
    assert!(pull.try_stage(&mut Vec::new()).unwrap().is_none());
    assert_eq!(pull.pending_count(), 0);
}

#[test]
fn donor_resume_uses_fresh_equations_and_request_budget_is_not_reset() {
    let objects = fixtures();
    let object = &objects[0];
    let mut seed = ReplicaSeed::new(plan(&objects));
    let limits = PullLimits { max_requests: object.sources as u64, ..PullLimits::default() };
    let mut pull = seed.begin_pull(&object.encoding, object.target(), &DEK, &[DonorId(1)], limits).unwrap();
    let first = pull.schedule(2).unwrap();
    pull.donor_failed(DonorId(1)).unwrap();
    assert_eq!(pull.pending_count(), 0);
    assert!(matches!(pull.schedule(1), Err(SeedPullError::Pull(PullError::NoAvailableDonor))));
    pull.donor_available(DonorId(1)).unwrap();
    let rest = pull.schedule(object.sources).unwrap();
    assert_eq!(rest.len(), object.sources - 2);
    assert!(rest[0].esi > first[1].esi);
    for request in rest {
        pull.expire(request).unwrap();
    }
    assert!(matches!(pull.schedule(1), Err(SeedPullError::Pull(PullError::RequestBudget))));
    assert!(pull.try_stage(&mut Vec::new()).unwrap().is_none());
}

#[test]
fn invalid_donors_or_limits_leave_seed_available_for_a_valid_pull() {
    let objects = fixtures();
    let object = &objects[0];
    let mut seed = ReplicaSeed::new(plan(&objects));
    assert!(matches!(seed.begin_pull(&object.encoding, object.target(), &DEK,
        &[DonorId(1), DonorId(1)], PullLimits::default()), Err(SeedPullError::Pull(PullError::InvalidDonors))));
    assert!(matches!(seed.begin_pull(&object.encoding, object.target(), &DEK,
        &[DonorId(1)], PullLimits { max_in_flight: 0, ..PullLimits::default() }),
        Err(SeedPullError::Pull(PullError::InvalidLimits))));
    let mut pull = seed.begin_pull(&object.encoding, object.target(), &DEK,
        &[DonorId(1)], PullLimits::default()).unwrap();
    let id = stage(&mut pull, object);
    pull.object_published(id).unwrap();
    assert_eq!(seed.published_count(), 1);
}
