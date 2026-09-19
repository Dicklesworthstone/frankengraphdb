use super::*;

const RELATION: RelationId = RelationId(7);
const WEIGHT: PropertyKeyId = PropertyKeyId(9);
type Edge = (EId, VId, RelationId, VId);
type Answer = (i128, Vec<Step>);

fn query(source: u128, target: u128, direction: GlaDirection, lo: u32, hi: u32) -> PreparedGraphCheapestPath {
    PreparedGraphCheapestPath::new(VId(source), VId(target), RELATION, direction, WEIGHT,
        GraphWalkBounds::new(lo, hi).unwrap()).unwrap()
}
fn plain(row: &GraphCostPath) -> Answer { (row.cost(), row.path().steps().to_vec()) }

// No suffix states, minimum-prefix pruning, heap or partitioning: construct the
// entire bounded bag of real edge-identified walks, then sort the finished bag.
fn oracle(query: &PreparedGraphCheapestPath, edges: &[Edge], weights: &BTreeMap<EId, CanonicalScalar>) -> Vec<Answer> {
    let mut layer = vec![(query.source(), 0_i128, Vec::<Step>::new())];
    let mut answers = Vec::new();
    for depth in 0..=query.bounds().maximum() {
        if depth >= query.bounds().minimum() {
            answers.extend(layer.iter().filter(|(end, _, _)| *end == query.target())
                .map(|(_, cost, steps)| (*cost, steps.clone())));
        }
        if depth == query.bounds().maximum() { break; }
        let mut next = Vec::new();
        for (end, cost, steps) in layer {
            for &(edge, left, relation, right) in edges {
                if relation != RELATION { continue; }
                let CanonicalScalar::Int(weight) = &weights[&edge] else { panic!("integer fixture"); };
                let mut destinations = Vec::new();
                if query.direction() != GlaDirection::Reverse && left == end { destinations.push(right); }
                if query.direction() != GlaDirection::Forward && right == end
                    && (query.direction() != GlaDirection::Undirected || left != right) {
                    destinations.push(left);
                }
                for destination in destinations {
                    let mut child = steps.clone(); child.push((edge, destination));
                    next.push((destination, cost + i128::from(*weight), child));
                }
            }
        }
        layer = next;
    }
    answers.sort();
    answers
}

#[test]
fn exhaustive_ranked_signed_walk_bags_equal_independent_enumeration() {
    let topology = [(0, 0), (0, 1), (0, 1), (1, 0)];
    for mut code in 0..4_usize.pow(topology.len() as u32) {
        let mut edges = Vec::new();
        let mut weights = BTreeMap::new();
        for (at, &(left, right)) in topology.iter().enumerate() {
            let choice = code % 4; code /= 4;
            if choice != 0 {
                let edge = EId(at as u128 + 1);
                edges.push((edge, VId(left), RELATION, VId(right)));
                weights.insert(edge, CanonicalScalar::Int(choice as i64 - 2));
            }
        }
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            for source in 0..2 {
                for target in 0..2 {
                    for maximum in 0..=3 {
                        for minimum in 0..=maximum {
                            let query = query(source, target, direction, minimum, maximum);
                            let expected = oracle(&query, &edges, &weights);
                            let actual = query.execute_k_with_control(u64::MAX, [VId(0), VId(1)], edges.iter().copied(),
                                |edge, key| { assert_eq!(key, WEIGHT); Ok::<_, ()>(weights.get(&edge)) }, |_| Ok(())).unwrap();
                            assert_eq!(actual.iter().map(plain).collect::<Vec<_>>(), expected);
                            let first = query.execute_with_control([VId(0), VId(1)], edges.iter().copied(),
                                |edge, _| Ok::<_, ()>(weights.get(&edge)), |_| Ok(())).unwrap();
                            assert_eq!(first.as_ref().map(plain), expected.first().cloned());
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn stopping_and_continuing_at_a_target_are_distinct_partitions() {
    let edges = [(EId(2), VId(0), RELATION, VId(0)), (EId(9), VId(0), RELATION, VId(1)),
        (EId(3), VId(1), RELATION, VId(1)), (EId(1), VId(1), RELATION, VId(0))];
    for weight in [-1, 0, 1, i64::MIN, i64::MAX] {
        let weights: BTreeMap<_, _> = edges.iter().map(|(id, ..)| (*id, CanonicalScalar::Int(weight))).collect();
        for (start, end) in [(0, 0), (0, 1)] {
            let query = query(start, end, GlaDirection::Forward, 0, 4);
            let expected = oracle(&query, &edges, &weights);
            for count in [0, 1, 2, 7, u64::MAX] {
                let actual = query.execute_k_with_control(count, [VId(0), VId(1)], edges,
                    |edge, _| Ok::<_, ()>(weights.get(&edge)), |_| Ok(())).unwrap();
                let take = usize::try_from(count).unwrap_or(usize::MAX).min(expected.len());
                assert_eq!(actual.iter().map(plain).collect::<Vec<_>>(), expected[..take]);
            }
        }
    }
}

#[test]
fn cursor_owns_admitted_costs_preserves_wide_id_ties_and_redacts_state() {
    let edges = [(EId(u128::MAX), VId(0), RELATION, VId(u128::MAX)),
        (EId(1_u128 << 100), VId(0), RELATION, VId(u128::MAX))];
    let mut weights = BTreeMap::from([(edges[0].0, CanonicalScalar::Int(5)), (edges[1].0, CanonicalScalar::Int(5))]);
    let query = query(0, u128::MAX, GlaDirection::Forward, 1, 1);
    let mut cursor = query.cursor_with_control([VId(0), VId(u128::MAX)], edges.into_iter().rev(),
        |edge, _| Ok::<_, ()>(weights.get(&edge)), |_| Ok(())).unwrap();
    weights.insert(edges[0].0, CanonicalScalar::Int(-20));
    assert!(format!("{cursor:?}").contains("[REDACTED]"));
    assert_eq!(plain(&cursor.next_with_control(|_| Ok::<_, ()>(())).unwrap().unwrap()), (5, vec![(edges[1].0, VId(u128::MAX))]));
    assert_eq!(plain(&cursor.next_with_control(|_| Ok::<_, ()>(())).unwrap().unwrap()), (5, vec![(edges[0].0, VId(u128::MAX))]));
    assert!(cursor.next_with_control(|_| Ok::<_, ()>(())).unwrap().is_none());
    assert!(cursor.is_exhausted());
    cursor.close(); cursor.close();
    assert!(cursor.next_with_control(|_| -> Result<(), ()> { panic!("closed cursor resumed"); }).unwrap().is_none());
    let fresh = query.execute_k_with_control(1, [VId(0), VId(u128::MAX)], edges,
        |edge, _| Ok::<_, ()>(weights.get(&edge)), |_| Ok(())).unwrap();
    assert_eq!(fresh[0].cost(), -20);
}

#[test]
fn first_four_of_two_to_the_fortieth_walks_do_not_enumerate_the_result_bag() {
    let mut edges = Vec::new();
    for depth in 0..40_u128 {
        for parallel in 0..2 { edges.push((EId(2 * depth + parallel), VId(depth), RELATION, VId(depth + 1))); }
    }
    let weight = CanonicalScalar::Int(1);
    let query = query(0, 40, GlaDirection::Forward, 40, 40);
    let mut work = 0;
    let mut control = |_| { work += 1; Ok::<_, ()>(()) };
    let mut cursor = query.cursor_with_control((0..=40).map(VId), edges.iter().copied(),
        |_, _| Ok(Some(&weight)), &mut control).unwrap();
    let first = cursor.next_with_control(&mut control).unwrap().unwrap();
    assert_eq!(first.cost(), 40);
    assert!(work < 5_000, "the first answer must not split its alternatives");
    assert!(cursor.search.as_ref().unwrap().heap.is_empty());
    assert!(cursor.search.as_ref().unwrap().pending.is_some());
    for ordinal in 1..4_u128 {
        let row = cursor.next_with_control(|_| Ok::<_, ()>(())).unwrap().unwrap();
        assert_eq!(row.path().steps(), (0..40_u128).map(|depth|
            (EId(depth * 2 + ((ordinal >> (39 - depth)) & 1)), VId(depth + 1))).collect::<Vec<_>>());
    }
    let result = query.execute_k_governed_with_edge_properties(4, 121, (0..=40).map(VId), edges,
        |_, _| Ok::<_, ()>(Some(&weight)), GqlQueryPolicy::new(121, 4, 50_000, 50_000), || Ok::<_, ()>(())).unwrap();
    assert_eq!(result.value.len(), 4);
    assert_eq!(result.rows.result_rows, 4);
}

#[test]
fn every_refusal_closes_all_state_and_a_fresh_cursor_retries_exactly() {
    let edges = [(EId(7), VId(0), RELATION, VId(1)), (EId(2), VId(0), RELATION, VId(0)),
        (EId(8), VId(1), RELATION, VId(0))];
    let weights = BTreeMap::from([(EId(7), CanonicalScalar::Int(4)), (EId(2), CanonicalScalar::Int(-1)), (EId(8), CanonicalScalar::Int(2))]);
    let query = query(0, 1, GlaDirection::Undirected, 0, 3);
    let mut total = 0;
    let baseline = query.execute_k_with_control(u64::MAX, [VId(0), VId(1)], edges,
        |edge, _| Ok::<_, usize>(weights.get(&edge)), |_| { total += 1; Ok(()) }).unwrap();
    for stop in 1..=total {
        let mut calls = 0;
        let mut control = |_| { calls += 1; if calls == stop { Err(stop) } else { Ok(()) } };
        match query.cursor_with_control([VId(0), VId(1)], edges,
            |edge, _| Ok(weights.get(&edge)), &mut control) {
            Err(GraphCheapestPathError::Source(at)) => assert_eq!(at, stop),
            Err(error) => panic!("unexpected domain error {error:?}"),
            Ok(mut cursor) => {
                loop {
                    match cursor.next_with_control(&mut control) {
                        Ok(Some(_)) => {}
                        Ok(None) => panic!("missed refusal boundary {stop}"),
                        Err(error) => { assert_eq!(error, GraphCheapestPathError::Source(stop)); break; }
                    }
                }
                assert!(cursor.search.is_none());
                assert!(cursor.next_with_control(|_| -> Result<(), usize> { panic!("refused cursor resumed"); }).unwrap().is_none());
            }
        }
        assert_eq!(calls, stop);
        let retry = query.execute_k_with_control(u64::MAX, [VId(0), VId(1)], edges,
            |edge, _| Ok::<_, usize>(weights.get(&edge)), |_| Ok(())).unwrap();
        assert_eq!(retry, baseline);
    }
}

#[test]
fn exact_combined_limits_count_only_the_requested_prefix_and_refuse_atomically() {
    let query = query(0, 0, GlaDirection::Forward, 0, 4);
    let edges = [(EId(1), VId(0), RELATION, VId(0)), (EId(2), VId(0), RELATION, VId(0))];
    let weight = CanonicalScalar::Int(0);
    let run = |count, policy| query.execute_k_governed_with_edge_properties(count, 3, [VId(0)], edges,
        |_, _| Ok::<_, ()>(Some(&weight)), policy, || Ok::<_, ()>(()));
    let baseline = run(5, GqlQueryPolicy::new(3, 5, u64::MAX, u64::MAX)).unwrap();
    let exact = GqlQueryPolicy::new(3, 5, baseline.evaluator.work_units, baseline.evaluator.scratch_entries);
    assert_eq!(run(5, exact).unwrap(), baseline);
    assert!(matches!(run(5, GqlQueryPolicy::new(2, 5, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(_))));
    assert!(matches!(run(5, GqlQueryPolicy::new(3, 4, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(_))));
    assert!(matches!(run(5, GqlQueryPolicy::new(3, 5, exact.evaluator.max_work_units - 1, u64::MAX)), Err(GqlQueryError::Evaluator(_))));
    assert!(matches!(run(5, GqlQueryPolicy::new(3, 5, u64::MAX, exact.evaluator.max_scratch_entries - 1)), Err(GqlQueryError::Evaluator(_))));
    assert_eq!(run(0, GqlQueryPolicy::new(3, 0, u64::MAX, u64::MAX)).unwrap().rows.result_rows, 0);
    let mut total = 0;
    query.execute_k_governed_with_edge_properties(5, 3, [VId(0)], edges,
        |_, _| Ok::<_, ()>(Some(&weight)), exact, || { total += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 1..=total {
        let mut calls = 0;
        let result = query.execute_k_governed_with_edge_properties(5, 3, [VId(0)], edges,
            |_, _| Ok::<_, ()>(Some(&weight)), exact,
            || { calls += 1; if calls == stop { Err(stop) } else { Ok(()) } });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
    }
}

#[test]
fn source_admission_is_not_bypassed_by_zero_count_or_missing_anchors() {
    let missing = query(99, 100, GlaDirection::Forward, 0, 0);
    let edge = (EId(1), VId(0), RELATION, VId(1));
    for count in [0, 1] {
        assert_eq!(missing.execute_k_with_control(count, [VId(0), VId(1)], [edge],
            |_, _| Ok::<_, ()>(None), |_| Ok(())), Err(GraphCheapestPathError::Cost(GraphPathCostError::MissingWeight)));
        assert_eq!(missing.execute_k_with_control(count, [VId(0), VId(1)], [edge],
            |_, _| Ok::<_, ()>(Some(&CanonicalScalar::Null)), |_| Ok(())), Err(GraphCheapestPathError::Cost(GraphPathCostError::NonIntegerWeight)));
    }
    let isolate = query(7, 7, GlaDirection::Forward, 0, 8).execute_k_with_control(u64::MAX,
        [VId(7)], [], |_, _| Ok::<_, ()>(None), |_| Ok(())).unwrap();
    assert_eq!(isolate[0].cost(), 0);
    assert!(isolate[0].path().is_empty());
}

#[test]
fn maximum_depth_is_iterative_and_keeps_exact_signed_costs() {
    let query = query(0, 0, GlaDirection::Forward, crate::MAX_GRAPH_WALK_HOPS, crate::MAX_GRAPH_WALK_HOPS);
    let weight = CanonicalScalar::Int(i64::MIN);
    let mut cursor = query.cursor_with_control([VId(0)], [(EId(1), VId(0), RELATION, VId(0))],
        |_, _| Ok::<_, ()>(Some(&weight)), |_| Ok(())).unwrap();
    let row = cursor.next_with_control(|_| Ok::<_, ()>(())).unwrap().unwrap();
    assert_eq!(row.cost(), i128::from(i64::MIN) * i128::from(crate::MAX_GRAPH_WALK_HOPS));
    assert_eq!(row.path().len(), crate::MAX_GRAPH_WALK_HOPS as usize);
    assert!(cursor.next_with_control(|_| Ok::<_, ()>(())).unwrap().is_none());
}
