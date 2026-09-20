use super::*;

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn z<T: Ord>(rows: impl IntoIterator<Item = (T, i128)>) -> ZSet<T> {
    ZSet::from_updates(
        rows.into_iter().map(|(k, w)| (k, ZWeight::from_i128(w))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn vertices(values: impl IntoIterator<Item = u128>) -> ZSet<u128> {
    z(values.into_iter().map(|v| (v, 1)))
}
// Full-graph repeated minimum propagation, independent of the affected-region
// BFS, canonical normalization and the retained member index under test.
fn oracle(vertices: &[u128], edges: &[(u128, u128)]) -> BTreeMap<u128, u128> {
    let mut labels: BTreeMap<_, _> = vertices.iter().map(|&v| (v, v)).collect();
    loop {
        let mut changed = false;
        for &(a, b) in edges {
            let low = labels[&a].min(labels[&b]);
            for v in [a, b] {
                if labels[&v] != low {
                    labels.insert(v, low);
                    changed = true;
                }
            }
        }
        if !changed {
            return labels;
        }
    }
}
fn check(state: &IncrementalComponents<u128>, vs: &[u128], edges: &[(u128, u128)]) {
    let expected = oracle(vs, edges);
    assert_eq!(
        state
            .pairs()
            .map(|(v, r)| (*v, *r))
            .collect::<BTreeMap<_, _>>(),
        expected
    );
    assert_eq!(
        state.component_count(),
        expected.values().collect::<BTreeSet<_>>().len()
    );
    assert_eq!(state.vertex_count(), vs.len());
    let result = state.snapshot(LIMBS, &mut allow).unwrap();
    assert_eq!(result.len(), vs.len());
    assert!(result.iter().all(|(_, w)| w == &ZWeight::ONE));
}
fn simple(mask: u64) -> Vec<(u128, u128)> {
    let mut bit = 0;
    let mut edges = Vec::new();
    for a in 0..4 {
        for b in a + 1..4 {
            if mask & (1 << bit) != 0 {
                edges.push((a, b));
            }
            bit += 1;
        }
    }
    edges
}

#[test]
fn every_four_vertex_graph_transition_has_the_exact_membership_derivative() {
    let vs = vertices(0..4);
    for before in 0..64 {
        for after in 0..64 {
            let a = simple(before);
            let b = simple(after);
            let mut state = IncrementalComponents::new();
            state
                .apply(&vs, &z(a.iter().map(|&e| (e, 1))), LIMBS, &mut allow)
                .unwrap();
            let mut sink = state.snapshot(LIMBS, &mut allow).unwrap();
            let delta = z(a.iter().map(|&e| (e, -1)).chain(b.iter().map(|&e| (e, 1))));
            let update = state
                .prepare(&ZSet::new(), &delta, LIMBS, &mut allow)
                .unwrap();
            assert_eq!(update.vertex_count(), 4);
            assert_eq!(
                update.component_count(),
                oracle(&[0, 1, 2, 3], &b)
                    .values()
                    .collect::<BTreeSet<_>>()
                    .len()
            );
            sink.integrate(&update.commit(), LIMBS, &mut allow).unwrap();
            check(&state, &[0, 1, 2, 3], &b);
            assert_eq!(sink, state.snapshot(LIMBS, &mut allow).unwrap());
        }
    }
}

#[test]
fn isolates_self_loops_cascades_and_changed_minima_use_full_width_identity() {
    let wide = 1_u128 << 100;
    let mut state = IncrementalComponents::new();
    let vs = [0, 1, 9, wide, u128::MAX];
    state
        .apply(
            &vertices(vs),
            &z([((0, 1), 1), ((1, 9), 1), ((wide, wide), 1)]),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    check(&state, &vs, &[(0, 1), (1, 9), (wide, wide)]);
    // Retire the canonical minimum and its incident support in one whole tick.
    state
        .apply(&z([(0, -1)]), &z([((0, 1), -1)]), LIMBS, &mut allow)
        .unwrap();
    assert_eq!(state.representative(&9), Some(&1));
    assert_eq!(state.representative(&0), None);
    state
        .apply(
            &ZSet::new(),
            &z([((1, 9), -1), ((wide, wide), -1)]),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    check(&state, &[1, 9, wide, u128::MAX], &[]);
    state
        .apply(
            &vertices([0]),
            &z([((u128::MAX, 9), 1), ((9, 0), 1)]),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    check(&state, &vs, &[(u128::MAX, 9), (9, 0)]);
    state
        .apply(
            &z(vs.map(|v| (v, -1))),
            &z([((u128::MAX, 9), -1), ((9, 0), -1)]),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    check(&state, &[], &[]);
}

#[test]
fn parallel_opposite_support_and_promoted_counts_do_not_split_early() {
    let mut state = IncrementalComponents::new();
    state
        .apply(
            &vertices([1, 2]),
            &z([((1, 2), i128::MAX)]),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    let pending = state
        .prepare(&ZSet::new(), &z([((2, 1), 1)]), LIMBS, &mut allow)
        .unwrap();
    assert_eq!(pending.affected_vertices(), 0);
    assert!(pending.delta().is_empty());
    pending.commit();
    assert!(state.edges.weight(&(1, 2)).unwrap().is_promoted());
    assert!(
        state
            .apply(&ZSet::new(), &z([((2, 1), -i128::MAX)]), LIMBS, &mut allow)
            .unwrap()
            .is_empty()
    );
    check(&state, &[1, 2], &[(1, 2)]);
    state
        .apply(&ZSet::new(), &z([((1, 2), -1)]), LIMBS, &mut allow)
        .unwrap();
    check(&state, &[1, 2], &[]);
}

fn seeded() -> IncrementalComponents<u128> {
    let mut state = IncrementalComponents::new();
    state
        .apply(
            &vertices(0..6),
            &z([((0, 1), 1), ((1, 2), 1), ((2, 0), 1), ((3, 4), 1)]),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    state
}
#[test]
fn every_checkpoint_and_downstream_drop_preserve_all_arrangements_and_retry() {
    let before = seeded();
    let vd = z([(0, -1), (7, 1)]);
    let ed = z([((0, 1), -1), ((2, 0), -1), ((2, 3), 1), ((7, 5), 1)]);
    let mut success = seeded();
    let mut calls = 0;
    let delta = success
        .apply(&vd, &ed, LIMBS, &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
    let mut state = seeded();
    drop(state.prepare(&vd, &ed, LIMBS, &mut allow).unwrap());
    assert_eq!(state, before);
    for stop in 1..=calls {
        let mut seen = 0;
        let error = state
            .prepare(&vd, &ed, LIMBS, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            })
            .unwrap_err();
        assert_eq!(error, ComponentError::Delta(ZSetError::Control(stop)));
        assert_eq!(state, before);
    }
    assert_eq!(state.apply(&vd, &ed, LIMBS, &mut allow).unwrap(), delta);
    assert_eq!(state, success);
    check(
        &state,
        &[1, 2, 3, 4, 5, 7],
        &[(1, 2), (2, 3), (3, 4), (7, 5)],
    );
}

#[test]
fn invalid_final_membership_or_endpoint_never_changes_state() {
    let before = seeded();
    let mut state = seeded();
    for (vd, ed, expected) in [
        (z([(1, 1)]), ZSet::new(), ComponentError::VertexMultiplicity),
        (
            z([(9, -1)]),
            ZSet::new(),
            ComponentError::VertexMultiplicity,
        ),
        (
            ZSet::new(),
            z([((0, 1), -2)]),
            ComponentError::NegativeEdgeMultiplicity,
        ),
        (
            z([(0, -1)]),
            z([((0, 1), -1)]),
            ComponentError::MissingEndpoint,
        ),
        (
            ZSet::new(),
            z([((9, 1), 1)]),
            ComponentError::MissingEndpoint,
        ),
    ] {
        assert_eq!(
            state.prepare(&vd, &ed, LIMBS, &mut allow).unwrap_err(),
            expected
        );
        assert_eq!(state, before);
    }
    let mut promoted = IncrementalComponents::new();
    promoted
        .apply(
            &vertices([0, 1]),
            &z([((0, 1), i128::MAX)]),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    assert!(matches!(
        promoted.prepare(
            &ZSet::new(),
            &z([((0, 1), 1)]),
            LimbLimit::new(0),
            &mut allow
        ),
        Err(ComponentError::Delta(ZSetError::Arithmetic(_)))
    ));
    assert_eq!(
        promoted.edges.weight(&(0, 1)),
        Some(&ZWeight::from_i128(i128::MAX))
    );
}

#[test]
fn unrelated_components_do_not_increase_local_repair_work_or_scratch() {
    let mut costs = Vec::new();
    for unrelated in [0, 16, 128] {
        let mut state = seeded();
        state
            .apply(
                &vertices(100..100 + unrelated * 2),
                &z((0..unrelated).map(|i| ((100 + 2 * i, 101 + 2 * i), 1))),
                LIMBS,
                &mut allow,
            )
            .unwrap();
        let mut work = 0;
        let mut scratch = 0;
        let pending = state
            .prepare(
                &ZSet::new(),
                &z([((0, 1), -1), ((0, 2), -1)]),
                LIMBS,
                &mut |event| {
                    match event {
                        ZSetEvent::Work => work += 1,
                        ZSetEvent::ScratchEntry => scratch += 1,
                    }
                    Ok::<_, usize>(())
                },
            )
            .unwrap();
        assert_eq!(pending.affected_vertices(), 3);
        assert_eq!(pending.delta().len(), 4);
        pending.commit();
        assert_eq!(state.representative(&0), Some(&0));
        assert_eq!(state.representative(&2), Some(&1));
        costs.push((work, scratch));
    }
    assert!(costs.windows(2).all(|pair| pair[0] == pair[1]), "{costs:?}");
}
