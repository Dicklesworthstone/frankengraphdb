use super::*;
use crate::ZSetError;

const LIMBS: LimbLimit = LimbLimit::new(4);

fn vertices(ids: impl IntoIterator<Item = (u8, i128)>) -> ZSet<u8> {
    ZSet::from_updates(
        ids.into_iter().map(|(v, w)| (v, ZWeight::from_i128(w))),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn edges(rows: impl IntoIterator<Item = ((u8, u8), i128)>) -> ZSet<(u8, u8)> {
    ZSet::from_updates(
        rows.into_iter().map(|(e, w)| (e, ZWeight::from_i128(w))),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn seed() -> IncrementalStrongComponents<u8> {
    let mut state = IncrementalStrongComponents::new();
    state
        .apply(
            &vertices((0..5).map(|v| (v, 1))),
            &edges([
                ((0, 1), 2),
                ((1, 0), 1),
                ((1, 2), 1),
                ((3, 4), 1),
                ((4, 3), 1),
            ]),
            LIMBS,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
    state
}
fn labels(state: &IncrementalStrongComponents<u8>) -> BTreeMap<u8, u8> {
    state.pairs().map(|(v, root)| (*v, *root)).collect()
}

// Independent tiny-graph oracle: reflexive reachability and mutual reachability,
// not DFS finish order, the maintained weak index or the production partitioner.
fn oracle(mask: u16) -> BTreeMap<u8, u8> {
    let mut reachable = [[false; 3]; 3];
    for (a, row) in reachable.iter_mut().enumerate() {
        for (b, cell) in row.iter_mut().enumerate() {
            *cell = a == b || mask & (1 << (a * 3 + b)) != 0;
        }
    }
    for k in 0..3 {
        for a in 0..3 {
            for b in 0..3 {
                reachable[a][b] |= reachable[a][k] && reachable[k][b];
            }
        }
    }
    (0..3)
        .map(|v| {
            let root = (0..3)
                .find(|r| reachable[v][*r] && reachable[*r][v])
                .unwrap();
            (v as u8, root as u8)
        })
        .collect()
}
fn mask_edges(mask: u16) -> ZSet<(u8, u8)> {
    edges(
        (0..9)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| (((bit / 3) as u8, (bit % 3) as u8), 1)),
    )
}
fn derivative(before: &BTreeMap<u8, u8>, after: &BTreeMap<u8, u8>) -> ZSet<(u8, u8)> {
    edges(
        before
            .iter()
            .map(|(v, r)| ((*v, *r), -1))
            .chain(after.iter().map(|(v, r)| ((*v, *r), 1))),
    )
}

#[test]
fn every_three_vertex_graph_and_single_arc_change_matches_independent_oracle() {
    let vertices = vertices((0..3).map(|v| (v, 1)));
    for mask in 0..512u16 {
        let mut state = IncrementalStrongComponents::new();
        let initial = state
            .apply(
                &vertices,
                &mask_edges(mask),
                LIMBS,
                &mut |_| Ok::<_, ()>(()),
            )
            .unwrap();
        let expected = oracle(mask);
        assert_eq!(labels(&state), expected, "initial mask {mask}");
        assert_eq!(initial, derivative(&BTreeMap::new(), &expected));
        for bit in 0..9 {
            let next_mask = mask ^ (1 << bit);
            let sign = if mask & (1 << bit) == 0 { 1 } else { -1 };
            let edge = ((bit / 3) as u8, (bit % 3) as u8);
            let delta = state
                .apply(&ZSet::new(), &edges([(edge, sign)]), LIMBS, &mut |_| {
                    Ok::<_, ()>(())
                })
                .unwrap();
            let after = oracle(next_mask);
            assert_eq!(labels(&state), after, "mask {mask} bit {bit}");
            assert_eq!(delta, derivative(&expected, &after));
            assert_eq!(
                state.component_count(),
                after.values().collect::<BTreeSet<_>>().len()
            );
            assert_eq!(state.vertex_count(), 3);
            let reverse = state
                .apply(&ZSet::new(), &edges([(edge, -sign)]), LIMBS, &mut |_| {
                    Ok::<_, ()>(())
                })
                .unwrap();
            assert_eq!(reverse, derivative(&after, &expected));
            assert_eq!(labels(&state), expected);
        }
    }
}

#[test]
fn direction_changes_rederive_even_when_the_weak_delta_is_empty() {
    let mut state = IncrementalStrongComponents::new();
    state
        .apply(
            &vertices((0..3).map(|v| (v, 1))),
            &edges([((0, 1), 1), ((1, 2), 1), ((0, 2), 1)]),
            LIMBS,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(state.component_count(), 3);
    let pending = state
        .prepare(
            &ZSet::new(),
            &edges([((0, 2), -1), ((2, 0), 1)]),
            LIMBS,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert!(pending.weak.delta().is_empty());
    assert_eq!(pending.affected_vertices(), 3);
    assert_eq!(pending.component_count(), 1);
    pending.commit();
    assert_eq!(labels(&state), BTreeMap::from([(0, 0), (1, 0), (2, 0)]));
}

#[test]
fn parallel_support_and_vertex_cascades_preserve_other_regions() {
    let mut state = seed();
    let unchanged = state
        .prepare(&ZSet::new(), &edges([((0, 1), -1)]), LIMBS, &mut |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(unchanged.affected_vertices(), 0);
    assert!(unchanged.delta().is_empty());
    unchanged.commit();
    let split = state
        .prepare(&ZSet::new(), &edges([((0, 1), -1)]), LIMBS, &mut |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(split.affected_vertices(), 3);
    assert_eq!(split.component_count(), 4);
    split.commit();
    assert_eq!(state.representative(&4), Some(&3));
    state
        .apply(
            &vertices([(1, -1)]),
            &edges([((1, 0), -1), ((1, 2), -1)]),
            LIMBS,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        labels(&state),
        BTreeMap::from([(0, 0), (2, 2), (3, 3), (4, 3)])
    );
    assert_eq!(state.component_count(), 3);
    assert_eq!(state.vertex_count(), 4);
    assert_eq!(state.representative(&1), None);
    let before = state.snapshot(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
    {
        let pending = state
            .prepare(
                &vertices([(9, 1)]),
                &edges([((9, 9), 1)]),
                LIMBS,
                &mut |_| Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(pending.affected_vertices(), 1);
        // Leaving the preparation uncommitted must abort every arrangement.
    }
    assert_eq!(
        state.snapshot(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap(),
        before
    );
    assert_eq!(state.representative(&9), None);
}

#[test]
fn malformed_directional_counts_and_dangling_endpoints_leave_all_state_unchanged() {
    let mut state = seed();
    for (vertex_delta, edge_delta, expected) in [
        (
            vertices([]),
            edges([((2, 1), -1), ((1, 2), 1)]),
            ComponentError::NegativeEdgeMultiplicity,
        ),
        (
            vertices([(0, -1)]),
            edges([((0, 1), -1), ((1, 0), -1)]),
            ComponentError::MissingEndpoint,
        ),
        (
            vertices([]),
            edges([((0, 9), 1)]),
            ComponentError::MissingEndpoint,
        ),
        (
            vertices([(0, 1)]),
            edges([]),
            ComponentError::VertexMultiplicity,
        ),
    ] {
        assert_eq!(
            state
                .prepare(&vertex_delta, &edge_delta, LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap_err(),
            expected
        );
        assert_eq!(state, seed());
    }
}

#[test]
fn every_control_refusal_and_dropped_update_preserves_every_arrangement() {
    let mut state = seed();
    let vertex_delta = vertices([(6, 1)]);
    let edge_delta = edges([((2, 0), 1), ((2, 6), 1), ((6, 3), 1)]);
    let mut total = 0;
    let expected = {
        let pending = state
            .prepare(&vertex_delta, &edge_delta, LIMBS, &mut |_| {
                total += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        pending
            .delta()
            .map(|row| Ok(*row), LIMBS, &mut |_| Ok::<_, usize>(()))
            .unwrap()
    };
    assert!(total > 20);
    assert_eq!(state, seed());
    for stop in 1..=total {
        let mut seen = 0;
        let error = state
            .prepare(&vertex_delta, &edge_delta, LIMBS, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            })
            .unwrap_err();
        assert_eq!(error, ComponentError::Delta(ZSetError::Control(stop)));
        assert_eq!(seen, stop);
        assert_eq!(state, seed(), "interruption {stop}");
    }
    let mut calls = 0;
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = state.prepare(&vertex_delta, &edge_delta, LIMBS, &mut |_| {
            calls += 1;
            assert!(calls != total / 2, "injected preparation unwind");
            Ok::<_, usize>(())
        });
    }));
    assert!(unwound.is_err());
    assert_eq!(state, seed());
    let actual = state
        .apply(&vertex_delta, &edge_delta, LIMBS, &mut |_| {
            Ok::<_, usize>(())
        })
        .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn iterative_walk_handles_a_long_cycle_and_local_updates_skip_unrelated_vertices() {
    let mut state = IncrementalStrongComponents::<u32>::new();
    let vertex_delta =
        ZSet::from_updates((0..10002).map(|v| (v, ZWeight::ONE)), LIMBS, &mut |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
    let edge_delta = ZSet::from_updates(
        (0..10000)
            .map(|v| ((v, (v + 1) % 10000), ZWeight::ONE))
            .chain([((10000, 10001), ZWeight::ONE)]),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    state
        .apply(&vertex_delta, &edge_delta, LIMBS, &mut |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(state.component_count(), 3);
    assert_eq!(state.representative(&9999), Some(&0));
    let edge_delta = ZSet::from_updates([((10001, 10000), ZWeight::ONE)], LIMBS, &mut |_| {
        Ok::<_, ()>(())
    })
    .unwrap();
    let mut work = 0;
    let pending = state
        .prepare(&ZSet::new(), &edge_delta, LIMBS, &mut |event| {
            if event == ZSetEvent::Work {
                work += 1;
            }
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(pending.affected_vertices(), 2);
    assert!(
        work < 256,
        "unrelated 10000-vertex cycle was traversed: {work}"
    );
    pending.commit();
    assert_eq!(state.component_count(), 2);
    assert_eq!(state.representative(&10001), Some(&10000));
}
