use super::*;
use crate::{DatabaseKeys, QueryResult, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, rows, 1_000_000, 1_000_000)
}
fn bag(counts: [i128; 2]) -> Bag {
    ZSet::from_updates(
        counts.into_iter().enumerate().map(|(key, count)| {
            (
                vec![QueryValue::Integer(key as i128)],
                ZWeight::from_i128(count),
            )
        }),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn append(history: &mut History, rows: Bag, limits: Limits) {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(1000),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let next = history.frontier().checked_successor().unwrap();
    history
        .prepare(next, rows, limits, &mut meter)
        .unwrap()
        .commit();
}
fn limits() -> Limits {
    Limits::new(32, 1000, 100_000).unwrap()
}
fn baseline(current: &Bag, meter: &mut Meter<'_>) -> Result<Bag, StandingQueryError> {
    let mut rows = ZSet::new();
    for (row, weight) in current.iter() {
        let cells = copy_row(row, meter).map_err(StandingQueryError::Delivery)?;
        meter
            .charge(ZSetEvent::Work)
            .map_err(StandingQueryError::Delivery)?;
        let weight = weight.checked_clone(LIMBS).unwrap();
        rows.accumulate(cells, weight, LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)
            .map_err(StandingQueryError::Delivery)?;
    }
    Ok(rows)
}
fn attempt(
    history: &History,
    at: CommitSeq,
    current: &Bag,
    policy: GqlQueryPolicy,
    mut checkpoint: impl FnMut() -> Result<(), StandingQueryFailure>,
) -> (Result<Bag, StandingQueryError>, StandingQueryStats) {
    let mut meter = Meter {
        policy,
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let result = history.snapshot_with(at, 1, &mut meter, |meter| {
        assert_eq!(meter.policy.rows.max_result_rows(), None);
        assert_eq!(meter.policy.rows.max_snapshot_records(), Some(0));
        assert_eq!(meter.policy.evaluator, policy.evaluator);
        Ok((history.frontier(), baseline(current, meter)?))
    });
    assert_eq!(
        meter.policy, policy,
        "private row allowance must be restored"
    );
    (result, meter.stats)
}

#[test]
fn all_729_three_revision_histories_reconstruct_independent_snapshot_counts() {
    for mut code in 0..729 {
        let mut counts = [[0_i128; 2]; 3];
        for state in &mut counts {
            for count in state {
                *count = code % 3;
                code /= 3;
            }
        }
        let mut history = History::new(CommitSeq(7));
        for pair in counts.windows(2) {
            append(
                &mut history,
                bag([pair[1][0] - pair[0][0], pair[1][1] - pair[0][1]]),
                limits(),
            );
        }
        let window = history.window();
        let current = bag(counts[2]);
        for (index, expected) in counts.into_iter().enumerate() {
            let result = attempt(
                &history,
                CommitSeq(7 + index as u64),
                &current,
                policy(2),
                || Ok(()),
            )
            .0
            .unwrap();
            assert_eq!(result, bag(expected));
            assert_eq!(history.window(), window);
            assert_eq!(current, bag(counts[2]));
        }
    }
}

#[test]
fn unavailable_or_broken_suffix_refuses_before_current_baseline_copy() {
    let mut history = History::new(CommitSeq(7));
    append(&mut history, bag([1, 0]), Limits::new(1, 10, 1000).unwrap());
    append(&mut history, bag([0, 0]), Limits::new(1, 10, 1000).unwrap());
    assert_eq!(history.retained_after(), CommitSeq(8));
    for at in [CommitSeq(6), CommitSeq(7), CommitSeq(10)] {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(0),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        assert!(matches!(
            history.snapshot_with(at, 1, &mut meter, |_| panic!(
                "unavailable cut copied a baseline"
            )),
            Err(StandingQueryError::ReplayGap { .. })
        ));
    }
    let mut history = History::new(CommitSeq(7));
    append(&mut history, bag([1, 0]), limits());
    append(&mut history, bag([0, 1]), limits());
    Arc::get_mut(&mut history.frames[1]).unwrap().from = CommitSeq(7);
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(2),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    assert!(matches!(
        history.snapshot_with(CommitSeq(7), 1, &mut meter, |_| panic!(
            "broken link copied a baseline"
        )),
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::InvalidDelta
        ))
    ));
    // A forged last frontier must not make a valid earlier link look terminal.
    Arc::get_mut(&mut history.frames[1]).unwrap().frontier = CommitSeq(8);
    assert!(matches!(
        history.snapshot_with(CommitSeq(7), 1, &mut meter, |_| panic!(
            "hidden suffix copied a baseline"
        )),
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::InvalidDelta
        ))
    ));
}

#[test]
fn only_historical_support_spends_rows_and_every_phase_spends_one_allowance() {
    let mut history = History::new(CommitSeq(7));
    append(&mut history, bag([1, 1]), limits());
    let current = bag([1, 1]);
    let (result, stats) = attempt(&history, CommitSeq(7), &current, policy(0), || Ok(()));
    assert!(result.unwrap().is_empty()); // current input has two rows
    let exact = GqlQueryPolicy::new(0, 0, stats.work_units, stats.scratch_entries);
    assert!(
        attempt(&history, CommitSeq(7), &current, exact, || Ok(()))
            .0
            .unwrap()
            .is_empty()
    );
    for (work, scratch, expected) in [
        (
            stats.work_units - 1,
            stats.scratch_entries,
            StandingQueryFailure::WorkBudget,
        ),
        (
            stats.work_units,
            stats.scratch_entries - 1,
            StandingQueryFailure::ScratchBudget,
        ),
    ] {
        assert!(matches!(
            attempt(&history, CommitSeq(7), &current, GqlQueryPolicy::new(0, 0, work, scratch), || Ok(())).0,
            Err(StandingQueryError::Delivery(error)) if error == expected
        ));
    }
    // Live reads still admit and copy the baseline; equal cuts skip no budget.
    assert!(matches!(
        attempt(&history, CommitSeq(8), &current, policy(1), || Ok(())).0,
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::ResultBudget
        ))
    ));
    let mut calls = 0;
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: GqlQueryPolicy::new(0, 2, 2, 1000),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let result = history.snapshot_with(CommitSeq(7), 1, &mut meter, |meter| {
        calls += 1;
        assert_eq!(meter.stats.work_units, 2, "coverage work was not reset");
        meter
            .charge(ZSetEvent::Work)
            .map_err(StandingQueryError::Delivery)?;
        unreachable!()
    });
    assert_eq!(calls, 1);
    assert!(matches!(
        result,
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::WorkBudget
        ))
    ));
    assert_eq!(meter.policy.rows.max_result_rows(), Some(2));
}

#[test]
fn promoted_multiplicities_and_reverse_tick_order_recover_real_intermediate_states() {
    let huge = ZWeight::from_i128(i128::MAX)
        .checked_add(&ZWeight::ONE, LIMBS)
        .unwrap();
    let make = |negative| {
        let weight = if negative {
            huge.checked_neg(LIMBS).unwrap()
        } else {
            huge.checked_clone(LIMBS).unwrap()
        };
        ZSet::from_updates([(vec![QueryValue::Integer(9)], weight)], LIMBS, &mut |_| {
            Ok::<_, ()>(())
        })
        .unwrap()
    };
    let mut history = History::new(CommitSeq(7));
    append(&mut history, make(false), limits());
    append(&mut history, make(true), limits());
    append(&mut history, make(false), limits());
    let current = make(false);
    for (at, present) in [(7, false), (8, true), (9, false), (10, true)] {
        let result = attempt(
            &history,
            CommitSeq(at),
            &current,
            policy(u64::from(present)),
            || Ok(()),
        )
        .0
        .unwrap();
        if present {
            let weight = result.iter().next().unwrap().1;
            assert!(weight.to_i128().is_none());
            assert_eq!(weight, &huge);
        } else {
            assert!(result.is_empty());
        }
    }
}

#[test]
fn malformed_shapes_negative_reconstruction_and_wrong_baseline_generation_fail_closed() {
    let mut history = History::new(CommitSeq(7));
    append(&mut history, bag([2, 0]), limits());
    assert!(matches!(
        attempt(&history, CommitSeq(7), &bag([1, 0]), policy(2), || Ok(())).0,
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::InvalidDelta
        ))
    ));
    assert!(matches!(
        attempt(&history, CommitSeq(8), &bag([-1, 0]), policy(2), || Ok(())).0,
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::InvalidDelta
        ))
    ));
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(2),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    assert!(matches!(
        history.snapshot_with(CommitSeq(8), 1, &mut meter, |_| Ok((
            CommitSeq(9),
            bag([2, 0])
        ))),
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::InvalidDelta
        ))
    ));
    let malformed =
        ZSet::from_updates(
            [(Vec::new(), ZWeight::ONE)],
            LIMBS,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
    Arc::get_mut(&mut history.frames[0]).unwrap().rows = Arc::new(malformed);
    assert!(matches!(
        attempt(&history, CommitSeq(7), &bag([2, 0]), policy(2), || Ok(())).0,
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::InvalidDelta
        ))
    ));
}

#[test]
fn every_checkpoint_and_unwind_preserves_history_current_bag_and_retry() {
    let mut history = History::new(CommitSeq(7));
    append(&mut history, bag([-2, 3]), limits());
    append(&mut history, bag([1, -1]), limits());
    let current = bag([1, 2]);
    let saved = current
        .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
        .unwrap();
    let frames: Vec<_> = history.frames.iter().map(Arc::clone).collect();
    let window = history.window();
    let mut calls = 0;
    let (result, _) = attempt(&history, CommitSeq(7), &current, policy(2), || {
        calls += 1;
        Ok(())
    });
    let expected = result.unwrap();
    assert_eq!(expected, bag([2, 0]));
    for stop in 1..=calls {
        let mut seen = 0;
        assert!(matches!(
            attempt(&history, CommitSeq(7), &current, policy(2), || {
                seen += 1;
                if seen == stop {
                    Err(StandingQueryFailure::Interrupted)
                } else {
                    Ok(())
                }
            })
            .0,
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::Interrupted
            ))
        ));
        assert_eq!(seen, stop);
        let mut seen = 0;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = attempt(&history, CommitSeq(7), &current, policy(2), || {
                    seen += 1;
                    assert_ne!(seen, stop, "injected rewind interruption");
                    Ok(())
                });
            }))
            .is_err()
        );
        assert_eq!(seen, stop);
        assert_eq!(current, saved);
        assert_eq!(history.window(), window);
        for (a, b) in history.frames.iter().zip(&frames) {
            assert!(Arc::ptr_eq(a, b));
        }
        assert_eq!(
            attempt(&history, CommitSeq(7), &current, policy(2), || Ok(()))
                .0
                .unwrap(),
            expected
        );
    }
}

fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn result_bag(result: QueryResult) -> Bag {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("read expected");
    };
    ZSet::from_updates(
        rows.into_iter().map(|row| (row, ZWeight::ONE)),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, p) in [(1, 3), (2, 3), (3, 7)] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(p))],
        );
    }
    batch
}

#[test]
fn retained_snapshots_match_ordinary_queries_through_real_commit_hooks_and_survive_writer_drop() {
    let ((), report) = run_async_under_lab(0x71a0_5101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let first = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let texts = [
            "MATCH (n) RETURN n.p AS p",
            "MATCH (n) RETURN DISTINCT n.p AS p",
            "MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 2",
            "MATCH (n) WITH n.p AS p WHERE p > 2 RETURN p",
            "MATCH (n) RETURN COUNT(*) AS c,SUM(n.p) AS s,AVG(n.p) AS a",
            "MATCH (n) RETURN n.p AS p,COUNT(*) AS c GROUP BY n.p",
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            "RETURN 7 AS fixed",
        ];
        let mut replays = Vec::new();
        for text in texts {
            let source = db
                .register_standing_native(&cx, text, &params, resolve, query_policy())
                .unwrap();
            replays.push(
                db.register_standing_replay(&cx, &source, 8, 1000, 100_000, query_policy())
                    .unwrap(),
            );
        }
        let mut expected = vec![(
            first,
            texts
                .iter()
                .map(|text| {
                    result_bag(
                        db.query(&cx, text, &params, resolve, query_policy())
                            .unwrap(),
                    )
                })
                .collect::<Vec<_>>(),
        )];
        for tick in 0..3 {
            let mut batch = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    batch.delete_vertex(VId(1));
                }
                1 => {
                    batch.set_vertex_property(
                        VId(3),
                        PropertyKeyId(1),
                        Some(CanonicalScalar::Int(2)),
                    );
                }
                _ => {
                    batch.set_vertex_property(
                        VId(2),
                        PropertyKeyId(99),
                        Some(CanonicalScalar::Int(1)),
                    );
                }
            }
            let at = db.write(&commit, batch).await.unwrap();
            expected.push((
                at,
                texts
                    .iter()
                    .map(|text| {
                        result_bag(
                            db.query(&cx, text, &params, resolve, query_policy())
                                .unwrap(),
                        )
                    })
                    .collect::<Vec<_>>(),
            ));
        }
        let windows: Vec<_> = replays
            .iter()
            .map(|handle| db.standing_replay_window(&cx, handle).unwrap())
            .collect();
        let mut retained = Vec::new();
        for (at, bags) in &expected {
            for (i, replay) in replays.iter().enumerate() {
                let result = db
                    .standing_replay_snapshot(&cx, replay, *at, policy(bags[i].len() as u64))
                    .unwrap();
                assert_eq!(result, bags[i], "{} at {at:?}", texts[i]);
                assert_eq!(db.standing_replay_window(&cx, replay).unwrap(), windows[i]);
                if !bags[i].is_empty() {
                    assert!(matches!(
                        db.standing_replay_snapshot(
                            &cx,
                            replay,
                            *at,
                            policy(bags[i].len() as u64 - 1)
                        ),
                        Err(StandingQueryError::Delivery(
                            StandingQueryFailure::ResultBudget
                        ))
                    ));
                }
                retained.push(result);
            }
        }
        drop(db);
        for (observed, wanted) in retained
            .iter()
            .zip(expected.iter().flat_map(|(_, bags)| bags))
        {
            assert_eq!(observed, wanted);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn eviction_rebuild_foreign_handles_and_failed_sinks_never_fall_back_to_live_results() {
    let ((), report) = run_async_under_lab(0x71a0_5102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let first = db.write(&commit, seed()).await.unwrap();
        let source = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n.p AS p",
                &GqlParameters::new(),
                resolve,
                query_policy(),
            )
            .unwrap();
        let replay = db
            .register_standing_replay(&cx, &source, 1, 1000, 100_000, query_policy())
            .unwrap();
        let failed = db
            .register_standing_replay(&cx, &source, 8, 0, 100_000, query_policy())
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.delete_vertex(VId(1));
        let second = db.write(&commit, batch).await.unwrap();
        assert!(
            db.standing_replay_snapshot(&cx, &failed, second, policy(100))
                .is_err()
        );
        let mut batch = WriteBatch::new(RelationId(1));
        batch.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
        let third = db.write(&commit, batch).await.unwrap();
        assert!(matches!(
            db.standing_replay_snapshot(&cx, &replay, first, policy(100)),
            Err(StandingQueryError::ReplayGap { .. })
        ));
        assert!(matches!(
            db.standing_replay_snapshot(&cx, &source, third, policy(100)),
            Err(StandingQueryError::Unsupported)
        ));
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.standing_replay_snapshot(&cx, &replay, third, policy(100)),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert!(
            db.standing_replay_snapshot(&cx, &replay, second, policy(100))
                .is_ok()
        );
        db.rebuild_standing_query(&cx, &replay, query_policy())
            .unwrap();
        assert!(matches!(
            db.standing_replay_snapshot(&cx, &replay, second, policy(100)),
            Err(StandingQueryError::ReplayGap { .. })
        ));
        assert_eq!(
            db.standing_replay_snapshot(&cx, &replay, third, policy(100))
                .unwrap(),
            db.standing_native_bag(&cx, &source, policy(100)).unwrap().1
        );
        assert!(matches!(
            db.standing_replay_snapshot(&cx, &replay, CommitSeq(u64::MAX), policy(0)),
            Err(StandingQueryError::ReplayGap { .. })
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
