//! Public latest-tick derivatives are exact selected bags, not a change log.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryHandle, WriteBatch};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::GqlQueryPolicy;
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId};

const P: PropertyKeyId = PropertyKeyId(1);
const LIMBS: LimbLimit = LimbLimit::new(4);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32])
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000) }
fn definition(distinct: bool, offset: u64, limit: Option<u64>) -> PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let query = builder.prepare_values(&[GraphColumn::property("p", "n", P)], offset, limit).unwrap();
    if distinct { query } else { query.with_duplicates() }
}
fn rows(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle) -> ZSet<GraphValueRow> {
    db.standing_rows(cx, handle).unwrap().rows().checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn check(
    db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle,
    definition: &PreparedGraphPattern<GraphValueRow>, previous: &mut ZSet<GraphValueRow>,
) {
    let delta = db.standing_row_delta(cx, handle).unwrap().unwrap();
    assert_eq!(delta.frontier(), db.frontier().unwrap());
    assert!(delta.ordered_rows().is_none());
    previous.integrate(delta.rows(), LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(*previous, rows(db, cx, handle));
    let eager = db.execute_graph_pattern_governed(cx, definition, policy()).unwrap().value;
    let expected = ZSet::from_updates(eager.into_iter().map(|row| (row, ZWeight::ONE)),
        LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(*previous, expected);
}

#[test]
fn latest_deltas_integrate_to_bags_and_distinct_pages_after_each_real_commit() {
    let ((), report) = run_async_under_lab(0x6465_6c01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (vid, value) in [(1, 1), (2, 1), (3, 2), (4, 3)] {
            seed.create_vertex(VId(vid), vec![], vec![(P, CanonicalScalar::Int(value))]);
        }
        db.write(&commit, seed).await.unwrap();
        let definitions = [definition(false, 0, None), definition(false, 1, Some(2)), definition(true, 1, Some(2))];
        let handles: Vec<_> = definitions.iter().map(|q|
            db.register_standing_rows(&cx, q.clone(), policy()).unwrap()).collect();
        let mut previous: Vec<_> = handles.iter().map(|h| {
            assert!(db.standing_row_delta(&cx, h).unwrap().is_none());
            rows(&db, &cx, h)
        }).collect();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(4)));
        let mut retire = WriteBatch::new(RelationId(1)); retire.delete_vertex(VId(2));
        let mut unrelated = WriteBatch::new(RelationId(1));
        unrelated.set_vertex_property(VId(3), PropertyKeyId(99), Some(CanonicalScalar::Int(7)));
        let mut previous_seq = db.frontier().unwrap();
        for batch in [edit, retire, unrelated] {
            let at = db.write(&commit, batch).await.unwrap();
            assert_eq!(at, CommitSeq(previous_seq.0 + 1));
            previous_seq = at;
            for ((h, q), old) in handles.iter().zip(&definitions).zip(&mut previous) {
                check(&db, &cx, h, q, old);
                // Repeated reads borrow the same accepted derivative, not an ACK.
                assert_eq!(db.standing_row_delta(&cx, h).unwrap().unwrap().frontier(), at);
            }
        }
        for h in &handles {
            assert!(db.standing_row_delta(&cx, h).unwrap().unwrap().rows().is_empty());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_views_do_not_export_old_deltas_and_rebuild_starts_a_new_baseline() {
    let ((), report) = run_async_under_lab(0x6465_6c02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
        let basis = db.write(&commit, seed).await.unwrap();
        let q = definition(false, 0, None);
        let limited = db.register_standing_rows(&cx, q.clone(), GqlQueryPolicy::new(100, 1, 1_000_000, 1_000_000)).unwrap();
        let healthy = db.register_standing_rows(&cx, q.clone(), policy()).unwrap();
        let mut insert = WriteBatch::new(RelationId(1));
        insert.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(2))]);
        let current = db.write(&commit, insert).await.unwrap();
        assert!(matches!(db.standing_row_delta(&cx, &limited),
            Err(StandingQueryError::Unavailable { frontier, .. }) if frontier == basis));
        assert_eq!(db.standing_row_delta(&cx, &healthy).unwrap().unwrap().frontier(), current);
        assert_eq!(db.rebuild_standing_query(&cx, &limited, policy()).unwrap(), current);
        assert!(db.standing_row_delta(&cx, &limited).unwrap().is_none());
        let mut previous = rows(&db, &cx, &limited);
        let mut retire = WriteBatch::new(RelationId(1)); retire.delete_vertex(VId(2));
        db.write(&commit, retire).await.unwrap();
        check(&db, &cx, &limited, &q, &mut previous);
        assert!(db.standing_row_delta(&cx, &limited).unwrap().unwrap().rows().iter()
            .all(|(_, weight)| weight.to_i128() == Some(-1)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn delta_access_checks_kind_owner_and_reopen_before_exposing_changes() {
    let ((), report) = run_async_under_lab(0x6465_6c03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let row = db.register_standing_rows(&cx, definition(false, 0, None), policy()).unwrap();
        let topology = db.register_standing_reachability(&cx, RelationId(1), policy()).unwrap();
        assert!(matches!(db.standing_row_delta(&cx, &topology), Err(StandingQueryError::Unsupported)));
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(foreign.standing_row_delta(&cx, &row), Err(StandingQueryError::ForeignHandle)));
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert!(matches!(reopened.standing_row_delta(&cx, &row), Err(StandingQueryError::ForeignHandle)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
