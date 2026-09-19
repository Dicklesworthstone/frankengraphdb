use super::*;

const R: RelationId = RelationId(7);
const W: PropertyKeyId = PropertyKeyId(9);
const MODES: [GraphCheapestPathMode; 4] = [
    GraphCheapestPathMode::Walk, GraphCheapestPathMode::Trail,
    GraphCheapestPathMode::Acyclic, GraphCheapestPathMode::Simple,
];
type Edge = (EId, VId, RelationId, VId);
type Answer = (i128, Vec<Step>);

fn query(source: u128, target: u128, direction: GlaDirection, lo: u32, hi: u32) -> PreparedGraphCheapestPath {
    PreparedGraphCheapestPath::new(VId(source), VId(target), R, direction, W,
        GraphWalkBounds::new(lo, hi).unwrap()).unwrap()
}
fn plain(rows: &[GraphCostPath]) -> Vec<Answer> {
    rows.iter().map(|row| (row.cost(), row.path().steps().to_vec())).collect()
}

// Independent complete-route predicates: do not call History, relaxed suffix
// DP, partition refinement, or pathfinder membership checks from the oracle.
fn permitted(mode: GraphCheapestPathMode, source: VId, steps: &[Step]) -> bool {
    if mode == GraphCheapestPathMode::Walk { return true; }
    if mode == GraphCheapestPathMode::Trail {
        return steps.iter().map(|step| step.0).collect::<BTreeSet<_>>().len() == steps.len();
    }
    let mut vertices: Vec<_> = std::iter::once(source).chain(steps.iter().map(|step| step.1)).collect();
    if mode == GraphCheapestPathMode::Simple && vertices.len() > 1 && vertices.last() == Some(&source) {
        vertices.pop();
    }
    vertices.iter().collect::<BTreeSet<_>>().len() == vertices.len()
}
fn oracle(q: &PreparedGraphCheapestPath, edges: &[Edge], weights: &BTreeMap<EId, CanonicalScalar>) -> Vec<Answer> {
    let mut layer = vec![(q.source(), 0_i128, Vec::<Step>::new())];
    let mut answers = Vec::new();
    for depth in 0..=q.bounds().maximum() {
        if depth >= q.bounds().minimum() {
            answers.extend(layer.iter().filter(|(end, _, steps)|
                *end == q.target() && permitted(q.mode(), q.source(), steps))
                .map(|(_, cost, steps)| (*cost, steps.clone())));
        }
        if depth == q.bounds().maximum() { break; }
        let mut next = Vec::new();
        for (end, cost, steps) in layer {
            for &(eid, left, relation, right) in edges {
                if relation != R { continue; }
                let CanonicalScalar::Int(weight) = &weights[&eid] else { panic!("integer fixture"); };
                let mut destinations = Vec::new();
                if q.direction() != GlaDirection::Reverse && left == end { destinations.push(right); }
                if q.direction() != GlaDirection::Forward && right == end
                    && (q.direction() != GlaDirection::Undirected || left != right) { destinations.push(left); }
                for destination in destinations {
                    let mut child = steps.clone(); child.push((eid, destination));
                    next.push((destination, cost + i128::from(*weight), child));
                }
            }
        }
        layer = next;
    }
    answers.sort(); answers
}

#[test]
fn exhaustive_signed_mode_ranking_matches_complete_walks_filtered_only_at_the_end() {
    let topology = [(0, 0), (0, 1), (0, 1), (1, 0)];
    for mut code in 0..256 {
        let mut edges = Vec::new(); let mut weights = BTreeMap::new();
        for (id, &(src, dst)) in topology.iter().enumerate() {
            let choice = code % 4; code /= 4;
            if choice != 0 {
                let eid = EId(id as u128);
                edges.push((eid, VId(src), R, VId(dst)));
                weights.insert(eid, CanonicalScalar::Int(choice as i64 - 2));
            }
        }
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            for source in 0..2 {
                for target in 0..2 {
                    for maximum in 0..=3 {
                        for minimum in 0..=maximum {
                            for mode in MODES {
                                let q = query(source, target, direction, minimum, maximum).with_mode(mode);
                                let expected = oracle(&q, &edges, &weights);
                                let actual = q.execute_k_with_control(u64::MAX, [VId(0), VId(1)], edges.iter().copied(),
                                    |eid, _| Ok::<_, ()>(weights.get(&eid)), |_| Ok(())).unwrap();
                                assert_eq!(plain(&actual), expected, "mode={mode:?}");
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn a_more_expensive_prefix_at_the_same_depth_and_vertex_can_be_the_only_legal_answer() {
    for mode in &MODES[1..] {
        let closing = if *mode == GraphCheapestPathMode::Trail { (7, 3, 0, 0) } else { (5, 3, 1, 0) };
        let fixture = [(1, 0, 1, 0), (2, 1, 3, 0), (3, 0, 2, 5), (4, 2, 3, 0), closing, (6, 1, 4, 0)];
        let edges: Vec<_> = fixture.iter().map(|&(id, a, b, _)| (EId(id), VId(a), R, VId(b))).collect();
        let weights = fixture.iter().map(|&(id, _, _, w)| (EId(id), CanonicalScalar::Int(w))).collect::<BTreeMap<_, _>>();
        let depth = if *mode == GraphCheapestPathMode::Trail { 5 } else { 4 };
        let q = query(0, 4, GlaDirection::Forward, depth, depth).with_mode(*mode);
        let expected = oracle(&q, &edges, &weights);
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].0, 5);
        assert_eq!(expected[0].1[0].0, EId(3));
        let result = q.execute_k_with_control(10, (0..5).map(VId), edges.iter().copied(),
            |eid, _| Ok::<_, ()>(weights.get(&eid)), |_| Ok(())).unwrap();
        assert_eq!(plain(&result), expected);
        let one = q.execute_with_control((0..5).map(VId), edges.iter().copied(),
            |eid, _| Ok::<_, ()>(weights.get(&eid)), |_| Ok(())).unwrap().unwrap();
        assert_eq!(one, result[0]);
    }
}

#[test]
fn simple_closure_is_terminal_but_is_not_edge_unique_trail() {
    let edges = [(EId(1), VId(0), R, VId(1))];
    let weight = CanonicalScalar::Int(-1);
    for (mode, expected) in [(GraphCheapestPathMode::Trail, 0), (GraphCheapestPathMode::Acyclic, 0), (GraphCheapestPathMode::Simple, 1)] {
        let q = query(0, 0, GlaDirection::Undirected, 2, 4).with_mode(mode);
        let rows = q.execute_k_with_control(u64::MAX, [VId(0), VId(1)], edges,
            |_, _| Ok::<_, ()>(Some(&weight)), |_| Ok(())).unwrap();
        assert_eq!(rows.len(), expected);
        if expected != 0 {
            assert_eq!(plain(&rows), vec![(-2, vec![(EId(1), VId(1)), (EId(1), VId(0))])]);
        }
    }
    let q = query(0, 0, GlaDirection::Undirected, 3, 4).with_mode(GraphCheapestPathMode::Simple);
    assert!(q.execute_k_with_control(10, [VId(0), VId(1)], edges,
        |_, _| Ok::<_, ()>(Some(&weight)), |_| Ok(())).unwrap().is_empty());
}

#[test]
fn mode_identity_is_distinct_and_default_walk_and_admission_transcripts_are_unchanged() {
    let base = query(0, 3, GlaDirection::Forward, 0, 4);
    let original = base.canonical_bytes();
    assert!(original.starts_with(b"fgdb:path-find:any-cheapest-int64-walk:path-lex:v1\0"));
    assert_eq!(base.clone().with_mode(GraphCheapestPathMode::Walk).canonical_bytes(), original);
    let identities: BTreeSet<_> = MODES.into_iter().map(|mode| {
        let q = base.clone().with_mode(mode);
        assert_eq!(q.mode(), mode);
        // WALK is explicitly a common admission superset, not a claim that
        // the selected mode and cost have already been applied by this plan.
        assert_eq!(q.input_pattern().plan().canonical_bytes(), base.input_pattern().plan().canonical_bytes());
        q.canonical_bytes()
    }).collect();
    assert_eq!(identities.len(), 4);
}

#[test]
fn forbidden_prefix_families_are_pruned_before_enumerating_exponentially_many_walks() {
    let edges = [(EId(1), VId(0), R, VId(0)), (EId(2), VId(0), R, VId(0)), (EId(3), VId(0), R, VId(1))];
    let weights = BTreeMap::from([(EId(1), CanonicalScalar::Int(-2)), (EId(2), CanonicalScalar::Int(-1)), (EId(3), CanonicalScalar::Int(5))]);
    for mode in &MODES[1..] {
        let q = query(0, 1, GlaDirection::Forward, 1, 80).with_mode(*mode);
        let rows = q.execute_k_governed_with_edge_properties(u64::MAX, 5, [VId(0), VId(1)], edges,
            |eid, _| Ok::<_, ()>(weights.get(&eid)), GqlQueryPolicy::new(5, 5, 10_000, 10_000), || Ok::<_, ()>(())).unwrap();
        let expected = if *mode == GraphCheapestPathMode::Trail { vec![2, 2, 3, 4, 5] } else { vec![5] };
        assert_eq!(rows.value.iter().map(GraphCostPath::cost).collect::<Vec<_>>(), expected);
    }
}

#[test]
fn every_history_refusal_is_terminal_and_every_retry_returns_the_same_complete_answer() {
    let edges = [(EId(1), VId(0), R, VId(0)), (EId(2), VId(0), R, VId(1)), (EId(3), VId(1), R, VId(0)), (EId(4), VId(1), R, VId(1))];
    let weights = BTreeMap::from([(EId(1), CanonicalScalar::Int(-1)), (EId(2), CanonicalScalar::Int(4)), (EId(3), CanonicalScalar::Int(0)), (EId(4), CanonicalScalar::Int(-2))]);
    for mode in &MODES[1..] {
        let q = query(0, 0, GlaDirection::Undirected, 0, 4).with_mode(*mode);
        let mut total = 0;
        let baseline = q.execute_k_with_control(u64::MAX, [VId(0), VId(1)], edges,
            |eid, _| Ok::<_, usize>(weights.get(&eid)), |_| { total += 1; Ok(()) }).unwrap();
        for stop in 1..=total {
            let mut calls = 0;
            let mut control = |_| { calls += 1; if calls == stop { Err(stop) } else { Ok(()) } };
            match q.cursor_with_control([VId(0), VId(1)], edges, |eid, _| Ok(weights.get(&eid)), &mut control) {
                Err(error) => assert_eq!(error, GraphCheapestPathError::Source(stop)),
                Ok(mut cursor) => {
                    loop {
                        match cursor.next_with_control(&mut control) {
                            Ok(Some(_)) => {}
                            Ok(None) => panic!("missed refusal {stop} in {mode:?}"),
                            Err(error) => { assert_eq!(error, GraphCheapestPathError::Source(stop)); break; }
                        }
                    }
                    assert!(cursor.search.is_none());
                    assert!(cursor.next_with_control(|_| -> Result<(), usize> { panic!("refused cursor resumed"); }).unwrap().is_none());
                }
            }
            assert_eq!(calls, stop);
            assert_eq!(q.execute_k_with_control(u64::MAX, [VId(0), VId(1)], edges,
                |eid, _| Ok::<_, usize>(weights.get(&eid)), |_| Ok(())).unwrap(), baseline);
        }
    }
}

#[test]
fn mode_budgets_are_cumulative_exact_and_zero_count_still_validates_costs() {
    let edges = [(EId(1), VId(0), R, VId(0)), (EId(2), VId(0), R, VId(1))];
    let weight = CanonicalScalar::Int(-1);
    for mode in &MODES[1..] {
        let q = query(0, 1, GlaDirection::Forward, 1, 4).with_mode(*mode);
        let run = |policy| q.execute_k_governed_with_edge_properties(10, 4, [VId(0), VId(1)], edges,
            |_, _| Ok::<_, ()>(Some(&weight)), policy, || Ok::<_, ()>(()));
        let baseline = run(GqlQueryPolicy::new(4, 10, u64::MAX, u64::MAX)).unwrap();
        let exact = GqlQueryPolicy::new(4, baseline.rows.result_rows, baseline.evaluator.work_units, baseline.evaluator.scratch_entries);
        assert_eq!(run(exact).unwrap(), baseline);
        for policy in [
            GqlQueryPolicy::new(4, baseline.rows.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(4, 10, baseline.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(4, 10, u64::MAX, baseline.evaluator.scratch_entries - 1),
        ] { assert!(run(policy).is_err()); }
        assert_eq!(q.execute_k_with_control(0, [VId(0), VId(1)], edges,
            |_, _| Ok::<_, ()>(None), |_| Ok(())), Err(GraphCheapestPathError::Cost(GraphPathCostError::MissingWeight)));
    }
}

#[test]
fn all_history_modes_handle_the_maximum_depth_iteratively_and_sum_wide_signed_weights() {
    let depth = crate::MAX_GRAPH_WALK_HOPS;
    let edges: Vec<_> = (0..depth).map(|i| (EId(u128::from(i)), VId(u128::from(i)), R, VId(u128::from(i) + 1))).collect();
    let weight = CanonicalScalar::Int(i64::MIN);
    for mode in &MODES[1..] {
        let q = query(0, u128::from(depth), GlaDirection::Forward, depth, depth).with_mode(*mode);
        let rows = q.execute_k_with_control(2, (0..=depth).map(|i| VId(u128::from(i))), edges.iter().copied(),
            |_, _| Ok::<_, ()>(Some(&weight)), |_| Ok(())).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cost(), i128::from(depth) * i128::from(i64::MIN));
        assert_eq!(rows[0].path().len(), depth as usize);
    }
}
