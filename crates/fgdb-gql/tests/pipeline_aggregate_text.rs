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
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
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

#[test]
fn leading_identity_bags_keep_the_seeded_source_for_terminal_aggregates() {
    use fgdb_gql::algebra::GraphValue;
    use std::cell::Cell;

    let template = PreparedGraphPipelineAggregateText::prepare_with_parameter_types(
        "UNWIND $nodes AS n MATCH (n)-[:R]->(m) RETURN COUNT(*) AS count, COUNT(m.p) AS present",
        &[("nodes", GqlParameterType::List)],
        symbols,
    )
    .unwrap();
    assert_eq!(template.graph_source_count(), 1);
    assert!(template.requires_relational_input());
    let arguments = GqlParameters::new()
        .with_list(
            "nodes",
            vec![
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(1)),
                GraphValue::Scalar(CanonicalScalar::Null),
            ],
        )
        .unwrap();
    let query = template.bind_relation_parameters(&arguments).unwrap();
    let calls = Cell::new(0);
    let property = CanonicalPropertyValue::Scalar(CanonicalScalar::Int(7));
    let result = query
        .execute_governed_with_seeded_elements(
            wide(),
            |_, _| panic!("a leading identity equality must not admit the whole graph"),
            |pattern, seeds, remaining| {
                calls.set(calls.get() + 1);
                assert_eq!(seeds, &[VId(1)]);
                Some(pattern.plan().execute_governed_with_properties(
                    3,
                    [VId(1), VId(3)],
                    [(VId(1), R, VId(3))],
                    |_, _| Ok::<_, &'static str>(true),
                    |_, _| Ok(Some(&property)),
                    remaining,
                    || Ok::<_, ()>(()),
                ))
            },
            |_, _, _| panic!("no procedure in this definition"),
            |_, _| Ok(None),
            || Ok(()),
        )
        .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(result.rows.snapshot_records, 3);
    assert_eq!(result.value.len(), 1);
    assert_eq!(result.value[0].values()[0].as_count(), Some(2));
    assert_eq!(result.value[0].values()[1].as_count(), Some(2));
}

#[test]
fn inline_call_aggregate_keeps_its_one_source_relational() {
    let template = PreparedGraphPipelineAggregateText::prepare(
        "CALL { MATCH (n) RETURN n } RETURN count(n) AS total",
        symbols,
    )
    .unwrap();
    assert_eq!(template.graph_source_count(), 1);
    assert!(template.requires_relational_input());
    let operators = template.template_operators();
    assert!(operators.contains(&"Apply"));
    let query = template
        .bind_relation_parameters(&GqlParameters::new())
        .unwrap();
    let result = query
        .execute_governed(
            wide(),
            |pattern, remaining| {
                pattern.plan().execute_governed_with_properties(
                    3,
                    [VId(1), VId(2), VId(3)],
                    [],
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    remaining,
                    || Ok::<_, ()>(()),
                )
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(3));
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
    let stored: Vec<_> = input
        .iter()
        .cloned()
        .map(CanonicalPropertyValue::from)
        .collect();
    query
        .execute_governed(
            input.len() as u64,
            (0..input.len()).map(|at| VId(at as u128)),
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, key| Ok((key == P).then(|| &stored[vid.0 as usize])),
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
    // The native pipeline reads `n.p` as a scalar column and prepares `n.p+1`
    // as a scalar expression. Since 31408b2c a dynamic column under
    // arithmetic stays a scalar load (it may hold a Float), so the typed mirror
    // spells it the same way.
    let plus = GraphIntegerExpression::prepare_scalar(&[
        GraphIntegerOp::ScalarColumn(1),
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
    let stored = input.clone().map(CanonicalPropertyValue::from);
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
        |vid, _| Ok(Some(&stored[vid.0 as usize])),
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
        |_, _| Err::<Option<&CanonicalPropertyValue>, _>("unreadable input"),
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
    let bad_value = CanonicalPropertyValue::from(CanonicalScalar::Bool(true));
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
    let stored = input.clone().map(CanonicalPropertyValue::from);
    let mut calls = 0;
    let measured = bound
        .execute_governed(
            3,
            [VId(0), VId(1), VId(2)],
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&stored[vid.0 as usize])),
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
                    |vid, _| Ok(Some(&stored[vid.0 as usize])),
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
            |vid, _| Ok(Some(&stored[vid.0 as usize])),
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

/// An unaliased summary is named by its source text (openCypher), and the
/// ALL, `*` and extremum DISTINCT spellings are one plan.
#[test]
fn summary_names_are_source_text_and_all_star_spellings_share_one_plan() {
    let implicit = PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n.p AS x RETURN COUNT(ALL *),SUM(x+1),AVG(x+1),MIN(x+1),MAX(x+1),COLLECT(x+1)",
        symbols,
    )
    .unwrap();
    assert_eq!(
        implicit.columns(),
        &[
            "COUNT(ALL *)",
            "SUM(x+1)",
            "AVG(x+1)",
            "MIN(x+1)",
            "MAX(x+1)",
            "COLLECT(x+1)"
        ]
    );
    let plain = query(
        "MATCH (n) WITH n.p AS x RETURN COUNT(ALL *) AS count,SUM(x+1) AS sum, \
         AVG(x+1) AS avg,MIN(x+1) AS min,MAX(x+1) AS max,COLLECT(x+1) AS collect",
    );
    let explicit = query(
        "MATCH (n) WITH n.p AS x RETURN COUNT(*) AS count,SUM(ALL x+1) AS sum, \
         AVG(x+1) AS avg,MIN(DISTINCT x+1) AS min,MAX(DISTINCT x+1) AS max, \
         COLLECT(x+1) AS collect",
    );
    assert_eq!(plain.canonical_bytes(), explicit.canonical_bytes());
    let bound = implicit.bind_parameters(&GqlParameters::new()).unwrap();
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
        let zero = CanonicalPropertyValue::from(CanonicalScalar::Int(0));
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
/// properties in its WHERE and pages exactly as the explicit column would.
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
}

/// fgdb-djlxq: the aggregate RETURN itself reads carried-vertex properties,
/// as grouping keys, aggregate arguments and GROUP BY items. Each form equals
/// the explicit `WITH n, n.p AS h` column. An unaliased key `n.p` is named
/// `p`. The private names never resolve, and HAVING and the result page
/// address only the output row: a boundary of the same width as that row
/// must not resolve `n.p` there.
#[test]
fn an_aggregate_return_reads_carried_properties_in_keys_arguments_and_group_by() {
    let input = [1, 2, 2, 3, 4].map(CanonicalScalar::Int);
    for (boundary, explicit) in [
        (
            "MATCH (n) WITH n RETURN n.p AS k, COUNT(*) AS c",
            "MATCH (n) WITH n, n.p AS h RETURN h AS k, COUNT(*) AS c",
        ),
        (
            "MATCH (n) WITH n WHERE n.p > 1 RETURN COUNT(n.p) AS c, SUM(n.p) AS s, MIN(n.p) AS lo",
            "MATCH (n) WITH n, n.p AS h WHERE h > 1 RETURN COUNT(h) AS c, SUM(h) AS s, MIN(h) AS lo",
        ),
        (
            "MATCH (n) WITH n AS m RETURN m.p AS k, COUNT(m) AS c GROUP BY m.p",
            "MATCH (n) WITH n AS m, n.p AS h RETURN h AS k, COUNT(m) AS c GROUP BY h",
        ),
        (
            "MATCH (n) WITH DISTINCT n RETURN n.p AS k, COUNT(*) AS c HAVING c > 1",
            "MATCH (n) WITH DISTINCT n, n.p AS h RETURN h AS k, COUNT(*) AS c HAVING c > 1",
        ),
    ] {
        let found = run(&query(boundary), &input, wide()).value;
        assert!(!found.is_empty(), "{boundary}");
        assert_eq!(
            found,
            run(&query(explicit), &input, wide()).value,
            "{boundary}"
        );
    }
    let grouped = run(
        &query("MATCH (n) WITH n RETURN n.p AS k, COUNT(*) AS c HAVING c > 1"),
        &input,
        wide(),
    )
    .value;
    assert_eq!(grouped.len(), 1);
    assert_eq!(grouped[0].values()[0].as_count(), Some(2));
    let template = PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n RETURN n.p, COUNT(*) AS c",
        symbols,
    )
    .unwrap();
    assert_eq!(template.columns(), &["n.p", "c"]);
    for text in [
        "MATCH (n) WITH n RETURN n.p AS k, COUNT(__fg_boundary_0) AS c",
        "MATCH (n) WITH n RETURN n.p AS k, COUNT(*) AS c GROUP BY __fg_boundary_0",
        // Two output columns, as the boundary row has (n plus hidden n.p).
        "MATCH (n) WITH n RETURN n.p AS k, COUNT(*) AS c HAVING n.p > 1",
        "MATCH (n) WITH n RETURN n.p AS k, COUNT(*) AS c ORDER BY n.p",
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
        "MATCH (n) WITH n.p AS x RETURN x+1 AS key,COUNT(*) AS rows GROUP BY x+2",
        "MATCH (n) WITH n.p AS x RETURN COUNT(*) AS rows GROUP BY x+1,x+1",
        "MATCH (n) WITH n.p AS x WITH x AS y RETURN SUM(x+1) AS total",
        "MATCH (n) WITH n.p AS x RETURN SUM(x+$p) AS total LIMIT $p",
        "MATCH (n) WITH n.p AS x RETURN COUNT(DISTINCT *)",
        // `SUM(x)+1` combines aggregate results after WITH since fgdb-4avot
        // (9cc5346a); it is no longer a refusal.
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
    PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n.p AS x RETURN COUNT(*) AS rows,SUM(x)+1 AS total",
        symbols,
    )
    .unwrap();
    // Unaliased computed keys and repeated summaries take their source text.
    for (text, columns) in [
        (
            "MATCH (n) WITH n.p AS x RETURN x+1,COUNT(*) AS rows",
            &["x+1", "rows"][..],
        ),
        (
            "MATCH (n) WITH n.p AS x RETURN SUM(x),SUM(x+1)",
            &["SUM(x)", "SUM(x+1)"][..],
        ),
        (
            "MATCH (n) WITH n.p AS x RETURN SUM(x),COUNT(*)",
            &["SUM(x)", "COUNT(*)"][..],
        ),
    ] {
        let prepared = PreparedGraphPipelineAggregateText::prepare(text, symbols).unwrap();
        assert_eq!(prepared.columns(), columns, "{text}");
    }
    let row = run(
        &query("MATCH (n) WITH n.p AS x RETURN SUM(x),SUM(x+1)"),
        &[CanonicalScalar::Int(7)],
        wide(),
    )
    .value
    .remove(0);
    assert_eq!(row.values()[0].as_integer(), Some(7));
    assert_eq!(row.values()[1].as_integer(), Some(8));
}

/// fgdb-cxjqp: a projected key reports the name of its first RETURN alias,
/// the name its result column has. Hosts that match the physical definition
/// against the visible columns (the buffered spill and streamed aggregate
/// layout checks) therefore accept an aliased or computed key. Only names
/// change: the definition's bytes equal those of the unaliased form.
#[test]
fn a_projected_key_reports_the_name_of_its_return_alias() {
    let relation = |text: &str| {
        let template = PreparedGraphPipelineAggregateText::prepare(text, symbols).unwrap();
        let bound = template
            .bind_relation_parameters(&GqlParameters::new())
            .unwrap();
        (template, bound)
    };
    for (text, keys) in [
        (
            "UNWIND [1, 2, 3] AS x RETURN x AS b, SUM(x) AS t GROUP BY x",
            &["b"][..],
        ),
        (
            "UNWIND [1, 2, 3] AS x RETURN x % 2 AS b, SUM(x) AS t GROUP BY x % 2",
            &["b"][..],
        ),
        (
            "UNWIND [1, 2, 3] AS x RETURN x, SUM(x) AS t GROUP BY x",
            &["x"][..],
        ),
        (
            "UNWIND [1, 2, 3] AS x RETURN x AS a, x AS b, SUM(x) AS t",
            &["a"][..],
        ),
        (
            "UNWIND [1, 2] AS x UNWIND [3] AS y RETURN y AS x, x AS y, COUNT(*) AS n GROUP BY x, y",
            &["x", "y"][..],
        ),
    ] {
        let (template, bound) = relation(text);
        assert_eq!(bound.key_columns(), keys, "{text}");
        assert_eq!(&template.columns()[..1], &keys[..1], "{text}");
    }
    assert_eq!(
        relation("UNWIND [1, 2, 3] AS x RETURN x AS b, SUM(x) AS t GROUP BY x")
            .1
            .canonical_bytes(),
        relation("UNWIND [1, 2, 3] AS x RETURN x, SUM(x) AS t GROUP BY x")
            .1
            .canonical_bytes()
    );
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

/// A leading UNWIND joined by a MATCH, then a WITH, feeds an aggregate RETURN
/// (`UNWIND $ids AS id MATCH (n {id: id}) WITH ... RETURN count(*)`), as the
/// same rows do behind a leading WITH: one row per joined occurrence, a
/// repeated value matching once per repeat, and an unmatched value none.
#[test]
fn a_leading_unwind_joined_by_match_feeds_an_aggregate_return() {
    let input = [
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(2),
        CanonicalScalar::Int(3),
    ];
    let totals = |text: &str| {
        let row = run(&query(text), &input, wide()).value.remove(0);
        (
            row.values()[0].as_count().unwrap(),
            row.values()[1].as_integer().unwrap(),
        )
    };
    let unwound = "UNWIND [2, 2, 3, 9] AS wanted MATCH (n {p: wanted}) WITH n.p AS matched \
                   RETURN COUNT(*) AS rows, SUM(matched) AS total";
    assert_eq!(totals(unwound), (3, 7));
    // Without a WITH the RETURN reads the implicit row: the unwound column
    // and the matched vertex, whose property is a hidden graph read.
    assert_eq!(
        totals(
            "UNWIND [2, 2, 3, 9] AS wanted MATCH (n {p: wanted}) \
             RETURN COUNT(*) AS rows, SUM(n.p) AS total"
        ),
        (3, 7)
    );
    assert_eq!(
        totals(&format!("WITH 1 AS one {unwound}")),
        totals(unwound),
        "{unwound}"
    );
    // Grouped by the matched value, in the written key order.
    let grouped = run(
        &query(
            "UNWIND [2, 2, 3] AS wanted MATCH (n {p: wanted}) WITH n.p AS matched \
             RETURN matched, COUNT(*) AS copies ORDER BY matched",
        ),
        &input,
        wide(),
    );
    let groups: Vec<_> = grouped
        .value
        .iter()
        .map(|row| {
            (
                row.keys()[0].as_scalar().cloned(),
                row.values()[0].as_count().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        vec![
            (Some(CanonicalScalar::Int(2)), 2),
            (Some(CanonicalScalar::Int(3)), 1)
        ]
    );
}

/// fgdb-4avot: a terminal RETURN after WITH may combine aggregate results with
/// arithmetic and grouping keys (`SUM(x) * 10 / COUNT(x)`, `x + COUNT(*)`), as
/// the plain MATCH ... RETURN facade already does. The rows equal that
/// facade's answer over the same data. ORDER BY may rank a combined item, as
/// in that facade. A non-aggregate operand that is not a grouping key refuses,
/// as in openCypher, and HAVING over a combined item still refuses.
#[test]
fn a_terminal_return_combines_aggregate_results_like_the_graph_facade() {
    let graph = |text: &str| {
        fgdb_gql::PreparedGraphAggregateText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    };
    let pipeline = |text: &str| {
        PreparedGraphPipelineAggregateText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    };
    let input = [
        CanonicalScalar::Int(5),
        CanonicalScalar::Int(5),
        CanonicalScalar::Null,
        CanonicalScalar::Int(9),
    ];
    for (with, plain) in [
        (
            "MATCH (n) WITH n.p AS x RETURN SUM(x) * 10 / COUNT(x) AS scaled, COUNT(*) + 1 AS more",
            "MATCH (n) RETURN SUM(n.p) * 10 / COUNT(n.p) AS scaled, COUNT(*) + 1 AS more",
        ),
        (
            "MATCH (n) WITH n.p AS x RETURN x, COUNT(*) * 2 AS doubled",
            "MATCH (n) RETURN n.p AS x, COUNT(*) * 2 AS doubled",
        ),
        (
            "MATCH (n) WITH n.p AS x RETURN x, x + COUNT(*) AS shifted",
            "MATCH (n) RETURN n.p AS x, n.p + COUNT(*) AS shifted",
        ),
    ] {
        let mut combined = run(&pipeline(with), &input, wide()).value;
        let mut reference = run(&graph(plain), &input, wide()).value;
        combined.sort();
        reference.sort();
        assert_eq!(combined, reference, "{with}");
        assert!(!combined.is_empty(), "{with}");
    }
    // ORDER BY over a combined item ranks the computed output tuple, as the
    // graph facade does. The rows are compared in delivered order.
    for (with, plain) in [
        (
            "MATCH (n) WITH n.p AS x RETURN x, COUNT(*) * 2 AS d ORDER BY d DESC, x",
            "MATCH (n) RETURN n.p AS x, COUNT(*) * 2 AS d ORDER BY d DESC, x",
        ),
        (
            "MATCH (n) WITH n.p AS x RETURN x, x + COUNT(*) AS s ORDER BY s LIMIT 2",
            "MATCH (n) RETURN n.p AS x, n.p + COUNT(*) AS s ORDER BY s LIMIT 2",
        ),
    ] {
        let ordered = run(&pipeline(with), &input, wide()).value;
        assert_eq!(ordered, run(&graph(plain), &input, wide()).value, "{with}");
        assert_eq!(
            ordered.len(),
            if with.contains("LIMIT") { 2 } else { 3 },
            "{with}"
        );
    }
    for text in [
        // y is not a grouping key of the RETURN.
        "MATCH (n) WITH n.p AS x, n.p AS y RETURN x, y + COUNT(*) AS z",
        "MATCH (n) WITH n.p AS x RETURN x, COUNT(*) * 2 AS d HAVING x > 1",
    ] {
        assert!(
            PreparedGraphPipelineAggregateText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// percentileDisc and percentileCont in a pipeline's terminal aggregate
/// RETURN (fgdb-qbaex) answer as the graph facade's do over the same rows,
/// NULL skipped and per group. p may be a grouping key, read after
/// aggregation, so out of [0, 1] it raises (TCK Aggregation6 [5]).
#[test]
fn a_terminal_return_takes_percentiles_like_the_graph_facade() {
    let graph = |text: &str| {
        fgdb_gql::PreparedGraphAggregateText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    };
    let input = [
        CanonicalScalar::Int(30),
        CanonicalScalar::Int(10),
        CanonicalScalar::Null,
        CanonicalScalar::Int(40),
        CanonicalScalar::Int(20),
    ];
    for (with, plain) in [
        (
            "MATCH (n) WITH n.p AS x \
             RETURN percentileDisc(x, 0.5) AS d, percentileCont(x, 0.25) AS c",
            "MATCH (n) RETURN percentileDisc(n.p, 0.5) AS d, percentileCont(n.p, 0.25) AS c",
        ),
        (
            "MATCH (n) WITH n.p AS x RETURN x, percentileDisc(x, 1.0) AS top",
            "MATCH (n) RETURN n.p AS x, percentileDisc(n.p, 1.0) AS top",
        ),
    ] {
        let mut pipeline = run(&query(with), &input, wide()).value;
        let mut reference = run(&graph(plain), &input, wide()).value;
        pipeline.sort();
        reference.sort();
        assert_eq!(pipeline, reference, "{with}");
        assert!(!pipeline.is_empty(), "{with}");
    }
    let stored: Vec<_> = input
        .iter()
        .cloned()
        .map(CanonicalPropertyValue::from)
        .collect();
    let failure =
        query("MATCH (n) WITH n.p AS deg LIMIT 100 RETURN percentileDisc(0.90, deg), deg")
            .execute_governed(
                input.len() as u64,
                (0..input.len()).map(|at| VId(at as u128)),
                [],
                |_, _| Ok::<_, ()>(true),
                |vid, key| Ok((key == P).then(|| &stored[vid.0 as usize])),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap_err();
    assert!(
        format!("{failure:?}").contains("NumberOutOfRange"),
        "{failure:?}"
    );
}

/// A terminal aggregate RETURN reads a pattern comprehension (fgdb-1f8f7,
/// TCK Pattern2 [6]). With no WITH, the RETURN reads its MATCH through an
/// implicit `WITH *`, whose head gathers the comprehension into a hidden list
/// column, one list per row; after a WITH, that WITH's head gathers it.
/// Vertex 0 has two parallel edges to vertex 1, vertex 1 one edge to vertex
/// 2, and vertex 2 none: lists of 2, 1 and 0 paths.
#[test]
fn a_terminal_aggregate_return_reads_pattern_comprehensions() {
    let edges = [
        (fgdb_types::EId(1), VId(0), R, VId(1)),
        (fgdb_types::EId(2), VId(0), R, VId(1)),
        (fgdb_types::EId(3), VId(1), R, VId(2)),
    ];
    let stored: Vec<CanonicalPropertyValue> = [10, 20, 30]
        .map(|p| CanonicalPropertyValue::from(CanonicalScalar::Int(p)))
        .to_vec();
    let rows = |text: &str| {
        let mut rows = query(text)
            .execute_governed_with_identified_properties(
                (stored.len() + edges.len()) as u64,
                (0..3).map(VId),
                edges,
                |_, _| Ok::<_, ()>(true),
                |vid, key| Ok((key == P).then(|| &stored[vid.0 as usize])),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .value;
        rows.sort();
        rows
    };
    let single = |text: &str| {
        let rows = rows(text);
        assert_eq!(rows.len(), 1, "{text}");
        rows[0].values()[0].clone()
    };
    // Every row has a list, empty or not: count counts all three.
    assert_eq!(
        single("MATCH (n) RETURN count([p = (n)-->() | p]) AS c").as_count(),
        Some(3)
    );
    assert_eq!(
        single("MATCH (n) RETURN sum(size([p = (n)-->() | p])) AS s").as_integer(),
        Some(3)
    );
    // After a WITH, with a WHERE inside the comprehension over its local.
    assert_eq!(
        single("MATCH (n) WITH n RETURN sum(size([(n)-->(m) WHERE m.p > 20 | m])) AS s")
            .as_integer(),
        Some(1)
    );
    // As a grouping key: degrees 2, 1 and 0, one vertex each.
    let grouped = rows("MATCH (n) RETURN size([(n)-->() | 1]) AS d, count(*) AS c");
    let keys: Vec<_> = grouped.iter().map(|row| row.keys().to_vec()).collect();
    let int = |n: i64| fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(n));
    assert_eq!(keys, [[int(0)], [int(1)], [int(2)]]);
    assert!(
        grouped
            .iter()
            .all(|row| row.values()[0].as_count() == Some(1))
    );
    for text in [
        "MATCH (n) RETURN count([x IN [n] | size([(x)-->() | 1])]) AS c",
        "MATCH (n)-->(k) WITH n AS k RETURN count([(k)-->() | 1]) AS c",
    ] {
        assert!(
            PreparedGraphPipelineAggregateText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// A terminal aggregate RETURN over collected paths (TCK List12 [4]):
/// `nodes(x)` and `relationships(x)` of each collected path value, and the
/// aggregate output evaluator indexing those compact vertex and edge lists
/// (`head(...)`, `[-1]`) as the lists of their elements. Two one-hop paths,
/// 0 -> 1 over edge 1 and 1 -> 2 over edge 2.
#[test]
fn an_aggregate_output_indexes_compact_vertex_and_edge_lists() {
    use fgdb_gql::algebra::GraphValue;
    let edges = [
        (fgdb_types::EId(1), VId(0), R, VId(1)),
        (fgdb_types::EId(2), VId(1), R, VId(2)),
    ];
    let stored: Vec<CanonicalPropertyValue> = [10, 20, 30]
        .map(|p| CanonicalPropertyValue::from(CanonicalScalar::Int(p)))
        .to_vec();
    let text = "MATCH p = (a)-[:R]->(b) RETURN [x IN collect(p) | head(nodes(x))] AS h, \
                [x IN collect(p) | relationships(x)[-1]] AS r, \
                [x IN collect(p) | nodes(x)[5]] AS beyond";
    let rows = fgdb_gql::PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .execute_governed_with_identified_properties(
            (stored.len() + edges.len()) as u64,
            (0..3).map(VId),
            edges,
            |_, _| Ok::<_, ()>(true),
            |vid, key| Ok((key == P).then(|| &stored[vid.0 as usize])),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap_or_else(|error| panic!("{error:?}"))
        .value;
    assert_eq!(rows.len(), 1);
    let list = |at: usize| {
        let mut values = rows[0].values()[at]
            .as_value()
            .and_then(GraphValue::as_list)
            .unwrap_or_else(|| panic!("column {at}"))
            .to_vec();
        values.sort();
        values
    };
    assert_eq!(
        list(0),
        [GraphValue::Vertex(VId(0)), GraphValue::Vertex(VId(1))]
    );
    assert_eq!(
        list(1),
        [
            GraphValue::Edge(fgdb_types::EId(1)),
            GraphValue::Edge(fgdb_types::EId(2))
        ]
    );
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    assert_eq!(list(2), [null.clone(), null]);
}

/// A terminal aggregate RETURN over count subqueries, `COUNT { ... }`: as
/// with a comprehension, the RETURN reads its MATCH
/// through an implicit `WITH *` whose head gathers the count. Vertex 0 has
/// two parallel edges to vertex 1, vertex 1 one edge to vertex 2.
#[test]
fn a_terminal_aggregate_return_reads_pattern_counts() {
    let edges = [
        (fgdb_types::EId(1), VId(0), R, VId(1)),
        (fgdb_types::EId(2), VId(0), R, VId(1)),
        (fgdb_types::EId(3), VId(1), R, VId(2)),
    ];
    let stored: Vec<CanonicalPropertyValue> = [10, 20, 30]
        .map(|p| CanonicalPropertyValue::from(CanonicalScalar::Int(p)))
        .to_vec();
    let rows = |text: &str| {
        let mut rows = query(text)
            .execute_governed_with_identified_properties(
                (stored.len() + edges.len()) as u64,
                (0..3).map(VId),
                edges,
                |_, _| Ok::<_, ()>(true),
                |vid, key| Ok((key == P).then(|| &stored[vid.0 as usize])),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .value;
        rows.sort();
        rows
    };
    let total = rows("MATCH (n) RETURN sum(COUNT { (n)-->() }) AS s");
    assert_eq!(total.len(), 1);
    assert_eq!(total[0].values()[0].as_integer(), Some(3));
    let total = rows("MATCH (n) RETURN sum(COUNT { MATCH (n)-->(m) WHERE m.p > 10 }) AS s");
    assert_eq!(total[0].values()[0].as_integer(), Some(3));
    let grouped = rows("MATCH (n) RETURN COUNT { (n)-->() } AS d, count(*) AS c");
    let int = |n: i64| fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(n));
    let keys: Vec<_> = grouped.iter().map(|row| row.keys().to_vec()).collect();
    assert_eq!(keys, [[int(0)], [int(1)], [int(2)]]);
    assert!(
        grouped
            .iter()
            .all(|row| row.values()[0].as_count() == Some(1))
    );
}
