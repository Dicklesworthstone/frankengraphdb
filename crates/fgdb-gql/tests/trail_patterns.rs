//! Edge-unique paths through the ordinary compiler, source policy and summaries.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GlaOperator, GraphColumn, GraphPathFunction, GraphPatternBuilder,
    GraphValueRow, PatternBuildError, PreparedGraphPattern,
};
use fgdb_gql::{
    GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphWalkBounds, PreparedGraphAggregate,
};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::Cell;
use std::collections::BTreeSet;

const R: RelationId = RelationId(1);
const VERTICES: [VId; 3] = [VId(1), VId(2), VId(3)];
const EDGES: [(EId, VId, RelationId, VId); 5] = [
    (EId(11), VId(1), R, VId(1)),
    (EId(12), VId(1), R, VId(2)),
    (EId(13), VId(1), R, VId(2)),
    (EId(14), VId(2), R, VId(1)),
    (EId(15), VId(1), R, VId(3)),
];
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 2_000_000) }
fn builder(direction: GlaDirection, low: u32, high: u32) -> GraphPatternBuilder {
    let mut b = GraphPatternBuilder::new();
    b.vertex("a").unwrap().vertex("b").unwrap();
    b.trail_walk("a", R, direction, "b", GraphWalkBounds::new(low, high).unwrap()).unwrap();
    b
}
fn query(direction: GlaDirection, low: u32, high: u32, capture: bool) -> PreparedGraphPattern<GraphValueRow> {
    let mut b = builder(direction, low, high);
    let mut columns = vec![GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")];
    if capture {
        b.capture_path("route").unwrap();
        columns.push(GraphColumn::path("route", "route", GraphPathFunction::Value));
        columns.push(GraphColumn::path("hops", "route", GraphPathFunction::Length));
    }
    b.prepare_values(&columns, 0, None).unwrap().with_duplicates()
}
fn run(query: &PreparedGraphPattern<GraphValueRow>, edges: &[(EId, VId, RelationId, VId)], policy: GqlQueryPolicy)
    -> fgdb_gql::GqlQueryExecution<GraphValueRow> {
    query.plan().execute_governed_with_identified_properties(
        (3 + edges.len()) as u64, VERTICES, edges.iter().copied(),
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), policy, || Ok::<_, ()>(()),
    ).unwrap()
}

#[test]
fn endpoint_only_trails_require_real_ids_and_refuse_legacy_projection_sources() {
    let b = builder(GlaDirection::Undirected, 1, 3);
    assert_eq!(b.prepare("b", 0, None).unwrap_err(), PatternBuildError::RequiresValueProjection);
    assert_eq!(b.prepare_bindings(&["a", "b"], 0, None).unwrap_err(), PatternBuildError::RequiresValueProjection);
    let plan = query(GlaDirection::Undirected, 1, 3, false);
    assert!(plan.plan().requires_identified_edges());
    assert!(!plan.plan().operators().iter().any(|op| matches!(op, GlaOperator::CapturePath { .. })));
    let consumed = Cell::new(0);
    let result = plan.plan().execute_governed_with_properties(
        8, VERTICES, EDGES.into_iter().map(|(_, s, r, d)| { consumed.set(consumed.get() + 1); (s, r, d) }),
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(()),
    );
    assert!(matches!(result, Err(GqlQueryError::IdentifiedEdgesRequired)));
    assert_eq!(consumed.get(), 0, "never fabricate EIds from triple ordinals");
    // Even empty input is not evidence that a missing identity source was valid.
    assert!(matches!(plan.plan().execute_governed_with_properties(
        0, [], [], |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())
    ), Err(GqlQueryError::IdentifiedEdgesRequired)));
}

#[test]
fn typed_capture_and_endpoint_bags_match_independent_complete_walk_filtering() {
    for mask in 0..32 {
        let edges: Vec<_> = EDGES.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, edge)| *edge).collect();
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            let mut oriented = Vec::new();
            for &(eid, left, _, right) in &edges {
                let (left, right) = if direction == GlaDirection::Reverse { (right, left) } else { (left, right) };
                oriented.push((eid, left, right));
                if direction == GlaDirection::Undirected && left != right { oriented.push((eid, right, left)); }
            }
            for maximum in 0..=3 {
                for minimum in 0..=maximum {
                    let mut layer: Vec<_> = VERTICES.into_iter().map(|v| (v, Vec::<(EId, VId)>::new())).collect();
                    let mut expected = Vec::new();
                    for depth in 0..=maximum {
                        if depth >= minimum {
                            expected.extend(layer.iter().filter(|(_, path)| {
                                path.iter().map(|step| step.0).collect::<BTreeSet<_>>().len() == path.len()
                            }).cloned());
                        }
                        let mut next = Vec::new();
                        for (start, path) in layer {
                            let endpoint = path.last().map_or(start, |step| step.1);
                            for &(edge, left, right) in &oriented {
                                if left == endpoint {
                                    let mut child = path.clone(); child.push((edge, right));
                                    next.push((start, child));
                                }
                            }
                        }
                        layer = next;
                    }
                    expected.sort();
                    let captured = run(&query(direction, minimum, maximum, true), &edges, wide());
                    let mut actual: Vec<_> = captured.value.iter().map(|row| {
                        let path = row.get(2).unwrap().as_path().unwrap();
                        assert_eq!(row.get(3).unwrap().as_scalar(), Some(&CanonicalScalar::Int(path.len() as i64)));
                        (path.start(), path.steps().to_vec())
                    }).collect();
                    actual.sort();
                    assert_eq!(actual, expected, "{mask} {direction:?} {minimum}..{maximum}");
                    let endpoints = run(&query(direction, minimum, maximum, false), &edges, wide());
                    let actual: Vec<_> = endpoints.value.iter().map(|row|
                        (row.get(0).unwrap().as_vertex().unwrap(), row.get(1).unwrap().as_vertex().unwrap())).collect();
                    let mut expected: Vec<_> = expected.iter().map(|(start, path)|
                        (*start, path.last().map_or(*start, |step| step.1))).collect();
                    expected.sort();
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}

#[test]
fn captured_paths_cost_more_than_endpoint_pulls_without_changing_multiplicity() {
    let plain = run(&query(GlaDirection::Forward, 1, 3, false), &EDGES, wide());
    let paths = run(&query(GlaDirection::Forward, 1, 3, true), &EDGES, wide());
    assert_eq!(plain.value.len(), paths.value.len());
    assert!(plain.evaluator.scratch_entries < paths.evaluator.scratch_entries);
    let one = [(EId(7), VId(1), R, VId(2))];
    assert!(run(&query(GlaDirection::Undirected, 2, 2, false), &one, wide()).value.is_empty());
    let parallel = [one[0], (EId(8), VId(1), R, VId(2))];
    assert_eq!(run(&query(GlaDirection::Undirected, 2, 2, false), &parallel, wide()).value.len(), 4);
    let looped = [(EId(7), VId(1), R, VId(1))];
    assert!(run(&query(GlaDirection::Forward, 1024, 1024, true), &looped,
        GqlQueryPolicy::new(4, 0, 1000, 1000)).value.is_empty());
}

#[test]
fn one_budget_covers_real_identity_admission_membership_and_result_release() {
    for capture in [false, true] {
        let plan = query(GlaDirection::Undirected, 1, 3, capture);
        let calls = Cell::new(0);
        let measured = plan.plan().execute_governed_with_identified_properties(
            8, VERTICES, EDGES, |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || {
                calls.set(calls.get() + 1); Ok::<_, usize>(())
            },
        ).unwrap();
        let caps = [8, measured.rows.result_rows, measured.evaluator.work_units, measured.evaluator.scratch_entries];
        assert_eq!(run(&plan, &EDGES, GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])), measured);
        for dimension in 0..4 {
            let mut cap = caps; cap[dimension] -= 1;
            assert!(plan.plan().execute_governed_with_identified_properties(
                8, VERTICES, EDGES, |_, _| Ok::<_, ()>(true), |_, _| Ok(None),
                GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3]), || Ok::<_, ()>(())
            ).is_err());
        }
        for stop in 1..=calls.get() {
            let mut at = 0;
            let result = plan.plan().execute_governed_with_identified_properties(
                8, VERTICES, EDGES, |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || {
                    at += 1; if at == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(at, stop);
        }
    }
}

#[test]
fn exact_aggregates_keep_path_identity_and_endpoint_only_trail_occurrences() {
    let paths = query(GlaDirection::Forward, 1, 3, true);
    let source = run(&paths, &EDGES, wide());
    let count = source.value.len() as u64;
    let hops: i128 = source.value.iter().map(|row| row.get(2).unwrap().as_path().unwrap().len() as i128).sum();
    let aggregate = PreparedGraphAggregate::prepare(paths, &[], &[
        GraphAggregate::count_rows("rows"), GraphAggregate::count_distinct("paths", 2),
        GraphAggregate::sum_int("hops", 3),
    ], 0, None).unwrap();
    let result = aggregate.execute_governed_with_identified_properties(
        8, VERTICES, EDGES, |_, _| Ok::<_, ()>(true), |_, _| Ok(None),
        GqlQueryPolicy::new(8, 1, 2_000_000, 2_000_000), || Ok::<_, ()>(()),
    ).unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(count));
    assert_eq!(result.value[0].values()[1].as_count(), Some(count));
    assert_eq!(result.value[0].values()[2].as_integer(), Some(hops));
    let endpoint = PreparedGraphAggregate::prepare(query(GlaDirection::Forward, 1, 3, false), &[],
        &[GraphAggregate::count_rows("rows")], 0, None).unwrap();
    let result = endpoint.execute_governed_with_identified_properties(
        8, VERTICES, EDGES, |_, _| Ok::<_, ()>(true), |_, _| Ok(None),
        GqlQueryPolicy::new(8, 1, 2_000_000, 2_000_000), || Ok::<_, ()>(()),
    ).unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(count));
}

#[test]
fn late_property_failure_and_rejected_builder_edits_do_not_become_empty_success() {
    let mut b = builder(GlaDirection::Forward, 1, 3);
    let columns = [GraphColumn::property("p", "b", PropertyKeyId(1))];
    let before = b.prepare_values(&columns, 0, None).unwrap();
    assert_eq!(b.trail_walk("missing", R, GlaDirection::Forward, "b", GraphWalkBounds::new(1, 2).unwrap()).unwrap_err(),
        PatternBuildError::UnknownVariable);
    assert_eq!(before, b.prepare_values(&columns, 0, None).unwrap());
    let plan = b.prepare_values(&columns, 0, Some(0)).unwrap();
    let result = plan.plan().execute_governed_with_identified_properties(
        8, VERTICES, EDGES, |_, _| Ok::<_, &str>(true), |vid, _| {
            if vid == VId(3) { Err("late property failure") } else { Ok(None) }
        }, wide(), || Ok::<_, ()>(()),
    );
    assert!(matches!(result, Err(GqlQueryError::Source("late property failure"))));
}
