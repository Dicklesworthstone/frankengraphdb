//! Text ORDER BY uses the existing typed order and complete-row collector.
//! Independent expected rows never obtain their order from a compiled query.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaOperator, GraphColumn, GraphPatternBuilder, GraphValueOrder, GraphValueRow,
    PreparedGraphPattern,
};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphPatternTextErrorKind,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind, VId};
use std::cell::Cell;
use std::cmp::Ordering;

const SCORE: PropertyKeyId = PropertyKeyId(1);
const PAYLOAD: PropertyKeyId = PropertyKeyId(2);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
        (GraphSymbolKind::Property, "payload") => Some(GraphSymbol::Property(PAYLOAD)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}
fn text(statement: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(statement, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}
fn ordering(query: &PreparedGraphPattern<GraphValueRow>) -> &[GraphValueOrder] {
    query.plan().operators().iter().find_map(|op| match op {
        GlaOperator::OrderByValueColumns { columns } => Some(columns.as_ref()),
        _ => None,
    }).unwrap_or(&[])
}
fn typed(offset: u64, count: Option<u64>) -> PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("x").unwrap();
    builder.prepare_values(&[
        GraphColumn::vertex("owner", "x"),
        GraphColumn::property("rank", "x", SCORE),
    ], offset, count).unwrap().with_duplicates()
}
fn plain(rows: &[GraphValueRow]) -> Vec<(VId, Option<i64>)> {
    rows.iter().map(|row| {
        let value = match row.get(1).unwrap().as_scalar().unwrap() {
            CanonicalScalar::Int(value) => Some(*value),
            CanonicalScalar::Null => None,
            _ => panic!("fixture has only integer/null scores"),
        };
        (row.get(0).unwrap().as_vertex().unwrap(), value)
    }).collect()
}
fn expected(input: &[VId], values: &[Option<i64>], distinct: bool,
    descending: bool, first: bool, offset: usize, count: usize) -> Vec<(VId, Option<i64>)> {
    let mut result: Vec<_> = input.iter().map(|vid| (*vid, values[vid.0 as usize])).collect();
    result.sort_by(|a, b| {
        let score = match (a.1, b.1) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => if first { Ordering::Less } else { Ordering::Greater },
            (Some(_), None) => if first { Ordering::Greater } else { Ordering::Less },
            (Some(a), Some(b)) => if descending { b.cmp(&a) } else { a.cmp(&b) },
        };
        score.then(a.0.cmp(&b.0))
    });
    if distinct { result.dedup(); }
    result.into_iter().skip(offset).take(count).collect()
}

#[test]
fn text_pages_match_independent_sort_across_nulls_directions_and_multiplicity() {
    let input = [VId(3), VId(0), VId(1), VId(0), VId(2), VId(3)];
    for distinct in [false, true] {
        for descending in [false, true] {
            for first in [false, true] {
                let template = PreparedGraphText::prepare(&format!(
                    "MATCH (x) RETURN {} x AS owner,x.score AS rank ORDER BY rank {} NULLS {} SKIP $off LIMIT $take",
                    if distinct { "DISTINCT" } else { "ALL" },
                    if descending { "DESC" } else { "ASC" },
                    if first { "FIRST" } else { "LAST" },
                ), symbols).unwrap();
                for offset in 0..=3 {
                    for count in 0..=3 {
                        let args = GqlParameters::new().with_uint64("off", offset as u64).unwrap()
                            .with_uint64("take", count as u64).unwrap();
                        let query = template.bind_parameters(&args).unwrap();
                        for mut code in 0..81 {
                            let values: [Option<i64>; 4] = std::array::from_fn(|_| {
                                let value = [None, Some(-1), Some(2)][code % 3];
                                code /= 3;
                                value
                            });
                            let scalars = values.map(|value| value.map_or(CanonicalScalar::Null, CanonicalScalar::Int));
                            let actual = query.plan().execute_with_properties_control(input, [],
                                |_, _| Ok::<_, ()>(true),
                                |vid, _| Ok(Some(&scalars[vid.0 as usize])), |_| Ok(())).unwrap();
                            assert_eq!(plain(&actual), expected(&input, &values, distinct,
                                descending, first, offset, count));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn text_and_typed_ordering_share_transcripts_and_immutable_rebinding() {
    let calls = Cell::new(0);
    let template = PreparedGraphText::prepare(
        "MATCH (x) RETURN ALL x AS owner,x.score AS rank ORDER BY rank DESC NULLS FIRST,owner ASC SKIP $off LIMIT $take",
        |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) },
    ).unwrap();
    let args = GqlParameters::new().with_uint64("off", 1).unwrap().with_uint64("take", 2).unwrap();
    let query = template.bind_parameters(&args).unwrap();
    let wanted = typed(1, Some(2)).with_order_by(&[
        GraphValueOrder::descending(1).with_nulls_first(true), GraphValueOrder::ascending(0),
    ]).unwrap();
    assert_eq!(query, wanted);
    assert_eq!(calls.get(), 1);
    let frozen = query.canonical_bytes();
    let changed = template.bind_parameters(&GqlParameters::new()
        .with_uint64("off", 0).unwrap().with_uint64("take", 1).unwrap()).unwrap();
    assert_ne!(changed.canonical_bytes(), frozen);
    assert_eq!(query.canonical_bytes(), frozen);
    assert_eq!(calls.get(), 1, "ORDER BY never re-resolves the catalog during rebinding");
    assert!(template.bind_parameters(&GqlParameters::new()
        .with_int64("off", 0).unwrap().with_uint64("take", 1).unwrap()).is_err());
    let renamed = text("MATCH (x) RETURN ALL x AS a,x.score AS b ORDER BY x.score DESC NULLS FIRST,x ASC SKIP 1 LIMIT 2");
    assert_eq!(renamed.canonical_bytes(), frozen);
    assert_eq!(text("MATCH (x) RETURN ALL x AS owner,x.score AS rank SKIP 1 LIMIT 2"), typed(1, Some(2)));
}

#[test]
fn aliases_star_and_bad_order_references_are_resolved_before_catalog_access() {
    assert!(PreparedGraphText::prepare("MATCH (a) RETURN a ORDER BY a", symbols).is_ok());
    for tail in ["ORDER BY missing", "ORDER BY x.payload", "ORDER BY rank,rank DESC",
        "ORDER rank", "ORDER BY", "ORDER BY rank NULLS MIDDLE", "ORDER BY rank ASC DESC",
        "ORDER BY $column", "ORDER BY rank LIMIT 1 SKIP 0", "ORDER BY rank, x.score"] {
        let calls = Cell::new(0);
        let result = PreparedGraphText::prepare(&format!("MATCH (x) RETURN x.score AS rank {tail}"),
            |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) });
        assert!(result.is_err(), "{tail}");
        assert_eq!(calls.get(), 0, "{tail}");
    }
    let alias = text("MATCH (a)-[:R]->(b) RETURN a AS b,b AS other ORDER BY b DESC");
    assert_eq!(ordering(&alias), &[GraphValueOrder::descending(0)]);
    let star = text("MATCH (a)-[:R]->(b) RETURN * ORDER BY b DESC");
    assert_eq!(ordering(&star), &[GraphValueOrder::descending(1)]);
    let statement = "MATCH (x) RETURN x.score AS rank ORDER BY rank, x.score";
    let failed = PreparedGraphText::prepare(statement, symbols).unwrap_err();
    assert_eq!(failed.offset, statement.rfind("x.score").unwrap());
    assert!(matches!(failed.kind, GraphPatternTextErrorKind::OrderBuild(
        fgdb_gql::algebra::GraphOrderError::DuplicateColumn { column: 0 })));
}

#[test]
fn optional_nulls_and_parallel_rows_are_ranked_after_scope_completion() {
    let query = text("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN a,b.score AS rank ORDER BY rank DESC NULLS FIRST");
    let score = CanonicalScalar::Int(5);
    let edges = [(VId(0), RelationId(1), VId(10)), (VId(0), RelationId(1), VId(10)),
        (VId(1), RelationId(1), VId(11))];
    let mut reads = 0;
    let rows = query.plan().execute_with_properties_control([VId(0), VId(1), VId(2)], edges,
        |_, _| Ok::<_, ()>(true), |vid, _| {
            reads += 1;
            assert!(vid == VId(10) || vid == VId(11));
            Ok((vid == VId(10)).then_some(&score))
        }, |_| Ok(())).unwrap();
    assert_eq!(plain(&rows), vec![(VId(1), None), (VId(2), None), (VId(0), Some(5)), (VId(0), Some(5))]);
    assert_eq!(reads, 3, "null-extended bindings never reach the property source");
}

#[test]
fn ranked_pages_do_not_hide_late_source_errors_or_copy_rejected_payloads() {
    let scores: Vec<_> = (0..32).map(|i| CanonicalScalar::Int(100 - i)).collect();
    let payload = CanonicalScalar::bytes(vec![7; 4096]).unwrap();
    let query = text("MATCH (x) RETURN x.score AS rank,x.payload AS data ORDER BY rank DESC LIMIT 1");
    let reads = Cell::new(0);
    let result = query.plan().execute_governed_with_properties(32, (0..32).map(VId), [],
        |_, _| Ok::<_, &str>(true), |vid, key| {
            reads.set(reads.get() + 1);
            Ok(Some(if key == SCORE { &scores[vid.0 as usize] } else { &payload }))
        }, wide(), || Ok::<_, usize>(())).unwrap();
    assert_eq!(reads.get(), 64);
    assert_eq!(result.value.len(), 1);
    assert!(result.evaluator.scratch_entries < 100, "rejected 4KiB payloads must not be cloned");
    for count in [0, 1] {
        let query = text(&format!("MATCH (x) RETURN x.score AS rank ORDER BY rank DESC LIMIT {count}"));
        let mut reads = 0;
        let result = query.plan().execute_with_properties_control((0..3).map(VId), [],
            |_, _| Ok(true), |vid, _| {
                reads += 1;
                if vid == VId(2) { Err("late property failure") }
                else { Ok(Some(&scores[vid.0 as usize])) }
            }, |_| Ok(()));
        assert_eq!(result, Err("late property failure"));
        assert_eq!(reads, 3);
    }
}

#[test]
fn ordered_text_has_exact_limits_and_propagates_each_interruption() {
    let scalars = [CanonicalScalar::Int(2), CanonicalScalar::Null, CanonicalScalar::Int(3)];
    let query = text("MATCH (x) RETURN x AS owner,x.score AS rank ORDER BY rank DESC SKIP 1 LIMIT 2");
    let run = |policy, stop: usize| {
        let mut calls = 0;
        let result = query.plan().execute_governed_with_properties(3, (0..3).map(VId), [],
            |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&scalars[vid.0 as usize])), policy,
            || { calls += 1; if calls == stop { Err(stop) } else { Ok(()) } });
        (calls, result)
    };
    let (calls, result) = run(wide(), usize::MAX);
    let measured = result.unwrap();
    assert_eq!(plain(&measured.value), vec![(VId(0), Some(2)), (VId(1), None)]);
    let exact = GqlQueryPolicy::new(3, 2, measured.evaluator.work_units, measured.evaluator.scratch_entries);
    assert_eq!(run(exact, usize::MAX).1.unwrap(), measured);
    for policy in [GqlQueryPolicy::new(3, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(3, 2, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(3, 2, u64::MAX, measured.evaluator.scratch_entries - 1)] {
        assert!(run(policy, usize::MAX).1.is_err());
    }
    for stop in 1..=calls {
        let (actual, result) = run(wide(), stop);
        assert_eq!(actual, stop);
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
    }
}

#[test]
fn scalar_parameters_and_aggregate_ordering_keep_their_existing_binding_contracts() {
    let calls = Cell::new(0);
    let template = PreparedGraphText::prepare_with_parameter_types(
        "MATCH (x) WHERE x.payload = $wanted RETURN x,x.score AS rank ORDER BY rank DESC LIMIT $take",
        &[("wanted", GqlParameterType::Scalar(CanonicalScalarKind::Text))],
        |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) },
    ).unwrap();
    let before = calls.get();
    let arguments = GqlParameters::new().with_text("wanted", "ORDER BY missing; 'literal'").unwrap()
        .with_uint64("take", 3).unwrap();
    let query = template.bind_parameters(&arguments).unwrap();
    assert_eq!(ordering(&query), &[GraphValueOrder::descending(1)]);
    assert_eq!(calls.get(), before);
    let aggregate = PreparedGraphAggregateText::prepare(
        "MATCH (x) RETURN x,COUNT(*) AS n GROUP BY x ORDER BY n DESC,x ASC LIMIT 1", symbols,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    assert!(ordering(aggregate.input_pattern()).is_empty());
    let rows = aggregate.execute_governed(3, [VId(2), VId(1), VId(2)], [],
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].keys()[0].as_vertex(), Some(VId(2)));
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(2));
}

#[test]
fn all_65_columns_and_utf8_prefixes_preserve_bounded_preparation() {
    let returned = (0..65).map(|at| format!("x AS c{at}")).collect::<Vec<_>>().join(",");
    let order = (0..65).rev().map(|at| format!("c{at} DESC")).collect::<Vec<_>>().join(",");
    let query = text(&format!("MATCH (x) RETURN DISTINCT {returned} ORDER BY {order}"));
    assert_eq!(ordering(&query).len(), 65);
    let rows = query.plan().execute_with_properties_control([VId(0), VId(u128::MAX)], [],
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), |_| Ok(())).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].len(), 65);
    assert!(rows[0].values().iter().all(|value| value.as_vertex() == Some(VId(u128::MAX))));
    let source = "\u{2003}MATCH (x) RETURN x ORDER BY x DESC NULLS FIRST LIMIT 1";
    for at in (0..=source.len()).filter(|at| source.is_char_boundary(*at)) {
        let _ = PreparedGraphText::prepare(&source[..at], symbols);
    }
}
