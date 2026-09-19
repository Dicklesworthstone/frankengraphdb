//! Public automatic recursive maintenance, checked against independent closure
//! recomputation from ordinary database reads after real committed writes.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, StandingQueryHandle,
    WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};
use fgdb_gql::{GqlQueryPolicy, GraphAggregate, GraphAggregateValue, PreparedGraphAggregate};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::collections::BTreeSet;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}

// Independent Floyd-Warshall over source reads, not the maintained indexes.
// Do not seed reflexivity: self pairs require a nonempty cycle.
fn oracle(db: &Database<MemVfs>, relation: RelationId) -> BTreeSet<(VId, VId)> {
    let vertices: Vec<_> = db
        .vertices()
        .unwrap()
        .into_iter()
        .map(|row| row.vid)
        .collect();
    let mut pairs: BTreeSet<_> = db
        .edges()
        .unwrap()
        .into_iter()
        .filter(|row| row.entry.relation == relation)
        .map(|row| (row.entry.src, row.entry.dst))
        .collect();
    for &middle in &vertices {
        for &source in &vertices {
            for &destination in &vertices {
                if pairs.contains(&(source, middle)) && pairs.contains(&(middle, destination)) {
                    pairs.insert((source, destination));
                }
            }
        }
    }
    pairs
}
fn check(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle, relation: RelationId) {
    let view = db.standing_reachability(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert_eq!(
        view.rows()
            .iter()
            .map(|(pair, weight)| {
                assert_eq!(weight, &ZWeight::ONE);
                *pair
            })
            .collect::<BTreeSet<_>>(),
        oracle(db, relation)
    );
}
fn count_definition() -> PreparedGraphAggregate {
    let mut graph = GraphPatternBuilder::new();
    graph.vertex("n").unwrap();
    let input = graph
        .prepare_values(&[GraphColumn::vertex("n", "n")], 0, None)
        .unwrap()
        .with_duplicates();
    PreparedGraphAggregate::prepare(input, &[], &[GraphAggregate::count_rows("count")], 0, None)
        .unwrap()
}
fn check_count(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle) {
    let view = db.standing_query(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert_eq!(view.rows().len(), 1);
    let (row, weight) = view.rows().iter().next().unwrap();
    assert_eq!(weight, &ZWeight::ONE);
    assert_eq!(
        row.values(),
        &[GraphAggregateValue::Count(
            db.vertices().unwrap().len() as u64
        )]
    );
}

#[test]
fn commits_automatically_maintain_cycles_parallel_edges_cascades_and_native_siblings() {
    let ((), report) = run_async_under_lab(0x6a72, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let r = db
            .register_standing_reachability(&query, R, policy())
            .unwrap();
        let s = db
            .register_standing_reachability(&query, S, policy())
            .unwrap();
        let count = db
            .register_standing_query(&query, count_definition(), policy())
            .unwrap();
        let wide = 1_u128 << 100;
        let verify = |db: &Database<MemVfs>| {
            check(db, &query, &r, R);
            check(db, &query, &s, S);
            check_count(db, &query, &count);
        };
        verify(&db);
        let mut seed = WriteBatch::new(R);
        for id in [0, 1, 2, 3, wide] {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        verify(&db);
        let mut a = WriteBatch::new(R);
        for (eid, from, to) in [
            (10, 0, 1),
            (11, 0, 1),
            (12, 1, 2),
            (13, 2, wide),
            (14, 3, 3),
        ] {
            a.add_edge(EId(eid), VId(from), VId(to), vec![]);
        }
        let mut b = WriteBatch::new(S);
        b.add_edge(EId(20), VId(2), VId(3), vec![]);
        b.add_edge(EId(21), VId(3), VId(wide), vec![]);
        db.write_atomic(&commit, vec![b, a]).await.unwrap();
        verify(&db);
        let mut a = WriteBatch::new(R);
        a.add_edge(EId(15), VId(wide), VId(0), vec![]);
        let mut b = WriteBatch::new(S);
        b.add_edge(EId(22), VId(wide), VId(2), vec![]);
        db.write_atomic(&commit, vec![a, b]).await.unwrap();
        verify(&db);
        let mut props = WriteBatch::new(R);
        props.set_vertex_property(VId(0), PropertyKeyId(99), Some(CanonicalScalar::Int(7)));
        db.write(&commit, props).await.unwrap();
        verify(&db);
        assert!(
            db.standing_reachability(&query, &r)
                .unwrap()
                .last_maintenance()
                .work_units
                < 500
        );
        let mut partial = WriteBatch::new(R);
        partial.delete_edge(EId(10));
        db.write(&commit, partial).await.unwrap();
        verify(&db);
        assert!(
            db.standing_reachability(&query, &r)
                .unwrap()
                .rows()
                .weight(&(VId(0), VId(0)))
                .is_some()
        );
        let mut a = WriteBatch::new(R);
        a.delete_edge(EId(11));
        let mut b = WriteBatch::new(S);
        b.delete_edge(EId(21));
        db.write_atomic(&commit, vec![a, b]).await.unwrap();
        verify(&db);
        let before = db.frontier().unwrap();
        let mut invalid = WriteBatch::new(R);
        invalid.add_edge(EId(99), VId(999), VId(0), vec![]);
        assert!(db.write(&commit, invalid).await.is_err());
        assert_eq!(db.frontier().unwrap(), before);
        verify(&db);
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(2));
        db.write(&commit, cascade).await.unwrap();
        verify(&db);
        let late = db
            .register_standing_reachability(&query, R, policy())
            .unwrap();
        check(&db, &query, &late, R);
        db.rebuild_standing_query(&query, &r, policy()).unwrap();
        db.rebuild_standing_query(&query, &count, policy()).unwrap();
        verify(&db);
        let mut empty = WriteBatch::new(R);
        for id in [0, 1, 3, wide] {
            empty.delete_vertex(VId(id));
        }
        db.write(&commit, empty).await.unwrap();
        verify(&db);
        check(&db, &query, &late, R);
        assert!(
            db.standing_reachability(&query, &r)
                .unwrap()
                .rows()
                .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_views_do_not_abort_durable_writes_and_rebuild_uses_final_not_historical_size() {
    let ((), report) = run_async_under_lab(0x6a73, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let limited = GqlQueryPolicy::new(100_000, 3, 10_000_000, 10_000_000);
        let r = db
            .register_standing_reachability(&query, R, limited)
            .unwrap();
        let s = db
            .register_standing_reachability(&query, S, policy())
            .unwrap();
        let count = db
            .register_standing_query(&query, count_definition(), policy())
            .unwrap();
        let mut first = WriteBatch::new(R);
        for id in 1..=3 {
            first.create_vertex(VId(id), vec![], vec![]);
        }
        first.add_edge(EId(1), VId(1), VId(2), vec![]);
        first.add_edge(EId(2), VId(2), VId(3), vec![]);
        let basis = db.write(&commit, first).await.unwrap();
        check(&db, &query, &r, R);
        let mut cycle = WriteBatch::new(R);
        cycle.add_edge(EId(3), VId(3), VId(1), vec![]);
        db.write(&commit, cycle).await.unwrap();
        assert_eq!(oracle(&db, R).len(), 9);
        assert!(matches!(db.standing_reachability(&query, &r),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget }) if frontier == basis));
        check(&db, &query, &s, S);
        check_count(&db, &query, &count);
        assert!(matches!(
            db.rebuild_standing_query(&query, &r, limited),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        let mut break_cycle = WriteBatch::new(R);
        break_cycle.delete_edge(EId(1));
        let current = db.write(&commit, break_cycle).await.unwrap();
        assert!(matches!(db.standing_reachability(&query, &r),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget }) if frontier == basis));
        assert_eq!(
            db.rebuild_standing_query(&query, &r, limited).unwrap(),
            current
        );
        check(&db, &query, &r, R);
        let mut suffix = WriteBatch::new(R);
        suffix.delete_edge(EId(3));
        db.write(&commit, suffix).await.unwrap();
        check(&db, &query, &r, R);
        check(&db, &query, &s, S);
        check_count(&db, &query, &count);
        let one = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let late = db.register_standing_reachability(&query, R, one).unwrap();
        check(&db, &query, &late, R);
        let before = *db
            .standing_reachability(&query, &r)
            .unwrap()
            .last_maintenance();
        assert!(matches!(
            db.rebuild_standing_query(
                &query,
                &r,
                GqlQueryPolicy::new(0, 100_000, 10_000_000, 10_000_000)
            ),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::SnapshotBudget
            ))
        ));
        check(&db, &query, &r, R);
        assert_eq!(
            db.standing_reachability(&query, &r)
                .unwrap()
                .last_maintenance(),
            &before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn typed_access_ownership_work_refusal_and_final_cardinality_swaps_are_enforced() {
    let ((), report) = run_async_under_lab(0x6a74, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let probe = db
            .register_standing_reachability(&query, R, policy())
            .unwrap();
        let init_work = db
            .standing_reachability(&query, &probe)
            .unwrap()
            .last_maintenance()
            .work_units;
        let tiny = db
            .register_standing_reachability(
                &query,
                R,
                GqlQueryPolicy::new(100_000, 100_000, init_work, 100_000),
            )
            .unwrap();
        let one = db
            .register_standing_reachability(
                &query,
                R,
                GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000),
            )
            .unwrap();
        let native = db
            .register_standing_query(&query, count_definition(), policy())
            .unwrap();
        assert!(matches!(
            db.standing_query(&query, &one),
            Err(StandingQueryError::Unsupported)
        ));
        assert!(matches!(
            db.standing_reachability(&query, &native),
            Err(StandingQueryError::Unsupported)
        ));
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.standing_reachability(&query, &one),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert!(matches!(
            foreign.rebuild_standing_query(&query, &one, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
        let mut first = WriteBatch::new(R);
        for id in [1, 2, 9, 10] {
            first.create_vertex(VId(id), vec![], vec![]);
        }
        first.add_edge(EId(1), VId(9), VId(10), vec![]);
        db.write(&commit, first).await.unwrap();
        assert!(matches!(
            db.standing_reachability(&query, &tiny),
            Err(StandingQueryError::Unavailable {
                frontier: CommitSeq::ORIGIN,
                reason: StandingQueryFailure::WorkBudget
            })
        ));
        check(&db, &query, &one, R);
        check_count(&db, &query, &native);
        let mut swap = WriteBatch::new(R);
        swap.delete_edge(EId(1));
        swap.add_edge(EId(2), VId(1), VId(2), vec![]);
        db.write(&commit, swap).await.unwrap();
        // The insertion sorts BEFORE the retraction; only the final size is 1.
        check(&db, &query, &one, R);
        assert_eq!(
            db.standing_reachability(&query, &one).unwrap().rows().len(),
            1
        );
        db.rebuild_standing_query(&query, &tiny, policy()).unwrap();
        check(&db, &query, &tiny, R);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sparse_live_maintenance_does_not_rescan_unrelated_components() {
    let ((), report) = run_async_under_lab(0x6a75, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut measurements = Vec::new();
        for components in [1_u128, 1_000] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let handle = db
                .register_standing_reachability(&query, R, policy())
                .unwrap();
            let mut seed = WriteBatch::new(R);
            for id in 0..components * 2 {
                seed.create_vertex(VId(id), vec![], vec![]);
            }
            for id in 0..components {
                seed.add_edge(EId(id + 1), VId(2 * id), VId(2 * id + 1), vec![]);
            }
            db.write(&commit, seed).await.unwrap();
            let mut close = WriteBatch::new(R);
            close.add_edge(EId(components + 1), VId(1), VId(0), vec![]);
            db.write(&commit, close).await.unwrap();
            let view = db.standing_reachability(&query, &handle).unwrap();
            measurements.push(*view.last_maintenance());
            assert_eq!(view.frontier(), db.frontier().unwrap());
            assert_eq!(view.rows().len(), components as usize + 3);
            for pair in [(VId(0), VId(0)), (VId(1), VId(0)), (VId(1), VId(1))] {
                assert_eq!(view.rows().weight(&pair), Some(&ZWeight::ONE));
            }
        }
        assert_eq!(measurements[0], measurements[1]);
        assert_eq!(measurements[1].delta_rows, 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_registration_does_not_replay_unrelated_property_history() {
    let ((), report) = run_async_under_lab(0x6a83, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut measurements = Vec::new();
        for property_commits in [0, 128] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut first = WriteBatch::new(R);
            for id in 1..=3 {
                first.create_vertex(VId(id), vec![], vec![]);
            }
            first.add_edge(EId(1), VId(1), VId(2), vec![]);
            first.add_edge(EId(2), VId(2), VId(3), vec![]);
            db.write(&commit, first).await.unwrap();
            for value in 0..property_commits {
                let mut edit = WriteBatch::new(R);
                edit.set_vertex_property(
                    VId(2),
                    PropertyKeyId(99),
                    Some(CanonicalScalar::Int(value)),
                );
                db.write(&commit, edit).await.unwrap();
            }
            // Two physical edge records suffice despite many vertex versions
            // and historical committed rows. All three closure pairs survive.
            let bounded = GqlQueryPolicy::new(2, 3, 10_000_000, 10_000_000);
            let handle = db
                .register_standing_reachability(&query, R, bounded)
                .unwrap();
            check(&db, &query, &handle, R);
            let stats = *db
                .standing_reachability(&query, &handle)
                .unwrap()
                .last_maintenance();
            assert_eq!(stats.delta_rows, 0);
            measurements.push(stats);
            db.rebuild_standing_query(&query, &handle, bounded).unwrap();
            assert_eq!(
                *db.standing_reachability(&query, &handle)
                    .unwrap()
                    .last_maintenance(),
                stats
            );
            let mut last = WriteBatch::new(R);
            last.delete_edge(EId(1));
            db.write(&commit, last).await.unwrap();
            check(&db, &query, &handle, R);
            assert_eq!(
                db.standing_reachability(&query, &handle)
                    .unwrap()
                    .rows()
                    .len(),
                1
            );
        }
        assert_eq!(measurements[0], measurements[1]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
