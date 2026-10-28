//! Named relationship MERGE paths use the resolved occurrence's native values.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphPath, GraphPathFunction, GraphValue};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryPolicy, GraphEdgeMergeBinding,
    GraphSetColumnType, GraphSymbol, GraphSymbolKind, PreparedGraphEdgeMergeQueryText,
    PreparedGraphWriteChainText,
};
use fgdb_types::{CanonicalScalar, EId, VId};
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
fn named_merge_path_functions_bind_without_catalog_reentry_and_encode_traversal() {
    let calls = Cell::new(0);
    let template = PreparedGraphEdgeMergeQueryText::prepare(
        "MATCH (a),(b) WHERE a.p=$left AND b.p=$right \
         MERGE walk=(a)-[r:R {p:$weight}]->(b) \
         RETURN walk AS path,length(walk) AS hops,nodes(walk) AS nodes,relationships(walk) AS edges",
        R,
        |kind: GraphSymbolKind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let resolved = calls.get();
    let args = GqlParameters::new()
        .with_int64("left", 1)
        .unwrap()
        .with_int64("right", 2)
        .unwrap()
        .with_int64("weight", 7)
        .unwrap();
    let before = args.canonical_bytes();
    let forward = template.bind_parameters(&args).unwrap();
    assert_eq!(calls.get(), resolved);
    assert_eq!(args.canonical_bytes(), before);
    assert_eq!(forward.columns(), ["path", "hops", "nodes", "edges"]);
    assert_eq!(
        forward.column_types(),
        [
            GraphSetColumnType::Path,
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Vertices,
            GraphSetColumnType::Edges
        ]
    );
    assert_eq!(
        forward.bindings(),
        [
            GraphPathFunction::Value,
            GraphPathFunction::Length,
            GraphPathFunction::Nodes,
            GraphPathFunction::Edges
        ]
        .map(|function| GraphEdgeMergeBinding::Path {
            reverse: false,
            function
        })
    );
    let prepare = |pattern: &str| {
        PreparedGraphEdgeMergeQueryText::prepare(
            &format!(
                "MATCH (a),(b) WHERE a.p=$left AND b.p=$right MERGE {pattern} \
                 RETURN trip AS path,length(trip) AS hops,nodes(trip) AS nodes,relationships(trip) AS edges"
            ),
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&args)
        .unwrap()
    };
    let renamed = prepare("trip=(a)-[edge:R {p:$weight}]->(b)");
    let reverse = prepare("trip=(b)<-[edge:R {p:$weight}]-(a)");
    assert_eq!(forward.canonical_bytes(), renamed.canonical_bytes());
    assert_eq!(
        forward.merge().canonical_bytes(),
        reverse.merge().canonical_bytes()
    );
    assert_ne!(forward.canonical_bytes(), reverse.canonical_bytes());
}

#[test]
fn named_paths_are_routed_through_direct_return_with_and_write_continuations() {
    for text in [
        "MATCH (a),(b) MERGE walk=(a)-[:R]->(b) RETURN walk",
        "MATCH (a),(b) WITH a,b MERGE walk=(a)-[:R]->(b) RETURN walk",
        "MATCH (a),(b) MERGE walk=(a)-[:R]->(b) WITH walk AS p \
         UNWIND nodes(p) AS n RETURN n,length(p) AS hops",
    ] {
        assert!(
            PreparedGraphEdgeMergeQueryText::has_return_clause(text).unwrap(),
            "{text}"
        );
        PreparedGraphEdgeMergeQueryText::prepare(text, R, symbols)
            .expect(text)
            .bind_parameters(&GqlParameters::new())
            .expect(text);
    }
    for text in [
        "CREATE (a),(b) MERGE walk=(a)-[:R]->(b) RETURN walk,length(walk) AS hops",
        "MATCH (a),(b) MERGE walk=(a)-[r:R]->(b) ON CREATE SET r.p=7 RETURN walk",
        "MATCH (a),(b) MERGE walk=(a)-[:R]->(b) CREATE (marker {p:length(walk)}) RETURN walk",
        "MATCH (a),(b) MERGE walk=(a)-[:R]->(b) WITH walk UNWIND nodes(walk) AS n \
         SET n.p=3 RETURN walk,length(walk) AS hops",
    ] {
        assert!(
            PreparedGraphWriteChainText::is_chain(text).unwrap(),
            "{text}"
        );
        PreparedGraphWriteChainText::prepare(text, R, symbols)
            .expect(text)
            .bind_parameters(&GqlParameters::new())
            .expect(text);
    }
}

#[test]
fn path_namespace_and_function_errors_refuse_before_catalog_resolution() {
    for text in [
        "MATCH (a),(b) MERGE a=(a)-[:R]->(b) RETURN a",
        "MATCH (a),(b) MERGE walk=(a)-[walk:R]->(b) RETURN walk",
        "MATCH (a),(b) WITH a,b,1 AS walk MERGE walk=(a)-[:R]->(b) RETURN walk",
        "MATCH (a),(b) MERGE walk=(a)-[:R]->(b) RETURN type(walk)",
        "MATCH (a),(b) MERGE walk=(a)-[:R]->(b) WITH a RETURN walk",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphEdgeMergeQueryText::prepare(
                text,
                R,
                |kind: GraphSymbolKind, name: &str| {
                    calls.set(calls.get() + 1);
                    symbols(kind, name)
                }
            )
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "invalid scope reached catalog: {text}");
    }
}

#[test]
fn native_merge_return_checks_complete_one_edge_path_shapes() {
    let query = PreparedGraphEdgeMergeQueryText::prepare(
        "MATCH (a),(b) MERGE walk=(a)-[:R]->(b) \
         RETURN walk,length(walk) AS hops,nodes(walk) AS nodes,relationships(walk) AS edges",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let valid = vec![
        GraphValue::Path(GraphPath::new(VId(1), Box::new([(EId(7), VId(2))]))),
        GraphValue::Scalar(CanonicalScalar::Int(1)),
        GraphValue::Vertices(Box::new([VId(1), VId(2)])),
        GraphValue::Edges(Box::new([EId(7)])),
    ];
    let finish = |row| {
        query.finish_governed::<(), ()>(
            vec![row],
            GqlExecutionStats::default(),
            GlaExecutionStats::default(),
            GqlQueryPolicy::new(100, 100, 100_000, 100_000),
            || Ok(()),
        )
    };
    assert_eq!(finish(valid.clone()).unwrap().value[0].values(), valid);
    for (at, invalid) in [
        (0, GraphValue::Path(GraphPath::new(VId(1), Box::new([])))),
        (1, GraphValue::Scalar(CanonicalScalar::Int(2))),
        (2, GraphValue::Vertices(Box::new([VId(1)]))),
        (3, GraphValue::Edges(Box::new([]))),
    ] {
        let mut row = valid.clone();
        row[at] = invalid;
        assert!(finish(row).is_err(), "invalid native path binding {at}");
    }
}
