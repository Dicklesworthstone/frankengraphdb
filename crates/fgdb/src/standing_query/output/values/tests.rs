use super::*;
use crate::standing_query::StandingQueryStats;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValueOrder};
use fgdb_gql::{GqlQueryPolicy, GraphAggregateValue};
use fgdb_types::CanonicalScalar;

fn definition(
    distinct: bool,
    offset: u64,
    count: Option<u64>,
    reverse: bool,
) -> PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let pattern = builder
        .prepare_values(
            &[GraphColumn::property("value", "n", PropertyKeyId(1))],
            offset,
            count,
        )
        .unwrap();
    let pattern = if distinct {
        pattern
    } else {
        pattern.with_duplicates()
    };
    if reverse {
        pattern
            .with_order_by(&[GraphValueOrder::descending(0)])
            .unwrap()
    } else {
        pattern
    }
}
fn delta(rows: &[(i64, u64, i128)]) -> ZSet<GraphAggregateRow> {
    let producer = definition(false, 0, None, false)
        .incremental_row_source_definition()
        .unwrap();
    ZSet::from_updates(
        rows.iter().map(|&(key, count, sign)| {
            (
                producer
                    .materialize_incremental_row(
                        vec![GraphValue::Scalar(CanonicalScalar::Int(key))],
                        vec![GraphAggregateValue::Count(count)],
                    )
                    .unwrap(),
                ZWeight::from_i128(sign),
            )
        }),
        LimbLimit::new(4),
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, rows, 10_000_000, 10_000_000)
}
fn apply(state: &mut State, changes: &[(i64, u64, i128)], rows: u64) -> StandingQueryStats {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(rows),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    state.prepare(&delta(changes), &mut meter).unwrap().commit();
    meter.stats
}
fn sequence(state: &State) -> Vec<i64> {
    state
        .ordered
        .iter()
        .map(|row| {
            let Some(CanonicalScalar::Int(value)) = row.get(0).and_then(GraphValue::as_scalar)
            else {
                panic!("integer fixture");
            };
            *value
        })
        .collect()
}
fn same(left: &State, right: &State) {
    assert_eq!(left.rows, right.rows);
    assert_eq!(left.ordered, right.ordered);
    let export = |state: &State| {
        state
            .candidates
            .iter()
            .map(|(rank, tuple)| {
                (
                    rank.counted.as_ref().clone(),
                    tuple.row.as_ref().clone(),
                    tuple.count,
                )
            })
            .collect::<Vec<_>>()
    };
    // Rank equality intentionally ignores count; compare full before-images.
    assert_eq!(export(left), export(right));
}

#[test]
fn tuple_counts_drive_all_distinct_windows_and_do_not_expand_large_offsets() {
    let initial = [(1, 4, 1), (2, 3, 1), (3, 2, 1)];
    let changes = [(1, 4, -1), (1, 2, 1), (2, 3, -1), (4, 5, 1)];
    for distinct in [false, true] {
        for reverse in [false, true] {
            for offset in [0, 1, 3, u64::MAX] {
                for count in [None, Some(0), Some(1), Some(4)] {
                    let mut state =
                        State::new(definition(distinct, offset, count, reverse)).unwrap();
                    apply(&mut state, &initial, 20);
                    apply(&mut state, &changes, 20);
                    let mut expected = Vec::new();
                    for (key, n) in [(1, 2), (3, 2), (4, 5)] {
                        let n = if distinct { 1 } else { n };
                        for _ in 0..n {
                            expected.push(key);
                        }
                    }
                    if reverse {
                        expected.reverse();
                    }
                    let expected: Vec<_> = expected
                        .into_iter()
                        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                        .take(
                            count
                                .and_then(|n| usize::try_from(n).ok())
                                .unwrap_or(usize::MAX),
                        )
                        .collect();
                    assert_eq!(sequence(&state), expected);
                    assert_eq!(
                        state
                            .rows
                            .total_weight(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                            .unwrap()
                            .to_i128(),
                        Some(expected.len() as i128)
                    );
                }
            }
        }
    }
    let mut wide = State::new(definition(false, u64::MAX - 2, Some(4), false)).unwrap();
    let stats = apply(&mut wide, &[(1, u64::MAX, 1), (2, 5, 1)], 4);
    assert_eq!(sequence(&wide), [1, 1, 2, 2]);
    assert!(stats.work_units < 10_000);
    apply(&mut wide, &[(1, u64::MAX, -1), (1, u64::MAX - 1, 1)], 4);
    assert_eq!(sequence(&wide), [1, 2, 2, 2]);
    // A second change validates that equal-rank replacement stored the new
    // full count-bearing key rather than BTreeMap retaining the old key.
    apply(&mut wide, &[(1, u64::MAX - 1, -1), (1, u64::MAX - 3, 1)], 4);
    assert_eq!(sequence(&wide), [2, 2, 2, 2]);
}

#[test]
fn every_row_sink_checkpoint_quota_and_dropped_guard_is_atomic_and_retryable() {
    for distinct in [false, true] {
        let seed = || {
            let mut state = State::new(definition(distinct, 1, Some(4), false)).unwrap();
            apply(&mut state, &[(1, 7, 1), (2, 2, 1), (3, 5, 1)], 4);
            state
        };
        let changes = delta(&[(1, 7, -1), (1, 2, 1), (2, 2, -1), (4, 3, 1)]);
        let before = seed();
        let mut success = seed();
        let mut calls = 0;
        let stats = {
            let mut checkpoint = || {
                calls += 1;
                Ok(())
            };
            let mut meter = Meter {
                policy: policy(4),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            success.prepare(&changes, &mut meter).unwrap().commit();
            meter.stats
        };
        for stop in 1..=calls {
            let mut candidate = seed();
            let mut seen = 0;
            {
                let mut checkpoint = || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                };
                let mut meter = Meter {
                    policy: policy(4),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                assert!(matches!(
                    candidate.prepare(&changes, &mut meter),
                    Err(StandingQueryFailure::Interrupted)
                ));
            }
            assert_eq!(seen, stop);
            same(&candidate, &before);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(4),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            drop(candidate.prepare(&changes, &mut meter).unwrap());
            same(&candidate, &before);
            candidate.prepare(&changes, &mut meter).unwrap().commit();
            same(&candidate, &success);
        }
        for reason in [
            StandingQueryFailure::WorkBudget,
            StandingQueryFailure::ScratchBudget,
            StandingQueryFailure::ResultBudget,
        ] {
            let mut candidate = seed();
            let mut bounded = policy(4);
            match reason {
                StandingQueryFailure::WorkBudget => {
                    bounded.evaluator.max_work_units = stats.work_units - 1
                }
                StandingQueryFailure::ScratchBudget => {
                    bounded.evaluator.max_scratch_entries = stats.scratch_entries - 1
                }
                _ => bounded = policy(0),
            }
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: bounded,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert!(
                matches!(candidate.prepare(&changes, &mut meter), Err(actual) if actual == reason)
            );
            same(&candidate, &before);
            let mut meter = Meter {
                policy: policy(4),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            candidate.prepare(&changes, &mut meter).unwrap().commit();
            same(&candidate, &success);
        }
    }
}

#[test]
fn malformed_counts_and_stale_before_images_fail_even_outside_an_empty_page() {
    let seed = || {
        let mut state = State::new(definition(false, u64::MAX, Some(0), false)).unwrap();
        apply(&mut state, &[(1, 7, 1)], 0);
        state
    };
    let before = seed();
    for bad in [
        vec![(1, 3, -1)],
        vec![(1, 2, 1)],
        vec![(2, 1, -1)],
        vec![(1, 7, -2)],
        vec![(1, 0, -1)],
        vec![(1, 7, -1), (1, 2, 1), (1, 3, 1)],
    ] {
        let mut candidate = seed();
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(0),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        assert!(matches!(
            candidate.prepare(&delta(&bad), &mut meter),
            Err(StandingQueryFailure::InvalidDelta)
        ));
        same(&candidate, &before);
    }
    let mut candidate = seed();
    apply(&mut candidate, &[(1, 7, -1), (1, 2, 1)], 0);
    apply(&mut candidate, &[(1, 2, -1)], 0);
    assert!(
        candidate.candidates.is_empty()
            && candidate.rows.is_empty()
            && candidate.ordered.is_empty()
    );
}

#[test]
fn source_to_row_sink_cancellation_preserves_publication_and_retries() {
    use crate::{Database, DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::RelationId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts, VId};
    let ((), report) = run_async_under_lab(0x6ac1, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let keys = || DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
        let seed = || {
            let mut batch = WriteBatch::new(RelationId(1));
            for id in 1..=4 {
                batch.create_vertex(
                    VId(id),
                    vec![],
                    vec![(PropertyKeyId(1), CanonicalScalar::Int((id % 2) as i64))],
                );
            }
            batch
        };
        let mut basis = Database::open_memory(&commit, keys()).await.unwrap();
        basis.write(&commit, seed()).await.unwrap();
        let mut driver = Database::open_memory(&commit, keys()).await.unwrap();
        driver.write(&commit, seed()).await.unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(1));
        change.set_vertex_property(VId(2), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
        let at = driver.write(&commit, change).await.unwrap();
        let delta = driver.delta_index().unwrap().get(at).unwrap().clone();
        let make = || {
            let definition = definition(false, 1, Some(2), false);
            let producer = definition.incremental_row_source_definition().unwrap();
            let mut output = State::new(definition).unwrap();
            let source = basis
                .prepare_standing_query_with_output(&query, producer, policy(2), Some(&mut output))
                .unwrap();
            (source, output)
        };
        let (before_source, before) = make();
        let (mut success_source, mut success) = make();
        let mut calls = 0;
        {
            let mut checkpoint = || {
                calls += 1;
                Ok(())
            };
            let mut meter = Meter {
                policy: policy(2),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            success_source
                .maintain_with_output(&delta, &mut meter, Some(&mut success))
                .unwrap();
        }
        for stop in 1..=calls {
            let (mut source, mut output) = make();
            let mut seen = 0;
            {
                let mut checkpoint = || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                };
                let mut meter = Meter {
                    policy: policy(2),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                assert_eq!(
                    source.maintain_with_output(&delta, &mut meter, Some(&mut output)),
                    Err(StandingQueryFailure::Interrupted)
                );
            }
            assert_eq!(seen, stop);
            same(&output, &before);
            assert_eq!(source.rows, before_source.rows);
            assert_eq!(source.frontier, before_source.frontier);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(2),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            source
                .maintain_with_output(&delta, &mut meter, Some(&mut output))
                .unwrap();
            assert_eq!(source.rows, success_source.rows);
            same(&output, &success);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
