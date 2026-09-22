#![cfg(not(target_arch = "wasm32"))]

#[path = "support/bonded.rs"]
mod support;

use fgdb_chronicle::symbol::{SymbolError, SymbolRecord};
use fgdb_chronicle::symbolize::SymbolizeError;
use fgdb_chronicle::transfer::{BondedPull, DonorId, PullError, PullLimits, SymbolAdmission};
use std::collections::BTreeSet;
use support::{DEK, Fixture};

#[test]
fn bonded_sources_recover_through_existing_crypto_and_fec_pipeline() {
    let fixture = Fixture::new(7);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(11), DonorId(22), DonorId(33)],
        PullLimits::default(),
    )
    .unwrap();
    let requests = pull.schedule(fixture.sources).unwrap();
    assert_eq!(requests.len(), fixture.sources);
    let mut seen = BTreeSet::new();
    for request in requests.iter().rev() {
        assert!(seen.insert(request.esi));
        assert_eq!(request.donor.0 / 11 - 1, u128::from(request.esi % 3));
        let bytes = &fixture.records[request.esi as usize];
        assert_eq!(
            pull.accept(request.donor, bytes, &mut Vec::new()).unwrap(),
            SymbolAdmission::Added
        );
        assert_eq!(
            pull.accept(request.donor, bytes, &mut Vec::new()).unwrap(),
            SymbolAdmission::Duplicate
        );
    }
    assert_eq!(pull.symbol_count(), fixture.sources);
    let object = pull.try_recover(&mut Vec::new()).unwrap().unwrap();
    assert_eq!(object.namespace(), support::namespace());
    assert_eq!(object.object_id(), fixture.encoding.object_id());
    assert_eq!(object.plaintext(), fixture.plaintext);
    assert_eq!(pull.decode_attempts(), 1);
    assert!(matches!(pull.schedule(1), Err(PullError::Closed)));
}

#[test]
fn donor_loss_preserves_accepted_symbols_and_survivors_supply_repairs() {
    let fixture = Fixture::new(9);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1), DonorId(2), DonorId(3)],
        PullLimits::default(),
    )
    .unwrap();
    let initial = pull.schedule(fixture.sources).unwrap();
    // Admit one contribution from the donor that will fail, then abandon its
    // other requests. Good contributions remain useful after its connection dies.
    let first = initial[0];
    pull.accept(
        first.donor,
        &fixture.records[first.esi as usize],
        &mut Vec::new(),
    )
    .unwrap();
    pull.donor_failed(first.donor).unwrap();
    assert_eq!(pull.symbol_count(), 1);
    for request in initial
        .iter()
        .filter(|request| request.donor != first.donor)
    {
        pull.accept(
            request.donor,
            &fixture.records[request.esi as usize],
            &mut Vec::new(),
        )
        .unwrap();
    }
    assert!(pull.try_recover(&mut Vec::new()).unwrap().is_none());
    let mut recovered = None;
    for _ in 0..8 {
        for request in pull.schedule(6).unwrap() {
            assert_ne!(request.donor, first.donor);
            pull.accept(
                request.donor,
                &fixture.records[request.esi as usize],
                &mut Vec::new(),
            )
            .unwrap();
        }
        recovered = pull.try_recover(&mut Vec::new()).unwrap();
        if recovered.is_some() {
            break;
        }
    }
    assert_eq!(
        recovered
            .expect("surviving donors supply sufficient repair equations")
            .plaintext(),
        fixture.plaintext
    );
}

#[test]
fn bad_mac_foreign_encoding_and_unsolicited_symbols_preserve_credit() {
    let fixture = Fixture::new(11);
    let foreign = Fixture::new(12);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        PullLimits::default(),
    )
    .unwrap();
    let request = pull.schedule(1).unwrap()[0];
    let mut forged = fixture.records[0].clone();
    *forged.last_mut().unwrap() ^= 1;
    assert!(matches!(
        pull.accept(request.donor, &forged, &mut Vec::new()),
        Err(PullError::Symbol(SymbolError::AuthenticationFailed))
    ));
    assert!(matches!(
        pull.accept(request.donor, &foreign.records[0], &mut Vec::new()),
        Err(PullError::Symbol(SymbolError::ForeignEncoding))
    ));
    assert!(matches!(
        pull.accept(request.donor, &fixture.records[1], &mut Vec::new()),
        Err(PullError::UnrequestedSymbol)
    ));
    assert_eq!(pull.pending_count(), 1);
    assert_eq!(pull.symbol_count(), 0);
    pull.accept(request.donor, &fixture.records[0], &mut Vec::new())
        .unwrap();
    assert_eq!(pull.pending_count(), 0);
}

#[test]
fn authenticated_conflicting_esi_is_not_a_second_equation() {
    let fixture = Fixture::new(13);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        PullLimits::default(),
    )
    .unwrap();
    pull.schedule(1).unwrap();
    pull.accept(DonorId(1), &fixture.records[0], &mut Vec::new())
        .unwrap();
    let mut conflict = SymbolRecord::verify(
        &fixture.records[0],
        &fixture.encoding,
        &DEK,
        &mut Vec::new(),
    )
    .unwrap();
    conflict.payload[0] ^= 1;
    let conflict = conflict.serialize(&fixture.encoding.symbol_auth_key(&DEK));
    assert!(matches!(
        pull.accept(DonorId(1), &conflict, &mut Vec::new()),
        Err(PullError::ConflictingSymbol)
    ));
    assert_eq!(pull.symbol_count(), 1);
}

#[test]
fn expiry_and_donor_reenable_never_reuse_request_coordinates() {
    let fixture = Fixture::new(15);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1), DonorId(2)],
        PullLimits::default(),
    )
    .unwrap();
    let requests = pull.schedule(4).unwrap();
    pull.expire(requests[0]).unwrap();
    assert!(matches!(
        pull.expire(requests[0]),
        Err(PullError::UnrequestedSymbol)
    ));
    assert!(matches!(
        pull.accept(requests[0].donor, &fixture.records[0], &mut Vec::new()),
        Err(PullError::UnrequestedSymbol)
    ));
    pull.donor_failed(DonorId(1)).unwrap();
    assert_eq!(pull.pending_count(), 2);
    pull.donor_available(DonorId(1)).unwrap();
    let later = pull.schedule(4).unwrap();
    assert!(later.iter().all(|request| request.esi >= 4));
    assert_eq!(pull.pending_count(), 6);
}

#[test]
fn bounded_storage_reserves_space_for_every_outstanding_request() {
    let fixture = Fixture::new(17);
    let limits = PullLimits {
        max_symbols: fixture.sources,
        max_in_flight: fixture.sources,
        max_wire_bytes: fixture.sources * fixture.records[0].len(),
        ..PullLimits::default()
    };
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        limits,
    )
    .unwrap();
    let requests = pull.schedule(usize::MAX).unwrap();
    assert_eq!(requests.len(), fixture.sources);
    assert!(pull.schedule(1).unwrap().is_empty());
    for request in requests {
        pull.accept(
            request.donor,
            &fixture.records[request.esi as usize],
            &mut Vec::new(),
        )
        .unwrap();
    }
    assert!(pull.schedule(1).unwrap().is_empty());
    assert_eq!(
        pull.try_recover(&mut Vec::new())
            .unwrap()
            .unwrap()
            .plaintext(),
        fixture.plaintext
    );
}

#[test]
fn request_and_authentication_budgets_are_enforced_even_for_retries() {
    let fixture = Fixture::new(19);
    let limits = PullLimits {
        max_requests: fixture.sources as u64,
        max_verifications: fixture.sources as u64,
        ..PullLimits::default()
    };
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        limits,
    )
    .unwrap();
    let requests = pull.schedule(fixture.sources).unwrap();
    for request in &requests {
        pull.expire(*request).unwrap();
    }
    assert!(matches!(pull.schedule(1), Err(PullError::RequestBudget)));
    let mut bad = fixture.records[0].clone();
    *bad.last_mut().unwrap() ^= 1;
    for _ in 0..fixture.sources {
        assert!(matches!(
            pull.accept(DonorId(1), &bad, &mut Vec::new()),
            Err(PullError::Symbol(_))
        ));
    }
    assert!(matches!(
        pull.accept(DonorId(1), &bad, &mut Vec::new()),
        Err(PullError::VerificationBudget)
    ));
    for _ in 0..100 {
        assert!(pull.try_recover(&mut Vec::new()).unwrap().is_none());
    }
    assert_eq!(pull.decode_attempts(), 0);
}

#[test]
fn unsupported_blocks_and_target_mismatch_fail_before_any_request() {
    let fixture = Fixture::new(21);
    let multiblock = Fixture::with_source_blocks(21, 2);
    assert!(matches!(
        BondedPull::new(
            &multiblock.encoding,
            multiblock.target(),
            &DEK,
            &[DonorId(1)],
            PullLimits::default()
        ),
        Err(PullError::UnsupportedSourceBlocks)
    ));
    let mut target = fixture.target();
    target.protected_len += 1;
    assert!(matches!(
        BondedPull::new(
            &fixture.encoding,
            target,
            &DEK,
            &[DonorId(1)],
            PullLimits::default()
        ),
        Err(PullError::InvalidTarget)
    ));
    assert!(matches!(
        BondedPull::new(
            &fixture.encoding,
            fixture.target(),
            &DEK,
            &[DonorId(1), DonorId(1)],
            PullLimits::default()
        ),
        Err(PullError::InvalidDonors)
    ));
}

#[test]
fn final_logical_identity_verification_is_not_replaced_by_donor_agreement() {
    let fixture = Fixture::new(23);
    let mut target = fixture.target();
    target.namespace = fgdb_types::DatabaseSecurityNamespaceId(core::array::from_fn(|_| 0x88));
    let mut pull = BondedPull::new(
        &fixture.encoding,
        target,
        &DEK,
        &[DonorId(1), DonorId(2)],
        PullLimits::default(),
    )
    .unwrap();
    for request in pull.schedule(fixture.sources).unwrap() {
        pull.accept(
            request.donor,
            &fixture.records[request.esi as usize],
            &mut Vec::new(),
        )
        .unwrap();
    }
    assert!(matches!(
        pull.try_recover(&mut Vec::new()),
        Err(PullError::Recovery(SymbolizeError::IdentityMismatch))
    ));
    assert!(matches!(
        pull.try_recover(&mut Vec::new()),
        Err(PullError::Closed)
    ));
}

#[test]
fn verified_object_debug_does_not_include_plaintext() {
    let fixture = Fixture::new(25);
    let object = fixture.verified();
    let text = format!("{object:?}");
    assert!(text.contains("[REDACTED]"));
    assert!(!text.contains(&format!("{:?}", fixture.plaintext)));
    assert_eq!(object.into_plaintext(), fixture.plaintext);
}
