use super::*;
use crate::GraphExactAverage;

fn exact_definition() -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare(input(), &[], &[
        GraphAggregate::count_rows("rows"),
        GraphAggregate::count("nonnull", 0),
        GraphAggregate::count_distinct("distinct", 0),
        GraphAggregate::sum_int("sum", 0),
        GraphAggregate::sum_int_distinct("distinct_sum", 0),
        GraphAggregate::average_int("average", 0),
        GraphAggregate::average_int_distinct("distinct_average", 0),
        GraphAggregate::min("minimum", 0),
        GraphAggregate::max("maximum", 0),
    ], 0, None).unwrap()
}

#[test]
fn all_nine_aggregates_equal_eager_results_for_4096_inputs() {
    let definition = exact_definition();
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let choices = [None, Some(CanonicalScalar::Null), Some(CanonicalScalar::Int(-3)),
        Some(CanonicalScalar::Int(0)), Some(CanonicalScalar::Int(7)),
        Some(CanonicalScalar::Int(7)), Some(CanonicalScalar::Int(i64::MIN)),
        Some(CanonicalScalar::Int(i64::MAX))];
    for mut code in 0..4096_usize {
        let visibility = code;
        let rows: Vec<_> = [0, 1, 1_u128 << 100, u128::MAX].into_iter().enumerate()
            .map(|(at, vid)| {
                let mut row = row(vid, choices[code % 8].clone());
                code /= 8;
                row.visible = (visibility + at) % 7 != 0;
                if (visibility + at) % 5 == 0 { row.labels.clear(); }
                row
            }).collect();
        let expected = expected(&definition, &rows);
        let input = source(rows);
        let dropped = Rc::clone(&input.dropped);
        let mut cursor = VertexAggregateCursor::new(input, plan.clone(), wide(), || Ok::<_, ()>(()));
        assert_eq!(cursor.next().unwrap().unwrap(), expected);
        assert_eq!(cursor.row_stats().snapshot_records, 4);
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert!(dropped.get());
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert!(cursor.next().is_none());
    }
    let mut cursor = VertexAggregateCursor::new(source(vec![]), plan, wide(), || Ok::<_, ()>(()));
    let empty = cursor.next().unwrap().unwrap();
    assert_eq!(empty, expected(&definition, &[]));
    assert!(empty.values()[..3].iter().all(|value| value.as_count() == Some(0)));
    assert!(empty.values()[3..].iter().all(GraphAggregateValue::is_null));
}

#[test]
fn native_vertex_ids_and_mixed_scalar_domains_survive_source_release() {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder.prepare_values(&[GraphColumn::property("p", "n", KEY),
        GraphColumn::vertex("id", "n")], 0, None).unwrap().with_duplicates();
    let definition = PreparedGraphAggregate::prepare(input, &[], &[
        GraphAggregate::count_distinct("scalars", 0), GraphAggregate::min("lo", 0),
        GraphAggregate::max("hi", 0), GraphAggregate::count_distinct("ids", 1),
        GraphAggregate::min("first", 1), GraphAggregate::max("last", 1),
    ], 0, None).unwrap();
    let secret = CanonicalScalar::ucs_basic_text(&"retained-secret".repeat(64)).unwrap();
    let rows = vec![row(0, Some(CanonicalScalar::Int(7))), row(1, Some(secret.clone())),
        row(1_u128 << 100, Some(CanonicalScalar::Int(7))), row(u128::MAX, None)];
    let expected = expected(&definition, &rows);
    let input = source(rows);
    let dropped = Rc::clone(&input.dropped);
    let mut cursor = VertexAggregateCursor::new(input, VertexAggregatePlan::compile(&definition).unwrap(),
        wide(), || Ok::<_, ()>(()));
    let result = cursor.next().unwrap().unwrap();
    assert!(dropped.get());
    assert_eq!(result, expected);
    assert_eq!(result.values()[0].as_count(), Some(2));
    assert_eq!(result.values()[3].as_count(), Some(4));
    assert_eq!(result.values()[4].as_value(), Some(&GraphValue::Vertex(VId(0))));
    assert_eq!(result.values()[5].as_value(), Some(&GraphValue::Vertex(VId(u128::MAX))));
    assert!(!format!("{cursor:?} {result:?}").contains("retained-secret"));
}

#[test]
fn averages_preserve_sub_float_fractions_and_distinct_support() {
    let definition = exact_definition();
    let high = i64::MAX;
    let rows = vec![row(0, Some(CanonicalScalar::Int(high))),
        row(1, Some(CanonicalScalar::Int(high - 1))), row(2, Some(CanonicalScalar::Int(high)))];
    let mut cursor = VertexAggregateCursor::new(source(rows.clone()),
        VertexAggregatePlan::compile(&definition).unwrap(), wide(), || Ok::<_, ()>(()));
    let result = cursor.next().unwrap().unwrap();
    assert_eq!(result, expected(&definition, &rows));
    assert_eq!(result.values()[5].as_average(), GraphExactAverage::new(3 * i128::from(high) - 1, 3));
    assert_eq!(result.values()[6].as_average(), GraphExactAverage::new(2 * i128::from(high) - 1, 2));
    assert_eq!(result.values()[4].as_integer(), Some(2 * i128::from(high) - 1));
}

#[test]
fn duplicates_do_not_allocate_more_membership_or_extremum_payloads() {
    let definition = exact_definition();
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let mut first = None;
    for count in [1_u64, 7, 256, 4096] {
        let rows = (0..count).map(|id| row(u128::from(id), Some(CanonicalScalar::Int(7)))).collect();
        let mut cursor = VertexAggregateCursor::new(source(rows), plan.clone(), wide(), || Ok::<_, ()>(()));
        let result = cursor.next().unwrap().unwrap();
        assert_eq!(result.values()[0].as_count(), Some(count));
        assert_eq!(result.values()[2].as_count(), Some(1));
        assert_eq!(result.values()[4].as_integer(), Some(7));
        assert_eq!(result.values()[5].as_average(), GraphExactAverage::new(7, 1));
        let scratch = cursor.evaluator_stats().scratch_entries;
        assert_eq!(*first.get_or_insert(scratch), scratch);
    }
    // COUNT DISTINCT text retains only one admitted clone despite repeated
    // source rows. Large payloads must cost more than a short scalar.
    let measure = |size: usize, count: u128| {
        let value = CanonicalScalar::ucs_basic_text(&"x".repeat(size)).unwrap();
        let rows = (0..count).map(|id| row(id, Some(value.clone()))).collect();
        let definition = PreparedGraphAggregate::prepare(input(), &[],
            &[GraphAggregate::count_distinct("n", 0), GraphAggregate::min("lo", 0)], 0, None).unwrap();
        let mut cursor = VertexAggregateCursor::new(source(rows), VertexAggregatePlan::compile(&definition).unwrap(),
            wide(), || Ok::<_, ()>(()));
        cursor.next().unwrap().unwrap();
        cursor.evaluator_stats().scratch_entries
    };
    assert_eq!(measure(4096, 1), measure(4096, 128));
    assert!(measure(4096, 1) > measure(1, 1));
}

#[test]
fn all_new_state_growth_and_finalization_checkpoints_are_fused_failures() {
    let plan = VertexAggregatePlan::compile(&exact_definition()).unwrap();
    let rows = vec![row(0, Some(CanonicalScalar::Int(-3))), row(1, Some(CanonicalScalar::Int(7)))];
    let mut calls = 0;
    let (expected, stats) = {
        let mut cursor = VertexAggregateCursor::new(source(rows.clone()), plan.clone(), wide(), || {
            calls += 1; Ok::<_, ()>(())
        });
        let result = cursor.next().unwrap().unwrap();
        (result, cursor.evaluator_stats())
    };
    for boundary in 0..calls {
        let input = source(rows.clone());
        let dropped = Rc::clone(&input.dropped);
        let mut visited = 0;
        let mut cursor = VertexAggregateCursor::new(input, plan.clone(), wide(), move || {
            let at = visited; visited += 1;
            if at == boundary { Err(()) } else { Ok(()) }
        });
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Interrupted(())))));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(dropped.get());
        assert!(cursor.next().is_none());
    }
    for (work, scratch, success) in [(stats.work_units, stats.scratch_entries, true),
        (stats.work_units - 1, stats.scratch_entries, false),
        (stats.work_units, stats.scratch_entries - 1, false)] {
        let mut cursor = VertexAggregateCursor::new(source(rows.clone()), plan.clone(),
            GqlQueryPolicy::new(2, 1, work, scratch), || Ok::<_, ()>(()));
        let result = cursor.next().unwrap();
        if success { assert_eq!(result.unwrap(), expected); }
        else { assert!(result.is_err()); assert_eq!(cursor.row_stats().result_rows, 0); }
        assert!(cursor.next().is_none());
    }
}

#[test]
fn late_new_numeric_errors_and_source_errors_never_release_retained_values() {
    for (function, average) in [(GraphAggregate::average_int("v", 0), true),
        (GraphAggregate::average_int_distinct("v", 0), true),
        (GraphAggregate::sum_int_distinct("v", 0), false)] {
        let definition = PreparedGraphAggregate::prepare(input(), &[],
            &[GraphAggregate::count_distinct("n", 0), function], 0, None).unwrap();
        let rows = vec![row(0, Some(CanonicalScalar::Int(7))),
            row(1, Some(CanonicalScalar::ucs_basic_text("secret").unwrap()))];
        let input = source(rows);
        let dropped = Rc::clone(&input.dropped);
        let mut cursor = VertexAggregateCursor::new(input, VertexAggregatePlan::compile(&definition).unwrap(),
            wide(), || Ok::<_, ()>(()));
        let error = cursor.next().unwrap().unwrap_err();
        if average {
            assert!(matches!(error, GqlQueryError::Source(GraphAggregateError::NonIntegerAverage { aggregate: 1 })));
        } else {
            assert!(matches!(error, GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate: 1 })));
        }
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(dropped.get()); assert!(cursor.next().is_none());
    }
    let mut input = source(vec![row(0, Some(CanonicalScalar::Int(7)))]);
    input.fail_at = Some(1);
    let dropped = Rc::clone(&input.dropped);
    let mut cursor = VertexAggregateCursor::new(input, VertexAggregatePlan::compile(&exact_definition()).unwrap(),
        wide(), || Ok::<_, ()>(()));
    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(GraphAggregateError::Source(
        VertexScanError::Source("source failed")))))));
    assert!(dropped.get()); assert!(cursor.next().is_none());
}

#[test]
fn distinct_membership_and_numeric_cells_are_atomic_on_growth_or_arithmetic_refusal() {
    let mut state = DistinctState::new(GraphAggregateFunction::SumIntDistinct);
    state.update::<(), ()>(Input::Scalar(Some(&CanonicalScalar::Int(7))), 0, &mut |_| Ok(())).unwrap();
    let refused = state.update::<(), ()>(Input::Scalar(Some(&CanonicalScalar::Int(9))), 0, &mut |event| {
        if event == VertexScanEvent::ScratchEntry { Err(GqlQueryError::Interrupted(())) } else { Ok(()) }
    });
    assert!(refused.is_err());
    assert_eq!(state.scalars.len(), 1);
    assert!(state.scalars.contains(&CanonicalScalar::Int(7)));
    assert!(matches!(state.into_numeric(), NumericState::Sum(Some(7))));
    for (function, numeric) in [
        (GraphAggregateFunction::CountDistinct, NumericState::Count(u64::MAX)),
        (GraphAggregateFunction::SumIntDistinct, NumericState::Sum(Some(i128::MAX))),
        (GraphAggregateFunction::AverageIntDistinct, NumericState::Average { sum: i128::MAX, count: 1 }),
        (GraphAggregateFunction::AverageIntDistinct, NumericState::Average { sum: 0, count: u64::MAX }),
    ] {
        let mut state = DistinctState::new(function);
        state.accumulator = numeric;
        assert!(matches!(state.update::<(), ()>(Input::Scalar(Some(&CanonicalScalar::Int(1))), 5,
            &mut |_| Ok(())), Err(GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate: 5 }))));
        assert!(state.scalars.is_empty() && state.vertices.is_empty());
        assert_eq!(state.max_payload, 0);
        assert!(matches!(state.into_numeric(), NumericState::Count(u64::MAX)
            | NumericState::Sum(Some(i128::MAX))
            | NumericState::Average { sum: i128::MAX, count: 1 }
            | NumericState::Average { sum: 0, count: u64::MAX }));
    }
    let value = CanonicalScalar::ucs_basic_text(&"private".repeat(256)).unwrap();
    let mut state = DistinctState::new(GraphAggregateFunction::CountDistinct);
    state.update::<(), ()>(Input::Scalar(Some(&value)), 0, &mut |_| Ok(())).unwrap();
    drop(value);
    assert_eq!(state.scalars.len(), 1);
    assert!(matches!(state.into_numeric(), NumericState::Count(1)));
}

#[test]
fn numeric_distinct_vertex_arguments_preserve_empty_nulls_and_typed_nonempty_errors() {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder.prepare_values(&[GraphColumn::vertex("id", "n")], 0, None).unwrap().with_duplicates();
    for (function, average) in [(GraphAggregate::sum_int_distinct("v", 0), false),
        (GraphAggregate::average_int_distinct("v", 0), true)] {
        let definition = PreparedGraphAggregate::prepare(input.clone(), &[], &[function], 0, None).unwrap();
        let plan = VertexAggregatePlan::compile(&definition).unwrap();
        let mut empty = VertexAggregateCursor::new(source(vec![]), plan.clone(), wide(), || Ok::<_, ()>(()));
        let result = empty.next().unwrap().unwrap();
        assert!(result.values()[0].is_null());
        assert_eq!(result, expected(&definition, &[]));
        let mut nonempty = VertexAggregateCursor::new(source(vec![row(u128::MAX, None)]), plan, wide(), || Ok::<_, ()>(()));
        let error = nonempty.next().unwrap().unwrap_err();
        assert!(match error {
            GqlQueryError::Source(GraphAggregateError::NonIntegerAverage { aggregate: 0 }) => average,
            GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate: 0 }) => !average,
            _ => false,
        });
        assert_eq!(nonempty.row_stats().result_rows, 0);
        assert!(nonempty.next().is_none());
    }
}

fn grouped_distinct(keys: &[usize]) -> PreparedGraphAggregate {
    let base = grouped_definition(keys);
    PreparedGraphAggregate::prepare(base.input_pattern().clone(), keys, &[
        GraphAggregate::count_rows("rows"),
        GraphAggregate::count_distinct("values", 1),
        GraphAggregate::sum_int_distinct("sum", 1),
        GraphAggregate::average_int_distinct("average", 1),
        GraphAggregate::count_distinct("ids", 2),
    ], 0, None).unwrap()
}

#[test]
fn grouped_distinct_support_is_local_and_matches_eager_for_4096_inputs() {
    let definition = grouped_distinct(&[0]);
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let buckets = [None, Some(CanonicalScalar::Null), Some(CanonicalScalar::Int(7)),
        Some(CanonicalScalar::ucs_basic_text("7").unwrap())];
    let values = [None, Some(-3), Some(7), Some(7)];
    for mut code in 0..4096_usize {
        let original = code;
        let rows: Vec<_> = (0..6).map(|at| {
            let bucket = buckets[code % 4].clone(); code /= 4;
            grouped_row(at, bucket, values[(original + at as usize) % 4])
        }).collect();
        let input = source(rows.clone());
        let dropped = Rc::clone(&input.dropped);
        let mut cursor = VertexAggregateCursor::new(input, plan.clone(), wide(), || Ok::<_, ()>(()));
        let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(actual, expected_groups(&definition, &rows));
        assert!(dropped.get());
        assert!(cursor.pending.is_none());
        assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
    }
    // The same argument in separate groups is counted once in EACH group.
    let rows = vec![grouped_row(0, None, Some(7)), grouped_row(1, None, Some(7)),
        grouped_row(u128::MAX, Some(CanonicalScalar::Int(7)), Some(7))];
    for keys in [&[0][..], &[0, 2][..], &[2, 0][..]] {
        let definition = grouped_distinct(keys);
        let mut cursor = VertexAggregateCursor::new(source(rows.clone()),
            VertexAggregatePlan::compile(&definition).unwrap(), wide(), || Ok::<_, ()>(()));
        let actual = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(actual, expected_groups(&definition, &rows));
        for group in actual {
            assert_eq!(group.values()[1].as_count(), Some(1));
            assert_eq!(group.values()[2].as_integer(), Some(7));
            assert_eq!(group.values()[3].as_average(), GraphExactAverage::new(7, 1));
        }
    }
}

#[test]
fn grouped_distinct_every_failure_keeps_delivered_prefix_and_releases_pending_support() {
    let definition = grouped_distinct(&[0]);
    let plan = VertexAggregatePlan::compile(&definition).unwrap();
    let rows = vec![grouped_row(0, Some(CanonicalScalar::Int(2)), Some(7)),
        grouped_row(1, None, Some(-3)), grouped_row(2, Some(CanonicalScalar::Int(1)), Some(7))];
    let expected = expected_groups(&definition, &rows);
    let mut count = 0;
    {
        let mut cursor = VertexAggregateCursor::new(source(rows.clone()), plan.clone(), wide(), || {
            count += 1; Ok::<_, usize>(())
        });
        assert_eq!(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected);
    }
    for stop in 1..=count {
        let input = source(rows.clone());
        let dropped = Rc::clone(&input.dropped);
        let mut calls = 0;
        let mut cursor = VertexAggregateCursor::new(input, plan.clone(), wide(), move || {
            calls += 1; if calls == stop { Err(stop) } else { Ok(()) }
        });
        let mut prefix = Vec::new();
        loop {
            match cursor.next() {
                Some(Ok(row)) => prefix.push(row),
                Some(Err(GqlQueryError::Interrupted(at))) => { assert_eq!(at, stop); break; }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        assert_eq!(prefix, expected[..prefix.len()]);
        assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
        assert!(cursor.pending.is_none());
        assert!(dropped.get());
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }
    let mut cursor = VertexAggregateCursor::new(source(rows), plan, wide(), || Ok::<_, ()>(()));
    cursor.next().unwrap().unwrap();
    assert!(cursor.pending.is_some());
    cursor.close();
    assert!(cursor.pending.is_none());
    assert!(cursor.next().is_none());
}
