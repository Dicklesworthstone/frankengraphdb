//! Native TRAIL stays identity-sensitive through scopes and relational consumers.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphMatchMode, GraphPathFunction, GraphPatternBuilder, GraphValue,
    GraphValueRow, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWalkBounds,
    PreparedGraphAggregateText, PreparedGraphPipelineAggregateText, PreparedGraphSetText,
    PreparedGraphText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const VERTICES: [VId; 3] = [VId(1), VId(2), VId(3)];
static VALUES: [CanonicalPropertyValue; 3] = [
    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(1)),
    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(2)),
    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(3)),
];
type Edge = (EId, VId, RelationId, VId);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 1000, 2_000_000, 2_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn matches(vid: VId, predicates: &[VertexPredicate]) -> Result<bool, ()> {
    Ok(predicates
        .iter()
        .all(|test| test.matches(&[], &[(P, VALUES[vid.0 as usize - 1].clone())])))
}
fn bind(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn run(plan: &PreparedGraphPattern<GraphValueRow>, edges: &[Edge]) -> Vec<GraphValueRow> {
    plan.plan()
        .execute_governed_with_identified_properties(
            (3 + edges.len()) as u64,
            VERTICES,
            edges.iter().copied(),
            matches,
            |vid, _| Ok(Some(&VALUES[vid.0 as usize - 1])),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
}

#[test]
fn native_and_typed_trails_have_identical_definitions_in_every_direction() {
    for (direction, atom) in [
        (GlaDirection::Forward, "(a)-[:R*0..3]->(b)"),
        (GlaDirection::Reverse, "(a)<-[:R*0..3]-(b)"),
        (GlaDirection::Undirected, "(a)-[:R*0..3]-(b)"),
    ] {
        for captured in [false, true] {
            let mut builder = GraphPatternBuilder::new();
            builder.match_mode(GraphMatchMode::DifferentEdges);
            builder.vertex("a").unwrap().vertex("b").unwrap();
            builder
                .trail_walk("a", R, direction, "b", GraphWalkBounds::new(0, 3).unwrap())
                .unwrap();
            let mut columns = vec![GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")];
            let (binding, output) = if captured {
                builder.capture_path("route").unwrap();
                columns.push(GraphColumn::path(
                    "route",
                    "route",
                    GraphPathFunction::Value,
                ));
                ("route = ", "a,b,route")
            } else {
                ("", "a,b")
            };
            let typed = builder
                .prepare_values(&columns, 0, None)
                .unwrap()
                .with_duplicates();
            let native = bind(&format!("MATCH {binding}TRAIL {atom} RETURN ALL {output}"));
            assert_eq!(typed.canonical_bytes(), native.canonical_bytes());
            assert!(native.plan().requires_identified_edges());
            assert_ne!(
                native.canonical_bytes(),
                bind(&format!("MATCH {binding}WALK {atom} RETURN ALL {output}")).canonical_bytes()
            );
        }
    }
}

#[test]
fn optional_required_and_negative_scopes_use_edge_uniqueness_not_vertex_closure() {
    let one = [(EId(11), VId(1), R, VId(2))];
    let optional = bind("MATCH (a) OPTIONAL MATCH TRAIL (a)-[:R*2]-(b) RETURN ALL a,b");
    let absent = run(&optional, &one);
    assert_eq!(absent.len(), 3);
    assert!(absent.iter().all(|row| row.get(1).unwrap().is_null()));
    let exists = bind("MATCH (a) WHERE EXISTS { MATCH TRAIL (a)-[:R*2]-(b) } RETURN ALL a");
    let anti = bind("MATCH (a) WHERE NOT EXISTS { MATCH TRAIL (a)-[:R*2]-(b) } RETURN ALL a");
    assert!(run(&exists, &one).is_empty());
    assert_eq!(run(&anti, &one).len(), 3);
    let required = bind(
        "MATCH (a) OPTIONAL MATCH TRAIL (a)-[:R*2]-(b) MATCH TRAIL (b)-[:R*0]->(c) RETURN ALL a,c",
    );
    assert!(
        run(&required, &one).is_empty(),
        "zero-hop TRAIL must not rebind null"
    );
    let parallel = [one[0], (EId(12), VId(1), R, VId(2))];
    assert_eq!(run(&exists, &parallel).len(), 2);
    assert_eq!(run(&anti, &parallel).len(), 1);
    let rows = run(&required, &parallel);
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|row| row.get(0) == row.get(1)));
    // The written destination is already bound: reverse the physical traversal
    // but retain the same EId membership and outer occurrence multiplicity.
    let reverse_bound = bind("MATCH (b) MATCH TRAIL (a)-[:R*2]-(b) RETURN ALL a,b");
    assert_eq!(run(&reverse_bound, &parallel).len(), 4);
}

#[test]
fn closed_trails_continue_but_independent_match_clauses_do_not_share_used_edges() {
    let edges = [
        (EId(11), VId(1), R, VId(2)),
        (EId(12), VId(2), R, VId(1)),
        (EId(13), VId(1), R, VId(3)),
    ];
    let text = "MATCH route = TRAIL (a)-[:R*3]->(b) WHERE a.p=1 RETURN ALL route,PATH_LENGTH(route) AS hops,NODES(route) AS vertices,EDGES(route) AS links";
    let rows = run(&bind(text), &edges);
    assert_eq!(rows.len(), 1);
    let route = rows[0].get(0).unwrap().as_path().unwrap();
    assert_eq!(
        route.steps(),
        &[(EId(11), VId(2)), (EId(12), VId(1)), (EId(13), VId(3))]
    );
    assert_eq!(
        rows[0].get(1).unwrap().as_scalar(),
        Some(&CanonicalScalar::Int(3))
    );
    assert_eq!(
        rows[0].get(2),
        Some(&GraphValue::Vertices(
            vec![VId(1), VId(2), VId(1), VId(3)].into_boxed_slice()
        ))
    );
    assert_eq!(
        rows[0].get(3),
        Some(&GraphValue::Edges(
            vec![EId(11), EId(12), EId(13)].into_boxed_slice()
        ))
    );
    assert!(
        run(
            &bind("MATCH SIMPLE (a)-[:R*3]->(b) WHERE a.p=1 RETURN ALL b"),
            &edges
        )
        .is_empty()
    );
    let separate = bind("MATCH TRAIL (a)-[:R*1]->(b) MATCH TRAIL (b)<-[:R*1]-(c) RETURN ALL a,b,c");
    let rows = run(&separate, &edges[..1]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(0), rows[0].get(2));
}

#[test]
fn parameters_bind_once_and_unsupported_restrictions_refuse_before_resolution() {
    let calls = Cell::new(0);
    let prepared = PreparedGraphText::prepare(
        "MATCH TRAIL (a)-[:R*1..3]->(b) WHERE a.p=$source RETURN ALL b SKIP $skip LIMIT $take",
        |kind: GraphSymbolKind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let resolutions = calls.get();
    let args = GqlParameters::new()
        .with_int64("source", 1)
        .unwrap()
        .with_uint64("skip", 0)
        .unwrap()
        .with_uint64("take", 10)
        .unwrap();
    let bound = prepared.bind_parameters(&args).unwrap();
    assert!(bound.plan().requires_identified_edges());
    assert_eq!(calls.get(), resolutions);
    assert!(prepared.bind_parameters(&GqlParameters::new()).is_err());
    for text in [
        "MATCH TRAIL (a) RETURN a",
        "MATCH TRAIL (a)-[:R]->(b) RETURN b",
        "MATCH TRAIL (a)-[:R*1025]->(b) RETURN b",
        "MATCH TRAIL (a)-[:R]->{3,2}(b) RETURN b",
        "MATCH TRAIL (a)-[:R*1]->(b)-[:R*1]->(c) RETURN c",
        "MATCH TRAIL (a)-[:R*1]->(b),(c) RETURN b",
        "MATCH ANY SHORTEST TRAIL (a)-[:R*1..3]->(b) RETURN b",
    ] {
        assert!(
            PreparedGraphText::prepare(
                text,
                |_: GraphSymbolKind, _: &str| -> Option<GraphSymbol> {
                    panic!("invalid path definition reached catalog: {text}")
                }
            )
            .is_err(),
            "{text}"
        );
    }
    // A TRAIL cannot reuse an edge, so an unbounded one ends by itself.
    assert!(PreparedGraphText::prepare("MATCH TRAIL (a)-[:R*1..]->(b) RETURN b", symbols).is_ok());
    // Contextual mode words do not prohibit ordinary variable names.
    assert!(PreparedGraphText::prepare("MATCH (trail) RETURN trail", symbols).is_ok());
}

#[test]
fn native_aggregate_with_and_compound_set_consumers_keep_the_identified_source() {
    let edges = [
        (EId(11), VId(1), R, VId(2)),
        (EId(12), VId(2), R, VId(1)),
        (EId(13), VId(1), R, VId(3)),
    ];
    for prefix in ["TRAIL", "route = TRAIL"] {
        let text = format!("MATCH {prefix} (a)-[:R*3]->(b) WHERE a.p=1 RETURN COUNT(*) AS n");
        let aggregate = PreparedGraphAggregateText::prepare(&text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let result = aggregate
            .execute_governed_with_identified_properties(
                6,
                VERTICES,
                edges,
                matches,
                |vid, _| Ok(Some(&VALUES[vid.0 as usize - 1])),
                policy(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(result.value[0].values()[0].as_count(), Some(1));
    }
    for (text, expected) in [
        (
            "MATCH TRAIL (a)-[:R*3]->(b) WHERE a.p=1 WITH b AS destination RETURN destination",
            1,
        ),
        (
            "MATCH TRAIL (a)-[:R*3]->(b) WHERE a.p=1 RETURN b UNION ALL MATCH TRAIL (a)-[:R*3]->(b) WHERE a.p=1 RETURN b",
            2,
        ),
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let result = query
            .execute_governed(
                policy(),
                |pattern, remaining| {
                    pattern.plan().execute_governed_with_identified_properties(
                        6,
                        VERTICES,
                        edges,
                        matches,
                        |vid, _| Ok(Some(&VALUES[vid.0 as usize - 1])),
                        remaining,
                        || Ok::<_, ()>(()),
                    )
                },
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(result.value.len(), expected);
        assert!(
            result
                .value
                .iter()
                .all(|row| row.get(0).unwrap().as_vertex() == Some(VId(3)))
        );
    }
    let aggregate = PreparedGraphPipelineAggregateText::prepare(
        "MATCH TRAIL (a)-[:R*3]->(b) WHERE a.p=1 WITH b AS destination RETURN COUNT(*) AS n",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let result = aggregate
        .execute_governed_with_identified_properties(
            6,
            VERTICES,
            edges,
            matches,
            |vid, _| Ok(Some(&VALUES[vid.0 as usize - 1])),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(1));
}

#[test]
fn closure_recipe_admits_only_distinct_directed_unbounded_endpoint_support() {
    use fgdb_delta_types::LabelId;

    let symbols = |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    };
    let prepare = |text| {
        PreparedGraphText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    };
    for text in [
        "MATCH (a)-[:R*]->(b) RETURN DISTINCT a,b",
        "MATCH TRAIL (a)-[:R*1..]->(b) RETURN DISTINCT a,b",
        "MATCH REPEATABLE ELEMENTS TRAIL (a)-[:R*]->(b) RETURN DISTINCT a,b",
        "MATCH (a)<-[:R|S*]-(b) RETURN DISTINCT b AS target,a AS origin",
        "MATCH (a)-[*]->(b) RETURN DISTINCT a AS origin,b AS target",
    ] {
        let pattern = prepare(text);
        let (source, output) = pattern
            .incremental_closure_definition()
            .unwrap_or_else(|| panic!("eligible endpoint closure: {text}"));
        let source = source.incremental_pattern().expect("one-hop support leaf");
        assert!(source.preserves_duplicates(), "parallel support: {text}");
        assert!(source.plan().scans_edges());
        assert_eq!(
            source.plan().edge_relations(),
            pattern.plan().edge_relations()
        );
        assert_eq!(
            output.columns().collect::<Vec<_>>(),
            pattern
                .columns()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
        assert!(source.incremental_row_source_definition().is_some());
        assert!(source.incremental_closure_definition().is_none());
    }
    for text in [
        "MATCH (a)-[:R*0..]->(b) RETURN DISTINCT a,b",
        "MATCH TRAIL (a)-[:R*0..]->(b) RETURN DISTINCT a,b",
        "MATCH REPEATABLE ELEMENTS TRAIL (a)-[:R*0..]->(b) RETURN DISTINCT a,b",
        "MATCH (a)<-[:R|S*0..]-(b) RETURN DISTINCT b AS target,a AS origin",
        "MATCH (a)-[*0..]->(b) RETURN DISTINCT a AS origin,b AS target",
        "MATCH (a)-[:R*0..]-(b) RETURN DISTINCT a,b",
    ] {
        let pattern = prepare(text);
        let (source, output) = pattern
            .incremental_closure_definition()
            .unwrap_or_else(|| panic!("eligible reflexive closure: {text}"));
        let (operation, quantifier, edges, lifetimes) = source.incremental_binary().unwrap();
        assert_eq!(operation, fgdb_gql::GraphSetOperation::Union);
        assert_eq!(quantifier, fgdb_gql::GraphSetQuantifier::All);
        let edges = edges.incremental_pattern().unwrap();
        let lifetimes = lifetimes.incremental_pattern().unwrap();
        assert!(edges.preserves_duplicates());
        assert!(lifetimes.preserves_duplicates());
        assert!(edges.plan().scans_edges());
        assert!(!lifetimes.plan().scans_edges());
        assert_eq!(edges.plan().edge_relations(), pattern.plan().edge_relations());
        assert_eq!(
            output.columns().collect::<Vec<_>>(),
            pattern
                .columns()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
        assert!(edges.incremental_row_source_definition().is_some());
        assert!(lifetimes.incremental_row_source_definition().is_some());
        assert!(edges.incremental_closure_definition().is_none());
        assert!(lifetimes.incremental_closure_definition().is_none());
        let actual: Vec<_> = run(lifetimes, &[])
            .iter()
            .map(|row| row.values().to_vec())
            .collect();
        let expected: Vec<_> = VERTICES
            .iter()
            .map(|&vertex| vec![GraphValue::Vertex(vertex), GraphValue::Vertex(vertex)])
            .collect();
        assert_eq!(actual, expected, "isolated vertex lifetimes: {text}");
    }
    for text in [
        "MATCH (a)-[:R*]->(b) RETURN a,b",
        "MATCH (a)-[:R*0..]->(b) RETURN a,b",
        "MATCH (a)-[:R*0..3]->(b) RETURN DISTINCT a,b",
        "MATCH (a)-[:R*2..]->(b) RETURN DISTINCT a,b",
        "MATCH (a)-[:R*1..3]->(b) RETURN DISTINCT a,b",
        "MATCH (a)-[:R*]-(b) RETURN DISTINCT a,b",
        "MATCH ACYCLIC (a)-[:R*]->(b) RETURN DISTINCT a,b",
        "MATCH p = (a)-[:R*]->(b) RETURN DISTINCT a,b,p",
        "MATCH (a:L)-[:R*]->(b) RETURN DISTINCT a,b",
        "MATCH (a)-[:R*]->(b) WHERE b.p=1 RETURN DISTINCT a,b",
        "MATCH (a)-[:R*]->(b) RETURN DISTINCT a",
        "MATCH (a)-[:R*]->(b) RETURN DISTINCT a AS first,a AS second",
        "MATCH (a)-[:R*]->(b) RETURN DISTINCT a,b ORDER BY b",
        "MATCH (a)-[:R*]->(b) RETURN DISTINCT a,b LIMIT 0",
        "MATCH (a)-[:R*]->(b) RETURN DISTINCT a,b SKIP 1",
    ] {
        assert!(
            prepare(text).incremental_closure_definition().is_none(),
            "path semantics cannot silently become closure: {text}"
        );
    }
}

#[test]
fn undirected_zero_hop_support_includes_isolates_without_admitting_nonempty_false_cycles() {
    let edge = [(EId(11), VId(1), R, VId(2))];
    let zero = bind("MATCH (a)-[:R*0..]-(b) RETURN DISTINCT a,b");
    let nonempty = bind("MATCH (a)-[:R*1..]-(b) RETURN DISTINCT a,b");
    assert!(zero.incremental_closure_definition().is_some());
    assert!(nonempty.incremental_closure_definition().is_none());
    for (query, expected) in [
        (&zero, vec![(1, 1), (1, 2), (2, 1), (2, 2), (3, 3)]),
        (&nonempty, vec![(1, 2), (2, 1)]),
    ] {
        let actual: Vec<_> = run(query, &edge)
            .iter()
            .map(|row| row.values().to_vec())
            .collect();
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(a, b)| vec![GraphValue::Vertex(VId(a)), GraphValue::Vertex(VId(b))])
            .collect();
        assert_eq!(actual, expected);
    }
}
