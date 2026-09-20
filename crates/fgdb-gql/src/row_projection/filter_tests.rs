use super::*;
use crate::algebra::{GraphValue, IntegerComparison};
use crate::{GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphSetOperand};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(4);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn row(value: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
        value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
    )])
}
fn z(rows: &[(Option<i64>, i128)]) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.iter()
            .map(|(value, count)| (row(*value), ZWeight::from_i128(*count))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn apply(
    state: &mut IncrementalRowProjection,
    changes: &ZSet<GraphValueRow>,
) -> ZSet<GraphValueRow> {
    state
        .prepare(changes, LIMBS, None, &mut allow)
        .unwrap()
        .commit()
}
fn plain(rows: &ZSet<GraphValueRow>) -> BTreeMap<GraphValueRow, i128> {
    rows.iter()
        .map(|(row, count)| (row.clone(), count.to_i128().unwrap()))
        .collect()
}
fn nonnull() -> Vec<GraphSetPredicateOp> {
    vec![GraphSetPredicateOp::IsNull {
        operand: GraphSetOperand::Column(0),
        is_null: false,
    }]
}
fn spec(quantifier: GraphSetQuantifier) -> RowProjectionSpec {
    let parity = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Literal(Some(2)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Remainder),
    ])
    .unwrap();
    RowProjectionSpec::new(
        vec![GraphSetColumnType::Scalar],
        vec![GraphSetProjection::new(
            "parity",
            GraphSetValue::Integer(parity),
        )],
        quantifier,
    )
    .unwrap()
    .with_filter(&nonnull())
    .unwrap()
}
fn bag(mut code: usize) -> ZSet<GraphValueRow> {
    z(&[None, Some(-1), Some(2), Some(4)].map(|value| {
        let count = (code % 3) as i128;
        code /= 3;
        (value, count)
    }))
}
// Full recomputation, independent of predicate bytecode and delta maintenance.
fn oracle(input: &ZSet<GraphValueRow>, q: GraphSetQuantifier) -> BTreeMap<GraphValueRow, i128> {
    let mut result = BTreeMap::new();
    for (r, count) in input.iter() {
        if let GraphValue::Scalar(CanonicalScalar::Int(value)) = r.values()[0] {
            *result.entry(row(Some(value % 2))).or_insert(0) += count.to_i128().unwrap();
        }
    }
    if q == GraphSetQuantifier::Distinct {
        for count in result.values_mut() {
            *count = 1;
        }
    }
    result
}

#[test]
fn filtered_projection_matches_every_small_bag_transition_and_inverse() {
    for q in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
        for before in 0..81 {
            for after in 0..81 {
                let (old, new) = (bag(before), bag(after));
                let mut state = IncrementalRowProjection::new(spec(q));
                let mut integrated = apply(&mut state, &old);
                assert_eq!(plain(&integrated), oracle(&old, q));
                let changes = new.minus(&old, LIMBS, &mut allow).unwrap();
                let delta = apply(&mut state, &changes);
                integrated.integrate(&delta, LIMBS, &mut allow).unwrap();
                assert_eq!(
                    plain(state.rows()),
                    oracle(&new, q),
                    "{q:?}: {before} -> {after}"
                );
                assert_eq!(&integrated, state.rows());
                assert_eq!(
                    state.total(),
                    &state.rows().total_weight(LIMBS, &mut allow).unwrap()
                );
                apply(&mut state, &changes.negated(LIMBS, &mut allow).unwrap());
                assert_eq!(plain(state.rows()), oracle(&old, q));
            }
        }
    }
}

#[test]
fn hidden_rows_keep_exact_input_admission_and_final_output_quotas() {
    let definition = RowProjectionSpec::selection(
        vec![GraphSetColumnType::Scalar],
        vec!["x".into()],
        &[GraphSetPredicateOp::Truth(Some(false))],
    )
    .unwrap();
    let seed = || {
        let mut state = IncrementalRowProjection::new(definition.clone());
        state
            .prepare(&z(&[(None, i128::MAX)]), LIMBS, Some(0), &mut allow)
            .unwrap()
            .commit();
        state
            .prepare(&z(&[(None, i128::MAX)]), LIMBS, Some(0), &mut allow)
            .unwrap()
            .commit();
        state
    };
    let mut state = seed();
    assert!(state.input.weight(&row(None)).unwrap().is_promoted());
    assert!(state.rows().is_empty());
    assert_eq!(state.total(), &ZWeight::ZERO);
    assert!(matches!(
        state.prepare(&z(&[(Some(4), -1)]), LIMBS, Some(0), &mut allow),
        Err(RowProjectionError::NegativeMultiplicity)
    ));
    assert_eq!(state, seed());
    for _ in 0..2 {
        state
            .prepare(&z(&[(None, -i128::MAX)]), LIMBS, Some(0), &mut allow)
            .unwrap()
            .commit();
    }
    assert!(state.input.is_empty());
    assert!(matches!(
        state.prepare(&z(&[(None, -1)]), LIMBS, Some(0), &mut allow),
        Err(RowProjectionError::NegativeMultiplicity)
    ));
    let bad = ZSet::from_updates(
        [(GraphValueRow::from_owned_values(vec![]), ZWeight::ONE)],
        LIMBS,
        &mut allow,
    )
    .unwrap();
    assert!(matches!(
        state.prepare(&bad, LIMBS, None, &mut allow),
        Err(RowProjectionError::InputSchema)
    ));
}

#[test]
fn selection_precedes_output_expressions_without_relaxing_definition_admission() {
    let divide = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(12)),
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ])
    .unwrap();
    let projection = RowProjectionSpec::new(
        vec![GraphSetColumnType::Scalar],
        vec![GraphSetProjection::new(
            "ratio",
            GraphSetValue::Integer(divide),
        )],
        GraphSetQuantifier::All,
    )
    .unwrap();
    let selected = projection
        .clone()
        .with_filter(&[GraphSetPredicateOp::IsNull {
            operand: GraphSetOperand::Column(0),
            is_null: true,
        }])
        .unwrap();
    let mut state = IncrementalRowProjection::new(selected);
    apply(&mut state, &z(&[(Some(0), 5), (None, 2)])); // Hidden division by zero is not evaluated.
    assert_eq!(plain(state.rows()), BTreeMap::from([(row(None), 2)]));
    apply(&mut state, &z(&[(Some(0), -5), (None, -2)]));
    assert!(state.rows().is_empty());
    assert!(matches!(
        projection.clone().with_filter(&[GraphSetPredicateOp::And]),
        Err(RowProjectionBuildError::Filter(
            GraphSetFilterError::InvalidStack { .. }
        ))
    ));
    assert!(matches!(
        projection.with_filter(&[GraphSetPredicateOp::IsNull {
            operand: GraphSetOperand::Column(1),
            is_null: false
        },]),
        Err(RowProjectionBuildError::Filter(
            GraphSetFilterError::UnknownInput { column: 1, .. }
        ))
    ));
}

#[test]
fn selection_preserves_metadata_full_width_domains_and_zero_column_bags() {
    let definition = RowProjectionSpec::selection(
        vec![GraphSetColumnType::Vertex, GraphSetColumnType::Scalar],
        vec!["same name".into(), "same name".into()],
        &[GraphSetPredicateOp::Truth(Some(true))],
    )
    .unwrap();
    assert_eq!(
        definition.columns().collect::<Vec<_>>(),
        vec!["same name", "same name"]
    );
    let value = GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::Scalar(CanonicalScalar::Int(7)),
    ]);
    let input =
        ZSet::from_updates([(value, ZWeight::from_i128(i128::MAX))], LIMBS, &mut allow).unwrap();
    let mut state = IncrementalRowProjection::new(definition);
    assert_eq!(apply(&mut state, &input), input);
    let empty = GraphValueRow::from_owned_values(vec![]);
    let input = ZSet::from_updates([(empty, ZWeight::from_i128(7))], LIMBS, &mut allow).unwrap();
    let definition =
        RowProjectionSpec::selection(vec![], vec![], &[GraphSetPredicateOp::Truth(Some(true))])
            .unwrap();
    let mut state = IncrementalRowProjection::new(definition);
    assert_eq!(apply(&mut state, &input), input);
    assert!(matches!(
        RowProjectionSpec::selection(vec![GraphSetColumnType::Scalar], vec![], &nonnull()),
        Err(RowProjectionBuildError::ColumnCount { .. })
    ));
}

#[test]
fn three_valued_boolean_and_incompatible_comparison_semantics_are_shared() {
    for a in [None, Some(false), Some(true)] {
        for b in [None, Some(false), Some(true)] {
            for and in [false, true] {
                let expected = if and {
                    if a == Some(false) || b == Some(false) {
                        Some(false)
                    } else {
                        a.zip(b).map(|(a, b)| a && b)
                    }
                } else if a == Some(true) || b == Some(true) {
                    Some(true)
                } else {
                    a.zip(b).map(|(a, b)| a || b)
                };
                for negate in [false, true] {
                    let mut code = vec![
                        GraphSetPredicateOp::Truth(a),
                        GraphSetPredicateOp::Truth(b),
                        if and {
                            GraphSetPredicateOp::And
                        } else {
                            GraphSetPredicateOp::Or
                        },
                    ];
                    if negate {
                        code.push(GraphSetPredicateOp::Not);
                    }
                    let definition = RowProjectionSpec::selection(
                        vec![GraphSetColumnType::Scalar],
                        vec!["x".into()],
                        &code,
                    )
                    .unwrap();
                    let mut state = IncrementalRowProjection::new(definition);
                    let delta = apply(&mut state, &z(&[(Some(1), 3)]));
                    let keep = (if negate {
                        expected.map(|x| !x)
                    } else {
                        expected
                    }) == Some(true);
                    assert_eq!(delta.is_empty(), !keep);
                    let mut seen = 0;
                    GraphSetPredicateOp::evaluate_row_with_control(
                        &code,
                        &row(Some(1)),
                        &mut |_| {
                            seen += 1;
                            Ok::<_, usize>(())
                        },
                    )
                    .unwrap();
                    assert_eq!(seen, code.len()); // Even FALSE AND ... / TRUE OR ... are eager.
                }
            }
        }
    }
    let code = vec![
        GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(0),
            comparison: IntegerComparison::Equal,
            right: GraphSetOperand::Column(1),
        },
        GraphSetPredicateOp::Not,
    ];
    let definition = RowProjectionSpec::selection(
        vec![GraphSetColumnType::Scalar; 2],
        vec!["a".into(), "b".into()],
        &code,
    )
    .unwrap();
    let text = CanonicalScalar::ucs_basic_text("1").unwrap();
    let rows = [
        vec![CanonicalScalar::Int(1), text],
        vec![CanonicalScalar::Int(1), CanonicalScalar::Null],
        vec![CanonicalScalar::Int(1), CanonicalScalar::Int(2)],
    ];
    let input = ZSet::from_updates(
        rows.into_iter().map(|values| {
            (
                GraphValueRow::from_owned_values(
                    values.into_iter().map(GraphValue::Scalar).collect(),
                ),
                ZWeight::ONE,
            )
        }),
        LIMBS,
        &mut allow,
    )
    .unwrap();
    let mut state = IncrementalRowProjection::new(definition);
    apply(&mut state, &input);
    assert_eq!(state.rows().len(), 1);
    assert_eq!(
        state.rows().iter().next().unwrap().0.values()[1],
        GraphValue::Scalar(CanonicalScalar::Int(2))
    );
}

#[test]
fn every_selection_checkpoint_drop_and_unwind_preserves_hidden_and_visible_state() {
    for q in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
        let seed = || {
            let mut state = IncrementalRowProjection::new(spec(q));
            apply(&mut state, &z(&[(None, 3), (Some(2), 2)]));
            state
        };
        let changes = z(&[(None, -2), (Some(2), -2), (Some(4), 3), (Some(-1), 1)]);
        let mut success = seed();
        let mut calls = 0;
        let expected = success
            .prepare(&changes, LIMBS, None, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap()
            .commit();
        for stop in 1..=calls {
            let mut state = seed();
            let mut seen = 0;
            assert_eq!(
                state
                    .prepare(&changes, LIMBS, None, &mut |_| {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    })
                    .unwrap_err(),
                RowProjectionError::Delta(ZSetError::Control(stop))
            );
            assert_eq!(state, seed());
            assert_eq!(apply(&mut state, &changes), expected);
            assert_eq!(state, success);
        }
        let mut state = seed();
        {
            let pending = state.prepare(&changes, LIMBS, None, &mut allow).unwrap();
            assert_eq!(pending.delta(), &expected);
        }
        assert_eq!(state, seed());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _pending = state.prepare(&changes, LIMBS, None, &mut allow).unwrap();
            panic!("downstream refusal");
        }));
        assert_eq!(state, seed());
    }
}

#[test]
fn filter_work_depends_on_changed_keys_not_hidden_support_or_multiplicity() {
    let mut observations = Vec::new();
    for size in [8, 1024] {
        for count in [1, 1_000_000] {
            let mut state = IncrementalRowProjection::new(spec(GraphSetQuantifier::All));
            let mut input = (0..size).map(|value| (Some(value), 1)).collect::<Vec<_>>();
            input.push((None, count));
            apply(&mut state, &z(&input));
            let mut events = [0; 2];
            state
                .prepare(
                    &z(&[(None, -count), (Some(2048), count)]),
                    LIMBS,
                    None,
                    &mut |event| {
                        events[match event {
                            ZSetEvent::Work => 0,
                            ZSetEvent::ScratchEntry => 1,
                        }] += 1;
                        Ok::<_, usize>(())
                    },
                )
                .unwrap()
                .commit();
            observations.push(events);
        }
    }
    assert!(observations.iter().all(|events| *events == observations[0]));
}
