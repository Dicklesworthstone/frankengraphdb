//! Independent finite oracles for correlated semijoins and antijoins.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphExistence, GraphPatternBuilder,
    GraphValueRow, IntegerComparison, PatternBuildError, PreparedGraphPattern, VertexPredicate};
use fgdb_gql::{GqlQueryError, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const T: RelationId = RelationId(3);
const P: PropertyKeyId = PropertyKeyId(1);
type Edge = (VId, RelationId, VId);

fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn node() -> GraphPatternBuilder {
    let mut b = GraphPatternBuilder::new(); b.vertex("a").unwrap(); b
}
fn path(direction: GlaDirection, closed: bool) -> GraphPatternBuilder {
    let mut b = GraphPatternBuilder::new();
    for name in if closed { vec!["a", "x"] } else { vec!["a", "x", "y"] } { b.vertex(name).unwrap(); }
    b.edge("a", R, direction, "x").unwrap();
    b.edge("x", S, direction, if closed { "a" } else { "y" }).unwrap(); b
}
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter().map(|row| row.get(0).unwrap().as_vertex().unwrap()).collect()
}
fn run(pattern: &PreparedGraphPattern<GraphValueRow>, vertices: &[VId], edges: &[Edge]) -> Vec<VId> {
    let result = pattern.plan().execute_governed_with_properties(
        (vertices.len() + edges.len()) as u64, vertices.iter().copied(), edges.iter().copied(),
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(()),
    ).unwrap(); ids(&result.value)
}
fn orient(edge: Edge, direction: GlaDirection) -> Vec<(VId, VId)> {
    let (s, _, d) = edge;
    match direction {
        GlaDirection::Forward => vec![(s, d)],
        GlaDirection::Reverse => vec![(d, s)],
        GlaDirection::Undirected if s != d => vec![(s, d), (d, s)],
        GlaDirection::Undirected => vec![(s, d)],
    }
}

#[test]
fn exhaustive_path_probes_match_concrete_assignment_oracle_and_do_not_multiply_outer_bags() {
    let outer = node();
    let universe = [(VId(0), R, VId(1)), (VId(1), R, VId(1)), (VId(u128::MAX), R, VId(0)),
        (VId(1), S, VId(0)), (VId(0), S, VId(1)), (VId(1), S, VId(u128::MAX))];
    let vertices = [VId(0), VId(1), VId(u128::MAX), VId(0)];
    for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
        for closed in [false, true] {
            let probe = path(direction, closed);
            for anti in [false, true] {
                let constraint = if anti { GraphExistence::not_exists(&probe) } else { GraphExistence::exists(&probe) };
                let pattern = outer.prepare_values_with_existence(&[constraint],
                    &[GraphColumn::vertex("owner", "a")], 0, None).unwrap().with_duplicates();
                assert!(!pattern.plan().scans_edges()); assert!(pattern.plan().reads_edges());
                for encoding in 0..3_usize.pow(universe.len() as u32) {
                    let mut encoded = encoding; let mut edges = Vec::new();
                    for edge in universe { for _ in 0..encoded % 3 { edges.push(edge); } encoded /= 3; }
                    let mut expected = Vec::new();
                    for owner in vertices {
                        let mut exists = false;
                        for &first in edges.iter().filter(|edge| edge.1 == R) {
                            for (a, x) in orient(first, direction) {
                                if a != owner { continue; }
                                for &second in edges.iter().filter(|edge| edge.1 == S) {
                                    for (source, y) in orient(second, direction) {
                                        exists |= source == x && (!closed || y == owner);
                                    }
                                }
                            }
                        }
                        if exists != anti { expected.push(owner); }
                    }
                    expected.sort();
                    assert_eq!(run(&pattern, &vertices, &edges), expected, "case={encoding}, {direction:?}, closed={closed}, anti={anti}");
                }
            }
        }
    }
}

#[test]
fn all_outer_correlations_are_enforced_and_inner_variables_are_clause_local() {
    let mut outer = node(); outer.vertex("b").unwrap(); outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let mut probe = GraphPatternBuilder::new();
    for name in ["x", "b", "a"] { probe.vertex(name).unwrap(); }
    probe.edge("x", T, GlaDirection::Forward, "b").unwrap();
    probe.edge("a", S, GlaDirection::Forward, "x").unwrap();
    let query = outer.prepare_values_with_existence(&[GraphExistence::exists(&probe)],
        &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")], 0, None).unwrap().with_duplicates();
    let edges = [(VId(1), R, VId(2)), (VId(1), R, VId(2)), (VId(1), R, VId(9)),
        (VId(1), S, VId(3)), (VId(1), S, VId(3)), (VId(3), T, VId(2))];
    assert_eq!(run(&query, &[], &edges), vec![VId(1), VId(1)]);
    let mut left = node(); left.vertex("x").unwrap(); left.edge("a", S, GlaDirection::Forward, "x").unwrap();
    let mut right = node(); right.vertex("x").unwrap(); right.edge("a", T, GlaDirection::Forward, "x").unwrap();
    let scopes = node().prepare_values_with_existence(&[GraphExistence::exists(&left), GraphExistence::exists(&right)],
        &[GraphColumn::vertex("a", "a")], 0, None).unwrap();
    assert_eq!(run(&scopes, &[VId(1)], &[(VId(1), S, VId(3)), (VId(1), T, VId(4))]), vec![VId(1)]);
    assert!(run(&scopes, &[VId(1)], &[(VId(1), S, VId(3))]).is_empty());
}

#[test]
fn existence_stops_at_the_first_complete_witness_but_errors_are_not_absence() {
    let mut probe = node(); probe.vertex("x").unwrap(); probe.edge("a", R, GlaDirection::Forward, "x").unwrap();
    probe.filter("x", VertexPredicate::IntegerProperty { key: P, comparison: IntegerComparison::Equal, value: 7 }).unwrap();
    for anti in [false, true] {
        let declaration = if anti { GraphExistence::not_exists(&probe) } else { GraphExistence::exists(&probe) };
        let query = node().prepare_values_with_existence(&[declaration], &[GraphColumn::vertex("a", "a")], 0, None).unwrap();
        let edges: Vec<_> = (2..100).map(|id| (VId(1), R, VId(id))).collect();
        let mut reads = 0;
        let output = query.plan().execute_governed_with_properties(99, [VId(1)], edges.iter().copied(),
            |vid, _| { reads += 1; assert_eq!(vid, VId(2)); Ok::<_, &str>(true) },
            |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap();
        assert_eq!(reads, 1); assert_eq!(output.value.is_empty(), anti);
        let refused = query.plan().execute_governed_with_properties(99, [VId(1)], edges,
            |_, _| Err::<bool, _>("unreadable probe vertex"), |_, _| Ok(None), wide(), || Ok::<_, ()>(()));
        assert!(matches!(refused, Err(GqlQueryError::Source("unreadable probe vertex"))));
    }
}

#[test]
fn correlated_label_tests_do_not_become_mandatory_outer_scan_labels() {
    let mut probe = node(); probe.filter("a", VertexPredicate::HasLabel(LabelId(7))).unwrap();
    let query = node().prepare_values_with_existence(&[GraphExistence::not_exists(&probe)],
        &[GraphColumn::vertex("a", "a")], 0, None).unwrap();
    assert_eq!(query.required_vertex_label(), None); assert!(!query.plan().reads_edges());
    let result = query.plan().execute_governed_with_properties(2, [VId(0), VId(1)], [],
        |vid, predicates| Ok::<_, ()>(predicates.iter().all(|p| p.matches(if vid == VId(0) { &[LabelId(7)] } else { &[] }, &[]))),
        |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(ids(&result.value), vec![VId(1)]);
}

#[test]
fn probes_share_the_policy_and_every_interruption_propagates_without_rows() {
    let probe = path(GlaDirection::Forward, false);
    let query = node().prepare_values_with_existence(&[GraphExistence::not_exists(&probe)],
        &[GraphColumn::vertex("a", "a")], 0, None).unwrap();
    let edges = [(VId(1), R, VId(2)), (VId(2), S, VId(3))];
    let vertices = [VId(1), VId(2), VId(3)];
    let run = |policy| query.plan().execute_governed_with_properties(5, vertices, edges,
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), policy, || Ok::<_, ()>(()));
    let full = run(wide()).unwrap(); assert_eq!(ids(&full.value), vec![VId(2), VId(3)]);
    let exact = GqlQueryPolicy::new(5, 2, full.evaluator.work_units, full.evaluator.scratch_entries);
    assert_eq!(run(exact).unwrap(), full);
    for cap in [GqlQueryPolicy::new(5, 2, full.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(5, 2, u64::MAX, full.evaluator.scratch_entries - 1)] {
        assert!(matches!(run(cap), Err(GqlQueryError::Evaluator(_))));
    }
    assert!(matches!(run(GqlQueryPolicy::new(5, 1, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(_))));
    let mut total = 0;
    query.plan().execute_governed_with_properties(5, vertices, edges, |_, _| Ok::<_, ()>(true),
        |_, _| Ok(None), wide(), || { total += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 1..=total {
        let mut at = 0;
        let result = query.plan().execute_governed_with_properties(5, vertices, edges, |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None), wide(), || { at += 1; if at == stop { Err(stop) } else { Ok(()) } });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop)); assert_eq!(at, stop);
    }
}

#[test]
fn definitions_are_scoped_bounded_and_change_logical_identity() {
    let base = node(); let probe = path(GlaDirection::Forward, false);
    let columns = [GraphColumn::vertex("a", "a")];
    assert_eq!(base.prepare_values_with_existence(&[], &columns, 0, None).unwrap(), base.prepare_values(&columns, 0, None).unwrap());
    let yes = base.prepare_values_with_existence(&[GraphExistence::exists(&probe)], &columns, 0, None).unwrap();
    let no = base.prepare_values_with_existence(&[GraphExistence::not_exists(&probe)], &columns, 0, None).unwrap();
    assert_ne!(yes.canonical_bytes(), no.canonical_bytes());
    assert_eq!(base.prepare_values_with_existence(&[GraphExistence::exists(&probe)], &[GraphColumn::vertex("x", "x")], 0, None).unwrap_err(), PatternBuildError::UnknownVariable);
    let mut uncorrelated = GraphPatternBuilder::new(); uncorrelated.vertex("z").unwrap();
    assert_eq!(base.prepare_values_with_existence(&[GraphExistence::exists(&uncorrelated)], &columns, 0, None).unwrap_err(), PatternBuildError::Disconnected);
    assert!(matches!(base.prepare_values_with_existence(&[GraphExistence::exists(&base); 65], &columns, 0, None), Err(PatternBuildError::LimitExceeded { .. })));
    assert!(matches!(base.prepare_values_with_existence(&[GraphExistence::exists(&probe); 33], &columns, 0, None), Err(PatternBuildError::LimitExceeded { .. })));
    assert!(!format!("{:?}", GraphExistence::exists(&probe)).contains("\"a\""));
    let property = CanonicalScalar::Int(7);
    let _ = property;
}
