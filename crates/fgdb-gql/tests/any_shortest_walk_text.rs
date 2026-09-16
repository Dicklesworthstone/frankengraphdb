//! Native ANY shortest is an explicit search selector, not RETURN DISTINCT.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GlaOperator, GraphColumn, GraphPatternBuilder, GraphValueRow,
    GraphWalkSearch, IntegerComparison, PreparedGraphPattern, VertexPredicate,
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
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000) }
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn modes(pattern: &PreparedGraphPattern<GraphValueRow>) -> Vec<GraphWalkSearch> {
    pattern.plan().operators().iter().filter_map(|op| match op {
        GlaOperator::VarLengthExpand { search, .. } => Some(*search), _ => None,
    }).collect()
}

#[test]
fn native_any_directions_and_rebinding_match_the_typed_builder_exactly() {
    for (atom, direction) in [("(a)-[:R*0..3]->(b)", GlaDirection::Forward),
        ("(a)<-[:R*0..3]-(b)", GlaDirection::Reverse),
        ("(a)-[:R*0..3]-(b)", GlaDirection::Undirected)] {
        let text = format!("match any shortest walk {atom} WHERE a.p=$key RETURN ALL a,b SKIP $off LIMIT $count");
        let mut calls = BTreeSet::new();
        let prepared = PreparedGraphText::prepare(&text, |kind, name| {
            assert!(calls.insert((kind, name.to_owned())));
            symbols(kind, name)
        }).unwrap();
        let args = GqlParameters::new().with_int64("key", 7).unwrap()
            .with_uint64("off", 1).unwrap().with_uint64("count", 3).unwrap();
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("a").unwrap().vertex("b").unwrap();
        builder.any_shortest_walk("a", R, direction, "b", GraphWalkBounds::new(0, 3).unwrap()).unwrap();
        builder.filter("a", VertexPredicate::IntegerProperty {
            key: P, comparison: IntegerComparison::Equal, value: 7,
        }).unwrap();
        let expected = builder.prepare_values(&[GraphColumn::vertex("a", "a"),
            GraphColumn::vertex("b", "b")], 1, Some(3)).unwrap().with_duplicates();
        assert_eq!(prepared.bind_parameters(&args).unwrap(), expected);
        assert_eq!(prepared.bind_parameters(&args).unwrap().canonical_bytes(), expected.canonical_bytes());
        assert_eq!(calls.len(), 2);
        assert_eq!(modes(&expected), vec![GraphWalkSearch::AnyShortest]);
        assert!(!format!("{prepared:?}").contains("shortest"));
    }
}

#[test]
fn native_any_preserves_zero_hops_lower_bounds_and_separate_return_quantifiers() {
    let edges = [(VId(1), R, VId(1)); 2];
    for (selector, interval, tail, expected) in [
        ("ANY SHORTEST WALK", "1..3", "a,b", 1),
        ("ALL SHORTEST WALK", "1..3", "a,b", 2),
        ("WALK", "1..3", "a,b", 14),
        ("ANY SHORTEST WALK", "2..3", "a,b", 1),
        ("ANY SHORTEST WALK", "0", "a,b", 2),
        ("ANY SHORTEST WALK", "1..3", "DISTINCT a,b", 1),
        ("ANY SHORTEST WALK", "1..3", "ALL a,b SKIP 1 LIMIT 1", 0),
    ] {
        let query = prepare(&format!("MATCH {selector} (a)-[:R*{interval}]->(b) RETURN {tail}"));
        let rows = query.plan().execute_governed_with_properties(4, [VId(1), VId(9)], edges,
            |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap();
        assert_eq!(rows.value.len(), expected, "{selector}, {interval}, {tail}");
    }
}

#[test]
fn scoped_any_aggregation_counts_pairs_and_existence_never_multiplies_outer_rows() {
    let query = PreparedGraphAggregateText::prepare(
        "MATCH (a) OPTIONAL MATCH ANY SHORTEST WALK (a)-[:R*1..3]->(b) RETURN a,COUNT(*) AS pairs,COUNT(b) AS matched GROUP BY a ORDER BY a",
        symbols,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let rows = query.execute_governed(4, [VId(1), VId(2)], [(VId(1), R, VId(1)); 2],
        |_, _| Ok::<_, ()>(true), |_, _| Ok::<Option<&CanonicalScalar>, ()>(None),
        wide(), || Ok::<_, ()>(())).unwrap().value;
    let counts = rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(),
        row.values()[0].as_count().unwrap(), row.values()[1].as_count().unwrap())).collect::<Vec<_>>();
    assert_eq!(counts, vec![(VId(1), 1, 1), (VId(2), 1, 0)]);
    for (quantifier, expected) in [("EXISTS", VId(1)), ("NOT EXISTS", VId(2))] {
        let query = prepare(&format!("MATCH (a) WHERE {quantifier} {{ MATCH ANY SHORTEST WALK (a)-[:R*1..3]->(b) }} RETURN a"));
        let rows = query.plan().execute_governed_with_properties(4, [VId(1), VId(2)],
            [(VId(1), R, VId(1)); 2], |_, _| Ok::<_, ()>(true), |_, _| Ok(None),
            wide(), || Ok::<_, ()>(())).unwrap();
        assert_eq!(rows.value.len(), 1);
        assert_eq!(rows.value[0].values()[0].as_vertex(), Some(expected));
    }
    let mixed = prepare("MATCH ALL SHORTEST WALK (s)-[:R*1..3]->(a) OPTIONAL MATCH ANY SHORTEST WALK (a)-[:S*0..2]->(b) RETURN s,a,b");
    assert_eq!(modes(&mixed), vec![GraphWalkSearch::AllShortest, GraphWalkSearch::AnyShortest]);
}

#[test]
fn malformed_any_selectors_and_compound_paths_refuse_before_catalog_access() {
    for text in ["MATCH ANY WALK (a)-[:R*1..3]->(b) RETURN a",
        "MATCH ANY ALL SHORTEST WALK (a)-[:R*1..3]->(b) RETURN a",
        "MATCH ANY SHORTEST WALK (a) RETURN a",
        "MATCH ANY SHORTEST WALK (a)-[:R]->(b) RETURN a",
        "MATCH ANY SHORTEST WALK (a)-[:R*1..]->(b) RETURN a",
        "MATCH ANY SHORTEST WALK (a)-[:R*$hops]->(b) RETURN a",
        "MATCH ANY SHORTEST WALK (a)-[:R*1..1025]->(b) RETURN a LIMIT 0",
        "MATCH ANY SHORTEST WALK (a)-[:R*1..3]->(b)-[:S]->(c) RETURN a LIMIT 0",
        "MATCH ANY SHORTEST WALK (a)-[:R*1..3]->(b),(c) RETURN a LIMIT 0",
        "MATCH (a) OPTIONAL MATCH ANY SHORTEST WALK (a)-[:R*1..3]->(b),(c) RETURN a"] {
        let calls = Cell::new(0);
        assert!(PreparedGraphText::prepare(text, |kind, name| {
            calls.set(calls.get() + 1); symbols(kind, name)
        }).is_err(), "{text}");
        assert_eq!(calls.get(), 0, "{text}");
    }
}

#[test]
fn any_scripts_and_parameter_batches_lower_all_write_selections_without_reparsing() {
    let text = "MATCH ANY SHORTEST WALK (a)-[:R*1..3]->(b) WHERE a.p=$key CREATE (n {q:1});\n\
        MATCH ANY SHORTEST WALK (a)-[:R*1..3]->(b) WHERE a.p=$key SET b.q=2;\n\
        MATCH ANY SHORTEST WALK (a)-[:R*1..3]->(b) WHERE a.p=$key DELETE b";
    let mut calls = BTreeSet::new();
    let script = PreparedGraphWriteScript::prepare(text, R, |kind, name| {
        assert!(calls.insert((kind, name.to_owned())));
        symbols(kind, name)
    }).unwrap();
    let args = GqlParameters::new().with_int64("key", 1).unwrap();
    let batch = script.bind_parameter_sets(&[args.clone(), args]).unwrap();
    assert_eq!(batch.argument_sets(), 2);
    for group in batch.program().statements().chunks_exact(3) {
        let [GraphWriteStatement::Insert(insert), GraphWriteStatement::Mutation(update),
            GraphWriteStatement::Delete(delete)] = group else { panic!("wrong write dispatch") };
        for selection in [insert.selection().unwrap(), update.selection(), delete.selection()] {
            assert_eq!(modes(selection), vec![GraphWalkSearch::AnyShortest]);
        }
    }
    assert_eq!(calls.len(), 3);
    assert_eq!(batch.location(5).unwrap().argument_set, 1);
    assert_eq!(batch.location(5).unwrap().statement, 2);
}

#[test]
fn any_binding_errors_preserve_utf8_offsets_and_reject_unrecognized_arguments() {
    let text = "\u{2003}MATCH ANY SHORTEST WALK (a)-[:R*1..3]->(b) WHERE a.p=$key RETURN b LIMIT $count";
    let prepared = PreparedGraphText::prepare(text, symbols).unwrap();
    let error = prepared.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(error.offset, text.find('$').unwrap());
    assert_eq!(error.kind, GraphPatternTextErrorKind::MissingParameter);
    let wrong = GqlParameters::new().with_uint64("key", 1).unwrap().with_uint64("count", 2).unwrap();
    assert!(matches!(prepared.bind_parameters(&wrong).unwrap_err().kind,
        GraphPatternTextErrorKind::ParameterTypeMismatch { .. }));
    let extra = GqlParameters::new().with_int64("key", 1).unwrap().with_uint64("count", 2).unwrap()
        .with_int64("extra", 3).unwrap();
    assert_eq!(prepared.bind_parameters(&extra).unwrap_err().kind, GraphPatternTextErrorKind::UnexpectedArguments);
    for at in (0..=text.len()).filter(|at| text.is_char_boundary(*at)) {
        let _ = PreparedGraphText::prepare(&text[..at], symbols);
    }
}
