use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn build(snapshot: &crate::Snapshot, native_rows: bool) -> State {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    State::from_snapshot(snapshot, Some(RelationId(1)), native_rows, &mut meter).unwrap()
}
fn unchanged(actual: &State, before: &State) {
    assert_eq!(actual.input, before.input);
    assert_eq!(actual.cores, before.cores);
    assert_eq!(actual.rows, before.rows);
    assert_eq!(actual.relational, before.relational);
    assert_eq!(actual.frontier, before.frontier);
    assert_eq!(actual.stats, before.stats);
    assert_eq!(actual.failure, before.failure);
    assert_eq!(actual.policy, before.policy);
    assert_eq!(actual.relation, before.relation);
    assert_eq!(actual.selected, before.selected);
}

#[test]
fn all_composed_refusals_and_exact_quota_boundaries_leave_accepted_state_intact() {
    for native_rows in [false, true] {
        composed_refusals(native_rows);
    }
}

fn composed_refusals(native_rows: bool) {
    let ((), report) = run_async_under_lab(0x6b63_0110, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let keys = DatabaseKeys::new(
            [0xf1; 32],
            DatabaseSecurityNamespaceId([0xf2; 32]),
            [0xf3; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for v in 0..4 {
            seed.create_vertex(VId(v), vec![], vec![]);
        }
        for (id, a, b) in [
            (1, 0, 1),
            (2, 0, 2),
            (3, 0, 3),
            (4, 1, 2),
            (5, 1, 3),
            (6, 2, 3),
        ] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let baseline = Arc::clone(&db.snapshot);
        let before = build(&baseline, native_rows);
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_edge(EId(1));
        db.write(&commit, change).await.unwrap();
        let batch = db.delta_since(baseline.frontier).unwrap().next().unwrap();
        let expected = build(&db.snapshot, native_rows);
        assert_ne!(before.rows, expected.rows);
        let mut state = build(&baseline, native_rows);
        let mut calls = 0;
        let stats = {
            let mut checkpoint = || {
                calls += 1;
                Ok(())
            };
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            state.maintain(&commit, batch, &mut meter).unwrap();
            meter.stats
        };
        assert_eq!(state.rows, expected.rows);
        assert_eq!(state.value_rows(), expected.value_rows());
        assert_eq!(state.value_delta().is_some(), native_rows);
        assert_eq!(state.cores, expected.cores);
        assert_eq!(state.input, expected.input);
        for stop in 1..=calls {
            let mut state = build(&baseline, native_rows);
            let mut visited = 0;
            {
                let mut checkpoint = || {
                    visited += 1;
                    if visited == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                };
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                assert_eq!(
                    state.maintain(&commit, batch, &mut meter),
                    Err(StandingQueryFailure::Interrupted)
                );
            }
            assert_eq!(visited, stop);
            unchanged(&state, &before);
        }
        for (result, work, scratch, expected_error) in [
            (4, stats.work_units, stats.scratch_entries, None),
            (
                3,
                stats.work_units,
                stats.scratch_entries,
                Some(StandingQueryFailure::ResultBudget),
            ),
            (
                4,
                stats.work_units - 1,
                stats.scratch_entries,
                Some(StandingQueryFailure::WorkBudget),
            ),
            (
                4,
                stats.work_units,
                stats.scratch_entries - 1,
                Some(StandingQueryFailure::ScratchBudget),
            ),
        ] {
            let mut state = build(&baseline, native_rows);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(100_000, result, work, scratch),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let outcome = state.maintain(&commit, batch, &mut meter);
            if let Some(error) = expected_error {
                assert_eq!(outcome, Err(error));
                unchanged(&state, &before);
            } else {
                outcome.unwrap();
                assert_eq!(state.rows, expected.rows);
                assert_eq!(state.value_rows(), expected.value_rows());
                assert_eq!(state.cores, expected.cores);
            }
        }
        // Combined source admission, not just edge records, precedes bootstrap.
        let records: usize = baseline
            .blocks
            .iter()
            .map(|b| b.len())
            .chain(baseline.patches.iter().map(|p| p.len()))
            .sum();
        assert!(records > 0 && !baseline.patches.is_empty());
        for allowance in [records as u64, records as u64 - 1] {
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(allowance, 4, 10_000_000, 10_000_000),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let result =
                State::from_snapshot(&baseline, Some(RelationId(1)), native_rows, &mut meter);
            if allowance == records as u64 {
                assert_eq!(result.unwrap().rows, before.rows);
            } else {
                assert!(matches!(result, Err(StandingQueryFailure::SnapshotBudget)));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn selected_pair(a: Option<u128>, b: Option<u128>) -> GraphValueRow {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::CanonicalScalar;
    let endpoint = |value: Option<u128>| {
        value.map_or(GraphValue::Scalar(CanonicalScalar::Null), |value| {
            GraphValue::Vertex(VId(value))
        })
    };
    GraphValueRow::from_owned_values(vec![endpoint(a), endpoint(b)])
}

fn selected_bag(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.into_iter()
            .map(|(row, weight)| (row, ZWeight::from_i128(weight))),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

fn build_selected(source: &ZSet<GraphValueRow>, native_rows: bool) -> State {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    State::from_selected(
        source,
        components::relation::Definition {
            input: 0,
            endpoints: [0, 1],
            width: 2,
        },
        CommitSeq::ORIGIN,
        native_rows,
        &mut meter,
    )
    .unwrap()
}

fn apply_selected(state: &mut State, delta: &ZSet<GraphValueRow>) {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    state.apply_selected(delta, &mut meter).unwrap();
}

// Enumerate every induced subset and its minimum degree. This independently
// derives each vertex's maximum k rather than invoking the peeling kernel.
fn selected_oracle(source: &ZSet<GraphValueRow>) -> ZSet<(VId, u64)> {
    use fgdb_gql::algebra::GraphValue;
    use std::collections::{BTreeMap, BTreeSet};
    let mut adjacency = BTreeMap::<VId, BTreeSet<VId>>::new();
    for (row, weight) in source.iter() {
        assert!(weight > &ZWeight::ZERO);
        let mut endpoints = [None; 2];
        for (index, value) in row.values().iter().enumerate() {
            match value {
                GraphValue::Vertex(vertex) => {
                    endpoints[index] = Some(*vertex);
                    adjacency.entry(*vertex).or_default();
                }
                value if value.is_null() => {}
                _ => panic!("invalid oracle fixture"),
            }
        }
        if let [Some(a), Some(b)] = endpoints
            && a != b
        {
            adjacency.get_mut(&a).unwrap().insert(b);
            adjacency.get_mut(&b).unwrap().insert(a);
        }
    }
    let vertices: Vec<_> = adjacency.keys().copied().collect();
    let mut cores = vec![0_u64; vertices.len()];
    for mask in 1..(1_u64 << vertices.len()) {
        let selected: BTreeSet<_> = vertices
            .iter()
            .enumerate()
            .filter_map(|(bit, &vertex)| (mask & (1 << bit) != 0).then_some(vertex))
            .collect();
        let degree = selected
            .iter()
            .map(|vertex| adjacency[vertex].intersection(&selected).count() as u64)
            .min()
            .unwrap();
        for (bit, core) in cores.iter_mut().enumerate() {
            if mask & (1 << bit) != 0 {
                *core = (*core).max(degree);
            }
        }
    }
    ZSet::from_updates(
        vertices
            .into_iter()
            .zip(cores)
            .map(|row| (row, ZWeight::ONE)),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

#[test]
fn selected_core_numbers_match_independent_subsets_after_every_edge_flip() {
    let candidates = [
        (0, 1),
        (0, 2),
        (0, u128::MAX),
        (1, 2),
        (1, u128::MAX),
        (2, u128::MAX),
    ];
    for mask in 0..64_u16 {
        let mut source =
            selected_bag([0, 1, 2, u128::MAX].map(|vertex| (selected_pair(Some(vertex), None), 1)));
        let edges = selected_bag(candidates.iter().enumerate().flat_map(|(bit, &(a, b))| {
            let present = i128::from(mask & (1 << bit) != 0);
            [
                (selected_pair(Some(a), Some(b)), 2 * present),
                (selected_pair(Some(b), Some(a)), present),
            ]
        }));
        source
            .integrate(&edges, LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        for (bit, &(a, b)) in candidates.iter().enumerate() {
            let sign = if mask & (1 << bit) == 0 { 1 } else { -1 };
            let delta = selected_bag([
                (selected_pair(Some(a), Some(b)), 2 * sign),
                (selected_pair(Some(b), Some(a)), sign),
            ]);
            let mut after = source
                .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            after
                .integrate(&delta, LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            for native_rows in [false, true] {
                let mut state = build_selected(&source, native_rows);
                assert_eq!(state.rows, selected_oracle(&source));
                let mut delivered = state
                    .value_rows()
                    .map(|rows| rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap());
                apply_selected(&mut state, &delta);
                assert_eq!(state.rows, selected_oracle(&after));
                if let Some(delivered) = &mut delivered {
                    delivered
                        .integrate(
                            state.value_delta().unwrap(),
                            LIMBS,
                            &mut |_| Ok::<_, ()>(()),
                        )
                        .unwrap();
                    assert_eq!(Some(&*delivered), state.value_rows());
                }
            }
        }
    }
}

#[test]
fn selected_core_support_keeps_duplicates_null_isolates_and_self_loops_exact() {
    let mut source = selected_bag([
        (selected_pair(Some(0), Some(1)), 2),
        (selected_pair(Some(1), Some(0)), 1),
        (selected_pair(None, Some(2)), 1),
        (selected_pair(Some(u128::MAX), Some(u128::MAX)), 3),
        (selected_pair(None, None), 7),
    ]);
    let mut state = build_selected(&source, true);
    assert_eq!(state.cores.core_number(&VId(u128::MAX)), Some(0));
    assert_eq!(state.cores.core_number(&VId(2)), Some(0));
    for delta in [
        selected_bag([(selected_pair(Some(0), Some(1)), -2)]),
        selected_bag([(selected_pair(Some(1), Some(0)), -1)]),
        selected_bag([
            (selected_pair(None, Some(2)), -1),
            (selected_pair(Some(u128::MAX), Some(u128::MAX)), -3),
        ]),
    ] {
        source
            .integrate(&delta, LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        apply_selected(&mut state, &delta);
        assert_eq!(state.rows, selected_oracle(&source));
    }
    assert!(state.rows.is_empty());
    assert!(state.value_rows().unwrap().is_empty());
    apply_selected(&mut state, &ZSet::new());
    assert!(state.value_delta().unwrap().is_empty());
}

#[test]
fn selected_core_refusals_and_cancellation_preserve_all_accepted_support_and_sinks() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::CanonicalScalar;
    let source = selected_bag(
        [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)]
            .map(|(a, b)| (selected_pair(Some(a), Some(b)), 1)),
    );
    let delta = selected_bag([(selected_pair(Some(0), Some(1)), -1)]);
    for native_rows in [false, true] {
        let before = build_selected(&source, native_rows);
        let mut success = build_selected(&source, native_rows);
        let mut calls = 0;
        let stats = {
            let mut checkpoint = || {
                calls += 1;
                Ok(())
            };
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            success.apply_selected(&delta, &mut meter).unwrap();
            meter.stats
        };
        assert_ne!(success.rows, before.rows);
        for stop in 1..=calls {
            let mut state = build_selected(&source, native_rows);
            let mut visited = 0;
            let mut checkpoint = || {
                visited += 1;
                if visited == stop {
                    Err(StandingQueryFailure::Interrupted)
                } else {
                    Ok(())
                }
            };
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                state.apply_selected(&delta, &mut meter),
                Err(StandingQueryFailure::Interrupted)
            );
            unchanged(&state, &before);
        }
        for (result, work, scratch, expected) in [
            (4, stats.work_units, stats.scratch_entries, None),
            (
                3,
                stats.work_units,
                stats.scratch_entries,
                Some(StandingQueryFailure::ResultBudget),
            ),
            (
                4,
                stats.work_units - 1,
                stats.scratch_entries,
                Some(StandingQueryFailure::WorkBudget),
            ),
            (
                4,
                stats.work_units,
                stats.scratch_entries - 1,
                Some(StandingQueryFailure::ScratchBudget),
            ),
        ] {
            let mut state = build_selected(&source, native_rows);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(0, result, work, scratch),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                state.apply_selected(&delta, &mut meter),
                expected.map_or(Ok(()), Err)
            );
            if expected.is_some() {
                unchanged(&state, &before);
            } else {
                unchanged(&state, &success);
            }
        }
        for invalid in [
            selected_bag([(selected_pair(Some(0), Some(1)), -2)]),
            selected_bag([(
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Scalar(CanonicalScalar::Null),
                    GraphValue::Scalar(CanonicalScalar::Int(1)),
                ]),
                1,
            )]),
            selected_bag([(GraphValueRow::from_owned_values(vec![]), 1)]),
        ] {
            let mut state = build_selected(&source, native_rows);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                state.apply_selected(&invalid, &mut meter),
                Err(StandingQueryFailure::InvalidDelta)
            );
            unchanged(&state, &before);
        }
    }
}
