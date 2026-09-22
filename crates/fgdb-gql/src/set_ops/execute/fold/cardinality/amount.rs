//! Cardinality witness with enough headroom for exact repeated i128 arithmetic.
//!
//! Saturation is NOT a numeric answer. At most MAX_GRAPH_SET_DEPTH pages can
//! subtract u64::MAX along a definition path. Thus a saturated 256-bit value
//! cannot fall back into u128 after paging. Zero annihilation and finite LIMIT
//! restore exact values. Only an exact <=u128 amount crosses the aggregate seam.
//! Four stack limbs have constant cost; callers charge before each operation.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Amount([u64; 4]);

impl Amount {
    pub(crate) const ZERO: Self = Self([0; 4]);
    pub(crate) const ONE: Self = Self([1, 0, 0, 0]);
    const MAX: Self = Self([u64::MAX; 4]);

    pub(crate) const fn from_u128(value: u128) -> Self {
        Self([value as u64, (value >> 64) as u64, 0, 0])
    }

    pub(crate) fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    pub(crate) fn to_u128(self) -> Option<u128> {
        (self.0[2] == 0 && self.0[3] == 0)
            .then(|| u128::from(self.0[0]) | (u128::from(self.0[1]) << 64))
    }

    pub(crate) fn to_u64(self) -> Option<u64> {
        self.to_u128().and_then(|value| u64::try_from(value).ok())
    }

    pub(crate) fn add(self, other: Self) -> Self {
        let mut output = [0; 4];
        let mut carry = 0_u128;
        for (at, result) in output.iter_mut().enumerate() {
            let sum = u128::from(self.0[at]) + u128::from(other.0[at]) + carry;
            *result = sum as u64;
            carry = sum >> 64;
        }
        if carry != 0 { Self::MAX } else { Self(output) }
    }

    pub(crate) fn multiply(self, other: Self) -> Self {
        let mut output = [0_u64; 8];
        for i in 0..4 {
            let mut carry = 0_u128;
            for j in 0..4 {
                // (2^64-1)^2 + two carries fits exactly in u128.
                let product = u128::from(self.0[i]) * u128::from(other.0[j])
                    + u128::from(output[i + j])
                    + carry;
                output[i + j] = product as u64;
                carry = product >> 64;
            }
            output[i + 4] = carry as u64;
        }
        if output[4..].iter().any(|&limb| limb != 0) {
            Self::MAX
        } else {
            Self([output[0], output[1], output[2], output[3]])
        }
    }

    pub(crate) fn subtract(self, value: u64) -> Self {
        let mut output = self.0;
        let (low, mut borrow) = output[0].overflowing_sub(value);
        output[0] = low;
        for limb in &mut output[1..] {
            let (next, underflow) = limb.overflowing_sub(u64::from(borrow));
            *limb = next;
            borrow = underflow;
        }
        if borrow { Self::ZERO } else { Self(output) }
    }

    pub(crate) fn limit(self, value: u64) -> Self {
        match self.to_u64() {
            Some(current) if current <= value => self,
            _ => Self::from_u128(u128::from(value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_borrows_and_zero_annihilation_cover_the_full_width() {
        let two_to_128 = Amount::from_u128(u128::MAX).add(Amount::ONE);
        assert_eq!(two_to_128.to_u128(), None);
        assert_eq!(two_to_128.subtract(1).to_u128(), Some(u128::MAX));
        assert_eq!(
            two_to_128.subtract(u64::MAX).to_u128(),
            Some(u128::MAX - u128::from(u64::MAX) + 1)
        );
        let saturated = two_to_128.multiply(two_to_128);
        assert_eq!(saturated, Amount::MAX);
        assert_eq!(saturated.multiply(Amount::ZERO), Amount::ZERO);
        assert_eq!(Amount::ZERO.multiply(saturated), Amount::ZERO);
        let mut paged = saturated;
        for _ in 0..crate::set_ops::MAX_GRAPH_SET_DEPTH {
            paged = paged.subtract(u64::MAX);
        }
        assert_eq!(paged.to_u128(), None);
        assert_eq!(paged.limit(17).to_u64(), Some(17));
        assert_eq!(Amount::ONE.subtract(2), Amount::ZERO);
    }

    #[test]
    fn small_products_and_pages_match_builtin_checked_arithmetic() {
        let values = [
            0,
            1,
            2,
            17,
            u64::MAX as u128,
            1_u128 << 64,
            (1_u128 << 96) + 7,
            u128::MAX,
        ];
        for a in values {
            for b in values {
                assert_eq!(
                    Amount::from_u128(a).add(Amount::from_u128(b)).to_u128(),
                    a.checked_add(b)
                );
                assert_eq!(
                    Amount::from_u128(a)
                        .multiply(Amount::from_u128(b))
                        .to_u128(),
                    a.checked_mul(b)
                );
            }
            for offset in [0, 1, 17, u64::MAX] {
                let paged = Amount::from_u128(a).subtract(offset);
                assert_eq!(paged.to_u128(), Some(a.saturating_sub(u128::from(offset))));
            }
        }
    }
}
