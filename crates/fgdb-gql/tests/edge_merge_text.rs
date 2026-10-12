//! Native directed relationship MERGE frontend laws.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GraphEdgeMergeTextErrorKind, GraphPatternTextErrorKind, GraphSymbol,
    GraphSymbolKind, PreparedGraphEdgeMergeText,
};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

#[test]
fn forward_reverse_and_renamed_forms_compile_to_the_same_directed_definition() {
    let forward_text = "MATCH (a),(b) WHERE a.p=$left AND b.p=$right MERGE (a)-[:R]->(b)";
    let calls = Cell::new(0);
    let forward = PreparedGraphEdgeMergeText::prepare(forward_text, R, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    let resolved = calls.get();
    assert_eq!(forward.parameter_schema().len(), 2);
    let args = GqlParameters::new()
        .with_int64("left", 1)
        .unwrap()
        .with_int64("right", 2)
        .unwrap();
    let forward = forward.bind_parameters(&args).unwrap();
    assert_eq!(
        calls.get(),
        resolved,
        "binding must not re-enter the catalog"
    );
    assert_eq!(
        (forward.source_column(), forward.destination_column()),
        (0, 1)
    );

    let reverse = PreparedGraphEdgeMergeText::prepare(
        "MATCH (a),(b) WHERE a.p=$left AND b.p=$right MERGE (b)<-[:R]-(a)",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&args)
    .unwrap();
    assert_eq!(forward.canonical_bytes(), reverse.canonical_bytes());

    let renamed = PreparedGraphEdgeMergeText::prepare(
        "MATCH (x),(y) WHERE x.p=$left AND y.p=$right MERGE (x)-[:R]->(y)",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&args)
    .unwrap();
    assert_eq!(forward.canonical_bytes(), renamed.canonical_bytes());
}

/// fgdb-at06q: a relationship variable, a property map and an undirected
/// pattern all compile. The variable names nothing the plain MERGE uses, so
/// its definition is the anonymous one. The map's values bind to exact
/// properties. An undirected MERGE is its own definition, distinct from the
/// directed one with the same endpoints.
#[test]
fn relationship_variables_property_maps_and_undirected_merges_compile() {
    use fgdb_types::CanonicalScalar;
    let bind = |text: &str, args: &GqlParameters| {
        PreparedGraphEdgeMergeText::prepare(text, R, symbols)
            .unwrap()
            .bind_parameters(args)
            .unwrap()
    };
    let none = GqlParameters::new();
    let anonymous = bind("MATCH (a),(b) MERGE (a)-[:R]->(b)", &none);
    let named = bind("MATCH (a),(b) MERGE (a)-[e:R]->(b)", &none);
    assert_eq!(named.canonical_bytes(), anonymous.canonical_bytes());
    assert!(!anonymous.undirected());
    assert!(anonymous.properties().is_empty());

    let literal = bind("MATCH (a),(b) MERGE (a)-[e:R {p: 1}]->(b)", &none);
    let parameter = bind(
        "MATCH (a),(b) MERGE (a)-[e:R {p: $p}]->(b)",
        &GqlParameters::new().with_int64("p", 1).unwrap(),
    );
    assert_eq!(literal.properties().len(), 1);
    assert_eq!(literal.properties()[0].0, P);
    assert_eq!(literal.properties()[0].1.value(), &CanonicalScalar::Int(1));
    assert_eq!(literal.canonical_bytes(), parameter.canonical_bytes());
    assert_ne!(literal.canonical_bytes(), anonymous.canonical_bytes());

    let undirected = bind("MATCH (a),(b) MERGE (a)-[:R]-(b)", &none);
    assert!(undirected.undirected());
    assert_eq!(
        (undirected.source_column(), undirected.destination_column()),
        (0, 1)
    );
    assert_ne!(undirected.canonical_bytes(), anonymous.canonical_bytes());

    // A NULL map value can neither match nor be created.
    let null = fgdb_gql::GqlScalarParameter::new(CanonicalScalar::Null).unwrap();
    assert_eq!(
        fgdb_gql::PreparedGraphEdgeMerge::prepare(
            anonymous.selection().clone(),
            R,
            0,
            1,
            vec![(P, null)],
        )
        .unwrap_err(),
        fgdb_gql::GraphEdgeMergeBuildError::NullProperty { key: P }
    );
}

#[test]
fn malformed_relationship_forms_refuse_before_catalog() {
    for text in [
        "MATCH (a),(b) MERGE (a)<-[:R]->(b)",
        "MATCH (a),(b) MERGE (a)-[a:R]->(b)",
        "MATCH (a),(b) MERGE (a)-[:R {}]->(b)",
        "MATCH (a),(b) MERGE (a)-[:R {p: null}]->(b)",
        "MATCH (a),(b) MERGE (a)-[:R {p: 1, p: 2}]->(b)",
        "MATCH (a),(b) MERGE (a)-[:R {p: a.p}]->(b)",
        "MATCH (a),(b) MERGE (a:Label)-[:R]->(b)",
        "MATCH (a) MERGE (a)-[:R]->(missing)",
        "MATCH (a),(b) MERGE (a)-[:R]->(b) ON CREATE SET a.p=1",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphEdgeMergeText::prepare(text, R, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(
            calls.get(),
            0,
            "malformed relationship MERGE reached catalog: {text}"
        );
    }
}

#[test]
fn resolved_relation_overrides_default_and_argument_errors_remain_typed() {
    let text = "MATCH (a),(b) WHERE a.p=$x AND b.p=$y MERGE (a)-[:R]->(b)";
    let template = PreparedGraphEdgeMergeText::prepare(text, RelationId(9), symbols).unwrap();
    let args = GqlParameters::new()
        .with_int64("x", 1)
        .unwrap()
        .with_int64("y", 2)
        .unwrap();
    assert_eq!(template.bind_parameters(&args).unwrap().relation(), R);
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert!(matches!(
        missing.kind,
        GraphEdgeMergeTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)
    ));
    let wrong = GqlParameters::new()
        .with_uint64("x", 1)
        .unwrap()
        .with_int64("y", 2)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphEdgeMergeTextErrorKind::Query(GraphPatternTextErrorKind::ParameterTypeMismatch { .. })
    ));
}

#[test]
fn self_loop_uses_one_projected_vertex_column_for_both_endpoints() {
    let merge = PreparedGraphEdgeMergeText::prepare(
        "MATCH (a) WHERE a.p=1 MERGE (a)-[:R]->(a)",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    assert_eq!((merge.source_column(), merge.destination_column()), (0, 0));
}
