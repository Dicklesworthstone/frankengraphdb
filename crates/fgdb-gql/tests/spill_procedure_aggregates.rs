//! The explicit procedure compiler keeps the existing terminal value domains.
//! This materialized semantic host exercises native stages and reducer cells;
//! authenticated file replay and byte admission are covered by fgdb integration.
use core::convert::Infallible;
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, GraphValueRow};
use fgdb_gql::spill_aggregate::{AsyncSpillAggregatePlan, SpillAggregateDefinition};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregate, GraphAggregateError, GraphAggregateRow, GraphAggregateValue, GraphExactAverage,
    PreparedGraphSet, PreparedGraphSetAggregate,
};
use fgdb_types::{CanonicalF64, CanonicalScalar};
use std::cmp::Ordering;
use std::collections::BTreeMap;

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn row(group: i64, value: GraphValue) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![int(group), value])
}
fn procedure() -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .procedure_call(
            "test".into(),
            "rows".into(),
            Vec::new(),
            Vec::new(),
            vec![("group".into(), "g".into()), ("value".into(), "x".into())],
            Vec::new(),
        )
        .unwrap()
}

// Independent whole-row tiebreaks and explicit NULL placement, as required by
// the compiled input barriers. All expressions still use the real stage VM.
fn compare(a: &GraphValueRow, b: &GraphValueRow, order: &[GraphValueOrder]) -> Ordering {
    for key in order {
        let (left, right) = (&a.values()[key.column], &b.values()[key.column]);
        let ordering = match (left.is_null(), right.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                if key.descending {
                    right.compare_orderability(left)
                } else {
                    left.compare_orderability(right)
                }
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    a.cmp(b)
}

fn reduce(
    definition: &SpillAggregateDefinition,
    rows: Vec<GraphValueRow>,
) -> Vec<GraphAggregateRow> {
    assert!(!definition.has_output_stage());
    assert_eq!(definition.result_window(), (0, None));
    let mut control = |_| Ok::<_, GqlQueryError<GraphAggregateError<Infallible>, ()>>(());
    // Complete input validation precedes all groups, including an empty one.
    for row in &rows {
        definition.validate_input(row, &mut control).unwrap();
    }
    let mut groups = BTreeMap::<Vec<GraphValue>, Vec<GraphValueRow>>::new();
    if rows.is_empty() && definition.group_key_columns().is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    for row in rows {
        let key = definition
            .group_key_columns()
            .iter()
            .map(|&column| row.values()[column].clone())
            .collect();
        groups.entry(key).or_default().push(row);
    }
    groups
        .into_iter()
        .map(|(key, rows)| {
            let mut state = definition.new_state(&mut control).unwrap();
            for row in &rows {
                definition.update(&mut state, row, &mut control).unwrap();
            }
            for column in definition.distinct_argument_columns() {
                let mut support = BTreeMap::new();
                for row in &rows {
                    support.entry(&row.values()[column]).or_insert(row);
                }
                for row in support.into_values() {
                    definition
                        .update_distinct_argument(&mut state, row, column, &mut control)
                        .unwrap();
                }
            }
            definition.finish(key, state, &mut control).unwrap()
        })
        .collect()
}

fn external(
    query: &PreparedGraphSetAggregate,
    source_rows: &[GraphValueRow],
) -> Vec<GraphAggregateRow> {
    assert!(AsyncSpillAggregatePlan::compile_relation(query).is_err());
    let AsyncSpillAggregatePlan::Relation { input, definition } =
        AsyncSpillAggregatePlan::compile_relation_with_single_procedure(query).unwrap()
    else {
        panic!("a procedure must remain in the complete relational input");
    };
    assert_eq!(input.source_count(), 1);
    let source = input.procedure_source().unwrap();
    assert_eq!(source.columns(), ["g", "x"]);
    assert!(
        source
            .evaluate_arguments(&mut |_| Ok::<_, ()>(()))
            .unwrap()
            .is_empty()
    );
    let mut rows = source_rows.to_vec();
    for stage in input.stages() {
        let mut output = Vec::new();
        for (index, row) in rows.into_iter().enumerate() {
            let mut cursor = stage.start(row, index, &mut |_| Ok::<_, ()>(())).unwrap();
            while let Some((row, next)) =
                cursor.next_with_control(&mut |_| Ok::<_, ()>(())).unwrap()
            {
                output.push(row);
                cursor = next;
            }
        }
        if let Some(order) = stage.order() {
            output.sort_by(|a, b| compare(a, b, order));
        }
        if stage.distinct() {
            output.dedup();
        }
        rows = output
            .into_iter()
            .skip(usize::try_from(stage.offset()).unwrap())
            .take(
                stage
                    .count()
                    .map(|count| usize::try_from(count).unwrap())
                    .unwrap_or(usize::MAX),
            )
            .collect();
    }
    reduce(&definition, rows)
}

fn native(query: &PreparedGraphSetAggregate, rows: &[GraphValueRow]) -> Vec<GraphAggregateRow> {
    let mut calls = 0;
    let result = query
        .execute_governed_with_procedures(
            GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
            |_, _| -> Result<_, GqlQueryError<Infallible, ()>> {
                panic!("the semantic fixture has no graph source")
            },
            |call, arguments, _| {
                calls += 1;
                assert_eq!((call.namespace(), call.name()), ("test", "rows"));
                assert_eq!(call.outputs(), ["group", "value"]);
                assert!(arguments.is_empty());
                Ok(GqlQueryExecution {
                    value: rows.to_vec(),
                    rows: GqlExecutionStats {
                        snapshot_records: 0,
                        result_rows: rows.len() as u64,
                    },
                    evaluator: GlaExecutionStats::default(),
                })
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(calls, 1);
    result.value
}

#[test]
fn procedure_summaries_keep_wide_integer_rational_and_binary64_domains() {
    let input = procedure()
        .with_order_by(&[GraphValueOrder::descending(1)])
        .unwrap();
    let query = PreparedGraphSetAggregate::prepare(
        input,
        &[],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 1),
            GraphAggregate::count_distinct("support", 1),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::sum_int_distinct("distinct_sum", 1),
            GraphAggregate::average_int("mean", 1),
            GraphAggregate::average_int_distinct("distinct_mean", 1),
            GraphAggregate::min("min", 1),
            GraphAggregate::max("max", 1),
            GraphAggregate::collect("all", 1),
            GraphAggregate::collect_distinct("distinct", 1),
        ],
        0,
        None,
    )
    .unwrap();
    let rows = vec![
        row(1, int(2)),
        row(1, int(i64::MAX)),
        row(1, null()),
        row(1, int(i64::MAX)),
    ];
    use GraphAggregateValue::{Average, Count, Integer, Value};
    let total = 2 * i128::from(i64::MAX) + 2;
    let distinct_total = i128::from(i64::MAX) + 2;
    let expected = vec![GraphAggregateRow::from_group_values(
        Vec::new(),
        vec![
            Count(4),
            Count(3),
            Count(2),
            Integer(total),
            Integer(distinct_total),
            Average(GraphExactAverage::new(total, 3).unwrap()),
            Average(GraphExactAverage::new(distinct_total, 2).unwrap()),
            Value(int(2)),
            Value(int(i64::MAX)),
            Value(GraphValue::List(
                vec![int(i64::MAX), int(i64::MAX), int(2)].into(),
            )),
            Value(GraphValue::List(vec![int(i64::MAX), int(2)].into())),
        ],
    )];
    let transcript = query.canonical_bytes();
    assert_eq!(external(&query, &rows), expected);
    assert_eq!(native(&query, &rows), expected);
    assert_eq!(query.canonical_bytes(), transcript);

    let floats = PreparedGraphSetAggregate::prepare(
        procedure(),
        &[],
        &[
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::average_int("mean", 1),
        ],
        0,
        None,
    )
    .unwrap();
    let float = |value| GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)));
    let rows =
        [1.0, 9_007_199_254_740_992.0, -9_007_199_254_740_992.0].map(|value| row(1, float(value)));
    let expected = vec![GraphAggregateRow::from_group_values(
        Vec::new(),
        vec![Value(float(1.0)), Value(float(1.0 / 3.0))],
    )];
    assert_eq!(external(&floats, &rows), expected);
    assert_eq!(native(&floats, &rows), expected);
}

#[test]
fn input_pages_preserve_collection_occurrences_and_empty_group_semantics() {
    let rows = vec![
        row(1, int(1)),
        row(2, null()),
        row(1, int(3)),
        row(2, int(2)),
        row(1, int(3)),
    ];
    let ordered = procedure()
        .with_order_by(&[GraphValueOrder::descending(1)])
        .unwrap();
    let aggregate = [
        GraphAggregate::count_rows("n"),
        GraphAggregate::collect("values", 1),
    ];
    let query = PreparedGraphSetAggregate::prepare(
        ordered.clone().with_page(1, Some(3)),
        &[0],
        &aggregate,
        0,
        None,
    )
    .unwrap();
    use GraphAggregateValue::{Count, Value};
    let expected = vec![
        GraphAggregateRow::from_group_values(
            vec![int(1)],
            vec![
                Count(2),
                Value(GraphValue::List(vec![int(3), int(1)].into())),
            ],
        ),
        GraphAggregateRow::from_group_values(
            vec![int(2)],
            vec![Count(1), Value(GraphValue::List(vec![int(2)].into()))],
        ),
    ];
    assert_eq!(external(&query, &rows), expected);
    assert_eq!(native(&query, &rows), expected);

    for keys in [Vec::new(), vec![0]] {
        let query = PreparedGraphSetAggregate::prepare(
            ordered.clone().with_page(0, Some(0)),
            &keys,
            &aggregate,
            0,
            None,
        )
        .unwrap();
        let expected = if keys.is_empty() {
            vec![GraphAggregateRow::from_group_values(
                Vec::new(),
                vec![Count(0), Value(GraphValue::List(Vec::new().into()))],
            )]
        } else {
            Vec::new()
        };
        assert_eq!(external(&query, &rows), expected);
        assert_eq!(native(&query, &rows), expected);
    }
}
