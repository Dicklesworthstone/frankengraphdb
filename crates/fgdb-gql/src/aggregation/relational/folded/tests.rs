use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, GraphValueOrder, IntegerComparison};
use crate::{
    GraphAggregateColumn, GraphAggregateFilter, GraphAggregateOrder, GraphAggregateTest,
    GraphSetProjection, GraphSetQuantifier, GraphSetValue,
};

type Execution = Result<GqlQueryExecution<GraphAggregateRow>, Failure<&'static str, usize>>;

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn value(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn list(values: &[Option<i64>]) -> GraphSetValue {
    GraphSetValue::List(
        values
            .iter()
            .map(|v| GraphSetValue::Value(value(*v)))
            .collect(),
    )
}
fn leaf() -> PreparedGraphSet {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    PreparedGraphSet::from(
        builder
            .prepare_values(
                &[GraphColumn::property("p", "n", PropertyKeyId(1))],
                0,
                None,
            )
            .unwrap()
            .with_duplicates(),
    )
}
fn source(
    values: &[Option<i64>],
) -> impl FnMut(
    &PreparedGraphPattern<GraphValueRow>,
    GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>>
+ '_ {
    move |_, _| {
        Ok(GqlQueryExecution {
            value: values
                .iter()
                .map(|v| GraphValueRow::from_owned_values(vec![value(*v)]))
                .collect(),
            rows: GqlExecutionStats {
                snapshot_records: values.len() as u64,
                result_rows: values.len() as u64,
            },
            evaluator: GlaExecutionStats::default(),
        })
    }
}
fn execute(
    query: &PreparedGraphAggregate,
    values: &[Option<i64>],
    policy: GqlQueryPolicy,
) -> Execution {
    query.execute_relational_with_source(policy, source(values), || Ok(()))
}
fn materialized(query: &PreparedGraphAggregate, values: &[Option<i64>]) -> Execution {
    query.execute_relational_materialized(wide(), source(values), || Ok(()))
}
fn definition(
    values: &[Option<i64>],
    grouped: bool,
    skip: u64,
    count: Option<u64>,
) -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare_relation(
        leaf().unwind("x".into(), list(values)).unwrap(),
        if grouped { &[0] } else { &[] },
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("present", 1),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::average_int("average", 1),
            GraphAggregate::count_distinct("distinct", 1),
            GraphAggregate::sum_int_distinct("distinct_sum", 1),
            GraphAggregate::average_int_distinct("distinct_average", 1),
            GraphAggregate::min("minimum_source", 0),
            GraphAggregate::max("maximum_source", 0),
        ],
        skip,
        count,
    )
    .unwrap()
}

#[test]
fn numeric_folds_match_materialized_bags_nulls_distinct_and_completed_output_clauses() {
    let input = [None, Some(-2), Some(3), Some(3)];
    for values in [
        vec![],
        vec![None],
        vec![Some(2), None, Some(-3), Some(2)],
        vec![Some(i64::MAX); 3],
    ] {
        for grouped in [false, true] {
            for (skip, count) in [(0, None), (0, Some(0)), (1, Some(1)), (u64::MAX, Some(2))] {
                let raw = definition(&values, grouped, skip, count);
                let filtered = raw
                    .clone()
                    .with_result_clauses(
                        &[GraphAggregateFilter {
                            column: GraphAggregateColumn::Aggregate(0),
                            test: GraphAggregateTest::Integer {
                                comparison: IntegerComparison::Greater,
                                value: 1,
                            },
                        }],
                        &[GraphAggregateOrder::descending(
                            GraphAggregateColumn::Aggregate(3),
                        )],
                    )
                    .unwrap();
                let projected = filtered
                    .clone()
                    .with_output_projection(vec![
                        GraphSetProjection::new(
                            "exact",
                            GraphSetValue::Column(usize::from(grouped) + 3),
                        ),
                        GraphSetProjection::new(
                            "same",
                            GraphSetValue::Column(usize::from(grouped) + 3),
                        ),
                    ])
                    .unwrap()
                    .with_distinct_output(true);
                for query in [raw, filtered, projected] {
                    let before = query.canonical_bytes();
                    assert!(query.folded_definition().is_some());
                    let actual = execute(&query, &input, wide()).unwrap();
                    assert_eq!(actual.value, materialized(&query, &input).unwrap().value);
                    assert_eq!(actual.rows.snapshot_records, input.len() as u64);
                    assert_eq!(actual.rows.result_rows, actual.value.len() as u64);
                    assert_eq!(query.canonical_bytes(), before);
                }
            }
        }
    }
    // Independent arithmetic, not a comparison to the other engine alone.
    let query = definition(&[Some(2), None, Some(-3), Some(2)], false, 0, None);
    let rows = execute(&query, &input, wide()).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values()[0].as_count(), Some(16));
    assert_eq!(rows[0].values()[1].as_count(), Some(12));
    assert_eq!(rows[0].values()[2].as_integer(), Some(4));
    assert_eq!(
        rows[0].values()[3].as_average(),
        crate::GraphExactAverage::new(1, 3)
    );
    assert_eq!(rows[0].values()[4].as_count(), Some(2));
    assert_eq!(rows[0].values()[5].as_integer(), Some(-1));
    assert_eq!(
        rows[0].values()[6].as_average(),
        crate::GraphExactAverage::new(-1, 2)
    );
}

#[test]
fn nested_expansion_and_local_pages_do_not_become_client_rows() {
    let values: Vec<_> = (0..24).map(Some).collect();
    let relation = leaf()
        .unwind("a".into(), list(&values))
        .unwrap()
        .unwind("b".into(), list(&values))
        .unwrap()
        .unwind("c".into(), list(&values))
        .unwrap();
    let query = PreparedGraphAggregate::prepare_relation(
        relation,
        &[],
        &[
            GraphAggregate::count_rows("n"),
            GraphAggregate::sum_int("total", 3),
        ],
        0,
        None,
    )
    .unwrap();
    let result = execute(
        &query,
        &[Some(1)],
        GqlQueryPolicy::new(1, 1, u64::MAX, u64::MAX),
    )
    .unwrap();
    assert_eq!(result.rows.result_rows, 1);
    assert_eq!(result.value[0].values()[0].as_count(), Some(24 * 24 * 24));
    assert_eq!(
        result.value[0].values()[1].as_integer(),
        Some(24 * 24 * (23 * 24 / 2))
    );

    let relation = leaf()
        .unwind("x".into(), list(&[Some(5), Some(7), Some(9)]))
        .unwrap()
        .with_page(1, Some(3))
        .nested()
        .unwrap()
        .with_page(1, Some(1));
    let query = PreparedGraphAggregate::prepare_relation(
        relation,
        &[],
        &[
            GraphAggregate::count_rows("n"),
            GraphAggregate::sum_int("total", 1),
        ],
        0,
        None,
    )
    .unwrap();
    let result = execute(&query, &[Some(0), Some(1)], wide()).unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(1));
    assert_eq!(result.value[0].values()[1].as_integer(), Some(9));
    assert_eq!(
        result.value,
        materialized(&query, &[Some(0), Some(1)]).unwrap().value
    );
}

#[test]
fn downstream_aggregate_failure_cannot_mask_later_relational_failure_even_at_limit_zero() {
    let payload = GraphSetValue::List(vec![
        GraphSetValue::List(vec![GraphSetValue::Value(GraphValue::Scalar(
            CanonicalScalar::Bool(true),
        ))]),
        GraphSetValue::Value(value(Some(8))),
    ]);
    for count in [0, 1] {
        let relation = leaf()
            .unwind("xs".into(), payload.clone())
            .unwrap()
            .unwind("x".into(), GraphSetValue::Column(1))
            .unwrap();
        let query = PreparedGraphAggregate::prepare_relation(
            relation,
            &[],
            &[GraphAggregate::sum_int("total", 2)],
            0,
            Some(count),
        )
        .unwrap();
        let expected = materialized(&query, &[Some(0)]).unwrap_err();
        assert!(matches!(
            &expected,
            GqlQueryError::Source(GraphAggregateError::InputRelation(
                crate::GraphSetExecutionError::Projection {
                    row: 1,
                    column: 2,
                    ..
                }
            ))
        ));
        assert_eq!(execute(&query, &[Some(0)], wide()).unwrap_err(), expected);
    }
    let query = PreparedGraphAggregate::prepare_relation(
        leaf()
            .unwind(
                "x".into(),
                GraphSetValue::List(vec![GraphSetValue::Value(GraphValue::Scalar(
                    CanonicalScalar::Bool(true),
                ))]),
            )
            .unwrap(),
        &[],
        &[GraphAggregate::sum_int("total", 1)],
        0,
        Some(0),
    )
    .unwrap();
    assert!(matches!(
        execute(&query, &[Some(0)], wide()),
        Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
            aggregate: 0
        }))
    ));
}

#[test]
fn exact_and_one_less_allowances_and_every_checkpoint_are_atomic() {
    let input = [Some(1), Some(2)];
    let query = definition(&[Some(2), None, Some(-1), Some(2)], true, 0, Some(1))
        .with_result_clauses(
            &[],
            &[GraphAggregateOrder::descending(
                GraphAggregateColumn::Aggregate(2),
            )],
        )
        .unwrap();
    let mut calls = 0;
    let baseline = query
        .execute_relational_with_source(wide(), source(&input), || {
            calls += 1;
            Ok(())
        })
        .unwrap();
    let used = baseline.evaluator;
    let exact = GqlQueryPolicy::new(2, 1, used.work_units, used.scratch_entries);
    assert_eq!(
        execute(&query, &input, exact).unwrap().value,
        baseline.value
    );
    for stop in 1..=calls {
        let mut seen = 0;
        let result = query.execute_relational_with_source(exact, source(&input), || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
    for policy in [
        GqlQueryPolicy::new(1, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(2, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(2, 1, used.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(2, 1, u64::MAX, used.scratch_entries - 1),
    ] {
        assert!(matches!(
            execute(&query, &input, policy),
            Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))
        ));
    }
}

#[test]
fn preflight_retains_value_sensitive_barriers_but_counts_can_elide_terminal_order() {
    let relation = leaf()
        .unwind(
            "x".into(),
            GraphSetValue::List(vec![
                GraphSetValue::Value(value(Some(7))),
                GraphSetValue::Value(GraphValue::Vertex(VId(9))),
                GraphSetValue::List(vec![GraphSetValue::Value(value(Some(2)))]),
            ]),
        )
        .unwrap();
    for aggregate in [
        GraphAggregate::min("v", 1),
        GraphAggregate::max("v", 1),
        GraphAggregate::collect("v", 1),
        GraphAggregate::collect_distinct("v", 1),
    ] {
        let query =
            PreparedGraphAggregate::prepare_relation(relation.clone(), &[], &[aggregate], 0, None)
                .unwrap();
        assert!(query.folded_definition().is_none());
        assert_eq!(
            execute(&query, &[Some(0)], wide()).unwrap().value,
            materialized(&query, &[Some(0)]).unwrap().value
        );
    }
    let relation = relation
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap();
    let query = PreparedGraphAggregate::prepare_relation(
        relation,
        &[],
        &[GraphAggregate::count_rows("n")],
        0,
        None,
    )
    .unwrap();
    assert!(query.uses_factorized_cardinality());
    assert!(query.folded_definition().is_some());
    assert_eq!(
        execute(&query, &[Some(0)], wide()).unwrap().value,
        materialized(&query, &[Some(0)]).unwrap().value
    );
    let simple = leaf()
        .project(
            vec![GraphSetProjection::new("p", GraphSetValue::Column(0))],
            GraphSetQuantifier::All,
        )
        .unwrap();
    assert!(!simple.has_foldable_expansion());
}

#[test]
fn compound_sources_are_not_reexecuted_and_all_are_read_before_an_empty_product() {
    let relation = leaf().cross_join(leaf()).unwrap();
    let query = PreparedGraphAggregate::prepare_set_relation(
        relation,
        &[],
        &[GraphAggregate::count_rows("n")],
        0,
        Some(0),
    )
    .unwrap();
    let mut calls = 0;
    let result = query.execute_relational_with_source(
        wide(),
        |_, _| {
            calls += 1;
            if calls == 2 {
                return Err(GqlQueryError::Source("later graph failed"));
            }
            Ok(GqlQueryExecution {
                value: vec![],
                rows: GqlExecutionStats {
                    snapshot_records: 0,
                    result_rows: 0,
                },
                evaluator: GlaExecutionStats::default(),
            })
        },
        || Ok::<_, usize>(()),
    );
    assert_eq!(calls, 2);
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
            crate::GraphSetExecutionError::Source("later graph failed")
        )))
    ));
}
