//! Conditional expressions must use the shared typed VM on every public path.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphIntegerErrorKind,
    GraphMutationError, GraphMutationIntent, GraphMutationPolicy, GraphSetExecutionError,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText, PreparedGraphMutationText,
    PreparedGraphSet, PreparedGraphSetText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
type Props = BTreeMap<(VId, PropertyKeyId), CanonicalScalar>;
type Triple = (VId, RelationId, VId);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000)
}
fn prepare(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn run<C>(
    query: &PreparedGraphSet,
    vertices: &[VId],
    edges: &[Triple],
    props: &Props,
    policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<()>, C>> {
    query.execute_governed(
        policy,
        |pattern, allowance| {
            pattern.plan().execute_governed_with_properties(
                (vertices.len() + edges.len()) as u64,
                vertices.iter().copied(),
                edges.iter().copied(),
                |vid, predicates| {
                    Ok::<_, ()>(predicates.iter().all(|predicate| {
                        predicate.matches_borrowed(
                            [],
                            props.iter().filter_map(|(&(owner, key), value)| {
                                (owner == vid).then_some((key, value))
                            }),
                        )
                    }))
                },
                |vid, key| Ok(props.get(&(vid, key))),
                allowance,
                || Ok::<_, C>(()),
            )
        },
        checkpoint,
    )
}
fn integer(value: &GraphValue) -> Option<i64> {
    match value.as_scalar().unwrap() {
        CanonicalScalar::Int(value) => Some(*value),
        CanonicalScalar::Null => None,
        _ => panic!("unexpected fixture scalar"),
    }
}
fn scalar(expression: &str) -> Option<i64> {
    let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
    integer(
        &run(&query, &[VId(1)], &[], &Props::new(), policy(), || {
            Ok::<_, ()>(())
        })
        .unwrap()
        .value[0]
            .values()[0],
    )
}

#[test]
fn first_true_branch_and_first_nonnull_equality_have_lazy_defaults() {
    for (expression, expected) in [
        (
            "CASE WHEN TRUE THEN 7 WHEN 1/0=0 THEN 8 ELSE 9 END",
            Some(7),
        ),
        (
            "CASE WHEN NULL THEN 7 WHEN FALSE THEN 8 ELSE 9 END",
            Some(9),
        ),
        ("CASE WHEN FALSE THEN 1/0 END", None),
        (
            "CASE 2 WHEN 1 THEN 1/0 WHEN 2 THEN 8 WHEN 2 THEN 9 ELSE 1/0 END",
            Some(8),
        ),
        ("CASE NULL WHEN NULL THEN 1 ELSE 2 END", Some(2)),
        ("CASE 3 WHEN 1 THEN 4 END", None),
        ("COALESCE(CASE WHEN NULL THEN 1 END, 5)+2", Some(7)),
        (
            "CASE WHEN (2+3)*4=20 AND NOT (NULL IS NOT NULL OR FALSE) THEN -7 ELSE 1/0 END",
            Some(-7),
        ),
        (
            "CASE WHEN -9223372036854775808 < 9223372036854775807 THEN 1 ELSE 0 END",
            Some(1),
        ),
        (
            "CASE CASE WHEN TRUE THEN 2 ELSE 1/0 END WHEN 2 THEN CASE WHEN TRUE THEN 8 END ELSE 9 END",
            Some(8),
        ),
    ] {
        assert_eq!(scalar(expression), expected, "{expression}");
    }
    let eager = prepare("MATCH (n) RETURN CASE WHEN TRUE OR 1/0=0 THEN 1 ELSE 0 END AS value");
    assert!(
        matches!(run(&eager, &[VId(1)], &[], &Props::new(), policy(), || Ok::<_, ()>(())),
        Err(GqlQueryError::Source(GraphSetExecutionError::Projection { error, .. }))
            if error.kind == GraphIntegerErrorKind::DivisionByZero)
    );
}

#[test]
fn nullable_numeric_conditions_and_parentheses_match_an_independent_oracle() {
    let query = prepare(
        "MATCH (n) RETURN CASE WHEN (n.p+1)>n.q OR n.p IS NULL AND NOT (n.q IS NULL) \
        THEN n.p*n.q ELSE 17 END AS result",
    );
    for p in [None, Some(-2), Some(0), Some(2)] {
        for q in [None, Some(-2), Some(0), Some(2)] {
            let mut props = Props::new();
            if let Some(p) = p {
                props.insert((VId(1), P), CanonicalScalar::Int(p));
            }
            props.insert(
                (VId(1), Q),
                q.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
            );
            let selected = p.zip(q).is_some_and(|(p, q)| p + 1 > q) || (p.is_none() && q.is_some());
            let expected = if selected {
                p.zip(q).map(|(p, q)| p * q)
            } else {
                Some(17)
            };
            let result = run(&query, &[VId(1)], &[], &props, policy(), || Ok::<_, ()>(())).unwrap();
            assert_eq!(
                integer(&result.value[0].values()[0]),
                expected,
                "{p:?}, {q:?}"
            );
        }
    }
}

#[test]
fn conditional_aggregation_groups_computed_values_and_retains_exact_averages() {
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(-2)),
        ((VId(2), P), CanonicalScalar::Int(0)),
        ((VId(3), P), CanonicalScalar::Int(3)),
    ]);
    let text = "MATCH (n) RETURN COUNT(*) AS rows, SUM(CASE WHEN n.p>=0 THEN n.p ELSE 0 END) AS total, \
        COUNT(CASE WHEN n.p>=0 THEN 1 END) AS selected, \
        AVG(CASE WHEN n.p>=0 THEN n.p ELSE 0 END) AS mean";
    let query = PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let result = query
        .execute_governed(
            4,
            (1..=4).map(VId),
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, key| Ok(props.get(&(vid, key))),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(result.value.len(), 1);
    let values = result.value[0].values();
    assert_eq!(values[0].as_count(), Some(4));
    assert_eq!(values[1].as_integer(), Some(3));
    assert_eq!(values[2].as_count(), Some(2));
    let average = values[3].as_average().unwrap();
    assert_eq!((average.numerator(), average.denominator()), (3, 4));
    let text = "MATCH (n) RETURN CASE WHEN n.p>=0 THEN 1 ELSE 0 END AS bucket, \
        SUM(COALESCE(n.p,0)) AS total GROUP BY CASE WHEN n.p>=0 THEN 1 ELSE 0 END ORDER BY bucket";
    let query = PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let result = query
        .execute_governed(
            4,
            (1..=4).map(VId),
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, key| Ok(props.get(&(vid, key))),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        result
            .value
            .iter()
            .map(|row| (integer(&row.keys()[0]), row.values()[0].as_integer()))
            .collect::<Vec<_>>(),
        vec![(Some(0), Some(-2)), (Some(1), Some(3))]
    );
}

#[test]
fn walk_occurrences_and_optional_nulls_survive_conditional_projection() {
    let edges = [
        (VId(1), R, VId(1)),
        (VId(1), R, VId(1)),
        (VId(1), R, VId(2)),
    ];
    for (quantifier, count) in [("", 11), ("DISTINCT", 1)] {
        let query = prepare(&format!(
            "MATCH WALK (a)-[:R*0..2]->(b) RETURN {quantifier} CASE WHEN TRUE THEN 7 ELSE 1/0 END AS value"
        ));
        let result = run(
            &query,
            &[VId(1), VId(2)],
            &edges,
            &Props::new(),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
        assert_eq!(result.value.len(), count);
    }
    let query = prepare(
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) \
        RETURN CASE WHEN b.p IS NULL THEN 9 ELSE b.p END AS value",
    );
    let result = run(
        &query,
        &[VId(1), VId(2)],
        &edges,
        &Props::new(),
        policy(),
        || Ok::<_, ()>(()),
    )
    .unwrap();
    assert_eq!(result.value.len(), 4);
    assert!(
        result
            .value
            .iter()
            .all(|row| integer(&row.values()[0]) == Some(9))
    );
}

#[test]
fn conditional_mutations_reuse_frozen_assignments_and_conflict_reduction() {
    let props = Props::from([((VId(1), P), CanonicalScalar::Int(2))]);
    let edges = [
        (VId(1), R, VId(1)),
        (VId(1), R, VId(1)),
        (VId(1), R, VId(2)),
    ];
    let query = PreparedGraphMutationText::prepare(
        "MATCH WALK (a)-[:R*0..2]->(b) \
        SET a.p=CASE WHEN a.p IS NULL THEN 1 ELSE a.p+1 END",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let source = |pattern: &fgdb_gql::algebra::PreparedGraphPattern<GraphValueRow>, allowance| {
        pattern.plan().execute_governed_with_properties(
            5,
            [VId(1), VId(2)],
            edges,
            |_, _| Ok::<_, ()>(true),
            |vid, key| Ok(props.get(&(vid, key))),
            allowance,
            || Ok::<_, ()>(()),
        )
    };
    let result = query
        .execute_governed(GraphMutationPolicy::new(policy(), 100), source, || {
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(result.stats().selection.result_rows, 11);
    assert_eq!(
        result.intents(),
        &[
            GraphMutationIntent::Property {
                vertex: VId(1),
                key: P,
                value: Some(CanonicalScalar::Int(3))
            },
            GraphMutationIntent::Property {
                vertex: VId(2),
                key: P,
                value: Some(CanonicalScalar::Int(1))
            },
        ]
    );
    let query = PreparedGraphMutationText::prepare(
        "MATCH (a)-[:R]->(b) \
        SET a.p=CASE WHEN b.p=2 THEN 0 ELSE 1 END",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    assert!(matches!(
        query.execute_governed(GraphMutationPolicy::new(policy(), 100), source, || Ok::<
            _,
            (),
        >(
            ()
        )),
        Err(GqlQueryError::Source(
            GraphMutationError::ConflictingAssignment { .. }
        ))
    ));
}

#[test]
fn parameters_are_admitted_once_and_all_branches_bind_before_execution() {
    let text = "\u{2003}MATCH (n) RETURN CASE $selector WHEN n.p THEN $value WHEN $other THEN $value ELSE $default END AS result LIMIT $limit";
    let calls = Cell::new(0);
    let template = PreparedGraphSetText::prepare(text, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(
        template
            .parameter_schema()
            .iter()
            .map(|spec| (spec.name.as_str(), spec.occurrences))
            .collect::<Vec<_>>(),
        vec![
            ("selector", 1),
            ("value", 2),
            ("other", 1),
            ("default", 1),
            ("limit", 1)
        ]
    );
    assert_eq!(
        template
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .offset,
        text.find("$selector").unwrap()
    );
    let args = GqlParameters::new()
        .with_int64("selector", 2)
        .unwrap()
        .with_int64("value", 5)
        .unwrap()
        .with_int64("other", 3)
        .unwrap()
        .with_int64("default", 7)
        .unwrap()
        .with_uint64("limit", 10)
        .unwrap();
    let query = template.bind_parameters(&args).unwrap();
    assert_eq!(
        query.canonical_bytes(),
        template.bind_parameters(&args).unwrap().canonical_bytes()
    );
    assert_eq!(calls.get(), 1);
    assert!(!format!("{template:?} {query:?}").contains("selector"));
}

#[test]
fn malformed_branches_and_excessive_definitions_refuse_before_catalog_callbacks() {
    for expression in [
        "CASE END",
        "CASE WHEN TRUE THEN END",
        "CASE WHEN 1 THEN 2 END",
        "CASE WHEN TRUE THEN 2 ELSE 'text' END",
        "CASE 1 WHEN TRUE THEN 2 END",
        "CASE WHEN n.p=1 THEN 2",
        "CASE WHEN TRUE THEN 2 ELSE 3 WHEN FALSE THEN 4 END",
        "CASE WHEN TRUE THEN 1 ELSE missing.p END",
        "CASE WHEN (n.p>0 THEN 1 END",
    ] {
        let calls = Cell::new(0);
        let text = format!("MATCH (n) RETURN {expression} AS value");
        assert!(
            PreparedGraphSetText::prepare(&text, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{expression}"
        );
        assert_eq!(calls.get(), 0);
    }
    let too_many = format!("CASE {} ELSE 0 END", "WHEN TRUE THEN 1 ".repeat(400));
    assert!(
        PreparedGraphSetText::prepare(&format!("MATCH (n) RETURN {too_many} AS value"), symbols)
            .is_err()
    );
    let too_deep = format!(
        "{}1{}",
        "CASE WHEN TRUE THEN ".repeat(65),
        " END".repeat(65)
    );
    assert!(
        PreparedGraphSetText::prepare(&format!("MATCH (n) RETURN {too_deep} AS value"), symbols)
            .is_err()
    );
    for text in [
        "MATCH (case) RETURN case",
        "MATCH (case) RETURN case.p AS p",
    ] {
        let old: PreparedGraphSet = PreparedGraphText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .into();
        assert_eq!(prepare(text).canonical_bytes(), old.canonical_bytes());
    }
    prepare("MATCH (case) RETURN CASE WHEN case.p IS NULL THEN 1 ELSE case.p END AS value");
    prepare("MATCH (not) RETURN CASE WHEN not.p>0 THEN 1 ELSE 0 END AS value");
}

#[test]
fn lazy_values_never_suppress_source_failures_or_selected_errors_under_limit_zero() {
    let query = prepare("MATCH (n) RETURN CASE WHEN TRUE THEN 7 ELSE n.q END AS value LIMIT 0");
    let result = query.execute_governed(
        policy(),
        |pattern, allowance| {
            pattern.plan().execute_governed_with_properties(
                1,
                [VId(1)],
                [],
                |_, _| Ok::<_, &str>(true),
                |_, _| Err::<Option<&CanonicalScalar>, _>("unreadable input"),
                allowance,
                || Ok::<_, ()>(()),
            )
        },
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphSetExecutionError::Source(
            "unreadable input"
        )))
    ));
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(0)),
        ((VId(2), P), CanonicalScalar::Int(2)),
    ]);
    let query =
        prepare("MATCH (n) RETURN CASE WHEN n.p=0 THEN 7 ELSE 1/(n.p-2) END AS value LIMIT 0");
    assert!(
        matches!(run(&query, &[VId(1), VId(2)], &[], &props, policy(), || Ok::<_, ()>(())),
        Err(GqlQueryError::Source(GraphSetExecutionError::Projection { error, .. }))
            if error.kind == GraphIntegerErrorKind::DivisionByZero)
    );
}

#[test]
fn exact_query_limits_and_each_conditional_checkpoint_refuse_without_output() {
    let query = prepare(
        "MATCH (n) RETURN CASE n.p WHEN 1 THEN 7 WHEN 2 THEN 8 ELSE 9 END AS value ORDER BY value LIMIT 2",
    );
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(1)),
        ((VId(2), P), CanonicalScalar::Int(2)),
    ]);
    let vertices = [VId(1), VId(2), VId(3)];
    let mut events = 0;
    let result = run(&query, &vertices, &[], &props, policy(), || {
        events += 1;
        Ok::<_, usize>(())
    })
    .unwrap();
    let exact = GqlQueryPolicy::new(
        result.rows.snapshot_records,
        result.rows.result_rows,
        result.evaluator.work_units,
        result.evaluator.scratch_entries,
    );
    assert_eq!(
        run(&query, &vertices, &[], &props, exact, || Ok::<_, usize>(())).unwrap(),
        result
    );
    for stop in 1..=events {
        let mut seen = 0;
        assert!(matches!(run(&query, &vertices, &[], &props, exact, || {
            seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(GqlQueryError::Interrupted(actual)) if actual == stop));
        assert_eq!(seen, stop);
    }
    for refusal in [
        GqlQueryPolicy::new(
            result.rows.snapshot_records - 1,
            u64::MAX,
            u64::MAX,
            u64::MAX,
        ),
        GqlQueryPolicy::new(u64::MAX, result.rows.result_rows - 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(
            u64::MAX,
            u64::MAX,
            result.evaluator.work_units - 1,
            u64::MAX,
        ),
        GqlQueryPolicy::new(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            result.evaluator.scratch_entries - 1,
        ),
    ] {
        assert!(run(&query, &vertices, &[], &props, refusal, || Ok::<_, ()>(())).is_err());
    }
}
