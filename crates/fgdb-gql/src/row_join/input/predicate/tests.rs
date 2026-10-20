use super::*;
use crate::GraphSetOperand;
use crate::algebra::IntegerComparison;
use fgdb_types::CanonicalScalar;

const LIMBS: LimbLimit = LimbLimit::new(16);
const KINDS: [RowJoinKind; 6] = [
    RowJoinKind::Inner, RowJoinKind::Left, RowJoinKind::Right,
    RowJoinKind::Full, RowJoinKind::Semi, RowJoinKind::Anti,
];

fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }
fn row(value: i64) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![
        GraphValue::Scalar(CanonicalScalar::Int(0)),
        GraphValue::Scalar(CanonicalScalar::Int(value)),
    ])
}
fn input(data: &[(i64, i128)]) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        data.iter().map(|&(value, count)| (row(value), ZWeight::from_i128(count))),
        LIMBS,
        &mut allow,
    ).unwrap()
}
fn definition(kind: RowJoinKind, keyed: bool) -> RowJoinSpec {
    let schema = [GraphSetColumnType::Scalar; 2];
    let spec = if keyed {
        RowJoinSpec::new(&schema, &schema, &[(0, 0)])
    } else {
        RowJoinSpec::cross(&schema, &schema)
    }.unwrap();
    spec.with_kind(kind).with_predicate(&[GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(1),
        comparison: IntegerComparison::Less,
        right: GraphSetOperand::Column(3),
    }]).unwrap()
}
fn seeded(
    kind: RowJoinKind, keyed: bool, left: &[(i64, i128)], right: &[(i64, i128)],
) -> IncrementalRowJoin {
    let mut join = IncrementalRowJoin::new(definition(kind, keyed));
    join.prepare(&input(left), &input(right), LIMBS, None, &mut allow).unwrap().commit();
    join
}

// Full integer/occurrence oracle: never call a join, arrangement, support
// overlay or production predicate to decide whether a row belongs in output.
fn oracle(
    kind: RowJoinKind, left: &[(i64, i128)], right: &[(i64, i128)],
) -> ZSet<GraphValueRow> {
    let pair = |left: Option<i64>, right: Option<i64>| {
        let mut cells = Vec::new();
        for side in [left, right] {
            if let Some(value) = side {
                cells.extend_from_slice(row(value).values());
            } else {
                cells.extend((0..2).map(|_| GraphValue::Scalar(CanonicalScalar::Null)));
            }
        }
        GraphValueRow::from_owned_values(cells)
    };
    let mut out = Vec::new();
    for &(l, lw) in left.iter().filter(|(_, n)| *n > 0) {
        let mut found = false;
        for &(r, rw) in right.iter().filter(|(_, n)| *n > 0) {
            if l < r {
                found = true;
                if matches!(kind, RowJoinKind::Inner | RowJoinKind::Left | RowJoinKind::Right | RowJoinKind::Full) {
                    out.push((pair(Some(l), Some(r)), ZWeight::from_i128(lw * rw)));
                }
            }
        }
        match kind {
            RowJoinKind::Semi if found => out.push((row(l), ZWeight::from_i128(lw))),
            RowJoinKind::Anti if !found => out.push((row(l), ZWeight::from_i128(lw))),
            RowJoinKind::Left | RowJoinKind::Full if !found => {
                out.push((pair(Some(l), None), ZWeight::from_i128(lw)));
            }
            _ => {}
        }
    }
    if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
        for &(r, rw) in right.iter().filter(|(_, n)| *n > 0) {
            if !left.iter().any(|&(l, lw)| lw > 0 && l < r) {
                out.push((pair(None, Some(r)), ZWeight::from_i128(rw)));
            }
        }
    }
    ZSet::from_updates(out, LIMBS, &mut allow).unwrap()
}

fn check_transition(
    kind: RowJoinKind, keyed: bool,
    left: &[(i64, i128)], right: &[(i64, i128)],
    next_left: &[(i64, i128)], next_right: &[(i64, i128)],
) {
    let mut join = seeded(kind, keyed, left, right);
    let before = oracle(kind, left, right);
    let after = oracle(kind, next_left, next_right);
    assert_eq!(join.rows(), &before);
    let dl = input(next_left).minus(&input(left), LIMBS, &mut allow).unwrap();
    let dr = input(next_right).minus(&input(right), LIMBS, &mut allow).unwrap();
    let expected = after.minus(&before, LIMBS, &mut allow).unwrap();
    {
        let tentative = join.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap();
        assert_eq!(tentative.delta(), &expected);
    }
    assert_eq!(join, seeded(kind, keyed, left, right));
    let delta = join.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap().commit();
    assert_eq!(delta, expected);
    assert_eq!(join.rows(), &after);
    assert_eq!(join.total(), &after.total_weight(LIMBS, &mut allow).unwrap());
    join.prepare(
        &dl.negated(LIMBS, &mut allow).unwrap(),
        &dr.negated(LIMBS, &mut allow).unwrap(),
        LIMBS, None, &mut allow,
    ).unwrap().commit();
    assert_eq!(join, seeded(kind, keyed, left, right));
}

#[test]
fn support_turnover_and_count_only_changes_match_complete_bags_in_both_directions() {
    for kind in KINDS {
        for keyed in [false, true] {
            // All old/new two-row support combinations, with non-unit counts.
            let data = |mask: usize, side: usize| {
                [1 + side as i64, 5 + side as i64].into_iter().enumerate()
                    .map(|(at, v)| (v, if mask & (1 << at) == 0 { 0 } else { at as i128 + 2 }))
                    .collect::<Vec<_>>()
            };
            for old in 0..16 {
                for new in 0..16 {
                    check_transition(kind, keyed, &data(old & 3, 0), &data(old >> 2, 1),
                        &data(new & 3, 0), &data(new >> 2, 1));
                }
            }
            // All old/new counts in 0..=2: includes support-preserving positive
            // and negative deltas, not just insertions and last retractions.
            for code in 0..81 {
                let old_l = (code % 3) as i128;
                let old_r = (code / 3 % 3) as i128;
                let new_l = (code / 9 % 3) as i128;
                let new_r = (code / 27) as i128;
                check_transition(kind, keyed, &[(1, old_l)], &[(2, old_r)],
                    &[(1, new_l)], &[(2, new_r)]);
            }
        }
    }
}

#[test]
fn opposite_support_changes_are_not_the_sign_of_the_weight_delta() {
    let old = Group::from_updates(
        [(Arc::new(row(1)), ZWeight::from_i128(3)), (Arc::new(row(2)), ZWeight::from_i128(2))],
        LIMBS, &mut allow,
    ).unwrap();
    let delta = Group::from_updates(
        [(Arc::new(row(1)), ZWeight::from_i128(-1)),
         (Arc::new(row(2)), ZWeight::from_i128(-2)),
         (Arc::new(row(3)), ZWeight::from_i128(4))],
        LIMBS, &mut allow,
    ).unwrap();
    let changes = Witnesses::new(&old, &delta, LIMBS, &mut allow).unwrap();
    assert_eq!(changes.added.len(), 1);
    assert_eq!(changes.removed.len(), 1);
    assert_eq!(changes.added[0].as_ref(), &row(3));
    assert!(changes.removed.contains(&&Arc::new(row(2))));
    assert!(!changes.removed.contains(&&Arc::new(row(1))));
    // One retained witness loses multiplicity, another disappears. Only the
    // second can make an untouched own row lose its final accepted witness.
    for kind in KINDS {
        check_transition(kind, true, &[(0, 2), (1, 3), (2, 4)], &[(1, 3), (2, 2)],
            &[(0, 2), (1, 3), (2, 4)], &[(1, 2), (3, 4)]);
    }
}

#[test]
fn unilateral_and_nonmatching_support_edits_have_delta_local_work_and_scratch() {
    // Count actual native events, excluding initialization. Same-key residual
    // joins make this distinguish a row-local delta from whole-group products.
    for kind in [RowJoinKind::Left, RowJoinKind::Right, RowJoinKind::Full, RowJoinKind::Semi, RowJoinKind::Anti] {
        for edit in 0..3 {
            let mut measurements = Vec::new();
            for n in [32_i64, 128] {
                let left: Vec<_> = (n..2*n).map(|v| (v, 2)).collect();
                let right: Vec<_> = (0..n).map(|v| (v, 2)).collect();
                let mut join = seeded(kind, true, &left, &right);
                let (dl, dr) = match edit {
                    0 => (input(&[(n, 1)]), ZSet::new()),
                    1 => (ZSet::new(), input(&[(0, 1)])),
                    _ => (ZSet::new(), input(&[(0, -2), (-1, 2)])),
                };
                let mut work = 0_u64;
                let mut scratch = 0_u64;
                let pending = join.prepare(&dl, &dr, LIMBS, None, &mut |event| {
                    work += 1;
                    scratch += u64::from(event == ZSetEvent::ScratchEntry);
                    Ok::<_, usize>(())
                }).unwrap();
                let mut next_left = left.clone();
                let mut next_right = right.clone();
                match edit {
                    0 => next_left[0].1 += 1,
                    1 => next_right[0].1 += 1,
                    _ => next_right[0] = (-1, 2),
                }
                let expected = oracle(kind, &next_left, &next_right)
                    .minus(&oracle(kind, &left, &right), LIMBS, &mut allow).unwrap();
                assert_eq!(pending.delta(), &expected);
                pending.commit();
                measurements.push((work, scratch));
            }
            assert!(measurements[1].0 <= 6 * measurements[0].0,
                "fourfold support must not cause a quadratic rescan: {kind:?} edit={edit} {measurements:?}");
            assert_eq!(measurements[1].1, measurements[0].1,
                "unchanged groups must not be cloned or copied into canceling outputs");
        }
    }
}

#[test]
fn every_delta_preparation_refusal_and_final_cap_preserves_both_arrangements() {
    let left = [(1, 2), (5, 3)];
    let right = [(2, 3), (6, 2)];
    let dl = input(&[(1, -1), (5, -3), (7, 2)]);
    let dr = input(&[(2, -3), (3, 2), (6, 1)]);
    for kind in KINDS {
        let expected = seeded(kind, true, &left, &right);
        let mut join = seeded(kind, true, &left, &right);
        let mut calls = 0;
        drop(join.prepare(&dl, &dr, LIMBS, None, &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        }).unwrap());
        assert_eq!(join, expected);
        for stop in 1..=calls {
            let mut seen = 0;
            let error = join.prepare(&dl, &dr, LIMBS, None, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            }).unwrap_err();
            assert_eq!(error, RowJoinError::Delta(ZSetError::Control(stop)));
            assert_eq!(seen, stop);
            assert_eq!(join, expected);
        }
        let after = oracle(kind, &[(1, 1), (7, 2)], &[(3, 2), (6, 3)]);
        let limit = after.total_weight(LIMBS, &mut allow).unwrap().to_i128().unwrap() as u64;
        if limit > 0 {
            assert_eq!(join.prepare(&dl, &dr, LIMBS, Some(limit - 1), &mut allow).unwrap_err(),
                RowJoinError::ResultBudget { limit: limit - 1 });
            assert_eq!(join, expected);
        }
        join.prepare(&dl, &dr, LIMBS, Some(limit), &mut allow).unwrap().commit();
        assert_eq!(join.rows(), &after);
    }
}

#[test]
fn predicated_presence_bulk_intake_does_not_enumerate_rejected_pair_derivatives() {
    for kind in [RowJoinKind::Semi, RowJoinKind::Anti] {
        let mut measurements = Vec::new();
        for n in [32_i64, 128] {
            let rows = input(&(0..n).map(|value| (value, 1)).collect::<Vec<_>>());
            let spec = definition(kind, true)
                .with_predicate(&[GraphSetPredicateOp::Truth(Some(true))]).unwrap();
            let mut join = IncrementalRowJoin::new(spec);
            let mut calls = 0_u64;
            join.prepare(&rows, &rows, LIMBS, Some(n as u64), &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            }).unwrap().commit();
            if kind == RowJoinKind::Semi { assert_eq!(join.rows(), &rows); }
            else { assert!(join.rows().is_empty()); }
            measurements.push(calls);
        }
        assert!(measurements[1] <= 6 * measurements[0], "{kind:?}: {measurements:?}");
    }
}

#[test]
fn right_count_only_updates_do_not_visit_unchanged_presence_rows() {
    for kind in [RowJoinKind::Semi, RowJoinKind::Anti] {
        let mut traces = Vec::new();
        for n in [32_i64, 128] {
            let left: Vec<_> = (n..2*n).map(|v| (v, 2)).collect();
            let right: Vec<_> = (0..n).map(|v| (v, 2)).collect();
            let mut join = seeded(kind, true, &left, &right);
            let mut events = Vec::new();
            let delta = join.prepare(&ZSet::new(), &input(&[(0, 1)]), LIMBS, None, &mut |event| {
                events.push(event);
                Ok::<_, usize>(())
            }).unwrap().commit();
            assert!(delta.is_empty());
            assert_eq!(join.rows(), &oracle(kind, &left, &right));
            traces.push(events);
        }
        assert_eq!(traces[0], traces[1], "{kind:?}: one count update must not scan the left bag");
    }
}
