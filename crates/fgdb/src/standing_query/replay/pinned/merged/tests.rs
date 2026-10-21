use super::*;
use crate::{DatabaseKeys, QueryResult, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, RelationBind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::collections::BTreeMap;

fn budget(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, rows, 10_000_000, 10_000_000)
}
fn ticks(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn key(n: i128) -> Vec<QueryValue> {
    vec![QueryValue::Integer(n)]
}
fn bag(updates: impl IntoIterator<Item = (Vec<QueryValue>, ZWeight)>) -> ZSet<Vec<QueryValue>> {
    ZSet::from_updates(updates, LimbLimit::new(8), &mut |_| Ok::<_, ()>(())).unwrap()
}
fn append(history: &mut History, rows: ZSet<Vec<QueryValue>>) {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: budget(u64::MAX),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let at = history.frontier().checked_successor().unwrap();
    history
        .prepare(
            at,
            rows,
            Limits::new(32, 100_000, 1_000_000).unwrap(),
            &mut meter,
        )
        .unwrap()
        .commit();
}
fn fixture() -> History {
    let mut history = History::new(CommitSeq(7));
    append(
        &mut history,
        bag([
            (key(1), ZWeight::from_i128(7)),
            (key(2), ZWeight::from_i128(-1)),
        ]),
    );
    append(&mut history, ZSet::new());
    append(
        &mut history,
        bag([
            (key(1), ZWeight::from_i128(-2)),
            (key(2), ZWeight::from_i128(5)),
        ]),
    );
    history
}
fn open(
    history: &History,
    before: CommitSeq,
    after: CommitSeq,
    max_units: usize,
    policy: GqlQueryPolicy,
    checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
) -> Result<NetPull, StandingQueryError> {
    let mut meter = Meter {
        policy,
        stats: StandingQueryStats::default(),
        checkpoint,
    };
    let pins = pin_interval(history, before, after, max_units, &mut meter)?;
    NetPull::from_pins(pins, before > after, &mut meter).map_err(StandingQueryError::Delivery)
}
fn drain(pull: &mut NetPull) -> Result<Vec<(Vec<QueryValue>, i128)>, StandingQueryError> {
    let mut rows = Vec::new();
    while let Some(item) = pull.advance(&mut || Ok(())) {
        let (row, weight) = item?;
        rows.push((row, weight.to_i128().unwrap()));
    }
    Ok(rows)
}
fn unpinned(history: &History) {
    assert!(
        history
            .frames
            .iter()
            .all(|frame| Arc::strong_count(frame) == 1)
    );
}

#[test]
fn all_small_signed_histories_match_independent_arithmetic_for_every_cut_pair() {
    // 3^6 histories, four cuts, both orientations and equal cuts. The oracle
    // adds independent integers; it shares neither production merge nor Z-set subtraction.
    for mut code in 0..729_u32 {
        let mut counts = [[0_i128; 2]; 3];
        let mut history = History::new(CommitSeq(10));
        for tick in &mut counts {
            for count in tick.iter_mut() {
                *count = i128::from(code % 3) - 1;
                code /= 3;
            }
            append(
                &mut history,
                bag(tick
                    .iter()
                    .enumerate()
                    .map(|(n, count)| (key(n as i128), ZWeight::from_i128(*count)))),
            );
        }
        let window = history.window();
        for a in 0_usize..=3 {
            for b in 0_usize..=3 {
                let expected: Vec<_> = (0..2)
                    .filter_map(|n| {
                        let sum: i128 = counts[a.min(b)..a.max(b)].iter().map(|tick| tick[n]).sum();
                        let sum = if a > b { -sum } else { sum };
                        (sum != 0).then(|| (key(n as i128), sum))
                    })
                    .collect();
                let mut pull = open(
                    &history,
                    CommitSeq(10 + a as u64),
                    CommitSeq(10 + b as u64),
                    history.units,
                    budget(expected.len() as u64),
                    &mut || Ok(()),
                )
                .unwrap();
                assert_eq!(drain(&mut pull).unwrap(), expected);
                assert_eq!(pull.delivered, expected.len() as u64);
                assert!(pull.merge.is_none());
                assert!(pull.advance(&mut || panic!("EOF polled control")).is_none());
                assert_eq!(history.window(), window);
                unpinned(&history);
            }
        }
    }
}

#[test]
fn complete_prefixes_empty_ticks_and_zero_net_quota_keep_the_selected_frontier() {
    let mut history = History::new(CommitSeq(2));
    append(&mut history, bag([(key(1), ZWeight::from_i128(9))]));
    append(&mut history, ZSet::new());
    append(&mut history, bag([(key(1), ZWeight::from_i128(-9))]));
    for n in [1, 2, 3, 17, usize::MAX] {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: budget(1),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let (end, pins) =
            PinnedReplay::open(&history, CommitSeq(2), ticks(n), history.units, &mut meter)
                .unwrap()
                .unwrap();
        let mut pull = NetPull::from_pins(pins, false, &mut meter).unwrap();
        assert_eq!(end, CommitSeq(2 + n.min(3) as u64));
        let expected = if n < 3 { vec![(key(1), 9)] } else { Vec::new() };
        assert_eq!(drain(&mut pull).unwrap(), expected);
        unpinned(&history);
    }
    let mut cancelled = open(
        &history,
        CommitSeq(2),
        CommitSeq(5),
        history.units,
        budget(0),
        &mut || Ok(()),
    )
    .unwrap();
    assert!(drain(&mut cancelled).unwrap().is_empty());
    let mut empty = open(
        &history,
        CommitSeq(3),
        CommitSeq(4),
        1,
        budget(0),
        &mut || Ok(()),
    )
    .unwrap();
    assert!(drain(&mut empty).unwrap().is_empty());
    let mut equal = open(
        &history,
        CommitSeq(5),
        CommitSeq(5),
        0,
        budget(0),
        &mut || Ok(()),
    )
    .unwrap();
    assert!(matches!(
        equal.advance(&mut || Err(StandingQueryFailure::Interrupted)),
        Some(Err(StandingQueryError::Delivery(
            StandingQueryFailure::Interrupted
        )))
    ));
    assert!(equal.merge.is_none());
}

#[test]
fn retained_coverage_and_pin_refusal_never_substitute_a_shorter_interval() {
    let mut history = fixture();
    let window = history.window();
    for (a, b) in [(6, 7), (7, 11), (11, 11), (6, 6), (11, 6)] {
        assert!(matches!(
            open(
                &history,
                CommitSeq(a),
                CommitSeq(b),
                usize::MAX,
                budget(100),
                &mut || Ok(())
            ),
            Err(StandingQueryError::ReplayGap { .. })
        ));
        unpinned(&history);
    }
    assert!(matches!(
        open(
            &history,
            CommitSeq(7),
            CommitSeq(10),
            history.units - 1,
            budget(100),
            &mut || Ok(())
        ),
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::ScratchBudget
        ))
    ));
    assert_eq!(history.window(), window);
    unpinned(&history);
    Arc::get_mut(&mut history.frames[1]).unwrap().from = CommitSeq(7);
    assert!(matches!(
        open(
            &history,
            CommitSeq(7),
            CommitSeq(10),
            usize::MAX,
            budget(100),
            &mut || Ok(())
        ),
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::InvalidDelta
        ))
    ));
    unpinned(&history);
    let mut terminal = History::new(CommitSeq(u64::MAX - 1));
    append(&mut terminal, bag([(key(i128::MAX), ZWeight::ONE)]));
    let mut reverse = open(
        &terminal,
        CommitSeq(u64::MAX),
        CommitSeq(u64::MAX - 1),
        terminal.units,
        budget(1),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(drain(&mut reverse).unwrap(), vec![(key(i128::MAX), -1)]);
}

#[test]
fn every_open_pull_and_eof_refusal_or_unwind_releases_pins_and_fuses_delivery() {
    let history = fixture();
    for reverse in [false, true] {
        let (a, b) = if reverse { (10, 7) } else { (7, 10) };
        let mut calls = 0;
        let mut checkpoint = || {
            calls += 1;
            Ok(())
        };
        let mut good = open(
            &history,
            CommitSeq(a),
            CommitSeq(b),
            history.units,
            budget(2),
            &mut checkpoint,
        )
        .unwrap();
        while let Some(item) = good.advance(&mut checkpoint) {
            item.unwrap();
        }
        let stats = good.stats;
        let total = calls;
        assert!(total > 5 && stats.work_units > 0 && stats.scratch_entries > 0);
        unpinned(&history);
        for stop in 1..=total {
            let mut seen = 0;
            let mut checkpoint = || {
                seen += 1;
                if seen == stop {
                    Err(StandingQueryFailure::Interrupted)
                } else {
                    Ok(())
                }
            };
            let result: Result<(), StandingQueryError> = match open(
                &history,
                CommitSeq(a),
                CommitSeq(b),
                history.units,
                budget(2),
                &mut checkpoint,
            ) {
                Err(error) => Err(error),
                Ok(mut pull) => {
                    let failure = loop {
                        match pull.advance(&mut checkpoint) {
                            Some(Ok(_)) => {}
                            Some(Err(error)) => break error,
                            None => panic!("missed injected refusal {stop}"),
                        }
                    };
                    assert!(pull.merge.is_none());
                    assert!(
                        pull.advance(&mut || panic!("refused cursor polled control"))
                            .is_none()
                    );
                    Err(failure)
                }
            };
            assert!(matches!(
                result,
                Err(StandingQueryError::Delivery(
                    StandingQueryFailure::Interrupted
                ))
            ));
            assert_eq!(seen, stop);
            unpinned(&history);
            let mut owner = None;
            let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut seen = 0;
                let mut checkpoint = || {
                    seen += 1;
                    assert_ne!(seen, stop, "injected unwind");
                    Ok(())
                };
                owner = Some(
                    open(
                        &history,
                        CommitSeq(a),
                        CommitSeq(b),
                        history.units,
                        budget(2),
                        &mut checkpoint,
                    )
                    .unwrap(),
                );
                while let Some(item) = owner.as_mut().unwrap().advance(&mut checkpoint) {
                    item.unwrap();
                }
            }));
            assert!(unwind.is_err());
            if let Some(mut pull) = owner {
                assert!(pull.merge.is_none());
                assert!(
                    pull.advance(&mut || panic!("unwound cursor polled control"))
                        .is_none()
                );
            }
            unpinned(&history);
        }
        for (work, scratch, rows, passes) in [
            (stats.work_units, stats.scratch_entries, 2, true),
            (stats.work_units - 1, stats.scratch_entries, 2, false),
            (stats.work_units, stats.scratch_entries - 1, 2, false),
            (stats.work_units, stats.scratch_entries, 1, false),
        ] {
            let result = open(
                &history,
                CommitSeq(a),
                CommitSeq(b),
                history.units,
                GqlQueryPolicy::new(0, rows, work, scratch),
                &mut || Ok(()),
            )
            .and_then(|mut pull| drain(&mut pull));
            assert_eq!(result.is_ok(), passes);
            unpinned(&history);
        }
        let mut retry = open(
            &history,
            CommitSeq(a),
            CommitSeq(b),
            history.units,
            budget(2),
            &mut || Ok(()),
        )
        .unwrap();
        let sign = if reverse { -1 } else { 1 };
        assert_eq!(
            drain(&mut retry).unwrap(),
            vec![(key(1), 5 * sign), (key(2), 4 * sign)]
        );
    }
}

#[test]
fn promoted_weights_and_chronological_overflow_are_not_narrowed_or_reordered() {
    let large = ZWeight::from_i128(i128::MAX)
        .checked_mul(&ZWeight::from_i128(3), LIMBS)
        .unwrap();
    let mut history = History::new(CommitSeq(0));
    append(
        &mut history,
        bag([(key(1), large.checked_clone(LIMBS).unwrap())]),
    );
    let mut reverse = open(
        &history,
        CommitSeq(1),
        CommitSeq(0),
        history.units,
        budget(1),
        &mut || Ok(()),
    )
    .unwrap();
    let (row, weight) = reverse.advance(&mut || Ok(())).unwrap().unwrap();
    assert_eq!(row, key(1));
    assert!(weight.is_promoted());
    assert_eq!(weight, large.checked_neg(LIMBS).unwrap());
    assert!(reverse.advance(&mut || Ok(())).is_none());
    let m = ZWeight::from_i128(i128::MAX);
    // Construct with spare temporary limbs; the canonical retained value has
    // four limbs and is admitted under the production limit when copied.
    let near_limit = m
        .checked_mul(&m, LimbLimit::new(8))
        .unwrap()
        .checked_mul(&ZWeight::from_i128(4), LimbLimit::new(8))
        .unwrap();
    let mut overflow = History::new(CommitSeq(0));
    append(
        &mut overflow,
        bag([(key(1), near_limit.checked_clone(LIMBS).unwrap())]),
    );
    append(
        &mut overflow,
        bag([(key(1), near_limit.checked_clone(LIMBS).unwrap())]),
    );
    append(
        &mut overflow,
        bag([(key(1), near_limit.checked_neg(LIMBS).unwrap())]),
    );
    for (a, b) in [(0, 3), (3, 0)] {
        let mut pull = open(
            &overflow,
            CommitSeq(a),
            CommitSeq(b),
            overflow.units,
            budget(1),
            &mut || Ok(()),
        )
        .unwrap();
        assert!(matches!(
            pull.advance(&mut || Ok(())),
            Some(Err(StandingQueryError::Delivery(
                StandingQueryFailure::Arithmetic
            )))
        ));
        assert!(pull.merge.is_none());
        unpinned(&overflow);
    }
    let oversized = near_limit
        .checked_mul(&ZWeight::from_i128(4), LimbLimit::new(8))
        .unwrap();
    let mut invalid = History::new(CommitSeq(0));
    append(
        &mut invalid,
        bag([(key(1), oversized.checked_clone(LimbLimit::new(8)).unwrap())]),
    );
    append(
        &mut invalid,
        bag([(key(1), oversized.checked_neg(LimbLimit::new(8)).unwrap())]),
    );
    assert!(matches!(
        open(
            &invalid,
            CommitSeq(0),
            CommitSeq(2),
            invalid.units,
            budget(0),
            &mut || Ok(())
        ),
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::Arithmetic
        ))
    ));
    unpinned(&invalid);
}

#[test]
fn large_output_keeps_one_head_per_tick_and_drop_releases_all_history() {
    let mut costs = Vec::new();
    for size in [16, 16_384] {
        let mut history = History::new(CommitSeq(0));
        append(&mut history, bag((0..size).map(|n| (key(n), ZWeight::ONE))));
        let weak = Arc::downgrade(&history.frames[0]);
        let mut pull = open(
            &history,
            CommitSeq(0),
            CommitSeq(1),
            history.units,
            budget(u64::MAX),
            &mut || Ok(()),
        )
        .unwrap();
        let opening = pull.stats;
        assert_eq!(pull.merge.as_ref().unwrap().heads.len(), 1);
        history.frames.clear();
        assert!(weak.upgrade().is_some());
        let first = pull.advance(&mut || Ok(())).unwrap().unwrap();
        assert_eq!(first.0, key(0));
        assert_eq!(first.1, ZWeight::ONE);
        assert_eq!(pull.merge.as_ref().unwrap().heads.len(), 1);
        costs.push((
            opening.work_units,
            opening.scratch_entries,
            pull.stats.work_units,
            pull.stats.scratch_entries,
        ));
        drop(pull);
        assert!(
            weak.upgrade().is_none(),
            "delivered owned row must not retain its source frame"
        );
    }
    assert_eq!(
        costs[0], costs[1],
        "opening/first row must not materialize all output"
    );
}

fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn symbols() -> RelationBind {
    RelationBind::new().with_property("p", PropertyKeyId(1))
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn result_bag(result: QueryResult) -> BTreeMap<Vec<QueryValue>, i128> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("read required");
    };
    let mut counts = BTreeMap::new();
    for row in rows {
        *counts.entry(row).or_default() += 1;
    }
    counts
}

#[test]
fn real_query_net_cursors_allow_writes_eviction_rebuild_and_database_destruction() {
    let ((), report) = run_async_under_lab(0x51a7_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [(1, 3), (2, 3), (3, 7)] {
            seed.create_vertex(
                VId(id),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
            );
        }
        let start = db.write(&commit, seed).await.unwrap();
        let params = GqlParameters::new();
        let texts = [
            "MATCH (n) RETURN n.p AS p",
            "MATCH (n) RETURN DISTINCT n.p AS p",
            "MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 2",
            "MATCH (n) RETURN COUNT(*) AS c, SUM(n.p) AS s, AVG(n.p) AS a",
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
        ];
        let mut logs = Vec::new();
        let mut expected = Vec::new();
        for text in texts {
            expected.push(result_bag(
                db.query(&cx, text, &params, symbols(), query_policy())
                    .unwrap(),
            ));
            let view = db
                .register_standing_native(&cx, text, &params, symbols(), query_policy())
                .unwrap();
            assert!(matches!(
                db.standing_replay_owned_difference_cursor(&cx, &view, start, start, 0, budget(0)),
                Err(StandingQueryError::Unsupported)
            ));
            logs.push(
                db.register_standing_replay(&cx, &view, 2, 1000, 10000, query_policy())
                    .unwrap(),
            );
        }
        for step in 0..2 {
            let mut edit = WriteBatch::new(RelationId(1));
            if step == 0 {
                edit.delete_vertex(VId(1));
            } else {
                edit.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(2)));
            }
            db.write(&commit, edit).await.unwrap();
        }
        let end = db.frontier().unwrap();
        for (text, expected) in texts.iter().zip(&mut expected) {
            for value in expected.values_mut() {
                *value = -*value;
            }
            for (row, count) in result_bag(
                db.query(&cx, text, &params, symbols(), query_policy())
                    .unwrap(),
            ) {
                *expected.entry(row).or_default() += count;
            }
            expected.retain(|_, count| *count != 0);
        }
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.standing_replay_owned_difference_cursor(
                &cx,
                &logs[0],
                start,
                end,
                10000,
                budget(100)
            ),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert!(
            db.standing_replay_owned_coalesced_cursor(&cx, &logs[0], end, ticks(1), 0, budget(0))
                .unwrap()
                .is_none()
        );
        let mut forward: Vec<_> = logs
            .iter()
            .map(|log| {
                let (selected, cursor) = db
                    .standing_replay_owned_coalesced_cursor(
                        &cx,
                        log,
                        start,
                        ticks(2),
                        10000,
                        budget(100),
                    )
                    .unwrap()
                    .unwrap();
                assert_eq!(selected, end);
                cursor
            })
            .collect();
        let mut reverse: Vec<_> = logs
            .iter()
            .map(|log| {
                db.standing_replay_owned_difference_cursor(&cx, log, end, start, 10000, budget(100))
                    .unwrap()
            })
            .collect();
        // This must type-check: neither opaque cursor borrows db or the handle.
        for value in [20, 30] {
            let mut edit = WriteBatch::new(RelationId(1));
            edit.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(value)));
            db.write(&commit, edit).await.unwrap();
        }
        assert!(matches!(
            db.standing_replay_owned_difference_cursor(
                &cx,
                &logs[0],
                start,
                end,
                10000,
                budget(100)
            ),
            Err(StandingQueryError::ReplayGap { .. })
        ));
        db.rebuild_standing_query(&cx, &logs[0], query_policy())
            .unwrap();
        drop(logs);
        drop(db);
        for (index, expected) in expected.iter().enumerate() {
            let actual: BTreeMap<_, _> = forward[index]
                .by_ref()
                .map(|item| {
                    let (row, weight) = item.unwrap();
                    (row, weight.to_i128().unwrap())
                })
                .collect();
            assert_eq!(&actual, expected);
            let actual: BTreeMap<_, _> = reverse[index]
                .by_ref()
                .map(|item| {
                    let (row, weight) = item.unwrap();
                    (row, -weight.to_i128().unwrap())
                })
                .collect();
            assert_eq!(&actual, expected);
            assert!(forward[index].next().is_none());
            assert!(reverse[index].next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
