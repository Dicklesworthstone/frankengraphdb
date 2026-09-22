use super::*;
use crate::{
    GraphIntegerExpression, GraphIntegerOp, GraphSetProjection, GraphSetQuantifier, GraphSetValue,
};

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    panic!("source-free factor opened a graph")
}
fn factor(values: &[Option<i64>]) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "x".into(),
            GraphSetValue::List(
                values
                    .iter()
                    .map(|value| {
                        GraphSetValue::Value(GraphValue::Scalar(
                            value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                        ))
                    })
                    .collect(),
            ),
        )
        .unwrap()
        .project(
            vec![GraphSetProjection::new(
                "x",
                GraphSetValue::Integer(
                    GraphIntegerExpression::prepare(&[GraphIntegerOp::Column(0)]).unwrap(),
                ),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap()
}
fn definition(aggregates: &[GraphAggregate<'_>]) -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare_set_relation(factor(&[Some(1)]), &[], aggregates, 0, None)
        .unwrap()
}
fn values(states: Vec<NumericState>) -> Vec<crate::GraphAggregateValue> {
    states
        .into_iter()
        .map(|state| state.finish_governed(&mut |_| Ok::<_, ()>(())).unwrap())
        .collect()
}

#[test]
fn repeated_numeric_and_support_cells_equal_expanded_updates() {
    let q = definition(&[
        GraphAggregate::count_rows("rows"),
        GraphAggregate::count("count", 0),
        GraphAggregate::sum_int("sum", 0),
        GraphAggregate::average_int("avg", 0),
        GraphAggregate::count_distinct("distinct", 0),
        GraphAggregate::sum_int_distinct("dsum", 0),
        GraphAggregate::average_int_distinct("davg", 0),
        GraphAggregate::min("min", 0),
        GraphAggregate::max("max", 0),
    ]);
    for repeats in 1..=17 {
        let mut compressed = new_states(&q, &mut |_| Ok::<_, Failure<(), ()>>(())).unwrap();
        let mut expanded = new_states(&q, &mut |_| Ok::<_, Failure<(), ()>>(())).unwrap();
        for value in [None, Some(-7), Some(0), Some(3), Some(-7), None] {
            let cell =
                GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int));
            update(
                &q,
                &mut compressed,
                &mut |_| &cell,
                Some(repeats),
                &mut |_| Ok::<_, Failure<(), ()>>(()),
            )
            .unwrap();
            for _ in 0..repeats {
                unit(&q, &mut expanded, &mut |_| &cell, &mut |_| {
                    Ok::<_, Failure<(), ()>>(())
                })
                .unwrap();
            }
        }
        assert_eq!(values(compressed), values(expanded));
    }
}

#[test]
fn the_first_failing_occurrence_wins_before_aggregate_column_order() {
    let q = definition(&[
        GraphAggregate::count_rows("count"),
        GraphAggregate::sum_int("sum", 0),
    ]);
    let cell = GraphValue::Scalar(CanonicalScalar::Int(1));
    for (count, sum, failing) in [
        (u64::MAX - 4, i128::MAX - 1, 1),
        (u64::MAX - 1, i128::MAX - 4, 0),
        (u64::MAX - 1, i128::MAX - 1, 0),
    ] {
        let make = || vec![NumericState::Count(count), NumericState::Sum(Some(sum))];
        let mut compressed = make();
        let actual = update(&q, &mut compressed, &mut |_| &cell, Some(5), &mut |_| {
            Ok::<_, Failure<(), ()>>(())
        });
        let mut expanded = make();
        let expected = (0..5).try_for_each(|_| {
            unit(&q, &mut expanded, &mut |_| &cell, &mut |_| {
                Ok::<_, Failure<(), ()>>(())
            })
        });
        assert_eq!(actual, expected);
        assert!(matches!(actual, Err(GqlQueryError::Source(
            GraphAggregateError::ArithmeticOverflow { aggregate }
        )) if aggregate == failing));
    }
}

#[test]
fn signed_distance_is_not_rejected_for_an_overflowing_standalone_product() {
    let q = definition(&[GraphAggregate::sum_int("sum", 0)]);
    for (initial, step, expected) in [(i128::MIN, 1, i128::MAX), (i128::MAX, -1, i128::MIN)] {
        let cell = GraphValue::Scalar(CanonicalScalar::Int(step));
        let mut states = vec![NumericState::Sum(Some(initial))];
        update(
            &q,
            &mut states,
            &mut |_| &cell,
            Some(u128::MAX),
            &mut |_| Ok::<_, Failure<(), ()>>(()),
        )
        .unwrap();
        assert_eq!(
            values(states),
            vec![crate::GraphAggregateValue::Integer(expected)]
        );
    }
    assert_eq!(advance_sum(0, i64::MIN, 1_u128 << 64), i128::MIN);
}

#[test]
fn oversized_repetitions_are_valid_for_zero_null_and_distinct_support() {
    let q = definition(&[
        GraphAggregate::sum_int("zero", 0),
        GraphAggregate::count_distinct("one", 0),
    ]);
    let zero = GraphValue::Scalar(CanonicalScalar::Int(0));
    let mut states = new_states(&q, &mut |_| Ok::<_, Failure<(), ()>>(())).unwrap();
    update(&q, &mut states, &mut |_| &zero, None, &mut |_| {
        Ok::<_, Failure<(), ()>>(())
    })
    .unwrap();
    assert_eq!(
        values(states),
        vec![
            crate::GraphAggregateValue::Integer(0),
            crate::GraphAggregateValue::Count(1)
        ]
    );
    let q = definition(&[
        GraphAggregate::count("null", 0),
        GraphAggregate::average_int("avg", 0),
    ]);
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let mut states = new_states(&q, &mut |_| Ok::<_, Failure<(), ()>>(())).unwrap();
    update(&q, &mut states, &mut |_| &null, None, &mut |_| {
        Ok::<_, Failure<(), ()>>(())
    })
    .unwrap();
    assert_eq!(values(states)[0], crate::GraphAggregateValue::Count(0));
}

#[test]
fn grouped_and_global_summaries_match_materialization_across_occurrence_pages() {
    for grouped in [false, true] {
        for n in 0..=4 {
            for (skip, limit) in [
                (0, None),
                (1, None),
                (1, Some(3)),
                (0, Some(0)),
                (u64::MAX, None),
            ] {
                let input = factor(&[None, Some(3), Some(-1), Some(3)])
                    .cross_join(factor(&vec![Some(7); n]))
                    .unwrap()
                    .with_page(skip, limit)
                    .nested()
                    .unwrap()
                    .with_page(0, Some(11));
                let q = PreparedGraphAggregate::prepare_set_relation(
                    input,
                    if grouped { &[0] } else { &[] },
                    &[
                        GraphAggregate::count_rows("rows"),
                        GraphAggregate::count("n", 0),
                        GraphAggregate::sum_int("sum", 0),
                        GraphAggregate::average_int("avg", 0),
                        GraphAggregate::count_distinct("d", 0),
                        GraphAggregate::sum_int_distinct("ds", 0),
                        GraphAggregate::average_int_distinct("da", 0),
                        GraphAggregate::min("min", 0),
                        GraphAggregate::max("max", 0),
                    ],
                    0,
                    None,
                )
                .unwrap();
                assert!(
                    q.input_relation()
                        .unwrap()
                        .has_repeated_factor(&q.repeated_columns())
                );
                let before = q.canonical_bytes();
                let actual = q
                    .execute_relational_with_source(wide(), no_source, || Ok(()))
                    .unwrap();
                let expected = q
                    .execute_relational_materialized(wide(), no_source, || Ok(()))
                    .unwrap();
                assert_eq!(actual.value, expected.value);
                assert_eq!(actual.rows.result_rows, actual.value.len() as u64);
                assert_eq!(q.canonical_bytes(), before);
            }
        }
    }
}

fn power(mut relation: PreparedGraphSet, squarings: usize) -> PreparedGraphSet {
    for _ in 0..squarings {
        relation = relation.clone().cross_join(relation).unwrap();
    }
    relation
}

#[test]
fn trillion_occurrence_groups_fit_factor_sized_budgets_and_keep_exact_averages() {
    let input = factor(&[Some(3), Some(3), Some(-1)])
        .cross_join(power(factor(&[Some(0); 32]), 3))
        .unwrap();
    let q = PreparedGraphAggregate::prepare_set_relation(
        input,
        &[0],
        &[
            GraphAggregate::count_rows("n"),
            GraphAggregate::sum_int("s", 0),
            GraphAggregate::average_int("a", 0),
            GraphAggregate::count_distinct("d", 0),
        ],
        0,
        None,
    )
    .unwrap();
    let result = q
        .execute_relational_with_source(
            GqlQueryPolicy::new(0, 2, 100_000, 20_000),
            no_source,
            || Ok(()),
        )
        .unwrap();
    let count = 1_u64 << 40;
    assert_eq!(result.value.len(), 2);
    assert_eq!(result.value[0].values()[0].as_count(), Some(count));
    assert_eq!(result.value[1].values()[0].as_count(), Some(2 * count));
    assert_eq!(
        result.value[1].values()[1],
        crate::GraphAggregateValue::Integer(6 * i128::from(count))
    );
    assert_eq!(
        result.value[1].values()[2],
        crate::GraphAggregateValue::Average(crate::GraphExactAverage::new(3, 1).unwrap())
    );
    assert_eq!(result.value[1].values()[3].as_count(), Some(1));
}

#[test]
fn weights_above_u64_do_not_spuriously_overflow_wide_sums_or_support() {
    let q = PreparedGraphAggregate::prepare_set_relation(
        factor(&[Some(1)])
            .cross_join(power(factor(&[Some(0); 16]), 4))
            .unwrap(),
        &[],
        &[GraphAggregate::sum_int("s", 0)],
        0,
        None,
    )
    .unwrap();
    let result = q
        .execute_relational_with_source(wide(), no_source, || Ok(()))
        .unwrap();
    assert_eq!(
        result.value[0].values()[0],
        crate::GraphAggregateValue::Integer(1_i128 << 64)
    );
    let enormous = power(factor(&[Some(0); 256]), 5);
    for step in [0, 1] {
        let q = PreparedGraphAggregate::prepare_set_relation(
            factor(&[Some(step)]).cross_join(enormous.clone()).unwrap(),
            &[],
            &[
                GraphAggregate::sum_int("s", 0),
                GraphAggregate::count_distinct("d", 0),
            ],
            0,
            None,
        )
        .unwrap();
        let result = q.execute_relational_with_source(wide(), no_source, || Ok(()));
        if step == 0 {
            let result = result.unwrap();
            assert_eq!(
                result.value[0].values()[0],
                crate::GraphAggregateValue::Integer(0)
            );
            assert_eq!(result.value[0].values()[1].as_count(), Some(1));
        } else {
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(
                    GraphAggregateError::ArithmeticOverflow { aggregate: 0 }
                ))
            ));
        }
    }
}

#[test]
fn exact_quotas_and_all_interruption_checkpoints_share_one_allowance() {
    let input = factor(&[Some(1), Some(2)])
        .cross_join(factor(&[Some(0); 5]))
        .unwrap();
    let q = PreparedGraphAggregate::prepare_set_relation(
        input,
        &[0],
        &[GraphAggregate::sum_int("s", 0)],
        0,
        None,
    )
    .unwrap();
    let mut calls = 0;
    let baseline = q
        .execute_relational_with_source(wide(), no_source, || {
            calls += 1;
            Ok(())
        })
        .unwrap();
    let exact = GqlQueryPolicy::new(
        0,
        2,
        baseline.evaluator.work_units,
        baseline.evaluator.scratch_entries,
    );
    assert_eq!(
        q.execute_relational_with_source(exact, no_source, || Ok(()))
            .unwrap()
            .value,
        baseline.value
    );
    for stop in 1..=calls {
        let mut seen = 0;
        let result = q.execute_relational_with_source(exact, no_source, || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
    for policy in [
        GqlQueryPolicy::new(0, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(0, 2, baseline.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(0, 2, u64::MAX, baseline.evaluator.scratch_entries - 1),
    ] {
        assert!(
            q.execute_relational_with_source(policy, no_source, || Ok(()))
                .is_err()
        );
    }
}

fn graph_leaf() -> PreparedGraphSet {
    let mut builder = crate::algebra::GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .prepare_values(
            &[crate::algebra::GraphColumn::property(
                "v",
                "n",
                fgdb_delta_types::PropertyKeyId(1),
            )],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into()
}

#[test]
fn graph_sources_execute_once_and_share_snapshot_admission() {
    let q = PreparedGraphAggregate::prepare_set_relation(
        graph_leaf().cross_join(graph_leaf()).unwrap(),
        &[],
        &[GraphAggregate::sum_int("s", 0)],
        0,
        None,
    )
    .unwrap();
    for max_snapshot in [3, 4] {
        let mut calls = 0;
        let scalar = CanonicalScalar::Int(3);
        let result = q.execute_relational_with_source(
            GqlQueryPolicy::new(max_snapshot, 1, 100_000, 10_000),
            |pattern, remaining| {
                calls += 1;
                pattern.plan().execute_governed_with_properties(
                    2,
                    [VId(1), VId(2)],
                    [],
                    |_, _| Ok::<_, &'static str>(true),
                    |_, _| Ok(Some(&scalar)),
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok(()),
        );
        assert_eq!(calls, 2);
        if max_snapshot == 4 {
            let result = result.unwrap();
            assert_eq!(result.rows.snapshot_records, 4);
            assert_eq!(
                result.value[0].values()[0],
                crate::GraphAggregateValue::Integer(12)
            );
        } else {
            assert!(matches!(result, Err(GqlQueryError::Rows(error))
                if error.dimension == GqlBudgetDimension::SnapshotRecords));
        }
    }
}

#[test]
fn later_right_source_failure_precedes_an_invalid_left_sum_even_at_limit_zero() {
    let left = PreparedGraphSet::singleton()
        .unwind(
            "bad".into(),
            GraphSetValue::List(vec![GraphSetValue::Value(GraphValue::Scalar(
                CanonicalScalar::Bool(true),
            ))]),
        )
        .unwrap();
    let q = PreparedGraphAggregate::prepare_set_relation(
        left.cross_join(graph_leaf()).unwrap(),
        &[],
        &[GraphAggregate::sum_int("s", 0)],
        0,
        Some(0),
    )
    .unwrap();
    let mut calls = 0;
    let result = q.execute_relational_with_source(
        wide(),
        |_, _| {
            calls += 1;
            Err(GqlQueryError::Source("right failed"))
        },
        || Ok::<_, usize>(()),
    );
    assert_eq!(calls, 1);
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
            crate::GraphSetExecutionError::Source("right failed")
        )))
    ));
}

#[test]
fn observed_right_columns_and_explicit_input_sort_remain_value_barriers() {
    let relation = factor(&[Some(2), Some(-1)])
        .cross_join(factor(&[Some(3); 4]))
        .unwrap();
    assert!(!relation.has_repeated_factor(&[1]));
    let ordered = relation
        .with_order_by(&[crate::algebra::GraphValueOrder::descending(0)])
        .unwrap();
    assert!(!ordered.has_repeated_factor(&[0]));
}

mod pipeline;
