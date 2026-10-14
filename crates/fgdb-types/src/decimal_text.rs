//! Bounded inverse of CanonicalDecimal's fixed-scale Display representation.
//! Parsing never rounds, rescales, allocates or goes through binary floating point.

use crate::decimal::{
    CanonicalDecimal, STRICT_PORTABLE_DECIMAL_PRECISION, STRICT_PORTABLE_DECIMAL_SCALE,
};
use core::fmt;

/// At most the profile precision's digits, one decimal point and one minus sign.
pub const MAX_DECIMAL_TEXT_BYTES: usize = STRICT_PORTABLE_DECIMAL_PRECISION as usize + 2;

/// Structural decimal-text refusal. Diagnostics never include the input value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecimalParseError {
    TooLong,
    InvalidSyntax,
    NonCanonical,
    OutOfRange,
}

impl fmt::Display for DecimalParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLong => "decimal text exceeds the canonical byte limit",
            Self::InvalidSyntax => "decimal text requires ASCII digits and the exact profile scale",
            Self::NonCanonical => "decimal text is not the canonical representation",
            Self::OutOfRange => "decimal text exceeds the profile's coefficient range",
        })
    }
}
impl std::error::Error for DecimalParseError {}

impl CanonicalDecimal {
    /// Parse exactly the representation emitted by this type's Display impl.
    ///
    /// Require an optional minus sign, an unsigned integral part without
    /// leading zeros, a decimal point, and exactly 18 fractional digits under
    /// the current STRICT_PORTABLE profile. Reject negative zero, whitespace,
    /// plus signs, exponents, digit separators and non-ASCII numeric characters.
    /// Shorter or longer scales refuse rather than implicitly round or rescale.
    ///
    /// The original byte length is bounded before traversal. Checked integer
    /// accumulation and the existing coefficient constructor own admission;
    /// parsing allocates nothing and never converts through a floating value.
    /// Every accepted text round-trips byte-for-byte through Display and the
    /// existing fixed-width canonical encoding. Arithmetic policy is unchanged.
    pub fn parse_canonical(text: &str) -> Result<Self, DecimalParseError> {
        if text.len() > MAX_DECIMAL_TEXT_BYTES {
            return Err(DecimalParseError::TooLong);
        }
        let (negative, unsigned) = match text.strip_prefix('-') {
            Some(unsigned) => (true, unsigned),
            None => (false, text),
        };
        let (whole, fraction) = unsigned
            .split_once('.')
            .ok_or(DecimalParseError::InvalidSyntax)?;
        if whole.is_empty()
            || fraction.len() != STRICT_PORTABLE_DECIMAL_SCALE as usize
            || !whole
                .bytes()
                .chain(fraction.bytes())
                .all(|byte| byte.is_ascii_digit())
        {
            return Err(DecimalParseError::InvalidSyntax);
        }
        if whole.len() > 1 && whole.starts_with('0') {
            return Err(DecimalParseError::NonCanonical);
        }
        if whole.len()
            > (STRICT_PORTABLE_DECIMAL_PRECISION - STRICT_PORTABLE_DECIMAL_SCALE) as usize
        {
            return Err(DecimalParseError::OutOfRange);
        }
        let mut coefficient = 0_i128;
        for digit in whole.bytes().chain(fraction.bytes()) {
            coefficient = coefficient
                .checked_mul(10)
                .and_then(|value| value.checked_add(i128::from(digit - b'0')))
                .ok_or(DecimalParseError::OutOfRange)?;
        }
        if negative {
            if coefficient == 0 {
                return Err(DecimalParseError::NonCanonical);
            }
            coefficient = coefficient
                .checked_neg()
                .ok_or(DecimalParseError::OutOfRange)?;
        }
        Self::from_coefficient(coefficient).map_err(|_| DecimalParseError::OutOfRange)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decimal::{MAX_DECIMAL_COEFFICIENT, MIN_DECIMAL_COEFFICIENT};

    #[test]
    fn canonical_text_retains_the_full_coefficient_domain_and_smallest_quantum() {
        for coefficient in [
            MIN_DECIMAL_COEFFICIENT,
            -9_007_199_254_740_993_000_000_000_000_000_001,
            -1_000_000_000_000_000_000,
            -1,
            0,
            1,
            1_000_000_000_000_000_000,
            9_007_199_254_740_993_000_000_000_000_000_001,
            MAX_DECIMAL_COEFFICIENT,
        ] {
            let value = CanonicalDecimal::from_coefficient(coefficient).unwrap();
            let text = value.to_string();
            let parsed = CanonicalDecimal::parse_canonical(&text).unwrap();
            assert_eq!(parsed.coefficient(), coefficient);
            assert_eq!(parsed.encode(), value.encode());
            assert_eq!(parsed.to_string(), text);
        }
        assert_eq!(
            CanonicalDecimal::parse_canonical("0.000000000000000001")
                .unwrap()
                .coefficient(),
            1
        );
        assert_eq!(
            CanonicalDecimal::parse_canonical("-0.000000000000000001")
                .unwrap()
                .coefficient(),
            -1
        );
        assert_eq!(
            CanonicalDecimal::from_coefficient(MIN_DECIMAL_COEFFICIENT)
                .unwrap()
                .to_string()
                .len(),
            MAX_DECIMAL_TEXT_BYTES
        );
    }

    #[test]
    fn alternate_scales_and_aliases_are_not_silently_normalized() {
        for text in [
            "",
            "-",
            ".000000000000000000",
            "1",
            "1.",
            "1.0",
            "1.00000000000000000",
            "1.0000000000000000000",
            "+1.000000000000000000",
            " 1.000000000000000000",
            "1.000000000000000000 ",
            "1.00000000000000000e0",
            "1_0.000000000000000000",
            "1,000000000000000000",
            "1.000000000.00000000",
            "NaN",
            "Infinity",
            "١.000000000000000000",
            "１.000000000000000000",
            "1.00000000000000000\0",
        ] {
            assert!(
                CanonicalDecimal::parse_canonical(text).is_err(),
                "accepted {text:?}"
            );
        }
        for text in [
            "00.000000000000000000",
            "01.230000000000000000",
            "-0.000000000000000000",
        ] {
            assert_eq!(
                CanonicalDecimal::parse_canonical(text),
                Err(DecimalParseError::NonCanonical)
            );
        }
    }

    #[test]
    fn oversized_input_and_values_outside_the_profile_refuse_without_rounding() {
        assert_eq!(
            CanonicalDecimal::parse_canonical("10000000000000000.000000000000000000"),
            Err(DecimalParseError::OutOfRange),
        );
        assert_eq!(
            CanonicalDecimal::parse_canonical(&"9".repeat(MAX_DECIMAL_TEXT_BYTES + 1)),
            Err(DecimalParseError::TooLong),
        );
        // The extra final zero is still an inadmissible scale, not permission
        // to call a rounding constructor. The neighbouring quantum is exact.
        assert!(CanonicalDecimal::parse_canonical("0.0000000000000000001").is_err());
        assert!(CanonicalDecimal::parse_canonical("0.0000000000000000010").is_err());
        for error in [
            DecimalParseError::TooLong,
            DecimalParseError::InvalidSyntax,
            DecimalParseError::NonCanonical,
            DecimalParseError::OutOfRange,
        ] {
            assert!(!error.to_string().contains("10000000000000000"));
        }
    }

    #[test]
    fn broad_deterministic_sample_has_one_text_and_one_binary_identity() {
        let mut state = 0xa076_1d64_78bd_642f_e703_7ed1_a0b4_28db_u128;
        let bound = MAX_DECIMAL_COEFFICIENT as u128 + 1;
        for _ in 0..2_000 {
            state = state
                .wrapping_mul(0xda94_2042_e4dd_58b5)
                .wrapping_add(1)
                .rotate_left(29);
            let magnitude = (state % bound) as i128;
            let coefficient = if state >> 127 == 0 {
                magnitude
            } else {
                -magnitude
            };
            let value = CanonicalDecimal::from_coefficient(coefficient).unwrap();
            let parsed = CanonicalDecimal::parse_canonical(&value.to_string()).unwrap();
            assert_eq!(parsed, value);
            assert_eq!(CanonicalDecimal::decode(&parsed.encode()).unwrap(), value);
        }
    }
}
