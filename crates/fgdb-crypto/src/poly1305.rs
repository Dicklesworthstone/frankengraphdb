//! Portable scalar Poly1305 (RFC 8439 §2.5): a one-time authenticator over
//! the prime field 2^130 - 5, computed with 26-bit limbs in u64 lanes so every
//! intermediate product fits u64 without overflow.
//!
//! Verified via the AEAD golden vectors (`tests/aead_vectors.rs`): a tag error
//! anywhere in this file fails every ciphertext-and-tag comparison.

/// One-time authenticator state. The key's `r` half is clamped per RFC 8439.
pub struct Poly1305 {
    r: [u64; 5],
    /// Precomputed 5 * r limbs 1..4 for the reduction fold.
    s_r: [u64; 4],
    pad: [u32; 4],
    acc: [u64; 5],
    buffer: [u8; 16],
    buffered: usize,
}

impl Poly1305 {
    pub fn new(key: &[u8; 32]) -> Self {
        // Clamp r (RFC 8439 §2.5.1) then split into 26-bit limbs.
        let t0 = u32::from_le_bytes([key[0], key[1], key[2], key[3]]) & 0x0fff_ffff;
        let t1 = u32::from_le_bytes([key[4], key[5], key[6], key[7]]) & 0x0fff_fffc;
        let t2 = u32::from_le_bytes([key[8], key[9], key[10], key[11]]) & 0x0fff_fffc;
        let t3 = u32::from_le_bytes([key[12], key[13], key[14], key[15]]) & 0x0fff_fffc;
        let r = [
            u64::from(t0) & 0x3ff_ffff,
            (u64::from(t0) >> 26 | u64::from(t1) << 6) & 0x3ff_ffff,
            (u64::from(t1) >> 20 | u64::from(t2) << 12) & 0x3ff_ffff,
            (u64::from(t2) >> 14 | u64::from(t3) << 18) & 0x3ff_ffff,
            u64::from(t3) >> 8,
        ];
        Self {
            r,
            s_r: [r[1] * 5, r[2] * 5, r[3] * 5, r[4] * 5],
            pad: [
                u32::from_le_bytes([key[16], key[17], key[18], key[19]]),
                u32::from_le_bytes([key[20], key[21], key[22], key[23]]),
                u32::from_le_bytes([key[24], key[25], key[26], key[27]]),
                u32::from_le_bytes([key[28], key[29], key[30], key[31]]),
            ],
            acc: [0; 5],
            buffer: [0; 16],
            buffered: 0,
        }
    }

    /// Absorb one 16-byte block. `final_partial` marks a short trailing block
    /// (already 0x01-terminated and zero-padded by the caller) whose high bit
    /// must NOT be added.
    fn process_block(&mut self, block: &[u8; 16], high_bit: bool) {
        let t0 = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
        let t1 = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
        let t2 = u32::from_le_bytes([block[8], block[9], block[10], block[11]]);
        let t3 = u32::from_le_bytes([block[12], block[13], block[14], block[15]]);

        self.acc[0] += u64::from(t0) & 0x3ff_ffff;
        self.acc[1] += (u64::from(t0) >> 26 | u64::from(t1) << 6) & 0x3ff_ffff;
        self.acc[2] += (u64::from(t1) >> 20 | u64::from(t2) << 12) & 0x3ff_ffff;
        self.acc[3] += (u64::from(t2) >> 14 | u64::from(t3) << 18) & 0x3ff_ffff;
        self.acc[4] += (u64::from(t3) >> 8) | if high_bit { 1 << 24 } else { 0 };

        // acc *= r  (mod 2^130 - 5), schoolbook with the 5x fold.
        let a = self.acc;
        let r = self.r;
        let sr = self.s_r;
        let d0 = a[0] * r[0] + a[1] * sr[3] + a[2] * sr[2] + a[3] * sr[1] + a[4] * sr[0];
        let d1 = a[0] * r[1] + a[1] * r[0] + a[2] * sr[3] + a[3] * sr[2] + a[4] * sr[1];
        let d2 = a[0] * r[2] + a[1] * r[1] + a[2] * r[0] + a[3] * sr[3] + a[4] * sr[2];
        let d3 = a[0] * r[3] + a[1] * r[2] + a[2] * r[1] + a[3] * r[0] + a[4] * sr[3];
        let d4 = a[0] * r[4] + a[1] * r[3] + a[2] * r[2] + a[3] * r[1] + a[4] * r[0];

        // Carry propagation back to 26-bit limbs.
        let mut c: u64;
        let mut h0 = d0 & 0x3ff_ffff;
        c = d0 >> 26;
        let mut h1 = d1 + c;
        c = h1 >> 26;
        h1 &= 0x3ff_ffff;
        let mut h2 = d2 + c;
        c = h2 >> 26;
        h2 &= 0x3ff_ffff;
        let mut h3 = d3 + c;
        c = h3 >> 26;
        h3 &= 0x3ff_ffff;
        let mut h4 = d4 + c;
        c = h4 >> 26;
        h4 &= 0x3ff_ffff;
        h0 += c * 5;
        c = h0 >> 26;
        h0 &= 0x3ff_ffff;
        h1 += c;

        self.acc = [h0, h1, h2, h3, h4];
    }

    /// Absorb bytes (RFC 8439 processes the message in 16-byte blocks).
    pub fn update(&mut self, mut input: &[u8]) {
        if self.buffered > 0 {
            let want = 16 - self.buffered;
            let take = want.min(input.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&input[..take]);
            self.buffered += take;
            input = &input[take..];
            if self.buffered == 16 {
                let block = self.buffer;
                self.process_block(&block, true);
                self.buffered = 0;
            }
        }
        while input.len() >= 16 {
            let mut block = [0u8; 16];
            block.copy_from_slice(&input[..16]);
            self.process_block(&block, true);
            input = &input[16..];
        }
        if !input.is_empty() {
            self.buffer[..input.len()].copy_from_slice(input);
            self.buffered = input.len();
        }
    }

    /// Finalize: pad any trailing partial block with 0x01 || zeros (no high
    /// bit), fully reduce, add the pad half of the key mod 2^128.
    pub fn finalize(mut self) -> [u8; 16] {
        if self.buffered > 0 {
            let mut block = [0u8; 16];
            block[..self.buffered].copy_from_slice(&self.buffer[..self.buffered]);
            block[self.buffered] = 1;
            self.process_block(&block, false);
        }

        // Full reduction: fold the final carry, then conditionally subtract p.
        let [mut h0, mut h1, mut h2, mut h3, mut h4] = self.acc;
        let mut c = h1 >> 26;
        h1 &= 0x3ff_ffff;
        h2 += c;
        c = h2 >> 26;
        h2 &= 0x3ff_ffff;
        h3 += c;
        c = h3 >> 26;
        h3 &= 0x3ff_ffff;
        h4 += c;
        c = h4 >> 26;
        h4 &= 0x3ff_ffff;
        h0 += c * 5;
        c = h0 >> 26;
        h0 &= 0x3ff_ffff;
        h1 += c;

        // Compute h + (-p) = h - (2^130 - 5) and select it when non-negative.
        let mut g0 = h0.wrapping_add(5);
        c = g0 >> 26;
        g0 &= 0x3ff_ffff;
        let mut g1 = h1.wrapping_add(c);
        c = g1 >> 26;
        g1 &= 0x3ff_ffff;
        let mut g2 = h2.wrapping_add(c);
        c = g2 >> 26;
        g2 &= 0x3ff_ffff;
        let mut g3 = h3.wrapping_add(c);
        c = g3 >> 26;
        g3 &= 0x3ff_ffff;
        let g4 = h4.wrapping_add(c).wrapping_sub(1 << 26);

        // Constant-shape select: mask is all-ones when h >= p.
        let mask = (g4 >> 63).wrapping_sub(1);
        h0 = (h0 & !mask) | (g0 & mask);
        h1 = (h1 & !mask) | (g1 & mask);
        h2 = (h2 & !mask) | (g2 & mask);
        h3 = (h3 & !mask) | (g3 & mask);
        h4 = (h4 & !mask) | (g4 & mask & 0x3ff_ffff);

        // Repack to four u32 words and add the pad with carry (mod 2^128).
        let w0 = (h0 | h1 << 26) as u32;
        let w1 = (h1 >> 6 | h2 << 20) as u32;
        let w2 = (h2 >> 12 | h3 << 14) as u32;
        let w3 = (h3 >> 18 | h4 << 8) as u32;

        let mut tag = [0u8; 16];
        let mut carry: u64 = 0;
        for (i, (&word, &pad)) in [w0, w1, w2, w3].iter().zip(self.pad.iter()).enumerate() {
            let sum = u64::from(word) + u64::from(pad) + carry;
            tag[4 * i..4 * i + 4].copy_from_slice(&(sum as u32).to_le_bytes());
            carry = sum >> 32;
        }
        tag
    }
}
