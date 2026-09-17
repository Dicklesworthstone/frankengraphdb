//! Native ALL SHORTEST WALK uses the ordinary MATCH compiler and bind map.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GlaOperator, GraphColumn, GraphPatternBuilder, GraphValueRow, GraphWalkSearch,
    IntegerComparison, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    GraphWalkBounds, GraphWriteStatement, PreparedGraphAggregateText, PreparedGraphText,
    PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;
use std::collections::BTreeSet;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn modes(pattern: &PreparedGraphPattern<GraphValueRow>) -> Vec<GraphWalkSearch> {
    pattern
        .plan()
        .operators()
        .iter()
        .filter_map(|op| match op {
            GlaOperator::VarLengthExpand { search, .. } => Some(*search),
            _ => None,
        })
        .collect()
}

#[test]
fn native_directions_bind_to_identical_typed_shortest_atoms_without_catalog_reentry() {
    for (pattern, direction) in [
        ("(a)-[:R*0..3]->(b)", GlaDirection::Forward),
        ("(a)<-[:R*0..3]-(b)", GlaDirection::Reverse),
        ("(a)-[:R*0..3]-(b)", GlaDirection::Undirected),
    ] {
        let text = format!(
            "MATCH ALL SHORTEST WALK {pattern} WHERE a.p=$key RETURN ALL a,b SKIP $off LIMIT $count"
        );
        let mut calls = BTreeSet::new();
        let template = PreparedGraphText::prepare(&text, |kind, name| {
            assert!(calls.insert((kind, name.to_owned())));
            symbols(kind, name)
        })
        .unwrap();
        let arguments = GqlParameters::new()
            .with_int64("key", 7)
            .unwrap()
            .with_uint64("off", 1)
            .unwrap()
            .with_uint64("count", 3)
            .unwrap();
        let bound = template.bind_parameters(&arguments).unwrap();
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("a").unwrap().vertex("b").unwrap();
        builder
            .shortest_walk("a", R, direction, "b", GraphWalkBounds::new(0, 3).unwrap())
            .unwrap();
        builder
            .filter(
                "a",
                VertexPredicate::IntegerProperty {
                    key: P,
                    comparison: IntegerComparison::Equal,
                    value: 7,
                },
            )
            .unwrap();
        let expected = builder
            .prepare_values(
                &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")],
                1,
                Some(3),
            )
            .unwrap()
            .with_duplicates();
        assert_eq!(bound, expected);
        assert_eq!(bound, template.bind_parameters(&arguments).unwrap());
        assert_eq!(calls.len(), 2);
        assert_eq!(template.statement(), text);
        assert_eq!(modes(&bound), vec![GraphWalkSearch::AllShortest]);
    }
}

#[test]
fn shortest_selector_and_return_quantifier_are_independent() {
    let edges = [(VId(1), R, VId(1)); 2];
    for (head, tail, expected) in [
        ("ALL SHORTEST WALK", "a,b", 2),
        ("ALL SHORTEST WALK", "DISTINCT a,b", 1),
        ("ALL SHORTEST WALK", "ALL a,b SKIP 1 LIMIT 1", 1),
        ("ALL SHORTEST WALK", "ALL a,b LIMIT 0", 0),
        ("WALK", "ALL a,b", 14),
    ] {
        let query = prepare(&format!("MATCH {head} (a)-[:R*1..3]->(b) RETURN {tail}"));
        let rows = query
            .plan()
            .execute_governed_with_properties(
                3,
                [VId(1)],
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(rows.value.len(), expected, "{head}, {tail}");
    }
}

#[test]
fn zero_hop_isolates_and_positive_lower_bounds_keep_native_semantics() {
    let zero = prepare("MATCH ALL SHORTEST WALK (a)-[:R*0]->(b) RETURN a,b");
    let rows = zero
        .plan()
        .execute_governed_with_properties(
            2,
            [VId(7), VId(9)],
            [],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let pairs = rows
        .value
        .iter()
        .map(|row| {
            (
                row.values()[0].as_vertex().unwrap(),
                row.values()[1].as_vertex().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(pairs, vec![(VId(7), VId(7)), (VId(9), VId(9))]);
    let positive = prepare("MATCH ALL SHORTEST WALK (a)-[:R*2..3]->(a) RETURN a");
    let rows = positive
        .plan()
        .execute_governed_with_properties(
            2,
            [VId(1)],
            [(VId(1), R, VId(1))],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        rows.value.len(),
        1,
        "a lower bound must not post-filter away the admissible closed walk"
    );
}

#[test]
fn each_match_scope_retains_its_own_explicit_search_selector() {
    let mixed = prepare(
        "MATCH WALK (s)-[:R*1..3]->(a) OPTIONAL MATCH ALL SHORTEST WALK (a)-[:S*0..2]->(b) RETURN s,a,b",
    );
    assert_eq!(
        modes(&mixed),
        vec![GraphWalkSearch::All, GraphWalkSearch::AllShortest]
    );
    let reverse = prepare(
        "MATCH ALL SHORTEST WALK (s)-[:R*1..3]->(a) OPTIONAL MATCH WALK (a)-[:S*0..2]->(b) RETURN s,a,b",
    );
    assert_eq!(
        modes(&reverse),
        vec![GraphWalkSearch::AllShortest, GraphWalkSearch::All]
    );
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [(VId(1), R, VId(2)), (VId(1), R, VId(2))];
    for (quantifier, expected) in [
        ("EXISTS", vec![VId(1)]),
        ("NOT EXISTS", vec![VId(2), VId(3)]),
    ] {
        let query = prepare(&format!(
            "MATCH (a) WHERE {quantifier} {{ MATCH ALL SHORTEST WALK (a)-[:R*1..3]->(b) }} RETURN a"
        ));
        let rows = query
            .plan()
            .execute_governed_with_properties(
                5,
                vertices,
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(
            rows.value
                .iter()
                .map(|row| row.values()[0].as_vertex().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn unsupported_shortest_shapes_refuse_even_with_limit_zero_and_before_catalog() {
    for text in [
        "MATCH SHORTEST WALK (a)-[:R*1..3]->(b) RETURN a",
        "MATCH ANY CHEAPEST WALK (a)-[:R*1..3]->(b) RETURN a",
        "MATCH ALL SHORTEST (a)-[:R*1..3]->(b) RETURN a",
        "MATCH ALL SHORTEST WALK (a) RETURN a",
        "MATCH ALL SHORTEST WALK (a)-[:R]->(b) RETURN a",
        "MATCH ALL SHORTEST WALK (a)-[:R*]->(b) RETURN a",
        "MATCH ALL SHORTEST WALK (a)-[:R*1..]->(b) RETURN a",
        "MATCH ALL SHORTEST WALK (a)-[:R*$hops]->(b) RETURN a",
        "MATCH ALL SHORTEST WALK (a)-[:R*3..1]->(b) RETURN a",
        "MATCH ALL SHORTEST WALK (a)-[:R*1..1025]->(b) RETURN a LIMIT 0",
        "MATCH ALL SHORTEST WALK (a)-[:R*1..3]->(b)-[:R]->(c) RETURN a LIMIT 0",
        "MATCH ALL SHORTEST WALK (a)-[:R*1..3]->(b),(c) RETURN a LIMIT 0",
        "MATCH (a) OPTIONAL MATCH ALL SHORTEST WALK (a)-[:R*1..3]->(b),(c) RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH ALL SHORTEST WALK (a)-[:R*1..3]->(b)-[:S*1..2]->(c) } RETURN a",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphText::prepare(text, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
}

#[test]
fn shortest_arguments_and_utf8_diagnostics_use_the_original_statement() {
    let text =
        "\u{2003}MATCH ALL SHORTEST WALK (a)-[:R*1..3]->(b) WHERE a.p=$key RETURN b LIMIT $count";
    let template = PreparedGraphText::prepare(text, symbols).unwrap();
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, text.find('$').unwrap());
    assert_eq!(missing.kind, GraphPatternTextErrorKind::MissingParameter);
    let wrong = GqlParameters::new()
        .with_uint64("key", 1)
        .unwrap()
        .with_uint64("count", 2)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
    ));
    for at in (0..=text.len()).filter(|at| text.is_char_boundary(*at)) {
        let _ = PreparedGraphText::prepare(&text[..at], symbols);
    }
    assert!(!format!("{template:?}").contains("SHORTEST"));
}

#[test]
fn native_shortest_selection_flows_through_write_scripts_without_another_matcher() {
    let script = PreparedGraphWriteScript::prepare(
        "CREATE (n {p:$key}); MATCH ALL SHORTEST WALK (a)-[:R*1..3]->(b) WHERE a.p=$key SET b.q=$value; MATCH ALL SHORTEST WALK (a)-[:R*1..3]->(b) WHERE a.p=$key DELETE b",
        R, symbols,
    ).unwrap();
    let arguments = GqlParameters::new()
        .with_int64("key", 1)
        .unwrap()
        .with_int64("value", 9)
        .unwrap();
    let program = script.bind_parameters(&arguments).unwrap();
    let [
        GraphWriteStatement::Insert(_),
        GraphWriteStatement::Mutation(update),
        GraphWriteStatement::Delete(delete),
    ] = program.statements()
    else {
        panic!("wrong native write dispatch")
    };
    assert_eq!(
        modes(update.selection()),
        vec![GraphWalkSearch::AllShortest]
    );
    assert_eq!(
        modes(delete.selection()),
        vec![GraphWalkSearch::AllShortest]
    );
    let batch = script
        .bind_parameter_sets(&[arguments.clone(), arguments])
        .unwrap();
    assert_eq!(batch.program().statements().len(), 6);
}

#[test]
fn aggregation_counts_shortest_occurrences_instead_of_all_walks_or_distinct_endpoints() {
    let query = PreparedGraphAggregateText::prepare(
        "MATCH (a) OPTIONAL MATCH ALL SHORTEST WALK (a)-[:R*1..3]->(b) RETURN a,COUNT(*) AS routes,COUNT(b) AS matched GROUP BY a ORDER BY a",
        symbols,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let rows = query
        .execute_governed(
            4,
            [VId(1), VId(2)],
            [(VId(1), R, VId(1)); 2],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok::<Option<&CanonicalScalar>, ()>(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value;
    let counts = rows
        .iter()
        .map(|row| {
            (
                row.keys()[0].as_vertex().unwrap(),
                row.values()[0].as_count().unwrap(),
                row.values()[1].as_count().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(counts, vec![(VId(1), 2, 2), (VId(2), 1, 0)]);
}
