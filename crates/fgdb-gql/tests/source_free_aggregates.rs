//! Genuine zero-source aggregates over Singleton/UNWIND/row stages. No fake
//! vertex, catalog lookup or source callback may be used to obtain a row.

use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlListParameter, GqlParameterValue, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphAggregateRow, GraphAggregateTextSlot, GraphExactAverage,
    PreparedGraphPipelineAggregateText, PreparedGraphSetAggregate,
};
use fgdb_types::CanonicalScalar;

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000)
}

fn template(text: &str) -> PreparedGraphPipelineAggregateText {
    PreparedGraphPipelineAggregateText::prepare(text, |_, _| {
        panic!("a source-free statement cannot resolve graph symbols")
    })
    .unwrap()
}

fn prepare(text: &str) -> PreparedGraphSetAggregate {
    template(text)
        .bind_relation_parameters(&GqlParameters::new())
        .unwrap()
}

fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), usize>> {
    panic!("source-free execution must never invoke the graph adapter")
}

fn run(plan: &PreparedGraphSetAggregate) -> GqlQueryExecution<GraphAggregateRow> {
    let result = plan
        .execute_governed(policy(), no_source, || Ok(()))
        .unwrap();
    assert_eq!(result.rows.snapshot_records, 0);
    result
}

#[test]
fn constant_pagination_preserves_aggregate_completion_and_group_order() {
    for (computed, literal) in [
        (
            "UNWIND [1,2,1,3] AS x RETURN x,count(*) AS n ORDER BY x SKIP 2 - 1 LIMIT toInteger(1.9)",
            "UNWIND [1,2,1,3] AS x RETURN x,count(*) AS n ORDER BY x SKIP 1 LIMIT 1",
        ),
        (
            "UNWIND [1,2,3] AS x RETURN sum(x) AS total LIMIT (1 - 1)",
            "UNWIND [1,2,3] AS x RETURN sum(x) AS total LIMIT 0",
        ),
    ] {
        assert_eq!(
            run(&prepare(computed)),
            run(&prepare(literal)),
            "{computed}"
        );
    }
}

#[test]
fn computed_having_keeps_hidden_summaries_and_aliases_in_one_exact_group() {
    let definition = template(
        "UNWIND [1,1,2,3,3,3] AS x \
         RETURN x AS bucket,count(*)+1 AS adjusted,count(*) AS copies \
         HAVING adjusted > 2 AND sum(x) BETWEEN 2 AND 9 \
         ORDER BY sum(x) + adjusted DESC",
    );
    assert_eq!(definition.columns(), ["bucket", "adjusted", "copies"]);
    let query = definition
        .bind_relation_parameters(&GqlParameters::new())
        .unwrap();
    // The two written COUNT items retain their existing summary identities;
    // HAVING and ORDER BY share the one additional hidden SUM.
    assert_eq!(query.evaluation_aggregate_columns().len(), 3);
    let rows = run(&query).value;
    assert_eq!(rows.len(), 2);
    for (row, (bucket, adjusted, copies)) in rows.iter().zip([(3, 4, 3), (1, 3, 2)]) {
        assert!(row.keys().is_empty());
        assert_eq!(row.values().len(), 3, "the hidden SUM is not returned");
        assert_eq!(
            row.values()[0].as_value(),
            Some(&GraphValue::Scalar(CanonicalScalar::Int(bucket)))
        );
        assert_eq!(row.values()[1].as_integer(), Some(adjusted));
        assert_eq!(row.values()[2].as_count(), Some(copies));
    }

    let empty = run(&prepare(
        "UNWIND [] AS x RETURN count(*) AS rows \
         HAVING coalesce(sum(x),0)+1 = 1",
    ));
    assert_eq!(empty.value.len(), 1);
    assert_eq!(empty.value[0].values().len(), 1);
    assert_eq!(empty.value[0].values()[0].as_count(), Some(0));
}

#[test]
fn computed_having_preserves_full_width_parameters_and_exact_averages() {
    use fgdb_gql::GqlParameterType;
    let definition = PreparedGraphPipelineAggregateText::prepare_with_parameter_types(
        "UNWIND [9223372036854775807,9223372036854775807] AS x \
         RETURN sum(x) AS total,avg(x) AS mean \
         HAVING total > $threshold AND mean = $half",
        &[
            ("threshold", GqlParameterType::UInt64),
            ("half", GqlParameterType::Int64),
        ],
        |_, _| panic!("source-free HAVING cannot access a catalog"),
    )
    .unwrap();
    let identity = definition.canonical_template_bytes();
    for (threshold, present) in [(u64::MAX - 2, true), (u64::MAX, false)] {
        let arguments = GqlParameters::new()
            .with_uint64("threshold", threshold)
            .unwrap()
            .with_int64("half", i64::MAX)
            .unwrap();
        let result = run(&definition.bind_relation_parameters(&arguments).unwrap());
        assert_eq!(result.value.len(), usize::from(present));
        if present {
            assert_eq!(
                result.value[0].values()[0].as_integer(),
                Some(i128::from(i64::MAX) * 2)
            );
            assert_eq!(
                result.value[0].values()[1].as_average(),
                GraphExactAverage::new(i128::from(i64::MAX), 1)
            );
        }
    }
    assert_eq!(definition.canonical_template_bytes(), identity);
}

#[test]
fn having_scalar_aliases_remain_lazy_but_boolean_leaves_and_input_errors_remain_eager() {
    for predicate in [
        "CASE WHEN count(*)=0 THEN 0 ELSE broken END > 0",
        "coalesce(0,broken) > 0",
    ] {
        let statement = format!("UNWIND [] AS x RETURN count(*)/0 AS broken HAVING {predicate}");
        assert!(run(&prepare(&statement)).value.is_empty(), "{predicate}");
    }
    for statement in [
        "UNWIND [1] AS x RETURN count(*) AS n HAVING n+1 > 0 OR 1/0=0 LIMIT 0",
        "UNWIND [0] AS x RETURN count(*) AS n HAVING sum(1/x)+1 > 0 LIMIT 0",
    ] {
        assert!(
            matches!(
                prepare(statement).execute_governed(policy(), no_source, || Ok(())),
                Err(GqlQueryError::Source(_))
            ),
            "{statement}"
        );
    }
}

#[test]
fn having_computed_parameters_rebind_once_and_keep_preparation_identity() {
    let definition = template(
        "UNWIND $values AS x RETURN count(*)+$delta AS adjusted \
         HAVING adjusted+$delta BETWEEN $minimum AND $maximum",
    );
    let identity = definition.canonical_template_bytes();
    assert_eq!(
        definition
            .parameter_schema()
            .iter()
            .find(|p| p.name == "delta")
            .unwrap()
            .occurrences,
        2
    );
    for (delta, minimum, maximum, present) in [(1, 4, 4, true), (3, 4, 4, false), (3, 8, 8, true)] {
        let arguments = GqlParameters::new()
            .with_list(
                "values",
                vec![GraphValue::Scalar(CanonicalScalar::Int(1)); 2],
            )
            .unwrap()
            .with_int64("delta", delta)
            .unwrap()
            .with_int64("minimum", minimum)
            .unwrap()
            .with_int64("maximum", maximum)
            .unwrap();
        let rows = run(&definition.bind_relation_parameters(&arguments).unwrap()).value;
        assert_eq!(rows.len(), usize::from(present));
        if present {
            assert_eq!(
                rows[0].values()[0].as_integer(),
                Some(i128::from(2 + delta))
            );
        }
    }
    assert_eq!(definition.canonical_template_bytes(), identity);
    assert!(
        definition
            .bind_relation_parameters(&GqlParameters::new())
            .is_err()
    );
}

#[test]
fn having_chained_comparisons_keep_row_filter_semantics_and_single_parameter_registration() {
    let legacy = prepare("UNWIND [1,1,2] AS k RETURN k,count(*) AS n HAVING 0 < n < 3 ORDER BY k");
    let rows = run(&legacy).value;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values()[0].as_count(), Some(2));
    assert_eq!(rows[1].values()[0].as_count(), Some(1));

    let definition =
        template("UNWIND [1,1,2] AS k RETURN k,count(*) AS n HAVING 0 < $pivot < n+1 ORDER BY k");
    assert_eq!(definition.parameter_schema()[0].occurrences, 1);
    let arguments = GqlParameters::new().with_int64("pivot", 2).unwrap();
    let rows = run(&definition.bind_relation_parameters(&arguments).unwrap()).value;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].keys(),
        &[GraphValue::Scalar(CanonicalScalar::Int(1))]
    );
    assert_eq!(rows[0].values()[0].as_count(), Some(2));
}

#[test]
fn computed_having_is_governed_before_final_pagination() {
    let plan = prepare(
        "UNWIND [1,1,2,3,3,3] AS x RETURN x AS k,count(*)+1 AS n \
         HAVING n+sum(x) > 4 ORDER BY k LIMIT 1",
    );
    let mut checkpoints = 0;
    let measured = plan
        .execute_governed(policy(), no_source, || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(measured.value.len(), 1);
    assert_eq!(measured.rows.snapshot_records, 0);
    let caps = [
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        plan.execute_governed(
            GqlQueryPolicy::new(0, caps[0], caps[1], caps[2]),
            no_source,
            || Ok(())
        )
        .unwrap(),
        measured
    );
    for dimension in 0..caps.len() {
        let mut bounds = caps;
        bounds[dimension] -= 1;
        assert!(
            plan.execute_governed(
                GqlQueryPolicy::new(0, bounds[0], bounds[1], bounds[2]),
                no_source,
                || Ok(())
            )
            .is_err()
        );
    }
    for stop in 1..=checkpoints {
        let mut current = 0;
        let result = plan.execute_governed(policy(), no_source, || {
            current += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(found)) if found == stop));
        assert_eq!(current, stop);
    }
}

#[test]
fn heterogeneous_extrema_use_language_order_and_keep_typed_numeric_ties() {
    let scalar = |value| GraphValue::Scalar(value);
    for (values, low, high) in [
        (
            "[1,2.0,5,3.2,NULL]",
            scalar(CanonicalScalar::Int(1)),
            scalar(CanonicalScalar::Int(5)),
        ),
        (
            "[1,'a',[1,2],0.2,'b']",
            GraphValue::List(
                vec![
                    scalar(CanonicalScalar::Int(1)),
                    scalar(CanonicalScalar::Int(2)),
                ]
                .into_boxed_slice(),
            ),
            scalar(CanonicalScalar::Int(1)),
        ),
        (
            "[1.0,1]",
            scalar(CanonicalScalar::Int(1)),
            scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(1.0))),
        ),
        (
            "[1,1.0]",
            scalar(CanonicalScalar::Int(1)),
            scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(1.0))),
        ),
    ] {
        let result = run(&prepare(&format!(
            "UNWIND {values} AS x RETURN min(x) AS low,max(x) AS high"
        )));
        assert_eq!(result.value.len(), 1);
        assert_eq!(result.value[0].values()[0].as_value(), Some(&low));
        assert_eq!(result.value[0].values()[1].as_value(), Some(&high));
    }
}

#[test]
fn return_and_with_sort_heterogeneous_lists_before_pagination() {
    use fgdb_gql::PreparedGraphSetText;
    let text = |value: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap());
    let int = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let expected: Vec<_> = [
        vec![],
        vec![text("a")],
        vec![text("a"), int(1)],
        vec![int(1)],
        vec![int(1), text("a")],
        vec![int(1), null.clone()],
        vec![null.clone(), int(1)],
        vec![null, int(2)],
    ]
    .into_iter()
    .map(|value| GraphValue::List(value.into_boxed_slice()))
    .collect();
    for tail in [
        "RETURN x ORDER BY x SKIP 1 LIMIT 6",
        "WITH x ORDER BY x SKIP 1 LIMIT 6 RETURN x ORDER BY x",
    ] {
        let query = PreparedGraphSetText::prepare(
            &format!(
                "UNWIND [[1],[1,NULL],['a',1],[NULL,2],[],[1,'a'],['a'],[NULL,1]] AS x {tail}"
            ),
            |_, _| None,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let result = query
            .execute_governed(policy(), no_source, || Ok(()))
            .unwrap();
        let actual: Vec<_> = result
            .value
            .iter()
            .map(|row| row.values()[0].clone())
            .collect();
        assert_eq!(actual, expected[1..7]);
    }
}

#[test]
fn standalone_aggregate_return_starts_with_one_real_empty_row() {
    let row = run(&prepare(
        "RETURN COUNT(*) AS rows,SUM(7) AS total,AVG(7) AS mean,COUNT(NULL) AS present",
    ))
    .value
    .remove(0);
    assert!(row.keys().is_empty());
    assert_eq!(row.values()[0].as_count(), Some(1));
    assert_eq!(row.values()[1].as_integer(), Some(7));
    assert_eq!(row.values()[2].as_average(), GraphExactAverage::new(7, 1));
    assert_eq!(row.values()[3].as_count(), Some(0));
    assert!(
        template("RETURN COUNT(*)")
            .bind_parameters(&GqlParameters::new())
            .is_err(),
        "the iterator API still requires one actual graph source"
    );
}

#[test]
fn empty_unwind_distinguishes_keyless_and_keyed_aggregation() {
    for prefix in ["UNWIND [] AS x", "UNWIND [1] AS x WITH x WHERE FALSE"] {
        let result = run(&prepare(&format!(
            "{prefix} RETURN COUNT(*) AS rows,SUM(x) AS total,AVG(x) AS mean"
        )));
        assert_eq!(result.value.len(), 1);
        assert_eq!(result.value[0].values()[0].as_count(), Some(0));
        assert!(
            result.value[0].values()[1..]
                .iter()
                .all(|value| value.is_null())
        );
        assert!(
            run(&prepare(&format!(
                "{prefix} RETURN x AS key,COUNT(*) AS rows"
            )))
            .value
            .is_empty()
        );
    }
}

#[test]
fn parameter_lists_preserve_nulls_occurrences_exact_averages_and_frozen_bindings() {
    let text = "UNWIND $xs AS x RETURN COUNT(*) AS rows,COUNT(x) AS present, \
        COUNT(DISTINCT x) AS unique,SUM(x*$step) AS total,AVG(x*$step) AS mean";
    let template = template(text);
    let arguments = |step| {
        let mut args = GqlParameters::new().with_int64("step", step).unwrap();
        let values = [
            CanonicalScalar::Int(5),
            CanonicalScalar::Int(5),
            CanonicalScalar::Null,
            CanonicalScalar::Int(9),
        ];
        args.insert(
            "xs",
            GqlParameterValue::List(
                GqlListParameter::new(values.into_iter().map(GraphValue::Scalar).collect())
                    .unwrap(),
            ),
        )
        .unwrap();
        args
    };
    assert_eq!(template.parameter_schema()[0].name, "xs");
    assert_eq!(template.parameter_schema()[0].occurrences, 1);
    assert_eq!(template.parameter_schema()[1].occurrences, 2);
    let bound = template.bind_relation_parameters(&arguments(2)).unwrap();
    let frozen = bound.canonical_bytes();
    let result = run(&bound);
    assert_eq!(result.value.len(), 1);
    let row = &result.value[0];
    assert_eq!(row.values()[0].as_count(), Some(4));
    assert_eq!(row.values()[1].as_count(), Some(3));
    assert_eq!(row.values()[2].as_count(), Some(2));
    assert_eq!(row.values()[3].as_integer(), Some(38));
    assert_eq!(row.values()[4].as_average(), GraphExactAverage::new(38, 3));
    let changed = template.bind_relation_parameters(&arguments(3)).unwrap();
    assert_ne!(changed.canonical_bytes(), frozen);
    assert_eq!(bound.canonical_bytes(), frozen);
    assert_eq!(run(&changed).value[0].values()[3].as_integer(), Some(57));
    assert!(
        template
            .bind_relation_parameters(&GqlParameters::new())
            .is_err()
    );
    let wrong = GqlParameters::new()
        .with_int64("xs", 4)
        .unwrap()
        .with_int64("step", 2)
        .unwrap();
    assert!(template.bind_relation_parameters(&wrong).is_err());
    assert!(
        template
            .bind_relation_parameters(&arguments(2).with_int64("extra", 7).unwrap(),)
            .is_err()
    );
}

#[test]
fn implicit_computed_keys_aliases_having_order_and_group_page_share_one_schema() {
    let head = "UNWIND [1,2,2,3,NULL] AS x \
        RETURN x/2 AS bucket,x/2 AS repeated,SUM(x+1) AS total,COUNT(*) AS rows";
    let clauses = " HAVING rows > 1 ORDER BY bucket DESC SKIP 0 LIMIT 1";
    let implicit = template(&format!("{head}{clauses}"));
    let explicit = template(&format!("{head} GROUP BY bucket{clauses}"));
    assert_eq!(
        implicit.canonical_template_bytes(),
        explicit.canonical_template_bytes()
    );
    assert_eq!(
        implicit.output_slots(),
        &[
            GraphAggregateTextSlot::GroupKey(0),
            GraphAggregateTextSlot::GroupKey(0),
            GraphAggregateTextSlot::Aggregate(0),
            GraphAggregateTextSlot::Aggregate(1),
        ]
    );
    let result = run(&implicit
        .bind_relation_parameters(&GqlParameters::new())
        .unwrap());
    assert_eq!(result.value.len(), 1);
    let row = &result.value[0];
    assert_eq!(row.keys(), &[GraphValue::Scalar(CanonicalScalar::Int(1))]);
    assert_eq!(row.values()[0].as_integer(), Some(10));
    assert_eq!(row.values()[1].as_count(), Some(3));
}

#[test]
fn with_distinct_filter_and_input_page_precede_computed_aggregate_arguments() {
    let row = run(&prepare(
        "WITH [0,2,2,5,9] AS xs UNWIND xs AS x WITH DISTINCT x WHERE x <> 0 \
         WITH x ORDER BY x LIMIT 2 RETURN SUM(10/x) AS total,COUNT(*) AS rows",
    ))
    .value
    .remove(0);
    assert_eq!(row.values()[0].as_integer(), Some(7));
    assert_eq!(row.values()[1].as_count(), Some(2));
    let row = run(&prepare(
        "UNWIND [[1,2],[3]] AS xs UNWIND xs AS x RETURN SUM(x) AS total",
    ))
    .value
    .remove(0);
    assert_eq!(row.values()[0].as_integer(), Some(6));
}

#[test]
fn aggregate_width_is_not_narrowed_to_the_scalar_domain() {
    let plan = prepare(
        "UNWIND [9223372036854775807,9223372036854775807] AS x \
         RETURN SUM(x+0) AS total,AVG(x+0) AS mean",
    );
    let row = run(&plan).value.remove(0);
    assert_eq!(row.values()[0].as_integer(), Some(i128::from(i64::MAX) * 2));
    assert_eq!(
        row.values()[1].as_average(),
        GraphExactAverage::new(i128::from(i64::MAX), 1)
    );
}

#[test]
fn limit_zero_cannot_hide_input_arithmetic_errors_or_cancelled_execution() {
    for text in [
        "RETURN SUM(1/0) AS total LIMIT 0",
        "UNWIND [0] AS x RETURN SUM(10/x) AS total LIMIT 0",
        "UNWIND [0] AS x RETURN 10/x AS key,COUNT(*) AS rows LIMIT 0",
    ] {
        let result = prepare(text).execute_governed(policy(), no_source, || Ok(()));
        assert!(matches!(result, Err(GqlQueryError::Source(_))), "{text}");
    }
    let empty = prepare("UNWIND [] AS x RETURN COUNT(1/0) AS rows LIMIT 0");
    assert!(run(&empty).value.is_empty());
    assert!(matches!(
        empty.execute_governed(policy(), no_source, || Err(17)),
        Err(GqlQueryError::Interrupted(17))
    ));
}

#[test]
fn zero_source_rows_work_scratch_and_cancellation_share_one_allowance() {
    let plan = prepare("UNWIND [1,1,2] AS x RETURN x AS key,SUM(x+1) AS total ORDER BY key DESC");
    let mut checkpoints = 0;
    let measured = plan
        .execute_governed(policy(), no_source, || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
    assert!(checkpoints > 0);
    assert_eq!(measured.rows.snapshot_records, 0);
    let caps = [
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert!(caps.iter().all(|cap| *cap > 0));
    let exact = GqlQueryPolicy::new(0, caps[0], caps[1], caps[2]);
    assert_eq!(
        plan.execute_governed(exact, no_source, || Ok(())).unwrap(),
        measured
    );
    for dimension in 0..3 {
        let mut bound = caps;
        bound[dimension] -= 1;
        assert!(
            plan.execute_governed(
                GqlQueryPolicy::new(0, bound[0], bound[1], bound[2]),
                no_source,
                || Ok(()),
            )
            .is_err()
        );
    }
    for stop in 1..=checkpoints {
        let mut at = 0;
        let result = plan.execute_governed(policy(), no_source, || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(found)) if found == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn real_singleton_depth_is_admitted_without_a_phantom_graph_level() {
    for (argument, repeats) in [
        ("x", fgdb_gql::MAX_GRAPH_SET_DEPTH - 3),
        ("x+1", fgdb_gql::MAX_GRAPH_SET_DEPTH - 4),
    ] {
        let text = format!(
            "WITH 1 AS x{} RETURN SUM({argument}) AS total",
            " WITH x".repeat(repeats)
        );
        let row = run(&prepare(&text)).value.remove(0);
        assert_eq!(
            row.values()[0].as_integer(),
            Some(if argument == "x" { 1 } else { 2 })
        );
        let excess = format!(
            "WITH 1 AS x{} RETURN SUM({argument}) AS total",
            " WITH x".repeat(repeats + 1)
        );
        assert!(
            PreparedGraphPipelineAggregateText::prepare(&excess, |_, _| {
                panic!("depth failure must precede catalog access")
            })
            .is_err()
        );
    }
}

#[test]
fn invalid_terminal_shapes_and_parameters_fail_before_any_source_or_catalog() {
    // An aggregate WITH stage is legal since fgdb-ezgeq, so this shape moved
    // from the refusal list to an executed answer: one singleton row counted
    // once, then summed.
    let staged = run(&prepare("WITH COUNT(*) AS n RETURN SUM(n)")).value;
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].values()[0].as_integer(), Some(1));
    for text in [
        "UNWIND [1] AS x RETURN SUM(SUM(x))",
        "UNWIND [1] AS x RETURN SUM(missing)",
        "UNWIND [1] AS x WITH x AS y RETURN SUM(x)",
        "UNWIND [1] AS x RETURN x,COUNT(*) AS n GROUP BY x+1",
        "UNWIND [1] AS x RETURN SUM(x+$p) AS n LIMIT $p",
        "RETURN COUNT(DISTINCT *)",
        "RETURN COUNT(*) AS n,SUM(1) AS n",
        "RETURN COUNT(*) LIMIT 0 MATCH (n)",
    ] {
        assert!(
            PreparedGraphPipelineAggregateText::prepare(text, |_, _| {
                panic!("invalid syntax must not resolve symbols")
            })
            .is_err(),
            "{text}"
        );
    }
    // A leading UNWIND joined by MATCH feeds an aggregate RETURN (lane6,
    // pipeline_aggregate_text proves its answers). It names no symbol, so it
    // prepares without a catalog call.
    assert!(
        PreparedGraphPipelineAggregateText::prepare(
            "UNWIND [1] AS x MATCH (n) RETURN COUNT(*)",
            |_, _| panic!("a symbol-free statement must not resolve symbols")
        )
        .is_ok()
    );
    let text = "\u{2003}WITH $p AS x RETURN SUM(x) AS total";
    let missing = template(text)
        .bind_relation_parameters(&GqlParameters::new())
        .unwrap_err();
    assert_eq!(missing.offset, text.find("$p").unwrap());
}
