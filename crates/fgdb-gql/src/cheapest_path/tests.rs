use super::*;

type Edge = (EId, VId, RelationId, VId);
type Answer = (i128, Vec<(EId, VId)>);
const RELATION: RelationId = RelationId(7);
const WEIGHT: PropertyKeyId = PropertyKeyId(9);

fn prepared(
    source: u128,
    target: u128,
    direction: GlaDirection,
    min: u32,
    max: u32,
) -> PreparedGraphCheapestPath {
    PreparedGraphCheapestPath::new(
        VId(source),
        VId(target),
        RELATION,
        direction,
        WEIGHT,
        GraphWalkBounds::new(min, max).unwrap(),
    )
    .unwrap()
}

fn answer(value: Option<GraphCostPath>) -> Option<Answer> {
    value.map(|value| (value.cost(), value.path().steps().to_vec()))
}

// Independent oracle: enumerate EVERY walk, retain its full edge-identity
// sequence, and minimize only after the complete finite bag is constructed.
fn oracle(
    query: &PreparedGraphCheapestPath,
    edges: &[Edge],
    weights: &BTreeMap<EId, CanonicalScalar>,
) -> Option<Answer> {
    let mut layer = vec![(query.source(), 0_i128, Vec::<(EId, VId)>::new())];
    let mut candidates = Vec::new();
    for depth in 0..=query.bounds().maximum() {
        if depth >= query.bounds().minimum() {
            for (vertex, cost, steps) in &layer {
                if *vertex == query.target() {
                    candidates.push((*cost, steps.clone()));
                }
            }
        }
        let mut next = Vec::new();
        for (vertex, cost, steps) in layer {
            for &(edge, left, relation, right) in edges {
                if relation != query.relation() {
                    continue;
                }
                let destinations: Vec<_> = match query.direction() {
                    GlaDirection::Forward => {
                        (left == vertex).then_some(right).into_iter().collect()
                    }
                    GlaDirection::Reverse => {
                        (right == vertex).then_some(left).into_iter().collect()
                    }
                    GlaDirection::Undirected => {
                        let mut destinations = Vec::new();
                        if left == vertex {
                            destinations.push(right);
                        }
                        if right == vertex && left != right {
                            destinations.push(left);
                        }
                        destinations
                    }
                };
                let CanonicalScalar::Int(weight) = weights[&edge] else {
                    panic!("oracle integer fixture");
                };
                for destination in destinations {
                    let mut child = steps.clone();
                    child.push((edge, destination));
                    next.push((destination, cost + i128::from(weight), child));
                }
            }
        }
        layer = next;
    }
    candidates.into_iter().min()
}

#[test]
fn exhaustive_signed_multigraphs_match_full_walk_enumeration_in_every_direction() {
    // Absent, negative, zero and positive choices independently per edge.
    // Includes a self-loop, parallel edges, a cycle and a disconnected vertex.
    let topology = [(0, 0), (0, 1), (0, 1), (1, 0), (1, 1)];
    for mut code in 0..4_usize.pow(topology.len() as u32) {
        let mut edges = Vec::new();
        let mut weights = BTreeMap::new();
        for (at, &(left, right)) in topology.iter().enumerate() {
            let choice = code % 4;
            code /= 4;
            if choice == 0 {
                continue;
            }
            let edge = EId(at as u128 + 1);
            edges.push((edge, VId(left), RELATION, VId(right)));
            weights.insert(edge, CanonicalScalar::Int(choice as i64 - 2));
        }
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for source in 0..=2 {
                for target in 0..=2 {
                    for maximum in 0..=3 {
                        for minimum in 0..=maximum {
                            let query = prepared(source, target, direction, minimum, maximum);
                            let result = query
                                .execute_with_control(
                                    [VId(0), VId(1), VId(2)],
                                    edges.iter().copied(),
                                    |edge, key| {
                                        assert_eq!(key, WEIGHT);
                                        Ok::<_, ()>(weights.get(&edge))
                                    },
                                    |_| Ok(()),
                                )
                                .unwrap();
                            assert_eq!(answer(result), oracle(&query, &edges, &weights));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn costs_dominate_hops_but_equal_costs_use_path_lex_not_shortest_length() {
    let edges = [
        (EId(9), VId(0), RELATION, VId(1)),
        (EId(1), VId(0), RELATION, VId(0)),
    ];
    let weights = BTreeMap::from([
        (EId(9), CanonicalScalar::Int(7)),
        (EId(1), CanonicalScalar::Int(0)),
    ]);
    let query = prepared(0, 1, GlaDirection::Forward, 1, 3);
    let run = |edges: Vec<Edge>| {
        query
            .execute_with_control(
                [VId(0), VId(1)],
                edges,
                |edge, _| Ok::<_, ()>(weights.get(&edge)),
                |_| Ok(()),
            )
            .unwrap()
    };
    let expected = Some((
        7,
        vec![(EId(1), VId(0)), (EId(1), VId(0)), (EId(9), VId(1))],
    ));
    assert_eq!(answer(run(edges.to_vec())), expected);
    assert_eq!(answer(run(edges.into_iter().rev().collect())), expected);
    // For the same source/target, the empty path is a strict lexicographic
    // prefix of every zero-cost cycle and must win when it is admitted.
    let identity = prepared(0, 0, GlaDirection::Forward, 0, 3);
    assert_eq!(
        answer(
            identity
                .execute_with_control(
                    [VId(0), VId(1)],
                    edges,
                    |edge, _| Ok::<_, ()>(weights.get(&edge)),
                    |_| Ok(())
                )
                .unwrap()
        ),
        Some((0, vec![]))
    );
}

#[test]
fn lower_hop_bounds_negative_cycles_and_wide_exact_costs_do_not_settle_vertices() {
    let edges = [
        (EId(1), VId(0), RELATION, VId(1)),
        (EId(2), VId(1), RELATION, VId(1)),
    ];
    let weights = BTreeMap::from([
        (EId(1), CanonicalScalar::Int(i64::MAX)),
        (EId(2), CanonicalScalar::Int(i64::MIN)),
    ]);
    let query = prepared(0, 1, GlaDirection::Forward, 2, 4);
    let result = query
        .execute_with_control(
            [VId(0), VId(1)],
            edges,
            |edge, _| Ok::<_, ()>(weights.get(&edge)),
            |_| Ok(()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        result.cost(),
        i128::from(i64::MAX) + 3 * i128::from(i64::MIN)
    );
    assert_eq!(result.path().len(), 4);
    let positive = CanonicalScalar::Int(i64::MAX);
    let exact = prepared(0, 1, GlaDirection::Forward, 4, 4)
        .execute_with_control(
            [VId(0), VId(1)],
            edges,
            |_, _| Ok::<_, ()>(Some(&positive)),
            |_| Ok(()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(exact.cost(), 4 * i128::from(i64::MAX));
}

#[test]
fn exponential_parallel_walks_share_states_and_capture_only_the_selected_path() {
    let mut edges = Vec::new();
    for depth in 0..40_u128 {
        for parallel in 0..2 {
            edges.push((
                EId(depth * 2 + parallel),
                VId(depth),
                RELATION,
                VId(depth + 1),
            ));
        }
    }
    let weight = CanonicalScalar::Int(1);
    let query = prepared(0, 40, GlaDirection::Forward, 40, 40);
    let execution = query
        .execute_governed_with_edge_properties(
            121,
            (0..=40).map(VId),
            edges,
            |_, _| Ok::<_, ()>(Some(&weight)),
            GqlQueryPolicy::new(121, 1, 5_000, 5_000),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(execution.value[0].cost(), 40);
    assert_eq!(
        execution.value[0].path().edges().collect::<Vec<_>>(),
        (0..40).map(|depth| EId(depth * 2)).collect::<Vec<_>>()
    );
    assert_eq!(execution.rows.result_rows, 1);
    assert!(execution.evaluator.work_units < 5_000);
}

#[test]
fn maximum_hop_depth_is_iterative_and_does_not_overflow_the_integer_cost_domain() {
    let weight = CanonicalScalar::Int(i64::MIN);
    let query = prepared(
        0,
        0,
        GlaDirection::Forward,
        crate::MAX_GRAPH_WALK_HOPS,
        crate::MAX_GRAPH_WALK_HOPS,
    );
    let result = query
        .execute_with_control(
            [VId(0)],
            [(EId(1), VId(0), RELATION, VId(0))],
            |_, _| Ok::<_, ()>(Some(&weight)),
            |_| Ok(()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(result.path().len(), crate::MAX_GRAPH_WALK_HOPS as usize);
    assert_eq!(
        result.cost(),
        i128::from(i64::MIN) * i128::from(crate::MAX_GRAPH_WALK_HOPS)
    );
}

#[test]
fn malformed_weights_topology_and_source_errors_refuse_without_substitution() {
    let query = prepared(0, 1, GlaDirection::Forward, 0, 2);
    let edge = (EId(1), VId(0), RELATION, VId(1));
    assert_eq!(
        query.execute_with_control(
            [VId(0), VId(1)],
            [edge],
            |_, _| Ok::<_, ()>(None),
            |_| Ok(())
        ),
        Err(GraphCheapestPathError::Cost(
            GraphPathCostError::MissingWeight
        ))
    );
    assert_eq!(
        query.execute_with_control(
            [VId(0), VId(1)],
            [edge],
            |_, _| Ok::<_, ()>(Some(&CanonicalScalar::Null)),
            |_| Ok(())
        ),
        Err(GraphCheapestPathError::Cost(
            GraphPathCostError::NonIntegerWeight
        ))
    );
    let weight = CanonicalScalar::Int(1);
    assert_eq!(
        query.execute_with_control(
            [VId(0), VId(1)],
            [edge, edge],
            |_, _| Ok::<_, ()>(Some(&weight)),
            |_| Ok(())
        ),
        Err(GraphCheapestPathError::Cost(
            GraphPathCostError::DuplicateEdge
        ))
    );
    assert_eq!(
        query.execute_with_control(
            [VId(0)],
            [edge],
            |_, _| Ok::<_, ()>(Some(&weight)),
            |_| Ok(())
        ),
        Err(GraphCheapestPathError::Cost(
            GraphPathCostError::DanglingEndpoint
        ))
    );
    assert_eq!(
        query.execute_with_control([VId(0), VId(1)], [edge], |_, _| Err(19), |_| Ok(())),
        Err(GraphCheapestPathError::Source(19))
    );
    let unreachable = (EId(2), VId(2), RELATION, VId(2));
    assert_eq!(
        query.execute_with_control(
            [VId(0), VId(1), VId(2)],
            [unreachable],
            |_, _| Ok::<_, ()>(None),
            |_| Ok(())
        ),
        Err(GraphCheapestPathError::Cost(
            GraphPathCostError::MissingWeight
        ))
    );
    // A different relation is not part of this PathFind's cost domain.
    assert_eq!(
        query
            .execute_with_control(
                [VId(0), VId(1)],
                [(EId(2), VId(0), RelationId(99), VId(1))],
                |_, _| -> Result<Option<&CanonicalScalar>, ()> { panic!("unrelated cost read") },
                |_| Ok(())
            )
            .unwrap(),
        None
    );
    assert_eq!(
        query
            .execute_with_control([], [], |_, _| Ok::<_, ()>(None), |_| Ok(()))
            .unwrap(),
        None
    );
}

#[test]
fn every_control_refusal_and_every_exact_limit_is_retryable_without_partial_output() {
    let query = prepared(0, 1, GlaDirection::Undirected, 0, 3);
    let edges = [
        (EId(7), VId(0), RELATION, VId(1)),
        (EId(2), VId(0), RELATION, VId(0)),
    ];
    let weights = BTreeMap::from([
        (EId(7), CanonicalScalar::Int(4)),
        (EId(2), CanonicalScalar::Int(-1)),
    ]);
    let policy = GqlQueryPolicy::new(4, 1, 100_000, 100_000);
    let mut calls = 0;
    let baseline = query
        .execute_governed_with_edge_properties(
            4,
            [VId(0), VId(1)],
            edges,
            |edge, _| Ok::<_, ()>(weights.get(&edge)),
            policy,
            || {
                calls += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    for stop in 1..=calls {
        let mut at = 0;
        let failed = query.execute_governed_with_edge_properties(
            4,
            [VId(0), VId(1)],
            edges,
            |edge, _| Ok::<_, ()>(weights.get(&edge)),
            policy,
            || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(failed, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
        let retry = query
            .execute_governed_with_edge_properties(
                4,
                [VId(0), VId(1)],
                edges,
                |edge, _| Ok::<_, ()>(weights.get(&edge)),
                policy,
                || Ok::<_, usize>(()),
            )
            .unwrap();
        assert_eq!(retry, baseline);
    }
    let exact = GqlQueryPolicy::new(
        4,
        1,
        baseline.evaluator.work_units,
        baseline.evaluator.scratch_entries,
    );
    let run = |policy| {
        query.execute_governed_with_edge_properties(
            4,
            [VId(0), VId(1)],
            edges,
            |edge, _| Ok::<_, ()>(weights.get(&edge)),
            policy,
            || Ok::<_, ()>(()),
        )
    };
    assert_eq!(run(exact).unwrap(), baseline);
    assert!(matches!(
        run(GqlQueryPolicy::new(3, 1, 100_000, 100_000)),
        Err(GqlQueryError::Rows(_))
    ));
    assert!(matches!(
        run(GqlQueryPolicy::new(4, 0, 100_000, 100_000)),
        Err(GqlQueryError::Rows(_))
    ));
    assert!(matches!(
        run(GqlQueryPolicy::new(
            4,
            1,
            exact.evaluator.max_work_units - 1,
            100_000
        )),
        Err(GqlQueryError::Evaluator(_))
    ));
    assert!(matches!(
        run(GqlQueryPolicy::new(
            4,
            1,
            100_000,
            exact.evaluator.max_scratch_entries - 1
        )),
        Err(GqlQueryError::Evaluator(_))
    ));
}

#[test]
fn definition_identity_binds_every_operand_and_debug_redacts_data() {
    let query = prepared(0, 1, GlaDirection::Forward, 0, 3);
    let base = query.canonical_bytes();
    for other in [
        prepared(2, 1, GlaDirection::Forward, 0, 3),
        prepared(0, 2, GlaDirection::Forward, 0, 3),
        prepared(0, 1, GlaDirection::Reverse, 0, 3),
        prepared(0, 1, GlaDirection::Forward, 1, 3),
        prepared(0, 1, GlaDirection::Forward, 0, 4),
        PreparedGraphCheapestPath::new(
            VId(0),
            VId(1),
            RelationId(8),
            GlaDirection::Forward,
            WEIGHT,
            query.bounds(),
        )
        .unwrap(),
        PreparedGraphCheapestPath::new(
            VId(0),
            VId(1),
            RELATION,
            GlaDirection::Forward,
            PropertyKeyId(10),
            query.bounds(),
        )
        .unwrap(),
    ] {
        assert_ne!(base, other.canonical_bytes());
    }
    assert!(format!("{query:?}").contains("[REDACTED]"));
    assert!(query.input_pattern().preserves_duplicates());
}
