//! ChaCha20-Poly1305 (RFC 8439 §2.8) and XChaCha20-Poly1305 (the 24-byte
//! nonce extension via HChaCha20), plus the §5.1 object-AEAD surface.
//!
//! The object AEAD is the ONE object-level AEAD operation of plan L280: a
//! random per-ciphertext DEK, AAD transcript
//! `("fgdb:object-aead:v1", ObjectId, CipherDescriptorWithoutDigest)`. The
//! transcript helper owns that concatenation exactly as the identity helpers
//! own theirs.
//!
//! Decryption is authenticate-then-decrypt: the tag is verified over the
//! ciphertext before any plaintext byte is produced, and failure returns an
//! error carrying nothing.

use crate::chacha20;
use crate::poly1305::Poly1305;

/// Authentication failure. Deliberately carries no detail: a decrypt error
/// must not become a padding-oracle side channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AeadError;

impl core::fmt::Display for AeadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AEAD authentication failed")
    }
}

impl core::error::Error for AeadError {}

const TAG_LEN: usize = 16;

fn compute_tag(otk: &[u8; 32], aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
    let mut mac = Poly1305::new(otk);
    mac.update(aad);
    mac.update(&[0u8; 16][..(16 - aad.len() % 16) % 16]);
    mac.update(ciphertext);
    mac.update(&[0u8; 16][..(16 - ciphertext.len() % 16) % 16]);
    let mut lengths = [0u8; 16];
    lengths[..8].copy_from_slice(&(aad.len() as u64).to_le_bytes());
    lengths[8..].copy_from_slice(&(ciphertext.len() as u64).to_le_bytes());
    mac.update(&lengths);
    mac.finalize()
}

#[inline]
fn constant_time_eq16(a: &[u8; 16], b: &[u8; 16]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// RFC 8439 seal: returns ciphertext || 16-byte tag.
pub fn chacha20poly1305_seal(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    plaintext: &[u8],
) -> Vec<u8> {
    let otk = chacha20::poly1305_key(key, nonce);
    let mut out = Vec::with_capacity(plaintext.len() + TAG_LEN);
    out.extend_from_slice(plaintext);
    chacha20::xor_stream(key, 1, nonce, &mut out);
    let tag = compute_tag(&otk, aad, &out);
    out.extend_from_slice(&tag);
    out
}

/// RFC 8439 open: verifies the tag over the ciphertext, then decrypts.
pub fn chacha20poly1305_open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    ciphertext_and_tag: &[u8],
) -> Result<Vec<u8>, AeadError> {
    if ciphertext_and_tag.len() < TAG_LEN {
        return Err(AeadError);
    }
    let split = ciphertext_and_tag.len() - TAG_LEN;
    let (ciphertext, tag_bytes) = ciphertext_and_tag.split_at(split);
    let mut expected_tag = [0u8; 16];
    expected_tag.copy_from_slice(tag_bytes);

    let otk = chacha20::poly1305_key(key, nonce);
    let tag = compute_tag(&otk, aad, ciphertext);
    if !constant_time_eq16(&tag, &expected_tag) {
        return Err(AeadError);
    }
    let mut plaintext = ciphertext.to_vec();
    chacha20::xor_stream(key, 1, nonce, &mut plaintext);
    Ok(plaintext)
}

/// XChaCha20-Poly1305 subkey/nonce derivation: HChaCha20 over the first 16
/// nonce bytes yields the subkey; the last 8 nonce bytes become the tail of a
/// 12-byte nonce whose first 4 bytes are zero.
fn xchacha_subparts(key: &[u8; 32], nonce24: &[u8; 24]) -> ([u8; 32], [u8; 12]) {
    let mut nonce16 = [0u8; 16];
    nonce16.copy_from_slice(&nonce24[..16]);
    let subkey = chacha20::hchacha20(key, &nonce16);
    let mut subnonce = [0u8; 12];
    subnonce[4..].copy_from_slice(&nonce24[16..]);
    (subkey, subnonce)
}

/// XChaCha20-Poly1305 seal (24-byte nonce — the at-rest object AEAD profile,
/// README `at_rest_encryption` chain).
pub fn xchacha20poly1305_seal(
    key: &[u8; 32],
    nonce: &[u8; 24],
    aad: &[u8],
    plaintext: &[u8],
) -> Vec<u8> {
    let (subkey, subnonce) = xchacha_subparts(key, nonce);
    chacha20poly1305_seal(&subkey, &subnonce, aad, plaintext)
}

/// XChaCha20-Poly1305 open.
pub fn xchacha20poly1305_open(
    key: &[u8; 32],
    nonce: &[u8; 24],
    aad: &[u8],
    ciphertext_and_tag: &[u8],
) -> Result<Vec<u8>, AeadError> {
    let (subkey, subnonce) = xchacha_subparts(key, nonce);
    chacha20poly1305_open(&subkey, &subnonce, aad, ciphertext_and_tag)
}

/// The §5.1 object-AEAD AAD domain string (plan L280).
pub const OBJECT_AEAD_DOMAIN: &[u8] = b"fgdb:object-aead:v1";

/// The §5.1 object-AEAD AAD transcript:
/// `("fgdb:object-aead:v1", ObjectId, CipherDescriptorWithoutDigest)` —
/// owned in one place so W2 call sites cannot drift the order.
pub fn object_aead_aad(object_id: &crate::Digest, canonical_cipher_descriptor: &[u8]) -> Vec<u8> {
    let mut aad =
        Vec::with_capacity(OBJECT_AEAD_DOMAIN.len() + 32 + canonical_cipher_descriptor.len());
    aad.extend_from_slice(OBJECT_AEAD_DOMAIN);
    aad.extend_from_slice(&object_id.0);
    aad.extend_from_slice(canonical_cipher_descriptor);
    aad
}
