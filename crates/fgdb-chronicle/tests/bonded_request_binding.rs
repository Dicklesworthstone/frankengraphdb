#![cfg(not(target_arch = "wasm32"))]

#[allow(dead_code)]
#[path = "support/bonded.rs"]
mod support;

use fgdb_chronicle::transfer::{BondedPull, DonorId, PullError, PullLimits, SymbolAdmission};
use fgdb_types::ObjectId;
use support::{DEK, Fixture};

#[test]
fn authenticated_record_cannot_answer_a_different_outstanding_request() {
    let fixture = Fixture::new(111);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        PullLimits::default(),
    )
    .unwrap();
    let requests = pull.schedule(2).unwrap();
    let mut verification = Vec::new();
    assert!(matches!(
        pull.accept_reply(requests[0], &fixture.records[1], &mut verification),
        Err(PullError::UnrequestedSymbol)
    ));
    assert_eq!(pull.pending_count(), 2);
    assert_eq!(pull.symbol_count(), 0);
    for request in requests {
        assert_eq!(
            pull.accept_reply(
                request,
                &fixture.records[request.esi as usize],
                &mut verification,
            )
            .unwrap(),
            SymbolAdmission::Added
        );
    }
    assert_eq!(pull.pending_count(), 0);
}

#[test]
fn duplicate_cannot_masquerade_as_a_fresh_equation() {
    let fixture = Fixture::new(112);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        PullLimits::default(),
    )
    .unwrap();
    let requests = pull.schedule(2).unwrap();
    let mut verification = Vec::new();
    pull.accept_reply(requests[0], &fixture.records[0], &mut verification)
        .unwrap();
    assert_eq!(
        pull.accept_reply(requests[0], &fixture.records[0], &mut verification)
            .unwrap(),
        SymbolAdmission::Duplicate
    );
    assert!(matches!(
        pull.accept_reply(requests[1], &fixture.records[0], &mut verification),
        Err(PullError::UnrequestedSymbol)
    ));
    assert_eq!(pull.pending_count(), 1);
    assert_eq!(pull.symbol_count(), 1);
    pull.accept_reply(requests[1], &fixture.records[1], &mut verification)
        .unwrap();
    assert_eq!(pull.pending_count(), 0);
}

#[test]
fn foreign_request_identity_never_consumes_credit() {
    let fixture = Fixture::new(113);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        PullLimits::default(),
    )
    .unwrap();
    let request = pull.schedule(1).unwrap()[0];
    let mut wrong_object = request;
    wrong_object.object_id = ObjectId([0; 32]);
    let mut wrong_encoding = request;
    wrong_encoding.encoding_id = Default::default();
    let mut wrong_block = request;
    wrong_block.source_block = 1;
    for foreign in [wrong_object, wrong_encoding, wrong_block] {
        assert_ne!(foreign, request);
        assert!(matches!(
            pull.accept_reply(foreign, &fixture.records[0], &mut Vec::new()),
            Err(PullError::UnrequestedSymbol)
        ));
        assert_eq!(pull.pending_count(), 1);
        assert_eq!(pull.symbol_count(), 0);
    }
}

#[test]
fn late_reply_and_bad_mac_leave_current_request_owned() {
    let fixture = Fixture::new(114);
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        PullLimits::default(),
    )
    .unwrap();
    let old = pull.schedule(1).unwrap()[0];
    pull.expire(old).unwrap();
    let current = pull.schedule(1).unwrap()[0];
    let mut verification = Vec::new();
    assert!(matches!(
        pull.accept_reply(old, &fixture.records[old.esi as usize], &mut verification),
        Err(PullError::UnrequestedSymbol)
    ));
    let mut corrupted = fixture.records[current.esi as usize].clone();
    *corrupted.last_mut().unwrap() ^= 1;
    assert!(matches!(
        pull.accept_reply(current, &corrupted, &mut verification),
        Err(PullError::Symbol(_))
    ));
    assert_eq!(pull.pending_count(), 1);
    assert_eq!(pull.symbol_count(), 0);
    pull.accept_reply(
        current,
        &fixture.records[current.esi as usize],
        &mut verification,
    )
    .unwrap();
    assert_eq!(pull.pending_count(), 0);
}
