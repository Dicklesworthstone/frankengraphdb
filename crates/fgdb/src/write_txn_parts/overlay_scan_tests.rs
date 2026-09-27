//! Differential sparse/bulk reads plus borrowing, lifecycle and refusal laws.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa7; 32], DatabaseSecurityNamespaceId([0xa8; 32]), [0xa9; 32])
}

async fn seed(cx: &CommitCx, count: u128) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for id in 1..=count {
        batch.create_vertex(VId(id), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(10))]);
        if id > 1 {
            batch.add_edge(EId(id - 1), VId(id - 1), VId(id), vec![(P, CanonicalScalar::Int(20))]);
        }
    }
    db.write(cx, batch).await.unwrap();
    db
}

fn collect(owner: &OverlayRows<'_, MemVfs>) -> (Vec<VertexRow>, Vec<EdgeRecord>) {
    let mut vertices = Vec::new();
    let mut edges = Vec::new();
    owner.visit_vertices(&mut |_| Ok::<_, ()>(()), |row, _| {
        vertices.push(row.clone());
        Ok(())
    }).unwrap();
    if let Some(result) = owner.visit_edges(&mut |_| Ok::<_, ()>(()), |entry, props, _| {
        edges.push(EdgeRecord { entry: entry.clone(), props: props.to_vec() });
        Ok(())
    }) {
        result.unwrap();
    }
    (vertices, edges)
}

#[test]
fn canonical_sparse_rows_match_bulk_effects_and_conflict_witnesses() {
    let ((), report) = run_async_under_lab(0xa701, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = seed(&cx, 96).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut batch = WriteBatch::new(R);
        for id in [0, u128::MAX] {
            batch.create_vertex(VId(id), vec![LabelId(3)], vec![(P, CanonicalScalar::Int(30))]);
        }
        batch.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(40)));
        batch.set_vertex_label(VId(3), LabelId(2), true);
        batch.delete_vertex(VId(5));
        batch.set_edge_property(EId(10), Q, Some(CanonicalScalar::Int(50)));
        batch.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![(Q, CanonicalScalar::Int(999))]);
        batch.add_edge(EId(0), VId(0), VId(1), vec![]);
        batch.add_edge(EId(u128::MAX), VId(u128::MAX), VId(u128::MAX), vec![]);
        batch.create_vertex(VId(1000), vec![], vec![]);
        batch.delete_vertex(VId(1000));
        txn.write(&mut db, batch).unwrap();
        let mut suffix = WriteBatch::new(S);
        suffix.add_edge(EId(300), VId(0), VId(u128::MAX), vec![]);
        txn.write_ordered(&mut db, vec![suffix]).unwrap();

        // Independent incumbent reconstruction owns its entire base map and
        // applies each net effect in place, rather than merging sparse streams.
        let expected = (txn.vertices(&db).unwrap(), txn.edges(&db).unwrap());
        let reads = txn.read_set.borrow().clone();
        let expansions = txn.match_expansions.borrow().clone();
        txn.read_set.borrow_mut().clear();
        txn.match_expansions.borrow_mut().clear();
        txn.scanned_vertices.set(false);
        txn.scanned_edges.set(false);
        {
            let owner = OverlayRows::new(&txn, &db, true, &mut || Ok(())).unwrap();
            assert_eq!(collect(&owner), expected);
            assert!(owner.vertices.len() < 10);
            assert!(owner.edges.as_ref().unwrap().len() < 10);
            assert_eq!(*txn.read_set.borrow(), reads);
            assert_eq!(*txn.match_expansions.borrow(), expansions);
            assert!(txn.scanned_vertices.get() && txn.scanned_edges.get());
        }
        assert!(!expected.0.iter().any(|row| row.vid == VId(1000)));
        assert!(!expected.1.iter().any(|row| row.entry.eid == EId(999)));
        assert!(reads.contains(&ElementId::Vertex(VId(1000))));
        assert!(reads.contains(&ElementId::Edge(EId(999))));
        txn.commit(&mut db, &cx).await.unwrap();
        assert!(db.vertex(VId(5)).unwrap().is_none());
        assert!(db.edge(EId(4)).unwrap().is_none() && db.edge(EId(5)).unwrap().is_none());
        assert_eq!(db.edge(EId(1)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(20))]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unchanged_payloads_are_borrowed_and_vertex_only_reads_do_not_build_edges() {
    let ((), report) = run_async_under_lab(0xa702, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = seed(&cx, 128).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(11)));
        batch.set_edge_property(EId(2), P, Some(CanonicalScalar::Int(21)));
        txn.write(&mut db, batch).unwrap();
        {
            let owner = OverlayRows::new(&txn, &db, false, &mut || Ok(())).unwrap();
            assert_eq!(owner.vertices.len(), 1);
            assert!(owner.edges.is_none());
            let mut borrowed = 0;
            owner.visit_vertices(&mut |_| Ok::<_, ()>(()), |row, _| {
                let basis = source::find_vertex(
                    &db.snapshot.patches, row.vid, txn.basis, &mut |_| Ok::<_, ()>(()),
                ).unwrap().unwrap();
                if row.vid == VId(2) {
                    assert!(!std::ptr::eq(row, basis));
                    assert_eq!(row.props, vec![(P, CanonicalScalar::Int(11))]);
                } else {
                    assert!(std::ptr::eq(row, basis), "unchanged payload was copied");
                    borrowed += 1;
                }
                Ok(())
            }).unwrap();
            assert_eq!(borrowed, 127);
            assert!(owner.visit_edges(&mut |_| Ok::<_, ()>(()), |_, _, _| Ok(())).is_none());
            assert!(!txn.scanned_edges.get());
        }
        // An edge-enabled owner likewise keeps only the one changed edge.
        {
            let owner = OverlayRows::new(&txn, &db, true, &mut || Ok(())).unwrap();
            assert_eq!(owner.edges.as_ref().unwrap().len(), 1);
            let mut original = BTreeMap::new();
            source::visit_edges_with_properties(&db.snapshot, txn.basis,
                &mut |_| Ok::<_, ()>(()), |entry, props, _| {
                    original.insert(entry.eid, (entry, props));
                    Ok(())
                },
            ).unwrap();
            owner.visit_edges(&mut |_| Ok::<_, ()>(()), |entry, props, _| {
                if entry.eid != EId(2) {
                    let (old, old_props) = original[&entry.eid];
                    assert!(std::ptr::eq(entry, old));
                    assert!(std::ptr::eq(props, old_props));
                }
                Ok(())
            }).unwrap().unwrap();
        }
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_basis_and_staged_tombstones_never_fall_back_to_newer_rows() {
    let ((), report) = run_async_under_lab(0xa703, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = seed(&cx, 8).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.delete_vertex(VId(2));
        batch.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(30)));
        txn.write(&mut db, batch).unwrap();
        let expected = (txn.vertices(&db).unwrap(), txn.edges(&db).unwrap());
        let mut live = WriteBatch::new(R);
        live.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(300)));
        live.create_vertex(VId(9), vec![], vec![]);
        live.add_edge(EId(9), VId(1), VId(9), vec![]);
        db.write(&cx, live).await.unwrap();
        {
            let owner = OverlayRows::new(&txn, &db, true, &mut || Ok(())).unwrap();
            assert_eq!(collect(&owner), expected);
            assert!(!expected.0.iter().any(|row| row.vid == VId(2) || row.vid == VId(9)));
        }
        let frontier = db.frontier().unwrap();
        assert!(txn.commit(&mut db, &cx).await.is_err());
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_sparse_preparation_and_scan_poll_can_stop_without_changing_staged_state() {
    let ((), report) = run_async_under_lab(0xa704, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = seed(&cx, 12).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(33)));
        batch.set_edge_property(EId(4), P, Some(CanonicalScalar::Int(44)));
        batch.delete_vertex(VId(7));
        txn.write(&mut db, batch).unwrap();
        let expected = (txn.vertices(&db).unwrap(), txn.edges(&db).unwrap());
        let frontier = db.frontier().unwrap();
        let run = |stop| -> Result<usize, usize> {
            let calls = Cell::new(0);
            let poll = || {
                let next = calls.get() + 1;
                calls.set(next);
                if next == stop { Err(next) } else { Ok(()) }
            };
            let owner = OverlayRows::new(&txn, &db, true, &mut || {
                poll().map_err(|_| WriteTxnError::AuthorizedMutationRefused)
            }).map_err(|error| {
                assert!(matches!(error, WriteTxnError::AuthorizedMutationRefused));
                calls.get()
            })?;
            owner.visit_vertices(&mut |_| poll(), |_, _| Ok(()))?;
            owner.visit_edges(&mut |_| poll(), |_, _, _| Ok(())).unwrap()?;
            Ok(calls.get())
        };
        let total = run(usize::MAX).unwrap();
        assert!(total > 20);
        for stop in 1..=total {
            assert_eq!(run(stop), Err(stop));
            assert_eq!(db.frontier().unwrap(), frontier);
        }
        assert_eq!((txn.vertices(&db).unwrap(), txn.edges(&db).unwrap()), expected);
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refusal_and_unwind_stop_before_the_next_output_and_leave_the_owner_reusable() {
    let ((), report) = run_async_under_lab(0xa705, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = seed(&cx, 32).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(0), vec![], vec![]);
        batch.add_edge(EId(0), VId(0), VId(1), vec![]);
        txn.write(&mut db, batch).unwrap();
        {
            let owner = OverlayRows::new(&txn, &db, true, &mut || Ok(())).unwrap();
            let expected = collect(&owner);
            let mut visits = 0;
            let result = owner.visit_vertices(&mut |_| Ok(()), |_, _| {
                visits += 1;
                Err(17)
            });
            assert_eq!(result, Err(17));
            assert_eq!(visits, 1);
            let mut visits = 0;
            let mut edge_polls = 0;
            let result = owner.visit_edges(&mut |_| {
                edge_polls += 1;
                Ok(())
            }, |_, _, _| {
                visits += 1;
                Err(23)
            }).unwrap();
            assert_eq!(result, Err(23));
            assert_eq!(visits, 1);
            assert!(edge_polls < 16, "a refused first row scanned the raw suffix: {edge_polls}");
            let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = owner.visit_vertices(&mut |_| Ok::<_, ()>(()), |_, _| {
                    panic!("injected consumer unwind")
                });
            }));
            assert!(unwind.is_err());
            assert_eq!(collect(&owner), expected);
        }
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn foreign_and_finished_transactions_refuse_before_copying_or_calling_controls() {
    let ((), report) = run_async_under_lab(0xa706, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = seed(&cx, 4).await;
        let foreign = seed(&cx, 4).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut controls = 0;
        let error = OverlayRows::new(&txn, &foreign, true, &mut || {
            controls += 1;
            Ok(())
        }).err().unwrap();
        assert!(matches!(error, WriteTxnError::WrongDatabase));
        assert_eq!(controls, 0);
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
        txn.write(&mut db, batch).unwrap();
        txn.commit(&mut db, &cx).await.unwrap();
        let error = OverlayRows::new(&txn, &db, true, &mut || {
            controls += 1;
            Ok(())
        }).err().unwrap();
        assert!(matches!(error, WriteTxnError::Finished));
        assert_eq!(controls, 0);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
