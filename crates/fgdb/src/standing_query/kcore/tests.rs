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
    State::from_snapshot(snapshot, RelationId(1), &mut meter).unwrap()
}
fn unchanged(actual: &State, before: &State) {
    assert_eq!(actual.input, before.input);
    assert_eq!(actual.cores, before.cores);
    assert_eq!(actual.rows, before.rows);
    assert_eq!(actual.frontier, before.frontier);
    assert_eq!(actual.stats, before.stats);
    assert_eq!(actual.failure, before.failure);
    assert_eq!(actual.policy, before.policy);
}

#[test]
fn all_composed_refusals_and_exact_quota_boundaries_leave_accepted_state_intact() {
    let ((), report) = run_async_under_lab(0x6b63_0110, |root| async move {
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
        let before = build(&baseline);
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_edge(EId(1));
        db.write(&commit, change).await.unwrap();
        let batch = db.delta_since(baseline.frontier).unwrap().next().unwrap();
        let expected = build(&db.snapshot);
        assert_ne!(before.rows, expected.rows);
        let mut state = build(&baseline);
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
        assert_eq!(state.cores, expected.cores);
        assert_eq!(state.input, expected.input);
        for stop in 1..=calls {
            let mut state = build(&baseline);
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
            let mut state = build(&baseline);
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
            let result = State::from_snapshot(&baseline, RelationId(1), &mut meter);
            if allowance == records as u64 {
                assert_eq!(result.unwrap().rows, before.rows);
            } else {
                assert!(matches!(result, Err(StandingQueryFailure::SnapshotBudget)));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
