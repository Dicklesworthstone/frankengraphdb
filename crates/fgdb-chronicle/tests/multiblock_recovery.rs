#![cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
#[path = "support/multiblock.rs"]
mod support;

use fgdb_chronicle::symbol::{SymbolError, SymbolRecord};
use fgdb_chronicle::symbolize::{RecoveryTarget, SymbolizeError, decode_object, encode_object};
use support::{DEK, Fixture, HEADER, KEY, KIND, namespace};

fn recover(fixture: &Fixture, records: &[Vec<u8>]) -> Result<Vec<u8>, SymbolizeError> {
    decode_object(
        &fixture.encoding,
        records,
        fixture.target(),
        &DEK,
        &mut Vec::new(),
    )
}

#[test]
fn independent_blocks_and_subblocks_recover_with_short_final_symbols() {
    for blocks in [2, 3, 5] {
        for subs in [1, 3, 8] {
            let fixture = Fixture::new(blocks, subs, 8);
            let mut records = fixture.all();
            records.reverse();
            assert_eq!(recover(&fixture, &records).unwrap(), fixture.plaintext);
            for block in 0..usize::from(blocks) {
                let encoded = encode_object(
                    &fixture.encoding,
                    &fixture.protected,
                    KIND,
                    block as u32,
                    8,
                    &DEK,
                )
                .unwrap();
                assert_eq!(encoded, fixture.records[block]);
            }
        }
    }
}

#[test]
fn each_blocks_missing_source_uses_only_its_own_repair_equations() {
    let fixture = Fixture::new(3, 3, 12);
    for missing in 0..fixture.sources[0] {
        let records: Vec<_> = fixture
            .records
            .iter()
            .flat_map(|block| {
                block
                    .iter()
                    .enumerate()
                    .filter(|(esi, _)| *esi != missing)
                    .map(|(_, record)| record.clone())
            })
            .collect();
        assert_eq!(recover(&fixture, &records).unwrap(), fixture.plaintext);
    }
}

#[test]
fn surplus_and_duplicate_symbols_cannot_replace_a_missing_block() {
    let fixture = Fixture::new(3, 1, 24);
    let mut records = fixture.records[0].clone();
    records.extend(fixture.records[0].clone());
    records.extend(fixture.records[2].clone());
    assert!(records.len() > fixture.sources.iter().sum::<usize>());
    assert_eq!(
        recover(&fixture, &records),
        Err(SymbolizeError::InsufficientSymbols)
    );
}

#[test]
fn same_esi_in_different_blocks_is_not_a_duplicate() {
    let fixture = Fixture::new(3, 1, 0);
    let mut records = fixture.all();
    // Every block includes ESI zero; equal coordinates are duplicates only
    // within that block. Exact duplicate responses remain harmless.
    records.extend(fixture.records[1].clone());
    records.reverse();
    assert_eq!(recover(&fixture, &records).unwrap(), fixture.plaintext);
}

#[test]
fn all_records_authenticate_even_when_an_earlier_block_is_missing() {
    let fixture = Fixture::new(3, 1, 0);
    let mut records = fixture.records[2].clone();
    *records.last_mut().unwrap().last_mut().unwrap() ^= 1;
    assert!(matches!(
        recover(&fixture, &records),
        Err(SymbolizeError::Symbol(_))
    ));
}

#[test]
fn authenticated_conflicting_equations_and_foreign_blocks_fail_closed() {
    let fixture = Fixture::new(3, 3, 0);
    let key = fixture.encoding.symbol_auth_key(&DEK);
    let mut record = SymbolRecord::verify(
        &fixture.records[1][0],
        &fixture.encoding,
        &DEK,
        &mut Vec::new(),
    )
    .unwrap();
    record.payload[0] ^= 1;
    let mut records = fixture.all();
    records.push(record.serialize(&key));
    assert_eq!(
        recover(&fixture, &records),
        Err(SymbolizeError::DecodeFailed)
    );
    // A correctly MACed record naming a block past the object's partition is
    // refused while it is authenticated: SymbolRecord::verify checks
    // source_block against the descriptor's block count before anything else
    // consumes it, so decode never reaches its own block lookup.
    record.source_block = 3;
    assert_eq!(
        recover(&fixture, &[record.serialize(&key)]),
        Err(SymbolizeError::Symbol(SymbolError::InconsistentLengths))
    );
}

#[test]
fn substituted_block_labels_cannot_authenticate_as_the_original_object() {
    let fixture = Fixture::new(3, 1, 0);
    let key = fixture.encoding.symbol_auth_key(&DEK);
    let mut records = fixture.all();
    for raw in &mut records {
        let mut record =
            SymbolRecord::verify(raw, &fixture.encoding, &DEK, &mut Vec::new()).unwrap();
        if record.source_block < 2 {
            record.source_block = 1 - record.source_block;
        }
        *raw = record.serialize(&key);
    }
    assert!(recover(&fixture, &records).is_err());
}

#[test]
fn nonzero_padding_is_not_hidden_by_whole_object_truncation() {
    let fixture = Fixture::new(3, 1, 0);
    let key = fixture.encoding.symbol_auth_key(&DEK);
    let mut records = fixture.all();
    let raw = records.last_mut().unwrap();
    let mut record = SymbolRecord::verify(raw, &fixture.encoding, &DEK, &mut Vec::new()).unwrap();
    *record.payload.last_mut().unwrap() = 1;
    *raw = record.serialize(&key);
    assert_eq!(
        recover(&fixture, &records),
        Err(SymbolizeError::DecodeFailed)
    );
}

#[test]
fn complete_reassembly_still_requires_namespace_and_logical_identity() {
    let fixture = Fixture::new(3, 3, 0);
    let records = fixture.all();
    let mut wrong = fixture.target();
    wrong.object_id.0[0] ^= 1;
    assert_eq!(
        decode_object(&fixture.encoding, &records, wrong, &DEK, &mut Vec::new()),
        Err(SymbolizeError::IdentityMismatch)
    );
    wrong = fixture.target();
    wrong.namespace.0[0] ^= 1;
    assert_eq!(
        decode_object(&fixture.encoding, &records, wrong, &DEK, &mut Vec::new()),
        Err(SymbolizeError::IdentityMismatch)
    );
    wrong = fixture.target();
    wrong.protected_len -= 1;
    assert_eq!(
        decode_object(&fixture.encoding, &records, wrong, &DEK, &mut Vec::new()),
        Err(SymbolizeError::InvalidParameters)
    );
}

#[test]
fn oti_disagreement_and_impossible_shapes_fail_before_symbol_materialization() {
    for mutation in 0..11 {
        let (encoding, protected, _) = Fixture::object(7, 4093, 256, 3, 3, 4, |d| match mutation {
            0 => d.oti_common ^= 1 << 24,                  // F disagrees.
            1 => d.oti_common |= 1 << 16,                  // Reserved byte.
            2 => d.oti_common ^= 1,                        // T disagrees.
            3 => d.oti_scheme ^= 1 << 24,                  // Z disagrees.
            4 => d.oti_scheme &= !0x00ff_ff00,             // N = 0.
            5 => d.oti_scheme &= !0xff,                    // Al = 0.
            6 => d.oti_scheme = (3 << 24) | (3 << 8) | 3,  // T not divisible by Al.
            7 => d.oti_scheme = (3 << 24) | (65 << 8) | 4, // Empty sub-symbols.
            8 => d.source_block_count = 0,
            9 => d.source_block_count = 256,
            _ => d.transfer_length += 1,
        });
        assert_eq!(
            encode_object(&encoding, &protected, KIND, 0, 0, &DEK),
            Err(SymbolizeError::InvalidParameters)
        );
        let target = RecoveryTarget {
            k_oid: &KEY,
            namespace: namespace(),
            object_id: encoding.object_id(),
            canonical_header: HEADER,
            protected_len: protected.len(),
        };
        assert_eq!(
            decode_object(&encoding, &[], target, &DEK, &mut Vec::new()),
            Err(SymbolizeError::InvalidParameters)
        );
    }
}

#[test]
fn invalid_selected_blocks_and_esi_exhaustion_fail_before_encoding() {
    let fixture = Fixture::new(3, 1, 0);
    for block in [3, u32::MAX] {
        assert_eq!(
            encode_object(&fixture.encoding, &fixture.protected, KIND, block, 0, &DEK),
            Err(SymbolizeError::InvalidParameters)
        );
    }
    assert_eq!(
        encode_object(
            &fixture.encoding,
            &fixture.protected,
            KIND,
            0,
            0x0100_0000,
            &DEK
        ),
        Err(SymbolizeError::InvalidParameters)
    );
    // Passing an individual block slice where a complete object is required is
    // not an alternate interpretation of the same descriptor.
    assert_eq!(
        encode_object(
            &fixture.encoding,
            &fixture.protected[..256],
            KIND,
            0,
            0,
            &DEK
        ),
        Err(SymbolizeError::InvalidParameters)
    );
}

#[test]
fn source_ceiling_applies_per_block_not_to_the_whole_object() {
    for blocks in [1, 2] {
        let (encoding, protected, _) = Fixture::object(9, 56_388, 1, blocks, 1, 1, |_| {});
        let target = RecoveryTarget {
            k_oid: &KEY,
            namespace: namespace(),
            object_id: encoding.object_id(),
            canonical_header: HEADER,
            protected_len: protected.len(),
        };
        // No decoder matrix is allocated: one block is invalid, two valid
        // blocks are merely missing their input equations.
        let expected = if blocks == 1 {
            SymbolizeError::InvalidParameters
        } else {
            SymbolizeError::InsufficientSymbols
        };
        assert_eq!(
            decode_object(&encoding, &[], target, &DEK, &mut Vec::new()),
            Err(expected)
        );
    }
}

#[test]
fn one_block_encoded_bytes_keep_the_established_contiguous_interpretation() {
    let fixture = Fixture::with_shape(11, 4093, 256, 1, 1, 4, 0);
    for (esi, raw) in fixture.records[0].iter().enumerate() {
        let record = SymbolRecord::verify(raw, &fixture.encoding, &DEK, &mut Vec::new()).unwrap();
        let start = esi * 256;
        let length = (fixture.protected.len() - start).min(256);
        assert_eq!(
            &record.payload[..length],
            &fixture.protected[start..start + length]
        );
        assert!(record.payload[length..].iter().all(|byte| *byte == 0));
    }
    assert_eq!(
        recover(&fixture, &fixture.all()).unwrap(),
        fixture.plaintext
    );
}
