use super::*;

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn z<T: Ord>(updates: impl IntoIterator<Item = (T, i128)>) -> ZSet<T> {
    ZSet::from_updates(
        updates
            .into_iter()
            .map(|(key, weight)| (key, ZWeight::from_i128(weight))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn vertices(values: impl IntoIterator<Item = u128>) -> ZSet<u128> {
    z(values.into_iter().map(|v| (v, 1)))
}
fn rows(state: &IncrementalCoreNumbers<u128>) -> BTreeMap<u128, u64> {
    state.numbers().map(|(v, n)| (*v, *n)).collect()
}

// Independent repeated threshold deletion. No degree-priority queue, no
// component labels, and no calls into the maintained graph or its snapshot.
fn oracle(vertices: &[u128], edges: &ZSet<(u128, u128)>) -> BTreeMap<u128, u64> {
    let mut adjacency: BTreeMap<u128, BTreeSet<u128>> =
        vertices.iter().map(|v| (*v, BTreeSet::new())).collect();
    for (&(a, b), weight) in edges.iter() {
        assert!(weight > &ZWeight::ZERO);
        assert!(adjacency.contains_key(&a) && adjacency.contains_key(&b));
        if a != b {
            adjacency.get_mut(&a).unwrap().insert(b);
            adjacency.get_mut(&b).unwrap().insert(a);
        }
    }
    let mut result: BTreeMap<_, _> = vertices.iter().map(|v| (*v, 0)).collect();
    for threshold in 1..vertices.len() {
        let mut alive: BTreeSet<_> = vertices.iter().copied().collect();
        loop {
            let doomed: Vec<_> = alive
                .iter()
                .copied()
                .filter(|v| {
                    adjacency[v]
                        .iter()
                        .filter(|neighbor| alive.contains(*neighbor))
                        .count()
                        < threshold
                })
                .collect();
            if doomed.is_empty() {
                break;
            }
            for v in doomed {
                alive.remove(&v);
            }
        }
        for v in alive {
            result.insert(v, threshold as u64);
        }
    }
    result
}
fn graph(mask: u64) -> ZSet<(u128, u128)> {
    let sides = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];
    z(sides
        .into_iter()
        .enumerate()
        .filter_map(|(at, edge)| (mask & (1 << at) != 0).then_some((edge, 1))))
}
fn build(v: &[u128], edges: &ZSet<(u128, u128)>) -> IncrementalCoreNumbers<u128> {
    let mut state = IncrementalCoreNumbers::new();
    state
        .apply(&vertices(v.iter().copied()), edges, LIMBS, &mut allow)
        .unwrap();
    state
}

#[test]
fn every_four_vertex_transition_and_reverse_matches_threshold_fixed_points() {
    let domain = [0, 1, 2, 3];
    for before in 0..64 {
        for after in 0..64 {
            let old = graph(before);
            let new = graph(after);
            let mut state = build(&domain, &old);
            let reference = build(&domain, &old);
            assert_eq!(rows(&state), oracle(&domain, &old));
            let mut sink = state.snapshot(LIMBS, &mut allow).unwrap();
            let change = new.minus(&old, LIMBS, &mut allow).unwrap();
            let delta = state
                .apply(&ZSet::new(), &change, LIMBS, &mut allow)
                .unwrap();
            sink.integrate(&delta, LIMBS, &mut allow).unwrap();
            assert_eq!(rows(&state), oracle(&domain, &new), "{before}->{after}");
            assert_eq!(sink, state.snapshot(LIMBS, &mut allow).unwrap());
            assert_eq!(state, build(&domain, &new));
            let inverse = change.negated(LIMBS, &mut allow).unwrap();
            state
                .apply(&ZSet::new(), &inverse, LIMBS, &mut allow)
                .unwrap();
            assert_eq!(state, reference, "round trip {before}->{after}");
        }
    }
}

#[test]
fn dense_core_decrements_cascade_but_isolates_and_wide_ids_remain_exact() {
    let domain = [0, 1, 1 << 100, u128::MAX];
    let mut edges = Vec::new();
    for i in 0..domain.len() {
        for j in i + 1..domain.len() {
            edges.push(((domain[i], domain[j]), 1));
        }
    }
    let complete = z(edges);
    let mut state = build(&domain, &complete);
    assert!(rows(&state).values().all(|n| *n == 3));
    let retired = z(domain[1..].iter().map(|v| ((0, *v), -1)));
    {
        let pending = state
            .prepare(&z([(0, -1)]), &retired, LIMBS, &mut allow)
            .unwrap();
        assert_eq!(pending.core_number(&0), None);
        assert_eq!(pending.core_number(&u128::MAX), Some(2));
        assert_eq!(pending.vertex_count(), 3);
        drop(pending);
    }
    assert_eq!(
        state.core_number(&0),
        Some(3),
        "guard drop must not retire the vertex"
    );
    state
        .apply(&z([(0, -1)]), &retired, LIMBS, &mut allow)
        .unwrap();
    assert!(rows(&state).values().all(|n| *n == 2));
    assert_eq!(state.core_number(&0), None);
    state
        .apply(&vertices([0]), &ZSet::new(), LIMBS, &mut allow)
        .unwrap();
    assert_eq!(state.core_number(&0), Some(0));
    let remaining = complete.plus(&retired, LIMBS, &mut allow).unwrap();
    assert_eq!(rows(&state), oracle(&domain, &remaining));
    state
        .apply(
            &ZSet::new(),
            &remaining.negated(LIMBS, &mut allow).unwrap(),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    assert!(rows(&state).values().all(|n| *n == 0));
}

#[test]
fn parallel_opposite_edges_and_loops_have_explicit_simple_support_semantics() {
    let domain = [0, 1, 2, 3];
    let edges = z([((0, 1), i128::MAX), ((1, 2), 1), ((2, 0), 1), ((3, 3), 100)]);
    let mut state = build(&domain, &edges);
    assert_eq!(
        rows(&state),
        BTreeMap::from([(0, 2), (1, 2), (2, 2), (3, 0)])
    );
    let pending = state
        .prepare(&ZSet::new(), &z([((1, 0), 1)]), LIMBS, &mut allow)
        .unwrap();
    assert_eq!(pending.affected_vertices(), 0);
    assert!(pending.delta().is_empty());
    let _ = pending.commit();
    assert!(state.topology.edges.weight(&(0, 1)).unwrap().is_promoted());
    let pending = state
        .prepare(&ZSet::new(), &z([((0, 1), -i128::MAX)]), LIMBS, &mut allow)
        .unwrap();
    assert_eq!(pending.affected_vertices(), 0);
    assert!(pending.delta().is_empty());
    let _ = pending.commit();
    assert_eq!(state.core_number(&0), Some(2));
    state
        .apply(&ZSet::new(), &z([((1, 0), -1)]), LIMBS, &mut allow)
        .unwrap();
    assert_eq!(
        rows(&state),
        BTreeMap::from([(0, 1), (1, 1), (2, 1), (3, 0)])
    );
    state
        .apply(&z([(3, -1)]), &z([((3, 3), -100)]), LIMBS, &mut allow)
        .unwrap();
    assert_eq!(state.core_number(&3), None);
}

#[test]
fn every_topology_peeling_output_and_downstream_refusal_preserves_the_old_state() {
    let old = graph(63);
    let change = z([((0, 1), -1), ((0, 2), -1), ((1, 3), -1)]);
    let reference = build(&[0, 1, 2, 3], &old);
    let mut success = build(&[0, 1, 2, 3], &old);
    let mut calls = 0;
    let pending = success
        .prepare(&ZSet::new(), &change, LIMBS, &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
    assert!(!pending.delta().is_empty());
    let _ = pending.commit();
    for stop in 1..=calls {
        let mut state = build(&[0, 1, 2, 3], &old);
        let mut visited = 0;
        let result = state
            .prepare(&ZSet::new(), &change, LIMBS, &mut |_| {
                visited += 1;
                if visited == stop { Err(stop) } else { Ok(()) }
            })
            .map(|pending| {
                drop(pending);
            });
        assert!(matches!(result,
            Err(CoreError::Delta(ZSetError::Control(at)))
            | Err(CoreError::Topology(ComponentError::Delta(ZSetError::Control(at)))) if at == stop
        ));
        assert_eq!(visited, stop);
        assert_eq!(state, reference);
        state
            .apply(&ZSet::new(), &change, LIMBS, &mut allow)
            .unwrap();
        assert_eq!(state, success);
    }
    let mut state = build(&[0, 1, 2, 3], &old);
    let mut sink = state.snapshot(LIMBS, &mut allow).unwrap();
    let old_sink = sink.checked_clone(LIMBS, &mut allow).unwrap();
    let pending = state
        .prepare(&ZSet::new(), &change, LIMBS, &mut allow)
        .unwrap();
    assert!(
        sink.prepare_update(pending.delta(), LIMBS, &mut |_| Err(99usize))
            .is_err()
    );
    drop(pending);
    assert_eq!(state, reference);
    assert_eq!(sink, old_sink);
}

#[test]
fn bad_endpoints_counts_and_promotion_refuse_before_any_accepted_change() {
    let edges = z([((0, 1), i128::MAX)]);
    let before = build(&[0, 1, 2], &edges);
    let cases = [
        (z([(0, -1)]), ZSet::new()),
        (z([(2, 1)]), ZSet::new()),
        (ZSet::new(), z([((2, 99), 1)])),
        (ZSet::new(), z([((1, 2), -1)])),
    ];
    for (vertices, delta) in cases {
        let mut state = build(&[0, 1, 2], &edges);
        assert!(matches!(
            state.prepare(&vertices, &delta, LIMBS, &mut allow),
            Err(CoreError::Topology(_))
        ));
        assert_eq!(state, before);
    }
    let mut state = build(&[0, 1, 2], &edges);
    assert!(matches!(
        state.prepare(
            &ZSet::new(),
            &z([((0, 1), 1)]),
            LimbLimit::new(0),
            &mut allow
        ),
        Err(CoreError::Topology(ComponentError::Delta(
            ZSetError::Arithmetic(_)
        )))
    ));
    assert_eq!(state, before);
    let pending = state
        .prepare(
            &z([(0, -1), (9, 1)]),
            &z([((0, 1), -i128::MAX), ((1, 9), 1)]),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    assert_eq!(pending.core_number(&0), None);
    assert_eq!(pending.core_number(&2), Some(0));
    assert_eq!(pending.core_number(&9), Some(1));
    let _ = pending.commit();
}

#[test]
fn unrelated_components_do_not_increase_update_work_or_scratch() {
    let mut measured = Vec::new();
    for extra in [0, 64, 512] {
        let domain: Vec<u128> = (0..3).chain(1000..1000 + extra).collect();
        let mut edge_rows = vec![((0, 1), 1), ((1, 2), 1), ((0, 2), 1)];
        for vertex in 1000..1000 + extra.saturating_sub(1) {
            edge_rows.push(((vertex, vertex + 1), 1));
        }
        let mut state = build(&domain, &z(edge_rows));
        let mut counts = (0, 0);
        let pending = state
            .prepare(&ZSet::new(), &z([((0, 1), -1)]), LIMBS, &mut |event| {
                match event {
                    ZSetEvent::Work => counts.0 += 1,
                    ZSetEvent::ScratchEntry => counts.1 += 1,
                }
                Ok::<_, usize>(())
            })
            .unwrap();
        assert_eq!(pending.affected_vertices(), 3);
        assert_eq!(pending.delta().len(), 6);
        let _ = pending.commit();
        assert_eq!(state.vertex_count(), 3 + extra as usize);
        measured.push(counts);
    }
    assert_eq!(measured[0], measured[1]);
    assert_eq!(measured[0], measured[2]);
}
