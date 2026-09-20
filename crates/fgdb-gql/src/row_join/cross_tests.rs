//! Independent complete-bag oracles for unconditional native joins.
use super::*;
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }
fn row(value: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
        value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))])
}
fn z(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(rows.into_iter().map(|(r, w)| (r, ZWeight::from_i128(w))),
        LIMBS, &mut allow).unwrap()
}
fn bag(code: usize) -> ZSet<GraphValueRow> {
    z([(row(None), (code % 3) as i128), (row(Some(7)), (code / 3) as i128)])
}
fn spec(kind: RowJoinKind) -> RowJoinSpec {
    RowJoinSpec::cross(&[GraphSetColumnType::Scalar], &[GraphSetColumnType::Scalar])
        .unwrap().with_kind(kind)
}
fn seed(kind: RowJoinKind, left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>) -> IncrementalRowJoin {
    let mut state = IncrementalRowJoin::new(spec(kind));
    state.prepare(left, right, LIMBS, None, &mut allow).unwrap().commit();
    state
}
fn plain(rows: &ZSet<GraphValueRow>) -> BTreeMap<GraphValueRow, i128> {
    rows.iter().map(|(r, w)| (r.clone(), w.to_i128().unwrap())).collect()
}
fn oracle(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, kind: RowJoinKind)
    -> BTreeMap<GraphValueRow, i128> {
    let mut result = BTreeMap::new();
    for (l, lw) in left.iter() {
        let lw = lw.to_i128().unwrap();
        match kind {
            RowJoinKind::Semi if !right.is_empty() => { result.insert(l.clone(), lw); }
            RowJoinKind::Anti if right.is_empty() => { result.insert(l.clone(), lw); }
            RowJoinKind::Left if right.is_empty() => {
                let mut values = l.values().to_vec();
                values.push(GraphValue::Scalar(CanonicalScalar::Null));
                *result.entry(GraphValueRow::from_owned_values(values)).or_insert(0) += lw;
            }
            RowJoinKind::Inner | RowJoinKind::Left => {
                for (r, rw) in right.iter() {
                    let mut values = l.values().to_vec(); values.extend_from_slice(r.values());
                    *result.entry(GraphValueRow::from_owned_values(values)).or_insert(0) += lw * rw.to_i128().unwrap();
                }
            }
            _ => {}
        }
    }
    result
}

#[test]
fn every_small_transition_including_simultaneous_changes_matches_complete_bag_oracle() {
    for kind in [RowJoinKind::Inner, RowJoinKind::Left, RowJoinKind::Semi, RowJoinKind::Anti] {
        for before in 0..81 {
            let (left, right) = (bag(before % 9), bag(before / 9));
            for after in 0..81 {
                let (next_left, next_right) = (bag(after % 9), bag(after / 9));
                let dl = next_left.minus(&left, LIMBS, &mut allow).unwrap();
                let dr = next_right.minus(&right, LIMBS, &mut allow).unwrap();
                let mut state = seed(kind, &left, &right);
                assert_eq!(plain(state.rows()), oracle(&left, &right, kind));
                let mut integrated = state.rows().checked_clone(LIMBS, &mut allow).unwrap();
                let delta = state.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap().commit();
                integrated.integrate(&delta, LIMBS, &mut allow).unwrap();
                assert_eq!(plain(state.rows()), oracle(&next_left, &next_right, kind),
                    "{kind:?}: {before}->{after}");
                assert_eq!(state.rows(), &integrated);
                assert_eq!(state.total(), &integrated.total_weight(LIMBS, &mut allow).unwrap());
                let reverse = state.prepare(&dl.negated(LIMBS, &mut allow).unwrap(),
                    &dr.negated(LIMBS, &mut allow).unwrap(), LIMBS, None, &mut allow).unwrap().commit();
                assert_eq!(reverse, delta.negated(LIMBS, &mut allow).unwrap());
                assert_eq!(state, seed(kind, &left, &right));
            }
        }
    }
}

#[test]
fn products_preserve_native_payloads_and_empty_tuple_multiplicity_without_expansion() {
    let id = GraphValue::Vertex(VId(u128::MAX));
    let list = GraphValue::List(vec![id.clone(), GraphValue::List(vec![
        GraphValue::Scalar(CanonicalScalar::Null)].into_boxed_slice())].into_boxed_slice());
    let left = GraphValueRow::from_owned_values(vec![list.clone()]);
    let right = GraphValueRow::from_owned_values(vec![id.clone()]);
    let definition = RowJoinSpec::cross(&[GraphSetColumnType::List], &[GraphSetColumnType::Any]).unwrap();
    assert!(definition.is_cross()); assert!(definition.keys().is_empty());
    assert_eq!(definition.column_types().collect::<Vec<_>>(),
        vec![GraphSetColumnType::List, GraphSetColumnType::Any]);
    let mut state = IncrementalRowJoin::new(definition);
    state.prepare(&z([(left.clone(), i128::MAX)]), &z([(right.clone(), 2)]), LIMBS, None, &mut allow)
        .unwrap().commit();
    let expected = GraphValueRow::from_owned_values(vec![list, id]);
    assert_eq!(state.rows().len(), 1);
    assert_eq!(state.rows().weight(&expected), Some(state.total()));
    assert!(state.total().is_promoted());
    assert_eq!(state.total(), &ZWeight::from_i128(i128::MAX).checked_mul(&ZWeight::from_i128(2), LIMBS).unwrap());
    state.prepare(&z([(left, -i128::MAX)]), &z([(right, -2)]), LIMBS, Some(0), &mut allow)
        .unwrap().commit();
    assert!(state.rows().is_empty());

    let unit = GraphValueRow::from_owned_values(vec![]);
    let mut state = IncrementalRowJoin::new(RowJoinSpec::cross(&[], &[]).unwrap());
    state.prepare(&z([(unit.clone(), 7)]), &z([(unit.clone(), 11)]), LIMBS, Some(77), &mut allow)
        .unwrap().commit();
    assert_eq!(state.rows().weight(&unit), Some(&ZWeight::from_i128(77)));
    // One empty frame can be the identity of an ordinary nonempty relation.
    let mut state = IncrementalRowJoin::new(RowJoinSpec::cross(&[], &[GraphSetColumnType::Scalar]).unwrap());
    state.prepare(&z([(unit, 1)]), &bag(8), LIMBS, None, &mut allow).unwrap().commit();
    assert_eq!(state.rows(), &bag(8));
}

#[test]
fn every_callback_refusal_drop_and_unwind_preserves_both_inputs_output_and_total() {
    let (left, right) = (bag(5), bag(7));
    let (dl, dr) = (bag(7).minus(&left, LIMBS, &mut allow).unwrap(),
        bag(4).minus(&right, LIMBS, &mut allow).unwrap());
    for kind in [RowJoinKind::Inner, RowJoinKind::Left, RowJoinKind::Semi, RowJoinKind::Anti] {
        let mut success = seed(kind, &left, &right); let mut calls = 0;
        let expected = success.prepare(&dl, &dr, LIMBS, None, &mut |_| {
            calls += 1; Ok::<_, usize>(())
        }).unwrap().commit();
        for stop in 1..=calls {
            let mut state = seed(kind, &left, &right); let mut seen = 0;
            assert!(state.prepare(&dl, &dr, LIMBS, None, &mut |_| {
                seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
            }).is_err());
            assert_eq!(seen, stop); assert_eq!(state, seed(kind, &left, &right));
            assert_eq!(state.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap().commit(), expected);
            assert_eq!(state, success);
        }
        let mut state = seed(kind, &left, &right);
        { let pending = state.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap();
            assert_eq!(pending.delta(), &expected); }
        assert_eq!(state, seed(kind, &left, &right));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _pending = state.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap();
            panic!("downstream refusal");
        }));
        assert!(result.is_err()); assert_eq!(state, seed(kind, &left, &right));
    }
}

#[test]
fn empty_opposite_input_and_zero_quota_do_not_hide_invalid_source_rows_or_retractions() {
    let mut state = seed(RowJoinKind::Inner, &ZSet::new(), &ZSet::new());
    assert!(matches!(state.prepare(&z([(row(None), -1)]), &ZSet::new(), LIMBS, Some(0), &mut allow),
        Err(RowJoinError::NegativeMultiplicity { side: 0 })));
    assert!(matches!(state.prepare(&ZSet::new(), &z([(row(None), -1)]), LIMBS, Some(0), &mut allow),
        Err(RowJoinError::NegativeMultiplicity { side: 1 })));
    let wrong = GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1))]);
    assert!(matches!(state.prepare(&z([(wrong, 1)]), &ZSet::new(), LIMBS, Some(0), &mut allow),
        Err(RowJoinError::InputSchema { side: 0 })));
    assert_eq!(state, seed(RowJoinKind::Inner, &ZSet::new(), &ZSet::new()));
    let mut nested = GraphValue::Scalar(CanonicalScalar::Null);
    for _ in 0..=GraphValue::MAX_LIST_DEPTH + 1 {
        nested = GraphValue::List(vec![nested].into_boxed_slice());
    }
    let mut state = IncrementalRowJoin::new(RowJoinSpec::cross(&[GraphSetColumnType::Any], &[]).unwrap());
    let invalid = GraphValueRow::from_owned_values(vec![nested]);
    assert!(matches!(state.prepare(&z([(invalid, 1)]), &ZSet::new(), LIMBS, Some(0), &mut allow),
        Err(RowJoinError::InputSchema { side: 0 })));
    assert!(state.rows().is_empty());
}

#[test]
fn final_occurrence_quota_is_atomic_and_work_does_not_expand_duplicate_counts() {
    let (left, right) = (z([(row(Some(1)), 1)]), z([(row(Some(2)), 1)]));
    let (dl, dr) = (z([(row(Some(1)), -1), (row(Some(3)), 1)]),
        z([(row(Some(2)), -1), (row(Some(4)), 1)]));
    let mut state = seed(RowJoinKind::Inner, &left, &right);
    state.prepare(&dl, &dr, LIMBS, Some(1), &mut allow).unwrap().commit();
    let accepted = seed(RowJoinKind::Inner, &z([(row(Some(3)), 1)]), &z([(row(Some(4)), 1)]));
    assert_eq!(state, accepted);
    assert!(matches!(state.prepare(&z([(row(None), 1)]), &ZSet::new(), LIMBS, Some(1), &mut allow),
        Err(RowJoinError::ResultBudget { limit: 1 })));
    assert_eq!(state, accepted);
    let mut observations = Vec::new();
    for count in [1, 1_000_000] {
        let mut state = seed(RowJoinKind::Inner, &z([(row(Some(1)), count)]), &z([(row(Some(2)), count)]));
        let mut work = [0; 2];
        state.prepare(&z([(row(Some(1)), -count), (row(Some(3)), count)]), &ZSet::new(), LIMBS, None,
            &mut |event| { work[match event { ZSetEvent::Work => 0, ZSetEvent::ScratchEntry => 1 }] += 1;
                Ok::<_, usize>(()) }).unwrap().commit();
        observations.push(work);
    }
    assert_eq!(observations[0], observations[1]);
}

#[test]
fn explicit_cross_admission_does_not_turn_invalid_equijoins_into_products() {
    use GraphSetColumnType as T;
    assert_eq!(RowJoinSpec::new(&[T::Scalar], &[T::Scalar], &[]).unwrap_err(), RowJoinBuildError::EmptyKeys);
    assert_eq!(RowJoinSpec::new(&[], &[T::Scalar], &[(0, 0)]).unwrap_err(), RowJoinBuildError::EmptyInput);
    assert!(matches!(RowJoinSpec::cross(&vec![T::Any; MAX_PATTERN_VERTICES], &[T::Scalar]),
        Err(RowJoinBuildError::TooManyColumns { .. })));
    let ordinary = RowJoinSpec::new(&[T::Scalar], &[T::Scalar], &[(0, 0)]).unwrap();
    assert!(!ordinary.is_cross());
    let mut ordinary = IncrementalRowJoin::new(ordinary);
    ordinary.prepare(&z([(row(None), 1)]), &z([(row(None), 1)]), LIMBS, None, &mut allow).unwrap().commit();
    assert!(ordinary.rows().is_empty());
    assert!(!format!("{:?}", spec(RowJoinKind::Inner)).contains("payload"));
}
