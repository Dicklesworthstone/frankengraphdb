//! Scoped text is checked against typed lowering and independent graph bags.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphMatchClause, GraphPatternBuilder, GraphValueRow,
    IntegerComparison, PatternBuildError, VertexPredicate,
};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphPatternTextErrorKind,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(3);
const P: PropertyKeyId = PropertyKeyId(4);
type Edge = (VId, RelationId, VId);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn query(text: &str) -> fgdb_gql::algebra::PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn ids(rows: &[GraphValueRow]) -> Vec<Vec<Option<VId>>> {
    rows.iter()
        .map(|row| {
            row.values()
                .iter()
                .map(|cell| {
                    if cell.is_null() {
                        None
                    } else {
                        Some(cell.as_vertex().expect("identity fixture"))
                    }
                })
                .collect()
        })
        .collect()
}
fn orient(edges: &[Edge], relation: RelationId, source: VId, direction: usize) -> Vec<VId> {
    let mut result = Vec::new();
    for &(left, actual, right) in edges {
        if actual != relation {
            continue;
        }
        if direction != 1 && left == source {
            result.push(right);
        }
        if direction != 0 && right == source && (direction == 1 || left != right) {
            result.push(left);
        }
    }
    result
}
fn path(left: &str, relation: &str, right: &str, direction: usize) -> String {
    match direction {
        0 => format!("({left})-[:{relation}]->({right})"),
        1 => format!("({left})<-[:{relation}]-({right})"),
        _ => format!("({left})-[:{relation}]-({right})"),
    }
}

#[test]
fn scoped_lowering_matches_typed_compilation_and_resolves_names_once() {
    let text = "MATCH (a:L) WHERE a.n >= $floor AND NOT EXISTS { MATCH (a)-[:S]->(hidden) WHERE hidden.n < $floor } \
        OPTIONAL MATCH (a)-[:R]->(b) WHERE b.n >= $floor \
        OPTIONAL MATCH (b)-[:S]->(c) WHERE c.n <= $ceiling \
        RETURN ALL a,b,c,c.n AS value SKIP $off LIMIT $take";
    let mut calls = BTreeMap::new();
    let prepared = PreparedGraphText::prepare(text, |kind, name| {
        *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls.len(), 4);
    assert!(calls.values().all(|count| *count == 1));
    assert_eq!(prepared.parameter_schema()[0].occurrences, 3);
    assert_eq!(
        prepared.parameter_schema()[0].parameter_type,
        GqlParameterType::Int64
    );
    let args = GqlParameters::new()
        .with_int64("floor", -1)
        .unwrap()
        .with_int64("ceiling", 9)
        .unwrap()
        .with_uint64("off", 1)
        .unwrap()
        .with_uint64("take", 4)
        .unwrap();
    let actual = prepared.bind_parameters(&args).unwrap();
    let mut root = GraphPatternBuilder::new();
    root.vertex("a").unwrap();
    root.filter("a", VertexPredicate::HasLabel(L)).unwrap();
    root.filter(
        "a",
        VertexPredicate::IntegerProperty {
            key: P,
            comparison: IntegerComparison::GreaterOrEqual,
            value: -1,
        },
    )
    .unwrap();
    let mut anti = GraphPatternBuilder::new();
    for name in ["a", "hidden"] {
        anti.vertex(name).unwrap();
    }
    anti.edge("a", S, GlaDirection::Forward, "hidden").unwrap();
    anti.filter(
        "hidden",
        VertexPredicate::IntegerProperty {
            key: P,
            comparison: IntegerComparison::Less,
            value: -1,
        },
    )
    .unwrap();
    let mut first = GraphPatternBuilder::new();
    for name in ["a", "b"] {
        first.vertex(name).unwrap();
    }
    first.edge("a", R, GlaDirection::Forward, "b").unwrap();
    first
        .filter(
            "b",
            VertexPredicate::IntegerProperty {
                key: P,
                comparison: IntegerComparison::GreaterOrEqual,
                value: -1,
            },
        )
        .unwrap();
    let mut second = GraphPatternBuilder::new();
    for name in ["b", "c"] {
        second.vertex(name).unwrap();
    }
    second.edge("b", S, GlaDirection::Forward, "c").unwrap();
    second
        .filter(
            "c",
            VertexPredicate::IntegerProperty {
                key: P,
                comparison: IntegerComparison::LessOrEqual,
                value: 9,
            },
        )
        .unwrap();
    let expected = root
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::not_exists(&anti),
                GraphMatchClause::optional(&first),
                GraphMatchClause::optional(&second),
            ],
            &[
                GraphColumn::vertex("a", "a"),
                GraphColumn::vertex("b", "b"),
                GraphColumn::vertex("c", "c"),
                GraphColumn::property("value", "c", P),
            ],
            1,
            Some(4),
        )
        .unwrap()
        .with_duplicates();
    assert_eq!(actual, expected);
    let frozen = actual.canonical_bytes();
    let changed = GqlParameters::new()
        .with_int64("floor", 0)
        .unwrap()
        .with_int64("ceiling", 8)
        .unwrap()
        .with_uint64("off", 0)
        .unwrap()
        .with_uint64("take", 2)
        .unwrap();
    assert_ne!(
        prepared
            .bind_parameters(&changed)
            .unwrap()
            .canonical_bytes(),
        frozen
    );
    assert_eq!(actual.canonical_bytes(), frozen);
    assert!(calls.values().all(|count| *count == 1));
    assert!(!format!("{prepared:?}").contains("hidden"));
}

#[test]
fn whole_and_chained_optional_paths_match_independent_oriented_bags() {
    let vertices = [VId(0), VId(1), VId(u128::MAX)];
    let universe = [
        (vertices[0], R, vertices[1]),
        (vertices[1], R, vertices[0]),
        (vertices[1], R, vertices[1]),
        (vertices[1], S, vertices[2]),
        (vertices[2], S, vertices[1]),
        (vertices[0], S, vertices[0]),
    ];
    for direction in 0..3 {
        let first = path("a", "R", "b", direction);
        let second = path("b", "S", "c", direction);
        for chained in [false, true] {
            let head = if chained {
                format!("MATCH (a) OPTIONAL MATCH {first} OPTIONAL MATCH {second}")
            } else {
                format!("MATCH (a) OPTIONAL MATCH {first},{second}")
            };
            let all = query(&format!("{head} RETURN ALL a,b,c"));
            let distinct = query(&format!("{head} RETURN DISTINCT a,b,c"));
            for mut encoded in 0..3_usize.pow(universe.len() as u32) {
                let mut edges = Vec::new();
                for edge in universe {
                    for _ in 0..encoded % 3 {
                        edges.push(edge);
                    }
                    encoded /= 3;
                }
                let mut expected = Vec::new();
                for a in vertices {
                    let mut complete = Vec::new();
                    for b in orient(&edges, R, a, direction) {
                        let targets = orient(&edges, S, b, direction);
                        if chained && targets.is_empty() {
                            complete.push(vec![Some(a), Some(b), None]);
                        }
                        for c in targets {
                            complete.push(vec![Some(a), Some(b), Some(c)]);
                        }
                    }
                    if complete.is_empty() {
                        complete.push(vec![Some(a), None, None]);
                    }
                    expected.extend(complete);
                }
                expected.sort();
                let run = |pattern: &fgdb_gql::algebra::PreparedGraphPattern<GraphValueRow>| {
                    pattern
                        .plan()
                        .execute_governed_with_properties(
                            (vertices.len() + edges.len()) as u64,
                            vertices,
                            edges.iter().copied(),
                            |_, _| Ok::<_, ()>(true),
                            |_, _| Ok(None),
                            wide(),
                            || Ok::<_, ()>(()),
                        )
                        .unwrap()
                };
                assert_eq!(ids(&run(&all).value), expected, "{head}");
                expected.dedup();
                assert_eq!(ids(&run(&distinct).value), expected, "{head}");
            }
        }
    }
}

#[test]
fn optional_predicates_are_inside_the_match_and_missing_bindings_never_read_vertex_zero() {
    let q =
        query("MATCH (a:L) OPTIONAL MATCH (a)-[:R]->(b) WHERE b.n >= 7 RETURN a,b,b.n AS value");
    let value = CanonicalScalar::Int(7);
    let edges = [(VId(0), R, VId(1)), (VId(0), R, VId(2))];
    let result = q
        .plan()
        .execute_governed_with_properties(
            4,
            [VId(0), VId(9)],
            edges,
            |vid, predicates| {
                Ok::<_, &str>(predicates.iter().all(|predicate| {
                    predicate.matches(
                        &[L],
                        &[(P, CanonicalScalar::Int(if vid == VId(2) { 7 } else { 1 }))],
                    )
                }))
            },
            |vid, _| {
                if vid == VId(2) {
                    Ok(Some(&value))
                } else {
                    Err("null binding called the property resolver")
                }
            },
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(result.value.len(), 2);
    assert_eq!(result.value[0].get(0).unwrap().as_vertex(), Some(VId(0)));
    assert_eq!(result.value[0].get(1).unwrap().as_vertex(), Some(VId(2)));
    assert_eq!(result.value[0].get(2).unwrap().as_scalar(), Some(&value));
    assert!(result.value[1].get(1).unwrap().is_null());
    assert!(result.value[1].get(2).unwrap().is_null());
}

#[test]
fn existential_scopes_are_local_and_do_not_multiply_outer_occurrences() {
    let text = "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(local) } \
        AND NOT EXISTS { MATCH (a)-[:S]->(local) } \
        OPTIONAL MATCH (a)-[:R]->(local) RETURN *";
    let q = query(text);
    assert_eq!(q.columns(), &["a", "local"]);
    let edges = [
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(1), S, VId(0)),
    ];
    let rows = q
        .plan()
        .execute_governed_with_properties(
            5,
            [VId(0), VId(1)],
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(ids(&rows.value), vec![vec![Some(VId(0)), Some(VId(1))]; 2]);
    // Contextual introducers do not reserve previously legal variable names.
    assert_eq!(
        query("MATCH (exists)-[:R]->(not) WHERE exists <> not RETURN exists,not").columns(),
        &["exists", "not"]
    );
}

#[test]
fn malformed_or_out_of_scope_definitions_refuse_before_catalog_access() {
    for text in [
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(local) } RETURN local",
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(local) } AND local.n > 0 RETURN a",
        "MATCH (a) OPTIONAL MATCH (b)-[:R]->(c) RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (b)-[:R]->(c) } RETURN a",
        "MATCH (a) WHERE NOT EXISTS { MATCH (a) RETURN a } RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (a) RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (a) WHERE EXISTS { MATCH (a) } } RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) } RETURN a",
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (b) } RETURN a",
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE b.n > 0 OR b.n < 0 RETURN a",
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) MATCH (b)-[:S]->(c) RETURN a",
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE missing.n > 0 RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) WHERE b.n > $x } RETURN a LIMIT $x",
    ] {
        let mut calls = 0;
        let result = PreparedGraphText::prepare(text, |kind, name| {
            calls += 1;
            symbols(kind, name)
        });
        assert!(result.is_err(), "{text}");
        assert_eq!(calls, 0, "invalid syntax/scope reached the catalog: {text}");
    }
    let text = "\u{2003}MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE b.n > $secret RETURN a";
    let prepared = PreparedGraphText::prepare(text, symbols).unwrap();
    let error = prepared.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(error.offset, text.find("$secret").unwrap());
    assert_eq!(error.kind, GraphPatternTextErrorKind::MissingParameter);
    assert!(!format!("{error:?} {error}").contains("secret"));
    for at in (0..=text.len()).filter(|at| text.is_char_boundary(*at)) {
        let _ = PreparedGraphText::prepare(&text[..at], symbols);
    }
}

#[test]
fn definition_limits_do_not_reset_at_optional_or_existential_boundaries() {
    let mut text = "MATCH (n0)".to_owned();
    for at in 0..64 {
        text.push_str(&format!(" OPTIONAL MATCH (n{at})-[:R]->(n{})", at + 1));
    }
    let max = query(&format!("{text} RETURN * LIMIT 0"));
    assert_eq!(max.columns().len(), 65);
    let error = PreparedGraphText::prepare(
        &format!("{text} OPTIONAL MATCH (n64)-[:R]->(overflow) RETURN *"),
        symbols,
    )
    .unwrap_err();
    assert!(matches!(
        error.kind,
        GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded { .. })
    ));
    let clauses = " AND EXISTS { MATCH (a) }".repeat(63);
    assert!(
        PreparedGraphText::prepare(
            &format!("MATCH (a) WHERE EXISTS {{ MATCH (a) }}{clauses} RETURN a"),
            symbols
        )
        .is_ok()
    );
    assert!(PreparedGraphText::prepare(&format!("MATCH (a) WHERE EXISTS {{ MATCH (a) }}{clauses} AND EXISTS {{ MATCH (a) }} RETURN a"), symbols).is_err());
    let predicates = " OPTIONAL MATCH (a:L:L:L:L)".repeat(64);
    assert!(
        PreparedGraphText::prepare(&format!("MATCH (a){predicates} RETURN a"), symbols).is_ok()
    );
    assert!(
        PreparedGraphText::prepare(&format!("MATCH (a:L){predicates} RETURN a"), symbols).is_err()
    );
}

#[test]
fn optional_text_and_aggregate_text_share_zero_preserving_semantics() {
    let text = "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) OPTIONAL MATCH (b)-[:S]->(c) \
        RETURN a,COUNT(*) AS rows,COUNT(c) AS present,COUNT(DISTINCT c) AS unique,SUM(c.n) AS total \
        GROUP BY a HAVING rows >= $minimum ORDER BY present ASC,a ASC LIMIT $take";
    let template = PreparedGraphAggregateText::prepare(text, symbols).unwrap();
    let args = GqlParameters::new()
        .with_int64("minimum", 1)
        .unwrap()
        .with_uint64("take", 10)
        .unwrap();
    let aggregate = template.bind_parameters(&args).unwrap();
    let scalar = CanonicalScalar::Int(7);
    let rows = aggregate
        .execute_governed(
            7,
            [VId(0), VId(1), VId(9)],
            [
                (VId(0), R, VId(2)),
                (VId(0), R, VId(2)),
                (VId(2), S, VId(3)),
                (VId(2), S, VId(3)),
            ],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&scalar)),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(rows.value.len(), 3);
    for row in &rows.value[..2] {
        assert_eq!(row.get(0).unwrap().as_count(), Some(1));
        assert_eq!(row.get(1).unwrap().as_count(), Some(0));
        assert_eq!(row.get(2).unwrap().as_count(), Some(0));
        assert!(row.get(3).unwrap().is_null());
    }
    let last = &rows.value[2];
    assert_eq!(last.keys()[0].as_vertex(), Some(VId(0)));
    assert_eq!(last.get(0).unwrap().as_count(), Some(4));
    assert_eq!(last.get(1).unwrap().as_count(), Some(4));
    assert_eq!(last.get(2).unwrap().as_count(), Some(1));
    assert_eq!(last.get(3).unwrap().as_integer(), Some(28));
}

#[test]
fn scoped_text_retains_all_limits_and_errors_never_become_null_extension() {
    let q = query("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE b.n >= 0 RETURN a,b,b.n AS value");
    let scalar = CanonicalScalar::Int(7);
    let vertices = [VId(0), VId(9)];
    let edges = [(VId(0), R, VId(1)), (VId(0), R, VId(2))];
    let run = |policy| {
        q.plan().execute_governed_with_properties(
            4,
            vertices,
            edges,
            |_, _| Ok::<_, &str>(true),
            |_, _| Ok(Some(&scalar)),
            policy,
            || Ok::<_, usize>(()),
        )
    };
    let full = run(wide()).unwrap();
    assert_eq!(full.value.len(), 3);
    let exact = GqlQueryPolicy::new(
        4,
        3,
        full.evaluator.work_units,
        full.evaluator.scratch_entries,
    );
    assert_eq!(run(exact).unwrap(), full);
    for policy in [
        GqlQueryPolicy::new(3, 3, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(4, 2, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(4, 3, full.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(4, 3, u64::MAX, full.evaluator.scratch_entries - 1),
    ] {
        assert!(run(policy).is_err());
    }
    let mut total = 0;
    q.plan()
        .execute_governed_with_properties(
            4,
            vertices,
            edges,
            |_, _| Ok::<_, &str>(true),
            |_, _| Ok(Some(&scalar)),
            wide(),
            || {
                total += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    for stop in 1..=total {
        let mut seen = 0;
        let result = q.plan().execute_governed_with_properties(
            4,
            vertices,
            edges,
            |_, _| Ok::<_, &str>(true),
            |_, _| Ok(Some(&scalar)),
            wide(),
            || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
    let result = q.plan().execute_governed_with_properties(
        4,
        vertices,
        edges,
        |vid, _| {
            if vid == VId(2) {
                Err("late optional predicate")
            } else {
                Ok(true)
            }
        },
        |_, _| Ok(Some(&scalar)),
        wide(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source("late optional predicate"))
    ));
}
