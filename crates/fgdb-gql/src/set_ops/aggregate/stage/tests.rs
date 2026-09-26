use super::*;
use crate::algebra::{GraphValue, IntegerComparison};
use crate::{GraphIntegerErrorKind, GraphSetOperand, GraphSetPredicateOp, GraphSetQuantifier};
use core::convert::Infallible;
use fgdb_types::{CanonicalScalar, VId};

fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, rows, 1_000_000, 1_000_000)
}
fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn input(values: &[Option<i64>]) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "x".into(),
            GraphSetValue::List(
                values
                    .iter()
                    .map(|value| {
                        GraphSetValue::Value(value.map_or_else(
                            || GraphValue::Scalar(CanonicalScalar::Null),
                            scalar,
                        ))
                    })
                    .collect(),
            ),
        )
        .unwrap()
}
fn source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<Infallible, Infallible>> {
    panic!("values-only grouping must not invent a graph source")
}
fn execute(query: &PreparedGraphSet, rows: u64) -> GqlQueryExecution<GraphValueRow> {
    query.execute_governed(policy(rows), source, || Ok(())).unwrap()
}

#[test]
fn grouped_counts_feed_filter_order_page_and_projection() {
    let grouped = input(&[Some(1), Some(2), Some(2), Some(3), Some(3), Some(3), None])
        .group_by(&[0], &[GraphAggregate::count_rows("n")])
        .unwrap();
    assert_eq!(grouped.columns(), &["x", "n"]);
    assert_eq!(
        grouped.column_types(),
        &[GraphSetColumnType::Any, GraphSetColumnType::Scalar]
    );
    let result = grouped
        .project(
            vec![
                GraphSetProjection::new("x", GraphSetValue::Column(0)),
                GraphSetProjection::new("n", GraphSetValue::Column(1)),
                GraphSetProjection::new("threshold", GraphSetValue::Value(scalar(1))),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .filter(&[GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(1),
            comparison: IntegerComparison::Greater,
            right: GraphSetOperand::Column(2),
        }])
        .unwrap()
        .with_order_by(&[GraphValueOrder::descending(1)])
        .unwrap()
        .with_page(0, Some(1))
        .project(
            vec![
                GraphSetProjection::new("key", GraphSetValue::Column(0)),
                GraphSetProjection::new("count", GraphSetValue::Column(1)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let result = execute(&result, 1);
    assert_eq!(result.rows.result_rows, 1);
    assert_eq!(result.value[0].values(), &[scalar(3), scalar(3)]);
}

#[test]
fn collect_then_unwind_then_group_reuses_native_bag_and_null_laws() {
    let grouped = input(&[Some(3), None, Some(1), Some(3)])
        .group_by(&[], &[GraphAggregate::collect("items", 0)])
        .unwrap();
    assert_eq!(grouped.column_types(), &[GraphSetColumnType::List]);
    let expanded = grouped
        .unwind("item".into(), GraphSetValue::Column(0))
        .unwrap()
        .project(
            vec![GraphSetProjection::new("item", GraphSetValue::Column(1))],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let rows = execute(&expanded, 3);
    assert_eq!(
        rows.value.iter().map(|row| row.values()[0].clone()).collect::<Vec<_>>(),
        vec![scalar(3), scalar(1), scalar(3)]
    );
    let regrouped = expanded.group_by(&[0], &[GraphAggregate::count_rows("n")]).unwrap();
    let rows = execute(&regrouped, 2);
    assert_eq!(rows.value[0].values(), &[scalar(1), scalar(1)]);
    assert_eq!(rows.value[1].values(), &[scalar(3), scalar(2)]);
}

#[test]
fn input_pages_and_distinct_are_not_distributed_across_grouping() {
    let query = input(&[Some(3), Some(3), Some(1), Some(3)])
        .with_page(1, Some(2))
        .group_by(
            &[],
            &[GraphAggregate::count_rows("n"), GraphAggregate::sum_int("sum", 0)],
        )
        .unwrap();
    assert_eq!(execute(&query, 1).value[0].values(), &[scalar(2), scalar(4)]);
    let distinct = input(&[Some(3), Some(3), Some(1), Some(3)])
        .project(
            vec![GraphSetProjection::new("x", GraphSetValue::Column(0))],
            GraphSetQuantifier::Distinct,
        )
        .unwrap()
        .group_by(&[], &[GraphAggregate::count_rows("n")])
        .unwrap();
    assert_eq!(execute(&distinct, 1).value[0].values(), &[scalar(2)]);
}

#[test]
fn empty_global_groups_grouped_empty_inputs_and_null_keys_stay_distinct() {
    let empty = input(&[])
        .group_by(
            &[],
            &[
                GraphAggregate::count_rows("n"),
                GraphAggregate::min("min", 0),
                GraphAggregate::collect("items", 0),
            ],
        )
        .unwrap();
    assert_eq!(
        execute(&empty, 1).value[0].values(),
        &[
            scalar(0),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::List(Vec::new().into_boxed_slice()),
        ]
    );
    let grouped_empty = input(&[]).group_by(&[0], &[GraphAggregate::count_rows("n")]).unwrap();
    assert!(execute(&grouped_empty, 0).value.is_empty());
    let nulls = input(&[None, None])
        .group_by(&[0], &[GraphAggregate::count_rows("n"), GraphAggregate::count("c", 0)])
        .unwrap();
    assert_eq!(
        execute(&nulls, 1).value[0].values(),
        &[GraphValue::Scalar(CanonicalScalar::Null), scalar(2), scalar(0)]
    );
}

#[test]
fn downstream_empty_pages_and_false_filters_cannot_hide_numeric_refusal() {
    let overflowing = input(&[Some(i64::MAX), Some(1)])
        .group_by(&[], &[GraphAggregate::sum_int("sum", 0)])
        .unwrap();
    for query in [
        overflowing.clone().with_page(0, Some(0)),
        overflowing.clone().filter(&[GraphSetPredicateOp::Truth(Some(false))]).unwrap(),
        overflowing.group_by(&[], &[GraphAggregate::count_rows("n")]).unwrap(),
    ] {
        let error = query.execute_governed(policy(0), source, || Ok(())).unwrap_err();
        match error {
            GqlQueryError::Source(GraphSetExecutionError::Aggregate(error)) => {
                assert!(matches!(*error, GraphAggregateError::OutputExpression {
                    error: crate::GraphIntegerError { kind: GraphIntegerErrorKind::Overflow, .. },
                    ..
                }));
            }
            other => panic!("expected checked aggregate overflow, got {other:?}"),
        }
    }
    let invalid = PreparedGraphSet::singleton()
        .unwind("x".into(), GraphSetValue::List(vec![
            GraphSetValue::Value(GraphValue::Vertex(VId(1))),
        ]))
        .unwrap()
        .group_by(&[], &[GraphAggregate::sum_int("sum", 0)])
        .unwrap()
        .with_page(0, Some(0));
    match invalid.execute_governed(policy(0), source, || Ok(())).unwrap_err() {
        GqlQueryError::Source(GraphSetExecutionError::Aggregate(error)) => {
            assert!(matches!(*error, GraphAggregateError::NonIntegerSum { aggregate: 0 }));
        }
        other => panic!("expected native sum refusal, got {other:?}"),
    }
}

#[test]
fn private_group_rows_do_not_consume_the_final_result_row_allowance() {
    let groups = input(&[Some(1), Some(2), Some(3), Some(4), Some(5)])
        .group_by(&[0], &[GraphAggregate::count_rows("n")])
        .unwrap();
    assert!(matches!(
        groups.execute_governed(policy(1), source, || Ok(())),
        Err(GqlQueryError::Rows(_))
    ));
    let summary = groups.group_by(&[], &[GraphAggregate::count_rows("groups")]).unwrap();
    let result = execute(&summary, 1);
    assert_eq!(result.rows.result_rows, 1);
    assert_eq!(result.value[0].values(), &[scalar(5)]);
}

#[test]
fn every_aggregate_stage_shares_the_same_work_and_scratch_meter() {
    let query = input(&[Some(1), Some(2), Some(2)])
        .group_by(&[0], &[GraphAggregate::count_rows("n")])
        .unwrap()
        .group_by(&[], &[GraphAggregate::sum_int("total", 1)])
        .unwrap();
    let baseline = execute(&query, 1);
    for limited in [
        GqlQueryPolicy::new(0, 1, baseline.evaluator.work_units - 1, 1_000_000),
        GqlQueryPolicy::new(0, 1, 1_000_000, baseline.evaluator.scratch_entries - 1),
    ] {
        assert!(matches!(
            query.execute_governed(limited, source, || Ok(())),
            Err(GqlQueryError::Evaluator(_))
        ));
    }
    let exact = GqlQueryPolicy::new(
        0, 1, baseline.evaluator.work_units, baseline.evaluator.scratch_entries,
    );
    assert_eq!(
        query.execute_governed(exact, source, || Ok(())).unwrap().value,
        baseline.value
    );
}

fn graph_input() -> PreparedGraphSet {
    let mut builder = crate::algebra::GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .prepare_values(&[crate::algebra::GraphColumn::vertex("n", "n")], 0, None)
        .unwrap()
        .with_duplicates()
        .into()
}

#[test]
fn nested_grouping_visits_each_real_source_once_even_beneath_limit_zero() {
    let query = graph_input()
        .combine(GraphSetOperation::Union, GraphSetQuantifier::All, graph_input())
        .unwrap()
        .group_by(&[], &[GraphAggregate::count_rows("matches")])
        .unwrap()
        .group_by(&[], &[GraphAggregate::sum_int("total", 0)])
        .unwrap();
    assert_eq!(query.operand_count(), 2);
    assert!(query.single_pattern_input().is_none());
    assert!(query.first_pattern_input().is_some());
    for count in [None, Some(0)] {
        let mut calls = 0;
        let result = query.clone().with_page(0, count).execute_governed(
            GqlQueryPolicy::new(4, 1, 1_000_000, 1_000_000),
            |pattern, remaining| {
                calls += 1;
                pattern.plan().execute_governed_with_properties(
                    2,
                    (1..=2).map(VId),
                    [],
                    |_, _| Ok::<_, Infallible>(true),
                    |_, _| Ok(None),
                    remaining,
                    || Ok::<_, Infallible>(()),
                )
            },
            || Ok::<_, Infallible>(()),
        ).unwrap();
        assert_eq!(calls, 2);
        assert_eq!(result.rows.snapshot_records, 4);
        if count == Some(0) {
            assert!(result.value.is_empty());
        } else {
            assert_eq!(result.value[0].values(), &[scalar(4)]);
        }
    }
}

#[test]
fn aggregate_definitions_share_depth_bounds_and_never_masquerade_as_leaves() {
    let group = graph_input().group_by(&[], &[GraphAggregate::count_rows("n")]).unwrap();
    assert_eq!(group.operand_count(), 1);
    assert!(group.single_pattern_input().is_some());
    assert!(group.incremental_pattern().is_none());
    assert!(group.incremental_scope().is_none());
    assert!(group.incremental_binary().is_none());
    assert!(group.incremental_projection().is_none());
    assert!(group.incremental_filter().is_none());
    assert!(!group.has_foldable_expansion());
    assert!(!group.has_factorized_cardinality());
    assert!(!group.has_repeated_factor(&[]));
    assert!(group.with_page(0, Some(0)).incremental_window().unwrap().is_none());
    let mut deepest = PreparedGraphSet::singleton();
    for _ in 1..MAX_GRAPH_SET_DEPTH {
        deepest = deepest.group_by(&[], &[GraphAggregate::count_rows("n")]).unwrap();
    }
    assert!(matches!(
        deepest.group_by(&[], &[GraphAggregate::count_rows("n")]),
        Err(GraphAggregateBuildError::RelationalInput(GraphSetBuildError::TooDeep { .. }))
    ));
}

#[test]
fn aggregate_transcripts_bind_function_distinct_and_child_selection() {
    let relation = input(&[Some(1), Some(1), Some(2)]);
    let count = relation.clone().group_by(&[], &[GraphAggregate::count("n", 0)]).unwrap();
    let distinct = relation.clone().group_by(&[], &[GraphAggregate::count_distinct("n", 0)]).unwrap();
    let page = relation.with_page(0, Some(1)).group_by(&[], &[GraphAggregate::count("n", 0)]).unwrap();
    assert_ne!(count.canonical_bytes(), distinct.canonical_bytes());
    assert_ne!(count.canonical_bytes(), page.canonical_bytes());
    assert_eq!(count.canonical_bytes()[b"fgdb:bounded-set:v1\0".len()], 9);
    let mut singleton = b"fgdb:bounded-set:v1\0".to_vec();
    singleton.push(5);
    singleton.extend_from_slice(&0_u64.to_be_bytes());
    singleton.extend_from_slice(&0_u64.to_be_bytes());
    singleton.push(0);
    assert_eq!(PreparedGraphSet::singleton().canonical_bytes(), singleton);
}

#[test]
fn nested_aggregate_errors_translate_only_the_original_host_error() {
    let error = GraphSetExecutionError::Aggregate(Box::new(
        GraphAggregateError::InputRelation(GraphSetExecutionError::Source("inner")),
    ));
    match error.map_source(str::len) {
        GraphSetExecutionError::Aggregate(error) => {
            assert_eq!(
                *error,
                GraphAggregateError::InputRelation(GraphSetExecutionError::Source(5))
            );
        }
        other => panic!("lost native aggregate cause: {other:?}"),
    }
}
