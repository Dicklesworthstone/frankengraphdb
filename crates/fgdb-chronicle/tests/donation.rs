#![cfg(not(target_arch = "wasm32"))]

use asupersync::raptorq::systematic::{SystematicEncoder, SystematicParams};
use fgdb_chronicle::donor::{BondedDonor, DonorError, DonorLimits};
use fgdb_chronicle::identity::{CipherDescriptor, EncodedObject, EncodingDescriptor, IdentifiedObject};
use fgdb_chronicle::symbol::SymbolRecord;
use fgdb_chronicle::symbolize::{RecoveryTarget, SymbolizeError, encode_object};
use fgdb_chronicle::transfer::{BondedPull, DonorId, PullLimits, PullRequest};
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId};

const KEY: [u8; 32] = [0x39; 32];
const DEK: [u8; 32] = [0x63; 32];
const HEADER: &[u8] = b"request-driven-donor";
const SIZE: usize = 64;
fn check() -> Result<(), &'static str> { Ok(()) }

struct Fixture {
    encoding: EncodedObject,
    protected: Vec<u8>,
    plaintext: Vec<u8>,
    blocks: usize,
    subblocks: usize,
}
impl Fixture {
    fn new(blocks: usize, subblocks: usize) -> Self {
        let plaintext: Vec<_> = (0..1021).map(|i| (i % 251) as u8).collect();
        let namespace = DatabaseSecurityNamespaceId([9; 32]);
        let object = IdentifiedObject::new(&KEY, namespace, 2, HEADER, &plaintext);
        let protected = object.protect(&DEK, CipherDescriptor {
            object_kind: 2, canonical_plaintext_len: plaintext.len() as u64,
            codec_profile: 1, compressed_len: plaintext.len() as u64,
            data_crypto_profile: 1, dek_id: [9; 16], object_nonce: [7; 24], object_tag_len: 16,
        }, &plaintext).unwrap();
        let bytes = protected.protected_bytes().to_vec();
        let encoding = protected.encode(EncodingDescriptor {
            fec_profile: 1, transfer_length: bytes.len() as u64,
            oti_common: if blocks == 1 { 0x0001_0002_0003_0004 } else {
                ((bytes.len() as u64) << 24) | SIZE as u64
            },
            oti_scheme: if blocks == 1 { 0x0005_0006 } else {
                ((blocks as u32) << 24) | ((subblocks as u32) << 8) | 4
            },
            symbol_size: SIZE as u16, source_block_count: blocks as u16, symbol_auth_profile: 1,
        });
        Self { encoding, protected: bytes, plaintext, blocks, subblocks }
    }
    fn target(&self) -> RecoveryTarget<'static> {
        RecoveryTarget { k_oid: &KEY, namespace: DatabaseSecurityNamespaceId([9; 32]),
            object_id: self.encoding.object_id(), canonical_header: HEADER,
            protected_len: self.protected.len() }
    }
    fn donor(&self, id: DonorId, roster: &[DonorId], limits: DonorLimits) -> BondedDonor<'_> {
        BondedDonor::new(&self.encoding, &self.protected, self.target(), &DEK,
            id, roster, limits, &mut Vec::new(), check).unwrap()
    }
    fn request(&self, donor: u128, block: u32, esi: u32) -> PullRequest {
        PullRequest { donor: DonorId(donor), object_id: self.encoding.object_id(),
            encoding_id: self.encoding.encoding_id(), source_block: block, esi }
    }
    // Independent sequential RFC sub-block byte walk, not Layout or its helpers.
    fn sources(&self, block: usize) -> Vec<Vec<u8>> {
        let total = self.protected.len().div_ceil(SIZE);
        let k = total / self.blocks + usize::from(block < total % self.blocks);
        let start = (block * (total / self.blocks) + block.min(total % self.blocks)) * SIZE;
        let n = if self.blocks == 1 { 1 } else { self.subblocks };
        let alignment = if self.blocks == 1 { 1 } else { 4 };
        let units = SIZE / alignment;
        let mut source = vec![Vec::new(); k];
        let mut position = start;
        for sub in 0..n {
            let width = (units / n + usize::from(sub < units % n)) * alignment;
            for symbol in &mut source {
                for _ in 0..width {
                    symbol.push(self.protected.get(position).copied().unwrap_or(0));
                    position += 1;
                }
            }
        }
        source
    }
}

#[test]
fn requested_symbols_match_independent_source_walk_and_existing_batch_records() {
    for (blocks, subblocks) in [(1, 1), (2, 1), (3, 3), (5, 7)] {
        let fixture = Fixture::new(blocks, subblocks);
        let mut donor = fixture.donor(DonorId(7), &[DonorId(7)], DonorLimits::default());
        for block in 0..blocks {
            let sources = fixture.sources(block);
            let batch = encode_object(&fixture.encoding, &fixture.protected, 2, block as u32, 6, &DEK).unwrap();
            for esi in (0..batch.len()).rev() {
                let record = donor.respond(fixture.request(7, block as u32, esi as u32), check).unwrap();
                assert_eq!(record, batch[esi]);
                let symbol = SymbolRecord::verify(&record, &fixture.encoding, &DEK, &mut Vec::new()).unwrap();
                assert_eq!((symbol.source_block, symbol.esi), (block as u32, esi as u32));
                if esi < sources.len() { assert_eq!(symbol.payload, sources[esi]); }
            }
        }
        assert_eq!(donor.usage().encoder_builds, blocks as u32);
        assert_eq!(donor.usage().cached_blocks, blocks);
    }
}

#[test]
fn maximum_esi_is_one_response_not_an_enormous_generated_prefix() {
    let fixture = Fixture::new(3, 3);
    let mut donor = fixture.donor(DonorId(7), &[DonorId(7)], DonorLimits::default());
    let request = fixture.request(7, 2, 0x00ff_ffff);
    let first = donor.respond(request, check).unwrap();
    assert_eq!(first.len(), donor.record_len());
    let record = SymbolRecord::verify(&first, &fixture.encoding, &DEK, &mut Vec::new()).unwrap();
    let seed = u64::from_be_bytes(fixture.encoding.encoding_id().0[..8].try_into().unwrap());
    let native = SystematicEncoder::new(&fixture.sources(2), SIZE, seed).unwrap();
    assert_eq!(record.payload, native.try_repair_symbol(request.esi).unwrap());
    assert_eq!(donor.respond(request, check).unwrap(), first);
    donor.respond(fixture.request(7, 2, 0), check).unwrap();
    assert_eq!(donor.usage().encoder_builds, 1);
    assert_eq!(donor.usage().requests, 3);
    assert_eq!(donor.usage().charged_wire_bytes, 3 * donor.record_len() as u64);
}

#[test]
fn complete_object_authentication_precedes_all_donation() {
    let fixture = Fixture::new(3, 3);
    let mut corrupt = fixture.protected.clone();
    corrupt[17] ^= 1;
    assert!(matches!(BondedDonor::new(&fixture.encoding, &corrupt, fixture.target(), &DEK,
        DonorId(7), &[DonorId(7)], DonorLimits::default(), &mut Vec::new(), check),
        Err(DonorError::Encoding(SymbolizeError::AuthenticationFailed | SymbolizeError::CiphertextIdentityMismatch))));
    for target in [
        RecoveryTarget { namespace: DatabaseSecurityNamespaceId([8; 32]), ..fixture.target() },
        RecoveryTarget { canonical_header: b"other", ..fixture.target() },
        RecoveryTarget { k_oid: &[0; 32], ..fixture.target() },
    ] {
        assert!(matches!(BondedDonor::new(&fixture.encoding, &fixture.protected, target, &DEK,
            DonorId(7), &[DonorId(7)], DonorLimits::default(), &mut Vec::new(), check),
            Err(DonorError::Encoding(SymbolizeError::IdentityMismatch))));
    }
    assert!(matches!(BondedDonor::new(&fixture.encoding, &fixture.protected,
        RecoveryTarget { object_id: ObjectId([1; 32]), ..fixture.target() }, &DEK,
        DonorId(7), &[DonorId(7)], DonorLimits::default(), &mut Vec::new(), check), Err(DonorError::InvalidObject)));
}

#[test]
fn object_and_roster_refusals_precede_cryptographic_work() {
    let fixture = Fixture::new(2, 3);
    let mut verification = Vec::new();
    for limits in [
        DonorLimits { max_protected_bytes: fixture.protected.len() - 1, ..DonorLimits::default() },
        DonorLimits { max_identity_header_bytes: HEADER.len() - 1, ..DonorLimits::default() },
    ] {
        assert!(matches!(BondedDonor::new(&fixture.encoding, &fixture.protected, fixture.target(), &DEK,
            DonorId(7), &[DonorId(7)], limits, &mut verification, check), Err(DonorError::ObjectBudget)));
    }
    for roster in [vec![], vec![DonorId(0)], vec![DonorId(7), DonorId(7)], vec![DonorId(8)]] {
        assert!(matches!(BondedDonor::new(&fixture.encoding, &fixture.protected, fixture.target(), &DEK,
            DonorId(7), &roster, DonorLimits::default(), &mut verification, check), Err(DonorError::InvalidDonors)));
    }
    assert!(verification.is_empty());
    let exact = DonorLimits { max_protected_bytes: fixture.protected.len(),
        max_identity_header_bytes: HEADER.len(), ..DonorLimits::default() };
    fixture.donor(DonorId(7), &[DonorId(7)], exact);
}

#[test]
fn every_request_coordinate_and_fixed_donor_residue_is_checked() {
    let fixture = Fixture::new(3, 3);
    let mut donor = fixture.donor(DonorId(9), &[DonorId(7), DonorId(9), DonorId(11)], DonorLimits::default());
    let valid = fixture.request(9, 1, 1);
    let mut foreign_encoding = valid;
    foreign_encoding.encoding_id.0[0] ^= 1;
    for request in [
        PullRequest { donor: DonorId(7), ..valid },
        PullRequest { object_id: ObjectId([3; 32]), ..valid },
        foreign_encoding,
        PullRequest { source_block: 3, ..valid },
        PullRequest { esi: 0x0100_0000, ..valid },
    ] {
        assert_eq!(donor.respond(request, check), Err(DonorError::ForeignRequest));
    }
    assert_eq!(donor.respond(PullRequest { esi: 0, ..valid }, check), Err(DonorError::UnownedEsi));
    assert_eq!(donor.usage().requests, 6);
    assert_eq!(donor.usage().charged_wire_bytes, 0);
    assert_eq!(donor.usage().encoder_builds, 0);
    donor.respond(valid, check).unwrap();
}

#[test]
fn original_sources_need_no_repair_matrix_or_cache() {
    let fixture = Fixture::new(3, 3);
    let limits = DonorLimits { max_encoder_builds: 0, max_repair_source_symbols: 0,
        max_matrix_cells: 0, max_cached_encoder_bytes: 0, ..DonorLimits::default() };
    let mut donor = fixture.donor(DonorId(7), &[DonorId(7)], limits);
    for block in 0..3 {
        for esi in 0..fixture.sources(block).len() {
            donor.respond(fixture.request(7, block as u32, esi as u32), check).unwrap();
        }
    }
    assert_eq!(donor.usage().encoder_builds, 0);
    assert_eq!(donor.usage().cached_encoder_bytes, 0);
    assert_eq!(donor.respond(fixture.request(7, 0, 99), check), Err(DonorError::EncoderShapeBudget));
}

#[test]
fn request_and_record_byte_budgets_include_retries() {
    let fixture = Fixture::new(1, 1);
    let request = fixture.request(7, 0, 0);
    let mut donor = fixture.donor(DonorId(7), &[DonorId(7)],
        DonorLimits { max_requests: 2, ..DonorLimits::default() });
    let bytes = donor.respond(request, check).unwrap();
    assert_eq!(donor.respond(request, check).unwrap(), bytes);
    assert_eq!(donor.respond(request, check), Err(DonorError::RequestBudget));
    let mut donor = fixture.donor(DonorId(7), &[DonorId(7)],
        DonorLimits { max_wire_bytes: bytes.len() as u64, ..DonorLimits::default() });
    donor.respond(request, check).unwrap();
    assert_eq!(donor.respond(request, check), Err(DonorError::WireBudget));
    assert_eq!(donor.usage().requests, 2);
    assert_eq!(donor.usage().charged_wire_bytes, bytes.len() as u64);
}

#[test]
fn exact_encoder_shape_and_cache_admission_precede_builds() {
    let fixture = Fixture::new(3, 3);
    let k = fixture.sources(0).len();
    let p = SystematicParams::try_for_source_block(k, SIZE).unwrap();
    let charge = (k + p.l) * (SIZE + std::mem::size_of::<Vec<u8>>())
        + std::mem::size_of::<SystematicEncoder>();
    let exact = DonorLimits { max_matrix_cells: p.l * p.l,
        max_repair_source_symbols: k, max_cached_encoder_bytes: charge,
        ..DonorLimits::default() };
    let mut donor = fixture.donor(DonorId(7), &[DonorId(7)], exact);
    donor.respond(fixture.request(7, 0, 99), check).unwrap();
    assert_eq!(donor.usage().cached_encoder_bytes, charge);
    assert_eq!(donor.respond(fixture.request(7, 1, 99), check), Err(DonorError::EncoderCacheBudget));
    assert_eq!(donor.usage().encoder_builds, 1);
    for (limits, expected) in [
        (DonorLimits { max_matrix_cells: p.l * p.l - 1, ..exact }, DonorError::EncoderShapeBudget),
        (DonorLimits { max_repair_source_symbols: k - 1, ..exact }, DonorError::EncoderShapeBudget),
        (DonorLimits { max_cached_encoder_bytes: charge - 1, ..exact }, DonorError::EncoderCacheBudget),
    ] {
        let mut donor = fixture.donor(DonorId(7), &[DonorId(7)], limits);
        assert_eq!(donor.respond(fixture.request(7, 0, 99), check), Err(expected));
        assert_eq!(donor.usage().encoder_builds, 0);
    }
}

#[test]
fn explicit_cache_release_does_not_refund_build_or_wire_work() {
    let fixture = Fixture::new(2, 3);
    let mut donor = fixture.donor(DonorId(7), &[DonorId(7)],
        DonorLimits { max_encoder_builds: 2, ..DonorLimits::default() });
    let request = fixture.request(7, 0, 99);
    let first = donor.respond(request, check).unwrap();
    let before = donor.usage();
    assert!(donor.release_encoder(0));
    assert!(!donor.release_encoder(0));
    assert_eq!(donor.usage().encoder_builds, before.encoder_builds);
    assert_eq!(donor.usage().charged_wire_bytes, before.charged_wire_bytes);
    assert_eq!(donor.usage().cached_encoder_bytes, 0);
    assert_eq!(donor.respond(request, check).unwrap(), first);
    donor.release_encoder(0);
    assert_eq!(donor.respond(request, check), Err(DonorError::EncoderBuildBudget));
    donor.respond(fixture.request(7, 0, 0), check).unwrap();
}

#[test]
fn every_response_checkpoint_can_refuse_without_returning_partial_bytes() {
    let fixture = Fixture::new(3, 3);
    for esi in [0, 99] {
        let request = fixture.request(7, 1, esi);
        let mut donor = fixture.donor(DonorId(7), &[DonorId(7)], DonorLimits::default());
        let mut total = 0;
        let expected = donor.respond(request, || { total += 1; check() }).unwrap();
        for stop in 1..=total {
            let mut donor = fixture.donor(DonorId(7), &[DonorId(7)], DonorLimits::default());
            let mut calls = 0;
            assert_eq!(donor.respond(request, || {
                calls += 1;
                if calls == stop { Err("revoked/cancelled") } else { Ok(()) }
            }), Err(DonorError::Control("revoked/cancelled")));
            assert_eq!(calls, stop);
            let spent = donor.usage();
            assert_eq!(spent.requests, 1);
            assert_eq!(donor.respond(request, check).unwrap(), expected);
            assert!(donor.usage().encoder_builds >= spent.encoder_builds);
            assert!(donor.usage().charged_wire_bytes >= spent.charged_wire_bytes);
        }
    }
}

#[test]
fn interrupted_constructor_and_panicking_build_cannot_issue_or_reset_work() {
    let fixture = Fixture::new(3, 3);
    for stop in 1..=4 {
        let mut calls = 0;
        assert!(matches!(BondedDonor::new(&fixture.encoding, &fixture.protected, fixture.target(), &DEK,
            DonorId(7), &[DonorId(7)], DonorLimits::default(), &mut Vec::new(), || {
                calls += 1; if calls == stop { Err("stop") } else { Ok(()) }
            }), Err(DonorError::Control("stop"))));
    }
    let mut donor = fixture.donor(DonorId(7), &[DonorId(7)],
        DonorLimits { max_encoder_builds: 1, ..DonorLimits::default() });
    let mut calls = 0;
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = donor.respond(fixture.request(7, 0, 99), || {
            calls += 1; assert_ne!(calls, 3, "interrupt admitted encoder preparation"); check()
        });
    })).is_err());
    assert_eq!(donor.usage().encoder_builds, 1);
    assert_eq!(donor.usage().cached_blocks, 0);
    assert_eq!(donor.respond(fixture.request(7, 0, 99), check), Err(DonorError::EncoderBuildBudget));
    donor.respond(fixture.request(7, 0, 0), check).unwrap();
    let debug = format!("{donor:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains(&format!("{:?}", fixture.protected)));
    assert!(!debug.contains(&format!("{DEK:?}")));
}

#[test]
fn bonded_receiver_recovers_from_on_demand_surviving_donors_without_record_tables() {
    for blocks in [1, 3, 5] {
        let fixture = Fixture::new(blocks, 3);
        let roster = [DonorId(7), DonorId(9), DonorId(11)];
        let mut first = fixture.donor(roster[0], &roster, DonorLimits::default());
        let mut second = fixture.donor(roster[1], &roster, DonorLimits::default());
        let mut pull = BondedPull::new(&fixture.encoding, fixture.target(), &DEK,
            &roster, PullLimits::default()).unwrap();
        pull.donor_failed(roster[2]).unwrap();
        let mut recovered = None;
        for _ in 0..128 {
            for request in pull.schedule_bonded(12, 12).unwrap() {
                let donor = if request.donor == roster[0] { &mut first } else { &mut second };
                let record = donor.respond(request, check).unwrap();
                pull.accept_reply(request, &record, &mut Vec::new()).unwrap();
            }
            recovered = pull.try_recover(&mut Vec::new()).unwrap();
            if recovered.is_some() { break; }
        }
        assert_eq!(recovered.expect("healthy donors generate fresh repair equations").plaintext(), fixture.plaintext);
        assert!(first.usage().encoder_builds <= blocks as u32);
        assert!(second.usage().encoder_builds <= blocks as u32);
    }
}
