//! The `SymbolRecord` authentication laws, each as an executable test.
//!
//! The load-bearing test is `mac_covers_every_serialized_header_field`: it
//! mutates each header field in the SERIALIZED bytes and requires
//! authentication to fail. A field the MAC does not cover is a field an
//! attacker edits freely, and the failure is silent — no test that only round
//! trips can see it.

use fgdb_chronicle::identity::{CipherDescriptor, EncodingDescriptor, IdentifiedObject};
use fgdb_chronicle::symbol::{HEADER_LEN_V1, SYMBOL_MAC_LEN_V1, SymbolError, SymbolRecord};
use fgdb_types::ids::DatabaseSecurityNamespaceId;

fn k_oid() -> [u8; 32] {
    core::array::from_fn(|i| (i as u8).wrapping_mul(3).wrapping_add(7))
}

fn dek() -> [u8; 32] {
    core::array::from_fn(|i| (i as u8).wrapping_mul(11).wrapping_add(1))
}

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId(core::array::from_fn(|i| (i as u8) ^ 0x5a))
}

fn cipher_descriptor() -> CipherDescriptor {
    CipherDescriptor {
        object_kind: 0x0002,
        canonical_plaintext_len: 512,
        codec_profile: 1,
        compressed_len: 512,
        data_crypto_profile: 1,
        dek_id: [9u8; 16],
        object_nonce: core::array::from_fn(|i| (i as u8).wrapping_mul(5).wrapping_add(3)),
        object_tag_len: 16,
    }
}

fn encoding_descriptor(fec_profile: u16) -> EncodingDescriptor {
    EncodingDescriptor {
        fec_profile,
        transfer_length: 4096,
        oti_common: 0x0001_0002_0003_0004,
        oti_scheme: 0x0005_0006,
        symbol_size: 1280,
        source_block_count: 4,
        symbol_auth_profile: 1,
    }
}

fn encoded(fec_profile: u16) -> fgdb_chronicle::EncodedObject {
    let payload: Vec<u8> = (0..512u32).map(|i| (i % 251) as u8).collect();
    let object = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, b"header", &payload);
    let protected = object
        .protect(&dek(), cipher_descriptor(), &payload)
        .expect("registered AEAD profile");
    protected.encode(encoding_descriptor(fec_profile))
}

fn symbol_payload() -> Vec<u8> {
    // The wire model admits exactly `symbol_size` payloads: the encoder
    // zero-pads every source symbol to the descriptor's size and asupersync's
    // decoder refuses anything else wholesale, so a test record must honour
    // the same law the production encoder does.
    (0..1280u32).map(|i| (i % 241) as u8).collect()
}

fn record(encoding: &fgdb_chronicle::EncodedObject) -> SymbolRecord {
    SymbolRecord::for_encoding(encoding, 1, 42, 0, symbol_payload())
}

#[test]
fn round_trips_through_serialize_and_verify() {
    let encoding = encoded(1);
    let original = record(&encoding);
    let bytes = original.serialize(&encoding.symbol_auth_key(&dek()));
    assert_eq!(bytes.len(), original.record_len() as usize);

    let parsed =
        SymbolRecord::verify(&bytes, &encoding, &dek(), &mut Vec::new()).expect("authentic record");
    assert_eq!(parsed, original, "verify must reconstruct the exact record");
    assert_eq!(parsed.payload, symbol_payload());
}

#[test]
fn symbol_record_fixed_width_fields_are_little_endian() {
    let encoding = encoded(1);
    let bytes = record(&encoding).serialize(&encoding.symbol_auth_key(&dek()));

    assert_eq!(&bytes[4..6], &1u16.to_le_bytes());
    assert_eq!(&bytes[6..8], &HEADER_LEN_V1.to_le_bytes());
    assert_eq!(&bytes[8..12], &(bytes.len() as u32).to_le_bytes());
    assert_eq!(&bytes[108..110], &0x0002u16.to_le_bytes());
    assert_eq!(&bytes[110..114], &1u32.to_le_bytes());
    assert_eq!(&bytes[114..118], &42u32.to_le_bytes());
}

/// THE WIRE-SHAPE LAW. Every symbol on the wire is exactly `symbol_size` —
/// the encoder zero-pads to it and asupersync's decoder refuses anything
/// else wholesale — so a SHORT-but-authentic payload must be rejected here
/// rather than admitted to poison the entire decode downstream.
#[test]
fn a_short_payload_is_refused_even_when_authentic() {
    let encoding = encoded(1);
    let short = SymbolRecord::for_encoding(&encoding, 1, 42, 0, vec![0u8; 1279]);
    let bytes = short.serialize(&encoding.symbol_auth_key(&dek()));
    assert_eq!(
        SymbolRecord::verify(&bytes, &encoding, &dek(), &mut Vec::new()),
        Err(SymbolError::InconsistentLengths),
        "a payload shorter than symbol_size is damage, not a shape to admit"
    );
}

/// THE TRANSCRIPT-TOTALITY LAW. Flip one bit at every byte offset of the
/// serialized header and require authentication to fail at each. A surviving
/// offset is a header field outside the MAC transcript.
#[test]
fn mac_covers_every_serialized_header_field() {
    let encoding = encoded(1);
    let bytes = record(&encoding).serialize(&encoding.symbol_auth_key(&dek()));

    for offset in 0..usize::from(HEADER_LEN_V1) {
        let mut corrupted = bytes.clone();
        corrupted[offset] ^= 0x01;
        let outcome = SymbolRecord::verify(&corrupted, &encoding, &dek(), &mut Vec::new());
        assert!(
            outcome.is_err(),
            "header byte {offset} is outside the MAC transcript or the framing checks"
        );
    }
}

/// The payload and the MAC itself are covered too — the whole record.
#[test]
fn mac_covers_the_payload_and_itself() {
    let encoding = encoded(1);
    let bytes = record(&encoding).serialize(&encoding.symbol_auth_key(&dek()));
    let payload_start = usize::from(HEADER_LEN_V1);

    for offset in payload_start..bytes.len() {
        let mut corrupted = bytes.clone();
        corrupted[offset] ^= 0x01;
        assert_eq!(
            SymbolRecord::verify(&corrupted, &encoding, &dek(), &mut Vec::new()),
            Err(SymbolError::AuthenticationFailed),
            "flipping byte {offset} (payload or MAC) must fail authentication"
        );
    }
}

/// A BARE RECORD IS NEVER SELF-AUTHORIZING: verification is only reachable
/// with the authenticated encoding, and a record from another encoding is
/// rejected as foreign — even though it is perfectly well-formed and its own
/// MAC is valid under its own key.
#[test]
fn a_record_from_another_encoding_is_rejected_as_foreign() {
    let first = encoded(1);
    let second = encoded(2);
    let bytes = record(&second).serialize(&second.symbol_auth_key(&dek()));

    // Well-formed and authentic under its OWN encoding...
    assert!(SymbolRecord::verify(&bytes, &second, &dek(), &mut Vec::new()).is_ok());
    // ...and foreign under another, so symbols from different EncodingIds
    // can never mix inside one decode.
    assert_eq!(
        SymbolRecord::verify(&bytes, &first, &dek(), &mut Vec::new()),
        Err(SymbolError::ForeignEncoding)
    );
}

/// `SymbolRecord::for_encoding` derives its kind from the authenticated
/// cipher descriptor. A record with a deliberately changed-but-re-MACed kind
/// is therefore still foreign to that encoding.
#[test]
fn a_mac_valid_wrong_kind_record_is_rejected_as_foreign() {
    let encoding = encoded(1);
    let mut wrong_kind = record(&encoding);
    wrong_kind.object_kind ^= 0x0001;
    let bytes = wrong_kind.serialize(&encoding.symbol_auth_key(&dek()));

    assert_eq!(
        SymbolRecord::verify(&bytes, &encoding, &dek(), &mut Vec::new()),
        Err(SymbolError::ForeignEncoding)
    );
}

/// The per-encoding key is what makes the previous law cryptographic rather
/// than merely a field comparison: the same bytes under the other encoding's
/// key do not authenticate.
#[test]
fn symbol_keys_do_not_transfer_between_encodings() {
    let first = encoded(1);
    let second = encoded(2);
    let mut forged = record(&first);
    // Relabel a first-encoding symbol as belonging to the second.
    forged.encoding_id = second.encoding_id();
    let bytes = forged.serialize(&first.symbol_auth_key(&dek()));

    assert_eq!(
        SymbolRecord::verify(&bytes, &second, &dek(), &mut Vec::new()),
        Err(SymbolError::AuthenticationFailed),
        "a relabelled symbol must not authenticate under the target encoding's key"
    );
}

/// The wrong DEK yields the wrong K_symbol yields no authentication.
#[test]
fn the_wrong_dek_cannot_authenticate() {
    let encoding = encoded(1);
    let bytes = record(&encoding).serialize(&encoding.symbol_auth_key(&dek()));
    let mut other_dek = dek();
    other_dek[0] ^= 0xff;
    assert_eq!(
        SymbolRecord::verify(&bytes, &encoding, &other_dek, &mut Vec::new()),
        Err(SymbolError::AuthenticationFailed)
    );
}

/// Truncation at every length short of the full record fails closed, and a
/// record whose declared length disagrees with its own fields is rejected
/// before any payload slice is taken.
#[test]
fn truncated_and_inconsistent_records_fail_closed() {
    let encoding = encoded(1);
    let bytes = record(&encoding).serialize(&encoding.symbol_auth_key(&dek()));

    for cut in 0..bytes.len() {
        assert!(
            SymbolRecord::verify(&bytes[..cut], &encoding, &dek(), &mut Vec::new()).is_err(),
            "a record truncated to {cut} bytes must fail closed"
        );
    }

    // record_len that does not match header + payload + MAC.
    let mut lying = bytes.clone();
    let bad_len = (bytes.len() as u32 + 64).to_le_bytes();
    lying[8..12].copy_from_slice(&bad_len);
    assert_eq!(
        SymbolRecord::verify(&lying, &encoding, &dek(), &mut Vec::new()),
        Err(SymbolError::InconsistentLengths)
    );

    // A symbol_len larger than the encoding's symbol_size is rejected on the
    // descriptor bound, not merely on the MAC.
    let mut oversized = record(&encoding);
    oversized.payload = vec![0u8; usize::from(encoding.descriptor().symbol_size) + 1];
    let oversized_bytes = oversized.serialize(&encoding.symbol_auth_key(&dek()));
    assert_eq!(
        SymbolRecord::verify(&oversized_bytes, &encoding, &dek(), &mut Vec::new()),
        Err(SymbolError::InconsistentLengths)
    );

    // A valid authenticated record is still one exact record, not an
    // authenticated prefix followed by bytes outside its MAC transcript.
    let mut suffixed = bytes.clone();
    suffixed.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(
        SymbolRecord::verify(&suffixed, &encoding, &dek(), &mut Vec::new()),
        Err(SymbolError::InconsistentLengths)
    );
}

/// Unknown framing is rejected, never ignored: a future version, a foreign
/// magic, or an unsupported MAC profile must not silently decode.
#[test]
fn unknown_framing_is_rejected_not_ignored() {
    let encoding = encoded(1);
    let bytes = record(&encoding).serialize(&encoding.symbol_auth_key(&dek()));

    let mut wrong_magic = bytes.clone();
    wrong_magic[0] = b'X';
    assert_eq!(
        SymbolRecord::verify(&wrong_magic, &encoding, &dek(), &mut Vec::new()),
        Err(SymbolError::UnsupportedFraming)
    );

    let mut future_version = bytes.clone();
    future_version[4..6].copy_from_slice(&2u16.to_le_bytes());
    assert_eq!(
        SymbolRecord::verify(&future_version, &encoding, &dek(), &mut Vec::new()),
        Err(SymbolError::UnsupportedFraming)
    );

    let mut wrong_mac_profile = bytes.clone();
    let profile_offset = usize::from(HEADER_LEN_V1) - 4;
    wrong_mac_profile[profile_offset..profile_offset + 2].copy_from_slice(&9u16.to_le_bytes());
    assert_eq!(
        SymbolRecord::verify(&wrong_mac_profile, &encoding, &dek(), &mut Vec::new()),
        Err(SymbolError::UnsupportedFraming)
    );

    let mut wrong_mac_len = bytes.clone();
    let len_offset = usize::from(HEADER_LEN_V1) - 2;
    wrong_mac_len[len_offset..len_offset + 2].copy_from_slice(&32u16.to_le_bytes());
    assert_eq!(
        SymbolRecord::verify(&wrong_mac_len, &encoding, &dek(), &mut Vec::new()),
        Err(SymbolError::UnsupportedFraming)
    );
}

/// A source block outside the encoding's declared block count is rejected:
/// the descriptor bounds the record, not the other way round.
#[test]
fn a_symbol_outside_the_declared_block_count_is_rejected() {
    let encoding = encoded(1);
    let mut out_of_range = record(&encoding);
    out_of_range.source_block = u32::from(encoding.descriptor().source_block_count);
    let bytes = out_of_range.serialize(&encoding.symbol_auth_key(&dek()));
    assert_eq!(
        SymbolRecord::verify(&bytes, &encoding, &dek(), &mut Vec::new()),
        Err(SymbolError::InconsistentLengths)
    );
}

/// Symbols never reuse the object AEAD nonce: the symbol MAC key is derived
/// from the DEK and EncodingId, and the record carries no nonce field at all.
#[test]
fn symbol_records_carry_no_aead_nonce() {
    let encoding = encoded(1);
    let bytes = record(&encoding).serialize(&encoding.symbol_auth_key(&dek()));
    let nonce = cipher_descriptor().object_nonce;
    assert!(
        !bytes.windows(nonce.len()).any(|window| window == nonce),
        "the object AEAD nonce must never appear in a symbol record"
    );
    assert_eq!(
        usize::from(HEADER_LEN_V1) + symbol_payload().len() + usize::from(SYMBOL_MAC_LEN_V1),
        bytes.len(),
        "the record is exactly header + payload + MAC; there is no room for a nonce"
    );
}
