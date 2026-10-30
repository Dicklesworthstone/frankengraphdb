//! Edge property binding, quantified hop predicates and identity domains.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
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
    inline_pattern_rows(&prepared)
}

fn inline_pattern_rows(prepared: &PreparedGraphPattern<GraphValueRow>) -> Vec<Vec<GraphValue>> {
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
        .unwrap()
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
    // Malformed fixed and quantified maps still refuse at their source site.
    for (text, expected) in [
        (
            "MATCH (a)-[r:R*1..2 {p: 5, p: 6}]->(b) RETURN a",
            "distinct property name in relationship map",
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

#[test]
fn quantified_relationship_maps_constrain_each_hop_and_each_atom() {
    let vertex = |id| GraphValue::Vertex(VId(id));
    let expected = vec![
        vec![vertex(1), vertex(2)],
        vec![vertex(3), vertex(1)],
        vec![vertex(3), vertex(2)],
    ];
    for text in [
        "MATCH (a)-[:R*1..3 {p: 5}]->(b) RETURN a,b",
        "MATCH (a)-[r:R*1..3 {p: 5}]->(b) RETURN a,b",
        "MATCH (a)-[*1..3 {p: 5}]->(b) RETURN a,b",
        "MATCH (a)-[:R* {p: 5}]->(b) RETURN a,b",
        "MATCH (a)-[:R*1..3 {p: toInteger('5')}]->(b) RETURN a,b",
        "MATCH (a)-[:R {p: 5}]->{1,3}(b) RETURN a,b",
        "MATCH (a)-[r:R {p: 5}]->{1,3}(b) RETURN a,b",
    ] {
        assert_eq!(inline_rows(text), expected, "{text}");
    }
    // The same relationship type and orientation can have independently
    // filtered adjacency in two atoms. One atom's map must not filter the
    // other's source or change the matching occurrence bag.
    assert_eq!(
        inline_rows("MATCH (a)-[:R*1..2 {p: 5}]->(b)-[:R*1..2 {p: 7}]->(c) RETURN a,c"),
        vec![vec![vertex(3), vertex(3)]]
    );
    assert_eq!(
        inline_rows("MATCH (a)<-[:R*1..3 {p: 5}]-(b) RETURN a,b"),
        vec![
            vec![vertex(1), vertex(3)],
            vec![vertex(2), vertex(1)],
            vec![vertex(2), vertex(3)],
        ]
    );
    // No generated relationship capture is exposed by an anonymous map.
    let star = PreparedGraphText::prepare("MATCH (a)-[:R* {p: 5}]->(b) RETURN *", symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    assert_eq!(star.columns(), ["a", "b"]);
}

#[test]
fn quantified_maps_rebind_without_catalog_reads_and_keep_zero_hop_truth() {
    use std::cell::Cell;
    let resolutions = Cell::new(0);
    let template = PreparedGraphText::prepare(
        "MATCH (a)-[:R*0..3 {p: $wanted}]->(b) RETURN a,b",
        |kind, name: &str| {
            resolutions.set(resolutions.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let frozen = resolutions.get();
    let vertex = |id| GraphValue::Vertex(VId(id));
    let zero_hops = vec![
        vec![vertex(1), vertex(1)],
        vec![vertex(2), vertex(2)],
        vec![vertex(3), vertex(3)],
    ];
    let seven = template
        .bind_parameters(&GqlParameters::new().with_int64("wanted", 7).unwrap())
        .unwrap();
    assert_eq!(
        inline_pattern_rows(&seven),
        vec![
            vec![vertex(1), vertex(1)],
            vec![vertex(1), vertex(3)],
            vec![vertex(2), vertex(2)],
            vec![vertex(3), vertex(3)],
        ]
    );
    let absent = template
        .bind_parameters(&GqlParameters::new().with_int64("wanted", 99).unwrap())
        .unwrap();
    assert_eq!(inline_pattern_rows(&absent), zero_hops);
    assert_ne!(seven.canonical_bytes(), absent.canonical_bytes());
    assert_eq!(resolutions.get(), frozen);
    assert_eq!(
        inline_rows("MATCH (a)-[:R*0..3 {p: NULL}]->(b) RETURN a,b"),
        zero_hops
    );
    assert!(inline_rows("MATCH (a)-[:R*1..3 {p: NULL}]->(b) RETURN a,b").is_empty());
    assert_eq!(
        inline_rows("MATCH (a)-[:R*0..0 {p: NULL}]->(b) RETURN a,b"),
        zero_hops
    );
    let secret = PreparedGraphText::prepare(
        "MATCH (a)-[:R*1..3 {p: 'confidential-hop-value'}]->(b) RETURN b",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let diagnostic = format!("{:?}", secret.plan());
    assert!(diagnostic.contains("[REDACTED]"));
    assert!(!diagnostic.contains("confidential-hop-value"));
}

#[test]
fn quantified_map_sources_fail_closed_even_when_no_output_is_requested() {
    let text =
        PreparedGraphText::prepare("MATCH (a)-[:R*1..3 {p: 5}]->(b) RETURN b LIMIT 0", symbols)
            .unwrap();
    let prepared = text.bind_parameters(&GqlParameters::new()).unwrap();
    assert!(prepared.plan().requires_identified_edges());
    assert!(prepared.plan().projects_edge_properties());
    let result = prepared.plan().execute_governed_with_element_properties(
        1,
        [VId(1), VId(2)],
        [(EId(1), VId(1), RelationId(1), VId(2))],
        |_, _| Ok::<_, &'static str>(true),
        |_, _| Ok(None),
        |_, _| Err("unreadable quantified edge payload"),
        GqlQueryPolicy::new(10, 10, 10000, 10000),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source("unreadable quantified edge payload"))
    ));
    let result = prepared.plan().execute_governed_with_identified_properties(
        1,
        [VId(1), VId(2)],
        [(EId(1), VId(1), RelationId(1), VId(2))],
        |_, _| Ok::<_, ()>(true),
        |_, _| Ok(None),
        GqlQueryPolicy::new(10, 10, 10000, 10000),
        || Ok::<_, ()>(()),
    );
    assert!(
        result.is_err(),
        "an absent edge reader must never widen a walk"
    );
}

#[test]
fn quantified_maps_preserve_stored_collection_equality_and_multiple_keys() {
    use fgdb_types::CanonicalF64;
    let int = |n| GraphValue::Scalar(CanonicalScalar::Int(n));
    let float = |n| GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(n)));
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let values = [
        GraphValue::List(vec![int(5), int(6)]),
        GraphValue::List(vec![float(5.0), float(6.0)]),
        GraphValue::List(vec![int(5), null.clone()]),
        GraphValue::map(vec![("a".into(), int(5))]).unwrap(),
    ];
    let stored: Vec<_> = values
        .iter()
        .map(|value| value.to_property_value().unwrap())
        .collect();
    let q = CanonicalPropertyValue::from(CanonicalScalar::Int(1));
    let resolver = |kind, name: &str| {
        if kind == GraphSymbolKind::Property && name == "q" {
            Some(GraphSymbol::Property(PropertyKeyId(2)))
        } else {
            symbols(kind, name)
        }
    };
    let execute = |text: &str, params: &GqlParameters| {
        let declarations: Vec<_> = params.parameter_types().collect();
        let prepared =
            PreparedGraphText::prepare_with_parameter_types(text, &declarations, resolver)
                .unwrap()
                .bind_parameters(params)
                .unwrap();
        prepared
            .plan()
            .execute_governed_with_element_properties(
                4,
                [VId(1), VId(2), VId(3), VId(4), VId(5)],
                (0..4).map(|edge| (EId(edge + 1), VId(1), RelationId(1), VId(edge + 2))),
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                |edge, key| {
                    Ok(match key {
                        PropertyKeyId(1) => stored.get(edge.0 as usize - 1),
                        PropertyKeyId(2) => Some(&q),
                        _ => None,
                    })
                },
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value
            .iter()
            .map(|row| row.values().to_vec())
            .collect::<Vec<_>>()
    };
    let expected = vec![
        vec![GraphValue::Vertex(VId(2))],
        vec![GraphValue::Vertex(VId(3))],
    ];
    let args = GqlParameters::new()
        .with_list("wanted", vec![int(5), int(6)])
        .unwrap();
    assert_eq!(
        execute(
            "MATCH (a)-[:R*1..2 {p: $wanted, q: 1}]->(b) RETURN b",
            &args
        ),
        expected
    );
    assert_eq!(
        execute(
            "MATCH (a)-[:R*1..2 {p: [5,6], q: 1}]->(b) RETURN b",
            &GqlParameters::new(),
        ),
        expected
    );
    assert!(
        execute(
            "MATCH (a)-[:R*1..2 {p: [5,6], q: 2}]->(b) RETURN b",
            &GqlParameters::new(),
        )
        .is_empty()
    );
    assert!(
        execute(
            "MATCH (a)-[:R*1..2 {p: [5,null]}]->(b) RETURN b",
            &GqlParameters::new(),
        )
        .is_empty(),
        "UNKNOWN collection equality does not admit an edge"
    );
    let map = GqlParameters::new()
        .with_map("wanted", vec![("a".into(), int(5))])
        .unwrap();
    assert_eq!(
        execute("MATCH (a)-[:R*1..2 {p: $wanted}]->(b) RETURN b", &map),
        vec![vec![GraphValue::Vertex(VId(5))]]
    );
}

#[test]
fn quantified_map_filtering_honors_every_cancellation_checkpoint() {
    use std::cell::Cell;
    let prepared =
        PreparedGraphText::prepare("MATCH (a)-[:R*1..2 {p: 5}]->(b) RETURN a,b", symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
    let five = CanonicalPropertyValue::from(CanonicalScalar::Int(5));
    let execute = |fail_at| {
        let checkpoints = Cell::new(0);
        let result = prepared.plan().execute_governed_with_element_properties(
            2,
            [VId(1), VId(2), VId(3)],
            [
                (EId(1), VId(1), RelationId(1), VId(2)),
                (EId(2), VId(2), RelationId(1), VId(3)),
            ],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            |_, _| Ok(Some(&five)),
            GqlQueryPolicy::new(100, 100, 100_000, 100_000),
            || {
                let at = checkpoints.get();
                checkpoints.set(at + 1);
                if fail_at == Some(at) { Err(at) } else { Ok(()) }
            },
        );
        (result, checkpoints.get())
    };
    let (result, checkpoints) = execute(None);
    assert_eq!(result.unwrap().value.len(), 3);
    assert!(checkpoints > 5);
    for at in 0..checkpoints {
        assert!(
            matches!(execute(Some(at)).0, Err(GqlQueryError::Interrupted(observed)) if observed == at),
            "checkpoint {at} must interrupt before releasing rows"
        );
    }
}
