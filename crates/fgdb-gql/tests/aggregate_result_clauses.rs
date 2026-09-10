//! Text clauses bind to the same typed completed-group operators.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GraphAggregateColumn, GraphAggregateError, GraphAggregateFilter, GraphAggregateOrder,
    GraphAggregateTest, GraphNullPlacement, GraphPatternTextErrorKind, GraphSymbol,
    GraphSymbolKind, GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy,
    MAX_AGGREGATE_FILTERS, PreparedGraphAggregateText,
};
use fgdb_gql::algebra::IntegerComparison;
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn args(minimum: i64, offset: u64, count: u64) -> GqlParameters {
    GqlParameters::new().with_int64("minimum", minimum).unwrap()
        .with_uint64("off", offset).unwrap().with_uint64("take", count).unwrap()
}
const HEAD: &str = "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n, a AS owner, SUM(b.p) AS total GROUP BY a";

#[test]
fn text_clauses_equal_typed_definitions_and_rebinding_never_resolves_again() {
    let text = format!("{HEAD} HAVING COUNT(*) >= $minimum AND total IS NOT NULL \
        ORDER BY SUM_INT(b.p) DESC NULLS FIRST, owner ASC SKIP $off LIMIT $take");
    let mut calls = BTreeMap::new();
    let template = PreparedGraphAggregateText::prepare(&text, |kind, name| {
        *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
        symbols(kind, name)
    }).unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.values().all(|count| *count == 1));
    assert_eq!(template.parameter_schema().iter().map(|spec| (spec.name.as_str(), spec.parameter_type))
        .collect::<Vec<_>>(), vec![("minimum", GqlParameterType::Int64), ("off", GqlParameterType::UInt64), ("take", GqlParameterType::UInt64)]);
    let plain = PreparedGraphAggregateText::prepare(&format!("{HEAD} SKIP 1 LIMIT 2"), symbols)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let expected = plain.with_result_clauses(&[
        GraphAggregateFilter { column: GraphAggregateColumn::Aggregate(0), test: GraphAggregateTest::Integer {
            comparison: IntegerComparison::GreaterOrEqual, value: 3,
        } },
        GraphAggregateFilter { column: GraphAggregateColumn::Aggregate(1), test: GraphAggregateTest::IsNotNull },
    ], &[
        GraphAggregateOrder { column: GraphAggregateColumn::Aggregate(1), descending: true, nulls: GraphNullPlacement::First },
        GraphAggregateOrder::ascending(GraphAggregateColumn::GroupKey(0)),
    ]).unwrap();
    let first = template.bind_parameters(&args(3, 1, 2)).unwrap();
    assert_eq!(first, expected);
    let bytes = first.canonical_bytes();
    assert_ne!(template.bind_parameters(&args(2, 1, 2)).unwrap().canonical_bytes(), bytes);
    assert_eq!(template.bind_parameters(&args(3, 1, 2)).unwrap(), first);
    assert_eq!(template.columns(), &["n", "owner", "total"]);
    assert!(calls.values().all(|count| *count == 1));
    assert!(!format!("{template:?}").contains("minimum"));
    assert!(matches!(template.bind_parameters(&GqlParameters::new()).unwrap_err().kind,
        GraphPatternTextErrorKind::MissingParameter));
    assert!(matches!(template.bind_parameters(&args(1, 0, 2).with_int64("extra", 9).unwrap()).unwrap_err().kind,
        GraphPatternTextErrorKind::UnexpectedArguments));
}

#[test]
fn malformed_hidden_or_duplicate_clauses_never_call_the_catalog() {
    for tail in [
        "HAVING missing > 0", "HAVING MIN(b.p) > 0", "HAVING total > 1.5",
        "HAVING total IS TRUE", "HAVING total IS NOT", "HAVING total > 0 OR n > 0",
        "ORDER BY missing", "ORDER BY total ASC DESC", "ORDER BY total NULLS MIDDLE",
        "ORDER BY total, SUM(b.p)", "ORDER BY 1", "ORDER BY b.p",
        "ORDER BY total HAVING n > 0", "LIMIT 1 ORDER BY total", "HAVING COUNT(DISTINCT *) > 0",
        "HAVING n > $x LIMIT $x", "HAVING total >= $", "HAVING total > 0;",
    ] {
        let mut calls = 0;
        let text = format!("{HEAD} {tail}");
        assert!(PreparedGraphAggregateText::prepare(&text, |kind, name| {
            calls += 1; symbols(kind, name)
        }).is_err(), "{text}");
        assert_eq!(calls, 0, "{text}");
    }
    let text = format!("{HEAD} HAVING {}", vec!["n > 0"; MAX_AGGREGATE_FILTERS + 1].join(" AND "));
    let mut calls = 0;
    assert!(PreparedGraphAggregateText::prepare(&text, |_, _| { calls += 1; None }).is_err());
    assert_eq!(calls, 0);
}

#[test]
fn filtering_and_ranking_use_complete_aggregates_before_pagination() {
    let text = format!("{HEAD} HAVING n >= $minimum ORDER BY total DESC NULLS LAST SKIP $off LIMIT $take");
    let template = PreparedGraphAggregateText::prepare(&text, symbols).unwrap();
    let values = [CanonicalScalar::Int(10), CanonicalScalar::Int(9), CanonicalScalar::Int(i64::MAX)];
    let edges = [
        (VId(1), RelationId(1), VId(10)), (VId(1), RelationId(1), VId(11)),
        (VId(2), RelationId(1), VId(10)), (VId(2), RelationId(1), VId(10)),
        (VId(3), RelationId(1), VId(12)), (VId(3), RelationId(1), VId(12)),
        (VId(4), RelationId(1), VId(13)),
    ];
    let property = |vid: VId, _: PropertyKeyId| Ok::<_, ()>(values.get(vid.0 as usize - 10));
    let run = |minimum, offset, count| template.bind_parameters(&args(minimum, offset, count)).unwrap()
        .execute_governed(7, [], edges, |_, _| Ok::<_, ()>(true), property, wide(), || Ok::<_, ()>(())).unwrap();
    let all = run(0, 0, 10);
    assert_eq!(all.value.iter().map(|row| row.keys()[0].as_vertex().unwrap()).collect::<Vec<_>>(),
        vec![VId(3), VId(2), VId(1), VId(4)]);
    assert_eq!(all.value[0].get(1).unwrap().as_integer(), Some(2 * i128::from(i64::MAX)));
    let page = run(2, 1, 1);
    assert_eq!(page.value, all.value[1..2]);
    assert_eq!(page.value[0].get(0).unwrap().as_count(), Some(2));
    assert!(run(3, 0, 1).value.is_empty());
    let frozen = template.bind_parameters(&args(0, 0, 10)).unwrap().canonical_bytes();
    assert_eq!(template.bind_parameters(&args(0, 0, 10)).unwrap().canonical_bytes(), frozen);
}

#[test]
fn empty_global_null_tests_and_alias_precedence_are_explicit() {
    for (having, expected) in [
        ("n = 0", 1), ("n > 0", 0), ("total IS NULL", 1),
        ("total IS NOT NULL", 0), ("total <> 0", 0),
    ] {
        let text = format!("MATCH (x) RETURN COUNT(*) AS n, SUM(x.p) AS total HAVING {having} ORDER BY total DESC NULLS FIRST");
        let aggregate = PreparedGraphAggregateText::prepare(&text, symbols).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap();
        let result = aggregate.execute_governed(0, [], [], |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap();
        assert_eq!(result.value.len(), expected, "{text}");
    }
    // A bare alias may have the same spelling as an input variable. The
    // qualification/function syntax is never interpreted as an alias.
    let text = "MATCH (x) RETURN x AS owner, COUNT(*) AS x GROUP BY x HAVING x > 0 ORDER BY x DESC";
    let aggregate = PreparedGraphAggregateText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap();
    assert_eq!(aggregate.having()[0].column, GraphAggregateColumn::Aggregate(0));
    assert_eq!(aggregate.ordering()[0].column, GraphAggregateColumn::Aggregate(0));
}

#[test]
fn ranked_execution_shares_every_budget_and_interruption_checkpoint() {
    let text = format!("{HEAD} HAVING total IS NOT NULL ORDER BY total DESC LIMIT 2");
    let aggregate = PreparedGraphAggregateText::prepare(&text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap();
    let edges: Vec<_> = (1..=8).map(|id| (VId(id), RelationId(1), VId(id))).collect();
    let scalars: Vec<_> = (1..=8).map(CanonicalScalar::Int).collect();
    let run = |policy| aggregate.execute_governed(8, [], edges.iter().copied(), |_, _| Ok::<_, ()>(true),
        |vid, _| Ok(scalars.get(vid.0 as usize - 1)), policy, || Ok::<_, ()>(()));
    let complete = run(wide()).unwrap();
    assert_eq!(complete.value[0].keys()[0].as_vertex(), Some(VId(8)));
    let exact = GqlQueryPolicy::new(8, 2, complete.evaluator.work_units, complete.evaluator.scratch_entries);
    assert_eq!(run(exact).unwrap(), complete);
    for policy in [GqlQueryPolicy::new(7, 2, u64::MAX, u64::MAX), GqlQueryPolicy::new(8, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(8, 2, complete.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(8, 2, u64::MAX, complete.evaluator.scratch_entries - 1)] {
        assert!(run(policy).is_err());
    }
    let mut events = 0;
    aggregate.execute_governed(8, [], edges.iter().copied(), |_, _| Ok::<_, ()>(true),
        |vid, _| Ok(scalars.get(vid.0 as usize - 1)), wide(), || { events += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 1..=events {
        let mut at = 0;
        let result = aggregate.execute_governed(8, [], edges.iter().copied(), |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(scalars.get(vid.0 as usize - 1)), wide(), || {
                at += 1; if at == stop { Err(stop) } else { Ok(()) }
            });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
    let boolean = CanonicalScalar::Bool(true);
    let invalid = PreparedGraphAggregateText::prepare(
        "MATCH (x) RETURN MIN(x.p) AS least HAVING least > 0 ORDER BY least LIMIT 0", symbols)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    assert!(matches!(invalid.execute_governed(1, [VId(1)], [], |_, _| Ok::<_, ()>(true),
        |_, _| Ok(Some(&boolean)), wide(), || Ok::<_, ()>(())),
        Err(GqlQueryError::Source(GraphAggregateError::NonIntegerHaving { predicate: 0 }))));
}
