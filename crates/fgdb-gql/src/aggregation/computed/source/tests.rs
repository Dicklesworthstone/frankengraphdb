use super::*;
use crate::{
    GqlParameters, GraphSetProjection, GraphSetValue, GraphSymbol, GraphSymbolKind,
    PreparedGraphText,
};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
fn leaf(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .with_duplicates()
}
fn input(values: &[CanonicalScalar]) -> GqlQueryExecution<GraphValueRow> {
    GqlQueryExecution {
        value: values
            .iter()
            .cloned()
            .map(|value| GraphValueRow::from_owned_values(vec![GraphValue::Scalar(value)]))
            .collect(),
        rows: GqlExecutionStats {
            snapshot_records: values.len() as u64,
            result_rows: values.len() as u64,
        },
        evaluator: GlaExecutionStats {
            work_units: values.len() as u64,
            scratch_entries: values.len() as u64,
        },
    }
}
fn run(
    query: &PreparedGraphAggregate,
    values: &[CanonicalScalar],
    policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> ResultOf<GqlQueryExecution<GraphAggregateRow>, u8, usize> {
    let mut calls = 0;
    let result = query.execute_with_source_governed(
        policy,
        |pattern, remaining| {
            calls += 1;
            assert_eq!(pattern, query.input_pattern());
            assert_eq!(remaining.rows.max_result_rows(), None);
            Ok(input(values))
        },
        checkpoint,
    );
    if result.is_ok() {
        assert_eq!(calls, 1);
    }
    result
}

#[test]
fn original_occurrences_and_all_aggregate_functions_survive_the_source_boundary() {
    let pattern = leaf("MATCH (n) RETURN n.p AS p");
    let functions = [
        GraphAggregate::count_rows("rows"),
        GraphAggregate::count("count", 0),
        GraphAggregate::count_distinct("unique", 0),
        GraphAggregate::sum_int("sum", 0),
        GraphAggregate::sum_int_distinct("unique_sum", 0),
        GraphAggregate::min("min", 0),
        GraphAggregate::max("max", 0),
        GraphAggregate::average_int("avg", 0),
        GraphAggregate::average_int_distinct("unique_avg", 0),
        GraphAggregate::collect("items", 0),
        GraphAggregate::collect_distinct("unique_items", 0),
    ];
    let query = PreparedGraphAggregate::prepare(pattern, &[], &functions, 0, None).unwrap();
    for code in 0..243 {
        let mut code = code;
        let mut values = Vec::new();
        for _ in 0..5 {
            values.push(match code % 3 {
                0 => CanonicalScalar::Null,
                1 => CanonicalScalar::Int(-2),
                _ => CanonicalScalar::Int(3),
            });
            code /= 3;
        }
        let result = run(&query, &values, policy(), || Ok(())).unwrap();
        assert_eq!(result.rows.snapshot_records, 5);
        assert_eq!(result.rows.result_rows, 1);
        let row = &result.value[0];
        let nonnull: Vec<_> = values
            .iter()
            .filter_map(|value| match value {
                CanonicalScalar::Int(value) => Some(*value),
                _ => None,
            })
            .collect();
        assert_eq!(row.get(0).unwrap().as_count(), Some(5));
        assert_eq!(row.get(1).unwrap().as_count(), Some(nonnull.len() as u64));
        assert_eq!(
            row.get(3).unwrap().as_integer(),
            (!nonnull.is_empty()).then(|| nonnull.iter().map(|v| i128::from(*v)).sum())
        );
        assert_eq!(
            row.get(9).unwrap().as_value(),
            Some(&GraphValue::List(
                nonnull
                    .iter()
                    .map(|v| GraphValue::Scalar(CanonicalScalar::Int(*v)))
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ))
        );
        // Same original receiver, independently entered with admitted source.
        // This comparison covers exact averages, extrema and distinct states.
        let original = query
            .finish_materialized_governed::<u8, usize>(input(&values), policy(), || Ok(()))
            .unwrap();
        assert_eq!(result.value, original.value);
    }
}

#[test]
fn computed_columns_are_applied_before_grouping_without_rebuilding_the_definition() {
    let pattern = leaf("MATCH (n) RETURN n, n.p AS p");
    let query = PreparedGraphAggregate::prepare_projected(
        pattern,
        vec![
            GraphSetProjection::new("value", GraphSetValue::Column(1)),
            GraphSetProjection::new("id", GraphSetValue::Column(0)),
            GraphSetProjection::new("again", GraphSetValue::Column(1)),
        ],
        &[0],
        &[
            GraphAggregate::sum_int("total", 2),
            GraphAggregate::count_rows("rows"),
        ],
        0,
        None,
    )
    .unwrap();
    let frozen = query.canonical_bytes();
    let result = query
        .execute_with_source_governed(
            policy(),
            |_, _| {
                Ok::<_, GqlQueryError<u8, usize>>(GqlQueryExecution {
                    value: [4, 4, 9]
                        .into_iter()
                        .enumerate()
                        .map(|(id, value)| {
                            GraphValueRow::from_owned_values(vec![
                                GraphValue::Vertex(VId(id as u128)),
                                GraphValue::Scalar(CanonicalScalar::Int(value)),
                            ])
                        })
                        .collect(),
                    rows: GqlExecutionStats {
                        snapshot_records: 3,
                        result_rows: 3,
                    },
                    evaluator: GlaExecutionStats::default(),
                })
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(result.value.len(), 2);
    assert_eq!(
        result.value[0].keys(),
        &[GraphValue::Scalar(CanonicalScalar::Int(4))]
    );
    assert_eq!(result.value[0].get(0).unwrap().as_integer(), Some(8));
    assert_eq!(result.value[0].get(1).unwrap().as_count(), Some(2));
    assert_eq!(result.value[1].get(0).unwrap().as_integer(), Some(9));
    assert_eq!(query.canonical_bytes(), frozen);
}

#[test]
fn malformed_sources_refuse_even_when_no_groups_will_be_delivered() {
    let query = PreparedGraphAggregate::prepare(
        leaf("MATCH (n) RETURN n.p AS p"),
        &[],
        &[GraphAggregate::count_rows("rows")],
        0,
        Some(0),
    )
    .unwrap();
    for bad in 0..6 {
        let result = query.execute_with_source_governed(
            policy(),
            |_, _| {
                let mut data = input(&[CanonicalScalar::Int(1)]);
                match bad {
                    0 => data.rows.result_rows = 0,
                    1 => data.value[0] = GraphValueRow::unit(),
                    2 => {
                        data.value[0] =
                            GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1))])
                    }
                    3 => data.rows.snapshot_records = 1001,
                    4 => data.evaluator.work_units = 1_000_001,
                    _ => data.evaluator.scratch_entries = 1_000_001,
                }
                Ok::<_, GqlQueryError<u8, usize>>(data)
            },
            || Ok(()),
        );
        assert!(result.is_err(), "malformed source case {bad}");
    }
    let sum = PreparedGraphAggregate::prepare(
        leaf("MATCH (n) RETURN n.p AS p"),
        &[],
        &[GraphAggregate::sum_int("total", 0)],
        0,
        Some(0),
    )
    .unwrap();
    assert!(matches!(
        run(
            &sum,
            &[CanonicalScalar::ucs_basic_text("not-an-integer").unwrap()],
            policy(),
            || Ok(())
        ),
        Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerSum { .. }
        ))
    ));
    assert!(matches!(
        query.execute_with_source_governed(
            policy(),
            |_, _| { Err::<GqlQueryExecution<GraphValueRow>, _>(GqlQueryError::Source(73_u8)) },
            || Ok::<_, usize>(())
        ),
        Err(GqlQueryError::Source(GraphAggregateError::Source(73)))
    ));
}

#[test]
fn source_and_group_share_inclusive_quotas_and_all_checkpoints_are_retryable() {
    let query = PreparedGraphAggregate::prepare(
        leaf("MATCH (n) RETURN n.p AS p"),
        &[],
        &[GraphAggregate::count_rows("rows")],
        0,
        None,
    )
    .unwrap();
    let values = [
        CanonicalScalar::Int(3),
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(3),
    ];
    let mut calls = 0;
    let result = run(&query, &values, policy(), || {
        calls += 1;
        Ok(())
    })
    .unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        assert!(matches!(run(&query, &values, policy(), || {
            seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(GqlQueryError::Interrupted(at)) if at == stop));
    }
    let w = result.evaluator.work_units;
    let s = result.evaluator.scratch_entries;
    assert_eq!(
        run(&query, &values, GqlQueryPolicy::new(3, 1, w, s), || Ok(()))
            .unwrap()
            .value,
        result.value
    );
    for denied in [
        GqlQueryPolicy::new(2, 1, w, s),
        GqlQueryPolicy::new(3, 0, w, s),
        GqlQueryPolicy::new(3, 1, w - 1, s),
        GqlQueryPolicy::new(3, 1, w, s - 1),
    ] {
        assert!(run(&query, &values, denied, || Ok(())).is_err());
    }
}

#[test]
fn source_free_relations_never_invent_a_pattern_callback() {
    for (count, expected) in [(None, 1), (Some(0), 0)] {
        let query = PreparedGraphAggregate::prepare_set_relation(
            PreparedGraphSet::singleton().with_page(0, count),
            &[],
            &[GraphAggregate::count_rows("rows")],
            0,
            None,
        )
        .unwrap();
        let result = query
            .execute_with_source_governed(
                policy(),
                |_, _| -> Result<_, GqlQueryError<u8, usize>> {
                    panic!("no graph exists in this definition")
                },
                || Ok(()),
            )
            .unwrap();
        assert_eq!(result.rows.snapshot_records, 0);
        assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(expected));
    }
}
