#![cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
#[path = "support/multiblock.rs"]
mod support;

use fgdb_chronicle::symbol::SymbolRecord;
use fgdb_chronicle::transfer::{BondedPull, DonorId, PullError, PullLimits, PullRequest, SymbolAdmission};
use std::collections::BTreeSet;
use support::{DEK, Fixture};

fn bytes(f: &Fixture, request: PullRequest) -> &[u8] {
    &f.records[request.source_block as usize][request.esi as usize]
}

fn pull<'a>(f: &'a Fixture, donors: &[DonorId], limits: PullLimits) -> BondedPull<'a> {
    BondedPull::new(&f.encoding, f.target(), &DEK, donors, limits).unwrap()
}

#[test]
fn exact_object_budget_reserves_every_unequal_block_before_extra_equations() {
    for blocks in [2, 3, 5] {
        let f = Fixture::new(blocks, 3, 16);
        let total: usize = f.sources.iter().sum();
        for maximum in [1, 2, 7, 64] {
            let limits = PullLimits {
                max_source_symbols: total, max_symbols: total, max_in_flight: total,
                max_wire_bytes: total * f.records[0][0].len(),
                max_requests: total as u64, max_verifications: total as u64,
                max_esi: f.sources[0] as u32 - 1, ..PullLimits::default()
            };
            let mut p = pull(&f, &[DonorId(1)], limits);
            let mut coordinates = BTreeSet::new();
            for _ in 0..total {
                let requests = p.schedule(maximum).unwrap();
                for request in requests.into_iter().rev() {
                    assert!(coordinates.insert((request.source_block, request.esi)));
                    p.accept_reply(request, bytes(&f, request), &mut Vec::new()).unwrap();
                }
                if p.symbol_count() == total { break; }
            }
            for (block, count) in f.sources.iter().enumerate() {
                assert_eq!(p.block_symbol_count(block as u32), Some(*count));
                for esi in 0..*count { assert!(coordinates.contains(&(block as u32, esi as u32))); }
            }
            assert!(p.schedule(1).unwrap().is_empty());
            assert_eq!(p.try_recover(&mut Vec::new()).unwrap().unwrap().plaintext(), f.plaintext);
            assert_eq!(p.decode_attempts(), 1);
            assert_eq!(p.pending_count(), 0);
        }
    }
}

#[test]
fn donor_residue_streams_are_independent_in_every_block() {
    let f = Fixture::new(3, 3, 16);
    let donors = [DonorId(11), DonorId(22), DonorId(33)];
    let mut p = pull(&f, &donors, PullLimits::default());
    let total: usize = f.sources.iter().sum();
    let requests = p.schedule(total).unwrap();
    assert_eq!(requests.len(), total);
    for block in 0..3 {
        let got: BTreeSet<_> = requests.iter().filter(|r| r.source_block == block)
            .map(|r| r.esi).collect();
        assert_eq!(got, (0..f.sources[block as usize] as u32).collect());
    }
    for request in requests {
        assert_eq!(request.donor.0 / 11 - 1, u128::from(request.esi % 3));
        p.accept_reply(request, bytes(&f, request), &mut Vec::new()).unwrap();
    }
    assert_eq!(p.try_recover(&mut Vec::new()).unwrap().unwrap().plaintext(), f.plaintext);
}

#[test]
fn exact_reply_binding_does_not_steal_equal_esi_from_another_block() {
    let f = Fixture::new(3, 1, 8);
    let mut p = pull(&f, &[DonorId(1)], PullLimits::default());
    let requests = p.schedule(3).unwrap();
    assert!(requests.iter().all(|r| r.esi == 0));
    assert_ne!(requests[0].source_block, requests[1].source_block);
    assert!(matches!(p.accept_reply(requests[0], bytes(&f, requests[1]), &mut Vec::new()),
        Err(PullError::UnrequestedSymbol)));
    assert_eq!(p.pending_count(), 3);
    assert_eq!(p.symbol_count(), 0);
    p.accept_reply(requests[0], bytes(&f, requests[0]), &mut Vec::new()).unwrap();
    assert_eq!(p.pending_count(), 2);
    assert_eq!(p.accept_reply(requests[0], bytes(&f, requests[0]), &mut Vec::new()).unwrap(), SymbolAdmission::Duplicate);
    assert_eq!(p.pending_count(), 2);
    p.accept_reply(requests[1], bytes(&f, requests[1]), &mut Vec::new()).unwrap();
    assert_eq!(p.block_symbol_count(0), Some(1));
    assert_eq!(p.block_symbol_count(1), Some(1));
    assert_eq!(p.block_symbol_count(3), None);
}

#[test]
fn expiration_and_bad_mac_leave_other_blocks_credit_intact() {
    let f = Fixture::new(3, 1, 8);
    let mut p = pull(&f, &[DonorId(1)], PullLimits::default());
    let requests = p.schedule(3).unwrap();
    let mut bad = bytes(&f, requests[1]).to_vec();
    *bad.last_mut().unwrap() ^= 1;
    assert!(matches!(p.accept_reply(requests[1], &bad, &mut Vec::new()), Err(PullError::Symbol(_))));
    assert_eq!(p.pending_count(), 3);
    let mut foreign = requests[0];
    foreign.source_block = 3;
    assert!(matches!(p.expire(foreign), Err(PullError::UnrequestedSymbol)));
    p.expire(requests[0]).unwrap();
    assert!(matches!(p.accept_reply(requests[0], bytes(&f, requests[0]), &mut Vec::new()), Err(PullError::UnrequestedSymbol)));
    p.accept_reply(requests[1], bytes(&f, requests[1]), &mut Vec::new()).unwrap();
    assert_eq!(p.pending_count(), 1);
    for request in p.schedule(8).unwrap() {
        assert_ne!((request.source_block, request.esi), (requests[0].source_block, requests[0].esi));
    }
}

#[test]
fn contradictory_authenticated_symbols_do_not_increment_block_progress() {
    let f = Fixture::new(3, 1, 8);
    let mut p = pull(&f, &[DonorId(1)], PullLimits::default());
    let request = p.schedule(1).unwrap()[0];
    p.accept_reply(request, bytes(&f, request), &mut Vec::new()).unwrap();
    let mut record = SymbolRecord::verify(bytes(&f, request), &f.encoding, &DEK, &mut Vec::new()).unwrap();
    record.payload[0] ^= 1;
    let conflict = record.serialize(&f.encoding.symbol_auth_key(&DEK));
    assert!(matches!(p.accept_reply(request, &conflict, &mut Vec::new()), Err(PullError::ConflictingSymbol)));
    assert_eq!(p.symbol_count(), 1);
    assert_eq!(p.block_symbol_count(request.source_block), Some(1));
}

#[test]
fn missing_block_does_not_burn_decode_budget_from_other_blocks_surplus() {
    let f = Fixture::new(3, 1, 64);
    let mut p = pull(&f, &[DonorId(1)], PullLimits::default());
    for request in p.schedule(64).unwrap() {
        if request.source_block == 2 { p.expire(request).unwrap(); }
        else { p.accept_reply(request, bytes(&f, request), &mut Vec::new()).unwrap(); }
    }
    assert!(p.symbol_count() >= f.sources.iter().sum::<usize>());
    for _ in 0..100 {
        assert!(p.try_recover(&mut Vec::new()).unwrap().is_none());
    }
    assert_eq!(p.decode_attempts(), 0);
    assert_eq!(p.block_symbol_count(2), Some(0));
    // Filled blocks do not consume the remaining storage/refill budget.
    let requests = p.schedule(6).unwrap();
    assert!(requests.iter().all(|r| r.source_block == 2));
}

#[test]
fn requests_and_mac_work_are_single_object_budgets_across_blocks() {
    let f = Fixture::new(3, 1, 8);
    let total: usize = f.sources.iter().sum();
    let mut p = pull(&f, &[DonorId(1)], PullLimits {
        max_requests: total as u64, max_verifications: total as u64, ..PullLimits::default()
    });
    let requests = p.schedule(total).unwrap();
    for request in &requests { p.expire(*request).unwrap(); }
    assert!(matches!(p.schedule(1), Err(PullError::RequestBudget)));
    let mut bad = f.records[0][0].clone();
    *bad.last_mut().unwrap() ^= 1;
    for _ in 0..total {
        assert!(matches!(p.accept(DonorId(1), &bad, &mut Vec::new()), Err(PullError::Symbol(_))));
    }
    assert!(matches!(p.accept(DonorId(1), &bad, &mut Vec::new()), Err(PullError::VerificationBudget)));
    assert_eq!(p.symbol_count(), 0);
}

#[test]
fn a_silent_donor_cannot_hold_every_block_behind_its_pending_equations() {
    let f = Fixture::new(3, 3, 128);
    let donors = [DonorId(1), DonorId(2), DonorId(3)];
    let mut p = pull(&f, &donors, PullLimits::default());
    let mut issued = BTreeSet::new();
    let mut silent = Vec::new();
    let mut recovered = None;
    for _ in 0..128 {
        for request in p.schedule_bonded(9, 9).unwrap() {
            assert!(issued.insert((request.source_block, request.esi)));
            if request.donor == DonorId(1) { silent.push(request); }
            else { p.accept_reply(request, bytes(&f, request), &mut Vec::new()).unwrap(); }
        }
        assert!(p.pending_count() <= 3);
        recovered = p.try_recover(&mut Vec::new()).unwrap();
        if recovered.is_some() { break; }
    }
    assert_eq!(recovered.unwrap().plaintext(), f.plaintext);
    assert!(!silent.is_empty());
    assert_eq!(p.pending_count(), 0);
}

#[test]
fn donor_reactivation_preserves_all_block_streams_and_accepted_equations() {
    let f = Fixture::new(3, 1, 16);
    let mut p = pull(&f, &[DonorId(1), DonorId(2)], PullLimits::default());
    let initial = p.schedule(12).unwrap();
    let first = initial[0];
    p.accept_reply(first, bytes(&f, first), &mut Vec::new()).unwrap();
    p.donor_failed(DonorId(1)).unwrap();
    for request in &initial {
        if request.donor == DonorId(2) { p.expire(*request).unwrap(); }
    }
    assert_eq!(p.pending_count(), 0);
    assert_eq!(p.block_symbol_count(first.source_block), Some(1));
    p.donor_available(DonorId(1)).unwrap();
    let old: BTreeSet<_> = initial.iter().map(|r| (r.source_block, r.esi)).collect();
    let later = p.schedule(12).unwrap();
    assert!(later.iter().any(|r| r.donor == DonorId(1)));
    for request in later {
        assert!(!old.contains(&(request.source_block, request.esi)));
        assert_eq!(request.esi % 2, (request.donor.0 - 1) as u32);
    }
    assert_eq!(p.symbol_count(), 1);
}

#[test]
fn decoder_shape_can_span_many_blocks_without_multiplying_object_limits() {
    let (encoding, protected, _) = Fixture::object(13, 56_388, 1, 2, 1, 1, |_| {});
    let target = fgdb_chronicle::symbolize::RecoveryTarget {
        k_oid: &support::KEY, namespace: support::namespace(), object_id: encoding.object_id(),
        canonical_header: support::HEADER, protected_len: protected.len(),
    };
    let total = protected.len();
    let limits = PullLimits { max_source_symbols: total, max_symbols: total,
        max_wire_bytes: total * 256, max_esi: total.div_ceil(2) as u32 - 1, ..PullLimits::default() };
    let mut p = BondedPull::new(&encoding, target, &DEK, &[DonorId(1)], limits).unwrap();
    assert_eq!(p.schedule(2).unwrap().len(), 2);
    assert!(matches!(BondedPull::new(&encoding, target, &DEK, &[DonorId(1)], PullLimits {
        max_source_symbols: total - 1, ..limits
    }), Err(PullError::InvalidLimits)));
    assert!(matches!(BondedPull::new(&encoding, target, &DEK, &[DonorId(1)], PullLimits {
        max_esi: limits.max_esi - 1, ..limits
    }), Err(PullError::InvalidLimits)));
}

#[test]
fn tiny_blocks_prefer_available_original_sources_over_unneeded_repairs() {
    let f = Fixture::with_shape(15, 1, 16, 2, 1, 4, 0);
    let mut p = pull(&f, &[DonorId(1), DonorId(2)], PullLimits {
        max_source_symbols: 2, max_symbols: 2, max_in_flight: 2,
        max_requests: 2, max_verifications: 2, max_esi: 0,
        ..PullLimits::default()
    });
    for block in 0..2 {
        let requests = p.schedule_bonded(1, 1).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!((requests[0].source_block, requests[0].esi), (block, 0));
        p.accept_reply(requests[0], bytes(&f, requests[0]), &mut Vec::new()).unwrap();
    }
    assert_eq!(p.try_recover(&mut Vec::new()).unwrap().unwrap().plaintext(), f.plaintext);
}

#[test]
fn a_smaller_window_never_overwrites_existing_cross_block_reservations() {
    let f = Fixture::new(3, 1, 16);
    let mut p = pull(&f, &[DonorId(1), DonorId(2), DonorId(3)], PullLimits::default());
    let initial = p.schedule_bonded(9, 9).unwrap();
    assert_eq!(p.pending_count(), 9);
    assert!(p.schedule_bonded(9, 4).unwrap().is_empty());
    assert_eq!(p.pending_count(), 9);
    for request in &initial[..6] { p.expire(*request).unwrap(); }
    let later = p.schedule_bonded(9, 4).unwrap();
    assert_eq!(later.len(), 1);
    assert_eq!(p.pending_count(), 4);
    assert!(!initial.contains(&later[0]));
    assert!(matches!(p.schedule_bonded(1, 0), Err(PullError::InvalidLimits)));
}
