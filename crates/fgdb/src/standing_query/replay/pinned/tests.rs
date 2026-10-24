use super::*;
use crate::{DatabaseKeys, QueryResult, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::{GqlParameters, RelationBind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const LIMBS: LimbLimit = LimbLimit::new(4);
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, rows, 1_000_000, 1_000_000)
}
fn ticks(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}
fn append(history: &mut History, key: i128, weight: ZWeight) {
    let rows = ZSet::from_updates(
        [(vec![QueryValue::Integer(key)], weight)],
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(u64::MAX),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let at = history.frontier().checked_successor().unwrap();
    history
        .prepare(at, rows, Limits::new(32, 100, 1000).unwrap(), &mut meter)
        .unwrap()
        .commit();
}
fn history() -> History {
    let mut history = History::new(CommitSeq(7));
    append(&mut history, 1, ZWeight::ONE);
    append(&mut history, 0, ZWeight::ZERO);
    append(&mut history, 2, ZWeight::from_i128(-3));
    history
}
fn open(
    history: &History,
    after: CommitSeq,
    max_ticks: usize,
    units: usize,
    budget: GqlQueryPolicy,
) -> Result<Option<(CommitSeq, PinnedReplay)>, StandingQueryError> {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: budget,
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    PinnedReplay::open(history, after, ticks(max_ticks), units, &mut meter)
}
fn assert_unpinned(history: &History) {
    assert!(
        history
            .frames
            .iter()
            .all(|frame| Arc::strong_count(frame) == 1)
    );
}

#[test]
fn owned_prefixes_match_the_original_tick_chain_and_pin_units_are_not_free() {
    let history = history();
    let window = history.window();
    for count in [1, 2, 3, 17, usize::MAX] {
        let selected = count.min(3);
        let units: usize = history
            .frames
            .iter()
            .take(selected)
            .map(|frame| frame.units)
            .sum();
        let (end, mut pull) = open(&history, CommitSeq(7), count, units, policy(2))
            .unwrap()
            .unwrap();
        assert_eq!(end, CommitSeq(7 + selected as u64));
        for (index, original) in history.frames.iter().take(selected).enumerate() {
            let frame = pull.advance(&mut || Ok(())).unwrap().unwrap();
            assert!(Arc::ptr_eq(&frame, original));
            assert_eq!(frame.from(), CommitSeq(7 + index as u64));
            assert_eq!(frame.frontier(), CommitSeq(8 + index as u64));
            if index == 1 {
                assert!(frame.rows().is_empty());
            }
        }
        assert!(pull.advance(&mut || Ok(())).is_none());
        assert!(
            pull.advance(&mut || panic!("EOF touched controls"))
                .is_none()
        );
        assert!(matches!(
            open(&history, CommitSeq(7), count, units - 1, policy(2)),
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ScratchBudget
            ))
        ));
        assert_eq!(history.window(), window);
        assert_unpinned(&history);
    }
    assert!(
        open(&history, CommitSeq(10), 1, 0, policy(0))
            .unwrap()
            .is_none()
    );
    let (_, mut empty_tick) = open(&history, CommitSeq(8), 1, 1, policy(0))
        .unwrap()
        .unwrap();
    let frame = empty_tick.advance(&mut || Ok(())).unwrap().unwrap();
    assert_eq!(frame.frontier(), CommitSeq(9));
    assert!(frame.rows().is_empty());
}

#[test]
fn coverage_refusals_do_not_return_a_prefix_or_leave_partial_pins() {
    let mut history = history();
    for cut in [CommitSeq(6), CommitSeq(11)] {
        assert!(matches!(
            open(&history, cut, 3, 100, policy(3)),
            Err(StandingQueryError::ReplayGap { .. })
        ));
        assert_unpinned(&history);
    }
    // The first frame can be privately pinned before the invalid middle link
    // is found. Refusing opening must release that earlier pin too.
    Arc::get_mut(&mut history.frames[1]).unwrap().from = CommitSeq(7);
    assert!(matches!(
        open(&history, CommitSeq(7), 3, 100, policy(3)),
        Err(StandingQueryError::Delivery(
            StandingQueryFailure::InvalidDelta
        ))
    ));
    assert_unpinned(&history);
    let mut terminal = History::new(CommitSeq(u64::MAX - 1));
    append(&mut terminal, 0, ZWeight::ZERO);
    let (end, mut pull) = open(&terminal, CommitSeq(u64::MAX - 1), 2, 1, policy(0))
        .unwrap()
        .unwrap();
    assert_eq!(end, CommitSeq(u64::MAX));
    assert_eq!(
        pull.advance(&mut || Ok(())).unwrap().unwrap().frontier(),
        end
    );
    assert!(pull.advance(&mut || Ok(())).is_none());
}

#[test]
fn a_delivery_allowance_is_cumulative_and_never_splits_a_frame() {
    let history = history();
    let (_, mut pull) = open(&history, CommitSeq(7), 3, 100, policy(1))
        .unwrap()
        .unwrap();
    assert_eq!(
        pull.advance(&mut || Ok(())).unwrap().unwrap().frontier(),
        CommitSeq(8)
    );
    assert!(
        pull.advance(&mut || Ok(()))
            .unwrap()
            .unwrap()
            .rows()
            .is_empty()
    );
    assert!(matches!(
        pull.advance(&mut || Ok(())),
        Some(Err(StandingQueryError::Delivery(
            StandingQueryFailure::ResultBudget
        )))
    ));
    assert_eq!(pull.delivered, 1);
    assert!(pull.frames.is_none());
    assert!(
        pull.advance(&mut || panic!("failed cursor touched controls"))
            .is_none()
    );
    assert_unpinned(&history);
    let promoted = ZWeight::from_i128(i128::MAX)
        .checked_add(&ZWeight::ONE, LIMBS)
        .unwrap();
    let mut large = History::new(CommitSeq(0));
    append(&mut large, 1, promoted);
    let (_, mut pull) = open(&large, CommitSeq(0), 1, 100, policy(1))
        .unwrap()
        .unwrap();
    let frame = pull.advance(&mut || Ok(())).unwrap().unwrap();
    assert!(frame.rows().iter().next().unwrap().1.is_promoted());
    assert_eq!(frame.rows().len(), 1);
    assert!(Arc::ptr_eq(&frame, &large.frames[0]));

    let mut wide = History::new(CommitSeq(0));
    let rows = ZSet::from_updates(
        [
            (vec![QueryValue::Integer(1)], ZWeight::ONE),
            (vec![QueryValue::Integer(2)], ZWeight::ONE),
        ],
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(2),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    wide.prepare(
        CommitSeq(1),
        rows,
        Limits::new(1, 2, 100).unwrap(),
        &mut meter,
    )
    .unwrap()
    .commit();
    let (_, mut pull) = open(&wide, CommitSeq(0), 1, 100, policy(1))
        .unwrap()
        .unwrap();
    assert!(matches!(
        pull.advance(&mut || Ok(())),
        Some(Err(StandingQueryError::Delivery(
            StandingQueryFailure::ResultBudget
        )))
    ));
    assert_eq!(pull.delivered, 0);
    assert!(pull.frames.is_none());
    assert_unpinned(&wide);
}

#[test]
fn every_open_pull_and_eof_checkpoint_releases_pins_on_refusal_or_unwind() {
    let history = history();
    let mut calls = 0;
    let mut checkpoint = || {
        calls += 1;
        Ok(())
    };
    let mut meter = Meter {
        policy: policy(2),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let (_, mut good) = PinnedReplay::open(&history, CommitSeq(7), ticks(3), 100, &mut meter)
        .unwrap()
        .unwrap();
    while let Some(frame) = good.advance(&mut checkpoint) {
        frame.unwrap();
    }
    let exact = GqlQueryPolicy::new(0, 2, good.stats.work_units, good.stats.scratch_entries);
    assert!(calls > 3 && good.stats.scratch_entries == 3);
    assert_unpinned(&history);
    for stop in 1..=calls {
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
            policy: exact,
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        match PinnedReplay::open(&history, CommitSeq(7), ticks(3), 100, &mut meter) {
            Err(StandingQueryError::Delivery(StandingQueryFailure::Interrupted)) => {}
            Ok(Some((_, mut pull))) => {
                loop {
                    match pull.advance(&mut checkpoint) {
                        Some(Ok(_)) => {}
                        Some(Err(StandingQueryError::Delivery(
                            StandingQueryFailure::Interrupted,
                        ))) => break,
                        _ => panic!("missed interruption {stop}"),
                    }
                }
                assert!(pull.frames.is_none());
                assert!(
                    pull.advance(&mut || panic!("refused cursor resumed"))
                        .is_none()
                );
            }
            _ => panic!("unexpected opening outcome"),
        }
        assert_eq!(visited, stop);
        assert_unpinned(&history);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut visited = 0;
            let mut checkpoint = || {
                visited += 1;
                assert_ne!(visited, stop, "injected callback unwind");
                Ok(())
            };
            let mut meter = Meter {
                policy: exact,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let (_, mut pull) =
                PinnedReplay::open(&history, CommitSeq(7), ticks(3), 100, &mut meter)
                    .unwrap()
                    .unwrap();
            while let Some(frame) = pull.advance(&mut checkpoint) {
                frame.unwrap();
            }
        }));
        assert!(panic.is_err());
        assert_unpinned(&history);
    }
    for (work, scratch, passes) in [
        (
            exact.evaluator.max_work_units,
            exact.evaluator.max_scratch_entries,
            true,
        ),
        (
            exact.evaluator.max_work_units - 1,
            exact.evaluator.max_scratch_entries,
            false,
        ),
        (
            exact.evaluator.max_work_units,
            exact.evaluator.max_scratch_entries - 1,
            false,
        ),
    ] {
        let result = (|| {
            let (_, mut pull) = open(
                &history,
                CommitSeq(7),
                3,
                100,
                GqlQueryPolicy::new(0, 2, work, scratch),
            )?
            .unwrap();
            while let Some(frame) = pull.advance(&mut || Ok(())) {
                frame?;
            }
            Ok::<_, StandingQueryError>(())
        })();
        assert_eq!(result.is_ok(), passes);
        assert_unpinned(&history);
    }
}

#[test]
fn drop_releases_only_the_cursors_remaining_pins_without_draining() {
    let mut history = history();
    let weak: Vec<_> = history.frames.iter().map(Arc::downgrade).collect();
    let (_, mut pull) = open(&history, CommitSeq(7), 3, 100, policy(2))
        .unwrap()
        .unwrap();
    history.frames.clear();
    let first = pull.advance(&mut || Ok(())).unwrap().unwrap();
    assert!(weak.iter().all(|frame| frame.upgrade().is_some()));
    drop(pull);
    assert!(weak[0].upgrade().is_some());
    assert!(weak[1..].iter().all(|frame| frame.upgrade().is_none()));
    drop(first);
    assert!(weak.iter().all(|frame| frame.upgrade().is_none()));
}

fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn symbols() -> RelationBind {
    RelationBind::new().with_property("p", PropertyKeyId(1))
}
fn result_bag(result: QueryResult) -> ZSet<Vec<QueryValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("read rows required");
    };
    ZSet::from_updates(
        rows.into_iter().map(|row| (row, ZWeight::ONE)),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

#[test]
fn pinned_delivery_survives_writes_eviction_rebuild_and_database_drop() {
    let ((), report) = run_async_under_lab(0x51a7_1001, |root| async move {
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
        let mut bags = Vec::new();
        for text in texts {
            let view = db
                .register_standing_native(&cx, text, &params, symbols(), query_policy())
                .unwrap();
            bags.push(
                db.standing_native_bag(&cx, &view, query_policy())
                    .unwrap()
                    .1,
            );
            assert!(matches!(
                db.standing_replay_owned_cursor(&cx, &view, start, ticks(2), 10000, policy(100)),
                Err(StandingQueryError::Unsupported)
            ));
            logs.push(
                db.register_standing_replay(&cx, &view, 2, 1000, 10000, query_policy())
                    .unwrap(),
            );
        }
        let mut expected = Vec::new();
        for step in 0..2 {
            let mut edit = WriteBatch::new(RelationId(1));
            if step == 0 {
                edit.delete_vertex(VId(1));
            } else {
                edit.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(2)));
            }
            db.write(&commit, edit).await.unwrap();
            expected.push(
                texts
                    .iter()
                    .map(|text| {
                        result_bag(
                            db.query(&cx, text, &params, symbols(), query_policy())
                                .unwrap(),
                        )
                    })
                    .collect::<Vec<_>>(),
            );
        }
        let end = db.frontier().unwrap();
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.standing_replay_owned_cursor(
                &cx,
                &logs[0],
                start,
                ticks(2),
                10000,
                policy(100)
            ),
            Err(StandingQueryError::ForeignHandle)
        ));
        let mut cursors = logs
            .iter()
            .map(|log| {
                let (selected, cursor) = db
                    .standing_replay_owned_cursor(&cx, log, start, ticks(2), 10000, policy(100))
                    .unwrap()
                    .unwrap();
                assert_eq!(selected, end);
                cursor
            })
            .collect::<Vec<_>>();
        // These writes must compile while the cursors are alive. All original
        // frames then leave the producer's two-tick retention window.
        for value in [20, 30] {
            let mut edit = WriteBatch::new(RelationId(1));
            edit.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(value)));
            db.write(&commit, edit).await.unwrap();
        }
        assert!(matches!(
            db.standing_replay_owned_cursor(&cx, &logs[0], start, ticks(2), 10000, policy(100)),
            Err(StandingQueryError::ReplayGap { .. })
        ));
        db.rebuild_standing_query(&cx, &logs[0], query_policy())
            .unwrap();
        drop(logs);
        drop(db);
        for (index, cursor) in cursors.iter_mut().enumerate() {
            let mut after = start;
            for answers in &expected {
                let frame = cursor.next().unwrap().unwrap();
                assert_eq!(frame.from(), after);
                after = frame.frontier();
                bags[index]
                    .integrate(frame.rows(), LIMBS, &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(bags[index], answers[index]);
            }
            assert_eq!(after, end);
            assert!(cursor.next().is_none());
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
