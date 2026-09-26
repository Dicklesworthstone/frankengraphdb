use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn build(snapshot: &crate::Snapshot) -> State {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    State::from_snapshot(snapshot, ComponentRelation::Weak(RelationId(1)), &mut meter).unwrap()
}
fn unchanged(actual: &State, before: &State) {
    assert_eq!(actual.input, before.input);
    assert_eq!(actual.components, before.components);
    assert_eq!(actual.rows, before.rows);
    assert_eq!(actual.frontier, before.frontier);
    assert_eq!(actual.stats, before.stats);
    assert_eq!(actual.policy, before.policy);
    assert_eq!(actual.failure, before.failure);
}

#[test]
fn every_composed_refusal_and_exact_budget_preserves_atomic_publication() {
    let ((), report) = run_async_under_lab(0x6363_6410, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let keys = DatabaseKeys::new(
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
            [0xb3; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for v in 0..5 {
            seed.create_vertex(VId(v), vec![], vec![]);
        }
        for (eid, a, b) in [(1, 0, 1), (2, 1, 2), (3, 3, 4)] {
            seed.add_edge(EId(eid), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let baseline = Arc::clone(&db.snapshot);
        let before = build(&baseline);
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(0));
        change.create_vertex(VId(6), vec![], vec![]);
        change.add_edge(EId(4), VId(2), VId(3), vec![]);
        db.write(&commit, change).await.unwrap();
        let batch = db.delta_since(baseline.frontier).unwrap().next().unwrap();
        let expected = build(&db.snapshot);
        let mut success = build(&baseline);
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
            success.maintain(&commit, batch, &mut meter).unwrap();
            meter.stats
        };
        assert_eq!(success.rows, expected.rows);
        assert_eq!(success.components, expected.components);
        assert_eq!(success.input, expected.input);
        assert!(calls > 0 && stats.work_units > 0 && stats.scratch_entries > 0);
        for stop in 1..=calls {
            let mut state = build(&baseline);
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
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                assert_eq!(
                    state.maintain(&commit, batch, &mut meter),
                    Err(StandingQueryFailure::Interrupted)
                );
            }
            assert_eq!(seen, stop);
            unchanged(&state, &before);
        }
        for (work, scratch, rows, refusal) in [
            (stats.work_units, stats.scratch_entries, 5, None),
            (
                stats.work_units - 1,
                stats.scratch_entries,
                5,
                Some(StandingQueryFailure::WorkBudget),
            ),
            (
                stats.work_units,
                stats.scratch_entries - 1,
                5,
                Some(StandingQueryFailure::ScratchBudget),
            ),
            (
                stats.work_units,
                stats.scratch_entries,
                4,
                Some(StandingQueryFailure::ResultBudget),
            ),
        ] {
            let mut state = build(&baseline);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(100_000, rows, work, scratch),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let result = state.maintain(&commit, batch, &mut meter);
            if let Some(reason) = refusal {
                assert_eq!(result, Err(reason));
                unchanged(&state, &before);
            } else {
                result.unwrap();
                assert_eq!(state.rows, expected.rows);
                assert_eq!(state.components, expected.components);
            }
        }
        // Shared bootstrap admits the COMBINED physical source size. A budget
        // sufficient for edges alone must not hide vertex history or isolates.
        let physical = db.snapshot.blocks.iter().map(|b| b.len()).sum::<usize>()
            + db.snapshot.patches.iter().map(|p| p.len()).sum::<usize>();
        assert!(physical > 0);
        for records in [physical as u64, physical as u64 - 1] {
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(records, 5, 10_000_000, 10_000_000),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let result = State::from_snapshot(
                &db.snapshot,
                ComponentRelation::Weak(RelationId(1)),
                &mut meter,
            );
            if records == physical as u64 {
                assert_eq!(result.unwrap().rows, expected.rows);
            } else {
                assert!(matches!(result, Err(StandingQueryFailure::SnapshotBudget)));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
