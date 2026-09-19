use super::*;

const LIMBS: LimbLimit = LimbLimit::new(16);
const OPS: [SetOperation; 6] = [SetOperation::UnionAll, SetOperation::UnionDistinct,
    SetOperation::IntersectAll, SetOperation::IntersectDistinct,
    SetOperation::ExceptAll, SetOperation::ExceptDistinct];
fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }
fn z(rows: &[(i32, i128)]) -> ZSet<i32> {
    ZSet::from_updates(rows.iter().map(|&(key, w)| (key, ZWeight::from_i128(w))),
        LIMBS, &mut allow).unwrap()
}
fn plain(rows: &ZSet<i32>) -> BTreeMap<i32, i128> {
    rows.iter().map(|(&key, weight)| (key, weight.to_i128().unwrap())).collect()
}
// Independent primitive bag arithmetic, not the kernel's weight helper.
fn expected(op: SetOperation, a: i128, b: i128) -> i128 {
    match op {
        SetOperation::UnionAll => a + b,
        SetOperation::UnionDistinct => i128::from(a + b != 0),
        SetOperation::IntersectAll => a.min(b),
        SetOperation::IntersectDistinct => i128::from(a != 0 && b != 0),
        SetOperation::ExceptAll => (a - b).max(0),
        SetOperation::ExceptDistinct => i128::from(a != 0 && b == 0),
    }
}

#[test]
fn every_small_bag_transition_matches_full_evaluation_for_all_six_laws() {
    for op in OPS {
        for a in 0..=4 {
            for b in 0..=4 {
                for next_a in 0..=4 {
                    for next_b in 0..=4 {
                        let mut state = IncrementalSet::new(op);
                        let mut output = state.apply(&z(&[(1, a), (2, 7)]),
                            &z(&[(1, b), (3, 9)]), LIMBS, &mut allow).unwrap();
                        let delta = state.apply(&z(&[(1, next_a - a)]),
                            &z(&[(1, next_b - b)]), LIMBS, &mut allow).unwrap();
                        assert_eq!(delta, z(&[(1, expected(op, next_a, next_b) - expected(op, a, b))]));
                        output.integrate(&delta, LIMBS, &mut allow).unwrap();
                        assert_eq!(output, z(&[(1, expected(op, next_a, next_b)),
                            (2, expected(op, 7, 0)), (3, expected(op, 0, 9))]));
                        assert_eq!(state.left_counts(), &z(&[(1, next_a), (2, 7)]));
                        assert_eq!(state.right_counts(), &z(&[(1, next_b), (3, 9)]));
                    }
                }
            }
        }
    }
}

#[test]
fn retained_duplicate_and_blocker_counts_make_deletions_and_swaps_exact() {
    let mut state = IncrementalSet::new(SetOperation::UnionDistinct);
    assert_eq!(state.apply(&z(&[(7, 3)]), &z(&[(7, 2)]), LIMBS, &mut allow).unwrap(), z(&[(7, 1)]));
    assert!(state.apply(&z(&[(7, -3)]), &z(&[(7, 1)]), LIMBS, &mut allow).unwrap().is_empty());
    assert!(state.apply(&z(&[]), &z(&[(7, -2)]), LIMBS, &mut allow).unwrap().is_empty());
    assert_eq!(state.apply(&z(&[]), &z(&[(7, -1)]), LIMBS, &mut allow).unwrap(), z(&[(7, -1)]));
    for op in [SetOperation::ExceptAll, SetOperation::ExceptDistinct] {
        let mut state = IncrementalSet::new(op);
        assert!(state.apply(&z(&[(7, 3)]), &z(&[(7, 5)]), LIMBS, &mut allow).unwrap().is_empty());
        assert_eq!(state.apply(&z(&[]), &z(&[(7, -4)]), LIMBS, &mut allow).unwrap(),
            z(&[(7, expected(op, 3, 1))]));
        assert_eq!(state.apply(&z(&[(7, -3)]), &z(&[(7, -1)]), LIMBS, &mut allow).unwrap(),
            z(&[(7, -expected(op, 3, 1))]));
        assert_eq!(state, IncrementalSet::new(op));
    }
}

fn seeded(op: SetOperation) -> IncrementalSet<i32> {
    let mut state = IncrementalSet::new(op);
    state.apply(&z(&[(1, 2), (2, 1)]), &z(&[(1, 3), (3, 2)]), LIMBS, &mut allow).unwrap();
    state
}

#[test]
fn every_refusal_dropped_guard_and_unwind_preserve_both_inputs_and_retry() {
    let dl = z(&[(1, -1), (2, -1), (4, 2)]);
    let dr = z(&[(1, -3), (3, -2), (4, 1)]);
    for op in OPS {
        let mut success = seeded(op);
        let mut calls = 0;
        let wanted = success.apply(&dl, &dr, LIMBS, &mut |_| {
            calls += 1; Ok::<_, usize>(())
        }).unwrap();
        for stop in 1..=calls {
            let mut state = seeded(op);
            let mut seen = 0;
            assert_eq!(state.apply(&dl, &dr, LIMBS, &mut |_| {
                seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
            }), Err(SetError::Delta(ZSetError::Control(stop))));
            assert_eq!(state, seeded(op));
            assert_eq!(state.apply(&dl, &dr, LIMBS, &mut allow).unwrap(), wanted);
            assert_eq!(state, success);
        }
        let mut state = seeded(op);
        { let update = state.prepare(&dl, &dr, LIMBS, &mut allow).unwrap();
            assert_eq!(update.delta(), &wanted); }
        assert_eq!(state, seeded(op));
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _pending = state.prepare(&dl, &dr, LIMBS, &mut allow).unwrap();
            panic!("downstream unwind");
        }));
        assert!(unwind.is_err());
        assert_eq!(state, seeded(op));
    }
}

#[test]
fn invalid_negative_final_inputs_fail_even_for_suppressed_outputs() {
    for op in OPS {
        for side in [SetInput::Left, SetInput::Right] {
            let mut state = IncrementalSet::new(op);
            let zero = z(&[]);
            let negative = z(&[(17, -1)]);
            let (left, right) = match side { SetInput::Left => (&negative, &zero),
                SetInput::Right => (&zero, &negative) };
            assert_eq!(state.apply(left, right, LIMBS, &mut allow),
                Err(SetError::NegativeMultiplicity { input: side }));
            assert_eq!(state, IncrementalSet::new(op));
        }
    }
    // Individual retractions may cancel insertions within one consolidated tick.
    let mut state = IncrementalSet::new(SetOperation::UnionAll);
    assert_eq!(state.apply(&z(&[(17, -1), (17, 2)]), &z(&[]), LIMBS, &mut allow).unwrap(), z(&[(17, 1)]));
}

#[test]
fn promoted_weights_are_exact_and_arithmetic_exhaustion_is_atomic() {
    let large = ZWeight::from_i128(i128::MAX).checked_add(&ZWeight::ONE, LIMBS).unwrap();
    let left = ZSet::from_updates([(1, large.checked_clone(LIMBS).unwrap())], LIMBS, &mut allow).unwrap();
    let right = z(&[(1, i128::MAX)]);
    let mut difference = IncrementalSet::new(SetOperation::ExceptAll);
    assert_eq!(difference.apply(&left, &right, LIMBS, &mut allow).unwrap(), z(&[(1, 1)]));
    let mut state = IncrementalSet::new(SetOperation::UnionAll);
    assert!(matches!(state.apply(&right, &z(&[(1, 1)]), LimbLimit::new(0), &mut allow),
        Err(SetError::Delta(ZSetError::Arithmetic(_)))));
    assert_eq!(state, IncrementalSet::new(SetOperation::UnionAll));
    let output = state.apply(&right, &z(&[(1, 1)]), LIMBS, &mut allow).unwrap();
    assert_eq!(output.weight(&1), Some(&large));
    assert_eq!(state.apply(&right.negated(LIMBS, &mut allow).unwrap(), &z(&[(1, -1)]),
        LIMBS, &mut allow).unwrap(), output.negated(LIMBS, &mut allow).unwrap());
    assert_eq!(state, IncrementalSet::new(SetOperation::UnionAll));
    // DISTINCT need not form an overflowing union count just to test presence.
    let mut distinct = IncrementalSet::new(SetOperation::UnionDistinct);
    assert_eq!(distinct.apply(&right, &right, LimbLimit::new(0), &mut allow).unwrap(), z(&[(1, 1)]));
}

#[test]
fn unchanged_support_is_neither_scanned_nor_cloned_and_empty_ticks_are_cancellable() {
    for op in OPS {
        let mut small = seeded(op);
        let mut large = seeded(op);
        let unrelated = z(&(100..10_100).map(|key| (key, 1)).collect::<Vec<_>>());
        large.apply(&unrelated, &unrelated, LIMBS, &mut allow).unwrap();
        let mut measurements = Vec::new();
        for state in [&mut small, &mut large] {
            let mut events = Vec::new();
            let delta = state.apply(&z(&[(1, -1)]), &z(&[(1, 1)]), LIMBS, &mut |event| {
                events.push(event); Ok::<_, usize>(())
            }).unwrap();
            measurements.push((plain(&delta), events));
        }
        assert_eq!(measurements[0], measurements[1]);
        assert_eq!(large.apply(&z(&[]), &z(&[]), LIMBS, &mut |_| Err(7)),
            Err(SetError::Delta(ZSetError::Control(7))));
    }
}

#[test]
fn an_output_sink_can_prepare_before_input_publication() {
    let mut state = seeded(SetOperation::IntersectAll);
    let mut output = z(&[(1, 2)]);
    let dl = z(&[(1, 1)]);
    let dr = z(&[(1, -1)]);
    {
        let pending = state.prepare(&dl, &dr, LIMBS, &mut allow).unwrap();
        let refused = output.prepare_update(pending.delta(), LIMBS, &mut |_| Err(9));
        assert!(matches!(refused, Err(ZSetError::Control(9))));
    }
    assert_eq!(state, seeded(SetOperation::IntersectAll));
    assert_eq!(output, z(&[(1, 2)]));
    let pending = state.prepare(&dl, &dr, LIMBS, &mut allow).unwrap();
    let sink = output.prepare_update(pending.delta(), LIMBS, &mut allow).unwrap();
    sink.commit();
    assert!(pending.commit().is_empty());
    assert_eq!(output, z(&[(1, 2)]));
    assert_eq!(state.left_counts(), &z(&[(1, 3), (2, 1)]));
    assert_eq!(state.right_counts(), &z(&[(1, 2), (3, 2)]));
}
