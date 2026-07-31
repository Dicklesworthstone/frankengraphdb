//! Laws of the durable capsule container.
//!
//! Doctrine 5's claim is that erasure coding is what lets this database have no
//! double-write journal: a torn or corrupt capsule *heals* rather than needing
//! something to roll back to. That claim is only worth making if the healing is
//! measured, so this file measures it in both directions — recovery up to the
//! budget, and **fail-closed one symbol past it**. A store that recovered up to
//! its budget and returned partial bytes beyond it would be worse than one that
//! never coded anything.
//!
//! The other half is that a rewritten container cannot redirect recovery. The
//! file describes itself; the commit *stream* says which object it must be. A
//! header that disagrees can only fail.

use fgdb_chronicle::IdentifiedObject;
use fgdb_chronicle::capsule::{
    CAPSULE_MAGIC, CapsuleError, CapsuleProfile, decode_container, encode_container, recover, seal,
};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};

const K_OID: [u8; 32] = [0x5a; 32];
const DEK: [u8; 32] = [0x3c; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const KIND: u16 = 0x0274;

fn plaintext() -> Vec<u8> {
    // Several symbols' worth, so erasure has something to work with.
    (0..2000u32).map(|i| (i % 251) as u8).collect()
}

fn profile() -> CapsuleProfile {
    CapsuleProfile::balanced()
}

fn sealed() -> fgdb_chronicle::capsule::SealedCapsule {
    seal(&K_OID, NAMESPACE, &DEK, KIND, &plaintext(), profile()).expect("seals")
}

fn recover_from(
    symbols: &[Vec<u8>],
    descriptor: &fgdb_chronicle::capsule::CapsuleDescriptor,
    object_id: ObjectId,
) -> Result<Vec<u8>, CapsuleError> {
    recover(descriptor, symbols, object_id, &K_OID, NAMESPACE, &DEK)
}

// ---------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------

#[test]
fn a_capsule_round_trips_through_its_container() {
    let capsule = sealed();
    let bytes = encode_container(&capsule);
    assert_eq!(&bytes[..4], &CAPSULE_MAGIC);

    let (descriptor, symbols) = decode_container(&bytes).expect("decodes");
    assert_eq!(descriptor, capsule.descriptor);
    assert_eq!(symbols, capsule.symbols);

    let recovered = recover_from(&symbols, &descriptor, capsule.object_id).expect("recovers");
    assert_eq!(
        recovered,
        plaintext(),
        "the plaintext survives the round trip"
    );
}

/// Sealing the same plaintext twice must produce identical bytes, or a
/// content-addressed store would hold two encodings of one object and
/// deduplication could never fire.
#[test]
fn sealing_is_deterministic() {
    let a = sealed();
    let b = sealed();
    assert_eq!(a.object_id, b.object_id);
    assert_eq!(a.descriptor, b.descriptor);
    assert_eq!(a.symbols, b.symbols);
    assert_eq!(encode_container(&a), encode_container(&b));
}

/// The identity is derived from the plaintext, not accepted from a caller, so
/// different content is a different object.
#[test]
fn different_plaintext_is_a_different_object() {
    let a = sealed();
    let mut other = plaintext();
    other[0] ^= 0x01;
    let b = seal(&K_OID, NAMESPACE, &DEK, KIND, &other, profile()).expect("seals");
    assert_ne!(a.object_id, b.object_id);
}

// ---------------------------------------------------------------------------
// THE ERASURE CLAIM, measured in both directions
// ---------------------------------------------------------------------------

/// Losing any `repair_symbols` symbols still recovers. Swept over the budget
/// rather than tested at one point, because "it survived losing 3" says nothing
/// about 8.
#[test]
fn losing_up_to_the_budget_still_recovers() {
    let capsule = sealed();
    let budget = profile().erasure_budget();
    assert!(budget > 0, "a zero budget would make this test vacuous");

    // Failures are collected so the sweep reports EVERY loss count that
    // misbehaves. Which counts fail is the diagnostic: "the first one" cannot
    // distinguish a budget that is off by one from a code that never heals.
    let mut failures: Vec<String> = Vec::new();
    for lost in 1..=budget {
        let surviving: Vec<Vec<u8>> = capsule.symbols[lost..].to_vec();
        match recover_from(&surviving, &capsule.descriptor, capsule.object_id) {
            Ok(recovered) if recovered == plaintext() => {}
            Ok(_) => failures.push(format!("losing {lost}: recovered the wrong bytes")),
            Err(error) => failures.push(format!("losing {lost}: {error}")),
        }
    }
    assert!(
        failures.is_empty(),
        "every loss up to the {budget}-symbol budget must recover; {} of {budget} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// One symbol past the budget fails CLOSED. Returning partial or wrong bytes
/// here would be far worse than failing: recovery would look like it worked.
#[test]
fn losing_one_more_than_the_budget_fails_closed() {
    let capsule = sealed();
    let budget = profile().erasure_budget();
    let surviving: Vec<Vec<u8>> = capsule.symbols[budget + 1..].to_vec();
    let result = recover_from(&surviving, &capsule.descriptor, capsule.object_id);
    assert!(
        result.is_err(),
        "recovery beyond the erasure budget must fail rather than return bytes"
    );
}

/// A corrupt symbol costs the same as a lost one, because every symbol carries
/// a MAC under a per-encoding key: it is refused BEFORE it can enter the linear
/// system, so it subtracts from the budget instead of poisoning the result.
///
/// This is the property that makes bit rot survivable rather than merely
/// detectable, and it is the one doctrine 5 leans on when it says no journal is
/// needed.
#[test]
fn corrupt_symbols_cost_the_same_as_lost_ones() {
    let capsule = sealed();
    let budget = profile().erasure_budget();

    let mut damaged = capsule.symbols.clone();
    for symbol in damaged.iter_mut().take(budget) {
        let midpoint = symbol.len() / 2;
        symbol[midpoint] ^= 0xff;
    }
    let recovered =
        recover_from(&damaged, &capsule.descriptor, capsule.object_id).expect("heals corruption");
    assert_eq!(
        recovered,
        plaintext(),
        "corrupting the budget's worth of symbols must still recover"
    );

    // And one past the budget fails closed, exactly as loss does.
    let mut over = capsule.symbols.clone();
    for symbol in over.iter_mut().take(budget + 1) {
        let midpoint = symbol.len() / 2;
        symbol[midpoint] ^= 0xff;
    }
    assert!(
        recover_from(&over, &capsule.descriptor, capsule.object_id).is_err(),
        "corruption beyond the budget must fail closed"
    );
}

// ---------------------------------------------------------------------------
// A rewritten container cannot redirect recovery
// ---------------------------------------------------------------------------

/// The declared `EncodingId` must recompute from the declared descriptors. A
/// rewritten frame can only fail; it cannot point recovery at other bytes.
#[test]
fn a_rewritten_encoding_id_is_refused() {
    let capsule = sealed();
    let mut descriptor = capsule.descriptor.clone();
    descriptor.encoding_id[0] ^= 0xff;
    assert!(
        matches!(
            recover_from(&capsule.symbols, &descriptor, capsule.object_id),
            Err(CapsuleError::DescriptorMismatch(_))
        ),
        "an EncodingId that is not the digest of its own descriptor must be refused"
    );
}

/// Changing a coding parameter changes the EncodingId, so the same tamper-check
/// catches it — no separate rule needed.
#[test]
fn a_rewritten_symbol_size_is_refused() {
    let capsule = sealed();
    let mut descriptor = capsule.descriptor.clone();
    descriptor.symbol_size = descriptor.symbol_size.wrapping_add(16);
    assert!(matches!(
        recover_from(&capsule.symbols, &descriptor, capsule.object_id),
        Err(CapsuleError::DescriptorMismatch(_))
    ));
}

/// The DECODE SIZE must be authenticated. `transfer_length` fixes K, and it
/// lives in the EncodingId transcript, so tampering with it is refused before
/// any decoder is constructed.
///
/// This law exists because the container used to carry a SECOND copy of this
/// length, `protected_len`, that the transcript did not cover — so a container
/// claiming `u64::MAX` reached `k = len.div_ceil(symbol_size)` unchecked. The
/// existing encoding_id and symbol_size laws both passed throughout, because
/// those fields were in the transcript; this is the field that was not.
#[test]
fn a_rewritten_transfer_length_is_refused() {
    let capsule = sealed();
    let mut descriptor = capsule.descriptor.clone();
    assert_eq!(
        descriptor.protected_len(),
        descriptor.transfer_length,
        "the decode length IS the authenticated transfer length"
    );

    descriptor.transfer_length = u64::MAX;
    assert!(
        matches!(
            recover_from(&capsule.symbols, &descriptor, capsule.object_id),
            Err(CapsuleError::DescriptorMismatch(_))
        ),
        "an unauthenticated decode size would let a container name its own \
         allocation"
    );

    // A smaller lie is refused by the same check — the guard is the transcript,
    // not a magnitude heuristic.
    let mut smaller = capsule.descriptor.clone();
    smaller.transfer_length = smaller.transfer_length.saturating_sub(1);
    assert!(matches!(
        recover_from(&capsule.symbols, &smaller, capsule.object_id),
        Err(CapsuleError::DescriptorMismatch(_))
    ));
}

/// A self-consistent descriptor is not necessarily authentic. `EncodingId` is
/// an unkeyed digest, so an attacker can rewrite `transfer_length` and
/// recompute the ID without possessing the DEK. Recovery must reject an
/// unsupported RFC 6330 source-block size as data, not let the infallible
/// decoder constructor panic the process.
#[test]
fn a_self_consistent_oversized_transfer_length_fails_without_panicking() {
    let capsule = sealed();
    let mut descriptor = capsule.descriptor.clone();
    descriptor.transfer_length = 56_404 * u64::from(descriptor.symbol_size);

    let identified = IdentifiedObject::new(&K_OID, NAMESPACE, KIND, &[], &plaintext());
    let protected = identified.protect(&DEK, descriptor.cipher_descriptor(), &plaintext());
    assert_eq!(
        protected.ciphertext_id().0,
        descriptor.ciphertext_id,
        "the control must preserve the capsule's authenticated ciphertext"
    );
    descriptor.encoding_id = protected
        .encode(descriptor.encoding_descriptor())
        .encoding_id()
        .0;

    assert!(matches!(
        recover_from(&[], &descriptor, capsule.object_id),
        Err(CapsuleError::Recovery(
            fgdb_chronicle::symbolize::SymbolizeError::InvalidParameters
        ))
    ));
}

/// THE ENCODER'S OWN BOUNDARY. The decoder side above refuses an oversized
/// source block as data (fgdb-raptorq-decoder-boundary-panic-hpjb). The
/// ENCODER owes the same law: past RFC 6330's systematic-table bound
/// (K = 56403), asupersync's parameter builder panics. The encoder must
/// validate K before materializing K separately allocated source buffers.
#[test]
fn an_oversized_source_block_is_refused_before_symbol_materialization() {
    // One-byte symbols make a 56,404-byte block one past the bound.
    let oversized: Vec<u8> = (0..56_404u32).map(|i| (i % 251) as u8).collect();
    let capsule = sealed();
    let mut descriptor = capsule.descriptor.clone();
    descriptor.symbol_size = 1;
    descriptor.transfer_length = oversized.len() as u64;

    let identified = IdentifiedObject::new(&K_OID, NAMESPACE, KIND, &[], &plaintext());
    let protected = identified.protect(&DEK, descriptor.cipher_descriptor(), &plaintext());
    let encoding = protected.encode(descriptor.encoding_descriptor());

    assert_eq!(
        fgdb_chronicle::symbolize::encode_object(&encoding, &oversized, KIND, 0, 4, &DEK),
        Err(fgdb_chronicle::symbolize::SymbolizeError::InvalidParameters),
        "a source block past the systematic-table bound must fail in preflight"
    );
}

/// The declared repair budget is fixed by the authenticated `fec_profile`.
/// Rewriting the redundant count must therefore fail rather than changing a
/// durability decision without changing the `EncodingId`.
#[test]
fn a_rewritten_repair_symbols_is_refused() {
    let capsule = sealed();
    let mut descriptor = capsule.descriptor.clone();
    descriptor.repair_symbols = descriptor.repair_symbols.wrapping_add(1);
    let declared_repair_symbols = descriptor.repair_symbols;
    let registered_repair_symbols = capsule.descriptor.repair_symbols;
    assert!(
        matches!(
            recover_from(&capsule.symbols, &descriptor, capsule.object_id),
            Err(CapsuleError::RepairBudgetMismatch {
                fec_profile,
                declared_repair_symbols: declared,
                registered_repair_symbols: registered,
            }) if fec_profile == capsule.descriptor.fec_profile
                && declared == declared_repair_symbols
                && registered == registered_repair_symbols
        ),
        "a repair-symbol count that disagrees with the authenticated FEC profile \
         must be refused"
    );
}

/// Recomputing the `EncodingId` does not let a frame invent a new repair
/// policy. The profile registry is closed independently of descriptor
/// self-consistency.
#[test]
fn a_self_consistent_unregistered_fec_profile_is_refused() {
    let capsule = sealed();
    let mut descriptor = capsule.descriptor.clone();
    descriptor.fec_profile = 2;

    let identified = IdentifiedObject::new(&K_OID, NAMESPACE, KIND, &[], &plaintext());
    let protected = identified.protect(&DEK, descriptor.cipher_descriptor(), &plaintext());
    assert_eq!(
        protected.ciphertext_id().0,
        descriptor.ciphertext_id,
        "the control must preserve the capsule's authenticated ciphertext"
    );
    descriptor.encoding_id = protected
        .encode(descriptor.encoding_descriptor())
        .encoding_id()
        .0;

    assert!(matches!(
        recover_from(&capsule.symbols, &descriptor, capsule.object_id),
        Err(CapsuleError::UnsupportedFecProfile { fec_profile: 2 })
    ));
}

/// Recovery proves it produced the object that was ASKED for, not merely some
/// object. The expected id comes from the commit marker, so a capsule that is
/// internally perfect but belongs to a different commit is still refused.
#[test]
fn recovering_under_the_wrong_object_id_is_refused() {
    let capsule = sealed();
    let mut wrong = capsule.object_id;
    wrong.0[0] ^= 0xff;
    assert!(
        recover_from(&capsule.symbols, &capsule.descriptor, wrong).is_err(),
        "a capsule must not recover under an identity that is not its own"
    );
}

// ---------------------------------------------------------------------------
// Container framing
// ---------------------------------------------------------------------------

#[test]
fn a_foreign_or_truncated_container_is_refused() {
    let capsule = sealed();
    let bytes = encode_container(&capsule);

    let mut foreign = bytes.clone();
    foreign[0] ^= 0xff;
    assert!(matches!(
        decode_container(&foreign),
        Err(CapsuleError::MalformedContainer)
    ));

    // Truncating inside the HEADER is malformed — the descriptor is not
    // optional and a partial one cannot be checked.
    for cut in 0..80 {
        assert!(
            decode_container(&bytes[..cut]).is_err(),
            "a {cut}-byte prefix must not decode as a container"
        );
    }
}

#[test]
fn an_unsupported_container_version_is_refused() {
    let capsule = sealed();
    let mut bytes = encode_container(&capsule);
    bytes[4..6].copy_from_slice(&99u16.to_be_bytes());
    assert!(matches!(
        decode_container(&bytes),
        Err(CapsuleError::UnsupportedFormat { format: 99 })
    ));
}

/// A container truncated in its SYMBOL region keeps the symbols that survived
/// and drops the partial one, because that is exactly the damage the erasure
/// code exists to absorb. Refusing to parse would turn a recoverable object
/// into an unrecoverable one — the decoder is the only thing that can decide
/// whether enough survived.
#[test]
fn a_container_truncated_in_its_symbols_still_recovers_within_budget() {
    let capsule = sealed();
    let bytes = encode_container(&capsule);
    let budget = profile().erasure_budget();

    // Cut one symbol's worth of bytes off the end, well within the budget.
    let symbol_frame = 4 + capsule.symbols[0].len();
    let cut = bytes.len() - symbol_frame;
    let (descriptor, symbols) = decode_container(&bytes[..cut]).expect("header still parses");
    assert_eq!(
        symbols.len(),
        capsule.symbols.len() - 1,
        "the partial trailing symbol is dropped, not fatal"
    );
    assert!(budget >= 1);
    let recovered = recover_from(&symbols, &descriptor, capsule.object_id).expect("recovers");
    assert_eq!(recovered, plaintext());
}
