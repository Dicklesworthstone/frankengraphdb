//! Executable laws for composable WITH grouping (fgdb-ezgeq).
use super::*;
use crate::algebra::{GraphValue, GraphValueRow};
use crate::{GqlQueryError, GqlQueryExecution, GqlQueryPolicy, PreparedGraphSetText};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000)
}
fn int(n: i64) -> GraphValue { GraphValue::Scalar(CanonicalScalar::Int(n)) }
fn row(cells: Vec<GraphValue>) -> GraphValueRow { GraphValueRow::from_owned_values(cells) }
fn prepared(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, |_, _| None).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn run(query: &PreparedGraphSet, policy: GqlQueryPolicy)
    -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<crate::GraphSetExecutionError<&'static str>, &'static str>>
{
    query.execute_governed(policy, |_, _| Err(GqlQueryError::Source("unexpected graph source")), || Ok(()))
}
fn values(text: &str) -> Vec<GraphValueRow> { run(&prepared(text), policy()).unwrap().value }

#[test]
fn grouped_rows_feed_where_order_pages_and_restore_declared_column_order() {
    assert_eq!(values("UNWIND [2, 1, 2, 3, 3, 3] AS n \
        WITH count(*) AS c, n AS key WHERE c > 1 ORDER BY c DESC LIMIT 1 RETURN key, c"),
        vec![row(vec![int(3), int(3)])]);
    assert_eq!(values("UNWIND [1, 1, 2] AS n WITH n AS a, count(*) AS c, n AS b RETURN a, c, b"),
        vec![row(vec![int(1), int(2), int(1)]), row(vec![int(2), int(1), int(2)])]);
}

#[test]
fn collection_can_be_unwound_and_grouped_again_without_losing_duplicates() {
    assert_eq!(values("UNWIND [3, 1, 3, NULL, 2] AS n \
        WITH collect(n) AS xs UNWIND xs AS x WITH x, count(*) AS c RETURN x, c ORDER BY x"),
        vec![row(vec![int(1), int(1)]), row(vec![int(2), int(1)]), row(vec![int(3), int(2)])]);
    assert_eq!(values("UNWIND [3, 1, 3, NULL, 2] AS n \
        WITH collect(DISTINCT n) AS xs RETURN xs"),
        vec![row(vec![GraphValue::List(vec![int(3), int(1), int(2)].into_boxed_slice())])]);
}

#[test]
fn empty_keyless_and_keyed_inputs_keep_native_zero_null_and_empty_list_laws() {
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    assert_eq!(values("UNWIND [] AS n WITH count(*) AS c, sum(n) AS s, collect(n) AS xs RETURN c, s, xs"),
        vec![row(vec![int(0), null.clone(), GraphValue::List(Vec::new().into_boxed_slice())])]);
    assert!(values("UNWIND [] AS n WITH n, count(*) AS c RETURN n, c").is_empty());
    assert_eq!(values("UNWIND [NULL, NULL] AS n WITH count(*) AS a, count(n) AS b RETURN a, b"),
        vec![row(vec![int(2), int(0)])]);
    assert_eq!(values("WITH count(*) AS c RETURN c"), vec![row(vec![int(1)])]);
}

#[test]
fn argument_distinct_input_pages_and_output_distinct_are_separate_boundaries() {
    assert_eq!(values("UNWIND [3, 1, 1, 2] AS n WITH n ORDER BY n LIMIT 3 \
        WITH sum(n) AS s, sum(DISTINCT n) AS d, count(DISTINCT n) AS c RETURN s, d, c"),
        vec![row(vec![int(4), int(3), int(2)])]);
    assert_eq!(values("UNWIND [1, 1, 2, 2] AS n WITH n, count(*) AS c WITH DISTINCT c RETURN c"),
        vec![row(vec![int(2)])]);
    assert_eq!(values("UNWIND [1, 1, 2, 2] AS n WITH n, count(*) AS c WITH sum(c) AS total RETURN total"),
        vec![row(vec![int(4)])]);
}

#[test]
fn bindings_are_reusable_typed_and_value_independent_in_template_identity() {
    let text = "UNWIND [1, 2, 2] AS n WITH n + $shift AS key, count(*) AS c \
        WHERE c >= $floor RETURN key, c ORDER BY key";
    let query = PreparedGraphSetText::prepare(text, |_, _| None).unwrap();
    let frozen = query.canonical_template_bytes();
    let bind = |shift, floor| {
        let mut args = GqlParameters::new();
        args.insert(String::from("shift"), GqlParameterValue::Int64(shift)).unwrap();
        args.insert(String::from("floor"), GqlParameterValue::Int64(floor)).unwrap();
        query.bind_parameters(&args).unwrap()
    };
    let first = bind(10, 2);
    let second = bind(20, 1);
    assert_eq!(run(&first, policy()).unwrap().value, vec![row(vec![int(12), int(2)])]);
    assert_eq!(run(&second, policy()).unwrap().value,
        vec![row(vec![int(21), int(1)]), row(vec![int(22), int(2)])]);
    assert_ne!(first.canonical_bytes(), second.canonical_bytes());
    assert_eq!(query.canonical_template_bytes(), frozen);
    assert!(query.bind_parameters(&GqlParameters::new()).is_err());
}

#[test]
fn invalid_group_scopes_and_nested_aggregates_refuse_before_catalog_calls() {
    for text in [
        "MATCH (n:L) WITH n AS x WITH count(*) AS c RETURN x",
        "MATCH (n:L) WITH n AS x WITH count(*) AS c, c AS next RETURN next",
        "MATCH (n:L) WITH n AS x WITH count(*) AS c, count(x) AS c RETURN c",
        "MATCH (n:L) WITH n AS x WITH sum(count(x)) AS c RETURN c",
        "MATCH (n:L) WITH n AS x WITH count(DISTINCT *) AS c RETURN c",
        "MATCH (n:L) WITH n AS x WITH sum([1, 2]) AS c RETURN c",
    ] {
        let mut calls = 0;
        let result = PreparedGraphSetText::prepare(text, |_, _| { calls += 1; None });
        assert!(result.is_err(), "{text}");
        assert_eq!(calls, 0, "{text}");
    }
}

#[test]
fn aggregate_tokens_in_strings_or_plain_aliases_do_not_select_grouping() {
    assert_eq!(values("WITH 3 AS count WITH count AS value RETURN value"), vec![row(vec![int(3)])]);
    assert_eq!(values("WITH 'count(x)' AS text RETURN size(text) AS n"), vec![row(vec![int(8)])]);
    assert_eq!(values("UNWIND [1, 2] AS n WITH n AS sum WITH sum AS n RETURN n"),
        vec![row(vec![int(1)]), row(vec![int(2)])]);
}

#[test]
fn intermediate_groups_do_not_spend_public_rows_and_all_work_remains_cumulative() {
    let query = prepared("UNWIND [1, 2, 3, 4] AS n WITH n, count(*) AS c RETURN n LIMIT 1");
    let baseline = run(&query, policy()).unwrap();
    let exact = GqlQueryPolicy::new(0, 1, baseline.evaluator.work_units, baseline.evaluator.scratch_entries);
    assert_eq!(run(&query, exact).unwrap().value, baseline.value);
    for restricted in [
        GqlQueryPolicy::new(0, 0, baseline.evaluator.work_units, baseline.evaluator.scratch_entries),
        GqlQueryPolicy::new(0, 1, baseline.evaluator.work_units - 1, baseline.evaluator.scratch_entries),
        GqlQueryPolicy::new(0, 1, baseline.evaluator.work_units, baseline.evaluator.scratch_entries - 1),
    ] { assert!(run(&query, restricted).is_err()); }
}

#[test]
fn numeric_failures_remain_visible_beneath_empty_output_pages() {
    for text in [
        "UNWIND [9223372036854775807, 1] AS n WITH sum(n) AS s RETURN s LIMIT 0",
        "UNWIND [1, 2] AS n WITH avg(n) AS a RETURN a LIMIT 0",
        "UNWIND [1, 'bad'] AS n WITH sum(n) AS s RETURN s LIMIT 0",
        "UNWIND [1, 0] AS n WITH sum(1 / n) AS s RETURN s LIMIT 0",
    ] { assert!(run(&prepared(text), policy()).is_err(), "{text}"); }
}

#[test]
fn cancellation_at_every_observed_checkpoint_releases_no_result() {
    let query = prepared("UNWIND [1, 2, 2] AS n WITH n, collect(n) AS xs UNWIND xs AS x RETURN x");
    let mut checkpoints = 0;
    query.execute_governed(policy(), |_, _| Err(GqlQueryError::Source("source")), || {
        checkpoints += 1; Ok::<_, &'static str>(())
    }).unwrap();
    assert!(checkpoints > 10);
    for stop in 0..checkpoints {
        let mut seen = 0;
        let result = query.execute_governed(policy(), |_, _| Err(GqlQueryError::Source("source")), || {
            let refuse = seen == stop; seen += 1;
            if refuse { Err("cancelled") } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted("cancelled"))), "{stop}");
    }
    assert_eq!(run(&query, policy()).unwrap().value, vec![row(vec![int(1)]), row(vec![int(2)]), row(vec![int(2)])]);
}

#[test]
fn grouping_after_a_graph_projection_admits_the_source_exactly_once() {
    let query = prepared("MATCH (n) WITH n WITH count(*) AS c RETURN c");
    let mut calls = 0;
    let result = query.execute_governed(GqlQueryPolicy::new(3, 1, 100_000, 100_000), |pattern, remaining| {
        calls += 1;
        pattern.plan().execute_governed_with_properties(3, [fgdb_types::VId(1), fgdb_types::VId(2), fgdb_types::VId(3)], [],
            |_, _| Ok::<_, &'static str>(true), |_, _| Ok(None), remaining, || Ok::<_, &'static str>(()))
    }, || Ok::<_, &'static str>(())).unwrap();
    assert_eq!(calls, 1);
    assert_eq!(result.rows.snapshot_records, 3);
    assert_eq!(result.value, vec![row(vec![int(3)])]);
}

#[test]
fn all_group_nodes_count_toward_the_shared_pre_catalog_depth_cap() {
    let mut text = String::from("MATCH (n:L) WITH n ");
    for _ in 0..22 { text.push_str("WITH count(*) AS c "); }
    text.push_str("RETURN c");
    let mut calls = 0;
    assert!(PreparedGraphSetText::prepare(&text, |_, _| { calls += 1; None }).is_err());
    assert_eq!(calls, 0);
}
