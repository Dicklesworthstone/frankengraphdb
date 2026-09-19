use super::*;

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn edges(rows: impl IntoIterator<Item = ((usize, usize), i128)>) -> ZSet<(usize, usize)> {
    ZSet::from_updates(
        rows.into_iter()
            .map(|(edge, weight)| (edge, ZWeight::from_i128(weight))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn graph(mask: u64, n: usize) -> ZSet<(usize, usize)> {
    let mut bit = 0;
    let mut rows = Vec::new();
    for a in 0..n {
        for b in a + 1..n {
            if mask & (1 << bit) != 0 {
                rows.push(((a, b), 1));
            }
            bit += 1;
        }
    }
    edges(rows)
}
// Independent cubic oracle: enumerate all vertex triples directly. It does
// not use neighborhoods, changed-side ownership or the derivative algorithm.
fn oracle(
    input: &ZSet<(usize, usize)>,
    n: usize,
    quantifier: TriangleQuantifier,
) -> ZSet<(usize, usize, usize)> {
    let mut counts = vec![vec![0_i128; n]; n];
    for (&(a, b), weight) in input.iter() {
        counts[a.min(b)][a.max(b)] += weight.to_i128().unwrap();
    }
    let mut rows = Vec::new();
    for a in 0..n {
        for b in a + 1..n {
            for c in b + 1..n {
                let product = counts[a][b] * counts[a][c] * counts[b][c];
                if product > 0 {
                    let weight = if quantifier == TriangleQuantifier::Distinct {
                        1
                    } else {
                        product
                    };
                    rows.push(((a, b, c), ZWeight::from_i128(weight)));
                }
            }
        }
    }
    ZSet::from_updates(rows, LIMBS, &mut allow).unwrap()
}
fn check_index(state: &IncrementalTriangles<usize>, n: usize) {
    for a in 0..n {
        for b in 0..n {
            assert_eq!(
                state.neighbors.get(&a).is_some_and(|row| row.contains(&b)),
                a != b && state.edge_weight(&a, &b).is_some()
            );
        }
    }
}

#[test]
fn every_four_vertex_transition_matches_independent_recomputation() {
    for quantifier in [TriangleQuantifier::Distinct, TriangleQuantifier::All] {
        for before in 0..64 {
            let initial = graph(before, 4);
            let expected_before = oracle(&initial, 4, quantifier);
            for after in 0..64 {
                let mut state = IncrementalTriangles::new(quantifier);
                assert_eq!(
                    state.apply(&initial, LIMBS, &mut allow).unwrap(),
                    expected_before
                );
                let final_input = graph(after, 4);
                let change = final_input.minus(&initial, LIMBS, &mut allow).unwrap();
                let expected_after = oracle(&final_input, 4, quantifier);
                let expected_delta = expected_after
                    .minus(&expected_before, LIMBS, &mut allow)
                    .unwrap();
                assert_eq!(
                    state.apply(&change, LIMBS, &mut allow).unwrap(),
                    expected_delta,
                    "before={before}, after={after}"
                );
                assert_eq!(state.snapshot(LIMBS, &mut allow).unwrap(), expected_after);
                assert_eq!(
                    state.total(),
                    &expected_after.total_weight(LIMBS, &mut allow).unwrap()
                );
                check_index(&state, 4);
                state
                    .apply(
                        &change.negated(LIMBS, &mut allow).unwrap(),
                        LIMBS,
                        &mut allow,
                    )
                    .unwrap();
                assert_eq!(state.snapshot(LIMBS, &mut allow).unwrap(), expected_before);
                check_index(&state, 4);
            }
        }
    }
}

#[test]
fn weighted_mixed_streams_include_all_same_tick_cross_terms() {
    for quantifier in [TriangleQuantifier::Distinct, TriangleQuantifier::All] {
        let mut random = 0x5eed_u64;
        let mut input = ZSet::new();
        let mut sink = ZSet::new();
        let mut state = IncrementalTriangles::new(quantifier);
        for tick in 0..1000 {
            let mut rows = Vec::new();
            for a in 0..7 {
                for b in a..7 {
                    random = random
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    rows.push(((a, b), ((random >> 32) % 4) as i128));
                }
            }
            let next = edges(rows);
            let change = next.minus(&input, LIMBS, &mut allow).unwrap();
            let pending = state.prepare(&change, LIMBS, &mut allow).unwrap();
            let expected = oracle(&next, 7, quantifier);
            assert_eq!(
                pending.total(),
                &expected.total_weight(LIMBS, &mut allow).unwrap()
            );
            let publication = sink
                .prepare_update(pending.delta(), LIMBS, &mut allow)
                .unwrap();
            let _ = pending.commit();
            publication.commit();
            assert_eq!(sink, expected, "tick={tick}, quantifier={quantifier:?}");
            assert_eq!(state.snapshot(LIMBS, &mut allow).unwrap(), expected);
            check_index(&state, 7);
            input = next;
        }
    }
}

#[test]
fn opposite_directions_parallel_edges_and_self_loops_have_explicit_semantics() {
    let seed = edges([
        ((0, 1), 2),
        ((1, 0), 3),
        ((0, 2), 7),
        ((2, 1), 11),
        ((0, 0), 23),
    ]);
    for quantifier in [TriangleQuantifier::Distinct, TriangleQuantifier::All] {
        let mut state = IncrementalTriangles::new(quantifier);
        let first = state.apply(&seed, LIMBS, &mut allow).unwrap();
        let expected = if quantifier == TriangleQuantifier::Distinct {
            1
        } else {
            385
        };
        assert_eq!(
            first.weight(&(0, 1, 2)),
            Some(&ZWeight::from_i128(expected))
        );
        assert_eq!(state.total(), &ZWeight::from_i128(expected));
        assert_eq!(state.edge_weight(&1, &0), Some(&ZWeight::from_i128(5)));
        // An orientation-only replacement is a zero unordered-pair delta.
        assert!(
            state
                .apply(&edges([((0, 1), -2), ((1, 0), 2)]), LIMBS, &mut allow)
                .unwrap()
                .is_empty()
        );
        let delta = state
            .apply(&edges([((0, 1), -2), ((0, 0), -23)]), LIMBS, &mut allow)
            .unwrap();
        if quantifier == TriangleQuantifier::Distinct {
            assert!(delta.is_empty());
        } else {
            assert_eq!(delta.weight(&(0, 1, 2)), Some(&ZWeight::from_i128(-154)));
        }
        state
            .apply(&edges([((1, 0), -3)]), LIMBS, &mut allow)
            .unwrap();
        assert!(state.total().is_zero());
        assert!(state.snapshot(LIMBS, &mut allow).unwrap().is_empty());
        assert!(matches!(
            state.prepare(&edges([((0, 0), -1)]), LIMBS, &mut allow),
            Err(TriangleError::NegativeMultiplicity)
        ));
        check_index(&state, 3);
    }
}

#[test]
fn every_control_refusal_and_downstream_drop_preserve_all_arrangements() {
    for quantifier in [TriangleQuantifier::Distinct, TriangleQuantifier::All] {
        let seed = edges([((0, 1), 2), ((1, 2), 1), ((0, 2), 1), ((2, 3), 1)]);
        let change = edges([((0, 1), -1), ((0, 2), -1), ((0, 3), 1), ((1, 3), 2)]);
        let mut state = IncrementalTriangles::new(quantifier);
        state.apply(&seed, LIMBS, &mut allow).unwrap();
        let mut reference = IncrementalTriangles::new(quantifier);
        reference.apply(&seed, LIMBS, &mut allow).unwrap();
        let mut events = 0;
        drop(
            state
                .prepare(&change, LIMBS, &mut |_| {
                    events += 1;
                    Ok::<_, usize>(())
                })
                .unwrap(),
        );
        assert_eq!(state, reference);
        for stop in 1..=events {
            let mut seen = 0;
            match state.prepare(&change, LIMBS, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            }) {
                Err(error) => assert_eq!(error, TriangleError::Delta(ZSetError::Control(stop))),
                Ok(update) => {
                    drop(update);
                    panic!("missed refusal {stop}");
                }
            }
            assert_eq!(seen, stop);
            assert_eq!(state, reference);
        }
        let mut sink = state.snapshot(LIMBS, &mut allow).unwrap();
        {
            let pending = state.prepare(&change, LIMBS, &mut allow).unwrap();
            assert!(
                sink.prepare_update(pending.delta(), LIMBS, &mut |_| Err(9))
                    .is_err()
            );
        }
        assert_eq!(state, reference);
        assert_eq!(sink, reference.snapshot(LIMBS, &mut allow).unwrap());
        let pending = state.prepare(&change, LIMBS, &mut allow).unwrap();
        let publication = sink
            .prepare_update(pending.delta(), LIMBS, &mut allow)
            .unwrap();
        let _ = pending.commit();
        publication.commit();
        assert_eq!(sink, state.snapshot(LIMBS, &mut allow).unwrap());
    }
}

#[test]
fn promotion_and_late_negative_counts_refuse_without_partial_publication() {
    let seed = edges([((0, 1), i128::MAX), ((1, 2), i128::MAX)]);
    let change = edges([((0, 2), 3)]);
    let mut state = IncrementalTriangles::new(TriangleQuantifier::All);
    state.apply(&seed, LIMBS, &mut allow).unwrap();
    let mut reference = IncrementalTriangles::new(TriangleQuantifier::All);
    reference.apply(&seed, LIMBS, &mut allow).unwrap();
    assert!(matches!(
        state.apply(&change, LimbLimit::new(0), &mut allow),
        Err(TriangleError::Delta(ZSetError::Arithmetic(_)))
    ));
    assert_eq!(state, reference);
    assert!(matches!(
        state.apply(&edges([((0, 2), 1), ((100, 101), -1)]), LIMBS, &mut allow),
        Err(TriangleError::NegativeMultiplicity)
    ));
    assert_eq!(state, reference);
    let expected = ZWeight::from_i128(i128::MAX)
        .checked_mul(&ZWeight::from_i128(i128::MAX), LIMBS)
        .unwrap()
        .checked_mul(&ZWeight::from_i128(3), LIMBS)
        .unwrap();
    let delta = state.apply(&change, LIMBS, &mut allow).unwrap();
    assert!(state.total().is_promoted());
    assert_eq!(state.total(), &expected);
    assert_eq!(delta.weight(&(0, 1, 2)), Some(&expected));
    assert_eq!(state.snapshot(LIMBS, &mut allow).unwrap(), delta);
    state
        .apply(
            &change.negated(LIMBS, &mut allow).unwrap(),
            LIMBS,
            &mut allow,
        )
        .unwrap();
    assert_eq!(state, reference);
}

#[test]
fn local_changes_do_not_scan_ten_thousand_unrelated_components() {
    for quantifier in [TriangleQuantifier::Distinct, TriangleQuantifier::All] {
        let seed = edges(
            [((0, 1), 1), ((1, 2), 1)]
                .into_iter()
                .chain((0..10_000).map(|n| ((100 + 2 * n, 101 + 2 * n), 1))),
        );
        let mut state = IncrementalTriangles::new(quantifier);
        state.apply(&seed, LIMBS, &mut allow).unwrap();
        for sign in [1, -1] {
            let mut events = 0;
            let delta = state
                .apply(&edges([((0, 2), sign)]), LIMBS, &mut |_| {
                    events += 1;
                    if events > 200 { Err(events) } else { Ok(()) }
                })
                .unwrap();
            assert_eq!(delta.weight(&(0, 1, 2)), Some(&ZWeight::from_i128(sign)));
        }
        assert!(state.total().is_zero());
        assert_eq!(state.edge_weight(&20_098, &20_099), Some(&ZWeight::ONE));
    }
}

#[test]
fn empty_updates_and_debug_output_do_not_invent_rows_or_leak_keys() {
    let mut state = IncrementalTriangles::new(TriangleQuantifier::All);
    assert!(
        state
            .apply(&ZSet::<(String, String)>::new(), LIMBS, &mut allow)
            .unwrap()
            .is_empty()
    );
    let seed = ZSet::from_updates(
        [(
            ("private-a".to_owned(), "private-b".to_owned()),
            ZWeight::from_i128(919191),
        )],
        LIMBS,
        &mut allow,
    )
    .unwrap();
    state.apply(&seed, LIMBS, &mut allow).unwrap();
    let pending = state.prepare(&ZSet::new(), LIMBS, &mut allow).unwrap();
    let debug = format!("{pending:?}");
    assert!(!debug.contains("private-"));
    assert!(!debug.contains("919191"));
    drop(pending);
    let debug = format!("{state:?}");
    assert!(!debug.contains("private-"));
    assert!(!debug.contains("919191"));
}
