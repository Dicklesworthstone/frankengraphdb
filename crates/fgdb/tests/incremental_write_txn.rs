//! Ordinary append/SET staging reuses admitted effects without changing the
//! transaction's snapshot, rollback, conflict, or durable publication laws.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteMismatchPolicy, WriteTxnError,
};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x7c; 32],
        DatabaseSecurityNamespaceId([0x7d; 32]),
        [0x7e; 32],
    )
}

fn seed_batch() -> WriteBatch {
    let mut seed = WriteBatch::new(R);
    for id in 1..=3 {
        seed.create_vertex(VId(id), vec![], vec![(P, int(0))]);
    }
    seed.add_edge(EId(10), VId(1), VId(2), vec![(P, int(0))]);
    seed
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    db.write(cx, seed_batch()).await.unwrap();
    db
}

#[test]
fn mixed_scripts_keep_prefix_values_births_one_commit_and_recovery() {
    let ((), report) = run_async_under_lab(0x6974_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(&cx, seed_batch()).await.unwrap();
        let before = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        for record in 0..24_u64 {
            let vid = VId(u128::from(100 + record));
            let eid = EId(u128::from(1000 + record));
            let value = i64::try_from(record).unwrap();
            let mut create = WriteBatch::new(R);
            create.ensure_vertex(VId(1), vec![], vec![]);
            create.create_vertex(vid, vec![], vec![(P, int(0))]);
            txn.write(&mut db, create).unwrap();
            let mut edge = WriteBatch::new(R);
            edge.add_edge(eid, vid, VId(2), vec![(P, int(0))]);
            txn.write(&mut db, edge).unwrap();
            let mut updates = WriteBatch::new(R);
            updates.set_vertex_property(vid, P, Some(int(value)));
            updates.compare_and_set_edge_property(
                eid,
                P,
                Some(int(0)),
                int(value),
                WriteMismatchPolicy::AbortWrite,
            );
            updates.set_vertex_property(VId(1), Q, Some(int(value)));
            txn.write(&mut db, updates).unwrap();
            let vertex = txn.vertex(&db, vid).unwrap().unwrap();
            assert_eq!(vertex.birth_ordinal, 6 * record + 2);
            assert_eq!(vertex.props, vec![(P, int(value).into())]);
            assert_eq!(
                txn.edge(&db, eid).unwrap().unwrap().props,
                vec![(P, int(value).into())]
            );
            assert!(db.vertex(vid).unwrap().is_none());
        }
        let committed = txn.commit(&mut db, &cx).await.unwrap();
        assert_eq!(committed, CommitSeq(before.0 + 1));
        assert_eq!(db.delta_since(before).unwrap().count(), 1);
        assert_eq!(db.edges().unwrap().len(), 25);
        assert!(pinned.vertex(VId(100)).unwrap().is_none());
        let expected_vertices = db.vertices().unwrap();
        let expected_edges = db.edges().unwrap();
        drop(db);
        let recovered = Database::open_with_vfs(&cx, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(recovered.frontier().unwrap(), committed);
        assert_eq!(recovered.vertices().unwrap(), expected_vertices);
        assert_eq!(recovered.edges().unwrap(), expected_edges);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_suffix_and_savepoint_restore_the_exact_prepared_prefix() {
    let ((), report) = run_async_under_lab(0x6974_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(50), vec![], vec![(P, int(1))]);
        prefix.add_edge(EId(500), VId(50), VId(2), vec![(P, int(1))]);
        txn.write(&mut db, prefix).unwrap();
        let saved = txn.staged_effect_digest().unwrap();
        txn.savepoint(&db, "prefix").unwrap();
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(50), P, Some(int(2)));
        txn.write(&mut db, change).unwrap();
        let changed = txn.staged_effect_digest().unwrap();
        let mut refused = WriteBatch::new(R);
        refused.create_vertex(VId(60), vec![], vec![]);
        refused.compare_and_set_vertex_property(
            VId(50),
            P,
            Some(int(99)),
            int(3),
            WriteMismatchPolicy::AbortWrite,
        );
        assert!(matches!(
            txn.write(&mut db, refused),
            Err(WriteTxnError::Write(WriteError::CompareAndSetMismatch(_)))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), changed);
        assert!(txn.vertex(&db, VId(60)).unwrap().is_none());
        txn.rollback_to_savepoint(&db, "prefix").unwrap();
        assert_eq!(txn.staged_effect_digest().unwrap(), saved);
        assert_eq!(
            txn.vertex(&db, VId(50)).unwrap().unwrap().props,
            vec![(P, int(1).into())]
        );
        let mut append = WriteBatch::new(R);
        append.ensure_vertex(VId(1), vec![], vec![]);
        append.create_vertex(VId(61), vec![], vec![]);
        txn.write(&mut db, append).unwrap();
        assert_eq!(txn.vertex(&db, VId(61)).unwrap().unwrap().birth_ordinal, 4);
        txn.commit(&mut db, &cx).await.unwrap();
        assert!(db.vertex(VId(60)).unwrap().is_none());
        assert_eq!(
            db.vertex(VId(50)).unwrap().unwrap().props,
            vec![(P, int(1).into())]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn successful_and_refused_suffix_observations_survive_rollback() {
    let ((), report) = run_async_under_lab(0x6974_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for observed in [VId(2), VId(3)] {
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(50), vec![], vec![]);
            txn.write(&mut db, prefix).unwrap();
            txn.savepoint(&db, "prefix").unwrap();
            let mut discarded = WriteBatch::new(R);
            discarded.set_vertex_property(VId(3), P, Some(int(5)));
            txn.write(&mut db, discarded).unwrap();
            let mut refused = WriteBatch::new(R);
            refused.compare_and_set_vertex_property(
                VId(2),
                P,
                Some(int(99)),
                int(5),
                WriteMismatchPolicy::AbortWrite,
            );
            assert!(txn.write(&mut db, refused).is_err());
            txn.rollback_to_savepoint(&db, "prefix").unwrap();
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_property(observed, P, Some(int(1)));
            db.write(&cx, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            assert!(matches!(
                txn.commit_strict(&mut db, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(VId(50)).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_op_dependencies_are_not_erased_by_incremental_extension() {
    let ((), report) = run_async_under_lab(0x6974_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for observed in [VId(1), VId(2)] {
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.compare_and_set_vertex_property(
                VId(1),
                P,
                Some(int(99)),
                int(5),
                WriteMismatchPolicy::NoOp,
            );
            prefix.ensure_vertex(VId(2), vec![], vec![]);
            prefix.create_vertex(VId(50), vec![], vec![]);
            txn.write(&mut db, prefix).unwrap();
            let mut suffix = WriteBatch::new(R);
            suffix.set_vertex_property(VId(50), P, Some(int(2)));
            txn.write(&mut db, suffix).unwrap();
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_property(observed, P, Some(int(1)));
            db.write(&cx, winner).await.unwrap();
            assert!(matches!(
                txn.commit_strict(&mut db, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
            ));
            assert!(db.vertex(VId(50)).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn deletion_fallback_preserves_spent_ids_and_edge_ensure_fallback_keeps_aliases() {
    let ((), report) = run_async_under_lab(0x6974_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(50), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let mut retired = WriteBatch::new(R);
        retired.delete_vertex(VId(50));
        txn.write(&mut db, retired).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let mut reuse = WriteBatch::new(R);
        reuse.create_vertex(VId(50), vec![], vec![]);
        assert!(matches!(
            txn.write(&mut db, reuse),
            Err(WriteTxnError::Write(WriteError::IdentitySpent {
                elem: ElementId::Vertex(VId(50))
            }))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        drop(txn);

        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(60), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let mut ensure = WriteBatch::new(R);
        ensure.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
        txn.write(&mut db, ensure).unwrap();
        let mut suffix = WriteBatch::new(R);
        suffix.set_vertex_property(VId(60), P, Some(int(7)));
        txn.write(&mut db, suffix).unwrap();
        txn.commit(&mut db, &cx).await.unwrap();
        assert!(db.edge(EId(999)).unwrap().is_none());
        assert!(db.edge(EId(10)).unwrap().is_some());
        assert_eq!(
            db.vertex(VId(60)).unwrap().unwrap().props,
            vec![(P, int(7).into())]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
