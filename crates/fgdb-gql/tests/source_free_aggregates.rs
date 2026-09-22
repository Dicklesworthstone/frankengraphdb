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
    for text in [
        "WITH COUNT(*) AS n RETURN SUM(n)",
        "UNWIND [1] AS x RETURN SUM(SUM(x))",
        "UNWIND [1] AS x RETURN SUM(missing)",
        "UNWIND [1] AS x WITH x AS y RETURN SUM(x)",
        "UNWIND [1] AS x RETURN x,COUNT(*) AS n GROUP BY x+1",
        "UNWIND [1] AS x RETURN SUM(x+$p) AS n LIMIT $p",
        "RETURN COUNT(DISTINCT *)",
        "RETURN COUNT(*) AS n,SUM(1) AS n",
        "RETURN COUNT(*) LIMIT 0 MATCH (n)",
        "UNWIND [1] AS x MATCH (n) RETURN COUNT(*)",
    ] {
        assert!(
            PreparedGraphPipelineAggregateText::prepare(text, |_, _| {
                panic!("invalid syntax must not resolve symbols")
            })
            .is_err(),
            "{text}"
        );
    }
    let text = "\u{2003}WITH $p AS x RETURN SUM(x) AS total";
    let missing = template(text)
        .bind_relation_parameters(&GqlParameters::new())
        .unwrap_err();
    assert_eq!(missing.offset, text.find("$p").unwrap());
}
