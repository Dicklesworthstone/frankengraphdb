//! Finite binary64 arithmetic with integer-only rounding.
//!
//! Arithmetic rounds once to nearest, ties to even, with gradual underflow.
//! Integral rounding has explicit floor/ceiling/nearest-ties-positive policies.
//! All results have canonical positive zero. No native floating arithmetic, libm, FMA,
//! allocation, or ambient rounding mode is used by the kernel. Non-finite
//! operands, overflow and division by zero are explicit refusals. This is not
//! the complete STRICT_PORTABLE numeric profile or a transcendental library.
//!
//! [`ExactBinary64Sum`] is the aggregate counterpart: it accumulates binary64
//! and integer inputs exactly and rounds once, so SUM and AVG are the
//! correctly rounded exact sum and mean whatever order the inputs arrive in.

use crate::CanonicalF64;

const SIGN: u64 = 1 << 63;
const FRACTION: u64 = (1 << 52) - 1;
const HIDDEN: u64 = 1 << 52;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatArithmeticError {
    NonFinite,
    Overflow,
    DivisionByZero,
}
impl core::fmt::Display for FloatArithmeticError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "finite binary64 arithmetic: {self:?}")
    }
}
impl core::error::Error for FloatArithmeticError {}

// Nonzero significands are normalized to 53 bits. The value is exactly
// (-1)^negative * significand * 2^exponent, even for subnormal inputs.
#[derive(Clone, Copy)]
struct Number {
    negative: bool,
    significand: u64,
    exponent: i32,
}
impl Number {
    fn read(value: CanonicalF64) -> Result<Self, FloatArithmeticError> {
        let bits = value.to_bits();
        let encoded = ((bits >> 52) & 0x7ff) as i32;
        if encoded == 0x7ff {
            return Err(FloatArithmeticError::NonFinite);
        }
        let mut significand = bits & FRACTION;
        let mut exponent = -1074;
        if encoded != 0 {
            significand |= HIDDEN;
            exponent = encoded - 1075;
        } else if significand != 0 {
            let shift = significand.leading_zeros() - 11;
            significand <<= shift;
            exponent -= shift as i32;
        }
        Ok(Self {
            negative: bits & SIGN != 0,
            significand,
            exponent,
        })
    }
}

fn value(bits: u64) -> CanonicalF64 {
    CanonicalF64::from_bits_canonical(bits).expect("kernel emits canonical finite bits")
}

// Shift with a sticky low bit. Guard/round/sticky bits survive exponent gaps
// larger than the machine word without an invalid shift or a lossy float cast.
fn jam(number: u128, shift: u32) -> u128 {
    if shift == 0 {
        number
    } else if shift >= 128 {
        u128::from(number != 0)
    } else {
        (number >> shift) | u128::from(number & ((1_u128 << shift) - 1) != 0)
    }
}

fn round_right(number: u128, shift: u32) -> u128 {
    if shift > 128 {
        return 0;
    }
    if shift == 128 {
        return u128::from(number > (1_u128 << 127));
    }
    if shift == 0 {
        return number;
    }
    let quotient = number >> shift;
    let remainder = number & ((1_u128 << shift) - 1);
    let half = 1_u128 << (shift - 1);
    quotient + u128::from(remainder > half || (remainder == half && quotient & 1 != 0))
}

// Round an exact magnitude (or a guard/round/sticky equivalent) only once.
// Choosing the subnormal quantum BEFORE rounding avoids double rounding at
// the normal/subnormal boundary and on results smaller than the minimum ulp.
fn pack(
    negative: bool,
    magnitude: u128,
    exponent: i32,
) -> Result<CanonicalF64, FloatArithmeticError> {
    if magnitude == 0 {
        return Ok(value(0));
    }
    let width = 128 - magnitude.leading_zeros() as i32;
    let shift = (width - 53).max(-1074 - exponent);
    let mut significand = if shift > 0 {
        round_right(magnitude, shift as u32)
    } else {
        magnitude << (-shift as u32)
    };
    let mut exponent = exponent + shift;
    if significand == 0 {
        return Ok(value(0));
    }
    if significand >= (1_u128 << 53) {
        significand >>= 1;
        exponent += 1;
    }
    let sign = if negative { SIGN } else { 0 };
    if significand < u128::from(HIDDEN) {
        debug_assert_eq!(exponent, -1074);
        return Ok(value(sign | significand as u64));
    }
    let biased = exponent + 1075;
    if biased >= 0x7ff {
        return Err(FloatArithmeticError::Overflow);
    }
    debug_assert!(biased > 0);
    Ok(value(
        sign | ((biased as u64) << 52) | (significand as u64 & FRACTION),
    ))
}

fn add(mut a: Number, mut b: Number) -> Result<CanonicalF64, FloatArithmeticError> {
    if a.significand == 0 {
        return pack(b.negative, u128::from(b.significand), b.exponent);
    }
    if b.significand == 0 {
        return pack(a.negative, u128::from(a.significand), a.exponent);
    }
    if (a.exponent, a.significand) < (b.exponent, b.significand) {
        core::mem::swap(&mut a, &mut b);
    }
    let left = u128::from(a.significand) << 3;
    let right = jam(
        u128::from(b.significand) << 3,
        (a.exponent - b.exponent) as u32,
    );
    let magnitude = if a.negative == b.negative {
        left + right
    } else {
        left - right
    };
    pack(a.negative, magnitude, a.exponent - 3)
}

#[derive(Clone, Copy)]
enum IntegralRounding {
    Floor,
    Ceiling,
    NearestTiesPositive,
}

// Inspect the exact binary fraction, not floor(x + 0.5): adding half can round
// a value just BELOW a tie up to that tie. Very small magnitudes are known to
// lie below half without shifting by more than a machine word. Values with a
// nonnegative significand exponent are already integral, including f64::MAX.
fn round_integral(
    input: CanonicalF64,
    mode: IntegralRounding,
) -> Result<CanonicalF64, FloatArithmeticError> {
    let number = Number::read(input)?;
    if number.significand == 0 || number.exponent >= 0 {
        return Ok(input);
    }
    let shift = number.exponent.unsigned_abs();
    let (whole, fractional, half_order) = if shift > 53 {
        (0, true, core::cmp::Ordering::Less)
    } else {
        let remainder = number.significand & ((1_u64 << shift) - 1);
        (
            number.significand >> shift,
            remainder != 0,
            remainder.cmp(&(1_u64 << (shift - 1))),
        )
    };
    let increment = fractional
        && match mode {
            IntegralRounding::Floor => number.negative,
            IntegralRounding::Ceiling => !number.negative,
            IntegralRounding::NearestTiesPositive => {
                half_order.is_gt() || (half_order.is_eq() && !number.negative)
            }
        };
    pack(
        number.negative,
        u128::from(whole) + u128::from(increment),
        0,
    )
}

impl CanonicalF64 {
    /// Convert a signed integer using round-to-nearest, ties-to-even. Integers
    /// outside binary64's exact precision are rounded, never truncated through
    /// a host cast. This explicit conversion does not change scalar equality.
    #[must_use]
    pub fn from_i64_rounded(number: i64) -> Self {
        pack(number < 0, u128::from(number.unsigned_abs()), 0)
            .expect("every i64 fits the finite binary64 exponent range")
    }

    pub fn checked_add(self, other: Self) -> Result<Self, FloatArithmeticError> {
        add(Number::read(self)?, Number::read(other)?)
    }

    pub fn checked_sub(self, other: Self) -> Result<Self, FloatArithmeticError> {
        let mut right = Number::read(other)?;
        right.negative = !right.negative;
        add(Number::read(self)?, right)
    }

    pub fn checked_mul(self, other: Self) -> Result<Self, FloatArithmeticError> {
        let a = Number::read(self)?;
        let b = Number::read(other)?;
        pack(
            a.negative != b.negative,
            u128::from(a.significand) * u128::from(b.significand),
            a.exponent + b.exponent,
        )
    }

    pub fn checked_div(self, other: Self) -> Result<Self, FloatArithmeticError> {
        let a = Number::read(self)?;
        let b = Number::read(other)?;
        if b.significand == 0 {
            return Err(FloatArithmeticError::DivisionByZero);
        }
        // Normalized inputs give a 64/65-bit quotient, hence at least eleven
        // rounding bits. A nonzero exact remainder becomes its sticky bit.
        let numerator = u128::from(a.significand) << 64;
        let denominator = u128::from(b.significand);
        let quotient = (numerator / denominator) | u128::from(numerator % denominator != 0);
        pack(
            a.negative != b.negative,
            quotient,
            a.exponent - b.exponent - 64,
        )
    }

    /// Truncating remainder, with the dividend's sign. Modular exponentiation
    /// bounds the largest binary64 exponent gap to at most twelve iterations;
    /// it never computes an overflowing floating quotient or product.
    pub fn checked_rem(self, other: Self) -> Result<Self, FloatArithmeticError> {
        let a = Number::read(self)?;
        let b = Number::read(other)?;
        if b.significand == 0 {
            return Err(FloatArithmeticError::DivisionByZero);
        }
        if a.significand == 0 || a.exponent < b.exponent {
            return Ok(self);
        }
        let divisor = u128::from(b.significand);
        let mut remainder = u128::from(a.significand) % divisor;
        let mut power = 2_u128;
        let mut gap = (a.exponent - b.exponent) as u32;
        while gap != 0 {
            if gap & 1 != 0 {
                remainder = remainder * power % divisor;
            }
            power = power * power % divisor;
            gap >>= 1;
        }
        pack(a.negative, remainder, b.exponent)
    }

    pub fn checked_neg(self) -> Result<Self, FloatArithmeticError> {
        let a = Number::read(self)?;
        Ok(value(if a.significand == 0 {
            0
        } else {
            self.to_bits() ^ SIGN
        }))
    }

    pub fn checked_abs(self) -> Result<Self, FloatArithmeticError> {
        Number::read(self)?;
        Ok(value(self.to_bits() & !SIGN))
    }

    /// Greatest integral binary64 value not greater than this finite input.
    /// No i64 narrowing occurs: all finite exponents are accepted.
    pub fn checked_floor(self) -> Result<Self, FloatArithmeticError> {
        round_integral(self, IntegralRounding::Floor)
    }

    /// Least integral binary64 value not less than this finite input.
    pub fn checked_ceil(self) -> Result<Self, FloatArithmeticError> {
        round_integral(self, IntegralRounding::Ceiling)
    }

    /// Nearest integral binary64 value, with exact half ties toward positive
    /// infinity, as in single-argument openCypher round(). This is deliberately
    /// distinct from arithmetic's nearest-even and Rust's ties-away rounding.
    /// Negative values rounded to zero produce canonical positive zero.
    pub fn checked_round(self) -> Result<Self, FloatArithmeticError> {
        round_integral(self, IntegralRounding::NearestTiesPositive)
    }

    /// The correctly rounded square root of a finite, nonnegative value, by
    /// an exact integer square root and one nearest-even rounding. A negative
    /// operand has no real root and refuses as non-finite, as does a
    /// non-finite operand.
    pub fn checked_sqrt(self) -> Result<Self, FloatArithmeticError> {
        let number = Number::read(self)?;
        if number.significand == 0 {
            return Ok(value(0));
        }
        if number.negative {
            return Err(FloatArithmeticError::NonFinite);
        }
        // value = significand * 2^exponent; make the exponent even, then
        // scale by an even power so the radicand fills 126 bits.
        let (mut significand, mut exponent) = (u128::from(number.significand), number.exponent);
        if exponent % 2 != 0 {
            significand <<= 1;
            exponent -= 1;
        }
        let scaled = significand << 72;
        let root = scaled.isqrt();
        let sticky = u128::from(root * root != scaled);
        pack(false, (root << 1) | sticky, (exponent - 72) / 2 - 1)
    }

    /// The integer part, rounding toward zero (openCypher toInteger). A value
    /// outside i64 overflows; a non-finite value refuses.
    pub fn checked_to_i64_truncated(self) -> Result<i64, FloatArithmeticError> {
        let number = Number::read(self)?;
        let magnitude = if number.exponent >= 0 {
            if number.exponent > 11 {
                return Err(FloatArithmeticError::Overflow);
            }
            u128::from(number.significand) << number.exponent
        } else {
            let shift = number.exponent.unsigned_abs();
            if shift >= 64 {
                0
            } else {
                u128::from(number.significand >> shift)
            }
        };
        if number.negative {
            if magnitude > 1_u128 << 63 {
                return Err(FloatArithmeticError::Overflow);
            }
            Ok((magnitude as i128).wrapping_neg() as i64)
        } else {
            i64::try_from(magnitude).map_err(|_| FloatArithmeticError::Overflow)
        }
    }
}

/// Fixed-point limbs of an exact sum, in units of the least subnormal
/// (2^-1074). A finite binary64 is below 2^1024 and an i128 below 2^127, so
/// 2^64 inputs stay below 2^1265: far inside 34 two's-complement limbs.
const SUM_LIMBS: usize = 34;
const SUM_BASE: i32 = -1074;

/// The exact sum of binary64 and integer inputs, rounded only when read.
///
/// Finite inputs accumulate without any rounding, so the result is the
/// correctly rounded (nearest, ties to even) exact sum or mean: independent of
/// input order, batching or partitioning, with no intermediate overflow or
/// cancellation loss. Reading follows IEEE 754 addition for the rest: a NaN,
/// or both infinities, give NaN; one infinity gives itself; an exact finite
/// result beyond binary64 range rounds to the infinity of its sign. Zero is
/// always canonical positive zero. Values are not exposed through Debug.
#[derive(Clone, PartialEq, Eq)]
pub struct ExactBinary64Sum {
    limbs: [u64; SUM_LIMBS],
    positive_infinity: bool,
    negative_infinity: bool,
    nan: bool,
}

impl Default for ExactBinary64Sum {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for ExactBinary64Sum {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ExactBinary64Sum([REDACTED])")
    }
}

impl ExactBinary64Sum {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            limbs: [0; SUM_LIMBS],
            positive_infinity: false,
            negative_infinity: false,
            nan: false,
        }
    }

    /// Add one binary64 exactly; non-finite inputs are remembered by class.
    pub fn add(&mut self, value: CanonicalF64) {
        self.add_repeated(value, 1);
    }

    /// Add `times` occurrences of one binary64 exactly, as one product.
    pub fn add_repeated(&mut self, value: CanonicalF64, times: u64) {
        if times == 0 {
            return;
        }
        let bits = value.to_bits();
        let encoded = ((bits >> 52) & 0x7ff) as u32;
        let fraction = bits & FRACTION;
        let negative = bits & SIGN != 0;
        if encoded == 0x7ff {
            if fraction != 0 {
                self.nan = true;
            } else if negative {
                self.negative_infinity = true;
            } else {
                self.positive_infinity = true;
            }
            return;
        }
        // value = significand * 2^(offset - 1074), subnormals included.
        let (significand, offset) = if encoded == 0 {
            (fraction, 0)
        } else {
            (fraction | HIDDEN, encoded - 1)
        };
        // A 53-bit significand times a u64 fits 117 bits.
        self.add_magnitude(
            negative,
            u128::from(significand) * u128::from(times),
            offset,
        );
    }

    /// Add one integer exactly (an integer is 2^1074 units).
    pub fn add_integer(&mut self, value: i128) {
        self.add_magnitude(value < 0, value.unsigned_abs(), SUM_BASE.unsigned_abs());
    }

    /// Add `times` occurrences of one i64 exactly; the product fits 127 bits.
    pub fn add_integer_repeated(&mut self, value: i64, times: u64) {
        self.add_magnitude(
            value < 0,
            u128::from(value.unsigned_abs()) * u128::from(times),
            SUM_BASE.unsigned_abs(),
        );
    }

    fn add_magnitude(&mut self, negative: bool, magnitude: u128, offset: u32) {
        if magnitude == 0 {
            return;
        }
        let index = (offset / 64) as usize;
        let shift = offset % 64;
        let low = magnitude << shift;
        let high = if shift == 0 {
            0
        } else {
            (magnitude >> (128 - shift)) as u64
        };
        let words = [low as u64, (low >> 64) as u64, high];
        let mut carry = false;
        for (at, limb) in self.limbs[index..].iter_mut().enumerate() {
            if at >= words.len() && !carry {
                break;
            }
            let word = words.get(at).copied().unwrap_or(0);
            let (value, first, second) = if negative {
                let (value, first) = limb.overflowing_sub(word);
                let (value, second) = value.overflowing_sub(u64::from(carry));
                (value, first, second)
            } else {
                let (value, first) = limb.overflowing_add(word);
                let (value, second) = value.overflowing_add(u64::from(carry));
                (value, first, second)
            };
            *limb = value;
            carry = first || second;
        }
    }

    fn special(&self) -> Option<CanonicalF64> {
        if self.nan || (self.positive_infinity && self.negative_infinity) {
            Some(CanonicalF64::new(f64::NAN))
        } else if self.positive_infinity {
            Some(CanonicalF64::new(f64::INFINITY))
        } else if self.negative_infinity {
            Some(CanonicalF64::new(f64::NEG_INFINITY))
        } else {
            None
        }
    }

    fn magnitude(&self) -> (bool, [u64; SUM_LIMBS]) {
        let negative = self.limbs[SUM_LIMBS - 1] & SIGN != 0;
        let mut magnitude = self.limbs;
        if negative {
            let mut carry = true;
            for limb in &mut magnitude {
                let (value, overflow) = (!*limb).overflowing_add(u64::from(carry));
                *limb = value;
                carry = overflow;
            }
        }
        (negative, magnitude)
    }

    /// The correctly rounded sum of every input so far.
    #[must_use]
    pub fn sum(&self) -> CanonicalF64 {
        if let Some(special) = self.special() {
            return special;
        }
        let (negative, magnitude) = self.magnitude();
        round_limbs(negative, &magnitude, false, SUM_BASE)
    }

    /// The correctly rounded exact sum divided by `count`; None for zero.
    #[must_use]
    pub fn mean(&self, count: u64) -> Option<CanonicalF64> {
        if count == 0 {
            return None;
        }
        if let Some(special) = self.special() {
            return Some(special);
        }
        let (negative, magnitude) = self.magnitude();
        // Divide magnitude * 2^64: the appended zero limb keeps every quotient
        // bit a u64 divisor can remove, and the remainder becomes sticky.
        let mut quotient = [0_u64; SUM_LIMBS + 1];
        let mut remainder = 0_u128;
        let divisor = u128::from(count);
        for at in (0..=SUM_LIMBS).rev() {
            let limb = if at == 0 { 0 } else { magnitude[at - 1] };
            let current = (remainder << 64) | u128::from(limb);
            quotient[at] = (current / divisor) as u64;
            remainder = current % divisor;
        }
        Some(round_limbs(
            negative,
            &quotient,
            remainder != 0,
            SUM_BASE - 64,
        ))
    }
}

/// Round `(-1)^negative * (magnitude + tail) * 2^base` once, where `tail` is a
/// nonzero fraction below bit zero when `sticky`. The top 126 bits plus one
/// jammed sticky bit carry every bit that can decide nearest-even rounding.
fn round_limbs(negative: bool, magnitude: &[u64], sticky: bool, base: i32) -> CanonicalF64 {
    let Some(top) = magnitude.iter().rposition(|limb| *limb != 0) else {
        // A tail alone is below half the least subnormal: base <= -1076.
        return value(0);
    };
    let width = top as u32 * 64 + (64 - magnitude[top].leading_zeros());
    let word = |at: usize| magnitude.get(at).map_or(0, |limb| u128::from(*limb));
    let (bits, exponent) = if width <= 126 {
        let exact = (word(1) << 64) | word(0);
        ((exact << 1) | u128::from(sticky), base - 1)
    } else {
        let shift = width - 126;
        let index = (shift / 64) as usize;
        let offset = shift % 64;
        let mut kept = ((word(index + 1) << 64) | word(index)) >> offset;
        if offset != 0 {
            kept |= word(index + 2) << (128 - offset);
        }
        let tail = sticky
            || magnitude[..index].iter().any(|limb| *limb != 0)
            || (offset != 0 && magnitude[index] & ((1_u64 << offset) - 1) != 0);
        ((kept << 1) | u128::from(tail), base + shift as i32 - 1)
    };
    match pack(negative, bits, exponent) {
        Ok(rounded) => rounded,
        Err(_) if negative => CanonicalF64::new(f64::NEG_INFINITY),
        Err(_) => CanonicalF64::new(f64::INFINITY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(bits: u64) -> CanonicalF64 {
        CanonicalF64::from_bits_canonical(bits).unwrap()
    }
    fn f(number: f64) -> CanonicalF64 {
        CanonicalF64::new(number)
    }

    #[test]
    fn round_ties_and_subnormal_transitions() {
        let one = bits(0x3ff0_0000_0000_0000);
        let half_ulp = bits(0x3ca0_0000_0000_0000);
        assert_eq!(one.checked_add(half_ulp), Ok(one));
        assert_eq!(
            bits(one.to_bits() + 1).checked_add(half_ulp),
            Ok(bits(one.to_bits() + 2))
        );
        assert_eq!(bits(1).checked_div(f(2.0)), Ok(bits(0)));
        assert_eq!(bits(3).checked_div(f(2.0)), Ok(bits(2)));
        assert_eq!(
            bits(0x0010_0000_0000_0000).checked_sub(bits(1)),
            Ok(bits(FRACTION))
        );
        assert_eq!(bits(FRACTION).checked_add(bits(1)), Ok(bits(HIDDEN)));
        assert_eq!(bits(SIGN | 1).checked_mul(f(0.5)), Ok(bits(0)));
    }

    #[test]
    fn exact_integer_conversion_rounds_both_tie_directions() {
        for (integer, encoded) in [
            (0, 0),
            (1, 0x3ff0_0000_0000_0000),
            ((1_i64 << 53) + 1, 0x4340_0000_0000_0000),
            ((1_i64 << 53) + 3, 0x4340_0000_0000_0002),
            (i64::MAX, 0x43e0_0000_0000_0000),
            (i64::MIN, 0xc3e0_0000_0000_0000),
        ] {
            assert_eq!(CanonicalF64::from_i64_rounded(integer).to_bits(), encoded);
        }
    }

    #[test]
    fn integral_rounding_keeps_signs_exact_ties_and_neighboring_values() {
        for (input, floor, ceil, nearest) in [
            (-2.75, -3.0, -2.0, -3.0),
            (-2.5, -3.0, -2.0, -2.0),
            (-1.5, -2.0, -1.0, -1.0),
            (-0.5, -1.0, 0.0, 0.0),
            (-0.25, -1.0, 0.0, 0.0),
            (0.0, 0.0, 0.0, 0.0),
            (0.25, 0.0, 1.0, 0.0),
            (0.5, 0.0, 1.0, 1.0),
            (1.5, 1.0, 2.0, 2.0),
            (2.75, 2.0, 3.0, 3.0),
        ] {
            assert_eq!(f(input).checked_floor(), Ok(f(floor)));
            assert_eq!(f(input).checked_ceil(), Ok(f(ceil)));
            assert_eq!(f(input).checked_round(), Ok(f(nearest)));
        }
        let half = 0x3fe0_0000_0000_0000;
        assert_eq!(bits(half - 1).checked_round(), Ok(f(0.0)));
        assert_eq!(bits(half + 1).checked_round(), Ok(f(1.0)));
        assert_eq!(bits(SIGN | (half - 1)).checked_round(), Ok(f(0.0)));
        assert_eq!(bits(SIGN | (half + 1)).checked_round(), Ok(f(-1.0)));
        assert_eq!(f(-0.0).checked_round().unwrap().to_bits(), 0);
    }

    #[test]
    fn integral_rounding_admits_every_finite_exponent_and_refuses_nonfinite() {
        for raw in [1, FRACTION, HIDDEN] {
            assert_eq!(bits(raw).checked_floor(), Ok(f(0.0)));
            assert_eq!(bits(raw).checked_ceil(), Ok(f(1.0)));
            assert_eq!(bits(raw).checked_round(), Ok(f(0.0)));
            assert_eq!(bits(SIGN | raw).checked_floor(), Ok(f(-1.0)));
            assert_eq!(bits(SIGN | raw).checked_ceil(), Ok(f(0.0)));
            assert_eq!(bits(SIGN | raw).checked_round(), Ok(f(0.0)));
        }
        for raw in [
            0x4330_0000_0000_0001,
            0x43e0_0000_0000_0000,
            0x7fef_ffff_ffff_ffff,
        ] {
            for sign in [0, SIGN] {
                let number = bits(sign | raw);
                assert_eq!(number.checked_floor(), Ok(number));
                assert_eq!(number.checked_ceil(), Ok(number));
                assert_eq!(number.checked_round(), Ok(number));
            }
        }
        for nonfinite in [f(f64::INFINITY), f(f64::NEG_INFINITY), f(f64::NAN)] {
            assert_eq!(
                nonfinite.checked_floor(),
                Err(FloatArithmeticError::NonFinite)
            );
            assert_eq!(
                nonfinite.checked_ceil(),
                Err(FloatArithmeticError::NonFinite)
            );
            assert_eq!(
                nonfinite.checked_round(),
                Err(FloatArithmeticError::NonFinite)
            );
        }
    }

    #[test]
    fn integral_rounding_matches_independent_hardware_neighbors() {
        let mut state = 0x696e_7465_6772_616c_u64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            if state & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 {
                continue;
            }
            let input = f64::from_bits(state);
            let lower = input.floor();
            let upper = input.ceil();
            let nearest = if input - lower < upper - input {
                lower
            } else {
                upper
            };
            assert_eq!(f(input).checked_floor(), Ok(f(lower)), "bits={state:016x}");
            assert_eq!(f(input).checked_ceil(), Ok(f(upper)), "bits={state:016x}");
            assert_eq!(
                f(input).checked_round(),
                Ok(f(nearest)),
                "bits={state:016x}"
            );
        }
    }

    #[test]
    fn cancellation_remainder_and_exception_boundaries() {
        let max = bits(0x7fef_ffff_ffff_ffff);
        assert_eq!(max.checked_add(max), Err(FloatArithmeticError::Overflow));
        assert_eq!(max.checked_mul(f(2.0)), Err(FloatArithmeticError::Overflow));
        assert_eq!(
            f(1.0).checked_div(bits(0)),
            Err(FloatArithmeticError::DivisionByZero)
        );
        assert_eq!(
            bits(0).checked_rem(bits(0)),
            Err(FloatArithmeticError::DivisionByZero)
        );
        assert_eq!(max.checked_sub(max), Ok(bits(0)));
        assert_eq!(f(-5.5).checked_rem(f(2.0)), Ok(f(-1.5)));
        assert_eq!(max.checked_rem(bits(1)), Ok(bits(0)));
        assert_eq!(bits(SIGN | 1).checked_abs(), Ok(bits(1)));
        assert_eq!(bits(0).checked_neg(), Ok(bits(0)));
        for nonfinite in [f(f64::INFINITY), f(f64::NEG_INFINITY), f(f64::NAN)] {
            assert_eq!(
                nonfinite.checked_add(f(1.0)),
                Err(FloatArithmeticError::NonFinite)
            );
            assert_eq!(
                f(1.0).checked_mul(nonfinite),
                Err(FloatArithmeticError::NonFinite)
            );
            assert_eq!(
                nonfinite.checked_neg(),
                Err(FloatArithmeticError::NonFinite)
            );
        }
    }

    #[test]
    fn bit_kernel_matches_ieee_operations_on_deterministic_finite_corpus() {
        let mut state = 0x6e75_6d65_7269_6331_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // Every exponent, including subnormals, but not non-finite inputs.
            let encoded = if state & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 {
                state ^ 0x0010_0000_0000_0000
            } else {
                state
            };
            f(f64::from_bits(encoded))
        };
        for _ in 0..20_000 {
            let a = next();
            let b = next();
            for (actual, expected) in [
                (a.checked_add(b), a.get() + b.get()),
                (a.checked_sub(b), a.get() - b.get()),
                (a.checked_mul(b), a.get() * b.get()),
                (a.checked_div(b), a.get() / b.get()),
                (a.checked_rem(b), a.get() % b.get()),
            ] {
                if b.to_bits() == 0 {
                    continue;
                }
                if expected.is_infinite() {
                    assert_eq!(actual, Err(FloatArithmeticError::Overflow));
                } else {
                    assert_eq!(
                        actual,
                        Ok(f(expected)),
                        "a={:016x} b={:016x}",
                        a.to_bits(),
                        b.to_bits()
                    );
                }
            }
        }
    }

    fn exact(values: &[f64]) -> ExactBinary64Sum {
        let mut sum = ExactBinary64Sum::new();
        for value in values {
            sum.add(f(*value));
        }
        sum
    }

    #[test]
    fn exact_sum_rounds_once_whatever_the_order() {
        let max = f64::MAX;
        for (values, expected) in [
            // Naive left-to-right addition gives 0.6000000000000001.
            (&[0.1, 0.2, 0.3][..], 0.6),
            // Naive addition loses the 1.0 to cancellation.
            (&[1e100, 1.0, -1e100][..], 1.0),
            // No intermediate overflow.
            (&[max, max, -max][..], max),
            (&[max, max][..], f64::INFINITY),
            (&[-max, -max][..], f64::NEG_INFINITY),
            (&[2.5, -2.5][..], 0.0),
            (&[][..], 0.0),
        ] {
            let mut order = values.to_vec();
            for _ in 0..values.len().max(1) * 2 {
                if !order.is_empty() {
                    order.rotate_left(1);
                }
                if order.len() > 2 {
                    order.swap(0, 1);
                }
                assert_eq!(exact(&order).sum(), f(expected), "{order:?}");
            }
        }
        assert_eq!(exact(&[-2.5, 2.5]).sum().to_bits(), 0);
        assert_eq!(exact(&[max, max]).mean(2), Some(f(max)));
        assert_eq!(exact(&[1.0, 2.0]).mean(2), Some(f(1.5)));
        assert_eq!(exact(&[1.0]).mean(0), None);
    }

    #[test]
    fn exact_sum_follows_ieee_for_non_finite_inputs() {
        let nan = f(f64::NAN);
        assert_eq!(exact(&[1.0, f64::NAN]).sum(), nan);
        assert_eq!(exact(&[f64::INFINITY, f64::NEG_INFINITY]).sum(), nan);
        assert_eq!(exact(&[f64::INFINITY, -f64::MAX]).sum(), f(f64::INFINITY));
        assert_eq!(
            exact(&[f64::NEG_INFINITY, 1.0]).mean(2),
            Some(f(f64::NEG_INFINITY))
        );
    }

    #[test]
    fn exact_mean_rounds_subnormal_quotients_to_nearest_even() {
        let units = |count: u64| bits(count);
        for (input, count, expected) in [(3, 3, 1), (3, 2, 2), (1, 3, 0), (2, 3, 1), (5, 2, 2)] {
            let mut sum = ExactBinary64Sum::new();
            sum.add(units(input));
            assert_eq!(sum.mean(count), Some(units(expected)), "{input}/{count}");
        }
        let mut negative = ExactBinary64Sum::new();
        negative.add(bits(SIGN | 1));
        assert_eq!(negative.mean(3).map(|value| value.to_bits()), Some(0));
    }

    #[test]
    fn exact_sum_mixes_integers_exactly() {
        let mut sum = ExactBinary64Sum::new();
        sum.add_integer(i128::from(i64::MAX));
        sum.add(f(0.5));
        // 2^63 - 0.5 rounds to 2^63.
        assert_eq!(sum.sum().to_bits(), 0x43e0_0000_0000_0000);
        let mut low = ExactBinary64Sum::new();
        low.add_integer(i128::MIN);
        assert_eq!(low.sum().to_bits(), 0xc7e0_0000_0000_0000);
        low.add_integer(i128::MAX);
        low.add_integer(1);
        assert_eq!(low.sum().to_bits(), 0);
        let mut mean = ExactBinary64Sum::new();
        for value in [1, 2, 4] {
            mean.add_integer(value);
        }
        assert_eq!(mean.mean(3), Some(f(7.0 / 3.0)));
    }

    #[test]
    fn square_roots_match_ieee_sqrt_on_a_deterministic_finite_corpus() {
        let mut state = 0x7371_7274_2d72_6f6f_u64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // Every exponent, including subnormals; finite and nonnegative.
            let mut bits = state & !SIGN;
            if bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 {
                bits ^= 0x0010_0000_0000_0000;
            }
            let x = f(f64::from_bits(bits));
            // IEEE 754 requires sqrt to be correctly rounded.
            assert_eq!(x.checked_sqrt(), Ok(f(x.get().sqrt())), "{bits:016x}");
        }
        for (input, root) in [(0.0, 0.0), (4.0, 2.0), (2.0, core::f64::consts::SQRT_2)] {
            assert_eq!(f(input).checked_sqrt(), Ok(f(root)));
        }
        assert_eq!(bits(1).checked_sqrt(), Ok(f(5e-324_f64.sqrt())));
        assert_eq!(f(-1.0).checked_sqrt(), Err(FloatArithmeticError::NonFinite));
        assert_eq!(
            f(f64::INFINITY).checked_sqrt(),
            Err(FloatArithmeticError::NonFinite)
        );
    }

    #[test]
    fn truncation_to_i64_rounds_toward_zero_and_refuses_overflow() {
        const TWO_63: f64 = 9_223_372_036_854_775_808.0;
        for (input, expected) in [
            (3.7, 3),
            (-3.7, -3),
            (0.5, 0),
            (-0.5, 0),
            (5e-324, 0),
            (9.007_199_254_740_993e15, 9_007_199_254_740_992),
            (-TWO_63, i64::MIN),
        ] {
            assert_eq!(f(input).checked_to_i64_truncated(), Ok(expected), "{input}");
        }
        for input in [TWO_63, -9.3e18, 1e300] {
            assert_eq!(
                f(input).checked_to_i64_truncated(),
                Err(FloatArithmeticError::Overflow),
                "{input}"
            );
        }
        assert_eq!(
            f(f64::NAN).checked_to_i64_truncated(),
            Err(FloatArithmeticError::NonFinite)
        );
    }

    #[test]
    fn repeated_addition_equals_that_many_single_additions() {
        for (value, times) in [(0.1, 10_u64), (-1e300, 7), (5e-324, 3), (f64::MAX, 2)] {
            let mut repeated = ExactBinary64Sum::new();
            repeated.add_repeated(f(value), times);
            let mut single = ExactBinary64Sum::new();
            for _ in 0..times {
                single.add(f(value));
            }
            assert_eq!(repeated, single, "{value} x {times}");
        }
        let mut repeated = ExactBinary64Sum::new();
        repeated.add_integer_repeated(i64::MIN, u64::MAX);
        let mut product = ExactBinary64Sum::new();
        product.add_integer(i128::from(i64::MIN) * i128::from(u64::MAX));
        assert_eq!(repeated, product);
        let mut none = ExactBinary64Sum::new();
        none.add_repeated(f(f64::NAN), 0);
        assert_eq!(none, ExactBinary64Sum::new());
    }

    #[test]
    fn exact_sum_matches_single_ieee_addition_and_exact_cancellation() {
        let mut state = 0x7375_6d6d_6174_696f_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let encoded = if state & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 {
                state ^ 0x0010_0000_0000_0000
            } else {
                state
            };
            f(f64::from_bits(encoded))
        };
        for _ in 0..20_000 {
            let (a, b) = (next(), next());
            let mut pair = ExactBinary64Sum::new();
            pair.add(a);
            pair.add(b);
            // One IEEE addition is the correctly rounded exact sum.
            assert_eq!(
                pair.sum(),
                f(a.get() + b.get()),
                "a={:016x} b={:016x}",
                a.to_bits(),
                b.to_bits()
            );
            assert_eq!(pair.mean(1), Some(pair.sum()));
            // Adding -a cancels a exactly at any magnitude gap.
            pair.add(a.checked_neg().unwrap());
            assert_eq!(pair.sum(), b);
        }
    }
}
