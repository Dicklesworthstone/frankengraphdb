//! Complete-bag reference checks for right/full incremental joins.
//! The oracle enumerates final bags, not derivatives or witness kernels.
use super::*;
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn row(key: Option<i64>, payload: i64) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![scalar(key), scalar(Some(payload))])
}
fn z(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.into_iter().map(|(row, count)| (row, ZWeight::from_i128(count))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn bag(code: usize, payload: i64) -> ZSet<GraphValueRow> {
    z([
        (row(Some(7), payload), (code % 3) as i128),
        (row(None, payload + 1), (code / 3) as i128),
    ])
}
fn spec(kind: RowJoinKind, cross: bool) -> RowJoinSpec {
    let columns = [GraphSetColumnType::Scalar; 2];
    let spec = if cross {
        RowJoinSpec::cross(&columns, &columns)
    } else {
        RowJoinSpec::new(&columns, &columns, &[(0, 0)])
    };
    spec.unwrap().with_kind(kind)
}
fn seed(
    spec: &RowJoinSpec,
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
) -> IncrementalRowJoin {
    let mut state = IncrementalRowJoin::new(spec.clone());
    state.prepare(left, right, LIMBS, None, &mut allow).unwrap().commit();
    state
}
fn joined(left: Option<&GraphValueRow>, right: Option<&GraphValueRow>) -> GraphValueRow {
    let mut values = match left {
        Some(row) => row.values().to_vec(),
        None => vec![scalar(None), scalar(None)],
    };
    match right {
        Some(row) => values.extend_from_slice(row.values()),
        None => values.extend([scalar(None), scalar(None)]),
    }
    GraphValueRow::from_owned_values(values)
}
fn oracle(
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
    kind: RowJoinKind,
    cross: bool,
) -> ZSet<GraphValueRow> {
    let matches = |l: &GraphValueRow, r: &GraphValueRow| {
        cross || (!l.values()[0].is_null() && l.values()[0] == r.values()[0])
    };
    let mut result = BTreeMap::new();
    for (l, lw) in left.iter() {
        let mut found = false;
        for (r, rw) in right.iter() {
            if matches(l, r) {
                found = true;
                *result.entry(joined(Some(l), Some(r))).or_insert(0) +=
                    lw.to_i128().unwrap() * rw.to_i128().unwrap();
            }
        }
        if !found && kind == RowJoinKind::Full {
            *result.entry(joined(Some(l), None)).or_insert(0) += lw.to_i128().unwrap();
        }
    }
    for (r, rw) in right.iter() {
        if !left.iter().any(|(l, _)| matches(l, r)) {
            *result.entry(joined(None, Some(r))).or_insert(0) += rw.to_i128().unwrap();
        }
    }
    z(result)
}

#[test]
fn every_small_outer_transition_matches_recomputation_and_exact_inverse() {
    for kind in [RowJoinKind::Right, RowJoinKind::Full] {
        for cross in [false, true] {
            let spec = spec(kind, cross);
            assert_eq!(spec.width(), 4);
            assert_eq!(spec.column_types().collect::<Vec<_>>(), vec![GraphSetColumnType::Scalar; 4]);
            for before in 0..81 {
                let (left, right) = (bag(before % 9, 10), bag(before / 9, 20));
                let previous = oracle(&left, &right, kind, cross);
                for after in 0..81 {
                    let (next_left, next_right) = (bag(after % 9, 10), bag(after / 9, 20));
                    let expected = oracle(&next_left, &next_right, kind, cross);
                    let dl = next_left.minus(&left, LIMBS, &mut allow).unwrap();
                    let dr = next_right.minus(&right, LIMBS, &mut allow).unwrap();
                    let mut state = seed(&spec, &left, &right);
                    assert_eq!(state.rows(), &previous);
                    let delta = state.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap().commit();
                    assert_eq!(delta, expected.minus(&previous, LIMBS, &mut allow).unwrap());
                    assert_eq!(state.rows(), &expected, "{kind:?}, cross={cross}: {before}->{after}");
                    assert_eq!(state.total(), &expected.total_weight(LIMBS, &mut allow).unwrap());
                    let inverse = state.prepare(
                        &dl.negated(LIMBS, &mut allow).unwrap(),
                        &dr.negated(LIMBS, &mut allow).unwrap(),
                        LIMBS,
                        None,
                        &mut allow,
                    ).unwrap().commit();
                    assert_eq!(inverse, delta.negated(LIMBS, &mut allow).unwrap());
                    assert_eq!(state, seed(&spec, &left, &right));
                }
            }
        }
    }
}

#[test]
fn right_and_full_keep_asymmetric_schema_order_and_native_payloads() {
    let id = GraphValue::Vertex(VId(u128::MAX));
    let payload = GraphValue::List(vec![scalar(Some(9)), id.clone()].into_boxed_slice());
    let left = GraphValueRow::from_owned_values(vec![payload.clone(), id.clone(), scalar(Some(11))]);
    let right = GraphValueRow::from_owned_values(vec![id.clone(), scalar(Some(22))]);
    let expected = GraphValueRow::from_owned_values(vec![payload, id.clone(), scalar(Some(11)), id.clone(), scalar(Some(22))]);
    let unmatched = GraphValueRow::from_owned_values(vec![scalar(None), scalar(None), scalar(None), id, scalar(Some(22))]);
    for kind in [RowJoinKind::Right, RowJoinKind::Full] {
        let spec = RowJoinSpec::new(
            &[GraphSetColumnType::List, GraphSetColumnType::Vertex, GraphSetColumnType::Scalar],
            &[GraphSetColumnType::Vertex, GraphSetColumnType::Scalar],
            &[(1, 0)],
        ).unwrap().with_kind(kind);
        assert_eq!(spec.column_types().collect::<Vec<_>>(), vec![
            GraphSetColumnType::List, GraphSetColumnType::Vertex, GraphSetColumnType::Scalar,
            GraphSetColumnType::Vertex, GraphSetColumnType::Scalar,
        ]);
        let mut state = seed(&spec, &z([(left.clone(), 2)]), &z([(right.clone(), 3)]));
        assert_eq!(state.rows(), &z([(expected.clone(), 6)]));
        state.prepare(&z([(left.clone(), -2)]), &ZSet::new(), LIMBS, Some(3), &mut allow).unwrap().commit();
        assert_eq!(state.rows(), &z([(unmatched.clone(), 3)]));
        state.prepare(&z([(left.clone(), 2)]), &ZSet::new(), LIMBS, Some(6), &mut allow).unwrap().commit();
        assert_eq!(state.rows(), &z([(expected.clone(), 6)]));
    }
}

#[test]
fn null_extensions_coalesce_but_null_keys_never_match() {
    let null_row = GraphValueRow::from_owned_values(vec![scalar(None)]);
    let output = GraphValueRow::from_owned_values(vec![scalar(None), scalar(None)]);
    for (kind, expected) in [(RowJoinKind::Right, 3), (RowJoinKind::Full, 5)] {
        let spec = RowJoinSpec::new(&[GraphSetColumnType::Scalar], &[GraphSetColumnType::Scalar], &[(0, 0)]).unwrap().with_kind(kind);
        let mut state = seed(&spec, &z([(null_row.clone(), 2)]), &z([(null_row.clone(), 3)]));
        assert_eq!(state.rows(), &z([(output.clone(), expected)]));
        state.prepare(&z([(null_row.clone(), -2)]), &z([(null_row.clone(), -3)]), LIMBS, Some(0), &mut allow).unwrap().commit();
        assert!(state.rows().is_empty());
    }
}

#[test]
fn empty_tuple_cross_joins_preserve_outer_occurrences_and_promoted_products() {
    let unit = GraphValueRow::from_owned_values(vec![]);
    for kind in [RowJoinKind::Right, RowJoinKind::Full] {
        let spec = RowJoinSpec::cross(&[], &[]).unwrap().with_kind(kind);
        for left in 0..=2 {
            for right in 0..=2 {
                let expected = if left > 0 && right > 0 { left * right }
                    else if right > 0 { right }
                    else if kind == RowJoinKind::Full { left } else { 0 };
                let state = seed(&spec, &z([(unit.clone(), left)]), &z([(unit.clone(), right)]));
                assert_eq!(state.rows(), &z([(unit.clone(), expected)]));
            }
        }
        let mut state = seed(&spec, &z([(unit.clone(), i128::MAX)]), &z([(unit.clone(), 2)]));
        assert!(state.total().is_promoted());
        assert_eq!(state.rows().len(), 1);
        assert_eq!(state.total(), &ZWeight::from_i128(i128::MAX).checked_mul(&ZWeight::from_i128(2), LIMBS).unwrap());
        state.prepare(&z([(unit.clone(), -i128::MAX)]), &ZSet::new(), LIMBS, Some(2), &mut allow).unwrap().commit();
        assert_eq!(state.rows(), &z([(unit.clone(), 2)]));
    }
}

#[test]
fn refusal_at_every_callback_drop_and_unwind_preserve_all_outer_arrangements() {
    let (left, right) = (z([(row(Some(7), 10), 2)]), z([(row(Some(9), 20), 3)]));
    let (next_left, next_right) = (z([(row(Some(9), 11), 2)]), z([(row(Some(7), 21), 3)]));
    let dl = next_left.minus(&left, LIMBS, &mut allow).unwrap();
    let dr = next_right.minus(&right, LIMBS, &mut allow).unwrap();
    for kind in [RowJoinKind::Right, RowJoinKind::Full] {
        let spec = spec(kind, false);
        let mut success = seed(&spec, &left, &right);
        let mut calls = 0;
        let expected_delta = success.prepare(&dl, &dr, LIMBS, None, &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        }).unwrap().commit();
        assert!(calls > 0);
        for stop in 1..=calls {
            let mut state = seed(&spec, &left, &right);
            let mut seen = 0;
            let error = state.prepare(&dl, &dr, LIMBS, None, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            }).unwrap_err();
            assert_eq!(error, RowJoinError::Delta(ZSetError::Control(stop)));
            assert_eq!(seen, stop);
            assert_eq!(state, seed(&spec, &left, &right));
            assert_eq!(state.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap().commit(), expected_delta);
            assert_eq!(state, success);
        }
        let mut state = seed(&spec, &left, &right);
        drop(state.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap());
        assert_eq!(state, seed(&spec, &left, &right));
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = state.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap();
            panic!("downstream rejection");
        })).is_err());
        assert_eq!(state, seed(&spec, &left, &right));
    }
}

#[test]
fn negative_rows_cannot_hide_in_positive_key_totals_and_diagnostics_keep_original_sides() {
    for kind in [RowJoinKind::Right, RowJoinKind::Full] {
        let spec = spec(kind, false);
        let empty = ZSet::new();
        let invalid = z([(row(Some(7), 1), -1), (row(Some(7), 2), 1)]);
        for side in 0..2 {
            let mut state = IncrementalRowJoin::new(spec.clone());
            let (left, right) = if side == 0 { (&invalid, &empty) } else { (&empty, &invalid) };
            assert_eq!(state.prepare(left, right, LIMBS, Some(0), &mut allow).unwrap_err(), RowJoinError::NegativeMultiplicity { side });
            assert_eq!(state, IncrementalRowJoin::new(spec.clone()));
        }
    }
}

#[test]
fn full_join_quota_applies_to_consolidated_final_bag_and_refusal_is_retryable() {
    let spec = spec(RowJoinKind::Full, false);
    let left = z([(row(Some(7), 10), 2)]);
    let right = z([(row(Some(7), 20), 3)]);
    let mut state = seed(&spec, &left, &ZSet::new());
    assert_eq!(state.prepare(&ZSet::new(), &right, LIMBS, Some(5), &mut allow).unwrap_err(), RowJoinError::ResultBudget { limit: 5 });
    assert_eq!(state, seed(&spec, &left, &ZSet::new()));
    state.prepare(&ZSet::new(), &right, LIMBS, Some(6), &mut allow).unwrap().commit();
    let replacement = z([(row(Some(9), 21), 3)]);
    let dr = replacement.minus(&right, LIMBS, &mut allow).unwrap();
    state.prepare(&ZSet::new(), &dr, LIMBS, Some(5), &mut allow).unwrap().commit();
    assert_eq!(state.rows(), &oracle(&left, &replacement, RowJoinKind::Full, false));
    assert_eq!(state.total(), &ZWeight::from_i128(5));
}

#[test]
fn replacing_same_key_witness_rows_never_exposes_a_spurious_null_extension() {
    let first = row(Some(7), 10);
    let second = row(Some(7), 11);
    let third = row(Some(7), 12);
    let right = z([(row(Some(7), 20), 3)]);
    for kind in [RowJoinKind::Right, RowJoinKind::Full] {
        let spec = spec(kind, false);
        let mut left = z([(first.clone(), 2), (second.clone(), 1)]);
        let mut state = seed(&spec, &left, &right);
        for next in [z([(second.clone(), 1)]), z([(third.clone(), 4)]), ZSet::new()] {
            let before = oracle(&left, &right, kind, false);
            let after = oracle(&next, &right, kind, false);
            let delta = next.minus(&left, LIMBS, &mut allow).unwrap();
            assert_eq!(
                state.prepare(&delta, &ZSet::new(), LIMBS, None, &mut allow).unwrap().commit(),
                after.minus(&before, LIMBS, &mut allow).unwrap()
            );
            assert_eq!(state.rows(), &after);
            left = next;
        }
    }
}
