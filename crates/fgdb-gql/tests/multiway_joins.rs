//! Multiway candidate pruning compared with complete assignment enumeration.
//! Expected bags never use an index, a compiled plan, or another query.

use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphMatchClause, GraphPatternBuilder,
    VertexPredicate, MAX_PATTERN_EDGES};
use fgdb_gql::{GlaExecutionLimits, GqlQueryError, GqlQueryPolicy};
use fgdb_types::VId;

fn builder(names: &[&str]) -> GraphPatternBuilder {
    let mut builder = GraphPatternBuilder::new();
    for name in names { builder.vertex(name).unwrap(); }
    builder
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, u64::MAX, u64::MAX) }
fn oriented(source: VId, destination: VId, left: VId, right: VId, direction: GlaDirection) -> bool {
    match direction {
        GlaDirection::Forward => source == left && destination == right,
        GlaDirection::Reverse => source == right && destination == left,
        GlaDirection::Undirected => (source == left && destination == right) || (source == right && destination == left),
    }
}

#[test]
fn multiple_closing_constraints_match_independent_complete_assignment_bags() {
    let ids = [VId(0), VId(1_u128 << 100)];
    let universe = [
        (ids[0], RelationId(1), ids[1]), (ids[0], RelationId(1), ids[1]),
        (ids[1], RelationId(2), ids[0]), (ids[1], RelationId(2), ids[1]),
        (ids[0], RelationId(3), ids[0]), (ids[0], RelationId(3), ids[1]),
        (ids[0], RelationId(4), ids[0]), (ids[1], RelationId(4), ids[0]),
        (ids[1], RelationId(5), ids[0]), (ids[0], RelationId(5), ids[1]),
    ];
    use GlaDirection::{Forward as F, Reverse as R, Undirected as U};
    for mask in 0..1024_usize {
        let edges: Vec<_> = universe.iter().enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0).map(|(_, edge)| *edge).collect();
        for (directions, shapes) in [([F, F, F], [0, 1, 2]), ([U, R, F], [1, 0, 2]), ([R, U, U], [0, 1, 0])] {
            let names = ["a", "b", "c"];
            let mut b = builder(&names);
            let mut atoms = vec![(0, RelationId(1), F, 1), (1, RelationId(2), F, 2)];
            for (i, (direction, shape)) in directions.into_iter().zip(shapes).enumerate() {
                let (source, target) = match shape { 0 => (0, 2), 1 => (2, 0), _ => (1, 2) };
                atoms.push((source, RelationId(i as u64 + 3), direction, target));
            }
            for &(source, relation, direction, target) in &atoms {
                b.edge(names[source], relation, direction, names[target]).unwrap();
            }
            let mut expected = Vec::new();
            for a in ids { for mid in ids { for c in ids {
                let values = [a, mid, c];
                let multiplicity = atoms.iter().map(|&(left, relation, direction, right)| {
                    edges.iter().filter(|&&(source, r, destination)| r == relation
                        && oriented(source, destination, values[left], values[right], direction)).count()
                }).product::<usize>();
                expected.extend(std::iter::repeat_n(values.to_vec(), multiplicity));
            } } }
            expected.sort_unstable();
            let distinct = b.prepare_bindings(&names, 0, None).unwrap();
            let all = distinct.clone().with_duplicates();
            let transcript = all.canonical_bytes();
            let result = all.plan().execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true)).unwrap();
            assert_eq!(result.iter().map(|row| row.values().to_vec()).collect::<Vec<_>>(), expected);
            assert_eq!(all.canonical_bytes(), transcript);
            expected.dedup();
            let result = distinct.plan().execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true)).unwrap();
            assert_eq!(result.iter().map(|row| row.values().to_vec()).collect::<Vec<_>>(), expected);
        }
    }
}

#[test]
fn late_absence_is_pruned_before_parallel_closing_combinations_are_expanded() {
    let n = 128_u64;
    let mut b = builder(&["a", "b", "c"]);
    for (source, relation, destination) in [("a", 1, "b"), ("b", 2, "c"),
        ("a", 3, "c"), ("a", 3, "c"), ("b", 4, "c")] {
        b.edge(source, RelationId(relation), GlaDirection::Forward, destination).unwrap();
    }
    let mut edges = vec![(VId(0), RelationId(1), VId(1))];
    for i in 0..n {
        let candidate = VId(u128::from(i + 2));
        edges.push((VId(1), RelationId(2), candidate));
        for _ in 0..16 { edges.push((VId(0), RelationId(3), candidate)); }
        edges.push((VId(1), RelationId(4), VId(u128::from(n + i + 2))));
    }
    let query = b.prepare_bindings(&["a", "c"], 0, None).unwrap().with_duplicates();
    let result = query.plan().execute_with_limits([], edges, |_, _| Ok::<_, ()>(true),
        GlaExecutionLimits::new(1024 * n, u64::MAX)).unwrap();
    assert!(result.value.is_empty());
    assert!(result.stats.work_units <= 1024 * n);
}

#[test]
fn maximum_definition_chain_keeps_one_real_witness_and_one_output_shape() {
    let mut b = builder(&["a", "b", "c"]);
    b.edge("a", RelationId(1), GlaDirection::Forward, "b").unwrap();
    b.edge("b", RelationId(2), GlaDirection::Forward, "c").unwrap();
    for _ in 2..MAX_PATTERN_EDGES {
        b.edge("a", RelationId(3), GlaDirection::Forward, "c").unwrap();
    }
    let edges = [(VId(0), RelationId(1), VId(u128::MAX)),
        (VId(u128::MAX), RelationId(2), VId(5)), (VId(0), RelationId(3), VId(5))];
    let query = b.prepare_bindings(&["c", "a", "b"], 0, None).unwrap().with_duplicates();
    let result = query.plan().execute([], edges, |_, _| Ok::<_, ()>(true)).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].values(), &[VId(5), VId(0), VId(u128::MAX)]);
}

#[test]
fn multiway_scopes_preserve_optional_bags_and_existential_multiplicity() {
    let mut root = builder(&["a"]);
    root.filter("a", VertexPredicate::HasLabel(LabelId(1))).unwrap();
    let mut child = builder(&["a", "c"]);
    for (s, r, d) in [("a", 2, "c"), ("a", 3, "c"), ("c", 4, "a")] {
        child.edge(s, RelationId(r), GlaDirection::Forward, d).unwrap();
    }
    let mut edges = vec![(VId(0), RelationId(2), VId(9)); 2];
    edges.extend([(VId(0), RelationId(3), VId(9)); 2]);
    edges.extend([(VId(9), RelationId(4), VId(0)); 3]);
    // Owner 1 has pairwise partial matches but no complete optional witness.
    edges.extend([(VId(1), RelationId(2), VId(9)), (VId(1), RelationId(3), VId(9))]);
    let run = |clause, columns: &[GraphColumn<'_>]| {
        let query = root.prepare_values_with_clauses(&[clause], columns, 0, None).unwrap().with_duplicates();
        query.plan().execute_governed_with_properties(12, [VId(0), VId(1), VId(9)], edges.iter().copied(),
            |vid, _| Ok::<_, ()>(vid != VId(9)), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap().value
    };
    let columns = [GraphColumn::vertex("owner", "a"), GraphColumn::vertex("child", "c")];
    let optional = run(GraphMatchClause::optional(&child), &columns);
    assert_eq!(optional.len(), 13);
    assert_eq!(optional.iter().filter(|row| row.get(1).unwrap().is_null()).count(), 1);
    let exists = run(GraphMatchClause::exists(&child), &columns[..1]);
    assert_eq!(exists.len(), 1);
    assert_eq!(exists[0].get(0).unwrap().as_vertex(), Some(VId(0)));
    let absent = run(GraphMatchClause::not_exists(&child), &columns[..1]);
    assert_eq!(absent.len(), 1);
    assert_eq!(absent[0].get(0).unwrap().as_vertex(), Some(VId(1)));
}

#[test]
fn multiple_closings_share_exact_resource_limits_and_every_interruption_checkpoint() {
    let mut b = builder(&["a", "b", "c"]);
    for (s, r, d) in [("a", 1, "b"), ("b", 2, "c"), ("a", 3, "c"), ("c", 4, "a"), ("b", 5, "c")] {
        b.edge(s, RelationId(r), GlaDirection::Forward, d).unwrap();
    }
    let edges = [(VId(0), RelationId(1), VId(1)), (VId(1), RelationId(2), VId(2)),
        (VId(1), RelationId(2), VId(2)), (VId(0), RelationId(3), VId(2)),
        (VId(0), RelationId(3), VId(2)), (VId(2), RelationId(4), VId(0)),
        (VId(1), RelationId(5), VId(2))];
    let query = b.prepare_bindings(&["a", "c"], 0, None).unwrap().with_duplicates();
    let mut total = 0;
    let measured = query.plan().execute_governed(7, [], edges, |_, _| Ok::<_, ()>(true), wide(), || {
        total += 1; Ok::<_, usize>(())
    }).unwrap();
    assert_eq!(measured.value.len(), 4);
    let exact = GqlQueryPolicy::new(7, 4, measured.evaluator.work_units, measured.evaluator.scratch_entries);
    assert_eq!(query.plan().execute_governed(7, [], edges, |_, _| Ok::<_, ()>(true), exact,
        || Ok::<_, usize>(())).unwrap(), measured);
    for cap in [GqlQueryPolicy::new(7, 4, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(7, 4, u64::MAX, measured.evaluator.scratch_entries - 1)] {
        assert!(matches!(query.plan().execute_governed(7, [], edges, |_, _| Ok::<_, ()>(true), cap,
            || Ok::<_, usize>(())), Err(GqlQueryError::Evaluator(_))));
    }
    for stop in 1..=total {
        let mut at = 0;
        let result = query.plan().execute_governed(7, [], edges, |_, _| Ok::<_, ()>(true), wide(), || {
            at += 1; if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(found)) if found == stop));
        assert_eq!(at, stop);
    }
}
