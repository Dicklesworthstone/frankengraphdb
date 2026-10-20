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

/// openCypher: a named variable-length relationship `[r*m..n]` is the list of
/// relationships it matched (TCK Match4 [1], [6], Match9 [2] to [4]). When it
/// is its pattern's only edge, that list is the edge list of the one path the
/// pattern matches, in traversal order. That holds in a later required or
/// OPTIONAL MATCH too, where an OPTIONAL row without a witness reads NULL.
#[test]
fn a_lone_named_variable_length_relationship_is_its_edge_list() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::{CanonicalScalar, EId, VId};
    let edges = [
        (EId(91), VId(1), RelationId(1), VId(2)),
        (EId(92), VId(2), RelationId(1), VId(3)),
    ];
    let list = |ids: &[u128]| {
        GraphValue::Edges(
            ids.iter()
                .map(|id| EId(*id))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
    };
    for (text, expected) in [
        (
            "MATCH (a)-[r:R*1..2]->(b) RETURN r",
            vec![list(&[91]), list(&[92]), list(&[91, 92])],
        ),
        ("MATCH (a)-[r:R*2]->(b) RETURN r", vec![list(&[91, 92])]),
        (
            "MATCH (a)-[r:R*2..2]-(b) RETURN r",
            vec![list(&[91, 92]), list(&[92, 91])],
        ),
        (
            "MATCH (a)-[:R]->(m) MATCH (m)-[r:R*1..2]->(b) RETURN r",
            vec![list(&[92])],
        ),
        (
            "MATCH (a)-[:R]->(m) OPTIONAL MATCH (m)-[r:R*1..2]->(b) RETURN r",
            vec![list(&[92]), GraphValue::Scalar(CanonicalScalar::Null)],
        ),
    ] {
        let found: Vec<_> = execute(text, &edges)
            .iter()
            .map(|row| row.values()[0].clone())
            .collect();
        assert_eq!(found.len(), expected.len(), "{text}: {found:?}");
        for value in &expected {
            assert!(found.contains(value), "{text}: {found:?}");
        }
    }
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

/// fgdb-tk2u6: a captured path may revisit a vertex, and a variable-length
/// relationship may sit anywhere in a chain (TCK Match6 [8], [12] to [14],
/// [17]). Whichever end the plan expands an edge or walk from, the path keeps
/// its declared order. Expected paths are enumerated by hand over parallel
/// R edges 91 and 92 from 1 to 2, R edge 93 from 2 to 3, and S edge 94 from
/// 3 to 4, with each relationship used at most once per path.
#[test]
fn captured_paths_revisit_vertices_and_chain_variable_length_relationships() {
    use fgdb_types::{EId, VId};
    let edges = [
        (EId(91), VId(1), RelationId(1), VId(2)),
        (EId(92), VId(1), RelationId(1), VId(2)),
        (EId(93), VId(2), RelationId(1), VId(3)),
        (EId(94), VId(3), RelationId(2), VId(4)),
    ];
    let path = |start: u128, steps: &[(u128, u128)]| {
        (
            VId(start),
            steps
                .iter()
                .map(|&(eid, vid)| (EId(eid), VId(vid)))
                .collect::<Vec<_>>(),
        )
    };
    for (text, expected) in [
        (
            "MATCH p = (n)-[:R]->(k)<-[:R]-(n) RETURN p",
            vec![path(1, &[(91, 2), (92, 1)]), path(1, &[(92, 2), (91, 1)])],
        ),
        (
            "MATCH p = (n)-[:R]-(k)-[:R]-(n) RETURN p",
            vec![
                path(1, &[(91, 2), (92, 1)]),
                path(1, &[(92, 2), (91, 1)]),
                path(2, &[(91, 1), (92, 2)]),
                path(2, &[(92, 1), (91, 2)]),
            ],
        ),
        (
            "MATCH p = (a)-[:R]->(b)-[:R*0..1]->(c) RETURN p",
            vec![
                path(1, &[(91, 2)]),
                path(1, &[(91, 2), (93, 3)]),
                path(1, &[(92, 2)]),
                path(1, &[(92, 2), (93, 3)]),
                path(2, &[(93, 3)]),
            ],
        ),
        (
            "MATCH p = (a)-[:R*1..2]->(b)-[:S]->(c) RETURN p",
            vec![
                path(1, &[(91, 2), (93, 3), (94, 4)]),
                path(1, &[(92, 2), (93, 3), (94, 4)]),
                path(2, &[(93, 3), (94, 4)]),
            ],
        ),
        (
            "MATCH p = (a)<-[:S]-(b)-[:R*2..2]-(c) RETURN p",
            vec![
                path(4, &[(94, 3), (93, 2), (91, 1)]),
                path(4, &[(94, 3), (93, 2), (92, 1)]),
            ],
        ),
    ] {
        let mut found: Vec<_> = execute(text, &edges)
            .iter()
            .map(|row| {
                let captured = row.values()[0]
                    .as_path()
                    .unwrap_or_else(|| panic!("{text}: {row:?}"));
                (captured.start(), captured.steps().to_vec())
            })
            .collect();
        found.sort();
        let mut expected = expected;
        expected.sort();
        assert_eq!(found, expected, "{text}");
    }
}

/// A later required or OPTIONAL MATCH may name its path (TCK Match7 [16] to
/// [20], Match9 [9], Comparison1 [14]). The path is exported like the
/// clause's vertices: NULL where an OPTIONAL clause has no witness, and in
/// declared order even when the clause is anchored at its far end. Expected
/// rows are enumerated by hand over R edge 91 from 1 to 2 and S edge 92 from
/// 2 to 3, with vertex 4 isolated.
#[test]
fn a_later_or_optional_match_names_its_path() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::{EId, VId};
    let edges = [
        (EId(91), VId(1), RelationId(1), VId(2)),
        (EId(92), VId(2), RelationId(2), VId(3)),
    ];
    type Path = Option<(VId, Vec<(EId, VId)>)>;
    let path = |start: u128, steps: &[(u128, u128)]| -> Path {
        Some((
            VId(start),
            steps
                .iter()
                .map(|&(eid, vid)| (EId(eid), VId(vid)))
                .collect(),
        ))
    };
    let cell = |value: &GraphValue| -> Path {
        match value {
            GraphValue::Scalar(fgdb_types::CanonicalScalar::Null) => None,
            value => {
                let captured = value.as_path().unwrap_or_else(|| panic!("{value:?}"));
                Some((captured.start(), captured.steps().to_vec()))
            }
        }
    };
    for (text, expected) in [
        (
            "MATCH (a) OPTIONAL MATCH p = (a)-[:R]->(b) RETURN p",
            vec![
                vec![path(1, &[(91, 2)])],
                vec![None],
                vec![None],
                vec![None],
            ],
        ),
        (
            "MATCH (a)-[:R]->(b) MATCH p = (b)-[:S]->(c) RETURN p",
            vec![vec![path(2, &[(92, 3)])]],
        ),
        (
            "MATCH p1 = (a)-[:R]->(b) MATCH p2 = (b)-[:S]->(c) RETURN p1, p2",
            vec![vec![path(1, &[(91, 2)]), path(2, &[(92, 3)])]],
        ),
        (
            // Anchored at its correlated last vertex, read in declared order.
            "MATCH (c) OPTIONAL MATCH p = (a)-[:R]->(b)-[:S]->(c) RETURN p",
            vec![
                vec![path(1, &[(91, 2), (92, 3)])],
                vec![None],
                vec![None],
                vec![None],
            ],
        ),
        (
            "MATCH (a) OPTIONAL MATCH p = (a)-[:R]->(b)-[:S*1..2]->(c) RETURN p",
            vec![
                vec![path(1, &[(91, 2), (92, 3)])],
                vec![None],
                vec![None],
                vec![None],
            ],
        ),
    ] {
        let mut found: Vec<Vec<Path>> = execute(text, &edges)
            .iter()
            .map(|row| row.values().iter().map(cell).collect())
            .collect();
        found.sort();
        let mut expected = expected;
        expected.sort();
        assert_eq!(found, expected, "{text}");
    }
    // The set and pipeline facades read a clause's path as a bare name too.
    for text in [
        "MATCH (a) OPTIONAL MATCH p = (a)-[:R]->(b) RETURN a, p",
        "MATCH (a) OPTIONAL MATCH p = (a)-[:R]->(b) RETURN p, length(p) AS l",
    ] {
        fgdb_gql::PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
    }
    for text in [
        // An existential body's locals never escape, so it names no path.
        "MATCH (a) WHERE EXISTS { MATCH p = (a)-->() } RETURN a",
        // A clause's path name must be fresh.
        "MATCH (a) OPTIONAL MATCH a = (a)-[:R]->(b) RETURN a",
        "MATCH p = (a)-[:R]->(b) OPTIONAL MATCH p = (b)-[:S]->(c) RETURN p",
    ] {
        assert!(PreparedGraphText::prepare(text, symbols).is_err(), "{text}");
    }
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

/// openCypher reads a path's `nodes(p)` and `relationships(p)`, and a
/// variable-length relationship variable, as lists of their elements in
/// traversal order, for every list operation: index, slice, size, reverse,
/// comprehension, quantifier and reduce (TCK Quantifier1-4 over
/// `tail(nodes(p))`, Match9 [1] `last(r)`).
#[test]
fn path_element_lists_answer_every_list_operation() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GqlParameters, GqlQueryPolicy, PreparedGraphSetText};
    use fgdb_types::{CanonicalScalar, EId, VId};
    let edges = [
        (EId(91), VId(1), RelationId(1), VId(2)),
        (EId(92), VId(2), RelationId(1), VId(3)),
    ];
    let first = |text: &str| {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let rows = query
            .execute_governed(
                GqlQueryPolicy::new(1000, 1000, 100_000, 100_000),
                |pattern, remaining| {
                    pattern.plan().execute_governed_with_identified_properties(
                        edges.len() as u64 + 4,
                        (1..=4).map(VId),
                        edges.iter().copied(),
                        |_, _| Ok::<_, ()>(true),
                        |_, _| Ok(None),
                        remaining,
                        || Ok::<_, ()>(()),
                    )
                },
                || Ok::<_, ()>(()),
            )
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .value;
        assert_eq!(rows.len(), 1, "{text}");
        rows[0].values()[0].clone()
    };
    let vertices = |ids: &[u128]| {
        GraphValue::List(ids.iter().map(|&id| GraphValue::Vertex(VId(id))).collect())
    };
    let edges_of =
        |ids: &[u128]| GraphValue::List(ids.iter().map(|&id| GraphValue::Edge(EId(id))).collect());
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let truth = |b: bool| GraphValue::Scalar(CanonicalScalar::Bool(b));
    let path = "MATCH p = (a)-[:R]->(b)-[:R]->(c) RETURN";
    for (item, expected) in [
        ("tail(nodes(p)) AS v", vertices(&[2, 3])),
        ("nodes(p)[1..] AS v", vertices(&[2, 3])),
        ("reverse(nodes(p)) AS v", vertices(&[3, 2, 1])),
        ("[x IN relationships(p) | x] AS v", edges_of(&[91, 92])),
        ("relationships(p)[0] AS v", GraphValue::Edge(EId(91))),
        ("last(relationships(p)) AS v", GraphValue::Edge(EId(92))),
        ("head(nodes(p)) AS v", GraphValue::Vertex(VId(1))),
        ("size(nodes(p)) AS v", int(3)),
        ("size(relationships(p)) AS v", int(2)),
        ("any(x IN nodes(p) WHERE true) AS v", truth(true)),
        (
            "single(x IN relationships(p) WHERE true) AS v",
            truth(false),
        ),
        ("reduce(n = 0, x IN relationships(p) | n + 1) AS v", int(2)),
    ] {
        assert_eq!(first(&format!("{path} {item}")), expected, "{item}");
    }
    for (text, expected) in [
        (
            "MATCH ()-[r:R*2]->() RETURN last(r) AS v",
            GraphValue::Edge(EId(92)),
        ),
        (
            "MATCH ()-[r:R*2]->() RETURN head(r) AS v",
            GraphValue::Edge(EId(91)),
        ),
        ("MATCH ()-[r:R*2]->() RETURN size(r) AS v", int(2)),
    ] {
        assert_eq!(first(text), expected, "{text}");
    }
}

/// The two-edge chain 1 -[91]-> 2 -[92]-> 3 for element property laws: `name`
/// is "a" on vertex 1 and "b" on vertex 2 (vertex 3 has none), and each edge
/// carries `w` equal to its id. With `with_reader`, `x.key` on an element reads
/// through the host reader and counts each read in `reads`. Without it, the
/// host reads no element properties.
fn chain_rows(
    text: &str,
    with_reader: bool,
    reads: &std::cell::Cell<usize>,
) -> Result<
    Vec<fgdb_gql::algebra::GraphValueRow>,
    fgdb_gql::GqlQueryError<fgdb_gql::GraphSetExecutionError<()>, ()>,
> {
    use fgdb_delta_types::{ElementId, PropertyKeyId};
    use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
    use fgdb_gql::{
        GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, PreparedGraphSetText,
        PreparedProcedureCall,
    };
    use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
    let keys = |kind: GraphSymbolKind, name: &str| match (kind, name) {
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(PropertyKeyId(7))),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(PropertyKeyId(8))),
        _ => symbols(kind, name),
    };
    let edges = [
        (EId(91), VId(1), RelationId(1), VId(2)),
        (EId(92), VId(2), RelationId(1), VId(3)),
    ];
    let stored = |element: ElementId, key: PropertyKeyId| {
        reads.set(reads.get() + 1);
        Ok::<_, ()>(
            match (element, key) {
                (ElementId::Vertex(VId(1)), PropertyKeyId(7)) => {
                    Some(CanonicalScalar::ucs_basic_text("a").unwrap())
                }
                (ElementId::Vertex(VId(2)), PropertyKeyId(7)) => {
                    Some(CanonicalScalar::ucs_basic_text("b").unwrap())
                }
                (ElementId::Edge(EId(id)), PropertyKeyId(8)) => {
                    Some(CanonicalScalar::Int(id as i64))
                }
                _ => None,
            }
            .map(CanonicalPropertyValue::from),
        )
    };
    let query = PreparedGraphSetText::prepare(text, keys)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    // Every chain edge has relation R, so `type(r)` reads its name (fgdb-fj106).
    // A conjunction with `type(r)` filters inside the pattern, so its accessors
    // serve the same names and weights as the row reader above.
    let edge_type = CanonicalScalar::ucs_basic_text("R").unwrap();
    let names: [fgdb_types::CanonicalPropertyValue; 2] = [
        CanonicalScalar::ucs_basic_text("a").unwrap().into(),
        CanonicalScalar::ucs_basic_text("b").unwrap().into(),
    ];
    let weights = edges.map(|(EId(id), ..)| {
        fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Int(id as i64))
    });
    let patterns = |pattern: &PreparedGraphPattern<GraphValueRow>, remaining: GqlQueryPolicy| {
        pattern.plan().execute_governed_with_element_accessors(
            edges.len() as u64 + 4,
            (1..=4).map(VId),
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |vertex, key| {
                Ok(match (vertex, key) {
                    (VId(id @ 1..=2), PropertyKeyId(7)) => Some(&names[id as usize - 1]),
                    _ => None,
                })
            },
            |edge, key| {
                Ok((key == PropertyKeyId(8))
                    .then(|| edges.iter().position(|(id, ..)| *id == edge))
                    .flatten()
                    .map(|at| &weights[at]))
            },
            |_| Ok(None),
            |_| Ok(Some(&edge_type)),
            remaining,
            || Ok::<_, ()>(()),
        )
    };
    let policy = GqlQueryPolicy::new(1000, 1000, 100_000, 100_000);
    let result = if with_reader {
        query.execute_governed_with_elements(
            policy,
            patterns,
            |_: &PreparedProcedureCall,
             _: &[GraphValue],
             _: GqlQueryPolicy|
             -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
                unreachable!("no CALL source")
            },
            stored,
            || Ok::<_, ()>(()),
        )
    } else {
        query.execute_governed(policy, patterns, || Ok::<_, ()>(()))
    };
    result.map(|execution| execution.value)
}

/// fgdb-xnd7x: `x.key` over a vertex or edge an expression produced (a path's
/// nodes or relationships, an UNWIND row) reads that element's stored property
/// through the host's reader, with the key bound at preparation. A key the
/// catalog lacks is NULL without a read, an absent property is NULL, a
/// non-element operand stays a typed error, and a host with no reader refuses
/// rather than guessing NULL (TCK Quantifier1-4 [8], [9]).
#[test]
fn element_values_read_their_properties_through_the_host_reader() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GqlQueryError, GraphIntegerErrorKind, GraphSetExecutionError};
    use fgdb_types::CanonicalScalar;
    use std::cell::Cell;
    let text = |value: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap());
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let list = |values: Vec<GraphValue>| GraphValue::List(values.into_boxed_slice());
    let reads = Cell::new(0_usize);
    let run = |text: &str, with_reader: bool| {
        chain_rows(text, with_reader, &reads).map(|rows| {
            assert_eq!(rows.len(), 1, "{text}");
            rows[0].values()[0].clone()
        })
    };
    let path = "MATCH p = (a)-[:R]->(b)-[:R]->(c) RETURN";
    for (item, expected, read_count) in [
        (
            "[x IN nodes(p) | x.name] AS v",
            list(vec![text("a"), text("b"), null.clone()]),
            3,
        ),
        (
            "[e IN relationships(p) | e.w] AS v",
            list(vec![int(91), int(92)]),
            2,
        ),
        (
            "any(x IN nodes(p) WHERE x.name = 'b') AS v",
            GraphValue::Scalar(CanonicalScalar::Bool(true)),
            3,
        ),
        (
            "[x IN tail(nodes(p)) | x.name] AS v",
            list(vec![text("b"), null.clone()]),
            2,
        ),
        // The catalog lacks `nokey`: NULL for every element, and no read.
        (
            "[x IN nodes(p) | x.nokey] AS v",
            list(vec![null.clone(), null.clone(), null.clone()]),
            0,
        ),
    ] {
        reads.set(0);
        let statement = format!("{path} {item}");
        assert_eq!(run(&statement, true).unwrap(), expected, "{item}");
        assert_eq!(reads.get(), read_count, "{item}");
    }
    // A carried path's elements read the same way after WITH and UNWIND: one
    // row per node, each node read once.
    reads.set(0);
    let unwound = chain_rows(
        "MATCH p = (a)-[:R]->(b)-[:R]->(c) WITH nodes(p) AS ns UNWIND ns AS n \
         RETURN n.name AS v",
        true,
        &reads,
    )
    .unwrap();
    let mut names: Vec<_> = unwound.iter().map(|row| row.values()[0].clone()).collect();
    names.sort();
    let mut expected = vec![text("a"), text("b"), null.clone()];
    expected.sort();
    assert_eq!(names, expected);
    assert_eq!(reads.get(), 3);
    let typed_error =
        |result: Result<GraphValue, GqlQueryError<GraphSetExecutionError<()>, ()>>| {
            matches!(
                result,
                Err(GqlQueryError::Source(GraphSetExecutionError::Projection { error, .. }))
                    if error.kind == GraphIntegerErrorKind::IncompatibleOperands
            )
        };
    // A non-element operand is still a typed error with a reader present.
    assert!(typed_error(run(
        &format!("{path} [x IN [1, 2] | x.name] AS v"),
        true
    )));
    // A host with no reader refuses instead of reading NULL.
    reads.set(0);
    assert!(typed_error(run(
        &format!("{path} [x IN nodes(p) | x.name] AS v"),
        false
    )));
    assert_eq!(reads.get(), 0);
}

/// fgdb-1c29d: a root MATCH WHERE over list or path values (a quantifier,
/// size(nodes(p)), relationships(p)) runs as a row filter after the match,
/// exactly as the explicit `WITH * WHERE` form does. Element properties inside
/// it read through the host reader (fgdb-xnd7x). In the chain 1 -> 2 -> 3,
/// `[:R*1..2]` matches the paths 1-2, 2-3 and 1-2-3.
#[test]
fn match_where_over_list_values_filters_after_the_match() {
    use std::cell::Cell;
    let reads = Cell::new(0_usize);
    let rows = |text: &str| {
        let mut rows =
            chain_rows(text, true, &reads).unwrap_or_else(|error| panic!("{text}: {error:?}"));
        rows.sort();
        rows
    };
    let path = "MATCH p = (a)-[:R*1..2]->(c)";
    for (predicate, count) in [
        ("all(x IN nodes(p) WHERE x.name IS NOT NULL)", 1),
        ("any(x IN nodes(p) WHERE x.name = 'b')", 3),
        ("size(nodes(p)) > 2", 1),
        ("none(e IN relationships(p) WHERE e.w = 92)", 1),
        (
            "single(x IN nodes(p) WHERE x.name = 'a') AND size(relationships(p)) = 1",
            1,
        ),
    ] {
        let deferred = rows(&format!("{path} WHERE {predicate} RETURN a, c"));
        let explicit = rows(&format!("{path} WITH * WHERE {predicate} RETURN a, c"));
        assert_eq!(deferred, explicit, "{predicate}");
        assert_eq!(deferred.len(), count, "{predicate}");
    }
    // fgdb-fj106: `type(r)` in WHERE reads the relationship's type through
    // the same row filter (both chain edges are R; no S edge exists), and
    // `type(r) = 'R'` keeps exactly the rows the typed pattern matches.
    let untyped = "MATCH (a)-[r]->(c)";
    for (predicate, count) in [
        ("type(r) = 'R'", 2),
        ("type(r) = 'S'", 0),
        ("NOT type(r) = 'R'", 0),
        ("type(r) = 'S' OR type(r) = 'R'", 2),
        ("type(r) = 'R' AND c.name = 'b'", 1),
    ] {
        let deferred = rows(&format!("{untyped} WHERE {predicate} RETURN a, c"));
        let explicit = rows(&format!("{untyped} WITH * WHERE {predicate} RETURN a, c"));
        assert_eq!(deferred, explicit, "{predicate}");
        assert_eq!(deferred.len(), count, "{predicate}");
    }
    assert_eq!(
        rows(&format!("{untyped} WHERE type(r) = 'R' RETURN a, c")),
        rows("MATCH (a)-[r:R]->(c) RETURN a, c")
    );
    // The deferred predicate belongs to the root MATCH, so v1 refuses another
    // MATCH clause between it and the RETURN rather than moving it past one.
    assert!(
        fgdb_gql::PreparedGraphSetText::prepare(
            "MATCH p = (a)-[:R]->(c) WHERE size(nodes(p)) > 1 MATCH (d) RETURN a",
            symbols,
        )
        .is_err()
    );
}
