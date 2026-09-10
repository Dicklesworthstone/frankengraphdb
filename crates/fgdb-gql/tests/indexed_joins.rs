//! Independent bag semantics and operation bounds for physical adjacency joins.
//! These tests do not derive expected rows from GLA or another prepared query.
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphMatchClause, GraphPatternBuilder, VertexPredicate};
use fgdb_gql::{GlaExecutionLimits, GqlQueryError, GqlQueryPolicy};
use fgdb_types::VId;

fn builder(names: &[&str]) -> GraphPatternBuilder {
    let mut result = GraphPatternBuilder::new();
    for name in names { result.vertex(name).unwrap(); }
    result
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, u64::MAX, u64::MAX) }
fn oriented(left: VId, right: VId, source: VId, destination: VId, direction: GlaDirection) -> bool {
    match direction {
        GlaDirection::Forward => source == left && destination == right,
        GlaDirection::Reverse => source == right && destination == left,
        GlaDirection::Undirected => (source == left && destination == right) || (source == right && destination == left),
    }
}

#[test]
fn cyclic_and_fan_in_bags_match_complete_assignment_multiplicities() {
    let ids = [VId(0), VId(1_u128 << 100)];
    let universe = [
        (ids[0], RelationId(1), ids[1]), (ids[0], RelationId(1), ids[1]),
        (ids[1], RelationId(1), ids[0]), (ids[0], RelationId(1), ids[0]),
        (ids[1], RelationId(2), ids[0]), (ids[0], RelationId(2), ids[1]),
        (ids[0], RelationId(3), ids[1]), (ids[1], RelationId(3), ids[0]),
    ];
    for mask in 0..256_usize {
        let edges: Vec<_> = universe.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, edge)| *edge).collect();
        for first_direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            for last_direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
                for fan_in in [false, true] {
                    let mut b = builder(&["a", "b", "c"]);
                    b.edge("a", RelationId(1), first_direction, "b").unwrap();
                    b.edge("b", RelationId(2), GlaDirection::Forward, "c").unwrap();
                    b.edge(if fan_in { "a" } else { "c" }, RelationId(3), last_direction,
                        if fan_in { "c" } else { "a" }).unwrap();
                    let mut expected = Vec::new();
                    for a in ids { for mid in ids { for c in ids {
                        let count = |relation, left, right, direction| edges.iter().filter(|&&(s, r, d)|
                            r == relation && oriented(left, right, s, d, direction)).count();
                        let paths = count(RelationId(1), a, mid, first_direction)
                            * count(RelationId(2), mid, c, GlaDirection::Forward)
                            * count(RelationId(3), if fan_in { a } else { c }, if fan_in { c } else { a }, last_direction);
                        expected.extend(std::iter::repeat_n(vec![a, mid, c], paths));
                    } } }
                    expected.sort_unstable();
                    let distinct = b.prepare_bindings(&["a", "b", "c"], 0, None).unwrap();
                    let all = distinct.clone().with_duplicates();
                    let before = all.canonical_bytes();
                    let actual = all.plan().execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true)).unwrap();
                    assert_eq!(actual.iter().map(|row| row.values().to_vec()).collect::<Vec<_>>(), expected);
                    assert_eq!(all.canonical_bytes(), before);
                    expected.dedup();
                    let actual = distinct.plan().execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true)).unwrap();
                    assert_eq!(actual.iter().map(|row| row.values().to_vec()).collect::<Vec<_>>(), expected);
                }
            }
        }
    }
}

#[test]
fn root_self_loop_seeks_keep_every_parallel_occurrence() {
    let mut b = builder(&["a"]);
    b.edge("a", RelationId(1), GlaDirection::Undirected, "a").unwrap();
    let edges = [(VId(0), RelationId(1), VId(9)), (VId(0), RelationId(1), VId(0)),
        (VId(0), RelationId(1), VId(0)), (VId(u128::MAX), RelationId(1), VId(u128::MAX))];
    assert_eq!(b.prepare("a", 0, None).unwrap().with_duplicates().plan()
        .execute([], edges, |_, _| Ok::<_, ()>(true)).unwrap(), vec![VId(0), VId(0), VId(u128::MAX)]);
}

#[test]
fn disjoint_fan_in_finishes_inside_a_linearithmic_work_allowance() {
    let n = 1024_u64;
    let mut b = builder(&["a", "b", "c"]);
    b.edge("a", RelationId(1), GlaDirection::Forward, "b").unwrap();
    b.edge("b", RelationId(2), GlaDirection::Forward, "c").unwrap();
    b.edge("a", RelationId(3), GlaDirection::Forward, "c").unwrap();
    let mut edges = vec![(VId(0), RelationId(1), VId(1))];
    for i in 0..n {
        edges.push((VId(1), RelationId(2), VId(u128::from(i + 2))));
        edges.push((VId(0), RelationId(3), VId(u128::from(i + n + 2))));
    }
    let pattern = b.prepare_bindings(&["a", "c"], 0, None).unwrap().with_duplicates();
    let execution = pattern.plan().execute_with_limits([], edges, |_, _| Ok::<_, ()>(true),
        GlaExecutionLimits::new(128 * n, u64::MAX)).unwrap();
    assert!(execution.value.is_empty());
    assert!(execution.stats.work_units <= 128 * n);
}

#[test]
fn intersection_never_skips_an_intervening_fallible_property_selection() {
    let mut b = builder(&["a", "b", "c"]);
    b.edge("a", RelationId(1), GlaDirection::Forward, "b").unwrap();
    b.edge("b", RelationId(2), GlaDirection::Forward, "c").unwrap();
    b.edge("c", RelationId(3), GlaDirection::Forward, "a").unwrap();
    b.filter("c", VertexPredicate::HasLabel(LabelId(1))).unwrap();
    let query = b.prepare("c", 0, None).unwrap();
    let result = query.plan().execute([], [
        (VId(0), RelationId(1), VId(1)), (VId(1), RelationId(2), VId(2)),
        (VId(9), RelationId(3), VId(0)),
    ], |vid, _| {
        assert_eq!(vid, VId(2));
        Err::<bool, _>("source error before the closing constraint")
    });
    assert_eq!(result, Err("source error before the closing constraint"));
}

#[test]
fn property_pair_selection_remains_a_fallible_barrier_to_intersection() {
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
    use fgdb_types::CanonicalScalar;

    let query = PreparedGraphText::prepare(
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) WHERE c.p = a.p RETURN a",
        |kind, name| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
            (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(RelationId(3))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        },
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let mut reads = 0;
    let result = query.plan().execute_governed_with_properties(
        3,
        [],
        [
            (VId(0), RelationId(1), VId(1)),
            (VId(1), RelationId(2), VId(2)),
            (VId(9), RelationId(3), VId(0)),
        ],
        |_, _| Ok(true),
        |_, _| {
            reads += 1;
            Err::<Option<&CanonicalScalar>, _>("property-pair source failed")
        },
        wide(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(result, Err(GqlQueryError::Source("property-pair source failed"))));
    assert_eq!(reads, 1, "a missing closing edge cannot suppress the earlier read");
}

#[test]
fn optional_semi_and_anti_clauses_keep_scope_and_bag_semantics() {
    let mut root = builder(&["a"]);
    root.filter("a", VertexPredicate::HasLabel(LabelId(1))).unwrap();
    let mut child = builder(&["a", "c"]);
    child.edge("a", RelationId(2), GlaDirection::Forward, "c").unwrap();
    child.edge("c", RelationId(3), GlaDirection::Forward, "a").unwrap();
    let edges = [(VId(1), RelationId(2), VId(3)), (VId(1), RelationId(2), VId(3)),
        (VId(3), RelationId(3), VId(1)), (VId(3), RelationId(3), VId(1)), (VId(3), RelationId(3), VId(1))];
    let run = |clause, columns: &[GraphColumn<'_>]| {
        let query = root.prepare_values_with_clauses(&[clause], columns, 0, None).unwrap().with_duplicates();
        query.plan().execute_governed_with_properties(8, [VId(1), VId(2), VId(3)], edges,
            |vid, _| Ok::<_, ()>(vid != VId(3)), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap().value
    };
    let columns = [GraphColumn::vertex("a", "a"), GraphColumn::vertex("c", "c")];
    let optional = run(GraphMatchClause::optional(&child), &columns);
    assert_eq!(optional.len(), 7);
    assert_eq!(optional.iter().filter(|row| row.get(1).unwrap().is_null()).count(), 1);
    let existence = run(GraphMatchClause::exists(&child), &columns[..1]);
    assert_eq!(existence.len(), 1);
    assert_eq!(existence[0].get(0).unwrap().as_vertex(), Some(VId(1)));
    let anti = run(GraphMatchClause::not_exists(&child), &columns[..1]);
    assert_eq!(anti.len(), 1);
    assert_eq!(anti[0].get(0).unwrap().as_vertex(), Some(VId(2)));
}

#[test]
fn indexed_cycles_share_exact_limits_and_every_interruption_checkpoint() {
    let mut b = builder(&["a", "b", "c"]);
    b.edge("a", RelationId(1), GlaDirection::Forward, "b").unwrap();
    b.edge("b", RelationId(2), GlaDirection::Forward, "c").unwrap();
    b.edge("c", RelationId(3), GlaDirection::Forward, "a").unwrap();
    let query = b.prepare_bindings(&["a", "c"], 0, None).unwrap().with_duplicates();
    let edges = [(VId(0), RelationId(1), VId(1)), (VId(1), RelationId(2), VId(2)),
        (VId(1), RelationId(2), VId(3)), (VId(3), RelationId(3), VId(0)),
        (VId(3), RelationId(3), VId(0))];
    let mut total = 0;
    let measured = query.plan().execute_governed(5, [], edges, |_, _| Ok::<_, ()>(true), wide(), || {
        total += 1; Ok::<_, usize>(())
    }).unwrap();
    assert_eq!(measured.value.len(), 2);
    let exact = GqlQueryPolicy::new(5, 2, measured.evaluator.work_units, measured.evaluator.scratch_entries);
    assert_eq!(query.plan().execute_governed(5, [], edges, |_, _| Ok::<_, ()>(true), exact,
        || Ok::<_, usize>(())).unwrap(), measured);
    for cap in [GqlQueryPolicy::new(5, 2, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(5, 2, u64::MAX, measured.evaluator.scratch_entries - 1)] {
        assert!(matches!(query.plan().execute_governed(5, [], edges, |_, _| Ok::<_, ()>(true), cap,
            || Ok::<_, usize>(())), Err(GqlQueryError::Evaluator(_))));
    }
    for stop in 1..=total {
        let mut at = 0;
        let result = query.plan().execute_governed(5, [], edges, |_, _| Ok::<_, ()>(true), wide(), || {
            at += 1; if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
}
