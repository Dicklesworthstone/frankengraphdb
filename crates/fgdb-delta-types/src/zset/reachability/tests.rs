use super::*;

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn edges(updates: &[(u32, u32, i128)]) -> ZSet<(u32, u32)> {
    ZSet::from_updates(
        updates
            .iter()
            .map(|&(s, t, w)| ((s, t), ZWeight::from_i128(w))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn plain(value: &ZSet<(u32, u32)>) -> BTreeMap<(u32, u32), i128> {
    value
        .iter()
        .map(|(key, weight)| (*key, weight.to_i128().unwrap()))
        .collect()
}
fn input(mask: u32) -> ZSet<(u32, u32)> {
    let rows: Vec<_> = (0..9)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| (bit / 3, bit % 3, 1))
        .collect();
    edges(&rows)
}
// Independent Floyd-Warshall; it neither uses affected-source invalidation
// nor the operator's traversal, arrangements or old closure.
fn oracle(mask: u32) -> ZSet<(u32, u32)> {
    let mut reach = [[false; 3]; 3];
    for (source, row) in reach.iter_mut().enumerate() {
        for (destination, present) in row.iter_mut().enumerate() {
            *present = mask & (1 << (source * 3 + destination)) != 0;
        }
    }
    for middle in 0..3 {
        for source in 0..3 {
            for destination in 0..3 {
                reach[source][destination] |= reach[source][middle] && reach[middle][destination];
            }
        }
    }
    let mut result = Vec::new();
    for (source, row) in reach.iter().enumerate() {
        for (destination, &present) in row.iter().enumerate() {
            if present {
                result.push((source as u32, destination as u32, 1));
            }
        }
    }
    edges(&result)
}

#[test]
fn every_three_vertex_graph_handles_single_and_simultaneous_support_changes() {
    for before in 0..512 {
        let initial = input(before);
        let expected_before = oracle(before);
        // One- and two-edge flips, plus the complement (all nine flips).
        let flips = (0..9)
            .map(|a| 1 << a)
            .chain((0..9).flat_map(|a| (a + 1..9).map(move |b| (1 << a) | (1 << b))))
            .chain(std::iter::once(511));
        for flip in flips {
            let mut operator = IncrementalReachability::new();
            assert_eq!(
                operator.apply(&initial, LIMBS, &mut allow).unwrap(),
                expected_before
            );
            let after = before ^ flip;
            let update = input(after).minus(&initial, LIMBS, &mut allow).unwrap();
            let expected_after = oracle(after);
            let expected_delta = expected_after
                .minus(&expected_before, LIMBS, &mut allow)
                .unwrap();
            let actual = operator.apply(&update, LIMBS, &mut allow).unwrap();
            assert_eq!(actual, expected_delta, "before={before}, after={after}");
            assert_eq!(
                operator.snapshot(LIMBS, &mut allow).unwrap(),
                expected_after
            );
            let inverse = update.negated(LIMBS, &mut allow).unwrap();
            operator.apply(&inverse, LIMBS, &mut allow).unwrap();
            assert_eq!(
                operator.snapshot(LIMBS, &mut allow).unwrap(),
                expected_before
            );
        }
    }
}

#[test]
fn parallel_edges_cycles_and_alternate_routes_do_not_self_support_deletions() {
    let mut operator = IncrementalReachability::new();
    operator
        .apply(
            &edges(&[(1, 2, 2), (2, 1, 1), (2, 3, 1), (1, 3, 1)]),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    assert!(operator.contains(&1, &1));
    assert!(
        operator
            .apply(&edges(&[(1, 2, -1)]), LIMBS, &mut allow)
            .unwrap()
            .is_empty()
    );
    let delta = operator
        .apply(&edges(&[(1, 2, -1)]), LIMBS, &mut allow)
        .unwrap();
    assert_eq!(
        plain(&delta),
        BTreeMap::from([((1, 1), -1), ((1, 2), -1), ((2, 2), -1)])
    );
    assert!(operator.contains(&1, &3));
    assert!(!operator.contains(&3, &3));
    let delta = operator
        .apply(
            &edges(&[(1, 3, -1), (2, 1, -1), (2, 3, -1)]),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    assert_eq!(delta.len(), 3);
    assert!(operator.pairs().next().is_none());
    assert!(operator.outgoing.is_empty());
    assert!(operator.predecessors.is_empty());
    assert!(operator.edges.is_empty());
}

#[test]
fn every_refusal_and_dropped_guard_preserve_all_arrangements() {
    let seed = edges(&[(0, 1, 2), (1, 0, 1), (1, 2, 1), (3, 4, 1)]);
    let delta = edges(&[(0, 1, -2), (2, 0, 1), (4, 0, 1)]);
    let build = || {
        let mut operator = IncrementalReachability::new();
        operator.apply(&seed, LIMBS, &mut allow).unwrap();
        operator
    };
    let before = build();
    let mut total = 0;
    let mut success = build();
    success
        .apply(&delta, LIMBS, &mut |_| {
            total += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
    for stop in 1..=total {
        let mut operator = build();
        let mut seen = 0;
        let result = operator.apply(&delta, LIMBS, &mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert_eq!(
            result,
            Err(ReachabilityError::Delta(ZSetError::Control(stop)))
        );
        assert_eq!(seen, stop);
        assert_eq!(operator, before);
        operator.apply(&delta, LIMBS, &mut allow).unwrap();
        assert_eq!(operator, success);
    }
    let mut operator = build();
    let mut sink = operator.snapshot(LIMBS, &mut allow).unwrap();
    {
        let prepared = operator.prepare(&delta, LIMBS, &mut allow).unwrap();
        // Even a refusal in the downstream materialized view aborts both.
        assert!(
            sink.prepare_update(prepared.delta(), LIMBS, &mut |_| Err(123))
                .is_err()
        );
    }
    assert_eq!(operator, before);
    assert_eq!(sink, before.snapshot(LIMBS, &mut allow).unwrap());
    let prepared = operator.prepare(&delta, LIMBS, &mut allow).unwrap();
    let output = sink
        .prepare_update(prepared.delta(), LIMBS, &mut allow)
        .unwrap();
    output.commit();
    prepared.commit();
    assert_eq!(operator, success);
    assert_eq!(sink, success.snapshot(LIMBS, &mut allow).unwrap());
}

#[test]
fn negative_counts_and_late_promotion_refuse_atomically() {
    let mut operator = IncrementalReachability::new();
    let seed = edges(&[(1, 2, i128::MAX)]);
    operator.apply(&seed, LIMBS, &mut allow).unwrap();
    let before = operator.snapshot(LIMBS, &mut allow).unwrap();
    assert_eq!(
        operator.apply(&edges(&[(3, 4, 1), (5, 6, -1)]), LIMBS, &mut allow),
        Err(ReachabilityError::NegativeMultiplicity)
    );
    assert_eq!(operator.snapshot(LIMBS, &mut allow).unwrap(), before);
    assert!(operator.edge_weight(&(3, 4)).is_none());
    assert!(matches!(
        operator.apply(&edges(&[(1, 2, 1)]), LimbLimit::new(0), &mut allow),
        Err(ReachabilityError::Delta(ZSetError::Arithmetic(_)))
    ));
    assert_eq!(
        operator.edge_weight(&(1, 2)),
        Some(&ZWeight::from_i128(i128::MAX))
    );
    assert!(
        operator
            .apply(&edges(&[(1, 2, 1)]), LIMBS, &mut allow)
            .unwrap()
            .is_empty()
    );
    assert!(operator.edge_weight(&(1, 2)).unwrap().is_promoted());
    assert!(
        operator
            .apply(&edges(&[(1, 2, -1)]), LIMBS, &mut allow)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        operator.edge_weight(&(1, 2)),
        Some(&ZWeight::from_i128(i128::MAX))
    );
}

#[test]
fn a_small_changed_component_does_not_scan_unrelated_graphs() {
    let updates: Vec<_> = (0..10_000).map(|n| (2 * n, 2 * n + 1, 1)).collect();
    let mut operator = IncrementalReachability::new();
    operator.apply(&edges(&updates), LIMBS, &mut allow).unwrap();
    let mut events = 0;
    let delta = operator
        .apply(&edges(&[(1, 0, 1)]), LIMBS, &mut |_| {
            events += 1;
            if events > 200 { Err(events) } else { Ok(()) }
        })
        .unwrap();
    assert_eq!(
        plain(&delta),
        BTreeMap::from([((0, 0), 1), ((1, 0), 1), ((1, 1), 1)])
    );
    assert!(operator.contains(&19_998, &19_999));
}
