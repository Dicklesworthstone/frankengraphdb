//! Ordering projected aggregates must use result positions and exact cells.
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{
    GraphColumn, GraphPatternBuilder, GraphValue, GraphValueOrder, IntegerComparison,
};
use fgdb_gql::spill_aggregate::{AsyncSpillAggregatePlan, SpillAggregatePlan};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregate, GraphAggregateBuildError, GraphAggregateColumn, GraphAggregateError,
    GraphAggregateFilter, GraphAggregateOrder, GraphAggregateRow, GraphAggregateTest,
    GraphSetProjection, GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000)
}

// Property sources hand the aggregate stored property values (fgdb-k3nub).
fn data(values: &[(i64, Option<i64>)]) -> Vec<[CanonicalPropertyValue; 2]> {
    values
        .iter()
        .map(|(group, value)| {
            [
                CanonicalScalar::Int(*group),
                value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
            ]
            .map(CanonicalPropertyValue::Scalar)
        })
        .collect()
}

fn execute<C>(
    query: &PreparedGraphAggregate,
    values: &[[CanonicalPropertyValue; 2]],
    policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<()>, C>> {
    query.execute_governed(
        values.len() as u64,
        (0..values.len()).map(|at| VId(at as u128 + 1)),
        [],
        |_, _| Ok::<_, ()>(true),
        |vid, key| {
            let row = &values[vid.0 as usize - 1];
            Ok(match key {
                PropertyKeyId(1) => Some(&row[0]),
                PropertyKeyId(2) => Some(&row[1]),
                _ => None,
            })
        },
        policy,
        checkpoint,
    )
}

fn native(offset: u64, count: Option<u64>) -> PreparedGraphAggregate {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder
        .prepare_values(
            &[
                GraphColumn::property("bucket", "n", PropertyKeyId(1)),
                GraphColumn::property("amount", "n", PropertyKeyId(2)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    PreparedGraphAggregate::prepare(
        input,
        &[0],
        &[
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::average_int("average", 1),
            GraphAggregate::count_rows("count"),
        ],
        offset,
        count,
    )
    .unwrap()
}

fn projection(query: PreparedGraphAggregate) -> PreparedGraphAggregate {
    query
        .with_output_projection(vec![
            GraphSetProjection::new("score", GraphSetValue::Column(2)),
            GraphSetProjection::new("bucket", GraphSetValue::Column(0)),
            GraphSetProjection::new("total", GraphSetValue::Column(1)),
        ])
        .unwrap()
}

fn order(column: usize, descending: bool, nulls_first: bool) -> GraphValueOrder {
    GraphValueOrder {
        column,
        descending,
        nulls_first,
    }
}

fn bucket(row: &GraphAggregateRow, column: usize) -> i64 {
    match row.values()[column]
        .as_value()
        .and_then(GraphValue::as_scalar)
    {
        Some(CanonicalScalar::Int(value)) => *value,
        other => panic!("expected bucket, got {other:?}"),
    }
}

#[test]
fn native_projected_positions_are_not_evaluation_positions() {
    // SUM ranks bucket 1 first, AVG ranks bucket 2 first. The projected score
    // is at zero, unlike both evaluation summaries. No ordinary-row cast.
    let values = data(&[(1, Some(10)), (1, Some(10)), (1, Some(10)), (2, Some(20))]);
    let query = projection(native(0, None))
        .with_projected_order_by(&[order(0, true, false)])
        .unwrap();
    let rows = execute(&query, &values, policy(), || Ok::<_, ()>(()))
        .unwrap()
        .value;
    assert_eq!(
        rows.iter().map(|row| bucket(row, 1)).collect::<Vec<_>>(),
        [2, 1]
    );
    assert!(
        rows.iter()
            .all(|row| row.keys().is_empty() && row.values().len() == 3)
    );
    assert_eq!(rows[0].values()[2].as_integer(), Some(20));
    assert_eq!(rows[1].values()[2].as_integer(), Some(30));
    assert!(query.ordering().is_empty());
    assert_eq!(query.projected_ordering(), &[order(0, true, false)]);
}

#[test]
fn projected_order_retains_wide_sums_and_fractional_averages() {
    let m = i64::MAX;
    let values = data(&[
        (1, Some(m)),
        (1, Some(m - 2)),
        (2, Some(m)),
        (2, Some(m - 1)),
        (3, None),
    ]);
    let query = projection(native(0, None))
        .with_projected_order_by(&[order(0, true, true)])
        .unwrap();
    let rows = execute(&query, &values, policy(), || Ok::<_, ()>(()))
        .unwrap()
        .value;
    assert_eq!(
        rows.iter().map(|row| bucket(row, 1)).collect::<Vec<_>>(),
        [3, 2, 1]
    );
    assert!(rows[0].values()[0].is_null());
    let average = rows[1].values()[0].as_average().unwrap();
    assert_eq!(average.numerator(), i128::from(m) * 2 - 1);
    assert_eq!(average.denominator(), 2);
    assert_eq!(
        rows[1].values()[2].as_integer(),
        Some(i128::from(m) * 2 - 1)
    );
    let average = rows[2].values()[0].as_average().unwrap();
    assert_eq!(
        (average.numerator(), average.denominator()),
        (i128::from(m) - 1, 1)
    );
}

#[test]
fn null_placement_is_independent_and_equal_scores_use_ascending_group_keys() {
    let values = data(&[(2, Some(1)), (1, Some(1)), (3, None), (4, Some(2))]);
    for (descending, nulls_first, expected) in [
        (false, false, [1, 2, 4, 3]),
        (false, true, [3, 1, 2, 4]),
        (true, false, [4, 1, 2, 3]),
        (true, true, [3, 4, 1, 2]),
    ] {
        let query = projection(native(0, None))
            .with_projected_order_by(&[order(0, descending, nulls_first)])
            .unwrap();
        let rows = execute(&query, &values, policy(), || Ok::<_, ()>(()))
            .unwrap()
            .value;
        assert_eq!(
            rows.iter().map(|row| bucket(row, 1)).collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn projected_distinct_precedes_page_without_deduplicating_matches() {
    let values = data(&[
        (3, Some(2)),
        (1, Some(2)),
        (1, Some(2)),
        (2, Some(4)),
        (4, Some(1)),
    ]);
    let query = native(1, Some(1))
        .with_output_projection(vec![GraphSetProjection::new(
            "score",
            GraphSetValue::Column(1),
        )])
        .unwrap()
        .with_projected_order_by(&[order(0, true, false)])
        .unwrap();
    let bag = execute(&query, &values, policy(), || Ok::<_, ()>(()))
        .unwrap()
        .value;
    assert_eq!(bag.len(), 1);
    assert_eq!(bag[0].values()[0].as_integer(), Some(4));
    let rows = execute(&query.with_distinct_output(true), &values, policy(), || {
        Ok::<_, ()>(())
    })
    .unwrap()
    .value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values()[0].as_integer(), Some(2));
}

#[test]
fn projected_builder_preserves_having_and_validates_replacement_orders() {
    let filter = GraphAggregateFilter {
        column: GraphAggregateColumn::Aggregate(0),
        test: GraphAggregateTest::Integer {
            comparison: IntegerComparison::Greater,
            value: 0,
        },
    };
    let raw = native(0, None)
        .with_result_clauses(
            &[filter],
            &[GraphAggregateOrder::descending(
                GraphAggregateColumn::GroupKey(0),
            )],
        )
        .unwrap();
    assert_eq!(
        raw.clone()
            .with_projected_order_by(&[order(0, false, false)])
            .unwrap_err(),
        GraphAggregateBuildError::RequiresOutputProjection
    );
    let query = projection(raw)
        .with_projected_order_by(&[order(0, true, false)])
        .unwrap();
    assert_eq!(query.having(), &[filter]);
    let values = data(&[
        (1, Some(10)),
        (1, Some(10)),
        (1, Some(10)),
        (2, Some(20)),
        (3, Some(-1)),
    ]);
    let rows = execute(&query, &values, policy(), || Ok::<_, ()>(()))
        .unwrap()
        .value;
    assert_eq!(
        rows.iter().map(|row| bucket(row, 1)).collect::<Vec<_>>(),
        [2, 1]
    );
    assert_eq!(
        query
            .clone()
            .with_projected_order_by(&[order(0, false, false), order(3, false, false),])
            .unwrap_err(),
        GraphAggregateBuildError::UnknownProjectedOrderColumn { column: 3 }
    );
    assert_eq!(
        query
            .clone()
            .with_projected_order_by(&[order(0, false, false), order(0, true, true),])
            .unwrap_err(),
        GraphAggregateBuildError::DuplicateProjectedOrder { column: 0 }
    );
    let later = query
        .clone()
        .with_projected_order_by(&[order(2, false, false)])
        .unwrap();
    assert_eq!(
        later
            .with_output_projection(vec![GraphSetProjection::new(
                "only",
                GraphSetValue::Column(1)
            ),])
            .unwrap_err(),
        GraphAggregateBuildError::UnknownProjectedOrderColumn { column: 2 }
    );
    let cleared = query.clone().with_projected_order_by(&[]).unwrap();
    assert!(cleared.ordering().is_empty() && cleared.projected_ordering().is_empty());
    assert_eq!(cleared.having(), &[filter]);
    let replaced = query
        .with_result_clauses(
            &[],
            &[GraphAggregateOrder::ascending(
                GraphAggregateColumn::GroupKey(0),
            )],
        )
        .unwrap();
    assert!(replaced.projected_ordering().is_empty());
    assert_eq!(replaced.ordering().len(), 1);
}

#[test]
fn native_transcript_pins_projected_order_and_unaffected_profiles_are_unchanged() {
    let raw = native(0, None);
    assert_eq!(
        raw.canonical_bytes(),
        raw.clone()
            .with_projected_order_by(&[])
            .unwrap()
            .canonical_bytes()
    );
    let base = projection(raw);
    let asc = base
        .clone()
        .with_projected_order_by(&[order(0, false, false)])
        .unwrap();
    let desc = base
        .clone()
        .with_projected_order_by(&[order(0, true, false)])
        .unwrap();
    let nulls = base
        .clone()
        .with_projected_order_by(&[order(0, false, true)])
        .unwrap();
    let column = base
        .clone()
        .with_projected_order_by(&[order(1, false, false)])
        .unwrap();
    let identities = [
        base.canonical_bytes(),
        asc.canonical_bytes(),
        desc.canonical_bytes(),
        nulls.canonical_bytes(),
        column.canonical_bytes(),
    ];
    for i in 0..identities.len() {
        for j in 0..i {
            assert_ne!(identities[i], identities[j]);
        }
    }
    assert_eq!(asc.canonical_bytes(), asc.clone().canonical_bytes());
}

#[test]
fn every_checkpoint_failure_is_retryable_and_zero_work_remains_a_refusal() {
    let query = projection(native(0, Some(1)))
        .with_projected_order_by(&[order(0, true, false)])
        .unwrap();
    let values = data(&[(1, Some(30)), (2, Some(20)), (3, Some(10))]);
    let mut checkpoints = 0;
    let expected = execute(&query, &values, policy(), || {
        checkpoints += 1;
        Ok::<_, usize>(())
    })
    .unwrap()
    .value;
    assert_eq!(expected.len(), 1);
    for stop in 0..checkpoints {
        let mut seen = 0;
        let result = execute(&query, &values, policy(), || {
            let at = seen;
            seen += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop + 1);
    }
    assert_eq!(
        execute(&query, &values, policy(), || Ok::<_, ()>(()))
            .unwrap()
            .value,
        expected
    );
    assert!(matches!(
        execute(
            &query,
            &values,
            GqlQueryPolicy::new(100, 100, 0, 1_000_000),
            || Ok::<_, ()>(())
        ),
        Err(GqlQueryError::Evaluator(_))
    ));
}

#[test]
fn spill_and_maintained_sources_keep_projected_rank_with_checked_companions() {
    let query = projection(native(0, None))
        .with_projected_order_by(&[order(0, true, false)])
        .unwrap();
    let before = query.canonical_bytes();
    assert!(query.has_incremental_ranking());
    let source = query.incremental_source_definition().unwrap();
    assert!(!source.has_incremental_output_transform());
    assert!(source.projected_ordering().is_empty());
    assert!(source.output_projection().is_none());
    assert_eq!(source.incremental_result_window(), (0, None));
    assert_eq!(source.input_pattern(), query.input_pattern());
    assert_eq!(
        source.evaluation_key_columns(),
        query.evaluation_key_columns()
    );
    assert_eq!(
        source.evaluation_aggregate_columns(),
        query.evaluation_aggregate_columns()
    );
    assert_eq!(query.canonical_bytes(), before);
    let synchronous = SpillAggregatePlan::compile(&query).unwrap();
    let asynchronous = AsyncSpillAggregatePlan::compile(&query).unwrap();
    let values = data(&[(1, Some(5)), (2, Some(9))]);
    let raw = execute(&source, &values, policy(), || Ok::<_, ()>(()))
        .unwrap()
        .value;
    for definition in [synchronous.definition(), asynchronous.definition()] {
        assert!(definition.has_projected_ordering());
        assert!(definition.has_ordering());
        assert!(definition.ordering().is_empty());
        assert!(definition.has_precomputed_output());
        let mut control = |_| Ok::<_, GqlQueryError<GraphAggregateError<()>, ()>>(());
        let a = definition
            .project_output(raw[0].clone(), &mut control)
            .unwrap();
        let b = definition
            .project_output(raw[1].clone(), &mut control)
            .unwrap();
        assert_eq!(
            query
                .project_incremental_output(&raw[0], &mut |_| Ok::<(), ()>(()))
                .unwrap(),
            Some(a.clone())
        );
        assert_eq!(
            query
                .project_incremental_output(&raw[1], &mut |_| Ok::<(), ()>(()))
                .unwrap(),
            Some(b.clone())
        );
        assert_eq!(
            definition
                .compare_precomputed_output(&raw[0], &a, &raw[1], &b, &mut control)
                .unwrap(),
            std::cmp::Ordering::Greater
        );
        // A legacy raw-only consumer must fail, not silently use group order.
        assert!(matches!(
            definition.compare_output(&raw[0], &raw[1], &mut control),
            Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput
            ))
        ));
        // The decisive first score cannot hide a malformed companion schema.
        assert!(matches!(
            definition.compare_precomputed_output(&raw[0], &a, &raw[1], &raw[1], &mut control),
            Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput
            ))
        ));
        // A final projected tuple is not a complete upstream group.
        assert!(
            query
                .project_incremental_output(&a, &mut |_| Ok::<(), ()>(()))
                .unwrap()
                .is_none()
        );
    }
}

fn catalog(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "g") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}

fn text(statement: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(statement, catalog)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

#[test]
fn computed_alias_and_returned_group_key_sort_through_text() {
    let query = text(
        "MATCH (n) RETURN n.g AS bucket, -sum(n.p) AS score ORDER BY score DESCENDING, bucket ASCENDING",
    );
    let values = data(&[
        (1, Some(10)),
        (1, Some(10)),
        (1, Some(10)),
        (3, Some(20)),
        (2, Some(20)),
    ]);
    let rows = execute(&query, &values, policy(), || Ok::<_, ()>(()))
        .unwrap()
        .value;
    assert_eq!(
        rows.iter().map(|row| bucket(row, 0)).collect::<Vec<_>>(),
        [2, 3, 1]
    );
    assert_eq!(rows[0].values()[1].as_integer(), Some(-20));
    assert!(query.ordering().is_empty());
    assert_eq!(
        query.projected_ordering(),
        &[order(1, true, false), order(0, false, false)]
    );
}

#[test]
fn mixed_returned_summary_and_computed_alias_keep_original_column_mapping() {
    let query = text(
        "MATCH (n) RETURN n.g AS bucket, sum(n.p) AS total, -count(*) AS score ORDER BY score, total DESC",
    );
    let values = data(&[
        (3, Some(3)),
        (3, Some(3)),
        (2, Some(5)),
        (2, Some(5)),
        (1, Some(99)),
    ]);
    let rows = execute(&query, &values, policy(), || Ok::<_, ()>(()))
        .unwrap()
        .value;
    assert_eq!(
        rows.iter().map(|row| bucket(row, 0)).collect::<Vec<_>>(),
        [2, 3, 1]
    );
    assert_eq!(
        query.projected_ordering(),
        &[order(2, false, false), order(1, true, false)]
    );
    let ordinary = text("MATCH (n) RETURN sum(n.p) AS total, -sum(n.p) AS score ORDER BY total");
    assert!(ordinary.projected_ordering().is_empty());
    assert_eq!(ordinary.ordering().len(), 1);
}

#[test]
fn prepared_alias_order_rebinds_once_and_retains_plan_identity() {
    let statement =
        "MATCH (n) RETURN n.g AS bucket, sum(n.p) * $scale AS score ORDER BY score DESC";
    let template = PreparedGraphAggregateText::prepare_with_parameter_types(
        statement,
        &[("scale", GqlParameterType::Int64)],
        catalog,
    )
    .unwrap();
    assert_eq!(template.parameter_schema().len(), 1);
    assert_eq!(template.parameter_schema()[0].occurrences, 1);
    let original = template.canonical_template_bytes();
    let a = template
        .bind_parameters(&GqlParameters::new().with_int64("scale", 1).unwrap())
        .unwrap();
    let b = template
        .bind_parameters(&GqlParameters::new().with_int64("scale", -1).unwrap())
        .unwrap();
    assert_ne!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(template.canonical_template_bytes(), original);
    assert!(template.template_operators().contains(&"OrderByAggregate"));
    let values = data(&[(1, Some(30)), (2, Some(20))]);
    let forward = execute(&a, &values, policy(), || Ok::<_, ()>(()))
        .unwrap()
        .value;
    let reverse = execute(&b, &values, policy(), || Ok::<_, ()>(()))
        .unwrap()
        .value;
    assert_eq!(
        forward.iter().map(|row| bucket(row, 0)).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        reverse.iter().map(|row| bucket(row, 0)).collect::<Vec<_>>(),
        [2, 1]
    );
    let ascending = PreparedGraphAggregateText::prepare_with_parameter_types(
        "MATCH (n) RETURN n.g AS bucket, sum(n.p) * $scale AS score ORDER BY score ASC",
        &[("scale", GqlParameterType::Int64)],
        catalog,
    )
    .unwrap();
    assert_ne!(
        template.canonical_template_bytes(),
        ascending.canonical_template_bytes()
    );
}

#[test]
fn computed_alias_sort_keeps_distinct_page_and_limit_zero_failures() {
    let query = text(
        "MATCH (n) RETURN DISTINCT -sum(n.p) AS score GROUP BY n.g ORDER BY score DESC SKIP 1 LIMIT 1",
    );
    let values = data(&[(1, Some(4)), (2, Some(4)), (3, Some(2)), (4, Some(1))]);
    let rows = execute(&query, &values, policy(), || Ok::<_, ()>(()))
        .unwrap()
        .value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values()[0].as_integer(), Some(-2));
    let broken = text("MATCH (n) RETURN sum(n.p) / 0 AS score GROUP BY n.g ORDER BY score LIMIT 0");
    assert!(matches!(
        execute(&broken, &values, policy(), || Ok::<_, ()>(())),
        Err(GqlQueryError::Source(
            GraphAggregateError::OutputExpression { .. }
        ))
    ));
}

#[test]
fn ambiguous_or_hidden_mixed_order_refuses_before_catalog_access() {
    for statement in [
        "MATCH (n) RETURN -sum(n.p) AS score GROUP BY n.g ORDER BY score, n.g",
        "MATCH (n) RETURN -sum(n.p) AS score GROUP BY n.g ORDER BY score, sum(n.p)",
        "MATCH (n) RETURN -sum(n.p) AS score ORDER BY score, score",
        "MATCH (n) RETURN -sum(n.p) AS score ORDER BY missing",
        "MATCH (n) RETURN -sum(n.p) AS score ORDER BY score + 1",
    ] {
        let calls = std::cell::Cell::new(0);
        let result = PreparedGraphAggregateText::prepare(statement, |kind, name| {
            calls.set(calls.get() + 1);
            catalog(kind, name)
        });
        assert!(result.is_err(), "{statement}");
        assert_eq!(calls.get(), 0, "{statement}");
    }
}
