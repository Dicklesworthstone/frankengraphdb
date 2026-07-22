//! Canonical fixed-scale decimals for the `STRICT_PORTABLE` scalar profile.
//!
//! A decimal has exactly one in-memory form: a signed 34-digit coefficient at
//! scale 18.  There is no stored per-value scale, so numerically equal inputs
//! normalize to the same coefficient before equality, hashing, ordering, or
//! encoding can observe them.  Rescaling is always explicit and uses
//! round-half-to-even; every overflow is rejected.

use std::fmt;

/// Decimal precision fixed by this `STRICT_PORTABLE` profile.
pub const STRICT_PORTABLE_DECIMAL_PRECISION: u32 = 34;

/// Decimal scale fixed by this `STRICT_PORTABLE` profile.
pub const STRICT_PORTABLE_DECIMAL_SCALE: u32 = 18;

/// Largest source or target scale accepted by profile rescaling.
pub const STRICT_PORTABLE_MAX_DECIMAL_SCALE: u32 = 38;

/// A canonical decimal payload is exactly one little-endian `i128`.
pub const DECIMAL_ENCODED_LEN: usize = size_of::<i128>();

/// Greatest canonical coefficient (the 34 significant digits of Decimal128).
pub const MAX_DECIMAL_COEFFICIENT: i128 = 9_999_999_999_999_999_999_999_999_999_999_999;

/// Smallest canonical coefficient.  The range is deliberately symmetric.
pub const MIN_DECIMAL_COEFFICIENT: i128 = -MAX_DECIMAL_COEFFICIENT;

/// A fixed-scale decimal under the `STRICT_PORTABLE` profile.
///
/// Its numeric value is `coefficient * 10^-18`.  The private field prevents
/// out-of-profile coefficients from entering the canonical value domain.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct CanonicalDecimal {
    coefficient: i128,
}

impl CanonicalDecimal {
    /// Constructs a value from an already scale-18 coefficient.
    pub fn from_coefficient(coefficient: i128) -> Result<Self, DecimalError> {
        validate_coefficient(coefficient)?;
        Ok(Self { coefficient })
    }

    /// Converts `coefficient * 10^-source_scale` into the canonical scale.
    ///
    /// Discarded digits are rounded to nearest with ties going to the even
    /// canonical coefficient.  Source and result precision are both checked;
    /// the function never saturates or wraps.
    pub fn from_scaled_half_even(
        coefficient: i128,
        source_scale: u32,
    ) -> Result<Self, DecimalError> {
        validate_scale(source_scale)?;
        validate_coefficient(coefficient)?;
        let canonical =
            rescale_half_even(coefficient, source_scale, STRICT_PORTABLE_DECIMAL_SCALE)?;
        Self::from_coefficient(canonical)
    }

    /// Constructs an exact integral decimal.
    pub fn from_integer(integer: i128) -> Result<Self, DecimalError> {
        Self::from_scaled_half_even(integer, 0)
    }

    /// Returns the canonical scale-18 coefficient.
    pub const fn coefficient(self) -> i128 {
        self.coefficient
    }

    /// Returns the profile's fixed scale.
    pub const fn scale() -> u32 {
        STRICT_PORTABLE_DECIMAL_SCALE
    }

    /// Checked addition in the canonical coefficient domain.
    pub fn checked_add(self, rhs: Self) -> Result<Self, DecimalError> {
        let coefficient = self.coefficient.checked_add(rhs.coefficient).ok_or(
            DecimalError::ArithmeticOverflow {
                operation: DecimalOperation::Add,
            },
        )?;
        Self::from_coefficient(coefficient)
    }

    /// Checked subtraction in the canonical coefficient domain.
    pub fn checked_sub(self, rhs: Self) -> Result<Self, DecimalError> {
        let coefficient = self.coefficient.checked_sub(rhs.coefficient).ok_or(
            DecimalError::ArithmeticOverflow {
                operation: DecimalOperation::Subtract,
            },
        )?;
        Self::from_coefficient(coefficient)
    }

    /// Checked negation in the canonical coefficient domain.
    pub fn checked_neg(self) -> Result<Self, DecimalError> {
        let coefficient =
            self.coefficient
                .checked_neg()
                .ok_or(DecimalError::ArithmeticOverflow {
                    operation: DecimalOperation::Negate,
                })?;
        Self::from_coefficient(coefficient)
    }

    /// Returns the unique fixed-width canonical payload.
    pub const fn encode(self) -> [u8; DECIMAL_ENCODED_LEN] {
        self.coefficient.to_le_bytes()
    }

    /// Decodes one complete canonical payload.
    ///
    /// The exact fixed-width bound is checked before copying any bytes.  This
    /// decoder performs no allocation, so attacker-controlled lengths cannot
    /// influence memory consumption.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecimalDecodeError> {
        if bytes.len() != DECIMAL_ENCODED_LEN {
            return Err(DecimalDecodeError::WrongLength {
                expected: DECIMAL_ENCODED_LEN,
                got: bytes.len(),
            });
        }

        let raw: &[u8; DECIMAL_ENCODED_LEN] =
            bytes
                .try_into()
                .map_err(|_| DecimalDecodeError::WrongLength {
                    expected: DECIMAL_ENCODED_LEN,
                    got: bytes.len(),
                })?;
        let coefficient = i128::from_le_bytes(*raw);
        Self::from_coefficient(coefficient)
            .map_err(|_| DecimalDecodeError::CoefficientOutOfRange { coefficient })
    }
}

impl fmt::Display for CanonicalDecimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let magnitude = self.coefficient.unsigned_abs();
        let unit = 10_u128.pow(STRICT_PORTABLE_DECIMAL_SCALE);
        let whole = magnitude / unit;
        let fraction = magnitude % unit;
        if self.coefficient < 0 {
            f.write_str("-")?;
        }
        write!(
            f,
            "{whole}.{fraction:0width$}",
            width = STRICT_PORTABLE_DECIMAL_SCALE as usize
        )
    }
}

/// Rescales a signed coefficient using deterministic round-half-to-even.
///
/// Both scales are bounded before exponentiation.  Scaling up uses checked
/// multiplication.  Scaling down works over an unsigned magnitude so
/// `i128::MIN` is handled without negation or absolute-value overflow.
pub fn rescale_half_even(
    coefficient: i128,
    source_scale: u32,
    target_scale: u32,
) -> Result<i128, DecimalError> {
    validate_scale(source_scale)?;
    validate_scale(target_scale)?;

    if source_scale == target_scale {
        return Ok(coefficient);
    }

    if source_scale < target_scale {
        let scale_delta = target_scale - source_scale;
        let factor = 10_i128
            .checked_pow(scale_delta)
            .ok_or(DecimalError::ArithmeticOverflow {
                operation: DecimalOperation::IncreaseScale,
            })?;
        return coefficient
            .checked_mul(factor)
            .ok_or(DecimalError::ArithmeticOverflow {
                operation: DecimalOperation::IncreaseScale,
            });
    }

    let scale_delta = source_scale - target_scale;
    let divisor = 10_u128
        .checked_pow(scale_delta)
        .ok_or(DecimalError::ArithmeticOverflow {
            operation: DecimalOperation::DecreaseScale,
        })?;
    let magnitude = coefficient.unsigned_abs();
    let quotient = magnitude / divisor;
    let remainder = magnitude % divisor;

    let twice_remainder = remainder
        .checked_mul(2)
        .ok_or(DecimalError::ArithmeticOverflow {
            operation: DecimalOperation::DecreaseScale,
        })?;
    let increment = twice_remainder > divisor || (twice_remainder == divisor && quotient % 2 == 1);
    let rounded =
        quotient
            .checked_add(u128::from(increment))
            .ok_or(DecimalError::ArithmeticOverflow {
                operation: DecimalOperation::DecreaseScale,
            })?;

    signed_from_magnitude(rounded, coefficient.is_negative())
}

fn validate_scale(scale: u32) -> Result<(), DecimalError> {
    if scale > STRICT_PORTABLE_MAX_DECIMAL_SCALE {
        return Err(DecimalError::ScaleOutOfRange {
            scale,
            max: STRICT_PORTABLE_MAX_DECIMAL_SCALE,
        });
    }
    Ok(())
}

fn validate_coefficient(coefficient: i128) -> Result<(), DecimalError> {
    if !(MIN_DECIMAL_COEFFICIENT..=MAX_DECIMAL_COEFFICIENT).contains(&coefficient) {
        return Err(DecimalError::CoefficientOutOfRange {
            coefficient,
            precision: STRICT_PORTABLE_DECIMAL_PRECISION,
        });
    }
    Ok(())
}

fn signed_from_magnitude(magnitude: u128, negative: bool) -> Result<i128, DecimalError> {
    const I128_MIN_MAGNITUDE: u128 = (i128::MAX as u128) + 1;

    if negative && magnitude == I128_MIN_MAGNITUDE {
        return Ok(i128::MIN);
    }

    let signed = i128::try_from(magnitude).map_err(|_| DecimalError::ArithmeticOverflow {
        operation: DecimalOperation::DecreaseScale,
    })?;
    if negative {
        signed
            .checked_neg()
            .ok_or(DecimalError::ArithmeticOverflow {
                operation: DecimalOperation::DecreaseScale,
            })
    } else {
        Ok(signed)
    }
}

/// Arithmetic stage that rejected instead of wrapping or saturating.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecimalOperation {
    IncreaseScale,
    DecreaseScale,
    Add,
    Subtract,
    Negate,
}

impl fmt::Display for DecimalOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::IncreaseScale => "scale increase",
            Self::DecreaseScale => "scale decrease",
            Self::Add => "addition",
            Self::Subtract => "subtraction",
            Self::Negate => "negation",
        };
        f.write_str(name)
    }
}

/// Typed rejection from decimal construction or arithmetic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecimalError {
    ScaleOutOfRange { scale: u32, max: u32 },
    CoefficientOutOfRange { coefficient: i128, precision: u32 },
    ArithmeticOverflow { operation: DecimalOperation },
}

impl fmt::Display for DecimalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScaleOutOfRange { scale, max } => {
                write!(f, "decimal scale {scale} exceeds profile maximum {max}")
            }
            Self::CoefficientOutOfRange {
                coefficient,
                precision,
            } => write!(
                f,
                "decimal coefficient {coefficient} exceeds profile precision {precision}"
            ),
            Self::ArithmeticOverflow { operation } => {
                write!(f, "decimal {operation} overflow")
            }
        }
    }
}

impl std::error::Error for DecimalError {}

/// Typed rejection from [`CanonicalDecimal::decode`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecimalDecodeError {
    WrongLength { expected: usize, got: usize },
    CoefficientOutOfRange { coefficient: i128 },
}

impl fmt::Display for DecimalDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength { expected, got } => {
                write!(f, "decimal payload length {got}, expected {expected}")
            }
            Self::CoefficientOutOfRange { coefficient } => write!(
                f,
                "encoded decimal coefficient {coefficient} is outside the canonical range"
            ),
        }
    }
}

impl std::error::Error for DecimalDecodeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn decimal(coefficient: i128) -> Result<CanonicalDecimal, DecimalError> {
        CanonicalDecimal::from_coefficient(coefficient)
    }

    fn hash_of(value: CanonicalDecimal) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn differently_scaled_inputs_normalize_to_one_value() -> Result<(), DecimalError> {
        let a = CanonicalDecimal::from_scaled_half_even(123, 2)?;
        let b = CanonicalDecimal::from_scaled_half_even(1_230, 3)?;
        let c = CanonicalDecimal::from_scaled_half_even(12_300, 4)?;

        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(hash_of(a), hash_of(b));
        assert_eq!(a.encode(), c.encode());
        assert_eq!(a.to_string(), "1.230000000000000000");
        assert_eq!(decimal(0)?.to_string(), "0.000000000000000000");
        Ok(())
    }

    #[test]
    fn rescaling_uses_half_even_for_both_signs() -> Result<(), DecimalError> {
        let target = STRICT_PORTABLE_DECIMAL_SCALE;
        let source = target + 1;

        for (input, expected) in [
            (14, 1),
            (15, 2),
            (16, 2),
            (24, 2),
            (25, 2),
            (26, 3),
            (35, 4),
            (-14, -1),
            (-15, -2),
            (-16, -2),
            (-24, -2),
            (-25, -2),
            (-26, -3),
            (-35, -4),
        ] {
            let got = CanonicalDecimal::from_scaled_half_even(input, source)?;
            assert_eq!(got.coefficient(), expected, "input={input}");
        }

        // A tie can occur behind more than one discarded digit as well.
        assert_eq!(rescale_half_even(150, 20, 18), Ok(2));
        assert_eq!(rescale_half_even(250, 20, 18), Ok(2));
        assert_eq!(rescale_half_even(-350, 20, 18), Ok(-4));
        Ok(())
    }

    #[test]
    fn overflow_and_out_of_profile_values_are_rejected() -> Result<(), DecimalError> {
        assert_eq!(
            CanonicalDecimal::from_coefficient(MAX_DECIMAL_COEFFICIENT + 1),
            Err(DecimalError::CoefficientOutOfRange {
                coefficient: MAX_DECIMAL_COEFFICIENT + 1,
                precision: STRICT_PORTABLE_DECIMAL_PRECISION,
            })
        );
        assert_eq!(
            CanonicalDecimal::from_scaled_half_even(1, STRICT_PORTABLE_MAX_DECIMAL_SCALE + 1),
            Err(DecimalError::ScaleOutOfRange {
                scale: STRICT_PORTABLE_MAX_DECIMAL_SCALE + 1,
                max: STRICT_PORTABLE_MAX_DECIMAL_SCALE,
            })
        );
        assert_eq!(
            rescale_half_even(i128::MAX, 0, STRICT_PORTABLE_MAX_DECIMAL_SCALE),
            Err(DecimalError::ArithmeticOverflow {
                operation: DecimalOperation::IncreaseScale,
            })
        );

        let max = decimal(MAX_DECIMAL_COEFFICIENT)?;
        let min = decimal(MIN_DECIMAL_COEFFICIENT)?;
        let quantum = decimal(1)?;
        assert!(matches!(
            max.checked_add(quantum),
            Err(DecimalError::CoefficientOutOfRange { .. })
        ));
        assert!(matches!(
            min.checked_sub(quantum),
            Err(DecimalError::CoefficientOutOfRange { .. })
        ));
        Ok(())
    }

    #[test]
    fn coefficient_order_is_the_numeric_total_order() -> Result<(), DecimalError> {
        let mut values = [
            decimal(1)?,
            decimal(MIN_DECIMAL_COEFFICIENT)?,
            decimal(0)?,
            decimal(MAX_DECIMAL_COEFFICIENT)?,
            decimal(-1)?,
        ];
        values.sort();
        assert_eq!(
            values.map(CanonicalDecimal::coefficient),
            [MIN_DECIMAL_COEFFICIENT, -1, 0, 1, MAX_DECIMAL_COEFFICIENT]
        );

        for a in values {
            for b in values {
                assert_eq!(a.cmp(&b).reverse(), b.cmp(&a));
                if a == b {
                    assert_eq!(hash_of(a), hash_of(b));
                    assert_eq!(a.encode(), b.encode());
                }
            }
        }
        Ok(())
    }

    #[test]
    fn canonical_payload_round_trips() -> Result<(), DecimalError> {
        for coefficient in [
            MIN_DECIMAL_COEFFICIENT,
            -10_000_000_000_000_001,
            -1,
            0,
            1,
            10_000_000_000_000_001,
            MAX_DECIMAL_COEFFICIENT,
        ] {
            let value = decimal(coefficient)?;
            assert_eq!(CanonicalDecimal::decode(&value.encode()), Ok(value));
        }

        // Deterministic broad sample over valid coefficients.
        let mut state = 0xA076_1D64_78BD_642Fu64;
        for _ in 0..2_000 {
            state = state
                .wrapping_add(0x9E37_79B9_7F4A_7C15)
                .rotate_left(17)
                .wrapping_mul(0x94D0_49BB_1331_11EB);
            let coefficient = i128::from(i64::from_le_bytes(state.to_le_bytes()));
            let value = decimal(coefficient)?;
            assert_eq!(CanonicalDecimal::decode(&value.encode()), Ok(value));
        }
        Ok(())
    }

    #[test]
    fn malformed_payloads_are_typed_rejections() {
        assert_eq!(
            CanonicalDecimal::decode(&[]),
            Err(DecimalDecodeError::WrongLength {
                expected: DECIMAL_ENCODED_LEN,
                got: 0,
            })
        );
        assert_eq!(
            CanonicalDecimal::decode(&[0; DECIMAL_ENCODED_LEN - 1]),
            Err(DecimalDecodeError::WrongLength {
                expected: DECIMAL_ENCODED_LEN,
                got: DECIMAL_ENCODED_LEN - 1,
            })
        );
        assert_eq!(
            CanonicalDecimal::decode(&[0; DECIMAL_ENCODED_LEN + 1]),
            Err(DecimalDecodeError::WrongLength {
                expected: DECIMAL_ENCODED_LEN,
                got: DECIMAL_ENCODED_LEN + 1,
            })
        );

        let noncanonical = MAX_DECIMAL_COEFFICIENT + 1;
        assert_eq!(
            CanonicalDecimal::decode(&noncanonical.to_le_bytes()),
            Err(DecimalDecodeError::CoefficientOutOfRange {
                coefficient: noncanonical,
            })
        );
        assert_eq!(
            CanonicalDecimal::decode(&i128::MIN.to_le_bytes()),
            Err(DecimalDecodeError::CoefficientOutOfRange {
                coefficient: i128::MIN,
            })
        );
    }

    #[test]
    fn i128_min_scale_down_never_uses_panicking_abs_or_negation() {
        // The discarded remainder has magnitude 8, so nearest rounding moves
        // one unit farther from zero than Rust's truncating division.
        assert_eq!(rescale_half_even(i128::MIN, 1, 0), Ok((i128::MIN / 10) - 1));
    }
}
