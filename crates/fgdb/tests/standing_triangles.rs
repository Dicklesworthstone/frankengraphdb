//! Real committed writes and independently recomputed unordered triangle bags.
//! No test graph substitutes for the database/Chronicle/Strata maintenance path.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, StandingQueryHandle,
    WriteBatch,
};
use fgdb_delta_types::zset::triangles::TriangleQuantifier;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::GqlQueryPolicy;
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::collections::BTreeMap;

type Triple = (VId, VId, VId);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn bounded(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, rows, 10_000_000, 10_000_000)
}

fn oracle(
    db: &Database<MemVfs>,
    relation: RelationId,
    quantifier: TriangleQuantifier,
) -> BTreeMap<Triple, i128> {
    let mut edges = BTreeMap::new();
    for row in db.edges().unwrap() {
        let e = row.entry;
        if e.relation != relation {
            continue;
        }
        let pair = if e.src < e.dst {
            (e.src, e.dst)
        } else {
            (e.dst, e.src)
        };
        *edges.entry(pair).or_insert(0i128) += 1;
    }
    let mut vertices: Vec<_> = db
        .vertices()
        .unwrap()
        .into_iter()
        .map(|row| row.vid)
        .collect();
    vertices.sort();
    let mut out = BTreeMap::new();
    for (i, &a) in vertices.iter().enumerate() {
        for (j, &b) in vertices.iter().enumerate().skip(i + 1) {
            for &c in vertices.iter().skip(j + 1) {
                let count = edges.get(&(a, b)).copied().unwrap_or(0)
                    * edges.get(&(a, c)).copied().unwrap_or(0)
                    * edges.get(&(b, c)).copied().unwrap_or(0);
                if count != 0 {
                    out.insert(
                        (a, b, c),
                        if quantifier == TriangleQuantifier::All {
                            count
                        } else {
                            1
                        },
                    );
                }
            }
        }
    }
    out
}
fn check(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    handle: &StandingQueryHandle,
    relation: RelationId,
    quantifier: TriangleQuantifier,
) {
    let expected = oracle(db, relation, quantifier);
    let view = db.standing_triangles(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert!(view.ordered_rows().is_none());
    let actual: BTreeMap<_, _> = view
        .rows()
        .iter()
        .map(|(key, weight)| {
            assert!(key.0 < key.1 && key.1 < key.2);
            (*key, weight.to_i128().unwrap())
        })
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(
        db.standing_triangle_total(cx, handle).unwrap().to_i128(),
        Some(expected.values().sum::<i128>())
    );
}

#[test]
fn ordinary_commits_maintain_relation_scoped_parallel_triangles_and_cascades() {
    let ((), report) = run_async_under_lab(0x7472_6901, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut handles = Vec::new();
        for relation in [R, S] {
            for q in [TriangleQuantifier::Distinct, TriangleQuantifier::All] {
                handles.push((
                    db.register_standing_triangles(&cx, relation, q, policy())
                        .unwrap(),
                    relation,
                    q,
                ));
            }
        }
        let reach = db.register_standing_reachability(&cx, R, policy()).unwrap();
        let verify = |db: &Database<MemVfs>| {
            for (handle, relation, q) in &handles {
                check(db, &cx, handle, *relation, *q);
            }
            assert_eq!(
                db.standing_reachability(&cx, &reach).unwrap().frontier(),
                db.frontier().unwrap()
            );
        };
        verify(&db);
        let wide = 1u128 << 100;
        let mut seed = WriteBatch::new(R);
        for id in [0, 1, 2, 3, wide] {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let mut a = WriteBatch::new(R);
        for (id, from, to) in [(1, 0, 1), (2, 1, 0), (3, 1, wide), (4, wide, 0), (5, 2, 2)] {
            a.add_edge(EId(id), VId(from), VId(to), vec![]);
        }
        let mut b = WriteBatch::new(S);
        for (id, from, to) in [(10, 0, 2), (11, 2, 3), (12, 3, 0)] {
            b.add_edge(EId(id), VId(from), VId(to), vec![]);
        }
        db.write_atomic(&commit, vec![b, a]).await.unwrap();
        verify(&db);
        assert_eq!(
            db.standing_triangle_total(&cx, &handles[1].0)
                .unwrap()
                .to_i128(),
            Some(2)
        );
        let mut mixed = WriteBatch::new(R);
        mixed.delete_edge(EId(1));
        mixed.add_edge(EId(6), VId(wide), VId(1), vec![]);
        mixed.add_edge(EId(7), VId(0), VId(wide), vec![]);
        db.write(&commit, mixed).await.unwrap();
        verify(&db);
        assert_eq!(
            db.standing_triangle_total(&cx, &handles[1].0)
                .unwrap()
                .to_i128(),
            Some(4)
        );
        let mut property = WriteBatch::new(R);
        property.set_vertex_property(VId(0), PropertyKeyId(1), Some(CanonicalScalar::Int(42)));
        db.write(&commit, property).await.unwrap();
        verify(&db);
        let basis = db.frontier().unwrap();
        let mut invalid = WriteBatch::new(R);
        invalid.add_edge(EId(99), VId(999), VId(0), vec![]);
        assert!(db.write(&commit, invalid).await.is_err());
        assert_eq!(db.frontier().unwrap(), basis);
        verify(&db);
        let mut cascade = WriteBatch::new(S);
        cascade.delete_vertex(VId(0));
        db.write(&commit, cascade).await.unwrap();
        verify(&db);
        for (handle, _, _) in &handles {
            assert!(
                db.standing_triangles(&cx, handle)
                    .unwrap()
                    .rows()
                    .is_empty()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_occurrences_are_bounded_and_failed_views_rebuild_from_current_not_historical_state() {
    let ((), report) = run_async_under_lab(0x7472_6902, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=3 {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        for (id, a, b) in [(1, 1, 2), (2, 2, 3), (3, 3, 1)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        let basis = db.write(&commit, seed).await.unwrap();
        let all = db
            .register_standing_triangles(&cx, R, TriangleQuantifier::All, bounded(2))
            .unwrap();
        let distinct = db
            .register_standing_triangles(&cx, R, TriangleQuantifier::Distinct, bounded(1))
            .unwrap();
        let mut more = WriteBatch::new(R);
        more.add_edge(EId(4), VId(2), VId(1), vec![]);
        more.add_edge(EId(5), VId(3), VId(2), vec![]);
        let accepted = db.write(&commit, more).await.unwrap();
        assert!(accepted > basis);
        assert_eq!(
            oracle(&db, R, TriangleQuantifier::All)
                .values()
                .sum::<i128>(),
            4
        );
        assert!(
            matches!(db.standing_triangles(&cx, &all), Err(StandingQueryError::Unavailable {
            frontier, reason: StandingQueryFailure::ResultBudget }) if frontier == basis)
        );
        assert!(matches!(
            db.standing_triangle_total(&cx, &all),
            Err(StandingQueryError::Unavailable { .. })
        ));
        check(&db, &cx, &distinct, R, TriangleQuantifier::Distinct);
        assert!(matches!(
            db.rebuild_standing_query(&cx, &all, bounded(2)),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        let mut less = WriteBatch::new(R);
        less.delete_edge(EId(4));
        less.delete_edge(EId(5));
        let current = db.write(&commit, less).await.unwrap();
        assert!(
            matches!(db.standing_triangles(&cx, &all), Err(StandingQueryError::Unavailable {
            frontier, .. }) if frontier == basis)
        );
        assert_eq!(
            db.rebuild_standing_query(&cx, &all, bounded(1)).unwrap(),
            current
        );
        check(&db, &cx, &all, R, TriangleQuantifier::All);
        let late = db
            .register_standing_triangles(&cx, R, TriangleQuantifier::All, bounded(1))
            .unwrap();
        check(&db, &cx, &late, R, TriangleQuantifier::All);
        assert_eq!(
            db.standing_triangles(&cx, &late)
                .unwrap()
                .last_maintenance()
                .delta_rows,
            0
        );
        let before = *db.standing_triangles(&cx, &all).unwrap().last_maintenance();
        assert!(matches!(
            db.rebuild_standing_query(&cx, &all, GqlQueryPolicy::new(0, 1, 100_000, 100_000)),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::SnapshotBudget
            ))
        ));
        check(&db, &cx, &all, R, TriangleQuantifier::All);
        assert_eq!(
            db.standing_triangles(&cx, &all).unwrap().last_maintenance(),
            &before
        );
        let mut delete = WriteBatch::new(R);
        delete.delete_edge(EId(1));
        db.write(&commit, delete).await.unwrap();
        for handle in [&all, &late] {
            check(&db, &cx, handle, R, TriangleQuantifier::All);
        }
        check(&db, &cx, &distinct, R, TriangleQuantifier::Distinct);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn final_cardinality_swaps_do_not_charge_transient_insertion_prefixes() {
    let ((), report) = run_async_under_lab(0x7472_6903, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in [1, 2, 3, 9, 10, 11] {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        for (id, a, b) in [(1, 9, 10), (2, 10, 11), (3, 11, 9)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let view = db
            .register_standing_triangles(&cx, R, TriangleQuantifier::All, bounded(1))
            .unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(1));
        for (id, a, b) in [(4, 1, 2), (5, 2, 3), (6, 3, 1)] {
            change.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, change).await.unwrap();
        check(&db, &cx, &view, R, TriangleQuantifier::All);
        assert_eq!(
            db.standing_triangles(&cx, &view)
                .unwrap()
                .rows()
                .iter()
                .next()
                .unwrap()
                .0,
            &(VId(1), VId(2), VId(3))
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_four_vertex_simple_graphs_match_independent_recomputation_after_real_commits() {
    let ((), report) = run_async_under_lab(0x7472_6904, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let ids = [0, 1, 2, 1u128 << 100];
        let mut seed = WriteBatch::new(R);
        for id in ids {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let all = db
            .register_standing_triangles(&cx, R, TriangleQuantifier::All, policy())
            .unwrap();
        let distinct = db
            .register_standing_triangles(&cx, R, TriangleQuantifier::Distinct, policy())
            .unwrap();
        let sides = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];
        let mut live = [None; 6];
        let mut next = 1u128;
        for mask in 0..64usize {
            let mut change = WriteBatch::new(R);
            let mut changed = false;
            for (bit, (a, b)) in sides.iter().copied().enumerate() {
                let wanted = mask & (1 << bit) != 0;
                if wanted && live[bit].is_none() {
                    let (a, b) = if (mask + bit) % 2 == 0 {
                        (a, b)
                    } else {
                        (b, a)
                    };
                    let eid = EId(next);
                    next += 1;
                    change.add_edge(eid, VId(ids[a]), VId(ids[b]), vec![]);
                    live[bit] = Some(eid);
                    changed = true;
                } else if !wanted && let Some(eid) = live[bit].take() {
                    change.delete_edge(eid);
                    changed = true;
                }
            }
            if changed {
                db.write(&commit, change).await.unwrap();
            }
            check(&db, &cx, &all, R, TriangleQuantifier::All);
            check(&db, &cx, &distinct, R, TriangleQuantifier::Distinct);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn typed_access_and_foreign_handles_refuse_without_disrupting_healthy_views() {
    let ((), report) = run_async_under_lab(0x7472_6905, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let triangle = db
            .register_standing_triangles(&cx, R, TriangleQuantifier::All, policy())
            .unwrap();
        let reach = db.register_standing_reachability(&cx, R, policy()).unwrap();
        assert!(matches!(
            db.standing_query(&cx, &triangle),
            Err(StandingQueryError::Unsupported)
        ));
        assert!(matches!(
            db.standing_rows(&cx, &triangle),
            Err(StandingQueryError::Unsupported)
        ));
        assert!(matches!(
            db.standing_reachability(&cx, &triangle),
            Err(StandingQueryError::Unsupported)
        ));
        assert!(matches!(
            db.standing_triangles(&cx, &reach),
            Err(StandingQueryError::Unsupported)
        ));
        assert!(matches!(
            db.standing_triangle_total(&cx, &reach),
            Err(StandingQueryError::Unsupported)
        ));
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.standing_triangles(&cx, &triangle),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert!(matches!(
            foreign.standing_triangle_total(&cx, &triangle),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert!(matches!(
            foreign.rebuild_standing_query(&cx, &triangle, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert_eq!(
            db.rebuild_standing_query(&cx, &triangle, policy()).unwrap(),
            CommitSeq::ORIGIN
        );
        check(&db, &cx, &triangle, R, TriangleQuantifier::All);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
