use super::*;
use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{DeltaRow, PropertyKeyId};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const LARGE: PropertyKeyId = PropertyKeyId(99);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa4; 32], DatabaseSecurityNamespaceId([0xa5; 32]), [0xa6; 32])
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 0..=2 {
        batch.create_vertex(VId(id), vec![LabelId(1), LabelId(2)], vec![
            (P, CanonicalScalar::Null),
            (Q, CanonicalScalar::Int(7)),
            (LARGE, CanonicalScalar::bytes(vec![0x35; 32_768]).unwrap()),
        ]);
    }
    batch.add_edge(EId(0), VId(0), VId(1), vec![(Q, CanonicalScalar::Int(8)),
        (LARGE, CanonicalScalar::bytes(vec![0x53; 32_768]).unwrap())]);
    batch.add_edge(EId(u128::MAX), VId(1), VId(2), vec![(P, CanonicalScalar::Null)]);
    db.write(cx, batch).await.unwrap();
}

fn compare_with_full_rows(txn: &WriteTxn, db: &Database<MemVfs>) {
    let previous_reads = txn.read_set.borrow().clone();
    let previous_expansions = txn.match_expansions.borrow().clone();
    let previous_scans = (txn.scanned_vertices.get(), txn.scanned_edges.get());
    for id in [0, 1, 2, 99, 300, u128::MAX] {
        for key in [P, Q, PropertyKeyId(3), LARGE] {
            let vertex = txn.point_vertex(db, VId(id)).unwrap()
                .and_then(|row| row.props.into_iter().find_map(|(k, v)| (k == key).then_some(v)));
            let edge = txn.point_edge(db, EId(id)).unwrap()
                .and_then(|row| row.props.into_iter().find_map(|(k, v)| (k == key).then_some(v)));
            assert_eq!(txn.vertex_property(db, VId(id), key).unwrap(), vertex, "vertex {id}:{key:?}");
            assert_eq!(txn.edge_property(db, EId(id), key).unwrap(), edge, "edge {id}:{key:?}");
        }
        for label in [LabelId(1), LabelId(2), LabelId(3), LabelId(99)] {
            let expected = txn.point_vertex(db, VId(id)).unwrap()
                .map(|row| row.labels.binary_search(&label).is_ok());
            assert_eq!(txn.vertex_has_label(db, VId(id), label).unwrap(), expected);
        }
    }
    // The independent row oracle does not add broad observations, and neither
    // may the projection. These reads remain eligible for precise validation.
    assert_eq!(*txn.read_set.borrow(), previous_reads);
    assert_eq!(*txn.match_expansions.borrow(), previous_expansions);
    assert_eq!((txn.scanned_vertices.get(), txn.scanned_edges.get()), previous_scans);
}

#[test]
fn projected_fields_match_canonical_rows_through_noops_cascades_and_rollback() {
    let ((), report) = run_async_under_lab(0x901a_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let mut txn = db.begin(&txcx).unwrap();
        txn.savepoint(&db, "before").unwrap();
        compare_with_full_rows(&txn, &db);
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(0), Q, Some(CanonicalScalar::Int(42)));
        changes.set_vertex_property(VId(1), P, None);
        changes.set_vertex_label(VId(1), LabelId(1), false);
        changes.set_vertex_label(VId(1), LabelId(3), true);
        changes.ensure_vertex(VId(0), vec![LabelId(99)], vec![(Q, CanonicalScalar::Int(-1))]);
        changes.compare_and_set_vertex_property(VId(1), Q,
            Some(CanonicalScalar::Int(-1)), CanonicalScalar::Int(-2), WriteMismatchPolicy::NoOp);
        changes.set_edge_property(EId(0), Q, Some(CanonicalScalar::Null));
        changes.create_vertex(VId(u128::MAX), vec![LabelId(3)], vec![(Q, CanonicalScalar::Int(17))]);
        changes.create_vertex(VId(99), vec![], vec![]);
        changes.delete_vertex(VId(99));
        changes.delete_vertex(VId(2));
        txn.write(&mut db, changes).unwrap();
        compare_with_full_rows(&txn, &db);
        assert_eq!(txn.vertex_property(&db, VId(1), P).unwrap(), None);
        assert_eq!(txn.edge_property(&db, EId(0), Q).unwrap(), Some(CanonicalScalar::Null));
        assert_eq!(txn.edge_property(&db, EId(u128::MAX), P).unwrap(), None);
        txn.rollback_to_savepoint(&db, "before").unwrap();
        compare_with_full_rows(&txn, &db);
        assert_eq!(txn.edge_property(&db, EId(u128::MAX), P).unwrap(), Some(CanonicalScalar::Null));
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn requested_payload_is_borrowed_from_its_actual_snapshot_or_prepared_effect() {
    let ((), report) = run_async_under_lab(0x901a_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let mut txn = db.begin(&txcx).unwrap();
        {
            let projection = txn.point_projection(&db, ElementId::Vertex(VId(0)),
                PointReadField::Property(LARGE)).unwrap();
            let row = crate::gql_exec::source::find_vertex(&db.snapshot.patches, VId(0), txn.basis(),
                &mut |_| Ok::<_, ()>(())).unwrap().unwrap();
            let expected = &row.props.iter().find(|(key, _)| *key == LARGE).unwrap().1;
            assert!(core::ptr::eq(projection.property.unwrap(), expected));
            let projection = txn.point_projection(&db, ElementId::Edge(EId(0)),
                PointReadField::Property(LARGE)).unwrap();
            let (block, row) = db.snapshot.adjacency_index
                .statement_at(&db.snapshot.blocks, EId(0), txn.basis()).unwrap();
            let props = db.snapshot.block_props[block].as_ref().unwrap();
            let fields = &props.rows[usize::from(props.locators[row]) - 1];
            let expected = &fields.iter().find(|(key, _)| *key == LARGE).unwrap().1;
            assert!(core::ptr::eq(projection.property.unwrap(), expected));
        }
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(0), LARGE, Some(CanonicalScalar::bytes(vec![0x73; 4096]).unwrap()));
        changes.set_edge_property(EId(0), LARGE, Some(CanonicalScalar::bytes(vec![0x74; 4096]).unwrap()));
        txn.write(&mut db, changes).unwrap();
        for element in [ElementId::Vertex(VId(0)), ElementId::Edge(EId(0))] {
            let projection = txn.point_projection(&db, element, PointReadField::Property(LARGE)).unwrap();
            let expected = txn.prepared.as_ref().unwrap().template.coordinate_entries().iter()
                .flat_map(|coordinate| &coordinate.rows).find_map(|effect| match effect {
                    DeltaRow::Property { elem, property, after, .. } if *elem == element && *property == LARGE => after.as_ref(),
                    _ => None,
                }).unwrap();
            assert!(core::ptr::eq(projection.property.unwrap(), expected));
        }
        assert!(txn.point_reads.borrow().is_empty(), "private borrowed resolution grants no result");
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn old_cut_and_reopened_cut_use_their_own_winning_payloads() {
    let ((), report) = run_async_under_lab(0x901a_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let old = db.begin(&txcx).unwrap();
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(0), Q, Some(CanonicalScalar::Int(100)));
        changes.set_edge_property(EId(0), Q, Some(CanonicalScalar::Int(101)));
        changes.set_vertex_label(VId(0), LabelId(1), false);
        changes.delete_vertex(VId(2));
        let frontier = db.write(&cx, changes).await.unwrap();
        assert_eq!(old.vertex_property(&db, VId(0), Q).unwrap(), Some(CanonicalScalar::Int(7)));
        assert_eq!(old.edge_property(&db, EId(0), Q).unwrap(), Some(CanonicalScalar::Int(8)));
        assert_eq!(old.vertex_has_label(&db, VId(0), LabelId(1)).unwrap(), Some(true));
        compare_with_full_rows(&old, &db);
        old.abort();
        db.compact(&cx).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&cx, vfs, &path, keys()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), frontier);
        let txn = db.begin(&txcx).unwrap();
        compare_with_full_rows(&txn, &db);
        assert_eq!(txn.vertex_property(&db, VId(0), Q).unwrap(), Some(CanonicalScalar::Int(100)));
        assert_eq!(txn.edge_property(&db, EId(0), Q).unwrap(), Some(CanonicalScalar::Int(101)));
        assert_eq!(txn.vertex_has_label(&db, VId(0), LabelId(1)).unwrap(), Some(false));
        assert_eq!(txn.edge_property(&db, EId(u128::MAX), P).unwrap(), None);
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn borrowed_resolution_stops_at_each_history_field_and_effect_checkpoint() {
    let ((), report) = run_async_under_lab(0x901a_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut changes = WriteBatch::new(R);
        changes.delete_vertex(VId(2));
        changes.set_vertex_property(VId(0), Q, Some(CanonicalScalar::Int(42)));
        txn.write(&mut db, changes).unwrap();
        for (element, field) in [
            (ElementId::Vertex(VId(0)), PointReadField::Property(Q)),
            (ElementId::Vertex(VId(1)), PointReadField::Label(LabelId(1))),
            (ElementId::Edge(EId(u128::MAX)), PointReadField::Property(P)),
        ] {
            let mut total = 0;
            txn.point_projection_with_control(&db, element, field, &mut |_| {
                total += 1; Ok::<_, usize>(())
            }, &|_| usize::MAX).unwrap();
            assert!(total > 3);
            for stop in 1..=total {
                let mut seen = 0;
                let result = txn.point_projection_with_control(&db, element, field, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }, &|_| usize::MAX);
                assert!(matches!(result, Err(at) if at == stop));
                assert_eq!(seen, stop);
            }
        }
        assert!(txn.point_reads.borrow().is_empty());
        assert!(txn.read_set.borrow().is_empty());
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
