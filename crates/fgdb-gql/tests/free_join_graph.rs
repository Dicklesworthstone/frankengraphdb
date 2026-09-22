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
