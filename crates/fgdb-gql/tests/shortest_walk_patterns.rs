//! ALL SHORTEST WALK is a real GLA search, not a post-filter over ordinary WALK.
//! Expected multiplicities come from independent, unpruned matrix layers.

use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GlaOperator, GraphColumn, GraphMatchClause, GraphPatternBuilder, GraphWalkSearch,
    PatternBuildError, VertexPredicate,
};
use fgdb_gql::{
    GlaExecutionError, GlaExecutionLimits, GqlQueryError, GqlQueryPolicy, GraphWalkBounds,
    MAX_GRAPH_WALK_HOPS,
};
use fgdb_types::VId;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
type Edge = (VId, RelationId, VId);

fn builder(names: &[&str]) -> GraphPatternBuilder {
    let mut result = GraphPatternBuilder::new();
    for name in names {
        result.vertex(name).unwrap();
    }
    result
}
fn bounds(minimum: u32, maximum: u32) -> GraphWalkBounds {
    GraphWalkBounds::new(minimum, maximum).unwrap()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

// Propagate EVERY walk through raw edge occurrences at EVERY depth. Select the
// first admissible layer separately for each endpoint, without pruning transit.
fn oracle(
    source: VId,
    direction: GlaDirection,
    minimum: u32,
    maximum: u32,
    edges: &[Edge],
) -> BTreeMap<VId, usize> {
    let mut layer = BTreeMap::from([(source, 1_usize)]);
    let mut answer = BTreeMap::new();
    for depth in 0..=maximum {
        if depth >= minimum {
            for (&vertex, &count) in &layer {
                answer.entry(vertex).or_insert(count);
            }
        }
        let mut next = BTreeMap::<VId, usize>::new();
        for (vertex, count) in layer {
            for &(left, relation, right) in edges {
                if relation != R {
                    continue;
                }
                match direction {
                    GlaDirection::Forward if left == vertex => {
                        *next.entry(right).or_default() += count
                    }
                    GlaDirection::Reverse if right == vertex => {
                        *next.entry(left).or_default() += count
                    }
                    GlaDirection::Undirected => {
                        if left == vertex {
                            *next.entry(right).or_default() += count;
                        }
                        if right == vertex && left != right {
                            *next.entry(left).or_default() += count;
                        }
                    }
                    _ => {}
                }
            }
        }
        layer = next;
    }
    answer
}

#[test]
fn multigraph_shortest_bags_distinct_and_pages_match_unpruned_layers() {
    let vertices = [VId(0), VId(1), VId(2), VId(9)];
    let universe = [
        (VId(0), R, VId(0)),
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(1), R, VId(0)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(2)),
        (VId(2), S, VId(9)),
    ];
    for mask in 0..128_usize {
        let edges = universe
            .iter()
            .enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, edge)| *edge)
            .collect::<Vec<_>>();
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for (minimum, maximum) in [(0, 0), (0, 2), (1, 1), (1, 3), (2, 3), (3, 4)] {
                let mut b = builder(&["a", "b"]);
                b.shortest_walk("a", R, direction, "b", bounds(minimum, maximum))
                    .unwrap();
                let mut expected = Vec::new();
                for source in vertices {
                    for (target, count) in oracle(source, direction, minimum, maximum, &edges) {
                        for _ in 0..count {
                            expected.push(vec![source, target]);
                        }
                    }
                }
                expected.sort();
                for distinct in [false, true] {
                    let mut expected = expected.clone();
                    if distinct {
                        expected.dedup();
                    }
                    for (offset, count) in [(0, None), (1, Some(3)), (0, Some(0))] {
                        let query = b.prepare_bindings(&["a", "b"], offset, count).unwrap();
                        let query = if distinct {
                            query
                        } else {
                            query.with_duplicates()
                        };
                        assert!(!query.plan().scans_edges());
                        assert!(query.plan().reads_edges());
                        let actual = query
                            .plan()
                            .execute(vertices, edges.iter().copied(), |_, _| Ok::<_, ()>(true))
                            .unwrap();
                        let page = expected
                            .iter()
                            .skip(offset as usize)
                            .take(count.unwrap_or(u64::MAX) as usize)
                            .cloned()
                            .collect::<Vec<_>>();
                        assert_eq!(
                            actual
                                .iter()
                                .map(|row| row.values().to_vec())
                                .collect::<Vec<_>>(),
                            page,
                            "mask={mask}, {direction:?}, {minimum}..{maximum}, distinct={distinct}"
                        );
                        let reversed = query
                            .plan()
                            .execute(
                                vertices.into_iter().rev(),
                                edges.iter().rev().copied(),
                                |_, _| Ok::<_, ()>(true),
                            )
                            .unwrap();
                        assert_eq!(
                            actual, reversed,
                            "input iteration order must not select a tie"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn ordinary_walk_bytes_are_frozen_and_shortest_has_a_distinct_tag() {
    let make = |shortest, names: [&str; 2]| {
        let mut b = builder(&names);
        if shortest {
            b.shortest_walk(names[0], R, GlaDirection::Forward, names[1], bounds(1, 3))
                .unwrap();
        } else {
            b.walk(names[0], R, GlaDirection::Forward, names[1], bounds(1, 3))
                .unwrap();
        }
        b.prepare_bindings(&names, 0, None)
            .unwrap()
            .with_duplicates()
    };
    let transcript = |tag| {
        let mut bytes = b"fgdb:bounded-gla:v1\0".to_vec();
        bytes.extend_from_slice(&5_u64.to_be_bytes());
        bytes.push(1); // ScanVertices
        bytes.push(tag);
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&R.0.to_be_bytes());
        bytes.push(0); // Forward
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&3_u32.to_be_bytes());
        bytes.push(10); // ProjectBindings
        bytes.extend_from_slice(&2_u64.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.push(11); // OrderByBindings
        bytes.push(9); // Limit
        bytes.extend_from_slice(&0_u64.to_be_bytes());
        bytes.push(0); // no count
        bytes
    };
    let all = make(false, ["a", "b"]);
    let shortest = make(true, ["a", "b"]);
    assert_eq!(all.canonical_bytes(), transcript(22));
    assert_eq!(shortest.canonical_bytes(), transcript(23));
    assert_ne!(all.canonical_bytes(), shortest.canonical_bytes());
    assert_eq!(
        shortest.canonical_bytes(),
        make(true, ["x", "y"]).canonical_bytes()
    );
    assert!(matches!(
        shortest.plan().operators()[1],
        GlaOperator::VarLengthExpand {
            search: GraphWalkSearch::AllShortest,
            ..
        }
    ));
}

#[test]
fn shortest_atom_can_expand_from_its_already_bound_destination() {
    let vertices = [VId(0), VId(1), VId(2), VId(3)];
    let edges = [
        (VId(0), S, VId(3)),
        (VId(0), S, VId(3)),
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(1), R, VId(3)),
        (VId(0), R, VId(2)),
        (VId(2), R, VId(3)),
        (VId(3), R, VId(3)),
    ];
    let mut b = builder(&["p", "b", "a"]);
    b.edge("p", S, GlaDirection::Forward, "b").unwrap();
    b.shortest_walk("a", R, GlaDirection::Forward, "b", bounds(1, 4))
        .unwrap();
    let query = b
        .prepare_bindings(&["p", "a", "b"], 0, None)
        .unwrap()
        .with_duplicates();
    assert!(query.plan().operators().iter().any(|op| matches!(
        op,
        GlaOperator::VarLengthExpand {
            direction: GlaDirection::Reverse,
            search: GraphWalkSearch::AllShortest,
            ..
        }
    )));
    let mut expected = Vec::new();
    for &(p, relation, target) in &edges {
        if relation != S {
            continue;
        }
        for source in vertices {
            let count = oracle(source, GlaDirection::Forward, 1, 4, &edges)
                .get(&target)
                .copied()
                .unwrap_or(0);
            for _ in 0..count {
                expected.push(vec![p, source, target]);
            }
        }
    }
    expected.sort();
    let rows = query
        .plan()
        .execute(vertices, edges, |_, _| Ok::<_, ()>(true))
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.values().to_vec())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn optional_and_existence_scopes_keep_shortest_mode_and_real_null_absence() {
    let vertices = [VId(0), VId(1), VId(2), VId(9)];
    let edges = [
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(2)),
    ];
    let outer = builder(&["b"]);
    let mut child = builder(&["a", "b"]);
    child
        .shortest_walk("a", R, GlaDirection::Forward, "b", bounds(1, 3))
        .unwrap();
    let query = outer
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&child)],
            &[GraphColumn::vertex("b", "b"), GraphColumn::vertex("a", "a")],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let rows = query
        .plan()
        .execute_with_properties_control(
            vertices,
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            |_| Ok(()),
        )
        .unwrap();
    let mut actual = rows
        .iter()
        .map(|row| {
            row.values()
                .iter()
                .map(|v| v.as_vertex())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut expected = Vec::new();
    let mut present = Vec::new();
    let mut absent = Vec::new();
    for target in vertices {
        let matches = oracle(target, GlaDirection::Reverse, 1, 3, &edges);
        if matches.is_empty() {
            expected.push(vec![Some(target), None]);
            absent.push(target);
        } else {
            present.push(target);
            for (source, count) in matches {
                for _ in 0..count {
                    expected.push(vec![Some(target), Some(source)]);
                }
            }
        }
    }
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    for (clause, expected) in [
        (GraphMatchClause::exists(&child), present),
        (GraphMatchClause::not_exists(&child), absent),
    ] {
        let query = outer
            .prepare_values_with_clauses(&[clause], &[GraphColumn::vertex("b", "b")], 0, None)
            .unwrap()
            .with_duplicates();
        let rows = query
            .plan()
            .execute_with_properties_control(
                vertices,
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.values()[0].as_vertex().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn endpoint_filters_do_not_remove_transit_vertices_or_swallow_source_errors() {
    let mut b = builder(&["a", "b"]);
    b.shortest_walk("a", R, GlaDirection::Forward, "b", bounds(1, 3))
        .unwrap();
    b.filter("a", VertexPredicate::HasLabel(LabelId(1)))
        .unwrap();
    b.filter("b", VertexPredicate::HasLabel(LabelId(2)))
        .unwrap();
    let query = b
        .prepare_bindings(&["a", "b"], 0, None)
        .unwrap()
        .with_duplicates();
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [(VId(1), R, VId(2)), (VId(2), R, VId(3))];
    let rows = query
        .plan()
        .execute(vertices, edges, |vid, predicates| {
            let labels = if vid == VId(1) {
                vec![LabelId(1)]
            } else if vid == VId(3) {
                vec![LabelId(2)]
            } else {
                vec![]
            };
            Ok::<_, ()>(
                predicates
                    .iter()
                    .all(|predicate| predicate.matches(&labels, &[])),
            )
        })
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.values().to_vec())
            .collect::<Vec<_>>(),
        vec![vec![VId(1), VId(3)]]
    );
    let failure = query.plan().execute(vertices, edges, |vid, _| {
        if vid == VId(3) {
            Err("unreadable endpoint")
        } else {
            Ok(true)
        }
    });
    assert_eq!(failure, Err("unreadable endpoint"));
}

#[test]
fn exact_resource_limits_and_every_checkpoint_are_enforced_by_the_real_executor() {
    let mut b = builder(&["a", "b"]);
    b.shortest_walk("a", R, GlaDirection::Forward, "b", bounds(1, 3))
        .unwrap();
    let query = b
        .prepare_bindings(&["a", "b"], 0, None)
        .unwrap()
        .with_duplicates();
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
        (VId(3), R, VId(1)),
    ];
    let mut checkpoints = 0;
    let measured = query
        .plan()
        .execute_governed(
            7,
            vertices,
            edges,
            |_, _| Ok::<_, ()>(true),
            wide(),
            || {
                checkpoints += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    let caps = [
        7,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    let exact = GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]);
    let rerun = query
        .plan()
        .execute_governed(
            7,
            vertices,
            edges,
            |_, _| Ok::<_, ()>(true),
            exact,
            || Ok::<_, usize>(()),
        )
        .unwrap();
    assert_eq!(rerun, measured);
    for dimension in 0..4 {
        let mut limited = caps;
        limited[dimension] -= 1;
        let result = query.plan().execute_governed(
            7,
            vertices,
            edges,
            |_, _| Ok::<_, ()>(true),
            GqlQueryPolicy::new(limited[0], limited[1], limited[2], limited[3]),
            || Ok::<_, usize>(()),
        );
        assert!(
            result.is_err(),
            "dimension {dimension} must refuse, not return a partial bag"
        );
    }
    for stop in 1..=checkpoints {
        let mut seen = 0;
        let result = query.plan().execute_governed(
            7,
            vertices,
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

#[test]
fn shortest_execution_prunes_cycles_instead_of_enumerating_then_filtering() {
    let mut all = builder(&["a", "b"]);
    all.walk(
        "a",
        R,
        GlaDirection::Forward,
        "b",
        bounds(1, MAX_GRAPH_WALK_HOPS),
    )
    .unwrap();
    let mut shortest = builder(&["a", "b"]);
    shortest
        .shortest_walk(
            "a",
            R,
            GlaDirection::Forward,
            "b",
            bounds(1, MAX_GRAPH_WALK_HOPS),
        )
        .unwrap();
    let all = all.prepare("b", 0, None).unwrap().with_duplicates();
    let shortest = shortest.prepare("b", 0, None).unwrap().with_duplicates();
    let edges = [(VId(1), R, VId(1)), (VId(1), R, VId(1))];
    let limits = GlaExecutionLimits::new(1_000, 1_000);
    let result = shortest
        .plan()
        .execute_with_limits([VId(1)], edges, |_, _| Ok::<_, ()>(true), limits)
        .unwrap();
    assert_eq!(result.value, vec![VId(1), VId(1)]);
    assert!(matches!(
        all.plan()
            .execute_with_limits([VId(1)], edges, |_, _| Ok::<_, ()>(true), limits),
        Err(GlaExecutionError::Limit(_))
    ));
}

#[test]
fn rejected_shortest_atom_does_not_change_an_existing_builder() {
    let mut b = builder(&["a", "b"]);
    b.walk("a", R, GlaDirection::Forward, "b", bounds(0, 0))
        .unwrap();
    let before = b.prepare_bindings(&["a", "b"], 0, None).unwrap();
    assert_eq!(
        b.shortest_walk("a", R, GlaDirection::Forward, "missing", bounds(1, 3))
            .unwrap_err(),
        PatternBuildError::UnknownVariable
    );
    assert_eq!(b.prepare_bindings(&["a", "b"], 0, None).unwrap(), before);
    for _ in 1..fgdb_gql::algebra::MAX_PATTERN_EDGES {
        b.shortest_walk("a", R, GlaDirection::Forward, "b", bounds(1, 1))
            .unwrap();
    }
    let before = b.prepare("a", 0, None).unwrap().canonical_bytes();
    assert!(
        b.shortest_walk("a", R, GlaDirection::Forward, "b", bounds(1, 1))
            .is_err()
    );
    assert_eq!(b.prepare("a", 0, None).unwrap().canonical_bytes(), before);
}
