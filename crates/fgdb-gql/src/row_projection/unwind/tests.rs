use super::*;
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }
fn int(n: i64) -> GraphValue { GraphValue::Scalar(CanonicalScalar::Int(n)) }
fn null() -> GraphValue { GraphValue::Scalar(CanonicalScalar::Null) }
fn list(values: Vec<GraphValue>) -> GraphValue { GraphValue::List(values.into_boxed_slice()) }
fn row(value: GraphValue) -> GraphValueRow { GraphValueRow::from_owned_values(vec![value]) }
fn z(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(rows.into_iter().map(|(r, n)| (r, ZWeight::from_i128(n))), LIMBS, &mut allow).unwrap()
}
fn spec() -> RowProjectionSpec {
    RowProjectionSpec::unwind(vec![GraphSetColumnType::Any], vec!["xs".into()],
        "element".into(), GraphSetValue::Column(0)).unwrap()
}
fn seed(input: &ZSet<GraphValueRow>) -> IncrementalRowProjection {
    let mut state = IncrementalRowProjection::new(spec());
    state.prepare(input, LIMBS, None, &mut allow).unwrap().commit(); state
}
fn bag(mut code: usize) -> ZSet<GraphValueRow> {
    z([null(), list(vec![]), list(vec![int(1), int(1)]), list(vec![int(1), int(2)])]
        .into_iter().map(|value| { let count = (code % 3) as i128; code /= 3; (row(value), count) }))
}
fn plain(rows: &ZSet<GraphValueRow>) -> BTreeMap<GraphValueRow, i128> {
    rows.iter().map(|(r, n)| (r.clone(), n.to_i128().unwrap())).collect()
}
fn oracle(input: &ZSet<GraphValueRow>) -> BTreeMap<GraphValueRow, i128> {
    let mut output = BTreeMap::new();
    for (r, count) in input.iter() {
        if let GraphValue::List(elements) = &r.values()[0] {
            for element in elements.iter() {
                let mut values = r.values().to_vec(); values.push(element.clone());
                *output.entry(GraphValueRow::from_owned_values(values)).or_insert(0) += count.to_i128().unwrap();
            }
        }
    }
    output
}

#[test]
fn all_small_transitions_and_inverses_match_complete_list_expansion() {
    for before in 0..81 {
        let old = bag(before);
        for after in 0..81 {
            let new = bag(after);
            let change = new.minus(&old, LIMBS, &mut allow).unwrap();
            let mut state = seed(&old);
            let mut integrated = state.rows().checked_clone(LIMBS, &mut allow).unwrap();
            assert_eq!(plain(&integrated), oracle(&old));
            let delta = state.prepare(&change, LIMBS, None, &mut allow).unwrap().commit();
            integrated.integrate(&delta, LIMBS, &mut allow).unwrap();
            assert_eq!(plain(state.rows()), oracle(&new), "{before}->{after}");
            assert_eq!(state.rows(), &integrated);
            assert_eq!(state.total(), &integrated.total_weight(LIMBS, &mut allow).unwrap());
            let reverse = state.prepare(&change.negated(LIMBS, &mut allow).unwrap(),
                LIMBS, None, &mut allow).unwrap().commit();
            assert_eq!(reverse, delta.negated(LIMBS, &mut allow).unwrap());
            assert_eq!(state, seed(&old));
        }
    }
}

#[test]
fn nested_lists_identities_null_elements_and_wide_counts_keep_their_native_domains() {
    let id = GraphValue::Vertex(VId(u128::MAX));
    let nested = list(vec![id.clone(), null()]);
    let xs = list(vec![id.clone(), id.clone(), nested.clone(), null()]);
    let input = row(xs.clone());
    let mut state = seed(&z([(input.clone(), i128::MAX)]));
    let pair = GraphValueRow::from_owned_values(vec![xs.clone(), id]);
    let expected = ZWeight::from_i128(i128::MAX).checked_mul(&ZWeight::from_i128(2), LIMBS).unwrap();
    assert!(expected.is_promoted());
    assert_eq!(state.rows().weight(&pair), Some(&expected));
    assert_eq!(state.rows().weight(&GraphValueRow::from_owned_values(vec![xs.clone(), nested])),
        Some(&ZWeight::from_i128(i128::MAX)));
    assert_eq!(state.rows().weight(&GraphValueRow::from_owned_values(vec![xs, null()])),
        Some(&ZWeight::from_i128(i128::MAX)));
    state.prepare(&z([(input, -i128::MAX)]), LIMBS, Some(0), &mut allow).unwrap().commit();
    assert!(state.rows().is_empty());
    // Unit tuples expand literals without manufacturing graph identities.
    let definition = RowProjectionSpec::unwind(vec![], vec![], "n".into(),
        GraphSetValue::List(vec![GraphSetValue::Value(int(9)), GraphSetValue::Value(int(9))])).unwrap();
    let mut state = IncrementalRowProjection::new(definition);
    state.prepare(&z([(GraphValueRow::from_owned_values(vec![]), 7)]), LIMBS, None, &mut allow).unwrap().commit();
    assert_eq!(state.rows(), &z([(row(int(9)), 14)]));
}

#[test]
fn downstream_distinct_retains_last_element_witness_and_all_participants_can_abort() {
    let a = row(list(vec![int(1), int(1)]));
    let b = row(list(vec![int(1), int(2)]));
    let mut expansion = seed(&z([(a.clone(), 2), (b.clone(), 3)]));
    let definition = RowProjectionSpec::new(expansion.spec().column_types().to_vec(),
        vec![GraphSetProjection::new("element", GraphSetValue::Column(1))], GraphSetQuantifier::Distinct).unwrap();
    let mut distinct = IncrementalRowProjection::new(definition.clone());
    distinct.prepare(expansion.rows(), LIMBS, None, &mut allow).unwrap().commit();
    assert_eq!(distinct.rows(), &z([(row(int(1)), 1), (row(int(2)), 1)]));
    let change = z([(a.clone(), -2)]);
    {
        let parent = expansion.prepare(&change, LIMBS, None, &mut allow).unwrap();
        let child = distinct.prepare(parent.delta(), LIMBS, Some(2), &mut allow).unwrap();
        assert!(child.delta().is_empty()); // Dropping BOTH stages accepts nothing.
    }
    assert_eq!(expansion, seed(&z([(a, 2), (b.clone(), 3)])));
    let parent = expansion.prepare(&change, LIMBS, None, &mut allow).unwrap();
    let child = distinct.prepare(parent.delta(), LIMBS, Some(2), &mut allow).unwrap();
    child.commit(); parent.commit();
    let parent = expansion.prepare(&z([(b, -3)]), LIMBS, Some(0), &mut allow).unwrap();
    let child = distinct.prepare(parent.delta(), LIMBS, Some(0), &mut allow).unwrap();
    child.commit(); parent.commit();
    assert!(distinct.rows().is_empty()); assert!(expansion.rows().is_empty());
}

#[test]
fn hidden_inputs_and_nonlist_errors_are_not_erased_by_empty_output_or_zero_quotas() {
    let mut state = seed(&ZSet::new());
    for value in [null(), list(vec![])] {
        let input = row(value);
        assert_eq!(state.prepare(&z([(input.clone(), -1)]), LIMBS, Some(0), &mut allow).unwrap_err(),
            RowProjectionError::NegativeMultiplicity);
        state.prepare(&z([(input.clone(), i128::MAX)]), LIMBS, Some(0), &mut allow).unwrap().commit();
        state.prepare(&z([(input, -i128::MAX)]), LIMBS, Some(0), &mut allow).unwrap().commit();
    }
    assert!(matches!(state.prepare(&z([(row(int(1)), 1)]), LIMBS, Some(0), &mut allow),
        Err(RowProjectionError::Expression { column: 1, error: GraphIntegerError {
            kind: GraphIntegerErrorKind::IncompatibleOperands, .. } })));
    assert_eq!(state, seed(&ZSet::new()));
    let safe = spec().with_filter(&[GraphSetPredicateOp::Truth(Some(false))]).unwrap();
    let mut filtered = IncrementalRowProjection::new(safe);
    filtered.prepare(&z([(row(int(1)), 1)]), LIMBS, Some(0), &mut allow).unwrap().commit();
    assert!(filtered.rows().is_empty());
    assert!(matches!(filtered.prepare(&z([(row(int(1)), -2)]), LIMBS, Some(0), &mut allow),
        Err(RowProjectionError::NegativeMultiplicity)));
    let invalid = GraphValueRow::from_owned_values(vec![]);
    assert!(matches!(filtered.prepare(&z([(invalid, 1)]), LIMBS, Some(0), &mut allow),
        Err(RowProjectionError::InputSchema)));
}

#[test]
fn each_checkpoint_and_final_quota_refusal_leave_full_input_and_expansion_unchanged() {
    let old = bag(53); let new = bag(72);
    let delta = new.minus(&old, LIMBS, &mut allow).unwrap();
    let mut successful = seed(&old); let mut calls = 0;
    let expected = successful.prepare(&delta, LIMBS, None, &mut |_| {
        calls += 1; Ok::<_, usize>(())
    }).unwrap().commit();
    for stop in 1..=calls {
        let mut state = seed(&old); let mut seen = 0;
        assert!(state.prepare(&delta, LIMBS, None, &mut |_| {
            seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
        }).is_err());
        assert_eq!(seen, stop); assert_eq!(state, seed(&old));
        assert_eq!(state.prepare(&delta, LIMBS, None, &mut allow).unwrap().commit(), expected);
        assert_eq!(state, successful);
    }
    let mut state = seed(&old);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _pending = state.prepare(&delta, LIMBS, None, &mut allow).unwrap();
        panic!("downstream failure");
    }));
    assert_eq!(state, seed(&old));
    let a = row(list(vec![int(1), int(1)])); let b = row(list(vec![int(2), int(2)]));
    let mut state = seed(&z([(a.clone(), 1)]));
    state.prepare(&z([(a, -1), (b.clone(), 1)]), LIMBS, Some(2), &mut allow).unwrap().commit();
    assert!(matches!(state.prepare(&z([(b.clone(), 1)]), LIMBS, Some(2), &mut allow),
        Err(RowProjectionError::ResultBudget { limit: 2 })));
    assert_eq!(state, seed(&z([(b, 1)])));
}

#[test]
fn work_depends_on_changed_elements_not_unrelated_support_or_duplicate_occurrences() {
    let mut observations = Vec::new();
    for size in [8, 1024] {
        for count in [1, 1_000_000] {
            let old = z((0..size).map(|n| (row(list(vec![int(n), int(n)])), count)));
            let mut state = seed(&old);
            let delta = z([(row(list(vec![int(0), int(0)])), -count),
                (row(list(vec![int(2048), int(2048)])), count)]);
            let mut events = [0; 2];
            state.prepare(&delta, LIMBS, None, &mut |event| {
                events[match event { ZSetEvent::Work => 0, ZSetEvent::ScratchEntry => 1 }] += 1;
                Ok::<_, usize>(())
            }).unwrap().commit();
            observations.push(events);
        }
    }
    assert!(observations.iter().all(|events| events == &observations[0]));
}

#[test]
fn checked_definition_preserves_inherited_names_but_checks_alias_shape_and_expression_scope() {
    use GraphSetColumnType as T;
    let inherited = vec!["left.x".into(), "left.x".into()];
    let spec = RowProjectionSpec::unwind(vec![T::Vertex, T::Scalar], inherited,
        "element".into(), GraphSetValue::List(vec![GraphSetValue::Column(0)])).unwrap();
    assert_eq!(spec.columns().collect::<Vec<_>>(), vec!["left.x", "left.x", "element"]);
    assert_eq!(spec.column_types(), &[T::Vertex, T::Scalar, T::Any]);
    assert_eq!(spec.quantifier(), GraphSetQuantifier::All);
    for name in ["xs", "bad.name", ""] {
        assert!(RowProjectionSpec::unwind(vec![T::List], vec!["xs".into()], name.into(), GraphSetValue::Column(0)).is_err());
    }
    assert!(RowProjectionSpec::unwind(vec![T::List], vec![], "x".into(), GraphSetValue::Column(0)).is_err());
    assert!(RowProjectionSpec::unwind(vec![T::List], vec!["xs".into()], "x".into(), GraphSetValue::Column(1)).is_err());
    assert!(RowProjectionSpec::unwind(vec![T::List; MAX_PATTERN_VERTICES], vec!["xs".into(); MAX_PATTERN_VERTICES],
        "x".into(), GraphSetValue::Column(0)).is_err());
    assert!(!format!("{spec:?}").contains("left.x"));
}
