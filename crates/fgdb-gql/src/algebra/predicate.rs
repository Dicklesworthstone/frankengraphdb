//! Bounded canonical scalar operands for the shared vertex predicate engine.
//!
//! The scalar union, equality, ordering and encoding remain fgdb_types-owned.
//! Query comparisons admit Int/Float pairs by exact numeric value, without
//! changing canonical storage identity or cross-kind ordering. Missing/null
//! and unrelated scalar kinds produce UNKNOWN. Within a kind, comparisons
//! retain STRICT_PORTABLE order, including canonical NaN/collation/time rules.

use super::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, IntegerComparison, VertexPredicate};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, ScalarEncodeError};
use std::cmp::Ordering;
use std::sync::Arc;

/// Definition admission bound, not an allocator-byte or execution-time limit.
/// Both the variable payload and complete canonical encoding must fit.
pub const MAX_SCALAR_PREDICATE_BYTES: usize = 65_536;

#[derive(Debug)]
pub enum ScalarPredicateError {
    LiteralTooLarge { limit: usize, observed: usize },
    Encoding(ScalarEncodeError),
}
impl core::fmt::Display for ScalarPredicateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::LiteralTooLarge { limit, observed } => write!(
                f,
                "scalar predicate literal uses {observed} bytes, limit {limit}"
            ),
            Self::Encoding(error) => write!(f, "scalar predicate encoding failed: {error}"),
        }
    }
}
impl core::error::Error for ScalarPredicateError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Encoding(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(PartialEq, Eq)]
struct ScalarOperand {
    value: CanonicalScalar,
    encoded: Box<[u8]>,
}

/// Immutable checked operand plus its exact canonical transcript. Encoding is
/// prepared fallibly once, never recreated by the executor or hashed in place
/// of value identity. Neither field is publicly mutable; Debug redacts values.
/// Cloning or changing the comparison shares the checked operand allocation.
#[derive(Clone, PartialEq, Eq)]
pub struct ScalarPredicate {
    operand: Arc<ScalarOperand>,
    comparison: IntegerComparison,
}
impl core::fmt::Debug for ScalarPredicate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ScalarPredicate")
            .field("comparison", &self.comparison)
            .field("value", &"[REDACTED]")
            .finish()
    }
}
impl ScalarPredicate {
    /// The existing six comparison operators also apply to canonical scalar
    /// operands. Int/Float comparisons use exact numeric value, not cross-kind
    /// storage ranks or a lossy integer-to-float coercion. Missing and unrelated
    /// scalar kinds do not become true under NotEqual.
    pub fn new(
        value: CanonicalScalar,
        comparison: IntegerComparison,
    ) -> Result<Self, ScalarPredicateError> {
        let payload = match &value {
            CanonicalScalar::Bytes(value) => value.as_slice().len(),
            CanonicalScalar::Text(value) => value
                .len()
                .saturating_add(value.canonical_sort_key().map_or(0, <[u8]>::len)),
            CanonicalScalar::Timestamp(value) => {
                value.zone().map_or(0, |zone| zone.identifier().len())
            }
            _ => 0,
        };
        check_size(payload)?;
        let encoded = value.encode().map_err(ScalarPredicateError::Encoding)?;
        check_size(encoded.len())?;
        Ok(Self {
            operand: Arc::new(ScalarOperand {
                value,
                encoded: encoded.into_boxed_slice(),
            }),
            comparison,
        })
    }

    /// Explicit plaintext operand export. No source scalar is cloned to match.
    #[must_use]
    pub fn value(&self) -> &CanonicalScalar {
        &self.operand.value
    }
    #[must_use]
    pub fn comparison(&self) -> IntegerComparison {
        self.comparison
    }

    /// Bind another operator to the same admitted scalar. This does not encode,
    /// copy the payload, change the original predicate, or weaken its bounds.
    #[must_use]
    pub fn with_comparison(&self, comparison: IntegerComparison) -> Self {
        Self {
            operand: Arc::clone(&self.operand),
            comparison,
        }
    }

    /// Explicit plaintext canonical-value export, excluding the comparison.
    /// The bytes are the immutable encoding checked during operand admission.
    #[must_use]
    pub fn canonical_value_bytes(&self) -> &[u8] {
        &self.operand.encoded
    }

    #[must_use]
    pub fn matches(&self, actual: Option<&CanonicalScalar>) -> bool {
        self.comparison
            .accepts_scalar_pair(actual, Some(self.value()))
    }

    pub(super) fn append_transcript(&self, bytes: &mut Vec<u8>) {
        bytes.push(self.comparison.tag());
        bytes.extend_from_slice(&(self.operand.encoded.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&self.operand.encoded);
    }

    fn comparison_work_units(&self) -> usize {
        self.operand
            .encoded
            .len()
            .div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
    }
}
fn check_size(observed: usize) -> Result<(), ScalarPredicateError> {
    if observed > MAX_SCALAR_PREDICATE_BYTES {
        Err(ScalarPredicateError::LiteralTooLarge {
            limit: MAX_SCALAR_PREDICATE_BYTES,
            observed,
        })
    } else {
        Ok(())
    }
}

impl IntegerComparison {
    /// Compare borrowed scalar operands without collapsing UNKNOWN into FALSE.
    /// Int/Float pairs compare numerically in either direction; other unequal
    /// scalar kinds, missing properties, and stored null remain UNKNOWN. Float
    /// NaN retains the existing STRICT_PORTABLE position after positive infinity.
    #[must_use]
    pub fn evaluate_scalar_pair(
        self,
        left: Option<&CanonicalScalar>,
        right: Option<&CanonicalScalar>,
    ) -> Option<bool> {
        let (left, right) = (left?, right?);
        let order = match (left, right) {
            (CanonicalScalar::Null, _) | (_, CanonicalScalar::Null) => return None,
            (CanonicalScalar::Int(left), CanonicalScalar::Float(right)) => {
                compare_integer_float(*left, right.get())
            }
            (CanonicalScalar::Float(left), CanonicalScalar::Int(right)) => {
                compare_integer_float(*right, left.get()).reverse()
            }
            _ if core::mem::discriminant(left) == core::mem::discriminant(right) => left.cmp(right),
            _ => return None,
        };
        Some(match self {
            Self::Equal => order == Ordering::Equal,
            Self::NotEqual => order != Ordering::Equal,
            Self::Greater => order == Ordering::Greater,
            Self::Less => order == Ordering::Less,
            Self::GreaterOrEqual => order != Ordering::Less,
            Self::LessOrEqual => order != Ordering::Greater,
        })
    }

    /// WHERE selection keeps only TRUE. Call evaluate_scalar_pair when the
    /// result participates in NOT/AND/OR; UNKNOWN must not be negated as FALSE.
    #[must_use]
    pub fn accepts_scalar_pair(
        self,
        left: Option<&CanonicalScalar>,
        right: Option<&CanonicalScalar>,
    ) -> bool {
        self.evaluate_scalar_pair(left, right) == Some(true)
    }
}

/// Compare without rounding the integer to binary64. In particular, 2^53+1
/// must not compare equal to 2^53, and i64::MAX is less than the float 2^63.
fn compare_integer_float(integer: i64, floating: f64) -> Ordering {
    const I64_LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if floating.is_nan() || floating >= I64_LIMIT {
        return Ordering::Less;
    }
    if floating < -I64_LIMIT {
        return Ordering::Greater;
    }
    // The range checks exclude infinities and both saturating-cast boundaries.
    let truncated = floating as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal => {
            // Only here is converting the integer back to f64 exact: it came
            // from this in-range float's integral part. Above 2^53 the float
            // has no fractional bits; below it every integral part fits.
            let integral = truncated as f64;
            if integral < floating {
                Ordering::Less
            } else if integral > floating {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        order => order,
    }
}

impl VertexPredicate {
    /// The one property-key selector used by canonical transaction overrides.
    /// A removed value must not fall through to its durable basis value.
    #[must_use]
    pub fn property_key(&self) -> Option<PropertyKeyId> {
        match self {
            Self::HasLabel(_) => None,
            Self::IntegerProperty { key, .. }
            | Self::ScalarProperty { key, .. }
            | Self::PropertyNull { key, .. } => Some(*key),
        }
    }

    /// Additional logical payload work reserved before a cache-miss predicate
    /// read/comparison. Legacy fixed-integer/label events remain unchanged.
    /// This is not preemption inside a scalar comparison or a byte-memory cap.
    #[must_use]
    pub fn comparison_work_units(&self) -> usize {
        match self {
            Self::ScalarProperty { predicate, .. } => predicate.comparison_work_units(),
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::CanonicalF64;

    const COMPARISONS: [IntegerComparison; 6] = [
        IntegerComparison::Equal,
        IntegerComparison::NotEqual,
        IntegerComparison::Greater,
        IntegerComparison::Less,
        IntegerComparison::GreaterOrEqual,
        IntegerComparison::LessOrEqual,
    ];

    fn accepts_order(comparison: IntegerComparison, order: Ordering) -> bool {
        match comparison {
            IntegerComparison::Equal => order.is_eq(),
            IntegerComparison::NotEqual => !order.is_eq(),
            IntegerComparison::Greater => order.is_gt(),
            IntegerComparison::Less => order.is_lt(),
            IntegerComparison::GreaterOrEqual => !order.is_lt(),
            IntegerComparison::LessOrEqual => !order.is_gt(),
        }
    }

    #[test]
    fn comparisons_preserve_canonical_order_except_for_mixed_numeric_pairs() {
        let values = [
            CanonicalScalar::Null,
            CanonicalScalar::Bool(false),
            CanonicalScalar::Bool(true),
            CanonicalScalar::Int(i64::MIN),
            CanonicalScalar::Int(0),
            CanonicalScalar::Int(i64::MAX),
            CanonicalScalar::Float(CanonicalF64::new(-0.0)),
            CanonicalScalar::Float(CanonicalF64::new(f64::INFINITY)),
            CanonicalScalar::Float(CanonicalF64::new(f64::NAN)),
            CanonicalScalar::ucs_basic_text("").unwrap(),
            CanonicalScalar::ucs_basic_text("O'Brien 🦀").unwrap(),
            CanonicalScalar::bytes(vec![]).unwrap(),
            CanonicalScalar::bytes(vec![0, 255]).unwrap(),
        ];
        for expected in &values {
            for comparison in COMPARISONS {
                let predicate = ScalarPredicate::new(expected.clone(), comparison).unwrap();
                assert!(!predicate.matches(None));
                for actual in &values {
                    let order = match (actual, expected) {
                        (CanonicalScalar::Null, _) | (_, CanonicalScalar::Null) => None,
                        // This fixture's floats are exactly zero, +inf, and NaN.
                        // Neither nonzero float can be reached by an i64.
                        (CanonicalScalar::Int(i), CanonicalScalar::Float(f)) => {
                            Some(if f.get() == 0.0 {
                                i.cmp(&0)
                            } else {
                                Ordering::Less
                            })
                        }
                        (CanonicalScalar::Float(f), CanonicalScalar::Int(i)) => {
                            Some(if f.get() == 0.0 {
                                0.cmp(i)
                            } else {
                                Ordering::Greater
                            })
                        }
                        _ if core::mem::discriminant(actual)
                            == core::mem::discriminant(expected) =>
                        {
                            // Independently encoded order, not the comparator.
                            Some(actual.encode().unwrap().cmp(&expected.encode().unwrap()))
                        }
                        _ => None,
                    };
                    let wanted = order.map(|order| accepts_order(comparison, order));
                    assert_eq!(
                        comparison.evaluate_scalar_pair(Some(actual), Some(expected)),
                        wanted
                    );
                    assert_eq!(predicate.matches(Some(actual)), wanted == Some(true));
                }
            }
        }
    }

    #[test]
    fn mixed_numeric_boundaries_are_exact_in_both_directions() {
        use Ordering::{Equal, Greater, Less};
        let cases = [
            (0, 0.0, Equal),
            (0, -0.0, Equal),
            (0, f64::from_bits(1), Less),
            (0, -f64::from_bits(1), Greater),
            (1, 1.5, Less),
            (-1, -1.5, Greater),
            (9_007_199_254_740_993, 9_007_199_254_740_992.0, Greater),
            (9_007_199_254_740_993, 9_007_199_254_740_994.0, Less),
            (-9_007_199_254_740_993, -9_007_199_254_740_992.0, Less),
            (i64::MIN, -9_223_372_036_854_775_808.0, Equal),
            (i64::MIN + 1, -9_223_372_036_854_775_808.0, Greater),
            (i64::MAX, 9_223_372_036_854_775_808.0, Less),
            (i64::MAX - 1023, 9_223_372_036_854_774_784.0, Equal),
            (i64::MIN, f64::NEG_INFINITY, Greater),
            (i64::MAX, f64::INFINITY, Less),
            (0, f64::NAN, Less),
        ];
        for (integer, floating, order) in cases {
            let integer = CanonicalScalar::Int(integer);
            let floating = CanonicalScalar::Float(CanonicalF64::new(floating));
            for comparison in COMPARISONS {
                assert_eq!(
                    comparison.evaluate_scalar_pair(Some(&integer), Some(&floating)),
                    Some(accepts_order(comparison, order))
                );
                assert_eq!(
                    comparison.evaluate_scalar_pair(Some(&floating), Some(&integer)),
                    Some(accepts_order(comparison, order.reverse()))
                );
            }
        }
    }

    #[test]
    fn mixed_numeric_fractional_grid_matches_scaled_integer_oracle() {
        for integer in -64_i64..=64 {
            for numerator in -512_i64..=512 {
                // Eighths are exactly representable, so integer scaling is an
                // independent oracle with no float conversion of the subject.
                let expected = (integer * 8).cmp(&numerator);
                assert_eq!(
                    compare_integer_float(integer, numerator as f64 / 8.0),
                    expected
                );
            }
        }
    }

    #[test]
    fn numeric_predicate_equality_does_not_change_storage_identity() {
        let integer = CanonicalScalar::Int(1);
        let floating = CanonicalScalar::Float(CanonicalF64::new(1.0));
        assert!(IntegerComparison::Equal.accepts_scalar_pair(Some(&integer), Some(&floating)));
        assert_ne!(integer, floating);
        assert_eq!(integer.cmp(&floating), Ordering::Less);
        assert_ne!(integer.encode().unwrap(), floating.encode().unwrap());
    }

    #[test]
    fn null_tests_include_missing_but_ordinary_inequality_does_not() {
        let key = PropertyKeyId(7);
        for null in [false, true] {
            let predicate = VertexPredicate::PropertyNull { key, is_null: null };
            assert_eq!(predicate.property_key(), Some(key));
            assert_eq!(predicate.matches(&[], &[]), null);
            assert_eq!(
                predicate.matches(&[], &[(key, CanonicalScalar::Null)]),
                null
            );
            assert_eq!(
                predicate.matches(&[], &[(key, CanonicalScalar::Bool(false))]),
                !null
            );
            assert_eq!(
                predicate.matches(&[], &[(PropertyKeyId(8), CanonicalScalar::Bool(false))]),
                null
            );
        }
        let predicate = VertexPredicate::ScalarProperty {
            key,
            predicate: ScalarPredicate::new(
                CanonicalScalar::Bool(true),
                IntegerComparison::NotEqual,
            )
            .unwrap(),
        };
        assert!(!predicate.matches(&[], &[]));
        assert!(!predicate.matches(&[], &[(key, CanonicalScalar::Null)]));
        assert!(!predicate.matches(&[], &[(key, CanonicalScalar::Int(1))]));
        assert!(predicate.matches(&[], &[(key, CanonicalScalar::Bool(false))]));
    }

    #[test]
    fn bounded_operands_bind_exact_values_but_do_not_disclose_them_in_debug() {
        let value = CanonicalScalar::ucs_basic_text("sensitive query value").unwrap();
        let predicate = ScalarPredicate::new(value.clone(), IntegerComparison::Equal).unwrap();
        assert_eq!(predicate.value(), &value);
        assert!(!format!("{predicate:?}").contains("sensitive"));
        let mut first = Vec::new();
        predicate.append_transcript(&mut first);
        let mut changed = Vec::new();
        ScalarPredicate::new(value, IntegerComparison::NotEqual)
            .unwrap()
            .append_transcript(&mut changed);
        assert_ne!(first, changed);
        let over = CanonicalScalar::bytes(vec![0; MAX_SCALAR_PREDICATE_BYTES + 1]).unwrap();
        assert!(matches!(
            ScalarPredicate::new(over, IntegerComparison::Equal),
            Err(ScalarPredicateError::LiteralTooLarge { .. })
        ));
        // Memcomparable framing can exceed the cap even with a raw payload at it.
        let framed = CanonicalScalar::bytes(vec![0; MAX_SCALAR_PREDICATE_BYTES]).unwrap();
        assert!(matches!(
            ScalarPredicate::new(framed, IntegerComparison::Equal),
            Err(ScalarPredicateError::LiteralTooLarge { .. })
        ));
    }

    #[test]
    fn rebinding_comparisons_shares_the_checked_operand_and_preserves_identity() {
        let value = CanonicalScalar::ucs_basic_text(&"x".repeat(4096)).unwrap();
        let equal = ScalarPredicate::new(value.clone(), IntegerComparison::Equal).unwrap();
        let different = equal.with_comparison(IntegerComparison::NotEqual);
        assert!(std::ptr::eq(equal.value(), different.value()));
        assert!(std::ptr::eq(
            equal.canonical_value_bytes(),
            different.canonical_value_bytes()
        ));
        assert_eq!(equal.canonical_value_bytes(), value.encode().unwrap());
        assert!(equal.matches(Some(&value)));
        assert!(!different.matches(Some(&value)));
        let mut actual = Vec::new();
        different.append_transcript(&mut actual);
        let mut expected = Vec::new();
        ScalarPredicate::new(value, IntegerComparison::NotEqual)
            .unwrap()
            .append_transcript(&mut expected);
        assert_eq!(actual, expected);
        drop(equal);
        assert!(!different.matches(Some(different.value())));
    }

    #[test]
    fn borrowed_pairs_preserve_direction_and_reject_absence_on_either_side() {
        let small = CanonicalScalar::Int(i64::MIN);
        let large = CanonicalScalar::Int(i64::MAX);
        let boolean = CanonicalScalar::Bool(false);
        let null = CanonicalScalar::Null;
        for comparison in COMPARISONS {
            for absent in [None, Some(&null), Some(&boolean)] {
                assert!(!comparison.accepts_scalar_pair(Some(&small), absent));
                assert!(!comparison.accepts_scalar_pair(absent, Some(&small)));
                assert_eq!(comparison.evaluate_scalar_pair(Some(&small), absent), None);
                assert_eq!(comparison.evaluate_scalar_pair(absent, Some(&small)), None);
            }
            assert_eq!(
                comparison.accepts_scalar_pair(Some(&small), Some(&large)),
                comparison.accepts(i64::MIN, i64::MAX)
            );
            assert_eq!(
                comparison.accepts_scalar_pair(Some(&large), Some(&small)),
                comparison.accepts(i64::MAX, i64::MIN)
            );
        }
    }
}
