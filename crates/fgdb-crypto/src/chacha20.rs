//! Portable scalar ChaCha20 (RFC 8439) and the HChaCha20 subkey function
//! (draft-irtf-cfrg-xchacha) used by the XChaCha20 nonce extension.
//!
//! Verified against golden vectors generated from the audited RustCrypto
//! implementation by the dev-time oracle (`tests/aead_vectors.rs` records the
//! provenance).
//!
//! Key words, working states, keystream blocks, Poly1305 one-time keys, and
//! XChaCha subkeys remain inside non-cloneable scrub-on-drop owners. This
//! erases their original safe-Rust storage before ordinary drop; it does not
//! claim to erase compiler-created register/spill copies or OS crash remnants.

use crate::zeroize::{Secret, scrub_words32};

const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// Original `u32` storage for key-derived ChaCha state.
///
/// Private and intentionally non-`Clone`: the algorithm makes the one explicit
/// initial-state copy it needs, and both owners scrub through `Drop`.
struct SensitiveWords32<const N: usize>([u32; N]);

impl<const N: usize> Drop for SensitiveWords32<N> {
    fn drop(&mut self) {
        scrub_words32(&mut self.0);
    }
}

#[inline(always)]
fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

#[inline(always)]
fn double_round(state: &mut [u32; 16]) {
    quarter_round(state, 0, 4, 8, 12);
    quarter_round(state, 1, 5, 9, 13);
    quarter_round(state, 2, 6, 10, 14);
    quarter_round(state, 3, 7, 11, 15);
    quarter_round(state, 0, 5, 10, 15);
    quarter_round(state, 1, 6, 11, 12);
    quarter_round(state, 2, 7, 8, 13);
    quarter_round(state, 3, 4, 9, 14);
}

fn key_words(key: &[u8; 32]) -> SensitiveWords32<8> {
    let mut words = SensitiveWords32([0u32; 8]);
    for (i, word) in words.0.iter_mut().enumerate() {
        *word = u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    words
}

/// One 64-byte ChaCha20 keystream block (RFC 8439 §2.3) for a 96-bit nonce
/// and 32-bit block counter.
fn block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> Secret<64> {
    let k = key_words(key);
    let n = [
        u32::from_le_bytes([nonce[0], nonce[1], nonce[2], nonce[3]]),
        u32::from_le_bytes([nonce[4], nonce[5], nonce[6], nonce[7]]),
        u32::from_le_bytes([nonce[8], nonce[9], nonce[10], nonce[11]]),
    ];
    let initial = SensitiveWords32([
        SIGMA[0], SIGMA[1], SIGMA[2], SIGMA[3], k.0[0], k.0[1], k.0[2], k.0[3], k.0[4], k.0[5],
        k.0[6], k.0[7], counter, n[0], n[1], n[2],
    ]);
    let mut state = SensitiveWords32(initial.0);
    for _ in 0..10 {
        double_round(&mut state.0);
    }
    let mut out = Secret::<64>::zeroed();
    for i in 0..16 {
        let word = state.0[i].wrapping_add(initial.0[i]);
        out.expose_mut()[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// XOR the ChaCha20 keystream over `data` in place, starting at
/// `initial_counter` (RFC 8439 §2.4).
pub fn xor_stream(key: &[u8; 32], initial_counter: u32, nonce: &[u8; 12], data: &mut [u8]) {
    for (block_index, chunk) in data.chunks_mut(64).enumerate() {
        let counter = initial_counter
            .checked_add(u32::try_from(block_index).expect("stream shorter than 2^32 blocks"))
            .expect("ChaCha20 block counter must not wrap within one message");
        let keystream = block(key, counter, nonce);
        for (byte, ks) in chunk.iter_mut().zip(keystream.expose().iter()) {
            *byte ^= ks;
        }
    }
}

/// The Poly1305 one-time key for an AEAD message: the first 32 bytes of
/// keystream block zero (RFC 8439 §2.6).
pub fn poly1305_key(key: &[u8; 32], nonce: &[u8; 12]) -> Secret<32> {
    let ks = block(key, 0, nonce);
    let mut out = Secret::<32>::zeroed();
    out.expose_mut().copy_from_slice(&ks.expose()[..32]);
    out
}

/// HChaCha20: derive an XChaCha20 subkey from the key and the first 16 bytes
/// of the 24-byte extended nonce. Runs the 20 ChaCha rounds and returns words
/// 0..4 and 12..16 WITHOUT the final feed-forward addition.
pub fn hchacha20(key: &[u8; 32], nonce16: &[u8; 16]) -> Secret<32> {
    let k = key_words(key);
    let mut state = SensitiveWords32([
        SIGMA[0],
        SIGMA[1],
        SIGMA[2],
        SIGMA[3],
        k.0[0],
        k.0[1],
        k.0[2],
        k.0[3],
        k.0[4],
        k.0[5],
        k.0[6],
        k.0[7],
        u32::from_le_bytes([nonce16[0], nonce16[1], nonce16[2], nonce16[3]]),
        u32::from_le_bytes([nonce16[4], nonce16[5], nonce16[6], nonce16[7]]),
        u32::from_le_bytes([nonce16[8], nonce16[9], nonce16[10], nonce16[11]]),
        u32::from_le_bytes([nonce16[12], nonce16[13], nonce16[14], nonce16[15]]),
    ]);
    for _ in 0..10 {
        double_round(&mut state.0);
    }
    let mut out = Secret::<32>::zeroed();
    for (i, &word_index) in [0usize, 1, 2, 3, 12, 13, 14, 15].iter().enumerate() {
        out.expose_mut()[4 * i..4 * i + 4].copy_from_slice(&state.0[word_index].to_le_bytes());
    }
    out
}
