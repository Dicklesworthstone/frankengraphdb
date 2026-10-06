//! Finite binary64 arithmetic with integer-only rounding.
//!
//! Arithmetic rounds once to nearest, ties to even, with gradual underflow.
//! Integral rounding has explicit floor/ceiling/nearest-ties-positive policies.
//! All results have canonical positive zero. No native floating arithmetic, libm, FMA,
//! allocation, or ambient rounding mode is used by the kernel. Non-finite
//! operands, overflow and division by zero are explicit refusals. This is not
//! the complete STRICT_PORTABLE numeric profile or a transcendental library.

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
    pack(number.negative, u128::from(whole) + u128::from(increment), 0)
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
        for raw in [0x4330_0000_0000_0001, 0x43e0_0000_0000_0000, 0x7fef_ffff_ffff_ffff] {
            for sign in [0, SIGN] {
                let number = bits(sign | raw);
                assert_eq!(number.checked_floor(), Ok(number));
                assert_eq!(number.checked_ceil(), Ok(number));
                assert_eq!(number.checked_round(), Ok(number));
            }
        }
        for nonfinite in [f(f64::INFINITY), f(f64::NEG_INFINITY), f(f64::NAN)] {
            assert_eq!(nonfinite.checked_floor(), Err(FloatArithmeticError::NonFinite));
            assert_eq!(nonfinite.checked_ceil(), Err(FloatArithmeticError::NonFinite));
            assert_eq!(nonfinite.checked_round(), Err(FloatArithmeticError::NonFinite));
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
            let nearest = if input - lower < upper - input { lower } else { upper };
            assert_eq!(f(input).checked_floor(), Ok(f(lower)), "bits={state:016x}");
            assert_eq!(f(input).checked_ceil(), Ok(f(upper)), "bits={state:016x}");
            assert_eq!(f(input).checked_round(), Ok(f(nearest)), "bits={state:016x}");
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
}
