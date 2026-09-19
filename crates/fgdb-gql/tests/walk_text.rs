//! Quantified text is a front end to the same typed bounded-WALK compiler.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphPatternBuilder, GraphValueRow, IntegerComparison,
    PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    GraphWalkBounds, MAX_GRAPH_WALK_HOPS, PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(3))),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(PropertyKeyId(4))),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 1_000_000)
}
fn predicate(vid: VId, predicates: &[VertexPredicate]) -> Result<bool, ()> {
    let labels = if vid == VId(1) || vid == VId(4) {
        vec![LabelId(3)]
    } else {
        vec![]
    };
    Ok(predicates.iter().all(|p| p.matches(&labels, &[])))
}

#[test]
fn quantified_text_has_the_same_plan_as_typed_walks_and_rebinds_without_resolving() {
    for (quantifier, minimum, maximum) in [("*0..3", 0, 3), ("*2", 2, 2), ("*..3", 1, 3)] {
        for (left, right, direction) in [
            ("-", "->", GlaDirection::Forward),
            ("<-", "-", GlaDirection::Reverse),
            ("-", "-", GlaDirection::Undirected),
        ] {
            let text = format!(
                "MATCH WALK (a:L){left}[:R{quantifier}]{right}(b) \
                WHERE b.n >= $floor RETURN a,b.n AS score SKIP $off LIMIT $take"
            );
            let calls = Cell::new(0);
            let template = PreparedGraphText::prepare(&text, |kind: GraphSymbolKind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .unwrap();
            assert_eq!(calls.get(), 3, "each unique graph symbol resolves once");
            let arguments = GqlParameters::new()
                .with_int64("floor", 5)
                .unwrap()
                .with_uint64("off", 1)
                .unwrap()
                .with_uint64("take", 3)
                .unwrap();
            let actual = template.bind_parameters(&arguments).unwrap();
            let mut b = GraphPatternBuilder::new();
            b.vertex("a").unwrap();
            b.vertex("b").unwrap();
            b.filter("a", VertexPredicate::HasLabel(LabelId(3)))
                .unwrap();
            b.walk(
                "a",
                RelationId(1),
                direction,
                "b",
                GraphWalkBounds::new(minimum, maximum).unwrap(),
            )
            .unwrap();
            b.filter(
                "b",
                VertexPredicate::IntegerProperty {
                    key: PropertyKeyId(4),
                    comparison: IntegerComparison::GreaterOrEqual,
                    value: 5,
                },
            )
            .unwrap();
            let expected = b
                .prepare_values(
                    &[
                        GraphColumn::vertex("a", "a"),
                        GraphColumn::property("score", "b", PropertyKeyId(4)),
                    ],
                    1,
                    Some(3),
                )
                .unwrap()
                .with_duplicates();
            assert_eq!(actual, expected, "{text}");
            let frozen = actual.canonical_bytes();
            let changed = GqlParameters::new()
                .with_int64("floor", 6)
                .unwrap()
                .with_uint64("off", 0)
                .unwrap()
                .with_uint64("take", 2)
                .unwrap();
            assert_ne!(
                template
                    .bind_parameters(&changed)
                    .unwrap()
                    .canonical_bytes(),
                frozen
            );
            assert_eq!(actual.canonical_bytes(), frozen);
            assert_eq!(calls.get(), 3);
        }
    }
    assert_eq!(
        prepare("MATCH (walk) RETURN walk"),
        prepare("MATCH WALK (walk) RETURN walk")
    );
}

#[test]
fn unsafe_ambiguous_or_unbounded_quantifiers_refuse_before_any_catalog_call() {
    for text in [
        "MATCH (a)-[:R*]->(b) RETURN a",
        "MATCH TRAIL (a)-[:R*]->(b) RETURN a",
        "MATCH WALK (a)-[:R*]->(b) RETURN a",
        "MATCH WALK (a)-[:R*..]->(b) RETURN a",
        "MATCH WALK (a)-[:R*1..]->(b) RETURN a",
        "MATCH WALK (a)-[:R*2..1]->(b) RETURN a",
        "MATCH WALK (a)-[:R*..0]->(b) RETURN a",
        "MATCH WALK (a)-[:R*-1..2]->(b) RETURN a",
        "MATCH WALK (a)-[:R*1...2]->(b) RETURN a",
        "MATCH WALK (a)-[:R*1,2]->(b) RETURN a",
        "MATCH WALK (a)-[:R*1.5]->(b) RETURN a",
        "MATCH WALK (a)-[:R*'2']->(b) RETURN a",
        "MATCH WALK (a)-[:R*$hops]->(b) RETURN a",
        "MATCH WALK (a)-[:R*0..$hops]->(b) RETURN a",
        "MATCH WALK (a)-[:R*1025]->(b) RETURN a LIMIT 0",
        "MATCH WALK (a)-[:R*4294967296]->(b) RETURN a",
        "MATCH WALK (a) OPTIONAL MATCH (a)-[:R*0..]->(b) RETURN a",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphText::prepare(text, |kind: GraphSymbolKind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    let unknown = PreparedGraphText::prepare("MATCH WALK (a)-[:Unknown*0]->(b) RETURN a", symbols)
        .unwrap_err();
    assert_eq!(
        unknown.kind,
        GraphPatternTextErrorKind::UnknownSymbol(GraphSymbolKind::Relation)
    );
    assert!(!format!("{unknown:?}").contains("Unknown*0"));
    let wrong = PreparedGraphText::prepare("MATCH WALK (a)-[:R*0]->(b) RETURN a", |_: GraphSymbolKind, _: &str| {
        Some(GraphSymbol::Property(PropertyKeyId(1)))
    })
    .unwrap_err();
    assert!(matches!(
        wrong.kind,
        GraphPatternTextErrorKind::WrongSymbolKind { .. }
    ));
}

#[test]
fn plain_match_bounded_walks_preserve_occurrences_and_zero_hops() {
    // One self-loop contributes once at each depth; two parallel loops
    // contribute 2^depth. Plain MATCH must not silently switch to TRAIL.
    for selector in ["MATCH", "MATCH WALK"] {
        for (bounds, expected) in [("0", 1), ("2", 4), ("0..2", 7), ("..2", 6)] {
            let text = format!("{selector} (a)-[:R*{bounds}]->(b) RETURN a,b");
            let rows = prepare(&text)
                .plan()
                .execute_governed_with_properties(
                    1,
                    [VId(1)],
                    [(VId(1), RelationId(1), VId(1)); 2],
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    policy(),
                    || Ok::<_, ()>(()),
                )
                .unwrap();
            let actual: Vec<_> = rows
                .value
                .iter()
                .map(|row| {
                    (
                        row.values()[0].as_vertex().unwrap(),
                        row.values()[1].as_vertex().unwrap(),
                    )
                })
                .collect();
            assert_eq!(actual, vec![(VId(1), VId(1)); expected], "{text}");
        }
    }
    let rows = prepare("MATCH (a) OPTIONAL MATCH (a)-[:R*0..2]->(b) RETURN a,b")
        .plan()
        .execute_governed_with_properties(
            1,
            [VId(7)],
            [],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let actual: Vec<_> = rows
        .value
        .iter()
        .map(|row| {
            (
                row.values()[0].as_vertex().unwrap(),
                row.values()[1].as_vertex().unwrap(),
            )
        })
        .collect();
    assert_eq!(actual, vec![(VId(7), VId(7))]);
}

#[test]
fn text_walks_preserve_zero_hop_isolates_and_repeated_edge_occurrences() {
    let zero = prepare("MATCH WALK (a)-[:R*0]->(b) RETURN a,b");
    let actual = zero
        .plan()
        .execute_governed_with_properties(
            2,
            [VId(0), VId(u128::MAX)],
            [],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let tuples: Vec<_> = actual
        .value
        .iter()
        .map(|row| {
            (
                row.values()[0].as_vertex().unwrap(),
                row.values()[1].as_vertex().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        tuples,
        vec![(VId(0), VId(0)), (VId(u128::MAX), VId(u128::MAX))]
    );
    let edges = [(VId(1), RelationId(1), VId(1)); 2];
    for (tail, count) in [("a,b", 14), ("DISTINCT a,b", 1), ("a,b SKIP 2 LIMIT 3", 3)] {
        let query = prepare(&format!("MATCH WALK (a)-[:R*1..3]->(b) RETURN {tail}"));
        let rows = query
            .plan()
            .execute_governed_with_properties(
                3,
                [VId(1)],
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                policy(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(rows.value.len(), count, "{tail}");
    }
    let maximal = prepare(&format!(
        "MATCH WALK (a)-[:R*{MAX_GRAPH_WALK_HOPS}]->(a) RETURN a"
    ));
    let rows = maximal
        .plan()
        .execute_governed_with_properties(
            2,
            [VId(1)],
            [edges[0]],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(rows.value.len(), 1);
}

#[test]
fn scoped_walk_text_feeds_optional_aggregation_and_semijoin_without_multiplying_witnesses() {
    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let edges = [
        (VId(1), RelationId(1), VId(2)),
        (VId(1), RelationId(1), VId(2)),
        (VId(2), RelationId(1), VId(3)),
    ];
    let template = PreparedGraphAggregateText::prepare(
        "MATCH (a:L) OPTIONAL MATCH WALK (a)-[:R*1..2]->(b) \
         RETURN a,COUNT(*) AS walks,COUNT(b) AS matched GROUP BY a \
         HAVING matched IN [0,2,4] ORDER BY a",
        symbols,
    )
    .unwrap();
    let query = template.bind_parameters(&GqlParameters::new()).unwrap();
    let rows = query
        .execute_governed(
            7,
            vertices,
            edges,
            predicate,
            |_, _| Ok(None),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value;
    let values: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                row.keys()[0].as_vertex().unwrap(),
                row.values()[0].as_count().unwrap(),
                row.values()[1].as_count().unwrap(),
            )
        })
        .collect();
    assert_eq!(values, vec![(VId(1), 4, 4), (VId(4), 1, 0)]);
    for (quantifier, expected) in [("EXISTS", VId(1)), ("NOT EXISTS", VId(4))] {
        let query = prepare(&format!(
            "MATCH (a:L) WHERE {quantifier} {{ \
            MATCH WALK (a)-[:R*2]->(b) }} RETURN a"
        ));
        let rows = query
            .plan()
            .execute_governed_with_properties(
                7,
                vertices,
                edges,
                predicate,
                |_, _| Ok(None),
                policy(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert_eq!(
            rows.len(),
            1,
            "a witness resolves existence, not another output occurrence"
        );
        assert_eq!(rows[0].values()[0].as_vertex(), Some(expected));
    }
}

#[test]
fn parameterized_endpoint_values_preserve_occurrences_across_multiple_walk_lengths() {
    let template = PreparedGraphText::prepare(
        "MATCH WALK (a)-[:R*0..2]->(b) WHERE b.n IN [$lo,$hi] RETURN b.n AS score",
        symbols,
    )
    .unwrap();
    let args = GqlParameters::new()
        .with_int64("lo", 2)
        .unwrap()
        .with_int64("hi", 3)
        .unwrap();
    let query = template.bind_parameters(&args).unwrap();
    let values = [
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(2),
        CanonicalScalar::Int(3),
    ];
    let result = query
        .plan()
        .execute_governed_with_properties(
            5,
            [VId(1), VId(2), VId(3)],
            [
                (VId(1), RelationId(1), VId(2)),
                (VId(2), RelationId(1), VId(3)),
            ],
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize - 1])),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let actual: Vec<_> = result
        .value
        .iter()
        .map(|row| row.values()[0].as_scalar().unwrap().clone())
        .collect();
    assert_eq!(
        actual,
        vec![
            CanonicalScalar::Int(2),
            CanonicalScalar::Int(2),
            CanonicalScalar::Int(3),
            CanonicalScalar::Int(3),
            CanonicalScalar::Int(3)
        ]
    );
}
