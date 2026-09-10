//! Bounded canonical scalar operands for the shared vertex predicate engine.
//!
//! The scalar union, equality, ordering and encoding remain fgdb_types-owned.
//! This profile compares only matching scalar kinds, without numeric coercion.
//! Missing properties and canonical null fail every ordinary comparison; null
//! tests are separate VertexPredicate operations. Within a kind, comparisons
//! use STRICT_PORTABLE order, including its canonical NaN/collation/time rules.

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
    /// operands. Comparisons never use cross-kind ranks to coerce an Int into
    /// Float, or to treat a missing/ill-typed property as NotEqual.
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
    /// Compare two borrowed canonical properties under the same rule as a
    /// scalar literal predicate. Missing/null/heterogeneous pairs never pass,
    /// including NotEqual. This is a WHERE-selection result, not a three-valued
    /// Boolean expression whose false result may safely be negated.
    #[must_use]
    pub fn accepts_scalar_pair(
        self,
        left: Option<&CanonicalScalar>,
        right: Option<&CanonicalScalar>,
    ) -> bool {
        let (Some(left), Some(right)) = (left, right) else {
            return false;
        };
        if matches!(left, CanonicalScalar::Null)
            || matches!(right, CanonicalScalar::Null)
            || core::mem::discriminant(left) != core::mem::discriminant(right)
        {
            return false;
        }
        let order = left.cmp(right);
        match self {
            Self::Equal => order == Ordering::Equal,
            Self::NotEqual => order != Ordering::Equal,
            Self::Greater => order == Ordering::Greater,
            Self::Less => order == Ordering::Less,
            Self::GreaterOrEqual => order != Ordering::Less,
            Self::LessOrEqual => order != Ordering::Greater,
        }
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

    #[test]
    fn every_comparison_uses_canonical_order_without_cross_kind_coercion() {
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
            for comparison in [
                IntegerComparison::Equal,
                IntegerComparison::NotEqual,
                IntegerComparison::Greater,
                IntegerComparison::Less,
                IntegerComparison::GreaterOrEqual,
                IntegerComparison::LessOrEqual,
            ] {
                let predicate = ScalarPredicate::new(expected.clone(), comparison).unwrap();
                assert!(!predicate.matches(None));
                for actual in &values {
                    let comparable = !matches!(actual, CanonicalScalar::Null)
                        && !matches!(expected, CanonicalScalar::Null)
                        && core::mem::discriminant(actual) == core::mem::discriminant(expected);
                    // Independently encoded order, not the predicate's comparator.
                    let order = actual.encode().unwrap().cmp(&expected.encode().unwrap());
                    let wanted = comparable
                        && match comparison {
                            IntegerComparison::Equal => order.is_eq(),
                            IntegerComparison::NotEqual => !order.is_eq(),
                            IntegerComparison::Greater => order.is_gt(),
                            IntegerComparison::Less => order.is_lt(),
                            IntegerComparison::GreaterOrEqual => !order.is_lt(),
                            IntegerComparison::LessOrEqual => !order.is_gt(),
                        };
                    assert_eq!(predicate.matches(Some(actual)), wanted);
                }
            }
        }
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
        let floating = CanonicalScalar::Float(CanonicalF64::new(0.0));
        let null = CanonicalScalar::Null;
        for comparison in [
            IntegerComparison::Equal,
            IntegerComparison::NotEqual,
            IntegerComparison::Greater,
            IntegerComparison::Less,
            IntegerComparison::GreaterOrEqual,
            IntegerComparison::LessOrEqual,
        ] {
            for absent in [None, Some(&null), Some(&floating)] {
                assert!(!comparison.accepts_scalar_pair(Some(&small), absent));
                assert!(!comparison.accepts_scalar_pair(absent, Some(&small)));
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
