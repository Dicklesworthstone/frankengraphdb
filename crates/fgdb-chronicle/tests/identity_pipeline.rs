//! The §5.1 identity-pipeline laws, each as an executable test.
//!
//! These are behavioural laws, not vector comparisons: the primitives beneath
//! them are already oracle-verified in fgdb-crypto. What is proved here is
//! that the four identity layers change independently in exactly the way the
//! plan requires — the property that lets branches, dedup, replication,
//! backup, recoding, and KMS rewrap each touch one layer.

use fgdb_chronicle::root::{OPENER_PAYLOAD_LEN, recover_root_object};
use fgdb_chronicle::{
    CipherDescriptor, CommitDraft, CommitMarker, CryptoVerificationEvent, EffectSource,
    EncodedObject, EncodingDescriptor, IdentifiedObject, IdentityMismatch, LocationForm,
    PackBuilder, PackDomain, PackError, PackProtectionProfile, PlacementDescriptor,
    RecoveredObjectError, RootBootstrap, RootRecoveryError, RootSlot, SymbolError, SymbolRecord,
    VerificationFailureClass, VerificationOperation, VerificationOutcome, WriteKeyDomain,
};
use fgdb_crypto::Digest;
use fgdb_types::CommitSeq;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};

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
        data_crypto_profile: fgdb_crypto::DATA_CRYPTO_PROFILE_XCHACHA20_POLY1305,
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

fn encoding_id_for(
    ciphertext_id: fgdb_crypto::Digest,
    descriptor: &EncodingDescriptor,
) -> fgdb_crypto::Digest {
    let mut canonical = Vec::with_capacity(32 + 2 + 8 + 8 + 4 + 2 + 2 + 2);
    canonical.extend_from_slice(&ciphertext_id.0);
    canonical.extend_from_slice(&descriptor.fec_profile.to_le_bytes());
    canonical.extend_from_slice(&descriptor.transfer_length.to_le_bytes());
    canonical.extend_from_slice(&descriptor.oti_common.to_le_bytes());
    canonical.extend_from_slice(&descriptor.oti_scheme.to_le_bytes());
    canonical.extend_from_slice(&descriptor.symbol_size.to_le_bytes());
    canonical.extend_from_slice(&descriptor.source_block_count.to_le_bytes());
    canonical.extend_from_slice(&descriptor.symbol_auth_profile.to_le_bytes());
    fgdb_crypto::encoding_id(&canonical)
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

#[test]
fn cipher_descriptor_fixed_width_fields_are_little_endian() {
    let descriptor = CipherDescriptor {
        object_kind: 0x1234,
        canonical_plaintext_len: 0x0102_0304_0506_0708,
        codec_profile: 0x1122,
        compressed_len: 0x2122_2324_2526_2728,
        data_crypto_profile: 0x3344,
        dek_id: [0x55; 16],
        object_nonce: [0x66; 24],
        object_tag_len: 0x7788,
    };

    let bytes = descriptor.canonical_bytes();
    assert_eq!(&bytes[0..2], &0x1234u16.to_le_bytes());
    assert_eq!(&bytes[2..10], &0x0102_0304_0506_0708u64.to_le_bytes());
    assert_eq!(&bytes[10..12], &0x1122u16.to_le_bytes());
    assert_eq!(&bytes[12..20], &0x2122_2324_2526_2728u64.to_le_bytes());
    assert_eq!(&bytes[20..22], &0x3344u16.to_le_bytes());
    assert_eq!(&bytes[62..64], &0x7788u16.to_le_bytes());
}

fn payload() -> Vec<u8> {
    (0..512u32).map(|i| (i % 251) as u8).collect()
}

fn root_bootstrap(cipher: &CipherDescriptor) -> RootBootstrap {
    RootBootstrap {
        root_encoding_id: [0; 32],
        root_placement_id: [0; 32],
        root_placement_epoch: 1,
        failure_domain_policy_id: 1,
        root_failure_domain_id: 1,
        segment_id: 1,
        offset: 0,
        encoded_len: 1,
        root_symbol_inventory_digest: [0; 32],
        object_kind: cipher.object_kind,
        canonical_plaintext_len: cipher.canonical_plaintext_len,
        codec_profile: cipher.codec_profile,
        compressed_len: cipher.compressed_len,
        data_crypto_profile: cipher.data_crypto_profile,
        dek_id: cipher.dek_id,
        nonce_len: 24,
        nonce_or_siv: cipher.object_nonce,
        object_tag_len: cipher.object_tag_len,
        fec_profile: 1,
        transfer_length: 1,
        oti_common: 0,
        oti_scheme: 0,
        symbol_size: 1,
        source_block_count: 1,
        symbol_auth_profile: 1,
        ciphertext_id: [0; 32],
        ciphertext_digest: [0; 32],
        opener_kind: 1,
        oid_key_id: [0; 16],
        opener_payload_len: 0,
        opener_payload: [0; OPENER_PAYLOAD_LEN],
        opener_digest: [0; 32],
    }
}

fn debug_marker(capsule_oid: ObjectId) -> CommitMarker {
    CommitMarker {
        logical_command_seq: 1,
        commit_seq: 1,
        effect_source: EffectSource::Local {
            capsule_ref: capsule_oid,
            logical_delta_template_digest: Digest([0x31; 32]),
        },
        prev_global: None,
        head_updates: Vec::new(),
        merge_record_oid: None,
        coordinate_schema_transition_digest: Digest([0x32; 32]),
        topology_epoch: 1,
        policy_epoch: 1,
        revocation_index: 1,
        txn_token: [0x33; 16],
        commit_hlc: 1,
        final_effect_digest: Digest([0x34; 32]),
        authorization_decision_digest: Digest([0x35; 32]),
        resource_effect_digest: Digest([0x36; 32]),
        payload_availability_certificate_oid: None,
        flags: 0,
    }
}

fn assert_debug_omits_bytes(rendered: &str, bytes: &[u8]) {
    let numeric_needle = bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        !rendered.contains(&numeric_needle),
        "Debug output exposed the planted canonical plaintext: {rendered}"
    );
}

#[test]
fn canonical_plaintext_is_redacted_from_direct_and_containing_debug_surfaces() {
    let plaintext = [
        0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xeb, 0xec,
    ];
    let object = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &plaintext);
    let object_debug = format!("{object:?}");
    assert_debug_omits_bytes(&object_debug, &plaintext);
    assert!(object_debug.contains("canonical_plaintext: \"[REDACTED]\""));
    assert!(object_debug.contains("canonical_plaintext_len:"));

    let domain = PackDomain {
        namespace: namespace(),
        tenant: 7,
        write_key: WriteKeyDomain::CommitStream,
        retention_class: 3,
    };
    let mut pack = PackBuilder::new(domain);
    pack.add(object.clone(), domain)
        .expect("the member belongs to the pack domain");
    let pack_debug = format!("{pack:?}");
    assert_debug_omits_bytes(&pack_debug, &plaintext);
    assert!(pack_debug.contains("member_count: 1"));
    assert!(pack_debug.contains("members: \"[REDACTED]\""));

    let marker = debug_marker(object.object_id());
    let draft = CommitDraft {
        commit_seq: CommitSeq(1),
        capsule_oid: object.object_id(),
        capsule_plaintext: &plaintext,
        marker: &marker,
    };
    let draft_debug = format!("{draft:?}");
    assert_debug_omits_bytes(&draft_debug, &plaintext);
    assert!(draft_debug.contains("capsule_plaintext_len: 12"));
    assert!(draft_debug.contains("capsule_plaintext: \"[REDACTED]\""));
    assert!(draft_debug.contains("marker: \"[REDACTED]\""));
}

/// The pipeline runs end to end and the protected bytes decrypt back to what
/// went in.
#[test]
fn pipeline_round_trips_through_all_four_layers() {
    let compressed = payload();
    let object = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let object_id = object.object_id();

    let protected = object
        .protect(&dek(), descriptor(1, compressed.len() as u64), &compressed)
        .expect("registered AEAD profile");
    assert_eq!(
        protected.object_id(),
        object_id,
        "identity survives protection"
    );
    assert_eq!(
        protected
            .open(&dek(), &mut Vec::new())
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
    let protected = object
        .protect(&dek(), descriptor(1, compressed.len() as u64), &compressed)
        .expect("registered AEAD profile");

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
    let protected = object
        .protect(&dek(), descriptor(1, compressed.len() as u64), &compressed)
        .expect("registered AEAD profile");
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
    let first = object
        .protect(&dek(), descriptor(1, compressed.len() as u64), &compressed)
        .expect("registered AEAD profile");

    let object_again = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let mut other_dek = dek();
    other_dek[0] ^= 0xff;
    let second = object_again
        .protect(
            &other_dek,
            descriptor(2, compressed.len() as u64),
            &compressed,
        )
        .expect("registered AEAD profile");

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
    let protected = object
        .protect(&dek(), descriptor(1, compressed.len() as u64), &compressed)
        .expect("registered AEAD profile");

    let mut wrong_dek = dek();
    wrong_dek[31] ^= 0x01;
    assert!(
        protected.open(&wrong_dek, &mut Vec::new()).is_err(),
        "the wrong DEK must fail closed"
    );

    // Rebuild the same object under a descriptor whose bound facts differ; the
    // ciphertext bytes cannot be transplanted onto it.
    let other = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let mut lying_descriptor = descriptor(1, compressed.len() as u64);
    lying_descriptor.object_kind = 0x0003;
    let relabelled = other
        .protect(&dek(), lying_descriptor, &compressed)
        .expect("registered AEAD profile");
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
    // The refusal follows from the identity difference, but the registry
    // clause bound to this test claims BOTH arms refuse deduplication, so
    // both arms observe it rather than leaving one to inference.
    assert!(!a.may_deduplicate_against(&c));
}

/// The object kind is part of full collision verification: two objects with
/// identical bytes but different kinds must not deduplicate even if a bucket
/// collides.
#[test]
fn collision_verification_checks_kind_and_length_not_just_the_digest() {
    let a = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let b = IdentifiedObject::new(&k_oid(), namespace(), 0x0003, header(), &payload());
    assert_ne!(
        a.object_id(),
        b.object_id(),
        "object kind is inside the keyed logical-identity transcript"
    );
    assert_eq!(&a.canonical_plaintext()[..2], &0x0002u16.to_le_bytes());
    assert_eq!(&b.canonical_plaintext()[..2], &0x0003u16.to_le_bytes());
    assert!(!a.verifies_as_same_object(&b));

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
        .protect(&dek(), descriptor(1, compressed.len() as u64), &compressed)
        .expect("registered AEAD profile");
    let before_encoded = before.encode(encoding(1));
    let before_placed = before_encoded.place(placement(1));

    // A rewrap changes the KeyWrap record only: same DEK, same descriptor,
    // same bytes. Everything identity-bearing must therefore be unchanged.
    let after = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload())
        .protect(&dek(), descriptor(1, compressed.len() as u64), &compressed)
        .expect("registered AEAD profile");
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
        let protected = object
            .protect(&dek(), descriptor(1, compressed.len() as u64), &compressed)
            .expect("registered AEAD profile");
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
    let protected = object
        .protect(&dek(), descriptor(1, compressed.len() as u64), &compressed)
        .expect("registered AEAD profile");

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

/// The governed `cargo-test:crypto-composition` entrypoint.
///
/// This proves the registered V1 composition that exists today: keyed logical
/// identity, the fixed passphrase-to-KEK profile, the production-only fresh
/// per-ciphertext material authority, profile-bound object AEAD, encoding and
/// placement identities, encoding-bound symbol authentication, and
/// rewrap-stable downstream identities.
/// It deliberately does NOT claim Chronicle writer or durable KeyWrap/root
/// integration, nonce-inventory persistence, signing/KMS support, runtime KDF
/// auto-tuning, statistical timing, optimized zeroization inspection, or
/// external audit.
#[test]
fn registered_crypto_composition_is_profile_bound_and_fail_closed() {
    let mut verification = Vec::new();
    let kdf_profile = fgdb_crypto::registered_passphrase_kdf_profile(1)
        .expect("profile 1 is the closed V1 passphrase-KDF profile");
    let kdf_spec = kdf_profile.spec();
    assert_eq!(kdf_profile.id(), 1);
    assert_eq!(kdf_spec.profile_id, 1);
    assert_eq!(kdf_spec.argon2_version, 0x13);
    assert_eq!(kdf_spec.variant, fgdb_crypto::argon2id::Variant::Argon2id);
    assert_eq!(kdf_spec.params.memory_kib, 65_536);
    assert_eq!(kdf_spec.params.passes, 3);
    assert_eq!(kdf_spec.params.lanes, 4);
    assert_eq!(kdf_spec.salt_len, 16);
    assert_eq!(kdf_spec.kek_len, 32);
    for id in 0..=u16::MAX {
        if id == kdf_profile.id() {
            assert_eq!(
                fgdb_crypto::registered_passphrase_kdf_profile(id),
                Some(kdf_profile)
            );
        } else {
            assert!(
                fgdb_crypto::registered_passphrase_kdf_profile(id).is_none(),
                "unregistered passphrase-KDF profile {id} entered the closed numeric registry"
            );
        }
    }

    let passphrase = b"correct horse battery staple";
    let salt = b"fgdb-kdf-salt-v1";
    let profiled_kek = fgdb_crypto::derive_passphrase_kek(1, passphrase, salt)
        .expect("the registered KDF profile derives a KEK");
    // Independent OpenSSL 3 ARGON2ID oracle, configured with the exact row
    // above (threads=1 is an execution choice; lanes=4 is the Argon2 input).
    let oracle_kek = [
        0xda, 0x77, 0xb3, 0xeb, 0x0b, 0x5f, 0x4d, 0x7c, 0x51, 0x06, 0x1f, 0xe6, 0xfc, 0x3b, 0xeb,
        0x38, 0x67, 0xdf, 0x61, 0x85, 0x2c, 0xfc, 0xff, 0x46, 0xc9, 0xf0, 0xd1, 0x9a, 0x26, 0x3d,
        0x76, 0xff,
    ];
    assert_eq!(
        profiled_kek.expose(),
        &oracle_kek,
        "profile 1 must match an independent implementation of its complete pinned tuple"
    );
    for unknown in [0, 2, u16::MAX] {
        assert_eq!(
            fgdb_crypto::derive_passphrase_kek(unknown, passphrase, salt)
                .expect_err("unknown KDF profile must fail"),
            fgdb_crypto::PassphraseKdfError::UnsupportedProfile {
                profile_id: unknown,
            },
            "unknown KDF profile {unknown} must fail before expensive derivation"
        );
    }
    for bad_salt in [&salt[..15], b"fgdb-kdf-salt-v1-extra".as_slice()] {
        assert_eq!(
            fgdb_crypto::derive_passphrase_kek(1, passphrase, bad_salt)
                .expect_err("wrong salt width must fail"),
            fgdb_crypto::PassphraseKdfError::SaltLength {
                profile_id: 1,
                expected: 16,
                actual: bad_salt.len(),
            },
            "profile 1 must reject a {}-byte salt before expensive derivation",
            bad_salt.len()
        );
    }

    let registered = fgdb_crypto::registered_object_aead_profile(1)
        .expect("profile 1 is the closed V1 object-AEAD profile");
    assert_eq!(registered.id(), 1);
    assert_eq!(registered.nonce_len(), 24);
    assert_eq!(registered.tag_len(), 16);
    for id in 0..=u16::MAX {
        if id == registered.id() {
            assert_eq!(
                fgdb_crypto::registered_object_aead_profile(id),
                Some(registered)
            );
        } else {
            assert!(
                fgdb_crypto::registered_object_aead_profile(id).is_none(),
                "unregistered profile {id} entered the closed numeric registry"
            );
        }
    }

    let mut protect_once =
        |material: fgdb_crypto::FreshObjectProtectionMaterial,
         forbidden_prior: Option<(&[u8], &[u8; 24], &[u8])>| {
            material.use_once(|material| {
                assert_eq!(material.profile(), registered);
                assert_eq!(
                    format!("{material:?}"),
                    "ObjectProtectionMaterialRef(redacted)",
                    "the borrowed DEK and nonce must not enter the registered gate transcript"
                );
                if let Some((prior_sealed, prior_nonce, prior_aad)) = forbidden_prior {
                    assert!(
                        fgdb_crypto::xchacha20poly1305_open(
                            material.dek(),
                            prior_nonce,
                            prior_aad,
                            prior_sealed,
                        )
                        .is_err(),
                        "fresh material reused the prior ciphertext's DEK"
                    );
                }
                let compressed = payload();
                let mut cipher = descriptor(0x5a, compressed.len() as u64);
                cipher.data_crypto_profile = registered.id();
                cipher.dek_id = material.dek_id();
                cipher.object_nonce = material.object_nonce();
                let identified =
                    IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
                let aad = fgdb_crypto::object_aead_aad(
                    &fgdb_crypto::Digest(identified.object_id().0),
                    &cipher.canonical_bytes(),
                );
                let protected = identified
                    .protect(material.dek(), cipher.clone(), &compressed)
                    .expect("fresh production material seals through the registered profile");
                assert_eq!(protected.descriptor(), &cipher);
                assert_eq!(
                    protected
                        .open(material.dek(), &mut verification)
                        .expect("fresh production material opens in its closure"),
                    compressed
                );
                (
                    protected.object_id(),
                    protected.ciphertext_id(),
                    cipher.dek_id,
                    cipher.object_nonce,
                    protected.protected_bytes().to_vec(),
                    aad,
                )
            })
        };
    let refused_test_source =
        fgdb_crypto::CryptoCx::new(fgdb_crypto::SystemEntropy::from_path_for_test("/dev/zero"))
            .fresh_object_protection_material(registered)
            .expect_err("a caller-chosen test path must not mint object key material");
    assert_eq!(
        refused_test_source,
        fgdb_crypto::EntropyError::NotApprovedForKeyMaterial {
            source_id: "system",
        },
        "the production-only capability must refuse before reading a caller-chosen path"
    );

    let crypto_cx = fgdb_crypto::CryptoCx::production();
    let first_material = crypto_cx
        .fresh_object_protection_material(registered)
        .expect("production secret entropy is available");
    assert_eq!(
        format!("{first_material:?}"),
        "FreshObjectProtectionMaterial(redacted)",
        "the owning DEK authority must redact every field"
    );
    let first_fresh = protect_once(first_material, None);
    let second_fresh = protect_once(
        crypto_cx
            .fresh_object_protection_material(registered)
            .expect("production secret entropy is available"),
        Some((&first_fresh.4, &first_fresh.3, &first_fresh.5)),
    );
    assert_eq!(
        first_fresh.0, second_fresh.0,
        "fresh physical protection must not move logical identity"
    );
    assert_ne!(
        first_fresh.1, second_fresh.1,
        "two per-ciphertext DEKs must produce distinct CiphertextIds"
    );
    assert_ne!(
        first_fresh.2, second_fresh.2,
        "two ciphertexts must not reuse one DEK identity"
    );
    assert_ne!(
        first_fresh.3, second_fresh.3,
        "two ciphertexts must not reuse one XChaCha nonce"
    );

    // Keep every identity-layer and rewrap law inside the one exact selector
    // that CI registers; separate green tests must not be able to conceal a
    // disconnected composition path.
    pipeline_round_trips_through_all_four_layers();
    recoding_changes_only_the_encoding_identity();
    moving_symbols_changes_only_the_placement_identity();
    re_encryption_changes_only_the_ciphertext_identity();
    the_aead_binds_identity_and_descriptor();
    dedup_does_not_cross_namespaces_or_keys();
    rewrapping_the_dek_cannot_move_any_identity();
    every_identity_recomputes_from_its_inputs();
    symbol_auth_keys_are_per_encoding();

    let compressed = payload();
    let cipher = descriptor(1, compressed.len() as u64);
    let identified = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let identified_object_id = identified.object_id();
    let protected = identified
        .protect(&dek(), cipher.clone(), &compressed)
        .expect("the registered profile seals");
    assert_eq!(
        protected.protected_bytes().len(),
        compressed.len() + usize::from(registered.tag_len()),
        "the registered durable tag width must equal the primitive output overhead"
    );
    let direct_aad = fgdb_crypto::object_aead_aad(
        &fgdb_crypto::Digest(identified_object_id.0),
        &cipher.canonical_bytes(),
    );
    assert_eq!(
        protected.protected_bytes(),
        fgdb_crypto::xchacha20poly1305_seal(&dek(), &cipher.object_nonce, &direct_aad, &compressed,),
        "profile 1 must dispatch to the named XChaCha20-Poly1305 primitive"
    );
    let encoded = protected.encode(encoding(1));
    let placed = encoded.place(placement(1));
    assert_eq!(placed.object_id(), protected.object_id());
    assert_eq!(placed.encoding_id(), encoded.encoding_id());
    assert_eq!(
        protected
            .open(&dek(), &mut verification)
            .expect("registered profile opens"),
        compressed
    );

    let logical = IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload());
    let mut other_key = k_oid();
    other_key[0] ^= 1;
    assert_ne!(
        logical.object_id(),
        IdentifiedObject::new(&other_key, namespace(), 0x0002, header(), &payload()).object_id(),
        "K_oid is outside the logical-object identity transcript"
    );
    let mut other_namespace = namespace();
    other_namespace.0[0] ^= 1;
    assert_ne!(
        logical.object_id(),
        IdentifiedObject::new(&k_oid(), other_namespace, 0x0002, header(), &payload()).object_id(),
        "the security namespace is outside the logical-object identity transcript"
    );
    assert_ne!(
        logical.object_id(),
        IdentifiedObject::new(&k_oid(), namespace(), 0x0003, header(), &payload()).object_id(),
        "object kind is outside the logical-object identity transcript"
    );
    assert_ne!(
        logical.object_id(),
        IdentifiedObject::new(
            &k_oid(),
            namespace(),
            0x0002,
            b"changed-canonical-header",
            &payload(),
        )
        .object_id(),
        "canonical header bytes are outside the logical-object identity transcript"
    );
    let mut other_payload = payload();
    other_payload[0] ^= 1;
    assert_ne!(
        logical.object_id(),
        IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &other_payload).object_id(),
        "canonical payload bytes are outside the logical-object identity transcript"
    );

    let mut cipher_mutations = Vec::new();
    let mut mutation = cipher.clone();
    mutation.object_kind ^= 1;
    cipher_mutations.push(("object_kind", mutation));
    let mut mutation = cipher.clone();
    mutation.canonical_plaintext_len += 1;
    cipher_mutations.push(("canonical_plaintext_len", mutation));
    let mut mutation = cipher.clone();
    mutation.codec_profile += 1;
    cipher_mutations.push(("codec_profile", mutation));
    let mut mutation = cipher.clone();
    mutation.compressed_len += 1;
    cipher_mutations.push(("compressed_len", mutation));
    let mut mutation = cipher.clone();
    mutation.dek_id[0] ^= 1;
    cipher_mutations.push(("dek_id", mutation));
    let mut mutation = cipher.clone();
    mutation.object_nonce[0] ^= 1;
    cipher_mutations.push(("object_nonce", mutation));
    for (field, mutation) in cipher_mutations {
        let substituted = EncodedObject::reconstruct(
            encoded.object_id(),
            mutation,
            encoded.ciphertext_id(),
            encoding(1),
            encoded.encoding_id(),
            &mut verification,
        )
        .expect("a valid-form cipher mutation does not alter EncodingId");
        assert!(
            substituted
                .open_recovered(protected.protected_bytes(), &dek(), &mut verification)
                .is_err(),
            "cipher descriptor field {field} is outside the object-AEAD transcript"
        );
    }

    let mut encoding_mutations = Vec::new();
    let mut mutation = encoding(1);
    mutation.fec_profile += 1;
    encoding_mutations.push(("fec_profile", mutation));
    let mut mutation = encoding(1);
    mutation.transfer_length += 1;
    encoding_mutations.push(("transfer_length", mutation));
    let mut mutation = encoding(1);
    mutation.oti_common += 1;
    encoding_mutations.push(("oti_common", mutation));
    let mut mutation = encoding(1);
    mutation.oti_scheme += 1;
    encoding_mutations.push(("oti_scheme", mutation));
    let mut mutation = encoding(1);
    mutation.symbol_size += 1;
    encoding_mutations.push(("symbol_size", mutation));
    let mut mutation = encoding(1);
    mutation.source_block_count += 1;
    encoding_mutations.push(("source_block_count", mutation));
    let mut mutation = encoding(1);
    mutation.symbol_auth_profile += 1;
    encoding_mutations.push(("symbol_auth_profile", mutation));
    for (field, mutation) in encoding_mutations {
        assert_ne!(
            protected.encode(mutation.clone()).encoding_id(),
            encoded.encoding_id(),
            "encoding descriptor field {field} is outside EncodingId"
        );
        assert_eq!(
            EncodedObject::reconstruct(
                encoded.object_id(),
                cipher.clone(),
                encoded.ciphertext_id(),
                mutation,
                encoded.encoding_id(),
                &mut verification,
            ),
            Err(IdentityMismatch::EncodingId),
            "durable recovery accepted a rewritten encoding field {field}"
        );
    }

    let mut forged_ciphertext_id = encoded.ciphertext_id();
    forged_ciphertext_id.0[0] ^= 1;
    let encoding_descriptor = encoding(1);
    let forged_encoding_id = encoding_id_for(forged_ciphertext_id, &encoding_descriptor);
    let forged_encoding = EncodedObject::reconstruct(
        encoded.object_id(),
        cipher.clone(),
        forged_ciphertext_id,
        encoding_descriptor,
        forged_encoding_id,
        &mut verification,
    )
    .expect("the forged EncodingId exactly matches the forged CiphertextId");
    assert_eq!(
        forged_encoding.open_recovered(protected.protected_bytes(), &dek(), &mut verification,),
        Err(RecoveredObjectError::CiphertextIdentityMismatch),
        "durable recovery must recompute CiphertextId even when EncodingId was consistently rewritten"
    );

    let mut placement_mutations = Vec::new();
    let mut mutation = placement(1);
    mutation.placement_epoch += 1;
    placement_mutations.push(("placement_epoch", mutation));
    let mut mutation = placement(1);
    mutation.failure_domain_policy += 1;
    placement_mutations.push(("failure_domain_policy", mutation));
    let mut mutation = placement(1);
    if let LocationForm::ContiguousSpan {
        failure_domain_id, ..
    } = &mut mutation.location_form
    {
        *failure_domain_id += 1;
    }
    placement_mutations.push(("contiguous.failure_domain_id", mutation));
    let mut mutation = placement(1);
    if let LocationForm::ContiguousSpan { segment_id, .. } = &mut mutation.location_form {
        *segment_id += 1;
    }
    placement_mutations.push(("contiguous.segment_id", mutation));
    let mut mutation = placement(1);
    if let LocationForm::ContiguousSpan { offset, .. } = &mut mutation.location_form {
        *offset += 1;
    }
    placement_mutations.push(("contiguous.offset", mutation));
    let mut mutation = placement(1);
    if let LocationForm::ContiguousSpan { encoded_len, .. } = &mut mutation.location_form {
        *encoded_len += 1;
    }
    placement_mutations.push(("contiguous.encoded_len", mutation));
    let mut mutation = placement(1);
    if let LocationForm::ContiguousSpan {
        symbol_inventory_digest,
        ..
    } = &mut mutation.location_form
    {
        symbol_inventory_digest.0[0] ^= 1;
    }
    placement_mutations.push(("contiguous.symbol_inventory_digest", mutation));
    placement_mutations.push((
        "location_form",
        PlacementDescriptor {
            placement_epoch: 1,
            failure_domain_policy: 2,
            location_form: LocationForm::Explicit {
                sorted_symbol_inventory: vec![1, 2, 3],
                failure_domains: vec![7],
            },
        },
    ));
    for (field, mutation) in placement_mutations {
        assert_ne!(
            encoded.place(mutation.clone()).placement_id(),
            placed.placement_id(),
            "placement descriptor field {field} is outside PlacementId"
        );
        assert_eq!(
            encoded.verify_placement(&mutation, placed.placement_id(), &mut verification),
            Err(IdentityMismatch::PlacementId),
            "durable recovery accepted a rewritten placement field {field}"
        );
    }

    let explicit = PlacementDescriptor {
        placement_epoch: 2,
        failure_domain_policy: 3,
        location_form: LocationForm::Explicit {
            sorted_symbol_inventory: vec![1, 2, 3],
            failure_domains: vec![7, 9],
        },
    };
    let explicit_placed = encoded.place(explicit.clone());
    for (field, location_form) in [
        (
            "explicit.sorted_symbol_inventory",
            LocationForm::Explicit {
                sorted_symbol_inventory: vec![1, 2, 4],
                failure_domains: vec![7, 9],
            },
        ),
        (
            "explicit.failure_domains",
            LocationForm::Explicit {
                sorted_symbol_inventory: vec![1, 2, 3],
                failure_domains: vec![7, 10],
            },
        ),
    ] {
        let mutation = PlacementDescriptor {
            location_form,
            ..explicit.clone()
        };
        assert_ne!(
            encoded.place(mutation.clone()).placement_id(),
            explicit_placed.placement_id(),
            "placement descriptor field {field} is outside PlacementId"
        );
        assert_eq!(
            encoded.verify_placement(&mutation, explicit_placed.placement_id(), &mut verification,),
            Err(IdentityMismatch::PlacementId),
            "durable recovery accepted a rewritten placement field {field}"
        );
    }

    let symbol = SymbolRecord::for_encoding(
        &encoded,
        1,
        42,
        0,
        (0..1280u32).map(|i| (i % 241) as u8).collect(),
    );
    let symbol_bytes = symbol.serialize(&encoded.symbol_auth_key(&dek()));
    assert_eq!(
        SymbolRecord::verify(&symbol_bytes, &encoded, &dek(), &mut verification)
            .expect("authentic symbol"),
        symbol
    );
    for offset in 0..symbol_bytes.len() {
        let mut corrupted = symbol_bytes.clone();
        corrupted[offset] ^= 0x01;
        assert!(
            SymbolRecord::verify(&corrupted, &encoded, &dek(), &mut verification).is_err(),
            "serialized symbol byte {offset} is outside framing checks and the MAC transcript"
        );
    }

    let recoded = protected.encode(encoding(2));
    let foreign_symbol = SymbolRecord::for_encoding(
        &recoded,
        1,
        42,
        0,
        (0..1280u32).map(|i| (i % 241) as u8).collect(),
    );
    let foreign_bytes = foreign_symbol.serialize(&recoded.symbol_auth_key(&dek()));
    assert_eq!(
        SymbolRecord::verify(&foreign_bytes, &encoded, &dek(), &mut verification),
        Err(SymbolError::ForeignEncoding),
        "a valid symbol from another EncodingId must not mix"
    );

    let mut wrong_dek = dek();
    wrong_dek[0] ^= 0x80;
    assert_eq!(
        SymbolRecord::verify(&symbol_bytes, &encoded, &wrong_dek, &mut verification),
        Err(SymbolError::AuthenticationFailed),
        "the symbol transcript must authenticate under the selected DEK"
    );
    let mut corrupted_symbol = symbol_bytes.clone();
    let payload_byte = corrupted_symbol.len() - 17;
    corrupted_symbol[payload_byte] ^= 0x01;
    assert_eq!(
        SymbolRecord::verify(&corrupted_symbol, &encoded, &dek(), &mut verification),
        Err(SymbolError::AuthenticationFailed),
        "a payload change must fail the symbol MAC"
    );

    let mut unsupported = cipher.clone();
    unsupported.data_crypto_profile = 2;
    assert_eq!(
        IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload()).protect(
            &dek(),
            unsupported.clone(),
            &compressed,
        ),
        Err(IdentityMismatch::UnsupportedDataCryptoProfile {
            data_crypto_profile: 2,
        })
    );
    assert_eq!(
        EncodedObject::reconstruct(
            encoded.object_id(),
            unsupported,
            encoded.ciphertext_id(),
            encoding(1),
            encoded.encoding_id(),
            &mut verification,
        ),
        Err(IdentityMismatch::UnsupportedDataCryptoProfile {
            data_crypto_profile: 2,
        }),
        "durable recovery must resolve the profile before accepting descriptors"
    );

    let domain = PackDomain {
        namespace: namespace(),
        tenant: 7,
        write_key: WriteKeyDomain::CommitStream,
        retention_class: 3,
    };
    let mut pack = PackBuilder::new(domain);
    pack.add(
        IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload()),
        domain,
    )
    .expect("the member belongs to the pack domain");
    assert!(
        matches!(
            pack.seal(
                &k_oid(),
                &dek(),
                PackProtectionProfile {
                    codec_profile: 1,
                    data_crypto_profile: 2,
                    dek_id: [7; 16],
                },
            ),
            Err(PackError::ProtectionProfile(
                IdentityMismatch::UnsupportedDataCryptoProfile {
                    data_crypto_profile: 2,
                }
            ))
        ),
        "the production pack seam must propagate profile refusal"
    );

    for bad_tag_len in [0, 15, 17, u16::MAX] {
        let mut invalid = cipher.clone();
        invalid.object_tag_len = bad_tag_len;
        assert_eq!(
            IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), &payload()).protect(
                &dek(),
                invalid.clone(),
                &compressed,
            ),
            Err(IdentityMismatch::ObjectTagLength {
                data_crypto_profile: 1,
                expected: 16,
                actual: bad_tag_len,
            }),
            "tag length {bad_tag_len} must be rejected before tag slicing"
        );
        assert_eq!(
            EncodedObject::reconstruct(
                encoded.object_id(),
                invalid,
                encoded.ciphertext_id(),
                encoding(1),
                encoded.encoding_id(),
                &mut verification,
            ),
            Err(IdentityMismatch::ObjectTagLength {
                data_crypto_profile: 1,
                expected: 16,
                actual: bad_tag_len,
            }),
            "durable recovery must reject tag length {bad_tag_len}"
        );
    }

    let bootstrap = root_bootstrap(&cipher);
    assert_eq!(
        bootstrap
            .cipher_descriptor()
            .expect("registered bootstrap profile"),
        cipher.clone(),
        "root bootstrap must preserve the complete admitted cipher descriptor"
    );
    for bad_nonce_len in [0, 23, 25] {
        let mut invalid = bootstrap.clone();
        invalid.nonce_len = bad_nonce_len;
        assert_eq!(
            invalid.cipher_descriptor(),
            Err(IdentityMismatch::ObjectNonceLength {
                data_crypto_profile: 1,
                expected: 24,
                actual: bad_nonce_len,
            }),
            "root bootstrap nonce length {bad_nonce_len} must not be discarded"
        );

        let slot = RootSlot {
            format_major: 1,
            format_minor: 0,
            slot_generation: 1,
            local_writer_fence_epoch: 1,
            database_id: [1; 16],
            database_security_namespace_id: namespace().0,
            cluster_incarnation: 1,
            incarnation_continuity_profile_id: 1,
            cluster_incarnation_continuity_digest: [2; 32],
            continuity_cas_version: 1,
            service_visibility_epoch: 1,
            root_manifest_oid: encoded.object_id().0,
            bootstrap: invalid,
        };
        assert_eq!(
            recover_root_object(
                &slot,
                &[],
                &k_oid(),
                &dek(),
                |_| slot.identity_tuple(),
                &mut verification,
            ),
            Err(RootRecoveryError::DescriptorMismatch(
                IdentityMismatch::ObjectNonceLength {
                    data_crypto_profile: 1,
                    expected: 24,
                    actual: bad_nonce_len,
                }
            )),
            "real root recovery must reject nonce length {bad_nonce_len} before symbol work"
        );
    }

    // Reconstructing with another well-formed descriptor is allowed only far
    // enough to let the AEAD authenticate the original ciphertext against the
    // substituted AAD; it must then fail without releasing plaintext.
    let mut substituted_cipher = cipher.clone();
    substituted_cipher.object_kind ^= 1;
    let substituted = EncodedObject::reconstruct(
        encoded.object_id(),
        substituted_cipher,
        encoded.ciphertext_id(),
        encoding(1),
        encoded.encoding_id(),
        &mut verification,
    )
    .expect("the encoding identity intentionally excludes the cipher descriptor");
    assert!(
        substituted
            .open_recovered(protected.protected_bytes(), &dek(), &mut verification)
            .is_err(),
        "descriptor substitution must fail the object-AEAD AAD"
    );

    let wrong_object =
        IdentifiedObject::new(&k_oid(), namespace(), 0x0002, header(), b"other payload");
    let substituted = EncodedObject::reconstruct(
        wrong_object.object_id(),
        cipher.clone(),
        encoded.ciphertext_id(),
        encoding(1),
        encoded.encoding_id(),
        &mut verification,
    )
    .expect("the encoding identity intentionally excludes logical identity");
    assert!(
        substituted
            .open_recovered(protected.protected_bytes(), &dek(), &mut verification)
            .is_err(),
        "logical-object substitution must fail the object-AEAD AAD"
    );

    let mut moved = placement(1);
    moved.placement_epoch += 1;
    assert_eq!(
        encoded.verify_placement(&moved, placed.placement_id(), &mut verification),
        Err(IdentityMismatch::PlacementId),
        "a durable placement must recompute against this EncodingId"
    );

    let accepted_open = CryptoVerificationEvent {
        profile_id: 1,
        object_kind: 0x0002,
        plaintext_len: 512,
        ciphertext_len: Some((compressed.len() + 16) as u64),
        encoding_id_prefix: None,
        operation: VerificationOperation::ObjectOpen,
        outcome: VerificationOutcome::Accepted,
    };
    assert!(
        verification.contains(&accepted_open),
        "the registered object-open path did not emit its exact success record: {verification:?}"
    );
    for (operation, failure) in [
        (
            VerificationOperation::EncodingReconstruction,
            VerificationFailureClass::UnsupportedDataCryptoProfile,
        ),
        (
            VerificationOperation::EncodingReconstruction,
            VerificationFailureClass::ObjectTagLength,
        ),
        (
            VerificationOperation::PlacementIdentity,
            VerificationFailureClass::PlacementIdentity,
        ),
        (
            VerificationOperation::SymbolRecord,
            VerificationFailureClass::ForeignEncoding,
        ),
        (
            VerificationOperation::SymbolRecord,
            VerificationFailureClass::Authentication,
        ),
        (
            VerificationOperation::RecoveredObjectOpen,
            VerificationFailureClass::CiphertextIdentity,
        ),
    ] {
        assert!(
            verification.iter().any(|event| {
                event.operation == operation && event.failure_class() == Some(failure)
            }),
            "typed verification failure {operation:?}/{failure:?} was not emitted"
        );
    }
    assert!(
        verification.iter().any(|event| {
            event.operation == VerificationOperation::SymbolRecord
                && event.encoding_id_prefix.is_some()
                && event.outcome == VerificationOutcome::Accepted
        }),
        "an accepted symbol did not carry its public EncodingId prefix"
    );
    let rendered = format!("{verification:?}");
    assert!(
        !rendered.contains(&format!("{:?}", dek())),
        "verification diagnostics exposed the DEK: {rendered}"
    );
    assert!(
        !rendered.contains(&format!("{:?}", cipher.object_nonce)),
        "verification diagnostics exposed the object nonce: {rendered}"
    );
    assert!(
        !rendered.contains("canonical-header") && !rendered.contains("payload"),
        "verification diagnostics exposed plaintext: {rendered}"
    );
}
