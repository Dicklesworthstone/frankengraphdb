//! BLAKE2b (RFC 7693): variable-length digests, optional keying.
//!
//! **WHY THIS EXISTS WHEN THE CRATE ALREADY OWNS BLAKE3.** BLAKE2b is not here
//! as an alternative hash — nothing in FrankenGraphDB should reach for it when
//! BLAKE3 will do. It is here because Argon2id is *defined* in terms of it
//! (RFC 9106 §3.2 builds `H` and the variable-length `H'` from BLAKE2b, and the
//! compression function G is BLAKE2b's), and the README's at-rest chain is
//! `Argon2id → KEK → per-DB DEK`. Substituting a different hash would not be
//! "our variant of Argon2id"; it would be an unanalyzed KDF wearing the name.
//! So the primitive arrives first, oracle-verified, before the KDF that needs
//! it — the same order increments 1 and 2 of this bead used.
//!
//! **CLOSED UNIVERSE** (doctrine #1): no external crypto crates. The oracle used
//! to generate this file's golden vectors is a scratchpad-only crate that never
//! enters the dependency graph, exactly as increment 1 did with BLAKE3.
//!
//! **SCOPE, STATED RATHER THAN IMPLIED.** This is the sequential, unsalted,
//! unpersonalized subset RFC 7693 §2.5 calls the common case, plus keying: it
//! covers digest lengths 1..=64, key lengths 0..=64, and the parameter block
//! fields Argon2id needs. Tree hashing (fanout/depth/leaf/node fields), salt and
//! personalization are NOT implemented, and are refused rather than ignored —
//! `Params` cannot express them, so no caller can believe it set one.

use core::cmp::min;

use crate::zeroize::{Secret, scrub_slice, scrub_words};

/// The BLAKE2b initialization vector — the same constants as SHA-512's IV
/// (RFC 7693 §2.6), which is why they look familiar and are not a typo.
const IV: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];

/// The message-word permutation schedule (RFC 7693 §2.7). Ten rows; rounds 10
/// and 11 reuse rows 0 and 1, which is what `round % 10` below expresses.
const SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

/// The block size in bytes (RFC 7693 §2.1: BLAKE2b's `bb`).
pub const BLOCK_LEN: usize = 128;
/// The largest digest BLAKE2b produces.
pub const MAX_DIGEST_LEN: usize = 64;
/// The largest key BLAKE2b accepts.
pub const MAX_KEY_LEN: usize = 64;

/// Why a BLAKE2b instance could not be constructed.
///
/// Constructed rather than silently clamped: a caller asking for a 65-byte
/// digest has misunderstood the primitive, and quietly handing back 64 bytes
/// would make two different requests indistinguishable in the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blake2bError {
    /// Digest length was zero or above [`MAX_DIGEST_LEN`].
    DigestLen { requested: usize },
    /// Key length was above [`MAX_KEY_LEN`].
    KeyLen { requested: usize },
}

impl core::fmt::Display for Blake2bError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DigestLen { requested } => write!(
                f,
                "BLAKE2b digest length must be 1..={MAX_DIGEST_LEN}, got {requested}"
            ),
            Self::KeyLen { requested } => write!(
                f,
                "BLAKE2b key length must be 0..={MAX_KEY_LEN}, got {requested}"
            ),
        }
    }
}

impl core::error::Error for Blake2bError {}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn g(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
    // Wrapping is the specification's arithmetic, not an overflow we tolerate.
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

/// The compression function F (RFC 7693 §3.2).
///
/// `counter` is the total number of bytes fed *including* this block, and
/// `last` sets the finalization flag. Both are the caller's responsibility
/// because BLAKE2b's last-block rule is a streaming property, not a block one.
fn compress(h: &mut [u64; 8], block: &[u8; BLOCK_LEN], counter: u128, last: bool) {
    let mut m = [0u64; 16];
    let (words, remainder) = block.as_chunks::<8>();
    debug_assert!(
        remainder.is_empty(),
        "the block length is a multiple of the word width"
    );
    for (word, bytes) in m.iter_mut().zip(words) {
        *word = u64::from_le_bytes(*bytes);
    }

    let mut v = [0u64; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&IV);
    v[12] ^= counter as u64;
    v[13] ^= (counter >> 64) as u64;
    if last {
        v[14] ^= u64::MAX;
    }

    for round in 0..12 {
        let s = &SIGMA[round % 10];
        g(&mut v, 0, 4, 8, 12, m[s[0]], m[s[1]]);
        g(&mut v, 1, 5, 9, 13, m[s[2]], m[s[3]]);
        g(&mut v, 2, 6, 10, 14, m[s[4]], m[s[5]]);
        g(&mut v, 3, 7, 11, 15, m[s[6]], m[s[7]]);
        g(&mut v, 0, 5, 10, 15, m[s[8]], m[s[9]]);
        g(&mut v, 1, 6, 11, 12, m[s[10]], m[s[11]]);
        g(&mut v, 2, 7, 8, 13, m[s[12]], m[s[13]]);
        g(&mut v, 3, 4, 9, 14, m[s[14]], m[s[15]]);
    }

    for i in 0..8 {
        h[i] ^= v[i] ^ v[i + 8];
    }

    // Both arrays are functions of the message (and, for keyed mode, the
    // secret key). They are the original stack storage, so erase them before
    // returning rather than waiting for ordinary stack reuse.
    scrub_words(&mut m);
    scrub_words(&mut v);
}

/// A streaming BLAKE2b instance.
///
/// **THE LAST BLOCK IS DECIDED AT FINALIZE, NOT AT UPDATE**, and that is the one
/// subtlety in the whole construction. A full 128-byte buffer is compressed only
/// once more input is known to follow; otherwise `finalize` would have already
/// consumed the final block without its finalization flag, and every message
/// whose length is an exact multiple of the block size would hash wrong. The
/// buffer therefore lags one block behind the input on purpose.
pub struct Blake2b {
    h: [u64; 8],
    buffer: [u8; BLOCK_LEN],
    buffered: usize,
    counter: u128,
    digest_len: usize,
}

impl Blake2b {
    /// An unkeyed instance producing `digest_len` bytes.
    pub fn new(digest_len: usize) -> Result<Self, Blake2bError> {
        Self::new_keyed(digest_len, &[])
    }

    /// A keyed instance producing `digest_len` bytes.
    ///
    /// Keying is not a MAC bolted on afterwards: RFC 7693 §2.9 prepends the key
    /// as a padded first block and encodes its length in the parameter block, so
    /// a keyed and an unkeyed hash of the same message differ from the first
    /// compression onward.
    pub fn new_keyed(digest_len: usize, key: &[u8]) -> Result<Self, Blake2bError> {
        if digest_len == 0 || digest_len > MAX_DIGEST_LEN {
            return Err(Blake2bError::DigestLen {
                requested: digest_len,
            });
        }
        if key.len() > MAX_KEY_LEN {
            return Err(Blake2bError::KeyLen {
                requested: key.len(),
            });
        }

        let mut h = IV;
        // Parameter block word 0 (RFC 7693 §2.8): digest length, key length,
        // fanout = 1, depth = 1. The remaining parameter words are zero, which
        // is what "sequential, unsalted, unpersonalized" means — and this crate
        // offers no way to set them, so that claim cannot silently become false.
        h[0] ^= 0x0101_0000 ^ ((key.len() as u64) << 8) ^ (digest_len as u64);

        let mut state = Blake2b {
            h,
            buffer: [0u8; BLOCK_LEN],
            buffered: 0,
            counter: 0,
            digest_len,
        };

        if !key.is_empty() {
            // The key occupies one whole zero-padded block.
            let mut key_block = Secret::<BLOCK_LEN>::zeroed();
            key_block.expose_mut()[..key.len()].copy_from_slice(key);
            state.update(key_block.expose());
        }
        Ok(state)
    }

    /// Feed input.
    pub fn update(&mut self, mut input: &[u8]) -> &mut Self {
        while !input.is_empty() {
            if self.buffered == BLOCK_LEN {
                // More input follows, so the buffered block is not the last.
                self.counter += BLOCK_LEN as u128;
                compress(&mut self.h, &self.buffer, self.counter, false);
                self.buffered = 0;
            }
            let take = min(BLOCK_LEN - self.buffered, input.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&input[..take]);
            self.buffered += take;
            input = &input[take..];
        }
        self
    }

    /// Finish, writing exactly `digest_len` bytes.
    ///
    /// Takes `self` by value: BLAKE2b's finalization mutates the state, and an
    /// instance that could be finalized twice would silently produce a second,
    /// different digest for the same input.
    pub fn finalize(mut self) -> Vec<u8> {
        self.counter += self.buffered as u128;
        // Zero-pad the final partial block. An all-zero tail is unambiguous here
        // because the byte counter, not the padding, delimits the message.
        for byte in self.buffer.iter_mut().skip(self.buffered) {
            *byte = 0;
        }
        compress(&mut self.h, &self.buffer, self.counter, true);

        let mut out = Vec::with_capacity(self.digest_len);
        for word in &self.h {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.truncate(self.digest_len);
        out
    }
}

impl Drop for Blake2b {
    fn drop(&mut self) {
        scrub_words(&mut self.h);
        scrub_slice(&mut self.buffer);
    }
}

/// One-shot unkeyed BLAKE2b.
pub fn blake2b(digest_len: usize, input: &[u8]) -> Result<Vec<u8>, Blake2bError> {
    let mut state = Blake2b::new(digest_len)?;
    state.update(input);
    Ok(state.finalize())
}

/// One-shot keyed BLAKE2b.
pub fn blake2b_keyed(digest_len: usize, key: &[u8], input: &[u8]) -> Result<Vec<u8>, Blake2bError> {
    let mut state = Blake2b::new_keyed(digest_len, key)?;
    state.update(input);
    Ok(state.finalize())
}
