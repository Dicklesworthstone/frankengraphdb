use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaSelection};
use fgdb_prism::{FnxSelection, FnxWeightError, FnxWeightSpec, MissingWeightPolicy};
use fgdb_types::{
    CanonicalF64, CanonicalList, CanonicalMap, CanonicalMapEntry, CanonicalPropertyValue,
    CanonicalScalar, CanonicalText,
};

fn property(missing: MissingWeightPolicy) -> FnxWeightSpec {
    FnxWeightSpec::Property {
        key: PropertyKeyId(8),
        missing,
    }
}

fn stored(value: CanonicalScalar) -> CanonicalPropertyValue {
    CanonicalPropertyValue::Scalar(value)
}

#[test]
fn missing_and_null_weights_follow_only_the_explicit_policy() {
    let null = stored(CanonicalScalar::Null);
    for missing in [None, Some(&null)] {
        assert_eq!(
            property(MissingWeightPolicy::Reject).resolve(missing),
            Err(FnxWeightError::Missing)
        );
        assert_eq!(
            property(MissingWeightPolicy::Unit).resolve(missing),
            Ok(1.0)
        );
        assert_eq!(
            property(MissingWeightPolicy::Zero).resolve(missing),
            Ok(0.0)
        );
    }
    assert_eq!(
        FnxWeightSpec::Unit.resolve(Some(&stored(CanonicalScalar::Bool(true)))),
        Ok(1.0)
    );
    assert_eq!(FnxWeightSpec::Unit.property_key(), None);
    assert_eq!(
        property(MissingWeightPolicy::Unit).property_key(),
        Some(PropertyKeyId(8))
    );
}

#[test]
fn integer_weights_are_exact_including_the_i64_boundary() {
    let spec = property(MissingWeightPolicy::Reject);
    for value in [
        0,
        1,
        -1,
        9_007_199_254_740_992,
        9_007_199_254_740_994,
        i64::MIN,
    ] {
        let weight = spec
            .resolve(Some(&stored(CanonicalScalar::Int(value))))
            .unwrap();
        assert_eq!(weight as i128, i128::from(value));
    }
    // An i64 round trip saturates at MAX and would incorrectly accept this.
    for value in [i64::MAX, 9_007_199_254_740_993, -9_007_199_254_740_993] {
        assert_eq!(
            spec.resolve(Some(&stored(CanonicalScalar::Int(value)))),
            Err(FnxWeightError::InexactInteger)
        );
    }
}

#[test]
fn numeric_values_do_not_coerce_booleans_or_accept_nonfinite_floats() {
    let spec = property(MissingWeightPolicy::Zero);
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            spec.resolve(Some(&stored(CanonicalScalar::Float(CanonicalF64::new(
                value
            ))))),
            Err(FnxWeightError::NonFinite)
        );
    }
    // A stored List or Map is a present value of the wrong kind (fgdb-k3nub):
    // it is NotNumeric under every missing policy, never absent, and a
    // one-element numeric List is not unwrapped.
    let one = CanonicalList::try_new(vec![stored(CanonicalScalar::Int(4))]).unwrap();
    let map = CanonicalMap::try_new(vec![CanonicalMapEntry::new(
        CanonicalText::new_ucs_basic("w").unwrap(),
        stored(CanonicalScalar::Int(4)),
    )])
    .unwrap();
    for value in [
        stored(CanonicalScalar::Bool(false)),
        stored(CanonicalScalar::ucs_basic_text("4").unwrap()),
        CanonicalPropertyValue::List(one),
        CanonicalPropertyValue::List(CanonicalList::try_new(vec![]).unwrap()),
        CanonicalPropertyValue::Map(map),
    ] {
        assert_eq!(spec.resolve(Some(&value)), Err(FnxWeightError::NotNumeric));
        assert_eq!(
            property(MissingWeightPolicy::Unit).resolve(Some(&value)),
            Err(FnxWeightError::NotNumeric)
        );
    }
    let zero = spec
        .resolve(Some(&stored(CanonicalScalar::Float(CanonicalF64::new(
            -0.0,
        )))))
        .unwrap();
    assert_eq!(zero.to_bits(), 0.0_f64.to_bits());
    assert_eq!(
        spec.resolve(Some(&stored(CanonicalScalar::Float(CanonicalF64::new(
            -2.25
        ))))),
        Ok(-2.25)
    );
    // A finite negative weight is legal projection data; PageRank itself refuses it.
}

#[test]
fn selection_digest_preserves_legacy_bytes_and_canonical_union_membership() {
    let base = FnxSelection {
        vertex_labels: SchemaSelection::ALL,
        relations: SchemaSelection::ALL,
        weight: FnxWeightSpec::Unit,
    };
    let legacy = |label: Option<LabelId>, relation: Option<RelationId>| {
        let mut hash = fgdb_crypto::Hasher::new();
        hash.update(b"fgdb:prism:induced-selection:v1");
        for id in [label.map(|id| id.0), relation.map(|id| id.0)] {
            if let Some(id) = id {
                hash.update(&[1]);
                hash.update(&id.to_le_bytes());
            } else {
                hash.update(&[0]);
            }
        }
        hash.update(&[0]);
        hash.finalize()
    };
    for (label, relation) in [
        (None, None),
        (Some(LabelId(u64::MAX)), Some(RelationId(0))),
        (None, Some(RelationId(u64::MAX))),
    ] {
        assert_eq!(
            FnxSelection {
                vertex_labels: label.into(),
                relations: relation.into(),
                ..base
            }
            .digest(),
            legacy(label, relation)
        );
    }
    let union = FnxSelection {
        vertex_labels: SchemaSelection::any_of(&[LabelId(7), LabelId(u64::MAX)]).unwrap(),
        relations: SchemaSelection::any_of(&[RelationId(1), RelationId(2)]).unwrap(),
        ..base
    };
    let reordered = FnxSelection {
        vertex_labels: SchemaSelection::any_of(&[LabelId(u64::MAX), LabelId(7), LabelId(7)])
            .unwrap(),
        relations: SchemaSelection::any_of(&[RelationId(2), RelationId(1), RelationId(2)]).unwrap(),
        ..base
    };
    assert_eq!(union, reordered);
    assert_eq!(union.digest(), reordered.digest());
    for different in [
        base,
        FnxSelection {
            vertex_labels: SchemaSelection::one(LabelId(7)),
            ..union
        },
        FnxSelection {
            relations: SchemaSelection::any_of(&[RelationId(1), RelationId(3)]).unwrap(),
            ..union
        },
    ] {
        assert_ne!(union.digest(), different.digest());
    }
}

#[test]
fn selection_identity_binds_filters_property_and_missing_value_law() {
    let base = FnxSelection {
        vertex_labels: None.into(),
        relations: None.into(),
        weight: FnxWeightSpec::Unit,
    };
    let variants = [
        FnxSelection {
            vertex_labels: Some(LabelId(0)).into(),
            ..base
        },
        FnxSelection {
            relations: Some(RelationId(0)).into(),
            ..base
        },
        FnxSelection {
            weight: property(MissingWeightPolicy::Reject),
            ..base
        },
        FnxSelection {
            weight: property(MissingWeightPolicy::Unit),
            ..base
        },
        FnxSelection {
            weight: property(MissingWeightPolicy::Zero),
            ..base
        },
        FnxSelection {
            weight: FnxWeightSpec::Property {
                key: PropertyKeyId(9),
                missing: MissingWeightPolicy::Zero,
            },
            ..base
        },
    ];
    for (i, selection) in variants.iter().enumerate() {
        assert_ne!(selection.digest(), base.digest());
        for other in &variants[..i] {
            assert_ne!(selection.digest(), other.digest());
        }
    }
    assert_eq!(base.digest(), base.digest());
}
