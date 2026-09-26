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
    CAPSULE_HEADER_BYTES_V1, CAPSULE_MAGIC, CapsuleError, CapsuleProfile,
    MAX_CAPSULE_CONTAINER_BYTES_V1, decode_container, encode_container, recover, seal,
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
    recover(
        descriptor,
        symbols,
        object_id,
        &K_OID,
        NAMESPACE,
        &DEK,
        &mut Vec::new(),
    )
}

// ---------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------

#[test]
fn a_capsule_round_trips_through_its_container() {
    let capsule = sealed();
    let bytes = encode_container(&capsule);
    assert_eq!(&bytes[..4], &CAPSULE_MAGIC);
    assert_eq!(
        MAX_CAPSULE_CONTAINER_BYTES_V1, 24_031_256,
        "the recovery ceiling is derived from the closed V1 profile and RFC source-symbol bound"
    );
    assert_eq!(CAPSULE_HEADER_BYTES_V1, 170);
    assert!(bytes.len() < MAX_CAPSULE_CONTAINER_BYTES_V1);

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
    let protected = identified
        .protect(&DEK, descriptor.cipher_descriptor(), &plaintext())
        .expect("registered AEAD profile");
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
    let protected = identified
        .protect(&DEK, descriptor.cipher_descriptor(), &plaintext())
        .expect("registered AEAD profile");
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
/// self-consistency. 9 is the first id past the registered balanced family
/// (1..=8, fgdb-myldi).
#[test]
fn a_self_consistent_unregistered_fec_profile_is_refused() {
    let capsule = sealed();
    let mut descriptor = capsule.descriptor.clone();
    descriptor.fec_profile = 9;

    let identified = IdentifiedObject::new(&K_OID, NAMESPACE, KIND, &[], &plaintext());
    let protected = identified
        .protect(&DEK, descriptor.cipher_descriptor(), &plaintext())
        .expect("registered AEAD profile");
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
        Err(CapsuleError::UnsupportedFecProfile { fec_profile: 9 })
    ));
}

/// A REGISTERED profile id fixes its symbol size, so a self-consistent frame
/// cannot pair it with another size: fec_profile 2 registers 512-byte
/// symbols, and this capsule's 256-byte symbols under it are refused, both
/// when the container is parsed and when the descriptor is validated.
#[test]
fn a_registered_fec_profile_with_another_symbol_size_is_refused() {
    let capsule = sealed();
    assert_eq!(
        (
            capsule.descriptor.fec_profile,
            capsule.descriptor.symbol_size
        ),
        (1, 256)
    );
    let mut descriptor = capsule.descriptor.clone();
    descriptor.fec_profile = 2;
    let identified = IdentifiedObject::new(&K_OID, NAMESPACE, KIND, &[], &plaintext());
    let protected = identified
        .protect(&DEK, descriptor.cipher_descriptor(), &plaintext())
        .expect("registered AEAD profile");
    descriptor.encoding_id = protected
        .encode(descriptor.encoding_descriptor())
        .encoding_id()
        .0;
    let mismatch = |error: &CapsuleError| {
        matches!(
            error,
            CapsuleError::SymbolSizeMismatch {
                fec_profile: 2,
                declared_symbol_size: 256,
                registered_symbol_size: 512,
            }
        )
    };
    let refused = recover_from(&capsule.symbols, &descriptor, capsule.object_id)
        .expect_err("a registered id with a foreign symbol size must not recover");
    assert!(mismatch(&refused), "{refused}");

    let mut forged = capsule.clone();
    forged.descriptor = descriptor;
    let parsed = decode_container(&encode_container(&forged))
        .expect_err("the container parser must refuse it too");
    assert!(mismatch(&parsed), "{parsed}");
}

/// **THE BALANCED FAMILY KEEPS K SMALL (fgdb-myldi).** RFC 6330 encoding is
/// roughly cubic in the source-symbol count K, so the writer picks, per
/// capsule, the smallest registered symbol size (256 << (id - 1)) that keeps K
/// at or under 128. For every member, the smallest capsule that selects it
/// (K = 65 above member 1, the exact K = 128 fit at member 1) names that
/// member with its registered symbol size, serializes every symbol with the
/// same fixed per-symbol overhead the recovery ceiling is computed from,
/// round-trips, heals the full repair budget, and fails closed one symbol
/// past it. The exhaustive selection laws are unit tests in `capsule.rs`.
#[test]
fn every_family_member_seals_round_trips_and_heals_its_budget() {
    let probe = seal(&K_OID, NAMESPACE, &DEK, KIND, &[7u8; 1000], profile()).expect("seals");
    let overhead = probe.descriptor.transfer_length as usize - 1000;
    let per_symbol = probe.symbols[0].len() - 256;
    let budget = profile().erasure_budget();
    for fec_profile in 1..=8u16 {
        let symbol_size = 256usize << (fec_profile - 1);
        let transfer_length = if fec_profile == 1 {
            128 * 256
        } else {
            128 * (symbol_size / 2) + 1
        };
        let plaintext: Vec<u8> = (0..transfer_length - overhead)
            .map(|i| (i % 251) as u8)
            .collect();
        let capsule = seal(&K_OID, NAMESPACE, &DEK, KIND, &plaintext, profile()).expect("seals");
        let d = &capsule.descriptor;
        assert_eq!(d.transfer_length as usize, transfer_length);
        assert_eq!(
            (d.fec_profile, usize::from(d.symbol_size)),
            (fec_profile, symbol_size)
        );
        assert_eq!(d.repair_symbols as usize, budget);
        let k = transfer_length.div_ceil(symbol_size);
        assert_eq!(capsule.symbols.len(), k + budget, "{fec_profile}");
        assert!(
            capsule
                .symbols
                .iter()
                .all(|symbol| symbol.len() == symbol_size + per_symbol),
            "{fec_profile}: the per-symbol overhead is fixed across the family"
        );
        let bytes = encode_container(&capsule);
        assert_eq!(
            bytes.len(),
            CAPSULE_HEADER_BYTES_V1 + capsule.symbols.len() * (4 + symbol_size + per_symbol),
            "{fec_profile}"
        );
        let (descriptor, symbols) = decode_container(&bytes).expect("container round trip");
        assert_eq!(
            recover_from(&symbols, &descriptor, capsule.object_id).expect("recovers"),
            plaintext,
            "{fec_profile}"
        );
        assert_eq!(
            recover_from(&symbols[budget..], &descriptor, capsule.object_id)
                .expect("heals the full budget"),
            plaintext,
            "{fec_profile}"
        );
        assert!(
            matches!(
                recover_from(&symbols[budget + 1..], &descriptor, capsule.object_id),
                Err(CapsuleError::Recovery(_))
            ),
            "{fec_profile}: one symbol past the budget fails closed"
        );
    }
}

/// Capsules up to 32 KiB are byte-identical to what the encoder wrote before
/// the balanced family existed: a 2,000-byte object and the largest object
/// whose sealed bytes fit 128 symbols of 256 bytes each (transfer length
/// 32,768) hash to the container digests captured from the pre-family
/// encoder at 87992018 (fgdb-myldi). Stores written before the family keep
/// reading and writing the same bytes for every commit at or under that size.
#[test]
fn capsules_up_to_32_kib_are_byte_identical_to_the_pre_family_encoding() {
    let probe = seal(&K_OID, NAMESPACE, &DEK, KIND, &[7u8; 1000], profile()).expect("seals");
    let overhead = probe.descriptor.transfer_length as usize - 1000;
    for (len, transfer_length, container_len, digest) in [
        (
            2000,
            2016,
            6986,
            "48e6ad9d7cb64ffdaf59e368265634a5aa1bb9a00083fb87d1d6ca6db3a17f75",
        ),
        (
            128 * 256 - overhead,
            32_768,
            58_106,
            "0e9b273b64f4a4775ac42028c3f4cd3cc253e89dd7bd972e501034298fdea530",
        ),
    ] {
        let plaintext: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let capsule = seal(&K_OID, NAMESPACE, &DEK, KIND, &plaintext, profile()).expect("seals");
        let bytes = encode_container(&capsule);
        assert_eq!(capsule.descriptor.transfer_length, transfer_length, "{len}");
        assert_eq!(bytes.len(), container_len, "{len}");
        assert_eq!(fgdb_crypto::hash(&bytes).to_hex(), digest, "{len}");
    }
}

/// Larger symbols never raise the recovery allocation bound: a sealed object
/// one byte past the largest a V1 capsule may carry (one source block of
/// 256-byte symbols) is refused, as it was before the family existed, even
/// though a 32 KiB-symbol member could encode it in 441 symbols. That every
/// member's container at the largest length fits the ceiling is a const
/// assertion in `capsule.rs`, computed from the per-symbol overhead the family
/// law above pins against real containers (fgdb-myldi).
#[test]
fn one_byte_past_the_largest_capsule_is_refused_under_every_symbol_size() {
    let probe = seal(&K_OID, NAMESPACE, &DEK, KIND, &[7u8; 1000], profile()).expect("seals");
    let overhead = probe.descriptor.transfer_length as usize - 1000;
    let one_more: Vec<u8> = (0..56_403 * 256 - overhead + 1)
        .map(|i| (i % 251) as u8)
        .collect();
    assert!(matches!(
        seal(&K_OID, NAMESPACE, &DEK, KIND, &one_more, profile()),
        Err(CapsuleError::Recovery(
            fgdb_chronicle::symbolize::SymbolizeError::InvalidParameters
        ))
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

#[test]
fn a_complete_container_with_trailing_bytes_is_refused() {
    let capsule = sealed();
    let mut bytes = encode_container(&capsule);
    bytes.extend_from_slice(b"hidden trailing bytes");
    assert!(matches!(
        decode_container(&bytes),
        Err(CapsuleError::MalformedContainer)
    ));
}

#[test]
fn a_declared_inventory_that_hides_a_whole_symbol_is_refused() {
    let capsule = sealed();
    let original = encode_container(&capsule);
    let count_offset = CAPSULE_HEADER_BYTES_V1 - 4;
    let original_count = u32::from_be_bytes(
        original[count_offset..CAPSULE_HEADER_BYTES_V1]
            .try_into()
            .expect("declared count occupies four bytes"),
    );
    assert!(original_count > 1, "the control needs a hidden symbol");

    let mut bytes = original.clone();
    bytes[count_offset..CAPSULE_HEADER_BYTES_V1]
        .copy_from_slice(&(original_count - 1).to_be_bytes());
    assert!(matches!(
        decode_container(&bytes),
        Err(CapsuleError::MalformedContainer)
    ));

    let mut oversized_inventory = original;
    oversized_inventory[count_offset..CAPSULE_HEADER_BYTES_V1]
        .copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(
        decode_container(&oversized_inventory),
        Err(CapsuleError::MalformedContainer)
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
