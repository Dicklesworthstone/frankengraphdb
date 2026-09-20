//! Native committed core-number maintenance versus independent threshold peeling.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure,
    StandingQueryHandle, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::GqlQueryPolicy;
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000) }
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa1; 32], DatabaseSecurityNamespaceId([0xa2; 32]), [0xa3; 32])
}
fn oracle(db: &Database<MemVfs>, relation: RelationId) -> BTreeMap<VId, u64> {
    let mut neighbors: BTreeMap<_, BTreeSet<VId>> = db.vertices().unwrap().into_iter()
        .map(|row| (row.vid, BTreeSet::new())).collect();
    for edge in db.edges().unwrap() {
        let e = edge.entry;
        if e.relation == relation && e.src != e.dst {
            neighbors.get_mut(&e.src).unwrap().insert(e.dst);
            neighbors.get_mut(&e.dst).unwrap().insert(e.src);
        }
    }
    let mut result: BTreeMap<_, _> = neighbors.keys().map(|v| (*v, 0)).collect();
    for k in 1..neighbors.len() {
        let mut alive: BTreeSet<_> = neighbors.keys().copied().collect();
        loop {
            let remove: Vec<_> = alive.iter().copied().filter(|v|
                neighbors[v].iter().filter(|u| alive.contains(*u)).count() < k).collect();
            if remove.is_empty() { break; }
            for v in remove { alive.remove(&v); }
        }
        for v in alive { result.insert(v, k as u64); }
    }
    result
}
fn check(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle, relation: RelationId) {
    let view = db.standing_core_numbers(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert!(view.ordered_rows().is_none());
    let actual: BTreeMap<_, _> = view.rows().iter().map(|(&(v, n), w)| {
        assert_eq!(w, &ZWeight::ONE);
        assert_eq!(db.standing_core_number(cx, handle, v).unwrap(), Some(n));
        (v, n)
    }).collect();
    assert_eq!(actual, oracle(db, relation));
}

#[test]
fn committed_core_changes_preserve_relation_scope_isolates_parallel_edges_and_cascades() {
    let ((), report) = run_async_under_lab(0x6b63_0101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let r = db.register_standing_core_numbers(&cx, R, policy()).unwrap();
        let s = db.register_standing_core_numbers(&cx, S, policy()).unwrap();
        let sibling = db.register_standing_components(&cx, R, policy()).unwrap();
        let domain = [0, 1, 2, 1 << 100, u128::MAX];
        let mut vertices = WriteBatch::new(S);
        for v in domain { vertices.create_vertex(VId(v), vec![], vec![]); }
        db.write(&commit, vertices).await.unwrap();
        check(&db, &cx, &r, R); check(&db, &cx, &s, S);
        assert_eq!(db.standing_core_number(&cx, &r, VId(u128::MAX)).unwrap(), Some(0));
        let mut edges = WriteBatch::new(R);
        let mut eid = 1;
        for i in 0..4 { for j in i+1..4 {
            edges.add_edge(EId(eid), VId(domain[i]), VId(domain[j]), vec![]); eid += 1;
        } }
        edges.add_edge(EId(7), VId(1), VId(0), vec![]);
        edges.add_edge(EId(8), VId(u128::MAX), VId(u128::MAX), vec![]);
        let mut other = WriteBatch::new(S);
        for (id, a, b) in [(20, 0, 1), (21, 1, 2), (22, 2, 0)] {
            other.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write_atomic(&commit, vec![other, edges]).await.unwrap();
        check(&db, &cx, &r, R); check(&db, &cx, &s, S);
        assert_eq!(db.standing_core_number(&cx, &r, VId(0)).unwrap(), Some(3));
        assert_eq!(db.standing_core_number(&cx, &s, VId(0)).unwrap(), Some(2));
        let mut property = WriteBatch::new(R);
        property.set_vertex_property(VId(0), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
        db.write(&commit, property).await.unwrap();
        assert_eq!(db.standing_core_numbers(&cx, &r).unwrap().last_maintenance().affected_vertices, 0);
        let mut partial = WriteBatch::new(R); partial.delete_edge(EId(1));
        db.write(&commit, partial).await.unwrap();
        assert_eq!(db.standing_core_numbers(&cx, &r).unwrap().last_maintenance().affected_vertices, 0);
        assert_eq!(db.standing_core_number(&cx, &r, VId(0)).unwrap(), Some(3));
        let mut final_support = WriteBatch::new(R); final_support.delete_edge(EId(7));
        db.write(&commit, final_support).await.unwrap();
        assert_eq!(db.standing_core_number(&cx, &r, VId(0)).unwrap(), Some(2));
        let mut cascade = WriteBatch::new(S); cascade.delete_vertex(VId(1));
        db.write(&commit, cascade).await.unwrap();
        check(&db, &cx, &r, R); check(&db, &cx, &s, S);
        assert_eq!(db.standing_core_number(&cx, &r, VId(1)).unwrap(), None);
        assert_eq!(db.standing_components(&cx, &sibling).unwrap().frontier(), db.frontier().unwrap());
        let before = db.frontier().unwrap();
        let mut invalid = WriteBatch::new(R); invalid.add_edge(EId(99), VId(12345), VId(0), vec![]);
        assert!(db.write(&commit, invalid).await.is_err());
        assert_eq!(db.frontier().unwrap(), before);
        check(&db, &cx, &r, R);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_four_vertex_graphs_maintain_exact_cores_through_mixed_committed_ticks() {
    let ((), report) = run_async_under_lab(0x6b63_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let h = db.register_standing_core_numbers(&cx, R, policy()).unwrap();
        let mut seed = WriteBatch::new(R);
        for v in 0..4 { seed.create_vertex(VId(v), vec![], vec![]); }
        db.write(&commit, seed).await.unwrap();
        let pairs = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];
        let mut live = BTreeMap::new(); let mut eid = 100;
        for mask in 0..64_u64 {
            let mut change = WriteBatch::new(R);
            for (at, &(a, b)) in pairs.iter().enumerate() {
                let wanted = mask & (1 << at) != 0;
                match (live.get(&at).copied(), wanted) {
                    (Some(id), false) => { change.delete_edge(EId(id)); live.remove(&at); }
                    (None, true) => { change.add_edge(EId(eid), VId(a), VId(b), vec![]);
                        live.insert(at, eid); eid += 1; }
                    _ => {}
                }
            }
            if mask != 0 { db.write(&commit, change).await.unwrap(); }
            check(&db, &cx, &h, R);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn result_failures_do_not_undo_writes_and_rebuild_uses_current_not_historical_size() {
    let ((), report) = run_async_under_lab(0x6b63_0103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let small = GqlQueryPolicy::new(100_000, 3, 10_000_000, 10_000_000);
        let limited = db.register_standing_core_numbers(&cx, R, small).unwrap();
        let healthy = db.register_standing_core_numbers(&cx, R, policy()).unwrap();
        let mut seed = WriteBatch::new(R);
        for v in 0..3 { seed.create_vertex(VId(v), vec![], vec![]); }
        for (id, a, b) in [(1, 0, 1), (2, 1, 2), (3, 2, 0)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        let basis = db.write(&commit, seed).await.unwrap();
        let mut add = WriteBatch::new(S); add.create_vertex(VId(9), vec![], vec![]);
        db.write(&commit, add).await.unwrap();
        assert!(matches!(db.standing_core_numbers(&cx, &limited), Err(StandingQueryError::Unavailable {
            frontier, reason: StandingQueryFailure::ResultBudget }) if frontier == basis));
        assert!(matches!(db.standing_core_number(&cx, &limited, VId(9)),
            Err(StandingQueryError::Unavailable { .. })));
        check(&db, &cx, &healthy, R);
        assert!(matches!(db.rebuild_standing_query(&cx, &limited, small),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::ResultBudget))));
        let mut remove = WriteBatch::new(S); remove.delete_vertex(VId(9));
        let current = db.write(&commit, remove).await.unwrap();
        assert_eq!(db.rebuild_standing_query(&cx, &limited, small).unwrap(), current);
        check(&db, &cx, &limited, R);
        let before = *db.standing_core_numbers(&cx, &limited).unwrap().last_maintenance();
        assert!(db.rebuild_standing_query(&cx, &limited, GqlQueryPolicy::new(0, 3, 0, 0)).is_err());
        assert_eq!(db.standing_core_numbers(&cx, &limited).unwrap().last_maintenance(), &before);
        check(&db, &cx, &limited, R);
        // Insertions sort before retirements; only final vertex cardinality matters.
        let mut swap = WriteBatch::new(R);
        swap.delete_vertex(VId(2)); swap.create_vertex(VId(10), vec![], vec![]);
        swap.add_edge(EId(10), VId(0), VId(10), vec![]);
        db.write(&commit, swap).await.unwrap();
        check(&db, &cx, &limited, R);
        assert_eq!(db.standing_core_numbers(&cx, &limited).unwrap().rows().len(), 3);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compact_reopen_rebuild_and_wrong_owner_or_kind_keep_the_native_boundaries() {
    let ((), report) = run_async_under_lab(0x6b63_0104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let old = db.register_standing_core_numbers(&cx, R, policy()).unwrap();
        let wrong_kind = db.register_standing_components(&cx, R, policy()).unwrap();
        assert!(matches!(db.standing_core_numbers(&cx, &wrong_kind), Err(StandingQueryError::Unsupported)));
        assert!(matches!(db.standing_query(&cx, &old), Err(StandingQueryError::Unsupported)));
        assert!(matches!(db.standing_components(&cx, &old), Err(StandingQueryError::Unsupported)));
        let mut seed = WriteBatch::new(R);
        for v in [0, 1, u128::MAX] { seed.create_vertex(VId(v), vec![], vec![]); }
        seed.add_edge(EId(1), VId(0), VId(1), vec![]);
        db.write(&commit, seed).await.unwrap();
        let expected = oracle(&db, R);
        db.compact(&commit).await.unwrap();
        check(&db, &cx, &old, R);
        db.rebuild_standing_query(&cx, &old, policy()).unwrap();
        check(&db, &cx, &old, R); drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert!(matches!(db.standing_core_numbers(&cx, &old), Err(StandingQueryError::ForeignHandle)));
        assert!(matches!(db.rebuild_standing_query(&cx, &old, policy()), Err(StandingQueryError::ForeignHandle)));
        let new = db.register_standing_core_numbers(&cx, R, policy()).unwrap();
        check(&db, &cx, &new, R);
        assert_eq!(oracle(&db, R), expected);
        assert_eq!(db.standing_core_number(&cx, &new, VId(u128::MAX)).unwrap(), Some(0));
        assert_eq!(db.standing_core_number(&cx, &new, VId(88)).unwrap(), None);
        assert_eq!(db.standing_core_numbers(&cx, &new).unwrap().last_maintenance().delta_rows, 0);
        assert!(db.frontier().unwrap() > CommitSeq::ORIGIN);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
