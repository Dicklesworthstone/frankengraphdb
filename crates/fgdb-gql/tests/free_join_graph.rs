use core::convert::Infallible;
use fgdb_gql::GlaExecutionEvent;
use fgdb_gql::free_join::{
    ColumnarTrie, FreeJoin, FreeJoinPlan, GraphTrie, GraphTrieError, JoinVariable as V, TrieCursor,
    TrieRelation,
};
use fgdb_types::VId;
use std::collections::BTreeMap;

fn allow(_: GlaExecutionEvent) -> Result<(), Infallible> {
    Ok(())
}

fn bag<T: TrieRelation<VId>>(plan: &FreeJoinPlan, inputs: &[T]) -> BTreeMap<Vec<VId>, u128> {
    let mut result = BTreeMap::new();
    FreeJoin::new(plan, inputs)
        .unwrap()
        .for_each_binding(&mut allow, |binding, _| {
            assert!(
                result
                    .insert(
                        binding.values().copied().collect(),
                        binding.multiplicity().unwrap()
                    )
                    .is_none()
            );
            Ok(())
        })
        .unwrap();
    result
}

#[test]
fn borrowed_keys_duplicate_runs_and_parent_navigation_are_exact() {
    let adjacency = BTreeMap::from([
        (VId(0), vec![]),
        (VId(1), vec![VId(0), VId(0), VId(u128::MAX)]),
        (VId(u128::MAX), vec![VId(1)]),
    ]);
    let trie = GraphTrie::adjacency([V(0), V(1)], &adjacency, 91, &mut allow).unwrap();
    let mut root = trie.cursor();
    assert_eq!(root.generation(), 91);
    assert_eq!(root.distinct_prefixes(1), 2);
    assert_eq!(root.distinct_prefixes(2), 3);
    assert!(std::ptr::eq(
        root.key().unwrap(),
        adjacency.get_key_value(&VId(1)).unwrap().0
    ));
    let mut child = root.open(&mut allow).unwrap().unwrap();
    assert!(std::ptr::eq(child.key().unwrap(), &adjacency[&VId(1)][0]));
    assert_eq!(child.distinct_prefixes(1), 2);
    assert_eq!(
        child.open(&mut allow).unwrap().unwrap().multiplicity(),
        Some(2)
    );
    child.seek(&VId(u128::MAX), &mut allow).unwrap();
    assert_eq!(
        child.open(&mut allow).unwrap().unwrap().multiplicity(),
        Some(1)
    );
    assert_eq!(root.key(), Some(&VId(1)));
    root.advance(&mut allow).unwrap();
    assert_eq!(root.key(), Some(&VId(u128::MAX)));
    assert_eq!(
        child.key(),
        Some(&VId(u128::MAX)),
        "parent advance cannot invalidate its borrowed child"
    );
}

#[test]
fn vertex_domains_and_self_loops_feed_the_same_factorized_join() {
    let vertices = [VId(1), VId(1), VId(2)];
    let adjacency = BTreeMap::from([
        (VId(1), vec![VId(1), VId(1), VId(2), VId(2), VId(2)]),
        (VId(2), vec![VId(1)]),
    ]);
    let plan = FreeJoinPlan::generic(
        vec![vec![V(0)], vec![V(0)], vec![V(0), V(1)]],
        vec![V(0), V(1)],
    )
    .unwrap();
    let inputs = [
        GraphTrie::vertices(V(0), &vertices, 9, &mut allow).unwrap(),
        GraphTrie::diagonal(V(0), &adjacency, 9, &mut allow).unwrap(),
        GraphTrie::adjacency([V(0), V(1)], &adjacency, 9, &mut allow).unwrap(),
    ];
    assert_eq!(
        bag(&plan, &inputs),
        BTreeMap::from([(vec![VId(1), VId(1)], 4), (vec![VId(1), VId(2)], 6),])
    );
    let factor = FreeJoin::new(&plan, &inputs)
        .unwrap()
        .factorize(&mut allow)
        .unwrap();
    assert_eq!(factor.cardinality(), Ok(10));
    assert_eq!(factor.generations(), &[9, 9, 9]);
    let mut cursor = factor.cursor().unwrap();
    cursor.seek(9);
    let last = cursor.next_batch(1, &mut allow).unwrap();
    assert_eq!(last.columns(), &[vec![VId(1)], vec![VId(2)]]);
}

#[test]
fn borrowed_triangle_matches_owned_tries_across_all_orders_and_bags() {
    let schemas = vec![vec![V(0), V(1)], vec![V(1), V(2)], vec![V(0), V(2)]];
    let orders = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut seed = 19_u64;
    for _ in 0..100 {
        let mut raw = vec![Vec::new(); 3];
        for rows in &mut raw {
            for a in 0..3_u128 {
                for b in 0..3_u128 {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    for _ in 0..(seed >> 32) % 3 {
                        rows.push(vec![VId(a), VId(b)]);
                    }
                }
            }
        }
        for order in orders {
            let plan =
                FreeJoinPlan::generic(schemas.clone(), order.into_iter().map(V).collect()).unwrap();
            let mut maps = Vec::new();
            for (at, rows) in raw.iter().enumerate() {
                let reverse = plan.required_order(at).unwrap()[0] != schemas[at][0];
                let mut map = BTreeMap::<VId, Vec<VId>>::new();
                for row in rows {
                    let (a, b) = if reverse {
                        (row[1], row[0])
                    } else {
                        (row[0], row[1])
                    };
                    map.entry(a).or_default().push(b);
                }
                for neighbors in map.values_mut() {
                    neighbors.sort();
                }
                maps.push(map);
            }
            let borrowed: Vec<_> = maps
                .iter()
                .enumerate()
                .map(|(at, map)| {
                    let order = plan.required_order(at).unwrap();
                    GraphTrie::adjacency([order[0], order[1]], map, 0, &mut allow).unwrap()
                })
                .collect();
            let owned: Vec<_> = raw
                .iter()
                .enumerate()
                .map(|(at, rows)| {
                    ColumnarTrie::new(
                        &schemas[at],
                        plan.required_order(at).unwrap(),
                        rows.clone(),
                        0,
                        &mut allow,
                    )
                    .unwrap()
                })
                .collect();
            assert_eq!(bag(&plan, &borrowed), bag(&plan, &owned));
        }
    }
}

#[test]
fn malformed_order_and_repeated_variables_are_not_silently_repaired() {
    let bad = BTreeMap::from([(VId(1), vec![VId(2), VId(0)])]);
    assert!(matches!(
        GraphTrie::adjacency([V(0), V(1)], &bad, 0, &mut allow),
        Err(GraphTrieError::UnsortedInput)
    ));
    assert!(matches!(
        GraphTrie::diagonal(V(0), &bad, 0, &mut allow),
        Err(GraphTrieError::UnsortedInput)
    ));
    assert!(matches!(
        GraphTrie::vertices(V(0), &[VId(2), VId(1)], 0, &mut allow),
        Err(GraphTrieError::UnsortedInput)
    ));
    assert!(matches!(
        GraphTrie::adjacency([V(0), V(0)], &bad, 0, &mut allow),
        Err(GraphTrieError::RepeatedVariable)
    ));
}

#[test]
fn every_build_and_seek_checkpoint_preserves_the_callers_refusal() {
    let map = BTreeMap::from([(VId(1), (0..128_u128).map(VId).collect())]);
    let mut total = 0;
    let trie = GraphTrie::adjacency([V(0), V(1)], &map, 3, &mut |_| {
        total += 1;
        Ok::<_, usize>(())
    })
    .unwrap();
    for stop in 0..total {
        let mut observed = 0;
        let result = GraphTrie::adjacency([V(0), V(1)], &map, 3, &mut |_| {
            let at = observed;
            observed += 1;
            if at == stop { Err(at) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GraphTrieError::Control(at)) if at == stop));
        assert_eq!(observed, stop + 1);
    }
    let mut total = 0;
    trie.cursor()
        .open(&mut allow)
        .unwrap()
        .unwrap()
        .seek(&VId(100), &mut |_| {
            total += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
    assert!(total < 10);
    for stop in 0..total {
        let mut cursor = trie.cursor().open(&mut allow).unwrap().unwrap();
        let mut observed = 0;
        assert_eq!(
            cursor.seek(&VId(100), &mut |_| {
                let at = observed;
                observed += 1;
                if at == stop { Err(at) } else { Ok(()) }
            }),
            Err(stop)
        );
        assert_eq!(observed, stop + 1);
        assert_eq!(
            cursor.key(),
            Some(&VId(0)),
            "refused seek cannot commit a new position"
        );
    }
}

#[test]
fn empty_descriptors_and_empty_vertex_domains_have_no_phantom_rows() {
    let map = BTreeMap::from([(VId(0), Vec::new())]);
    for trie in [
        GraphTrie::adjacency([V(0), V(1)], &map, 0, &mut allow).unwrap(),
        GraphTrie::diagonal(V(0), &map, 0, &mut allow).unwrap(),
        GraphTrie::vertices(V(0), &[], 0, &mut allow).unwrap(),
    ] {
        let mut cursor = trie.cursor();
        assert_eq!(cursor.key(), None);
        assert_eq!(cursor.distinct_prefixes(1), 0);
        assert!(cursor.open(&mut allow).unwrap().is_none());
        cursor.seek(&VId(u128::MAX), &mut allow).unwrap();
        cursor.advance(&mut allow).unwrap();
        assert_eq!(cursor.key(), None);
    }
}

mod calibrated_factorized_pages {
    use core::convert::Infallible;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::algebra::{
        GlaDirection, GraphPatternBuilder, GraphValue, GraphValueRow, PreparedGraphPattern,
    };
    use fgdb_gql::free_join::{CalibratedFreeJoin, ConformalJoinCalibration, JoinStrategy};
    use fgdb_gql::{
        GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
        PreparedGraphText,
    };
    use fgdb_types::{EId, VId};
    use std::cell::Cell;

    type Edge = (EId, VId, RelationId, VId);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }

    fn prepare(tail: &str) -> PreparedGraphPattern<GraphValueRow> {
        PreparedGraphText::prepare_with_resolver(
            &format!(
                "MATCH REPEATABLE ELEMENTS (a)-[:R]->(x), (a)-[:R]->(y), (a)-[:R]->(z) {tail}"
            ),
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
    }

    fn edges(weights: [usize; 2]) -> Vec<Edge> {
        let mut result = Vec::new();
        for (target, count) in [(VId(2), weights[0]), (VId(3), weights[1])] {
            for _ in 0..count {
                result.push((EId(result.len() as u128 + 1), VId(1), RelationId(1), target));
            }
        }
        result
    }

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(10_000, 10_000, 250_000, 250_000)
    }

    fn fallback() -> ConformalJoinCalibration {
        ConformalJoinCalibration {
            observed_residual_override: Some(0.95),
            ..ConformalJoinCalibration::default()
        }
    }

    #[test]
    fn factorized_and_generic_projections_preserve_bags_distinct_hidden_keys_and_pages() {
        let input = edges([2, 3]);
        for tail in [
            "RETURN ALL x, y, z ORDER BY x, y, z",
            "RETURN ALL x, y, z ORDER BY x, y, z SKIP 3 LIMIT 7",
            "RETURN ALL x ORDER BY x SKIP 7 LIMIT 5",
            "RETURN DISTINCT x ORDER BY x",
            "RETURN DISTINCT x, y, z ORDER BY x, y, z SKIP 1 LIMIT 3",
            "RETURN ALL a, y ORDER BY x DESC, y, z SKIP 4 LIMIT 8",
            "RETURN ALL x ORDER BY y.score DESC, x SKIP 3 LIMIT 7",
            "RETURN ALL x, y, z ORDER BY x, y, z LIMIT 0",
        ] {
            let query = prepare(tail);
            let plan = query.plan();
            let physical = CalibratedFreeJoin::try_compile(plan.operators()).unwrap();
            let baseline = plan
                .execute_governed_with_identified_properties(
                    input.len() as u64,
                    [VId(1), VId(2), VId(3)],
                    input.iter().copied(),
                    |_, _| Ok::<_, Infallible>(true),
                    |_, _| Ok(None),
                    policy(),
                    || Ok::<_, Infallible>(()),
                )
                .unwrap();
            for (calibration, strategy) in [
                (
                    ConformalJoinCalibration::default(),
                    JoinStrategy::FactorizedFreeJoin,
                ),
                (fallback(), JoinStrategy::WorstCaseOptimalGeneric),
            ] {
                let (actual, card) = physical
                    .execute_governed_with_calibration(
                        plan,
                        input.len() as u64,
                        [VId(1), VId(2), VId(3)],
                        input.iter().copied(),
                        |_, _| Ok::<_, Infallible>(true),
                        |_, _| Ok(None),
                        &calibration,
                        policy(),
                        || Ok::<_, Infallible>(()),
                    )
                    .unwrap();
                assert_eq!(card.selected_strategy, strategy, "{tail}");
                assert_eq!(actual.value, baseline.value, "{tail}");
                assert_eq!(actual.rows.result_rows, actual.value.len() as u64);
            }
        }
    }

    #[test]
    fn weighted_prefixes_keep_canonical_visible_ties_for_bounded_pages() {
        // Prefix Unit weights for x and y repeat a suffix containing both
        // z=2 and z=3. Visiting weighted bindings groups those occurrences
        // differently from flattening the factor tree. Every ORDER BY a key
        // is equal, so the complete visible row must break ties: eighteen
        // z=2 occurrences precede nine z=3 occurrences, regardless of arrival.
        let input = edges([2, 1]);
        for (tail, expected_vertices) in [
            (
                "RETURN ALL z ORDER BY a LIMIT 20",
                [vec![VId(2); 18], vec![VId(3); 2]].concat(),
            ),
            (
                "RETURN ALL z ORDER BY a SKIP 17 LIMIT 5",
                vec![VId(2), VId(3), VId(3), VId(3), VId(3)],
            ),
        ] {
            let expected: Vec<_> = expected_vertices
                .into_iter()
                .map(|vid| GraphValueRow::from_owned_values(vec![GraphValue::Vertex(vid)]))
                .collect();
            let query = prepare(tail);
            let plan = query.plan();
            let physical = CalibratedFreeJoin::try_compile(plan.operators()).unwrap();
            let baseline = plan
                .execute_governed_with_identified_properties(
                    input.len() as u64,
                    [VId(1), VId(2), VId(3)],
                    input.iter().copied(),
                    |_, _| Ok::<_, Infallible>(true),
                    |_, _| Ok(None),
                    policy(),
                    || Ok::<_, Infallible>(()),
                )
                .unwrap();
            assert_eq!(baseline.value, expected, "{tail}");
            for (calibration, strategy) in [
                (
                    ConformalJoinCalibration::default(),
                    JoinStrategy::FactorizedFreeJoin,
                ),
                (fallback(), JoinStrategy::WorstCaseOptimalGeneric),
            ] {
                let (actual, card) = physical
                    .execute_governed_with_calibration(
                        plan,
                        input.len() as u64,
                        [VId(1), VId(2), VId(3)],
                        input.iter().copied(),
                        |_, _| Ok::<_, Infallible>(true),
                        |_, _| Ok(None),
                        &calibration,
                        policy(),
                        || Ok::<_, Infallible>(()),
                    )
                    .unwrap();
                assert_eq!(card.selected_strategy, strategy, "{tail}");
                assert_eq!(actual.value, baseline.value, "{tail}");
                assert_eq!(actual.rows.result_rows, expected.len() as u64);
            }
        }
    }

    #[test]
    fn bounded_factorized_pages_do_not_flatten_millions_of_parallel_edge_occurrences() {
        // 256 physical edges, eight complete vertex assignments and 256^3 bag
        // occurrences. A DISTINCT or small page fits the same governed budget
        // as its live generic incumbent; enumerating the occurrence bag cannot.
        let input = edges([128, 128]);
        for (tail, length) in [
            ("RETURN DISTINCT x ORDER BY x", 2),
            ("RETURN ALL x, y, z ORDER BY x, y, z LIMIT 0", 0),
            ("RETURN ALL x, y, z ORDER BY x, y, z SKIP 5 LIMIT 7", 7),
            ("RETURN ALL x ORDER BY x DESC LIMIT 1", 1),
            ("RETURN ALL a, y ORDER BY x DESC, y, z SKIP 5 LIMIT 7", 7),
        ] {
            let query = prepare(tail);
            let plan = query.plan();
            let physical = CalibratedFreeJoin::try_compile(plan.operators()).unwrap();
            let mut results = Vec::new();
            for (calibration, strategy) in [
                (
                    ConformalJoinCalibration::default(),
                    JoinStrategy::FactorizedFreeJoin,
                ),
                (fallback(), JoinStrategy::WorstCaseOptimalGeneric),
            ] {
                let (actual, card) = physical
                    .execute_governed_with_calibration(
                        plan,
                        input.len() as u64,
                        [VId(1), VId(2), VId(3)],
                        input.iter().copied(),
                        |_, _| Ok::<_, Infallible>(true),
                        |_, _| Ok(None),
                        &calibration,
                        policy(),
                        || Ok::<_, Infallible>(()),
                    )
                    .unwrap();
                assert_eq!(card.selected_strategy, strategy, "{tail}");
                assert_eq!(actual.value.len(), length, "{tail}");
                results.push(actual.value);
            }
            assert_eq!(results[0], results[1], "{tail}");
        }
    }

    #[test]
    fn factorized_pages_keep_late_visible_and_hidden_projection_errors() {
        let input = edges([2, 3]);
        for tail in [
            "RETURN ALL x.score AS score ORDER BY score LIMIT 0",
            "RETURN DISTINCT x.score AS score ORDER BY score LIMIT 1",
            "RETURN ALL x.score AS score ORDER BY score SKIP 200 LIMIT 1",
            "RETURN ALL a ORDER BY x.score LIMIT 0",
            "RETURN ALL a ORDER BY x.score LIMIT 1",
        ] {
            let query = prepare(tail);
            let plan = query.plan();
            let physical = CalibratedFreeJoin::try_compile(plan.operators()).unwrap();
            for calibration in [ConformalJoinCalibration::default(), fallback()] {
                let successful_reads = Cell::new(0);
                let result = physical.execute_governed_with_calibration(
                    plan,
                    input.len() as u64,
                    [VId(1), VId(2), VId(3)],
                    input.iter().copied(),
                    |_, _| Ok::<_, &str>(true),
                    |vid, _| {
                        if vid == VId(3) {
                            Err("late property source")
                        } else {
                            successful_reads.set(successful_reads.get() + 1);
                            Ok(None)
                        }
                    },
                    &calibration,
                    policy(),
                    || Ok::<_, Infallible>(()),
                );
                assert!(
                    matches!(result, Err(GqlQueryError::Source("late property source"))),
                    "{tail}: {result:?}"
                );
                assert!(
                    successful_reads.get() > 0,
                    "refusal must follow an earlier binding"
                );
            }
        }
    }

    #[test]
    fn overflowing_offset_plus_count_never_wraps_a_factorized_page_cap() {
        let mut builder = GraphPatternBuilder::default();
        for name in ["a", "x", "y", "z"] {
            builder.vertex(name).unwrap();
        }
        for leaf in ["x", "y", "z"] {
            builder
                .edge("a", RelationId(1), GlaDirection::Forward, leaf)
                .unwrap();
        }
        let query = builder
            .prepare_bindings(&["x", "y", "z"], u64::MAX, Some(2))
            .unwrap()
            .with_duplicates();
        let plan = query.plan();
        let physical = CalibratedFreeJoin::try_compile(plan.operators()).unwrap();
        let input = edges([2, 3]);
        for (calibration, strategy) in [
            (
                ConformalJoinCalibration::default(),
                JoinStrategy::FactorizedFreeJoin,
            ),
            (fallback(), JoinStrategy::WorstCaseOptimalGeneric),
        ] {
            let (actual, card) = physical
                .execute_governed_with_calibration(
                    plan,
                    input.len() as u64,
                    [VId(1), VId(2), VId(3)],
                    input.iter().copied(),
                    |_, _| Ok::<_, Infallible>(true),
                    |_, _| Ok(None),
                    &calibration,
                    policy(),
                    || Ok::<_, Infallible>(()),
                )
                .unwrap();
            assert_eq!(card.selected_strategy, strategy);
            assert!(actual.value.is_empty());
        }
    }
}
