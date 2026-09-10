//! Cross-binding predicates run inside the one GLA visitor, not after output.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphMatchClause, GraphPatternBuilder,
    GraphValueRow, IntegerComparison, MAX_PATTERN_PREDICATES, PatternBuildError, VertexPredicate};
use fgdb_gql::{GraphAggregate, GqlQueryError, GqlQueryPolicy, PreparedGraphAggregate};
use fgdb_types::{CanonicalScalar, VId};

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const HIGH: VId = VId((1_u128 << 100) + 3);
const IDS: [VId; 3] = [VId(0), VId(1), HIGH];
const OPS: [IntegerComparison; 6] = [IntegerComparison::Equal, IntegerComparison::NotEqual,
    IntegerComparison::Less, IntegerComparison::Greater,
    IntegerComparison::LessOrEqual, IntegerComparison::GreaterOrEqual];

fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 10000, u64::MAX, u64::MAX) }
fn builder(names: &[&str]) -> GraphPatternBuilder {
    let mut b = GraphPatternBuilder::new();
    for name in names { b.vertex(name).unwrap(); }
    b
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<Option<VId>>> {
    rows.iter().map(|row| row.values().iter().map(|cell| cell.as_vertex()).collect()).collect()
}
fn position(vid: VId) -> usize { IDS.iter().position(|id| *id == vid).unwrap() }
fn expected(left: Option<&CanonicalScalar>, right: Option<&CanonicalScalar>, op: IntegerComparison) -> bool {
    // Independent nullable-integer truth table, not the production predicate.
    let (Some(CanonicalScalar::Int(left)), Some(CanonicalScalar::Int(right))) = (left, right) else { return false; };
    match op {
        IntegerComparison::Equal => left == right, IntegerComparison::NotEqual => left != right,
        IntegerComparison::Less => left < right, IntegerComparison::Greater => left > right,
        IntegerComparison::LessOrEqual => left <= right, IntegerComparison::GreaterOrEqual => left >= right,
    }
}
fn oriented(s: VId, d: VId, direction: GlaDirection) -> Vec<(VId, VId)> {
    match direction {
        GlaDirection::Forward => vec![(s, d)], GlaDirection::Reverse => vec![(d, s)],
        GlaDirection::Undirected if s != d => vec![(s, d), (d, s)],
        GlaDirection::Undirected => vec![(s, d)],
    }
}

#[test]
fn all_comparisons_match_independent_multigraph_path_enumeration() {
    let left = [Some(CanonicalScalar::Int(i64::MIN)), Some(CanonicalScalar::Int(7)), None];
    let right = [Some(CanonicalScalar::Int(7)), Some(CanonicalScalar::Int(i64::MAX)), Some(CanonicalScalar::Null)];
    let universe = [(IDS[0], R, IDS[1]), (IDS[0], R, IDS[1]), (IDS[1], R, IDS[0]),
        (IDS[1], S, IDS[0]), (IDS[1], S, IDS[1]), (IDS[0], S, IDS[2])];
    for mask in 0..64 {
        let edges: Vec<_> = universe.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, edge)| *edge).collect();
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            for comparison in OPS {
                let mut b = builder(&["c", "a", "b"]);
                b.edge("a", R, direction, "b").unwrap();
                b.edge("b", S, direction, "c").unwrap();
                b.compare_properties("a", P, comparison, "c", Q).unwrap();
                let query = b.prepare_values(&[GraphColumn::vertex("owner", "a"), GraphColumn::vertex("last", "c")], 0, None)
                    .unwrap().with_duplicates();
                let mut wanted = Vec::new();
                for &(s, r, d) in &edges {
                    if r != R { continue; }
                    for (a, via) in oriented(s, d, direction) {
                        for &(s2, r2, d2) in &edges {
                            if r2 != S { continue; }
                            for (b, c) in oriented(s2, d2, direction) {
                                if via == b && expected(left[position(a)].as_ref(), right[position(c)].as_ref(), comparison) {
                                    wanted.push(vec![Some(a), Some(c)]);
                                }
                            }
                        }
                    }
                }
                wanted.sort();
                let actual = query.plan().execute_governed_with_properties(edges.len() as u64, [], edges.iter().copied(),
                    |_, _| Ok::<_, ()>(true), |vid, key| Ok(if key == P { left[position(vid)].as_ref() } else { right[position(vid)].as_ref() }),
                    wide(), || Ok::<_, ()>(())).unwrap();
                assert_eq!(plain(&actual.value), wanted, "mask={mask}, direction={direction:?}, comparison={comparison:?}");
                assert!(query.plan().needs_vertex_values());
                assert!(!query.plan().projects_properties(), "unprojected predicates still require property admission");
            }
        }
    }
}

#[test]
fn optional_and_existence_apply_pair_filters_before_scope_success() {
    let root = builder(&["a"]);
    let mut child = builder(&["c", "a"]);
    // The scope compiler reverses this first edge to start at correlation a.
    child.edge("c", R, GlaDirection::Reverse, "a").unwrap();
    child.compare_properties("a", P, IntegerComparison::Less, "c", Q).unwrap();
    let left = [CanonicalScalar::Int(1), CanonicalScalar::Int(8), CanonicalScalar::Int(10)];
    let right = [CanonicalScalar::Int(0), CanonicalScalar::Int(5), CanonicalScalar::Null];
    let edges = [(IDS[0], R, IDS[1]), (IDS[0], R, IDS[1]), (IDS[1], R, IDS[1])];
    let query = root.prepare_values_with_clauses(&[GraphMatchClause::optional(&child)],
        &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("c", "c")], 0, None).unwrap().with_duplicates();
    let result = query.plan().execute_governed_with_properties(6, IDS, edges, |_, _| Ok::<_, ()>(true),
        |vid, key| Ok(Some(if key == P { &left[position(vid)] } else { &right[position(vid)] })), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(plain(&result.value), vec![vec![Some(IDS[0]), Some(IDS[1])], vec![Some(IDS[0]), Some(IDS[1])],
        vec![Some(IDS[1]), None], vec![Some(IDS[2]), None]]);
    for (clause, wanted) in [(GraphMatchClause::exists(&child), vec![IDS[0]]),
        (GraphMatchClause::not_exists(&child), vec![IDS[1], IDS[2]])] {
        let query = root.prepare_values_with_clauses(&[clause], &[GraphColumn::vertex("a", "a")], 0, None).unwrap().with_duplicates();
        let result = query.plan().execute_governed_with_properties(6, IDS, edges, |_, _| Ok::<_, ()>(true),
            |vid, key| Ok(Some(if key == P { &left[position(vid)] } else { &right[position(vid)] })), wide(), || Ok::<_, ()>(())).unwrap();
        assert_eq!(plain(&result.value), wanted.into_iter().map(|vid| vec![Some(vid)]).collect::<Vec<_>>());
    }
    let aggregate = PreparedGraphAggregate::prepare(query, &[0], &[GraphAggregate::count_rows("rows"), GraphAggregate::count("matched", 1)], 0, None).unwrap();
    let result = aggregate.execute_governed(6, IDS, edges, |_, _| Ok::<_, ()>(true),
        |vid, key| Ok(Some(if key == P { &left[position(vid)] } else { &right[position(vid)] })), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(result.value.iter().map(|row| (row.get(0).unwrap().as_count(), row.get(1).unwrap().as_count())).collect::<Vec<_>>(),
        vec![(Some(2), Some(2)), (Some(1), Some(0)), (Some(1), Some(0))]);
}

#[test]
fn comparisons_are_not_cached_by_only_the_left_vertex_and_can_use_one_vertex_twice() {
    let values = [CanonicalScalar::Int(5), CanonicalScalar::Int(2), CanonicalScalar::Int(9)];
    let mut b = builder(&["a", "b"]);
    b.edge("a", R, GlaDirection::Forward, "b").unwrap();
    b.compare_properties("a", P, IntegerComparison::Less, "b", P).unwrap();
    let query = b.prepare_values(&[GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")], 0, None).unwrap().with_duplicates();
    let rows = query.plan().execute_governed_with_properties(2, [], [(IDS[0], R, IDS[1]), (IDS[0], R, IDS[2])],
        |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&values[position(vid)])), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(plain(&rows.value), vec![vec![Some(IDS[0]), Some(IDS[2])]]);
    let mut single = builder(&["a"]);
    single.compare_properties("a", P, IntegerComparison::Less, "a", Q).unwrap();
    let query = single.prepare_values(&[GraphColumn::vertex("a", "a")], 0, None).unwrap();
    let rows = query.plan().execute_governed_with_properties(1, [IDS[0]], [], |_, _| Ok::<_, ()>(true),
        |_, key| Ok(Some(if key == P { &values[1] } else { &values[2] })), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(plain(&rows.value), vec![vec![Some(IDS[0])]]);
}

#[test]
fn pair_selection_preserves_exact_limits_and_every_interruption_checkpoint() {
    let mut b = builder(&["a", "b"]);
    b.edge("a", R, GlaDirection::Forward, "b").unwrap();
    b.compare_properties("a", P, IntegerComparison::Less, "b", Q).unwrap();
    let query = b.prepare_values(&[GraphColumn::vertex("a", "a")], 0, None).unwrap().with_duplicates();
    let values = [CanonicalScalar::ucs_basic_text(&"a".repeat(2048)).unwrap(), CanonicalScalar::ucs_basic_text(&"b".repeat(4096)).unwrap()];
    let run = |policy| query.plan().execute_governed_with_properties(2, [], [(IDS[0], R, IDS[1]); 2],
        |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&values[position(vid)])), policy, || Ok::<_, usize>(()));
    let result = run(wide()).unwrap();
    assert_eq!(result.value.len(), 2);
    let exact = GqlQueryPolicy::new(2, 2, result.evaluator.work_units, result.evaluator.scratch_entries);
    assert_eq!(run(exact).unwrap(), result);
    assert!(matches!(run(GqlQueryPolicy::new(2, 1, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(_))));
    assert!(matches!(run(GqlQueryPolicy::new(2, 2, result.evaluator.work_units - 1, u64::MAX)), Err(GqlQueryError::Evaluator(_))));
    assert!(matches!(run(GqlQueryPolicy::new(2, 2, u64::MAX, result.evaluator.scratch_entries - 1)), Err(GqlQueryError::Evaluator(_))));
    let mut total = 0;
    query.plan().execute_governed_with_properties(2, [], [(IDS[0], R, IDS[1]); 2],
        |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&values[position(vid)])), wide(), || { total += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 1..=total {
        let mut at = 0;
        let result = query.plan().execute_governed_with_properties(2, [], [(IDS[0], R, IDS[1]); 2],
            |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&values[position(vid)])), wide(),
            || { at += 1; if at == stop { Err(stop) } else { Ok(()) } });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(actual)) if actual == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn definitions_are_bounded_immutable_and_cannot_escape_property_source_requirements() {
    let mut b = builder(&["a", "b"]);
    b.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let original = b.prepare_values(&[GraphColumn::vertex("a", "a")], 0, None).unwrap();
    assert_eq!(b.compare_properties("missing", P, IntegerComparison::Equal, "b", Q).unwrap_err(), PatternBuildError::UnknownVariable);
    assert_eq!(b.prepare_values(&[GraphColumn::vertex("a", "a")], 0, None).unwrap(), original);
    b.compare_properties("a", P, IntegerComparison::Equal, "b", Q).unwrap();
    assert_eq!(b.prepare("a", 0, None).unwrap_err(), PatternBuildError::RequiresValueProjection);
    assert_eq!(b.prepare_bindings(&["a", "b"], 0, None).unwrap_err(), PatternBuildError::RequiresValueProjection);
    let compared = b.prepare_values(&[GraphColumn::vertex("a", "a")], 0, None).unwrap();
    assert_ne!(compared.canonical_bytes(), original.canonical_bytes());
    for _ in 1..MAX_PATTERN_PREDICATES { b.compare_properties("a", P, IntegerComparison::Equal, "b", Q).unwrap(); }
    let full = b.prepare_values(&[GraphColumn::vertex("a", "a")], 0, None).unwrap();
    assert!(matches!(b.compare_properties("a", P, IntegerComparison::Equal, "b", Q), Err(PatternBuildError::LimitExceeded { .. })));
    assert!(matches!(b.filter("a", VertexPredicate::PropertyNull { key: P, is_null: false }), Err(PatternBuildError::LimitExceeded { .. })));
    assert_eq!(b.prepare_values(&[GraphColumn::vertex("a", "a")], 0, None).unwrap(), full);
    assert_eq!(compared.plan().operators().iter().filter(|op| matches!(op, fgdb_gql::algebra::GlaOperator::CompareProperties { .. })).count(), 1);
}
