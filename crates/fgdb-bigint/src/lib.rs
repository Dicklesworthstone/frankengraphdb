//! Canonical, allocation-bounded signed-limb exact integers.
//!
//! [`BigInt`] is the exact arithmetic kernel that Ripple's checked `i128`
//! `ZWeight` fast path will promote into before overflow. Its private
//! representation is normalized, so equality, total order, and hashing agree
//! structurally. This crate intentionally defines no durable byte encoding;
//! generated format code owns that contract.
//!
//! Every operation that may allocate a variable number of limbs requires an
//! explicit [`LimbLimit`]. There is no unlimited arithmetic overload.

#![forbid(unsafe_code)]

use std::{cmp::Ordering, fmt};

/// A canonical arbitrary-precision signed integer.
#[derive(PartialEq, Eq, Hash, Debug)]
pub struct BigInt {
    negative: bool,
    /// Little-endian magnitude limbs; empty iff the value is zero; the last
    /// (most-significant) limb is never zero.
    limbs: Vec<u64>,
}

/// The sign of a canonical integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sign {
    Negative,
    Zero,
    Positive,
}

/// Maximum logical magnitude limbs an allocating operation may produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LimbLimit(usize);

impl LimbLimit {
    pub const fn new(max_limbs: usize) -> Self {
        Self(max_limbs)
    }

    pub const fn max_limbs(self) -> usize {
        self.0
    }

    fn ensure(
        self,
        operation: ArithmeticOperation,
        required_limbs: usize,
    ) -> Result<(), ArithmeticError> {
        if required_limbs <= self.0 {
            Ok(())
        } else {
            Err(ArithmeticError::LimbLimitExceeded {
                operation,
                required_limbs,
                limit: self.0,
            })
        }
    }
}

/// Arithmetic operation reported by resource errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithmeticOperation {
    Clone,
    Negate,
    Add,
    Subtract,
    Multiply,
}

impl fmt::Display for ArithmeticOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clone => f.write_str("clone"),
            Self::Negate => f.write_str("negate"),
            Self::Add => f.write_str("add"),
            Self::Subtract => f.write_str("subtract"),
            Self::Multiply => f.write_str("multiply"),
        }
    }
}

/// Stable failure surface for bounded arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithmeticError {
    LimbLimitExceeded {
        operation: ArithmeticOperation,
        required_limbs: usize,
        limit: usize,
    },
    CapacityOverflow {
        operation: ArithmeticOperation,
    },
    AllocationFailed {
        operation: ArithmeticOperation,
        requested_limbs: usize,
    },
}

impl fmt::Display for ArithmeticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::LimbLimitExceeded {
                operation,
                required_limbs,
                limit,
            } => write!(
                f,
                "{operation} requires {required_limbs} magnitude limbs, limit is {limit}"
            ),
            Self::CapacityOverflow { operation } => {
                write!(f, "{operation} limb-count arithmetic overflowed")
            }
            Self::AllocationFailed {
                operation,
                requested_limbs,
            } => write!(
                f,
                "{operation} could not reserve {requested_limbs} magnitude limbs"
            ),
        }
    }
}

impl std::error::Error for ArithmeticError {}

/// Stable rejection surface for importing an already allocated canonical
/// magnitude from the separately owned codec layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstructionError {
    LimbLimitExceeded { required_limbs: usize, limit: usize },
    ZeroWithMagnitude { limb_count: usize },
    NonzeroSignWithoutMagnitude { sign: Sign },
    HighZeroLimb { sign: Sign, limb_count: usize },
}

impl fmt::Display for ConstructionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::LimbLimitExceeded {
                required_limbs,
                limit,
            } => write!(
                f,
                "canonical magnitude has {required_limbs} limbs, limit is {limit}"
            ),
            Self::ZeroWithMagnitude { limb_count } => {
                write!(f, "zero sign carried {limb_count} magnitude limbs")
            }
            Self::NonzeroSignWithoutMagnitude { sign } => {
                write!(f, "{sign:?} sign requires a nonempty magnitude")
            }
            Self::HighZeroLimb { sign, limb_count } => write!(
                f,
                "{sign:?} magnitude with {limb_count} limbs has a high zero limb"
            ),
        }
    }
}

impl std::error::Error for ConstructionError {}

impl BigInt {
    /// The canonical zero.
    pub const fn zero() -> Self {
        BigInt {
            negative: false,
            limbs: Vec::new(),
        }
    }

    /// True iff the value is zero.
    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    /// True iff the value is strictly negative.
    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// Verifies the representation invariants. Always true for values built
    /// through this crate's API; exposed so property suites can assert it.
    pub fn is_canonical(&self) -> bool {
        match self.limbs.last() {
            None => !self.negative,
            Some(&ms) => ms != 0,
        }
    }

    /// Number of magnitude limbs (0 for zero). Resource accounting for limb
    /// allocation hangs off this.
    pub fn limb_count(&self) -> usize {
        self.limbs.len()
    }

    /// Canonical sign; zero is never negative.
    pub fn sign(&self) -> Sign {
        if self.limbs.is_empty() {
            Sign::Zero
        } else if self.negative {
            Sign::Negative
        } else {
            Sign::Positive
        }
    }

    /// Borrowed canonical magnitude for the separately owned codec layer.
    ///
    /// The slice is little-endian in base 2^64 and has no high zero limb.
    /// It is an in-memory view, not a durable encoding.
    pub fn magnitude_limbs_le(&self) -> &[u64] {
        &self.limbs
    }

    /// Imports a canonical sign and little-endian magnitude without copying or
    /// defining any durable byte representation.
    ///
    /// A boxed slice makes the transferred allocation exactly length-shaped.
    /// The codec must enforce its byte-length bound before allocating that
    /// slice; this constructor independently enforces the logical limb limit.
    pub fn from_canonical_limbs(
        sign: Sign,
        limbs_le: Box<[u64]>,
        limit: LimbLimit,
    ) -> Result<Self, ConstructionError> {
        let limb_count = limbs_le.len();
        if limb_count > limit.max_limbs() {
            return Err(ConstructionError::LimbLimitExceeded {
                required_limbs: limb_count,
                limit: limit.max_limbs(),
            });
        }
        match (sign, limb_count) {
            (Sign::Zero, 0) => return Ok(Self::zero()),
            (Sign::Zero, _) => {
                return Err(ConstructionError::ZeroWithMagnitude { limb_count });
            }
            (Sign::Negative | Sign::Positive, 0) => {
                return Err(ConstructionError::NonzeroSignWithoutMagnitude { sign });
            }
            (Sign::Negative | Sign::Positive, _) => {}
        }
        if limbs_le.last() == Some(&0) {
            return Err(ConstructionError::HighZeroLimb { sign, limb_count });
        }
        Ok(Self {
            negative: sign == Sign::Negative,
            limbs: limbs_le.into_vec(),
        })
    }

    fn allocate_limbs(
        operation: ArithmeticOperation,
        required_limbs: usize,
        limit: LimbLimit,
    ) -> Result<Vec<u64>, ArithmeticError> {
        limit.ensure(operation, required_limbs)?;
        if required_limbs > (isize::MAX as usize) / std::mem::size_of::<u64>() {
            return Err(ArithmeticError::CapacityOverflow { operation });
        }
        let mut limbs = Vec::new();
        limbs
            .try_reserve_exact(required_limbs)
            .map_err(|_| ArithmeticError::AllocationFailed {
                operation,
                requested_limbs: required_limbs,
            })?;
        Ok(limbs)
    }

    fn from_sign_magnitude(negative: bool, mut limbs: Vec<u64>) -> Self {
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        let negative = negative && !limbs.is_empty();
        let out = BigInt { negative, limbs };
        debug_assert!(out.is_canonical());
        out
    }

    pub fn from_i64(v: i64) -> Self {
        Self::from_i128(v as i128)
    }

    pub fn from_u64(v: u64) -> Self {
        if v == 0 {
            Self::zero()
        } else {
            Self {
                negative: false,
                limbs: vec![v],
            }
        }
    }

    pub fn from_u128(v: u128) -> Self {
        let lo = (v & u128::from(u64::MAX)) as u64;
        let hi = (v >> 64) as u64;
        if hi != 0 {
            Self {
                negative: false,
                limbs: vec![lo, hi],
            }
        } else {
            Self::from_u64(lo)
        }
    }

    pub fn from_i128(v: i128) -> Self {
        let negative = v < 0;
        // i128::MIN magnitude does not fit in i128; go through u128.
        let mag = v.unsigned_abs();
        let lo = (mag & u128::from(u64::MAX)) as u64;
        let hi = (mag >> 64) as u64;
        let limbs = if hi != 0 {
            vec![lo, hi]
        } else if lo != 0 {
            vec![lo]
        } else {
            return Self::zero();
        };
        Self { negative, limbs }
    }

    /// Fallibly copies this value within an explicit limb budget.
    pub fn checked_clone(&self, limit: LimbLimit) -> Result<Self, ArithmeticError> {
        let mut limbs = Self::allocate_limbs(ArithmeticOperation::Clone, self.limbs.len(), limit)?;
        limbs.extend_from_slice(&self.limbs);
        Ok(Self {
            negative: self.negative,
            limbs,
        })
    }

    /// Converts back to `i128` when the value fits, `None` otherwise.
    /// This is the demotion side of the `ZWeight` promotion boundary.
    pub fn to_i128(&self) -> Option<i128> {
        if self.limbs.len() > 2 {
            return None;
        }
        let lo = u128::from(*self.limbs.first().unwrap_or(&0));
        let hi = u128::from(*self.limbs.get(1).unwrap_or(&0));
        let mag = (hi << 64) | lo;
        if self.negative {
            // |i128::MIN| = 2^127.
            if mag > 1u128 << 127 {
                return None;
            }
            if mag == 1u128 << 127 {
                return Some(i128::MIN);
            }
            Some(-(mag as i128))
        } else {
            if mag > i128::MAX as u128 {
                return None;
            }
            Some(mag as i128)
        }
    }

    fn cmp_magnitude(a: &[u64], b: &[u64]) -> Ordering {
        if a.len() != b.len() {
            return a.len().cmp(&b.len());
        }
        for (x, y) in a.iter().rev().zip(b.iter().rev()) {
            match x.cmp(y) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        Ordering::Equal
    }

    fn add_magnitude_required(a: &[u64], b: &[u64]) -> Result<usize, ArithmeticError> {
        let length = a.len().max(b.len());
        let mut carry = 0u128;
        for index in 0..length {
            carry = u128::from(a.get(index).copied().unwrap_or(0))
                + u128::from(b.get(index).copied().unwrap_or(0))
                + (carry >> 64);
        }
        if carry >> 64 == 0 {
            Ok(length)
        } else {
            length
                .checked_add(1)
                .ok_or(ArithmeticError::CapacityOverflow {
                    operation: ArithmeticOperation::Add,
                })
        }
    }

    fn add_magnitude(
        a: &[u64],
        b: &[u64],
        operation: ArithmeticOperation,
        limit: LimbLimit,
    ) -> Result<Vec<u64>, ArithmeticError> {
        let required = Self::add_magnitude_required(a, b)
            .map_err(|_| ArithmeticError::CapacityOverflow { operation })?;
        let mut out = Self::allocate_limbs(operation, required, limit)?;
        let mut carry = 0u128;
        for index in 0..required {
            let sum = u128::from(a.get(index).copied().unwrap_or(0))
                + u128::from(b.get(index).copied().unwrap_or(0))
                + carry;
            out.push(sum as u64);
            carry = sum >> 64;
        }
        debug_assert_eq!(carry, 0);
        Ok(out)
    }

    /// Returns the exact canonical result length. Requires `a >= b`.
    fn sub_magnitude_required(a: &[u64], b: &[u64]) -> usize {
        debug_assert!(Self::cmp_magnitude(a, b) != Ordering::Less);
        let mut borrow = 0u64;
        let mut required = 0;
        for (index, &x) in a.iter().enumerate() {
            let y = b.get(index).copied().unwrap_or(0);
            let (difference, first_borrow) = x.overflowing_sub(y);
            let (difference, second_borrow) = difference.overflowing_sub(borrow);
            borrow = u64::from(first_borrow || second_borrow);
            if difference != 0 {
                required = index + 1;
            }
        }
        debug_assert_eq!(borrow, 0);
        required
    }

    fn sub_magnitude(
        a: &[u64],
        b: &[u64],
        operation: ArithmeticOperation,
        limit: LimbLimit,
    ) -> Result<Vec<u64>, ArithmeticError> {
        let required = Self::sub_magnitude_required(a, b);
        let mut out = Self::allocate_limbs(operation, required, limit)?;
        let mut borrow = 0u64;
        for (index, &x) in a.iter().enumerate() {
            let y = b.get(index).copied().unwrap_or(0);
            let (difference, first_borrow) = x.overflowing_sub(y);
            let (difference, second_borrow) = difference.overflowing_sub(borrow);
            borrow = u64::from(first_borrow || second_borrow);
            if index < required {
                out.push(difference);
            }
        }
        debug_assert_eq!(borrow, 0);
        Ok(out)
    }

    fn add_product_to_accumulator(
        accumulator: &mut [u64; 3],
        left: u64,
        right: u64,
    ) -> Result<(), ArithmeticError> {
        let product = u128::from(left) * u128::from(right);
        let (low, carry_low) = accumulator[0].overflowing_add(product as u64);
        accumulator[0] = low;
        let (high, carry_high) = accumulator[1].overflowing_add((product >> 64) as u64);
        let (high, carry_from_low) = high.overflowing_add(u64::from(carry_low));
        accumulator[1] = high;
        let carry = u64::from(carry_high || carry_from_low);
        accumulator[2] =
            accumulator[2]
                .checked_add(carry)
                .ok_or(ArithmeticError::CapacityOverflow {
                    operation: ArithmeticOperation::Multiply,
                })?;
        Ok(())
    }

    fn accumulate_product_column(
        a: &[u64],
        b: &[u64],
        column: usize,
        accumulator: &mut [u64; 3],
    ) -> Result<(), ArithmeticError> {
        let first_left = column.saturating_sub(b.len() - 1);
        let last_left = column.min(a.len() - 1);
        if first_left <= last_left {
            for (left_index, &left) in a.iter().enumerate().take(last_left + 1).skip(first_left) {
                let right_index = column - left_index;
                Self::add_product_to_accumulator(accumulator, left, b[right_index])?;
            }
        }
        Ok(())
    }

    fn shift_product_accumulator(accumulator: &mut [u64; 3]) {
        *accumulator = [accumulator[1], accumulator[2], 0];
    }

    fn mul_magnitude_required(a: &[u64], b: &[u64]) -> Result<usize, ArithmeticError> {
        if a.is_empty() || b.is_empty() {
            return Ok(0);
        }
        let columns = a
            .len()
            .checked_add(b.len())
            .and_then(|sum| sum.checked_sub(1))
            .ok_or(ArithmeticError::CapacityOverflow {
                operation: ArithmeticOperation::Multiply,
            })?;
        let mut accumulator = [0u64; 3];
        let mut required = 0;
        for column in 0..columns {
            Self::accumulate_product_column(a, b, column, &mut accumulator)?;
            if accumulator[0] != 0 {
                required = column + 1;
            }
            Self::shift_product_accumulator(&mut accumulator);
        }
        let mut column = columns;
        while accumulator != [0, 0, 0] {
            if accumulator[0] != 0 {
                required = column
                    .checked_add(1)
                    .ok_or(ArithmeticError::CapacityOverflow {
                        operation: ArithmeticOperation::Multiply,
                    })?;
            }
            Self::shift_product_accumulator(&mut accumulator);
            if accumulator != [0, 0, 0] {
                column = column
                    .checked_add(1)
                    .ok_or(ArithmeticError::CapacityOverflow {
                        operation: ArithmeticOperation::Multiply,
                    })?;
            }
        }
        Ok(required)
    }

    fn mul_magnitude(a: &[u64], b: &[u64], limit: LimbLimit) -> Result<Vec<u64>, ArithmeticError> {
        let required = Self::mul_magnitude_required(a, b)?;
        let mut out = Self::allocate_limbs(ArithmeticOperation::Multiply, required, limit)?;
        if required == 0 {
            return Ok(out);
        }
        let source_columns = a
            .len()
            .checked_add(b.len())
            .and_then(|sum| sum.checked_sub(1))
            .ok_or(ArithmeticError::CapacityOverflow {
                operation: ArithmeticOperation::Multiply,
            })?;
        let mut accumulator = [0u64; 3];
        for column in 0..required {
            if column < source_columns {
                Self::accumulate_product_column(a, b, column, &mut accumulator)?;
            }
            out.push(accumulator[0]);
            Self::shift_product_accumulator(&mut accumulator);
        }
        debug_assert_ne!(out.last(), Some(&0));
        Ok(out)
    }

    pub fn checked_neg(&self, limit: LimbLimit) -> Result<Self, ArithmeticError> {
        let mut limbs = Self::allocate_limbs(ArithmeticOperation::Negate, self.limbs.len(), limit)?;
        limbs.extend_from_slice(&self.limbs);
        Ok(Self {
            negative: !self.negative && !limbs.is_empty(),
            limbs,
        })
    }

    pub fn checked_add(&self, other: &Self, limit: LimbLimit) -> Result<Self, ArithmeticError> {
        if self.negative == other.negative {
            return Ok(Self::from_sign_magnitude(
                self.negative,
                Self::add_magnitude(&self.limbs, &other.limbs, ArithmeticOperation::Add, limit)?,
            ));
        }
        match Self::cmp_magnitude(&self.limbs, &other.limbs) {
            Ordering::Equal => Ok(Self::zero()),
            Ordering::Greater => Ok(Self::from_sign_magnitude(
                self.negative,
                Self::sub_magnitude(&self.limbs, &other.limbs, ArithmeticOperation::Add, limit)?,
            )),
            Ordering::Less => Ok(Self::from_sign_magnitude(
                other.negative,
                Self::sub_magnitude(&other.limbs, &self.limbs, ArithmeticOperation::Add, limit)?,
            )),
        }
    }

    pub fn checked_sub(&self, other: &Self, limit: LimbLimit) -> Result<Self, ArithmeticError> {
        if self.negative != other.negative {
            return Ok(Self::from_sign_magnitude(
                self.negative,
                Self::add_magnitude(
                    &self.limbs,
                    &other.limbs,
                    ArithmeticOperation::Subtract,
                    limit,
                )?,
            ));
        }
        match Self::cmp_magnitude(&self.limbs, &other.limbs) {
            Ordering::Equal => Ok(Self::zero()),
            Ordering::Greater => Ok(Self::from_sign_magnitude(
                self.negative,
                Self::sub_magnitude(
                    &self.limbs,
                    &other.limbs,
                    ArithmeticOperation::Subtract,
                    limit,
                )?,
            )),
            Ordering::Less => Ok(Self::from_sign_magnitude(
                !self.negative,
                Self::sub_magnitude(
                    &other.limbs,
                    &self.limbs,
                    ArithmeticOperation::Subtract,
                    limit,
                )?,
            )),
        }
    }

    pub fn checked_mul(&self, other: &Self, limit: LimbLimit) -> Result<Self, ArithmeticError> {
        // `LimbLimit` bounds result allocation. CPU work is intentionally a
        // separate concern for the downstream resource-ledger/Cx wrapper.
        Ok(Self::from_sign_magnitude(
            self.negative != other.negative,
            Self::mul_magnitude(&self.limbs, &other.limbs, limit)?,
        ))
    }
}

impl PartialOrd for BigInt {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BigInt {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => Self::cmp_magnitude(&self.limbs, &other.limbs),
            (true, true) => Self::cmp_magnitude(&other.limbs, &self.limbs),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    const TEST_LIMIT: LimbLimit = LimbLimit::new(64);

    /// SplitMix64: deterministic, seed-reported PRNG (std-only; the closed
    /// dependency universe applies to test tooling too).
    struct SplitMix64(u64);
    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }
        fn bigint(&mut self, max_limbs: usize) -> BigInt {
            let n = (self.next() as usize) % (max_limbs + 1);
            let limbs: Vec<u64> = (0..n).map(|_| self.next()).collect();
            BigInt::from_sign_magnitude(self.next() % 2 == 1, limbs)
        }
    }

    fn hash_of(v: &BigInt) -> u64 {
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    /// Independent multiplication oracle: decompose the right magnitude into
    /// bits and use only signed-limb addition and doubling.
    fn shift_add_product(left: &BigInt, right: &BigInt) -> BigInt {
        let mut result = BigInt::zero();
        let mut addend = BigInt::from_sign_magnitude(false, left.limbs.clone());
        for &limb in &right.limbs {
            for bit in 0..64 {
                if limb & (1u64 << bit) != 0 {
                    result = result
                        .checked_add(&addend, TEST_LIMIT)
                        .expect("shift-add oracle result fits its test budget");
                }
                addend = addend
                    .checked_add(&addend, TEST_LIMIT)
                    .expect("shift-add oracle addend fits its test budget");
            }
        }
        if left.negative != right.negative {
            result
                .checked_neg(TEST_LIMIT)
                .expect("shift-add oracle sign fits its test budget")
        } else {
            result
        }
    }

    #[test]
    fn zero_is_canonical_and_unique() {
        assert!(BigInt::zero().is_canonical());
        assert_eq!(BigInt::from_i128(0), BigInt::zero());
        assert_eq!(BigInt::from_u64(0), BigInt::zero());
        assert_eq!(BigInt::from_u128(0), BigInt::zero());
        assert_eq!(BigInt::zero().sign(), Sign::Zero);
        assert_eq!(BigInt::zero().magnitude_limbs_le(), &[]);

        let five = BigInt::from_i128(5);
        let cancellation = five
            .checked_sub(&five, LimbLimit::new(0))
            .expect("cancellation requires no limb budget");
        assert_eq!(cancellation, BigInt::zero());
        assert_eq!(cancellation.sign(), Sign::Zero);
        assert!(!cancellation.is_negative());
    }

    #[test]
    fn scalar_constructors_retain_no_known_zero_limb_slack() {
        for value in [
            BigInt::from_u64(0),
            BigInt::from_u64(1),
            BigInt::from_u128(0),
            BigInt::from_u128(u128::from(u64::MAX)),
            BigInt::from_u128(u128::from(u64::MAX) + 1),
            BigInt::from_i128(0),
            BigInt::from_i128(-1),
            BigInt::from_i128(i128::MIN),
        ] {
            assert_eq!(value.limbs.capacity(), value.limbs.len());
            assert!(value.is_canonical());
        }
    }

    #[test]
    fn canonical_limb_import_is_zero_copy_bounded_and_strict() {
        let negative = BigInt::from_canonical_limbs(
            Sign::Negative,
            vec![0, 1].into_boxed_slice(),
            LimbLimit::new(2),
        )
        .expect("canonical two-limb input");
        assert_eq!(negative.sign(), Sign::Negative);
        assert_eq!(negative.magnitude_limbs_le(), &[0, 1]);
        assert_eq!(negative.limbs.capacity(), negative.limbs.len());

        assert_eq!(
            BigInt::from_canonical_limbs(
                Sign::Positive,
                vec![0, 1].into_boxed_slice(),
                LimbLimit::new(1),
            ),
            Err(ConstructionError::LimbLimitExceeded {
                required_limbs: 2,
                limit: 1,
            })
        );
        assert_eq!(
            BigInt::from_canonical_limbs(Sign::Zero, vec![1].into_boxed_slice(), LimbLimit::new(1),),
            Err(ConstructionError::ZeroWithMagnitude { limb_count: 1 })
        );
        assert_eq!(
            BigInt::from_canonical_limbs(
                Sign::Positive,
                Vec::new().into_boxed_slice(),
                LimbLimit::new(0),
            ),
            Err(ConstructionError::NonzeroSignWithoutMagnitude {
                sign: Sign::Positive,
            })
        );
        assert_eq!(
            BigInt::from_canonical_limbs(
                Sign::Negative,
                vec![1, 0].into_boxed_slice(),
                LimbLimit::new(2),
            ),
            Err(ConstructionError::HighZeroLimb {
                sign: Sign::Negative,
                limb_count: 2,
            })
        );
    }

    #[test]
    fn i128_round_trip_and_boundaries() {
        for v in [
            0i128,
            1,
            -1,
            i128::from(i64::MAX),
            i128::from(i64::MIN),
            i128::MAX,
            i128::MIN,
            i128::MAX - 1,
            i128::MIN + 1,
        ] {
            let b = BigInt::from_i128(v);
            assert!(b.is_canonical(), "not canonical for {v}");
            assert_eq!(b.to_i128(), Some(v), "round trip failed for {v}");
        }
        // Just past the promotion boundary: |x| = 2^127 (positive side) must
        // demote to None; the negative side is exactly i128::MIN.
        let two127 = BigInt::from_i128(i128::MAX)
            .checked_add(&BigInt::from_i128(1), TEST_LIMIT)
            .expect("2^127 fits the test budget");
        assert_eq!(two127.to_i128(), None);
        let negative_two127 = two127
            .checked_neg(TEST_LIMIT)
            .expect("negation fits the test budget");
        assert_eq!(negative_two127.to_i128(), Some(i128::MIN));
        assert_eq!(
            negative_two127
                .checked_sub(&BigInt::from_i128(1), TEST_LIMIT)
                .expect("value below i128::MIN fits the test budget")
                .to_i128(),
            None
        );
    }

    #[test]
    fn carry_borrow_and_product_cross_limb_boundaries() {
        let max = BigInt::from_u64(u64::MAX);
        let one = BigInt::from_u64(1);
        let two_to_64 = max
            .checked_add(&one, LimbLimit::new(2))
            .expect("carry creates exactly two limbs");
        assert_eq!(two_to_64.magnitude_limbs_le(), &[0, 1]);
        assert_eq!(
            two_to_64
                .checked_sub(&one, LimbLimit::new(1))
                .expect("borrow normalizes back to one limb")
                .magnitude_limbs_le(),
            &[u64::MAX]
        );

        let square = max
            .checked_mul(&max, LimbLimit::new(2))
            .expect("u64::MAX squared occupies two limbs");
        assert_eq!(square.magnitude_limbs_le(), &[1, u64::MAX - 1]);
        assert!(square.is_canonical());

        let max_three = BigInt::from_sign_magnitude(false, vec![u64::MAX, u64::MAX, u64::MAX]);
        let carried = max_three
            .checked_add(&one, LimbLimit::new(4))
            .expect("three-limb carry chain fits four limbs");
        assert_eq!(carried.magnitude_limbs_le(), &[0, 0, 0, 1]);
        assert_eq!(
            carried
                .checked_sub(&one, LimbLimit::new(3))
                .expect("three-limb borrow chain returns to its input"),
            max_three
        );

        let two_to_64_squared = two_to_64
            .checked_mul(&two_to_64, LimbLimit::new(3))
            .expect("two-by-two product takes n+m-1 limbs");
        assert_eq!(two_to_64_squared.magnitude_limbs_le(), &[0, 0, 1]);
        assert_eq!(
            two_to_64.checked_mul(&two_to_64, LimbLimit::new(2)),
            Err(ArithmeticError::LimbLimitExceeded {
                operation: ArithmeticOperation::Multiply,
                required_limbs: 3,
                limit: 2,
            })
        );

        let max_two = BigInt::from_sign_magnitude(false, vec![u64::MAX, u64::MAX]);
        let four_limb_product = max_two
            .checked_mul(&max_two, LimbLimit::new(4))
            .expect("maximal two-by-two product takes n+m limbs");
        assert_eq!(four_limb_product.limb_count(), 4);
        assert_eq!(
            max_two.checked_mul(&max_two, LimbLimit::new(3)),
            Err(ArithmeticError::LimbLimitExceeded {
                operation: ArithmeticOperation::Multiply,
                required_limbs: 4,
                limit: 3,
            })
        );
    }

    #[test]
    fn multi_limb_products_match_shift_add_oracle_and_exact_budget() {
        for seed in [19u64, 0x5EED_5EED, u64::MAX - 1] {
            let mut rng = SplitMix64(seed);
            for _ in 0..80 {
                let left = rng.bigint(3);
                let right = rng.bigint(3);
                let expected = shift_add_product(&left, &right);
                let actual = left
                    .checked_mul(&right, TEST_LIMIT)
                    .expect("three-limb product fits the test budget");
                assert_eq!(
                    actual, expected,
                    "seed={seed}, left={left:?}, right={right:?}"
                );

                let required_limbs = expected.limb_count();
                if required_limbs != 0 {
                    let limit = required_limbs - 1;
                    assert_eq!(
                        left.checked_mul(&right, LimbLimit::new(limit)),
                        Err(ArithmeticError::LimbLimitExceeded {
                            operation: ArithmeticOperation::Multiply,
                            required_limbs,
                            limit,
                        }),
                        "seed={seed}, multiplication did not report its exact result size"
                    );
                }
            }
        }
    }

    #[test]
    fn arithmetic_requires_and_reports_exact_limb_budgets() {
        let one_limb = BigInt::from_u64(u64::MAX);
        let one = BigInt::from_u64(1);
        let two_limbs = one_limb
            .checked_add(&one, LimbLimit::new(2))
            .expect("setup value");

        for (actual, operation) in [
            (
                two_limbs.checked_clone(LimbLimit::new(1)),
                ArithmeticOperation::Clone,
            ),
            (
                two_limbs.checked_neg(LimbLimit::new(1)),
                ArithmeticOperation::Negate,
            ),
            (
                one_limb.checked_add(&one, LimbLimit::new(1)),
                ArithmeticOperation::Add,
            ),
            (
                BigInt::from_i128(i128::MAX).checked_sub(&BigInt::from_i128(-1), LimbLimit::new(1)),
                ArithmeticOperation::Subtract,
            ),
            (
                one_limb.checked_mul(&one_limb, LimbLimit::new(1)),
                ArithmeticOperation::Multiply,
            ),
        ] {
            assert_eq!(
                actual,
                Err(ArithmeticError::LimbLimitExceeded {
                    operation,
                    required_limbs: 2,
                    limit: 1,
                })
            );
        }

        assert_eq!(
            BigInt::zero()
                .checked_mul(&two_limbs, LimbLimit::new(0))
                .expect("zero product allocates no limbs"),
            BigInt::zero()
        );
        assert_eq!(
            one.checked_sub(&one, LimbLimit::new(0))
                .expect("cancellation allocates no limbs"),
            BigInt::zero()
        );
        assert_eq!(
            one.checked_add(&BigInt::zero(), LimbLimit::new(0)),
            Err(ArithmeticError::LimbLimitExceeded {
                operation: ArithmeticOperation::Add,
                required_limbs: 1,
                limit: 0,
            })
        );

        assert_eq!(
            BigInt::allocate_limbs(
                ArithmeticOperation::Clone,
                (isize::MAX as usize) / std::mem::size_of::<u64>() + 1,
                LimbLimit::new(usize::MAX),
            ),
            Err(ArithmeticError::CapacityOverflow {
                operation: ArithmeticOperation::Clone,
            })
        );
    }

    #[test]
    fn ring_laws_hold_on_randomized_values() {
        for seed in [1u64, 0xDEAD_BEEF, 0xF6DB, u64::MAX] {
            let mut rng = SplitMix64(seed);
            for _ in 0..200 {
                let a = rng.bigint(5);
                let b = rng.bigint(5);
                let c = rng.bigint(5);
                let ab = a.checked_add(&b, TEST_LIMIT).expect("a + b");
                let ba = b.checked_add(&a, TEST_LIMIT).expect("b + a");
                assert_eq!(ab, ba, "seed={seed}, a={a:?}, b={b:?}, c={c:?}");

                let ab = a.checked_mul(&b, TEST_LIMIT).expect("a * b");
                let ba = b.checked_mul(&a, TEST_LIMIT).expect("b * a");
                assert_eq!(ab, ba, "seed={seed}, a={a:?}, b={b:?}, c={c:?}");

                let left = a
                    .checked_add(&b, TEST_LIMIT)
                    .expect("a + b")
                    .checked_add(&c, TEST_LIMIT)
                    .expect("(a + b) + c");
                let right_inner = b.checked_add(&c, TEST_LIMIT).expect("b + c");
                let right = a
                    .checked_add(&right_inner, TEST_LIMIT)
                    .expect("a + (b + c)");
                assert_eq!(left, right, "seed={seed}, add associativity");

                let left = a
                    .checked_mul(&b, TEST_LIMIT)
                    .expect("a * b")
                    .checked_mul(&c, TEST_LIMIT)
                    .expect("(a * b) * c");
                let right_inner = b.checked_mul(&c, TEST_LIMIT).expect("b * c");
                let right = a
                    .checked_mul(&right_inner, TEST_LIMIT)
                    .expect("a * (b * c)");
                assert_eq!(left, right, "seed={seed}, multiply associativity");

                let b_plus_c = b.checked_add(&c, TEST_LIMIT).expect("b + c");
                let left = a.checked_mul(&b_plus_c, TEST_LIMIT).expect("a * (b + c)");
                let ab = a.checked_mul(&b, TEST_LIMIT).expect("a * b");
                let ac = a.checked_mul(&c, TEST_LIMIT).expect("a * c");
                let right = ab.checked_add(&ac, TEST_LIMIT).expect("ab + ac");
                assert_eq!(left, right, "seed={seed}, distributivity");

                let negative_a = a.checked_neg(TEST_LIMIT).expect("-a");
                assert_eq!(
                    a.checked_add(&negative_a, LimbLimit::new(0))
                        .expect("a + -a"),
                    BigInt::zero(),
                    "seed={seed}, additive inverse"
                );
                assert!(a.is_canonical() && b.is_canonical() && c.is_canonical());
            }
        }
    }

    #[test]
    fn i128_agreement_for_in_range_arithmetic() {
        for seed in [7u64, 42, 0xC0FFEE] {
            let mut rng = SplitMix64(seed);
            for _ in 0..500 {
                let x = rng.next() as i64;
                let y = rng.next() as i64;
                let (bx, by) = (BigInt::from_i64(x), BigInt::from_i64(y));
                let ctx = format!("seed={seed} x={x} y={y}");
                assert_eq!(
                    bx.checked_add(&by, TEST_LIMIT).expect("i64 sum").to_i128(),
                    Some(i128::from(x) + i128::from(y)),
                    "sum: {ctx}"
                );
                assert_eq!(
                    bx.checked_sub(&by, TEST_LIMIT)
                        .expect("i64 difference")
                        .to_i128(),
                    Some(i128::from(x) - i128::from(y)),
                    "difference: {ctx}"
                );
                assert_eq!(
                    bx.checked_mul(&by, TEST_LIMIT)
                        .expect("i64 product")
                        .to_i128(),
                    Some(i128::from(x) * i128::from(y)),
                    "product: {ctx}"
                );
                assert_eq!(bx.cmp(&by), x.cmp(&y), "order agreement: {ctx}");
            }
        }
    }

    #[test]
    fn equal_values_have_identical_hashes_and_representations() {
        for seed in [3u64, 0xB16B00B5, 0x1234_5678_9ABC_DEF0] {
            let mut rng = SplitMix64(seed);
            for _ in 0..300 {
                let a = rng.bigint(6);
                // A second construction path must reach the identical
                // canonical value: (a + 1) - 1.
                let one = BigInt::from_i128(1);
                let b = a
                    .checked_add(&one, TEST_LIMIT)
                    .expect("a + 1")
                    .checked_sub(&one, TEST_LIMIT)
                    .expect("(a + 1) - 1");
                assert_eq!(b, a, "seed={seed} canonical uniqueness via arithmetic");
                assert_eq!(hash_of(&b), hash_of(&a), "seed={seed} equal => same hash");
                assert_eq!(
                    b.magnitude_limbs_le(),
                    a.magnitude_limbs_le(),
                    "seed={seed} equal => one normalized limb representation"
                );
                assert_eq!(b.sign(), a.sign(), "seed={seed} equal => same sign");
            }
        }
    }

    #[test]
    fn ordering_is_total_and_transitive_on_random_triples() {
        for seed in [11u64, 0xACE0FBA5E] {
            let mut rng = SplitMix64(seed);
            for _ in 0..300 {
                let mut v = [rng.bigint(4), rng.bigint(4), rng.bigint(4)];
                v.sort();
                assert!(
                    v[0] <= v[1] && v[1] <= v[2] && v[0] <= v[2],
                    "seed={seed} transitivity broke on {v:?}"
                );
            }
        }
    }
}
