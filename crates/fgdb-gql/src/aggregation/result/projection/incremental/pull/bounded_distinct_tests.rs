use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, IntegerComparison};
use crate::{GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp as Op, GraphSetProjection};
use fgdb_delta_types::PropertyKeyId;
use std::collections::BTreeSet;

fn definition(offset: u64, count: Option<u64>, descending: bool, nulls: GraphNullPlacement)
    -> PreparedGraphAggregate {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder.prepare_values(&[
        GraphColumn::property("key", "n", PropertyKeyId(1)),
        GraphColumn::property("amount", "n", PropertyKeyId(2)),
    ], 0, None).unwrap().with_duplicates();
    PreparedGraphAggregate::prepare(input, &[0], &[
        GraphAggregate::count_rows("count"), GraphAggregate::average_int("average", 1),
    ], offset, count).unwrap().with_result_clauses(&[], &[
        GraphAggregateOrder { column: GraphAggregateColumn::Aggregate(1), descending, nulls },
    ]).unwrap().with_key_output_columns(&[]).unwrap()
        .with_aggregate_output_prefix(1).unwrap().with_distinct_output(true)
}
fn row(key: i64, class: u64, rank: Option<(i128, u64)>) -> GraphAggregateRow {
    GraphAggregateRow::from_group_values(
        vec![GraphValue::Scalar(CanonicalScalar::Int(key))],
        vec![GraphAggregateValue::Count(class), rank.map_or(
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
            |(n, d)| GraphAggregateValue::Average(GraphExactAverage::new(n, d).unwrap()),
        )],
    )
}
fn order(a: &GraphAggregateRow, b: &GraphAggregateRow, desc: bool, nulls: GraphNullPlacement) -> Ordering {
    let value = |row: &GraphAggregateRow| row.values()[1].as_average();
    let order = match (value(a), value(b)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => if nulls == GraphNullPlacement::First { Ordering::Less } else { Ordering::Greater },
        (Some(_), None) => if nulls == GraphNullPlacement::First { Ordering::Greater } else { Ordering::Less },
        (Some(a), Some(b)) => {
            // Only the independent SMALL-domain fixture oracle multiplies.
            // Production must use the existing overflow-free exact comparator.
            let order = (a.numerator() * i128::from(b.denominator()))
                .cmp(&(b.numerator() * i128::from(a.denominator())));
            if desc { order.reverse() } else { order }
        }
    };
    order.then_with(|| a.keys().cmp(b.keys()))
}
fn classes(rows: &[GraphAggregateRow]) -> Vec<u64> {
    rows.iter().map(|r| r.values()[0].as_count().unwrap()).collect()
}
fn check_index(ranking: &mut StreamedGroupRanking) {
    assert!(ranking.heap.len() <= ranking.prefix);
    let index = ranking.distinct.as_mut().unwrap();
    assert_eq!(index.len(), ranking.heap.len());
    for (at, row) in ranking.heap.iter().enumerate() {
        assert_eq!(index.position(row.distinct_key.as_ref().unwrap(), &mut |_| Ok::<_, ()>(())).unwrap(), Some(at));
    }
}

#[test]
fn distinct_prefix_matches_sort_then_dedup_then_page_for_all_small_class_rank_assignments() {
    for code in 0..256_u64 {
        let rows: Vec<_> = (0..4).map(|at| {
            let choice = (code >> (2 * at)) & 3;
            row(at, 1 + choice % 2, if choice < 2 { None } else { Some((i128::from(at - 2), 3)) })
        }).collect();
        for desc in [false, true] {
            for nulls in [GraphNullPlacement::First, GraphNullPlacement::Last] {
                let mut sorted = rows.clone();
                sorted.sort_by(|a, b| order(a, b, desc, nulls));
                let mut seen = BTreeSet::new();
                let expected: Vec<_> = sorted.iter().map(|r| r.values()[0].as_count().unwrap())
                    .filter(|class| seen.insert(*class)).collect();
                for (skip, take) in [(0, None), (0, Some(0)), (0, Some(1)), (1, Some(1)),
                    (2, Some(2)), (u64::MAX, Some(u64::MAX))] {
                    for reverse in [false, true] {
                        let logical = definition(skip, take, desc, nulls);
                        assert!(!logical.supports_incremental_maintenance());
                        let bytes = logical.canonical_bytes();
                        let q = logical.prepare_streamed_output().unwrap();
                        let mut ranking = q.streamed_group_ranking(rows.len());
                        let mut input = rows.clone();
                        if reverse { input.reverse(); }
                        let mut control = |event| {
                            assert_ne!(event, GlaExecutionEvent::ResultRow);
                            Ok::<_, QueryError<(), ()>>(())
                        };
                        for row in input {
                            ranking.push(&q, row, &mut control).unwrap();
                            check_index(&mut ranking);
                        }
                        let actual = ranking.finish(&q, &mut control).unwrap();
                        let wanted: Vec<_> = expected.iter().copied()
                            .skip(usize::try_from(skip).unwrap_or(usize::MAX))
                            .take(take.and_then(|n| usize::try_from(n).ok()).unwrap_or(usize::MAX)).collect();
                        assert_eq!(classes(&actual), wanted);
                        assert_eq!(logical.canonical_bytes(), bytes);
                    }
                }
            }
        }
    }
}

#[test]
fn improved_residents_and_evicted_classes_update_their_actual_heap_slots() {
    let q = definition(0, Some(2), false, GraphNullPlacement::Last).prepare_streamed_output().unwrap();
    let mut ranking = q.streamed_group_ranking(7);
    let mut control = |_| Ok::<_, QueryError<(), ()>>(());
    // A100 is evicted by C70; A60 must re-enter. B90 was not deleted from
    // the input, merely no longer competitive. A50 improves a resident class.
    for (key, class, rank) in [(0, 1, 100), (1, 2, 90), (2, 3, 70), (3, 1, 60),
        (4, 1, 50), (5, 2, 80), (6, 3, 40)] {
        ranking.push(&q, row(key, class, Some((rank, 1))), &mut control).unwrap();
        check_index(&mut ranking);
    }
    assert_eq!(classes(&ranking.finish(&q, &mut control).unwrap()), [3, 1]);
}

#[test]
fn numeric_equivalence_keeps_the_best_ranked_concrete_representation_and_not_enum_order() {
    let expression = GraphIntegerExpression::prepare(&[
        Op::Column(1), Op::Literal(Some(2)), Op::Compare(IntegerComparison::Greater),
        Op::Column(2), Op::Column(1), Op::Case,
    ]).unwrap();
    let q = definition(0, None, true, GraphNullPlacement::Last)
        .with_result_clauses(&[], &[GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(0))]).unwrap()
        .with_output_projection(vec![GraphSetProjection::new("value", GraphSetValue::Integer(expression))]).unwrap()
        .prepare_streamed_output().unwrap();
    let mut ranking = q.streamed_group_ranking(3);
    let mut control = |_| Ok::<_, QueryError<(), ()>>(());
    ranking.push(&q, row(0, 2, Some((4, 2))), &mut control).unwrap(); // Count(2)
    ranking.push(&q, row(1, 3, Some((6, 3))), &mut control).unwrap(); // Average(2/1), better rank
    ranking.push(&q, row(2, 4, Some((3, 2))), &mut control).unwrap(); // Distinct 3/2, ranks first
    check_index(&mut ranking);
    let result = ranking.finish(&q, &mut control).unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].values()[0], GraphAggregateValue::Average(GraphExactAverage::new(3, 2).unwrap()));
    assert_eq!(result[1].values()[0], GraphAggregateValue::Average(GraphExactAverage::new(2, 1).unwrap()));

    // Index keys normalize numeric equivalence, but never conflate a vertex,
    // edge, text, list or NULL with an integer of the same spelling.
    let values = [
        GraphAggregateValue::Count(2), GraphAggregateValue::Integer(2),
        GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2))),
        GraphAggregateValue::Average(GraphExactAverage::new(6, 3).unwrap()),
        GraphAggregateValue::Value(GraphValue::Vertex(VId(2))),
        GraphAggregateValue::Value(GraphValue::Edge(fgdb_types::EId(2))),
        GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
        GraphAggregateValue::Integer(i128::MIN), GraphAggregateValue::Integer(i128::MAX),
    ];
    let keys: BTreeSet<_> = values.into_iter().map(|v| {
        DistinctIndex::key(&GraphAggregateRow::from_group_values(vec![], vec![v]), &mut |_| Ok::<_, ()>(())).unwrap()
    }).collect();
    assert_eq!(keys.len(), 6);
}

#[test]
fn duplicate_zero_column_output_and_huge_windows_do_not_allocate_by_limit() {
    for skip in [0, 1, u64::MAX] {
        let q = definition(skip, Some(u64::MAX), false, GraphNullPlacement::First)
            .with_aggregate_output_prefix(0).unwrap().prepare_streamed_output().unwrap();
        let mut ranking = q.streamed_group_ranking(1024);
        let mut control = |_| Ok::<_, QueryError<(), ()>>(());
        for key in 0..1024 {
            ranking.push(&q, row(key, 1, None), &mut control).unwrap();
            check_index(&mut ranking);
            assert!(ranking.heap.len() <= 1);
        }
        let actual = ranking.finish(&q, &mut control).unwrap();
        assert_eq!(actual.len(), if skip == 0 { 1 } else { 0 });
        if skip == 0 { assert!(actual[0].keys().is_empty() && actual[0].values().is_empty()); }
    }
}

#[test]
fn finite_distinct_prefix_never_retains_all_losing_output_classes() {
    let q = definition(1, Some(2), true, GraphNullPlacement::Last).prepare_streamed_output().unwrap();
    let mut ranking = q.streamed_group_ranking(4096);
    let mut control = |_| Ok::<_, QueryError<(), ()>>(());
    for key in 0..4096_i64 {
        ranking.push(&q, row(key, key as u64, Some((i128::from(key), 1))), &mut control).unwrap();
        check_index(&mut ranking);
        assert!(ranking.heap.len() <= 3);
    }
    assert_eq!(classes(&ranking.finish(&q, &mut control).unwrap()), [4094, 4093]);
}

#[test]
fn every_distinct_key_index_swap_and_selection_refusal_aborts_and_retries() {
    let q = definition(1, Some(1), false, GraphNullPlacement::First).prepare_streamed_output().unwrap();
    let run = |stop| {
        let mut calls = 0;
        let result = (|| {
            let mut control = |_| {
                calls += 1;
                if calls == stop { Err(GqlQueryError::<GraphAggregateError<()>, _>::Interrupted(stop)) } else { Ok(()) }
            };
            let mut ranking = q.streamed_group_ranking(5);
            for (key, class, rank) in [(0, 1, 100), (1, 2, 90), (2, 3, 80), (3, 1, 70), (4, 3, 60)] {
                ranking.push(&q, row(key, class, Some((rank, 1))), &mut control)?;
            }
            ranking.finish(&q, &mut control)
        })();
        (result, calls)
    };
    let (expected, calls) = run(usize::MAX);
    assert_eq!(classes(expected.as_ref().unwrap()), [1]);
    for stop in 1..=calls {
        let (refused, seen) = run(stop);
        assert_eq!(refused, Err(GqlQueryError::Interrupted(stop)));
        assert_eq!(seen, stop);
    }
    assert_eq!(run(usize::MAX), (expected, calls));
}

#[test]
fn duplicate_or_evicted_outputs_never_hide_late_expression_errors_even_at_limit_zero() {
    for (skip, take) in [(0, Some(1)), (0, Some(0)), (u64::MAX, Some(u64::MAX))] {
        let q = definition(skip, take, false, GraphNullPlacement::Last)
            .with_output_projection(vec![GraphSetProjection::new("reciprocal", GraphSetValue::Integer(
                GraphIntegerExpression::prepare(&[
                    Op::Literal(Some(1)), Op::Column(1), Op::Literal(Some(1)),
                    Op::Binary(GraphIntegerBinary::Subtract), Op::Binary(GraphIntegerBinary::Divide),
                ]).unwrap(),
            ))]).unwrap().prepare_streamed_output().unwrap();
        let mut ranking = q.streamed_group_ranking(3);
        let mut control = |_| Ok::<_, QueryError<(), ()>>(());
        for key in 0..2 { ranking.push(&q, row(key, 2, Some((i128::from(key), 1))), &mut control).unwrap(); }
        assert!(matches!(ranking.push(&q, row(2, 1, Some((100, 1))), &mut control),
            Err(GqlQueryError::Source(GraphAggregateError::OutputExpression { column: 0, .. }))));
    }
}
