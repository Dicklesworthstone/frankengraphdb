use super::*;
use crate::GraphSetOperand;
use crate::algebra::IntegerComparison;
use fgdb_types::{CanonicalScalar, VId};

const LIMBS: LimbLimit = LimbLimit::new(16);
const KINDS: [RowJoinKind; 6] = [
    RowJoinKind::Inner, RowJoinKind::Left, RowJoinKind::Right,
    RowJoinKind::Full, RowJoinKind::Semi, RowJoinKind::Anti,
];

fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }

fn row(values: &[i64]) -> GraphValueRow {
    GraphValueRow::from_owned_values(values.iter()
        .map(|value| GraphValue::Scalar(CanonicalScalar::Int(*value))).collect())
}
fn bag(rows: &[(GraphValueRow, i128)]) -> ZSet<GraphValueRow> {
    ZSet::from_updates(rows.iter().map(|(row, count)| (row.clone(), ZWeight::from_i128(*count))),
        LIMBS, &mut allow).unwrap()
}
fn less(left: usize, right: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(left), comparison: IntegerComparison::Less,
        right: GraphSetOperand::Column(right),
    }
}
fn spec(kind: RowJoinKind, keyed: bool) -> RowJoinSpec {
    let schema = [GraphSetColumnType::Scalar; 2];
    let spec = if keyed {
        RowJoinSpec::new(&schema, &schema, &[(0, 0)])
    } else { RowJoinSpec::cross(&schema, &schema) }.unwrap();
    spec.with_kind(kind).with_predicate(&[less(1, 3)]).unwrap()
}
fn tuples(mask: u8, left: bool) -> Vec<(GraphValueRow, i128)> {
    let values = if left { [1, 5] } else { [4, 8] };
    (0..2).map(|i| (row(&[1, values[i]]),
        if mask & (1 << i) == 0 { 0 } else { (i + 2) as i128 })).collect()
}

// Full primitive-count recomputation, not the derivative or shared predicate IR.
fn oracle(kind: RowJoinKind, keyed: bool, left: &[(GraphValueRow, i128)],
    right: &[(GraphValueRow, i128)]) -> ZSet<GraphValueRow> {
    let scalar = |row: &GraphValueRow, at: usize| match &row.values()[at] {
        GraphValue::Scalar(CanonicalScalar::Int(value)) => *value,
        _ => panic!("integer oracle input"),
    };
    let matches = |l: &GraphValueRow, r: &GraphValueRow| {
        (!keyed || scalar(l, 0) == scalar(r, 0)) && scalar(l, 1) < scalar(r, 1)
    };
    let joined = |l: Option<&GraphValueRow>, r: Option<&GraphValueRow>| {
        let mut values = Vec::new();
        for side in [l, r] {
            if let Some(side) = side { values.extend_from_slice(side.values()); }
            else { values.extend((0..2).map(|_| GraphValue::Scalar(CanonicalScalar::Null))); }
        }
        GraphValueRow::from_owned_values(values)
    };
    let mut output = Vec::new();
    for (l, lw) in left.iter().filter(|(_, count)| *count > 0) {
        let witnesses: Vec<_> = right.iter().filter(|(r, rw)| *rw > 0 && matches(l, r)).collect();
        if matches!(kind, RowJoinKind::Inner | RowJoinKind::Left | RowJoinKind::Right | RowJoinKind::Full) {
            for (r, rw) in &witnesses { output.push((joined(Some(l), Some(r)), lw * rw)); }
        }
        match kind {
            RowJoinKind::Left | RowJoinKind::Full if witnesses.is_empty() => {
                output.push((joined(Some(l), None), *lw));
            }
            RowJoinKind::Semi if !witnesses.is_empty() => output.push((l.clone(), *lw)),
            RowJoinKind::Anti if witnesses.is_empty() => output.push((l.clone(), *lw)),
            _ => {}
        }
    }
    if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
        for (r, rw) in right.iter().filter(|(_, count)| *count > 0) {
            if !left.iter().any(|(l, lw)| *lw > 0 && matches(l, r)) {
                output.push((joined(None, Some(r)), *rw));
            }
        }
    }
    bag(&output)
}

#[test]
fn all_join_kinds_match_full_recomputation_across_every_small_bag_transition() {
    for kind in KINDS {
        for keyed in [false, true] {
            for old in 0_u8..16 {
                for new in 0_u8..16 {
                    let left = tuples(old & 3, true);
                    let right = tuples(old >> 2, false);
                    let next_left = tuples(new & 3, true);
                    let next_right = tuples(new >> 2, false);
                    let mut join = IncrementalRowJoin::new(spec(kind, keyed));
                    join.prepare(&bag(&left), &bag(&right), LIMBS, None, &mut allow).unwrap().commit();
                    let before = oracle(kind, keyed, &left, &right);
                    assert_eq!(join.rows(), &before);
                    let dl = bag(&next_left).minus(&bag(&left), LIMBS, &mut allow).unwrap();
                    let dr = bag(&next_right).minus(&bag(&right), LIMBS, &mut allow).unwrap();
                    let expected = oracle(kind, keyed, &next_left, &next_right);
                    let change = join.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap().commit();
                    assert_eq!(change, expected.minus(&before, LIMBS, &mut allow).unwrap());
                    assert_eq!(join.rows(), &expected);
                    assert_eq!(join.total(), &expected.total_weight(LIMBS, &mut allow).unwrap());
                }
            }
        }
    }
}

#[test]
fn outer_on_is_not_a_post_filter_and_witnesses_are_row_specific() {
    let left = vec![(row(&[1, 1]), 2), (row(&[1, 5]), 3), (row(&[2, 1]), 4)];
    let right = vec![(row(&[1, 4]), 3), (row(&[1, 8]), 2), (row(&[3, 0]), 1)];
    for kind in KINDS {
        for keyed in [false, true] {
            let mut join = IncrementalRowJoin::new(spec(kind, keyed));
            join.prepare(&bag(&left), &bag(&right), LIMBS, None, &mut allow).unwrap().commit();
            assert_eq!(join.rows(), &oracle(kind, keyed, &left, &right));
            // Only the greater left payload loses its last accepted witness.
            let delta = bag(&[(row(&[1, 8]), -2)]);
            join.prepare(&ZSet::new(), &delta, LIMBS, None, &mut allow).unwrap().commit();
            assert_eq!(join.rows(), &oracle(kind, keyed, &left, &[right[0].clone(), right[2].clone()]));
        }
    }
}

#[test]
fn zero_column_theta_joins_consolidate_identical_full_outer_extensions() {
    for kind in KINDS {
        for truth in [None, Some(false), Some(true)] {
            let spec = RowJoinSpec::cross(&[], &[]).unwrap()
                .with_predicate(&[GraphSetPredicateOp::Truth(truth)]).unwrap().with_kind(kind);
            assert!(!spec.is_cross());
            let mut join = IncrementalRowJoin::new(spec);
            let count = match (kind, truth == Some(true)) {
                (RowJoinKind::Inner, false) | (RowJoinKind::Semi, false) | (RowJoinKind::Anti, true) => 0,
                (RowJoinKind::Semi, true) | (RowJoinKind::Left, false) | (RowJoinKind::Anti, false) => 2,
                (RowJoinKind::Right, false) => 3,
                (RowJoinKind::Full, false) => 5,
                _ => 6,
            };
            join.prepare(&bag(&[(row(&[]), 2)]), &bag(&[(row(&[]), 3)]), LIMBS,
                Some(count), &mut allow).unwrap().commit();
            assert_eq!(join.rows(), &bag(&[(row(&[]), i128::from(count))]));
            join.prepare(&bag(&[(row(&[]), -2)]), &bag(&[(row(&[]), -3)]), LIMBS,
                Some(0), &mut allow).unwrap().commit();
            assert!(join.rows().is_empty());
        }
    }
}

#[test]
fn null_keys_never_match_and_null_predicate_payloads_remain_unknown_under_not() {
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let keyed_row = GraphValueRow::from_owned_values(vec![null.clone(), GraphValue::Scalar(CanonicalScalar::Int(1))]);
    for kind in KINDS {
        let definition = RowJoinSpec::new(&[GraphSetColumnType::Scalar; 2],
            &[GraphSetColumnType::Scalar; 2], &[(0, 0)]).unwrap()
            .with_kind(kind).with_predicate(&[GraphSetPredicateOp::Truth(Some(true))]).unwrap();
        let mut join = IncrementalRowJoin::new(definition);
        join.prepare(&bag(&[(keyed_row.clone(), 2)]), &bag(&[(keyed_row.clone(), 3)]),
            LIMBS, None, &mut allow).unwrap().commit();
        let expected_total = match kind {
            RowJoinKind::Inner | RowJoinKind::Semi => 0,
            RowJoinKind::Left | RowJoinKind::Anti => 2,
            RowJoinKind::Right => 3,
            RowJoinKind::Full => 5,
        };
        assert_eq!(join.total(), &ZWeight::from_i128(expected_total));
    }
    let schema = [GraphSetColumnType::Scalar];
    let code = [less(0, 1), GraphSetPredicateOp::Not];
    let mut join = IncrementalRowJoin::new(RowJoinSpec::cross(&schema, &schema).unwrap()
        .with_predicate(&code).unwrap().with_kind(RowJoinKind::Left));
    let l = row(&[1]);
    for value in [null.clone(), GraphValue::Scalar(CanonicalScalar::Bool(true))] {
        let r = GraphValueRow::from_owned_values(vec![value]);
        join.prepare(&bag(&[(l.clone(), 1)]), &bag(&[(r, 1)]), LIMBS, None, &mut allow).unwrap().commit();
    }
    let padded = GraphValueRow::from_owned_values(vec![l.values()[0].clone(), null]);
    assert_eq!(join.rows(), &bag(&[(padded, 2)]));
}

#[test]
fn borrowed_pair_evaluation_matches_flattened_values_and_control_events_at_every_split() {
    let flat = GraphValueRow::from_owned_values(vec![
        GraphValue::Scalar(CanonicalScalar::Int(4)), GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::Scalar(CanonicalScalar::Bool(true)), GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Int(9))].into_boxed_slice()),
    ]);
    for a in 0..flat.len() {
        for b in 0..flat.len() {
            let code = [less(a, b), GraphSetPredicateOp::Not,
                GraphSetPredicateOp::IsNull { operand: GraphSetOperand::Column(b), is_null: true },
                GraphSetPredicateOp::Or];
            GraphSetPredicateOp::validate_schema(&vec![GraphSetColumnType::Any; flat.len()], &code).unwrap();
            let mut expected_events = Vec::new();
            let expected = GraphSetPredicateOp::evaluate_row_with_control(&code, &flat, &mut |event| {
                expected_events.push(event); Ok::<_, usize>(())
            }).unwrap();
            for split in 0..=flat.len() {
                let left = GraphValueRow::from_owned_values(flat.values()[..split].to_vec());
                let right = GraphValueRow::from_owned_values(flat.values()[split..].to_vec());
                let mut events = Vec::new();
                let actual = GraphSetPredicateOp::evaluate_pair_with_control(&code, &left, &right, &mut |event| {
                    events.push(event); Ok::<_, usize>(())
                }).unwrap();
                assert_eq!((actual, events), (expected, expected_events.clone()));
            }
        }
    }
}

#[test]
fn definition_and_hidden_input_admission_cannot_be_bypassed_by_false_on() {
    let definition = RowJoinSpec::cross(&[GraphSetColumnType::Scalar], &[GraphSetColumnType::Scalar]).unwrap();
    assert!(matches!(definition.clone().with_predicate(&[]), Err(RowJoinBuildError::Predicate(GraphSetFilterError::Empty))));
    assert!(matches!(definition.clone().with_predicate(&[GraphSetPredicateOp::Not]), Err(RowJoinBuildError::Predicate(GraphSetFilterError::InvalidStack { .. }))));
    assert!(matches!(definition.clone().with_predicate(&[less(0, 2)]), Err(RowJoinBuildError::Predicate(GraphSetFilterError::UnknownInput { .. }))));
    for kind in KINDS {
        let definition = definition.clone().with_kind(kind)
            .with_predicate(&[GraphSetPredicateOp::Truth(Some(false))]).unwrap();
        let mut join = IncrementalRowJoin::new(definition.clone());
        for side in 0..2 {
            let mut deltas = [ZSet::new(), ZSet::new()];
            deltas[side] = bag(&[(row(&[1]), -1)]);
            assert_eq!(join.prepare(&deltas[0], &deltas[1], LIMBS, Some(0), &mut allow).unwrap_err(),
                RowJoinError::NegativeMultiplicity { side });
            assert_eq!(join, IncrementalRowJoin::new(definition.clone()));
            deltas[side] = bag(&[(row(&[1, 2]), 1)]);
            assert_eq!(join.prepare(&deltas[0], &deltas[1], LIMBS, Some(0), &mut allow).unwrap_err(),
                RowJoinError::InputSchema { side });
        }
    }
}

#[test]
fn selection_and_existence_do_not_multiply_rejected_or_witness_occurrences() {
    let schema = [GraphSetColumnType::Scalar];
    for (kind, truth, expected) in [
        (RowJoinKind::Inner, false, 0), (RowJoinKind::Left, false, 2),
        (RowJoinKind::Semi, true, 2), (RowJoinKind::Anti, true, 0),
    ] {
        let definition = RowJoinSpec::cross(&schema, &schema).unwrap().with_kind(kind)
            .with_predicate(&[GraphSetPredicateOp::Truth(Some(truth))]).unwrap();
        let mut join = IncrementalRowJoin::new(definition);
        join.prepare(&bag(&[(row(&[1]), 2)]), &bag(&[(row(&[2]), i128::MAX)]),
            LimbLimit::new(0), Some(expected), &mut allow).unwrap().commit();
        assert_eq!(join.total(), &ZWeight::from_i128(i128::from(expected)));
    }
    let definition = RowJoinSpec::cross(&schema, &schema).unwrap()
        .with_predicate(&[GraphSetPredicateOp::Truth(Some(true))]).unwrap();
    let mut join = IncrementalRowJoin::new(definition.clone());
    assert!(matches!(join.prepare(&bag(&[(row(&[1]), 2)]), &bag(&[(row(&[2]), i128::MAX)]),
        LimbLimit::new(0), None, &mut allow), Err(RowJoinError::Delta(ZSetError::Arithmetic(_)))));
    assert_eq!(join, IncrementalRowJoin::new(definition));
}

fn seeded(kind: RowJoinKind) -> IncrementalRowJoin {
    let mut join = IncrementalRowJoin::new(spec(kind, true));
    join.prepare(&bag(&[(row(&[1, 1]), 2), (row(&[1, 5]), 1)]),
        &bag(&[(row(&[1, 4]), 3)]), LIMBS, None, &mut allow).unwrap().commit();
    join
}

#[test]
fn every_refusal_and_dropped_guard_preserves_inputs_output_and_total_and_can_retry() {
    let left = bag(&[(row(&[1, 1]), -1), (row(&[2, 1]), 2)]);
    let right = bag(&[(row(&[1, 4]), -3), (row(&[1, 8]), 2), (row(&[2, 4]), 1)]);
    for kind in KINDS {
        let mut success = seeded(kind);
        let mut calls = 0;
        let expected = success.prepare(&left, &right, LIMBS, None, &mut |_| {
            calls += 1; Ok::<_, usize>(())
        }).unwrap().commit();
        assert!(calls > 0);
        for stop in 0..calls {
            let mut join = seeded(kind);
            let mut at = 0;
            let error = join.prepare(&left, &right, LIMBS, None, &mut |_| {
                let current = at; at += 1;
                if current == stop { Err(stop) } else { Ok(()) }
            }).unwrap_err();
            assert_eq!(error, RowJoinError::Delta(ZSetError::Control(stop)));
            assert_eq!(join, seeded(kind));
            assert_eq!(join.prepare(&left, &right, LIMBS, None, &mut allow).unwrap().commit(), expected);
            assert_eq!(join, success);
        }
        let mut join = seeded(kind);
        drop(join.prepare(&left, &right, LIMBS, None, &mut allow).unwrap());
        assert_eq!(join, seeded(kind));
        if success.total() > &ZWeight::ZERO {
            assert_eq!(join.prepare(&left, &right, LIMBS, Some(0), &mut allow).unwrap_err(),
                RowJoinError::ResultBudget { limit: 0 });
            assert_eq!(join, seeded(kind));
        }
    }
}

#[test]
fn unrelated_groups_do_not_change_predicate_work_or_delta() {
    let extra_left: Vec<_> = (100..200).map(|key| (row(&[key, 1]), 2)).collect();
    let extra_right: Vec<_> = (100..200).map(|key| (row(&[key, 4]), 3)).collect();
    for kind in KINDS {
        let mut small = seeded(kind);
        let mut large = seeded(kind);
        large.prepare(&bag(&extra_left), &bag(&extra_right), LIMBS, None, &mut allow).unwrap().commit();
        let mut results = Vec::new();
        for join in [&mut small, &mut large] {
            let mut events = Vec::new();
            let delta = join.prepare(&bag(&[(row(&[1, 1]), 1)]), &ZSet::new(), LIMBS, None, &mut |event| {
                events.push(event); Ok::<_, usize>(())
            }).unwrap().commit();
            results.push((delta, events));
        }
        assert_eq!(results[0], results[1]);
    }
}
