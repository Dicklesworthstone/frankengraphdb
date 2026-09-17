//! ANY shortest selection coalesces search states, not surrounding result rows.

use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GlaOperator, GraphColumn, GraphMatchClause, GraphPatternBuilder, GraphWalkSearch,
    PatternBuildError, VertexPredicate,
};
use fgdb_gql::{GqlQueryError, GqlQueryPolicy, GraphWalkBounds, MAX_GRAPH_WALK_HOPS};
use fgdb_types::VId;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
fn builder(names: &[&str]) -> GraphPatternBuilder {
    let mut builder = GraphPatternBuilder::new();
    for name in names {
        builder.vertex(name).unwrap();
    }
    builder
}
fn bounds(minimum: u32, maximum: u32) -> GraphWalkBounds {
    GraphWalkBounds::new(minimum, maximum).unwrap()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

#[test]
fn any_selects_endpoint_pairs_not_globally_distinct_projected_values() {
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
        (VId(2), R, VId(3)),
    ];
    let mut b = builder(&["a", "b"]);
    b.any_shortest_walk("a", R, GlaDirection::Forward, "b", bounds(1, 3))
        .unwrap();
    let query = b.prepare("b", 0, None).unwrap().with_duplicates();
    assert!(query.plan().reads_edges());
    assert!(!query.plan().scans_edges());
    assert!(query.plan().operators().iter().any(|op| matches!(
        op,
        GlaOperator::VarLengthExpand {
            search: GraphWalkSearch::AnyShortest,
            ..
        }
    )));
    let rows = query
        .plan()
        .execute(vertices, edges, |_, _| Ok::<_, ()>(true))
        .unwrap();
    assert_eq!(
        rows,
        vec![VId(2), VId(3), VId(3)],
        "two distinct source pairs projecting vertex 3 are two occurrences"
    );
    assert_eq!(
        b.prepare("b", 0, None)
            .unwrap()
            .plan()
            .execute(vertices, edges, |_, _| Ok::<_, ()>(true))
            .unwrap(),
        vec![VId(2), VId(3)]
    );
    assert_eq!(
        b.prepare("b", 1, Some(2))
            .unwrap()
            .with_duplicates()
            .plan()
            .execute(
                vertices.into_iter().rev(),
                edges.into_iter().rev(),
                |_, _| Ok::<_, ()>(true)
            )
            .unwrap(),
        vec![VId(3), VId(3)]
    );

    let mut all = builder(&["a", "b"]);
    all.shortest_walk("a", R, GlaDirection::Forward, "b", bounds(1, 3))
        .unwrap();
    assert_eq!(
        all.prepare("b", 0, None)
            .unwrap()
            .with_duplicates()
            .plan()
            .execute(vertices, edges, |_, _| Ok::<_, ()>(true))
            .unwrap(),
        vec![
            VId(2),
            VId(2),
            VId(3),
            VId(3),
            VId(3),
            VId(3),
            VId(3),
            VId(3)
        ]
    );
}

#[test]
fn selector_transcripts_are_distinct_and_ordinary_tags_stay_frozen() {
    for (search, tag) in [
        (GraphWalkSearch::All, 22),
        (GraphWalkSearch::AllShortest, 23),
        (GraphWalkSearch::AnyShortest, 24),
    ] {
        let mut b = builder(&["a", "b"]);
        match search {
            GraphWalkSearch::All => b.walk("a", R, GlaDirection::Forward, "b", bounds(0, 3)),
            GraphWalkSearch::AllShortest => {
                b.shortest_walk("a", R, GlaDirection::Forward, "b", bounds(0, 3))
            }
            GraphWalkSearch::AnyShortest => {
                b.any_shortest_walk("a", R, GlaDirection::Forward, "b", bounds(0, 3))
            }
        }
        .unwrap();
        let query = b
            .prepare_bindings(&["a", "b"], 0, None)
            .unwrap()
            .with_duplicates();
        let mut expected = b"fgdb:bounded-gla:v1\0".to_vec();
        expected.extend_from_slice(&5_u64.to_be_bytes());
        expected.extend([1, tag]);
        expected.extend_from_slice(&0_u32.to_be_bytes());
        expected.extend_from_slice(&1_u64.to_be_bytes());
        expected.push(0);
        expected.extend_from_slice(&0_u32.to_be_bytes());
        expected.extend_from_slice(&3_u32.to_be_bytes());
        expected.push(10);
        expected.extend_from_slice(&2_u64.to_be_bytes());
        expected.extend_from_slice(&0_u32.to_be_bytes());
        expected.extend_from_slice(&1_u32.to_be_bytes());
        expected.extend([11, 9]);
        expected.extend_from_slice(&0_u64.to_be_bytes());
        expected.push(0);
        assert_eq!(query.canonical_bytes(), expected, "{search:?}");
    }
}

#[test]
fn reversed_binding_keeps_one_pair_per_incoming_outer_occurrence() {
    let mut b = builder(&["s", "b", "a"]);
    b.edge("s", S, GlaDirection::Forward, "b").unwrap();
    b.any_shortest_walk("a", R, GlaDirection::Forward, "b", bounds(1, 3))
        .unwrap();
    let query = b
        .prepare_bindings(&["s", "a"], 0, None)
        .unwrap()
        .with_duplicates();
    assert!(query.plan().operators().iter().any(|op| matches!(
        op,
        GlaOperator::VarLengthExpand {
            direction: GlaDirection::Reverse,
            search: GraphWalkSearch::AnyShortest,
            ..
        }
    )));
    let edges = [
        (VId(8), S, VId(3)),
        (VId(8), S, VId(3)),
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
        (VId(2), R, VId(3)),
    ];
    let rows = query
        .plan()
        .execute([VId(1), VId(2), VId(3), VId(8)], edges, |_, _| {
            Ok::<_, ()>(true)
        })
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.values().to_vec())
            .collect::<Vec<_>>(),
        vec![
            vec![VId(8), VId(1)],
            vec![VId(8), VId(1)],
            vec![VId(8), VId(2)],
            vec![VId(8), VId(2)]
        ]
    );
}

#[test]
fn optional_any_preserves_outer_duplicates_and_nulls_without_multiplying_ties() {
    let mut outer = builder(&["s", "a"]);
    outer.edge("s", S, GlaDirection::Forward, "a").unwrap();
    let mut inner = builder(&["a", "b"]);
    inner
        .any_shortest_walk("a", R, GlaDirection::Forward, "b", bounds(1, 2))
        .unwrap();
    let query = outer
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&inner)],
            &[GraphColumn::vertex("s", "s"), GraphColumn::vertex("b", "b")],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let edges = [
        (VId(0), S, VId(1)),
        (VId(0), S, VId(1)),
        (VId(8), S, VId(9)),
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
    ];
    let rows = query
        .plan()
        .execute_governed_with_properties(
            10,
            [VId(0), VId(1), VId(2), VId(8), VId(9)],
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let pairs = rows
        .value
        .iter()
        .map(|row| (row.values()[0].as_vertex(), row.values()[1].as_vertex()))
        .collect::<Vec<_>>();
    assert_eq!(
        pairs,
        vec![
            (Some(VId(0)), Some(VId(2))),
            (Some(VId(0)), Some(VId(2))),
            (Some(VId(8)), None)
        ]
    );
}

#[test]
fn endpoint_predicates_do_not_prune_transit_and_source_errors_are_not_absence() {
    let mut b = builder(&["a", "b"]);
    b.any_shortest_walk("a", R, GlaDirection::Forward, "b", bounds(2, 3))
        .unwrap();
    b.filter("b", VertexPredicate::HasLabel(LabelId(7)))
        .unwrap();
    let query = b
        .prepare_bindings(&["a", "b"], 0, None)
        .unwrap()
        .with_duplicates();
    let edges = [(VId(1), R, VId(2)), (VId(2), R, VId(3))];
    let rows = query
        .plan()
        .execute([VId(1), VId(2), VId(3)], edges, |vid, _| {
            Ok::<_, &str>(vid == VId(3))
        })
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.values().to_vec())
            .collect::<Vec<_>>(),
        vec![vec![VId(1), VId(3)]]
    );
    let result = query
        .plan()
        .execute([VId(1), VId(2), VId(3)], edges, |_, _| {
            Err::<bool, _>("unreadable endpoint")
        });
    assert!(matches!(result, Err("unreadable endpoint")));
    let before = query.canonical_bytes();
    assert_eq!(
        b.any_shortest_walk("a", R, GlaDirection::Forward, "missing", bounds(0, 2))
            .unwrap_err(),
        PatternBuildError::UnknownVariable
    );
    assert_eq!(
        b.prepare_bindings(&["a", "b"], 0, None)
            .unwrap()
            .with_duplicates()
            .canonical_bytes(),
        before
    );
}

#[test]
fn governed_any_handles_maximum_lower_bound_without_enumerating_exponential_ties() {
    let edges = [(VId(1), R, VId(1)); 8];
    let mut b = builder(&["a", "b"]);
    b.any_shortest_walk(
        "a",
        R,
        GlaDirection::Forward,
        "b",
        bounds(MAX_GRAPH_WALK_HOPS, MAX_GRAPH_WALK_HOPS),
    )
    .unwrap();
    let query = b.prepare("b", 0, None).unwrap().with_duplicates();
    let cap = GqlQueryPolicy::new(9, 1, 25_000, 5_000);
    let run = |policy| {
        query.plan().execute_governed(
            9,
            [VId(1)],
            edges,
            |_, _| Ok::<_, ()>(true),
            policy,
            || Ok::<_, usize>(()),
        )
    };
    let measured = run(cap).unwrap();
    assert_eq!(measured.value, vec![VId(1)]);
    let exact = GqlQueryPolicy::new(
        9,
        1,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    );
    assert_eq!(run(exact).unwrap(), measured);
    for short in [
        GqlQueryPolicy::new(8, 1, 25_000, 5_000),
        GqlQueryPolicy::new(9, 0, 25_000, 5_000),
        GqlQueryPolicy::new(9, 1, measured.evaluator.work_units - 1, 5_000),
        GqlQueryPolicy::new(9, 1, 25_000, measured.evaluator.scratch_entries - 1),
    ] {
        assert!(run(short).is_err());
    }
    let mut all = builder(&["a", "b"]);
    all.shortest_walk(
        "a",
        R,
        GlaDirection::Forward,
        "b",
        bounds(MAX_GRAPH_WALK_HOPS, MAX_GRAPH_WALK_HOPS),
    )
    .unwrap();
    assert!(matches!(
        all.prepare("b", 0, None).unwrap().plan().execute_governed(
            9,
            [VId(1)],
            edges,
            |_, _| Ok::<_, ()>(true),
            cap,
            || Ok::<_, usize>(())
        ),
        Err(GqlQueryError::Evaluator(_))
    ));
}

#[test]
fn any_plan_propagates_every_checkpoint_refusal_without_partial_results() {
    let mut b = builder(&["a", "b"]);
    b.any_shortest_walk("a", R, GlaDirection::Undirected, "b", bounds(1, 3))
        .unwrap();
    let query = b
        .prepare_bindings(&["a", "b"], 0, None)
        .unwrap()
        .with_duplicates();
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
    ];
    let mut calls = 0;
    let baseline = query
        .plan()
        .execute_governed(
            6,
            [VId(1), VId(2), VId(3)],
            edges,
            |_, _| Ok::<_, ()>(true),
            wide(),
            || {
                calls += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    assert!(!baseline.value.is_empty());
    for stop in 1..=calls {
        let mut seen = 0;
        let result = query.plan().execute_governed(
            6,
            [VId(1), VId(2), VId(3)],
            edges,
            |_, _| Ok::<_, ()>(true),
            wide(),
            || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
}
