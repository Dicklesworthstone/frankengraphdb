//! Decremental closure must run through real commits and the existing standing
//! result owner, not only through an isolated algebra fixture. The oracle reads
//! committed topology independently and recomputes non-reflexive closure.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure,
    StandingQueryHandle, WriteBatch,
};
use fgdb_delta_types::{RelationId, ZWeight};
use fgdb_gql::GqlQueryPolicy;
use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId};
use std::collections::BTreeSet;

const R: RelationId = RelationId(1);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xd1; 32], DatabaseSecurityNamespaceId([0xd2; 32]), [0xd3; 32])
}
fn oracle(db: &Database<MemVfs>) -> BTreeSet<(VId, VId)> {
    let vertices: Vec<_> = db.vertices().unwrap().into_iter().map(|row| row.vid).collect();
    let mut result: BTreeSet<_> = db.edges().unwrap().into_iter()
        .filter(|row| row.entry.relation == R)
        .map(|row| (row.entry.src, row.entry.dst)).collect();
    for &middle in &vertices {
        for &source in &vertices {
            for &target in &vertices {
                if result.contains(&(source, middle)) && result.contains(&(middle, target)) {
                    result.insert((source, target));
                }
            }
        }
    }
    result
}
fn pairs(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle) -> BTreeSet<(VId, VId)> {
    let view = db.standing_reachability(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    view.rows().iter().map(|(pair, weight)| {
        assert_eq!(weight, &ZWeight::ONE);
        *pair
    }).collect()
}

#[test]
fn tail_edge_deletion_uses_sparse_maintenance_and_reinsertions_keep_both_indexes_sound() {
    let ((), report) = run_async_under_lab(0xdec1_7101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        for n in [64_u128, 128] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            for vertex in 0..=n { seed.create_vertex(VId(vertex), vec![], vec![]); }
            for source in 0..n {
                seed.add_edge(EId(1000 + source), VId(source), VId(source + 1), vec![]);
            }
            db.write(&commit, seed).await.unwrap();
            let handle = db.register_standing_reachability(&cx, R, policy()).unwrap();
            let mut cut = WriteBatch::new(R);
            cut.delete_edge(EId(1000 + n - 1));
            let frontier = db.write(&commit, cut).await.unwrap();
            let view = db.standing_reachability(&cx, &handle).unwrap();
            assert_eq!(view.frontier(), frontier);
            let update_work = view.last_maintenance().work_units;
            let update_scratch = view.last_maintenance().scratch_entries;
            assert_eq!(view.rows().len() as u128, n * (n - 1) / 2);
            assert!(view.rows().iter().all(|(&(source, target), weight)| {
                source < target && target.0 < n && weight == &ZWeight::ONE
            }));
            // Fixed logical work bounds, not a wall-clock benchmark. Removing
            // a terminal edge must not reconstruct each long unchanged prefix.
            assert!(u128::from(update_work) < 64 * n + 512, "work={update_work}, n={n}");
            assert!(u128::from(update_scratch) < 32 * n + 512, "scratch={update_scratch}, n={n}");
            let expected: BTreeSet<_> = (0..n).flat_map(|source| {
                (source + 1..n).map(move |target| (VId(source), VId(target)))
            }).collect();
            assert_eq!(pairs(&db, &cx, &handle), expected);
            let rebuilt = db.register_standing_reachability(&cx, R, policy()).unwrap();
            assert_eq!(pairs(&db, &cx, &rebuilt), expected);
            assert!(db.standing_reachability(&cx, &rebuilt).unwrap()
                .last_maintenance().work_units > update_work);
            let mut reconnect = WriteBatch::new(R);
            reconnect.add_edge(EId(9000), VId(n - 1), VId(n), vec![]);
            db.write(&commit, reconnect).await.unwrap();
            let expected: BTreeSet<_> = (0..n).flat_map(|source| {
                (source + 1..=n).map(move |target| (VId(source), VId(target)))
            }).collect();
            assert_eq!(pairs(&db, &cx, &handle), expected);
            assert_eq!(pairs(&db, &cx, &rebuilt), expected);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn last_parallel_support_and_alternate_boundary_paths_control_cycle_retractions() {
    let ((), report) = run_async_under_lab(0xdec1_7102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let original = db.register_standing_reachability(&cx, R, policy()).unwrap();
        let mut seed = WriteBatch::new(R);
        for vertex in 0..4 { seed.create_vertex(VId(vertex), vec![], vec![]); }
        for (eid, source, target) in [(1, 0, 1), (2, 0, 1), (3, 1, 2), (4, 2, 1), (5, 0, 3), (6, 3, 2)] {
            seed.add_edge(EId(eid), VId(source), VId(target), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let rebuilt = db.register_standing_reachability(&cx, R, policy()).unwrap();
        let baseline = oracle(&db);
        for eid in [1, 2, 5, 4] {
            let mut cut = WriteBatch::new(R);
            cut.delete_edge(EId(eid));
            db.write(&commit, cut).await.unwrap();
            let expected = oracle(&db);
            assert_eq!(pairs(&db, &cx, &original), expected);
            assert_eq!(pairs(&db, &cx, &rebuilt), expected);
            if eid <= 2 {
                assert_eq!(expected, baseline, "parallel or alternate path still supplies support");
            } else {
                assert!(!expected.contains(&(VId(0), VId(1))));
                assert!(!expected.contains(&(VId(0), VId(2))));
                assert_eq!(expected.contains(&(VId(1), VId(1))), eid == 5);
                assert_eq!(expected.contains(&(VId(2), VId(2))), eid == 5);
            }
        }
        // A vertex cascade generates several edge retractions in one commit.
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(2));
        db.write(&commit, cascade).await.unwrap();
        assert_eq!(pairs(&db, &cx, &original), oracle(&db));
        assert_eq!(pairs(&db, &cx, &rebuilt), oracle(&db));
        assert!(pairs(&db, &cx, &original).is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_refused_rebuild_preserves_the_accepted_decremental_generation_and_its_policy() {
    let ((), report) = run_async_under_lab(0xdec1_7103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let n = 24_u128;
        let mut seed = WriteBatch::new(R);
        for vertex in 0..=n { seed.create_vertex(VId(vertex), vec![], vec![]); }
        for source in 0..n {
            seed.add_edge(EId(100 + source), VId(source), VId(source + 1), vec![]);
        }
        let basis = db.write(&commit, seed).await.unwrap();
        let healthy = db.register_standing_reachability(&cx, R, policy()).unwrap();
        // Publish a real decremental generation before attempting a rebuild
        // whose policy refuses immediately. The old accepted policy and both
        // maintained topology directions must remain usable afterward.
        let mut cut = WriteBatch::new(R);
        cut.delete_edge(EId(100 + n - 1));
        db.write(&commit, cut).await.unwrap();
        let after = pairs(&db, &cx, &healthy);
        assert_eq!(after, oracle(&db));
        assert!(db.frontier().unwrap() > basis);
        // A rejected rebuild must leave the accepted deletion result and its
        // exact frontier unchanged, even if the successor policy has no work.
        let before_stats = *db.standing_reachability(&cx, &healthy).unwrap().last_maintenance();
        assert!(matches!(db.rebuild_standing_query(&cx, &healthy,
            GqlQueryPolicy::new(100_000, 100_000, 0, 0)),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::WorkBudget))));
        assert_eq!(pairs(&db, &cx, &healthy), after);
        assert_eq!(db.standing_reachability(&cx, &healthy).unwrap().last_maintenance(), &before_stats);
        let mut next = WriteBatch::new(R);
        next.delete_edge(EId(100 + n - 2));
        db.write(&commit, next).await.unwrap();
        assert_eq!(pairs(&db, &cx, &healthy), oracle(&db));
        assert_eq!(db.delta_since(CommitSeq(0)).unwrap().count(), 3);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
