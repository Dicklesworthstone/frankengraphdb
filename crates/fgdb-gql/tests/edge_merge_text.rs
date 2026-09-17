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

#[test]
fn malformed_or_semantically_unimplemented_relationship_forms_refuse_before_catalog() {
    for text in [
        "MATCH (a),(b) MERGE (a)-[:R]-(b)",
        "MATCH (a),(b) MERGE (a)<-[:R]->(b)",
        "MATCH (a),(b) MERGE (a)-[e:R]->(b)",
        "MATCH (a),(b) MERGE (a)-[:R {p:1}]->(b)",
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
fn relation_coordinate_mismatch_and_argument_errors_remain_typed() {
    let text = "MATCH (a),(b) WHERE a.p=$x AND b.p=$y MERGE (a)-[:R]->(b)";
    let mismatch = PreparedGraphEdgeMergeText::prepare(text, RelationId(9), symbols).unwrap_err();
    assert_eq!(mismatch.kind, GraphEdgeMergeTextErrorKind::RelationMismatch);
    assert_eq!(mismatch.offset, text.rfind("R").unwrap());

    let template = PreparedGraphEdgeMergeText::prepare(text, R, symbols).unwrap();
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
