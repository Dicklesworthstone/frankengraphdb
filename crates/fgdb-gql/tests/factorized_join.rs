use fgdb_gql::GlaExecutionEvent;
use fgdb_gql::free_join::{
    ColumnarTrie, FactorNodeKind, FactorizedBatch, FactorizedReadError, FreeJoin, FreeJoinPlan,
    JoinVariable as V, MultiplicityOverflow,
};

fn inputs(
    plan: &FreeJoinPlan,
    schemas: &[Vec<V>],
    data: &[Vec<Vec<i64>>],
) -> Vec<ColumnarTrie<i64>> {
    schemas
        .iter()
        .zip(data)
        .enumerate()
        .map(|(at, (schema, rows))| {
            ColumnarTrie::new(
                schema,
                plan.required_order(at).unwrap(),
                rows.clone(),
                at as u64 + 40,
                &mut |_| Ok::<_, ()>(()),
            )
            .unwrap()
        })
        .collect()
}

fn flattened(batch: &FactorizedBatch<i64>, page: usize) -> Vec<Vec<i64>> {
    let mut cursor = batch.cursor().unwrap();
    let mut result = Vec::new();
    loop {
        let chunk = cursor.next_batch(page, &mut |_| Ok::<_, ()>(())).unwrap();
        if chunk.row_count() == 0 {
            break;
        }
        for row in 0..chunk.row_count() {
            result.push(chunk.columns().iter().map(|column| column[row]).collect());
        }
    }
    result
}

#[test]
fn independent_star_branches_are_products_not_flat_cartesian_intermediates() {
    let n = 200_i64;
    let schemas = vec![vec![V(0), V(1)], vec![V(0), V(2)], vec![V(0), V(3)]];
    let plan = FreeJoinPlan::generic(schemas.clone(), vec![V(0), V(1), V(2), V(3)]).unwrap();
    let data = vec![(0..n).map(|x| vec![1, x]).collect(); 3];
    let tries = inputs(&plan, &schemas, &data);
    let batch = FreeJoin::new(&plan, &tries)
        .unwrap()
        .factorize(&mut |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(batch.cardinality(), Ok((n * n * n) as u128));
    assert_eq!(batch.stored_values(), 1 + 3 * n as usize);
    assert!(batch.node_count() < 20 * n as usize);
    assert!(
        batch
            .node_kinds()
            .any(|kind| kind == FactorNodeKind::Product)
    );
    assert_eq!(batch.generations(), &[40, 41, 42]);
    let mut cursor = batch.cursor().unwrap();
    cursor.seek((n * n * n - 2) as u128);
    let mut work = 0;
    let last = cursor
        .next_batch(10, &mut |event| {
            if event == GlaExecutionEvent::Work {
                work += 1;
            }
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(last.row_count(), 2);
    assert_eq!(
        last.columns(),
        &[vec![1, 1], vec![199, 199], vec![199, 199], vec![198, 199]]
    );
    assert!(
        work < 200,
        "rank seek must not traverse eight million occurrences"
    );
    assert_eq!(cursor.position(), (n * n * n) as u128);
}

#[test]
fn factorization_matches_generic_join_bags_for_cycles_and_interleaved_components() {
    let cases = [
        (
            vec![vec![V(0), V(1)], vec![V(1), V(2)], vec![V(2), V(0)]],
            vec![
                vec![vec![1, 2], vec![1, 2], vec![4, 2]],
                vec![vec![2, 3], vec![2, 3], vec![2, 5]],
                vec![vec![3, 1], vec![5, 4]],
            ],
            vec![V(0), V(1), V(2)],
        ),
        // Components {0,2} and {1,3} interleave. They must not reorder columns
        // or invent a relation between unrelated keys while finding a product.
        (
            vec![vec![V(0), V(2)], vec![V(1), V(3)]],
            vec![
                vec![vec![1, 3], vec![1, 3], vec![2, 4]],
                vec![vec![5, 7], vec![6, 8]],
            ],
            vec![V(0), V(1), V(2), V(3)],
        ),
        (
            vec![vec![], vec![V(0)], vec![V(1)]],
            vec![
                vec![vec![], vec![]],
                vec![vec![1], vec![1], vec![2]],
                vec![vec![3], vec![4]],
            ],
            vec![V(0), V(1)],
        ),
    ];
    for (schemas, rows, order) in cases {
        let plan = FreeJoinPlan::generic(schemas.clone(), order).unwrap();
        let tries = inputs(&plan, &schemas, &rows);
        let join = FreeJoin::new(&plan, &tries).unwrap();
        let mut expected = Vec::new();
        join.for_each_binding(&mut |_| Ok::<_, ()>(()), |binding, _| {
            for _ in 0..binding.multiplicity().unwrap() {
                expected.push(binding.values().copied().collect::<Vec<_>>());
            }
            Ok(())
        })
        .unwrap();
        expected.sort();
        let batch = join.factorize(&mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(batch.cardinality(), Ok(expected.len() as u128));
        for page in [1, 2, 7, 1000] {
            let mut actual = flattened(&batch, page);
            actual.sort();
            assert_eq!(actual, expected);
        }
        assert_eq!(
            flattened(&batch, 1),
            flattened(&batch, 7),
            "batch boundaries cannot change occurrence order"
        );
    }
}

#[test]
fn every_factor_build_and_batch_boundary_refuses_without_partial_success() {
    let schemas = vec![vec![V(0), V(1)], vec![V(0), V(2)]];
    let plan = FreeJoinPlan::generic(schemas.clone(), vec![V(0), V(1), V(2)]).unwrap();
    let tries = inputs(
        &plan,
        &schemas,
        &[vec![vec![1, 2], vec![1, 3]], vec![vec![1, 4], vec![1, 5]]],
    );
    let join = FreeJoin::new(&plan, &tries).unwrap();
    let mut total = 0;
    let batch = join
        .factorize(&mut |_| {
            total += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
    for refuse in 0..total {
        let mut observed = 0;
        let result = join.factorize(&mut |_| {
            let at = observed;
            observed += 1;
            if at == refuse { Err(at) } else { Ok(()) }
        });
        assert!(matches!(result, Err(error) if error == refuse));
        assert_eq!(observed, refuse + 1);
    }
    let mut total = 0;
    batch
        .cursor()
        .unwrap()
        .next_batch(4, &mut |_| {
            total += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
    for refuse in 0..total {
        let mut cursor = batch.cursor().unwrap();
        let mut observed = 0;
        let result = cursor.next_batch(4, &mut |_| {
            let at = observed;
            observed += 1;
            if at == refuse { Err(at) } else { Ok(()) }
        });
        assert!(matches!(result, Err(FactorizedReadError::Control(error)) if error == refuse));
        assert_eq!(observed, refuse + 1);
        assert_eq!(
            cursor.position(),
            0,
            "failed batch cannot advance committed cursor position"
        );
        assert!(cursor.is_failed());
        assert!(matches!(
            cursor.next_batch(1, &mut |_| Ok::<_, usize>(())),
            Err(FactorizedReadError::Failed)
        ));
    }
}

#[test]
fn nullary_units_empty_products_and_overflow_have_distinct_semantics() {
    let plan = FreeJoinPlan::generic(vec![], vec![]).unwrap();
    let data: Vec<ColumnarTrie<i64>> = vec![];
    let unit = FreeJoin::new(&plan, &data)
        .unwrap()
        .factorize(&mut |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(unit.cardinality(), Ok(1));
    assert_eq!(flattened(&unit, 3), vec![Vec::<i64>::new()]);
    let schemas = vec![vec![]; 50];
    let plan = FreeJoinPlan::generic(schemas.clone(), vec![]).unwrap();
    let mut rows = vec![vec![vec![]; 1000]; 50];
    let tries = inputs(&plan, &schemas, &rows);
    let overflow = FreeJoin::new(&plan, &tries)
        .unwrap()
        .factorize(&mut |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(overflow.cardinality(), Err(MultiplicityOverflow));
    assert!(matches!(overflow.cursor(), Err(MultiplicityOverflow)));
    rows[49].clear();
    let tries = inputs(&plan, &schemas, &rows);
    let empty = FreeJoin::new(&plan, &tries)
        .unwrap()
        .factorize(&mut |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(
        empty.cardinality(),
        Ok(0),
        "zero annihilates an overflowing product"
    );
    assert!(flattened(&empty, 1).is_empty());
}
