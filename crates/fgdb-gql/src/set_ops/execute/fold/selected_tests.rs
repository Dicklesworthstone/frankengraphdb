//! Selected products must not recover their Cartesian bag inside aggregates.
use super::*;
use crate::algebra::IntegerComparison;
use crate::{GraphAggregate, PreparedGraphSetAggregate};
use fgdb_types::CanonicalScalar;

fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn row(value: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![scalar(value)])
}
fn equal(a: usize, b: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(b),
    }
}
fn sequence(values: &[Option<i64>], name: &str) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            name.into(),
            GraphSetValue::List(
                values
                    .iter()
                    .copied()
                    .map(|value| GraphSetValue::Value(scalar(value)))
                    .collect(),
            ),
        )
        .unwrap()
}
fn query() -> PreparedGraphSet {
    sequence(&[Some(2), Some(1), Some(2), None], "left")
        .cross_join(sequence(&[Some(1), Some(2), Some(2), None], "right"))
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("right_value", GraphSetValue::Column(1)),
                GraphSetProjection::new("left_value", GraphSetValue::Column(0)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .filter(&[equal(0, 1)])
        .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<usize, usize>> {
    panic!("source-free relation must not manufacture a graph")
}

#[test]
fn count_and_order_sensitive_aggregates_consume_the_same_selected_sequence() {
    let query = query().with_page(1, Some(3));
    let expected = vec![
        GraphValueRow::from_owned_values(vec![scalar(Some(2)); 2]),
        GraphValueRow::from_owned_values(vec![scalar(Some(1)); 2]),
        GraphValueRow::from_owned_values(vec![scalar(Some(2)); 2]),
    ];
    let mut actual = Vec::new();
    let (rows, _) = query
        .fold_governed(
            policy(),
            no_source,
            || Ok(()),
            |row, _| {
                actual.push(row);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(rows.snapshot_records, 0);
    assert_eq!(rows.result_rows, 0); // intermediate rows are not public results
    assert_eq!(
        query
            .count_governed(policy(), no_source, || Ok(()))
            .unwrap()
            .0,
        Some(3)
    );
    let summary = PreparedGraphSetAggregate::prepare(
        query,
        &[],
        &[
            GraphAggregate::count_rows("count"),
            GraphAggregate::sum_int("sum", 0),
            GraphAggregate::collect("sequence", 1),
        ],
        0,
        None,
    )
    .unwrap();
    let result = summary
        .execute_governed(policy(), no_source, || Ok::<_, usize>(()))
        .unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(3));
    assert_eq!(result.value[0].get(1).unwrap().as_integer(), Some(5));
    assert_eq!(
        result.value[0].get(2).unwrap().as_value(),
        Some(&GraphValue::List(
            vec![scalar(Some(2)), scalar(Some(1)), scalar(Some(2))].into_boxed_slice()
        ))
    );
}

#[test]
fn equality_count_summarizes_large_duplicate_runs_without_visiting_each_pair() {
    let left = vec![row(Some(7)); 10_000];
    let right = vec![row(Some(7)); 10_000];
    let mut context = (0_usize, 0_usize, 0_usize, 0_u128);
    selected_cross::count_with_context(
        &left,
        &right,
        &[equal(0, 1)],
        None,
        &mut context,
        |context, event| {
            if event == GlaExecutionEvent::ScratchEntry {
                context.1 += 1;
            } else {
                context.0 += 1;
            }
            Ok::<_, usize>(())
        },
        |count, context| {
            context.2 += 1;
            context.3 += count as u128;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(context.3, 100_000_000);
    assert_eq!(context.2, left.len());
    assert_eq!(context.1, right.len() + 1); // ordinals plus one key, no result cells
    assert!(context.0 < 200 * (left.len() + right.len()));
    // A real aggregate owns source setup, physical counting and final delivery.
    let values = vec![Some(7); 128];
    let input = sequence(&values, "a")
        .cross_join(sequence(&values, "b"))
        .unwrap()
        .filter(&[equal(0, 1)])
        .unwrap();
    let summary =
        PreparedGraphSetAggregate::prepare(input, &[], &[GraphAggregate::count_rows("n")], 0, None)
            .unwrap();
    let result = summary
        .execute_governed(GqlQueryPolicy::new(0, 1, 100_000, 5_000), no_source, || {
            Ok::<_, usize>(())
        })
        .unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(16_384));
    assert_eq!(result.rows.result_rows, 1);
    assert!(result.evaluator.scratch_entries < 5_000);
}

#[test]
fn count_residuals_disjunctions_nulls_and_not_do_not_inherit_the_pure_equality_shortcut() {
    let left = [row(None), row(Some(1)), row(Some(2)), row(Some(2))];
    let right = [row(Some(2)), row(None), row(Some(1)), row(Some(2))];
    for code in [
        vec![equal(0, 1)],
        vec![
            equal(0, 1),
            GraphSetPredicateOp::Truth(Some(true)),
            GraphSetPredicateOp::Or,
        ],
        vec![equal(0, 1), GraphSetPredicateOp::Not],
        vec![
            equal(0, 1),
            GraphSetPredicateOp::Truth(None),
            GraphSetPredicateOp::And,
        ],
        vec![GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(0),
            comparison: IntegerComparison::Less,
            right: GraphSetOperand::Column(1),
        }],
    ] {
        GraphSetPredicateOp::validate_schema(&[GraphSetColumnType::Scalar; 2], &code).unwrap();
        let mut count = 0;
        selected_cross::count_with_context(
            &left,
            &right,
            &code,
            None,
            &mut count,
            |_, _| Ok::<_, usize>(()),
            |amount, count| {
                *count += amount;
                Ok(())
            },
        )
        .unwrap();
        let expected = left
            .iter()
            .flat_map(|a| right.iter().map(move |b| (a, b)))
            .filter(|(a, b)| {
                let pair = GraphValueRow::from_owned_values(
                    a.values().iter().chain(b.values()).cloned().collect(),
                );
                GraphSetPredicateOp::evaluate_row_with_control(&code, &pair, &mut |_| {
                    Ok::<_, usize>(())
                })
                .unwrap()
            })
            .count();
        assert_eq!(count, expected);
    }
}

#[test]
fn counted_windows_and_nested_factors_apply_once_after_complete_input_selection() {
    for offset in [0, 1, 4, 5, 6, u64::MAX] {
        for limit in [None, Some(0), Some(1), Some(3), Some(u64::MAX)] {
            let input = query().with_page(offset, limit);
            let count = input
                .count_governed(policy(), no_source, || Ok(()))
                .unwrap()
                .0;
            let expected = 5_u64.saturating_sub(offset);
            assert_eq!(
                count,
                Some(limit.map_or(expected, |limit| expected.min(limit)))
            );
        }
    }
    let nested = query().cross_join(query()).unwrap();
    assert_eq!(
        nested
            .count_governed(policy(), no_source, || Ok(()))
            .unwrap()
            .0,
        Some(25)
    );
    let distinct = query()
        .project(
            vec![GraphSetProjection::new("key", GraphSetValue::Column(0))],
            GraphSetQuantifier::Distinct,
        )
        .unwrap();
    assert_eq!(
        distinct
            .count_governed(policy(), no_source, || Ok(()))
            .unwrap()
            .0,
        Some(2)
    );
    let before_filter = sequence(&[Some(2), Some(1)], "a")
        .cross_join(sequence(&[Some(1), Some(2)], "b"))
        .unwrap()
        .with_page(0, Some(1))
        .filter(&[equal(0, 1)])
        .unwrap();
    assert_eq!(
        before_filter
            .count_governed(policy(), no_source, || Ok(()))
            .unwrap()
            .0,
        Some(0)
    );
}

#[test]
fn every_folded_probe_and_count_checkpoint_cancels_without_a_successful_partial_result() {
    let query = query();
    let mut checkpoints = 0;
    let expected = query
        .count_governed(policy(), no_source, || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
    for stop in 1..=checkpoints {
        let mut seen = 0;
        let result = query.count_governed(policy(), no_source, || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
    }
    assert_eq!(
        query
            .count_governed(policy(), no_source, || Ok(()))
            .unwrap()
            .0,
        expected.0
    );
    let mut checkpoints = 0;
    query
        .fold_governed(
            policy(),
            no_source,
            || {
                checkpoints += 1;
                Ok(())
            },
            |_, _| Ok(()),
        )
        .unwrap();
    for stop in 1..=checkpoints {
        let mut seen = 0;
        let result = query.fold_governed(
            policy(),
            no_source,
            || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            },
            |_, _| Ok(()),
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
    }
}

#[test]
fn deferred_sink_failure_never_stops_upstream_probe_validation() {
    let query = query();
    let mut checkpoints = 0;
    let mut consumed = 0;
    let failure = query.fold_governed(
        policy(),
        no_source,
        || {
            checkpoints += 1;
            Ok(())
        },
        |_, _| {
            consumed += 1;
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                77_usize,
            )))
        },
    );
    assert!(matches!(
        failure,
        Err(GqlQueryError::Source(GraphSetExecutionError::Source(77)))
    ));
    assert_eq!(consumed, 1);
    let mut seen = 0;
    let mut consumed = 0;
    let failure = query.fold_governed(
        policy(),
        no_source,
        || {
            seen += 1;
            if seen == checkpoints { Err(99) } else { Ok(()) }
        },
        |_, _| {
            consumed += 1;
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                77_usize,
            )))
        },
    );
    assert!(matches!(failure, Err(GqlQueryError::Interrupted(99))));
    assert_eq!(consumed, 1);
}

#[test]
fn count_and_fold_finish_both_graph_inputs_before_any_output_or_limit_zero() {
    let leaf = || {
        let mut builder = crate::algebra::GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        PreparedGraphSet::from(
            builder
                .prepare_values(&[crate::algebra::GraphColumn::vertex("n", "n")], 0, None)
                .unwrap(),
        )
    };
    let input = leaf()
        .cross_join(leaf())
        .unwrap()
        .filter(&[equal(0, 1)])
        .unwrap()
        .with_page(0, Some(0));
    for counting in [false, true] {
        let mut sources = 0;
        let mut source = |_: &PreparedGraphPattern<GraphValueRow>, _: GqlQueryPolicy| {
            sources += 1;
            if sources == 1 {
                Ok(GqlQueryExecution {
                    value: vec![],
                    rows: GqlExecutionStats {
                        snapshot_records: 0,
                        result_rows: 0,
                    },
                    evaluator: GlaExecutionStats::default(),
                })
            } else {
                Err(GqlQueryError::Source(88_usize))
            }
        };
        let result = if counting {
            input
                .count_governed(policy(), &mut source, || Ok::<_, usize>(()))
                .map(|_| ())
        } else {
            input
                .fold_governed(
                    policy(),
                    &mut source,
                    || Ok::<_, usize>(()),
                    |_, _| panic!("no output"),
                )
                .map(|_| ())
        };
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(88)))
        ));
        assert_eq!(sources, 2);
    }
}
