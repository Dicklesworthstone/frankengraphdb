//! Fixed-scale products and quotients without a floating-point intermediate.
//! Keep quotient/remainder pairs, not an overflowing unscaled coefficient.

use super::{
    CanonicalDecimal, DecimalError, DecimalOperation, MAX_DECIMAL_COEFFICIENT,
    STRICT_PORTABLE_DECIMAL_SCALE,
};

const UNIT: u128 = 10_u128.pow(STRICT_PORTABLE_DECIMAL_SCALE);
const LIMIT: u128 = MAX_DECIMAL_COEFFICIENT as u128;

impl CanonicalDecimal {
    /// Multiply and round once to scale 18, with ties to an even coefficient.
    /// A wide intermediate is not an overflow when the rounded result fits
    /// the profile. Sub-quantum results round to canonical zero when required.
    /// No allocation, float conversion, saturation or double rounding occurs.
    pub fn checked_mul(self, rhs: Self) -> Result<Self, DecimalError> {
        self.product_ratio(
            rhs,
            rhs.coefficient().unsigned_abs(),
            UNIT,
            DecimalOperation::Multiply,
        )
    }

    /// Divide and round once to scale 18, with ties to an even coefficient.
    /// Division by zero (including zero divided by zero) is a typed error;
    /// a result outside the coefficient range is an arithmetic overflow.
    /// The numerator is never first multiplied in a narrowing i128 domain.
    pub fn checked_div(self, rhs: Self) -> Result<Self, DecimalError> {
        if rhs.coefficient() == 0 {
            return Err(DecimalError::DivisionByZero);
        }
        self.product_ratio(
            rhs,
            UNIT,
            rhs.coefficient().unsigned_abs(),
            DecimalOperation::Divide,
        )
    }

    /// Exact remainder after truncating the mathematical quotient toward zero.
    /// This is not multiplication by the scale-rounded result of checked_div.
    /// A nonzero remainder has the dividend's sign, independently of rhs's
    /// sign; its magnitude is less than rhs's. No rescaling is needed.
    pub fn checked_rem(self, rhs: Self) -> Result<Self, DecimalError> {
        if rhs.coefficient() == 0 {
            return Err(DecimalError::DivisionByZero);
        }
        let coefficient = self.coefficient().checked_rem(rhs.coefficient()).ok_or(
            DecimalError::ArithmeticOverflow {
                operation: DecimalOperation::Remainder,
            },
        )?;
        Self::from_coefficient(coefficient)
    }

    fn product_ratio(
        self,
        rhs: Self,
        multiplier: u128,
        divisor: u128,
        operation: DecimalOperation,
    ) -> Result<Self, DecimalError> {
        let overflow = || DecimalError::ArithmeticOverflow { operation };
        let magnitude = rounded_product(self.coefficient().unsigned_abs(), multiplier, divisor)
            .ok_or_else(overflow)?;
        let coefficient = i128::try_from(magnitude).map_err(|_| overflow())?;
        let negative = self.coefficient().is_negative() != rhs.coefficient().is_negative();
        Self::from_coefficient(if negative { -coefficient } else { coefficient })
    }
}

fn rounded_product(a: u128, b: u128, divisor: u128) -> Option<u128> {
    let (quotient, remainder) = match a.checked_mul(b) {
        Some(product) => (product / divisor, product % divisor),
        None => wide_product_quotient(a, b, divisor)?,
    };
    // Comparing r with d-r avoids overflowing a doubled remainder. Round only
    // after the complete exact quotient/remainder has been computed.
    let half = divisor - remainder;
    let increment = remainder > half || (remainder == half && quotient % 2 == 1);
    quotient
        .checked_add(u128::from(increment))
        .filter(|&coefficient| coefficient <= LIMIT)
}

// Invariant after each multiplier prefix p: a*p = q*divisor + r, 0<=r<divisor.
// For its next bit t, double that pair and add t*(a/divisor, a%divisor).
// The new remainder is below 3*divisor, so at most two subtractions normalize
// it. All inputs are profile magnitudes (or UNIT): q<=LIMIT at loop entry,
// a/divisor<=LIMIT and divisor<=LIMIT, hence every temporary is <3*10^34,
// well below u128::MAX. Checked arithmetic additionally guards the boundary.
// A quotient over LIMIT cannot shrink as more nonnegative bits are appended.
// At most 113 iterations are needed for a 34-digit coefficient; storage is
// fixed-size. This is a decimal kernel, not a general unbounded integer type.
fn wide_product_quotient(a: u128, b: u128, divisor: u128) -> Option<(u128, u128)> {
    let (add_q, add_r) = (a / divisor, a % divisor);
    let (mut quotient, mut remainder) = (0_u128, 0_u128);
    for bit in (0..(u128::BITS - b.leading_zeros())).rev() {
        quotient = quotient.checked_mul(2)?;
        remainder = remainder.checked_mul(2)?;
        if (b >> bit) & 1 != 0 {
            quotient = quotient.checked_add(add_q)?;
            remainder = remainder.checked_add(add_r)?;
        }
        for _ in 0..2 {
            if remainder >= divisor {
                remainder -= divisor;
                quotient = quotient.checked_add(1)?;
            }
        }
        if quotient > LIMIT {
            return None;
        }
    }
    Some((quotient, remainder))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(coefficient: i128) -> CanonicalDecimal {
        CanonicalDecimal::from_coefficient(coefficient).unwrap()
    }

    #[test]
    fn wide_intermediates_are_not_confused_with_final_overflow() {
        let max = d(MAX_DECIMAL_COEFFICIENT);
        let one = d(UNIT as i128);
        let two = d(2 * UNIT as i128);
        assert!(MAX_DECIMAL_COEFFICIENT.checked_mul(UNIT as i128).is_none());
        for sign in [-1, 1] {
            let value = d(sign * MAX_DECIMAL_COEFFICIENT);
            assert_eq!(value.checked_mul(one), Ok(value));
            assert_eq!(one.checked_mul(value), Ok(value));
            assert_eq!(value.checked_div(one), Ok(value));
            assert_eq!(value.checked_div(value), Ok(one));
        }
        assert_eq!(max.checked_div(two), Ok(d(5 * 10_i128.pow(33))));
        assert_eq!(
            d(MAX_DECIMAL_COEFFICIENT / 2).checked_mul(two),
            Ok(d(MAX_DECIMAL_COEFFICIENT - 1))
        );
        let trillion = CanonicalDecimal::from_integer(1_000_000_000_000).unwrap();
        let thousand = CanonicalDecimal::from_integer(1_000).unwrap();
        assert_eq!(
            trillion.checked_mul(thousand),
            CanonicalDecimal::from_integer(1_000_000_000_000_000)
        );
        assert_eq!(
            trillion.checked_div(thousand),
            CanonicalDecimal::from_integer(1_000_000_000)
        );
    }

    #[test]
    fn multiplication_and_division_round_half_even_for_both_signs() {
        for left_sign in [-1, 1] {
            for right_sign in [-1, 1] {
                let sign = left_sign * right_sign;
                for (input, expected) in [(1, 0), (3, 2), (5, 2), (7, 4)] {
                    assert_eq!(
                        d(left_sign * input).checked_mul(d(right_sign * (UNIT / 2) as i128)),
                        Ok(d(sign * expected)),
                    );
                    assert_eq!(
                        d(left_sign * input).checked_div(d(right_sign * (2 * UNIT) as i128)),
                        Ok(d(sign * expected)),
                    );
                }
                for (factor, expected) in [(UNIT / 2 - 1, 0), (UNIT / 2, 0), (UNIT / 2 + 1, 1)] {
                    assert_eq!(
                        d(left_sign).checked_mul(d(right_sign * factor as i128)),
                        Ok(d(sign * expected))
                    );
                }
                for (divisor, expected) in [(2 * UNIT - 1, 1), (2 * UNIT, 0), (2 * UNIT + 1, 0)] {
                    assert_eq!(
                        d(left_sign).checked_div(d(right_sign * divisor as i128)),
                        Ok(d(sign * expected))
                    );
                }
            }
        }
        assert_eq!(d(1).checked_mul(d(1)), Ok(d(0)));
        assert_eq!(d(-1).checked_div(d(MAX_DECIMAL_COEFFICIENT)), Ok(d(0)));
        assert_eq!(d(0).encode(), d(-1).checked_mul(d(1)).unwrap().encode());
    }

    #[test]
    fn decimal_quotients_retain_the_last_coefficient_digit() {
        for (a, b, expected) in [
            (1, 3, 333_333_333_333_333_333),
            (2, 3, 666_666_666_666_666_667),
            (1, 6, 166_666_666_666_666_667),
            (22, 7, 3_142_857_142_857_142_857),
        ] {
            for sign in [-1, 1] {
                assert_eq!(
                    CanonicalDecimal::from_integer(sign * a)
                        .unwrap()
                        .checked_div(CanonicalDecimal::from_integer(b).unwrap()),
                    Ok(d(sign * expected)),
                );
            }
        }
        let large = d(9_007_199_254_740_993_000_000_000_000_000_001);
        assert_eq!(
            large.checked_div(d(2 * UNIT as i128)),
            Ok(d(4_503_599_627_370_496_500_000_000_000_000_000))
        );
    }

    #[test]
    fn zero_divisors_and_out_of_profile_results_refuse_instead_of_wrapping() {
        for value in [0, 1, -1, MAX_DECIMAL_COEFFICIENT, -MAX_DECIMAL_COEFFICIENT] {
            assert_eq!(
                d(value).checked_div(d(0)),
                Err(DecimalError::DivisionByZero)
            );
            assert_eq!(
                d(value).checked_rem(d(0)),
                Err(DecimalError::DivisionByZero)
            );
            assert_eq!(d(value).checked_mul(d(0)), Ok(d(0)));
        }
        for sign in [-1, 1] {
            let max = d(sign * MAX_DECIMAL_COEFFICIENT);
            assert_eq!(
                max.checked_mul(d(2 * UNIT as i128)),
                Err(DecimalError::ArithmeticOverflow {
                    operation: DecimalOperation::Multiply
                })
            );
            for divisor in [1, UNIT as i128 - 1] {
                assert_eq!(
                    max.checked_div(d(divisor)),
                    Err(DecimalError::ArithmeticOverflow {
                        operation: DecimalOperation::Divide
                    })
                );
            }
        }
        assert_eq!(d(0).checked_div(d(1)), Ok(d(0)));
    }

    #[test]
    fn remainder_is_exact_and_has_the_dividends_sign() {
        for left_sign in [-1, 1] {
            for right_sign in [-1, 1] {
                assert_eq!(
                    d(left_sign * 5_500_000_000_000_000_000)
                        .checked_rem(d(right_sign * 2_000_000_000_000_000_000)),
                    Ok(d(left_sign * 1_500_000_000_000_000_000)),
                );
                assert_eq!(
                    d(left_sign).checked_rem(d(right_sign * MAX_DECIMAL_COEFFICIENT)),
                    Ok(d(left_sign))
                );
                assert_eq!(
                    d(left_sign * MAX_DECIMAL_COEFFICIENT).checked_rem(d(right_sign)),
                    Ok(d(0))
                );
            }
        }
    }

    #[test]
    fn bounded_long_division_agrees_with_independent_direct_integer_products() {
        // Exercise the wide algorithm even when a direct product fits. Both
        // operand orders include full-width multiplier prefixes, zero and
        // divisors above/below each operand; expected values use u128 division.
        let mut state = 0x9e37_79b9_7f4a_7c15_94d0_49bb_1331_11eb_u128;
        for _ in 0..2_000 {
            state = state
                .wrapping_mul(0xda94_2042_e4dd_58b5)
                .wrapping_add(1)
                .rotate_left(31);
            let a = state % (LIMIT + 1);
            let b = (state >> 112) % 1_025;
            for divisor in [1, UNIT, (state.rotate_left(53) % LIMIT) + 1] {
                let product = a.checked_mul(b).unwrap();
                let quotient = product / divisor;
                let expected = (quotient <= LIMIT).then_some((quotient, product % divisor));
                assert_eq!(wide_product_quotient(a, b, divisor), expected);
                assert_eq!(wide_product_quotient(b, a, divisor), expected);
            }
        }
    }
}
