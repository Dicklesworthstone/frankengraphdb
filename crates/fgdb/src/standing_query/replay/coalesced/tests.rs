use super::*;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphAggregateValue, GraphExactAverage};
use fgdb_types::CanonicalScalar;

fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, rows, 10_000_000, 10_000_000)
}
fn limit(ticks: usize) -> NonZeroUsize {
    NonZeroUsize::new(ticks).unwrap()
}
fn key(value: i64) -> Vec<QueryValue> {
    vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
        value,
    )))]
}
fn bag(updates: impl IntoIterator<Item = (Vec<QueryValue>, ZWeight)>) -> ZSet<Vec<QueryValue>> {
    ZSet::from_updates(updates, LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn small(counts: [i128; 2]) -> ZSet<Vec<QueryValue>> {
    bag(counts
        .into_iter()
        .enumerate()
        .map(|(i, count)| (key(i as i64), ZWeight::from_i128(count))))
}
fn append(history: &mut History, rows: ZSet<Vec<QueryValue>>) {
    let at = history.frontier().checked_successor().unwrap();
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(u64::MAX),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    history
        .prepare(
            at,
            rows,
            Limits::new(4096, 100_000, 1_000_000).unwrap(),
            &mut meter,
        )
        .unwrap()
        .commit();
}
fn run(
    history: &History,
    after: CommitSeq,
    ticks: usize,
    budget: GqlQueryPolicy,
) -> (Result<Coalesced, StandingQueryError>, StandingQueryStats) {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: budget,
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let result = history.coalesce(after, limit(ticks), &mut meter);
    (result, meter.stats)
}
fn assert_unchanged(
    history: &History,
    window: StandingReplayWindow,
    frames: &[Arc<StandingReplayBatch>],
) {
    assert_eq!(history.window(), window);
    assert_eq!(history.frames.len(), frames.len());
    for (actual, original) in history.frames.iter().zip(frames) {
        assert!(Arc::ptr_eq(actual, original));
    }
}

#[test]
fn every_three_snapshot_bag_history_obeys_telescoping_and_bounded_prefix_laws() {
    // 729 histories; the oracle is arithmetic on three independent snapshots,
    // not the replay fold or another production Z-set difference.
    for mut code in 0..729_u32 {
        let mut counts = [[0_i128; 2]; 3];
        for snapshot in &mut counts {
            for count in snapshot {
                *count = i128::from(code % 3);
                code /= 3;
            }
        }
        let mut history = History::new(CommitSeq(10));
        for pair in counts.windows(2) {
            append(
                &mut history,
                small([pair[1][0] - pair[0][0], pair[1][1] - pair[0][1]]),
            );
        }
        let frames = history.frames.iter().cloned().collect::<Vec<_>>();
        let window = history.window();
        for ticks in [1, 2, 17, usize::MAX] {
            let (at, rows) = run(&history, CommitSeq(10), ticks, policy(2))
                .0
                .unwrap()
                .unwrap();
            let end = ticks.min(2);
            assert_eq!(at, CommitSeq(10 + end as u64));
            assert_eq!(
                rows,
                small([counts[end][0] - counts[0][0], counts[end][1] - counts[0][1]])
            );
            assert_unchanged(&history, window, &frames);
        }
        let (at, rows) = run(&history, CommitSeq(11), 2, policy(2))
            .0
            .unwrap()
            .unwrap();
        assert_eq!(at, CommitSeq(12));
        assert_eq!(
            rows,
            small([counts[2][0] - counts[1][0], counts[2][1] - counts[1][1]])
        );
        assert!(
            run(&history, at, usize::MAX, policy(0))
                .0
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn cancelled_changes_and_empty_ticks_advance_under_zero_final_tuple_quota() {
    let mut history = History::new(CommitSeq(0));
    append(&mut history, small([7, 0]));
    append(&mut history, ZSet::new());
    append(&mut history, small([-7, 0]));
    let (at, rows) = run(&history, CommitSeq(0), 3, policy(0))
        .0
        .unwrap()
        .unwrap();
    assert_eq!(at, CommitSeq(3));
    assert!(rows.is_empty());
    assert!(matches!(
        run(&history, CommitSeq(0), 2, policy(0)).0,
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::ResultBudget
        ))
    ));
    let (at, rows) = run(&history, CommitSeq(1), 1, policy(0))
        .0
        .unwrap()
        .unwrap();
    assert_eq!(at, CommitSeq(2));
    assert!(rows.is_empty());
    assert!(
        run(&history, CommitSeq(3), 1, policy(0))
            .0
            .unwrap()
            .is_none()
    );
}

#[test]
fn gaps_and_invalid_internal_successors_never_become_an_available_suffix() {
    let mut history = History::new(CommitSeq(u64::MAX - 2));
    append(&mut history, small([1, 0]));
    append(&mut history, small([-1, 1]));
    let (at, rows) = run(&history, CommitSeq(u64::MAX - 2), usize::MAX, policy(2))
        .0
        .unwrap()
        .unwrap();
    assert_eq!(at, CommitSeq(u64::MAX));
    assert_eq!(rows, small([0, 1]));
    assert!(run(&history, at, 1, policy(0)).0.unwrap().is_none());
    assert!(matches!(
        run(&history, CommitSeq(0), 1, policy(2)).0,
        Err(StandingQueryError::ReplayGap { .. })
    ));
    let mut ordinary = History::new(CommitSeq(5));
    append(&mut ordinary, ZSet::new());
    append(&mut ordinary, ZSet::new());
    assert!(matches!(
        run(&ordinary, CommitSeq(8), 1, policy(0)).0,
        Err(StandingQueryError::ReplayGap { .. })
    ));
    // Removal simulates a retired prefix; no baseline can be backfilled.
    let retained = ordinary.frames.pop_front().unwrap();
    ordinary.rows -= retained.rows.len();
    ordinary.units -= retained.units;
    assert!(matches!(
        run(&ordinary, CommitSeq(5), 2, policy(0)).0,
        Err(StandingQueryError::ReplayGap {
            retained_after: CommitSeq(6),
            ..
        })
    ));
    assert_eq!(
        run(&ordinary, CommitSeq(6), 2, policy(0))
            .0
            .unwrap()
            .unwrap()
            .0,
        CommitSeq(7)
    );
    // An internal discontinuity is not merely empty data to skip.
    Arc::get_mut(ordinary.frames.front_mut().unwrap())
        .unwrap()
        .from = CommitSeq(5);
    assert!(matches!(
        run(&ordinary, CommitSeq(5), 2, policy(0)).0,
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::InvalidDelta
        ))
    ));
}

#[test]
fn every_work_scratch_and_completion_refusal_preserves_replay_for_retry() {
    let mut history = History::new(CommitSeq(2));
    append(&mut history, small([4, -2]));
    append(&mut history, small([-3, 0]));
    append(&mut history, small([0, 5]));
    let frames = history.frames.iter().cloned().collect::<Vec<_>>();
    let window = history.window();
    let mut calls = 0;
    let (expected, stats) = {
        let mut checkpoint = || {
            calls += 1;
            Ok(())
        };
        let mut meter = Meter {
            policy: policy(2),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let rows = history
            .coalesce(CommitSeq(2), limit(3), &mut meter)
            .unwrap()
            .unwrap();
        (rows, meter.stats)
    };
    assert!(calls > 0 && stats.work_units > 0 && stats.scratch_entries > 0);
    for stop in 1..=calls {
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
            assert!(matches!(
                history.coalesce(CommitSeq(2), limit(3), &mut meter),
                Err(StandingQueryError::Delivery(
                    StandingQueryFailure::Interrupted
                ))
            ));
        }
        assert_eq!(seen, stop);
        assert_unchanged(&history, window, &frames);
        assert_eq!(
            run(&history, CommitSeq(2), 3, policy(2))
                .0
                .unwrap()
                .unwrap(),
            expected
        );
    }
    for (work, scratch, rows, passes) in [
        (stats.work_units, stats.scratch_entries, 2, true),
        (stats.work_units - 1, stats.scratch_entries, 2, false),
        (stats.work_units, stats.scratch_entries - 1, 2, false),
        (stats.work_units, stats.scratch_entries, 1, false),
    ] {
        let result = run(
            &history,
            CommitSeq(2),
            3,
            GqlQueryPolicy::new(0, rows, work, scratch),
        )
        .0;
        assert_eq!(result.is_ok(), passes);
        assert_unchanged(&history, window, &frames);
    }
    let mut calls = 0;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut checkpoint = || {
                calls += 1;
                assert_ne!(calls, 4, "injected unwind");
                Ok(())
            };
            let mut meter = Meter {
                policy: policy(2),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let _ = history.coalesce(CommitSeq(2), limit(3), &mut meter);
        }))
        .is_err()
    );
    assert_unchanged(&history, window, &frames);
}

#[test]
fn native_values_and_promoted_signed_weights_are_not_narrowed_or_expanded() {
    let huge = ZWeight::from_i128(i128::MAX)
        .checked_mul(&ZWeight::from_i128(2), LIMBS)
        .unwrap();
    let tuple = vec![
        QueryValue::Value(GraphValue::Vertex(VId(u128::MAX))),
        GraphAggregateValue::Count(u64::MAX),
        GraphAggregateValue::Integer(i128::MIN),
        GraphAggregateValue::Average(GraphExactAverage::new(-7, 3).unwrap()),
    ];
    let mut history = History::new(CommitSeq(0));
    append(
        &mut history,
        bag([(tuple.clone(), huge.checked_clone(LIMBS).unwrap())]),
    );
    append(
        &mut history,
        bag([(key(8), huge.checked_neg(LIMBS).unwrap())]),
    );
    let (_, rows) = run(&history, CommitSeq(0), 2, policy(2))
        .0
        .unwrap()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.weight(&tuple), Some(&huge));
    assert_eq!(
        rows.weight(&key(8)),
        Some(&huge.checked_neg(LIMBS).unwrap())
    );
    assert!(rows.iter().all(|(_, weight)| weight.to_i128().is_none()));
    // Checked arithmetic is enforced even if a later tick would cancel it.
    let mut over = huge;
    while let Ok(next) = over.checked_add(&over, LIMBS) {
        over = next;
    }
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(2),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let total = add_weight(&ZWeight::ZERO, &over, &mut meter).unwrap();
    assert!(matches!(
        add_weight(&total, &over, &mut meter),
        Err(StandingQueryFailure::Arithmetic)
    ));
}

#[test]
fn sorted_merge_has_one_head_per_tick_not_one_entry_per_intermediate_key() {
    for width in [2_i64, 16_384] {
        let left = bag((0..=width).map(|n| (key(n), ZWeight::ONE)));
        let right = bag((0..width).map(|n| (key(n), ZWeight::from_i128(-1))));
        let empty = ZSet::new();
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(1),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let mut merged =
            SortedMerge::new([left.iter(), empty.iter(), right.iter()], &mut meter).unwrap();
        assert_eq!(merged.sources.len(), 2);
        assert_eq!(merged.heads.len(), 2);
        let capacity = (merged.sources.capacity(), merged.heads.capacity());
        let (row, weight) = merged.next(&mut meter).unwrap().unwrap();
        assert_eq!(row, &key(width));
        assert_eq!(weight, ZWeight::ONE);
        assert_eq!(merged.sources.len(), 2);
        assert!(merged.heads.len() <= 2);
        assert_eq!(
            (merged.sources.capacity(), merged.heads.capacity()),
            capacity
        );
        assert!(merged.next(&mut meter).unwrap().is_none());
        assert_eq!(
            (merged.sources.capacity(), merged.heads.capacity()),
            capacity
        );
        // All inputs are borrowed, including the row selected for delivery.
        assert!(std::ptr::eq(row, left.iter().next_back().unwrap().0));
        assert_eq!(left.len(), width as usize + 1);
        assert_eq!(right.len(), width as usize);
    }
}

#[test]
fn all_signed_three_tick_merges_equal_independent_column_sums() {
    for mut code in 0..15_625_u32 {
        let mut weights = [[0_i128; 2]; 3];
        for tick in &mut weights {
            for weight in tick {
                *weight = i128::from(code % 5) - 2;
                code /= 5;
            }
        }
        let sources: Vec<_> = weights.iter().copied().map(small).collect();
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(2),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let mut merged = SortedMerge::new(sources.iter().map(ZSet::iter), &mut meter).unwrap();
        let expected: Vec<_> = (0..2)
            .filter_map(|column| {
                let sum: i128 = weights.iter().map(|tick| tick[column]).sum();
                (sum != 0).then(|| (key(column as i64), sum))
            })
            .collect();
        let mut actual = Vec::new();
        while let Some((row, weight)) = merged.next(&mut meter).unwrap() {
            actual.push((row.clone(), weight.to_i128().unwrap()));
            assert!(merged.heads.len() <= 3 && merged.sources.len() <= 3);
        }
        assert_eq!(actual, expected);
    }
}

#[test]
fn equal_keys_fold_oldest_first_and_never_hide_oversized_input_weights() {
    let mut big = ZWeight::from_i128(i128::MAX);
    while let Ok(next) = big.checked_add(&big, LIMBS) {
        big = next;
    }
    for cancel_middle in [true, false] {
        let mut history = History::new(CommitSeq(0));
        append(
            &mut history,
            bag([(key(0), big.checked_clone(LIMBS).unwrap())]),
        );
        for negative in [cancel_middle, !cancel_middle] {
            let weight = if negative {
                big.checked_neg(LIMBS).unwrap()
            } else {
                big.checked_clone(LIMBS).unwrap()
            };
            append(&mut history, bag([(key(0), weight)]));
        }
        let result = run(&history, CommitSeq(0), 3, policy(1)).0;
        if cancel_middle {
            assert_eq!(result.unwrap().unwrap().1.weight(&key(0)), Some(&big));
        } else {
            // Same mathematical final sum, but the chronological prefix
            // exceeds admission. Reordering additions would hide this failure.
            assert!(matches!(
                result,
                Err(StandingQueryError::Delivery(
                    StandingQueryFailure::Arithmetic
                ))
            ));
        }
    }
    let wider = LimbLimit::new(8);
    let oversized = big.checked_add(&big, wider).unwrap();
    let first = ZSet::from_updates(
        [(key(0), oversized.checked_clone(wider).unwrap())],
        wider,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let second = ZSet::from_updates(
        [(key(0), oversized.checked_neg(wider).unwrap())],
        wider,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(0),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let mut merged = SortedMerge::new([first.iter(), second.iter()], &mut meter).unwrap();
    assert!(matches!(
        merged.next(&mut meter),
        Err(StandingQueryFailure::Arithmetic)
    ));
    let stats = meter.stats;
    assert!(merged.next(&mut meter).unwrap().is_none());
    assert_eq!(meter.stats, stats);
}

#[test]
fn merge_keeps_native_representation_identity_and_refuses_invalid_source_order() {
    let representations = [
        vec![QueryValue::Count(7)],
        vec![QueryValue::Integer(7)],
        key(7),
        vec![QueryValue::Average(GraphExactAverage::new(7, 1).unwrap())],
    ];
    let mut history = History::new(CommitSeq(0));
    append(
        &mut history,
        bag(representations
            .iter()
            .cloned()
            .map(|row| (row, ZWeight::ONE))),
    );
    append(&mut history, bag([(key(7), ZWeight::from_i128(-1))]));
    let (_, rows) = run(&history, CommitSeq(0), 2, policy(3))
        .0
        .unwrap()
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.weight(&key(7)).is_none());
    for index in [0, 1, 3] {
        assert_eq!(rows.weight(&representations[index]), Some(&ZWeight::ONE));
    }
    // Production supplies sealed sorted Z-sets. This negative control guards
    // the internal merge's monotonic source assumption against future adapters.
    for sequence in [vec![key(1), key(0)], vec![key(0), key(0)]] {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(2),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let one = ZWeight::ONE;
        let mut merged =
            SortedMerge::new([sequence.iter().map(|row| (row, &one))], &mut meter).unwrap();
        assert!(matches!(
            merged.next(&mut meter),
            Err(StandingQueryFailure::InvalidDelta)
        ));
        assert!(merged.next(&mut meter).unwrap().is_none());
    }
}

#[test]
fn cancelled_payloads_are_not_copied_and_unselected_history_is_not_scanned() {
    let cancelled = |bytes| {
        let tuple = vec![QueryValue::Value(GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(&"x".repeat(bytes)).unwrap(),
        ))];
        let mut history = History::new(CommitSeq(0));
        append(&mut history, bag([(tuple.clone(), ZWeight::ONE)]));
        append(&mut history, bag([(tuple, ZWeight::from_i128(-1))]));
        let (result, stats) = run(&history, CommitSeq(0), 2, policy(0));
        assert!(result.unwrap().unwrap().1.is_empty());
        stats.scratch_entries
    };
    assert_eq!(cancelled(1), cancelled(65_536));
    let mut usages = Vec::new();
    for ticks in [2_u64, 128] {
        let mut history = History::new(CommitSeq(0));
        for _ in 0..ticks {
            append(&mut history, small([1, 0]));
        }
        let (result, stats) = run(&history, CommitSeq(ticks - 1), 1, policy(1));
        assert_eq!(result.unwrap().unwrap().1, small([1, 0]));
        usages.push(stats);
    }
    assert_eq!(usages[0], usages[1]);
}
