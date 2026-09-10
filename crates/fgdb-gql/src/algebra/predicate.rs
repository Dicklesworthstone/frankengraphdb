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
            Self::LiteralTooLarge { limit, observed } => write!(f, "scalar predicate literal uses {observed} bytes, limit {limit}"),
            Self::Encoding(error) => write!(f, "scalar predicate encoding failed: {error}"),
        }
    }
}
impl core::error::Error for ScalarPredicateError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self { Self::Encoding(error) => Some(error), _ => None }
    }
}

/// Immutable checked operand plus its exact canonical transcript. Encoding is
/// prepared fallibly once, never recreated by the executor or hashed in place
/// of value identity. Neither field is publicly mutable; Debug redacts values.
#[derive(Clone, PartialEq, Eq)]
pub struct ScalarPredicate {
    value: CanonicalScalar,
    comparison: IntegerComparison,
    encoded: Box<[u8]>,
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
    pub fn new(value: CanonicalScalar, comparison: IntegerComparison) -> Result<Self, ScalarPredicateError> {
        let payload = match &value {
            CanonicalScalar::Bytes(value) => value.as_slice().len(),
            CanonicalScalar::Text(value) => value.len().saturating_add(value.canonical_sort_key().map_or(0, <[u8]>::len)),
            CanonicalScalar::Timestamp(value) => value.zone().map_or(0, |zone| zone.identifier().len()),
            _ => 0,
        };
        check_size(payload)?;
        let encoded = value.encode().map_err(ScalarPredicateError::Encoding)?;
        check_size(encoded.len())?;
        Ok(Self { value, comparison, encoded: encoded.into_boxed_slice() })
    }

    /// Explicit plaintext operand export. No source scalar is cloned to match.
    #[must_use]
    pub fn value(&self) -> &CanonicalScalar { &self.value }
    #[must_use]
    pub fn comparison(&self) -> IntegerComparison { self.comparison }

    #[must_use]
    pub fn matches(&self, actual: Option<&CanonicalScalar>) -> bool {
        let Some(actual) = actual else { return false; };
        if matches!(actual, CanonicalScalar::Null) || matches!(&self.value, CanonicalScalar::Null)
            || core::mem::discriminant(actual) != core::mem::discriminant(&self.value)
        {
            return false;
        }
        let order = actual.cmp(&self.value);
        match self.comparison {
            IntegerComparison::Equal => order == Ordering::Equal,
            IntegerComparison::NotEqual => order != Ordering::Equal,
            IntegerComparison::Greater => order == Ordering::Greater,
            IntegerComparison::Less => order == Ordering::Less,
            IntegerComparison::GreaterOrEqual => order != Ordering::Less,
            IntegerComparison::LessOrEqual => order != Ordering::Greater,
        }
    }

    pub(super) fn append_transcript(&self, bytes: &mut Vec<u8>) {
        bytes.push(self.comparison.tag());
        bytes.extend_from_slice(&(self.encoded.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&self.encoded);
    }

    fn comparison_work_units(&self) -> usize {
        self.encoded.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
    }
}
fn check_size(observed: usize) -> Result<(), ScalarPredicateError> {
    if observed > MAX_SCALAR_PREDICATE_BYTES {
        Err(ScalarPredicateError::LiteralTooLarge { limit: MAX_SCALAR_PREDICATE_BYTES, observed })
    } else { Ok(()) }
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
            CanonicalScalar::Null, CanonicalScalar::Bool(false), CanonicalScalar::Bool(true),
            CanonicalScalar::Int(i64::MIN), CanonicalScalar::Int(0), CanonicalScalar::Int(i64::MAX),
            CanonicalScalar::Float(CanonicalF64::new(-0.0)),
            CanonicalScalar::Float(CanonicalF64::new(f64::INFINITY)),
            CanonicalScalar::Float(CanonicalF64::new(f64::NAN)),
            CanonicalScalar::ucs_basic_text("").unwrap(),
            CanonicalScalar::ucs_basic_text("O'Brien 🦀").unwrap(),
            CanonicalScalar::bytes(vec![]).unwrap(), CanonicalScalar::bytes(vec![0, 255]).unwrap(),
        ];
        for expected in &values {
            for comparison in [IntegerComparison::Equal, IntegerComparison::NotEqual,
                IntegerComparison::Greater, IntegerComparison::Less,
                IntegerComparison::GreaterOrEqual, IntegerComparison::LessOrEqual] {
                let predicate = ScalarPredicate::new(expected.clone(), comparison).unwrap();
                assert!(!predicate.matches(None));
                for actual in &values {
                    let comparable = !matches!(actual, CanonicalScalar::Null)
                        && !matches!(expected, CanonicalScalar::Null)
                        && core::mem::discriminant(actual) == core::mem::discriminant(expected);
                    // Independently encoded order, not the predicate's comparator.
                    let order = actual.encode().unwrap().cmp(&expected.encode().unwrap());
                    let wanted = comparable && match comparison {
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
            assert_eq!(predicate.matches(&[], &[(key, CanonicalScalar::Null)]), null);
            assert_eq!(predicate.matches(&[], &[(key, CanonicalScalar::Bool(false))]), !null);
            assert_eq!(predicate.matches(&[], &[(PropertyKeyId(8), CanonicalScalar::Bool(false))]), null);
        }
        let predicate = VertexPredicate::ScalarProperty { key,
            predicate: ScalarPredicate::new(CanonicalScalar::Bool(true), IntegerComparison::NotEqual).unwrap() };
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
        let mut first = Vec::new(); predicate.append_transcript(&mut first);
        let mut changed = Vec::new();
        ScalarPredicate::new(value, IntegerComparison::NotEqual).unwrap().append_transcript(&mut changed);
        assert_ne!(first, changed);
        let over = CanonicalScalar::bytes(vec![0; MAX_SCALAR_PREDICATE_BYTES + 1]).unwrap();
        assert!(matches!(ScalarPredicate::new(over, IntegerComparison::Equal),
            Err(ScalarPredicateError::LiteralTooLarge { .. })));
        // Memcomparable framing can exceed the cap even with a raw payload at it.
        let framed = CanonicalScalar::bytes(vec![0; MAX_SCALAR_PREDICATE_BYTES]).unwrap();
        assert!(matches!(ScalarPredicate::new(framed, IntegerComparison::Equal),
            Err(ScalarPredicateError::LiteralTooLarge { .. })));
    }
}
