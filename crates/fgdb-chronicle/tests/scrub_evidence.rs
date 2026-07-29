//! Scrub-evidence laws.
//!
//! The point of these tests is that the three verdicts are genuinely
//! DISTINGUISHABLE on real inputs. A scrub that can only say pass/fail cannot
//! report the state that matters most — corruption present, object still
//! recoverable — which is the window where re-encoding is cheap. If
//! `Degraded` were unreachable, the reporting would look fine and be useless.

use fgdb_chronicle::identity::{CipherDescriptor, EncodingDescriptor, IdentifiedObject};
use fgdb_chronicle::scrub::{LostReason, ScrubVerdict, scrub_object};
use fgdb_chronicle::symbolize::{RecoveryTarget, encode_object, source_symbol_count};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};

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

fn dek() -> [u8; 32] {
    core::array::from_fn(|i| (i as u8).wrapping_mul(11).wrapping_add(1))
}

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId(core::array::from_fn(|i| (i as u8) ^ 0x5a))
}

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

struct Fixture {
    encoding: fgdb_chronicle::EncodedObject,
    symbols: Vec<Vec<u8>>,
    protected_len: usize,
    object_id: ObjectId,
    source_count: usize,
}

fn fixture(repair_symbols: u32) -> Fixture {
    let plaintext = payload();
    let object = IdentifiedObject::new(K_OID, namespace(), OBJECT_KIND, b"hdr", &plaintext);
    let object_id = object.object_id();
    let protected = object.protect(&dek(), cipher_descriptor(), &plaintext);
    let protected_len = protected.protected_bytes().len();
    let encoding = protected.encode(EncodingDescriptor {
        fec_profile: 1,
        transfer_length: protected_len as u64,
        oti_common: 0x0001_0002_0003_0004,
        oti_scheme: 0x0005_0006,
        symbol_size: SYMBOL_SIZE,
        source_block_count: 1,
        symbol_auth_profile: 1,
    });
    let symbols = encode_object(
        &encoding,
        protected.protected_bytes(),
        OBJECT_KIND,
        0,
        repair_symbols,
        &dek(),
    )
    .expect("symbolization");
    Fixture {
        encoding,
        symbols,
        protected_len,
        object_id,
        source_count: source_symbol_count(protected_len, SYMBOL_SIZE),
    }
}

fn target(f: &Fixture) -> RecoveryTarget<'static> {
    RecoveryTarget {
        k_oid: K_OID,
        namespace: namespace(),
        object_id: f.object_id,
        canonical_header: b"hdr",
        protected_len: f.protected_len,
    }
}

/// Rot one byte of a symbol's payload — the shape real bit rot takes.
fn rot(symbol: &mut [u8], nth: usize) {
    let index = symbol.len() - 20 - nth;
    symbol[index] ^= 0x01;
}

#[test]
fn a_healthy_object_scrubs_intact_with_a_decode_proof() {
    let f = fixture(8);
    let report = scrub_object(&f.encoding, &f.symbols, target(&f), &dek());

    assert_eq!(report.verdict, ScrubVerdict::Intact);
    assert!(!report.needs_maintenance(), "intact needs no action");
    assert_eq!(report.corrupt_symbols(), 0);
    assert_eq!(report.symbols_authentic, f.symbols.len());
    assert_eq!(report.source_symbols, f.source_count);
    assert!(
        report.decode_proof_hash.is_some(),
        "a decode ran, so there must be attestable proof"
    );
}

/// THE MAINTENANCE WINDOW. Corruption present, object still recoverable —
/// the state a boolean cannot express and an operator most needs to see.
#[test]
fn corruption_within_the_budget_reports_degraded_not_failure() {
    let f = fixture(8);
    let mut symbols = f.symbols.clone();
    rot(&mut symbols[2], 0);
    rot(&mut symbols[5], 1);
    rot(&mut symbols[9], 2);

    let report = scrub_object(&f.encoding, &symbols, target(&f), &dek());

    assert_eq!(
        report.verdict,
        ScrubVerdict::Degraded {
            corrupt_symbols: 3,
            surviving_overhead: report.symbols_authentic - f.source_count,
        },
        "each rotted symbol is located by its MAC, and the remaining headroom \
         is reported so re-encoding can be sized"
    );
    assert!(report.needs_maintenance(), "degraded is actionable");
    assert_eq!(report.corrupt_symbols(), 3);
    assert!(report.decode_proof_hash.is_some());
}

/// Corruption is LOCATED, not merely detected: the count of failing symbols
/// tracks the number rotted, one for one, across the whole range.
#[test]
fn every_rotted_symbol_is_located_individually() {
    let f = fixture(12);
    for corrupt_count in 1..=6usize {
        let mut symbols = f.symbols.clone();
        for nth in 0..corrupt_count {
            rot(&mut symbols[nth], nth);
        }
        let report = scrub_object(&f.encoding, &symbols, target(&f), &dek());
        assert_eq!(
            report.corrupt_symbols(),
            corrupt_count,
            "MACs must locate exactly {corrupt_count} corruptions"
        );
        assert_eq!(report.symbols_authentic, f.symbols.len() - corrupt_count);
    }
}

/// Beyond the budget the scrub is LOST, with a typed reason so escalation is
/// chosen rather than guessed — and still no bytes are returned.
#[test]
fn corruption_beyond_the_budget_reports_lost_with_a_reason() {
    let f = fixture(2);
    let mut symbols = f.symbols.clone();
    // Rot far more symbols than two repair symbols can cover.
    for nth in 0..8usize {
        rot(&mut symbols[nth], nth);
    }

    let report = scrub_object(&f.encoding, &symbols, target(&f), &dek());
    assert_eq!(
        report.verdict,
        ScrubVerdict::Lost {
            reason: LostReason::InsufficientSymbols
        },
        "escalation should be replica/backup repair or rebuild-from-suffix"
    );
    assert!(report.needs_maintenance());
    assert_eq!(report.corrupt_symbols(), 8);
}

/// An object scrubbed against the wrong identity is Lost with the identity
/// reason — distinct from insufficiency, because the escalations differ:
/// one is "find more bytes", the other is "these are the wrong bytes".
#[test]
fn the_wrong_identity_is_lost_for_a_different_reason() {
    let f = fixture(8);
    let mut wrong = f.object_id;
    wrong.0[0] ^= 0x01;
    let report = scrub_object(
        &f.encoding,
        &f.symbols,
        RecoveryTarget {
            object_id: wrong,
            ..target(&f)
        },
        &dek(),
    );
    assert_eq!(
        report.verdict,
        ScrubVerdict::Lost {
            reason: LostReason::IdentityMismatch
        }
    );
    assert_eq!(
        report.corrupt_symbols(),
        0,
        "no symbol was corrupt; the object asked for was simply not this one"
    );
}

/// The proof hash is deterministic for a given symbol set, so two operators
/// scrubbing the same bytes produce the same attestation — that is what makes
/// it evidence rather than a log line.
#[test]
fn the_decode_proof_attestation_is_deterministic() {
    let f = fixture(8);
    let first = scrub_object(&f.encoding, &f.symbols, target(&f), &dek());
    let second = scrub_object(&f.encoding, &f.symbols, target(&f), &dek());
    assert_eq!(first.decode_proof_hash, second.decode_proof_hash);
    assert_eq!(first, second, "the whole report is deterministic");
}

/// Different survival patterns produce different proofs: the attestation
/// commits to HOW the decode went, not merely that it succeeded.
#[test]
fn different_symbol_sets_attest_differently() {
    let f = fixture(8);
    let intact = scrub_object(&f.encoding, &f.symbols, target(&f), &dek());

    let mut degraded_symbols = f.symbols.clone();
    rot(&mut degraded_symbols[1], 0);
    let degraded = scrub_object(&f.encoding, &degraded_symbols, target(&f), &dek());

    assert_ne!(
        intact.decode_proof_hash, degraded.decode_proof_hash,
        "a decode that had to route around a lost symbol is a different decode"
    );
}
