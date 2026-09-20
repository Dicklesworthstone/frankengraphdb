use super::*;
use crate::algebra::{GraphValue, IntegerComparison};
use crate::{GqlScalarParameter, GraphSetOperand};
use fgdb_types::{CanonicalScalar, VId};

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn row(value: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
        value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
    )])
}
fn z(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.into_iter()
            .map(|(row, n)| (row, ZWeight::from_i128(n))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn positive() -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(0),
        comparison: IntegerComparison::Greater,
        right: GraphSetOperand::Literal(GqlScalarParameter::new(CanonicalScalar::Int(0)).unwrap()),
    }
}
fn spec(code: &[GraphSetPredicateOp]) -> RowFilterSpec {
    RowFilterSpec::new(vec![GraphSetColumnType::Scalar], code).unwrap()
}
fn state(code: &[GraphSetPredicateOp], input: &ZSet<GraphValueRow>) -> IncrementalRowFilter {
    let mut state = IncrementalRowFilter::new(spec(code));
    state
        .prepare(input, LIMBS, None, &mut allow)
        .unwrap()
        .commit();
    state
}

#[test]
fn every_small_bag_transition_matches_independent_three_valued_selection() {
    use GraphSetPredicateOp as Op;
    let null = Op::IsNull {
        operand: GraphSetOperand::Column(0),
        is_null: true,
    };
    let cases = [
        vec![Op::Truth(Some(true))],
        vec![Op::Truth(Some(false))],
        vec![Op::Truth(None)],
        vec![Op::Truth(None), Op::Not],
        vec![positive()],
        vec![positive(), Op::Not],
        vec![null.clone()],
        vec![positive(), null.clone(), Op::Or],
        vec![positive(), null, Op::And],
    ];
    let domain = [None, Some(-1), Some(2)];
    let bag = |n: usize| {
        z(domain
            .iter()
            .enumerate()
            .map(|(i, value)| (row(*value), ((n / 3usize.pow(i as u32)) % 3) as i128)))
    };
    for (mode, code) in cases.iter().enumerate() {
        let oracle = |n: usize| {
            z(domain.iter().enumerate().filter_map(|(i, value)| {
                let keep = match mode {
                    0 => true,
                    1..=3 | 8 => false,
                    4 => value.is_some_and(|n| n > 0),
                    5 => value.is_some_and(|n| n <= 0),
                    6 => value.is_none(),
                    7 => value.is_none_or(|n| n > 0),
                    _ => unreachable!(),
                };
                keep.then(|| (row(*value), ((n / 3usize.pow(i as u32)) % 3) as i128))
            }))
        };
        for before in 0..27 {
            for after in 0..27 {
                let initial = bag(before);
                let next = bag(after);
                let change = next
                    .plus(
                        &initial.negated(LIMBS, &mut allow).unwrap(),
                        LIMBS,
                        &mut allow,
                    )
                    .unwrap();
                let mut filter = state(code, &initial);
                assert_eq!(filter.rows(), &oracle(before));
                let delta = filter
                    .prepare(&change, LIMBS, None, &mut allow)
                    .unwrap()
                    .commit();
                let mut integrated = oracle(before);
                integrated.integrate(&delta, LIMBS, &mut allow).unwrap();
                assert_eq!(integrated, oracle(after), "mode {mode}, {before}->{after}");
                assert_eq!(filter.rows(), &integrated);
                assert_eq!(
                    filter.total(),
                    &integrated.total_weight(LIMBS, &mut allow).unwrap()
                );
                assert_eq!(filter.input, next);
                let reverse = filter
                    .prepare(
                        &change.negated(LIMBS, &mut allow).unwrap(),
                        LIMBS,
                        None,
                        &mut allow,
                    )
                    .unwrap()
                    .commit();
                assert_eq!(reverse, delta.negated(LIMBS, &mut allow).unwrap());
                assert_eq!(filter, state(code, &initial));
            }
        }
    }
}

#[test]
fn null_mixed_scalar_and_full_width_identity_rules_are_not_set_equality() {
    use GraphSetPredicateOp as Op;
    let scalar_rows = z([
        (row(None), 3),
        (row(Some(0)), 5),
        (row(Some(2)), 7),
        (
            GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Bool(true))]),
            11,
        ),
        (
            GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                CanonicalScalar::ucs_basic_text("2").unwrap(),
            )]),
            13,
        ),
    ]);
    let filter = state(&[positive(), Op::Not], &scalar_rows);
    assert_eq!(filter.rows(), &z([(row(Some(0)), 5)])); // UNKNOWN remains UNKNOWN under NOT.
    let id = VId(u128::MAX);
    let identity_rows = z([
        (
            GraphValueRow::from_owned_values(vec![GraphValue::Vertex(id), GraphValue::Vertex(id)]),
            2,
        ),
        (
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(id),
                GraphValue::Vertex(VId(0)),
            ]),
            3,
        ),
        (
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(id),
                GraphValue::Scalar(CanonicalScalar::Null),
            ]),
            5,
        ),
    ]);
    let code = [Op::Compare {
        left: GraphSetOperand::Column(0),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(1),
    }];
    let mut filter = IncrementalRowFilter::new(
        RowFilterSpec::new(vec![GraphSetColumnType::Vertex; 2], &code).unwrap(),
    );
    filter
        .prepare(&identity_rows, LIMBS, Some(2), &mut allow)
        .unwrap()
        .commit();
    assert_eq!(filter.rows().len(), 1);
    assert_eq!(filter.total(), &ZWeight::from_i128(2));
    let list = GraphValueRow::from_owned_values(vec![GraphValue::List(
        vec![GraphValue::Vertex(id)].into(),
    )]);
    let code = [Op::IsNull {
        operand: GraphSetOperand::Column(0),
        is_null: false,
    }];
    let mut list_filter = IncrementalRowFilter::new(
        RowFilterSpec::new(vec![GraphSetColumnType::Any], &code).unwrap(),
    );
    list_filter
        .prepare(
            &z([(list.clone(), 4), (row(None), 6)]),
            LIMBS,
            None,
            &mut allow,
        )
        .unwrap()
        .commit();
    assert_eq!(list_filter.rows(), &z([(list, 4)]));
}

#[test]
fn malformed_and_negative_rejected_inputs_cannot_hide_behind_false_or_zero_budget() {
    use GraphSetPredicateOp as Op;
    assert!(matches!(
        RowFilterSpec::new(
            vec![GraphSetColumnType::Scalar; MAX_PATTERN_VERTICES + 1],
            &[Op::Truth(Some(false))]
        ),
        Err(RowFilterBuildError::InputWidth { .. })
    ));
    assert!(matches!(
        RowFilterSpec::new(vec![], &[Op::Not]),
        Err(RowFilterBuildError::Predicate(
            GraphSetFilterError::InvalidStack { .. }
        ))
    ));
    assert!(matches!(
        RowFilterSpec::new(
            vec![],
            &[
                Op::Truth(Some(false)),
                Op::IsNull {
                    operand: GraphSetOperand::Column(0),
                    is_null: true
                },
                Op::And
            ]
        ),
        Err(RowFilterBuildError::Predicate(
            GraphSetFilterError::UnknownInput { .. }
        ))
    ));
    let code = [Op::Truth(Some(false))];
    let initial = z([(row(Some(1)), 2)]);
    let mut filter = state(&code, &initial);
    assert!(matches!(
        filter.prepare(
            &z([(row(Some(1)), -3), (row(Some(2)), 3)]),
            LIMBS,
            Some(0),
            &mut allow
        ),
        Err(RowFilterError::NegativeMultiplicity)
    ));
    assert_eq!(filter, state(&code, &initial));
    let wrong = GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1))]);
    assert!(matches!(
        filter.prepare(&z([(wrong, 1)]), LIMBS, Some(0), &mut allow),
        Err(RowFilterError::InputSchema)
    ));
    assert!(matches!(
        filter.prepare(&z([(GraphValueRow::unit(), 1)]), LIMBS, Some(0), &mut allow),
        Err(RowFilterError::InputSchema)
    ));
    filter
        .prepare(
            &z([(row(Some(1)), -2), (row(Some(9)), 4)]),
            LIMBS,
            Some(0),
            &mut allow,
        )
        .unwrap()
        .commit();
    assert!(filter.rows().is_empty());
    assert_eq!(filter.input, z([(row(Some(9)), 4)]));
    let mut singleton =
        IncrementalRowFilter::new(RowFilterSpec::new(vec![], &[Op::Truth(Some(true))]).unwrap());
    singleton
        .prepare(&z([(GraphValueRow::unit(), 1)]), LIMBS, Some(1), &mut allow)
        .unwrap()
        .commit();
    assert_eq!(singleton.rows().len(), 1);
}

#[test]
fn every_checkpoint_drop_unwind_and_exact_budget_preserves_atomic_retry() {
    let code = [positive()];
    let initial = z([(row(Some(-1)), 3), (row(Some(5)), 2)]);
    let change = z([(row(Some(-1)), -2), (row(Some(5)), -2), (row(Some(3)), 4)]);
    let mut success = state(&code, &initial);
    let mut events = Vec::new();
    let wanted = success
        .prepare(&change, LIMBS, Some(4), &mut |event| {
            events.push(event);
            Ok::<_, usize>(())
        })
        .unwrap()
        .commit();
    for stop in 1..=events.len() {
        let mut filter = state(&code, &initial);
        let mut calls = 0;
        let error = filter
            .prepare(&change, LIMBS, Some(4), &mut |_| {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            })
            .unwrap_err();
        assert_eq!(error, RowFilterError::Delta(ZSetError::Control(stop)));
        assert_eq!(filter, state(&code, &initial));
        assert_eq!(
            filter
                .prepare(&change, LIMBS, Some(4), &mut allow)
                .unwrap()
                .commit(),
            wanted
        );
        assert_eq!(filter, success);
    }
    let mut filter = state(&code, &initial);
    {
        let pending = filter.prepare(&change, LIMBS, Some(4), &mut allow).unwrap();
        assert_eq!(pending.delta(), &wanted);
    }
    assert_eq!(filter, state(&code, &initial));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _pending = filter.prepare(&change, LIMBS, Some(4), &mut allow).unwrap();
        std::panic::resume_unwind(Box::new("downstream unwind"));
    }));
    assert!(caught.is_err());
    assert_eq!(filter, state(&code, &initial));
    for dimension in [ZSetEvent::Work, ZSetEvent::ScratchEntry] {
        let exact = events.iter().filter(|&&event| event == dimension).count();
        for limit in [exact, exact - 1] {
            let mut filter = state(&code, &initial);
            let mut seen = 0;
            let result = filter
                .prepare(&change, LIMBS, Some(4), &mut |event| {
                    if event == dimension {
                        seen += 1;
                    }
                    if seen > limit { Err(999) } else { Ok(()) }
                })
                .map(|pending| pending.commit());
            if limit == exact {
                assert_eq!(result.unwrap(), wanted);
            } else {
                assert!(result.is_err());
                assert_eq!(filter, state(&code, &initial));
            }
        }
    }
}

#[test]
fn final_occurrence_quota_and_promoted_totals_never_expand_a_bag() {
    let code = [positive()];
    let initial = z([(row(Some(9)), 2)]);
    let change = z([(row(Some(1)), 2), (row(Some(9)), -2)]);
    let mut filter = state(&code, &initial);
    assert!(matches!(
        filter.prepare(&change, LIMBS, Some(1), &mut allow),
        Err(RowFilterError::ResultBudget { limit: 1 })
    ));
    assert_eq!(filter, state(&code, &initial));
    filter
        .prepare(&change, LIMBS, Some(2), &mut allow)
        .unwrap()
        .commit();
    assert_eq!(filter.rows(), &z([(row(Some(1)), 2)]));
    let huge = z([(row(Some(1)), i128::MAX), (row(Some(2)), 1)]);
    let mut filter = IncrementalRowFilter::new(spec(&code));
    assert!(matches!(
        filter.prepare(&huge, LimbLimit::new(0), None, &mut allow),
        Err(RowFilterError::Delta(ZSetError::Arithmetic(_)))
    ));
    assert_eq!(filter, IncrementalRowFilter::new(spec(&code)));
    filter
        .prepare(&huge, LIMBS, None, &mut allow)
        .unwrap()
        .commit();
    assert!(filter.total().is_promoted());
    assert_eq!(filter.rows().len(), 2);
    filter
        .prepare(
            &huge.negated(LIMBS, &mut allow).unwrap(),
            LIMBS,
            Some(0),
            &mut allow,
        )
        .unwrap()
        .commit();
    assert_eq!(filter, IncrementalRowFilter::new(spec(&code)));
}

#[test]
fn maintenance_work_ignores_unrelated_rows_and_occurrence_expansion() {
    let code = [positive()];
    let mut reference = None;
    for extra in [0, 512] {
        for multiplicity in [1, 1_i128 << 60] {
            let initial = z(std::iter::once((row(Some(2)), multiplicity))
                .chain((100..100 + extra).map(|n| (row(Some(n)), 1))));
            let mut filter = state(&code, &initial);
            let change = z([(row(Some(2)), -multiplicity), (row(Some(3)), multiplicity)]);
            let mut events = Vec::new();
            filter
                .prepare(&change, LIMBS, None, &mut |event| {
                    events.push(event);
                    Ok::<_, usize>(())
                })
                .unwrap()
                .commit();
            match &reference {
                Some(wanted) => assert_eq!(&events, wanted),
                None => reference = Some(events),
            }
        }
    }
}
