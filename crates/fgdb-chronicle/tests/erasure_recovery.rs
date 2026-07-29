//! Erasure-recovery laws: what survives loss, and what must never survive it.
//!
//! These tests exercise the whole durability path — protect, encode,
//! symbolize, lose symbols, recover, reopen, recompute identity — because
//! that composition is the actual product promise. RaptorQ itself is
//! asupersync's, already conformance-tested there; what is proved here is
//! Chronicle's contract around it:
//!
//!   * inside the repair budget, loss is invisible;
//!   * beyond it, recovery FAILS rather than returning partial or wrong bytes;
//!   * a forged or foreign symbol never enters the linear system;
//!   * recovered bytes must recompute the requested `ObjectId` (FG-INV-09).

use fgdb_chronicle::identity::{CipherDescriptor, EncodingDescriptor, IdentifiedObject};
use fgdb_chronicle::symbolize::{
    RecoveryTarget, SymbolizeError, decode_object, encode_object, source_symbol_count,
};
use fgdb_types::ids::DatabaseSecurityNamespaceId;

const SYMBOL_SIZE: u16 = 256;
const OBJECT_KIND: u16 = 0x0002;

static K_OID_BYTES: [u8; 32] = {
    let mut bytes = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        bytes[i] = ((i as u8).wrapping_mul(3)).wrapping_add(7);
        i += 1;
    }
    bytes
};
const K_OID: &[u8; 32] = &K_OID_BYTES;

fn k_oid() -> [u8; 32] {
    K_OID_BYTES
}

fn dek() -> [u8; 32] {
    core::array::from_fn(|i| (i as u8).wrapping_mul(11).wrapping_add(1))
}

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId(core::array::from_fn(|i| (i as u8) ^ 0x5a))
}

fn header() -> &'static [u8] {
    b"canonical-header"
}

/// 4 KiB of deterministic payload: 16 source symbols at 256 bytes, enough for
/// the loss patterns below to be meaningful.
fn payload() -> Vec<u8> {
    (0..4096u32).map(|i| (i % 251) as u8).collect()
}

fn cipher_descriptor() -> CipherDescriptor {
    CipherDescriptor {
        object_kind: OBJECT_KIND,
        canonical_plaintext_len: 4096,
        codec_profile: 1,
        compressed_len: 4096,
        data_crypto_profile: 1,
        dek_id: [9u8; 16],
        object_nonce: core::array::from_fn(|i| (i as u8).wrapping_mul(5).wrapping_add(3)),
        object_tag_len: 16,
    }
}

fn encoding_descriptor(protected_len: usize) -> EncodingDescriptor {
    EncodingDescriptor {
        fec_profile: 1,
        transfer_length: protected_len as u64,
        oti_common: 0x0001_0002_0003_0004,
        oti_scheme: 0x0005_0006,
        symbol_size: SYMBOL_SIZE,
        source_block_count: 1,
        symbol_auth_profile: 1,
    }
}

struct Fixture {
    encoding: fgdb_chronicle::EncodedObject,
    symbols: Vec<Vec<u8>>,
    protected_len: usize,
    object_id: fgdb_types::ids::ObjectId,
    plaintext: Vec<u8>,
    source_count: usize,
}

/// Protect, encode, and symbolize one object with `repair_symbols` of repair
/// overhead — the erasure budget, since the decoder supplies the code's own
/// constraint equations.
fn fixture(repair_symbols: u32) -> Fixture {
    let plaintext = payload();
    let object = IdentifiedObject::new(&k_oid(), namespace(), OBJECT_KIND, header(), &plaintext);
    let object_id = object.object_id();
    let protected = object.protect(&dek(), cipher_descriptor(), &plaintext);
    let protected_len = protected.protected_bytes().len();
    let encoding = protected.encode(encoding_descriptor(protected_len));
    let source_count = source_symbol_count(protected_len, SYMBOL_SIZE);
    let symbols = encode_object(
        &encoding,
        protected.protected_bytes(),
        OBJECT_KIND,
        0,
        repair_symbols,
        &dek(),
    )
    .expect("symbolization must succeed");
    Fixture {
        encoding,
        symbols,
        protected_len,
        object_id,
        plaintext,
        source_count,
    }
}

fn target(f: &Fixture) -> RecoveryTarget<'static> {
    RecoveryTarget {
        k_oid: K_OID,
        namespace: namespace(),
        object_id: f.object_id,
        canonical_header: header(),
        protected_len: f.protected_len,
    }
}

fn recover(f: &Fixture, symbols: &[Vec<u8>]) -> Result<Vec<u8>, SymbolizeError> {
    decode_object(&f.encoding, symbols, target(f), &dek())
}

#[test]
fn a_complete_symbol_set_recovers_the_object() {
    let f = fixture(8);
    assert_eq!(
        f.symbols.len(),
        f.source_count + 8,
        "every source symbol plus the repair budget"
    );
    let recovered = recover(&f, &f.symbols).expect("a complete set must recover");
    assert_eq!(recovered, f.plaintext, "recovery must be byte-exact");
}

/// THE DURABILITY PROMISE. Losing any single symbol — including a source
/// symbol, which is the case plain replication cannot survive — is invisible
/// as long as repair symbols remain.
#[test]
fn losing_any_single_symbol_is_invisible() {
    let f = fixture(8);
    for drop_index in 0..f.symbols.len() {
        let surviving: Vec<Vec<u8>> = f
            .symbols
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != drop_index)
            .map(|(_, symbol)| symbol.clone())
            .collect();
        let recovered = recover(&f, &surviving);
        assert!(
            recovered.is_ok(),
            "dropping symbol {drop_index} broke recovery: {recovered:?}"
        );
        assert_eq!(
            recovered.expect("asserted ok above"),
            f.plaintext,
            "recovery after dropping {drop_index}"
        );
    }
}

/// Losing several source symbols at once still recovers while repair symbols
/// cover the loss — the property that makes bit rot a maintenance event
/// rather than an outage.
#[test]
fn losing_multiple_source_symbols_recovers_within_the_budget() {
    let f = fixture(8);
    // Drop four source symbols; eight repair symbols remain available.
    let surviving: Vec<Vec<u8>> = f
        .symbols
        .iter()
        .enumerate()
        .filter(|(index, _)| !matches!(index, 0 | 3 | 7 | 11))
        .map(|(_, symbol)| symbol.clone())
        .collect();
    let recovered = recover(&f, &surviving).expect("four losses are inside an eight-symbol budget");
    assert_eq!(recovered, f.plaintext);
}

/// BEYOND THE BUDGET, FAIL CLOSED. With fewer surviving symbols than the code
/// needs, recovery must return an error — never partial bytes, never wrong
/// bytes. "Fail-closed beyond overhead" is the plan's own phrasing.
#[test]
fn beyond_the_repair_budget_recovery_fails_closed() {
    let f = fixture(2);
    // Keep far fewer symbols than K: no code can solve this.
    let surviving: Vec<Vec<u8>> = f.symbols.iter().take(f.source_count / 2).cloned().collect();
    let outcome = recover(&f, &surviving);
    assert!(
        matches!(
            outcome,
            Err(SymbolizeError::InsufficientSymbols)
                | Err(SymbolizeError::AuthenticationFailed)
                | Err(SymbolizeError::IdentityMismatch)
        ),
        "recovery beyond the budget must fail, got {outcome:?}"
    );
}

/// An empty symbol set is the degenerate loss case and must also fail closed
/// rather than returning an empty "object".
#[test]
fn no_symbols_recovers_nothing() {
    let f = fixture(4);
    assert!(recover(&f, &[]).is_err(), "no symbols must not recover");
}

/// A corrupted symbol never enters the linear system: it is rejected at
/// authentication, so it cannot perturb the recovered bytes. This is why
/// per-symbol MACs exist rather than a single whole-object checksum.
#[test]
fn a_corrupted_symbol_is_rejected_before_it_can_perturb_a_decode() {
    let f = fixture(8);
    let mut tampered = f.symbols.clone();
    let last = tampered[3].len() - 20;
    tampered[3][last] ^= 0x01;

    let outcome = recover(&f, &tampered);
    assert!(
        matches!(outcome, Err(SymbolizeError::Symbol(_))),
        "a corrupted symbol must be rejected as a symbol error, got {outcome:?}"
    );

    // Dropping the corrupted symbol entirely still recovers: corruption
    // degrades to erasure, which is exactly the design.
    let surviving: Vec<Vec<u8>> = f
        .symbols
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 3)
        .map(|(_, symbol)| symbol.clone())
        .collect();
    assert_eq!(
        recover(&f, &surviving).expect("corruption degrades to erasure"),
        f.plaintext
    );
}

/// A symbol from a DIFFERENT encoding of the same object is rejected as
/// foreign. Symbols from different `EncodingId`s never mix.
#[test]
fn a_symbol_from_another_encoding_cannot_join_a_decode() {
    let f = fixture(8);

    // Re-encode the same protected object under a different FEC profile.
    let plaintext = payload();
    let object = IdentifiedObject::new(&k_oid(), namespace(), OBJECT_KIND, header(), &plaintext);
    let protected = object.protect(&dek(), cipher_descriptor(), &plaintext);
    let other_encoding = protected.encode(EncodingDescriptor {
        fec_profile: 2,
        ..encoding_descriptor(f.protected_len)
    });
    let other_symbols = encode_object(
        &other_encoding,
        protected.protected_bytes(),
        OBJECT_KIND,
        0,
        8,
        &dek(),
    )
    .expect("second symbolization");

    let mut mixed: Vec<Vec<u8>> = f.symbols.clone();
    mixed[5] = other_symbols[5].clone();
    assert!(
        matches!(recover(&f, &mixed), Err(SymbolizeError::Symbol(_))),
        "a foreign-encoding symbol must be rejected"
    );
}

/// FG-INV-09 AS AN EXECUTABLE LAW: recovery is checked against the identity
/// that was asked for. Requesting a different `ObjectId` from a perfectly
/// valid symbol set must fail — content addressing means these bytes simply
/// are not that object.
#[test]
fn recovered_bytes_must_recompute_the_requested_identity() {
    let f = fixture(8);
    let mut wrong_id = f.object_id;
    wrong_id.0[0] ^= 0x01;

    let outcome = decode_object(
        &f.encoding,
        &f.symbols,
        RecoveryTarget {
            object_id: wrong_id,
            ..target(&f)
        },
        &dek(),
    );
    assert_eq!(
        outcome,
        Err(SymbolizeError::IdentityMismatch),
        "recovery must not hand back bytes for an identity they do not compute"
    );

    // The same law under a different namespace: the identity transcript binds
    // the namespace, so recovery into the wrong namespace is a mismatch.
    let other_ns = DatabaseSecurityNamespaceId(core::array::from_fn(|i| (i as u8) ^ 0xa5));
    assert_eq!(
        decode_object(
            &f.encoding,
            &f.symbols,
            RecoveryTarget {
                namespace: other_ns,
                ..target(&f)
            },
            &dek(),
        ),
        Err(SymbolizeError::IdentityMismatch)
    );
}

/// Symbolization is deterministic: the same encoding and bytes produce the
/// same symbol records, so a re-encode after a crash is idempotent rather
/// than a second, differently-coded copy.
#[test]
fn symbolization_is_deterministic() {
    let first = fixture(4);
    let second = fixture(4);
    assert_eq!(
        first.symbols, second.symbols,
        "the same object under the same encoding must symbolize identically"
    );
}
