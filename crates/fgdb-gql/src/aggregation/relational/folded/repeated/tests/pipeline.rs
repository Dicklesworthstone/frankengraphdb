//! Integrated aggregate regressions for repetition-preserving row stages.
use super::*;

fn constant_list(length: usize) -> GraphSetValue {
    GraphSetValue::List(
        (0..length)
            .map(|at| GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(at as i64))))
            .collect(),
    )
}

fn expand(input: PreparedGraphSet, name: &str, length: usize) -> PreparedGraphSet {
    input.unwind(name.into(), constant_list(length)).unwrap()
}

fn sum_definition(input: PreparedGraphSet, count: Option<u64>) -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare_set_relation(
        input,
        &[],
        &[
            GraphAggregate::sum_int("sum", 0),
            GraphAggregate::count_rows("rows"),
        ],
        0,
        count,
    )
    .unwrap()
}

fn broken_integer() -> GraphSetValue {
    GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(crate::GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    )
}

#[test]
fn trillion_occurrence_unwind_chains_keep_group_values_and_remapped_aliases() {
    let mut input = factor(&[Some(3), Some(3), Some(-1)]);
    for at in 0..8 {
        input = expand(input, &format!("unused_{at}"), 32);
    }
    let input = input
        .project(
            vec![
                GraphSetProjection::new("key", GraphSetValue::Column(0)),
                GraphSetProjection::new("amount", GraphSetValue::Column(0)),
                GraphSetProjection::new(
                    "missing",
                    GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                ),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let q = PreparedGraphAggregate::prepare_set_relation(
        input,
        &[0],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::average_int("average", 1),
            GraphAggregate::count_distinct("distinct", 1),
            GraphAggregate::count("nonnull", 2),
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
    let result = q
        .execute_relational_with_source(
            GqlQueryPolicy::new(0, 2, 100_000, 20_000),
            no_source,
            || Ok(()),
        )
        .unwrap();
    let n = 1_u64 << 40;
    assert_eq!(result.value.len(), 2);
    assert_eq!(result.value[0].values()[0].as_count(), Some(n));
    assert_eq!(
        result.value[0].values()[1],
        crate::GraphAggregateValue::Integer(-i128::from(n))
    );
    assert_eq!(result.value[1].values()[0].as_count(), Some(2 * n));
    assert_eq!(
        result.value[1].values()[1],
        crate::GraphAggregateValue::Integer(6 * i128::from(n))
    );
    assert_eq!(
        result.value[1].values()[2],
        crate::GraphAggregateValue::Average(crate::GraphExactAverage::new(3, 1).unwrap())
    );
    assert_eq!(result.value[1].values()[3].as_count(), Some(1));
    assert_eq!(result.value[1].values()[4].as_count(), Some(0));
    assert_eq!(q.canonical_bytes(), before);
}

#[test]
fn reordered_duplicate_and_literal_columns_keep_original_occurrence_pages() {
    for grouped in [false, true] {
        for length in 0..=4 {
            for (skip, count) in [
                (0, None),
                (1, Some(5)),
                (4, Some(3)),
                (0, Some(0)),
                (u64::MAX, None),
            ] {
                let input = expand(
                    factor(&[None, Some(2), Some(-3), Some(2)]),
                    "discard",
                    length,
                )
                .with_page(skip, count)
                .project(
                    vec![
                        GraphSetProjection::new(
                            "five",
                            GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(5))),
                        ),
                        GraphSetProjection::new("key", GraphSetValue::Column(0)),
                        GraphSetProjection::new("amount", GraphSetValue::Column(0)),
                        GraphSetProjection::new(
                            "null",
                            GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                        ),
                    ],
                    GraphSetQuantifier::All,
                )
                .unwrap()
                .nested()
                .unwrap()
                .with_page(1, Some(7));
                let q = PreparedGraphAggregate::prepare_set_relation(
                    input,
                    if grouped { &[1] } else { &[] },
                    &[
                        GraphAggregate::count_rows("rows"),
                        GraphAggregate::sum_int("fixed", 0),
                        GraphAggregate::sum_int("sum", 2),
                        GraphAggregate::average_int("avg", 2),
                        GraphAggregate::count_distinct("d", 2),
                        GraphAggregate::count("none", 3),
                        GraphAggregate::min("min", 2),
                        GraphAggregate::max("max", 2),
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
                let actual = q
                    .execute_relational_with_source(wide(), no_source, || Ok(()))
                    .unwrap();
                let expected = q
                    .execute_relational_materialized(wide(), no_source, || Ok(()))
                    .unwrap();
                assert_eq!(actual.value, expected.value);
                assert_eq!(actual.rows.result_rows, actual.value.len() as u64);
            }
        }
    }
}

#[test]
fn literal_only_projection_can_drop_all_carrier_columns_without_dropping_rows() {
    let input = expand(factor(&[Some(1), Some(2)]), "discard", 3)
        .project(
            vec![GraphSetProjection::new(
                "amount",
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(-7))),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_page(1, Some(4));
    let q = sum_definition(input, None);
    assert!(q.input_relation().unwrap().has_repeated_factor(&[0]));
    let result = q
        .execute_relational_with_source(wide(), no_source, || Ok(()))
        .unwrap();
    assert_eq!(
        result.value[0].values()[0],
        crate::GraphAggregateValue::Integer(-28)
    );
    assert_eq!(result.value[0].values()[1].as_count(), Some(4));
}

#[test]
fn discarded_constant_list_expressions_fail_unless_their_actual_input_is_empty() {
    for empty in [false, true] {
        for output_limit in [None, Some(0)] {
            let input = factor(if empty { &[] } else { &[Some(1)] })
                .unwind("unused".into(), GraphSetValue::List(vec![broken_integer()]))
                .unwrap();
            let q = sum_definition(input, output_limit);
            let actual = q.execute_relational_with_source(wide(), no_source, || Ok(()));
            let expected = q.execute_relational_materialized(wide(), no_source, || Ok(()));
            assert_eq!(
                actual.as_ref().map(|result| &result.value),
                expected.as_ref().map(|result| &result.value)
            );
            if empty {
                assert!(actual.is_ok());
            } else {
                assert!(actual.is_err());
            }
        }
    }
    // A constant scalar is not a list. Even an input LIMIT 0 on this stage
    // occurs AFTER expression evaluation, so it cannot erase that data error.
    let input = factor(&[Some(1)])
        .unwind(
            "unused".into(),
            GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(7))),
        )
        .unwrap()
        .with_page(0, Some(0));
    let q = sum_definition(input, None);
    assert!(
        q.execute_relational_with_source(wide(), no_source, || Ok(()))
            .is_err()
    );
}

#[test]
fn upstream_failure_precedes_a_discarded_constant_error_and_sources_are_not_retried() {
    let q = sum_definition(
        graph_leaf()
            .unwind("unused".into(), GraphSetValue::List(vec![broken_integer()]))
            .unwrap(),
        Some(0),
    );
    let mut calls = 0;
    let result = q.execute_relational_with_source(
        wide(),
        |_, _| {
            calls += 1;
            Err(GqlQueryError::Source("upstream"))
        },
        || Ok::<_, usize>(()),
    );
    assert_eq!(calls, 1);
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
            crate::GraphSetExecutionError::Source("upstream")
        )))
    ));
}

#[test]
fn observed_appended_values_and_unused_fallible_projections_stay_value_sensitive() {
    let observed = expand(factor(&[Some(1), Some(2)]), "observed", 3);
    assert!(!observed.has_repeated_factor(&[1]));
    let q = PreparedGraphAggregate::prepare_set_relation(
        observed,
        &[],
        &[GraphAggregate::sum_int("sum", 1)],
        0,
        None,
    )
    .unwrap();
    assert_eq!(
        q.execute_relational_with_source(wide(), no_source, || Ok(()))
            .unwrap()
            .value,
        q.execute_relational_materialized(wide(), no_source, || Ok(()))
            .unwrap()
            .value
    );

    let dependent = factor(&[Some(1), Some(2)])
        .unwind(
            "dependent".into(),
            GraphSetValue::List(vec![GraphSetValue::Column(0)]),
        )
        .unwrap();
    assert!(!dependent.has_repeated_factor(&[0]));

    let input = expand(factor(&[Some(1)]), "discard", 3)
        .project(
            vec![
                GraphSetProjection::new("amount", GraphSetValue::Column(0)),
                GraphSetProjection::new("unused_failure", broken_integer()),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
    assert!(!input.has_repeated_factor(&[0]));
    let q = sum_definition(input, Some(0));
    let actual = q.execute_relational_with_source(wide(), no_source, || Ok(()));
    let expected = q.execute_relational_materialized(wide(), no_source, || Ok(()));
    assert!(actual.is_err());
    assert_eq!(
        actual.as_ref().map(|r| &r.value),
        expected.as_ref().map(|r| &r.value)
    );
}

#[test]
fn completed_group_order_hidden_keys_and_output_pages_reuse_the_existing_stage() {
    let input = expand(factor(&[Some(1), Some(1), Some(3), Some(-2)]), "discard", 5)
        .project(
            vec![GraphSetProjection::new("amount", GraphSetValue::Column(0))],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let q = PreparedGraphAggregate::prepare_set_relation(
        input,
        &[0],
        &[
            GraphAggregate::sum_int("sum", 0),
            GraphAggregate::count_rows("count"),
        ],
        1,
        Some(1),
    )
    .unwrap()
    .with_result_clauses(
        &[],
        &[crate::GraphAggregateOrder::descending(
            crate::GraphAggregateColumn::Aggregate(0),
        )],
    )
    .unwrap()
    .with_key_output_columns(&[])
    .unwrap()
    .with_aggregate_output_prefix(1)
    .unwrap();
    let actual = q
        .execute_relational_with_source(wide(), no_source, || Ok(()))
        .unwrap();
    let expected = q
        .execute_relational_materialized(wide(), no_source, || Ok(()))
        .unwrap();
    assert_eq!(actual.value, expected.value);
    assert_eq!(actual.value.len(), 1);
    assert!(actual.value[0].keys().is_empty());
    assert_eq!(
        actual.value[0].values(),
        &[crate::GraphAggregateValue::Integer(10)]
    );
    assert_eq!(actual.rows.result_rows, 1);
}

#[test]
fn every_pipeline_checkpoint_and_cumulative_quota_can_refuse_without_a_partial_result() {
    let q = sum_definition(
        expand(factor(&[Some(-2), Some(3)]), "unused", 7)
            .project(
                vec![GraphSetProjection::new("renamed", GraphSetValue::Column(0))],
                GraphSetQuantifier::All,
            )
            .unwrap()
            .with_page(2, Some(9)),
        None,
    );
    let mut calls = 0;
    let baseline = q
        .execute_relational_with_source(wide(), no_source, || {
            calls += 1;
            Ok(())
        })
        .unwrap();
    let exact = GqlQueryPolicy::new(
        0,
        1,
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
        GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(0, 1, baseline.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(0, 1, u64::MAX, baseline.evaluator.scratch_entries - 1),
    ] {
        assert!(
            q.execute_relational_with_source(policy, no_source, || Ok(()))
                .is_err()
        );
    }
}
