use super::*;
use crate::DatabaseKeys;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{DeltaRow, PropertyKeyId};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const OTHER: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);

async fn fixture(cx: &CommitCx) -> Database<crate::MemVfs> {
    let keys = DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    );
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(10))]);
    seed.create_vertex(VId(2), vec![], vec![]);
    db.write(cx, seed).await.unwrap();
    db
}

fn read_conflict<T>(result: Result<T, WriteTxnError>) {
    assert!(matches!(
        result,
        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
            law: "FG-LAW-FCW-READ-01",
            ..
        }))
    ));
}

#[test]
fn label_dependent_field_edit_survives_unrelated_label_change_but_not_full_read() {
    let ((), report) = run_async_under_lab(0xf13d_0101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        for broad in [false, true] {
            let mut db = fixture(&cx).await;
            let mut txn = db.begin(&tcx).unwrap();
            assert_eq!(txn.vertex_has_label(&db, VId(1), L).unwrap(), Some(true));
            assert!(txn.read_set.borrow().is_empty());
            if broad {
                txn.vertex(&db, VId(1)).unwrap();
            }
            let mut mine = WriteBatch::new(R);
            mine.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(11)));
            txn.write(&mut db, mine).unwrap();
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_label(VId(1), OTHER, true);
            db.write(&cx, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            let result = txn.commit_disjoint_fields_rebased(&mut db, &cx, 1).await;
            if broad {
                read_conflict(result);
                assert_eq!(db.frontier().unwrap(), frontier);
            } else {
                assert!(result.is_ok());
                assert_eq!(db.delta_since(frontier).unwrap().count(), 1);
                let row = db.vertex(VId(1)).unwrap().unwrap();
                assert_eq!(row.labels, vec![L, OTHER]);
                assert_eq!(row.props, vec![(P, CanonicalScalar::Int(11))]);
            }
            assert_eq!(tcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn label_absence_lifetimes_and_aba_remain_observed_after_rollback() {
    let ((), report) = run_async_under_lab(0xf13d_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        for case in 0..4 {
            let mut db = fixture(&cx).await;
            let mut txn = db.begin(&tcx).unwrap();
            txn.savepoint(&db, "before_read").unwrap();
            let mut winner = WriteBatch::new(R);
            match case {
                0 => {
                    assert_eq!(
                        txn.vertex_has_label(&db, VId(1), OTHER).unwrap(),
                        Some(false)
                    );
                    winner.set_vertex_label(VId(1), OTHER, true);
                }
                1 => {
                    assert_eq!(txn.vertex_has_label(&db, VId(9), L).unwrap(), None);
                    // Even an unlabelled creation changes the None answer.
                    winner.create_vertex(VId(9), vec![], vec![]);
                }
                2 => {
                    assert_eq!(txn.vertex_has_label(&db, VId(1), L).unwrap(), Some(true));
                    winner.delete_vertex(VId(1));
                }
                _ => {
                    assert_eq!(txn.vertex_has_label(&db, VId(1), L).unwrap(), Some(true));
                    winner.set_vertex_label(VId(1), L, false);
                }
            }
            txn.rollback_to_savepoint(&db, "before_read").unwrap();
            txn.release_savepoint(&db, "before_read").unwrap();
            db.write(&cx, winner).await.unwrap();
            if case == 3 {
                let mut restore = WriteBatch::new(R);
                restore.set_vertex_label(VId(1), L, true);
                db.write(&cx, restore).await.unwrap();
            }
            let old = txn.basis();
            read_conflict(txn.refresh_snapshot(&db, &tcx));
            assert_eq!(txn.basis(), old);
            assert_eq!(txn.state(), EmbeddedTxnState::Active);
            read_conflict(txn.finish(&mut db, &cx).await);
            assert!(txn.point_reads.borrow().is_empty());
            assert_eq!(tcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn label_projection_uses_canonical_staged_membership_and_noop_semantics() {
    let ((), report) = run_async_under_lab(0xf13d_0103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        let mut db = fixture(&cx).await;
        let mut txn = db.begin(&tcx).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_label(VId(1), L, false);
        batch.set_vertex_label(VId(1), OTHER, true);
        batch.ensure_vertex(VId(1), vec![L], vec![]);
        batch.create_vertex(VId(7), vec![L], vec![]);
        txn.write(&mut db, batch).unwrap();
        assert_eq!(txn.vertex_has_label(&db, VId(1), L).unwrap(), Some(false));
        assert_eq!(
            txn.vertex_has_label(&db, VId(1), OTHER).unwrap(),
            Some(true)
        );
        assert_eq!(txn.vertex_has_label(&db, VId(7), L).unwrap(), Some(true));
        assert!(txn.read_set.borrow().is_empty());
        let mut delete = WriteBatch::new(R);
        delete.delete_vertex(VId(7));
        txn.write(&mut db, delete).unwrap();
        assert_eq!(txn.vertex_has_label(&db, VId(7), L).unwrap(), None);
        txn.commit(&mut db, &cx).await.unwrap();
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().labels, vec![OTHER]);
        assert!(db.vertex(VId(7)).unwrap().is_none());
        assert!(matches!(
            txn.vertex_has_label(&db, VId(1), L),
            Err(WriteTxnError::Finished)
        ));
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn append_rebase_checks_the_label_used_to_choose_its_edge() {
    let ((), report) = run_async_under_lab(0xf13d_0104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        for changed in [false, true] {
            let mut db = fixture(&cx).await;
            let mut txn = db.begin(&tcx).unwrap();
            assert_eq!(txn.vertex_has_label(&db, VId(1), L).unwrap(), Some(true));
            let mut append = WriteBatch::new(R);
            append.add_edge(EId(9), VId(1), VId(2), vec![]);
            txn.write(&mut db, append).unwrap();
            let mut winner = WriteBatch::new(R);
            winner.add_edge(EId(8), VId(1), VId(2), vec![]);
            if changed {
                winner.set_vertex_label(VId(1), L, false);
            } else {
                // PropertyKeyId(1) must not collide with LabelId(1).
                winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
            }
            db.write(&cx, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            let result = txn.commit_append_only_rebased(&mut db, &cx, 1).await;
            if changed {
                read_conflict(result);
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.edge(EId(9)).unwrap().is_none());
            } else {
                assert!(result.is_ok());
                assert!(db.edge(EId(9)).unwrap().is_some());
                assert_eq!(db.delta_since(frontier).unwrap().count(), 1);
            }
            assert_eq!(tcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn label_and_property_witnesses_do_not_alias_and_point_sets_union() {
    let vertex = ElementId::Vertex(VId(1));
    let membership = DeltaRow::LabelMembership {
        vid: VId(1),
        label: L,
        before: false,
        after: true,
    };
    let property = DeltaRow::Property {
        elem: vertex,
        property: P,
        before: None,
        after: Some(CanonicalScalar::Int(1)),
    };
    let mut reads = PointReads::default();
    reads.record(vertex, PointReadField::Property(P));
    assert_eq!(reads.conflict(&membership, &mut || Ok(())).unwrap(), None);
    reads.record(vertex, PointReadField::Label(L));
    assert_eq!(
        reads.conflict(&membership, &mut || Ok(())).unwrap(),
        Some(vertex)
    );
    assert_eq!(
        reads.conflict(&property, &mut || Ok(())).unwrap(),
        Some(vertex)
    );
    reads.clear();
    reads.record(vertex, PointReadField::Label(L));
    assert_eq!(reads.conflict(&property, &mut || Ok(())).unwrap(), None);
    assert!(matches!(
        reads.conflict(&membership, &mut || Err(WriteTxnError::NoPreparedWrite)),
        Err(WriteTxnError::NoPreparedWrite)
    ));
    assert_eq!(
        reads.conflict(&membership, &mut || Ok(())).unwrap(),
        Some(vertex)
    );
}
