//! Untyped atoms range over concrete edge occurrences, including mixed paths.

use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::{EdgeRelation, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{EId, VId};
use std::collections::BTreeMap;

type Edge = (VId, RelationId, VId);

fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(7))),
        _ => None,
    })
    .unwrap_or_else(|error| panic!("{text}: {error}"))
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

fn run(text: &str, edges: &[Edge]) -> Vec<Vec<Option<VId>>> {
    prepare(text)
        .plan()
        .execute_governed_with_properties(
            edges.len() as u64 + 4,
            (0..4).map(VId),
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            GqlQueryPolicy::new(1000, 10000, 1_000_000, 1_000_000),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
        .iter()
        .map(|row| row.values().iter().map(|value| value.as_vertex()).collect())
        .collect()
}

fn fixture() -> Vec<Edge> {
    vec![
        (VId(0), RelationId(0), VId(1)),
        (VId(0), RelationId(u64::MAX), VId(1)),
        (VId(1), RelationId(7), VId(2)),
        (VId(2), RelationId(0), VId(2)),
        (VId(2), RelationId(u64::MAX), VId(0)),
    ]
}

#[test]
fn omitted_types_and_brackets_preserve_parallel_edges_and_self_loop_multiplicity() {
    let edges = fixture();
    for (atoms, direction) in [
        (["(a)-->(b)", "(a)-[]->(b)"], 0),
        (["(a)<--(b)", "(a)<-[]-(b)"], 1),
        (["(a)--(b)", "(a)-[]-(b)"], 2),
    ] {
        // Independent occurrence oracle: orient each physical edge exactly
        // once, and add the opposite orientation only for a non-loop.
        let mut expected = Vec::new();
        for &(left, _, right) in &edges {
            if direction != 1 {
                expected.push(vec![Some(left), Some(right)]);
            }
            if direction == 1 || (direction == 2 && left != right) {
                expected.push(vec![Some(right), Some(left)]);
            }
        }
        expected.sort();
        for atom in atoms {
            let text = format!("MATCH {atom} RETURN ALL a,b");
            assert_eq!(run(&text, &edges), expected, "{text}");
            assert_eq!(prepare(&text).plan().edge_relations(), None);
            let mut distinct = expected.clone();
            distinct.dedup();
            assert_eq!(
                run(&format!("MATCH {atom} RETURN DISTINCT a,b"), &edges),
                distinct,
            );
        }
    }
    assert_eq!(
        run("MATCH (a)-->(b)-[:R]->(c) RETURN a,c", &edges),
        vec![vec![Some(VId(0)), Some(VId(2))]; 2],
    );
}

#[test]
fn bounded_mixed_walks_and_shortest_selectors_match_an_independent_path_enumerator() {
    let edges = fixture();
    for (minimum, maximum) in [(0, 0), (0, 3), (1, 3), (2, 3)] {
        let mut paths = Vec::new();
        for start in (0..4).map(VId) {
            let mut frontier = vec![start];
            for depth in 0..=maximum {
                if depth >= minimum {
                    paths.extend(frontier.iter().map(|end| (start, *end, depth)));
                }
                frontier = frontier
                    .iter()
                    .flat_map(|current| {
                        edges.iter().filter_map(move |&(source, _, target)| {
                            (source == *current).then_some(target)
                        })
                    })
                    .collect();
            }
        }
        let mut shortest = BTreeMap::new();
        for &(start, end, depth) in &paths {
            shortest
                .entry((start, end))
                .and_modify(|best: &mut usize| *best = (*best).min(depth))
                .or_insert(depth);
        }
        for selector in ["WALK", "ALL SHORTEST WALK", "ANY SHORTEST WALK"] {
            let mut expected: Vec<_> = paths
                .iter()
                .filter(|&&(start, end, depth)| {
                    selector == "WALK" || shortest[&(start, end)] == depth
                })
                .map(|&(start, end, _)| vec![Some(start), Some(end)])
                .collect();
            expected.sort();
            if selector == "ANY SHORTEST WALK" {
                expected.dedup();
            }
            let text = format!("MATCH {selector} (a)-[*{minimum}..{maximum}]->(b) RETURN ALL a,b");
            assert_eq!(run(&text, &edges), expected, "{text}");
        }
    }
}

#[test]
fn untyped_optional_and_existence_keep_null_extension_and_isolates() {
    let edges = fixture();
    let mut expected: Vec<_> = edges
        .iter()
        .map(|&(source, _, target)| vec![Some(source), Some(target)])
        .collect();
    expected.push(vec![Some(VId(3)), None]);
    expected.sort();
    assert_eq!(
        run("MATCH (a) OPTIONAL MATCH (a)-->(b) RETURN ALL a,b", &edges),
        expected,
    );
    assert_eq!(
        run(
            "MATCH (a) WHERE NOT EXISTS { MATCH (a)-->(b) } RETURN a",
            &edges
        ),
        vec![vec![Some(VId(3))]],
    );
}

#[test]
fn captured_shortest_and_trail_paths_keep_real_mixed_relation_edge_ids() {
    let edges = [
        (EId(50), VId(0), RelationId(0), VId(1)),
        (EId(60), VId(1), RelationId(u64::MAX), VId(2)),
    ];
    for atom in [
        "shortestPath((a)-[*2..3]->(b))",
        "TRAIL (a)-[*2..3]->(b)",
        "ACYCLIC (a)-[*2..3]->(b)",
        "SIMPLE (a)-[*2..3]->(b)",
    ] {
        let text = format!("MATCH p = {atom} RETURN p");
        let rows = prepare(&text)
            .plan()
            .execute_governed_with_identified_properties(
                5,
                (0..3).map(VId),
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                GqlQueryPolicy::new(1000, 1000, 100_000, 100_000),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert_eq!(rows.len(), 1, "{text}");
        let path = rows[0].values()[0].as_path().unwrap();
        assert_eq!(path.start(), VId(0));
        assert_eq!(path.steps(), &[(EId(50), VId(1)), (EId(60), VId(2))]);
    }
}

#[test]
fn typed_plan_bytes_keep_the_existing_transcript_and_cannot_alias_any() {
    let typed = prepare("MATCH (a)-[:R]->(b) RETURN a,b");
    // Frozen v1 transcript: four operators, typed ScanEdges(R=7, Forward),
    // ProjectValues(vertex 0, vertex 1), canonical value order, unlimited page.
    let mut expected = b"fgdb:bounded-gla:v1\0".to_vec();
    expected.extend_from_slice(&[
        0, 0, 0, 0, 0, 0, 0, 4, 2, 0, 0, 0, 0, 0, 0, 0, 7, 0, 12, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 1, 13, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ]);
    assert_eq!(typed.canonical_bytes(), expected);
    assert_eq!(typed.plan().edge_relations(), Some([RelationId(7)].into()));
    let any = prepare("MATCH (a)-->(b) RETURN a,b");
    assert_ne!(any.canonical_bytes(), expected);
    assert_eq!(any.plan().edge_relations(), None);
    assert!(EdgeRelation::Any.matches(RelationId(0)));
    assert!(EdgeRelation::Any.matches(RelationId(u64::MAX)));
    for invalid in [
        "MATCH (a)<-->(b) RETURN a",
        "MATCH (a)-[:]->(b) RETURN a",
        "MATCH (a)-[*]->(b) RETURN a",
        "MATCH (a)-[*1..]->(b) RETURN a",
    ] {
        assert!(PreparedGraphText::prepare(invalid, |_, _: &str| None).is_err());
    }
}
