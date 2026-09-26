//! Acceptance tests for captured graph paths.
use fgdb_delta_types::RelationId;
use fgdb_gql::{GraphSymbol, GraphSymbolKind, PreparedGraphText};

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

fn execute(
    text: &str,
    edges: &[(
        fgdb_types::EId,
        fgdb_types::VId,
        RelationId,
        fgdb_types::VId,
    )],
) -> Vec<fgdb_gql::algebra::GraphValueRow> {
    let pattern = PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&fgdb_gql::GqlParameters::new())
        .unwrap();
    pattern
        .plan()
        .execute_governed_with_identified_properties(
            edges.len() as u64 + 4,
            (1..=4).map(fgdb_types::VId),
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            fgdb_gql::GqlQueryPolicy::new(1000, 1000, 100_000, 100_000),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
}

#[test]
fn fixed_path_functions_preserve_traversal_order_and_filter_rows() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::{CanonicalScalar, EId, VId};
    let edges = [
        (EId(91), VId(1), RelationId(1), VId(2)),
        (EId(92), VId(2), RelationId(2), VId(3)),
    ];
    let rows = execute(
        "MATCH p = (a)-[:R]->(b)-[:S]->(c) WHERE path_length(p)=2 AND nodes(p) IS NOT NULL AND edges(p) IS NOT NULL RETURN p, path_length(p) AS hops, nodes(p) AS ns, edges(p) AS es",
        &edges,
    );
    assert_eq!(rows.len(), 1);
    let values = rows[0].values();
    let path = values[0].as_path().unwrap();
    assert_eq!(path.start(), VId(1));
    assert_eq!(path.steps(), &[(EId(91), VId(2)), (EId(92), VId(3))]);
    assert_eq!(values[1], GraphValue::Scalar(CanonicalScalar::Int(2)));
    assert_eq!(
        values[2],
        GraphValue::Vertices(vec![VId(1), VId(2), VId(3)].into_boxed_slice())
    );
    assert_eq!(
        values[3],
        GraphValue::Edges(vec![EId(91), EId(92)].into_boxed_slice())
    );
    assert!(
        execute(
            "MATCH p = (a)-[:R]->(b)-[:S]->(c) WHERE path_length(p)=1 RETURN p",
            &edges
        )
        .is_empty()
    );
    let reverse = execute("MATCH p = (c)<-[:S]-(b)<-[:R]-(a) RETURN p", &edges);
    assert_eq!(
        reverse[0].values()[0].as_path().unwrap().steps(),
        &[(EId(92), VId(2)), (EId(91), VId(1))]
    );
}

/// fgdb-j687q: openCypher `length(p)` is PATH_LENGTH, in RETURN, WHERE and
/// ORDER BY, with the identical plan.
#[test]
fn opencypher_length_is_path_length() {
    use fgdb_gql::GqlParameters;
    let plan = |text: &str| {
        PreparedGraphText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .canonical_bytes()
    };
    for (cypher, gql) in [
        (
            "MATCH p = (a)-[:R]->(b)-[:S]->(c) WHERE length(p)=2 RETURN p, length(p) AS hops",
            "MATCH p = (a)-[:R]->(b)-[:S]->(c) WHERE path_length(p)=2 RETURN p, path_length(p) AS hops",
        ),
        (
            "MATCH p = ANY SHORTEST WALK (a)-[:R*0..4]->(b) RETURN p ORDER BY LENGTH(p) DESC",
            "MATCH p = ANY SHORTEST WALK (a)-[:R*0..4]->(b) RETURN p ORDER BY path_length(p) DESC",
        ),
    ] {
        assert_eq!(plan(cypher), plan(gql), "{cypher}");
    }
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::{CanonicalScalar, EId, VId};
    let edges = [
        (EId(91), VId(1), RelationId(1), VId(2)),
        (EId(92), VId(2), RelationId(2), VId(3)),
    ];
    let rows = execute(
        "MATCH p = (a)-[:R]->(b)-[:S]->(c) WHERE length(p)=2 RETURN length(p) AS hops",
        &edges,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].values()[0],
        GraphValue::Scalar(CanonicalScalar::Int(2))
    );
}

#[test]
fn shortest_ties_parallel_edges_and_zero_hops_retain_identity() {
    use fgdb_types::{EId, VId};
    let edges = [
        (EId(91), VId(1), RelationId(1), VId(2)),
        (EId(92), VId(2), RelationId(1), VId(4)),
        (EId(93), VId(1), RelationId(1), VId(3)),
        (EId(94), VId(3), RelationId(1), VId(4)),
        (EId(95), VId(1), RelationId(1), VId(2)),
    ];
    let text = "MATCH p = ALL SHORTEST WALK (a)-[:R*0..4]->(b) WHERE path_length(p)=2 RETURN p";
    let rows = execute(text, &edges);
    let routes = |rows: &[fgdb_gql::algebra::GraphValueRow]| {
        rows.iter()
            .map(|row| {
                row.values()[0]
                    .as_path()
                    .unwrap()
                    .edges()
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        routes(&rows),
        vec![
            vec![EId(91), EId(92)],
            vec![EId(93), EId(94)],
            vec![EId(95), EId(92)]
        ]
    );
    let mut shuffled = edges;
    shuffled.reverse();
    assert_eq!(execute(text, &shuffled), rows);
    let any = execute(
        "MATCH p = ANY SHORTEST WALK (a)-[:R*0..4]->(b) WHERE path_length(p)=2 RETURN p",
        &edges,
    );
    assert_eq!(routes(&any), vec![vec![EId(91), EId(92)]]);
    let zeros = execute(
        "MATCH p = ALL SHORTEST WALK (a)-[:R*0..4]->(b) WHERE path_length(p)=0 RETURN p",
        &edges,
    );
    assert_eq!(zeros.len(), 4);
    for (row, vertex) in zeros.iter().zip((1..=4).map(VId)) {
        let path = row.values()[0].as_path().unwrap();
        assert_eq!(path.nodes().collect::<Vec<_>>(), vec![vertex]);
        assert!(path.edges().next().is_none());
    }
}

/// fgdb-j687q: openCypher startNode(r)/endNode(r) of a directed single-hop
/// edge is the endpoint vertex the pattern binds, the identical plan to
/// naming it, in either written direction and for an anonymous endpoint. An
/// undirected or quantified edge has no static endpoint and refuses.
#[test]
fn opencypher_start_and_end_node_are_the_bound_endpoints() {
    use fgdb_gql::GqlParameters;
    let plan = |text: &str| {
        PreparedGraphText::prepare(text, symbols)
            .expect(text)
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .canonical_bytes()
    };
    for (cypher, named) in [
        (
            "MATCH (a)-[r:R]->(b) RETURN startNode(r) AS s, endNode(r) AS e",
            "MATCH (a)-[r:R]->(b) RETURN a AS s, b AS e",
        ),
        (
            "MATCH (a)<-[r:R]-(b) RETURN STARTNODE(r) AS s, endnode(r) AS e",
            "MATCH (a)<-[r:R]-(b) RETURN b AS s, a AS e",
        ),
    ] {
        assert_eq!(plan(cypher), plan(named), "{cypher}");
    }
    // The relational (set-text) RETURN resolves them the same way.
    let set_plan = |text: &str| {
        fgdb_gql::PreparedGraphSetText::prepare(text, symbols)
            .expect(text)
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .canonical_bytes()
    };
    assert_eq!(
        set_plan("MATCH (a)<-[r:R]-(b) RETURN DISTINCT startNode(r) AS s, endNode(r) AS e"),
        set_plan("MATCH (a)<-[r:R]-(b) RETURN DISTINCT b AS s, a AS e")
    );
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::{EId, VId};
    let edges = [(EId(91), VId(1), RelationId(1), VId(2))];
    for (text, start, end) in [
        (
            "MATCH (a)-[r:R]->(b) RETURN startNode(r) AS s, endNode(r) AS e",
            1,
            2,
        ),
        (
            "MATCH (b)<-[r:R]-(a) RETURN startNode(r) AS s, endNode(r) AS e",
            1,
            2,
        ),
        (
            "MATCH ()-[r:R]->() RETURN startNode(r) AS s, endNode(r) AS e",
            1,
            2,
        ),
    ] {
        let rows = execute(text, &edges);
        assert_eq!(rows.len(), 1, "{text}");
        assert_eq!(
            rows[0].values(),
            &[GraphValue::Vertex(VId(start)), GraphValue::Vertex(VId(end))],
            "{text}"
        );
    }
    for text in [
        "MATCH (a)-[r:R]-(b) RETURN startNode(r) AS s",
        "MATCH (a)-[r:R*1..2]->(b) RETURN endNode(r) AS e",
        "MATCH (a)-[r:R]->(b) RETURN startNode(a) AS s",
        "MATCH (a)-[r:R]->(b) RETURN startNode(q) AS s",
    ] {
        assert!(PreparedGraphText::prepare(text, symbols).is_err(), "{text}");
    }
}
