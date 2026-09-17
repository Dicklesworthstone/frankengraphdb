//! Completed row stages feed the existing exact aggregate/result engine.
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{
    GraphColumn, GraphPatternBuilder, GraphValueOrder, GraphValueRow, PreparedGraphPattern,
};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphAggregateBuildError,
    GraphAggregateColumn, GraphAggregateError, GraphAggregateOrder, GraphExactAverage,
    GraphSetExecutionError, GraphSetOperation, GraphSetQuantifier, GraphSymbol, GraphSymbolKind,
    MAX_GRAPH_SET_DEPTH, PreparedGraphAggregate, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;

const P: PropertyKeyId = PropertyKeyId(1);
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000)
}
fn pattern() -> PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .prepare_values(
            &[
                GraphColumn::vertex("owner", "n"),
                GraphColumn::property("value", "n", P),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
}
fn run(
    query: &PreparedGraphAggregate,
    values: &[CanonicalScalar],
    policy: GqlQueryPolicy,
) -> fgdb_gql::GqlQueryExecution<fgdb_gql::GraphAggregateRow> {
    query
        .execute_governed(
            values.len() as u64,
            (0..values.len()).map(|i| VId(i as u128)),
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            policy,
            || Ok::<_, ()>(()),
        )
        .unwrap()
}
fn relation(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, |kind, name| match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

#[test]
fn all_functions_summarize_the_completed_page_not_the_original_graph() {
    let input: PreparedGraphSet = pattern().into();
    let input = input
        .with_order_by(&[GraphValueOrder::ascending(0)])
        .unwrap()
        .with_page(1, Some(5));
    let query = PreparedGraphAggregate::prepare_relation(
        input,
        &[],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 1),
            GraphAggregate::count_distinct("unique", 1),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::min("min", 1),
            GraphAggregate::max("max", 1),
            GraphAggregate::sum_int_distinct("unique_sum", 1),
            GraphAggregate::average_int("avg", 1),
            GraphAggregate::average_int_distinct("unique_avg", 1),
        ],
        0,
        None,
    )
    .unwrap();
    let values = [99, 5, 5, 0, 9, 13, -100].map(CanonicalScalar::Int);
    let mut values = values.to_vec();
    values[3] = CanonicalScalar::Null;
    // Seven admitted input rows and five intermediate results must not consume
    // the public one-row aggregate allowance.
    let result = run(
        &query,
        &values,
        GqlQueryPolicy::new(7, 1, 1_000_000, 1_000_000),
    );
    assert_eq!(result.rows.snapshot_records, 7);
    assert_eq!(result.rows.result_rows, 1);
    let output = result.value[0].values();
    assert_eq!(output[0].as_count(), Some(5));
    assert_eq!(output[1].as_count(), Some(4));
    assert_eq!(output[2].as_count(), Some(3));
    assert_eq!(output[3].as_integer(), Some(32));
    assert_eq!(
        output[4].as_value().unwrap().as_scalar(),
        Some(&CanonicalScalar::Int(5))
    );
    assert_eq!(
        output[5].as_value().unwrap().as_scalar(),
        Some(&CanonicalScalar::Int(13))
    );
    assert_eq!(output[6].as_integer(), Some(27));
    assert_eq!(output[7].as_average(), GraphExactAverage::new(32, 4));
    assert_eq!(output[8].as_average(), GraphExactAverage::new(27, 3));
    assert_eq!(query.input_pattern(), &pattern());
    assert!(query.input_relation().is_some());
    assert!(query.input_projection().is_none());
}

#[test]
fn native_row_stages_and_distinct_feed_independent_summary_oracles() {
    for mut code in 0..81 {
        let mut input = Vec::new();
        for _ in 0..4 {
            input.push(match code % 3 {
                0 => CanonicalScalar::Null,
                1 => CanonicalScalar::Int(-2),
                _ => CanonicalScalar::Int(5),
            });
            code /= 3;
        }
        for distinct in [false, true] {
            let quantifier = if distinct { "DISTINCT " } else { "" };
            let text = format!(
                "MATCH (n) WITH n AS owner, n.p AS x ORDER BY owner SKIP 1 LIMIT 2 WITH {quantifier}x WHERE x IS NOT NULL RETURN x"
            );
            let query = PreparedGraphAggregate::prepare_relation(
                relation(&text),
                &[],
                &[
                    GraphAggregate::count_rows("n"),
                    GraphAggregate::sum_int("s", 0),
                    GraphAggregate::average_int("a", 0),
                ],
                0,
                None,
            )
            .unwrap();
            let mut expected: Vec<i64> = input[1..3]
                .iter()
                .filter_map(|v| match v {
                    CanonicalScalar::Int(value) => Some(*value),
                    _ => None,
                })
                .collect();
            expected.sort();
            if distinct {
                expected.dedup();
            }
            let rows = run(&query, &input, wide()).value;
            let values = rows[0].values();
            assert_eq!(values[0].as_count(), Some(expected.len() as u64));
            if expected.is_empty() {
                assert!(values[1].is_null());
                assert!(values[2].is_null());
            } else {
                let sum: i128 = expected.iter().map(|value| i128::from(*value)).sum();
                assert_eq!(values[1].as_integer(), Some(sum));
                assert_eq!(
                    values[2].as_average(),
                    GraphExactAverage::new(sum, expected.len() as u64)
                );
            }
        }
    }
}

#[test]
fn exact_domains_and_keyless_versus_keyed_empty_input_remain_unchanged() {
    let input: PreparedGraphSet = pattern().into();
    let query = PreparedGraphAggregate::prepare_relation(
        input.clone(),
        &[],
        &[
            GraphAggregate::sum_int("s", 1),
            GraphAggregate::average_int("a", 1),
        ],
        0,
        None,
    )
    .unwrap();
    let values = [i64::MAX, i64::MAX, i64::MIN].map(CanonicalScalar::Int);
    let result = run(&query, &values, wide());
    let row = &result.value[0];
    let sum = i128::from(i64::MAX) * 2 + i128::from(i64::MIN);
    assert_eq!(row.values()[0].as_integer(), Some(sum));
    assert_eq!(row.values()[1].as_average(), GraphExactAverage::new(sum, 3));
    let empty = run(&query, &[], wide());
    assert!(empty.value[0].values().iter().all(|value| value.is_null()));
    let grouped = PreparedGraphAggregate::prepare_relation(
        input,
        &[0],
        &[GraphAggregate::count_rows("n")],
        0,
        None,
    )
    .unwrap();
    assert!(run(&grouped, &[], wide()).value.is_empty());
}

#[test]
fn result_order_hidden_keys_and_distinct_stay_in_the_existing_result_engine() {
    let source = pattern();
    let input: PreparedGraphSet = source.clone().into();
    let make = |query: PreparedGraphAggregate| {
        query
            .with_key_output_columns(&[])
            .unwrap()
            .with_distinct_output(true)
            .with_result_clauses(
                &[],
                &[GraphAggregateOrder::descending(
                    GraphAggregateColumn::Aggregate(0),
                )],
            )
            .unwrap()
    };
    let ordinary = make(
        PreparedGraphAggregate::prepare(
            source,
            &[0],
            &[GraphAggregate::sum_int("s", 1)],
            1,
            Some(2),
        )
        .unwrap(),
    );
    let pipeline = make(
        PreparedGraphAggregate::prepare_relation(
            input,
            &[0],
            &[GraphAggregate::sum_int("s", 1)],
            1,
            Some(2),
        )
        .unwrap(),
    );
    let input = [1, 5, 5, 9].map(CanonicalScalar::Int);
    assert_eq!(
        run(&pipeline, &input, wide()).value,
        run(&ordinary, &input, wide()).value
    );
    assert_ne!(pipeline.canonical_bytes(), ordinary.canonical_bytes());
    assert_eq!(
        pipeline.clone().canonical_bytes(),
        pipeline.canonical_bytes()
    );
    assert!(!format!("{pipeline:?}").contains("owner"));
}

#[test]
fn one_allowance_covers_input_grouping_and_every_interruption_boundary() {
    let query = PreparedGraphAggregate::prepare_relation(
        relation("MATCH (n) WITH n.p + 1 AS x WHERE x > 0 RETURN x"),
        &[0],
        &[GraphAggregate::count_rows("n")],
        0,
        None,
    )
    .unwrap();
    let input = [1, 1, 2].map(CanonicalScalar::Int);
    let calls = Cell::new(0);
    let measure = query
        .execute_governed(
            3,
            [VId(0), VId(1), VId(2)],
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&input[vid.0 as usize])),
            wide(),
            || {
                calls.set(calls.get() + 1);
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    let caps = [
        measure.rows.snapshot_records,
        measure.rows.result_rows,
        measure.evaluator.work_units,
        measure.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(
            &query,
            &input,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])
        ),
        measure
    );
    for dimension in 0..4 {
        let mut cap = caps;
        cap[dimension] -= 1;
        assert!(
            query
                .execute_governed(
                    3,
                    [VId(0), VId(1), VId(2)],
                    [],
                    |_, _| Ok::<_, ()>(true),
                    |vid, _| Ok(Some(&input[vid.0 as usize])),
                    GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3]),
                    || Ok::<_, ()>(())
                )
                .is_err()
        );
    }
    for stop in 1..=calls.get() {
        let mut at = 0;
        let result = query.execute_governed(
            3,
            [VId(0), VId(1), VId(2)],
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&input[vid.0 as usize])),
            wide(),
            || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn late_pipeline_source_and_numeric_failures_are_not_hidden_by_zero_output() {
    let query = PreparedGraphAggregate::prepare_relation(
        relation("MATCH (n) WITH n.p AS x RETURN 10 / x AS y"),
        &[],
        &[GraphAggregate::count_rows("n")],
        0,
        Some(0),
    )
    .unwrap();
    let input = [CanonicalScalar::Int(2), CanonicalScalar::Int(0)];
    let result = query.execute_governed(
        2,
        [VId(0), VId(1)],
        [],
        |_, _| Ok::<_, &str>(true),
        |vid, _| Ok(Some(&input[vid.0 as usize])),
        wide(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
            GraphSetExecutionError::Projection { .. }
        )))
    ));
    let error = query.execute_governed(
        2,
        [VId(0), VId(1)],
        [],
        |_, _| Ok(true),
        |vid, _| {
            if vid == VId(1) {
                Err("late read")
            } else {
                Ok(Some(&input[0]))
            }
        },
        wide(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        error,
        Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
            GraphSetExecutionError::Source("late read")
        )))
    ));
    let safe = PreparedGraphAggregate::prepare_relation(
        relation("MATCH (n) WITH n.p AS x WHERE x <> 0 RETURN 10 / x AS y"),
        &[],
        &[GraphAggregate::sum_int("s", 0)],
        0,
        None,
    )
    .unwrap();
    assert_eq!(
        run(&safe, &input, wide()).value[0].values()[0].as_integer(),
        Some(5)
    );
}

#[test]
fn multi_source_schema_and_total_depth_refuse_before_any_execution() {
    let leaf: PreparedGraphSet = pattern().into();
    let binary = leaf
        .clone()
        .combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            leaf.clone(),
        )
        .unwrap();
    assert!(matches!(
        PreparedGraphAggregate::prepare_relation(
            binary,
            &[],
            &[GraphAggregate::count_rows("n")],
            0,
            Some(0)
        ),
        Err(GraphAggregateBuildError::RequiresSingleGraphSource)
    ));
    assert!(matches!(
        PreparedGraphAggregate::prepare_relation(
            leaf.clone(),
            &[2],
            &[GraphAggregate::count_rows("n")],
            0,
            None
        ),
        Err(GraphAggregateBuildError::UnknownColumn { column: 2 })
    ));
    assert!(matches!(
        PreparedGraphAggregate::prepare_relation(
            leaf.clone(),
            &[0, 0],
            &[GraphAggregate::count_rows("n")],
            0,
            None
        ),
        Err(GraphAggregateBuildError::DuplicateKey { column: 0 })
    ));
    let mut deepest = leaf;
    for _ in 1..MAX_GRAPH_SET_DEPTH {
        deepest = deepest.nested().unwrap();
    }
    assert!(matches!(
        PreparedGraphAggregate::prepare_relation(
            deepest,
            &[],
            &[GraphAggregate::count_rows("n")],
            0,
            None
        ),
        Err(GraphAggregateBuildError::RelationalInput(_))
    ));
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let distinct = builder
        .prepare_values(&[GraphColumn::vertex("n", "n")], 0, None)
        .unwrap();
    assert!(matches!(
        PreparedGraphAggregate::prepare(
            distinct,
            &[],
            &[GraphAggregate::count_rows("count")],
            0,
            None
        ),
        Err(GraphAggregateBuildError::RequiresUnpaginatedAll)
    ));
}
