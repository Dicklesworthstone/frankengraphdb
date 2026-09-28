//! Native endpoint functions over the same frozen occurrence used by CREATE.
//! Anonymous endpoints and incoming arrows must not need a post-write scan.

use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertIntent, GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphInsertQueryBatch,
    GraphInsertQueryError, GraphSetColumnType, GraphSymbol, GraphSymbolKind,
    PreparedGraphInsertQuery, PreparedGraphInsertQueryText,
};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
type ResultOf = Result<GraphInsertQueryBatch, GqlQueryError<GraphInsertQueryError<(), ()>, ()>>;
type Edge = (EId, VId, RelationId, VId);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn policy(records: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(records, 1_000, 2_000_000, 1_000_000),
        1_000,
        1_000,
    )
}
fn identity(request: GraphInsertRequest) -> Result<ElementId, ()> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(100 + row as u128 * 16 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(1_000 + row as u128 * 16 + edge as u128))
        }
    })
}
fn prepare(text: &str) -> PreparedGraphInsertQuery {
    PreparedGraphInsertQueryText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn run(query: &PreparedGraphInsertQuery) -> ResultOf {
    query.execute_governed(
        policy(0),
        |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
            panic!("created endpoints must not be recovered by scanning the graph")
        },
        identity,
        || Ok(()),
    )
}
fn values(batch: &GraphInsertQueryBatch) -> Vec<Vec<GraphValue>> {
    batch
        .returning()
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}
fn run_matched(query: &PreparedGraphInsertQuery, vertices: &[VId], edges: &[Edge]) -> ResultOf {
    query.execute_governed(
        policy(1_000),
        |selection, allowance| {
            selection.plan().execute_governed_with_element_properties(
                (vertices.len() + edges.len()) as u64,
                vertices.iter().copied(),
                edges.iter().copied(),
                |_, predicates| Ok(predicates.is_empty()),
                |_, _| Ok(None),
                |_, _| Ok(None),
                allowance,
                || Ok(()),
            )
        },
        identity,
        || Ok(()),
    )
}

#[test]
fn anonymous_created_endpoints_follow_physical_direction_without_graph_access() {
    for (arrow, source, destination) in [
        ("()-[e:R]->()", VId(100), VId(101)),
        ("()<-[e:R]-()", VId(101), VId(100)),
    ] {
        let query = prepare(&format!(
            "CREATE {arrow} RETURN e,startNode(e) AS source,endNode(e) AS destination"
        ));
        assert_eq!(
            query.column_types(),
            &[
                GraphSetColumnType::Edge,
                GraphSetColumnType::Vertex,
                GraphSetColumnType::Vertex,
            ]
        );
        let batch = run(&query).unwrap();
        assert_eq!(
            values(&batch),
            vec![vec![
                GraphValue::Edge(EId(1_000)),
                GraphValue::Vertex(source),
                GraphValue::Vertex(destination),
            ]]
        );
        assert_eq!(
            batch.insertion().intents()[2],
            GraphInsertIntent::Edge {
                edge: EId(1_000),
                relation: R,
                source,
                destination,
                properties: vec![],
            }
        );
    }
}

#[test]
fn endpoint_aliases_do_not_depend_on_which_equivalent_binding_was_seen_first() {
    let query = prepare("CREATE (a)-[e:R]->(a) RETURN startNode(e),endNode(e),a");
    assert_eq!(query.columns(), &["startNode", "endNode", "a"]);
    assert_eq!(
        values(&run(&query).unwrap()),
        vec![vec![GraphValue::Vertex(VId(100)); 3]]
    );
    let query = prepare("CREATE (a)-[e:R]->(a) RETURN a,endNode(e),startNode(e)");
    assert_eq!(query.columns(), &["a", "endNode", "startNode"]);
    assert_eq!(
        values(&run(&query).unwrap()),
        vec![vec![GraphValue::Vertex(VId(100)); 3]]
    );
}

#[test]
fn function_names_remain_legal_bindings_and_calls_are_case_insensitive() {
    let query = prepare(
        "CREATE (startNode)-[endNode:R]->() \
        RETURN startNode,endNode,STARTNODE(endNode) AS source,ENDNODE(endNode) AS target",
    );
    assert_eq!(
        values(&run(&query).unwrap()),
        vec![vec![
            GraphValue::Vertex(VId(100)),
            GraphValue::Edge(EId(1_000)),
            GraphValue::Vertex(VId(100)),
            GraphValue::Vertex(VId(101)),
        ]]
    );
}

#[test]
fn matched_and_created_endpoints_preserve_duplicate_source_occurrences() {
    let query = prepare(
        "MATCH (a)-[old:R]->(b) CREATE (copy)<-[new:R]-(b) \
        RETURN startNode(old) AS original_source,endNode(old) AS original_target, \
        startNode(new) AS source,endNode(new) AS target ORDER BY target",
    );
    let vertices = [VId(1), VId(2)];
    let edges = [(EId(10), VId(1), R, VId(2)), (EId(11), VId(1), R, VId(2))];
    let batch = run_matched(&query, &vertices, &edges).unwrap();
    assert_eq!(
        values(&batch),
        [VId(100), VId(116)]
            .into_iter()
            .map(|target| vec![
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(target),
            ])
            .collect::<Vec<_>>()
    );
    assert_eq!(batch.insertion().stats().created_vertices, 2);
    assert_eq!(batch.insertion().stats().created_edges, 2);
    for (row, target) in [VId(100), VId(116)].into_iter().enumerate() {
        assert!(
            batch
                .insertion()
                .intents()
                .contains(&GraphInsertIntent::Edge {
                    edge: EId(1_000 + row as u128 * 16),
                    relation: R,
                    source: VId(2),
                    destination: target,
                    properties: vec![],
                })
        );
    }
}

#[test]
fn matched_incoming_endpoints_keep_the_shared_match_direction_law() {
    let query = prepare(
        "MATCH (left)<-[old:R]-(right) CREATE (n) \
        RETURN startNode(old) AS source,endNode(old) AS target",
    );
    let batch = run_matched(&query, &[VId(1), VId(2)], &[(EId(10), VId(2), R, VId(1))]).unwrap();
    assert_eq!(
        values(&batch),
        vec![vec![GraphValue::Vertex(VId(2)), GraphValue::Vertex(VId(1))]]
    );
}

#[test]
fn parameterized_unwind_endpoint_pages_do_not_suppress_creation() {
    let calls = Cell::new(0);
    let template = PreparedGraphInsertQueryText::prepare(
        "UNWIND $items AS x CREATE (a {p:x})<-[e:R]-() \
        RETURN startNode(e) AS source,endNode(e) AS target,x ORDER BY target LIMIT $cap",
        R,
        |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    assert_eq!(calls.get(), 2);
    let args = GqlParameters::new()
        .with_list(
            "items",
            vec![
                GraphValue::Scalar(CanonicalScalar::Int(2)),
                GraphValue::Scalar(CanonicalScalar::Int(1)),
            ],
        )
        .unwrap()
        .with_uint64("cap", 1)
        .unwrap();
    let query = template.bind_parameters(&args).unwrap();
    assert_eq!(
        query.canonical_bytes(),
        template.bind_parameters(&args).unwrap().canonical_bytes()
    );
    let batch = run(&query).unwrap();
    assert_eq!(
        values(&batch),
        vec![vec![
            GraphValue::Vertex(VId(101)),
            GraphValue::Vertex(VId(100)),
            GraphValue::Scalar(CanonicalScalar::Int(2)),
        ]]
    );
    assert_eq!(batch.insertion().stats().created_vertices, 4);
    assert_eq!(batch.insertion().stats().created_edges, 2);
    assert_eq!(
        calls.get(),
        2,
        "endpoint binding must not resolve the catalog again"
    );
}

#[test]
fn empty_matched_and_unwind_endpoint_queries_create_and_return_nothing() {
    let query = prepare("MATCH (a)-[e:R]->(b) CREATE (n) RETURN startNode(e) AS source");
    let batch = run_matched(&query, &[], &[]).unwrap();
    assert!(batch.insertion().intents().is_empty());
    assert!(batch.returning().value.is_empty());
    let query = prepare("UNWIND [] AS x CREATE ()-[e:R]->() RETURN endNode(e) AS target");
    let result: ResultOf = query.execute_governed(
        policy(0),
        |_, _| panic!("empty UNWIND has no graph source"),
        |_| panic!("empty UNWIND has no identities"),
        || Ok(()),
    );
    let batch = result.unwrap();
    assert!(batch.insertion().intents().is_empty());
    assert!(batch.returning().value.is_empty());
}

#[test]
fn endpoint_arguments_and_result_domains_are_checked_before_catalog_callbacks() {
    for text in [
        "CREATE (a)-[e:R]->(b) RETURN startNode(a)",
        "CREATE (a)-[e:R]->(b) RETURN endNode(missing)",
        "CREATE (a)-[e:R]->(b) RETURN startNode()",
        "CREATE (a)-[e:R]->(b) RETURN startNode(e,e)",
        "CREATE (a)-[e:R]->(b) RETURN endNode($edge)",
        "CREATE (a)-[e:R]->(b) RETURN startNode(e.p)",
        "CREATE (a)-[e:R]->(b) RETURN startNode(e).p",
        "CREATE (a)-[e:R]->(b) RETURN startNode(e)+1 AS bad",
        "UNWIND [1] AS x CREATE (a)-[e:R]->(b) RETURN endNode(x)",
        "MATCH (a) CREATE (n) RETURN startNode(a)",
    ] {
        let calls = Cell::new(0);
        let result = PreparedGraphInsertQueryText::prepare(text, R, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        });
        assert!(result.is_err(), "unexpectedly accepted {text}");
        assert_eq!(
            calls.get(),
            0,
            "catalog access before invalid endpoint refusal: {text}"
        );
    }
}

#[test]
fn match_dispatch_does_not_treat_binding_names_as_creation_clauses() {
    for text in [
        "MATCH (create) RETURN create",
        "MATCH (insert) WHERE insert IS NOT NULL RETURN insert",
        "MATCH (create) WHERE create.p = 1 RETURN create",
        "MATCH (n) RETURN n.p AS CREATE,n.p AS RETURN",
        "MATCH (n) WHERE n.p = 'CREATE (x) RETURN x' RETURN n",
    ] {
        assert!(
            !PreparedGraphInsertQueryText::has_return_clause(text).unwrap(),
            "{text}"
        );
    }
    for text in [
        "MATCH (a) CREATE (a)-[e:R]->() RETURN endNode(e)",
        "MATCH (a) INSERT ()-[e:R]->(a) RETURN startNode(e)",
    ] {
        assert!(
            PreparedGraphInsertQueryText::has_return_clause(text).unwrap(),
            "{text}"
        );
    }
}
