use core::convert::Infallible;
use fgdb_gql::GlaExecutionEvent;
use fgdb_gql::free_join::{
    ColumnarTrie, FreeJoin, FreeJoinPlan, JoinVariable as V, MultiplicityOverflow, TrieBuildError,
    TrieCursor, TrieRelation,
};
use std::collections::BTreeMap;

type WeightedRows = Vec<(Vec<u128>, usize)>;

fn allow(_: GlaExecutionEvent) -> Result<(), Infallible> {
    Ok(())
}

fn inputs(
    plan: &FreeJoinPlan,
    schemas: &[Vec<V>],
    data: &[WeightedRows],
) -> Vec<ColumnarTrie<u128>> {
    schemas
        .iter()
        .zip(data)
        .enumerate()
        .map(|(at, (schema, rows))| {
            ColumnarTrie::new_weighted(
                schema,
                plan.required_order(at).unwrap(),
                rows.clone(),
                at as u64 + 40,
                &mut allow,
            )
            .unwrap()
        })
        .collect()
}

fn collect(plan: &FreeJoinPlan, tries: &[ColumnarTrie<u128>]) -> BTreeMap<Vec<u128>, u128> {
    let mut result = BTreeMap::new();
    FreeJoin::new(plan, tries)
        .unwrap()
        .for_each_binding(&mut allow, |binding, _| {
            let row = binding.values().copied().collect();
            assert!(
                result
                    .insert(row, binding.multiplicity().unwrap())
                    .is_none()
            );
            Ok(())
        })
        .unwrap();
    result
}

#[test]
fn weighted_triangle_adds_duplicate_inputs_and_multiplies_matching_leaves() {
    let schemas = vec![vec![V(0), V(1)], vec![V(1), V(2)], vec![V(0), V(2)]];
    let data = vec![
        vec![(vec![1, 2], 2), (vec![1, 2], 3), (vec![1, 3], 7)],
        vec![(vec![2, 4], 11), (vec![3, 8], 1)],
        vec![(vec![1, 4], 13)],
    ];
    for (order, row) in [
        (vec![V(0), V(1), V(2)], vec![1, 2, 4]),
        (vec![V(2), V(1), V(0)], vec![4, 2, 1]),
    ] {
        let plan = FreeJoinPlan::generic(schemas.clone(), order).unwrap();
        let tries = inputs(&plan, &schemas, &data);
        assert_eq!(collect(&plan, &tries), BTreeMap::from([(row, 715)]));
        let batch = FreeJoin::new(&plan, &tries)
            .unwrap()
            .factorize(&mut allow)
            .unwrap();
        assert_eq!(batch.cardinality(), Ok(715));
        assert_eq!(batch.generations(), &[40, 41, 42]);
    }
}

#[test]
fn weights_follow_rows_through_nary_schema_permutation_and_sorting() {
    let schemas = vec![vec![V(2), V(0), V(1)]];
    let plan = FreeJoinPlan::generic(schemas.clone(), vec![V(1), V(2), V(0)]).unwrap();
    let data = vec![vec![
        (vec![9, 0, 2], 3),
        (vec![4, u128::MAX, 8], 7),
        (vec![9, 0, 2], 5),
    ]];
    let tries = inputs(&plan, &schemas, &data);
    assert_eq!(
        collect(&plan, &tries),
        BTreeMap::from([(vec![2, 9, 0], 8), (vec![8, 4, u128::MAX], 7),])
    );
    assert_eq!(tries[0].stored_keys(), 6);
}

#[test]
fn weighted_input_controls_do_not_expand_occurrences() {
    let mut events = Vec::new();
    let small = ColumnarTrie::new_weighted(
        &[V(0)],
        &[V(0)],
        vec![(vec![7_u128], 1)],
        19,
        &mut |event| {
            events.push(event);
            Ok::<_, Infallible>(())
        },
    )
    .unwrap();
    let mut weighted_events = Vec::new();
    let huge = ColumnarTrie::new_weighted(
        &[V(0)],
        &[V(0)],
        vec![(vec![7_u128], usize::MAX)],
        19,
        &mut |event| {
            weighted_events.push(event);
            Ok::<_, Infallible>(())
        },
    )
    .unwrap();
    assert_eq!(events, weighted_events);
    assert_eq!(small.stored_keys(), huge.stored_keys());
    assert_eq!(
        huge.cursor()
            .open(&mut allow)
            .unwrap()
            .unwrap()
            .multiplicity(),
        Some(usize::MAX)
    );
    let mut unit_events = Vec::new();
    ColumnarTrie::new(&[V(0)], &[V(0)], vec![vec![7_u128]], 19, &mut |event| {
        unit_events.push(event);
        Ok::<_, Infallible>(())
    })
    .unwrap();
    assert_eq!(events, unit_events);
}

#[test]
fn weighted_products_seek_the_last_of_ten_to_the_thirty_six_rows() {
    let order = vec![V(0), V(1), V(2), V(3), V(4), V(5)];
    let schemas: Vec<_> = order.iter().map(|&variable| vec![variable]).collect();
    let data: Vec<_> = (0..6_u128)
        .map(|key| vec![(vec![key], 1_000_000)])
        .collect();
    let plan = FreeJoinPlan::generic(schemas.clone(), order).unwrap();
    let tries = inputs(&plan, &schemas, &data);
    let batch = FreeJoin::new(&plan, &tries)
        .unwrap()
        .factorize(&mut allow)
        .unwrap();
    let count = 10_u128.pow(36);
    assert_eq!(batch.cardinality(), Ok(count));
    assert_eq!(batch.stored_values(), 6);
    let mut cursor = batch.cursor().unwrap();
    cursor.seek(count - 1);
    let mut work = 0;
    let page = cursor
        .next_batch(2, &mut |event| {
            work += usize::from(event == GlaExecutionEvent::Work);
            Ok::<_, Infallible>(())
        })
        .unwrap();
    assert_eq!(page.row_count(), 1);
    assert_eq!(
        page.columns(),
        &[vec![0], vec![1], vec![2], vec![3], vec![4], vec![5]]
    );
    assert_eq!(cursor.position(), count);
    assert!(
        work < 200,
        "weighted rank seek must not enumerate skipped occurrences"
    );
}

#[test]
fn factorized_bindings_keep_weighted_products_unions_and_nullary_relations() {
    let schemas = vec![vec![], vec![V(0), V(1)], vec![V(0), V(2)], vec![V(0), V(3)]];
    let data = vec![
        vec![(vec![], 2), (vec![], 3)],
        vec![(vec![10, 1], 2), (vec![10, 2], 3), (vec![20, 4], 7)],
        vec![(vec![10, 5], 11), (vec![10, 6], 13), (vec![20, 8], 17)],
        vec![(vec![10, 9], 19), (vec![20, 0], 23)],
    ];
    let expected = [
        ([10, 1, 5, 9], 5 * 2 * 11 * 19),
        ([10, 1, 6, 9], 5 * 2 * 13 * 19),
        ([10, 2, 5, 9], 5 * 3 * 11 * 19),
        ([10, 2, 6, 9], 5 * 3 * 13 * 19),
        ([20, 4, 8, 0], 5 * 7 * 17 * 23),
    ];
    for order in [vec![V(0), V(1), V(2), V(3)], vec![V(1), V(0), V(3), V(2)]] {
        let wanted: BTreeMap<Vec<u128>, u128> = expected
            .iter()
            .map(|(row, weight)| (order.iter().map(|v| row[v.0 as usize]).collect(), *weight))
            .collect();
        let plan = FreeJoinPlan::generic(schemas.clone(), order).unwrap();
        let tries = inputs(&plan, &schemas, &data);
        let batch = FreeJoin::new(&plan, &tries)
            .unwrap()
            .factorize(&mut allow)
            .unwrap();
        let mut observed = BTreeMap::new();
        batch
            .for_each_binding(&mut allow, |binding, _| {
                let row = binding.values().copied().collect();
                assert!(
                    observed
                        .insert(row, binding.multiplicity().unwrap())
                        .is_none()
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(observed, wanted);
        assert_eq!(observed, collect(&plan, &tries));
        assert_eq!(batch.cardinality(), Ok(wanted.values().sum()));
    }
}

#[test]
fn factorized_binding_visits_ignore_occurrence_count_even_when_u128_overflows() {
    let order: Vec<_> = (0..7).map(V).collect();
    let schemas: Vec<_> = order.iter().map(|&variable| vec![variable]).collect();
    let plan = FreeJoinPlan::generic(schemas.clone(), order).unwrap();
    let mut traces = Vec::new();
    for weight in [1, 1_000_000] {
        let data: Vec<_> = (0..7_u128).map(|key| vec![(vec![key], weight)]).collect();
        let tries = inputs(&plan, &schemas, &data);
        let batch = FreeJoin::new(&plan, &tries)
            .unwrap()
            .factorize(&mut allow)
            .unwrap();
        let expected_count = if weight == 1 {
            Ok(1)
        } else {
            Err(MultiplicityOverflow)
        };
        assert_eq!(batch.cardinality(), expected_count);
        let mut trace = Vec::new();
        let mut emissions = 0;
        batch
            .for_each_binding(
                &mut |event| {
                    trace.push(event);
                    Ok::<_, Infallible>(())
                },
                |binding, _| {
                    emissions += 1;
                    assert_eq!(
                        binding.values().copied().collect::<Vec<_>>(),
                        (0..7).collect::<Vec<_>>()
                    );
                    assert_eq!(binding.multiplicity(), expected_count);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(emissions, 1);
        traces.push(trace);
    }
    assert_eq!(
        traces[0], traces[1],
        "visits must not flatten bag occurrences"
    );
}

#[test]
fn factorized_binding_control_and_sink_refusals_leave_the_batch_reusable() {
    let schemas = vec![vec![V(0), V(1)], vec![V(0), V(2)], vec![V(0), V(3)]];
    let plan = FreeJoinPlan::generic(schemas.clone(), vec![V(0), V(1), V(2), V(3)]).unwrap();
    let data = vec![vec![(vec![0, 1], 2), (vec![0, 2], 3)]; 3];
    let tries = inputs(&plan, &schemas, &data);
    let batch = FreeJoin::new(&plan, &tries)
        .unwrap()
        .factorize(&mut allow)
        .unwrap();
    let mut total = 0;
    batch
        .for_each_binding(
            &mut |_| {
                total += 1;
                Ok::<_, usize>(())
            },
            |_, _| Ok(()),
        )
        .unwrap();
    assert!(total > 0);
    for stop in 0..total {
        let mut observed = 0;
        let result = batch.for_each_binding(
            &mut |_| {
                let at = observed;
                observed += 1;
                if at == stop { Err(at) } else { Ok(()) }
            },
            |_, _| Ok(()),
        );
        assert_eq!(result, Err(stop));
        assert_eq!(observed, stop + 1);
    }
    let mut emitted = 0;
    assert_eq!(
        batch.for_each_binding(&mut |_| Ok(()), |_, _| {
            emitted += 1;
            if emitted == 2 { Err("sink") } else { Ok(()) }
        }),
        Err("sink")
    );
    assert_eq!(emitted, 2);
    let mut observed = BTreeMap::new();
    batch
        .for_each_binding(&mut allow, |binding, _| {
            assert!(
                observed
                    .insert(
                        binding.values().copied().collect(),
                        binding.multiplicity().unwrap()
                    )
                    .is_none()
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(observed, collect(&plan, &tries));
}

#[test]
fn zero_arity_width_and_weight_errors_remain_distinct() {
    let unit =
        ColumnarTrie::<u128>::new_weighted(&[], &[], vec![(vec![], 2), (vec![], 5)], 1, &mut allow)
            .unwrap();
    assert_eq!(unit.cursor().multiplicity(), Some(7));
    let empty = ColumnarTrie::<u128>::new_weighted(&[], &[], vec![], 1, &mut allow).unwrap();
    assert_eq!(empty.cursor().multiplicity(), Some(0));
    assert!(matches!(
        ColumnarTrie::new_weighted(&[V(0)], &[V(0)], vec![(vec![1_u128], 0)], 1, &mut allow),
        Err(TrieBuildError::ZeroMultiplicity { row: 0 })
    ));
    assert!(matches!(
        ColumnarTrie::<u128>::new_weighted(&[V(0)], &[V(0)], vec![(vec![], 1)], 1, &mut allow),
        Err(TrieBuildError::RowArity {
            row: 0,
            expected: 1,
            actual: 0
        })
    ));
    for schema in [vec![], vec![V(0)]] {
        let row = vec![1_u128; schema.len()];
        assert!(matches!(
            ColumnarTrie::new_weighted(
                &schema,
                &schema,
                vec![(row.clone(), usize::MAX), (row, 1)],
                1,
                &mut allow,
            ),
            Err(TrieBuildError::MultiplicityOverflow)
        ));
    }
}

#[test]
fn weighted_empty_input_annihilates_an_overflowing_factorized_product() {
    let order = vec![V(0), V(1), V(2), V(3), V(4), V(5), V(6)];
    let schemas: Vec<_> = order.iter().map(|&variable| vec![variable]).collect();
    let mut data: Vec<_> = (0..7_u128)
        .map(|key| vec![(vec![key], 1_000_000)])
        .collect();
    let plan = FreeJoinPlan::generic(schemas.clone(), order).unwrap();
    let tries = inputs(&plan, &schemas, &data);
    let overflow = FreeJoin::new(&plan, &tries)
        .unwrap()
        .factorize(&mut allow)
        .unwrap();
    assert_eq!(overflow.cardinality(), Err(MultiplicityOverflow));
    data[6].clear();
    let tries = inputs(&plan, &schemas, &data);
    let empty = FreeJoin::new(&plan, &tries)
        .unwrap()
        .factorize(&mut allow)
        .unwrap();
    assert_eq!(empty.cardinality(), Ok(0));
    assert!(collect(&plan, &tries).is_empty());
    let mut emissions = 0;
    empty
        .for_each_binding(&mut allow, |_, _| {
            emissions += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(emissions, 0);
}

#[test]
fn every_weighted_build_checkpoint_refuses_without_a_partial_trie() {
    let rows = vec![(vec![3_u128, 9], 4), (vec![1, 2], 7), (vec![3, 9], 2)];
    let mut total = 0;
    ColumnarTrie::new_weighted(&[V(0), V(1)], &[V(1), V(0)], rows.clone(), 1, &mut |_| {
        total += 1;
        Ok::<_, usize>(())
    })
    .unwrap();
    for stop in 0..total {
        let mut observed = 0;
        let result =
            ColumnarTrie::new_weighted(&[V(0), V(1)], &[V(1), V(0)], rows.clone(), 1, &mut |_| {
                let at = observed;
                observed += 1;
                if at == stop { Err(at) } else { Ok(()) }
            });
        assert!(matches!(result, Err(TrieBuildError::Control(at)) if at == stop));
        assert_eq!(observed, stop + 1);
    }
}

#[test]
fn exhaustive_weighted_triangles_match_an_independent_assignment_oracle() {
    let variables = [V(0), V(1), V(2)];
    let schemas = vec![vec![V(0), V(1)], vec![V(1), V(2)], vec![V(0), V(2)]];
    let orders = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for first in 0_u8..16 {
        for second in 0_u8..16 {
            for third in 0_u8..16 {
                let masks = [first, second, third];
                let data: Vec<WeightedRows> = masks
                    .iter()
                    .map(|&mask| {
                        (0..4_usize)
                            .filter(|&bit| mask & (1 << bit) != 0)
                            .map(|bit| {
                                (
                                    vec![(bit / 2) as u128, (bit % 2) as u128],
                                    if bit % 2 == 0 { 2 } else { 3 },
                                )
                            })
                            .collect()
                    })
                    .collect();
                for order in orders {
                    let plan = FreeJoinPlan::generic(
                        schemas.clone(),
                        order.map(|at| variables[at]).to_vec(),
                    )
                    .unwrap();
                    let tries = inputs(&plan, &schemas, &data);
                    let mut expected = BTreeMap::new();
                    for a in 0..2_usize {
                        for b in 0..2_usize {
                            for c in 0..2_usize {
                                let pairs = [(a, b), (b, c), (a, c)];
                                if masks
                                    .iter()
                                    .zip(pairs)
                                    .all(|(&mask, (x, y))| mask & (1 << (2 * x + y)) != 0)
                                {
                                    let binding = [a as u128, b as u128, c as u128];
                                    let weight = pairs
                                        .iter()
                                        .map(|&(_, y)| if y == 0 { 2_u128 } else { 3 })
                                        .product();
                                    expected.insert(order.map(|at| binding[at]).to_vec(), weight);
                                }
                            }
                        }
                    }
                    assert_eq!(
                        collect(&plan, &tries),
                        expected,
                        "masks={masks:?}, order={order:?}"
                    );
                }
            }
        }
    }
}
