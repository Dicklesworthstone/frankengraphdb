use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison};
use crate::{GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp as Op, GraphSetProjection};
use fgdb_delta_types::PropertyKeyId;
use core::convert::Infallible;
use std::collections::BTreeSet;

fn allow(_: GlaExecutionEvent) -> Result<(), QueryError<Infallible, usize>> { Ok(()) }
fn definition() -> PreparedGraphAggregate {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder.prepare_values(&[
        GraphColumn::property("key", "n", PropertyKeyId(1)),
        GraphColumn::property("value", "n", PropertyKeyId(2)),
    ], 0, None).unwrap().with_duplicates();
    PreparedGraphAggregate::prepare(input, &[0], &[GraphAggregate::sum_int("sum", 1)], 0, None).unwrap()
}
fn row(key: i64, value: Option<i128>) -> GraphAggregateRow {
    GraphAggregateRow::from_group_values(vec![GraphValue::Scalar(CanonicalScalar::Int(key))],
        vec![value.map_or_else(|| GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
            GraphAggregateValue::Integer)])
}
fn select(
    query: &PreparedGraphAggregate, rows: Vec<GraphAggregateRow>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<Infallible, usize>>,
) -> Result<Vec<GraphAggregateRow>, QueryError<Infallible, usize>> {
    let mut state = query.streamed_group_ranking(rows.len());
    for row in rows { state.push(query, row, control)?; }
    state.finish(query, control)
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Option<i128>> {
    rows.iter().map(|row| row.values()[0].as_integer()).collect()
}

#[test]
fn distinct_before_topk_and_pages_matches_40960_independent_full_sort_cases() {
    let choices = [None, Some(-3_i128), Some(0), Some(2)];
    for code in 0..256_usize {
        let values: Vec<_> = (0..4).map(|i| choices[(code >> (i * 2)) & 3]).collect();
        let input: Vec<_> = (0..4).rev().map(|i| row(i as i64, values[i])).collect();
        for descending in [false, true] {
            for nulls in [GraphNullPlacement::First, GraphNullPlacement::Last] {
                for distinct in [false, true] {
                    let mut expected: Vec<_> = values.iter().copied().enumerate().collect();
                    expected.sort_by(|(ak, a), (bk, b)| {
                        let order = match (a, b) {
                            (None, None) => Ordering::Equal,
                            (None, _) => if nulls == GraphNullPlacement::First { Ordering::Less } else { Ordering::Greater },
                            (_, None) => if nulls == GraphNullPlacement::First { Ordering::Greater } else { Ordering::Less },
                            (Some(a), Some(b)) => if descending { b.cmp(a) } else { a.cmp(b) },
                        };
                        order.then_with(|| ak.cmp(bk))
                    });
                    let mut seen = BTreeSet::new();
                    expected.retain(|(_, value)| !distinct || seen.insert(*value));
                    for offset in 0..5_u64 { for count in 0..4_u64 {
                        let mut query = definition().with_key_output_columns(&[]).unwrap()
                            .with_result_clauses(&[], &[GraphAggregateOrder {
                                column: GraphAggregateColumn::Aggregate(0), descending, nulls,
                            }]).unwrap().with_distinct_output(distinct);
                        query.offset = offset; query.count = Some(count);
                        let physical = query.prepare_streamed_output().unwrap();
                        assert!(physical.has_streamed_output_stage());
                        let actual = select(&physical, input.clone(), &mut allow).unwrap();
                        assert_eq!(plain(&actual), expected.iter().skip(offset as usize).take(count as usize)
                            .map(|(_, value)| *value).collect::<Vec<_>>());
                    }}
                }
            }
        }
    }
}

#[test]
fn distinct_representative_uses_hidden_rank_and_retains_concrete_numeric_domains() {
    let base = definition();
    let query = PreparedGraphAggregate::prepare(base.input_pattern().clone(), &[0], &[
        GraphAggregate::count_rows("count"), GraphAggregate::sum_int("sum", 1),
    ], 0, None).unwrap();
    let expression = GraphIntegerExpression::prepare(&[Op::Column(2), Op::Column(1), Op::Coalesce]).unwrap();
    let input = vec![
        GraphAggregateRow::from_group_values(row(0, None).keys.into_vec(), vec![GraphAggregateValue::Count(2),
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null))]),
        GraphAggregateRow::from_group_values(row(1, None).keys.into_vec(), vec![GraphAggregateValue::Count(1),
            GraphAggregateValue::Integer(2)]),
    ];
    for descending in [false, true] {
        let query = query.clone().with_result_clauses(&[], &[GraphAggregateOrder {
            column: GraphAggregateColumn::Aggregate(0), descending, nulls: GraphNullPlacement::Last,
        }]).unwrap().with_output_projection(vec![GraphSetProjection::new("selected",
            GraphSetValue::Integer(expression.clone()))]).unwrap().with_distinct_output(true);
        let physical = query.prepare_streamed_output().unwrap();
        for rows in [input.clone(), input.iter().rev().cloned().collect()] {
            let result = select(&physical, rows, &mut allow).unwrap();
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].values(), &[if descending { GraphAggregateValue::Count(2) }
                else { GraphAggregateValue::Integer(2) }]);
        }
    }
}

#[test]
fn duplicate_groups_retain_one_class_and_do_not_crowd_a_distinct_page() {
    let mut query = definition().with_key_output_columns(&[]).unwrap().with_distinct_output(true)
        .with_result_clauses(&[], &[GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(0))]).unwrap();
    query.count = Some(2);
    let mut state = query.streamed_group_ranking(4097);
    for key in (0..4096).rev() {
        state.push(&query, row(key, Some(7)), &mut allow).unwrap();
        assert_eq!(state.distinct.as_ref().unwrap().len(), 1);
        assert_eq!(state.heap.len(), 1);
    }
    state.push(&query, row(4096, Some(-1)), &mut allow).unwrap();
    assert_eq!(state.distinct.as_ref().unwrap().len(), 2);
    assert_eq!(plain(&state.finish(&query, &mut allow).unwrap()), vec![Some(7), Some(-1)]);
    // Without explicit order, the COMPLETE key still determines representative
    // order, not canonical output-value order or order of class insertion.
    let query = definition().with_key_output_columns(&[]).unwrap().with_distinct_output(true);
    assert_eq!(plain(&select(&query, vec![row(2, Some(-1)), row(1, Some(7)), row(0, Some(7))], &mut allow).unwrap()),
        vec![Some(7), Some(-1)]);
}

#[test]
fn every_class_rank_projection_and_sort_checkpoint_refuses_and_retries() {
    let query = definition().with_key_output_columns(&[]).unwrap().with_distinct_output(true)
        .with_result_clauses(&[], &[GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(0))]).unwrap();
    let input = vec![row(3, None), row(2, Some(1)), row(1, Some(7)), row(0, Some(7))];
    let before = query.canonical_bytes();
    let mut calls = 0;
    let result = select(&query, input.clone(), &mut |event| {
        assert_ne!(event, GlaExecutionEvent::ResultRow);
        calls += 1; Ok(())
    }).unwrap();
    assert!(calls > 20);
    for stop in 1..=calls {
        let mut at = 0;
        let failure = select(&query, input.clone(), &mut |_| {
            at += 1;
            if at == stop { Err(GqlQueryError::Interrupted(stop)) } else { Ok(()) }
        });
        assert!(matches!(failure, Err(GqlQueryError::Interrupted(found)) if found == stop));
        assert_eq!(at, stop); assert_eq!(query.canonical_bytes(), before);
        assert_eq!(select(&query, input.clone(), &mut allow).unwrap(), result);
    }
}

#[test]
fn losing_or_offpage_output_errors_are_not_hidden_by_distinct_or_empty_prefixes() {
    let expression = GraphIntegerExpression::prepare(&[
        Op::Literal(Some(1)), Op::Column(1), Op::Binary(GraphIntegerBinary::Divide),
    ]).unwrap();
    for (offset, count) in [(0, 0), (0, 1), (u64::MAX, u64::MAX)] {
        let mut query = definition().with_result_clauses(&[], &[
            GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(0)),
        ]).unwrap().with_output_projection(vec![GraphSetProjection::new("reciprocal",
            GraphSetValue::Integer(expression.clone()))]).unwrap().with_distinct_output(true);
        query.offset = offset; query.count = Some(count);
        let input = vec![row(0, Some(1)), row(1, Some(0))];
        assert!(matches!(select(&query, input.clone(), &mut allow),
            Err(GqlQueryError::Source(GraphAggregateError::OutputExpression { .. }))));
        let predicate = GraphHavingExpression::prepare(&[GraphHavingOp::Compare {
            left: GraphHavingOperand::Column(GraphAggregateColumn::Aggregate(0)),
            comparison: IntegerComparison::Greater,
            right: GraphHavingOperand::Integer(0),
        }]).unwrap();
        query.having_expression = Some(predicate);
        let selected = select(&query, input, &mut allow).unwrap();
        assert_eq!(selected.len(), usize::from(offset == 0 && count != 0));
    }
}
