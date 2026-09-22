use super::*;

const LIMBS: LimbLimit = LimbLimit::new(16);
type Join = IncrementalJoin<i32, i32, i32>;

fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}

fn z<T: Ord + Clone>(rows: &[(T, i128)]) -> ZSet<T> {
    ZSet::from_updates(
        rows.iter()
            .map(|(row, count)| (row.clone(), ZWeight::from_i128(*count))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}

fn less<C>(_: &i32, left: &i32, right: &i32, control: &mut C) -> Result<bool, ZSetError<usize>>
where
    C: FnMut(ZSetEvent) -> Result<(), usize>,
{
    event(control, ZSetEvent::Work)?;
    Ok(left < right)
}

// Primitive full recomputation, independent of the derivative's three terms.
fn oracle(left: &ZSet<(i32, i32)>, right: &ZSet<(i32, i32)>) -> ZSet<(i32, i32, i32)> {
    let mut rows = Vec::new();
    for (&(key, l), lw) in left.iter() {
        for (&(other, r), rw) in right.iter() {
            if key == other && l < r {
                rows.push(((key, l, r), lw.to_i128().unwrap() * rw.to_i128().unwrap()));
            }
        }
    }
    z(&rows)
}

#[test]
fn filtered_derivative_matches_signed_full_recomputation_including_simultaneous_changes() {
    for a in -2..=2 {
        for b in -2..=2 {
            for da in -2..=2 {
                for db in -2..=2 {
                    let left = z(&[((1, 1), a), ((1, 5), 2), ((9, 0), -1)]);
                    let right = z(&[((1, 4), b), ((2, 6), -2)]);
                    let dl = z(&[((1, 1), da), ((3, 2), -1)]);
                    let dr = z(&[((1, 4), db), ((3, 3), 2)]);
                    let mut join = Join::new();
                    let mut rows = join
                        .prepare_filtered(&left, &right, LIMBS, &mut allow, less)
                        .unwrap()
                        .commit();
                    assert_eq!(rows, oracle(&left, &right));
                    let delta = join
                        .prepare_filtered(&dl, &dr, LIMBS, &mut allow, less)
                        .unwrap()
                        .commit();
                    rows.integrate(&delta, LIMBS, &mut allow).unwrap();
                    let left = left.plus(&dl, LIMBS, &mut allow).unwrap();
                    let right = right.plus(&dr, LIMBS, &mut allow).unwrap();
                    assert_eq!(rows, oracle(&left, &right));
                    assert_eq!(
                        join.snapshot_filtered(LIMBS, &mut allow, less).unwrap(),
                        rows
                    );
                }
            }
        }
    }
}

#[test]
fn rejected_pairs_do_not_multiply_weights_but_both_inputs_remain_retained() {
    let left = z(&[((1, 5), i128::MAX)]);
    let right = z(&[((1, 4), 2)]);
    let mut join = Join::new();
    let rows = join
        .prepare_filtered(&left, &right, LimbLimit::new(0), &mut allow, less)
        .unwrap()
        .commit();
    assert!(rows.is_empty());
    assert_eq!(
        join.left_weight(&1, &5),
        Some(&ZWeight::from_i128(i128::MAX))
    );
    assert_eq!(join.right_weight(&1, &4), Some(&ZWeight::from_i128(2)));
    assert!(
        join.snapshot_filtered(LimbLimit::new(0), &mut allow, less)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        join.snapshot(LimbLimit::new(0), &mut allow),
        Err(ZSetError::Arithmetic(_))
    ));
    join.prepare_filtered(
        &left.negated(LIMBS, &mut allow).unwrap(),
        &right.negated(LIMBS, &mut allow).unwrap(),
        LimbLimit::new(0),
        &mut allow,
        less,
    )
    .unwrap()
    .commit();
    assert_eq!(join, Join::new());
}

fn seeded() -> Join {
    let mut join = Join::new();
    join.prepare_filtered(
        &z(&[((1, 1), 2), ((1, 5), 1)]),
        &z(&[((1, 4), 3)]),
        LIMBS,
        &mut allow,
        less,
    )
    .unwrap()
    .commit();
    join
}

#[test]
fn every_control_refusal_and_dropped_guard_preserves_both_inputs_and_allows_retry() {
    let dl = z(&[((1, 1), -2), ((2, 1), 3)]);
    let dr = z(&[((1, 4), -3), ((2, 4), 2)]);
    let mut success = seeded();
    let mut calls = 0;
    let expected = success
        .prepare_filtered(
            &dl,
            &dr,
            LIMBS,
            &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            },
            less,
        )
        .unwrap()
        .commit();
    assert!(calls > 0);
    for stop in 1..=calls {
        let mut join = seeded();
        let mut seen = 0;
        assert_eq!(
            join.prepare_filtered(
                &dl,
                &dr,
                LIMBS,
                &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                },
                less
            )
            .unwrap_err(),
            ZSetError::Control(stop)
        );
        assert_eq!(join, seeded());
        assert_eq!(
            join.prepare_filtered(&dl, &dr, LIMBS, &mut allow, less)
                .unwrap()
                .commit(),
            expected
        );
        assert_eq!(join, success);
    }
    let mut join = seeded();
    drop(
        join.prepare_filtered(&dl, &dr, LIMBS, &mut allow, less)
            .unwrap(),
    );
    assert_eq!(join, seeded());
}

#[test]
fn predicate_error_after_an_accepted_pair_is_atomic() {
    let mut join = seeded();
    let mut calls = 0;
    let error = join
        .prepare_filtered(
            &z(&[((1, 1), 1), ((1, 5), -1)]),
            &ZSet::new(),
            LIMBS,
            &mut allow,
            |_, _, _, _| {
                calls += 1;
                if calls == 2 {
                    Err(ZSetError::Control(91))
                } else {
                    Ok(true)
                }
            },
        )
        .unwrap_err();
    assert_eq!(calls, 2);
    assert_eq!(error, ZSetError::Control(91));
    assert_eq!(join, seeded());
}

#[test]
fn filtered_ticks_do_not_inspect_unrelated_keys() {
    let mut small = seeded();
    let mut large = seeded();
    let left: Vec<_> = (100..1100).map(|key| ((key, 1), 1)).collect();
    let right: Vec<_> = (100..1100).map(|key| ((key, 4), 1)).collect();
    large
        .prepare_filtered(&z(&left), &z(&right), LIMBS, &mut allow, less)
        .unwrap()
        .commit();
    let mut results = Vec::new();
    for join in [&mut small, &mut large] {
        let mut events = Vec::new();
        let rows = join
            .prepare_filtered(
                &z(&[((1, 1), 1)]),
                &ZSet::new(),
                LIMBS,
                &mut |event| {
                    events.push(event);
                    Ok::<_, usize>(())
                },
                less,
            )
            .unwrap()
            .commit();
        results.push((rows, events));
    }
    assert_eq!(results[0], results[1]);
}
