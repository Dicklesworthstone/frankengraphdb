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
        |_: &crate::algebra::PreparedGraphPattern<crate::algebra::GraphValueRow>,
         _: crate::GqlQueryPolicy| {
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

fn correlated_body() -> PreparedGraphSet {
    PreparedGraphSet::arguments(
        vec!["imported".into()],
        vec![crate::GraphSetColumnType::Scalar],
    )
    .unwrap()
    .unwind(
        "member".into(),
        GraphSetValue::Range {
            start: Box::new(GraphSetValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(1),
            ))),
            end: Box::new(GraphSetValue::Column(0)),
            step: None,
        },
    )
    .unwrap()
}

fn assert_occurrence_summary(row: &crate::GraphAggregateRow, expected: &[Option<i64>]) {
    let nonnull: Vec<_> = expected.iter().flatten().copied().collect();
    let distinct: Vec<_> = nonnull
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let values = row.values();
    assert_eq!(values[0].as_count(), Some(expected.len() as u64));
    assert_eq!(values[1].as_count(), Some(nonnull.len() as u64));
    assert_eq!(values[4].as_count(), Some(distinct.len() as u64));
    for (numbers, sum_at, average_at) in [(&nonnull, 2, 3), (&distinct, 5, 6)] {
        if numbers.is_empty() {
            assert!(values[sum_at].is_null());
            assert!(values[average_at].is_null());
        } else {
            let sum = numbers.iter().map(|&n| i128::from(n)).sum();
            assert_eq!(values[sum_at].as_integer(), Some(sum));
            assert_eq!(
                values[average_at].as_average(),
                Some(crate::GraphExactAverage::new(sum, numbers.len() as u64).unwrap())
            );
        }
    }
    for (expected, at) in [(nonnull.iter().min(), 7), (nonnull.iter().max(), 8)] {
        if let Some(&expected) = expected {
            assert_eq!(
                values[at].as_value(),
                Some(&GraphValue::Scalar(CanonicalScalar::Int(expected)))
            );
        } else {
            assert!(values[at].is_null());
        }
    }
}

#[test]
fn correlated_outer_aggregates_match_an_explicit_occurrence_oracle() {
    let outer = [None, Some(2), Some(2), Some(-1)];
    for grouped in [false, true] {
        for optional in [false, true] {
            for body_skip in [0, 1] {
                for body_limit in [None, Some(0), Some(2)] {
                    // Enumerate the small logical bag without an Apply,
                    // cardinality kernel, aggregate updater or query evaluator.
                    let mut occurrences = Vec::new();
                    for value in outer {
                        let members = value.unwrap_or(0).max(0) as u64;
                        let members = members.saturating_sub(body_skip);
                        let members = body_limit.map_or(members, |limit| members.min(limit));
                        let members = if optional { members.max(1) } else { members };
                        for _ in 0..members {
                            occurrences.push(value);
                        }
                    }
                    let body = correlated_body().with_page(body_skip, body_limit);
                    let input = if optional {
                        factor(&outer).optional_apply(body, vec![0])
                    } else {
                        factor(&outer).apply(body, vec![0])
                    }
                    .unwrap();
                    for (skip, limit) in [(0, None), (1, Some(3)), (u64::MAX, Some(0))] {
                        let input = input.clone().with_page(skip, limit);
                        let q = PreparedGraphAggregate::prepare_set_relation(
                            input,
                            if grouped { &[0] } else { &[] },
                            &[
                                GraphAggregate::count_rows("rows"),
                                GraphAggregate::count("nonnull", 0),
                                GraphAggregate::sum_int("sum", 0),
                                GraphAggregate::average_int("avg", 0),
                                GraphAggregate::count_distinct("distinct", 0),
                                GraphAggregate::sum_int_distinct("distinct_sum", 0),
                                GraphAggregate::average_int_distinct("distinct_avg", 0),
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
                        let transcript = q.canonical_bytes();
                        let actual = q
                            .execute_relational_with_source(wide(), no_source, || Ok(()))
                            .unwrap();
                        let materialized = q
                            .execute_relational_materialized(wide(), no_source, || Ok(()))
                            .unwrap();
                        assert_eq!(actual.value, materialized.value);
                        assert_eq!(q.canonical_bytes(), transcript);
                        let start = (u128::from(skip)).min(occurrences.len() as u128) as usize;
                        let remaining = &occurrences[start..];
                        let take = limit.map_or(remaining.len(), |n| {
                            u128::from(n).min(remaining.len() as u128) as usize
                        });
                        let selected = &remaining[..take];
                        if grouped {
                            let keys: std::collections::BTreeSet<_> =
                                selected.iter().copied().collect();
                            assert_eq!(actual.value.len(), keys.len());
                            for row in &actual.value {
                                let key = match row.keys()[0].as_scalar().unwrap() {
                                    CanonicalScalar::Null => None,
                                    CanonicalScalar::Int(n) => Some(*n),
                                    _ => panic!("the oracle has only integer/null keys"),
                                };
                                assert!(keys.contains(&key));
                                let members: Vec<_> = selected
                                    .iter()
                                    .copied()
                                    .filter(|value| *value == key)
                                    .collect();
                                assert_occurrence_summary(row, &members);
                            }
                        } else {
                            assert_eq!(actual.value.len(), 1);
                            assert_occurrence_summary(&actual.value[0], selected);
                        }
                    }
                }
            }
        }
    }
}

fn nested_correlated_input() -> PreparedGraphSet {
    let inner =
        PreparedGraphSet::arguments(vec!["copies".into()], vec![crate::GraphSetColumnType::Any])
            .unwrap()
            .unwind(
                "member".into(),
                GraphSetValue::Range {
                    start: Box::new(GraphSetValue::Value(GraphValue::Scalar(
                        CanonicalScalar::Int(1),
                    ))),
                    end: Box::new(GraphSetValue::Column(0)),
                    step: None,
                },
            )
            .unwrap();
    let body =
        PreparedGraphSet::arguments(vec!["copies".into()], vec![crate::GraphSetColumnType::Any])
            .unwrap()
            .apply(inner, vec![0])
            .unwrap();
    // The aggregate observes x, but the body imports the OTHER outer column.
    // Dropping that column before body invocation would change every fanout.
    expand(factor(&[Some(5), Some(5), None]), "copies", 3)
        .apply(body, vec![1])
        .unwrap()
}

#[test]
fn correlated_fanout_keeps_unobserved_imports_nested_frames_and_projection_aliases() {
    let input = nested_correlated_input()
        .project(
            vec![
                GraphSetProjection::new(
                    "missing",
                    GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                ),
                GraphSetProjection::new("amount", GraphSetValue::Column(0)),
                GraphSetProjection::new("same", GraphSetValue::Column(0)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let q = PreparedGraphAggregate::prepare_set_relation(
        input,
        &[],
        &[
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count_distinct("distinct", 2),
            GraphAggregate::count("absent", 0),
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
    let result = q
        .execute_relational_with_source(wide(), no_source, || Ok(()))
        .unwrap();
    assert_eq!(
        result.value,
        q.execute_relational_materialized(wide(), no_source, || Ok(()))
            .unwrap()
            .value
    );
    // Each of the three outer x occurrences has body sizes 0, 1 and 2.
    assert_eq!(result.value[0].values()[0].as_integer(), Some(30));
    assert_eq!(result.value[0].values()[1].as_count(), Some(9));
    assert_eq!(result.value[0].values()[2].as_count(), Some(1));
    assert_eq!(result.value[0].values()[3].as_count(), Some(0));
}

#[test]
fn correlated_body_failures_precede_aggregate_failures_and_empty_pages() {
    let input = factor(&[Some(1), Some(1), Some(1)])
        .project(
            vec![GraphSetProjection::new(
                "bad_numeric",
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(true))),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let body = PreparedGraphSet::arguments(
        vec!["imported".into()],
        vec![crate::GraphSetColumnType::Scalar],
    )
    .unwrap()
    .cross_join(graph_leaf())
    .unwrap();
    for input_limit in [None, Some(0)] {
        for output_limit in [None, Some(0)] {
            let q = sum_definition(
                input
                    .clone()
                    .apply(body.clone(), vec![0])
                    .unwrap()
                    .with_page(0, input_limit),
                output_limit,
            );
            for materialized in [false, true] {
                let mut calls = 0;
                let source = |pattern: &PreparedGraphPattern<GraphValueRow>, remaining| {
                    calls += 1;
                    if calls == 3 {
                        return Err(GqlQueryError::Source("later body wins"));
                    }
                    pattern.plan().execute_governed_with_properties(
                        1,
                        [VId(1)],
                        [],
                        |_, _| Ok::<_, &'static str>(true),
                        |_, _| Ok(None),
                        remaining,
                        || Ok::<_, usize>(()),
                    )
                };
                let result = if materialized {
                    q.execute_relational_materialized(wide(), source, || Ok(()))
                } else {
                    q.execute_relational_with_source(wide(), source, || Ok(()))
                };
                assert_eq!(calls, 3);
                assert!(matches!(
                    result,
                    Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
                        crate::GraphSetExecutionError::Source("later body wins")
                    )))
                ));
            }
        }
    }
    // Unused body expressions still execute before its local empty page.
    let body = correlated_body()
        .project(
            vec![GraphSetProjection::new("unused_bad", broken_integer())],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_page(0, Some(0));
    let q = sum_definition(factor(&[Some(1)]).apply(body, vec![0]).unwrap(), Some(0));
    let actual = q.execute_relational_with_source(wide(), no_source, || Ok(()));
    let expected = q.execute_relational_materialized(wide(), no_source, || Ok(()));
    assert!(actual.is_err());
    assert_eq!(
        actual.as_ref().map(|r| &r.value),
        expected.as_ref().map(|r| &r.value)
    );
}

#[test]
fn correlated_source_visits_and_snapshot_limits_accumulate_per_outer_occurrence() {
    let body = PreparedGraphSet::arguments(
        vec!["imported".into()],
        vec![crate::GraphSetColumnType::Scalar],
    )
    .unwrap()
    .cross_join(graph_leaf())
    .unwrap();
    let q = sum_definition(
        factor(&[Some(1), Some(1), Some(1)])
            .apply(body, vec![0])
            .unwrap(),
        None,
    );
    for records in [5, 6] {
        let mut calls = 0;
        let result = q.execute_relational_with_source(
            GqlQueryPolicy::new(records, 1, 100_000, 10_000),
            |pattern: &PreparedGraphPattern<GraphValueRow>, remaining| {
                calls += 1;
                pattern.plan().execute_governed_with_properties(
                    2,
                    [VId(1), VId(2)],
                    [],
                    |_, _| Ok::<_, &'static str>(true),
                    |_, _| Ok(None),
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok(()),
        );
        assert_eq!(calls, 3);
        if records == 6 {
            let result = result.unwrap();
            assert_eq!(result.rows.snapshot_records, 6);
            assert_eq!(result.value[0].values()[0].as_integer(), Some(6));
            assert_eq!(result.value[0].values()[1].as_count(), Some(6));
        } else {
            assert!(matches!(result, Err(GqlQueryError::Rows(_))));
        }
    }
}

#[test]
fn body_dependent_ordered_and_collecting_aggregates_keep_ordinary_execution() {
    let input = factor(&[Some(2), Some(2), None])
        .apply(correlated_body(), vec![0])
        .unwrap();
    let observed_body = PreparedGraphAggregate::prepare_set_relation(
        input.clone(),
        &[],
        &[GraphAggregate::sum_int("body_sum", 2)],
        0,
        None,
    )
    .unwrap();
    assert!(!input.has_repeated_factor(&[2]));
    let ordered = input
        .clone()
        .with_order_by(&[crate::algebra::GraphValueOrder::descending(2)])
        .unwrap()
        .with_page(1, Some(2));
    assert!(!ordered.has_repeated_factor(&[0]));
    let collected = PreparedGraphAggregate::prepare_set_relation(
        input,
        &[],
        &[GraphAggregate::collect("values", 0)],
        0,
        None,
    )
    .unwrap();
    assert!(collected.folded_definition().is_none());
    for q in [observed_body, sum_definition(ordered, None), collected] {
        assert_eq!(
            q.execute_relational_with_source(wide(), no_source, || Ok(()))
                .unwrap()
                .value,
            q.execute_relational_materialized(wide(), no_source, || Ok(()))
                .unwrap()
                .value
        );
    }
}

#[test]
fn correlated_repetition_pages_recover_large_counts_and_preserve_overflow() {
    let body = PreparedGraphSet::arguments(
        vec!["imported".into()],
        vec![crate::GraphSetColumnType::Scalar],
    )
    .unwrap()
    .unwind(
        "all_ints".into(),
        GraphSetValue::Range {
            start: Box::new(GraphSetValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(i64::MIN),
            ))),
            end: Box::new(GraphSetValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(i64::MAX),
            ))),
            step: None,
        },
    )
    .unwrap();
    let input = factor(&[Some(1), Some(1)]).apply(body, vec![0]).unwrap();
    let paged = input.clone().with_page(u64::MAX, Some(3));
    let result = sum_definition(paged, None)
        .execute_relational_with_source(GqlQueryPolicy::new(0, 1, 20_000, 5_000), no_source, || {
            Ok(())
        })
        .unwrap();
    // Skip leaves one occurrence in the first 2^64 run, then two in the next.
    assert_eq!(result.value[0].values()[0].as_integer(), Some(3));
    assert_eq!(result.value[0].values()[1].as_count(), Some(3));
    for limit in [None, Some(0)] {
        let error = sum_definition(input.clone(), limit)
            .execute_relational_with_source(wide(), no_source, || Ok(()))
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate: 1 })
        ));
    }
}

#[test]
fn every_correlated_aggregate_checkpoint_and_resource_boundary_is_fail_stop() {
    let q = sum_definition(nested_correlated_input(), None);
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
