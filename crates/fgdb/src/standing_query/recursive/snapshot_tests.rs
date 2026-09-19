//! Bootstrap/rebuild from real, authenticated database generations. Retirement
//! mutates only the derived in-process window in these private tests; it does
//! not authorize deleting Chronicle or immutable Strata objects.
use super::*;
use crate::{DatabaseKeys, MemVfs, StandingQueryHandle, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LocalDeltaBatchIndex, ZWeight};
use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::collections::BTreeSet;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}
fn retire(db: &mut Database<MemVfs>) {
    let at = db.frontier().unwrap();
    std::sync::Arc::make_mut(&mut db.snapshot)
        .delta_index
        .retire_prefix(at)
        .unwrap();
    assert!(db.delta_index().unwrap().is_empty());
}
fn oracle(db: &Database<MemVfs>, relation: RelationId) -> BTreeSet<Pair> {
    let vertices = db.vertices().unwrap();
    let mut pairs: BTreeSet<_> = db
        .edges()
        .unwrap()
        .iter()
        .filter(|row| row.entry.relation == relation)
        .map(|row| (row.entry.src, row.entry.dst))
        .collect();
    for middle in &vertices {
        for source in &vertices {
            for destination in &vertices {
                if pairs.contains(&(source.vid, middle.vid))
                    && pairs.contains(&(middle.vid, destination.vid))
                {
                    pairs.insert((source.vid, destination.vid));
                }
            }
        }
    }
    pairs
}
fn plain(rows: &ZSet<Pair>) -> BTreeSet<Pair> {
    rows.iter()
        .map(|(pair, weight)| {
            assert_eq!(weight, &ZWeight::ONE);
            *pair
        })
        .collect()
}
fn check(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle, relation: RelationId) {
    let view = db.standing_reachability(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert_eq!(plain(view.rows()), oracle(db, relation));
}

#[test]
fn fully_retired_registration_rebuild_and_live_cascades_use_one_snapshot_cut() {
    let ((), report) = run_async_under_lab(0x6a81, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let limited = GqlQueryPolicy::new(100_000, 3, 10_000_000, 10_000_000);
        let maintained = db
            .register_standing_reachability(&query, R, limited)
            .unwrap();
        let mut first = WriteBatch::new(R);
        for id in 1..=4 {
            first.create_vertex(VId(id), vec![], vec![]);
        }
        for (id, from, to) in [(1, 1, 2), (2, 1, 2), (3, 2, 3)] {
            first.add_edge(EId(id), VId(from), VId(to), vec![]);
        }
        let basis = db.write(&commit, first).await.unwrap();
        let mut other = WriteBatch::new(S);
        other.add_edge(EId(20), VId(2), VId(4), vec![]);
        let mut cycle = WriteBatch::new(R);
        cycle.add_edge(EId(4), VId(3), VId(1), vec![]);
        db.write_atomic(&commit, vec![other, cycle]).await.unwrap();
        retire(&mut db);
        assert!(db.delta_since(CommitSeq::ORIGIN).is_err());
        assert!(matches!(
            db.rebuild_standing_query(&query, &maintained, limited),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert!(matches!(db.standing_reachability(&query, &maintained),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget }) if frontier == basis));
        let late_r = db
            .register_standing_reachability(&query, R, policy())
            .unwrap();
        let late_s = db
            .register_standing_reachability(&query, S, policy())
            .unwrap();
        check(&db, &query, &late_r, R);
        check(&db, &query, &late_s, S);
        assert_eq!(
            db.standing_reachability(&query, &late_r)
                .unwrap()
                .last_maintenance()
                .delta_rows,
            0
        );
        let mut remove_cycle = WriteBatch::new(R);
        remove_cycle.delete_edge(EId(4));
        let current = db.write(&commit, remove_cycle).await.unwrap();
        retire(&mut db);
        assert_eq!(
            db.rebuild_standing_query(&query, &maintained, limited)
                .unwrap(),
            current
        );
        check(&db, &query, &maintained, R);
        let mut partial = WriteBatch::new(R);
        partial.delete_edge(EId(1));
        db.write(&commit, partial).await.unwrap();
        retire(&mut db);
        check(&db, &query, &maintained, R);
        check(&db, &query, &late_r, R);
        assert_eq!(
            db.standing_reachability(&query, &late_r)
                .unwrap()
                .rows()
                .len(),
            3
        );
        let mut cascade = WriteBatch::new(S);
        cascade.delete_vertex(VId(2));
        db.write(&commit, cascade).await.unwrap();
        retire(&mut db);
        for (handle, relation) in [(&maintained, R), (&late_r, R), (&late_s, S)] {
            check(&db, &query, handle, relation);
            db.rebuild_standing_query(&query, handle, policy()).unwrap();
            check(&db, &query, handle, relation);
            assert!(
                db.standing_reachability(&query, handle)
                    .unwrap()
                    .rows()
                    .is_empty()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_snapshot_bootstrap_checkpoint_and_budget_refusal_preserves_the_owner() {
    let ((), report) = run_async_under_lab(0x6a82, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut first = WriteBatch::new(R);
        for id in 1..=3 {
            first.create_vertex(VId(id), vec![], vec![]);
        }
        first.add_edge(EId(1), VId(1), VId(2), vec![]);
        first.add_edge(EId(2), VId(2), VId(3), vec![]);
        db.write(&commit, first).await.unwrap();
        retire(&mut db);
        let handle = db
            .register_standing_reachability(&query, R, policy())
            .unwrap();
        let old_rows = plain(db.standing_reachability(&query, &handle).unwrap().rows());
        let old_stats = *db
            .standing_reachability(&query, &handle)
            .unwrap()
            .last_maintenance();
        let old_frontier = db.frontier().unwrap();
        let index = db.delta_index().unwrap().clone();
        let mut calls = 0;
        let success = {
            let mut checkpoint = || {
                calls += 1;
                Ok(())
            };
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            State::from_snapshot(&db.snapshot, R, &mut meter).unwrap()
        };
        assert_eq!(success.stats, old_stats);
        assert_eq!(success.stats.delta_rows, 0);
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
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                assert!(matches!(
                    State::from_snapshot(&db.snapshot, R, &mut meter),
                    Err(StandingQueryFailure::Interrupted)
                ));
            }
            assert_eq!(seen, stop);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let retried = State::from_snapshot(&db.snapshot, R, &mut meter).unwrap();
            assert_eq!(retried.input, success.input);
            assert_eq!(retried.rows, success.rows);
            assert_eq!(retried.frontier, old_frontier);
            assert_eq!(db.standing_queries.len(), 1);
            assert_eq!(db.delta_index().unwrap(), &index);
            assert_eq!(
                plain(db.standing_reachability(&query, &handle).unwrap().rows()),
                old_rows
            );
            assert_eq!(
                db.standing_reachability(&query, &handle)
                    .unwrap()
                    .last_maintenance(),
                &old_stats
            );
        }
        let records = db
            .snapshot
            .blocks
            .iter()
            .map(|block| block.len() as u64)
            .sum::<u64>();
        for (bounded, wanted) in [
            (
                GqlQueryPolicy::new(records - 1, 100_000, 10_000_000, 10_000_000),
                StandingQueryFailure::SnapshotBudget,
            ),
            (
                GqlQueryPolicy::new(100_000, 100_000, success.stats.work_units - 1, 10_000_000),
                StandingQueryFailure::WorkBudget,
            ),
            (
                GqlQueryPolicy::new(
                    100_000,
                    100_000,
                    10_000_000,
                    success.stats.scratch_entries - 1,
                ),
                StandingQueryFailure::ScratchBudget,
            ),
            (
                GqlQueryPolicy::new(
                    100_000,
                    success.rows.len() as u64 - 1,
                    10_000_000,
                    10_000_000,
                ),
                StandingQueryFailure::ResultBudget,
            ),
        ] {
            assert!(
                matches!(db.rebuild_standing_query(&query, &handle, bounded),
                Err(StandingQueryError::Maintenance(reason)) if reason == wanted)
            );
            assert_eq!(
                plain(db.standing_reachability(&query, &handle).unwrap().rows()),
                old_rows
            );
            assert_eq!(
                db.standing_reachability(&query, &handle)
                    .unwrap()
                    .last_maintenance(),
                &old_stats
            );
            assert_eq!(db.frontier().unwrap(), old_frontier);
        }
        // A decoded floor without retained boundary evidence cannot authorize
        // snapshot bootstrap; even a failed rebuild leaves the prior view intact.
        std::sync::Arc::make_mut(&mut db.snapshot).delta_index =
            LocalDeltaBatchIndex::from_parts_for_test(old_frontier, old_frontier, vec![]);
        assert!(matches!(
            db.rebuild_standing_query(&query, &handle, policy()),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::InvalidDelta
            ))
        ));
        assert_eq!(
            plain(db.standing_reachability(&query, &handle).unwrap().rows()),
            old_rows
        );
        std::sync::Arc::make_mut(&mut db.snapshot).delta_index = index;
        db.rebuild_standing_query(&query, &handle, policy())
            .unwrap();
        check(&db, &query, &handle, R);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
