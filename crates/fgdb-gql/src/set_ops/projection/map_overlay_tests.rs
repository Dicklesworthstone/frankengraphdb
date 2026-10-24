use super::*;
use crate::{GraphIntegerBinary, GraphIntegerErrorKind, GraphIntegerOp};

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn map(entries: &[(&str, GraphValue)]) -> GraphValue {
    GraphValue::map(
        entries
            .iter()
            .map(|(key, value)| ((*key).into(), value.clone()))
            .collect(),
    )
    .unwrap()
}

fn overlay(base: GraphSetValue, entries: &[(&str, GraphSetValue)]) -> GraphSetValue {
    GraphSetValue::MapOverlay {
        base: Box::new(base),
        keys: entries
            .iter()
            .map(|(key, _)| Box::<str>::from(*key))
            .collect(),
        values: entries.iter().map(|(_, value)| value.clone()).collect(),
    }
}

fn evaluate(
    value: &GraphSetValue,
    row: &GraphValueRow,
) -> Result<GraphValue, GraphIntegerErrorKind> {
    evaluate_value(value, row, 0, &mut |_| Ok::<_, ()>(())).map_err(|error| match error {
        ProjectionFailure::Arithmetic { error, .. } => error.kind,
        ProjectionFailure::Control(()) => unreachable!("fixture control is unlimited"),
    })
}

#[test]
fn map_overlay_merges_canonical_names_and_overrides_including_null_and_empty_keys() {
    let base = map(&[
        ("", int(1)),
        ("b", int(2)),
        ("nested", map(&[("x", int(3))])),
        ("z", int(4)),
    ]);
    let row = GraphValueRow::from_owned_values(vec![base.clone()]);
    let expression = overlay(
        GraphSetValue::Column(0),
        &[
            ("", GraphSetValue::Value(int(5))),
            ("a", GraphSetValue::Value(int(6))),
            (
                "b",
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
            ),
            ("λ", GraphSetValue::Value(int(7))),
        ],
    );
    assert_eq!(
        GraphSetProjection::admit_output(&expression, &[GraphSetColumnType::Any], 0),
        Ok(GraphSetColumnType::Any)
    );
    let actual = evaluate(&expression, &row).unwrap();
    assert_eq!(
        actual,
        map(&[
            ("", int(5)),
            ("a", int(6)),
            ("b", GraphValue::Scalar(CanonicalScalar::Null)),
            ("nested", map(&[("x", int(3))])),
            ("z", int(4)),
            ("λ", int(7)),
        ])
    );
    assert!(actual.validate_bounds());
    assert_eq!(row.get(0), Some(&base), "the source map remains frozen");
    assert_eq!(
        evaluate(&overlay(GraphSetValue::Column(0), &[]), &row).unwrap(),
        base
    );
}

#[test]
fn map_overlay_null_and_invalid_base_are_decided_before_override_execution() {
    let failing = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    let expression = overlay(GraphSetValue::Column(0), &[("late", failing)]);
    GraphSetProjection::admit_output(&expression, &[GraphSetColumnType::Any], 0).unwrap();
    assert_eq!(
        evaluate(
            &expression,
            &GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Null)])
        ),
        Ok(GraphValue::Scalar(CanonicalScalar::Null))
    );
    assert_eq!(
        evaluate(&expression, &GraphValueRow::from_owned_values(vec![int(1)])),
        Err(GraphIntegerErrorKind::NonMap)
    );
    assert_eq!(
        evaluate(
            &expression,
            &GraphValueRow::from_owned_values(vec![map(&[])])
        ),
        Err(GraphIntegerErrorKind::DivisionByZero)
    );
    assert_eq!(
        evaluate(
            &overlay(GraphSetValue::Column(0), &[]),
            &GraphValueRow::from_owned_values(vec![int(1)])
        ),
        Err(GraphIntegerErrorKind::NonMap)
    );
}

#[test]
fn map_overlay_admits_every_override_even_when_null_would_skip_it() {
    let null = GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
    let expression = overlay(null.clone(), &[("invalid", GraphSetValue::Column(4))]);
    assert_eq!(
        GraphSetProjection::admit_output(&expression, &[], 2),
        Err(GraphSetProjectionError::UnknownInput {
            column: 2,
            input: 4
        })
    );
    for entries in [
        vec![
            ("b", GraphSetValue::Value(int(1))),
            ("a", GraphSetValue::Value(int(2))),
        ],
        vec![
            ("a", GraphSetValue::Value(int(1))),
            ("a", GraphSetValue::Value(int(2))),
        ],
    ] {
        let invalid = overlay(null.clone(), &entries);
        assert_eq!(
            GraphSetProjection::admit_output(&invalid, &[], 2),
            Err(GraphSetProjectionError::InvalidValue { column: 2 })
        );
    }
    let malformed = GraphSetValue::MapOverlay {
        base: Box::new(null),
        keys: vec!["a".into()].into(),
        values: vec![],
    };
    assert_eq!(
        GraphSetProjection::admit_output(&malformed, &[], 2),
        Err(GraphSetProjectionError::InvalidValue { column: 2 })
    );
    assert_eq!(
        GraphSetProjection::admit_output(&overlay(GraphSetValue::Value(int(1)), &[]), &[], 2),
        Err(GraphSetProjectionError::MapInput { column: 2 })
    );
    assert_eq!(
        GraphSetProjection::admit_output(
            &overlay(GraphSetValue::Column(0), &[]),
            &[GraphSetColumnType::Vertex],
            2
        ),
        Err(GraphSetProjectionError::MapInput { column: 2 })
    );
}

#[test]
fn map_overlay_reserves_each_growth_and_cannot_publish_after_any_control_refusal() {
    let text = GraphValue::Scalar(CanonicalScalar::ucs_basic_text(&"payload".repeat(40)).unwrap());
    let row = GraphValueRow::from_owned_values(vec![map(&[("keep", text), ("replace", int(2))])]);
    let expression = overlay(
        GraphSetValue::Column(0),
        &[
            ("add", GraphSetValue::Value(int(3))),
            ("replace", GraphSetValue::Value(int(4))),
        ],
    );
    let mut events = Vec::new();
    let measured = evaluate_value(&expression, &row, 0, &mut |event| {
        events.push(event);
        Ok::<_, usize>(())
    });
    let Ok(expected) = measured else {
        panic!("unlimited evaluation must succeed");
    };
    assert!(
        events
            .iter()
            .any(|event| matches!(event, GlaExecutionEvent::Work))
    );
    assert!(
        events
            .iter()
            .filter(|event| matches!(event, GlaExecutionEvent::ScratchEntry))
            .count()
            > 5
    );
    for stop in 1..=events.len() {
        let mut seen = 0;
        let result = evaluate_value(&expression, &row, 0, &mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(ProjectionFailure::Control(at)) if at == stop));
        assert_eq!(seen, stop);
    }
    assert_eq!(evaluate(&expression, &row).unwrap(), expected);
}

#[test]
fn map_overlay_rejects_a_result_exceeding_native_value_depth() {
    let mut nested = int(1);
    for _ in 0..GraphValue::MAX_LIST_DEPTH {
        nested = GraphValue::List(vec![nested].into_boxed_slice());
    }
    assert!(nested.validate_bounds());
    let expression = overlay(
        GraphSetValue::Column(0),
        &[("deep", GraphSetValue::Value(nested))],
    );
    GraphSetProjection::admit_output(&expression, &[GraphSetColumnType::Any], 0).unwrap();
    assert_eq!(
        evaluate(
            &expression,
            &GraphValueRow::from_owned_values(vec![map(&[])])
        ),
        Err(GraphIntegerErrorKind::Overflow)
    );
}

#[test]
fn map_overlay_identity_binds_base_and_overrides_without_retagging_map_literals() {
    fn bytes(value: &GraphSetValue) -> Vec<u8> {
        let mut bytes = Vec::new();
        value.append_canonical_bytes(&mut bytes);
        bytes
    }
    let expression = overlay(
        GraphSetValue::Column(0),
        &[("a", GraphSetValue::Value(int(1)))],
    );
    let identity = bytes(&expression);
    assert_eq!(identity[0], 18);
    for different in [
        overlay(
            GraphSetValue::Column(1),
            &[("a", GraphSetValue::Value(int(1)))],
        ),
        overlay(
            GraphSetValue::Column(0),
            &[("b", GraphSetValue::Value(int(1)))],
        ),
        overlay(
            GraphSetValue::Column(0),
            &[("a", GraphSetValue::Value(int(2)))],
        ),
        overlay(GraphSetValue::Column(0), &[]),
    ] {
        assert_ne!(identity, bytes(&different));
    }
    for (guard, tag) in [(None, 14), (Some(Box::new(GraphSetValue::Column(0))), 17)] {
        let literal = GraphSetValue::MapLiteral {
            keys: vec!["a".into()].into(),
            values: vec![GraphSetValue::Value(int(1))],
            guard,
        };
        assert_eq!(bytes(&literal)[0], tag);
        assert_ne!(identity, bytes(&literal));
    }
}
