//! Acceptance tests for captured graph paths.
use fgdb_gql::{GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::RelationId;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        _ => None,
    }
}

#[test]
fn fixed_and_shortest_path_bindings_prepare() {
    for text in [
        "MATCH p = (a)-[:R]->(b)-[:S]->(c) RETURN p",
        "MATCH p = ANY SHORTEST WALK (a)-[:R*0..4]->(b) RETURN p",
        "MATCH p = ALL SHORTEST WALK (a)-[:R*0..4]->(b) RETURN p",
    ] {
        PreparedGraphText::prepare(text, symbols).expect("path binding must prepare");
    }
}
