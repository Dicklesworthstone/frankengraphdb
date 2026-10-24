//! Streaming SHA-512 (FIPS 180-4 / RFC 6234), used by pure Ed25519.
//!
//! The compression schedule and memory accesses do not depend on message
//! contents. Lengths are public. Owned state is scrubbed on drop with the
//! same deliberately limited guarantee as [`crate::zeroize`]. This is not a
//! replacement for the pinned-toolchain code-generation and external audit.

use crate::zeroize::{scrub_slice, scrub_words};

const IV: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];
const K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

/// A bounded-memory SHA-512 state. Not `Clone` or `Debug`: a state may contain
/// an Ed25519 seed or nonce prefix. Finalization consumes and scrubs it.
pub struct Sha512 {
    state: [u64; 8],
    block: [u8; 128],
    used: usize,
    bytes: u128,
}

impl Sha512 {
    /// Start a new SHA-512 transcript.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: IV,
            block: [0; 128],
            used: 0,
            bytes: 0,
        }
    }

    /// Append bytes without buffering the full message.
    ///
    /// # Panics
    /// Panics before modifying the transcript if its byte length would reach
    /// 2^125: SHA-512's encoded bit length must fit in 128 bits.
    pub fn update(&mut self, mut input: &[u8]) {
        let bytes = self
            .bytes
            .checked_add(input.len() as u128)
            .filter(|n| *n < (1_u128 << 125))
            .expect("SHA-512 message exceeds its 128-bit bit-length field");
        self.bytes = bytes;
        if self.used != 0 {
            let n = input.len().min(128 - self.used);
            self.block[self.used..self.used + n].copy_from_slice(&input[..n]);
            self.used += n;
            input = &input[n..];
            if self.used != 128 {
                return;
            }
            compress(&mut self.state, &self.block);
            self.used = 0;
        }
        while input.len() >= 128 {
            let block: &[u8; 128] = input[..128].try_into().expect("one full block");
            compress(&mut self.state, block);
            input = &input[128..];
        }
        self.block[..input.len()].copy_from_slice(input);
        self.used = input.len();
    }

    /// Complete the transcript, returning the 64-byte digest.
    #[must_use]
    pub fn finalize(self) -> [u8; 64] {
        let mut out = [0; 64];
        self.finalize_into(&mut out);
        out
    }

    /// Complete directly into caller-owned storage (e.g. `Secret<64>`).
    /// The hash state is consumed and scrubbed on return.
    pub fn finalize_into(mut self, out: &mut [u8; 64]) {
        self.block[self.used] = 0x80;
        self.block[self.used + 1..].fill(0);
        if self.used >= 112 {
            compress(&mut self.state, &self.block);
            self.block.fill(0);
        }
        self.block[112..].copy_from_slice(&(self.bytes << 3).to_be_bytes());
        compress(&mut self.state, &self.block);
        for (word, dest) in self.state.iter().zip(out.as_chunks_mut::<8>().0) {
            *dest = word.to_be_bytes();
        }
    }
}

impl Default for Sha512 {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Sha512 {
    fn drop(&mut self) {
        scrub_words(&mut self.state);
        scrub_slice(&mut self.block);
    }
}

/// Hash one byte slice with SHA-512.
#[must_use]
pub fn sha512(input: &[u8]) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(input);
    h.finalize()
}

fn compress(state: &mut [u64; 8], block: &[u8; 128]) {
    let mut w = [0_u64; 80];
    for (dest, bytes) in w[..16].iter_mut().zip(block.as_chunks::<8>().0) {
        *dest = u64::from_be_bytes(*bytes);
    }
    for i in 16..80 {
        let x = w[i - 15];
        let y = w[i - 2];
        let s0 = x.rotate_right(1) ^ x.rotate_right(8) ^ (x >> 7);
        let s1 = y.rotate_right(19) ^ y.rotate_right(61) ^ (y >> 6);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let mut v = *state;
    for i in 0..80 {
        let s1 = v[4].rotate_right(14) ^ v[4].rotate_right(18) ^ v[4].rotate_right(41);
        let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
        let t1 = v[7]
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = v[0].rotate_right(28) ^ v[0].rotate_right(34) ^ v[0].rotate_right(39);
        let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
        let t2 = s0.wrapping_add(maj);
        v = [
            t1.wrapping_add(t2),
            v[0],
            v[1],
            v[2],
            v[3].wrapping_add(t1),
            v[4],
            v[5],
            v[6],
        ];
    }
    for (dest, word) in state.iter_mut().zip(v.iter()) {
        *dest = dest.wrapping_add(*word);
    }
    scrub_words(&mut w);
    scrub_words(&mut v);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn published_vectors() {
        let cases: &[(&[u8], &str)] = &[
            (b"", "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"),
            (b"abc", "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"),
            (b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu", "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"),
        ];
        for &(input, expected) in cases {
            assert_eq!(hex(&sha512(input)), expected);
            for split in 0..=input.len() {
                let mut h = Sha512::new();
                h.update(&input[..split]);
                h.update(&[]);
                h.update(&input[split..]);
                assert_eq!(hex(&h.finalize()), expected, "split={split}");
            }
        }
        let mut h = Sha512::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&h.finalize()),
            "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973ebde0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b"
        );
    }

    #[test]
    fn byte_and_block_chunking_agree_at_every_padding_boundary() {
        // Expected digests below come from Python hashlib/OpenSSL, not this
        // implementation. Chunking checks also cross a previously partial block.
        let cases = [
            (
                111,
                "fa9121c7b32b9e01733d034cfc78cbf67f926c7ed83e82200ef86818196921760b4beff48404df811b953828274461673c68d04e297b0eb7b2b4d60fc6b566a2",
            ),
            (
                112,
                "c01d080efd492776a1c43bd23dd99d0a2e626d481e16782e75d54c2503b5dc32bd05f0f1ba33e568b88fd2d970929b719ecbb152f58f130a407c8830604b70ca",
            ),
            (
                127,
                "828613968b501dc00a97e08c73b118aa8876c26b8aac93df128502ab360f91bab50a51e088769a5c1eff4782ace147dce3642554199876374291f5d921629502",
            ),
            (
                128,
                "b73d1929aa615934e61a871596b3f3b33359f42b8175602e89f7e06e5f658a243667807ed300314b95cacdd579f3e33abdfbe351909519a846d465c59582f321",
            ),
            (
                129,
                "4f681e0bd53cda4b5a2041cc8a06f2eabde44fb16c951fbd5b87702f07aeab611565b19c47fde30587177ebb852e3971bbd8d3fd30da18d71037dfbd98420429",
            ),
        ];
        for (len, expected) in cases {
            let input = vec![b'a'; len];
            assert_eq!(hex(&sha512(&input)), expected, "len={len}");
            for width in [1, 7, 63, 64, 111, 112, 127, 128, 129] {
                let mut h = Sha512::new();
                for chunk in input.chunks(width) {
                    h.update(chunk);
                }
                let mut out = [0; 64];
                h.finalize_into(&mut out);
                assert_eq!(hex(&out), expected, "len={len}, width={width}");
            }
        }
    }

    #[test]
    fn length_overflow_refuses_before_changing_state() {
        let mut h = Sha512::new();
        h.bytes = (1_u128 << 125) - 1;
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.update(&[0]))).is_err());
        assert_eq!(h.bytes, (1_u128 << 125) - 1);
        assert_eq!(h.state, IV);
        assert_eq!(h.used, 0);
    }
}
