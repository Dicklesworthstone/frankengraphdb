//! Declared canonical arguments are bound into the existing scoped compiler.
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaOperator, GraphValueRow, PreparedGraphPattern, VertexPredicate};
use fgdb_gql::{
    GqlParameterType, GqlParameterValue, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
    PreparedGraphText,
};
use fgdb_types::{CanonicalF64, CanonicalScalar, CanonicalScalarKind, VId};
use std::cell::Cell;

const P: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, u64::MAX, u64::MAX)
}
fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn run(
    pattern: &PreparedGraphPattern<GraphValueRow>,
    values: &[Option<CanonicalScalar>],
) -> Vec<VId> {
    pattern
        .plan()
        .execute_governed_with_properties(
            values.len() as u64,
            (0..values.len()).map(|at| VId(at as u128)),
            [],
            |vid, predicates| {
                let property = values[vid.0 as usize].as_ref().map(|value| (P, value));
                Ok::<_, ()>(
                    predicates
                        .iter()
                        .all(|predicate| predicate.matches_borrowed([], property)),
                )
            },
            |vid, _| Ok(values[vid.0 as usize].as_ref()),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
        .iter()
        .map(|row| row.get(0).unwrap().as_vertex().unwrap())
        .collect()
}

#[test]
fn scalar_bindings_match_independent_encoded_order_for_every_supported_comparison() {
    let values = [
        None,
        Some(CanonicalScalar::Null),
        Some(CanonicalScalar::Bool(false)),
        Some(CanonicalScalar::Bool(true)),
        Some(CanonicalScalar::Int(-1)),
        Some(CanonicalScalar::Int(1)),
        Some(CanonicalScalar::Float(CanonicalF64::new(0.0))),
        Some(CanonicalScalar::Float(CanonicalF64::new(f64::NAN))),
        Some(text("")),
        Some(text("O'Reilly 🦀")),
        Some(CanonicalScalar::bytes(vec![0, 255]).unwrap()),
    ];
    for expected in values.iter().flatten() {
        for (operator, comparison) in [
            ("=", std::cmp::Ordering::Equal),
            ("<", std::cmp::Ordering::Less),
            (">", std::cmp::Ordering::Greater),
            ("<>", std::cmp::Ordering::Equal),
            ("<=", std::cmp::Ordering::Greater),
            (">=", std::cmp::Ordering::Less),
        ] {
            let template = PreparedGraphText::prepare_with_parameter_types(
                &format!("MATCH (n) WHERE n.p {operator} $value RETURN n"),
                &[(
                    "value",
                    GqlParameterType::Scalar(CanonicalScalarKind::of(expected)),
                )],
                symbols,
            )
            .unwrap();
            let arguments = GqlParameters::new()
                .with_scalar("value", expected.clone())
                .unwrap();
            let query = template.bind_parameters(&arguments).unwrap();
            let wanted: Vec<_> = values
                .iter()
                .enumerate()
                .filter_map(|(at, actual)| {
                    let actual = actual.as_ref()?;
                    if matches!(actual, CanonicalScalar::Null)
                        || matches!(expected, CanonicalScalar::Null)
                        || CanonicalScalarKind::of(actual) != CanonicalScalarKind::of(expected)
                    {
                        return None;
                    }
                    let order = actual.encode().unwrap().cmp(&expected.encode().unwrap());
                    let accepts = if ["<>", "<=", ">="].contains(&operator) {
                        order != comparison
                    } else {
                        order == comparison
                    };
                    accepts.then_some(VId(at as u128))
                })
                .collect();
            assert_eq!(
                run(&query, &values),
                wanted,
                "operator={operator}, kind={:?}",
                CanonicalScalarKind::of(expected)
            );
        }
    }
}

#[test]
fn arbitrary_text_is_an_argument_not_syntax_and_rebindings_share_the_admitted_payload() {
    let calls = Cell::new(0);
    let source = "MATCH (n) WHERE n.p >= $value AND n.p <= $value RETURN n LIMIT $take";
    let template = PreparedGraphText::prepare_with_parameter_types(
        source,
        &[("value", GqlParameterType::Scalar(CanonicalScalarKind::Text))],
        |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(template.parameter_schema()[0].occurrences, 2);
    assert_eq!(
        template.parameter_schema()[1].parameter_type,
        GqlParameterType::UInt64
    );
    let payload = "O'Reilly 🦀 ' OR n.p IS NOT NULL $take\0";
    let arguments = GqlParameters::new()
        .with_text("value", payload)
        .unwrap()
        .with_uint64("take", 10)
        .unwrap();
    let GqlParameterValue::Scalar(shared) = arguments.get("value").unwrap() else {
        panic!("scalar argument");
    };
    let first = template.bind_parameters(&arguments).unwrap();
    let mut occurrences = 0;
    for operator in first.plan().operators() {
        if let GlaOperator::Select { predicates, .. } = operator {
            for predicate in predicates {
                if let VertexPredicate::ScalarProperty { predicate, .. } = predicate {
                    assert!(std::ptr::eq(predicate.value(), shared.value()));
                    occurrences += 1;
                }
            }
        }
    }
    assert_eq!(occurrences, 2);
    let frozen = first.canonical_bytes();
    let second = template
        .bind_parameters(
            &GqlParameters::new()
                .with_text("value", "other")
                .unwrap()
                .with_uint64("take", 10)
                .unwrap(),
        )
        .unwrap();
    assert_ne!(frozen, second.canonical_bytes());
    assert_eq!(first.canonical_bytes(), frozen);
    assert_eq!(calls.get(), 1);
    assert_eq!(template.statement(), source);
    drop(arguments);
    drop(template);
    assert_eq!(
        run(&first, &[None, Some(text(payload)), Some(text("other"))]),
        vec![VId(1)]
    );
    assert!(!format!("{first:?} {shared:?}").contains(payload));
}

#[test]
fn declarations_and_exact_argument_types_fail_without_reinterpreting_positions() {
    let text_kind = GqlParameterType::Scalar(CanonicalScalarKind::Text);
    for (source, declarations) in [
        (
            "MATCH (n) WHERE n.p=$x RETURN n",
            vec![("x", text_kind), ("x", text_kind)],
        ),
        ("MATCH (n) WHERE n.p=$x RETURN n", vec![("$x", text_kind)]),
        (
            "MATCH (n) WHERE n.p=$x RETURN n",
            vec![("unused", text_kind)],
        ),
        ("MATCH (n) RETURN n LIMIT $x", vec![("x", text_kind)]),
        (
            "MATCH (n) WHERE n.p=$x RETURN n LIMIT $x",
            vec![("x", text_kind)],
        ),
        (
            "MATCH (n) WHERE n.p=$x RETURN n",
            vec![("x", GqlParameterType::UInt64)],
        ),
        ("MATCH (n) WHERE n.p='$x' RETURN n", vec![("x", text_kind)]),
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphText::prepare_with_parameter_types(source, &declarations, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{source}"
        );
        assert_eq!(calls.get(), 0);
    }
    let template = PreparedGraphText::prepare_with_parameter_types(
        "MATCH (n) WHERE n.p=$x RETURN n",
        &[("x", text_kind)],
        symbols,
    )
    .unwrap();
    for wrong in [
        GqlParameters::new().with_bool("x", true).unwrap(),
        GqlParameters::new().with_int64("x", 1).unwrap(),
        GqlParameters::new().with_uint64("x", 1).unwrap(),
    ] {
        assert!(matches!(
            template.bind_parameters(&wrong).unwrap_err().kind,
            GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
        ));
    }
    assert_eq!(
        template
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .kind,
        GraphPatternTextErrorKind::MissingParameter
    );
    let extra = GqlParameters::new()
        .with_text("x", "ready")
        .unwrap()
        .with_bool("extra", true)
        .unwrap();
    assert_eq!(
        template.bind_parameters(&extra).unwrap_err().kind,
        GraphPatternTextErrorKind::UnexpectedArguments
    );
    let null = template
        .bind_parameters(&GqlParameters::new().with_null("x").unwrap())
        .unwrap();
    assert!(
        run(
            &null,
            &[Some(text("ready")), None, Some(CanonicalScalar::Null)]
        )
        .is_empty()
    );
}

#[test]
fn numeric_defaults_and_explicit_numeric_declarations_keep_the_same_compiled_definition() {
    let source = "MATCH (n) WHERE n.p >= $x RETURN n SKIP $skip LIMIT $take";
    let declarations = [
        ("take", GqlParameterType::UInt64),
        ("x", GqlParameterType::Int64),
    ];
    let arguments = GqlParameters::new()
        .with_int64("x", -3)
        .unwrap()
        .with_uint64("skip", 0)
        .unwrap()
        .with_uint64("take", 10)
        .unwrap();
    let old = PreparedGraphText::prepare(source, symbols).unwrap();
    let explicit =
        PreparedGraphText::prepare_with_parameter_types(source, &declarations, symbols).unwrap();
    assert_eq!(old.parameter_schema(), explicit.parameter_schema());
    assert_eq!(
        old.bind_parameters(&arguments).unwrap(),
        explicit.bind_parameters(&arguments).unwrap()
    );
    let scalar = GqlParameters::new()
        .with_text("x", "-3")
        .unwrap()
        .with_uint64("skip", 0)
        .unwrap()
        .with_uint64("take", 10)
        .unwrap();
    assert!(old.bind_parameters(&scalar).is_err());
}

#[test]
fn scoped_scalar_arguments_compose_with_null_extension_existence_and_group_ranking() {
    let head = "MATCH (n:L) WHERE NOT EXISTS { MATCH (n)-[:R]->(blocked) WHERE blocked.p=$ban } \
        OPTIONAL MATCH (n)-[:R]->(c) WHERE c.p=$wanted";
    let types = [
        (
            "wanted",
            GqlParameterType::Scalar(CanonicalScalarKind::Text),
        ),
        ("ban", GqlParameterType::Scalar(CanonicalScalarKind::Bool)),
    ];
    let arguments = GqlParameters::new()
        .with_text("wanted", "ready")
        .unwrap()
        .with_bool("ban", true)
        .unwrap();
    let template = PreparedGraphText::prepare_with_parameter_types(
        &format!("{head} RETURN n,c"),
        &types,
        symbols,
    )
    .unwrap();
    let query = template.bind_parameters(&arguments).unwrap();
    let ready = text("ready");
    let banned = CanonicalScalar::Bool(true);
    let edges = [
        (VId(1), RelationId(1), VId(10)),
        (VId(1), RelationId(1), VId(10)),
        (VId(3), RelationId(1), VId(11)),
    ];
    let matches = |vid, predicates: &[VertexPredicate]| {
        let value = match vid {
            VId(10) => Some(&ready),
            VId(11) => Some(&banned),
            _ => None,
        };
        Ok::<_, ()>(
            predicates
                .iter()
                .all(|p| p.matches_borrowed([LabelId(1)], value.map(|v| (P, v)))),
        )
    };
    let rows = query
        .plan()
        .execute_governed_with_properties(
            6,
            [VId(1), VId(2), VId(3)],
            edges,
            matches,
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], rows[1]);
    assert_eq!(rows[2].get(0).unwrap().as_vertex(), Some(VId(2)));
    assert!(rows[2].get(1).unwrap().is_null());
    let aggregate = PreparedGraphAggregateText::prepare_with_parameter_types(
        &format!(
            "{head} RETURN n,COUNT(*) AS occurrences,COUNT(c) AS present GROUP BY n \
            HAVING occurrences >= $minimum ORDER BY present DESC,n ASC LIMIT $take"
        ),
        &types,
        symbols,
    )
    .unwrap();
    let args = arguments
        .with_int64("minimum", 1)
        .unwrap()
        .with_uint64("take", 2)
        .unwrap();
    let summary = aggregate
        .bind_parameters(&args)
        .unwrap()
        .execute_governed(
            6,
            [VId(1), VId(2), VId(3)],
            edges,
            matches,
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(summary.value.len(), 2);
    assert_eq!(summary.value[0].get(0).unwrap().as_count(), Some(2));
    assert_eq!(summary.value[1].get(1).unwrap().as_count(), Some(0));
    let mut wrong_types = types.to_vec();
    wrong_types.push((
        "minimum",
        GqlParameterType::Scalar(CanonicalScalarKind::Int),
    ));
    assert!(
        PreparedGraphAggregateText::prepare_with_parameter_types(
            &format!("{head} RETURN COUNT(*) AS total HAVING total >= $minimum"),
            &wrong_types,
            symbols
        )
        .is_err()
    );
}

#[test]
fn scalar_bindings_keep_exact_limits_and_every_interruption_boundary() {
    let source = "MATCH (n) WHERE n.p=$x RETURN n,n.p AS value";
    let template = PreparedGraphText::prepare_with_parameter_types(
        source,
        &[("x", GqlParameterType::Scalar(CanonicalScalarKind::Text))],
        symbols,
    )
    .unwrap();
    let value = text(&"x".repeat(129));
    let query = template
        .bind_parameters(
            &GqlParameters::new()
                .with_scalar("x", value.clone())
                .unwrap(),
        )
        .unwrap();
    let mut checkpoints = 0;
    let full = query
        .plan()
        .execute_governed_with_properties(
            1,
            [VId(0)],
            [],
            |_, predicates| {
                Ok::<_, ()>(
                    predicates
                        .iter()
                        .all(|p| p.matches(&[], &[(P, value.clone())])),
                )
            },
            |_, _| Ok(Some(&value)),
            wide(),
            || {
                checkpoints += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    let exact = GqlQueryPolicy::new(
        1,
        1,
        full.evaluator.work_units,
        full.evaluator.scratch_entries,
    );
    let again = query
        .plan()
        .execute_governed_with_properties(
            1,
            [VId(0)],
            [],
            |_, predicates| {
                Ok::<_, ()>(
                    predicates
                        .iter()
                        .all(|p| p.matches_borrowed([], [(P, &value)])),
                )
            },
            |_, _| Ok(Some(&value)),
            exact,
            || Ok::<_, usize>(()),
        )
        .unwrap();
    assert_eq!(full, again);
    for stop in 1..=checkpoints {
        let mut at = 0;
        let refused = query.plan().execute_governed_with_properties(
            1,
            [VId(0)],
            [],
            |_, predicates| {
                Ok::<_, ()>(
                    predicates
                        .iter()
                        .all(|p| p.matches_borrowed([], [(P, &value)])),
                )
            },
            |_, _| Ok(Some(&value)),
            wide(),
            || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(refused, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
}
