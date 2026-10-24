//! Compound output fields use native row expressions after set arithmetic.
//! No graph source is available to these tests: an accidental source read fails.
use fgdb_gql::algebra::{GraphValueOrder, GraphValueRow};
use fgdb_gql::{
    GqlParameterType, GqlParameterValue, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphSetOperation, GraphSetQuantifier, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::CanonicalScalar;
use std::cell::Cell;

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000)
}

fn prepare(text: &str) -> PreparedGraphSetText {
    PreparedGraphSetText::prepare(text, |_, _| None).expect(text)
}

fn plan(text: &str) -> PreparedGraphSet {
    prepare(text)
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn rows(plan: &PreparedGraphSet) -> Vec<GraphValueRow> {
    plan.execute_governed(
        policy(),
        |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
        || Ok::<_, usize>(()),
    )
    .expect("source-free set ordering")
    .value
}

fn first_column(rows: &[GraphValueRow]) -> Vec<i64> {
    rows.iter()
        .map(|row| match row.get(0).unwrap().as_scalar().unwrap() {
            CanonicalScalar::Int(value) => *value,
            other => panic!("expected integer fixture, got {other:?}"),
        })
        .collect()
}

#[test]
fn map_field_ordering_preserves_all_six_set_bags() {
    for (operator, expected) in [
        ("UNION ALL", vec![4, 3, 2, 2, 2, 1, 1, 1]),
        ("UNION DISTINCT", vec![4, 3, 2, 1]),
        ("INTERSECT ALL", vec![2, 1]),
        ("INTERSECT DISTINCT", vec![2, 1]),
        ("EXCEPT ALL", vec![3, 1]),
        ("EXCEPT DISTINCT", vec![3]),
    ] {
        let text = format!(
            "UNWIND [1,1,2,3] AS x RETURN x AS value, {{rank: -x}} AS doc \
             {operator} UNWIND [1,2,2,4] AS y \
             RETURN y AS value, {{rank: -y}} AS doc ORDER BY doc.rank"
        );
        let bound = plan(&text);
        let output = rows(&bound);
        assert_eq!(first_column(&output), expected, "{text}");
        assert_eq!(bound.columns(), &["value", "doc"]);
        assert!(output.iter().all(|row| row.get(2).is_none()));
    }
}

#[test]
fn union_alignment_precedes_map_field_evaluation() {
    let bound = plan(
        "RETURN 1 AS value, {rank: 9} AS doc \
         UNION ALL RETURN {rank: 1} AS doc, 2 AS value \
         ORDER BY doc.rank ASCENDING",
    );
    assert_eq!(bound.columns(), &["value", "doc"]);
    let output = rows(&bound);
    assert_eq!(first_column(&output), vec![2, 1]);
    assert!(output.iter().all(|row| row.get(2).is_none()));
}

#[test]
fn nested_fields_missing_values_and_explicit_nulls_keep_null_placement() {
    let input = "RETURN 1 AS value, {meta: {rank: 9}} AS doc \
                 UNION ALL RETURN 2 AS value, {meta: {rank: 4}} AS doc \
                 UNION ALL RETURN 3 AS value, {} AS doc \
                 UNION ALL RETURN 4 AS value, {meta: null} AS doc";
    assert_eq!(
        first_column(&rows(&plan(&format!(
            "{input} ORDER BY doc.meta.rank DESCENDING NULLS FIRST, value"
        )))),
        vec![3, 4, 1, 2]
    );
    assert_eq!(
        first_column(&rows(&plan(&format!(
            "{input} ORDER BY doc.meta.rank ASCENDING NULLS LAST, value DESC"
        )))),
        vec![2, 1, 4, 3]
    );
}

#[test]
fn map_ordering_keeps_operand_and_outer_pages_separate() {
    let text = "(RETURN 1 AS value, {rank: 3} AS doc \
                  UNION ALL RETURN 2 AS value, {rank: 1} AS doc \
                  ORDER BY doc.rank DESC LIMIT 1) \
                UNION ALL \
                (RETURN 3 AS value, {rank: 5} AS doc \
                  UNION ALL RETURN 4 AS value, {rank: 2} AS doc \
                  ORDER BY doc.rank DESC LIMIT 1) \
                ORDER BY doc.rank DESC";
    assert_eq!(first_column(&rows(&plan(text))), vec![3, 1]);
    assert_eq!(
        first_column(&rows(&plan(&format!("{text} SKIP 1 LIMIT 1")))),
        vec![1]
    );
    // A parenthesized single read also exposes an output-row boundary.
    assert_eq!(
        first_column(&rows(&plan(
            "(UNWIND [1,2,3] AS x RETURN x AS value, {rank: -x} AS doc) \
             ORDER BY doc.rank LIMIT 2"
        ))),
        vec![3, 2]
    );
}

#[test]
fn quoted_dotted_alias_keeps_the_original_column_only_plan() {
    let text = "RETURN 1 AS `doc.rank` UNION ALL RETURN 9 AS `doc.rank` \
                ORDER BY `doc.rank` DESC";
    let expected = plan("RETURN 1 AS `doc.rank`")
        .combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            plan("RETURN 9 AS `doc.rank`"),
        )
        .unwrap()
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap();
    let bound = plan(text);
    assert_eq!(bound.canonical_bytes(), expected.canonical_bytes());
    assert_eq!(first_column(&rows(&bound)), vec![9, 1]);
}

#[test]
fn map_page_rebinding_preserves_template_and_changes_bound_identity() {
    let text = "RETURN $seed AS value, {rank: 1} AS doc \
                UNION ALL RETURN 9 AS value, {rank: 2} AS doc \
                ORDER BY doc.rank DESC SKIP $skip LIMIT $take";
    let prepared = PreparedGraphSetText::prepare_with_parameter_types(
        text,
        &[
            ("seed", GqlParameterType::Int64),
            ("skip", GqlParameterType::UInt64),
            ("take", GqlParameterType::UInt64),
        ],
        |_, _| None,
    )
    .unwrap();
    let template = prepared.canonical_template_bytes();
    let mut identities = Vec::new();
    for (skip, take, expected) in [(0, 2, vec![9, 2]), (1, 1, vec![2])] {
        let mut arguments = GqlParameters::new();
        for (name, value) in [
            ("seed", GqlParameterValue::Int64(2)),
            ("skip", GqlParameterValue::UInt64(skip)),
            ("take", GqlParameterValue::UInt64(take)),
        ] {
            arguments.insert(name.to_owned(), value).unwrap();
        }
        let bound = prepared.bind_parameters(&arguments).unwrap();
        assert_eq!(first_column(&rows(&bound)), expected);
        assert_eq!(prepared.canonical_template_bytes(), template);
        identities.push(bound.canonical_bytes());
    }
    assert_ne!(identities[0], identities[1]);
}

#[test]
fn private_graph_bindings_refuse_before_catalog_access() {
    let input = "MATCH (n:A) RETURN n.p AS value \
                 UNION ALL MATCH (m:B) RETURN m.p AS value";
    for tail in [
        "ORDER BY n.p",
        "ORDER BY m.p DESC LIMIT 0",
        "ORDER BY value, n.p",
        "ORDER BY missing.rank",
    ] {
        let calls = Cell::new(0);
        let text = format!("{input} {tail}");
        let result = PreparedGraphSetText::prepare(&text, |_, _| {
            calls.set(calls.get() + 1);
            None
        });
        assert!(result.is_err(), "{text}");
        assert_eq!(calls.get(), 0, "out-of-scope lookup reached the catalog");
    }
}

#[test]
fn malformed_map_input_is_not_hidden_by_zero_limit() {
    let bound = plan(
        "RETURN 1 AS value, {rank: 3} AS doc \
         UNION ALL RETURN 2 AS value, 7 AS doc \
         ORDER BY doc.rank LIMIT 0",
    );
    let source_calls = Cell::new(0);
    let result = bound.execute_governed(
        policy(),
        |_, _| {
            source_calls.set(source_calls.get() + 1);
            Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1))
        },
        || Ok::<_, usize>(()),
    );
    assert_eq!(source_calls.get(), 0);
    assert!(matches!(result, Err(GqlQueryError::Source(_))));
    assert!(
        rows(&plan(
            "RETURN 1 AS value, {rank: 3} AS doc \
         UNION ALL RETURN 2 AS value, {} AS doc ORDER BY doc.rank LIMIT 0"
        ))
        .is_empty()
    );
}

#[test]
fn every_observed_cancellation_point_refuses_the_complete_page() {
    let bound = plan(
        "UNWIND [1,2,3] AS x RETURN x AS value, {rank: -x} AS doc \
         UNION ALL RETURN 4 AS value, {rank: -4} AS doc \
         ORDER BY doc.rank SKIP 1 LIMIT 2",
    );
    let calls = Cell::new(0);
    let baseline = bound
        .execute_governed(
            policy(),
            |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
            || {
                calls.set(calls.get() + 1);
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    assert_eq!(first_column(&baseline.value), vec![3, 2]);
    let total = calls.get();
    assert!(total > 0);
    for cut in 1..=total {
        calls.set(0);
        let result = bound.execute_governed(
            policy(),
            |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
            || {
                calls.set(calls.get() + 1);
                if calls.get() == cut { Err(cut) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == cut));
    }
}

#[test]
fn exhausted_admission_is_not_reset_by_the_compound_map_page() {
    let bound = plan(
        "RETURN 1 AS value, {rank: 3} AS doc \
         UNION ALL RETURN 2 AS value, {rank: 1} AS doc ORDER BY doc.rank",
    );
    let result = bound.execute_governed(
        GqlQueryPolicy::new(10_000, 10_000, 0, 0),
        |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
        || Ok::<_, usize>(()),
    );
    assert!(result.is_err());
}
