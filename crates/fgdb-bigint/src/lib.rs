//! Canonical, allocation-bounded signed-limb exact integers.
//!
//! [`BigInt`] is the exact arithmetic kernel that Ripple's checked `i128`
//! `ZWeight` fast path will promote into before overflow. Its private
//! representation is normalized, so equality, total order, and hashing agree
//! structurally. This crate intentionally defines no durable byte encoding;
//! generated format code owns that contract.
//!
//! Fixed-width scalar constructors allocate at most two limbs. Every operation
//! that can accept, produce, or reserve a variable number of limbs requires an
//! explicit [`LimbLimit`]. There is no unlimited arithmetic overload.
//!
//! ```
//! use fgdb_bigint::{BigInt, LimbLimit, Sign};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let limit = LimbLimit::new(4);
//! let imported =
//!     BigInt::from_canonical_limbs(Sign::Positive, vec![1].into_boxed_slice(), limit)?;
//! let one = BigInt::from_u64(1);
//! let _ = imported.checked_clone(limit)?;
//! let _ = imported.checked_neg(limit)?;
//! let _ = imported.checked_add(&one, limit)?;
//! let _ = imported.checked_sub(&one, limit)?;
//! let _ = imported.checked_mul(&one, limit)?;
//! let _ = imported.checked_div_rem(&one, limit)?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

use std::{cmp::Ordering, fmt};

/// A canonical arbitrary-precision signed integer.
///
/// Its variable-size representation cannot be constructed directly.
///
/// ```compile_fail,E0451
/// use fgdb_bigint::BigInt;
///
/// let _ = BigInt {
///     negative: false,
///     limbs: vec![1],
/// };
/// ```
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
    Divide,
}

impl fmt::Display for ArithmeticOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clone => f.write_str("clone"),
            Self::Negate => f.write_str("negate"),
            Self::Add => f.write_str("add"),
            Self::Subtract => f.write_str("subtract"),
            Self::Multiply => f.write_str("multiply"),
            Self::Divide => f.write_str("divide"),
        }
    }
}

/// Stable failure surface for bounded arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithmeticError {
    DivisionByZero,
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
            Self::DivisionByZero => f.write_str("cannot divide by zero"),
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
    ///
    /// ```compile_fail,E0061
    /// use fgdb_bigint::{BigInt, Sign};
    ///
    /// let _ = BigInt::from_canonical_limbs(
    ///     Sign::Positive,
    ///     vec![1].into_boxed_slice(),
    /// );
    /// ```
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

    fn allocate_workspace_limbs(
        operation: ArithmeticOperation,
        required_limbs: usize,
        limit: LimbLimit,
    ) -> Result<Vec<u64>, ArithmeticError> {
        Self::allocate_limbs(operation, required_limbs, limit)
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
    ///
    /// ```compile_fail,E0061
    /// use fgdb_bigint::BigInt;
    ///
    /// let value = BigInt::from_u64(1);
    /// let _ = value.checked_clone();
    /// ```
    ///
    /// `BigInt` deliberately does not implement the unbounded `Clone` trait.
    ///
    /// ```compile_fail,E0599
    /// use fgdb_bigint::BigInt;
    ///
    /// let value = BigInt::from_u64(1);
    /// let _ = value.clone();
    /// ```
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

    fn quotient_limb_count(dividend: &[u64], divisor: &[u64]) -> Result<usize, ArithmeticError> {
        debug_assert!(!divisor.is_empty());
        debug_assert!(Self::cmp_magnitude(dividend, divisor) != Ordering::Less);
        let limb_offset = dividend.len() - divisor.len();
        if Self::cmp_magnitude(&dividend[limb_offset..], divisor) == Ordering::Less {
            Ok(limb_offset)
        } else {
            limb_offset
                .checked_add(1)
                .ok_or(ArithmeticError::CapacityOverflow {
                    operation: ArithmeticOperation::Divide,
                })
        }
    }

    fn div_rem_magnitude_by_limb(
        dividend: &[u64],
        divisor: u64,
        quotient_limbs: usize,
        limit: LimbLimit,
    ) -> Result<(Vec<u64>, Vec<u64>), ArithmeticError> {
        debug_assert_ne!(divisor, 0);
        limit.ensure(ArithmeticOperation::Divide, quotient_limbs)?;
        let mut quotient =
            Self::allocate_limbs(ArithmeticOperation::Divide, quotient_limbs, limit)?;
        quotient.resize(quotient_limbs, 0);

        let mut remainder = 0u128;
        for (index, &limb) in dividend.iter().enumerate().rev() {
            let partial = (remainder << 64) | u128::from(limb);
            let quotient_limb = partial / u128::from(divisor);
            remainder = partial % u128::from(divisor);
            if index < quotient_limbs {
                quotient[index] = quotient_limb as u64;
            } else {
                debug_assert_eq!(quotient_limb, 0);
            }
        }

        let remainder_limbs = usize::from(remainder != 0);
        limit.ensure(ArithmeticOperation::Divide, remainder_limbs)?;
        let mut remainder_out =
            Self::allocate_limbs(ArithmeticOperation::Divide, remainder_limbs, limit)?;
        if remainder != 0 {
            remainder_out.push(remainder as u64);
        }
        debug_assert_ne!(quotient.last(), Some(&0));
        Ok((quotient, remainder_out))
    }

    fn normalized_division_operand(
        magnitude: &[u64],
        shift: u32,
        extra_high_limb: bool,
        limit: LimbLimit,
    ) -> Result<Vec<u64>, ArithmeticError> {
        let required_limbs = magnitude
            .len()
            .checked_add(usize::from(extra_high_limb))
            .ok_or(ArithmeticError::CapacityOverflow {
                operation: ArithmeticOperation::Divide,
            })?;
        let mut normalized =
            Self::allocate_workspace_limbs(ArithmeticOperation::Divide, required_limbs, limit)?;
        if shift == 0 {
            normalized.extend_from_slice(magnitude);
            if extra_high_limb {
                normalized.push(0);
            }
            return Ok(normalized);
        }

        let mut carry = 0u64;
        for &limb in magnitude {
            normalized.push((limb << shift) | carry);
            carry = limb >> (64 - shift);
        }
        if extra_high_limb {
            normalized.push(carry);
        } else {
            debug_assert_eq!(carry, 0);
        }
        Ok(normalized)
    }

    fn subtract_divisor_product(
        normalized_dividend: &mut [u64],
        offset: usize,
        normalized_divisor: &[u64],
        quotient_limb: u64,
    ) -> bool {
        let mut product_carry = 0u128;
        let mut borrow = 0u64;
        for (index, &divisor_limb) in normalized_divisor.iter().enumerate() {
            let product = u128::from(quotient_limb) * u128::from(divisor_limb) + product_carry;
            product_carry = product >> 64;
            let (difference, first_borrow) =
                normalized_dividend[offset + index].overflowing_sub(product as u64);
            let (difference, second_borrow) = difference.overflowing_sub(borrow);
            normalized_dividend[offset + index] = difference;
            borrow = u64::from(first_borrow || second_borrow);
        }

        let high_index = offset + normalized_divisor.len();
        let high_subtrahend = product_carry + u128::from(borrow);
        let high_limb = u128::from(normalized_dividend[high_index]);
        if high_limb >= high_subtrahend {
            normalized_dividend[high_index] = (high_limb - high_subtrahend) as u64;
            false
        } else {
            normalized_dividend[high_index] = ((1u128 << 64) + high_limb - high_subtrahend) as u64;
            true
        }
    }

    fn add_divisor_back(
        normalized_dividend: &mut [u64],
        offset: usize,
        normalized_divisor: &[u64],
    ) {
        let mut carry = 0u128;
        for (index, &divisor_limb) in normalized_divisor.iter().enumerate() {
            let sum =
                u128::from(normalized_dividend[offset + index]) + u128::from(divisor_limb) + carry;
            normalized_dividend[offset + index] = sum as u64;
            carry = sum >> 64;
        }
        let high_index = offset + normalized_divisor.len();
        normalized_dividend[high_index] =
            normalized_dividend[high_index].wrapping_add(carry as u64);
    }

    fn div_rem_magnitude_knuth(
        dividend: &[u64],
        divisor: &[u64],
        quotient_limbs: usize,
        limit: LimbLimit,
    ) -> Result<(Vec<u64>, Vec<u64>), ArithmeticError> {
        debug_assert!(divisor.len() >= 2);
        debug_assert!(Self::cmp_magnitude(dividend, divisor) == Ordering::Greater);
        debug_assert!(quotient_limbs > 0);

        limit.ensure(ArithmeticOperation::Divide, quotient_limbs)?;
        let mut quotient =
            Self::allocate_limbs(ArithmeticOperation::Divide, quotient_limbs, limit)?;
        quotient.resize(quotient_limbs, 0);

        let Some(&divisor_most_significant) = divisor.last() else {
            return Err(ArithmeticError::DivisionByZero);
        };
        let shift = divisor_most_significant.leading_zeros();
        let normalized_divisor = Self::normalized_division_operand(divisor, shift, false, limit)?;
        let mut normalized_dividend =
            Self::normalized_division_operand(dividend, shift, true, limit)?;
        let divisor_high = u128::from(normalized_divisor[divisor.len() - 1]);
        let divisor_next = u128::from(normalized_divisor[divisor.len() - 2]);
        let base = 1u128 << 64;

        for offset in (0..quotient_limbs).rev() {
            let high_index = offset + divisor.len();
            let trial_numerator = (u128::from(normalized_dividend[high_index]) << 64)
                | u128::from(normalized_dividend[high_index - 1]);
            let mut trial_quotient = trial_numerator / divisor_high;
            let mut trial_remainder = trial_numerator % divisor_high;
            while trial_quotient == base
                || trial_quotient * divisor_next
                    > base * trial_remainder + u128::from(normalized_dividend[high_index - 2])
            {
                trial_quotient -= 1;
                trial_remainder += divisor_high;
                if trial_remainder >= base {
                    break;
                }
            }
            debug_assert!(trial_quotient < base);

            let mut quotient_limb = trial_quotient as u64;
            if Self::subtract_divisor_product(
                &mut normalized_dividend,
                offset,
                &normalized_divisor,
                quotient_limb,
            ) {
                debug_assert_ne!(quotient_limb, 0);
                quotient_limb = quotient_limb.wrapping_sub(1);
                Self::add_divisor_back(&mut normalized_dividend, offset, &normalized_divisor);
            }
            quotient[offset] = quotient_limb;
        }

        if shift != 0 {
            let mut carry = 0u64;
            for index in (0..divisor.len()).rev() {
                let limb = normalized_dividend[index];
                normalized_dividend[index] = (limb >> shift) | carry;
                carry = limb << (64 - shift);
            }
            debug_assert_eq!(carry, 0);
        }
        let remainder_limbs = normalized_dividend[..divisor.len()]
            .iter()
            .rposition(|&limb| limb != 0)
            .map_or(0, |index| index + 1);
        limit.ensure(ArithmeticOperation::Divide, remainder_limbs)?;
        let mut remainder =
            Self::allocate_limbs(ArithmeticOperation::Divide, remainder_limbs, limit)?;
        remainder.extend_from_slice(&normalized_dividend[..remainder_limbs]);

        debug_assert_ne!(quotient.last(), Some(&0));
        debug_assert!(
            Self::cmp_magnitude(&remainder, divisor) == Ordering::Less,
            "division remainder must be smaller than the divisor"
        );
        Ok((quotient, remainder))
    }

    /// Negates this value within an explicit limb budget.
    ///
    /// ```compile_fail,E0061
    /// use fgdb_bigint::BigInt;
    ///
    /// let value = BigInt::from_u64(1);
    /// let _ = value.checked_neg();
    /// ```
    ///
    /// The unary negation operator is absent because it has no place to carry
    /// a [`LimbLimit`].
    ///
    /// ```compile_fail,E0600
    /// use fgdb_bigint::BigInt;
    ///
    /// let value = BigInt::from_u64(1);
    /// let _ = -value;
    /// ```
    pub fn checked_neg(&self, limit: LimbLimit) -> Result<Self, ArithmeticError> {
        let mut limbs = Self::allocate_limbs(ArithmeticOperation::Negate, self.limbs.len(), limit)?;
        limbs.extend_from_slice(&self.limbs);
        Ok(Self {
            negative: !self.negative && !limbs.is_empty(),
            limbs,
        })
    }

    /// Adds two values within an explicit limb budget.
    ///
    /// ```compile_fail,E0061
    /// use fgdb_bigint::BigInt;
    ///
    /// let left = BigInt::from_u64(1);
    /// let right = BigInt::from_u64(2);
    /// let _ = left.checked_add(&right);
    /// ```
    ///
    /// The `+` operator is absent because it has no place to carry a
    /// [`LimbLimit`].
    ///
    /// ```compile_fail,E0369
    /// use fgdb_bigint::BigInt;
    ///
    /// let _ = BigInt::from_u64(1) + BigInt::from_u64(2);
    /// ```
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

    /// Subtracts two values within an explicit limb budget.
    ///
    /// ```compile_fail,E0061
    /// use fgdb_bigint::BigInt;
    ///
    /// let left = BigInt::from_u64(1);
    /// let right = BigInt::from_u64(2);
    /// let _ = left.checked_sub(&right);
    /// ```
    ///
    /// The `-` operator is absent because it has no place to carry a
    /// [`LimbLimit`].
    ///
    /// ```compile_fail,E0369
    /// use fgdb_bigint::BigInt;
    ///
    /// let _ = BigInt::from_u64(1) - BigInt::from_u64(2);
    /// ```
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

    /// Multiplies two values within an explicit limb budget.
    ///
    /// ```compile_fail,E0061
    /// use fgdb_bigint::BigInt;
    ///
    /// let left = BigInt::from_u64(1);
    /// let right = BigInt::from_u64(2);
    /// let _ = left.checked_mul(&right);
    /// ```
    ///
    /// The `*` operator is absent because it has no place to carry a
    /// [`LimbLimit`].
    ///
    /// ```compile_fail,E0369
    /// use fgdb_bigint::BigInt;
    ///
    /// let _ = BigInt::from_u64(1) * BigInt::from_u64(2);
    /// ```
    pub fn checked_mul(&self, other: &Self, limit: LimbLimit) -> Result<Self, ArithmeticError> {
        // `LimbLimit` bounds variable-size allocation. CPU work is
        // intentionally a separate concern for the downstream
        // resource-ledger/Cx wrapper.
        Ok(Self::from_sign_magnitude(
            self.negative != other.negative,
            Self::mul_magnitude(&self.limbs, &other.limbs, limit)?,
        ))
    }

    /// Divides `self` by `divisor`, returning `(quotient, remainder)`.
    ///
    /// Signed division truncates toward zero, matching Rust integer `/` and
    /// `%`: a nonzero quotient is negative exactly when the operands have
    /// different signs, while a nonzero remainder has the dividend's sign.
    /// The result is exact:
    ///
    /// `self == quotient * divisor + remainder`
    ///
    /// and `abs(remainder) < abs(divisor)`. Every result and normalization
    /// workspace allocation must fit `limit`; a zero divisor is rejected
    /// before any resource admission. Multi-limb division uses normalized
    /// base-2^64 long division with quotient-digit correction, never
    /// value-proportional repeated subtraction.
    ///
    /// ```compile_fail,E0061
    /// use fgdb_bigint::BigInt;
    ///
    /// let dividend = BigInt::from_u64(2);
    /// let divisor = BigInt::from_u64(1);
    /// let _ = dividend.checked_div_rem(&divisor);
    /// ```
    ///
    /// Neither `/` nor `%` is implemented because those operators have no
    /// place to carry a [`LimbLimit`].
    ///
    /// ```compile_fail,E0369
    /// use fgdb_bigint::BigInt;
    ///
    /// let _ = BigInt::from_u64(2) / BigInt::from_u64(1);
    /// ```
    ///
    /// ```compile_fail,E0369
    /// use fgdb_bigint::BigInt;
    ///
    /// let _ = BigInt::from_u64(2) % BigInt::from_u64(1);
    /// ```
    pub fn checked_div_rem(
        &self,
        divisor: &Self,
        limit: LimbLimit,
    ) -> Result<(Self, Self), ArithmeticError> {
        if divisor.is_zero() {
            return Err(ArithmeticError::DivisionByZero);
        }
        if self.is_zero() {
            return Ok((Self::zero(), Self::zero()));
        }

        let magnitude_order = Self::cmp_magnitude(&self.limbs, &divisor.limbs);
        if magnitude_order == Ordering::Less {
            limit.ensure(ArithmeticOperation::Divide, self.limbs.len())?;
            let mut remainder =
                Self::allocate_limbs(ArithmeticOperation::Divide, self.limbs.len(), limit)?;
            remainder.extend_from_slice(&self.limbs);
            return Ok((
                Self::zero(),
                Self::from_sign_magnitude(self.negative, remainder),
            ));
        }
        if magnitude_order == Ordering::Equal {
            limit.ensure(ArithmeticOperation::Divide, 1)?;
            let mut quotient = Self::allocate_limbs(ArithmeticOperation::Divide, 1, limit)?;
            quotient.push(1);
            return Ok((
                Self::from_sign_magnitude(self.negative != divisor.negative, quotient),
                Self::zero(),
            ));
        }

        let quotient_limbs = Self::quotient_limb_count(&self.limbs, &divisor.limbs)?;
        let (quotient, remainder) = if divisor.limbs.len() == 1 {
            Self::div_rem_magnitude_by_limb(&self.limbs, divisor.limbs[0], quotient_limbs, limit)?
        } else {
            Self::div_rem_magnitude_knuth(&self.limbs, &divisor.limbs, quotient_limbs, limit)?
        };
        Ok((
            Self::from_sign_magnitude(self.negative != divisor.negative, quotient),
            Self::from_sign_magnitude(self.negative, remainder),
        ))
    }

    /// Canonical byte encoding: exactly one byte string per value.
    ///
    /// Zero is the single octet `0x00`. A nonzero value is its sign octet
    /// (`0x01` positive, `0x02` negative), a little-endian `u64` limb count,
    /// then that many little-endian `u64` magnitude limbs whose last limb is
    /// nonzero. Uniqueness holds because the in-memory representation is
    /// already canonical and the layout adds no degrees of freedom.
    pub fn encode_canonical_bytes(&self) -> Vec<u8> {
        if self.is_zero() {
            return vec![encoding_tag::ZERO];
        }
        let mut out = Vec::with_capacity(1 + 8 + self.limbs.len() * 8);
        out.push(if self.negative {
            encoding_tag::NEGATIVE
        } else {
            encoding_tag::POSITIVE
        });
        out.extend_from_slice(&(self.limbs.len() as u64).to_le_bytes());
        for limb in &self.limbs {
            out.extend_from_slice(&limb.to_le_bytes());
        }
        out
    }

    /// Decode a canonical byte encoding, failing closed on every
    /// malformation.
    ///
    /// The declared limb count is bounded by `limit` and by the actual byte
    /// length BEFORE any allocation, so a hostile header cannot make this
    /// reserve memory it was never sent; the length comparison runs in `u128`
    /// so no `limit` choice can make it wrap. Canonicality itself is then
    /// enforced by [`BigInt::from_canonical_limbs`] — one validator, not two.
    pub fn decode_canonical_bytes(bytes: &[u8], limit: LimbLimit) -> Result<Self, DecodeError> {
        let (&tag, rest) = bytes.split_first().ok_or(DecodeError::Empty)?;
        let sign = match tag {
            encoding_tag::ZERO => {
                if !rest.is_empty() {
                    return Err(DecodeError::TrailingBytes {
                        expected_len: 1,
                        actual_len: bytes.len(),
                    });
                }
                return Ok(Self::zero());
            }
            encoding_tag::POSITIVE => Sign::Positive,
            encoding_tag::NEGATIVE => Sign::Negative,
            tag => return Err(DecodeError::UnknownSignTag { tag }),
        };
        if rest.len() < 8 {
            return Err(DecodeError::TruncatedLimbCount {
                available_bytes: rest.len(),
            });
        }
        let (count_bytes, magnitude) = rest.split_at(8);
        let mut count_le = [0u8; 8];
        count_le.copy_from_slice(count_bytes);
        let declared = u64::from_le_bytes(count_le);
        if declared > limit.max_limbs() as u64 {
            return Err(DecodeError::DeclaredLimbsExceedLimit {
                declared_limbs: declared,
                limit: limit.max_limbs(),
            });
        }
        if (magnitude.len() as u128) != (declared as u128) * 8 {
            return Err(DecodeError::MagnitudeLengthMismatch {
                declared_limbs: declared,
                actual_bytes: magnitude.len(),
            });
        }
        let requested_limbs = declared as usize;
        let mut limbs = Vec::new();
        limbs
            .try_reserve_exact(requested_limbs)
            .map_err(|_| DecodeError::AllocationFailed { requested_limbs })?;
        let (chunks, remainder) = magnitude.as_chunks::<8>();
        debug_assert!(remainder.is_empty(), "length was validated above");
        for &limb_le in chunks {
            limbs.push(u64::from_le_bytes(limb_le));
        }
        Self::from_canonical_limbs(sign, limbs.into_boxed_slice(), limit)
            .map_err(DecodeError::NotCanonical)
    }
}

/// Sign octets of the canonical byte encoding. Durable values: assigned once,
/// never renumbered.
mod encoding_tag {
    pub const ZERO: u8 = 0x00;
    pub const POSITIVE: u8 = 0x01;
    pub const NEGATIVE: u8 = 0x02;
}

/// Why a canonical byte decoding was rejected.
///
/// Every variant names the exact malformation so a fuzz counterexample is
/// reproducible from the error alone; `NotCanonical` carries the construction
/// error because decode routes through [`BigInt::from_canonical_limbs`] rather
/// than re-stating its laws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Empty,
    UnknownSignTag {
        tag: u8,
    },
    TruncatedLimbCount {
        available_bytes: usize,
    },
    DeclaredLimbsExceedLimit {
        declared_limbs: u64,
        limit: usize,
    },
    MagnitudeLengthMismatch {
        declared_limbs: u64,
        actual_bytes: usize,
    },
    TrailingBytes {
        expected_len: usize,
        actual_len: usize,
    },
    AllocationFailed {
        requested_limbs: usize,
    },
    NotCanonical(ConstructionError),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Empty => write!(f, "empty input has no sign octet"),
            Self::UnknownSignTag { tag } => write!(f, "unknown sign octet 0x{tag:02x}"),
            Self::TruncatedLimbCount { available_bytes } => {
                write!(
                    f,
                    "limb count needs 8 bytes, only {available_bytes} present"
                )
            }
            Self::DeclaredLimbsExceedLimit {
                declared_limbs,
                limit,
            } => write!(f, "declared {declared_limbs} limbs, limit is {limit}"),
            Self::MagnitudeLengthMismatch {
                declared_limbs,
                actual_bytes,
            } => write!(
                f,
                "declared {declared_limbs} limbs but {actual_bytes} magnitude bytes follow"
            ),
            Self::TrailingBytes {
                expected_len,
                actual_len,
            } => write!(
                f,
                "encoding is {expected_len} bytes, input carries {actual_len}"
            ),
            Self::AllocationFailed { requested_limbs } => {
                write!(f, "allocation of {requested_limbs} limbs failed")
            }
            Self::NotCanonical(ref error) => write!(f, "decoded limbs are not canonical: {error}"),
        }
    }
}

impl std::error::Error for DecodeError {}

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

    fn assert_division_identity(
        dividend: &BigInt,
        divisor: &BigInt,
        quotient: &BigInt,
        remainder: &BigInt,
    ) {
        let product = quotient
            .checked_mul(divisor, TEST_LIMIT)
            .expect("division identity product fits the test budget");
        let reconstructed = product
            .checked_add(remainder, TEST_LIMIT)
            .expect("division identity sum fits the test budget");
        assert_eq!(&reconstructed, dividend);
        assert!(
            BigInt::cmp_magnitude(&remainder.limbs, &divisor.limbs) == Ordering::Less,
            "remainder {remainder:?} must be smaller in magnitude than divisor {divisor:?}"
        );
        assert!(quotient.is_canonical() && remainder.is_canonical());
        if !quotient.is_zero() {
            assert_eq!(
                quotient.is_negative(),
                dividend.is_negative() != divisor.is_negative()
            );
        }
        if !remainder.is_zero() {
            assert_eq!(remainder.is_negative(), dividend.is_negative());
        }
    }

    #[test]
    fn exhaustive_small_signed_domain_matches_i128_division() {
        for dividend in i8::MIN..=i8::MAX {
            for divisor in i8::MIN..=i8::MAX {
                if divisor == 0 {
                    continue;
                }
                let dividend_i128 = i128::from(dividend);
                let divisor_i128 = i128::from(divisor);
                let dividend_big = BigInt::from_i128(dividend_i128);
                let divisor_big = BigInt::from_i128(divisor_i128);
                let (quotient, remainder) = dividend_big
                    .checked_div_rem(&divisor_big, LimbLimit::new(1))
                    .expect("i8 quotient and remainder occupy at most one limb");
                assert_eq!(
                    quotient.to_i128(),
                    Some(dividend_i128 / divisor_i128),
                    "quotient mismatch for {dividend_i128} / {divisor_i128}"
                );
                assert_eq!(
                    remainder.to_i128(),
                    Some(dividend_i128 % divisor_i128),
                    "remainder mismatch for {dividend_i128} % {divisor_i128}"
                );
                assert_division_identity(&dividend_big, &divisor_big, &quotient, &remainder);
            }
        }
    }

    #[test]
    fn i128_boundary_matrix_matches_checked_native_division() {
        let values = [
            i128::MIN,
            i128::MIN + 1,
            -(1i128 << 126),
            -(1i128 << 65),
            -((1i128 << 64) + 1),
            -(1i128 << 64),
            -(u64::MAX as i128),
            -3,
            -2,
            -1,
            0,
            1,
            2,
            3,
            u64::MAX as i128,
            1i128 << 64,
            (1i128 << 64) + 1,
            1i128 << 65,
            1i128 << 126,
            i128::MAX - 1,
            i128::MAX,
        ];
        for &dividend in &values {
            for &divisor in &values {
                let dividend_big = BigInt::from_i128(dividend);
                let divisor_big = BigInt::from_i128(divisor);
                if divisor == 0 {
                    assert_eq!(
                        dividend_big.checked_div_rem(&divisor_big, LimbLimit::new(2)),
                        Err(ArithmeticError::DivisionByZero)
                    );
                    continue;
                }
                let (quotient, remainder) = dividend_big
                    .checked_div_rem(&divisor_big, LimbLimit::new(3))
                    .expect("i128 division needs at most three result limbs");
                if let Some(expected) = dividend.checked_div(divisor) {
                    assert_eq!(
                        quotient.to_i128(),
                        Some(expected),
                        "quotient mismatch for {dividend} / {divisor}"
                    );
                } else {
                    assert_eq!((dividend, divisor), (i128::MIN, -1));
                    assert_eq!(quotient.sign(), Sign::Positive);
                    assert_eq!(quotient.magnitude_limbs_le(), &[0, 1u64 << 63]);
                }
                if let Some(expected) = dividend.checked_rem(divisor) {
                    assert_eq!(
                        remainder.to_i128(),
                        Some(expected),
                        "remainder mismatch for {dividend} % {divisor}"
                    );
                } else {
                    assert_eq!((dividend, divisor), (i128::MIN, -1));
                    assert_eq!(remainder, BigInt::zero());
                }
                assert_division_identity(&dividend_big, &divisor_big, &quotient, &remainder);
            }
        }
    }

    #[test]
    fn u128_boundary_matrix_matches_native_division() {
        let values = [
            0,
            1,
            2,
            u128::from(u64::MAX - 1),
            u128::from(u64::MAX),
            1u128 << 64,
            (1u128 << 64) + 1,
            (1u128 << 127) - 1,
            1u128 << 127,
            (1u128 << 127) + 1,
            u128::MAX - 1,
            u128::MAX,
        ];
        for &dividend in &values {
            for &divisor in &values {
                let dividend_big = BigInt::from_u128(dividend);
                let divisor_big = BigInt::from_u128(divisor);
                if divisor == 0 {
                    assert_eq!(
                        dividend_big.checked_div_rem(&divisor_big, LimbLimit::new(2)),
                        Err(ArithmeticError::DivisionByZero)
                    );
                    continue;
                }
                let (quotient, remainder) = dividend_big
                    .checked_div_rem(&divisor_big, LimbLimit::new(3))
                    .expect("u128 division needs at most three workspace/result limbs");
                assert_eq!(
                    quotient,
                    BigInt::from_u128(dividend / divisor),
                    "quotient mismatch for {dividend} / {divisor}"
                );
                assert_eq!(
                    remainder,
                    BigInt::from_u128(dividend % divisor),
                    "remainder mismatch for {dividend} % {divisor}"
                );
                assert_division_identity(&dividend_big, &divisor_big, &quotient, &remainder);
            }
        }
    }

    #[test]
    fn knuth_trial_correction_and_add_back_vectors_are_exact() {
        let cases = [
            // The first estimate is base + 1. The two-digit guard must reduce
            // it twice rather than letting the narrowing cast wrap the digit.
            (
                vec![0, 1u64 << 63, 1u64 << 63],
                vec![(1u64 << 63) + 1, 1u64 << 63],
                vec![u64::MAX],
                vec![(1u64 << 63) + 1, (1u64 << 63) - 1],
            ),
            // An interior trial digit requires both permitted correction
            // steps before subtraction.
            (
                vec![
                    17_949_602_623_054_607_959,
                    11_154_557_356_834_803_885,
                    10_514_998_908_788_136_365,
                    13_046_051_152_631_070_964,
                    6_310_274_242_980_859_722,
                    2_726_540_312_051_596_134,
                    12_447_584_222_245_729_637,
                ],
                vec![
                    10_976_685_742_615_999_155,
                    18_410_391_486_344_873_006,
                    11_574_686_656_902_173_857,
                ],
                vec![
                    15_537_162_608_830_708_470,
                    15_417_723_965_371_022_023,
                    11_443_288_293_163_043_518,
                    1_391_149_364_795_528_082,
                    1,
                ],
                vec![
                    8_388_440_447_448_084_565,
                    6_457_466_909_459_772_174,
                    676_628_760_763_417_625,
                ],
            ),
            // The two-high-limb estimate passes the guard by one but the full
            // product is too large, exercising subtract-underflow + add-back.
            (
                vec![0, 0, 0, 1],
                vec![1, 0, 1u64 << 63],
                vec![1],
                vec![u64::MAX, u64::MAX, (1u64 << 63) - 1],
            ),
        ];

        for (dividend, divisor, expected_quotient, expected_remainder) in cases {
            let dividend = BigInt::from_sign_magnitude(false, dividend);
            let divisor = BigInt::from_sign_magnitude(false, divisor);
            let (quotient, remainder) = dividend
                .checked_div_rem(&divisor, TEST_LIMIT)
                .expect("correction vector fits the test budget");
            assert_eq!(
                quotient,
                BigInt::from_sign_magnitude(false, expected_quotient)
            );
            assert_eq!(
                remainder,
                BigInt::from_sign_magnitude(false, expected_remainder)
            );
            assert_division_identity(&dividend, &divisor, &quotient, &remainder);
        }
    }

    #[test]
    fn multi_limb_division_recovers_adversarial_quotients_and_remainders() {
        let cases = [
            (
                BigInt::from_sign_magnitude(false, vec![u64::MAX, u64::MAX, 1]),
                BigInt::from_sign_magnitude(false, vec![u64::MAX, 1]),
                BigInt::from_sign_magnitude(false, vec![u64::MAX - 1, 1]),
            ),
            (
                BigInt::from_sign_magnitude(false, vec![0, u64::MAX, u64::MAX]),
                BigInt::from_sign_magnitude(false, vec![u64::MAX, u64::MAX]),
                BigInt::from_u64(1),
            ),
            (
                BigInt::from_sign_magnitude(false, vec![u64::MAX, 0, 1]),
                BigInt::from_sign_magnitude(false, vec![0, 1]),
                BigInt::from_sign_magnitude(false, vec![u64::MAX, 0]),
            ),
            (
                BigInt::from_sign_magnitude(false, vec![1, 0, 0, 1]),
                BigInt::from_sign_magnitude(false, vec![u64::MAX - 1, 2]),
                BigInt::from_sign_magnitude(false, vec![u64::MAX - 2, 2]),
            ),
        ];

        for (quotient, divisor, remainder) in cases {
            assert!(BigInt::cmp_magnitude(&remainder.limbs, &divisor.limbs) == Ordering::Less);
            let product = quotient
                .checked_mul(&divisor, TEST_LIMIT)
                .expect("constructed dividend product");
            let dividend = product
                .checked_add(&remainder, TEST_LIMIT)
                .expect("constructed dividend remainder");
            for (dividend_negative, divisor_negative) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let signed_dividend =
                    BigInt::from_sign_magnitude(dividend_negative, dividend.limbs.clone());
                let signed_divisor =
                    BigInt::from_sign_magnitude(divisor_negative, divisor.limbs.clone());
                let (actual_quotient, actual_remainder) = signed_dividend
                    .checked_div_rem(&signed_divisor, TEST_LIMIT)
                    .expect("constructed division fits the test budget");
                assert_eq!(
                    actual_quotient,
                    BigInt::from_sign_magnitude(
                        dividend_negative != divisor_negative,
                        quotient.limbs.clone(),
                    )
                );
                assert_eq!(
                    actual_remainder,
                    BigInt::from_sign_magnitude(dividend_negative, remainder.limbs.clone())
                );
                assert_division_identity(
                    &signed_dividend,
                    &signed_divisor,
                    &actual_quotient,
                    &actual_remainder,
                );
            }
        }
    }

    #[test]
    fn randomized_multi_limb_division_preserves_identity_and_bounds() {
        for seed in [0u64, 0xD1A1_DED0, u64::MAX] {
            let mut rng = SplitMix64(seed);
            for _ in 0..300 {
                let dividend = rng.bigint(6);
                let mut divisor = rng.bigint(4);
                if divisor.is_zero() {
                    divisor = BigInt::from_u64(1);
                }
                let (quotient, remainder) = dividend
                    .checked_div_rem(&divisor, TEST_LIMIT)
                    .expect("six-limb division fits the test budget");
                assert_division_identity(&dividend, &divisor, &quotient, &remainder);
            }
        }
    }

    #[test]
    fn division_enforces_exact_limits_and_error_precedence() {
        let zero = BigInt::zero();
        let one = BigInt::from_u64(1);
        let three_limb = BigInt::from_sign_magnitude(false, vec![0, 0, 1]);
        assert_eq!(
            three_limb.checked_div_rem(&zero, LimbLimit::new(0)),
            Err(ArithmeticError::DivisionByZero),
            "division by zero precedes every resource check"
        );
        assert_eq!(
            zero.checked_div_rem(&one, LimbLimit::new(0)),
            Ok((BigInt::zero(), BigInt::zero()))
        );

        assert_eq!(
            three_limb.checked_div_rem(&one, LimbLimit::new(2)),
            Err(ArithmeticError::LimbLimitExceeded {
                operation: ArithmeticOperation::Divide,
                required_limbs: 3,
                limit: 2,
            }),
            "quotient admission reports its exact normalized size"
        );

        let two_limb_remainder = BigInt::from_sign_magnitude(false, vec![u64::MAX, 1]);
        let divisor = BigInt::from_sign_magnitude(false, vec![0, 2]);
        let dividend = divisor
            .checked_add(&two_limb_remainder, TEST_LIMIT)
            .expect("one times divisor plus a two-limb remainder");
        assert_eq!(
            dividend.checked_div_rem(&divisor, LimbLimit::new(1)),
            Err(ArithmeticError::LimbLimitExceeded {
                operation: ArithmeticOperation::Divide,
                required_limbs: 2,
                limit: 1,
            }),
            "remainder admission reports its exact normalized size after quotient fits"
        );
        assert_eq!(
            one.checked_div_rem(&divisor, LimbLimit::new(0)),
            Err(ArithmeticError::LimbLimitExceeded {
                operation: ArithmeticOperation::Divide,
                required_limbs: 1,
                limit: 0,
            }),
            "a smaller dividend is the exact remainder"
        );
        assert_eq!(
            divisor
                .checked_div_rem(&divisor, LimbLimit::new(1))
                .expect("equal magnitudes need one quotient limb"),
            (BigInt::from_u64(1), BigInt::zero())
        );
    }

    #[test]
    fn division_workspace_capacity_and_allocation_failures_are_typed() {
        assert_eq!(
            BigInt::allocate_workspace_limbs(ArithmeticOperation::Divide, 2, LimbLimit::new(1),),
            Err(ArithmeticError::LimbLimitExceeded {
                operation: ArithmeticOperation::Divide,
                required_limbs: 2,
                limit: 1,
            })
        );

        let first_impossible_limb_count = (isize::MAX as usize) / std::mem::size_of::<u64>() + 1;
        assert_eq!(
            BigInt::allocate_workspace_limbs(
                ArithmeticOperation::Divide,
                first_impossible_limb_count,
                LimbLimit::new(usize::MAX),
            ),
            Err(ArithmeticError::CapacityOverflow {
                operation: ArithmeticOperation::Divide,
            })
        );

        let largest_addressable_limb_count = (isize::MAX as usize) / std::mem::size_of::<u64>();
        assert_eq!(
            BigInt::allocate_workspace_limbs(
                ArithmeticOperation::Divide,
                largest_addressable_limb_count,
                LimbLimit::new(usize::MAX),
            ),
            Err(ArithmeticError::AllocationFailed {
                operation: ArithmeticOperation::Divide,
                requested_limbs: largest_addressable_limb_count,
            })
        );
    }

    #[test]
    fn public_division_limit_covers_normalization_workspace_not_only_results() {
        let dividend = BigInt::from_sign_magnitude(false, vec![0, 0, 1]);
        let divisor = BigInt::from_sign_magnitude(false, vec![0, 1]);

        assert_eq!(
            dividend.checked_div_rem(&divisor, LimbLimit::new(2)),
            Err(ArithmeticError::LimbLimitExceeded {
                operation: ArithmeticOperation::Divide,
                required_limbs: 4,
                limit: 2,
            }),
            "the dividend normalization workspace is admitted before allocation"
        );

        let (quotient, remainder) = dividend
            .checked_div_rem(&divisor, LimbLimit::new(4))
            .expect("a four-limb workspace budget admits the exact result");
        assert_eq!(quotient, BigInt::from_sign_magnitude(false, vec![0, 1]));
        assert_eq!(remainder, BigInt::zero());
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

    /// Round-trip: the canonical limb export is exactly the import form.
    /// This is the crate's durable-shape identity -- `magnitude_limbs_le` plus
    /// `sign()` is the only canonical external rendering of a value, so if the
    /// pair did not reconstruct the original, any consumer that persisted the
    /// limbs would read back a different number.
    #[test]
    fn canonical_limb_export_round_trips_through_import() {
        for seed in [3u64, 0x5EED, 0xFEED_FACE, u64::MAX - 1] {
            let mut rng = SplitMix64(seed);
            for _ in 0..300 {
                let v = rng.bigint(5);
                let limbs: Box<[u64]> = v.magnitude_limbs_le().to_vec().into_boxed_slice();
                let back = BigInt::from_canonical_limbs(v.sign(), limbs, TEST_LIMIT)
                    .expect("canonical export re-imports");
                assert_eq!(back, v, "seed={seed} round trip changed the value: {v:?}");
                assert_eq!(back.sign(), v.sign(), "seed={seed} sign drifted: {v:?}");
                assert_eq!(
                    back.magnitude_limbs_le(),
                    v.magnitude_limbs_le(),
                    "seed={seed} magnitude drifted: {v:?}"
                );
                assert!(back.is_canonical(), "seed={seed} re-import not canonical");
            }
        }
    }

    /// Metamorphic: strict order is preserved by adding the same value to both
    /// sides. Stated for ANY addend, not only positive ones, because addition
    /// here is over signed integers where translation invariance is total --
    /// the documented boundary is the limb budget, not the sign of the addend.
    #[test]
    fn comparison_is_translation_invariant_under_addition() {
        for seed in [7u64, 0xB0A710AD] {
            let mut rng = SplitMix64(seed);
            for _ in 0..300 {
                let a = rng.bigint(4);
                let b = rng.bigint(4);
                let c = rng.bigint(4);
                let ac = a.checked_add(&c, TEST_LIMIT).expect("a + c");
                let bc = b.checked_add(&c, TEST_LIMIT).expect("b + c");
                assert_eq!(
                    a.cmp(&b),
                    ac.cmp(&bc),
                    "seed={seed} order changed under +c: a={a:?} b={b:?} c={c:?}"
                );
            }
        }
    }

    /// Metamorphic inverse pair: subtraction undoes addition exactly.
    #[test]
    fn subtraction_inverts_addition_on_random_values() {
        for seed in [13u64, 0xA11CE] {
            let mut rng = SplitMix64(seed);
            for _ in 0..300 {
                let a = rng.bigint(4);
                let b = rng.bigint(4);
                let sum = a.checked_add(&b, TEST_LIMIT).expect("a + b");
                let recovered = sum.checked_sub(&b, TEST_LIMIT).expect("(a + b) - b");
                assert_eq!(recovered, a, "seed={seed} a={a:?} b={b:?}");
                let recovered_other = sum.checked_sub(&a, TEST_LIMIT).expect("(a + b) - a");
                assert_eq!(recovered_other, b, "seed={seed} a={a:?} b={b:?}");
            }
        }
    }

    /// Metamorphic inverse pair: dividing a constructed product by one factor
    /// returns the other exactly, with a zero remainder. The existing division
    /// test pins q*d+r=n for arbitrary pairs; this pins the exact-inverse
    /// direction, which is the case a truncating bug would still satisfy.
    #[test]
    fn division_inverts_multiplication_with_zero_remainder() {
        for seed in [17u64, 0xC0FFEE] {
            let mut rng = SplitMix64(seed);
            for _ in 0..300 {
                let a = rng.bigint(3);
                let b = rng.bigint(2);
                if b.is_zero() {
                    continue;
                }
                let product = a.checked_mul(&b, TEST_LIMIT).expect("a * b");
                let (quotient, remainder) = product
                    .checked_div_rem(&b, TEST_LIMIT)
                    .expect("(a * b) / b");
                assert!(
                    remainder.is_zero(),
                    "seed={seed} exact product left a remainder: a={a:?} b={b:?} r={remainder:?}"
                );
                assert_eq!(quotient, a, "seed={seed} a={a:?} b={b:?}");
            }
        }
    }

    /// Zero, one and the widest representable edges are ordinary inputs, not
    /// special cases: the identity and absorbing laws hold on them with the
    /// same operators and the same budget as any random value.
    #[test]
    fn identity_and_absorbing_elements_are_ordinary_inputs() {
        let one = BigInt::from_u64(1);
        let edges = [
            BigInt::zero(),
            BigInt::from_u64(1),
            BigInt::from_i64(-1),
            BigInt::from_u64(u64::MAX),
            BigInt::from_u128(u128::MAX),
            BigInt::from_i128(i128::MIN),
            BigInt::from_i128(i128::MAX),
        ];
        for v in &edges {
            assert_eq!(
                &v.checked_add(&BigInt::zero(), TEST_LIMIT).expect("v + 0"),
                v,
                "additive identity on {v:?}"
            );
            assert_eq!(
                &v.checked_mul(&one, TEST_LIMIT).expect("v * 1"),
                v,
                "multiplicative identity on {v:?}"
            );
            assert!(
                v.checked_mul(&BigInt::zero(), TEST_LIMIT)
                    .expect("v * 0")
                    .is_zero(),
                "absorbing zero on {v:?}"
            );
            assert!(
                v.checked_sub(v, TEST_LIMIT).expect("v - v").is_zero(),
                "self difference on {v:?}"
            );
            let (q, r) = v.checked_div_rem(&one, TEST_LIMIT).expect("v / 1");
            assert_eq!(&q, v, "division by one on {v:?}");
            assert!(r.is_zero(), "division by one left a remainder on {v:?}");
            assert_eq!(
                &v.checked_neg(TEST_LIMIT)
                    .expect("-v")
                    .checked_neg(TEST_LIMIT)
                    .expect("--v"),
                v,
                "double negation on {v:?}"
            );
        }
    }

    /// Limb-boundary behaviour at and either side of the exposed limit: a value
    /// whose magnitude needs exactly n limbs is admitted by LimbLimit(n) and
    /// refused by LimbLimit(n-1), with the required width reported exactly.
    #[test]
    fn limb_limit_admits_the_required_width_and_refuses_one_less() {
        for width in 1usize..=5 {
            let mut limbs = vec![0u64; width];
            limbs[width - 1] = 1;
            let exact = BigInt::from_canonical_limbs(
                Sign::Positive,
                limbs.clone().into_boxed_slice(),
                LimbLimit::new(width),
            )
            .expect("width fits its own limit");
            assert_eq!(exact.limb_count(), width);

            // Asserted as a VALUE rather than matched-with-panic. ConstructionError
            // derives PartialEq, so equality pins the reported required_limbs and
            // limit exactly and states the whole expectation in one place -- and it
            // avoids introducing an unwinding macro into a file that has none.
            assert_eq!(
                BigInt::from_canonical_limbs(
                    Sign::Positive,
                    limbs.into_boxed_slice(),
                    LimbLimit::new(width - 1),
                ),
                Err(ConstructionError::LimbLimitExceeded {
                    required_limbs: width,
                    limit: width - 1,
                }),
                "width {width} must be refused at limit {} with exact widths reported",
                width - 1
            );

            // The same boundary on an allocating operation rather than import.
            let doubled = exact.checked_add(&exact, LimbLimit::new(width + 1));
            assert!(doubled.is_ok(), "doubling width {width} fits width+1");
            assert!(
                exact.checked_clone(LimbLimit::new(width)).is_ok(),
                "clone of width {width} fits its own limit"
            );
            assert!(
                exact.checked_clone(LimbLimit::new(width - 1)).is_err(),
                "clone of width {width} must not fit width-1"
            );
        }
    }

    // =====================================================================
    // The canonical byte codec (tjk residue: byte-level encode/decode
    // coherence). Round-trip, injectivity, and one exact-variant rejection
    // per malformation — a rejection asserted only as is_err() could pass on
    // a DIFFERENT failure kind than the one the mutation constructs.
    // =====================================================================

    #[test]
    fn codec_round_trip_randomized_and_adversarial() -> Result<(), String> {
        let seed = 0x5eed_b16e_c0de_0001u64;
        let mut rng = SplitMix64(seed);
        let mut values: Vec<BigInt> = (0..500).map(|_| rng.bigint(8)).collect();
        values.extend([
            BigInt::zero(),
            BigInt::from_i64(-1),
            BigInt::from_u64(1),
            BigInt::from_u64(u64::MAX),
            BigInt::from_u128(u64::MAX as u128 + 1),
            BigInt::from_u128(u128::MAX),
            BigInt::from_i128(i128::MIN),
            BigInt::from_i128(i128::MAX),
        ]);
        for value in &values {
            let bytes = value.encode_canonical_bytes();
            let decoded = BigInt::decode_canonical_bytes(&bytes, TEST_LIMIT).map_err(|error| {
                format!("seed {seed:#x}: bytes {bytes:02x?}: round trip failed: {error}")
            })?;
            assert_eq!(&decoded, value, "seed {seed:#x}: bytes {bytes:02x?}");
            assert_eq!(
                bytes,
                value.encode_canonical_bytes(),
                "encoding must be deterministic"
            );
        }
        // Injectivity over the sample: distinct values, distinct encodings —
        // and order survives the round trip.
        values.sort();
        values.dedup();
        let mut encodings: Vec<Vec<u8>> =
            values.iter().map(BigInt::encode_canonical_bytes).collect();
        let before = encodings.len();
        encodings.sort();
        encodings.dedup();
        assert_eq!(before, encodings.len(), "two values shared an encoding");
        for pair in values.windows(2) {
            let a = BigInt::decode_canonical_bytes(&pair[0].encode_canonical_bytes(), TEST_LIMIT)
                .expect("sorted sample re-decodes");
            let b = BigInt::decode_canonical_bytes(&pair[1].encode_canonical_bytes(), TEST_LIMIT)
                .expect("sorted sample re-decodes");
            assert!(a < b, "order must survive the round trip");
        }
        Ok(())
    }

    #[test]
    fn zero_encodes_as_the_single_zero_octet() {
        assert_eq!(BigInt::zero().encode_canonical_bytes(), vec![0x00]);
    }

    #[test]
    fn decode_rejects_each_malformation_with_its_exact_variant() {
        let two_limbs = BigInt::from_u128(u64::MAX as u128 + 7);
        let good = two_limbs.encode_canonical_bytes();

        assert_eq!(
            BigInt::decode_canonical_bytes(&[], TEST_LIMIT),
            Err(DecodeError::Empty)
        );
        assert_eq!(
            BigInt::decode_canonical_bytes(&[0x03], TEST_LIMIT),
            Err(DecodeError::UnknownSignTag { tag: 0x03 })
        );
        assert_eq!(
            BigInt::decode_canonical_bytes(&[0x01, 0x01, 0x00], TEST_LIMIT),
            Err(DecodeError::TruncatedLimbCount { available_bytes: 2 })
        );
        // Append control on zero: the single octet plus anything is invalid.
        assert_eq!(
            BigInt::decode_canonical_bytes(&[0x00, 0x00], TEST_LIMIT),
            Err(DecodeError::TrailingBytes {
                expected_len: 1,
                actual_len: 2
            })
        );
        // Truncate control: drop the final magnitude byte.
        assert_eq!(
            BigInt::decode_canonical_bytes(&good[..good.len() - 1], TEST_LIMIT),
            Err(DecodeError::MagnitudeLengthMismatch {
                declared_limbs: 2,
                actual_bytes: 15
            })
        );
        // Append control: one byte past the declared magnitude.
        let mut appended = good.clone();
        appended.push(0x00);
        assert_eq!(
            BigInt::decode_canonical_bytes(&appended, TEST_LIMIT),
            Err(DecodeError::MagnitudeLengthMismatch {
                declared_limbs: 2,
                actual_bytes: 17
            })
        );
        // Length-before-allocation: a hostile count with a short buffer must
        // fail on the LIMIT check, proving the bound runs before any reserve.
        let mut hostile = vec![0x01];
        hostile.extend_from_slice(&u64::MAX.to_le_bytes());
        hostile.extend_from_slice(&[0u8; 8]);
        assert_eq!(
            BigInt::decode_canonical_bytes(&hostile, TEST_LIMIT),
            Err(DecodeError::DeclaredLimbsExceedLimit {
                declared_limbs: u64::MAX,
                limit: TEST_LIMIT.max_limbs()
            })
        );
        // The limit binds real decodes too: two limbs against a one-limb cap.
        assert_eq!(
            BigInt::decode_canonical_bytes(&good, LimbLimit::new(1)),
            Err(DecodeError::DeclaredLimbsExceedLimit {
                declared_limbs: 2,
                limit: 1
            })
        );
        // High-zero-limb forgery: widen the count and append a zero limb — the
        // canonicality validator, not the codec, must name it.
        let mut forged = good.clone();
        forged[1] = 3;
        forged.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            BigInt::decode_canonical_bytes(&forged, TEST_LIMIT),
            Err(DecodeError::NotCanonical(ConstructionError::HighZeroLimb {
                sign: Sign::Positive,
                limb_count: 3
            }))
        );
        // Negative-zero forgery: a sign octet with a zero limb count.
        let mut negative_zero = vec![0x02];
        negative_zero.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            BigInt::decode_canonical_bytes(&negative_zero, TEST_LIMIT),
            Err(DecodeError::NotCanonical(
                ConstructionError::NonzeroSignWithoutMagnitude {
                    sign: Sign::Negative
                }
            ))
        );
    }
}
