use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{DeltaRow, PropertyKeyId, SchemaEpoch};
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId, PurposeContexts};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

/// The whole-row oracle for single-property reads: the requested property's
/// value in a full row, with a stored Null kept as Some(Null).
fn take_point_property(
    properties: Vec<(PropertyKeyId, CanonicalScalar)>,
    requested: PropertyKeyId,
) -> Option<CanonicalScalar> {
    properties
        .into_iter()
        .find_map(|(key, value)| (key == requested).then_some(value))
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![(P, CanonicalScalar::Int(10)), (Q, CanonicalScalar::Int(20))],
        );
    }
    // Same numeric vertex/edge identity must not alias a witness.
    batch.add_edge(
        EId(1),
        VId(1),
        VId(2),
        vec![(P, CanonicalScalar::Int(30)), (Q, CanonicalScalar::Int(40))],
    );
    db.write(cx, batch).await.unwrap();
}

async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    seed(&mut db, cx).await;
    db
}

fn read(
    txn: &WriteTxn,
    db: &Database<MemVfs>,
    edge: bool,
    key: PropertyKeyId,
) -> Option<CanonicalScalar> {
    if edge {
        txn.edge_property(db, EId(1), key).unwrap()
    } else {
        txn.vertex_property(db, VId(1), key).unwrap()
    }
}

fn set(edge: bool, key: PropertyKeyId, value: Option<CanonicalScalar>) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    if edge {
        batch.set_edge_property(EId(1), key, value);
    } else {
        batch.set_vertex_property(VId(1), key, value);
    }
    batch
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
fn read_modify_write_on_disjoint_fields_matches_serial_execution_and_reopen() {
    let ((), report) = run_async_under_lab(0xf13d_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        for edge in [false, true] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            seed(&mut db, &cx).await;
            let mut serial = database(&cx).await;
            let mut left = db.begin(&tcx).unwrap();
            let mut right = db.begin(&tcx).unwrap();
            let Some(CanonicalScalar::Int(a)) = read(&left, &db, edge, P) else {
                panic!("integer")
            };
            let Some(CanonicalScalar::Int(b)) = read(&right, &db, edge, Q) else {
                panic!("integer")
            };
            assert!(left.read_set.borrow().is_empty());
            assert!(right.read_set.borrow().is_empty());
            let first = set(edge, P, Some(CanonicalScalar::Int(a + 1)));
            let second = set(edge, Q, Some(CanonicalScalar::Int(b + 2)));
            left.write(&mut db, first.clone()).unwrap();
            right.write(&mut db, second.clone()).unwrap();
            left.commit(&mut db, &cx).await.unwrap();
            let before = db.frontier().unwrap();
            right
                .commit_disjoint_fields_rebased(&mut db, &cx, 1)
                .await
                .unwrap();
            assert_eq!(db.delta_since(before).unwrap().count(), 1);
            serial.write(&cx, first).await.unwrap();
            serial.write(&cx, second).await.unwrap();
            let vertices = serial.vertices().unwrap();
            let edges = serial.edges().unwrap();
            assert_eq!(db.vertices().unwrap(), vertices);
            assert_eq!(db.edges().unwrap(), edges);
            assert!(right.point_reads.borrow().is_empty());
            drop(db);
            let db = Database::open_with_vfs(&cx, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(db.vertices().unwrap(), vertices);
            assert_eq!(db.edges().unwrap(), edges);
            assert_eq!(tcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unrelated_fields_can_refresh_but_full_getters_remain_whole_object_reads() {
    let ((), report) = run_async_under_lab(0xf13d_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        for edge in [false, true] {
            for broad in [false, true] {
                let mut db = database(&cx).await;
                let mut txn = db.begin(&tcx).unwrap();
                let value = read(&txn, &db, edge, P);
                if broad {
                    if edge {
                        txn.edge(&db, EId(1)).unwrap();
                    } else {
                        txn.vertex(&db, VId(1)).unwrap();
                    }
                }
                db.write(&cx, set(edge, Q, Some(CanonicalScalar::Int(99))))
                    .await
                    .unwrap();
                let basis = txn.basis();
                // Point reads remain pinned until the EXPLICIT refresh.
                assert_eq!(read(&txn, &db, edge, P), value);
                if broad {
                    read_conflict(txn.refresh_snapshot(&db, &tcx));
                    assert_eq!(txn.basis(), basis);
                    read_conflict(txn.finish(&mut db, &cx).await);
                } else {
                    assert_eq!(
                        txn.refresh_snapshot(&db, &tcx).unwrap(),
                        db.frontier().unwrap()
                    );
                    assert_eq!(read(&txn, &db, edge, P), value);
                    let frontier = db.frontier().unwrap();
                    assert!(matches!(
                        txn.finish(&mut db, &cx).await.unwrap(),
                        EmbeddedTxnCompletion::ReadClosed { .. }
                    ));
                    assert_eq!(db.frontier().unwrap(), frontier);
                }
                assert!(txn.point_reads.borrow().is_empty());
                assert_eq!(tcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn changed_and_restored_properties_conflict_on_every_completion_policy() {
    let ((), report) = run_async_under_lab(0xf13d_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        for edge in [false, true] {
            for policy in 0..4 {
                let mut db = database(&cx).await;
                let mut txn = db.begin(&tcx).unwrap();
                let original = read(&txn, &db, edge, P);
                if policy == 1 {
                    txn.write(&mut db, set(edge, Q, Some(CanonicalScalar::Int(77))))
                        .unwrap();
                } else if policy == 2 {
                    let mut append = WriteBatch::new(R);
                    append.add_edge(EId(9), VId(1), VId(3), vec![]);
                    txn.write(&mut db, append).unwrap();
                }
                db.write(&cx, set(edge, P, Some(CanonicalScalar::Int(101))))
                    .await
                    .unwrap();
                db.write(&cx, set(edge, P, original.clone())).await.unwrap();
                let frontier = db.frontier().unwrap();
                assert_eq!(read(&txn, &db, edge, P), original);
                match policy {
                    0 => read_conflict(txn.finish(&mut db, &cx).await),
                    1 => read_conflict(txn.commit_disjoint_fields_rebased(&mut db, &cx, 1).await),
                    2 => read_conflict(txn.commit_append_only_rebased(&mut db, &cx, 1).await),
                    _ => {
                        read_conflict(txn.refresh_snapshot(&db, &tcx));
                        assert_eq!(txn.state(), EmbeddedTxnState::Active);
                        assert!(!txn.point_reads.borrow().is_empty());
                        txn.abort();
                    }
                }
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.edge(EId(9)).unwrap().is_none());
                assert_eq!(tcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn absence_and_cascade_lifetimes_survive_savepoint_rollback() {
    let ((), report) = run_async_under_lab(0xf13d_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        for case in 0..5 {
            let mut db = database(&cx).await;
            let mut txn = db.begin(&tcx).unwrap();
            txn.savepoint(&db, "before_read").unwrap();
            let mut winner = WriteBatch::new(R);
            match case {
                0 => {
                    assert_eq!(
                        txn.vertex_property(&db, VId(1), PropertyKeyId(9)).unwrap(),
                        None
                    );
                    winner.set_vertex_property(
                        VId(1),
                        PropertyKeyId(9),
                        Some(CanonicalScalar::Int(1)),
                    );
                }
                1 => {
                    assert_eq!(
                        txn.edge_property(&db, EId(1), PropertyKeyId(9)).unwrap(),
                        None
                    );
                    winner.set_edge_property(
                        EId(1),
                        PropertyKeyId(9),
                        Some(CanonicalScalar::Int(1)),
                    );
                }
                2 => {
                    assert_eq!(txn.vertex_property(&db, VId(9), P).unwrap(), None);
                    winner.create_vertex(VId(9), vec![], vec![]);
                }
                3 => {
                    assert_eq!(txn.edge_property(&db, EId(9), P).unwrap(), None);
                    winner.add_edge(EId(9), VId(1), VId(2), vec![]);
                }
                _ => {
                    assert!(txn.edge_property(&db, EId(1), P).unwrap().is_some());
                    winner.delete_vertex(VId(2));
                }
            }
            txn.rollback_to_savepoint(&db, "before_read").unwrap();
            txn.release_savepoint(&db, "before_read").unwrap();
            assert!(txn.read_set.borrow().is_empty());
            assert!(!txn.point_reads.borrow().is_empty());
            db.write(&cx, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            read_conflict(txn.finish(&mut db, &cx).await);
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(tcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn point_values_use_canonical_overlay_and_kind_separation() {
    let ((), report) = run_async_under_lab(0xf13d_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        let mut db = database(&cx).await;
        let mut txn = db.begin(&tcx).unwrap();
        let mut changes = set(false, P, Some(CanonicalScalar::Int(42)));
        changes.set_edge_property(EId(1), P, None);
        changes.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(7))]);
        changes.add_edge(EId(7), VId(7), VId(3), vec![(P, CanonicalScalar::Int(8))]);
        changes.ensure_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(-1))]);
        txn.write(&mut db, changes).unwrap();
        assert_eq!(
            txn.vertex_property(&db, VId(1), P).unwrap(),
            Some(CanonicalScalar::Int(42))
        );
        assert_eq!(txn.edge_property(&db, EId(1), P).unwrap(), None);
        assert_eq!(
            txn.vertex_property(&db, VId(7), P).unwrap(),
            Some(CanonicalScalar::Int(7))
        );
        assert_eq!(
            txn.edge_property(&db, EId(7), P).unwrap(),
            Some(CanonicalScalar::Int(8))
        );
        assert!(txn.read_set.borrow().is_empty());
        // Whole-row getters are the independent public output oracle.
        for vid in [VId(1), VId(7)] {
            let row = txn.vertex(&db, vid).unwrap().unwrap();
            assert_eq!(
                txn.vertex_property(&db, vid, P).unwrap(),
                take_point_property(row.props, P)
            );
        }
        let mut retirement = WriteBatch::new(R);
        retirement.delete_vertex(VId(7));
        txn.write(&mut db, retirement).unwrap();
        assert_eq!(txn.vertex_property(&db, VId(7), P).unwrap(), None);
        assert_eq!(txn.edge_property(&db, EId(7), P).unwrap(), None);
        txn.abort();
        assert_eq!(
            take_point_property(vec![(P, CanonicalScalar::Null)], P),
            Some(CanonicalScalar::Null)
        );
        let mut narrow = db.begin(&tcx).unwrap();
        assert_eq!(
            narrow.vertex_property(&db, VId(1), P).unwrap(),
            Some(CanonicalScalar::Int(10))
        );
        db.write(&cx, set(true, P, Some(CanonicalScalar::Int(999))))
            .await
            .unwrap();
        narrow.finish(&mut db, &cx).await.unwrap();
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn field_reads_prevent_write_skew_in_both_commit_orders() {
    let ((), report) = run_async_under_lab(0xf13d_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        for reverse in [false, true] {
            let mut db = database(&cx).await;
            let mut left = db.begin(&tcx).unwrap();
            let mut right = db.begin(&tcx).unwrap();
            assert!(read(&left, &db, false, Q).is_some());
            assert!(read(&right, &db, false, P).is_some());
            left.write(&mut db, set(false, P, Some(CanonicalScalar::Int(0))))
                .unwrap();
            right
                .write(&mut db, set(false, Q, Some(CanonicalScalar::Int(0))))
                .unwrap();
            let (winner, loser) = if reverse {
                (&mut right, &mut left)
            } else {
                (&mut left, &mut right)
            };
            winner
                .commit_disjoint_fields_rebased(&mut db, &cx, 1)
                .await
                .unwrap();
            let frontier = db.frontier().unwrap();
            read_conflict(loser.commit_disjoint_fields_rebased(&mut db, &cx, 1).await);
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(tcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn point_read_lifecycle_and_every_completion_checkpoint_keep_cleanup() {
    let ((), report) = run_async_under_lab(0xf13d_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        let mut db = database(&cx).await;
        let other = database(&cx).await;
        let mut txn = db.begin(&tcx).unwrap();
        assert!(matches!(
            txn.vertex_property(&other, VId(1), P),
            Err(WriteTxnError::WrongDatabase)
        ));
        assert!(matches!(
            txn.edge_property(&other, EId(1), P),
            Err(WriteTxnError::WrongDatabase)
        ));
        assert!(txn.point_reads.borrow().is_empty());
        read(&txn, &db, false, P);
        drop(txn.finish(&mut db, &cx));
        assert_eq!(txn.state(), EmbeddedTxnState::Active);
        assert!(!txn.point_reads.borrow().is_empty());
        let mut winner = set(false, Q, Some(CanonicalScalar::Int(99)));
        winner.delete_vertex(VId(3));
        db.write(&cx, winner).await.unwrap();
        let mut total = 0;
        txn.complete_controlled(&mut db, &cx, None, false, || {
            total += 1;
            Ok(())
        })
        .await
        .unwrap();
        assert!(total > 5);
        assert!(matches!(
            txn.edge_property(&db, EId(1), P),
            Err(WriteTxnError::Finished)
        ));
        for stop in 1..=total {
            let mut db = database(&cx).await;
            let mut txn = db.begin(&tcx).unwrap();
            read(&txn, &db, false, P);
            let mut winner = set(false, Q, Some(CanonicalScalar::Int(99)));
            winner.delete_vertex(VId(3));
            db.write(&cx, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            let mut seen = 0;
            let result = txn
                .complete_controlled(&mut db, &cx, None, false, || {
                    seen += 1;
                    if seen == stop {
                        Err(WriteTxnError::NoPreparedWrite)
                    } else {
                        Ok(())
                    }
                })
                .await;
            assert!(matches!(result, Err(WriteTxnError::NoPreparedWrite)));
            assert_eq!(seen, stop);
            assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
            assert!(txn.point_reads.borrow().is_empty());
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(tcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn point_witness_matches_only_its_field_and_lifetime_and_refuses_unknown_effects() {
    let mut reads = PointReads::default();
    reads.record(ElementId::Edge(EId(5)), PointReadField::Property(P));
    let mut calls = 0;
    let mut check = || {
        calls += 1;
        Ok(())
    };
    for (element, key, expected) in [
        (ElementId::Edge(EId(5)), P, true),
        (ElementId::Edge(EId(5)), Q, false),
        (ElementId::Vertex(VId(5)), P, false),
    ] {
        let row = DeltaRow::Property {
            elem: element,
            property: key,
            before: None,
            after: None,
        };
        assert_eq!(
            reads.conflict(&row, &mut check).unwrap().is_some(),
            expected
        );
    }
    let cascade = DeltaRow::DeleteVertex {
        vid: VId(1),
        before_version: ObjectId([1; 32]),
        sorted_retired_incident_edges: vec![EId(2), EId(5)],
    };
    assert_eq!(
        reads.conflict(&cascade, &mut check).unwrap(),
        Some(ElementId::Edge(EId(5)))
    );
    let schema = DeltaRow::Schema {
        transition_oid: ObjectId([2; 32]),
        before_epoch: SchemaEpoch(1),
        after_epoch: SchemaEpoch(2),
    };
    assert_eq!(
        reads.conflict(&schema, &mut check).unwrap(),
        Some(ElementId::Edge(EId(5)))
    );
    assert!(calls >= 7);
    reads.clear();
    assert_eq!(
        reads
            .conflict(&schema, &mut || panic!("no point-read work"))
            .unwrap(),
        None
    );
}
