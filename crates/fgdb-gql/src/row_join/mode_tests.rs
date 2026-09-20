use super::*;
use fgdb_types::{CanonicalScalar, VId};

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn row(key: Option<i64>, payload: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(
        [key, payload]
            .into_iter()
            .map(|v| GraphValue::Scalar(v.map_or(CanonicalScalar::Null, CanonicalScalar::Int)))
            .collect(),
    )
}
fn bag(rows: &[(GraphValueRow, i128)]) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.iter()
            .map(|(row, count)| (row.clone(), ZWeight::from_i128(*count))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn seed(
    kind: RowJoinKind,
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
) -> IncrementalRowJoin {
    let spec = RowJoinSpec::new(
        &[GraphSetColumnType::Scalar; 2],
        &[GraphSetColumnType::Scalar; 2],
        &[(0, 0)],
    )
    .unwrap()
    .with_kind(kind);
    let mut join = IncrementalRowJoin::new(spec);
    join.prepare(left, right, LIMBS, None, &mut allow)
        .unwrap()
        .commit();
    join
}

// Independent full bag semantics, not the derivative, arrangements or counted
// presence kernels. NULL never matches; multiplicities remain occurrence counts.
fn oracle(
    kind: RowJoinKind,
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
) -> ZSet<GraphValueRow> {
    let mut output = Vec::new();
    for (left, lw) in left.iter() {
        let matches: Vec<_> = right
            .iter()
            .filter(|(right, _)| {
                !left.values()[0].is_null()
                    && !right.values()[0].is_null()
                    && left.values()[0] == right.values()[0]
            })
            .collect();
        match kind {
            RowJoinKind::Inner | RowJoinKind::Left if !matches.is_empty() => {
                for (right, rw) in matches {
                    let row = GraphValueRow::from_owned_values(
                        left.values()
                            .iter()
                            .chain(right.values())
                            .cloned()
                            .collect(),
                    );
                    output.push((
                        row,
                        ZWeight::from_i128(lw.to_i128().unwrap() * rw.to_i128().unwrap()),
                    ));
                }
            }
            RowJoinKind::Left => {
                let mut values = left.values().to_vec();
                values.extend((0..2).map(|_| GraphValue::Scalar(CanonicalScalar::Null)));
                output.push((
                    GraphValueRow::from_owned_values(values),
                    lw.checked_clone(LIMBS).unwrap(),
                ));
            }
            RowJoinKind::Semi if !matches.is_empty() => {
                output.push((left.clone(), lw.checked_clone(LIMBS).unwrap()))
            }
            RowJoinKind::Anti if matches.is_empty() => {
                output.push((left.clone(), lw.checked_clone(LIMBS).unwrap()))
            }
            _ => {}
        }
    }
    ZSet::from_updates(output, LIMBS, &mut allow).unwrap()
}

#[test]
fn all_outer_semi_anti_weighted_transitions_equal_independent_bag_semantics() {
    for kind in [RowJoinKind::Left, RowJoinKind::Semi, RowJoinKind::Anti] {
        for mut code in 0..6561_u32 {
            let mut weights = [0_i128; 8];
            for weight in &mut weights {
                *weight = i128::from(code % 3);
                code /= 3;
            }
            let make = |at| {
                bag(&[
                    (row(Some(0), Some(10)), weights[at]),
                    (row(Some(1), Some(20)), weights[at + 1]),
                ])
            };
            let (left, right, next_left, next_right) = (make(0), make(2), make(4), make(6));
            let mut join = seed(kind, &left, &right);
            let before = oracle(kind, &left, &right);
            let after = oracle(kind, &next_left, &next_right);
            assert_eq!(join.rows(), &before);
            let dl = next_left.minus(&left, LIMBS, &mut allow).unwrap();
            let dr = next_right.minus(&right, LIMBS, &mut allow).unwrap();
            let expected = after.minus(&before, LIMBS, &mut allow).unwrap();
            {
                let pending = join.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap();
                assert_eq!(pending.delta(), &expected);
            }
            assert_eq!(join, seed(kind, &left, &right));
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
            assert_eq!(join, seed(kind, &left, &right));
        }
    }
}

#[test]
fn null_keys_first_last_witnesses_and_same_tick_replacement_preserve_exact_rows() {
    let left = bag(&[(row(Some(1), Some(10)), 2), (row(None, Some(30)), 3)]);
    let initial = bag(&[(row(Some(1), None), 2), (row(None, Some(99)), 5)]);
    let successors = [
        bag(&[(row(Some(1), None), 1), (row(None, Some(99)), 5)]),
        bag(&[(row(Some(1), Some(40)), 4), (row(None, Some(99)), 5)]),
        bag(&[(row(None, Some(99)), 5)]),
        ZSet::new(),
        initial.checked_clone(LIMBS, &mut allow).unwrap(),
    ];
    for kind in [RowJoinKind::Left, RowJoinKind::Semi, RowJoinKind::Anti] {
        let mut current = initial.checked_clone(LIMBS, &mut allow).unwrap();
        let mut join = seed(kind, &left, &current);
        for next in &successors {
            let change = next.minus(&current, LIMBS, &mut allow).unwrap();
            let before = oracle(kind, &left, &current);
            let after = oracle(kind, &left, next);
            let delta = join
                .prepare(&ZSet::new(), &change, LIMBS, None, &mut allow)
                .unwrap()
                .commit();
            assert_eq!(delta, after.minus(&before, LIMBS, &mut allow).unwrap());
            assert_eq!(join.rows(), &after);
            current = next.checked_clone(LIMBS, &mut allow).unwrap();
        }
    }
}

#[test]
fn projected_presence_counts_cannot_hide_an_invalid_right_row_retraction() {
    let left = bag(&[(row(Some(1), Some(10)), 2)]);
    let right = bag(&[(row(Some(1), Some(20)), 3), (row(None, Some(30)), 4)]);
    for kind in [RowJoinKind::Left, RowJoinKind::Semi, RowJoinKind::Anti] {
        for key in [Some(1), None] {
            let mut join = seed(kind, &left, &right);
            // Net key projection is zero: a key-count-only validator misses it.
            let invalid = bag(&[(row(key, Some(99)), -1), (row(key, Some(98)), 1)]);
            assert_eq!(
                join.prepare(&ZSet::new(), &invalid, LIMBS, None, &mut allow)
                    .unwrap_err(),
                RowJoinError::NegativeMultiplicity { side: 1 }
            );
            assert_eq!(join, seed(kind, &left, &right));
        }
    }
}

#[test]
fn every_kind_has_atomic_cancellation_drop_unwind_and_final_occurrence_admission() {
    let left = bag(&[(row(Some(1), Some(10)), 2), (row(Some(2), Some(30)), 3)]);
    let right = bag(&[(row(Some(1), Some(20)), 1)]);
    let dl = bag(&[(row(Some(1), Some(10)), -1), (row(Some(2), Some(31)), 2)]);
    let dr = bag(&[(row(Some(1), Some(20)), -1), (row(Some(2), Some(40)), 2)]);
    for kind in [RowJoinKind::Left, RowJoinKind::Semi, RowJoinKind::Anti] {
        let before = seed(kind, &left, &right);
        let mut success = seed(kind, &left, &right);
        let mut count = 0;
        success
            .prepare(&dl, &dr, LIMBS, None, &mut |_| {
                count += 1;
                Ok::<_, usize>(())
            })
            .unwrap()
            .commit();
        assert!(count > 0);
        for stop in 1..=count {
            let mut join = seed(kind, &left, &right);
            let mut at = 0;
            assert_eq!(
                join.prepare(&dl, &dr, LIMBS, None, &mut |_| {
                    at += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                })
                .unwrap_err(),
                RowJoinError::Delta(ZSetError::Control(stop))
            );
            assert_eq!(at, stop);
            assert_eq!(join, before);
        }
        let mut join = seed(kind, &left, &right);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _pending = join.prepare(&dl, &dr, LIMBS, None, &mut allow).unwrap();
            panic!("downstream refused before publication");
        }));
        assert!(panic.is_err());
        assert_eq!(join, before);
        let total = u64::try_from(success.total().to_i128().unwrap()).unwrap();
        assert!(total > 0);
        assert_eq!(
            join.prepare(&dl, &dr, LIMBS, Some(total - 1), &mut allow)
                .unwrap_err(),
            RowJoinError::ResultBudget { limit: total - 1 }
        );
        assert_eq!(join, before);
        join.prepare(&dl, &dr, LIMBS, Some(total), &mut allow)
            .unwrap()
            .commit();
        assert_eq!(join, success);
    }
}

#[test]
fn output_schema_null_extended_vertices_and_composite_keys_keep_native_domains() {
    let schema = [GraphSetColumnType::Vertex, GraphSetColumnType::Scalar];
    let left_row = GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::Scalar(CanonicalScalar::Int(9)),
    ]);
    let left = bag(&[(left_row.clone(), 2)]);
    for kind in [RowJoinKind::Left, RowJoinKind::Semi, RowJoinKind::Anti] {
        let spec = RowJoinSpec::new(&schema, &schema, &[(0, 0), (1, 1)])
            .unwrap()
            .with_kind(kind);
        assert_eq!(spec.kind(), kind);
        assert_eq!(spec.width(), if kind == RowJoinKind::Left { 4 } else { 2 });
        assert_eq!(
            spec.column_types().collect::<Vec<_>>(),
            if kind == RowJoinKind::Left {
                [schema, schema].concat()
            } else {
                schema.to_vec()
            }
        );
        let mut join = IncrementalRowJoin::new(spec);
        join.prepare(&left, &ZSet::new(), LIMBS, None, &mut allow)
            .unwrap()
            .commit();
        if kind == RowJoinKind::Left {
            let values = join.rows().iter().next().unwrap().0.values();
            assert_eq!(values[0], GraphValue::Vertex(VId(u128::MAX)));
            assert!(values[2].is_null() && values[3].is_null());
            assert!(
                join.spec()
                    .column_types()
                    .zip(values)
                    .all(|(kind, value)| kind.accepts(value))
            );
        }
        join.prepare(&ZSet::new(), &left, LIMBS, None, &mut allow)
            .unwrap()
            .commit();
        if kind == RowJoinKind::Anti {
            assert!(join.rows().is_empty());
        } else {
            assert_eq!(
                join.total(),
                &ZWeight::from_i128(if kind == RowJoinKind::Semi { 2 } else { 4 })
            );
        }
        // A different scalar kind in a composite key is not an integer match.
        let different = GraphValueRow::from_owned_values(vec![
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("9").unwrap()),
        ]);
        assert!(
            join.prepare(
                &ZSet::new(),
                &bag(&[(different, 5)]),
                LIMBS,
                None,
                &mut allow
            )
            .unwrap()
            .delta()
            .is_empty()
        );
    }
}

#[test]
fn semi_anti_witness_changes_do_not_enumerate_a_cartesian_product() {
    let run = |kind, width: i64| {
        let left = bag(&(0..width)
            .map(|n| (row(Some(1), Some(n)), 1))
            .collect::<Vec<_>>());
        let right = bag(&(0..width)
            .map(|n| (row(Some(1), Some(n)), 2))
            .collect::<Vec<_>>());
        let mut join = seed(kind, &left, &right);
        let mut events = [0; 2];
        let delta = join
            .prepare(
                &ZSet::new(),
                &bag(&[(row(Some(1), Some(0)), -1)]),
                LIMBS,
                None,
                &mut |event| {
                    events[usize::from(event == ZSetEvent::ScratchEntry)] += 1;
                    Ok::<_, usize>(())
                },
            )
            .unwrap()
            .commit();
        assert!(delta.is_empty());
        events
    };
    for kind in [RowJoinKind::Semi, RowJoinKind::Anti] {
        assert_eq!(run(kind, 1), run(kind, 2048));
    }
}

#[test]
fn semi_and_outer_null_extension_keep_wide_left_multiplicity_exact() {
    let huge = bag(&[(row(Some(1), Some(10)), i128::MAX)]);
    let one = bag(&[(row(Some(1), Some(20)), 1)]);
    for kind in [RowJoinKind::Left, RowJoinKind::Semi, RowJoinKind::Anti] {
        let mut join = seed(kind, &huge, &ZSet::new());
        let right = if kind == RowJoinKind::Anti {
            ZSet::new()
        } else {
            one.checked_clone(LIMBS, &mut allow).unwrap()
        };
        join.prepare(&huge, &right, LIMBS, None, &mut allow)
            .unwrap()
            .commit();
        assert!(join.total().is_promoted());
        assert_eq!(
            join.total(),
            &ZWeight::from_i128(i128::MAX)
                .checked_mul(&ZWeight::from_i128(2), LIMBS)
                .unwrap()
        );
        assert_eq!(join.rows().len(), 1);
    }
}
