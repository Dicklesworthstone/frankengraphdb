use super::*;
use crate::{DatabaseKeys, QueryResult, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

const LIMBS: LimbLimit = LimbLimit::new(4);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}
fn batch(support: usize) -> ZSet<Vec<QueryValue>> {
    ZSet::from_updates(
        (0..support).map(|key| {
            (
                vec![QueryValue::Integer(key as i128)],
                ZWeight::from_i128(-1),
            )
        }),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn append(
    history: &mut History,
    support: usize,
    limits: Limits,
) -> Result<(), StandingQueryFailure> {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let at = history.frontier().checked_successor().unwrap();
    history
        .prepare(at, batch(support), limits, &mut meter)?
        .commit();
    Ok(())
}
fn references(history: &History) -> Vec<Arc<StandingReplayBatch>> {
    history.frames.iter().map(Arc::clone).collect()
}
fn unchanged(history: &History, window: StandingReplayWindow, frames: &[Arc<StandingReplayBatch>]) {
    assert_eq!(history.window(), window);
    assert_eq!(history.frames.len(), frames.len());
    for (a, b) in history.frames.iter().zip(frames) {
        assert!(Arc::ptr_eq(a, b));
    }
}

#[test]
fn bounded_history_matches_independent_maximal_suffix_oracle() {
    // Four ternary input sizes, all combinations of tick/support/payload caps.
    // Oracle chooses a maximal suffix backwards instead of simulating eviction.
    for ticks in 1..=3 {
        for rows in 0..=4 {
            for units in 1..=10 {
                let limits = Limits::new(ticks, rows, units).unwrap();
                for mut code in 0..81 {
                    let mut history = History::new(CommitSeq(7));
                    let mut oracle: Vec<(CommitSeq, usize, usize)> = Vec::new();
                    let mut frontier = CommitSeq(7);
                    for _ in 0..4 {
                        let n = code % 3;
                        code /= 3;
                        let cost = 1 + 3 * n; // tuple slot + one limb + Integer cell
                        let result = append(&mut history, n, limits);
                        if n > rows || cost > units {
                            assert!(result.is_err());
                        } else {
                            result.unwrap();
                            frontier = frontier.checked_successor().unwrap();
                            oracle.push((frontier, n, cost));
                            let mut total_rows = 0;
                            let mut total_units = 0;
                            let mut keep = 0;
                            for (_, nr, nu) in oracle.iter().rev() {
                                if keep == ticks
                                    || total_rows + nr > rows
                                    || total_units + nu > units
                                {
                                    break;
                                }
                                total_rows += nr;
                                total_units += nu;
                                keep += 1;
                            }
                            oracle = oracle[oracle.len() - keep..].to_vec();
                        }
                        assert_eq!(history.frontier(), frontier);
                        assert_eq!(history.frames.len(), oracle.len());
                        assert_eq!(
                            history.rows,
                            oracle.iter().map(|entry| entry.1).sum::<usize>()
                        );
                        assert_eq!(
                            history.units,
                            oracle.iter().map(|entry| entry.2).sum::<usize>()
                        );
                        let oldest = oracle
                            .first()
                            .map_or(CommitSeq(7), |entry| CommitSeq(entry.0.0 - 1));
                        assert_eq!(history.retained_after(), oldest);
                        for (at, nr, _) in &oracle {
                            let frame = history.next(CommitSeq(at.0 - 1)).unwrap().unwrap();
                            assert_eq!(frame.frontier(), *at);
                            assert_eq!(frame.rows().len(), *nr);
                            assert_eq!(frame.rows(), &batch(*nr));
                        }
                        assert!(history.next(frontier).unwrap().is_none());
                        assert!(matches!(
                            history.next(CommitSeq(oldest.0 - 1)),
                            Err(StandingQueryError::ReplayGap { .. })
                        ));
                        assert!(matches!(
                            history.next(frontier.checked_successor().unwrap()),
                            Err(StandingQueryError::ReplayGap { .. })
                        ));
                    }
                }
            }
        }
    }
}

#[test]
fn every_preparation_checkpoint_and_unwind_preserves_retained_history() {
    let limits = Limits::new(3, 3, 100).unwrap();
    let mut history = History::new(CommitSeq::ORIGIN);
    for _ in 0..3 {
        append(&mut history, 1, limits).unwrap();
    }
    let before = history.window();
    let frames = references(&history);
    let mut calls = 0;
    let mut checkpoint = || {
        calls += 1;
        Ok(())
    };
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    drop(
        history
            .prepare(CommitSeq(4), batch(2), limits, &mut meter)
            .unwrap(),
    );
    let stats = meter.stats;
    unchanged(&history, before, &frames);
    for stop in 1..=calls {
        let mut seen = 0;
        let mut checkpoint = || {
            seen += 1;
            if seen == stop {
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
        let result = history
            .prepare(CommitSeq(4), batch(2), limits, &mut meter)
            .map(drop);
        assert_eq!(result, Err(StandingQueryFailure::Interrupted));
        assert_eq!(seen, stop);
        unchanged(&history, before, &frames);
    }
    for stop in 1..=calls {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut seen = 0;
            let mut checkpoint = || {
                seen += 1;
                assert_ne!(seen, stop);
                Ok(())
            };
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            history
                .prepare(CommitSeq(4), batch(2), limits, &mut meter)
                .map(drop)
        }));
        assert!(result.is_err());
        unchanged(&history, before, &frames);
    }
    for (work, scratch, expected) in [
        (
            stats.work_units - 1,
            stats.scratch_entries,
            Some(StandingQueryFailure::WorkBudget),
        ),
        (
            stats.work_units,
            stats.scratch_entries - 1,
            Some(StandingQueryFailure::ScratchBudget),
        ),
        (stats.work_units, stats.scratch_entries, None),
    ] {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: GqlQueryPolicy::new(0, 0, work, scratch),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let result = history
            .prepare(CommitSeq(4), batch(2), limits, &mut meter)
            .map(drop);
        assert_eq!(result.err(), expected);
        unchanged(&history, before, &frames);
    }
    append(&mut history, 2, limits).unwrap();
    assert_eq!(history.window().retained_after, CommitSeq(2));
    assert_eq!(history.window().rows, 3);
    // External immutable references remain valid after their cache entries retire.
    assert_eq!(frames[0].frontier(), CommitSeq(1));
    assert_eq!(frames[0].rows(), &batch(1));
}

#[test]
fn exact_weights_empty_ticks_limits_and_sequence_exhaustion() {
    assert!(Limits::new(0, 0, 1).is_none());
    assert!(Limits::new(1, 0, 0).is_none());
    let mut empty = History::new(CommitSeq::ORIGIN);
    for _ in 0..20 {
        append(&mut empty, 0, Limits::new(2, 0, 2).unwrap()).unwrap();
    }
    assert_eq!(
        empty.window(),
        StandingReplayWindow {
            retained_after: CommitSeq(18),
            frontier: CommitSeq(20),
            ticks: 2,
            rows: 0,
            payload_units: 2,
        }
    );
    let huge = ZWeight::from_i128(i128::MAX)
        .checked_add(&ZWeight::ONE, LIMBS)
        .unwrap();
    let negative = huge
        .checked_add(&ZWeight::ONE, LIMBS)
        .unwrap()
        .checked_neg(LIMBS)
        .unwrap();
    let delta = ZSet::from_updates(
        [
            (vec![QueryValue::Integer(3)], huge),
            (vec![QueryValue::Integer(4)], negative),
        ],
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let mut history = History::new(CommitSeq(100));
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    history
        .prepare(
            CommitSeq(101),
            delta,
            Limits::new(1, 2, 100).unwrap(),
            &mut meter,
        )
        .unwrap()
        .commit();
    let frame = history.next(CommitSeq(100)).unwrap().unwrap();
    assert_eq!(frame.rows().len(), 2);
    assert!(
        frame
            .rows()
            .iter()
            .all(|(_, weight)| weight.to_i128().is_none())
    );
    assert!(frame.rows().weight(&vec![QueryValue::Integer(3)]).unwrap() > &ZWeight::ZERO);
    assert!(frame.rows().weight(&vec![QueryValue::Integer(4)]).unwrap() < &ZWeight::ZERO);
    assert!(Arc::ptr_eq(&frame.shared_rows(), &frame.shared_rows()));
    let mut terminal = History::new(CommitSeq(u64::MAX));
    let result = terminal
        .prepare(
            CommitSeq::ORIGIN,
            batch(0),
            Limits::new(1, 0, 1).unwrap(),
            &mut meter,
        )
        .map(drop);
    assert_eq!(result, Err(StandingQueryFailure::InvalidDelta));
    assert_eq!(terminal.frontier(), CommitSeq(u64::MAX));
    assert!(terminal.next(CommitSeq(u64::MAX)).unwrap().is_none());
}

#[test]
fn preparation_work_does_not_scan_unaffected_retained_prefix() {
    let limits = Limits::new(1000, 1000, 10000).unwrap();
    let mut costs = Vec::new();
    for count in [8, 128] {
        let mut history = History::new(CommitSeq::ORIGIN);
        for _ in 0..count {
            append(&mut history, 1, limits).unwrap();
        }
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        drop(
            history
                .prepare(CommitSeq(count + 1), batch(1), limits, &mut meter)
                .unwrap(),
        );
        costs.push((meter.stats.work_units, meter.stats.scratch_entries));
    }
    assert_eq!(costs[0], costs[1]);
}

fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, value) in [(1, 3), (2, 3), (3, 7)] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    batch
}
fn result_bag(result: QueryResult) -> ZSet<Vec<QueryValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("query rows required");
    };
    ZSet::from_updates(
        rows.into_iter().map(|row| (row, ZWeight::ONE)),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

#[test]
fn commit_hook_retains_every_final_output_tick_without_polling_or_graph_replay() {
    let ((), report) = run_async_under_lab(0x6dde_61, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let texts = [
            "MATCH (n) RETURN n.p AS p",
            "MATCH (n) RETURN DISTINCT n.p AS p",
            "MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 2",
            "MATCH (n) RETURN COUNT(*) AS c,SUM(n.p) AS s,AVG(n.p) AS a",
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            "MATCH (n) WITH n.p AS p RETURN p AS key,COUNT(*) AS c GROUP BY p ORDER BY key LIMIT 2",
            "RETURN 7 AS fixed",
        ];
        let mut views = Vec::new();
        let mut logs = Vec::new();
        let mut bags = Vec::new();
        for text in texts {
            let view = db
                .register_standing_native(&cx, text, &params, resolve, policy())
                .unwrap();
            bags.push(db.standing_native_bag(&cx, &view, policy()).unwrap().1);
            let replay_policy = GqlQueryPolicy::new(0, 100_000, 10_000_000, 10_000_000);
            logs.push(
                db.register_standing_replay(&cx, &view, 16, 1000, 100000, replay_policy)
                    .unwrap(),
            );
            views.push(view);
        }
        let mut cuts = Vec::new();
        let mut expected = Vec::new();
        for tick in 0..3 {
            let mut batch = WriteBatch::new(RelationId(1));
            if tick == 0 {
                batch.delete_vertex(VId(1));
            } else if tick == 1 {
                batch.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(2)));
            } else {
                batch.set_vertex_property(VId(2), PropertyKeyId(99), Some(CanonicalScalar::Int(1)));
            }
            cuts.push(db.write(&commit, batch).await.unwrap());
            expected.push(
                texts
                    .iter()
                    .map(|text| {
                        result_bag(db.query(&cx, text, &params, resolve, policy()).unwrap())
                    })
                    .collect::<Vec<_>>(),
            );
        }
        // First replay access is AFTER all writes. Each original tick survives.
        for (index, log) in logs.iter().enumerate() {
            let mut after = start;
            for (tick, at) in cuts.iter().enumerate() {
                let frame = db
                    .standing_replay_next(&cx, log, after, policy())
                    .unwrap()
                    .unwrap();
                assert_eq!(frame.from(), after);
                assert_eq!(frame.frontier(), *at);
                assert!(Arc::ptr_eq(
                    &frame,
                    &db.standing_replay_next(&cx, log, after, policy())
                        .unwrap()
                        .unwrap()
                ));
                if tick == 2 || index == texts.len() - 1 {
                    assert!(frame.rows().is_empty());
                }
                bags[index]
                    .integrate(frame.rows(), LIMBS, &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(bags[index], expected[tick][index]);
                after = *at;
            }
            assert!(
                db.standing_replay_next(&cx, log, after, policy())
                    .unwrap()
                    .is_none()
            );
            let window = db.standing_replay_window(&cx, log).unwrap();
            assert_eq!(window.retained_after, start);
            assert_eq!(window.frontier, *cuts.last().unwrap());
            assert_eq!(window.ticks, 3);
            assert_eq!(
                bags[index],
                db.standing_native_bag(&cx, &views[index], policy())
                    .unwrap()
                    .1
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sink_overflow_isolated_from_durable_commit_and_rebuild_cannot_fabricate_history() {
    let ((), report) = run_async_under_lab(0x6dde_62, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let view = db
            .register_standing_native(&cx, "MATCH (n) RETURN n.p AS p", &params, resolve, policy())
            .unwrap();
        let bounded = db
            .register_standing_replay(&cx, &view, 2, 0, 10, policy())
            .unwrap();
        let healthy = db
            .register_standing_replay(&cx, &view, 2, 100, 10000, policy())
            .unwrap();
        let count = db.standing_queries.len();
        assert!(matches!(
            db.register_standing_replay(&cx, &view, 0, 1, 1, policy()),
            Err(StandingQueryError::InvalidReplayLimits)
        ));
        assert_eq!(db.standing_queries.len(), count);
        let mut update = WriteBatch::new(RelationId(1));
        update.delete_vertex(VId(1));
        let at = db.write(&commit, update).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(db.standing_native_bag(&cx, &view, policy()).is_ok());
        assert!(
            matches!(db.standing_replay_next(&cx, &bounded, start, policy()),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget }) if frontier == start)
        );
        assert!(
            db.standing_replay_next(&cx, &healthy, start, policy())
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            db.standing_replay_next(&cx, &healthy, start, GqlQueryPolicy::new(0, 0, 100, 100)),
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert!(
            db.standing_replay_next(&cx, &healthy, start, policy())
                .unwrap()
                .is_some()
        );
        db.rebuild_standing_query(&cx, &bounded, policy()).unwrap();
        assert!(
            matches!(db.standing_replay_next(&cx, &bounded, start, policy()),
            Err(StandingQueryError::ReplayGap { retained_after, .. }) if retained_after == at)
        );
        assert!(
            db.standing_replay_next(&cx, &bounded, at, policy())
                .unwrap()
                .is_none()
        );
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.standing_replay_next(&cx, &healthy, start, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
