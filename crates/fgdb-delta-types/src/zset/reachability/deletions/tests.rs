use super::*;

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}

fn z(rows: impl IntoIterator<Item = ((usize, usize), i128)>) -> ZSet<(usize, usize)> {
    ZSet::from_updates(
        rows.into_iter()
            .map(|(edge, weight)| (edge, ZWeight::from_i128(weight))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}

// Independent finite-domain closure. No diagonal is initialized unless a real
// positive-length path reaches it; no retained arrangement or deletion cone is
// consulted. This checks graphs with loops and cyclic strongly connected parts.
fn oracle(edges: &BTreeSet<(usize, usize)>, n: usize) -> BTreeSet<(usize, usize)> {
    let mut paths = vec![vec![false; n]; n];
    for &(source, target) in edges {
        paths[source][target] = true;
    }
    for middle in 0..n {
        for source in 0..n {
            for target in 0..n {
                let through_middle = paths[source][middle] && paths[middle][target];
                paths[source][target] |= through_middle;
            }
        }
    }
    paths
        .iter()
        .enumerate()
        .flat_map(|(source, row)| {
            row.iter()
                .enumerate()
                .filter_map(move |(target, &present)| present.then_some((source, target)))
        })
        .collect()
}

fn pairs(state: &IncrementalReachability<usize>) -> BTreeSet<(usize, usize)> {
    state
        .pairs()
        .map(|(&source, &target)| (source, target))
        .collect()
}

fn arrangements(
    state: &IncrementalReachability<usize>,
    edges: &BTreeSet<(usize, usize)>,
    expected: &BTreeSet<(usize, usize)>,
) {
    let flatten = |relation: &Relation<usize>| -> BTreeSet<(usize, usize)> {
        relation
            .iter()
            .flat_map(|(&source, row)| row.iter().map(move |&target| (source, target)))
            .collect()
    };
    assert_eq!(flatten(&state.outgoing), *edges);
    assert_eq!(
        flatten(&state.incoming),
        edges.iter().map(|&(s, t)| (t, s)).collect::<BTreeSet<_>>()
    );
    assert_eq!(
        flatten(&state.predecessors),
        expected
            .iter()
            .map(|&(s, t)| (t, s))
            .collect::<BTreeSet<_>>()
    );
    for relation in [
        &state.outgoing,
        &state.incoming,
        &state.reachable,
        &state.predecessors,
    ] {
        assert!(
            relation.values().all(|row| !row.is_empty()),
            "empty retained arrangement group"
        );
    }
    assert_eq!(pairs(state), *expected);
}

#[test]
fn every_three_vertex_decremental_transition_and_reinsertion_matches_batch() {
    // Each of the nine directed edges is absent, retained, or removed.
    // All 3^9 old >= new topology pairs are covered, not just single-edge ticks.
    for mut code in 0..3_usize.pow(9) {
        let mut old = BTreeSet::new();
        let mut removed = BTreeSet::new();
        for edge in 0..9 {
            let pair = (edge / 3, edge % 3);
            let state = code % 3;
            if state != 0 {
                old.insert(pair);
            }
            if state == 2 {
                removed.insert(pair);
            }
            code /= 3;
        }
        let mut operator = IncrementalReachability::new();
        operator
            .apply(&z(old.iter().map(|&edge| (edge, 1))), LIMBS, &mut allow)
            .unwrap();
        let before = oracle(&old, 3);
        arrangements(&operator, &old, &before);
        let update = operator
            .prepare(
                &z(removed.iter().map(|&edge| (edge, -1))),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        assert!(
            update.replacements.is_empty(),
            "deletion copied an old source row"
        );
        assert!(update.additions.is_empty());
        let delta = update.commit();
        let remaining: BTreeSet<_> = old.difference(&removed).copied().collect();
        let after = oracle(&remaining, 3);
        let losses: BTreeSet<_> = before.difference(&after).copied().collect();
        assert_eq!(
            delta.iter().map(|(edge, _)| *edge).collect::<BTreeSet<_>>(),
            losses
        );
        assert!(delta.iter().all(|(_, weight)| weight.to_i128() == Some(-1)));
        arrangements(&operator, &remaining, &after);
        let mut sink = z(before.iter().map(|&pair| (pair, 1)));
        sink.integrate(&delta, LIMBS, &mut allow).unwrap();
        assert_eq!(sink, operator.snapshot(LIMBS, &mut allow).unwrap());
        // Every reverse/topology index must support a later insertion, not just
        // the right one-off exported answer after the deletion.
        operator
            .apply(&z(removed.iter().map(|&edge| (edge, 1))), LIMBS, &mut allow)
            .unwrap();
        arrangements(&operator, &old, &before);
    }
}

#[test]
fn boundary_paths_rederive_cycles_but_disconnected_cycles_cannot_self_support() {
    let mut edges: BTreeSet<_> = [(0, 1), (1, 2), (2, 1), (0, 3), (3, 2)].into();
    let mut operator = IncrementalReachability::new();
    operator
        .apply(&z(edges.iter().map(|&edge| (edge, 1))), LIMBS, &mut allow)
        .unwrap();
    let delta = operator
        .apply(&z([((0, 1), -1)]), LIMBS, &mut allow)
        .unwrap();
    assert!(
        delta.is_empty(),
        "the alternate boundary path still reaches the cycle"
    );
    edges.remove(&(0, 1));
    arrangements(&operator, &edges, &oracle(&edges, 4));
    operator
        .apply(&z([((0, 3), -1)]), LIMBS, &mut allow)
        .unwrap();
    edges.remove(&(0, 3));
    assert!(!operator.contains(&0, &1));
    assert!(!operator.contains(&0, &2));
    assert!(operator.contains(&1, &1));
    assert!(operator.contains(&2, &2));
    arrangements(&operator, &edges, &oracle(&edges, 4));
    operator
        .apply(&z([((2, 1), -1)]), LIMBS, &mut allow)
        .unwrap();
    edges.remove(&(2, 1));
    assert!(!operator.contains(&1, &1));
    assert!(!operator.contains(&2, &2));
    assert!(operator.contains(&1, &2));
    arrangements(&operator, &edges, &oracle(&edges, 4));
}

#[test]
fn every_cancellation_boundary_and_downstream_abort_preserve_all_arrangements() {
    let base = z([
        ((0, 1), 1),
        ((1, 2), 1),
        ((2, 1), 1),
        ((2, 3), 1),
        ((0, 4), 1),
        ((4, 2), 1),
        ((3, 5), 1),
    ]);
    let removal = z([((0, 1), -1), ((4, 2), -1), ((2, 3), -1)]);
    let mut operator = IncrementalReachability::new();
    operator.apply(&base, LIMBS, &mut allow).unwrap();
    let mut reference = IncrementalReachability::new();
    reference.apply(&base, LIMBS, &mut allow).unwrap();
    let mut calls = 0;
    {
        let update = operator
            .prepare(&removal, LIMBS, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        assert!(!update.delta().is_empty());
        drop(update);
    }
    assert_eq!(operator, reference);
    for stop in 1..=calls {
        let mut visited = 0;
        match operator.prepare(&removal, LIMBS, &mut |_| {
            visited += 1;
            if visited == stop { Err(stop) } else { Ok(()) }
        }) {
            Err(error) => assert_eq!(error, ReachabilityError::Delta(ZSetError::Control(stop))),
            Ok(update) => {
                drop(update);
                panic!("cancellation boundary {stop} was not visited");
            }
        }
        assert_eq!(operator, reference);
    }
    let mut sink = reference.snapshot(LIMBS, &mut allow).unwrap();
    let sink_before = reference.snapshot(LIMBS, &mut allow).unwrap();
    {
        let update = operator.prepare(&removal, LIMBS, &mut allow).unwrap();
        assert!(
            sink.prepare_update(update.delta(), LIMBS, &mut |_| Err::<(), _>(123usize))
                .is_err()
        );
        drop(update);
    }
    assert_eq!(operator, reference);
    assert_eq!(sink, sink_before);
    let expected = operator
        .prepare(&removal, LIMBS, &mut allow)
        .unwrap()
        .commit();
    sink.integrate(&expected, LIMBS, &mut allow).unwrap();
    assert_eq!(sink, operator.snapshot(LIMBS, &mut allow).unwrap());
}

#[test]
fn tail_deletion_work_and_scratch_scale_with_lost_pairs_not_unchanged_paths() {
    for n in [32, 64, 128] {
        let mut operator = IncrementalReachability::new();
        operator
            .apply(&z((0..n).map(|a| ((a, a + 1), 1))), LIMBS, &mut allow)
            .unwrap();
        let (mut work, mut scratch) = (0, 0);
        let delta = operator
            .apply(&z([((n - 1, n), -1)]), LIMBS, &mut |event| {
                match event {
                    ZSetEvent::Work => work += 1,
                    ZSetEvent::ScratchEntry => scratch += 1,
                }
                Ok::<_, usize>(())
            })
            .unwrap();
        assert_eq!(delta.len(), n);
        assert!(delta.iter().all(|(&(source, target), weight)| source < n
            && target == n
            && weight.to_i128() == Some(-1)));
        // These are fixed logical-operation bounds, not elapsed-time claims.
        // The former whole-source rederivation is quadratic on this fixture.
        assert!(work < 32 * n + 64, "work={work}, n={n}");
        assert!(scratch < 12 * n + 64, "scratch={scratch}, n={n}");
        assert_eq!(operator.pairs().count(), n * (n - 1) / 2);
        assert!(operator.contains(&0, &(n - 1)));
        assert!(!operator.contains(&0, &n));
    }
}

#[test]
fn unrelated_components_do_not_enter_deletion_work_or_scratch() {
    let measure = |unrelated: usize| {
        let mut operator = IncrementalReachability::new();
        let base = z([(0, 1), (1, 2), (2, 3)]
            .into_iter()
            .chain((10..10 + unrelated).map(|a| (a, a + 1)))
            .map(|edge| (edge, 1)));
        operator.apply(&base, LIMBS, &mut allow).unwrap();
        let mut events = (0, 0);
        let delta = operator
            .apply(&z([((2, 3), -1)]), LIMBS, &mut |event| {
                match event {
                    ZSetEvent::Work => events.0 += 1,
                    ZSetEvent::ScratchEntry => events.1 += 1,
                }
                Ok::<_, usize>(())
            })
            .unwrap();
        assert_eq!(delta.len(), 3);
        events
    };
    assert_eq!(measure(0), measure(128));
}

#[test]
fn partial_parallel_retractions_and_mixed_ticks_keep_incoming_support_exact() {
    let mut operator = IncrementalReachability::new();
    operator
        .apply(&z([((0, 1), i128::MAX), ((1, 2), 1)]), LIMBS, &mut allow)
        .unwrap();
    operator
        .apply(&z([((0, 1), 1)]), LIMBS, &mut allow)
        .unwrap();
    assert!(operator.edge_weight(&(0, 1)).unwrap().is_promoted());
    assert!(
        operator
            .apply(&z([((0, 1), -i128::MAX)]), LIMBS, &mut allow)
            .unwrap()
            .is_empty()
    );
    assert_eq!(operator.edge_weight(&(0, 1)), Some(&ZWeight::ONE));
    let before = operator.snapshot(LIMBS, &mut allow).unwrap();
    assert!(matches!(
        operator.prepare(&z([((0, 1), -2)]), LIMBS, &mut allow),
        Err(ReachabilityError::NegativeMultiplicity)
    ));
    assert_eq!(operator.snapshot(LIMBS, &mut allow).unwrap(), before);
    operator
        .apply(
            &z([((0, 1), -1), ((0, 2), 1), ((2, 1), 1)]),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    let mut edges: BTreeSet<_> = [(1, 2), (0, 2), (2, 1)].into();
    arrangements(&operator, &edges, &oracle(&edges, 3));
    operator
        .apply(&z([((0, 2), -1)]), LIMBS, &mut allow)
        .unwrap();
    edges.remove(&(0, 2));
    arrangements(&operator, &edges, &oracle(&edges, 3));
    assert!(!operator.contains(&0, &1));
    assert!(operator.contains(&1, &1));
}
