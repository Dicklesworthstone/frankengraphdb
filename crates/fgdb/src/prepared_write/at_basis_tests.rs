use super::*;
use crate::{DatabaseKeys, DatabaseState, MemVfs, WriteMismatchPolicy};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x39; 32],
        DatabaseSecurityNamespaceId([0x3a; 32]),
        [0x3b; 32],
    )
}

fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 1..=3 {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(10))]);
    }
    batch.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(10))]);
    batch
}

fn change_vertex(id: u128, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.set_vertex_property(VId(id), P, Some(CanonicalScalar::Int(value)));
    batch
}

fn value(db: &Database<MemVfs>, id: u128) -> CanonicalScalar {
    db.vertex(VId(id)).unwrap().unwrap().props[0].1.clone()
}

fn expect_conflict(result: Result<CommitSeq, WriteError>) {
    assert!(matches!(result, Err(WriteError::FirstCommitterWins { .. })));
}

#[test]
fn historical_preparation_keeps_exact_effects_and_commits_beside_disjoint_writes() {
    let ((), report) = run_async_under_lab(0x6261_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.write(&cx, seed()).await.unwrap();
        let pinned = db.pinned_read_view().unwrap();
        let expected = db.prepare_write(change_vertex(1, 20)).unwrap();
        let winner = db.write(&cx, change_vertex(3, 99)).await.unwrap();
        db.compact(&cx).await.unwrap();
        let root_before = db.partition_root().unwrap();
        let snapshot_before = Arc::clone(&db.snapshot);
        let prepared = db.prepare_write_at(basis, change_vertex(1, 20)).unwrap();
        assert_eq!(prepared.basis(), basis);
        assert_eq!(prepared.template, expected.template);
        assert!(Arc::ptr_eq(&db.snapshot, &snapshot_before));
        assert_eq!(db.frontier().unwrap(), winner);
        assert_eq!(db.partition_root().unwrap(), root_before);
        assert_eq!(value(&db, 1), CanonicalScalar::Int(10));
        let committed = db.commit_prepared(&cx, prepared).await.unwrap();
        assert_eq!(committed, CommitSeq(winner.0 + 1));
        assert_eq!(value(&db, 3), CanonicalScalar::Int(99));
        assert_eq!(
            pinned.vertex(VId(1)).unwrap().unwrap().props[0].1,
            CanonicalScalar::Int(10)
        );
        drop(snapshot_before);
        drop(db);
        let reopened = Database::open_with_vfs(&cx, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(value(&reopened, 1), CanonicalScalar::Int(20));
        assert_eq!(value(&reopened, 3), CanonicalScalar::Int(99));
        assert_eq!(reopened.frontier().unwrap(), committed);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_guards_and_delete_versions_do_not_observe_the_live_successor() {
    let ((), report) = run_async_under_lab(0x6261_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let basis = db.write(&cx, seed()).await.unwrap();
        let mut deletion = WriteBatch::new(RelationId(1));
        deletion.delete_vertex(VId(1));
        let expected_delete = db.prepare_write(deletion.clone()).unwrap();
        let mut winner = change_vertex(1, 90);
        winner.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(90)));
        winner.add_edge(EId(11), VId(1), VId(3), vec![]);
        let live = db.write(&cx, winner).await.unwrap();
        let original = Arc::clone(&db.snapshot);
        let historical_delete = db.prepare_write_at(basis, deletion).unwrap();
        assert_eq!(historical_delete.template, expected_delete.template);
        for edge in [false, true] {
            let mut guard = WriteBatch::new(RelationId(1));
            if edge {
                guard.compare_and_set_edge_property(
                    EId(10),
                    P,
                    Some(CanonicalScalar::Int(10)),
                    CanonicalScalar::Int(20),
                    WriteMismatchPolicy::AbortWrite,
                );
            } else {
                guard.compare_and_set_vertex_property(
                    VId(1),
                    P,
                    Some(CanonicalScalar::Int(10)),
                    CanonicalScalar::Int(20),
                    WriteMismatchPolicy::AbortWrite,
                );
            }
            assert!(matches!(
                db.prepare_write(guard.clone()),
                Err(WriteError::CompareAndSetMismatch(_))
            ));
            let prepared = db.prepare_write_at(basis, guard).unwrap();
            assert_eq!(prepared.basis(), basis);
            expect_conflict(db.commit_prepared(&cx, prepared).await);
            assert!(Arc::ptr_eq(&db.snapshot, &original));
            assert_eq!(db.frontier().unwrap(), live);
        }
        expect_conflict(db.commit_prepared(&cx, historical_delete).await);
        assert!(db.vertex(VId(1)).unwrap().is_some());
        assert!(db.edge(EId(11)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap(), live);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_spent_identities_refusals_and_unwind_restore_live_state() {
    let ((), report) = run_async_under_lab(0x6261_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let first = db.write(&cx, seed()).await.unwrap();
        let mut deletion = WriteBatch::new(RelationId(1));
        deletion.delete_vertex(VId(1));
        let spent_basis = db.write(&cx, deletion).await.unwrap();
        let live = db.write(&cx, change_vertex(3, 99)).await.unwrap();
        let original = Arc::clone(&db.snapshot);
        let mut recreate = WriteBatch::new(RelationId(1));
        recreate.create_vertex(VId(1), vec![], vec![]);
        assert!(matches!(
            db.prepare_write_at(spent_basis, recreate.clone()),
            Err(WriteTxnError::Write(WriteError::IdentitySpent {
                elem: ElementId::Vertex(VId(1))
            }))
        ));
        let at_origin = db.prepare_write_at(CommitSeq(0), recreate).unwrap();
        assert_eq!(at_origin.basis(), CommitSeq(0));
        expect_conflict(db.commit_prepared(&cx, at_origin).await);
        assert!(matches!(
            db.prepare_write_at(CommitSeq(live.0 + 1), change_vertex(3, 1)),
            Err(WriteTxnError::Read(crate::ReadError::BeyondFrontier { .. }))
        ));
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scope = db.preparation_basis(first).unwrap();
            panic!("exercise preparation-scope restoration");
        }));
        assert!(unwound.is_err());
        assert!(Arc::ptr_eq(&db.snapshot, &original));
        assert_eq!(
            db.state(),
            DatabaseState::Healthy {
                published_frontier: live
            }
        );
        assert_eq!(value(&db, 3), CanonicalScalar::Int(99));
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.writer.is_vertex_spent(VId(1)));
        db.write(&cx, change_vertex(3, 100)).await.unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_atomic_and_ordered_composition_keep_their_original_contracts() {
    let ((), report) = run_async_under_lab(0x6261_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let basis = db.write(&cx, seed()).await.unwrap();
        let mut prefix = WriteBatch::new(RelationId(9));
        prefix.create_vertex(VId(5), vec![], vec![]);
        prefix.add_edge(EId(50), VId(1), VId(5), vec![]);
        let mut suffix = WriteBatch::new(RelationId(2));
        suffix.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(30)));
        suffix.add_edge(EId(60), VId(5), VId(2), vec![]);
        let ordered = vec![prefix, suffix];
        let expected = db.prepare_ordered_writes(ordered.clone()).unwrap();
        let mut left = WriteBatch::new(RelationId(7));
        left.add_edge(EId(70), VId(1), VId(2), vec![]);
        let mut right = WriteBatch::new(RelationId(8));
        right.add_edge(EId(80), VId(2), VId(1), vec![]);
        let atomic = vec![left, right];
        let expected_atomic = db.prepare_atomic_writes(atomic.clone()).unwrap();
        let live = db.write(&cx, change_vertex(3, 99)).await.unwrap();
        let prepared = db
            .prepare_ordered_writes_at(basis, ordered.clone())
            .unwrap();
        assert_eq!(prepared.template, expected.template);
        assert_eq!(prepared.basis(), basis);
        assert!(db.prepare_atomic_writes_at(basis, ordered).is_err());
        let independent = db.prepare_atomic_writes_at(basis, atomic).unwrap();
        assert_eq!(independent.template, expected_atomic.template);
        assert_eq!(independent.basis(), basis);
        assert_eq!(db.frontier().unwrap(), live);
        let committed = db.commit_prepared(&cx, prepared).await.unwrap();
        assert_eq!(committed, CommitSeq(live.0 + 1));
        assert_eq!(
            db.edge(EId(50)).unwrap().unwrap().entry.relation,
            RelationId(9)
        );
        assert_eq!(
            db.edge(EId(60)).unwrap().unwrap().entry.relation,
            RelationId(2)
        );
        assert_eq!(value(&db, 5), CanonicalScalar::Int(30));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
