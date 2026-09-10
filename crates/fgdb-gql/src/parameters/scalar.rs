//! A scalar argument reuses the predicate engine's checked immutable operand.
//! There is no second scalar encoder, scalar union, or unbounded text-to-query
//! conversion. All argument names still live in the one GqlParameters map.

use super::{GqlParameterError, GqlParameterType, GqlParameterValue, GqlParameters};
use crate::algebra::{IntegerComparison, ScalarPredicate, ScalarPredicateError, MAX_SCALAR_PREDICATE_BYTES};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};

/// A checked canonical scalar argument. Construction admits and encodes the
/// value once; cloning and binding additional occurrences share that storage.
#[derive(Clone, PartialEq, Eq)]
pub struct GqlScalarParameter {
    operand: ScalarPredicate,
}

impl GqlScalarParameter {
    pub fn new(value: CanonicalScalar) -> Result<Self, ScalarPredicateError> {
        Ok(Self { operand: ScalarPredicate::new(value, IntegerComparison::Equal)? })
    }

    /// Explicit plaintext export; Debug does not reveal the scalar or its bytes.
    #[must_use]
    pub fn value(&self) -> &CanonicalScalar { self.operand.value() }

    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] { self.operand.canonical_value_bytes() }

    #[must_use]
    pub fn kind(&self) -> CanonicalScalarKind {
        CanonicalScalarKind::of(self.value())
    }

    pub(crate) fn predicate(&self, comparison: IntegerComparison) -> ScalarPredicate {
        self.operand.with_comparison(comparison)
    }
}

impl core::fmt::Debug for GqlScalarParameter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GqlScalarParameter")
            .field("kind", &self.kind())
            .field("value", &"[REDACTED]")
            .finish()
    }
}

impl GqlParameterType {
    /// Canonical scalar declarations accept their exact kind or canonical null.
    /// Null is not a numeric conversion: it remains null and cannot satisfy an
    /// ordinary comparison. Legacy Int64/UInt64 declarations remain nonnullable.
    #[must_use]
    pub fn accepts(self, actual: Self) -> bool {
        self == actual || matches!((self, actual),
            (Self::Scalar(_), Self::Scalar(CanonicalScalarKind::Null)))
    }
}

impl GqlParameters {
    /// Admit one canonical scalar before it enters the shared argument map.
    /// Duplicate names across numeric and scalar arguments still refuse.
    pub fn with_scalar(mut self, name: impl Into<String>, value: CanonicalScalar) -> Result<Self, GqlParameterError> {
        let value = GqlScalarParameter::new(value).map_err(|_| GqlParameterError::ScalarLiteral)?;
        self.insert(name, GqlParameterValue::Scalar(value))?;
        Ok(self)
    }

    /// Text is an operand, never query syntax. Quotes and parameter-looking
    /// payloads require no escaping by the caller. UCS_BASIC is explicit.
    pub fn with_text(self, name: impl Into<String>, value: &str) -> Result<Self, GqlParameterError> {
        if value.len() > MAX_SCALAR_PREDICATE_BYTES { return Err(GqlParameterError::ScalarLiteral); }
        let value = CanonicalScalar::ucs_basic_text(value).map_err(|_| GqlParameterError::ScalarLiteral)?;
        self.with_scalar(name, value)
    }

    pub fn with_bool(self, name: impl Into<String>, value: bool) -> Result<Self, GqlParameterError> {
        self.with_scalar(name, CanonicalScalar::Bool(value))
    }

    pub fn with_null(self, name: impl Into<String>) -> Result<Self, GqlParameterError> {
        self.with_scalar(name, CanonicalScalar::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PreparedGqlTemplate, RelationBind};
    use fgdb_delta_types::{LabelId, PropertyKeyId};

    #[test]
    fn arguments_share_payloads_across_clones_and_comparison_occurrences() {
        let argument = GqlScalarParameter::new(CanonicalScalar::ucs_basic_text(&"x".repeat(4096)).unwrap()).unwrap();
        let copy = argument.clone();
        let equal = argument.predicate(IntegerComparison::Equal);
        let lesser = argument.predicate(IntegerComparison::Less);
        assert!(std::ptr::eq(argument.value(), copy.value()));
        assert!(std::ptr::eq(argument.value(), equal.value()));
        assert!(std::ptr::eq(equal.value(), lesser.value()));
        assert!(std::ptr::eq(argument.canonical_bytes(), equal.canonical_value_bytes()));
        drop(argument);
        assert!(equal.matches(Some(copy.value())));
        assert!(!lesser.matches(Some(copy.value())));
    }

    #[test]
    fn scalar_and_numeric_names_share_one_map_and_numeric_transcripts_are_unchanged() {
        let mut arguments = GqlParameters::new().with_int64("n", 7).unwrap();
        let frozen = arguments.clone();
        let scalar = GqlScalarParameter::new(CanonicalScalar::Bool(true)).unwrap();
        assert!(matches!(arguments.insert("n", GqlParameterValue::Scalar(scalar)), Err(GqlParameterError::Duplicate { .. })));
        assert_eq!(arguments, frozen);
        let mut expected = b"fgdb:gql-parameters:v1\0".to_vec();
        expected.extend_from_slice(&1_u64.to_be_bytes());
        expected.extend_from_slice(&1_u64.to_be_bytes()); expected.push(b'n');
        expected.push(0); expected.extend_from_slice(&7_i64.to_be_bytes());
        assert_eq!(arguments.canonical_bytes(), expected);
        let a = arguments.with_text("status", "ready").unwrap();
        let b = GqlParameters::new().with_text("status", "ready").unwrap().with_int64("n", 7).unwrap();
        assert_eq!(a.canonical_bytes(), b.canonical_bytes());
        assert_ne!(a.canonical_bytes(), GqlParameters::new().with_int64("n", 7).unwrap().with_text("status", "other").unwrap().canonical_bytes());
        assert!(!format!("{a:?}").contains("ready"));
    }

    #[test]
    fn scalar_nullability_never_weakens_legacy_numeric_type_checks() {
        let null = GqlParameterType::Scalar(CanonicalScalarKind::Null);
        for kind in [CanonicalScalarKind::Null, CanonicalScalarKind::Bool, CanonicalScalarKind::Int,
            CanonicalScalarKind::Decimal, CanonicalScalarKind::Float, CanonicalScalarKind::Text,
            CanonicalScalarKind::Timestamp, CanonicalScalarKind::Bytes] {
            assert!(GqlParameterType::Scalar(kind).accepts(null));
            assert!(GqlParameterType::Scalar(kind).accepts(GqlParameterType::Scalar(kind)));
            assert!(!GqlParameterType::Int64.accepts(GqlParameterType::Scalar(kind)));
        }
        let bind = RelationBind::new().with_label("L", LabelId(1)).with_property("p", PropertyKeyId(1));
        let template = PreparedGqlTemplate::prepare("MATCH (n:L) WHERE n.p=$value RETURN n", &bind).unwrap();
        for args in [GqlParameters::new().with_text("value", "7").unwrap(),
            GqlParameters::new().with_scalar("value", CanonicalScalar::Int(7)).unwrap(),
            GqlParameters::new().with_null("value").unwrap()] {
            assert!(matches!(template.bind_parameters(&args), Err(GqlParameterError::TypeMismatch { .. })));
        }
        assert!(template.bind_parameters(&GqlParameters::new().with_int64("value", 7).unwrap()).unwrap().verifies_definition());
    }

    #[test]
    fn scalar_admission_refuses_large_values_and_debug_does_not_export_them() {
        assert!(matches!(GqlParameters::new().with_text("secret", &"x".repeat(60_000)), Err(GqlParameterError::ScalarLiteral)));
        let bytes = CanonicalScalar::bytes(vec![0; MAX_SCALAR_PREDICATE_BYTES + 1]).unwrap();
        assert!(GqlScalarParameter::new(bytes).is_err());
        let args = GqlParameters::new().with_text("secret", "not query syntax ' $name").unwrap();
        let value = args.get("secret").unwrap();
        assert_eq!(value.parameter_type(), GqlParameterType::Scalar(CanonicalScalarKind::Text));
        assert!(!format!("{args:?} {value:?}").contains("not query syntax"));
    }
}
