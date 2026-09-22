use fgdb_gql::GlaExecutionEvent;
use fgdb_gql::free_join::{
    ColumnarTrie, FreeJoin, FreeJoinPlan, JoinPlanError, JoinVariable as V,
    MultiplicityOverflow, TrieBuildError, TrieCursor, TrieRelation,
};
use std::cell::Cell;
use std::collections::BTreeMap;

fn tries(plan: &FreeJoinPlan, schemas: &[Vec<V>], inputs: &[Vec<Vec<i64>>]) -> Vec<ColumnarTrie<i64>> {
    schemas.iter().zip(inputs).enumerate().map(|(at, (schema, rows))| {
        ColumnarTrie::new(schema, plan.required_order(at).unwrap(), rows.clone(), 7, &mut |_| Ok::<_, ()>(())).unwrap()
    }).collect()
}

fn execute(plan: &FreeJoinPlan, schemas: &[Vec<V>], inputs: &[Vec<Vec<i64>>]) -> (Vec<Vec<i64>>, u64) {
    let tries = tries(plan, schemas, inputs);
    let join = FreeJoin::new(plan, &tries).unwrap();
    let mut rows = Vec::new();
    let mut work = 0_u64;
    join.for_each_binding(&mut |event| {
        if event == GlaExecutionEvent::Work { work += 1; }
        Ok::<_, ()>(())
    }, |binding, _| {
        for _ in 0..binding.multiplicity().unwrap() {
            rows.push(binding.values().copied().collect());
        }
        Ok(())
    }).unwrap();
    (rows, work)
}

// Deliberately no trie, prefix intersection, plan grouping or multiplicity
// arithmetic: enumerate the original occurrence bags using nested natural joins.
fn oracle(order: &[V], schemas: &[Vec<V>], inputs: &[Vec<Vec<i64>>]) -> Vec<Vec<i64>> {
    let mut bindings = vec![BTreeMap::new()];
    for (schema, input) in schemas.iter().zip(inputs) {
        let mut next = Vec::new();
        for outer in &bindings {
            for tuple in input {
                if schema.iter().zip(tuple).all(|(v, key)| outer.get(v).is_none_or(|old| old == key)) {
                    let mut row = outer.clone();
                    for (&v, &key) in schema.iter().zip(tuple) { row.insert(v, key); }
                    next.push(row);
                }
            }
        }
        bindings = next;
    }
    let mut rows: Vec<Vec<i64>> = bindings.iter().map(|row| order.iter().map(|v| row[v]).collect()).collect();
    rows.sort();
    rows
}

#[test]
fn binary_grouped_generic_and_occurrence_oracle_agree() {
    let cases = [
        (vec![vec![V(0), V(1)], vec![V(1), V(2)]], vec![
            vec![vec![1, 2], vec![1, 2], vec![4, 9]],
            vec![vec![2, 3], vec![2, 3], vec![2, 5]],
        ]),
        (vec![vec![V(0)], vec![V(1)]], vec![vec![vec![1], vec![1]], vec![vec![2], vec![3]]]),
        (vec![vec![V(0), V(1)], vec![V(1), V(0)]], vec![
            vec![vec![1, 2], vec![1, 2], vec![3, 4]],
            vec![vec![2, 1], vec![2, 1], vec![4, 3]],
        ]),
        (vec![vec![], vec![V(0)]], vec![vec![vec![], vec![]], vec![vec![8], vec![8]]]),
        (vec![vec![], vec![]], vec![vec![vec![], vec![]], vec![vec![], vec![], vec![]]]),
        (vec![vec![V(0)], vec![V(0)]], vec![vec![], vec![vec![1]]]),
    ];
    for (schemas, inputs) in cases {
        let binary = FreeJoinPlan::binary(schemas[0].clone(), schemas[1].clone()).unwrap();
        let generic = FreeJoinPlan::generic(schemas.clone(), binary.variables().to_vec()).unwrap();
        let expected = oracle(binary.variables(), &schemas, &inputs);
        assert_eq!(execute(&binary, &schemas, &inputs).0, expected);
        assert_eq!(execute(&generic, &schemas, &inputs).0, expected);
        let reversed: Vec<_> = inputs.iter().map(|rows| rows.iter().rev().cloned().collect()).collect();
        assert_eq!(execute(&binary, &schemas, &reversed).0, expected);
    }
}

#[test]
fn exhaustive_two_vertex_triangles_under_every_variable_order() {
    let schemas = vec![vec![V(0), V(1)], vec![V(1), V(2)], vec![V(2), V(0)]];
    let orders = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
    let relation = |mask: u16| -> Vec<Vec<i64>> {
        (0..4_u32).filter(|&bit| mask & (1_u16 << bit) != 0).map(|bit| vec![i64::from(bit / 2), i64::from(bit % 2)]).collect()
    };
    for bits in 0..4096_u16 {
        let inputs = vec![relation(bits & 15), relation((bits >> 4) & 15), relation(bits >> 8)];
        for order in orders {
            let plan = FreeJoinPlan::generic(schemas.clone(), order.map(V).to_vec()).unwrap();
            assert_eq!(execute(&plan, &schemas, &inputs).0, oracle(plan.variables(), &schemas, &inputs), "bits={bits}, order={order:?}");
        }
    }
}

#[test]
fn multiway_bags_multiply_only_at_complete_bindings() {
    let schemas = vec![vec![V(0), V(1)], vec![V(1), V(2)], vec![V(2), V(0)]];
    let inputs = vec![vec![vec![1, 2]; 3], vec![vec![2, 3]; 4], vec![vec![3, 1]; 5]];
    let plan = FreeJoinPlan::generic(schemas.clone(), vec![V(0), V(1), V(2)]).unwrap();
    let data = tries(&plan, &schemas, &inputs);
    let mut emissions = 0;
    FreeJoin::new(&plan, &data).unwrap().for_each_binding(&mut |_| Ok::<_, ()>(()), |binding, control| {
        emissions += 1;
        assert_eq!(binding.multiplicity(), Ok(60));
        assert_eq!(binding.get(V(1)), Some(&2));
        let mut occurrences = 0;
        binding.for_each_occurrence(control, |_, _| { occurrences += 1; Ok(()) })?;
        assert_eq!(occurrences, 60);
        Ok(())
    }).unwrap();
    assert_eq!(emissions, 1, "duplicates must not enumerate incomplete prefixes");
}

#[test]
fn skewed_triangle_does_not_enumerate_quadratic_wedges() {
    fn measure(n: i64) -> u64 {
        let schemas = vec![vec![V(0), V(1)], vec![V(1), V(2)], vec![V(0), V(2)]];
        let inputs = vec![
            (0..n).map(|a| vec![a, 0]).collect(),
            (0..n).map(|c| vec![0, c]).collect(),
            (0..n).map(|a| vec![a, a]).collect(),
        ];
        let plan = FreeJoinPlan::generic(schemas.clone(), vec![V(0), V(1), V(2)]).unwrap();
        let (rows, work) = execute(&plan, &schemas, &inputs);
        assert_eq!(rows, (0..n).map(|a| vec![a, 0, a]).collect::<Vec<_>>());
        assert!(work < 256 * n as u64, "unexpected candidate work: {work}");
        work
    }
    let small = measure(64);
    let large = measure(128);
    assert!(large < small * 3, "a doubled adversary must not quadruple join work");
}

#[test]
fn cursor_seeks_are_monotone_and_parents_keep_their_generation() {
    let trie = ColumnarTrie::new(&[V(0), V(1)], &[V(0), V(1)],
        vec![vec![4, 3], vec![1, 2], vec![1, 2], vec![1, 7], vec![4, 8]],
        91, &mut |_| Ok::<_, ()>(())).unwrap();
    let mut root = trie.cursor();
    assert_eq!(root.distinct_prefixes(1), 2);
    assert_eq!(root.distinct_prefixes(2), 4);
    let mut child = root.open(&mut |_| Ok::<_, ()>(())).unwrap().unwrap();
    assert_eq!(child.key(), Some(&2));
    assert_eq!(child.open(&mut |_| Ok::<_, ()>(())).unwrap().unwrap().multiplicity(), Some(2));
    child.seek(&6, &mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(child.key(), Some(&7));
    child.seek(&0, &mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(child.key(), Some(&7));
    child.advance(&mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(child.key(), None);
    assert_eq!(root.key(), Some(&1));
    root.seek(&4, &mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(root.generation(), 91);
    assert_eq!(root.open(&mut |_| Ok::<_, ()>(())).unwrap().unwrap().key(), Some(&3));
    root.seek(&i64::MAX, &mut |_| Ok::<_, ()>(())).unwrap();
    assert!(root.open(&mut |_| Ok::<_, ()>(())).unwrap().is_none());
}

#[test]
fn invalid_schemas_orders_and_rows_fail_before_joining() {
    assert_eq!(FreeJoinPlan::generic(vec![vec![V(0)]], vec![]), Err(JoinPlanError::MissingVariable(V(0))));
    assert_eq!(FreeJoinPlan::new(vec![vec![V(0)], vec![V(1)]], vec![vec![V(0), V(1)]]), Err(JoinPlanError::UncoveredGroup(0)));
    assert_eq!(FreeJoinPlan::generic(vec![vec![V(0), V(0)]], vec![V(0)]), Err(JoinPlanError::DuplicateVariable(V(0))));
    let touched = Cell::new(false);
    let rows = std::iter::once(vec![1_i64]).inspect(|_| touched.set(true));
    assert!(matches!(ColumnarTrie::new(&[V(0)], &[V(1)], rows, 0, &mut |_| Ok::<_, ()>(())), Err(TrieBuildError::Schema(_))));
    assert!(!touched.get());
    assert!(matches!(ColumnarTrie::new(&[V(0)], &[V(0)], vec![vec![1_i64, 2]], 0, &mut |_| Ok::<_, ()>(())), Err(TrieBuildError::RowArity { row: 0, expected: 1, actual: 2 })));
    let plan = FreeJoinPlan::generic(vec![vec![V(0), V(1)]], vec![V(0), V(1)]).unwrap();
    let wrong = ColumnarTrie::new(&[V(0), V(1)], &[V(1), V(0)], vec![vec![1_i64, 2]], 0, &mut |_| Ok::<_, ()>(())).unwrap();
    assert!(matches!(FreeJoin::new(&plan, &[wrong]), Err(JoinPlanError::AttributeOrder { relation: 0 })));
}

#[test]
fn every_execution_control_boundary_can_refuse_without_resumption() {
    let schemas = vec![vec![V(0), V(1)], vec![V(1), V(2)]];
    let inputs = vec![vec![vec![1, 2], vec![3, 2]], vec![vec![2, 4], vec![2, 5]]];
    let plan = FreeJoinPlan::binary(schemas[0].clone(), schemas[1].clone()).unwrap();
    let data = tries(&plan, &schemas, &inputs);
    let join = FreeJoin::new(&plan, &data).unwrap();
    let mut boundaries = 0;
    join.for_each_binding(&mut |_| { boundaries += 1; Ok::<_, usize>(()) }, |_, _| Ok(())).unwrap();
    for refuse in 0..boundaries {
        let mut observed = 0;
        let result = join.for_each_binding(&mut |_| {
            let at = observed;
            observed += 1;
            if at == refuse { Err(at) } else { Ok(()) }
        }, |_, _| Ok(()));
        assert_eq!(result, Err(refuse));
        assert_eq!(observed, refuse + 1, "no callback may run after refusal");
    }
}

#[test]
fn huge_factored_multiplicity_is_checked_and_cancellable_not_wrapped() {
    let schemas = vec![vec![]; 50];
    let inputs = vec![vec![vec![]; 1000]; 50];
    let plan = FreeJoinPlan::generic(schemas.clone(), vec![]).unwrap();
    let data = tries(&plan, &schemas, &inputs);
    FreeJoin::new(&plan, &data).unwrap().for_each_binding(&mut |_| Ok::<_, ()>(()), |binding, _| {
        assert_eq!(binding.multiplicity(), Err(MultiplicityOverflow));
        let mut occurrences = 0;
        let result = binding.for_each_occurrence(&mut |_| Ok::<_, ()>(()), |_, _| {
            occurrences += 1;
            if occurrences == 5 { Err(()) } else { Ok(()) }
        });
        assert_eq!(result, Err(()));
        assert_eq!(occurrences, 5);
        Ok(())
    }).unwrap();
}

#[test]
fn empty_join_is_one_empty_binding_and_physical_orders_have_distinct_transcripts() {
    let plan = FreeJoinPlan::generic(vec![], vec![]).unwrap();
    let data: Vec<ColumnarTrie<i64>> = vec![];
    let mut rows = 0;
    FreeJoin::new(&plan, &data).unwrap().for_each_binding(&mut |_| Ok::<_, ()>(()), |binding, _| {
        rows += 1;
        assert_eq!(binding.values().count(), 0);
        assert_eq!(binding.multiplicity(), Ok(1));
        Ok(())
    }).unwrap();
    assert_eq!(rows, 1);
    let schemas = vec![vec![V(0), V(1)], vec![V(1), V(2)]];
    let first = FreeJoinPlan::generic(schemas.clone(), vec![V(0), V(1), V(2)]).unwrap();
    let second = FreeJoinPlan::generic(schemas, vec![V(1), V(0), V(2)]).unwrap();
    assert_ne!(first.canonical_bytes(), second.canonical_bytes());
}
