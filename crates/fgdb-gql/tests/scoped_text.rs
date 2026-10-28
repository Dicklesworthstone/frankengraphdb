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
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
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
    let prepared = PreparedGraphText::prepare(text, |kind: GraphSymbolKind, name: &str| {
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
    let stored = CanonicalPropertyValue::from(value.clone());
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
                        &[(
                            P,
                            CanonicalPropertyValue::Scalar(CanonicalScalar::Int(
                                if vid == VId(2) { 7 } else { 1 },
                            )),
                        )],
                    )
                }))
            },
            |vid, _| {
                if vid == VId(2) {
                    Ok(Some(&stored))
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

/// fgdb-44d8n: an openCypher pattern predicate IS its EXISTS spelling, with
/// identical plan bytes and so identical rows. This holds bare and under NOT,
/// in every direction, with labels and property maps, correlated to two outer
/// variables, and as an AND conjunct beside an ordinary predicate. Mixed
/// under OR it is refused exactly as EXISTS is. A parenthesised expression
/// followed by arithmetic or a comparison stays an expression.
#[test]
fn pattern_predicates_are_their_exists_spelling() {
    for (pattern, exists) in [
        (
            "MATCH (a) WHERE (a)-[:R]->() RETURN a",
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->() } RETURN a",
        ),
        (
            "MATCH (a) WHERE NOT (a)-[:R]->() RETURN a",
            "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->() } RETURN a",
        ),
        (
            "MATCH (a) WHERE (a)<-[:S]-(:L) RETURN a",
            "MATCH (a) WHERE EXISTS { MATCH (a)<-[:S]-(:L) } RETURN a",
        ),
        (
            "MATCH (a) WHERE (a:L)-[:R]-(:L {n: 2}) RETURN a",
            "MATCH (a) WHERE EXISTS { MATCH (a:L)-[:R]-(:L {n: 2}) } RETURN a",
        ),
        (
            "MATCH (a) WHERE a.n > 1 AND (a)-[:R]->() AND NOT (a)-[:S]->() RETURN a",
            "MATCH (a) WHERE a.n > 1 AND EXISTS { MATCH (a)-[:R]->() } \
             AND NOT EXISTS { MATCH (a)-[:S]->() } RETURN a",
        ),
        (
            "MATCH (a)-[:R]->(b) WHERE NOT (b)-[:S]->(a) RETURN a, b",
            "MATCH (a)-[:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:S]->(a) } RETURN a, b",
        ),
        // A subquery RETURN of literals or `*` keeps every row, so it is the
        // same existence test (TCK ExistentialSubquery2 [1]).
        (
            "MATCH (a) WHERE (a)-[:R]->() RETURN a",
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->() RETURN true } RETURN a",
        ),
        (
            "MATCH (a) WHERE NOT (a)-[:R]->() RETURN a",
            "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->() RETURN *, 1 AS one } RETURN a",
        ),
    ] {
        assert_eq!(
            query(pattern).canonical_bytes(),
            query(exists).canonical_bytes(),
            "{pattern}"
        );
    }
    // An aggregate or a computed RETURN item could change the subquery's rows.
    for text in [
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->() RETURN count(*) } RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) RETURN b } RETURN a",
    ] {
        assert!(PreparedGraphText::prepare(text, symbols).is_err(), "{text}");
    }
    // Rows on a fixture: 0 -R-> 1, 2 -S-> 0.
    let rows = |text: &str| {
        let result = query(text)
            .plan()
            .execute_governed_with_properties(
                5,
                [VId(0), VId(1), VId(2)],
                [(VId(0), R, VId(1)), (VId(2), S, VId(0))],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        ids(&result.value)
    };
    assert_eq!(
        rows("MATCH (a) WHERE (a)-[:R]->() RETURN a"),
        vec![vec![Some(VId(0))]]
    );
    assert_eq!(
        rows("MATCH (a) WHERE NOT (a)-[:R]->() RETURN a"),
        vec![vec![Some(VId(1))], vec![Some(VId(2))]]
    );
    assert_eq!(
        rows("MATCH (a) WHERE (a)<-[:S]-() RETURN a"),
        vec![vec![Some(VId(0))]]
    );
    // Under OR it is a scope inside a compound Boolean, refused as EXISTS is.
    assert!(matches!(
        PreparedGraphText::prepare("MATCH (a) WHERE a.n > 1 OR (a)-[:R]->() RETURN a", symbols)
            .unwrap_err()
            .kind,
        GraphPatternTextErrorKind::UnsupportedBooleanScope
    ));
    // Parentheses followed by arithmetic or a comparison are not patterns.
    for text in [
        "MATCH (a) WHERE (a.n) - 2 > 0 RETURN a",
        "MATCH (a) WHERE (a.n + 1) <-3 RETURN a",
        // `- -` is edge-shaped; only the node-pattern body check rejects it.
        "MATCH (a) WHERE (a.n) - -1 > 0 RETURN a",
    ] {
        assert!(PreparedGraphText::prepare(text, symbols).is_ok(), "{text}");
    }
    // A pattern predicate tests bindings and introduces none (openCypher
    // UndefinedVariable, TCK Pattern1 [10]). The first new named vertex or
    // edge is refused at its own offset, bare, under NOT and in exists(...).
    // Anonymous nodes and edges bind nothing, and the EXISTS { MATCH ... }
    // spelling still declares existential locals.
    for (text, at) in [
        ("MATCH (a) WHERE (a)-[r]->(b) RETURN a", "r]"),
        ("MATCH (a) WHERE (b)-->(a) RETURN a", "b)"),
        ("MATCH (a) WHERE ()-[r]-() RETURN a", "r]"),
        ("MATCH (a) WHERE NOT (a)-[:R]->(m:L) RETURN a", "m:"),
        ("MATCH (a) WHERE exists((a)<-[r:S]-()) RETURN a", "r:"),
        (
            "MATCH (a) WHERE (a)-[:R]->(:L)<-[s:S]-(b {n: 5}) RETURN a",
            "s:",
        ),
    ] {
        let error = PreparedGraphText::prepare(text, symbols).unwrap_err();
        assert_eq!(
            (error.kind, error.offset),
            (
                GraphPatternTextErrorKind::UnknownVariable,
                text.find(at).unwrap()
            ),
            "{text}"
        );
    }
    assert!(
        PreparedGraphText::prepare(
            "MATCH (a) WHERE EXISTS { MATCH (a)-[r]->(b) } RETURN a",
            symbols
        )
        .is_ok()
    );
}

#[test]
fn required_match_after_optional_preserves_bound_correlations_without_rebinding_null() {
    let pattern = query("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) MATCH (b)-[:S]->(c) RETURN a,b,c");
    let actual = pattern
        .plan()
        .execute_governed_with_properties(
            7,
            [VId(1), VId(2), VId(3), VId(4)],
            [
                (VId(1), R, VId(2)),
                (VId(2), S, VId(3)),
                (VId(4), S, VId(3)),
            ],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        ids(&actual.value),
        vec![vec![Some(VId(1)), Some(VId(2)), Some(VId(3))]]
    );
}

#[test]
fn malformed_or_out_of_scope_definitions_refuse_before_catalog_access() {
    for text in [
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(local) } RETURN local",
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(local) } AND local.n > 0 RETURN a",
        "MATCH (a) OPTIONAL MATCH (b)-[:R]->(c) RETURN missing",
        "MATCH (a) WHERE EXISTS { MATCH (b)-[:R]->(c) } RETURN c",
        "MATCH (a) WHERE NOT EXISTS { MATCH (a) RETURN a } RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (a) RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (a) WHERE EXISTS { MATCH (local) } AND local.n > 0 } RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) } RETURN a",
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (local) } RETURN local",
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE b.n > 0 OR RETURN a",
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE b.n > 0 OR OR b.n < 0 RETURN a",
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE missing.n > 0 RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) WHERE b.n > $x } RETURN a LIMIT $x",
    ] {
        let mut calls = 0;
        let result = PreparedGraphText::prepare(text, |kind: GraphSymbolKind, name: &str| {
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
    let scalar = CanonicalPropertyValue::Scalar(CanonicalScalar::Int(7));
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
    let scalar = CanonicalPropertyValue::Scalar(CanonicalScalar::Int(7));
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

/// openCypher's `exists(<pattern>)` (fgdb-20foe) IS the pattern predicate:
/// identical plan bytes to the EXISTS { MATCH ... } spelling, bare, under
/// NOT, and as an AND conjunct. `exists(n.p)` (a property test) is not a
/// pattern and keeps its refusal, and a variable named exists stays one.
#[test]
fn exists_function_on_a_pattern_is_the_pattern_predicate() {
    for (function, exists) in [
        (
            "MATCH (a) WHERE exists((a)-[:R]->()) RETURN a",
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->() } RETURN a",
        ),
        (
            "MATCH (a) WHERE NOT exists((a)<-[:S]-(:L)) RETURN a",
            "MATCH (a) WHERE NOT EXISTS { MATCH (a)<-[:S]-(:L) } RETURN a",
        ),
        (
            "MATCH (a) WHERE a.n > 1 AND EXISTS((a)-[:R]->()) RETURN a",
            "MATCH (a) WHERE a.n > 1 AND EXISTS { MATCH (a)-[:R]->() } RETURN a",
        ),
    ] {
        assert_eq!(
            query(function).canonical_bytes(),
            query(exists).canonical_bytes(),
            "{function}"
        );
    }
    assert!(PreparedGraphText::prepare("MATCH (a) WHERE exists(a.n) RETURN a", symbols).is_err());
    assert_eq!(
        query("MATCH (exists)-[:R]->(b) WHERE exists.n = 1 RETURN b").columns(),
        &["b"]
    );
}

/// A bare pattern in a Boolean position of a projection is its existence
/// test, compiled exactly as the `exists(...)` spelling: after NOT, AND, OR
/// or XOR, inside parentheses there, and as a searched CASE's condition,
/// whether the RETURN reads the MATCH bindings directly or past a WITH (LDBC
/// SNB IC7, `NOT ((liker)-[:KNOWS]-(person)) AS isNew`). A whole RETURN or
/// WITH item is no Boolean position and stays refused (TCK Pattern1 [22],
/// [23]).
#[test]
fn a_bare_pattern_in_a_boolean_position_is_its_existence_test() {
    let template = |text: &str| {
        fgdb_gql::PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .canonical_template_bytes()
    };
    for (bare, exists) in [
        (
            "MATCH (a), (b) RETURN NOT (a)-[:R]-(b) AS e",
            "MATCH (a), (b) RETURN NOT exists((a)-[:R]-(b)) AS e",
        ),
        (
            "MATCH (a), (b) RETURN NOT ((a)-[:R]-(b)) AS e",
            "MATCH (a), (b) RETURN NOT exists((a)-[:R]-(b)) AS e",
        ),
        // `AND EXISTS` itself ends a projection's conjunction (the graph
        // clause parser owns a scoped EXISTS), so this reference spelling
        // parenthesizes it.
        (
            "MATCH (a) RETURN a.n > 1 AND (a)-[:R]->() AS e",
            "MATCH (a) RETURN a.n > 1 AND (exists((a)-[:R]->())) AS e",
        ),
        (
            "MATCH (a) RETURN a.n > 1 OR (a)<-[:S]-(:L) AS e",
            "MATCH (a) RETURN a.n > 1 OR exists((a)<-[:S]-(:L)) AS e",
        ),
        (
            "MATCH (a) RETURN a.n > 1 XOR (a)-[:R]->() AS e",
            "MATCH (a) RETURN a.n > 1 XOR exists((a)-[:R]->()) AS e",
        ),
        (
            "MATCH (a) RETURN CASE WHEN (a)-[:R]->() THEN 1 ELSE 0 END AS e",
            "MATCH (a) RETURN CASE WHEN exists((a)-[:R]->()) THEN 1 ELSE 0 END AS e",
        ),
        (
            "MATCH (a), (b) WITH a, b RETURN NOT ((a)-[:R]-(b)) AS e",
            "MATCH (a), (b) WITH a, b RETURN NOT exists((a)-[:R]-(b)) AS e",
        ),
    ] {
        assert_eq!(template(bare), template(exists), "{bare}");
    }
    for text in [
        "MATCH (n) RETURN (n)-[:R]->() AS x",
        "MATCH (n) WITH (n)-[:R]->() AS x RETURN x",
    ] {
        assert!(
            fgdb_gql::PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// fgdb-8tb9l: in a projection, `x AND exists(...)` is one conjunction,
/// compiled exactly as its parenthesized spelling. A graph clause's WHERE
/// still owns a scoped EXISTS after AND.
#[test]
fn a_projection_conjoins_an_existence_test_after_and() {
    let template = |text: &str| {
        fgdb_gql::PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .canonical_template_bytes()
    };
    for (written, parenthesized) in [
        (
            "MATCH (a) RETURN a.n > 1 AND exists((a)-[:R]->()) AS e",
            "MATCH (a) RETURN a.n > 1 AND (exists((a)-[:R]->())) AS e",
        ),
        (
            "MATCH (a) RETURN a.n > 1 AND NOT exists((a)-[:R]->()) AS e",
            "MATCH (a) RETURN a.n > 1 AND (NOT exists((a)-[:R]->())) AS e",
        ),
        (
            "MATCH (a) WITH a RETURN a.n > 1 AND EXISTS { MATCH (a)-[:R]->() } AS e",
            "MATCH (a) WITH a RETURN a.n > 1 AND (EXISTS { MATCH (a)-[:R]->() }) AS e",
        ),
    ] {
        assert_eq!(template(written), template(parenthesized), "{written}");
    }
    assert_eq!(
        query("MATCH (a) WHERE a.n > 1 AND EXISTS { MATCH (a)-[:R]->() } RETURN a")
            .canonical_bytes(),
        query("MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->() } AND a.n > 1 RETURN a")
            .canonical_bytes()
    );
}

/// fgdb-fj106 design 1: a relationship type test in a scoped WHERE (an
/// EXISTS body, an OPTIONAL MATCH) narrows that clause's edge, as the typed
/// pattern does: `type(r) = 'R'`, `'R' = type(r)`, `r:R` and `type(r) IN
/// [...]`. A type no catalog names matches nothing, and a test that the
/// written type excludes leaves nothing; neither is an error. Under OR or NOT
/// the test is no flat conjunct and refuses (TCK ExistentialSubquery1 [4]).
#[test]
fn scoped_relationship_type_tests_narrow_their_edge() {
    use fgdb_types::EId;
    let edges = [
        (EId(10), VId(0), R, VId(1)),
        (EId(11), VId(0), S, VId(2)),
        (EId(12), VId(1), S, VId(2)),
    ];
    let rows = |text: &str| {
        let mut rows = ids(&query(text)
            .plan()
            .execute_governed_with_identified_properties(
                5,
                [VId(0), VId(1), VId(2)],
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .value);
        rows.sort();
        rows
    };
    let typed = rows("MATCH (a) WHERE EXISTS { (a)-[r:R]->() } RETURN a");
    assert_eq!(typed, vec![vec![Some(VId(0))]]);
    for test in ["type(r) = 'R'", "'R' = type(r)", "r:R", "type(r) IN ['R']"] {
        let text = format!("MATCH (a) WHERE EXISTS {{ (a)-[r]->() WHERE {test} }} RETURN a");
        assert_eq!(rows(&text), typed, "{text}");
    }
    assert_eq!(
        rows("MATCH (a) WHERE EXISTS { (a)-[r]->() WHERE type(r) IN ['R', 'S'] } RETURN a"),
        vec![vec![Some(VId(0))], vec![Some(VId(1))]]
    );
    for empty in [
        "MATCH (a) WHERE EXISTS { (a)-[r]->() WHERE type(r) = 'NA' } RETURN a",
        "MATCH (a) WHERE EXISTS { (a)-[r:R]->() WHERE type(r) = 'S' } RETURN a",
        "MATCH (a) WHERE EXISTS { (a)-[r]->() WHERE type(r) IN [] } RETURN a",
    ] {
        assert!(rows(empty).is_empty(), "{empty}");
    }
    assert_eq!(
        rows("MATCH (a) OPTIONAL MATCH (a)-[r]->(b) WHERE type(r) = 'S' RETURN a, b"),
        vec![
            vec![Some(VId(0)), Some(VId(2))],
            vec![Some(VId(1)), Some(VId(2))],
            vec![Some(VId(2)), None],
        ]
    );
    for refused in [
        "MATCH (a) WHERE EXISTS { (a)-[r]->() WHERE type(r) = 'R' OR a.n = 1 } RETURN a",
        "MATCH (a) WHERE EXISTS { (a)-[r]->() WHERE NOT type(r) = 'R' } RETURN a",
    ] {
        assert!(
            PreparedGraphText::prepare(refused, symbols).is_err(),
            "{refused}"
        );
    }
}

#[test]
fn nested_existence_matches_finite_assignment_oracle_without_multiplying_bags() {
    let vertices = [VId(0), VId(1), VId(2), VId(0)];
    let universe = [
        (VId(0), R, VId(1)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(2)),
        (VId(0), S, VId(2)),
        (VId(1), S, VId(0)),
        (VId(2), S, VId(1)),
    ];
    for direction in 0..3 {
        for outer_anti in [false, true] {
            for inner_anti in [false, true] {
                let text = format!(
                    "MATCH (a) WHERE {}EXISTS {{ MATCH {} WHERE {}EXISTS {{ MATCH {} }} }} RETURN ALL a",
                    if outer_anti { "NOT " } else { "" },
                    path("a", "R", "x", direction),
                    if inner_anti { "NOT " } else { "" },
                    path("x", "S", "y", direction),
                );
                let prepared = query(&text);
                for mask in 0..1_u32 << universe.len() {
                    let mut edges: Vec<_> = universe
                        .iter()
                        .enumerate()
                        .filter(|(at, _)| mask & (1 << at) != 0)
                        .map(|(_, edge)| *edge)
                        .collect();
                    // Parallel occurrences cannot multiply either existential.
                    if let Some(edge) = edges.first().copied() {
                        edges.push(edge);
                    }
                    let mut expected: Vec<_> = vertices
                        .iter()
                        .copied()
                        .filter(|&a| {
                            orient(&edges, R, a, direction)
                                .into_iter()
                                .any(|x| !orient(&edges, S, x, direction).is_empty() != inner_anti)
                                != outer_anti
                        })
                        .map(|a| vec![Some(a)])
                        .collect();
                    expected.sort();
                    let result = prepared
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
                        .unwrap();
                    assert_eq!(ids(&result.value), expected, "{text}; mask={mask}");
                }
            }
        }
    }
}

#[test]
fn nested_existence_carries_ancestors_but_keeps_local_names_and_nulls_private() {
    use fgdb_types::EId;
    let run = |text: &str| {
        let result = query(text)
            .plan()
            .execute_governed_with_identified_properties(
                7,
                [VId(0), VId(1), VId(2), VId(3)],
                [
                    (EId(1), VId(0), R, VId(1)),
                    (EId(2), VId(0), R, VId(2)),
                    (EId(3), VId(1), S, VId(3)),
                ],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        ids(&result.value)
    };
    // a is absent from both intermediate MATCH patterns. The deepest scope
    // must receive the original a, rather than scan an unrelated replacement.
    assert_eq!(
        run(
            "MATCH (a) WHERE EXISTS { MATCH (x) WHERE EXISTS { MATCH (y) \
             WHERE EXISTS { MATCH (a)-[:R]->(x)-[:S]->(y) } } } RETURN a"
        ),
        vec![vec![Some(VId(0))]],
    );
    // Different MATCH scopes may witness with the same edge. Flattening this
    // into one DIFFERENT EDGES pattern would incorrectly require parallel IDs.
    assert_eq!(
        run("MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) \
             WHERE EXISTS { MATCH (a)-[:R]->(b) } } RETURN a"),
        vec![vec![Some(VId(0))]],
    );
    // A rejected positive OPTIONAL witness is not a match. With no surviving
    // witness its local is extended with NULL exactly once.
    assert_eq!(
        run("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) \
             WHERE NOT EXISTS { MATCH (b)-[:S]->(c) WHERE EXISTS { MATCH (c) } } \
             RETURN ALL a,b"),
        vec![
            vec![Some(VId(0)), Some(VId(2))],
            vec![Some(VId(1)), None],
            vec![Some(VId(2)), None],
            vec![Some(VId(3)), None],
        ],
    );
    // The outer b is nullable. Passing it through an independent scope must
    // not turn it into a positive scan, even for a zero-hop nested pattern.
    assert_eq!(
        run("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) \
             MATCH (z) WHERE EXISTS { MATCH (x) \
             WHERE NOT EXISTS { MATCH (b)-[:R*0..0]->(x) } } RETURN a,b,z"),
        (0..4)
            .flat_map(|a| {
                let bs = if a == 0 {
                    vec![Some(VId(1)), Some(VId(2))]
                } else {
                    vec![None]
                };
                bs.into_iter()
                    .flat_map(move |b| (0..4).map(move |z| vec![Some(VId(a)), b, Some(VId(z))]))
            })
            .collect::<Vec<_>>(),
    );
    // A nested local can be reused by a later independent clause, but never
    // exported implicitly by EXISTS.
    assert_eq!(
        run("MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(x) \
             WHERE EXISTS { MATCH (x)-[:S]->(local) } } MATCH (local) RETURN a,local"),
        (0..4)
            .map(|id| vec![Some(VId(0)), Some(VId(id))])
            .collect::<Vec<_>>(),
    );
    for text in [
        "MATCH (a) WHERE EXISTS { MATCH (x) WHERE EXISTS { MATCH (local) } } RETURN local",
        "MATCH (a) WHERE EXISTS { MATCH (x) WHERE EXISTS { MATCH (local) } AND local.n=1 } RETURN a",
    ] {
        assert!(PreparedGraphText::prepare(text, symbols).is_err(), "{text}");
    }
}

#[test]
fn nested_existence_rebinds_parameters_and_has_distinct_bounded_template_identity() {
    use std::cell::Cell;
    let text = "MATCH (a) WHERE EXISTS { MATCH (x) WHERE EXISTS { MATCH (a)-[:R]->(x) \
        WHERE x.n > $floor } } RETURN a";
    let calls = Cell::new(0);
    let template = PreparedGraphText::prepare(text, |kind, name: &str| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    let before = calls.get();
    let bind = |floor| {
        template
            .bind_parameters(&GqlParameters::new().with_int64("floor", floor).unwrap())
            .unwrap()
    };
    let first = bind(1);
    assert_ne!(first.canonical_bytes(), bind(2).canonical_bytes());
    assert_eq!(first.canonical_bytes(), bind(1).canonical_bytes());
    assert_eq!(calls.get(), before);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    let matches = |vid: VId, predicates: &[VertexPredicate]| {
        Ok::<_, ()>(predicates.iter().all(|predicate| {
            predicate.matches(
                &[],
                &[(
                    P,
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(vid.0 as i64)),
                )],
            )
        }))
    };
    for (floor, wanted) in [(1, vec![vec![Some(VId(0))]]), (2, vec![])] {
        let bound = bind(floor);
        let rows = bound
            .plan()
            .execute_governed_with_properties(
                3,
                [VId(0), VId(2)],
                [(VId(0), R, VId(2))],
                matches,
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(ids(&rows.value), wanted);
        let aggregate = PreparedGraphAggregateText::prepare(
            &text.replace("RETURN a", "RETURN count(*) AS c"),
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new().with_int64("floor", floor).unwrap())
        .unwrap();
        let rows = aggregate
            .execute_governed(
                3,
                [VId(0), VId(2)],
                [(VId(0), R, VId(2))],
                matches,
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].get(0).unwrap().as_count(),
            Some(wanted.len() as u64)
        );
    }
    let negative = text.replacen(
        "WHERE EXISTS { MATCH (a)",
        "WHERE NOT EXISTS { MATCH (a)",
        1,
    );
    assert_ne!(
        template.template_bytes(),
        PreparedGraphText::prepare(&negative, symbols)
            .unwrap()
            .template_bytes(),
    );
    let nested = |count: usize| {
        format!(
            "MATCH (a) WHERE {}MATCH (a){} RETURN a",
            "EXISTS { MATCH (a) WHERE ".repeat(count.saturating_sub(1)) + "EXISTS { ",
            " }".repeat(count)
        )
    };
    assert!(PreparedGraphText::prepare(&nested(64), symbols).is_ok());
    let mut refused_calls = 0;
    let error = PreparedGraphText::prepare(&nested(65), |kind, name: &str| {
        refused_calls += 1;
        symbols(kind, name)
    })
    .unwrap_err();
    assert!(matches!(
        error.kind,
        GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded { .. })
    ));
    assert_eq!(
        refused_calls, 0,
        "depth is refused before catalog observation"
    );
}

#[test]
fn nested_existence_preserves_short_circuit_limits_and_every_interruption_boundary() {
    let pattern = query(
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) \
        WHERE EXISTS { MATCH (b)-[:S]->(c) WHERE c.n + 1 >= 0 } } RETURN a",
    );
    let scalar = CanonicalPropertyValue::Scalar(CanonicalScalar::Int(1));
    let vertices = [VId(0)];
    let edges = [
        (VId(0), R, VId(1)),
        (VId(0), R, VId(2)),
        (VId(1), S, VId(3)),
        (VId(2), S, VId(4)),
    ];
    let mut checkpoints = 0;
    let full = pattern
        .plan()
        .execute_governed_with_properties(
            5,
            vertices,
            edges,
            |_, _| Ok::<_, &str>(true),
            |vid, _| {
                if vid == VId(4) {
                    Err("unrequested witness")
                } else {
                    Ok(Some(&scalar))
                }
            },
            wide(),
            || {
                checkpoints += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    assert_eq!(ids(&full.value), vec![vec![Some(VId(0))]]);
    for stop in 1..=checkpoints {
        let mut seen = 0;
        let result = pattern.plan().execute_governed_with_properties(
            5,
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
    let run = |policy| {
        pattern.plan().execute_governed_with_properties(
            5,
            vertices,
            edges,
            |_, _| Ok::<_, &str>(true),
            |_, _| Ok(Some(&scalar)),
            policy,
            || Ok::<_, usize>(()),
        )
    };
    assert_eq!(
        run(GqlQueryPolicy::new(
            5,
            1,
            full.evaluator.work_units,
            full.evaluator.scratch_entries
        ))
        .unwrap(),
        full
    );
    for policy in [
        GqlQueryPolicy::new(4, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(5, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(5, 1, full.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(5, 1, u64::MAX, full.evaluator.scratch_entries - 1),
    ] {
        assert!(run(policy).is_err());
    }
    let error = pattern
        .plan()
        .execute_governed_with_properties(
            5,
            vertices,
            edges,
            |_, _| Ok::<_, &str>(true),
            |_, _| Err("requested nested property"),
            wide(),
            || Ok::<_, usize>(()),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        GqlQueryError::Source("requested nested property")
    ));
}

#[test]
fn nested_existence_keeps_sibling_collection_group_slots_and_occurrences() {
    use fgdb_gql::{PreparedGraphSetText, algebra::GraphValue};
    let query = PreparedGraphSetText::prepare(
        "MATCH (a) RETURN a, \
         [(a)-[:R]->(b) WHERE EXISTS { MATCH (b)-[:S]->() } | b] AS connected, \
         [(a)-[:R]->(b) | b] AS all_neighbors",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let result = query
        .execute_governed_with_elements(
            wide(),
            |pattern, remaining| {
                pattern.plan().execute_governed_with_properties(
                    8,
                    [VId(0), VId(1), VId(2), VId(3)],
                    [
                        (VId(0), R, VId(1)),
                        (VId(0), R, VId(1)),
                        (VId(0), R, VId(2)),
                        (VId(1), S, VId(3)),
                    ],
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    remaining,
                    || Ok::<_, ()>(()),
                )
            },
            |_, _, _| panic!("fixture has no procedure source"),
            |_, _| panic!("identity collections read no properties"),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let list = |ids: &[u128]| {
        GraphValue::List(ids.iter().map(|&id| GraphValue::Vertex(VId(id))).collect())
    };
    let mut expected = vec![GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(0)),
        list(&[1, 1]),
        list(&[1, 1, 2]),
    ])];
    expected.extend((1..=3).map(|id| {
        GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(id)), list(&[]), list(&[])])
    }));
    assert_eq!(result.value, expected);
}

#[test]
fn nested_existence_keeps_anonymous_nodes_distinct_across_ancestor_frames() {
    let run = |text: &str| {
        let result = query(text)
            .plan()
            .execute_governed_with_properties(
                9,
                [VId(0), VId(1), VId(2)],
                [(VId(0), R, VId(1)), (VId(0), S, VId(2))],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        ids(&result.value)
    };
    for text in [
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->() \
         WHERE EXISTS { MATCH (a)-[:S]->() } } RETURN a",
        "MATCH (a)-[:R]->() WHERE EXISTS { MATCH (a)-[:S]->() } RETURN a",
        "MATCH (a)-[:R]->() WHERE EXISTS { MATCH (a)-[:S]->() \
         WHERE EXISTS { MATCH (a)-[:R]->() } } RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->() } \
         AND EXISTS { MATCH (a)-[:S]->() } RETURN a",
    ] {
        assert_eq!(run(text), vec![vec![Some(VId(0))]], "{text}");
    }
    assert!(
        run("MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->() \
             WHERE NOT EXISTS { MATCH (a)-[:S]->() } } RETURN a")
        .is_empty(),
    );
    assert_eq!(
        run("MATCH ()-[:R]->(b) WHERE EXISTS { MATCH ()-[:S]->() } RETURN b"),
        vec![vec![Some(VId(1))]],
    );
    let full = std::iter::repeat_n("()", fgdb_gql::algebra::MAX_PATTERN_VERTICES)
        .collect::<Vec<_>>()
        .join(",");
    let error = PreparedGraphAggregateText::prepare(
        &format!("MATCH {full} WHERE EXISTS {{ MATCH () }} RETURN count(*) AS c"),
        symbols,
    )
    .unwrap_err();
    assert!(matches!(
        error.kind,
        GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
            dimension: fgdb_gql::algebra::PatternLimitDimension::Vertices,
            ..
        })
    ));
}

#[test]
fn nested_existence_audits_imported_and_element_names_at_every_depth() {
    use fgdb_gql::{GraphSetTextErrorKind, PreparedGraphSetText};

    for (text, expected) in [
        (
            "MATCH (a) WITH a MATCH (z) WHERE EXISTS { MATCH (x) \
             WHERE EXISTS { MATCH (a)-[:R]->(x) } } RETURN a,z",
            GraphSetTextErrorKind::Expected("imported vertices in the root MATCH"),
        ),
        (
            "MATCH (a) RETURN [v IN [a] | COUNT { MATCH (x) \
             WHERE EXISTS { MATCH (v)-[:R]->(x) } }] AS n",
            GraphSetTextErrorKind::Expected("imported vertices in the root MATCH"),
        ),
        (
            "MATCH (a) RETURN [v IN [a] | [(x)-[:S]->(y) \
             WHERE EXISTS { MATCH (v)-[:R]->(x) } | x]] AS n",
            GraphSetTextErrorKind::Expected(
                "imported vertices in the comprehension's positive pattern",
            ),
        ),
        (
            "MATCH (a) RETURN any(v IN [a] WHERE EXISTS { MATCH (x) \
             WHERE EXISTS { MATCH (v)-[:R]->(x) } }) AS n",
            GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::Expected(
                "a pattern count that reads no enclosing comprehension local",
            )),
        ),
        (
            "MATCH (a), (z) WITH z WHERE EXISTS { MATCH (x) \
             WHERE EXISTS { MATCH (a)-[:R]->(x) } } RETURN z",
            GraphSetTextErrorKind::Expected(
                "an existence predicate over bindings its WITH carries under their own names",
            ),
        ),
    ] {
        let error = match PreparedGraphSetText::prepare(text, symbols) {
            Ok(_) => panic!("silently rebound an out-of-scope name: {text}"),
            Err(error) => error,
        };
        assert_eq!(error.kind, expected, "{text}");
    }
    // The supported forms keep the imported vertex in the root MATCH, or
    // use an actual graph ancestor rather than a per-element local.
    for text in [
        "MATCH (a) WITH a MATCH (a), (z) WHERE EXISTS { MATCH (x) \
         WHERE EXISTS { MATCH (a)-[:R]->(x) } } RETURN a,z",
        "MATCH (a) RETURN [v IN [a] | COUNT { MATCH (x) \
         WHERE EXISTS { MATCH (a)-[:R]->(x) } }] AS n",
        "MATCH (a) RETURN [v IN [a] | [(x)-[:S]->(y) \
         WHERE EXISTS { MATCH (a)-[:R]->(x) } | x]] AS n",
        "MATCH (a) WITH a WHERE EXISTS { MATCH (x) \
         WHERE EXISTS { MATCH (a)-[:R]->(x) } } RETURN a",
    ] {
        PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap();
    }
}
mod tuple_comprehensions {
    use super::{L, P, R, S, symbols, wide};
    use fgdb_delta_types::ElementId;
    use fgdb_gql::algebra::{
        GlaDirection, GraphColumn, GraphMatchClause, GraphPath, GraphPatternBuilder, GraphValue,
        GraphValueRow, MAX_PATTERN_VERTICES, PatternBuildError, PatternLimitDimension,
    };
    use fgdb_gql::{
        GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetExecutionError,
        PreparedGraphSet, PreparedGraphSetText,
    };
    use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    type Edge = (EId, VId, fgdb_delta_types::RelationId, VId);
    type Result = std::result::Result<
        GqlQueryExecution<GraphValueRow>,
        GqlQueryError<GraphSetExecutionError<&'static str>, usize>,
    >;
    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn list(values: Vec<GraphValue>) -> GraphValue {
        GraphValue::List(values.into_boxed_slice())
    }
    fn row(values: Vec<GraphValue>) -> GraphValueRow {
        GraphValueRow::from_owned_values(values)
    }
    fn prepare(text: &str) -> PreparedGraphSet {
        PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    }
    fn vertex_property(vid: VId) -> i64 {
        (vid.0 & 255) as i64 + 1
    }
    fn edge_property(eid: EId) -> i64 {
        (eid.0 & 255) as i64 + 100
    }
    fn run(
        query: &PreparedGraphSet,
        vertices: &[VId],
        edges: &[Edge],
        policy: GqlQueryPolicy,
        checkpoint: &mut impl FnMut() -> std::result::Result<(), usize>,
        fail_edge_property: bool,
    ) -> Result {
        let values: BTreeMap<VId, CanonicalPropertyValue> = vertices
            .iter()
            .map(|&vid| (vid, CanonicalScalar::Int(vertex_property(vid)).into()))
            .collect();
        let checkpoint = RefCell::new(checkpoint);
        query.execute_governed_with_elements(
            policy,
            |pattern, allowance| {
                pattern.plan().execute_governed_with_identified_properties(
                    (vertices.len() + edges.len()) as u64,
                    vertices.iter().copied(),
                    edges.iter().copied(),
                    |vid, predicates| {
                        Ok::<_, &'static str>(
                            predicates.iter().all(|predicate| {
                                predicate.matches(&[L], &[(P, values[&vid].clone())])
                            }),
                        )
                    },
                    |vid, key| Ok((key == P).then(|| &values[&vid])),
                    allowance,
                    || (checkpoint.borrow_mut())(),
                )
            },
            |_, _, _| panic!("tuple fixture has no procedure source"),
            |element, key| {
                if key != P {
                    return Ok(None);
                }
                Ok(Some(CanonicalPropertyValue::Scalar(CanonicalScalar::Int(
                    match element {
                        ElementId::Vertex(vid) => vertex_property(vid),
                        ElementId::Edge(eid) => {
                            if fail_edge_property {
                                return Err("tuple edge property failure");
                            }
                            edge_property(eid)
                        }
                    },
                ))))
            },
            || (checkpoint.borrow_mut())(),
        )
    }
    fn execute(text: &str, vertices: &[VId], edges: &[Edge]) -> Vec<GraphValueRow> {
        run(
            &prepare(text),
            vertices,
            edges,
            wide(),
            &mut || Ok(()),
            false,
        )
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .value
    }
    fn neighbors(
        edges: &[Edge],
        relation: fgdb_delta_types::RelationId,
        source: VId,
        direction: usize,
    ) -> Vec<(VId, EId)> {
        let mut result = Vec::new();
        for &(edge, from, actual, to) in edges {
            if actual != relation {
                continue;
            }
            if direction != 1 && from == source {
                result.push((to, edge));
            }
            if direction != 0 && to == source && (direction == 1 || from != to) {
                result.push((from, edge));
            }
        }
        result.sort();
        result
    }

    #[test]
    fn witness_tuples_match_finite_multigraph_assignments_in_all_directions() {
        let vertices = [VId(0), VId(1), VId(2), VId(0)];
        let universe = [
            (EId(10), VId(0), R, VId(1)),
            (EId(11), VId(0), R, VId(1)),
            (EId(12), VId(1), R, VId(2)),
            (EId(13), VId(1), S, VId(0)),
            (EId(14), VId(1), S, VId(2)),
            (EId(15), VId(2), S, VId(2)),
        ];
        for (direction, pattern) in [
            "(a)-[r:R]->(b)-[s:S]->(c)",
            "(a)<-[r:R]-(b)<-[s:S]-(c)",
            "(a)-[r:R]-(b)-[s:S]-(c)",
        ]
        .into_iter()
        .enumerate()
        {
            let text = format!("MATCH (a) RETURN ALL a,[{pattern} | [r,b,s,c]] AS witnesses");
            let query = prepare(&text);
            for mask in 0..(1_u32 << universe.len()) {
                let mut edges: Vec<_> = universe
                    .iter()
                    .enumerate()
                    .filter_map(|(bit, &edge)| (mask & (1 << bit) != 0).then_some(edge))
                    .collect();
                let mut expected: Vec<_> = vertices
                    .iter()
                    .map(|&a| {
                        let mut witnesses = Vec::new();
                        for (b, r) in neighbors(&edges, R, a, direction) {
                            for (c, s) in neighbors(&edges, S, b, direction) {
                                witnesses.push(list(vec![
                                    GraphValue::Edge(r),
                                    GraphValue::Vertex(b),
                                    GraphValue::Edge(s),
                                    GraphValue::Vertex(c),
                                ]));
                            }
                        }
                        row(vec![GraphValue::Vertex(a), list(witnesses)])
                    })
                    .collect();
                expected.sort();
                // Source order changes must not change the tuple associations,
                // parallel occurrences or canonical adjacency enumeration.
                for reversed in [false, true] {
                    if reversed {
                        edges.reverse();
                    }
                    let mut actual = run(&query, &vertices, &edges, wide(), &mut || Ok(()), false)
                        .unwrap()
                        .value;
                    actual.sort();
                    assert_eq!(actual, expected, "{text}; mask={mask}; reversed={reversed}");
                }
            }
        }
    }

    #[test]
    fn tuple_projection_reads_nodes_edges_paths_and_outer_or_shadowed_locals() {
        let vertices = [VId(0), VId(1), VId(2)];
        let edges = [
            (EId(10), VId(0), R, VId(1)),
            (EId(11), VId(0), R, VId(1)),
            (EId(12), VId(1), R, VId(2)),
        ];
        let documents: Vec<_> = vertices
            .iter()
            .map(|&a| {
                row(vec![
                    GraphValue::Vertex(a),
                    list(
                        neighbors(&edges, R, a, 0)
                            .into_iter()
                            .map(|(b, r)| {
                                GraphValue::map(vec![
                                    ("node".into(), GraphValue::Vertex(b)),
                                    ("edge".into(), GraphValue::Edge(r)),
                                    (
                                        "sum".into(),
                                        int(vertex_property(a)
                                            + vertex_property(b)
                                            + edge_property(r)),
                                    ),
                                ])
                                .unwrap()
                            })
                            .collect(),
                    ),
                ])
            })
            .collect();
        assert_eq!(
            execute(
                "MATCH (a) RETURN a,[(a)-[r:R]->(b) | {node:b, edge:r, sum:b.n+r.n+a.n}] AS docs ORDER BY a",
                &vertices,
                &edges
            ),
            documents
        );
        let paths: Vec<_> = vertices
            .iter()
            .map(|&a| {
                row(vec![
                    GraphValue::Vertex(a),
                    list(
                        neighbors(&edges, R, a, 0)
                            .into_iter()
                            .map(|(b, r)| {
                                list(vec![
                                    GraphValue::Path(GraphPath::new(a, Box::new([(r, b)]))),
                                    GraphValue::Edge(r),
                                    GraphValue::Vertex(b),
                                ])
                            })
                            .collect(),
                    ),
                ])
            })
            .collect();
        assert_eq!(
            execute(
                "MATCH (a) RETURN a,[p=(a)-[r:R]->(b) | [p,r,b]] AS paths ORDER BY a",
                &vertices,
                &edges
            ),
            paths
        );
        for (body, expected) in [
            (
                "[b IN [10,20] | b+r.n]",
                list(vec![
                    list(vec![int(120), int(130)]),
                    list(vec![int(121), int(131)]),
                ]),
            ),
            (
                "reduce(total=b.n,x IN [1,2] | total+x+r.n)",
                list(vec![int(225), int(227)]),
            ),
            (
                "single(b IN [0,110] WHERE b=r.n)",
                list(vec![
                    GraphValue::Scalar(CanonicalScalar::Bool(true)),
                    GraphValue::Scalar(CanonicalScalar::Bool(false)),
                ]),
            ),
            (
                "{b:1,r:2}",
                list(vec![
                    GraphValue::map(vec![
                        ("b".into(), int(1)),
                        ("r".into(), int(2))
                    ])
                    .unwrap();
                    2
                ]),
            ),
        ] {
            let text = format!("MATCH (a {{n:1}}) RETURN [(a)-[r:R]->(b) | {body}] AS v");
            assert_eq!(
                execute(&text, &vertices, &edges),
                vec![row(vec![expected])],
                "{text}"
            );
        }
        // The inner pattern is independent of x, so it gathers once per graph
        // row. Its tuple's Let fields must not displace the outer x binding.
        assert_eq!(
            execute(
                "MATCH (a {n:1}) RETURN [x IN [10,20] | [(a)-[r:R]->(b) | x+b.n+r.n]] AS v",
                &vertices,
                &edges
            ),
            vec![row(vec![list(vec![
                list(vec![int(122), int(123)]),
                list(vec![int(132), int(133)])
            ])])]
        );
        for text in [
            "MATCH (a) RETURN [(a)-[r:R]->(b) | [r,b]] AS v,b",
            "MATCH (a) RETURN [(a)-[r:R]->(b) | [r,b]] AS v,r",
            "MATCH (a) RETURN [x IN [a] | [(x)-[r:R]->(b) | [r,b]]] AS v",
        ] {
            assert!(
                PreparedGraphSetText::prepare(text, symbols).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn tuple_collectors_keep_nullable_correlations_sibling_groups_and_empty_lists() {
        let vertices = [VId(0), VId(1), VId(2)];
        let edges = [(EId(10), VId(0), S, VId(1)), (EId(11), VId(1), R, VId(2))];
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        let empty = list(vec![]);
        assert_eq!(
            execute(
                "MATCH (a) OPTIONAL MATCH (a)-[:S]->(missing) RETURN a,missing,[(missing)-[r:R]->(b) | [r,b]] AS v ORDER BY a",
                &vertices,
                &edges
            ),
            vec![
                row(vec![
                    GraphValue::Vertex(VId(0)),
                    GraphValue::Vertex(VId(1)),
                    list(vec![list(vec![
                        GraphValue::Edge(EId(11)),
                        GraphValue::Vertex(VId(2))
                    ])])
                ]),
                row(vec![
                    GraphValue::Vertex(VId(1)),
                    null.clone(),
                    empty.clone()
                ]),
                row(vec![GraphValue::Vertex(VId(2)), null, empty.clone()]),
            ]
        );
        assert_eq!(
            execute(
                "MATCH (a {n:1}) RETURN [(a)-[r:R]->(b) | [r,b]] AS first,[(a)-[r:S]->(b) | [b,r]] AS second",
                &vertices,
                &edges
            ),
            vec![row(vec![
                empty,
                list(vec![list(vec![
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Edge(EId(10))
                ])])
            ])]
        );
    }

    #[test]
    fn tuple_projection_limits_interruptions_and_property_errors_never_return_partial_rows() {
        let vertices = [VId(0), VId(1), VId(2)];
        let edges = [(EId(10), VId(0), R, VId(1)), (EId(11), VId(0), R, VId(2))];
        let query = prepare("MATCH (a) RETURN a,[(a)-[r:R]->(b) | [b.n,r.n]] AS v ORDER BY a");
        let mut checkpoints = 0;
        let complete = run(
            &query,
            &vertices,
            &edges,
            wide(),
            &mut || {
                checkpoints += 1;
                Ok(())
            },
            false,
        )
        .unwrap();
        for stop in 1..=checkpoints {
            let mut at = 0;
            let refused = run(
                &query,
                &vertices,
                &edges,
                wide(),
                &mut || {
                    at += 1;
                    if at == stop { Err(at) } else { Ok(()) }
                },
                false,
            );
            assert!(
                matches!(refused,Err(GqlQueryError::Interrupted(at)) if at==stop),
                "cut {stop}"
            );
            assert_eq!(at, stop, "no control resumes after refusal");
        }
        let exact = GqlQueryPolicy::new(
            complete.rows.snapshot_records,
            complete.rows.result_rows,
            complete.evaluator.work_units,
            complete.evaluator.scratch_entries,
        );
        assert_eq!(
            run(&query, &vertices, &edges, exact, &mut || Ok(()), false).unwrap(),
            complete
        );
        for dimension in 0..4 {
            let mut limits = [
                complete.rows.snapshot_records,
                complete.rows.result_rows,
                complete.evaluator.work_units,
                complete.evaluator.scratch_entries,
            ];
            assert!(limits[dimension] > 0);
            limits[dimension] -= 1;
            let refused = run(
                &query,
                &vertices,
                &edges,
                GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                &mut || Ok(()),
                false,
            );
            assert!(
                matches!(
                    refused,
                    Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))
                ),
                "dimension {dimension}: {refused:?}"
            );
        }
        assert!(matches!(
            run(&query, &vertices, &edges, wide(), &mut || Ok(()), true),
            Err(GqlQueryError::Source(_))
        ));
        // A dead CASE branch still refers to both locals, but its property
        // failure stays dead; tuple capture itself never reads properties.
        let dead = prepare(
            "MATCH (a {n:1}) RETURN [(a)-[r:R]->(b) | CASE WHEN true THEN b ELSE r.n/0 END] AS v",
        );
        assert_eq!(
            run(&dead, &vertices, &edges, wide(), &mut || Ok(()), true)
                .unwrap()
                .value,
            vec![row(vec![list(vec![
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(2))
            ])])]
        );
        for (text, expected) in [
            (
                "MATCH (a {n:1}) RETURN [(a)-[r:R]->(b) | CASE WHEN true THEN b.n ELSE r.n/0 END] AS v",
                vec![int(2), int(3)],
            ),
            (
                "MATCH (a {n:1}) RETURN [(a)-[r:R]->(b) | CASE WHEN true THEN b.n WHEN r.n > 0 THEN 0 ELSE 1 END] AS v",
                vec![int(2), int(3)],
            ),
            (
                "MATCH (a {n:1}) RETURN [(a)-[r:R]->(b) | coalesce(b.n,1+r.n/0)] AS v",
                vec![int(2), int(3)],
            ),
            (
                "MATCH (a {n:1}) RETURN [(a)-[r:R]->() | CASE WHEN true THEN r ELSE r.n/0 END] AS v",
                vec![GraphValue::Edge(EId(10)), GraphValue::Edge(EId(11))],
            ),
            (
                "MATCH (a {n:1}) RETURN [(a)-[r:R]->() | coalesce(7,r.n/0)] AS v",
                vec![int(7), int(7)],
            ),
            (
                "MATCH (a {n:1}) WITH [(a)-[r:R]->() | r] AS edges RETURN [r IN edges | CASE WHEN true THEN 7 ELSE r.n/0 END] AS v",
                vec![int(7), int(7)],
            ),
        ] {
            assert_eq!(
                run(
                    &prepare(text),
                    &vertices,
                    &edges,
                    wide(),
                    &mut || Ok(()),
                    true
                )
                .unwrap_or_else(|error| panic!("{text}: {error:?}"))
                .value,
                vec![row(vec![list(expected)])],
                "{text}"
            );
        }
        let failing = prepare("MATCH (a {n:1}) RETURN [(a)-[r:R]->(b) | b.n/(r.n-110)] AS v");
        assert!(run(&failing, &vertices, &edges, wide(), &mut || Ok(()), false).is_err());
        assert_eq!(
            run(&query, &vertices, &edges, wide(), &mut || Ok(()), false).unwrap(),
            complete
        );
    }

    #[test]
    fn typed_tuple_width_identity_and_full_width_bindings_are_bounded() {
        let mut outer = GraphPatternBuilder::new();
        outer.vertex("a").unwrap();
        let mut inner = GraphPatternBuilder::new();
        inner.vertex("a").unwrap().vertex("b").unwrap();
        inner.edge("a", R, GlaDirection::Forward, "b").unwrap();
        let compile = |clause| {
            outer.prepare_values_with_clauses(
                &[clause],
                &[GraphColumn::collection("w", 0)],
                0,
                None,
            )
        };
        let scalar = compile(GraphMatchClause::collect(&inner, Some("b"))).unwrap();
        let singleton = compile(GraphMatchClause::collect_tuple(&inner, vec!["b"])).unwrap();
        assert_ne!(
            scalar.plan().canonical_bytes(),
            singleton.plan().canonical_bytes()
        );
        let query = compile(GraphMatchClause::collect_tuple(
            &inner,
            vec!["b"; MAX_PATTERN_VERTICES],
        ))
        .unwrap();
        let a = VId((1_u128 << 127) + 7);
        let b = VId((1_u128 << 120) + 9);
        let result = query
            .plan()
            .execute_governed_with_properties(
                3,
                [a, b],
                [(a, R, b)],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        // Typed value plans end with OrderByValues: the empty collection
        // sorts before the nonempty one, independently of root scan order.
        assert_eq!(
            result.value,
            vec![
                row(vec![list(vec![])]),
                row(vec![list(vec![list(vec![
                    GraphValue::Vertex(b);
                    MAX_PATTERN_VERTICES
                ])])]),
            ]
        );
        assert!(matches!(
            compile(GraphMatchClause::collect_tuple(
                &inner,
                vec!["b"; MAX_PATTERN_VERTICES + 1]
            )),
            Err(PatternBuildError::LimitExceeded {
                dimension: PatternLimitDimension::Columns,
                ..
            })
        ));
        assert!(matches!(
            compile(GraphMatchClause::collect_tuple(&inner, vec!["missing"])),
            Err(PatternBuildError::UnknownVariable)
        ));
    }
}
