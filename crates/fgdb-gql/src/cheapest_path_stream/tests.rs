use super::*;
use crate::algebra::GlaDirection;
use crate::{GraphCheapestPathMode, GraphPathCostError, GraphWalkBounds};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(7);
const W: PropertyKeyId = PropertyKeyId(9);
const MODES: [GraphCheapestPathMode; 4] = [GraphCheapestPathMode::Walk,
    GraphCheapestPathMode::Trail, GraphCheapestPathMode::Acyclic, GraphCheapestPathMode::Simple];
type Edge = (EId, VId, RelationId, VId);
type Answer = (i128, Vec<(EId, VId)>);
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 10_000_000, 10_000_000) }
fn query(mode: GraphCheapestPathMode, direction: GlaDirection, target: VId, lo: u32, hi: u32) -> PreparedGraphCheapestPath {
    PreparedGraphCheapestPath::new(VId(0), target, R, direction, W,
        GraphWalkBounds::new(lo, hi).unwrap()).unwrap().with_mode(mode)
}
fn fixture(mask: u8) -> (Vec<Edge>, BTreeMap<EId, CanonicalScalar>) {
    let raw = [(1, 0, 0, -1), (2, 0, 1, 2), (3, 0, 1, 2),
        (4, 1, 2, -4), (5, 2, 0, 1), (6, 1, 1, 0)];
    let selected: Vec<_> = raw.into_iter().enumerate()
        .filter(|(at, _)| mask & (1 << at) != 0).map(|(_, row)| row).collect();
    (selected.iter().map(|&(id, from, to, _)| (EId(id), VId(from), R, VId(to))).collect(),
        selected.iter().map(|&(id, _, _, weight)| (EId(id), CanonicalScalar::Int(weight))).collect())
}
fn open<C>(q: &PreparedGraphCheapestPath, k: u64, p: GqlQueryPolicy, checkpoint: impl FnMut() -> Result<(), C>)
    -> StreamResult<GraphCheapestPathStream, (), C> {
    let (edges, weights) = fixture(63);
    q.stream_governed_with_edge_properties(k, edges.len() as u64 + 3, [VId(0), VId(1), VId(2)], edges,
        |id, _| Ok(weights.get(&id)), p, checkpoint)
}
fn rows(stream: &mut GraphCheapestPathStream) -> Vec<GraphCostPath> {
    let mut rows = Vec::new();
    while let Some(row) = stream.next_with_checkpoint(|| Ok::<_, ()>(())).unwrap() { rows.push(row); }
    rows
}
fn plain(rows: &[GraphCostPath]) -> Vec<Answer> {
    rows.iter().map(|row| (row.cost(), row.path().steps().to_vec())).collect()
}

// Independent oracle: enumerate the entire finite WALK bag first, then test
// complete-route repetition laws and sort by (cost, canonical path). It does
// not use the production suffix table, dominance, heap or partition refinement.
fn oracle(q: &PreparedGraphCheapestPath, k: u64, edges: &[Edge], weights: &BTreeMap<EId, CanonicalScalar>) -> Vec<Answer> {
    let mut layer = vec![(q.source(), 0_i128, Vec::<(EId, VId)>::new())];
    let mut answers = Vec::new();
    for depth in 0..=q.bounds().maximum() {
        if depth >= q.bounds().minimum() {
            for (end, cost, steps) in &layer {
                if *end != q.target() { continue; }
                let allowed = match q.mode() {
                    GraphCheapestPathMode::Walk => true,
                    GraphCheapestPathMode::Trail => steps.iter().map(|s| s.0).collect::<BTreeSet<_>>().len() == steps.len(),
                    mode => {
                        let mut ids: Vec<_> = std::iter::once(q.source()).chain(steps.iter().map(|s| s.1)).collect();
                        if mode == GraphCheapestPathMode::Simple && ids.len() > 1 && ids.last() == Some(&q.source()) { ids.pop(); }
                        ids.iter().collect::<BTreeSet<_>>().len() == ids.len()
                    }
                };
                if allowed { answers.push((*cost, steps.clone())); }
            }
        }
        if depth == q.bounds().maximum() { break; }
        let mut next = Vec::new();
        for (end, cost, steps) in layer {
            for &(eid, from, relation, to) in edges {
                if relation != q.relation() { continue; }
                let CanonicalScalar::Int(weight) = &weights[&eid] else { panic!("integer fixture"); };
                let mut destinations = Vec::new();
                if q.direction() != GlaDirection::Reverse && from == end { destinations.push(to); }
                if q.direction() != GlaDirection::Forward && to == end
                    && (q.direction() != GlaDirection::Undirected || from != to) { destinations.push(from); }
                for to in destinations {
                    let mut child = steps.clone(); child.push((eid, to));
                    next.push((to, cost + i128::from(*weight), child));
                }
            }
        }
        layer = next;
    }
    answers.sort();
    answers.truncate(usize::try_from(k).unwrap_or(usize::MAX));
    answers
}

#[test]
fn every_mode_and_direction_matches_complete_routes_and_eager_accounting_across_page_sizes() {
    for mask in [0, 10, 4, 15, 31, 63] {
        let (edges, weights) = fixture(mask);
        for mode in MODES {
            for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
                for lo in [0, 2] {
                    for target in [VId(0), VId(2)] {
                        let q = query(mode, direction, target, lo, 3);
                        for k in [0, 1, 5] {
                            let eager = q.execute_k_governed_with_edge_properties(k, 3 + edges.len() as u64,
                                [VId(0), VId(1), VId(2)], edges.iter().copied(), |id, _| Ok::<_, ()>(weights.get(&id)),
                                policy(), || Ok::<_, ()>(())).unwrap();
                            assert_eq!(plain(&eager.value), oracle(&q, k, &edges, &weights));
                            for page_size in [1, 2, 7] {
                                let mut stream = q.stream_governed_with_edge_properties(k, 3 + edges.len() as u64,
                                    [VId(0), VId(1), VId(2)], edges.iter().copied(), |id, _| Ok::<_, ()>(weights.get(&id)),
                                    policy(), || Ok::<_, ()>(())).unwrap();
                                assert_eq!(stream.row_stats().result_rows, 0);
                                let mut actual = Vec::new();
                                while stream.state() == GraphCheapestPathStreamState::Open {
                                    for _ in 0..page_size {
                                        let Some(row) = stream.next_with_checkpoint(|| Ok::<_, ()>(())).unwrap() else { break; };
                                        actual.push(row);
                                    }
                                }
                                assert_eq!(actual, eager.value);
                                assert_eq!(stream.row_stats(), eager.rows);
                                assert_eq!(stream.evaluator_stats(), eager.evaluator);
                                assert_eq!(stream.state(), GraphCheapestPathStreamState::Exhausted);
                                assert!(stream.next_with_checkpoint(|| -> Result<(), ()> { panic!("terminal checkpoint"); }).unwrap().is_none());
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn exact_limits_include_prior_admission_and_one_less_refuses_without_reset_or_partial_row() {
    for mode in MODES {
        let q = query(mode, GlaDirection::Forward, VId(2), 0, 4);
        let mut full = open(&q, 5, policy(), || Ok::<_, ()>(())).unwrap();
        let expected = rows(&mut full);
        let r = full.row_stats(); let e = full.evaluator_stats();
        let exact = GqlQueryPolicy::new(r.snapshot_records, r.result_rows, e.work_units, e.scratch_entries);
        let mut same = open(&q, 5, exact, || Ok::<_, ()>(())).unwrap();
        assert_eq!(rows(&mut same), expected);
        for limited in [
            GqlQueryPolicy::new(r.snapshot_records - 1, 1000, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, r.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, u64::MAX, e.scratch_entries - 1),
        ] {
            let mut cursor = match open(&q, 5, limited, || Ok::<_, ()>(())) {
                Ok(cursor) => cursor,
                Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)) => continue,
                Err(other) => panic!("unexpected refusal: {other:?}"),
            };
            let mut delivered = Vec::new();
            loop {
                match cursor.next_with_checkpoint(|| Ok::<_, ()>(())) {
                    Ok(Some(row)) => delivered.push(row),
                    Ok(None) => panic!("quota refusal was hidden as exhaustion"),
                    Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)) => break,
                    Err(other) => panic!("unexpected pull refusal: {other:?}"),
                }
            }
            assert_eq!(delivered, expected[..delivered.len()]);
            assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
            assert_eq!(cursor.state(), GraphCheapestPathStreamState::Failed);
            cursor.close();
            assert_eq!(cursor.state(), GraphCheapestPathStreamState::Failed);
            assert!(cursor.next_with_checkpoint(|| -> Result<(), ()> { panic!("refused cursor resumed"); }).unwrap().is_none());
        }
        let (edges, weights) = fixture(63);
        let prior = GlaExecutionStats { work_units: 101, scratch_entries: 17 };
        let total = GqlQueryPolicy::new(r.snapshot_records, r.result_rows, e.work_units + 101, e.scratch_entries + 17);
        let mut combined = q.stream_governed_with_admission(5, r.snapshot_records,
            [VId(0), VId(1), VId(2)], edges.iter().copied(), |id, _| Ok::<_, ()>(weights.get(&id)), prior,
            total, || Ok::<_, ()>(())).unwrap();
        assert_eq!(rows(&mut combined), expected);
        assert_eq!(combined.evaluator_stats(), GlaExecutionStats { work_units: e.work_units + 101, scratch_entries: e.scratch_entries + 17 });
    }
}

#[test]
fn cancellation_at_every_open_and_pull_checkpoint_delivers_only_the_completed_prefix() {
    for mode in MODES {
        let q = query(mode, GlaDirection::Undirected, VId(2), 0, 3);
        let mut checkpoints = 0;
        let mut good = open(&q, 4, policy(), || { checkpoints += 1; Ok::<_, usize>(()) }).unwrap();
        let mut expected = Vec::new();
        while let Some(row) = good.next_with_checkpoint(|| { checkpoints += 1; Ok::<_, usize>(()) }).unwrap() { expected.push(row); }
        for stop in 1..=checkpoints {
            let calls = Cell::new(0);
            let mut checkpoint = || {
                calls.set(calls.get() + 1);
                if calls.get() == stop { Err(stop) } else { Ok(()) }
            };
            let mut stream = match open(&q, 4, policy(), &mut checkpoint) {
                Err(GqlQueryError::Interrupted(at)) => { assert_eq!(at, stop); assert_eq!(calls.get(), stop); continue; }
                Err(other) => panic!("unexpected open error: {other:?}"),
                Ok(stream) => stream,
            };
            let mut delivered = Vec::new();
            loop {
                match stream.next_with_checkpoint(&mut checkpoint) {
                    Ok(Some(row)) => delivered.push(row),
                    Ok(None) => panic!("interruption became EOF at {stop}"),
                    Err(GqlQueryError::Interrupted(at)) => { assert_eq!(at, stop); break; }
                    Err(other) => panic!("unexpected pull error: {other:?}"),
                }
            }
            assert_eq!(calls.get(), stop);
            assert_eq!(delivered, expected[..delivered.len()]);
            assert_eq!(stream.row_stats().result_rows, delivered.len() as u64);
            assert_eq!(stream.state(), GraphCheapestPathStreamState::Failed);
            assert!(stream.next_with_checkpoint(|| -> Result<(), usize> { panic!("checkpoint after failure"); }).unwrap().is_none());
        }
    }
}

#[test]
fn close_and_k_boundary_never_expand_the_unrequested_suffix() {
    // Two self loops encode 2^40 exact-length WALK answers. Requesting one must
    // not enumerate the other answers, and close must never refine its prefix.
    let q = query(GraphCheapestPathMode::Walk, GlaDirection::Forward, VId(0), 40, 40);
    let costs = BTreeMap::from([(EId(1), CanonicalScalar::Int(-1)), (EId(2), CanonicalScalar::Int(1))]);
    let edges = [(EId(1), VId(0), R, VId(0)), (EId(2), VId(0), R, VId(0))];
    let start = |k| q.stream_governed_with_edge_properties(k, 3, [VId(0)], edges,
        |id, _| Ok::<_, ()>(costs.get(&id)), GqlQueryPolicy::new(3, 1, 10_000, 10_000), || Ok::<_, ()>(())).unwrap();
    let mut one = start(1);
    let first = one.next_with_checkpoint(|| Ok::<_, ()>(())).unwrap().unwrap();
    assert_eq!(first.cost(), -40);
    assert_eq!(one.state(), GraphCheapestPathStreamState::Exhausted);
    let mut many = start(u64::MAX);
    assert_eq!(many.next_with_checkpoint(|| Ok::<_, ()>(())).unwrap(), Some(first));
    assert_eq!(many.evaluator_stats(), one.evaluator_stats());
    many.close(); many.close();
    assert_eq!(many.state(), GraphCheapestPathStreamState::Closed);
    let before = many.evaluator_stats();
    for cursor in [&mut one, &mut many] {
        assert!(cursor.next_with_checkpoint(|| -> Result<(), ()> { panic!("backpressure read ahead"); }).unwrap().is_none());
    }
    assert_eq!(many.evaluator_stats(), before);
}

#[test]
fn opening_zero_count_or_missing_anchors_still_validates_all_selected_costs() {
    let q = query(GraphCheapestPathMode::Walk, GlaDirection::Forward, VId(99), 0, 0);
    let edges = [(EId(1), VId(0), R, VId(1))];
    for count in [0, 1] {
        let missing = q.stream_governed_with_edge_properties(count, 3, [VId(0), VId(1)], edges,
            |_, _| Ok::<_, ()>(None), policy(), || Ok::<_, ()>(()));
        assert!(matches!(missing, Err(GqlQueryError::Source(GraphCheapestPathError::Cost(GraphPathCostError::MissingWeight)))));
        let refused = q.stream_governed_with_edge_properties(count, 3, [VId(0), VId(1)], edges,
            |_, _| Err::<Option<&CanonicalScalar>, _>("source refused"), policy(), || Ok::<_, ()>(()));
        assert!(matches!(refused, Err(GqlQueryError::Source(GraphCheapestPathError::Source("source refused")))));
    }
}

#[test]
fn impossible_prior_usage_refuses_before_source_access_and_counter_overflow_is_wide() {
    let q = query(GraphCheapestPathMode::Walk, GlaDirection::Forward, VId(2), 0, 3);
    for prior in [GlaExecutionStats { work_units: 2, scratch_entries: 0 }, GlaExecutionStats { work_units: 0, scratch_entries: 2 }] {
        let result = q.stream_governed_with_admission(0, 0, std::iter::from_fn(|| -> Option<VId> { panic!("over-budget source driven"); }), [],
            |_, _| Ok::<_, ()>(None), prior, GqlQueryPolicy::new(0, 0, 1, 1), || Ok::<_, ()>(()));
        assert!(matches!(result, Err(GqlQueryError::Evaluator(error)) if error.observed == 2 && error.limit == 1));
    }
    let result = q.stream_governed_with_admission(0, 0, [VId(0)], [], |_, _| Ok::<_, ()>(None),
        GlaExecutionStats { work_units: u64::MAX, scratch_entries: 0 },
        GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX), || Ok::<_, ()>(()));
    assert!(matches!(result, Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(u64::MAX) + 1));
}

#[test]
fn stream_owns_costs_and_full_width_identities_without_debug_disclosure() {
    let source = VId(1_u128 << 100); let target = VId(u128::MAX);
    let q = PreparedGraphCheapestPath::new(source, target, R, GlaDirection::Forward, W,
        GraphWalkBounds::new(1, 1).unwrap()).unwrap();
    let mut stream = {
        let mut costs = BTreeMap::from([(EId(u128::MAX), CanonicalScalar::Int(i64::MIN))]);
        let stream = q.stream_governed_with_edge_properties(1, 3, [source, target],
            [(EId(u128::MAX), source, R, target)], |id, _| Ok::<_, ()>(costs.get(&id)),
            policy(), || Ok::<_, ()>(())).unwrap();
        costs.clear();
        stream
    };
    let debug = format!("{stream:?}");
    assert!(!debug.contains(&u128::MAX.to_string()));
    let row = stream.next_with_checkpoint(|| Ok::<_, ()>(())).unwrap().unwrap();
    assert_eq!(row.cost(), i128::from(i64::MIN));
    assert_eq!(row.path().steps(), &[(EId(u128::MAX), target)]);
}
