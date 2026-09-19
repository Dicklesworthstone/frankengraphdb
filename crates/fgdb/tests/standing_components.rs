//! Public component analytics over the real Chronicle/Strata lab composition.
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
// Recompute by full-graph minimum propagation from ordinary source reads.
// No maintained member/neighbor index or component implementation is consulted.
fn oracle(db: &Database<MemVfs>, relation: RelationId) -> BTreeMap<VId, VId> {
    let mut labels: BTreeMap<_, _> = db.vertices().unwrap().iter().map(|row| (row.vid, row.vid)).collect();
    let edges = db.edges().unwrap();
    loop {
        let mut changed = false;
        for edge in &edges {
            let edge = &edge.entry;
            if edge.relation != relation { continue; }
            let root = labels[&edge.src].min(labels[&edge.dst]);
            for vertex in [edge.src, edge.dst] {
                if labels[&vertex] != root { labels.insert(vertex, root); changed = true; }
            }
        }
        if !changed { return labels; }
    }
}
fn check(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle, relation: RelationId) {
    let expected = oracle(db, relation);
    let view = db.standing_components(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert!(view.ordered_rows().is_none());
    let rows: BTreeMap<_, _> = view.rows().iter().map(|((v, root), weight)| {
        assert_eq!(weight, &ZWeight::ONE);
        (*v, *root)
    }).collect();
    assert_eq!(rows, expected);
    assert_eq!(db.standing_component_count(cx, handle).unwrap(), expected.values().collect::<BTreeSet<_>>().len());
    for (vertex, root) in expected { assert_eq!(db.standing_component(cx, handle, vertex).unwrap(), Some(root)); }
    assert_eq!(db.standing_component(cx, handle, VId(999_999)).unwrap(), None);
}

#[test]
fn writes_maintain_isolates_parallel_edges_relation_scope_and_vertex_cascades() {
    let ((), report) = run_async_under_lab(0x6363_6401, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let r = db.register_standing_components(&cx, R, policy()).unwrap();
        let s = db.register_standing_components(&cx, S, policy()).unwrap();
        let reachability = db.register_standing_reachability(&cx, R, policy()).unwrap();
        check(&db, &cx, &r, R); check(&db, &cx, &s, S);
        let wide = 1_u128 << 100;
        let mut seed = WriteBatch::new(S);
        for v in [0, 1, 2, 3, 4, wide] { seed.create_vertex(VId(v), vec![], vec![]); }
        db.write(&commit, seed).await.unwrap();
        assert_eq!(db.standing_component_count(&cx, &r).unwrap(), 6);
        let mut a = WriteBatch::new(R);
        for (id, from, to) in [(10, 0, 1), (11, 1, 0), (12, 1, 2), (13, 2, 0), (14, 2, 3), (15, 4, 4)] {
            a.add_edge(EId(id), VId(from), VId(to), vec![]);
        }
        let mut b = WriteBatch::new(S);
        b.add_edge(EId(20), VId(0), VId(wide), vec![]);
        b.add_edge(EId(21), VId(3), VId(4), vec![]);
        db.write_atomic(&commit, vec![b, a]).await.unwrap();
        check(&db, &cx, &r, R); check(&db, &cx, &s, S);
        assert_eq!(db.standing_component_count(&cx, &r).unwrap(), 3);
        let mut props = WriteBatch::new(R);
        props.set_vertex_property(VId(0), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
        db.write(&commit, props).await.unwrap();
        assert_eq!(db.standing_components(&cx, &r).unwrap().last_maintenance().affected_vertices, 0);
        let mut partial = WriteBatch::new(R); partial.delete_edge(EId(10));
        db.write(&commit, partial).await.unwrap();
        assert_eq!(db.standing_components(&cx, &r).unwrap().last_maintenance().affected_vertices, 0);
        check(&db, &cx, &r, R);
        // The vertex was created under S, has R and S incident edges, and is
        // retired under S. Every registry observes the same complete cascade.
        let mut cascade = WriteBatch::new(S); cascade.delete_vertex(VId(0));
        db.write(&commit, cascade).await.unwrap();
        check(&db, &cx, &r, R); check(&db, &cx, &s, S);
        assert_eq!(db.standing_component(&cx, &r, VId(0)).unwrap(), None);
        assert_eq!(db.standing_reachability(&cx, &reachability).unwrap().frontier(), db.frontier().unwrap());
        let mut split = WriteBatch::new(R); split.delete_edge(EId(12)); split.delete_edge(EId(14));
        db.write(&commit, split).await.unwrap();
        check(&db, &cx, &r, R); check(&db, &cx, &s, S);
        assert_eq!(db.standing_component_count(&cx, &r).unwrap(), 5);
        let basis = db.frontier().unwrap();
        let mut invalid = WriteBatch::new(R); invalid.add_edge(EId(99), VId(0), VId(1), vec![]);
        assert!(db.write(&commit, invalid).await.is_err());
        assert_eq!(db.frontier().unwrap(), basis); check(&db, &cx, &r, R);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failure_fences_only_the_derived_view_and_rebuild_uses_current_membership() {
    let ((), report) = run_async_under_lab(0x6363_6402, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let two = GqlQueryPolicy::new(100_000, 2, 10_000_000, 10_000_000);
        let limited = db.register_standing_components(&cx, R, two).unwrap();
        let healthy = db.register_standing_components(&cx, R, policy()).unwrap();
        let mut seed = WriteBatch::new(R);
        for v in 0..3 { seed.create_vertex(VId(v), vec![], vec![]); }
        seed.add_edge(EId(1), VId(0), VId(1), vec![]);
        let advanced = db.write(&commit, seed).await.unwrap();
        assert!(advanced > CommitSeq::ORIGIN);
        assert!(matches!(db.standing_components(&cx, &limited), Err(StandingQueryError::Unavailable {
            frontier: CommitSeq::ORIGIN, reason: StandingQueryFailure::ResultBudget })));
        assert!(matches!(db.standing_component_count(&cx, &limited), Err(StandingQueryError::Unavailable { .. })));
        assert!(matches!(db.standing_component(&cx, &limited, VId(100)), Err(StandingQueryError::Unavailable { .. })));
        check(&db, &cx, &healthy, R);
        assert!(matches!(db.rebuild_standing_query(&cx, &limited, two),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::ResultBudget))));
        let mut retire = WriteBatch::new(S); retire.delete_vertex(VId(0));
        let now = db.write(&commit, retire).await.unwrap();
        assert_eq!(db.rebuild_standing_query(&cx, &limited, two).unwrap(), now);
        check(&db, &cx, &limited, R); check(&db, &cx, &healthy, R);
        let before = *db.standing_components(&cx, &limited).unwrap().last_maintenance();
        assert!(matches!(db.rebuild_standing_query(&cx, &limited, GqlQueryPolicy::new(0, 2, 100_000, 100_000)),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::SnapshotBudget))));
        check(&db, &cx, &limited, R);
        assert_eq!(db.standing_components(&cx, &limited).unwrap().last_maintenance(), &before);
        let late = db.register_standing_components(&cx, R, two).unwrap();
        assert_eq!(db.standing_components(&cx, &late).unwrap().last_maintenance().delta_rows, 0);
        check(&db, &cx, &late, R);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn equal_size_membership_replacement_is_admitted_at_exact_final_row_limit() {
    let ((), report) = run_async_under_lab(0x6363_6403, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for v in [9, 10] { seed.create_vertex(VId(v), vec![], vec![]); }
        seed.add_edge(EId(1), VId(9), VId(10), vec![]);
        db.write(&commit, seed).await.unwrap();
        let handle = db.register_standing_components(&cx, R,
            GqlQueryPolicy::new(100_000, 2, 100_000, 100_000)).unwrap();
        let mut swap = WriteBatch::new(R);
        swap.create_vertex(VId(1), vec![], vec![]); swap.delete_vertex(VId(9));
        swap.add_edge(EId(2), VId(1), VId(10), vec![]);
        db.write(&commit, swap).await.unwrap();
        check(&db, &cx, &handle, R);
        assert_eq!(db.standing_component(&cx, &handle, VId(10)).unwrap(), Some(VId(1)));
        assert_eq!(db.standing_component_count(&cx, &handle).unwrap(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_four_vertex_simple_graphs_agree_with_independent_source_recomputation() {
    let ((), report) = run_async_under_lab(0x6363_6404, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        for mask in 0..64 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let handle = db.register_standing_components(&cx, R, policy()).unwrap();
            let mut batch = WriteBatch::new(R);
            for v in 0..4 { batch.create_vertex(VId(v), vec![], vec![]); }
            let sides = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];
            for (index, (a, b)) in sides.into_iter().enumerate() {
                if mask & (1 << index) != 0 { batch.add_edge(EId(index as u128 + 1), VId(a), VId(b), vec![]); }
            }
            db.write(&commit, batch).await.unwrap();
            check(&db, &cx, &handle, R);
            let mut delete = WriteBatch::new(R); delete.delete_vertex(VId(0));
            db.write(&commit, delete).await.unwrap(); check(&db, &cx, &handle, R);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compaction_reopen_and_handle_kind_checks_preserve_current_state_boundaries() {
    let ((), report) = run_async_under_lab(0x6363_6405, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let handle = db.register_standing_components(&cx, R, policy()).unwrap();
        let reach = db.register_standing_reachability(&cx, R, policy()).unwrap();
        let mut seed = WriteBatch::new(R);
        for v in [0, 1, u128::MAX] { seed.create_vertex(VId(v), vec![], vec![]); }
        seed.add_edge(EId(1), VId(1), VId(u128::MAX), vec![]);
        db.write(&commit, seed).await.unwrap();
        assert!(matches!(db.standing_components(&cx, &reach), Err(StandingQueryError::Unsupported)));
        assert!(matches!(db.standing_component_count(&cx, &reach), Err(StandingQueryError::Unsupported)));
        assert!(matches!(db.standing_reachability(&cx, &handle), Err(StandingQueryError::Unsupported)));
        assert!(matches!(db.standing_query(&cx, &handle), Err(StandingQueryError::Unsupported)));
        db.compact(&commit).await.unwrap();
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap(); check(&db, &cx, &handle, R);
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert!(matches!(db.standing_components(&cx, &handle), Err(StandingQueryError::ForeignHandle)));
        assert!(matches!(db.rebuild_standing_query(&cx, &handle, policy()), Err(StandingQueryError::ForeignHandle)));
        let fresh = db.register_standing_components(&cx, R, policy()).unwrap(); check(&db, &cx, &fresh, R);
        let mut split = WriteBatch::new(R); split.delete_edge(EId(1));
        db.write(&commit, split).await.unwrap(); check(&db, &cx, &fresh, R);
        assert_eq!(db.standing_component_count(&cx, &fresh).unwrap(), 3);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
