use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{FnxSelection, FnxWeightError, FnxWeightSpec, MissingWeightPolicy};
use fgdb_types::{CanonicalF64, CanonicalScalar};

fn property(missing: MissingWeightPolicy) -> FnxWeightSpec {
    FnxWeightSpec::Property { key: PropertyKeyId(8), missing }
}

#[test]
fn missing_and_null_weights_follow_only_the_explicit_policy() {
    for missing in [None, Some(&CanonicalScalar::Null)] {
        assert_eq!(property(MissingWeightPolicy::Reject).resolve(missing), Err(FnxWeightError::Missing));
        assert_eq!(property(MissingWeightPolicy::Unit).resolve(missing), Ok(1.0));
        assert_eq!(property(MissingWeightPolicy::Zero).resolve(missing), Ok(0.0));
    }
    assert_eq!(FnxWeightSpec::Unit.resolve(Some(&CanonicalScalar::Bool(true))), Ok(1.0));
    assert_eq!(FnxWeightSpec::Unit.property_key(), None);
    assert_eq!(property(MissingWeightPolicy::Unit).property_key(), Some(PropertyKeyId(8)));
}

#[test]
fn integer_weights_are_exact_including_the_i64_boundary() {
    let spec = property(MissingWeightPolicy::Reject);
    for value in [0, 1, -1, 9_007_199_254_740_992, 9_007_199_254_740_994, i64::MIN] {
        let weight = spec.resolve(Some(&CanonicalScalar::Int(value))).unwrap();
        assert_eq!(weight as i128, i128::from(value));
    }
    // An i64 round trip saturates at MAX and would incorrectly accept this.
    for value in [i64::MAX, 9_007_199_254_740_993, -9_007_199_254_740_993] {
        assert_eq!(spec.resolve(Some(&CanonicalScalar::Int(value))), Err(FnxWeightError::InexactInteger));
    }
}

#[test]
fn numeric_values_do_not_coerce_booleans_or_accept_nonfinite_floats() {
    let spec = property(MissingWeightPolicy::Zero);
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(spec.resolve(Some(&CanonicalScalar::Float(CanonicalF64::new(value)))), Err(FnxWeightError::NonFinite));
    }
    for value in [CanonicalScalar::Bool(false), CanonicalScalar::ucs_basic_text("4").unwrap()] {
        assert_eq!(spec.resolve(Some(&value)), Err(FnxWeightError::NotNumeric));
    }
    let zero = spec.resolve(Some(&CanonicalScalar::Float(CanonicalF64::new(-0.0)))).unwrap();
    assert_eq!(zero.to_bits(), 0.0_f64.to_bits());
    assert_eq!(spec.resolve(Some(&CanonicalScalar::Float(CanonicalF64::new(-2.25)))), Ok(-2.25));
    // A finite negative weight is legal projection data; PageRank itself refuses it.
}

#[test]
fn selection_identity_binds_filters_property_and_missing_value_law() {
    let base = FnxSelection { vertex_label: None, relation: None, weight: FnxWeightSpec::Unit };
    let variants = [
        FnxSelection { vertex_label: Some(LabelId(0)), ..base },
        FnxSelection { relation: Some(RelationId(0)), ..base },
        FnxSelection { weight: property(MissingWeightPolicy::Reject), ..base },
        FnxSelection { weight: property(MissingWeightPolicy::Unit), ..base },
        FnxSelection { weight: property(MissingWeightPolicy::Zero), ..base },
        FnxSelection { weight: FnxWeightSpec::Property { key: PropertyKeyId(9), missing: MissingWeightPolicy::Zero }, ..base },
    ];
    for (i, selection) in variants.iter().enumerate() {
        assert_ne!(selection.digest(), base.digest());
        for other in &variants[..i] { assert_ne!(selection.digest(), other.digest()); }
    }
    assert_eq!(base.digest(), base.digest());
}
