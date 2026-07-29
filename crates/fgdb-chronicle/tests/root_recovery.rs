//! Root-slot and recovery-rule laws.
//!
//! `manifest.root` is the only mutable object in a database directory, so its
//! recovery rule is the single point where Chronicle can lose data by being
//! clever. The rule's sharpest clause is a NEGATIVE one — never silently roll
//! back to an older authenticated state — and a rule that only ever moves
//! forward has to be tested from the direction it refuses to go.

use fgdb_chronicle::root::{
    NONCE_CAPACITY, OPENER_PAYLOAD_LEN, ROOT_FILE_LEN, RootBootstrap, RootSelection, RootSlot,
    SLOT_A_OFFSET, SLOT_B_OFFSET, SLOT_LEN, SlotError, select_root,
};

fn bootstrap(seed: u8) -> RootBootstrap {
    RootBootstrap {
        root_encoding_id: [seed; 32],
        root_placement_id: [seed.wrapping_add(1); 32],
        root_placement_epoch: 9,
        failure_domain_policy_id: 2,
        root_failure_domain_id: 7,
        segment_id: 11,
        offset: 4096,
        encoded_len: 8192,
        root_symbol_inventory_digest: [seed.wrapping_add(2); 32],
        object_kind: 0x0001,
        canonical_plaintext_len: 512,
        codec_profile: 1,
        compressed_len: 512,
        data_crypto_profile: 1,
        dek_id: [seed.wrapping_add(3); 16],
        nonce_len: 24,
        nonce_or_siv: [seed.wrapping_add(4); NONCE_CAPACITY],
        object_tag_len: 16,
        fec_profile: 1,
        transfer_length: 4096,
        oti_common: 0x0001_0002_0003_0004,
        oti_scheme: 0x0005_0006,
        symbol_size: 256,
        source_block_count: 1,
        symbol_auth_profile: 1,
        ciphertext_id: [seed.wrapping_add(5); 32],
        ciphertext_digest: [seed.wrapping_add(6); 32],
        opener_kind: 1,
        oid_key_id: [seed.wrapping_add(7); 16],
        opener_payload_len: 64,
        opener_payload: {
            let mut payload = [0u8; OPENER_PAYLOAD_LEN];
            for (index, byte) in payload.iter_mut().take(64).enumerate() {
                *byte = (index as u8).wrapping_add(seed);
            }
            payload
        },
        opener_digest: [seed.wrapping_add(8); 32],
    }
}

fn slot(generation: u64, seed: u8) -> RootSlot {
    RootSlot {
        format_major: 1,
        format_minor: 0,
        slot_generation: generation,
        local_writer_fence_epoch: 3,
        database_id: [0xaa; 16],
        database_security_namespace_id: [0x5a; 32],
        cluster_incarnation: 4,
        incarnation_continuity_profile_id: 1,
        cluster_incarnation_continuity_digest: [0xc3; 32],
        continuity_cas_version: 12,
        service_visibility_epoch: 5,
        root_manifest_oid: [0x77; 32],
        bootstrap: bootstrap(seed),
    }
}

/// Project the `Selected` arm, or `None`. Returning an Option keeps the tests
/// free of both a diverging helper and a `panic!` macro: the diagnostic rides
/// on `expect`, which names the unexpected selection just as well.
fn selected(selection: &RootSelection) -> Option<(&RootSlot, u8, Option<SlotError>)> {
    match selection {
        RootSelection::Selected {
            slot,
            index,
            other_rejected,
        } => Some((slot, *index, *other_rejected)),
        _ => None,
    }
}

/// Project the `IdenticalPair` arm, or `None`.
fn identical_pair(selection: &RootSelection) -> Option<&RootSlot> {
    match selection {
        RootSelection::IdenticalPair { slot } => Some(slot),
        _ => None,
    }
}

/// Build a two-slot file from optional slot bytes; `None` leaves zeroes,
/// which is what an uninitialised or wiped slot looks like.
fn root_file(a: Option<[u8; SLOT_LEN]>, b: Option<[u8; SLOT_LEN]>) -> Vec<u8> {
    let mut file = vec![0u8; ROOT_FILE_LEN];
    if let Some(bytes) = a {
        file[SLOT_A_OFFSET..SLOT_A_OFFSET + SLOT_LEN].copy_from_slice(&bytes);
    }
    if let Some(bytes) = b {
        file[SLOT_B_OFFSET..SLOT_B_OFFSET + SLOT_LEN].copy_from_slice(&bytes);
    }
    file
}

#[test]
fn a_slot_round_trips_through_exactly_4096_bytes() {
    let original = slot(7, 1);
    let bytes = original.serialize();
    assert_eq!(bytes.len(), SLOT_LEN);
    let parsed = RootSlot::parse(&bytes).expect("a freshly written slot must parse");
    assert_eq!(parsed, original, "every field must survive the round trip");
    assert_eq!(parsed.identity_tuple(), original.identity_tuple());
}

/// THE TEAR CHECKSUM COVERS EVERYTHING. Flip one bit anywhere in the slot —
/// header, any bootstrap field, reserved padding, or the checksum itself — and
/// the slot must be discarded. A field outside the checksum is a field a torn
/// write can silently change.
#[test]
fn the_tear_checksum_covers_every_byte_of_the_slot() {
    let bytes = slot(7, 1).serialize();
    for offset in 0..SLOT_LEN {
        let mut corrupted = bytes;
        corrupted[offset] ^= 0x01;
        assert!(
            RootSlot::parse(&corrupted).is_err(),
            "byte {offset} is outside the tear checksum or the framing checks"
        );
    }
}

/// A torn write destroys at most the slot being written; the other still
/// recovers the database. This is the whole reason there are two slots.
#[test]
fn a_torn_slot_is_discarded_and_the_other_recovers() {
    let good = slot(9, 1).serialize();
    let mut torn = slot(10, 2).serialize();
    torn[2048] ^= 0xff; // mid-slot tear, as a partial sector write leaves

    let file = root_file(Some(good), Some(torn));
    let selection = select_root(&file);
    let (slot, index, other_rejected) =
        selected(&selection).expect("a credible slot must be selected");
    assert_eq!(slot.slot_generation, 9, "the surviving slot recovers");
    assert_eq!(index, 0);
    assert_eq!(
        other_rejected,
        Some(SlotError::TearDetected),
        "the operator must be told the last write was torn"
    );
}

/// Step 2 of the rule: the highest generation wins, from either physical slot.
#[test]
fn the_highest_generation_wins_from_either_slot() {
    let older = slot(4, 1).serialize();
    let newer = slot(5, 2).serialize();

    for (file, expected_index) in [
        (root_file(Some(newer), Some(older)), 0u8),
        (root_file(Some(older), Some(newer)), 1u8),
    ] {
        let selection = select_root(&file);
        let (slot, index, _) = selected(&selection).expect("the newer slot must be selected");
        assert_eq!(slot.slot_generation, 5);
        assert_eq!(index, expected_index);
    }
}

/// THE NEGATIVE CLAUSE, and the one that protects acknowledged commits: when
/// the NEWEST slot is structurally credible, recovery must select it. It must
/// never prefer an older authenticated state — that state may predate a commit
/// the database already acknowledged, so silently rolling back is data loss
/// wearing the costume of recovery.
#[test]
fn recovery_never_rolls_back_to_an_older_credible_slot() {
    for newer_generation in [1u64, 2, 17, u64::MAX / 2, u64::MAX] {
        let older = slot(0, 1).serialize();
        let newer = slot(newer_generation, 2).serialize();
        let file = root_file(Some(older), Some(newer));
        let selection = select_root(&file);
        let (slot, _, _) = selected(&selection).expect("the newest slot must be selected");
        assert_eq!(
            slot.slot_generation, newer_generation,
            "recovery must move forward only"
        );
    }
}

/// Step 4: equal generations are acceptable for read recovery ONLY when the
/// complete authenticated bytes are identical.
#[test]
fn an_identical_equal_generation_pair_is_readable() {
    let bytes = slot(6, 1).serialize();
    let selection = select_root(&root_file(Some(bytes), Some(bytes)));
    let slot = identical_pair(&selection).expect("an identical pair is readable");
    assert_eq!(slot.slot_generation, 6);
}

/// Two credible slots at the same generation that DISAGREE fail closed. There
/// is no rule that can choose between two equally-current published roots, and
/// guessing risks discarding an acknowledged commit.
#[test]
fn a_divergent_equal_generation_pair_fails_closed() {
    let one = slot(6, 1).serialize();
    let other = slot(6, 99).serialize(); // same generation, different bootstrap
    assert_ne!(one, other);
    assert_eq!(
        select_root(&root_file(Some(one), Some(other))),
        RootSelection::DivergentPair { generation: 6 }
    );
}

/// Recovery never invents a root: with no credible slot it fails closed and
/// reports why each slot was rejected.
#[test]
fn no_credible_slot_fails_closed() {
    // Two zeroed slots — an uninitialised or wiped file.
    assert_eq!(
        select_root(&root_file(None, None)),
        RootSelection::NoCredibleSlot {
            slot_a: SlotError::UnsupportedFraming,
            slot_b: SlotError::UnsupportedFraming,
        }
    );

    // Both torn.
    let mut a = slot(1, 1).serialize();
    let mut b = slot(2, 2).serialize();
    a[100] ^= 0x01;
    b[100] ^= 0x01;
    assert_eq!(
        select_root(&root_file(Some(a), Some(b))),
        RootSelection::NoCredibleSlot {
            slot_a: SlotError::TearDetected,
            slot_b: SlotError::TearDetected,
        }
    );

    // A file that is not even two slots long.
    assert!(matches!(
        select_root(&[0u8; 100]),
        RootSelection::NoCredibleSlot { .. }
    ));
}

/// Unknown framing is rejected, never guessed at: a foreign magic or a
/// different format MAJOR must not be parsed as if it were understood.
#[test]
fn unknown_framing_is_rejected() {
    let mut wrong_magic = slot(1, 1).serialize();
    wrong_magic[0] = b'X';
    assert_eq!(
        RootSlot::parse(&wrong_magic),
        Err(SlotError::UnsupportedFraming)
    );

    let mut future_major = slot(1, 1);
    future_major.format_major = 2;
    assert_eq!(
        RootSlot::parse(&future_major.serialize()),
        Err(SlotError::UnsupportedFraming),
        "a breaking-major root must not be read by this build"
    );

    // A higher MINOR is still readable: additive-minor is the durable-format
    // contract, and refusing it would make every additive change breaking.
    let mut future_minor = slot(1, 1);
    future_minor.format_minor = 7;
    assert_eq!(
        RootSlot::parse(&future_minor.serialize())
            .expect("an additive-minor root stays readable")
            .format_minor,
        7
    );
}

/// A declared inline length beyond its fixed capacity is a read primitive
/// pointed off the end of the record. Rejected structurally.
#[test]
fn an_impossible_inline_length_is_rejected() {
    let mut oversized = slot(1, 1);
    oversized.bootstrap.opener_payload_len = (OPENER_PAYLOAD_LEN + 1) as u16;
    assert_eq!(
        RootSlot::parse(&oversized.serialize()),
        Err(SlotError::InconsistentLengths)
    );

    let mut long_nonce = slot(1, 1);
    long_nonce.bootstrap.nonce_len = (NONCE_CAPACITY + 1) as u16;
    assert_eq!(
        RootSlot::parse(&long_nonce.serialize()),
        Err(SlotError::InconsistentLengths)
    );
}

/// The bootstrap descriptor is self-sufficient: everything needed to open the
/// root is IN the slot, so recovery never consults the index the root itself
/// must bootstrap.
#[test]
fn the_bootstrap_descriptor_is_self_sufficient() {
    let parsed = RootSlot::parse(&slot(3, 1).serialize()).expect("parses");
    let b = &parsed.bootstrap;

    // Cipher descriptor inputs.
    assert_ne!(b.dek_id, [0u8; 16]);
    assert!(b.nonce_len > 0 && b.object_tag_len > 0);
    assert!(b.canonical_plaintext_len > 0 && b.compressed_len > 0);
    // Encoding descriptor inputs.
    assert_ne!(b.ciphertext_id, [0u8; 32]);
    assert!(b.symbol_size > 0 && b.source_block_count > 0 && b.transfer_length > 0);
    // Placement inputs — a fully described contiguous span.
    assert!(b.encoded_len > 0);
    assert_ne!(b.root_symbol_inventory_digest, [0u8; 32]);
    // Key recovery: the opener bundle recovers both the root DEK and K_oid.
    assert!(b.opener_payload_len > 0);
    assert_ne!(b.oid_key_id, [0u8; 16]);
    assert_ne!(b.opener_digest, [0u8; 32]);
    // And the root's own identity, which the opened object must match.
    assert_ne!(parsed.root_manifest_oid, [0u8; 32]);
}

/// Serialization is deterministic, so republishing identical state produces
/// identical bytes — which is what makes the identical-pair rule meaningful.
#[test]
fn serialization_is_deterministic() {
    assert_eq!(slot(2, 5).serialize(), slot(2, 5).serialize());
}
