//! Private arithmetic modulo 2^255-19. Canonical 16-bit radix limbs (top
//! limb: 15 bits). Product accumulation uses u64 only: each convolution lane
//! is <=16*(2^16-1)^2; folding the upper half by 38 stays below 2^42.
//! All carry/subtract/select loops have fixed bounds, including secret paths.

use crate::zeroize::scrub_words;

const MASK: u64 = 0xffff;
const P: [u64; 16] = [
    65517, 65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535,
    65535, 65535, 32767,
];

// Not Copy/Clone: every owned intermediate scrubs its native word storage.
// This does not promise erasure of compiler-created moves or register spills.
pub(super) struct Field(pub(super) [u64; 16]);
impl Drop for Field {
    fn drop(&mut self) {
        scrub_words(&mut self.0);
    }
}

impl Field {
    pub(super) const ZERO: Self = Self([0; 16]);
    pub(super) const ONE: Self = Self([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    pub(super) const D: Self = Self([
        30883, 4953, 19914, 30187, 55467, 16705, 2637, 112, 59544, 30585, 16505, 36039, 65139,
        11119, 27886, 20995,
    ]);
    pub(super) const SQRT_M1: Self = Self([
        41136, 18958, 6951, 50414, 58488, 44335, 6150, 12099, 55207, 15867, 153, 11085, 57099,
        20417, 9344, 11139,
    ]);

    fn normalize(mut h: [u64; 16]) -> Self {
        // First pass reduces the <2^42 convolution limbs. Second propagates
        // its folded top carry. Third handles the possible all-ones ripple:
        // then the top carry is zero, and all limbs have their stated width.
        for _ in 0..3 {
            for i in 0..15 {
                h[i + 1] += h[i] >> 16;
                h[i] &= MASK;
            }
            let carry = h[15] >> 15;
            h[15] &= 0x7fff;
            h[0] += 19 * carry;
        }
        let mut reduced = [0u64; 16];
        let mut borrow = 0;
        for i in 0..16 {
            let word = h[i] + (1 << 16) - P[i] - borrow;
            reduced[i] = word & MASK;
            borrow = 1 ^ (word >> 16);
        }
        let mask = 0u64.wrapping_sub(1 - borrow);
        for i in 0..16 {
            h[i] = (h[i] & !mask) | (reduced[i] & mask);
        }
        scrub_words(&mut reduced);
        Self(h)
    }

    pub(super) fn add(&self, rhs: &Self) -> Self {
        let mut h = [0; 16];
        for (i, word) in h.iter_mut().enumerate() {
            *word = self.0[i] + rhs.0[i];
        }
        Self::normalize(h)
    }

    pub(super) fn sub(&self, rhs: &Self) -> Self {
        let mut h = [0; 16];
        for (i, word) in h.iter_mut().enumerate() {
            *word = self.0[i] + 2 * P[i] - rhs.0[i];
        }
        Self::normalize(h)
    }

    pub(super) fn mul(&self, rhs: &Self) -> Self {
        let mut product = [0u64; 32];
        for i in 0..16 {
            for j in 0..16 {
                product[i + j] += self.0[i] * rhs.0[j];
            }
        }
        // 2^256 = 38 (mod p). There is no secret-indexed lookup.
        let mut h = [0; 16];
        for i in 0..16 {
            h[i] = product[i] + 38 * product[i + 16];
        }
        scrub_words(&mut product);
        Self::normalize(h)
    }
    pub(super) fn square(&self) -> Self {
        self.mul(self)
    }
    pub(super) fn negate(&self) -> Self {
        Self::ZERO.sub(self)
    }

    pub(super) fn select(a: &Self, b: &Self, bit: u8) -> Self {
        let mask = 0u64.wrapping_sub(u64::from(bit & 1));
        let mut h = [0; 16];
        for (i, word) in h.iter_mut().enumerate() {
            *word = (a.0[i] & !mask) | (b.0[i] & mask);
        }
        Self(h)
    }
    pub(super) fn equals(&self, rhs: &Self) -> bool {
        let mut difference = 0;
        for i in 0..16 {
            difference |= self.0[i] ^ rhs.0[i];
        }
        difference == 0
    }
    pub(super) fn is_zero(&self) -> bool {
        self.equals(&Self::ZERO)
    }
    pub(super) fn parity(&self) -> u8 {
        (self.0[0] & 1) as u8
    }

    fn power(&self, exponent: &[u8; 32]) -> Self {
        let mut result = Self::ONE;
        for i in (0..255).rev() {
            let square = result.square();
            let product = square.mul(self);
            result = Self::select(&square, &product, (exponent[i / 8] >> (i % 8)) & 1);
        }
        result
    }
    pub(super) fn invert(&self) -> Self {
        let mut exponent = [0xff; 32];
        exponent[0] = 0xeb;
        exponent[31] = 0x7f;
        self.power(&exponent)
    }
    pub(super) fn sqrt_candidate(&self) -> Self {
        let mut exponent = [0xff; 32];
        exponent[0] = 0xfe;
        exponent[31] = 0x0f;
        self.power(&exponent)
    }
    pub(super) fn to_bytes(&self) -> [u8; 32] {
        let mut bytes = [0; 32];
        for i in 0..16 {
            bytes[2 * i..2 * i + 2].copy_from_slice(&(self.0[i] as u16).to_le_bytes());
        }
        bytes
    }
    /// Canonical unsigned field bytes only: do not reduce an attacker encoding.
    pub(super) fn from_bytes(bytes: &[u8; 32]) -> Option<Self> {
        let mut h = [0; 16];
        for i in 0..16 {
            h[i] = u64::from(u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]));
        }
        let normalized = Self::normalize(h);
        (normalized.to_bytes() == *bytes).then_some(normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small(n: u64) -> Field {
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&n.to_le_bytes());
        Field::from_bytes(&bytes).unwrap()
    }
    #[test]
    fn field_carries_canonicality_and_inverse() {
        let mut pm1 = [0xff; 32];
        pm1[0] = 0xec;
        pm1[31] = 0x7f;
        let minus_one = Field::from_bytes(&pm1).unwrap();
        assert!(minus_one.add(&Field::ONE).is_zero());
        assert!(minus_one.square().equals(&Field::ONE));
        assert!(Field::ZERO.sub(&Field::ONE).equals(&minus_one));
        assert!(Field::SQRT_M1.square().equals(&minus_one));
        assert!(Field::D.mul(&small(121666)).add(&small(121665)).is_zero());
        for first in 0xed..=0xff {
            let mut p = pm1;
            p[0] = first;
            assert!(Field::from_bytes(&p).is_none());
        }
        let mut high = [0; 32];
        high[31] = 0x80;
        assert!(Field::from_bytes(&high).is_none());
        for n in [1, 2, 19, 65535, 65536, u64::MAX] {
            let value = small(n);
            assert!(value.mul(&value.invert()).equals(&Field::ONE));
            assert!(Field::select(&value, &Field::ZERO, 0).equals(&value));
            assert!(Field::select(&value, &Field::ZERO, 1).is_zero());
        }
    }
}
