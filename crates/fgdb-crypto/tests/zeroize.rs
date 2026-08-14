//! Zeroization contract tests (bead fgdb-w1-crypto-y5o, increment 4).
//!
//! **WHAT A ZEROIZATION TEST CAN HONESTLY ASSERT.** It cannot read the bytes
//! after the drop: the storage is gone, and reaching it would be undefined
//! behaviour in a crate that forbids `unsafe` outright. Every test here is
//! therefore written against something actually observable, and the one claim
//! that is NOT observable is named rather than faked.
//!
//! The interesting case is "somebody deletes the `Drop` impl". No behavioural
//! test can see that — a `Secret` without a destructor behaves identically in
//! every observable way. `core::mem::needs_drop` can, and that is what
//! `the_destructor_still_exists` is for. Without it, the entire scrub could be
//! removed and this file would stay green, which is precisely the
//! green-over-nothing shape the workspace keeps finding.
//!
//! The fence has no observable language-level effect, so this file cannot prove
//! it survives optimization. The separate live `w1_crypto_codegen_e2e` gate
//! supplies that deliberately narrower instrument: on the pinned supported
//! host/toolchain it requires the release object to retain unconditional
//! memset-targeted byte and word boundaries. That remains measured artifact
//! evidence, not a portable Rust-language zeroization theorem.

use fgdb_crypto::zeroize::{Secret, scrub_slice, scrub_words};

/// Scrubbing zeroes the bytes. The directly observable half of the contract.
#[test]
fn scrub_zeroes_the_bytes() {
    let mut secret = Secret::new([0xa5u8; 32]);
    assert_eq!(
        secret.expose(),
        &[0xa5u8; 32],
        "the fixture must start nonzero"
    );

    secret.scrub();

    assert_eq!(
        secret.expose(),
        &[0u8; 32],
        "scrub left key material in the buffer"
    );
}

/// THE MUTATION-CATCHABLE TEST: the destructor exists.
///
/// A `Secret` is a plain byte array plus a `Drop` impl. Delete the impl and
/// nothing observable changes — no value differs, no API moves — so every other
/// test in this file stays green while the type silently stops scrubbing.
/// `needs_drop` is the one probe that distinguishes them.
#[test]
fn the_destructor_still_exists() {
    assert!(
        core::mem::needs_drop::<Secret<32>>(),
        "Secret has no destructor, so nothing scrubs it at end of scope — the \
         zeroization guarantee is gone even though every behavioural test passes"
    );
    // CONTROL: the probe can return false, so the assertion above is not
    // vacuously true of every type.
    assert!(
        !core::mem::needs_drop::<[u8; 32]>(),
        "a bare array must not need drop, or needs_drop cannot discriminate"
    );
}

/// A secret must not print itself.
///
/// The bead's logging rule is explicit — verify paths log a failure class and
/// "never keys, nonces, or plaintext" — and a derived `Debug` is the usual way
/// that rule gets broken without anyone deciding to break it.
#[test]
fn debug_redacts_the_material() {
    let secret = Secret::new([0xde_u8; 16]);
    let rendered = format!("{secret:?}");

    assert!(
        !rendered.contains("222") && !rendered.to_lowercase().contains("de"),
        "Debug rendered the key material: {rendered}"
    );
    assert!(
        rendered.contains("redacted"),
        "Debug should say it redacted something, got {rendered}"
    );
}

/// The in-place fill path: a primitive writes through `expose_mut`, and the
/// same scrub still applies.
#[test]
fn a_filled_secret_scrubs_the_same_way() {
    let mut secret = Secret::<64>::zeroed();
    secret.expose_mut().fill(0x7f);
    assert_eq!(secret.expose()[0], 0x7f, "the fill must land");

    secret.scrub();
    // Compared as a whole array rather than byte-by-byte: `ubs` reads
    // `*b == 0` over a value named `secret` as a non-constant-time secret
    // comparison. This is an all-zeros check on already-scrubbed bytes, but the
    // whole-array form says the same thing and leaves no false positive to
    // waive — and a waiver on a crypto file is the artifact worth avoiding.
    assert_eq!(secret.expose(), &[0u8; 64], "a filled secret did not scrub");
}

/// The loose-buffer helper, for material this crate does not own the type of.
#[test]
fn scrub_slice_zeroes_a_caller_owned_buffer() {
    let mut plaintext = vec![0x11u8; 100];
    scrub_slice(&mut plaintext);
    assert!(
        plaintext.iter().all(|b| *b == 0),
        "scrub_slice left bytes behind"
    );

    // Empty and single-byte buffers must not panic on the boundary.
    scrub_slice(&mut []);
    let mut one = [0x22u8; 1];
    scrub_slice(&mut one);
    assert_eq!(one, [0u8; 1]);
}

/// The word-storage boundary used by Argon2 and BLAKE2b has the same directly
/// observable overwrite behavior, including boundary lengths.
#[test]
fn scrub_words_zeroes_caller_owned_state() {
    let mut state = vec![0xfeed_face_dead_beef_u64; 17];
    scrub_words(&mut state);
    assert!(
        state.iter().all(|word| *word == 0),
        "scrub_words left derived state behind"
    );

    scrub_words(&mut []);
    let mut one = [u64::MAX];
    scrub_words(&mut one);
    assert_eq!(one, [0]);
}

/// Scrubbing twice is legal and idempotent — `scrub` then `drop` is the normal
/// path when a caller releases a key early, and it must not be a special case.
#[test]
fn scrubbing_twice_is_idempotent() {
    let mut secret = Secret::new([0x5au8; 32]);
    secret.scrub();
    secret.scrub();
    assert_eq!(secret.expose(), &[0u8; 32]);
    // And the implicit drop that follows scrubs a third time without incident.
}
