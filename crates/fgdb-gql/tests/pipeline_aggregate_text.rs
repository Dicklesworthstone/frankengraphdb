//! Native WITH summaries retain row-stage boundaries and exact result domains.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValueOrder, IntegerComparison};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregate, GraphAggregateColumn, GraphAggregateError, GraphAggregateOrder,
    GraphAggregateRow, GraphAggregateTextSlot, GraphExactAverage, GraphHavingExpression,
    GraphHavingOp, GraphHavingOperand, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp,
    GraphPatternTextErrorKind, GraphPipelineAggregateTextErrorKind, GraphSetExecutionError,
    GraphSetProjection, GraphSetQuantifier, GraphSetTextErrorKind, GraphSetValue, GraphSymbol,
    GraphSymbolKind, PreparedGraphAggregate, PreparedGraphPipelineAggregateText, PreparedGraphSet,
};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeSet;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000)
}
fn query(text: &str) -> PreparedGraphAggregate {
    PreparedGraphPipelineAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn run(
    query: &PreparedGraphAggregate,
    input: &[CanonicalScalar],
    policy: GqlQueryPolicy,
) -> GqlQueryExecution<GraphAggregateRow> {
    query
        .execute_governed(
            input.len() as u64,
            (0..input.len()).map(|at| VId(at as u128)),
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, key| Ok((key == P).then(|| &input[vid.0 as usize])),
            policy,
            || Ok::<_, ()>(()),
        )
        .unwrap()
}

#[test]
fn native_pipeline_matches_typed_ir_and_retains_written_result_order() {
    let text = "MATCH (n) WITH n AS owner,n.p AS x ORDER BY owner SKIP $off LIMIT $take WITH owner,x+$bump AS score RETURN SUM(score) AS total,owner AS person,COUNT(*) AS copies GROUP BY owner HAVING total >= $min ORDER BY total DESC LIMIT $groups";
    let mut calls = BTreeSet::new();
    let template = PreparedGraphPipelineAggregateText::prepare(text, |kind, name| {
        assert!(calls.insert((kind, name.to_owned())));
        symbols(kind, name)
    })
    .unwrap();
    let args = GqlParameters::new()
        .with_uint64("off", 1)
        .unwrap()
        .with_uint64("take", 3)
        .unwrap()
        .with_int64("bump", 1)
        .unwrap()
        .with_int64("min", 3)
        .unwrap()
        .with_uint64("groups", 2)
        .unwrap();
    let native = template.bind_parameters(&args).unwrap();
    assert_eq!(template.statement(), text);
    assert_eq!(template.columns(), &["total", "person", "copies"]);
    assert_eq!(
        template.output_slots(),
        &[
            GraphAggregateTextSlot::Aggregate(0),
            GraphAggregateTextSlot::GroupKey(0),
            GraphAggregateTextSlot::Aggregate(1),
        ]
    );
    assert_eq!(template.parameter_schema().len(), 5);
    assert_eq!(calls.len(), 1);
    assert_eq!(native, template.bind_parameters(&args).unwrap());
    assert!(!format!("{template:?}").contains("$bump"));

    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input: PreparedGraphSet = builder
        .prepare_values(
            &[
                GraphColumn::vertex("_return_input_0", "n"),
                GraphColumn::property("_return_input_1", "n", P),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into();
    let input = input
        .project(
            vec![
                GraphSetProjection::new("owner", GraphSetValue::Column(0)),
                GraphSetProjection::new("x", GraphSetValue::Column(1)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_order_by(&[GraphValueOrder::ascending(0)])
        .unwrap()
        .with_page(1, Some(3));
    let plus = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Column(1),
        GraphIntegerOp::Literal(Some(1)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Add),
    ])
    .unwrap();
    let input = input
        .project(
            vec![
                GraphSetProjection::new("owner", GraphSetValue::Column(0)),
                GraphSetProjection::new("score", GraphSetValue::Integer(plus)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let having = GraphHavingExpression::prepare(&[GraphHavingOp::Compare {
        left: GraphHavingOperand::Column(GraphAggregateColumn::Aggregate(0)),
        comparison: IntegerComparison::GreaterOrEqual,
        right: GraphHavingOperand::Integer(3),
    }])
    .unwrap();
    let typed = PreparedGraphAggregate::prepare_relation(
        input,
        &[0],
        &[
            GraphAggregate::sum_int("total", 1),
            GraphAggregate::count_rows("copies"),
        ],
        0,
        Some(2),
    )
    .unwrap()
    .with_result_clauses(
        &[],
        &[GraphAggregateOrder::descending(
            GraphAggregateColumn::Aggregate(0),
        )],
    )
    .unwrap()
    .with_having_expression(&having)
    .unwrap();
    assert_eq!(native.canonical_bytes(), typed.canonical_bytes());
    let values = [5, 2, 2, 9, 100].map(CanonicalScalar::Int);
    let result = run(&native, &values, wide());
    assert_eq!(result, run(&typed, &values, wide()));
    assert_eq!(
        result
            .value
            .iter()
            .map(|row| (
                row.keys()[0].as_vertex().unwrap(),
                row.values()[0].as_integer().unwrap(),
                row.values()[1].as_count().unwrap(),
            ))
            .collect::<Vec<_>>(),
        vec![(VId(3), 10, 1), (VId(1), 3, 1)]
    );
}

#[test]
fn all_numeric_functions_and_boolean_having_preserve_exact_fractions() {
    let text = "MATCH (n) WITH n.p AS x RETURN COUNT(*) AS rows,COUNT(x) AS nn,COUNT(DISTINCT x) AS unique,SUM(x) AS total,SUM(DISTINCT x) AS unique_total,AVG(x) AS mean,AVG(DISTINCT x) AS unique_mean,MIN(x) AS minimum,MAX(x) AS maximum HAVING NOT (mean <= $threshold) AND mean < 7 AND (nn=3 OR FALSE)";
    let template = PreparedGraphPipelineAggregateText::prepare(text, symbols).unwrap();
    let bound = template
        .bind_parameters(&GqlParameters::new().with_int64("threshold", 6).unwrap())
        .unwrap();
    let input = [
        CanonicalScalar::Int(5),
        CanonicalScalar::Int(5),
        CanonicalScalar::Null,
        CanonicalScalar::Int(9),
    ];
    let result = run(&bound, &input, wide());
    assert_eq!(result.value.len(), 1);
    let values = result.value[0].values();
    assert_eq!(values[0].as_count(), Some(4));
    assert_eq!(values[1].as_count(), Some(3));
    assert_eq!(values[2].as_count(), Some(2));
    assert_eq!(values[3].as_integer(), Some(19));
    assert_eq!(values[4].as_integer(), Some(14));
    assert_eq!(values[5].as_average(), GraphExactAverage::new(19, 3));
    assert_eq!(values[6].as_average(), GraphExactAverage::new(7, 1));
    assert_eq!(
        values[7].as_value().unwrap().as_scalar(),
        Some(&CanonicalScalar::Int(5))
    );
    assert_eq!(
        values[8].as_value().unwrap().as_scalar(),
        Some(&CanonicalScalar::Int(9))
    );
    let exact = query("MATCH (n) WITH n.p AS x RETURN SUM_INT(x) AS total,AVG_INT(x) AS mean");
    let large = [
        CanonicalScalar::Int(i64::MAX),
        CanonicalScalar::Int(i64::MAX),
    ];
    let result = run(&exact, &large, wide());
    assert_eq!(
        result.value[0].values()[0].as_integer(),
        Some(i128::from(i64::MAX) * 2)
    );
    assert_eq!(
        result.value[0].values()[1].as_average(),
        GraphExactAverage::new(i128::from(i64::MAX), 1)
    );
    let absent = query("MATCH (n) WITH n.p AS x RETURN MAX(x) AS maximum HAVING NOT (maximum=0)");
    assert!(
        run(&absent, &[CanonicalScalar::Null], wide())
            .value
            .is_empty()
    );
    let empty =
        query("MATCH (n) WITH n.p AS x RETURN COUNT(*) AS n,SUM(x) AS total,AVG(x) AS mean");
    let result = run(&empty, &[], wide());
    assert_eq!(result.value.len(), 1);
    assert_eq!(result.value[0].values()[0].as_count(), Some(0));
    assert!(
        result.value[0].values()[1..]
            .iter()
            .all(|value| value.is_null())
    );
}

#[test]
fn input_distinct_and_output_distinct_have_separate_grouping_boundaries() {
    let input = [
        CanonicalScalar::Int(2),
        CanonicalScalar::Int(2),
        CanonicalScalar::Int(5),
        CanonicalScalar::Null,
    ];
    for (quantifier, expected) in [("", 2), ("DISTINCT ", 1)] {
        let bound = query(&format!(
            "MATCH (n) WITH {quantifier}n.p AS x WHERE x IS NOT NULL RETURN x AS key,COUNT(*) AS occurrences ORDER BY key"
        ));
        let result = run(&bound, &input, wide());
        assert_eq!(result.value.len(), 2);
        assert_eq!(
            result.value[0].keys()[0].as_scalar(),
            Some(&CanonicalScalar::Int(2))
        );
        assert_eq!(result.value[0].values()[0].as_count(), Some(expected));
        assert_eq!(result.value[1].values()[0].as_count(), Some(1));
    }
    for (quantifier, expected) in [("ALL ", 4), ("DISTINCT ", 1)] {
        let bound = query(&format!(
            "MATCH (n) WITH n AS owner RETURN {quantifier}COUNT(*) AS occurrences GROUP BY owner HAVING occurrences=1 ORDER BY occurrences DESC"
        ));
        let result = run(&bound, &input, wide());
        assert_eq!(result.value.len(), expected);
        assert!(result.value.iter().all(|row| row.keys().is_empty()));
    }
}

#[test]
fn nullable_optional_and_shortest_bags_survive_the_row_aggregate_boundary() {
    for (mode, atom, expected) in [
        ("", ":R", 2),
        ("ALL SHORTEST WALK ", ":R*1..3", 2),
        ("ANY SHORTEST WALK ", ":R*1..3", 1),
    ] {
        let bound = query(&format!(
            "MATCH (a) OPTIONAL MATCH {mode}(a)-[{atom}]->(b) WITH a AS owner,b AS peer RETURN owner,COUNT(*) AS rows,COUNT(peer) AS matched ORDER BY owner"
        ));
        let result = bound
            .execute_governed(
                4,
                [VId(1), VId(2)],
                [(VId(1), R, VId(2)); 2],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        let counts = result
            .value
            .iter()
            .map(|row| {
                (
                    row.keys()[0].as_vertex().unwrap(),
                    row.values()[0].as_count().unwrap(),
                    row.values()[1].as_count().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(counts, vec![(VId(1), expected, expected), (VId(2), 1, 0)]);
    }
}

#[test]
fn malformed_grouped_tails_and_definition_limits_refuse_before_catalog_access() {
    let too_deep = format!(
        "MATCH (n) WITH n.p AS x{} RETURN SUM(x) AS s",
        " WITH x".repeat(62)
    );
    let too_many_having = format!(
        "MATCH (n) WITH n.p AS x RETURN SUM(x) AS s HAVING {}",
        vec!["TRUE"; 65].join(" AND ")
    );
    for text in [
        "MATCH (n) RETURN COUNT(*) AS n",
        "MATCH (n) WITH n.p AS x RETURN SUM(n.p) AS s",
        "MATCH (n) WITH n AS v RETURN AVG(v) AS mean",
        "MATCH (n) WITH n.p AS x RETURN SUM(x+) AS s",
        "MATCH (n) WITH n.p AS x RETURN MEDIAN(x) AS s",
        "MATCH (n) WITH n.p AS x RETURN COUNT(DISTINCT *) AS n",
        "MATCH (n) WITH n.p AS x RETURN x,COUNT(*) AS n GROUP BY x+1",
        "MATCH (n) WITH n.p AS x RETURN SUM(x) AS x GROUP BY x",
        "MATCH (n) WITH n.p AS x WITH x AS y RETURN SUM(x) AS s",
        "MATCH (n) WITH n.p AS x RETURN SUM(x) AS s HAVING missing=1",
        "MATCH (n) WITH n.p AS x RETURN SUM(x) AS s GROUP BY x,x",
        "MATCH (n) WITH n.p AS x RETURN SUM(x) AS s LIMIT 0 MATCH (m)",
        "MATCH (n) WITH n.p AS x WHERE x >= $cap RETURN SUM(x) AS s LIMIT $cap",
        &too_deep,
        &too_many_having,
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphPipelineAggregateText::prepare(text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls, 0, "{text}");
    }
}

#[test]
fn one_argument_table_keeps_original_utf8_offsets_and_never_reopens_catalog() {
    let text = "\u{2003}MATCH (n) WITH n.p AS x WHERE x >= $cut RETURN SUM(x) AS total HAVING total >= $cut LIMIT $count";
    let mut calls = 0;
    let template = PreparedGraphPipelineAggregateText::prepare(text, |kind, name| {
        calls += 1;
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(template.parameter_schema().len(), 2);
    assert_eq!(template.parameter_schema()[0].occurrences, 2);
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, text.find("$cut").unwrap());
    assert!(matches!(
        missing.kind,
        GraphPipelineAggregateTextErrorKind::Input(GraphSetTextErrorKind::Pattern(
            GraphPatternTextErrorKind::MissingParameter
        ))
    ));
    let partial = GqlParameters::new().with_int64("cut", 2).unwrap();
    assert_eq!(
        template.bind_parameters(&partial).unwrap_err().offset,
        text.find("$count").unwrap()
    );
    let args = partial.with_uint64("count", 1).unwrap();
    let bound = template.bind_parameters(&args).unwrap();
    assert_eq!(bound, template.bind_parameters(&args).unwrap());
    assert_eq!(calls, 1);
    let wrong = GqlParameters::new()
        .with_uint64("cut", 2)
        .unwrap()
        .with_uint64("count", 1)
        .unwrap();
    assert!(template.bind_parameters(&wrong).is_err());
    let extra = args.with_int64("extra", 3).unwrap();
    assert!(template.bind_parameters(&extra).is_err());
    assert!(
        PreparedGraphPipelineAggregateText::prepare_with_parameter_types(
            text,
            &[("cut", GqlParameterType::UInt64)],
            symbols
        )
        .is_err()
    );
    for end in (0..=text.len()).filter(|at| text.is_char_boundary(*at)) {
        let _ = PreparedGraphPipelineAggregateText::prepare(&text[..end], symbols);
    }
}

#[test]
fn later_arithmetic_observes_surviving_rows_but_failures_are_never_partial_success() {
    let bound = query(
        "MATCH (n) WITH n.p AS x WHERE x <> 0 WITH CASE WHEN x > 0 THEN 10/x ELSE 0 END AS y RETURN SUM(y) AS total",
    );
    let input = [-1, 0, 2, 5].map(CanonicalScalar::Int);
    assert_eq!(
        run(&bound, &input, wide()).value[0].values()[0].as_integer(),
        Some(7)
    );
    let fails = query("MATCH (n) WITH n.p AS x WITH 10/x AS y RETURN COUNT(*) AS n LIMIT 0");
    let result = fails.execute_governed(
        4,
        (0..4).map(VId),
        [],
        |_, _| Ok::<_, ()>(true),
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
    let source_error = fails.execute_governed(
        1,
        [VId(0)],
        [],
        |_, _| Ok(true),
        |_, _| Err::<Option<&CanonicalScalar>, _>("unreadable input"),
        wide(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        source_error,
        Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
            GraphSetExecutionError::Source("unreadable input")
        )))
    ));
    let non_integer = query("MATCH (n) WITH n.p AS x RETURN SUM(x) AS total LIMIT 0");
    let bad_value = CanonicalScalar::Bool(true);
    assert!(matches!(
        non_integer.execute_governed(
            1,
            [VId(0)],
            [],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&bad_value)),
            wide(),
            || Ok::<_, ()>(())
        ),
        Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
            aggregate: 0
        }))
    ));
}

#[test]
fn native_complete_pipeline_uses_one_query_allowance_and_every_checkpoint() {
    let bound = query(
        "MATCH (n) WITH n.p AS x WHERE x > 0 RETURN x AS key,SUM(x+1) AS n HAVING n > 0 ORDER BY key DESC",
    );
    let input = [1, 1, 2].map(CanonicalScalar::Int);
    let mut calls = 0;
    let measured = bound
        .execute_governed(
            3,
            [VId(0), VId(1), VId(2)],
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&input[vid.0 as usize])),
            wide(),
            || {
                calls += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(
            &bound,
            &input,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])
        ),
        measured
    );
    for dimension in 0..4 {
        let mut cap = caps;
        cap[dimension] -= 1;
        assert!(
            bound
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
    for stop in 1..=calls {
        let mut at = 0;
        let result = bound.execute_governed(
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
fn terminal_expressions_share_one_compact_projection_without_recounting_parameters() {
    let text = "MATCH (n) WITH n.p AS x,n AS unused \
        RETURN SUM(x*$step) AS total,x*$step AS bucket,AVG(x*$step) AS mean,COUNT(*) AS rows \
        ORDER BY bucket NULLS FIRST";
    let mut resolutions = 0;
    let template = PreparedGraphPipelineAggregateText::prepare(text, |kind, name| {
        resolutions += 1;
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(resolutions, 1);
    assert_eq!(template.parameter_schema().len(), 1);
    assert_eq!(template.parameter_schema()[0].occurrences, 3);
    assert_eq!(template.columns(), &["total", "bucket", "mean", "rows"]);
    let args = GqlParameters::new().with_int64("step", 2).unwrap();
    let bound = template.bind_parameters(&args).unwrap();
    // The explicit equivalent drops the unused input and computes one column.
    // All three terminal occurrences must share that exact relational input.
    let explicit = PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n.p AS x,n AS unused \
         WITH x*$step AS __fgdb_pipeline_input_0 \
         RETURN SUM(__fgdb_pipeline_input_0) AS total,__fgdb_pipeline_input_0 AS bucket, \
         AVG(__fgdb_pipeline_input_0) AS mean,COUNT(*) AS rows \
         GROUP BY __fgdb_pipeline_input_0 ORDER BY bucket NULLS FIRST",
        symbols,
    )
    .unwrap()
    .bind_parameters(&args)
    .unwrap();
    assert_eq!(bound.canonical_bytes(), explicit.canonical_bytes());
    let values = [
        CanonicalScalar::Null,
        CanonicalScalar::Int(2),
        CanonicalScalar::Int(2),
        CanonicalScalar::Int(3),
    ];
    let result = run(&bound, &values, wide());
    assert_eq!(result.value.len(), 3);
    for (row, (key, total, mean, count)) in result.value.iter().zip([
        (CanonicalScalar::Null, None, None, 1),
        (
            CanonicalScalar::Int(4),
            Some(8),
            GraphExactAverage::new(4, 1),
            2,
        ),
        (
            CanonicalScalar::Int(6),
            Some(6),
            GraphExactAverage::new(6, 1),
            1,
        ),
    ]) {
        assert_eq!(row.keys()[0].as_scalar(), Some(&key));
        assert_eq!(row.values()[0].as_integer(), total);
        assert_eq!(row.values()[1].as_average(), mean);
        assert_eq!(row.values()[2].as_count(), Some(count));
    }
    let frozen = bound.canonical_bytes();
    let rebound = template
        .bind_parameters(&GqlParameters::new().with_int64("step", 3).unwrap())
        .unwrap();
    assert_ne!(frozen, rebound.canonical_bytes());
    assert_eq!(bound.canonical_bytes(), frozen);
    assert_eq!(resolutions, 1);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    assert!(
        template
            .bind_parameters(&args.with_int64("unused", 7).unwrap())
            .is_err()
    );
}

#[test]
fn whole_expression_grouping_supports_explicit_aliases_and_repeated_output_keys() {
    let base = "MATCH (n) WITH n.p AS x \
        RETURN x AS original,x+1 AS next,x+1 AS repeated,COUNT(*) AS rows";
    let inferred = query(base);
    for group in ["x,x+1", "original,next"] {
        assert_eq!(
            inferred.canonical_bytes(),
            query(&format!("{base} GROUP BY {group}")).canonical_bytes()
        );
    }
    let template = PreparedGraphPipelineAggregateText::prepare(base, symbols).unwrap();
    assert_eq!(
        template.output_slots(),
        &[
            GraphAggregateTextSlot::GroupKey(0),
            GraphAggregateTextSlot::GroupKey(1),
            GraphAggregateTextSlot::GroupKey(1),
            GraphAggregateTextSlot::Aggregate(0),
        ]
    );
    let input = [1, 1, 2].map(CanonicalScalar::Int);
    let result = run(&inferred, &input, wide());
    assert_eq!(result.value.len(), 2);
    for (row, (key, count)) in result.value.iter().zip([(1, 2), (2, 1)]) {
        assert_eq!(row.keys().len(), 2);
        assert_eq!(row.keys()[0].as_scalar(), Some(&CanonicalScalar::Int(key)));
        assert_eq!(
            row.keys()[1].as_scalar(),
            Some(&CanonicalScalar::Int(key + 1))
        );
        assert_eq!(row.values()[0].as_count(), Some(count));
    }
    let compact = query("MATCH (n) WITH n.p AS x RETURN x+1 AS key,COUNT(*) AS rows");
    assert!(run(&compact, &[], wide()).value.is_empty());
}

#[test]
fn optional_summary_aliases_and_all_star_keep_native_default_names() {
    let implicit = PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n.p AS x RETURN COUNT(ALL *),SUM(x+1),AVG(x+1),MIN(x+1),MAX(x+1),COLLECT(x+1)",
        symbols,
    )
    .unwrap();
    assert_eq!(
        implicit.columns(),
        &["count", "sum", "avg", "min", "max", "collect"]
    );
    let explicit = query(
        "MATCH (n) WITH n.p AS x RETURN COUNT(*) AS count,SUM(ALL x+1) AS sum, \
         AVG(x+1) AS avg,MIN(DISTINCT x+1) AS min,MAX(DISTINCT x+1) AS max, \
         COLLECT(x+1) AS collect",
    );
    let bound = implicit.bind_parameters(&GqlParameters::new()).unwrap();
    assert_eq!(bound.canonical_bytes(), explicit.canonical_bytes());
    let input = [1, 1, 3].map(CanonicalScalar::Int);
    let rows = run(&bound, &input, wide()).value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values()[0].as_count(), Some(3));
    assert_eq!(rows[0].values()[1].as_integer(), Some(8));
    assert_eq!(
        rows[0].values()[2].as_average(),
        GraphExactAverage::new(8, 3)
    );
}

#[test]
fn terminal_projection_stays_after_filters_input_pages_and_distinct() {
    let input = [-1, 0, 2, 5].map(CanonicalScalar::Int);
    let filtered = query(
        "MATCH (n) WITH n.p AS x WHERE x <> 0 WITH x ORDER BY x LIMIT 2 \
         RETURN SUM(10/x) AS total",
    );
    assert_eq!(
        run(&filtered, &input, wide()).value[0].values()[0].as_integer(),
        Some(-5)
    );
    let input = [1, 1, 2].map(CanonicalScalar::Int);
    for (quantifier, sum) in [("ALL", 7), ("DISTINCT", 5)] {
        let bound = query(&format!(
            "MATCH (n) WITH {quantifier} n.p AS x RETURN SUM(x+1) AS total"
        ));
        assert_eq!(
            run(&bound, &input, wide()).value[0].values()[0].as_integer(),
            Some(sum)
        );
    }
    // Output LIMIT is NOT an input short-circuit. Arithmetic remains typed
    // failure rather than a successful empty result, even inside an aggregate.
    for text in [
        "MATCH (n) WITH n.p AS x RETURN SUM(10/x) AS total LIMIT 0",
        "MATCH (n) WITH n.p AS x RETURN COUNT(10/x) AS rows LIMIT 0",
        "MATCH (n) WITH n.p AS x RETURN 10/x AS key,COUNT(*) AS rows LIMIT 0",
    ] {
        let bound = query(text);
        let zero = CanonicalScalar::Int(0);
        assert!(matches!(
            bound.execute_governed(
                1,
                [VId(0)],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(Some(&zero)),
                wide(),
                || Ok::<_, ()>(()),
            ),
            Err(GqlQueryError::Source(_))
        ));
    }
}

/// fgdb-1tgko: the WITH before an aggregate RETURN reads carried-vertex
/// properties in its WHERE and pages exactly as the explicit column would,
/// and the hidden columns never reach the aggregate's input schema.
#[test]
fn a_with_before_an_aggregate_reads_carried_properties_in_its_where_and_pages() {
    let input = [1, 2, 2, 3, 4].map(CanonicalScalar::Int);
    for (boundary, explicit, expected) in [
        (
            "MATCH (n) WITH n WHERE n.p = 2 RETURN COUNT(*) AS c",
            "MATCH (n) WITH n, n.p AS h WHERE h = 2 RETURN COUNT(*) AS c",
            2,
        ),
        (
            "MATCH (n) WITH n AS m ORDER BY m.p DESC LIMIT 3 RETURN COUNT(m) AS c",
            "MATCH (n) WITH n AS m, n.p AS h ORDER BY h DESC LIMIT 3 RETURN COUNT(m) AS c",
            3,
        ),
        (
            "MATCH (n) WITH n LIMIT 4 WHERE n.p > 1 RETURN COUNT(*) AS c",
            "MATCH (n) WITH n, n.p AS h LIMIT 4 WHERE h > 1 RETURN COUNT(*) AS c",
            3,
        ),
    ] {
        let found = run(&query(boundary), &input, wide()).value;
        assert_eq!(
            found,
            run(&query(explicit), &input, wide()).value,
            "{boundary}"
        );
        assert_eq!(
            found[0].values()[0].as_count(),
            Some(expected),
            "{boundary}"
        );
    }
    let template = PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n WHERE n.p = 2 RETURN n AS owner, COUNT(*) AS c",
        symbols,
    )
    .unwrap();
    assert_eq!(template.columns(), &["owner", "c"]);
    // The private names never resolve. A carried property inside the
    // aggregate RETURN itself is outside the boundary scope until fgdb-djlxq
    // lifts it deliberately: the hidden columns are projected away first.
    for text in [
        "MATCH (n) WITH n WHERE n.p = 2 RETURN COUNT(__fg_boundary_0) AS c",
        "MATCH (n) WITH n WHERE n.p = 2 RETURN COUNT(n.p) AS c",
    ] {
        assert!(
            PreparedGraphPipelineAggregateText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

#[test]
fn computed_arguments_preserve_wide_aggregate_domains_and_empty_input() {
    let bound = query("MATCH (n) WITH n.p AS x RETURN SUM(x+0) AS total,AVG(x+0) AS mean");
    let large = [
        CanonicalScalar::Int(i64::MAX),
        CanonicalScalar::Int(i64::MAX),
    ];
    let row = run(&bound, &large, wide()).value.remove(0);
    assert_eq!(row.values()[0].as_integer(), Some(i128::from(i64::MAX) * 2));
    assert_eq!(
        row.values()[1].as_average(),
        GraphExactAverage::new(i128::from(i64::MAX), 1)
    );
    let empty = query("MATCH (n) WITH n.p AS x RETURN COUNT(1/0) AS rows,SUM(1/0) AS total");
    let row = run(&empty, &[], wide()).value.remove(0);
    assert_eq!(row.values()[0].as_count(), Some(0));
    assert!(row.values()[1].is_null());
    // Literal/list/scalar argument types reuse the ordinary row compiler;
    // lists are nonnull values even when their elements are null.
    let list = query("MATCH (n) WITH n.p AS x RETURN COUNT(DISTINCT [x,x+1]) AS rows");
    let input = [
        CanonicalScalar::Null,
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(1),
    ];
    assert_eq!(
        run(&list, &input, wide()).value[0].values()[0].as_count(),
        Some(2)
    );
}

#[test]
fn computed_terminal_definition_limits_and_invalid_scopes_refuse_before_resolution() {
    let too_deep = format!(
        "MATCH (n) WITH n.p AS x{} RETURN SUM(x+1) AS s",
        " WITH x".repeat(fgdb_gql::MAX_GRAPH_SET_DEPTH - 3)
    );
    let valid_depth = format!(
        "MATCH (n) WITH n.p AS x{} RETURN SUM(x+1) AS s",
        " WITH x".repeat(fgdb_gql::MAX_GRAPH_SET_DEPTH - 4)
    );
    query(&valid_depth);
    for text in [
        "MATCH (n) WITH n.p AS x RETURN SUM(SUM(x)) AS total",
        "MATCH (n) WITH n.p AS x RETURN SUM([x]) AS total",
        "MATCH (n) WITH n.p AS x RETURN x+1,COUNT(*) AS rows",
        "MATCH (n) WITH n.p AS x RETURN x+1 AS key,COUNT(*) AS rows GROUP BY x+2",
        "MATCH (n) WITH n.p AS x RETURN COUNT(*) AS rows GROUP BY x+1,x+1",
        "MATCH (n) WITH n.p AS x RETURN SUM(x),SUM(x+1)",
        "MATCH (n) WITH n.p AS x WITH x AS y RETURN SUM(x+1) AS total",
        "MATCH (n) WITH n.p AS x RETURN SUM(x+$p) AS total LIMIT $p",
        "MATCH (n) WITH n.p AS x RETURN COUNT(DISTINCT *)",
        "MATCH (n) WITH n.p AS x RETURN COUNT(*) AS rows,SUM(x)+1 AS total",
        &too_deep,
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphPipelineAggregateText::prepare(text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls, 0, "{text}");
    }
}

#[test]
fn existing_group_names_and_private_projection_names_cannot_be_captured() {
    // Swapping public output aliases must not reinterpret a previously valid
    // GROUP BY that names the completed WITH schema in its original order.
    let bound =
        query("MATCH (n) WITH n.p AS x,n AS y RETURN x AS y,y AS x,COUNT(*) AS rows GROUP BY x,y");
    assert_eq!(bound.group_key_columns(), &[0, 1]);
    let row = run(&bound, &[CanonicalScalar::Int(7)], wide())
        .value
        .remove(0);
    assert_eq!(row.keys()[0].as_scalar(), Some(&CanonicalScalar::Int(7)));
    assert_eq!(row.keys()[1].as_vertex(), Some(VId(0)));
    let bound = query(
        "MATCH (n) WITH n.p AS __fgdb_pipeline_input_0 \
         RETURN __fgdb_pipeline_input_0+1 AS key, \
         SUM(__fgdb_pipeline_input_0+1) AS __fgdb_pipeline_input_1",
    );
    let row = run(&bound, &[CanonicalScalar::Int(7)], wide())
        .value
        .remove(0);
    assert_eq!(row.keys()[0].as_scalar(), Some(&CanonicalScalar::Int(8)));
    assert_eq!(row.values()[0].as_integer(), Some(8));
}
