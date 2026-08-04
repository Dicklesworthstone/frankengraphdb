//! Exact integer weights for Z-set deltas.
//!
//! [`ZWeight`] keeps values that fit in `i128` inline. An operation that
//! would overflow that representation is retried exactly with
//! [`fgdb_bigint::BigInt`] under the caller's [`LimbLimit`]. Results are
//! immediately demoted when they fit in `i128` again, so every value built
//! through the public API has one canonical representation.
//!
//! This module defines an in-memory arithmetic value, not a durable encoding.
//! Durable tags and bytes remain owned by the generated-format layer.

use std::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
};

use fgdb_bigint::{ArithmeticError, BigInt, LimbLimit};

/// A resource or arithmetic failure from an exact weight operation.
///
/// The underlying error names the failed operation and the exact required
/// and admitted limb counts. `ZWeight` deliberately has no wrapping,
/// saturating, or approximate fallback.
pub type ZWeightError = ArithmeticError;

/// An exact integer Z-set weight with a checked `i128` fast representation.
///
/// Arithmetic is fallible only when the explicit limb budget cannot admit an
/// exact promoted result (or the allocation itself fails). Native `i128`
/// overflow is never observable: the operation promotes before producing a
/// value.
pub struct ZWeight {
    repr: Repr,
}

enum Repr {
    Fast(i128),
    Promoted(BigInt),
}

impl ZWeight {
    /// The additive identity.
    pub const ZERO: Self = Self::from_i128(0);

    /// The multiplicative identity.
    pub const ONE: Self = Self::from_i128(1);

    /// Constructs an inline exact weight.
    pub const fn from_i128(value: i128) -> Self {
        Self {
            repr: Repr::Fast(value),
        }
    }

    /// Takes ownership of a bounded canonical bigint and normalizes it into a
    /// weight.
    ///
    /// The value is demoted when it fits in `i128`. Otherwise its existing
    /// allocation is transferred without copying. Constructing the bigint is
    /// where the caller must have enforced its [`LimbLimit`].
    pub fn from_bigint(value: BigInt) -> Self {
        match value.to_i128() {
            Some(inline) => Self::from_i128(inline),
            None => Self {
                repr: Repr::Promoted(value),
            },
        }
    }

    /// Returns the inline value when this exact integer fits in `i128`.
    pub fn to_i128(&self) -> Option<i128> {
        match &self.repr {
            Repr::Fast(value) => Some(*value),
            Repr::Promoted(value) => value.to_i128(),
        }
    }

    /// True when the value currently needs the signed-limb representation.
    pub fn is_promoted(&self) -> bool {
        matches!(self.repr, Repr::Promoted(_))
    }

    /// True when this is the additive identity.
    pub fn is_zero(&self) -> bool {
        self.signum() == 0
    }

    /// Number of canonical base-2^64 magnitude limbs.
    ///
    /// Inline values report their logical magnitude length, not an allocation
    /// size. This makes the metric independent of the storage representation.
    pub fn magnitude_limb_count(&self) -> usize {
        match &self.repr {
            Repr::Fast(value) => {
                let magnitude = value.unsigned_abs();
                if magnitude == 0 {
                    0
                } else if magnitude >> 64 == 0 {
                    1
                } else {
                    2
                }
            }
            Repr::Promoted(value) => value.limb_count(),
        }
    }

    /// Verifies the canonical storage invariant.
    ///
    /// All publicly constructed values satisfy this: inline exactly when the
    /// value fits `i128`, otherwise a canonical promoted integer.
    pub fn is_canonical(&self) -> bool {
        match &self.repr {
            Repr::Fast(_) => true,
            Repr::Promoted(value) => value.is_canonical() && value.to_i128().is_none(),
        }
    }

    /// Copies this value under an explicit promoted-limb budget.
    ///
    /// Inline values allocate nothing and therefore succeed under a zero-limb
    /// budget.
    pub fn checked_clone(&self, limit: LimbLimit) -> Result<Self, ZWeightError> {
        match &self.repr {
            Repr::Fast(value) => Ok(Self::from_i128(*value)),
            Repr::Promoted(value) => value.checked_clone(limit).map(Self::from_bigint),
        }
    }

    /// Exact addition in the integer group.
    pub fn checked_add(&self, other: &Self, limit: LimbLimit) -> Result<Self, ZWeightError> {
        match (&self.repr, &other.repr) {
            (Repr::Fast(left), Repr::Fast(right)) => {
                if let Some(result) = left.checked_add(*right) {
                    return Ok(Self::from_i128(result));
                }
                BigInt::from_i128(*left)
                    .checked_add(&BigInt::from_i128(*right), limit)
                    .map(Self::from_bigint)
            }
            (Repr::Fast(left), Repr::Promoted(right)) => BigInt::from_i128(*left)
                .checked_add(right, limit)
                .map(Self::from_bigint),
            (Repr::Promoted(left), Repr::Fast(right)) => left
                .checked_add(&BigInt::from_i128(*right), limit)
                .map(Self::from_bigint),
            (Repr::Promoted(left), Repr::Promoted(right)) => {
                left.checked_add(right, limit).map(Self::from_bigint)
            }
        }
    }

    /// Exact subtraction in the integer group.
    pub fn checked_sub(&self, other: &Self, limit: LimbLimit) -> Result<Self, ZWeightError> {
        match (&self.repr, &other.repr) {
            (Repr::Fast(left), Repr::Fast(right)) => {
                if let Some(result) = left.checked_sub(*right) {
                    return Ok(Self::from_i128(result));
                }
                BigInt::from_i128(*left)
                    .checked_sub(&BigInt::from_i128(*right), limit)
                    .map(Self::from_bigint)
            }
            (Repr::Fast(left), Repr::Promoted(right)) => BigInt::from_i128(*left)
                .checked_sub(right, limit)
                .map(Self::from_bigint),
            (Repr::Promoted(left), Repr::Fast(right)) => left
                .checked_sub(&BigInt::from_i128(*right), limit)
                .map(Self::from_bigint),
            (Repr::Promoted(left), Repr::Promoted(right)) => {
                left.checked_sub(right, limit).map(Self::from_bigint)
            }
        }
    }

    /// Exact additive inverse.
    pub fn checked_neg(&self, limit: LimbLimit) -> Result<Self, ZWeightError> {
        match &self.repr {
            Repr::Fast(value) => match value.checked_neg() {
                Some(result) => Ok(Self::from_i128(result)),
                None => BigInt::from_i128(*value)
                    .checked_neg(limit)
                    .map(Self::from_bigint),
            },
            Repr::Promoted(value) => value.checked_neg(limit).map(Self::from_bigint),
        }
    }

    /// Exact multiplication of two integer weights.
    pub fn checked_mul(&self, other: &Self, limit: LimbLimit) -> Result<Self, ZWeightError> {
        match (&self.repr, &other.repr) {
            (Repr::Fast(left), Repr::Fast(right)) => {
                if let Some(result) = left.checked_mul(*right) {
                    return Ok(Self::from_i128(result));
                }
                BigInt::from_i128(*left)
                    .checked_mul(&BigInt::from_i128(*right), limit)
                    .map(Self::from_bigint)
            }
            (Repr::Fast(left), Repr::Promoted(right)) => BigInt::from_i128(*left)
                .checked_mul(right, limit)
                .map(Self::from_bigint),
            (Repr::Promoted(left), Repr::Fast(right)) => left
                .checked_mul(&BigInt::from_i128(*right), limit)
                .map(Self::from_bigint),
            (Repr::Promoted(left), Repr::Promoted(right)) => {
                left.checked_mul(right, limit).map(Self::from_bigint)
            }
        }
    }

    /// Exact multiplication by an inline integer weight.
    pub fn checked_mul_i128(&self, factor: i128, limit: LimbLimit) -> Result<Self, ZWeightError> {
        self.checked_mul(&Self::from_i128(factor), limit)
    }

    fn signum(&self) -> i8 {
        match &self.repr {
            Repr::Fast(value) => match value.cmp(&0) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            },
            Repr::Promoted(value) => {
                if value.is_zero() {
                    0
                } else if value.is_negative() {
                    -1
                } else {
                    1
                }
            }
        }
    }

    fn magnitude_limb(&self, index: usize) -> u64 {
        match &self.repr {
            Repr::Fast(value) => {
                let magnitude = value.unsigned_abs();
                match index {
                    0 => magnitude as u64,
                    1 => (magnitude >> 64) as u64,
                    _ => 0,
                }
            }
            Repr::Promoted(value) => value.magnitude_limbs_le().get(index).copied().unwrap_or(0),
        }
    }

    fn cmp_magnitude(&self, other: &Self) -> Ordering {
        let left_len = self.magnitude_limb_count();
        let right_len = other.magnitude_limb_count();
        match left_len.cmp(&right_len) {
            Ordering::Equal => {}
            different => return different,
        }
        for index in (0..left_len).rev() {
            match self.magnitude_limb(index).cmp(&other.magnitude_limb(index)) {
                Ordering::Equal => {}
                different => return different,
            }
        }
        Ordering::Equal
    }
}

impl Default for ZWeight {
    fn default() -> Self {
        Self::ZERO
    }
}

impl From<i128> for ZWeight {
    fn from(value: i128) -> Self {
        Self::from_i128(value)
    }
}

impl From<BigInt> for ZWeight {
    fn from(value: BigInt) -> Self {
        Self::from_bigint(value)
    }
}

impl PartialEq for ZWeight {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ZWeight {}

impl PartialOrd for ZWeight {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ZWeight {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.signum().cmp(&other.signum()) {
            Ordering::Equal => {}
            different => return different,
        }
        if self.signum() < 0 {
            other.cmp_magnitude(self)
        } else {
            self.cmp_magnitude(other)
        }
    }
}

impl Hash for ZWeight {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.signum().hash(state);
        self.magnitude_limb_count().hash(state);
        for index in 0..self.magnitude_limb_count() {
            self.magnitude_limb(index).hash(state);
        }
    }
}

impl fmt::Debug for ZWeight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Magnitude<'a>(&'a ZWeight);

        impl fmt::Debug for Magnitude<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let mut list = f.debug_list();
                for index in 0..self.0.magnitude_limb_count() {
                    list.entry(&self.0.magnitude_limb(index));
                }
                list.finish()
            }
        }

        f.debug_struct("ZWeight")
            .field("signum", &self.signum())
            .field("magnitude_limbs_le", &Magnitude(self))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;

    use fgdb_bigint::ArithmeticOperation;

    use super::*;

    const TEST_LIMIT: LimbLimit = LimbLimit::new(16);

    fn hash_of(value: &ZWeight) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    fn assert_canonical(value: &ZWeight) {
        assert!(value.is_canonical(), "noncanonical result: {value:?}");
        assert_eq!(
            value.is_promoted(),
            value.to_i128().is_none(),
            "storage does not match the numeric range: {value:?}"
        );
    }

    fn promoted_positive_boundary() -> ZWeight {
        ZWeight::from_i128(i128::MAX)
            .checked_add(&ZWeight::ONE, LimbLimit::new(2))
            .expect("2^127 needs exactly two limbs")
    }

    fn promoted_negative_boundary() -> ZWeight {
        ZWeight::from_i128(i128::MIN)
            .checked_sub(&ZWeight::ONE, LimbLimit::new(2))
            .expect("-2^127 - 1 needs two limbs")
    }

    #[test]
    fn inline_construction_and_identities_are_canonical() {
        for value in [i128::MIN, -1, 0, 1, i128::MAX] {
            let weight = ZWeight::from_i128(value);
            assert_eq!(weight.to_i128(), Some(value));
            assert!(!weight.is_promoted());
            assert_canonical(&weight);
        }
        assert!(ZWeight::ZERO.is_zero());
        assert_eq!(ZWeight::default(), ZWeight::ZERO);
    }

    #[test]
    fn addition_promotes_before_overflow_and_demotes_after_cancellation() {
        let promoted = promoted_positive_boundary();
        assert!(promoted.is_promoted());
        assert_eq!(promoted.to_i128(), None);
        assert!(promoted > ZWeight::from_i128(i128::MAX));
        assert_canonical(&promoted);

        let demoted = promoted
            .checked_sub(&ZWeight::ONE, LimbLimit::new(2))
            .expect("the two-limb intermediate is admitted");
        assert_eq!(demoted.to_i128(), Some(i128::MAX));
        assert!(!demoted.is_promoted());
        assert_canonical(&demoted);
    }

    #[test]
    fn subtraction_promotes_below_min_and_demotes_back_to_min() {
        let promoted = promoted_negative_boundary();
        assert!(promoted < ZWeight::from_i128(i128::MIN));
        assert_canonical(&promoted);

        let demoted = promoted
            .checked_add(&ZWeight::ONE, LimbLimit::new(2))
            .expect("the two-limb intermediate is admitted");
        assert_eq!(demoted.to_i128(), Some(i128::MIN));
        assert_canonical(&demoted);
    }

    #[test]
    fn negation_crosses_both_sides_of_the_i128_boundary() {
        let positive = ZWeight::from_i128(i128::MIN)
            .checked_neg(LimbLimit::new(2))
            .expect("abs(i128::MIN) needs two limbs");
        assert!(positive.is_promoted());
        assert!(positive > ZWeight::from_i128(i128::MAX));

        let min_again = positive
            .checked_neg(LimbLimit::new(2))
            .expect("negation retains two limbs then demotes");
        assert_eq!(min_again.to_i128(), Some(i128::MIN));
        assert_canonical(&min_again);

        let below_min = promoted_negative_boundary();
        let above_max = below_min
            .checked_neg(LimbLimit::new(2))
            .expect("negation preserves the exact magnitude");
        assert!(above_max > ZWeight::from_i128(i128::MAX));
        assert_canonical(&above_max);
    }

    #[test]
    fn multiplication_is_exact_and_zero_demotes_without_a_limb_budget() {
        let product = ZWeight::from_i128(i128::MAX)
            .checked_mul_i128(2, LimbLimit::new(2))
            .expect("the product needs two magnitude limbs");
        assert!(product.is_promoted());
        assert_canonical(&product);

        let inverse_product = ZWeight::from_i128(-i128::MAX)
            .checked_mul_i128(-2, LimbLimit::new(2))
            .expect("equal exact product");
        assert_eq!(product, inverse_product);

        let zero = product
            .checked_mul(&ZWeight::ZERO, LimbLimit::new(0))
            .expect("zero product has no magnitude limbs");
        assert_eq!(zero, ZWeight::ZERO);
        assert_canonical(&zero);
    }

    #[test]
    fn limb_limits_fail_with_exact_operation_and_requirement() {
        assert_eq!(
            ZWeight::from_i128(i128::MAX).checked_add(&ZWeight::ONE, LimbLimit::new(1)),
            Err(ZWeightError::LimbLimitExceeded {
                operation: ArithmeticOperation::Add,
                required_limbs: 2,
                limit: 1,
            })
        );
        assert_eq!(
            ZWeight::from_i128(i128::MIN).checked_neg(LimbLimit::new(1)),
            Err(ZWeightError::LimbLimitExceeded {
                operation: ArithmeticOperation::Negate,
                required_limbs: 2,
                limit: 1,
            })
        );

        let two_to_127 = promoted_positive_boundary();
        assert_eq!(
            two_to_127.checked_mul(&two_to_127, LimbLimit::new(3)),
            Err(ZWeightError::LimbLimitExceeded {
                operation: ArithmeticOperation::Multiply,
                required_limbs: 4,
                limit: 3,
            })
        );
        assert_eq!(
            two_to_127.checked_clone(LimbLimit::new(1)),
            Err(ZWeightError::LimbLimitExceeded {
                operation: ArithmeticOperation::Clone,
                required_limbs: 2,
                limit: 1,
            })
        );
    }

    #[test]
    fn equality_order_and_hash_are_numeric_not_variant_based() {
        let canonical = ZWeight::from_i128(42);
        // The private invalid shape is constructed only to prove the trait
        // implementations do not accidentally hash or compare the enum tag.
        let alternate = ZWeight {
            repr: Repr::Promoted(BigInt::from_i128(42)),
        };
        assert!(!alternate.is_canonical());
        assert_eq!(canonical, alternate);
        assert_eq!(canonical.cmp(&alternate), Ordering::Equal);
        assert_eq!(hash_of(&canonical), hash_of(&alternate));

        let negative = ZWeight::from_i128(-42);
        let negative_alternate = ZWeight {
            repr: Repr::Promoted(BigInt::from_i128(-42)),
        };
        assert_eq!(negative, negative_alternate);
        assert_eq!(hash_of(&negative), hash_of(&negative_alternate));
    }

    #[test]
    fn importing_bigints_always_demotes_when_possible() {
        let inline = ZWeight::from_bigint(BigInt::from_i128(i128::MIN));
        assert_eq!(inline.to_i128(), Some(i128::MIN));
        assert!(!inline.is_promoted());

        let promoted = promoted_positive_boundary();
        assert!(promoted.is_promoted());
        assert_canonical(&promoted);
    }

    #[test]
    fn additive_group_laws_hold_across_storage_boundaries() {
        let values = [
            ZWeight::from_i128(i128::MIN),
            ZWeight::from_i128(-1),
            ZWeight::ZERO,
            ZWeight::ONE,
            ZWeight::from_i128(i128::MAX),
            promoted_positive_boundary(),
            promoted_negative_boundary(),
        ];

        for value in &values {
            let inverse = value.checked_neg(TEST_LIMIT).expect("inverse");
            assert_eq!(
                value.checked_add(&inverse, LimbLimit::new(0)),
                Ok(ZWeight::ZERO),
                "inverse law failed for {value:?}"
            );
            assert_eq!(
                value.checked_add(&ZWeight::ZERO, TEST_LIMIT),
                value.checked_clone(TEST_LIMIT),
                "right identity failed for {value:?}"
            );
            assert_eq!(
                ZWeight::ZERO.checked_add(value, TEST_LIMIT),
                value.checked_clone(TEST_LIMIT),
                "left identity failed for {value:?}"
            );
        }

        for left in &values {
            for right in &values {
                assert_eq!(
                    left.checked_add(right, TEST_LIMIT),
                    right.checked_add(left, TEST_LIMIT),
                    "commutativity failed for {left:?}, {right:?}"
                );
                for third in &values {
                    let left_grouped = left
                        .checked_add(right, TEST_LIMIT)
                        .and_then(|sum| sum.checked_add(third, TEST_LIMIT));
                    let right_grouped = right
                        .checked_add(third, TEST_LIMIT)
                        .and_then(|sum| left.checked_add(&sum, TEST_LIMIT));
                    assert_eq!(
                        left_grouped, right_grouped,
                        "associativity failed for {left:?}, {right:?}, {third:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn multiplication_is_commutative_and_distributes_over_addition() {
        let values = [
            ZWeight::from_i128(-2),
            ZWeight::from_i128(-1),
            ZWeight::ZERO,
            ZWeight::ONE,
            ZWeight::from_i128(2),
            promoted_positive_boundary(),
            promoted_negative_boundary(),
        ];

        for left in &values {
            for right in &values {
                assert_eq!(
                    left.checked_mul(right, TEST_LIMIT),
                    right.checked_mul(left, TEST_LIMIT),
                    "multiplication commutativity failed for {left:?}, {right:?}"
                );
                for third in &values {
                    let sum = right.checked_add(third, TEST_LIMIT).expect("sum");
                    let lhs = left.checked_mul(&sum, TEST_LIMIT);
                    let rhs = left.checked_mul(right, TEST_LIMIT).and_then(|first| {
                        left.checked_mul(third, TEST_LIMIT)
                            .and_then(|second| first.checked_add(&second, TEST_LIMIT))
                    });
                    assert_eq!(
                        lhs, rhs,
                        "distributivity failed for {left:?}, {right:?}, {third:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn native_i128_domain_matches_checked_arithmetic_exactly() {
        let values = [
            i128::MIN,
            i128::MIN + 1,
            -(1i128 << 64),
            -1,
            0,
            1,
            1i128 << 64,
            i128::MAX - 1,
            i128::MAX,
        ];

        for left in values {
            for right in values {
                let left_weight = ZWeight::from_i128(left);
                let right_weight = ZWeight::from_i128(right);

                let sum = left_weight
                    .checked_add(&right_weight, TEST_LIMIT)
                    .expect("exact sum");
                assert_canonical(&sum);
                if let Some(expected) = left.checked_add(right) {
                    assert_eq!(sum.to_i128(), Some(expected));
                } else {
                    assert!(sum.is_promoted());
                }

                let difference = left_weight
                    .checked_sub(&right_weight, TEST_LIMIT)
                    .expect("exact difference");
                assert_canonical(&difference);
                if let Some(expected) = left.checked_sub(right) {
                    assert_eq!(difference.to_i128(), Some(expected));
                } else {
                    assert!(difference.is_promoted());
                }

                let product = left_weight
                    .checked_mul(&right_weight, TEST_LIMIT)
                    .expect("exact product");
                assert_canonical(&product);
                if let Some(expected) = left.checked_mul(right) {
                    assert_eq!(product.to_i128(), Some(expected));
                } else {
                    assert!(product.is_promoted());
                }
            }
        }
    }
}
