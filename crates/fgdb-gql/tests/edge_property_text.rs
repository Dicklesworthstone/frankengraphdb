//! Fixed-length edge property binding and identity-domain separation.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphPatternTextErrorKind, GraphSymbol,
    GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
#[test]
fn fixed_edge_properties_use_eid_and_preserve_missing_null() {
    let text =
        PreparedGraphText::prepare("MATCH (a)-[r:R]->(b) RETURN r.p AS p ORDER BY p", symbols)
            .unwrap();
    let prepared = text.bind_parameters(&GqlParameters::new()).unwrap();
    let value = CanonicalScalar::Int(2003);
    let stored = CanonicalPropertyValue::from(value.clone());
    let wrong_domain = CanonicalPropertyValue::from(CanonicalScalar::Int(99));
    let result = prepared
        .plan()
        .execute_governed_with_element_properties(
            2,
            [VId(1), VId(2), VId(3)],
            [
                (EId(1), VId(1), RelationId(1), VId(2)),
                (EId(2), VId(1), RelationId(1), VId(3)),
            ],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&wrong_domain)),
            |eid, _| Ok((eid == EId(1)).then_some(&stored)),
            GqlQueryPolicy::new(2, 10, 10000, 10000),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        result
            .value
            .iter()
            .map(|row| row.values().to_vec())
            .collect::<Vec<_>>(),
        vec![
            vec![GraphValue::Scalar(value)],
            vec![GraphValue::Scalar(CanonicalScalar::Null)]
        ]
    );
}
#[test]
fn edge_property_source_errors_are_not_null() {
    let text = PreparedGraphText::prepare("MATCH (a)-[r:R]->(b) RETURN r.p AS p LIMIT 0", symbols)
        .unwrap();
    let prepared = text.bind_parameters(&GqlParameters::new()).unwrap();
    let result = prepared.plan().execute_governed_with_element_properties(
        1,
        [VId(1), VId(2)],
        [(EId(1), VId(1), RelationId(1), VId(2))],
        |_, _| Ok::<_, &'static str>(true),
        |_, _| Ok(None),
        |_, _| Err("unreadable edge payload"),
        GqlQueryPolicy::new(1, 10, 10000, 10000),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source("unreadable edge payload"))
    ));
}

/// Rows of a two-column vertex result over a fixture where edge 1 (1->2)
/// and edge 3 (3->1) have p = 5, edge 2 (1->3) has p = 7, vertex 1 has
/// p = 5 and vertex 3 has p = 9. The vertex test evaluates the plan's own
/// predicates against those values.
fn inline_rows(text: &str) -> Vec<Vec<GraphValue>> {
    let prepared = PreparedGraphText::prepare(text, symbols)
        .unwrap_or_else(|error| panic!("{text}: {error}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let five = CanonicalPropertyValue::from(CanonicalScalar::Int(5));
    let seven = CanonicalPropertyValue::from(CanonicalScalar::Int(7));
    let nine = CanonicalPropertyValue::from(CanonicalScalar::Int(9));
    let vertex_p = |vid: VId| match vid.0 {
        1 => Some(&five),
        3 => Some(&nine),
        _ => None,
    };
    let mut rows: Vec<Vec<GraphValue>> = prepared
        .plan()
        .execute_governed_with_element_properties(
            3,
            [VId(1), VId(2), VId(3)],
            [
                (EId(1), VId(1), RelationId(1), VId(2)),
                (EId(2), VId(1), RelationId(1), VId(3)),
                (EId(3), VId(3), RelationId(1), VId(1)),
            ],
            |vid, predicates| {
                let properties: Vec<_> = vertex_p(vid)
                    .map(|value| (PropertyKeyId(1), value.clone()))
                    .into_iter()
                    .collect();
                Ok::<_, ()>(
                    predicates
                        .iter()
                        .all(|predicate| predicate.matches(&[], &properties)),
                )
            },
            |vid, _| Ok(vertex_p(vid)),
            |eid, _| Ok(Some(if eid == EId(2) { &seven } else { &five })),
            GqlQueryPolicy::new(100, 100, 10000, 10000),
            || Ok::<_, ()>(()),
        )
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect();
    rows.sort();
    rows
}

#[test]
fn inline_relationship_and_anonymous_node_maps_filter_like_where() {
    // openCypher Match2 [5], Match3 [5], Match6 [4] and [7].
    let vertex = |id| GraphValue::Vertex(VId(id));
    for (inline, filtered, expected) in [
        (
            "MATCH (a)-[r:R {p: 5}]->(b) RETURN a, b",
            "MATCH (a)-[r:R]->(b) WHERE r.p = 5 RETURN a, b",
            vec![vec![vertex(1), vertex(2)], vec![vertex(3), vertex(1)]],
        ),
        (
            "MATCH (a)-[r {p: 7}]-(b) RETURN a, b",
            "MATCH (a)-[r]-(b) WHERE r.p = 7 RETURN a, b",
            vec![vec![vertex(1), vertex(3)], vec![vertex(3), vertex(1)]],
        ),
        (
            "MATCH ({p: 5})-[r:R]->(b) RETURN b",
            "MATCH (a)-[r:R]->(b) WHERE a.p = 5 RETURN b",
            vec![vec![vertex(2)], vec![vertex(3)]],
        ),
        (
            "MATCH (a)-[r:R {p: 5}]->({p: 5}) RETURN a, r.p",
            "MATCH (a)-[r:R]->(b) WHERE r.p = 5 AND b.p = 5 RETURN a, r.p",
            vec![vec![vertex(3), GraphValue::Scalar(CanonicalScalar::Int(5))]],
        ),
        (
            "MATCH (a)-[r:R {p: 6}]->(b) RETURN a, b",
            "MATCH (a)-[r:R]->(b) WHERE r.p = 6 RETURN a, b",
            vec![],
        ),
        // An unnamed relationship's map filters a private capture of it.
        (
            "MATCH (a)-[:R {p: 5}]->(b) RETURN a, b",
            "MATCH (a)-[r:R]->(b) WHERE r.p = 5 RETURN a, b",
            vec![vec![vertex(1), vertex(2)], vec![vertex(3), vertex(1)]],
        ),
        (
            "MATCH (a)-[{p: 7}]-(b) RETURN a, b",
            "MATCH (a)-[r]-(b) WHERE r.p = 7 RETURN a, b",
            vec![vec![vertex(1), vertex(3)], vec![vertex(3), vertex(1)]],
        ),
        // Two in one pattern and one in each of two clauses: every private
        // capture is a different relationship.
        (
            "MATCH (a)-[:R {p: 5}]->(b)-[:R {p: 5}]->(c) RETURN a, c",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) WHERE r.p = 5 AND s.p = 5 RETURN a, c",
            vec![vec![vertex(3), vertex(2)]],
        ),
        (
            "MATCH (a)-[:R {p: 5}]->(b) MATCH (b)-[:R {p: 7}]->(c) RETURN a, c",
            "MATCH (a)-[r:R {p: 5}]->(b) MATCH (b)-[s:R {p: 7}]->(c) RETURN a, c",
            vec![vec![vertex(3), vertex(3)]],
        ),
        (
            "MATCH (a) OPTIONAL MATCH (a)-[:R {p: 7}]->(b) RETURN a, b",
            "MATCH (a) OPTIONAL MATCH (a)-[r:R {p: 7}]->(b) RETURN a, b",
            vec![
                vec![vertex(1), vertex(3)],
                vec![vertex(2), GraphValue::Scalar(CanonicalScalar::Null)],
                vec![vertex(3), GraphValue::Scalar(CanonicalScalar::Null)],
            ],
        ),
        (
            "MATCH (a) WHERE (a)-[:R {p: 7}]->() RETURN a",
            "MATCH (a) WHERE EXISTS { MATCH (a)-[r:R]->() WHERE r.p = 7 } RETURN a",
            vec![vec![vertex(1)]],
        ),
    ] {
        assert_eq!(inline_rows(inline), expected, "{inline}");
        assert_eq!(inline_rows(filtered), expected, "{filtered}");
    }
    // The private capture is no column of RETURN *.
    let star = PreparedGraphText::prepare("MATCH (a)-[:R {p: 5}]->(b) RETURN *", symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    assert_eq!(star.columns(), ["a", "b"]);
    // A walk's map would range over a list of relationships.
    for (text, expected) in [
        (
            "MATCH (a)-[r:R*1..2 {p: 5}]->(b) RETURN a",
            "fixed-length relationship for a property map",
        ),
        (
            "MATCH (a)-[:R*1..2 {p: 5}]->(b) RETURN a",
            "fixed-length relationship for a property map",
        ),
        (
            "MATCH (a)-[r:R {p: 5, p: 6}]->(b) RETURN a",
            "distinct property name in relationship map",
        ),
        (
            "MATCH (a)-[r:R {p: a.p}]->(b) RETURN a",
            "literal or typed parameter in relationship map",
        ),
    ] {
        assert_eq!(
            PreparedGraphText::prepare(text, symbols).unwrap_err().kind,
            GraphPatternTextErrorKind::Expected(expected),
            "{text}"
        );
    }
}
