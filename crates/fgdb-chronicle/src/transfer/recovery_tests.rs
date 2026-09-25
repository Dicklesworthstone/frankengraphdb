//! Real Chronicle crypto and foundation FEC, with a discovered rank-deficient
//! two-equation witness. No mocked decoder result decides cache/scheduler tests.

use super::*;
use crate::identity::{CipherDescriptor, EncodingDescriptor, IdentifiedObject};
use crate::identity::{CryptoVerificationEvent, VerificationOperation};
use crate::symbolize::{blocks::repair_encoder, decode_object};
use std::panic::{AssertUnwindSafe, catch_unwind};

const KEY: [u8; 32] = [37; 32];
const DEK: [u8; 32] = [73; 32];
const HEADER: &[u8] = b"retained-independent-blocks";
const SIZE: usize = 16;

struct Fixture {
    encoding: EncodedObject,
    protected: Vec<u8>,
    plaintext: Vec<u8>,
    blocks: usize,
}

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId(core::array::from_fn(|i| i as u8 ^ 0x61))
}

impl Fixture {
    fn new(blocks: usize) -> Self {
        let plaintext: Vec<u8> = (0..blocks * 2 * SIZE - 16)
            .map(|i| (i % 251) as u8)
            .collect();
        let object = IdentifiedObject::new(&KEY, namespace(), 2, HEADER, &plaintext);
        let protected = object
            .protect(
                &DEK,
                CipherDescriptor {
                    object_kind: 2,
                    canonical_plaintext_len: plaintext.len() as u64,
                    codec_profile: 1,
                    compressed_len: plaintext.len() as u64,
                    data_crypto_profile: 1,
                    dek_id: [9; 16],
                    object_nonce: [31; 24],
                    object_tag_len: 16,
                },
                &plaintext,
            )
            .unwrap();
        let bytes = protected.protected_bytes().to_vec();
        let encoding = protected.encode(EncodingDescriptor {
            fec_profile: 1,
            transfer_length: bytes.len() as u64,
            oti_common: (bytes.len() as u64) << 24 | SIZE as u64,
            oti_scheme: (blocks as u32) << 24 | 2 << 8 | 1,
            symbol_size: SIZE as u16,
            source_block_count: blocks as u16,
            symbol_auth_profile: 1,
        });
        Self {
            encoding,
            protected: bytes,
            plaintext,
            blocks,
        }
    }

    fn target(&self) -> RecoveryTarget<'static> {
        RecoveryTarget {
            k_oid: &KEY,
            namespace: namespace(),
            object_id: self.encoding.object_id(),
            canonical_header: HEADER,
            protected_len: self.protected.len(),
        }
    }

    fn record(&self, block: u32, esi: u32) -> Vec<u8> {
        let layout = Layout::new(&self.encoding, self.protected.len()).unwrap();
        let mut payload = vec![0; SIZE];
        if esi < 2 {
            layout
                .copy_source_symbol(&self.protected, block, esi, &mut payload)
                .unwrap();
        } else {
            let mut source = vec![vec![0; SIZE]; 2];
            for (index, symbol) in source.iter_mut().enumerate() {
                layout
                    .copy_source_symbol(&self.protected, block, index as u32, symbol)
                    .unwrap();
            }
            payload = repair_encoder(&self.encoding, &source)
                .unwrap()
                .try_repair_symbol(esi)
                .unwrap();
        }
        SymbolRecord::for_encoding(&self.encoding, block, esi, 0, payload)
            .serialize(&self.encoding.symbol_auth_key(&DEK))
    }

    fn limits(&self, records: usize) -> PullLimits {
        PullLimits {
            max_symbols: records,
            max_in_flight: 1,
            max_wire_bytes: records * self.record(0, 0).len(),
            max_requests: 1_000_000,
            max_decode_attempts: 3,
            ..PullLimits::default()
        }
    }

    // Probe the actual encoder on basis source bytes. Equal nonzero images
    // identify dependent equations for ANY two source symbols under this code.
    // The ordinary decoder below independently has to confirm the rank failure.
    fn dependent_repairs(&self) -> (u32, u32) {
        let mut basis = vec![vec![0; SIZE]; 2];
        basis[0][0] = 1;
        basis[1][1] = 1;
        let encoder = repair_encoder(&self.encoding, &basis).unwrap();
        let mut seen = BTreeMap::new();
        for esi in 2..65_538 {
            let image = encoder.try_repair_symbol(esi).unwrap();
            assert!(image[2..].iter().all(|value| *value == 0));
            let signature = [image[0], image[1]];
            // ubs:ignore -- two repair-symbol bytes in a rank test, not authentication material.
            if signature == [0, 0] {
                continue;
            }
            if let Some(previous) = seen.insert(signature, esi) {
                return (previous, esi);
            }
        }
        panic!("bounded foundation-code search must exhibit two dependent repairs");
    }

    fn initial(&self, pull: &mut BondedPull<'_>, deficient: &[u32], pair: (u32, u32)) {
        while (0..self.blocks).any(|block| pull.block_symbol_count(block as u32) != Some(2)) {
            let request = pull.schedule(1).unwrap()[0];
            let keep = if deficient.contains(&request.source_block) {
                request.esi == pair.0 || request.esi == pair.1
            } else {
                request.esi < 2
            };
            if keep {
                pull.accept_reply(
                    request,
                    &self.record(request.source_block, request.esi),
                    &mut Vec::new(),
                )
                .unwrap();
            } else {
                pull.expire(request).unwrap();
            }
        }
    }

    fn rescue(&self, block: u32, pair: (u32, u32)) -> u32 {
        // Independent ordinary batch recovery proves this third equation works;
        // no test-only rank flag is supplied to the incremental implementation.
        let mut records = Vec::new();
        for number in 0..self.blocks as u32 {
            for esi in if number == block {
                [pair.0, pair.1]
            } else {
                [0, 1]
            } {
                records.push(self.record(number, esi));
            }
        }
        assert_eq!(
            decode_object(
                &self.encoding,
                &records,
                self.target(),
                &DEK,
                &mut Vec::new()
            ),
            Err(SymbolizeError::InsufficientSymbols)
        );
        for esi in pair.1 + 1..pair.1 + 257 {
            records.push(self.record(block, esi));
            let result = decode_object(
                &self.encoding,
                &records,
                self.target(),
                &DEK,
                &mut Vec::new(),
            );
            records.pop();
            match result {
                Ok(bytes) => {
                    assert_eq!(bytes, self.plaintext);
                    return esi;
                }
                Err(SymbolizeError::InsufficientSymbols) => {}
                other => panic!("valid repair equations must not be corrupt: {other:?}"),
            }
        }
        panic!("bounded independent decoder probe must find a completing equation");
    }

    fn deliver(&self, pull: &mut BondedPull<'_>, block: u32, esi: u32) {
        loop {
            let request = pull.schedule(1).unwrap()[0];
            assert_eq!(
                request.source_block, block,
                "a satisfied block must not consume the last slot"
            );
            assert!(request.esi <= esi);
            if request.esi == esi {
                pull.accept_reply(request, &self.record(block, esi), &mut Vec::new())
                    .unwrap();
                break;
            }
            pull.expire(request).unwrap();
        }
    }
}

#[test]
fn one_rank_deficient_block_can_use_the_last_object_wide_record_slot() {
    let fixture = Fixture::new(2);
    let pair = fixture.dependent_repairs();
    for bad in 0..2 {
        let mut pull = BondedPull::new(
            &fixture.encoding,
            fixture.target(),
            &DEK,
            &[DonorId(1)],
            fixture.limits(5),
        )
        .unwrap();
        fixture.initial(&mut pull, &[bad], pair);
        assert!(pull.try_recover(&mut Vec::new()).unwrap().is_none());
        assert_eq!(pull.recovered_block_count(), 1);
        assert_eq!(pull.recovery.passes, [1, 1]); // Also visits the block AFTER a failure.
        assert_eq!(pull.block_targets().unwrap()[bad as usize], 3);
        assert_eq!(pull.block_targets().unwrap()[1 - bad as usize], 2);
        let rescue = fixture.rescue(bad, pair);
        fixture.deliver(&mut pull, bad, rescue);
        let mut events = Vec::new();
        assert_eq!(
            pull.try_recover(&mut events).unwrap().unwrap().plaintext(),
            fixture.plaintext
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.operation == VerificationOperation::SymbolRecord)
                .count(),
            3
        );
        assert_eq!(
            pull.recovery.passes[1 - bad as usize],
            1,
            "unchanged healthy block not decoded again"
        );
        assert_eq!(pull.recovery.passes[bad as usize], 2);
        assert_eq!(pull.symbol_count(), 5);
        assert_eq!(pull.decode_attempts(), 2);
        assert_eq!(pull.pending_count(), 0);
        assert!(matches!(
            pull.try_recover(&mut Vec::new()),
            Err(PullError::Closed)
        ));
    }
}

#[test]
fn unchanged_or_duplicate_input_does_not_retry_a_failed_round() {
    let fixture = Fixture::new(2);
    let pair = fixture.dependent_repairs();
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        fixture.limits(5),
    )
    .unwrap();
    fixture.initial(&mut pull, &[1], pair);
    assert!(pull.try_recover(&mut Vec::new()).unwrap().is_none());
    for _ in 0..32 {
        assert_eq!(
            pull.accept(DonorId(1), &fixture.record(0, 0), &mut Vec::new())
                .unwrap(),
            SymbolAdmission::Duplicate
        );
        let mut events = Vec::new();
        assert!(pull.try_recover(&mut events).unwrap().is_none());
        assert!(events.is_empty());
        assert_eq!(pull.recovered_block_count(), 1);
    }
    assert_eq!(pull.decode_attempts(), 1);
    assert_eq!(pull.recovery.passes, [1, 1]);
}

#[test]
fn every_deficient_block_must_improve_before_another_object_round() {
    let fixture = Fixture::new(3);
    let pair = fixture.dependent_repairs();
    let mut limits = fixture.limits(8);
    limits.max_in_flight = 2;
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        limits,
    )
    .unwrap();
    fixture.initial(&mut pull, &[0, 2], pair);
    assert!(pull.try_recover(&mut Vec::new()).unwrap().is_none());
    assert_eq!(pull.recovery.passes, [1, 1, 1]);
    // Drive both deficit streams normally, withholding one final response.
    let rescues = [fixture.rescue(0, pair), fixture.rescue(2, pair)];
    let mut held = None;
    while pull.block_symbol_count(0) != Some(3) || held.is_none() {
        let request = pull.schedule(1).unwrap()[0];
        assert_ne!(request.source_block, 1);
        let chosen = rescues[usize::from(request.source_block == 2)];
        if request.esi == chosen {
            if request.source_block == 2 {
                held = Some(request);
            } else {
                pull.accept_reply(request, &fixture.record(0, chosen), &mut Vec::new())
                    .unwrap();
            }
        } else {
            assert!(request.esi < chosen);
            pull.expire(request).unwrap();
        }
    }
    let mut events = Vec::new();
    assert!(pull.try_recover(&mut events).unwrap().is_none());
    assert!(events.is_empty());
    assert_eq!(pull.decode_attempts(), 1);
    let request = held.unwrap();
    pull.accept_reply(request, &fixture.record(2, request.esi), &mut Vec::new())
        .unwrap();
    assert_eq!(
        pull.try_recover(&mut Vec::new())
            .unwrap()
            .unwrap()
            .plaintext(),
        fixture.plaintext
    );
    assert_eq!(pull.recovery.passes, [2, 1, 2]);
    assert_eq!(pull.decode_attempts(), 2);
}

#[test]
fn donor_failure_and_reactivation_preserve_reconstructed_blocks_and_spent_work() {
    let fixture = Fixture::new(2);
    let pair = fixture.dependent_repairs();
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        fixture.limits(5),
    )
    .unwrap();
    fixture.initial(&mut pull, &[1], pair);
    assert!(pull.try_recover(&mut Vec::new()).unwrap().is_none());
    let attempts = (pull.requests, pull.verifications, pull.decode_attempts());
    pull.donor_failed(DonorId(1)).unwrap();
    assert!(matches!(pull.schedule(1), Err(PullError::NoAvailableDonor)));
    pull.donor_available(DonorId(1)).unwrap();
    assert_eq!(
        (pull.requests, pull.verifications, pull.decode_attempts()),
        attempts
    );
    assert_eq!(pull.recovered_block_count(), 1);
    fixture.deliver(&mut pull, 1, fixture.rescue(1, pair));
    assert_eq!(
        pull.try_recover(&mut Vec::new())
            .unwrap()
            .unwrap()
            .plaintext(),
        fixture.plaintext
    );
    assert_eq!(pull.recovery.passes, [1, 2]);
}

#[test]
fn paused_block_work_resumes_without_a_second_round_or_repeated_authentication() {
    let fixture = Fixture::new(5);
    for steps in 0..=5 {
        let mut pull = BondedPull::new(
            &fixture.encoding,
            fixture.target(),
            &DEK,
            &[DonorId(1)],
            fixture.limits(10),
        )
        .unwrap();
        fixture.initial(&mut pull, &[], (0, 0));
        for _ in 0..steps {
            assert!(matches!(
                pull.advance_recovery(&mut Vec::new()).unwrap(),
                recovery::Advance::Progress
            ));
        }
        assert_eq!(pull.recovered_block_count(), steps);
        let mut events = Vec::new();
        assert_eq!(
            pull.try_recover(&mut events).unwrap().unwrap().plaintext(),
            fixture.plaintext
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.operation == VerificationOperation::SymbolRecord)
                .count(),
            2 * (5 - steps)
        );
        assert_eq!(pull.recovery.passes, [1; 5]);
        assert_eq!(pull.decode_attempts(), 1);
    }
}

#[test]
fn late_extra_equation_invalidates_only_its_own_cached_block() {
    let fixture = Fixture::new(3);
    let mut limits = fixture.limits(7);
    limits.max_in_flight = 7;
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        limits,
    )
    .unwrap();
    let requests = pull.schedule(7).unwrap();
    assert_eq!(requests.len(), 7);
    let late = *requests.iter().find(|request| request.esi == 2).unwrap();
    assert_eq!(late.source_block, 0);
    for request in requests.into_iter().filter(|request| request.esi < 2) {
        pull.accept_reply(
            request,
            &fixture.record(request.source_block, request.esi),
            &mut Vec::new(),
        )
        .unwrap();
    }
    assert!(matches!(
        pull.advance_recovery(&mut Vec::new()).unwrap(),
        recovery::Advance::Progress
    ));
    assert_eq!(pull.recovered_block_count(), 1);
    pull.accept_reply(late, &fixture.record(0, 2), &mut Vec::new())
        .unwrap();
    assert_eq!(pull.recovered_block_count(), 0);
    assert_eq!(
        pull.decode_attempts(),
        1,
        "interrupted work is not refunded"
    );
    assert_eq!(
        pull.try_recover(&mut Vec::new())
            .unwrap()
            .unwrap()
            .plaintext(),
        fixture.plaintext
    );
    assert_eq!(pull.recovery.passes, [2, 1, 1]);
    assert_eq!(pull.decode_attempts(), 2);
}

#[test]
fn cached_blocks_cannot_bypass_the_final_namespace_identity_boundary() {
    let fixture = Fixture::new(2);
    let pair = fixture.dependent_repairs();
    let mut target = fixture.target();
    target.namespace.0[0] ^= 1;
    let mut pull = BondedPull::new(
        &fixture.encoding,
        target,
        &DEK,
        &[DonorId(1)],
        fixture.limits(5),
    )
    .unwrap();
    fixture.initial(&mut pull, &[1], pair);
    assert!(pull.try_recover(&mut Vec::new()).unwrap().is_none());
    fixture.deliver(&mut pull, 1, fixture.rescue(1, pair));
    assert!(matches!(
        pull.try_recover(&mut Vec::new()),
        Err(PullError::Recovery(SymbolizeError::IdentityMismatch))
    ));
    assert!(matches!(
        pull.try_recover(&mut Vec::new()),
        Err(PullError::Closed)
    ));
}

struct PanicAt(VerificationOperation);
impl CryptoVerificationSink for PanicAt {
    fn record(&mut self, event: CryptoVerificationEvent) {
        assert_ne!(event.operation, self.0, "injected observation panic");
    }
}

#[test]
fn observation_panics_fence_the_pull_at_symbol_open_and_completion_boundaries() {
    let fixture = Fixture::new(2);
    for operation in [
        VerificationOperation::SymbolRecord,
        VerificationOperation::RecoveredObjectOpen,
        VerificationOperation::ObjectRecovery,
    ] {
        let mut pull = BondedPull::new(
            &fixture.encoding,
            fixture.target(),
            &DEK,
            &[DonorId(1)],
            fixture.limits(4),
        )
        .unwrap();
        fixture.initial(&mut pull, &[], (0, 0));
        assert!(
            catch_unwind(AssertUnwindSafe(
                || pull.try_recover(&mut PanicAt(operation))
            ))
            .is_err()
        );
        assert_eq!(pull.decode_attempts(), 1);
        assert!(matches!(
            pull.try_recover(&mut Vec::new()),
            Err(PullError::Closed)
        ));
        assert!(matches!(pull.schedule(1), Err(PullError::Closed)));
    }
}

#[test]
fn round_limit_is_object_wide_and_cannot_be_reset_by_donor_reactivation() {
    let fixture = Fixture::new(2);
    let pair = fixture.dependent_repairs();
    let mut limits = fixture.limits(5);
    limits.max_decode_attempts = 1;
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        limits,
    )
    .unwrap();
    fixture.initial(&mut pull, &[1], pair);
    assert!(pull.try_recover(&mut Vec::new()).unwrap().is_none());
    fixture.deliver(&mut pull, 1, fixture.rescue(1, pair));
    pull.donor_failed(DonorId(1)).unwrap();
    pull.donor_available(DonorId(1)).unwrap();
    for _ in 0..8 {
        assert!(matches!(
            pull.try_recover(&mut Vec::new()),
            Err(PullError::DecodeBudget)
        ));
    }
    assert_eq!(pull.recovery.passes, [1, 1]);
    assert_eq!(pull.recovered_block_count(), 1);
}

#[test]
fn maximum_source_block_population_completes_in_one_object_round() {
    let fixture = Fixture::new(255);
    let mut limits = fixture.limits(510);
    limits.max_decode_attempts = 1;
    let mut pull = BondedPull::new(
        &fixture.encoding,
        fixture.target(),
        &DEK,
        &[DonorId(1)],
        limits,
    )
    .unwrap();
    fixture.initial(&mut pull, &[], (0, 0));
    assert_eq!(
        pull.try_recover(&mut Vec::new())
            .unwrap()
            .unwrap()
            .plaintext(),
        fixture.plaintext
    );
    assert_eq!(pull.recovery.passes, vec![1; 255]);
    assert_eq!(pull.decode_attempts(), 1);
    assert_eq!(pull.recovered_block_count(), 255);
}
