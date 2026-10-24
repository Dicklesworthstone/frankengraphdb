//! Fixed-schedule arithmetic modulo the Ed25519 prime subgroup order.
use crate::zeroize::{Secret, scrub_words};

pub(super) const ORDER: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
];
const L: [u64; 4] = [
    0x5812631a5cf5d3ed,
    0x14def9dea2f79cd6,
    0,
    0x1000000000000000,
];

/// Replace with value-L iff value>=L, with fixed word operations.
fn subtract_order(value: &mut [u64; 4]) -> u64 {
    let mut difference = [0; 4];
    let mut borrow = 0u64;
    for i in 0..4 {
        let (word, b1) = value[i].overflowing_sub(L[i]);
        let (word, b2) = word.overflowing_sub(borrow);
        difference[i] = word;
        borrow = u64::from(b1 | b2);
    }
    let mask = 0u64.wrapping_sub(1 - borrow);
    for i in 0..4 {
        value[i] = (value[i] & !mask) | (difference[i] & mask);
    }
    scrub_words(&mut difference);
    borrow
}

pub(super) fn canonical(bytes: &[u8; 32]) -> bool {
    let mut value = [0; 4];
    for (word, bytes) in value.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *word = u64::from_le_bytes(*bytes);
    }
    let below = subtract_order(&mut value) == 1;
    scrub_words(&mut value);
    below
}

/// Binary long division with exactly 512 rounds. Before each shift the
/// remainder is <L, so the shifted value fits 253 bits and one subtraction
/// suffices. Neither loop count nor indexing depends on secret input.
pub(super) fn reduce(bytes: &[u8; 64]) -> Secret<32> {
    let mut remainder = [0u64; 4];
    for i in (0..512).rev() {
        let mut carry = u64::from((bytes[i / 8] >> (i % 8)) & 1);
        for word in &mut remainder {
            let next = *word >> 63;
            *word = (*word << 1) | carry;
            carry = next;
        }
        subtract_order(&mut remainder);
    }
    let mut output = Secret::zeroed();
    for (bytes, word) in output
        .expose_mut()
        .as_chunks_mut::<8>()
        .0
        .iter_mut()
        .zip(&remainder)
    {
        *bytes = word.to_le_bytes();
    }
    scrub_words(&mut remainder);
    output
}

/// (a*b+c) mod L. The largest byte-product column is <2^22; even maximal
/// 256-bit inputs fit the complete 512-bit product-plus-addend representation.
pub(super) fn mul_add(a: &[u8; 32], b: &[u8; 32], c: &[u8; 32]) -> Secret<32> {
    let mut product = [0u64; 64];
    for i in 0..32 {
        product[i] += u64::from(c[i]);
        for j in 0..32 {
            product[i + j] += u64::from(a[i]) * u64::from(b[j]);
        }
    }
    for i in 0..63 {
        product[i + 1] += product[i] >> 8;
        product[i] &= 255;
    }
    let mut bytes = Secret::zeroed();
    for (byte, word) in bytes.expose_mut().iter_mut().zip(&product) {
        *byte = *word as u8;
    }
    scrub_words(&mut product);
    reduce(bytes.expose())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reduction_and_canonical_scalar_boundaries() {
        assert!(!canonical(&ORDER));
        assert!(!canonical(&[0xff; 32]));
        let mut below = ORDER;
        below[0] -= 1;
        assert!(canonical(&below));
        let mut wide = [0; 64];
        wide[..32].copy_from_slice(&ORDER);
        assert_eq!(reduce(&wide).expose(), &[0; 32]);
        wide[0] += 1;
        let mut one = [0; 32];
        one[0] = 1;
        assert_eq!(reduce(&wide).expose(), &one);
        assert_eq!(mul_add(&below, &below, &[0; 32]).expose(), &one);
        assert_eq!(mul_add(&below, &one, &one).expose(), &[0; 32]);
        assert!(canonical(reduce(&[0xff; 64]).expose()));
    }
}
