//! End-to-end self-sufficient root recovery.
//!
//! This is the property the whole durability story rests on: given ONLY a
//! `manifest.root` file at a fixed offset and the symbols on disk, a database
//! can be recovered — no index, no prior state, no side channel. Everything
//! needed to open the root is in the slot, which is what makes it safe for
//! everything else to be immutable and content-addressed.
//!
//! So this test builds a real root the way a publisher would (protect →
//! encode → place → symbolize), writes a real slot describing it, throws the
//! builders away, and recovers from the bytes alone.

use fgdb_chronicle::identity::{CipherDescriptor, EncodingDescriptor, IdentifiedObject};
use fgdb_chronicle::root::{
    IdentityTuple, NONCE_CAPACITY, OPENER_PAYLOAD_LEN, RootBootstrap, RootRecoveryError, RootSlot,
    recover_root_object,
};
use fgdb_chronicle::symbol::SymbolError;
use fgdb_chronicle::symbolize::{SymbolizeError, encode_object};
use fgdb_types::ids::DatabaseSecurityNamespaceId;

const SYMBOL_SIZE: u16 = 256;
const ROOT_KIND: u16 = 0x0001;
const HEADER: &[u8] = b"root-manifest-header";

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

const NAMESPACE_BYTES: [u8; 32] = {
    let mut bytes = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        bytes[i] = (i as u8) ^ 0x5a;
        i += 1;
    }
    bytes
};

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId(NAMESPACE_BYTES)
}

/// The root manifest's canonical payload. Its first bytes encode the identity
/// tuple, so a recovered root can be checked against the slot that named it —
/// standing in for the real `RootManifest` decode, whose union arms are still
/// in the G0 decision batch.
fn root_payload(cluster_incarnation: u64, visibility_epoch: u64) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&cluster_incarnation.to_be_bytes());
    payload.extend_from_slice(&visibility_epoch.to_be_bytes());
    payload.extend_from_slice(&(0..1024u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>());
    payload
}

/// Read the identity tuple back out of a recovered root, the way the real
/// manifest decoder will.
fn tuple_from_recovered(slot: &RootSlot) -> impl Fn(&[u8]) -> IdentityTuple + '_ {
    move |recovered: &[u8]| {
        let mut incarnation = [0u8; 8];
        let mut visibility = [0u8; 8];
        // The header rides in front of the payload in the canonical plaintext.
        let start = HEADER.len();
        incarnation.copy_from_slice(&recovered[start..start + 8]);
        visibility.copy_from_slice(&recovered[start + 8..start + 16]);
        IdentityTuple {
            cluster_incarnation: u64::from_be_bytes(incarnation),
            service_visibility_epoch: u64::from_be_bytes(visibility),
            ..slot.identity_tuple()
        }
    }
}

struct Published {
    slot: RootSlot,
    symbols: Vec<Vec<u8>>,
    payload: Vec<u8>,
}

/// Publish a root exactly as the write path will: identify, protect, encode,
/// place, symbolize — then describe all of it in a slot.
fn publish(cluster_incarnation: u64, visibility_epoch: u64, repair_symbols: u32) -> Published {
    let payload = root_payload(cluster_incarnation, visibility_epoch);
    let object = IdentifiedObject::new(K_OID, namespace(), ROOT_KIND, HEADER, &payload);
    let object_id = object.object_id();

    let cipher = CipherDescriptor {
        object_kind: ROOT_KIND,
        canonical_plaintext_len: (HEADER.len() + payload.len()) as u64,
        codec_profile: 1,
        compressed_len: (HEADER.len() + payload.len()) as u64,
        data_crypto_profile: 1,
        dek_id: [3u8; 16],
        object_nonce: core::array::from_fn(|i| (i as u8).wrapping_mul(5).wrapping_add(3)),
        object_tag_len: 16,
    };
    let mut canonical = Vec::from(HEADER);
    canonical.extend_from_slice(&payload);
    let protected = object.protect(&dek(), cipher.clone(), &canonical);
    let protected_len = protected.protected_bytes().len();

    let encoding_descriptor = EncodingDescriptor {
        fec_profile: 1,
        transfer_length: protected_len as u64,
        oti_common: 0x0001_0002_0003_0004,
        oti_scheme: 0x0005_0006,
        symbol_size: SYMBOL_SIZE,
        source_block_count: 1,
        symbol_auth_profile: 1,
    };
    let encoded = protected.encode(encoding_descriptor.clone());
    let symbols = encode_object(
        &encoded,
        protected.protected_bytes(),
        ROOT_KIND,
        0,
        repair_symbols,
        &dek(),
    )
    .expect("root symbolization");

    let inventory_digest = fgdb_crypto::hash(b"root symbol inventory");
    let bootstrap = RootBootstrap {
        root_encoding_id: encoded.encoding_id().0,
        // Filled in below once the placement descriptor is known.
        root_placement_id: [0u8; 32],
        root_placement_epoch: 1,
        failure_domain_policy_id: 2,
        root_failure_domain_id: 7,
        segment_id: 11,
        offset: 0,
        encoded_len: protected_len as u64,
        root_symbol_inventory_digest: inventory_digest.0,
        object_kind: cipher.object_kind,
        canonical_plaintext_len: cipher.canonical_plaintext_len,
        codec_profile: cipher.codec_profile,
        compressed_len: cipher.compressed_len,
        data_crypto_profile: cipher.data_crypto_profile,
        dek_id: cipher.dek_id,
        nonce_len: NONCE_CAPACITY as u16,
        nonce_or_siv: cipher.object_nonce,
        object_tag_len: cipher.object_tag_len,
        fec_profile: encoding_descriptor.fec_profile,
        transfer_length: encoding_descriptor.transfer_length,
        oti_common: encoding_descriptor.oti_common,
        oti_scheme: encoding_descriptor.oti_scheme,
        symbol_size: encoding_descriptor.symbol_size,
        source_block_count: encoding_descriptor.source_block_count,
        symbol_auth_profile: encoding_descriptor.symbol_auth_profile,
        ciphertext_id: encoded.ciphertext_id().0,
        ciphertext_digest: fgdb_crypto::hash(protected.protected_bytes()).0,
        opener_kind: 1,
        oid_key_id: [4u8; 16],
        opener_payload_len: 32,
        opener_payload: {
            let mut bundle = [0u8; OPENER_PAYLOAD_LEN];
            bundle[..32].copy_from_slice(&[0x5cu8; 32]);
            bundle
        },
        opener_digest: fgdb_crypto::hash(b"opener bundle").0,
    };

    let mut slot = RootSlot {
        format_major: 1,
        format_minor: 0,
        slot_generation: 1,
        local_writer_fence_epoch: 3,
        database_id: [0xaa; 16],
        database_security_namespace_id: NAMESPACE_BYTES,
        cluster_incarnation,
        incarnation_continuity_profile_id: 1,
        cluster_incarnation_continuity_digest: [0xc3; 32],
        continuity_cas_version: 12,
        service_visibility_epoch: visibility_epoch,
        root_manifest_oid: object_id.0,
        bootstrap,
    };
    // The placement id is the digest of its own descriptor against this
    // encoding — computed the same way recovery will recompute it.
    let placement = slot.bootstrap.placement_descriptor();
    let placed = encoded.place(placement);
    slot.bootstrap.root_placement_id = placed.placement_id().0;

    Published {
        slot,
        symbols,
        payload,
    }
}

/// THE PROPERTY EVERYTHING RESTS ON: a database recovers from a fixed offset
/// with no index and no prior state.
#[test]
fn a_root_recovers_from_its_slot_and_symbols_alone() {
    let published = publish(4, 5, 8);
    let recovered = recover_root_object(
        &published.slot,
        &published.symbols,
        K_OID,
        &dek(),
        tuple_from_recovered(&published.slot),
    )
    .expect("a well-formed root must recover from its slot alone");

    let mut expected = Vec::from(HEADER);
    expected.extend_from_slice(&published.payload);
    assert_eq!(recovered, expected, "recovery must be byte-exact");
}

/// Recovery survives symbol loss exactly as any other object does — the root
/// is not a special case that skips erasure coding.
#[test]
fn root_recovery_survives_symbol_loss_within_the_budget() {
    let published = publish(4, 5, 8);
    let surviving: Vec<Vec<u8>> = published
        .symbols
        .iter()
        .enumerate()
        .filter(|(index, _)| !matches!(index, 0 | 2 | 5))
        .map(|(_, symbol)| symbol.clone())
        .collect();

    assert!(
        recover_root_object(
            &published.slot,
            &surviving,
            K_OID,
            &dek(),
            tuple_from_recovered(&published.slot),
        )
        .is_ok(),
        "three losses inside an eight-symbol budget must recover"
    );
}

/// A REWRITTEN DESCRIPTOR IS CAUGHT BEFORE ANY SYMBOL IS TRUSTED. Editing the
/// declared EncodingId — or any field the EncodingId is computed over — must
/// fail at the descriptor check, not later and not never.
#[test]
fn a_rewritten_encoding_descriptor_is_refused_before_decoding() {
    let published = publish(4, 5, 8);

    let mut tampered = published.slot.clone();
    tampered.bootstrap.root_encoding_id[0] ^= 0x01;
    assert!(
        matches!(
            recover_root_object(
                &tampered,
                &published.symbols,
                K_OID,
                &dek(),
                tuple_from_recovered(&published.slot),
            ),
            Err(RootRecoveryError::DescriptorMismatch(_))
        ),
        "a declared EncodingId that does not recompute must be refused"
    );

    // Editing a field the EncodingId covers is the same defect from the other
    // direction: the descriptor no longer digests to its declared identity.
    let mut retuned = published.slot.clone();
    retuned.bootstrap.symbol_size = 512;
    assert!(matches!(
        recover_root_object(
            &retuned,
            &published.symbols,
            K_OID,
            &dek(),
            tuple_from_recovered(&published.slot),
        ),
        Err(RootRecoveryError::DescriptorMismatch(_))
    ));
}

/// A rewritten PLACEMENT is how recovery gets pointed at another object's
/// bytes, so it is verified too — and before any symbol is read.
#[test]
fn a_rewritten_placement_descriptor_is_refused() {
    let published = publish(4, 5, 8);
    let mut tampered = published.slot.clone();
    tampered.bootstrap.segment_id = 999;
    assert!(
        matches!(
            recover_root_object(
                &tampered,
                &published.symbols,
                K_OID,
                &dek(),
                tuple_from_recovered(&published.slot),
            ),
            Err(RootRecoveryError::DescriptorMismatch(_))
        ),
        "a placement whose id no longer recomputes must be refused"
    );
}

/// A slot naming a different root cannot adopt these bytes — and WHICH layer
/// catches it is instructive.
///
/// MEASURED: the rejection arrives as `ForeignEncoding`, not
/// `IdentityMismatch`, because every symbol carries the logical OID it belongs
/// to and `SymbolRecord::verify` binds it against the reconstructed encoding.
/// A relabelled slot is therefore refused BEFORE any decode runs — one layer
/// earlier than the keyed-ObjectId recomputation that would also have caught
/// it. Two independent layers reject it; this pins the outer one, so a future
/// change that drops the symbol binding cannot pass silently on the strength
/// of the inner one.
#[test]
fn a_slot_naming_a_different_root_cannot_adopt_these_bytes() {
    let published = publish(4, 5, 8);
    let mut tampered = published.slot.clone();
    tampered.root_manifest_oid[0] ^= 0x01;
    assert_eq!(
        recover_root_object(
            &tampered,
            &published.symbols,
            K_OID,
            &dek(),
            tuple_from_recovered(&published.slot),
        ),
        Err(RootRecoveryError::Recovery(SymbolizeError::Symbol(
            SymbolError::ForeignEncoding
        )))
    );
}

/// THE LAST CHECK, and the one that stops a perfectly valid foreign root:
/// the recovered root's own authenticated identity tuple must agree with the
/// slot that pointed at it. Here the root was published for one incarnation
/// and the slot claims another — every byte is authentic, and it is still
/// refused.
#[test]
fn a_root_from_another_incarnation_is_refused_even_though_authentic() {
    let published = publish(4, 5, 8);
    let mut lying_slot = published.slot.clone();
    lying_slot.cluster_incarnation = 99;

    assert_eq!(
        recover_root_object(
            &lying_slot,
            &published.symbols,
            K_OID,
            &dek(),
            tuple_from_recovered(&lying_slot),
        ),
        Err(RootRecoveryError::IdentityTupleMismatch),
        "an authentic root from another incarnation must never be adopted"
    );
}

/// The same law on the visibility epoch: a root that predates the slot's
/// declared service visibility is refused rather than silently serving.
#[test]
fn a_root_from_another_visibility_epoch_is_refused() {
    let published = publish(4, 5, 8);
    let mut lying_slot = published.slot.clone();
    lying_slot.service_visibility_epoch = 77;
    assert_eq!(
        recover_root_object(
            &lying_slot,
            &published.symbols,
            K_OID,
            &dek(),
            tuple_from_recovered(&lying_slot),
        ),
        Err(RootRecoveryError::IdentityTupleMismatch)
    );
}

/// Beyond the repair budget recovery fails closed — the root is not exempt
/// from the fail-closed rule that governs every other object.
#[test]
fn root_recovery_beyond_the_budget_fails_closed() {
    let published = publish(4, 5, 1);
    let survivors: Vec<Vec<u8>> = published.symbols.iter().take(2).cloned().collect();
    assert!(
        recover_root_object(
            &published.slot,
            &survivors,
            K_OID,
            &dek(),
            tuple_from_recovered(&published.slot),
        )
        .is_err(),
        "too few symbols must fail closed, never return partial bytes"
    );
}

/// The wrong database identity key cannot open the root: recovery is keyed,
/// so a stolen disk without `K_oid` yields nothing.
#[test]
fn the_wrong_identity_key_recovers_nothing() {
    let published = publish(4, 5, 8);
    let mut wrong = K_OID_BYTES;
    wrong[0] ^= 0xff;
    assert!(
        recover_root_object(
            &published.slot,
            &published.symbols,
            &wrong,
            &dek(),
            tuple_from_recovered(&published.slot),
        )
        .is_err(),
        "recovery without the database identity key must fail"
    );
}
