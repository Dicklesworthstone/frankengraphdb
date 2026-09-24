//! Native WITH stages use one GLA source and the existing relational executor.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GraphColumn, GraphPatternBuilder, GraphValueOrder, GraphValueRow, IntegerComparison,
};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GqlScalarParameter,
    GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphSetExecutionError,
    GraphSetOperand as Arg, GraphSetPredicateOp as Op, GraphSetProjection, GraphSetQuantifier,
    GraphSetTextErrorKind, GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphSet,
    PreparedGraphSetText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::{Cell, RefCell};
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
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn prepare(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn run(
    query: &PreparedGraphSet,
    values: &[CanonicalScalar],
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), usize>,
) -> Result<
    GqlQueryExecution<GraphValueRow>,
    GqlQueryError<GraphSetExecutionError<&'static str>, usize>,
> {
    let checkpoint = RefCell::new(checkpoint);
    query.execute_governed(
        policy,
        |pattern, remaining| {
            pattern.plan().execute_governed_with_properties(
                values.len() as u64,
                (0..values.len()).map(|at| VId(at as u128)),
                [],
                |vid, predicates| {
                    Ok::<_, &'static str>(
                        predicates
                            .iter()
                            .all(|test| test.matches(&[], &[(P, values[vid.0 as usize].clone())])),
                    )
                },
                |vid, _| Ok(Some(&values[vid.0 as usize])),
                remaining,
                || (checkpoint.borrow_mut())(),
            )
        },
        || (checkpoint.borrow_mut())(),
    )
}
fn ints(rows: &[GraphValueRow], column: usize) -> Vec<Option<i64>> {
    rows.iter()
        .map(|row| match row.get(column).unwrap().as_scalar() {
            Some(CanonicalScalar::Int(n)) => Some(*n),
            Some(CanonicalScalar::Null) => None,
            _ => panic!("expected nullable integer output"),
        })
        .collect()
}
fn integer(column: usize, n: i64, op: GraphIntegerBinary) -> GraphSetValue {
    GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Column(column),
            GraphIntegerOp::Literal(Some(n)),
            GraphIntegerOp::Binary(op),
        ])
        .unwrap(),
    )
}

#[test]
fn native_pipeline_matches_typed_projection_filter_and_page_composition() {
    let text = "MATCH (n) WITH n AS owner,n.p+1 AS score ORDER BY score DESC LIMIT $top WHERE score > $min WITH owner,score%2 AS bucket RETURN owner,bucket ORDER BY owner";
    let mut calls = BTreeSet::new();
    let template = PreparedGraphSetText::prepare(text, |kind, name| {
        assert!(calls.insert((kind, name.to_owned())));
        symbols(kind, name)
    })
    .unwrap();
    let args = GqlParameters::new()
        .with_uint64("top", 3)
        .unwrap()
        .with_int64("min", 2)
        .unwrap();
    let actual = template.bind_parameters(&args).unwrap();
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input: PreparedGraphSet = builder
        .prepare_values(
            &[
                GraphColumn::vertex("n", "n"),
                GraphColumn::property("p", "n", P),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into();
    let expected = input
        .project(
            vec![
                GraphSetProjection::new("owner", GraphSetValue::Column(0)),
                GraphSetProjection::new("score", integer(1, 1, GraphIntegerBinary::Add)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_order_by(&[GraphValueOrder::descending(1)])
        .unwrap()
        .with_page(0, Some(3))
        .filter(&[Op::Compare {
            left: Arg::Column(1),
            comparison: IntegerComparison::Greater,
            right: Arg::Literal(GqlScalarParameter::new(CanonicalScalar::Int(2)).unwrap()),
        }])
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("owner", GraphSetValue::Column(0)),
                GraphSetProjection::new("bucket", integer(1, 2, GraphIntegerBinary::Remainder)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("owner", GraphSetValue::Column(0)),
                GraphSetProjection::new("bucket", GraphSetValue::Column(1)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_order_by(&[GraphValueOrder::ascending(0)])
        .unwrap();
    // Private source aliases are not public schema or logical plan identity.
    assert_eq!(actual.canonical_bytes(), expected.canonical_bytes());
    assert_eq!(template.bind_parameters(&args).unwrap(), actual);
    assert_eq!(calls.len(), 1);
    assert_eq!(template.columns(), &["owner", "bucket"]);
    let values = [1, 2, 3, 4].map(CanonicalScalar::Int);
    let rows = run(&actual, &values, wide(), &mut || Ok(())).unwrap().value;
    assert_eq!(
        rows.iter()
            .map(|row| row.get(0).unwrap().as_vertex().unwrap())
            .collect::<Vec<_>>(),
        vec![VId(1), VId(2), VId(3)]
    );
    assert_eq!(ints(&rows, 1), vec![Some(1), Some(0), Some(1)]);
}

#[test]
fn each_stage_preserves_bags_and_its_page_before_the_next_filter() {
    let values = [1, 2, 2, 3, 4].map(CanonicalScalar::Int);
    for (text, expected) in [
        ("MATCH (n) WITH n.p AS p RETURN p", vec![1, 2, 2, 3, 4]),
        (
            "MATCH (n) WITH DISTINCT n.p AS p RETURN p",
            vec![1, 2, 3, 4],
        ),
        (
            "MATCH (n) WITH n.p AS p ORDER BY p DESC LIMIT 2 WHERE p < 4 RETURN p",
            vec![3],
        ),
        (
            "MATCH (n) WITH n.p AS p WHERE p < 4 WITH p ORDER BY p DESC LIMIT 2 RETURN p",
            vec![2, 3],
        ),
        (
            "MATCH (n) WITH n.p AS p WITH 7 AS x RETURN x",
            vec![7, 7, 7, 7, 7],
        ),
        (
            "MATCH (n) WITH n.p AS p WITH DISTINCT 7 AS x RETURN x",
            vec![7],
        ),
    ] {
        let query = prepare(text);
        let result = run(&query, &values, wide(), &mut || Ok(())).unwrap();
        assert_eq!(result.rows.snapshot_records, values.len() as u64);
        assert_eq!(
            ints(&result.value, 0),
            expected.into_iter().map(Some).collect::<Vec<_>>(),
            "{text}"
        );
    }
}

#[test]
fn alias_arithmetic_case_and_coalesce_reuse_checked_lazy_execution() {
    let values = [
        CanonicalScalar::Int(0),
        CanonicalScalar::Int(2),
        CanonicalScalar::Null,
    ];
    let query = prepare(
        "MATCH (n) WITH n,n.p AS value WITH n,CASE WHEN value IS NULL THEN 7 WHEN value=0 THEN 99 ELSE 10/value END AS score RETURN n,COALESCE(score,1/0) AS result ORDER BY n",
    );
    assert_eq!(
        ints(
            &run(&query, &values, wide(), &mut || Ok(())).unwrap().value,
            1
        ),
        vec![Some(99), Some(5), Some(7)]
    );
    let safe = prepare("MATCH (n) WITH n.p AS value WHERE value <> 0 RETURN 10/value AS result");
    assert_eq!(
        ints(
            &run(&safe, &values, wide(), &mut || Ok(())).unwrap().value,
            0
        ),
        vec![Some(5)]
    );
    let unsafe_page = prepare("MATCH (n) WITH n.p AS value RETURN 10/value AS result LIMIT 0");
    assert!(matches!(
        run(&unsafe_page, &values, wide(), &mut || Ok(())),
        Err(GqlQueryError::Source(
            GraphSetExecutionError::Projection { .. }
        ))
    ));
}

#[test]
fn nullable_aliases_and_boolean_filters_do_not_turn_unknown_into_true() {
    let values = [
        CanonicalScalar::Null,
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(2),
        CanonicalScalar::Bool(true),
    ];
    let query =
        prepare("MATCH (n) WITH n,n.p AS value WHERE NOT (value=1 OR value IS NULL) RETURN n");
    let result = run(&query, &values, wide(), &mut || Ok(())).unwrap();
    assert_eq!(result.value.len(), 1);
    assert_eq!(result.value[0].get(0).unwrap().as_vertex(), Some(VId(2)));
    let absent =
        prepare("MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH n,m WHERE m IS NULL RETURN n,m");
    let result = run(&absent, &values, wide(), &mut || Ok(())).unwrap();
    assert_eq!(result.value.len(), 4);
    assert!(result.value.iter().all(|row| row.get(1).unwrap().is_null()));
}

#[test]
fn computed_with_predicates_filter_the_projected_rows() {
    // 15c37d16 made computed WITH predicates and Boolean row aliases legal;
    // both forms used to be on the refusal list below. A NULL input makes the
    // comparison unknown, and unknown is filtered out, never kept.
    let values = [
        CanonicalScalar::Int(-3),
        CanonicalScalar::Int(-1),
        CanonicalScalar::Int(0),
        CanonicalScalar::Int(4),
        CanonicalScalar::Null,
    ];
    let query = prepare("MATCH (n) WITH n.p AS p WHERE p+1>0 RETURN p");
    let result = run(&query, &values, wide(), &mut || Ok(())).unwrap();
    let mut kept = ints(&result.value, 0);
    kept.sort();
    assert_eq!(kept, vec![Some(0), Some(4)]);

    // A Boolean row alias is a predicate too: only true survives, and a
    // non-Boolean value is a typed execution error, never coerced.
    let alias = prepare("MATCH (n) WITH n.p AS p WHERE p RETURN p");
    let booleans = [
        CanonicalScalar::Bool(true),
        CanonicalScalar::Bool(false),
        CanonicalScalar::Null,
    ];
    let result = run(&alias, &booleans, wide(), &mut || Ok(())).unwrap();
    assert_eq!(result.value.len(), 1);
    assert_eq!(
        result.value[0].get(0).unwrap().as_scalar(),
        Some(&CanonicalScalar::Bool(true))
    );
    assert!(run(&alias, &[CanonicalScalar::Int(1)], wide(), &mut || Ok(())).is_err());
}

#[test]
fn discarded_names_sibling_aliases_and_unsupported_forms_refuse_before_catalog() {
    for text in [
        "MATCH (n) WITH n.p AS score RETURN n",
        "MATCH (n) WITH n.p AS score RETURN score.p",
        "MATCH (n) WITH n.p AS score WITH score AS first,first+1 AS second RETURN second",
        "MATCH (n) WITH n RETURN n+1 AS bad",
        "MATCH (n) WITH n.p AS p WITH p AS x,p AS x RETURN x",
        // Vertex-valued WITH imports now continue into MATCH; scalar imports
        // still cannot become vertices. multipart_reads tests the positive form.
        "MATCH (n) WITH n.p AS n MATCH (n)-[:R]->(m) RETURN m",
        "MATCH (n) WITH n WHERE n > n RETURN n",
        "MATCH (n) WITH n.p AS p RETURN missing LIMIT 0",
        "MATCH (n) WITH n.p AS p ORDER BY missing RETURN p",
        "MATCH (n) WITH n.p AS p ORDER BY p,p RETURN p",
        "MATCH (n) WITH COUNT(*) AS count RETURN count",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    let valid = format!("MATCH (n){} RETURN n", " WITH n".repeat(62));
    assert!(PreparedGraphSetText::prepare(&valid, symbols).is_ok());
    let invalid = format!("MATCH (n){} RETURN n", " WITH n".repeat(63));
    assert!(matches!(
        PreparedGraphSetText::prepare(&invalid, symbols)
            .unwrap_err()
            .kind,
        GraphSetTextErrorKind::SetBuild(_)
    ));
}

#[test]
fn parameter_contracts_and_original_utf8_offsets_span_all_stages() {
    let text = "\u{2003}MATCH (n) WITH n.p+$add AS score LIMIT $top WHERE score>$min RETURN score*$scale AS answer";
    let template = PreparedGraphSetText::prepare(text, symbols).unwrap();
    assert_eq!(template.parameter_schema().len(), 4);
    let args = GqlParameters::new()
        .with_int64("add", 1)
        .unwrap()
        .with_uint64("top", 5)
        .unwrap()
        .with_int64("min", 0)
        .unwrap();
    let missing = template.bind_parameters(&args).unwrap_err();
    assert_eq!(missing.offset, text.find("$scale").unwrap());
    let bound = template
        .bind_parameters(&args.clone().with_int64("scale", 2).unwrap())
        .unwrap();
    assert_ne!(
        bound.canonical_bytes(),
        template
            .bind_parameters(&args.with_int64("scale", 3).unwrap())
            .unwrap()
            .canonical_bytes()
    );
    let calls = Cell::new(0);
    let conflict = "MATCH (n) WITH n.p+$x AS score LIMIT $x RETURN score";
    assert!(
        PreparedGraphSetText::prepare(conflict, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(calls.get(), 0);
    for at in (0..=text.len()).filter(|at| text.is_char_boundary(*at)) {
        let _ = PreparedGraphSetText::prepare(&text[..at], symbols);
    }
    assert!(!format!("{template:?}").contains("score"));
}

#[test]
fn pipeline_leaves_compose_with_union_and_preserve_literal_token_boundaries() {
    let text = "(MATCH (n) WITH n.p AS p WHERE p>1 RETURN p LIMIT 1) UNION ALL (MATCH (n) WITH n.p AS p WHERE p<2 RETURN p) ORDER BY p DESC";
    let rows = run(
        &prepare(text),
        &[1, 2, 3].map(CanonicalScalar::Int),
        wide(),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(ints(&rows.value, 0), vec![Some(2), Some(1)]);
    assert_eq!(rows.rows.snapshot_records, 6);
    let literal = prepare("MATCH (n) WITH 'WITH $missing RETURN' AS text RETURN text");
    assert_eq!(
        run(
            &literal,
            &[CanonicalScalar::Null, CanonicalScalar::Null],
            wide(),
            &mut || Ok(())
        )
        .unwrap()
        .value
        .len(),
        2
    );
}

#[test]
fn full_pipeline_shares_exact_resource_limits_and_all_interruption_points() {
    let query = prepare("MATCH (n) WITH n.p+1 AS x WHERE x>1 WITH x*2 AS y RETURN y");
    let values = [1, 2, 3].map(CanonicalScalar::Int);
    let calls = Cell::new(0);
    let measured = run(&query, &values, wide(), &mut || {
        calls.set(calls.get() + 1);
        Ok(())
    })
    .unwrap();
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(
            &query,
            &values,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            &mut || Ok(())
        )
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut limit = caps;
        limit[dimension] -= 1;
        assert!(
            run(
                &query,
                &values,
                GqlQueryPolicy::new(limit[0], limit[1], limit[2], limit[3]),
                &mut || Ok(())
            )
            .is_err()
        );
    }
    for stop in 1..=calls.get() {
        let mut at = 0;
        assert!(
            matches!(run(&query, &values, wide(), &mut || { at += 1; if at==stop {Err(stop)} else {Ok(())} }),
            Err(GqlQueryError::Interrupted(value)) if value==stop)
        );
        assert_eq!(at, stop);
    }
}
