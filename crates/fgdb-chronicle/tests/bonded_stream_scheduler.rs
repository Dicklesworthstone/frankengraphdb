#![cfg(not(target_arch = "wasm32"))]

#[allow(dead_code)]
#[path = "support/bonded.rs"]
mod support;

use fgdb_chronicle::transfer::{BondedPull, DonorId, PullError, PullLimits};
use support::{DEK, Fixture};

#[test]
fn slow_donors_cannot_accumulate_the_healthy_donors_window() {
    let fixture = Fixture::new(131);
    let donors = [DonorId(1), DonorId(2), DonorId(3)];
    let mut pull = BondedPull::new(
        &fixture.encoding, fixture.target(), &DEK, &donors, PullLimits::default(),
    ).unwrap();
    let requests = pull.schedule_bonded(8, 8).unwrap();
    assert_eq!(requests.len(), 8);
    for (donor, expected) in [(DonorId(1), 3), (DonorId(2), 3), (DonorId(3), 2)] {
        assert_eq!(requests.iter().filter(|request| request.donor == donor).count(), expected);
    }
    let mut healthy: Vec<_> = requests.into_iter()
        .filter(|request| request.donor == DonorId(3)).collect();
    for _ in 0..32 {
        for request in healthy {
            pull.expire(request).unwrap();
        }
        healthy = pull.schedule_bonded(8, 8).unwrap();
        assert_eq!(healthy.len(), 2);
        assert!(healthy.iter().all(|request| request.donor == DonorId(3)));
        assert_eq!(pull.pending_count(), 8);
    }
}

#[test]
fn failure_reassigns_credits_without_renumbering_esi_residues() {
    let fixture = Fixture::new(132);
    let donors = [DonorId(1), DonorId(2), DonorId(3)];
    let mut pull = BondedPull::new(
        &fixture.encoding, fixture.target(), &DEK, &donors, PullLimits::default(),
    ).unwrap();
    let first = pull.schedule_bonded(6, 6).unwrap();
    pull.donor_failed(DonorId(1)).unwrap();
    pull.donor_failed(DonorId(2)).unwrap();
    for request in first.into_iter().filter(|request| request.donor == DonorId(3)) {
        pull.expire(request).unwrap();
    }
    let next = pull.schedule_bonded(6, 6).unwrap();
    assert_eq!(next.len(), 6);
    assert!(next.iter().all(|request| request.donor == DonorId(3) && request.esi % 3 == 2));
    assert_eq!(pull.donor_count(), 3);
}

#[test]
fn lowering_window_is_backpressure_not_unsigned_underflow() {
    let fixture = Fixture::new(133);
    let mut pull = BondedPull::new(
        &fixture.encoding, fixture.target(), &DEK, &[DonorId(1)], PullLimits::default(),
    ).unwrap();
    let requests = pull.schedule_bonded(8, 8).unwrap();
    assert!(pull.schedule_bonded(8, 1).unwrap().is_empty());
    assert_eq!(pull.pending_count(), requests.len());
    assert!(matches!(pull.schedule_bonded(8, 0), Err(PullError::InvalidLimits)));
}
