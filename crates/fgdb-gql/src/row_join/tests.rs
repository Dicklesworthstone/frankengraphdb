use super::*;
use fgdb_types::{CanonicalScalar, VId};

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn row(values: &[Option<i64>]) -> GraphValueRow {
    GraphValueRow::from_owned_values(
        values
            .iter()
            .map(|value| {
                GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
            })
            .collect(),
    )
}
fn bag(rows: &[(GraphValueRow, i128)]) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.iter()
            .map(|(row, weight)| (row.clone(), ZWeight::from_i128(*weight))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn spec() -> RowJoinSpec {
    RowJoinSpec::new(
        &[GraphSetColumnType::Scalar; 2],
        &[GraphSetColumnType::Scalar; 2],
        &[(0, 0)],
    )
    .unwrap()
}
fn seeded(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>) -> IncrementalRowJoin {
    let mut join = IncrementalRowJoin::new(spec());
    join.prepare(left, right, LIMBS, None, &mut allow)
        .unwrap()
        .commit();
    join
}
// Independent full-product oracle: no arrangements, delta terms or join code.
fn oracle(
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
    keys: &[(usize, usize)],
) -> ZSet<GraphValueRow> {
    let mut out = Vec::new();
    for (l, lw) in left.iter() {
        for (r, rw) in right.iter() {
            if keys.iter().all(|&(a, b)| {
                !l.values()[a].is_null()
                    && !r.values()[b].is_null()
                    && l.values()[a] == r.values()[b]
            }) {
                out.push((
                    GraphValueRow::from_owned_values(
                        l.values().iter().chain(r.values()).cloned().collect(),
                    ),
                    ZWeight::from_i128(lw.to_i128().unwrap() * rw.to_i128().unwrap()),
                ));
            }
        }
    }
    ZSet::from_updates(out, LIMBS, &mut allow).unwrap()
}

#[test]
fn all_6561_two_key_weighted_transitions_equal_independent_full_products() {
    for mut code in 0..6561 {
        let mut weights = [0; 8];
        for weight in &mut weights {
            *weight = code % 3;
            code /= 3;
        }
        let make = |at| {
            bag(&[
                (row(&[Some(0), Some(10)]), weights[at]),
                (row(&[Some(1), Some(20)]), weights[at + 1]),
            ])
        };
        let (left, right, next_left, next_right) = (make(0), make(2), make(4), make(6));
        let mut join = seeded(&left, &right);
        let before = oracle(&left, &right, &[(0, 0)]);
        let after = oracle(&next_left, &next_right, &[(0, 0)]);
        assert_eq!(join.rows(), &before);
        let dl = next_left.minus(&left, LIMBS, &mut allow).unwrap();
        let dr = next_right.minus(&right, LIMBS, &mut allow).unwrap();
        let expected = after.minus(&before, LIMBS, &mut allow).unwrap();
        {
            let pending = join.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap();
            assert_eq!(pending.delta(), &expected);
            // A downstream refusal drops the entire tentative update.
        }
        assert_eq!(join, seeded(&left, &right));
        assert_eq!(
            join.prepare(&dl, &dr, LIMBS, None, &mut allow)
                .unwrap()
                .commit(),
            expected
        );
        assert_eq!(join.rows(), &after);
        assert_eq!(
            join.total(),
            &after.total_weight(LIMBS, &mut allow).unwrap()
        );
        join.prepare(
            &dl.negated(LIMBS, &mut allow).unwrap(),
            &dr.negated(LIMBS, &mut allow).unwrap(),
            LIMBS,
            None,
            &mut allow,
        )
        .unwrap()
        .commit();
        assert_eq!(join, seeded(&left, &right));
    }
}

#[test]
fn composite_keys_nulls_canonical_domains_and_full_width_vertices_are_preserved() {
    let make = |id, value| {
        GraphValueRow::from_owned_values(vec![
            GraphValue::Vertex(VId(id)),
            GraphValue::Scalar(value),
        ])
    };
    let left = bag(&[
        (make(u128::MAX, CanonicalScalar::Int(7)), 2),
        (make(0, CanonicalScalar::Int(7)), 1),
        (make(9, CanonicalScalar::Null), 3),
    ]);
    let right = bag(&[
        (make(u128::MAX, CanonicalScalar::Int(7)), 3),
        (make(u128::MAX, CanonicalScalar::Int(8)), 1),
        (make(9, CanonicalScalar::Null), 4),
    ]);
    let schema = [GraphSetColumnType::Vertex, GraphSetColumnType::Scalar];
    let mut join =
        IncrementalRowJoin::new(RowJoinSpec::new(&schema, &schema, &[(0, 0), (1, 1)]).unwrap());
    join.prepare(&left, &right, LIMBS, Some(6), &mut allow)
        .unwrap()
        .commit();
    assert_eq!(join.rows(), &oracle(&left, &right, &[(0, 0), (1, 1)]));
    assert_eq!(join.total(), &ZWeight::from_i128(6));
    // Canonical integer and text domains must never collide under key encoding.
    let text = CanonicalScalar::ucs_basic_text("7").unwrap();
    let other = bag(&[(make(u128::MAX, text), 8)]);
    assert!(
        join.prepare(&ZSet::new(), &other, LIMBS, Some(6), &mut allow)
            .unwrap()
            .delta()
            .is_empty()
    );
    let null_retraction = bag(&[(make(9, CanonicalScalar::Null), -4)]);
    assert!(matches!(
        join.prepare(&null_retraction, &ZSet::new(), LIMBS, None, &mut allow),
        Err(RowJoinError::NegativeMultiplicity { side: 0 })
    ));
    assert_eq!(join.total(), &ZWeight::from_i128(6));
}

#[test]
fn schema_and_negative_unmatched_counts_refuse_before_any_publication() {
    use GraphSetColumnType::{Any, Scalar, Vertex};
    assert_eq!(
        RowJoinSpec::new(&[], &[Scalar], &[(0, 0)]),
        Err(RowJoinBuildError::EmptyInput)
    );
    assert_eq!(
        RowJoinSpec::new(&[Scalar], &[Scalar], &[]),
        Err(RowJoinBuildError::EmptyKeys)
    );
    assert_eq!(
        RowJoinSpec::new(&[Scalar], &[Vertex], &[(0, 0)]),
        Err(RowJoinBuildError::KeyType { key: 0 })
    );
    assert_eq!(
        RowJoinSpec::new(&[Scalar], &[Scalar], &[(0, 1)]),
        Err(RowJoinBuildError::KeyColumn { side: 1, column: 1 })
    );
    assert_eq!(
        RowJoinSpec::new(&[Any], &[Scalar], &[(0, 0)]),
        Err(RowJoinBuildError::UnsupportedColumn { side: 0, column: 0 })
    );
    assert!(matches!(
        RowJoinSpec::new(&[Scalar; MAX_PATTERN_VERTICES], &[Scalar], &[(0, 0)]),
        Err(RowJoinBuildError::TooManyColumns { .. })
    ));
    assert_eq!(
        RowJoinSpec::new(&[Scalar], &[Scalar], &[(0, 0); MAX_PATTERN_VERTICES + 1]),
        Err(RowJoinBuildError::TooManyKeys)
    );
    let mut join = seeded(&ZSet::new(), &ZSet::new());
    for (left, right, expected) in [
        (bag(&[(row(&[Some(5), Some(1)]), -1)]), ZSet::new(), 0),
        (ZSet::new(), bag(&[(row(&[None, Some(1)]), -1)]), 1),
    ] {
        assert_eq!(
            join.prepare(&left, &right, LIMBS, None, &mut allow)
                .unwrap_err(),
            RowJoinError::NegativeMultiplicity { side: expected }
        );
        assert_eq!(join, seeded(&ZSet::new(), &ZSet::new()));
    }
    let wrong = bag(&[(row(&[None]), 1)]);
    assert_eq!(
        join.prepare(&wrong, &ZSet::new(), LIMBS, None, &mut allow)
            .unwrap_err(),
        RowJoinError::InputSchema { side: 0 }
    );
    let wrong = bag(&[(
        GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Vertex(VId(1)),
        ]),
        1,
    )]);
    assert_eq!(
        join.prepare(&wrong, &ZSet::new(), LIMBS, None, &mut allow)
            .unwrap_err(),
        RowJoinError::InputSchema { side: 0 }
    );
    assert!(join.rows().is_empty());
}

#[test]
fn every_preparation_checkpoint_and_downstream_drop_preserves_all_arrangements() {
    let left = bag(&[(row(&[Some(1), Some(10)]), 2)]);
    let right = bag(&[(row(&[Some(1), Some(20)]), 3)]);
    let dl = bag(&[
        (row(&[Some(1), Some(10)]), -1),
        (row(&[Some(1), Some(11)]), 1),
    ]);
    let dr = bag(&[
        (row(&[Some(1), Some(20)]), -2),
        (row(&[Some(1), Some(21)]), 4),
    ]);
    let before = seeded(&left, &right);
    let mut success = seeded(&left, &right);
    let mut calls = 0;
    let mut counts = [0; 2];
    success
        .prepare(&dl, &dr, LIMBS, None, &mut |event| {
            calls += 1;
            counts[usize::from(event == ZSetEvent::ScratchEntry)] += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .commit();
    assert!(calls > 0 && counts.iter().all(|n| *n > 0));
    for stop in 1..=calls {
        let mut join = seeded(&left, &right);
        let mut visited = 0;
        assert_eq!(
            join.prepare(&dl, &dr, LIMBS, None, &mut |_| {
                visited += 1;
                if visited == stop { Err(stop) } else { Ok(()) }
            })
            .unwrap_err(),
            RowJoinError::Delta(ZSetError::Control(stop))
        );
        assert_eq!(visited, stop);
        assert_eq!(join, before);
        join.prepare(&dl, &dr, LIMBS, None, &mut allow)
            .unwrap()
            .commit();
        assert_eq!(join, success);
    }
    for dimension in 0..2 {
        for less in [false, true] {
            let mut join = seeded(&left, &right);
            let mut used = [0; 2];
            let limit = counts[dimension] - usize::from(less);
            let outcome = join
                .prepare(&dl, &dr, LIMBS, None, &mut |event| {
                    let at = usize::from(event == ZSetEvent::ScratchEntry);
                    used[at] += 1;
                    if at == dimension && used[at] > limit {
                        Err(dimension)
                    } else {
                        Ok(())
                    }
                })
                .map(RowJoinUpdate::commit);
            if less {
                assert!(outcome.is_err());
                assert_eq!(join, before);
            } else {
                outcome.unwrap();
                assert_eq!(join, success);
            }
        }
    }
}

#[test]
fn final_occurrences_not_intermediate_prefix_and_checked_wide_products_own_admission() {
    let left = bag(&[(row(&[Some(1), Some(90)]), 2)]);
    let right = bag(&[(row(&[Some(1), Some(20)]), 3)]);
    let mut join = seeded(&left, &right);
    let swap = bag(&[
        (row(&[Some(1), Some(90)]), -2),
        (row(&[Some(1), Some(1)]), 2),
    ]);
    assert!(matches!(
        join.prepare(&swap, &ZSet::new(), LIMBS, Some(5), &mut allow),
        Err(RowJoinError::ResultBudget { limit: 5 })
    ));
    assert_eq!(join, seeded(&left, &right));
    join.prepare(&swap, &ZSet::new(), LIMBS, Some(6), &mut allow)
        .unwrap()
        .commit();
    assert_eq!(join.total(), &ZWeight::from_i128(6));
    let huge = bag(&[(row(&[Some(1), Some(2)]), i128::MAX)]);
    let two = bag(&[(row(&[Some(1), Some(3)]), 2)]);
    let mut wide = IncrementalRowJoin::new(spec());
    assert!(matches!(
        wide.prepare(&huge, &two, LimbLimit::new(0), None, &mut allow),
        Err(RowJoinError::Delta(ZSetError::Arithmetic(_)))
    ));
    assert!(wide.rows().is_empty());
    assert!(matches!(
        wide.prepare(&huge, &two, LIMBS, Some(u64::MAX), &mut allow),
        Err(RowJoinError::ResultBudget { .. })
    ));
    assert!(wide.rows().is_empty());
    wide.prepare(&huge, &two, LIMBS, None, &mut allow)
        .unwrap()
        .commit();
    assert!(wide.total().is_promoted());
    assert_eq!(wide.rows().len(), 1);
    wide.prepare(
        &huge.negated(LIMBS, &mut allow).unwrap(),
        &two.negated(LIMBS, &mut allow).unwrap(),
        LIMBS,
        Some(0),
        &mut allow,
    )
    .unwrap()
    .commit();
    assert!(wide.rows().is_empty());
    assert_eq!(wide.total(), &ZWeight::ZERO);
}

#[test]
fn unrelated_join_groups_do_not_enter_changed_key_work_or_scratch() {
    let run = |extra: i64| {
        let rows: Vec<_> = (0..=extra)
            .map(|key| (row(&[Some(key), Some(10)]), 1))
            .collect();
        let all = bag(&rows);
        let mut join = seeded(&all, &all);
        let delta = bag(&[(row(&[Some(0), Some(10)]), 1)]);
        let mut used = [0; 2];
        let result = join
            .prepare(&delta, &delta, LIMBS, None, &mut |event| {
                used[usize::from(event == ZSetEvent::ScratchEntry)] += 1;
                Ok::<_, usize>(())
            })
            .unwrap()
            .commit();
        (used, result)
    };
    let small = run(1);
    let large = run(2048);
    assert_eq!(small, large);
}
