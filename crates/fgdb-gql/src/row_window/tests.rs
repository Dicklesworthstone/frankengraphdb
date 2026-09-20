use super::*;
use crate::algebra::{GlaOperator, GraphValue};
use crate::algebra_exec::ProjectedRows;
use fgdb_types::{CanonicalScalar, VId};

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}

fn row(id: u64, rank: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(id.into())),
        GraphValue::Scalar(rank.map_or(CanonicalScalar::Null, CanonicalScalar::Int)),
    ])
}
fn rows() -> [GraphValueRow; 3] {
    [row(0, None), row(1, Some(-5)), row(2, Some(7))]
}
fn z(updates: &[(GraphValueRow, i128)]) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        updates
            .iter()
            .map(|(row, weight)| (row.clone(), ZWeight::from_i128(*weight))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn bag(code: i128) -> [i128; 3] {
    [code % 3, code / 3 % 3, code / 9 % 3]
}
fn spec(
    quantifier: GraphSetQuantifier,
    descending: bool,
    nulls_first: bool,
    offset: u64,
    count: u64,
) -> RowWindowSpec {
    RowWindowSpec::new(
        vec![GraphSetColumnType::Vertex, GraphSetColumnType::Scalar],
        vec![GraphValueOrder {
            column: 1,
            descending,
            nulls_first,
        }],
        quantifier,
        offset,
        count,
    )
    .unwrap()
}
fn compressed(stage: &IncrementalRowWindow) -> Vec<(GraphValueRow, i128)> {
    stage
        .rows()
        .map(|(row, weight)| (row.clone(), weight.to_i128().unwrap()))
        .collect()
}

// Exercise the ordinary GLA collector, not the new weighted selection code.
// Tiny fixtures intentionally expand occurrences before ordinary pagination.
fn batch(
    input: &[GraphValueRow; 3],
    counts: [i128; 3],
    spec: &RowWindowSpec,
) -> Vec<GraphValueRow> {
    let mut collector = ProjectedRows::<GraphValueRow>::for_plan(
        spec.quantifier == GraphSetQuantifier::Distinct,
        &[
            GlaOperator::OrderByValueColumns {
                columns: Arc::clone(&spec.order),
            },
            GlaOperator::Limit {
                offset: 0,
                count: None,
            },
        ],
    );
    // Reverse source enumeration to test deterministic ordering.
    for (row, weight) in input.iter().zip(counts).rev() {
        for _ in 0..weight {
            if collector
                .should_retain_value(row, &mut |_| Ok::<_, ()>(()))
                .unwrap()
            {
                collector.insert_value(row.clone());
            }
        }
    }
    collector
        .into_rows()
        .skip(spec.offset as usize)
        .take(spec.count as usize)
        .collect()
}

#[test]
fn all_and_distinct_deltas_match_batch_gla_for_every_small_transition_and_null_order() {
    let input = rows();
    for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
        for descending in [false, true] {
            for nulls_first in [false, true] {
                let spec = spec(quantifier, descending, nulls_first, 1, 2);
                for before_code in 0..27 {
                    for after_code in 0..27 {
                        let before = bag(before_code);
                        let after = bag(after_code);
                        let seed = z(&input
                            .iter()
                            .zip(before)
                            .map(|(row, n)| (row.clone(), n))
                            .collect::<Vec<_>>());
                        let changes = z(&input
                            .iter()
                            .enumerate()
                            .map(|(at, row)| (row.clone(), after[at] - before[at]))
                            .collect::<Vec<_>>());
                        let mut stage = IncrementalRowWindow::new(spec.clone());
                        let mut sink = stage.apply(&seed, LIMBS, None, &mut allow).unwrap();
                        let expected_before = batch(&input, before, &spec);
                        assert_eq!(
                            sink,
                            z(&expected_before
                                .into_iter()
                                .map(|row| (row, 1))
                                .collect::<Vec<_>>())
                        );
                        let delta = stage.apply(&changes, LIMBS, None, &mut allow).unwrap();
                        sink.integrate(&delta, LIMBS, &mut allow).unwrap();
                        let expected = batch(&input, after, &spec);
                        assert_eq!(
                            sink,
                            z(&expected
                                .iter()
                                .cloned()
                                .map(|row| (row, 1))
                                .collect::<Vec<_>>())
                        );
                        let actual: Vec<_> = compressed(&stage)
                            .into_iter()
                            .flat_map(|(row, n)| (0..n).map(move |_| row.clone()))
                            .collect();
                        assert_eq!(actual, expected);
                        assert_eq!(stage.total().to_i128(), Some(expected.len() as i128));
                    }
                }
            }
        }
    }
}

#[test]
fn distinct_retains_raw_duplicates_and_promotes_tail_only_after_last_retraction() {
    let a = row(1, Some(7));
    let b = row(2, Some(7));
    let mut stage = IncrementalRowWindow::new(spec(
        GraphSetQuantifier::Distinct,
        true,
        true,
        0,
        1,
    ));
    stage
        .apply(&z(&[(a.clone(), 4), (b.clone(), 1)]), LIMBS, Some(1), &mut allow)
        .unwrap();
    assert!(
        stage
            .apply(&z(&[(a.clone(), -3)]), LIMBS, Some(1), &mut allow)
            .unwrap()
            .is_empty()
    );
    assert_eq!(compressed(&stage), vec![(a.clone(), 1)]);
    assert_eq!(
        stage
            .apply(&z(&[(a.clone(), -1)]), LIMBS, Some(1), &mut allow)
            .unwrap(),
        z(&[(a, -1), (b.clone(), 1)])
    );
    assert_eq!(compressed(&stage), vec![(b, 1)]);
}

fn seeded(quantifier: GraphSetQuantifier) -> IncrementalRowWindow {
    let mut stage = IncrementalRowWindow::new(spec(quantifier, true, false, 0, 2));
    stage
        .apply(
            &z(&[
                (row(0, None), 3),
                (row(1, Some(-5)), 2),
                (row(2, Some(7)), 1),
            ]),
            LIMBS,
            None,
            &mut allow,
        )
        .unwrap();
    stage
}

#[test]
fn every_checkpoint_downstream_drop_and_unwind_preserve_all_participants() {
    let changes = z(&[(row(2, Some(7)), -1), (row(3, Some(9)), 1)]);
    for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
        let mut success = seeded(quantifier);
        let mut calls = 0;
        let expected = success
            .apply(&changes, LIMBS, None, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        for stop in 1..=calls {
            let mut stage = seeded(quantifier);
            let mut seen = 0;
            assert_eq!(
                stage.apply(&changes, LIMBS, None, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(RowWindowError::Delta(ZSetError::Control(stop)))
            );
            assert_eq!(stage, seeded(quantifier));
            assert_eq!(
                stage.apply(&changes, LIMBS, None, &mut allow).unwrap(),
                expected
            );
            assert_eq!(stage, success);
        }
        let mut stage = seeded(quantifier);
        let mut sink = z(&compressed(&stage));
        {
            let update = stage.prepare(&changes, LIMBS, None, &mut allow).unwrap();
            let _pending_sink = sink
                .prepare_update(update.delta(), LIMBS, &mut allow)
                .unwrap();
        }
        assert_eq!(stage, seeded(quantifier));
        assert_eq!(sink, z(&compressed(&stage)));
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _pending = stage.prepare(&changes, LIMBS, None, &mut allow).unwrap();
            panic!("downstream refusal");
        }));
        assert!(unwind.is_err());
        assert_eq!(stage, seeded(quantifier));
        let update = stage.prepare(&changes, LIMBS, None, &mut allow).unwrap();
        let pending_sink = sink
            .prepare_update(update.delta(), LIMBS, &mut allow)
            .unwrap();
        let _ = update.commit();
        pending_sink.commit();
        assert_eq!(stage, success);
        assert_eq!(sink, z(&compressed(&stage)));
    }
}

#[test]
fn invisible_schema_errors_and_raw_negative_counts_cannot_hide_behind_distinct_or_limit_zero() {
    for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
        let definition = spec(quantifier, true, false, 0, 0);
        let mut stage = IncrementalRowWindow::new(definition.clone());
        let seed = z(&[(row(1, Some(1)), 1)]);
        stage.apply(&seed, LIMBS, Some(0), &mut allow).unwrap();
        assert_eq!(
            stage.apply(&z(&[(row(1, Some(1)), -2)]), LIMBS, Some(0), &mut allow),
            Err(RowWindowError::NegativeMultiplicity)
        );
        assert_eq!(
            stage.apply(&z(&[(row(99, Some(-99)), -1)]), LIMBS, Some(0), &mut allow),
            Err(RowWindowError::NegativeMultiplicity)
        );
        let bad = GraphValueRow::from_owned_values(vec![
            GraphValue::Vertex(VId(999)),
            GraphValue::Vertex(VId(1)),
        ]);
        assert_eq!(
            stage.apply(&z(&[(bad, 1)]), LIMBS, Some(0), &mut allow),
            Err(RowWindowError::InputSchema)
        );
        let mut expected = IncrementalRowWindow::new(definition);
        expected.apply(&seed, LIMBS, Some(0), &mut allow).unwrap();
        assert_eq!(stage, expected);
    }
}

#[test]
fn final_occurrence_quota_accepts_atomic_swaps_and_refuses_growth_without_publication() {
    let definition = spec(GraphSetQuantifier::All, false, false, 0, 10);
    let mut stage = IncrementalRowWindow::new(definition);
    stage
        .apply(&z(&[(row(2, Some(7)), 1)]), LIMBS, Some(1), &mut allow)
        .unwrap();
    stage
        .apply(
            &z(&[(row(1, Some(-5)), 1), (row(2, Some(7)), -1)]),
            LIMBS,
            Some(1),
            &mut allow,
        )
        .unwrap();
    let changes = z(&[(row(0, None), 1)]);
    assert_eq!(
        stage.apply(&changes, LIMBS, Some(1), &mut allow),
        Err(RowWindowError::ResultBudget { limit: 1 })
    );
    assert_eq!(compressed(&stage), vec![(row(1, Some(-5)), 1)]);
    stage.apply(&changes, LIMBS, Some(2), &mut allow).unwrap();
    assert_eq!(stage.total().to_i128(), Some(2));
}

#[test]
fn promoted_multiplicity_and_large_payloads_remain_compressed_and_cancellable() {
    let value = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
        CanonicalScalar::bytes(vec![42; 8192]).unwrap(),
    )]);
    let weight = ZWeight::from_i128(i128::MAX)
        .checked_add(&ZWeight::ONE, LIMBS)
        .unwrap();
    let seed = ZSet::from_updates([(value, weight)], LIMBS, &mut allow).unwrap();
    let definition = RowWindowSpec::new(
        vec![GraphSetColumnType::Scalar],
        vec![],
        GraphSetQuantifier::All,
        u64::MAX,
        u64::MAX,
    )
    .unwrap();
    let mut stage = IncrementalRowWindow::new(definition.clone());
    let mut calls = 0;
    stage
        .apply(&seed, LIMBS, None, &mut |_| {
            calls += 1;
            assert!(calls < 2000);
            Ok::<_, usize>(())
        })
        .unwrap();
    assert_eq!(stage.total().to_i128(), Some(i128::from(u64::MAX)));
    assert_eq!(stage.rows().len(), 1);
    assert!(calls > 256, "both owned payload copies are charged");
    let mut denied = IncrementalRowWindow::new(definition.clone());
    assert!(denied.apply(&seed, LimbLimit::new(0), None, &mut allow).is_err());
    assert_eq!(denied, IncrementalRowWindow::new(definition));
    stage
        .apply(
            &seed.negated(LIMBS, &mut allow).unwrap(),
            LIMBS,
            None,
            &mut allow,
        )
        .unwrap();
    assert!(stage.rows().next().is_none());
}

#[test]
fn schema_and_order_admission_and_zero_column_identity_are_explicit() {
    assert!(matches!(
        RowWindowSpec::new(
            vec![],
            vec![GraphValueOrder::ascending(0)],
            GraphSetQuantifier::All,
            0,
            1,
        ),
        Err(RowWindowBuildError::Order(GraphOrderError::UnknownColumn {
            column: 0,
        }))
    ));
    assert!(matches!(
        RowWindowSpec::new(
            vec![GraphSetColumnType::Scalar],
            vec![GraphValueOrder::ascending(0), GraphValueOrder::descending(0)],
            GraphSetQuantifier::All,
            0,
            1,
        ),
        Err(RowWindowBuildError::Order(GraphOrderError::DuplicateColumn {
            column: 0,
        }))
    ));
    assert!(matches!(
        RowWindowSpec::new(
            vec![GraphSetColumnType::Any; MAX_PATTERN_VERTICES + 1],
            vec![],
            GraphSetQuantifier::All,
            0,
            1,
        ),
        Err(RowWindowBuildError::InputWidth { .. })
    ));
    let definition = RowWindowSpec::new(vec![], vec![], GraphSetQuantifier::All, 1, 2).unwrap();
    let mut stage = IncrementalRowWindow::new(definition);
    stage
        .apply(&z(&[(GraphValueRow::unit(), 4)]), LIMBS, None, &mut allow)
        .unwrap();
    assert_eq!(compressed(&stage), vec![(GraphValueRow::unit(), 2)]);
}

#[test]
fn native_diagnostics_never_include_payloads() {
    let value = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
        CanonicalScalar::bytes(b"secret-window-value".to_vec()).unwrap(),
    )]);
    let definition = RowWindowSpec::new(
        vec![GraphSetColumnType::Scalar],
        vec![],
        GraphSetQuantifier::Distinct,
        0,
        1,
    )
    .unwrap();
    let mut stage = IncrementalRowWindow::new(definition);
    let seed = z(&[(value, 1)]);
    let pending = stage.prepare(&seed, LIMBS, None, &mut allow).unwrap();
    assert!(!format!("{pending:?}").contains("secret-window-value"));
    let _ = pending.commit();
    assert!(!format!("{stage:?}").contains("secret-window-value"));
}
