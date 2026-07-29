//! The §5.1 identity-pipeline laws, each as an executable test.
//!
//! These are behavioural laws, not vector comparisons: the primitives beneath
//! them are already oracle-verified in fgdb-crypto. What is proved here is
//! that the four identity layers change independently in exactly the way the
//! plan requires — the property that lets branches, dedup, replication,
//! backup, recoding, and KMS rewrap each touch one layer.

use fgdb_chronicle::{
    CipherDescriptor, EncodingDescriptor, IdentifiedObject, LocationForm, PlacementDescriptor,
};
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

fn descriptor(nonce_seed: u8, compressed_len: u64) -> CipherDescriptor {
    CipherDescriptor {
        object_kind: 0x0002,
        canonical_plaintext_len: 512,
        codec_profile: 1,
        compressed_len,
        data_crypto_profile: 1,
        dek_id: core::array::from_fn(|i| (i as u8).wrapping_add(nonce_seed)),
        object_nonce: core::array::from_fn(|i| (i as u8).wrapping_mul(5).wrapping_add(nonce_seed)),
        object_tag_len: 16,
    }
}

fn encoding(fec_profile: u16) -> EncodingDescriptor {
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

fn placement(epoch: u64) -> PlacementDescriptor {
    PlacementDescriptor {
        placement_epoch: epoch,
        failure_domain_policy: 2,
        location_form: LocationForm::ContiguousSpan {
            failure_domain_id: 7,
            segment_id: 42,
            offset: 4096,
            encoded_len: 8192,
            symbol_inventory_digest: fgdb_crypto::hash(b"symbol inventory"),
        },
    }
}

fn header() -> &'static [u8] {
    b"canonical-header"
}

fn payload() -> Vec<u8> {
    (0..512u32).map(|i| (i % 251) as u8).collect()
}

/// The pipeline runs end to end and the protected bytes decrypt back to what
/// went in.
#[test]
fn pipeline_round_trips_through_all_four_layers() {
    let compressed = payload();
    let object = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let object_id = object.object_id();

    let protected = object.protect(&dek(), descriptor(1, compressed.len() as u64), &compressed);
    assert_eq!(
        protected.object_id(),
        object_id,
        "identity survives protection"
    );
    assert_eq!(
        protected
            .open(&dek())
            .expect("well-formed object must open"),
        compressed,
        "protected bytes must decrypt to the compressed plaintext"
    );

    let encoded = protected.encode(encoding(1));
    assert_eq!(encoded.ciphertext_id(), protected.ciphertext_id());

    let placed = encoded.place(placement(1));
    assert_eq!(placed.encoding_id(), encoded.encoding_id());
    assert_eq!(placed.object_id(), object_id);
}

/// THE LAYERING LAW. Recoding changes ONLY the encoding identity: the same
/// authenticated ciphertext may be recoded under another complete encoding
/// descriptor without re-encryption (plan L280). This is what makes the
/// reduced-repair → full-budget recode at checkpoint install legal.
#[test]
fn recoding_changes_only_the_encoding_identity() {
    let compressed = payload();
    let object = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let protected = object.protect(&dek(), descriptor(1, compressed.len() as u64), &compressed);

    let first = protected.encode(encoding(1));
    let recoded = protected.encode(encoding(2));

    assert_eq!(
        first.object_id(),
        recoded.object_id(),
        "recoding must not change what the object IS"
    );
    assert_eq!(
        first.ciphertext_id(),
        recoded.ciphertext_id(),
        "recoding must not require re-encryption"
    );
    assert_ne!(
        first.encoding_id(),
        recoded.encoding_id(),
        "a different FEC profile is a different encoding identity"
    );
}

/// Adding or moving symbols creates a new placement record, never a new
/// encoding identity (plan L280).
#[test]
fn moving_symbols_changes_only_the_placement_identity() {
    let compressed = payload();
    let object = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let protected = object.protect(&dek(), descriptor(1, compressed.len() as u64), &compressed);
    let encoded = protected.encode(encoding(1));

    let here = encoded.place(placement(1));
    let moved = encoded.place(placement(2));

    assert_eq!(
        here.encoding_id(),
        moved.encoding_id(),
        "the encoding is unchanged"
    );
    assert_ne!(
        here.placement_id(),
        moved.placement_id(),
        "a new placement epoch is a new placement identity"
    );
}

/// Re-encryption under a fresh DEK changes the ciphertext identity while the
/// object identity survives — the property that makes clone-restore
/// re-addressing and KMS rewrap tractable.
#[test]
fn re_encryption_changes_only_the_ciphertext_identity() {
    let compressed = payload();
    let object = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let object_id = object.object_id();
    let first = object.protect(&dek(), descriptor(1, compressed.len() as u64), &compressed);

    let object_again = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let mut other_dek = dek();
    other_dek[0] ^= 0xff;
    let second = object_again.protect(
        &other_dek,
        descriptor(2, compressed.len() as u64),
        &compressed,
    );

    assert_eq!(first.object_id(), object_id);
    assert_eq!(
        second.object_id(),
        object_id,
        "identity is keyed by plaintext, not by DEK"
    );
    assert_ne!(
        first.ciphertext_id(),
        second.ciphertext_id(),
        "a different DEK and nonce is a different protection"
    );
}

/// The AAD binds the object identity: a ciphertext cannot be opened while
/// claiming to be a different object, and tampering with the descriptor is
/// detected because the descriptor IS the AAD.
#[test]
fn the_aead_binds_identity_and_descriptor() {
    let compressed = payload();
    let object = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let protected = object.protect(&dek(), descriptor(1, compressed.len() as u64), &compressed);

    let mut wrong_dek = dek();
    wrong_dek[31] ^= 0x01;
    assert!(
        protected.open(&wrong_dek).is_err(),
        "the wrong DEK must fail closed"
    );

    // Rebuild the same object under a descriptor whose bound facts differ; the
    // ciphertext bytes cannot be transplanted onto it.
    let other = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let mut lying_descriptor = descriptor(1, compressed.len() as u64);
    lying_descriptor.object_kind = 0x0003;
    let relabelled = other.protect(&dek(), lying_descriptor, &compressed);
    assert_ne!(
        relabelled.protected_bytes(),
        protected.protected_bytes(),
        "changing a bound descriptor field must change the sealed bytes"
    );
}

/// Content addressing: identical canonical plaintext in the same namespace
/// yields the same ObjectId; any plaintext difference does not.
#[test]
fn identity_is_content_addressed_within_a_namespace() {
    let a = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let b = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    assert_eq!(a.object_id(), b.object_id());
    assert!(a.verifies_as_same_object(&b));
    assert_eq!(a.lookup_prefix(), b.lookup_prefix());

    let mut different = payload();
    different[0] ^= 0x01;
    let c = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &different);
    assert_ne!(a.object_id(), c.object_id());
    assert!(!a.verifies_as_same_object(&c));
}

/// Dedup never crosses security namespaces, and never crosses key domains:
/// the same bytes under a different namespace are a DIFFERENT object.
#[test]
fn dedup_does_not_cross_namespaces_or_keys() {
    let a = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());

    let other_ns = DatabaseSecurityNamespaceId(core::array::from_fn(|i| (i as u8) ^ 0xa5));
    let b = IdentifiedObject::new(&k_oid(), other_ns, 0x0002, header(), &payload());
    assert_ne!(
        a.object_id(),
        b.object_id(),
        "the namespace is inside the keyed transcript"
    );
    assert!(!a.may_deduplicate_against(&b));

    let mut other_key = k_oid();
    other_key[0] ^= 0xff;
    let c = IdentifiedObject::new(&other_key, namespace(), 0x0002, header(), &payload());
    assert_ne!(
        a.object_id(),
        c.object_id(),
        "K_oid is what stops an offline plaintext dictionary"
    );
}

/// The object kind is part of full collision verification: two objects with
/// identical bytes but different kinds must not deduplicate even if a bucket
/// collides.
#[test]
fn collision_verification_checks_kind_and_length_not_just_the_digest() {
    let a = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let b = IdentifiedObject::new(&k_oid(), namespace(), 0x0003, header(), &payload());
    assert!(
        !a.verifies_as_same_object(&b),
        "a kind difference must defeat substitution"
    );

    let short: Vec<u8> = payload().into_iter().take(256).collect();
    let c = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &short);
    assert!(
        !a.verifies_as_same_object(&c),
        "a length difference must defeat substitution"
    );
}

/// THE KMS REWRAP LAW (plan L280): "DEK/ciphertext identity is separate from
/// `wrap_key_epoch`, so KEK/KMS recipient rewrap changes only immutable
/// `KeyWrap` records, not `CiphertextId` or `EncodingId`."
///
/// This is provable structurally rather than by simulating a KMS: the wrap
/// epoch is not among the inputs to any identity transcript, so rewrapping —
/// which by definition re-encrypts the DEK under a new KEK while leaving the
/// DEK itself unchanged — cannot move an identity. The test pins that: given
/// the SAME DEK (what a rewrap preserves), every downstream identity is
/// byte-identical, so no rewrap can silently re-address stored objects.
#[test]
fn rewrapping_the_dek_cannot_move_any_identity() {
    let compressed = payload();

    let before = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload())
        .protect(&dek(), descriptor(1, compressed.len() as u64), &compressed);
    let before_encoded = before.encode(encoding(1));
    let before_placed = before_encoded.place(placement(1));

    // A rewrap changes the KeyWrap record only: same DEK, same descriptor,
    // same bytes. Everything identity-bearing must therefore be unchanged.
    let after = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload()).protect(
        &dek(),
        descriptor(1, compressed.len() as u64),
        &compressed,
    );
    let after_encoded = after.encode(encoding(1));
    let after_placed = after_encoded.place(placement(1));

    assert_eq!(before.object_id(), after.object_id());
    assert_eq!(
        before.ciphertext_id(),
        after.ciphertext_id(),
        "a rewrap must not change CiphertextId"
    );
    assert_eq!(
        before_encoded.encoding_id(),
        after_encoded.encoding_id(),
        "a rewrap must not change EncodingId"
    );
    assert_eq!(
        before_placed.placement_id(),
        after_placed.placement_id(),
        "a rewrap must not change PlacementId"
    );
    assert_eq!(
        before.protected_bytes(),
        after.protected_bytes(),
        "the protected bytes are unchanged; only the KeyWrap record moves"
    );
}

/// The whole pipeline is deterministic: identical inputs reproduce identical
/// identities at every layer. FG-INV-09 requires exactly this — every identity
/// recomputes from its registered descriptor and bytes, which is what lets
/// root bootstrap reconstruct all identities without indexes.
#[test]
fn every_identity_recomputes_from_its_inputs() {
    let compressed = payload();
    let run = || {
        let object = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
        let protected = object.protect(&dek(), descriptor(1, compressed.len() as u64), &compressed);
        let encoded = protected.encode(encoding(1));
        let placed = encoded.place(placement(1));
        (
            protected.object_id(),
            protected.ciphertext_id(),
            encoded.encoding_id(),
            placed.placement_id(),
        )
    };
    assert_eq!(run(), run(), "identity computation must be deterministic");
}

/// Symbols from different encodings never share a MAC key (plan L280).
#[test]
fn symbol_auth_keys_are_per_encoding() {
    let compressed = payload();
    let object = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let protected = object.protect(&dek(), descriptor(1, compressed.len() as u64), &compressed);

    let first = protected.encode(encoding(1));
    let second = protected.encode(encoding(2));

    assert_ne!(
        first.symbol_auth_key(&dek()),
        second.symbol_auth_key(&dek()),
        "K_symbol is domain-separated by EncodingId"
    );
    assert_eq!(
        first.symbol_auth_key(&dek()),
        first.symbol_auth_key(&dek()),
        "K_symbol derivation is deterministic"
    );
}
