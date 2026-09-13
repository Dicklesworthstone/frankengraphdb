//! Public compiler/executor tests for bounded WALK, independent of the cursor.
//! The oracle propagates integer multiplicities over raw edge occurrences.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GlaOperator, GraphBindingRow, GraphColumn, GraphMatchClause,
    GraphPatternBuilder, GraphValueRow, IntegerComparison, PatternBuildError,
    PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{
    GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphWalkBounds,
    MAX_GRAPH_WALK_HOPS, PreparedGraphAggregate,
};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
type Edge = (VId, RelationId, VId);

fn builder(names: &[&str]) -> GraphPatternBuilder {
    let mut result = GraphPatternBuilder::new();
    for name in names { result.vertex(name).unwrap(); }
    result
}
fn bounds(minimum: u32, maximum: u32) -> GraphWalkBounds {
    GraphWalkBounds::new(minimum, maximum).unwrap()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn tuples(rows: &[GraphBindingRow]) -> Vec<Vec<VId>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}
fn nullable(rows: &[GraphValueRow]) -> Vec<Vec<Option<VId>>> {
    rows.iter().map(|row| row.values().iter().map(|value| value.as_vertex()).collect()).collect()
}

// Matrix-layer oracle. No depth-first frames, GLA slots, deduplicated index,
// compiler control flow or query evaluator is shared with the implementation.
fn walks(source: VId, relation: RelationId, direction: GlaDirection,
    minimum: u32, maximum: u32, edges: &[Edge]) -> BTreeMap<VId, usize> {
    let mut layer = BTreeMap::from([(source, 1_usize)]);
    let mut answer = BTreeMap::<VId, usize>::new();
    for depth in 0..=maximum {
        if depth >= minimum {
            for (&vertex, &count) in &layer { *answer.entry(vertex).or_default() += count; }
        }
        let mut next = BTreeMap::<VId, usize>::new();
        for (vertex, count) in layer {
            for &(left, actual, right) in edges {
                if actual != relation { continue; }
                match direction {
                    GlaDirection::Forward if left == vertex => {
                        *next.entry(right).or_default() += count;
                    }
                    GlaDirection::Reverse if right == vertex => {
                        *next.entry(left).or_default() += count;
                    }
                    GlaDirection::Undirected => {
                        if left == vertex { *next.entry(right).or_default() += count; }
                        if right == vertex && left != right { *next.entry(left).or_default() += count; }
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
fn bounded_walk_multigraph_bags_and_pages_match_matrix_layers() {
    let vertices = [VId(0), VId(1), VId(2), VId(9)];
    let universe = [
        (VId(0), R, VId(0)), (VId(0), R, VId(1)), (VId(0), R, VId(1)),
        (VId(1), R, VId(0)), (VId(1), R, VId(2)), (VId(2), R, VId(2)),
        (VId(2), S, VId(9)),
    ];
    for mask in 0..128_usize {
        let edges: Vec<_> = universe.iter().enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0).map(|(_, edge)| *edge).collect();
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            for (minimum, maximum) in [(0, 0), (0, 2), (1, 1), (1, 3), (2, 3)] {
                let mut b = builder(&["a", "b"]);
                b.walk("a", R, direction, "b", bounds(minimum, maximum)).unwrap();
                let mut expected = Vec::new();
                for source in vertices {
                    for (target, count) in walks(source, R, direction, minimum, maximum, &edges) {
                        for _ in 0..count { expected.push(vec![source, target]); }
                    }
                }
                expected.sort();
                for distinct in [false, true] {
                    let mut answer = expected.clone();
                    if distinct { answer.dedup(); }
                    for (offset, count) in [(0, None), (1, Some(3)), (0, Some(0))] {
                        let query = b.prepare_bindings(&["a", "b"], offset, count).unwrap();
                        let query = if distinct { query } else { query.with_duplicates() };
                        assert!(!query.plan().scans_edges());
                        assert!(query.plan().reads_edges());
                        let actual = query.plan().execute(vertices, edges.iter().copied(),
                            |_, _| Ok::<_, ()>(true)).unwrap();
                        let page: Vec<_> = answer.iter().skip(offset as usize)
                            .take(count.unwrap_or(u64::MAX) as usize).cloned().collect();
                        assert_eq!(tuples(&actual), page,
                            "mask={mask}, {direction:?}, {minimum}..{maximum}, distinct={distinct}");
                        let reversed = query.plan().execute(vertices.into_iter().rev(), edges.iter().rev().copied(),
                            |_, _| Ok::<_, ()>(true)).unwrap();
                        assert_eq!(reversed, actual, "source iteration order is not result order");
                    }
                }
            }
        }
    }
}

#[test]
fn mixed_walk_atoms_close_cycles_and_reverse_binding_without_changing_multiplicity() {
    let vertices = [VId(0), VId(1), VId(2), VId(3), VId(4)];
    let edges = [
        (VId(0), S, VId(1)), (VId(0), S, VId(4)),
        (VId(1), R, VId(2)), (VId(1), R, VId(2)),
        (VId(2), R, VId(3)), (VId(3), R, VId(1)),
    ];
    let fixed = |left, right| edges.iter().filter(|&&(a, r, b)| a == left && r == S && b == right).count();
    let mut expected = Vec::new();
    for a in vertices { for b in vertices { for c in vertices { for d in vertices {
        let incoming = walks(c, R, GlaDirection::Forward, 1, 3, &edges);
        let outgoing = walks(c, R, GlaDirection::Forward, 0, 2, &edges);
        let count = fixed(a, b) * incoming.get(&b).copied().unwrap_or(0)
            * outgoing.get(&d).copied().unwrap_or(0) * fixed(a, d);
        for _ in 0..count { expected.push(vec![a, b, c, d]); }
    }}}}
    expected.sort();
    assert_eq!(expected.len(), 4);
    for order in [[0, 1, 2, 3], [1, 0, 2, 3], [3, 2, 1, 0]] {
        let mut b = builder(&["a", "b", "c", "d"]);
        for atom in order {
            match atom {
                0 => { b.edge("a", S, GlaDirection::Forward, "b").unwrap(); }
                1 => { b.walk("c", R, GlaDirection::Forward, "b", bounds(1, 3)).unwrap(); }
                2 => { b.walk("c", R, GlaDirection::Forward, "d", bounds(0, 2)).unwrap(); }
                _ => { b.edge("a", S, GlaDirection::Forward, "d").unwrap(); }
            }
        }
        let query = b.prepare_bindings(&["a", "b", "c", "d"], 0, None).unwrap().with_duplicates();
        assert!(matches!(query.plan().operators().first(), Some(GlaOperator::ScanVertices)));
        let actual = query.plan().execute(vertices, edges, |_, _| Ok::<_, ()>(true)).unwrap();
        assert_eq!(tuples(&actual), expected, "order={order:?}");
    }
}

#[test]
fn optional_walk_endpoints_do_not_rebind_null_and_probes_resolve_at_complete_witnesses() {
    let vertices = [VId(0), VId(1), VId(2), VId(3), VId(4)];
    let edges = [
        (VId(0), S, VId(1)), (VId(0), S, VId(4)),
        (VId(1), R, VId(2)), (VId(1), R, VId(2)),
        (VId(2), R, VId(3)), (VId(3), R, VId(1)),
    ];
    let mut root = builder(&["a", "anchor"]);
    root.edge("a", S, GlaDirection::Forward, "anchor").unwrap();
    let mut child = builder(&["anchor", "b"]);
    child.walk("anchor", R, GlaDirection::Forward, "b", bounds(1, 2)).unwrap();
    let mut zero = builder(&["b", "c"]);
    zero.walk("b", R, GlaDirection::Forward, "c", bounds(0, 0)).unwrap();
    let columns = [GraphColumn::vertex("anchor", "anchor"), GraphColumn::vertex("b", "b")];
    let run = |query: &PreparedGraphPattern<GraphValueRow>| {
        query.plan().execute_governed_with_properties(11, vertices, edges,
            |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap().value
    };
    let real = vec![vec![Some(VId(1)), Some(VId(2))], vec![Some(VId(1)), Some(VId(2))],
        vec![Some(VId(1)), Some(VId(3))], vec![Some(VId(1)), Some(VId(3))]];
    let mut all = real.clone();
    all.push(vec![Some(VId(4)), None]);
    for (last, expected) in [
        (GraphMatchClause::optional(&zero), all.clone()),
        (GraphMatchClause::exists(&zero), real),
        (GraphMatchClause::not_exists(&zero), vec![vec![Some(VId(4)), None]]),
    ] {
        let query = root.prepare_values_with_clauses(
            &[GraphMatchClause::optional(&child), last], &columns, 0, None,
        ).unwrap().with_duplicates();
        assert!(!query.plan().scans_edges());
        assert_eq!(nullable(&run(&query)), expected);
    }
    let mut backwards = builder(&["b", "anchor"]);
    backwards.walk("b", R, GlaDirection::Forward, "anchor", bounds(0, 2)).unwrap();
    let query = root.prepare_values_with_clauses(&[GraphMatchClause::optional(&backwards)],
        &columns, 0, None).unwrap().with_duplicates();
    assert_eq!(nullable(&run(&query)), vec![
        vec![Some(VId(1)), Some(VId(1))], vec![Some(VId(1)), Some(VId(2))],
        vec![Some(VId(1)), Some(VId(3))], vec![Some(VId(4)), Some(VId(4))],
    ]);
}

#[test]
fn endpoint_filters_do_not_remove_transit_vertices_and_late_errors_are_not_hidden_by_limit() {
    let key = PropertyKeyId(7);
    let mut b = builder(&["a", "b"]);
    b.walk("a", R, GlaDirection::Forward, "b", bounds(1, 3)).unwrap();
    b.filter("a", VertexPredicate::HasLabel(LabelId(1))).unwrap();
    b.filter("b", VertexPredicate::IntegerProperty {
        key, comparison: IntegerComparison::GreaterOrEqual, value: 3,
    }).unwrap();
    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let edges = [(VId(1), R, VId(2)), (VId(2), R, VId(3)), (VId(3), R, VId(4))];
    let query = b.prepare_bindings(&["a", "b"], 0, None).unwrap().with_duplicates();
    let actual = query.plan().execute(vertices, edges, |vid, predicates| {
        let labels = if vid == VId(1) { vec![LabelId(1)] } else { vec![] };
        Ok::<_, ()>(predicates.iter().all(|p| p.matches(&labels, &[(key, CanonicalScalar::Int(vid.0 as i64))])))
    }).unwrap();
    assert_eq!(tuples(&actual), vec![vec![VId(1), VId(3)], vec![VId(1), VId(4)]]);
    let mut b = builder(&["a", "b"]);
    b.walk("a", R, GlaDirection::Forward, "b", bounds(1, 2)).unwrap();
    let value = CanonicalScalar::Int(1);
    for count in [Some(0), Some(1), None] {
        let query = b.prepare_values(&[GraphColumn::property("value", "b", key)], 0, count).unwrap();
        let mut released = 0;
        let actual = query.plan().execute_with_properties_control(vertices, edges,
            |_, _| Ok::<_, &str>(true),
            |vid, _| if vid == VId(3) { Err("late walk property failure") } else { Ok(Some(&value)) },
            |event| { if event == fgdb_gql::GlaExecutionEvent::ResultRow { released += 1; } Ok(()) });
        assert_eq!(actual, Err("late walk property failure"));
        assert_eq!(released, 0);
    }
}

#[test]
fn path_work_scratch_and_result_limits_are_exact_and_every_checkpoint_can_interrupt() {
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [(VId(1), R, VId(2)), (VId(1), R, VId(2)), (VId(2), R, VId(1)), (VId(2), R, VId(3))];
    let mut b = builder(&["a", "b"]);
    b.walk("a", R, GlaDirection::Forward, "b", bounds(0, 3)).unwrap();
    let query = b.prepare_bindings(&["a", "b"], 1, Some(2)).unwrap().with_duplicates();
    let mut checkpoints = 0;
    let measured = query.plan().execute_governed(7, vertices, edges, |_, _| Ok::<_, ()>(true), wide(), || {
        checkpoints += 1; Ok::<_, usize>(())
    }).unwrap();
    assert_eq!(measured.value.len(), 2);
    let exact = GqlQueryPolicy::new(7, 2, measured.evaluator.work_units, measured.evaluator.scratch_entries);
    let repeated = query.plan().execute_governed(7, vertices, edges, |_, _| Ok::<_, ()>(true), exact, || Ok::<_, usize>(())).unwrap();
    assert_eq!(repeated, measured);
    for policy in [
        GqlQueryPolicy::new(6, 2, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(7, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(7, 2, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(7, 2, u64::MAX, measured.evaluator.scratch_entries - 1),
    ] {
        assert!(query.plan().execute_governed(7, vertices, edges, |_, _| Ok::<_, ()>(true), policy, || Ok::<_, usize>(())).is_err());
    }
    for stop in 1..=checkpoints {
        let mut seen = 0;
        let result = query.plan().execute_governed(7, vertices, edges, |_, _| Ok::<_, ()>(true), wide(), || {
            seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
}

#[test]
fn streaming_aggregates_count_walk_occurrences_not_just_reachable_endpoints() {
    let vertices = [VId(1), VId(2), VId(3), VId(9)];
    let edges = [(VId(1), R, VId(2)), (VId(1), R, VId(2)), (VId(2), R, VId(1)), (VId(2), R, VId(3))];
    let mut b = builder(&["a", "b"]);
    b.walk("a", R, GlaDirection::Forward, "b", bounds(0, 3)).unwrap();
    let child = b.prepare_values(&[GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")],
        0, None).unwrap().with_duplicates();
    let aggregate = PreparedGraphAggregate::prepare(child, &[0], &[
        GraphAggregate::count_rows("walks"), GraphAggregate::count_distinct("reachable", 1),
    ], 0, None).unwrap();
    let rows = aggregate.execute_governed(8, vertices, edges, |_, _| Ok::<_, ()>(true),
        |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap().value;
    assert_eq!(rows.len(), vertices.len());
    for row in rows {
        let source = row.keys()[0].as_vertex().unwrap();
        let expected = walks(source, R, GlaDirection::Forward, 0, 3, &edges);
        assert_eq!(row.values()[0].as_count(), Some(expected.values().sum::<usize>() as u64));
        assert_eq!(row.values()[1].as_count(), Some(expected.len() as u64));
    }
}

#[test]
fn walk_definitions_are_immutable_bounded_and_transcript_sensitive() {
    let mut b = builder(&["a", "b"]);
    b.walk("a", R, GlaDirection::Forward, "b", bounds(0, 2)).unwrap();
    let original = b.prepare_bindings(&["a", "b"], 0, None).unwrap();
    let frozen = original.canonical_bytes();
    assert_eq!(b.walk("a", R, GlaDirection::Forward, "unknown", bounds(0, 1)).unwrap_err(),
        PatternBuildError::UnknownVariable);
    assert_eq!(b.prepare_bindings(&["a", "b"], 0, None).unwrap(), original);
    let mut variants = BTreeSet::new();
    for (minimum, maximum) in [(0, 0), (0, 2), (1, 2), (1, 3)] {
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            let mut other = builder(&["x", "y"]);
            other.walk("x", R, direction, "y", bounds(minimum, maximum)).unwrap();
            let prepared = other.prepare_bindings(&["x", "y"], 0, None).unwrap();
            assert!(variants.insert(prepared.canonical_bytes()));
            if minimum == 0 && maximum == 2 && direction == GlaDirection::Forward {
                assert_eq!(prepared.canonical_bytes(), frozen);
            }
        }
    }
    b.identity("a", "b", true).unwrap();
    assert_eq!(original.canonical_bytes(), frozen);
    assert_ne!(b.prepare_bindings(&["a", "b"], 0, None).unwrap().canonical_bytes(), frozen);
    let mut maximum = builder(&["a", "b"]);
    maximum.walk("a", R, GlaDirection::Forward, "b", bounds(MAX_GRAPH_WALK_HOPS, MAX_GRAPH_WALK_HOPS)).unwrap();
    let rows = maximum.prepare_bindings(&["a", "b"], 0, None).unwrap().plan()
        .execute([VId(7)], [(VId(7), R, VId(7))], |_, _| Ok::<_, ()>(true)).unwrap();
    assert_eq!(tuples(&rows), vec![vec![VId(7), VId(7)]]);
}
