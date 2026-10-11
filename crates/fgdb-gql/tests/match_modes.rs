//! Clause-wide relationship identity through the public MATCH compiler.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern, VertexPredicate};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
    PreparedGraphSetText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const VERTICES: [VId; 4] = [VId(1), VId(2), VId(3), VId(4)];
const NUMBERS: [CanonicalScalar; 4] = [
    CanonicalScalar::Int(1),
    CanonicalScalar::Int(2),
    CanonicalScalar::Int(3),
    CanonicalScalar::Int(4),
];
type Edge = (EId, VId, RelationId, VId);

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 4_000_000, 4_000_000)
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R" | "REL1" | "T1") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S" | "T2") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "A" | "Label1") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "B" | "Label2") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn matches(vid: VId, predicates: &[VertexPredicate]) -> Result<bool, ()> {
    Ok(predicates.iter().all(|predicate| {
        predicate.matches(
            &[LabelId(vid.0 as u64)],
            &[(P, NUMBERS[vid.0 as usize - 1].clone())],
        )
    }))
}

fn bind(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn run(text: &str, edges: &[Edge]) -> Vec<GraphValueRow> {
    bind(text)
        .plan()
        .execute_governed_with_identified_properties(
            (VERTICES.len() + edges.len()) as u64,
            VERTICES,
            edges.iter().copied(),
            matches,
            |vid, key| Ok((key == P).then_some(&NUMBERS[vid.0 as usize - 1])),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .value
}

#[test]
fn fixed_paths_do_not_reuse_an_edge_in_either_direction_or_a_self_loop() {
    let one = [(EId(11), VId(1), R, VId(2))];
    for pattern in [
        "(a)-[:R]->(b)<-[:R]-(c)",
        "(a)<-[:R]-(b)-[:R]->(c)",
        "(a)-[:R]-(b)-[:R]-(c)",
    ] {
        let text = format!("MATCH {pattern} RETURN a,b,c");
        assert!(run(&text, &one).is_empty(), "{text}");
        assert!(
            !run(
                &format!("MATCH REPEATABLE ELEMENTS {pattern} RETURN a,b,c"),
                &one
            )
            .is_empty()
        );
    }
    let loop_edge = [(EId(11), VId(1), R, VId(1))];
    assert!(run("MATCH (a)-[:R]-(b)-[:R]-(c) RETURN a,b,c", &loop_edge).is_empty());
    assert_eq!(run("MATCH (a)-[:R]-(b) RETURN a,b", &loop_edge).len(), 1);
    assert_eq!(
        run(
            "MATCH REPEATABLE ELEMENTS (a)-[:R]-(b)-[:R]-(c) RETURN a,b,c",
            &loop_edge
        )
        .len(),
        1
    );
}

#[test]
fn parallel_relationships_keep_their_own_identity_and_bag_multiplicity() {
    let edges = [(EId(11), VId(1), R, VId(2)), (EId(12), VId(1), R, VId(2))];
    let text = "MATCH (a)-[r:R]->(b)<-[s:R]-(c) RETURN a,r,b,s,c";
    let rows = run(text, &edges);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.get(1) != row.get(3)));
    assert!(rows.iter().all(|row| row.get(0) == row.get(4)));
    assert_eq!(
        run(
            "MATCH REPEATABLE ELEMENTS (a)-[r:R]->(b)<-[s:R]-(c) RETURN a,r,b,s,c",
            &edges
        )
        .len(),
        4
    );
    let mut reversed = edges;
    reversed.reverse();
    assert_eq!(run(text, &reversed), rows);
    assert_eq!(
        run("MATCH (a)-[:R]-(b)-[:R]-(c) RETURN a,b,c", &edges).len(),
        4
    );
}

#[test]
fn comma_parts_share_the_constraint_and_later_clauses_reset_it() {
    let edges = [(EId(11), VId(1), R, VId(2))];
    assert!(run("MATCH (a)-[:R]->(b), (c)-[:R]->(d) RETURN a,b,c,d", &edges).is_empty());
    for text in [
        "MATCH (a)-[:R]->(b) MATCH (b)<-[:R]-(c) RETURN a,b,c",
        "MATCH (a)-[:R]->(b) OPTIONAL MATCH (b)<-[:R]-(c) RETURN a,b,c",
        "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b), (b)<-[:R]-(c) RETURN a,b,c",
    ] {
        let rows = run(text, &edges);
        assert_eq!(rows.len(), 1, "{text}");
        assert_eq!(rows[0].get(0), rows[0].get(2), "{text}");
    }
    let optional = run(
        "MATCH (a:A) OPTIONAL MATCH (a)-[:R]->(b)<-[:R]-(c) RETURN a,b,c",
        &edges,
    );
    assert_eq!(optional.len(), 1);
    assert!(optional[0].get(1).unwrap().is_null());
    assert!(optional[0].get(2).unwrap().is_null());
    assert!(
        run(
            "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b) MATCH (b)<-[:R]-(c)-[:R]->(d) RETURN a,d",
            &edges,
        )
        .is_empty()
    );
}

#[test]
fn quantified_segments_share_relationships_with_the_rest_of_the_same_pattern() {
    let edges = [(EId(11), VId(1), R, VId(2)), (EId(12), VId(2), R, VId(3))];
    for text in [
        "MATCH (a:A)-[:R*1..2]->(b)<-[:R]-(c) RETURN a,b,c",
        "MATCH (a:A)-[:R]->(b)<-[:R*1..2]-(c) RETURN a,b,c",
        "MATCH (a:A)-[:R*1..2]->(b), (b)<-[:R*1..2]-(c) RETURN a,b,c",
        "MATCH (a:A)-[:R*2..3]-(a) RETURN a",
    ] {
        assert!(run(text, &edges).is_empty(), "{text}");
    }
    let rows = run("MATCH (a:A)-[:R*0]->(b)-[:R]->(c) RETURN a,b,c", &edges);
    assert_eq!(rows.len(), 1, "a zero-hop segment consumes no relationship");
    assert_eq!(rows[0].get(0), rows[0].get(1));
    assert_eq!(
        run(
            "MATCH REPEATABLE ELEMENTS (a:A)-[:R*2]-(b) RETURN b",
            &edges
        )
        .len(),
        2
    );
    assert_eq!(run("MATCH (a:A)-[:R*2]-(b) RETURN b", &edges).len(), 1);
}

#[test]
fn pattern_predicates_and_exists_have_their_own_edge_distinct_witnesses() {
    let one = [(EId(11), VId(1), R, VId(2))];
    for text in [
        "MATCH (n) WHERE (n)-[:REL1*2]-() RETURN n",
        "MATCH (n),(m) WHERE (n)-[:REL1*2]-(m) RETURN n,m",
        "MATCH (n) WHERE exists((n)-[:REL1*2]-()) RETURN n",
        "MATCH (n) WHERE EXISTS { MATCH (n)-[:REL1*2]-() } RETURN n",
        "MATCH REPEATABLE ELEMENTS (n) WHERE (n)-[:REL1*2]-() RETURN n",
    ] {
        assert!(run(text, &one).is_empty(), "{text}");
    }
    assert_eq!(
        run("MATCH (n) WHERE NOT (n)-[:REL1*2]-() RETURN n", &one).len(),
        4
    );
    assert_eq!(
        run(
            "MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (b)<-[:R]-(a) } RETURN a,b",
            &one
        )
        .len(),
        1
    );
    assert_eq!(
        run(
            "MATCH (n) WHERE EXISTS { MATCH REPEATABLE ELEMENTS (n)-[:REL1*2]-() } RETURN n",
            &one
        )
        .len(),
        2
    );
}

#[test]
fn backtracking_does_not_inflate_counts_or_leak_into_captured_paths() {
    let edges = [(EId(11), VId(1), R, VId(2)), (EId(12), VId(2), S, VId(2))];
    let rows = run("MATCH (x:A)-[r1]->(y)-[r2]-(z) RETURN x,r1,y,r2,z", &edges);
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0].get(1), rows[0].get(3));
    let one = [(EId(11), VId(2), R, VId(1))];
    assert!(run("MATCH p = (a:Label1)<--(:Label2)--() RETURN p", &one).is_empty());

    let edges = [
        (EId(11), VId(1), R, VId(2)),
        (EId(12), VId(2), R, VId(3)),
        (EId(13), VId(2), S, VId(4)),
    ];
    for (mode, expected) in [("", 2), ("REPEATABLE ELEMENTS ", 3)] {
        let text = format!("MATCH {mode}(:A)-->()--() RETURN count(*) AS n");
        let query = PreparedGraphAggregateText::prepare(&text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let result = query
            .execute_governed_with_identified_properties(
                7,
                VERTICES,
                edges,
                matches,
                |_, _| Ok(None),
                policy(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(result.value[0].values()[0].as_count(), Some(expected));
    }
    for row in run("MATCH p = (a:A)-->()--() RETURN p", &edges) {
        let path = row.get(0).unwrap().as_path().unwrap();
        assert_eq!(path.steps().len(), 2);
        assert_ne!(path.steps()[0].0, path.steps()[1].0);
    }
}

#[test]
fn shortest_selection_finds_a_valid_trail_after_an_invalid_shorter_walk() {
    let edges = [
        (EId(11), VId(1), R, VId(2)),
        (EId(12), VId(1), R, VId(3)),
        (EId(13), VId(3), R, VId(4)),
        (EId(14), VId(4), R, VId(1)),
    ];
    for selector in ["ANY SHORTEST", "ALL SHORTEST"] {
        let text = format!("MATCH p = {selector} (a:A)-[:R*2..3]-(b) WHERE a=b RETURN p");
        let paths = run(&text, &edges);
        assert_eq!(paths.len(), if selector == "ANY SHORTEST" { 1 } else { 2 });
        for row in paths {
            let path = row.get(0).unwrap().as_path().unwrap();
            assert_eq!(
                path.steps().len(),
                3,
                "invalid two-hop backtracks cannot settle the endpoint"
            );
            let mut ids: Vec<_> = path.steps().iter().map(|step| step.0).collect();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), 3);
        }
    }
}

#[test]
fn row_pipelines_and_set_arms_preserve_the_match_mode() {
    let edges = [(EId(11), VId(1), R, VId(2))];
    for (text, expected) in [
        ("MATCH (a)-[:R]->(b)<-[:R]-(c) WITH a RETURN a", 0),
        (
            "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)<-[:R]-(c) WITH a RETURN a",
            1,
        ),
        (
            "MATCH (a)-[:R]->(b) WITH a,b MATCH (b)<-[:R]-(c) RETURN a,c",
            1,
        ),
        (
            "MATCH (a)-[:R]->(b)<-[:R]-(c) RETURN a UNION ALL MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)<-[:R]-(c) RETURN a",
            1,
        ),
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let rows = query
            .execute_governed(
                policy(),
                |pattern, budget| {
                    pattern.plan().execute_governed_with_identified_properties(
                        5,
                        VERTICES,
                        edges,
                        matches,
                        |_, _| Ok(None),
                        budget,
                        || Ok::<_, ()>(()),
                    )
                },
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert_eq!(rows.len(), expected, "{text}");
        assert!(
            rows.iter()
                .all(|row| matches!(row.get(0), Some(GraphValue::Vertex(VId(1)))))
        );
    }
}

#[test]
fn explicit_and_implicit_modes_have_stable_distinct_plan_identities() {
    let implicit = bind("MATCH (a)-[:R]->(b)<-[:R]-(c) RETURN a,b,c");
    for spelling in [
        "DIFFERENT EDGES",
        "DIFFERENT RELATIONSHIPS",
        "DIFFERENT EDGE BINDINGS",
    ] {
        let text = format!("MATCH {spelling} (a)-[:R]->(b)<-[:R]-(c) RETURN a,b,c");
        assert_eq!(implicit.canonical_bytes(), bind(&text).canonical_bytes());
    }
    assert_ne!(
        implicit.canonical_bytes(),
        bind("MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)<-[:R]-(c) RETURN a,b,c").canonical_bytes()
    );
}
