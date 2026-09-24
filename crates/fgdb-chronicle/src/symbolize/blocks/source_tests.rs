use super::*;
use crate::donor::{BondedDonor, DonorLimits};
use crate::identity::{CipherDescriptor, EncodingDescriptor, IdentifiedObject};
use crate::symbolize::{RecoveryTarget, decode_object};
use crate::transfer::{DonorId, PullRequest};
use fgdb_types::DatabaseSecurityNamespaceId;

const KEY: [u8; 32] = [0x39; 32];
const DEK: [u8; 32] = [0x63; 32];
const HEADER: &[u8] = b"systematic-source-recovery";

struct Fixture {
    encoding: EncodedObject,
    protected: Vec<u8>,
    plaintext: Vec<u8>,
    records: Vec<Vec<u8>>,
}
impl Fixture {
    fn new(total: usize, size: usize, blocks: usize, subblocks: usize, padding: usize) -> Self {
        let bytes = total * size - padding;
        assert!(bytes > 16);
        let plaintext: Vec<_> = (0..bytes - 16).map(|i| (i % 251) as u8).collect();
        let object = IdentifiedObject::new(
            &KEY,
            DatabaseSecurityNamespaceId([9; 32]),
            2,
            HEADER,
            &plaintext,
        );
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
                    object_nonce: [7; 24],
                    object_tag_len: 16,
                },
                &plaintext,
            )
            .unwrap();
        let encoding = protected.encode(EncodingDescriptor {
            fec_profile: 1,
            transfer_length: bytes as u64,
            oti_common: ((bytes as u64) << 24) | size as u64,
            oti_scheme: ((blocks as u32) << 24) | ((subblocks as u32) << 8) | 1,
            symbol_size: size as u16,
            source_block_count: blocks as u16,
            symbol_auth_profile: 1,
        });
        let protected = protected.protected_bytes().to_vec();
        assert_eq!(protected.len(), bytes);
        let mut records = Vec::new();
        // Independent sequential source walker; do not call the encoder or
        // Layout's materialize/copy helpers to manufacture the test input.
        let mut position = 0;
        for block in 0..blocks {
            let k = total / blocks + usize::from(block < total % blocks);
            let mut source = vec![Vec::new(); k];
            let n = if blocks == 1 { 1 } else { subblocks };
            for sub in 0..n {
                let width = size / n + usize::from(sub < size % n);
                for symbol in &mut source {
                    for _ in 0..width {
                        symbol.push(protected.get(position).copied().unwrap_or(0));
                        position += 1;
                    }
                }
            }
            for (esi, symbol) in source.into_iter().enumerate() {
                records.push(
                    SymbolRecord::for_encoding(&encoding, block as u32, esi as u32, 0, symbol)
                        .serialize(&encoding.symbol_auth_key(&DEK)),
                );
            }
        }
        Self {
            encoding,
            protected,
            plaintext,
            records,
        }
    }
    fn target(&self) -> RecoveryTarget<'static> {
        RecoveryTarget {
            k_oid: &KEY,
            namespace: DatabaseSecurityNamespaceId([9; 32]),
            object_id: self.encoding.object_id(),
            canonical_header: HEADER,
            protected_len: self.protected.len(),
        }
    }
    fn record(&self, bytes: &[u8]) -> SymbolRecord {
        SymbolRecord::verify(bytes, &self.encoding, &DEK, &mut Vec::new()).unwrap()
    }
    fn restore(
        &self,
        records: &[Vec<u8>],
        native: &mut Vec<u32>,
    ) -> Result<Vec<u8>, SymbolizeError> {
        decode_protected_observed(
            &self.encoding,
            records,
            self.protected.len(),
            &DEK,
            &mut Vec::new(),
            |block| native.push(block),
        )
    }
}

#[test]
fn complete_originals_restore_unequal_subblocks_without_any_native_decoder() {
    for (total, size, blocks, subblocks, padding) in [
        (17, 64, 1, 1, 7),
        (17, 64, 3, 3, 7),
        (31, 32, 5, 7, 3),
        (255, 4, 255, 1, 0),
    ] {
        let fixture = Fixture::new(total, size, blocks, subblocks, padding);
        let mut records = fixture.records.clone();
        records.reverse();
        records.push(records[0].clone()); // Exact authenticated duplicate, not another equation.
        let mut native = Vec::new();
        assert_eq!(
            fixture.restore(&records, &mut native).unwrap(),
            fixture.protected
        );
        assert!(native.is_empty());
        assert_eq!(
            decode_object(
                &fixture.encoding,
                &records,
                fixture.target(),
                &DEK,
                &mut Vec::new()
            )
            .unwrap(),
            fixture.plaintext
        );
    }
}

#[test]
fn complete_source_path_and_actual_foundation_decoder_agree_in_same_invocation() {
    let fixture = Fixture::new(17, 64, 3, 3, 7);
    let layout = Layout::new(&fixture.encoding, fixture.protected.len()).unwrap();
    let mut reference = vec![0; fixture.protected.len()];
    for number in 0..layout.blocks() {
        let block = layout.block(number as u32).unwrap();
        let decoder = InactivationDecoder::try_new(
            block.symbols,
            layout.symbol_size,
            code_seed(&fixture.encoding),
        )
        .unwrap();
        let mut received = decoder.constraint_symbols();
        for raw in &fixture.records {
            let record = fixture.record(raw);
            if record.source_block == number as u32 {
                received.push(ReceivedSymbol::source(record.esi, record.payload));
            }
        }
        // ubs:ignore -- actual foundation erasure decoder, not a JWT decoder.
        let decoded = decoder.decode(&received).unwrap();
        block.restore(&decoded.source, &mut reference).unwrap();
    }
    let mut native = Vec::new();
    assert_eq!(
        fixture.restore(&fixture.records, &mut native).unwrap(),
        reference
    );
    assert_eq!(reference, fixture.protected);
    assert!(native.is_empty());
}

#[test]
fn only_erased_blocks_use_native_reconstruction() {
    let fixture = Fixture::new(17, 64, 3, 3, 7);
    let layout = Layout::new(&fixture.encoding, fixture.protected.len()).unwrap();
    let mut records: Vec<_> = fixture
        .records
        .iter()
        .filter(|raw| {
            let record = fixture.record(raw);
            !(record.source_block == 1 && record.esi < 2)
        })
        .cloned()
        .collect();
    let k = layout.source_symbols(1).unwrap();
    let encoded = encode_block(&fixture.encoding, &fixture.protected, 1, 8, &DEK).unwrap();
    records.extend_from_slice(&encoded[k..]);
    let mut native = Vec::new();
    assert_eq!(
        fixture.restore(&records, &mut native).unwrap(),
        fixture.protected
    );
    assert_eq!(native, [1]);
    assert_eq!(
        decode_object(
            &fixture.encoding,
            &records,
            fixture.target(),
            &DEK,
            &mut Vec::new()
        )
        .unwrap(),
        fixture.plaintext
    );
}

#[test]
fn source_presence_not_equation_count_controls_the_shortcut() {
    for k in 1..=6 {
        for mask in 0..(1usize << (k + 2)) {
            let group: BTreeMap<_, _> = (0..k + 2)
                .filter(|esi| mask & (1 << esi) != 0)
                .map(|esi| (esi as u32, (0, Vec::new())))
                .collect();
            assert_eq!(complete_systematic(&group, k), mask == (1 << k) - 1);
        }
    }
    let fixture = Fixture::new(17, 64, 3, 3, 7);
    let mut records: Vec<_> = fixture
        .records
        .iter()
        .filter(|raw| fixture.record(raw).source_block != 2)
        .cloned()
        .collect();
    records.extend_from_slice(&fixture.records[..3]);
    let mut native = Vec::new();
    assert_eq!(
        fixture.restore(&records, &mut native),
        Err(SymbolizeError::InsufficientSymbols)
    );
    assert!(native.is_empty());
}

#[test]
fn trailing_bad_mac_and_authenticated_conflicts_are_not_skipped() {
    let fixture = Fixture::new(17, 64, 3, 3, 7);
    let mut records = fixture.records.clone();
    let mut bad = records[0].clone();
    *bad.last_mut().unwrap() ^= 1;
    records.push(bad);
    let mut native = Vec::new();
    assert!(matches!(
        fixture.restore(&records, &mut native),
        Err(SymbolizeError::Symbol(_))
    ));
    assert!(native.is_empty());
    records.pop();
    let mut conflict = fixture.record(&records[0]);
    conflict.payload[0] ^= 1;
    records.push(conflict.serialize(&fixture.encoding.symbol_auth_key(&DEK)));
    assert_eq!(
        fixture.restore(&records, &mut native),
        Err(SymbolizeError::DecodeFailed)
    );
    assert!(native.is_empty());
}

#[test]
fn direct_sources_still_require_zero_padding_and_whole_object_authentication() {
    let fixture = Fixture::new(17, 64, 3, 3, 7);
    let mut records = fixture.records.clone();
    let mut padding = fixture.record(records.last().unwrap());
    *padding.payload.last_mut().unwrap() = 1;
    *records.last_mut().unwrap() = padding.serialize(&fixture.encoding.symbol_auth_key(&DEK));
    let mut native = Vec::new();
    assert_eq!(
        fixture.restore(&records, &mut native),
        Err(SymbolizeError::DecodeFailed)
    );
    assert!(native.is_empty());

    let mut records = fixture.records.clone();
    let mut corrupt = fixture.record(&records[0]);
    corrupt.payload[0] ^= 1; // Authenticated but incorrect source; no duplicate coordinate.
    records[0] = corrupt.serialize(&fixture.encoding.symbol_auth_key(&DEK));
    assert!(matches!(
        decode_object(
            &fixture.encoding,
            &records,
            fixture.target(),
            &DEK,
            &mut Vec::new()
        ),
        Err(SymbolizeError::AuthenticationFailed | SymbolizeError::CiphertextIdentityMismatch)
    ));
    assert_eq!(
        decode_object(
            &fixture.encoding,
            &fixture.records,
            RecoveryTarget {
                namespace: DatabaseSecurityNamespaceId([8; 32]),
                ..fixture.target()
            },
            &DEK,
            &mut Vec::new()
        ),
        Err(SymbolizeError::IdentityMismatch)
    );
}

#[test]
fn surplus_repair_equations_retain_the_existing_native_validation_path() {
    let fixture = Fixture::new(17, 64, 3, 3, 7);
    let mut records = fixture.records.clone();
    let block = encode_block(&fixture.encoding, &fixture.protected, 0, 1, &DEK).unwrap();
    records.push(block.last().unwrap().clone());
    let mut native = Vec::new();
    assert_eq!(
        fixture.restore(&records, &mut native).unwrap(),
        fixture.protected
    );
    assert_eq!(native, [0]);
}

#[test]
fn thousands_of_original_symbols_round_trip_without_encoder_or_decoder_matrices() {
    let fixture = Fixture::new(4096, 1, 1, 1, 0);
    let mut donor = BondedDonor::new(
        &fixture.encoding,
        &fixture.protected,
        fixture.target(),
        &DEK,
        DonorId(7),
        &[DonorId(7)],
        DonorLimits {
            max_repair_source_symbols: 0,
            max_matrix_cells: 0,
            max_cached_encoder_bytes: 0,
            max_encoder_builds: 0,
            ..DonorLimits::default()
        },
        &mut Vec::new(),
        || Ok::<(), ()>(()),
    )
    .unwrap();
    let mut records = Vec::new();
    for esi in (0..4096).rev() {
        records.push(
            donor
                .respond(
                    PullRequest {
                        donor: DonorId(7),
                        object_id: fixture.encoding.object_id(),
                        encoding_id: fixture.encoding.encoding_id(),
                        source_block: 0,
                        esi,
                    },
                    || Ok::<(), ()>(()),
                )
                .unwrap(),
        );
    }
    assert_eq!(donor.usage().encoder_builds, 0);
    let protected = decode_protected_observed(
        &fixture.encoding,
        &records,
        fixture.protected.len(),
        &DEK,
        &mut Vec::new(),
        |_| panic!("complete original symbols must not allocate an erasure decoder"),
    )
    .unwrap();
    assert_eq!(protected, fixture.protected);
    assert_eq!(
        decode_object(
            &fixture.encoding,
            &records,
            fixture.target(),
            &DEK,
            &mut Vec::new()
        )
        .unwrap(),
        fixture.plaintext
    );
}
