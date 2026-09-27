use super::*;
use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{DeltaRow, PropertyKeyId, SchemaEpoch};
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId, PurposeContexts};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const ABSENT: PropertyKeyId = PropertyKeyId(3);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}

async fn seed(database: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=4 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![(P, CanonicalScalar::Int(0)), (Q, CanonicalScalar::Int(0))],
        );
    }
    batch.add_edge(
        EId(10), VId(1), VId(2),
        vec![(P, CanonicalScalar::Int(0)), (Q, CanonicalScalar::Int(0))],
    );
    database.write(cx, batch).await.unwrap();
}

fn program() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(50), vec![], vec![(P, CanonicalScalar::Int(1))]);
    batch.set_vertex_property(VId(50), P, Some(CanonicalScalar::Int(7)));
    batch.add_edge(EId(60), VId(1), VId(50), vec![(P, CanonicalScalar::Int(2))]);
    batch.set_edge_property(EId(60), P, Some(CanonicalScalar::Int(9)));
    batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
    batch.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(11)));
    batch.set_vertex_label(VId(1), LabelId(2), true);
    batch.compare_and_set_vertex_property(
        VId(1), P, Some(CanonicalScalar::Int(7)),
        CanonicalScalar::Int(8), WriteMismatchPolicy::AbortWrite,
    );
    batch.set_vertex_property(VId(1), ABSENT, None);
    batch
}

fn concurrent() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(8)));
    batch.set_edge_property(EId(10), Q, Some(CanonicalScalar::Int(10)));
    batch.set_vertex_label(VId(1), LabelId(3), true);
    batch.add_edge(EId(70), VId(1), VId(3), vec![]);
    batch
}

fn assert_conflict(result: Result<CommitSeq, WriteTxnError>, expected_law: &'static str) {
    assert!(matches!(
        result,
        Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law, .. }))
            if law == expected_law
    ));
}

#[test]
fn mixed_creation_updates_and_cas_publish_once_and_match_serial_reopen() {
    let ((), report) = run_async_under_lab(0x91ed_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
            .await.unwrap();
        seed(&mut db, &cx).await;
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let observed = txn.vertex(&db, VId(4)).unwrap().unwrap();
        txn.write_ordered(&mut db, vec![program()]).unwrap();
        let original = txn.prepared.as_ref().unwrap().template.clone();
        let mut ordinary = db.begin(&txcx).unwrap();
        ordinary.write_ordered(&mut db, vec![program()]).unwrap();
        let frontier = db.write(&cx, concurrent()).await.unwrap();
        assert_conflict(ordinary.commit(&mut db, &cx).await, "FG-LAW-FCW-01");
        assert!(db.vertex(VId(50)).unwrap().is_none());
        assert!(db.edge(EId(60)).unwrap().is_none());
        assert_eq!(txn.vertex(&db, VId(4)).unwrap(), Some(observed));
        let seq = txn.commit_mixed_rebased(&mut db, &cx, 64).await.unwrap();
        assert_eq!(seq, CommitSeq(frontier.0 + 1));
        {
            let tail = db.delta_since(frontier).unwrap().collect::<Vec<_>>();
            assert_eq!(tail.len(), 1);
            assert_eq!(tail[0].coordinate_entries(), original.coordinate_entries());
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
        assert!(pinned.vertex(VId(50)).unwrap().is_none());
        assert_eq!(pinned.vertex(VId(1)).unwrap().unwrap().labels, vec![LabelId(1)]);
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().labels, vec![LabelId(1), LabelId(2), LabelId(3)]);
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props[0].1, CanonicalScalar::Int(8));
        assert_eq!(db.vertex(VId(50)).unwrap().unwrap().props[0].1, CanonicalScalar::Int(7));
        assert_eq!(db.edge(EId(60)).unwrap().unwrap().props[0].1, CanonicalScalar::Int(9));
        let mut serial = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut serial, &cx).await;
        serial.write(&cx, concurrent()).await.unwrap();
        let mut serial_txn = serial.begin(&txcx).unwrap();
        serial_txn.write_ordered(&mut serial, vec![program()]).unwrap();
        serial_txn.commit(&mut serial, &cx).await.unwrap();
        assert_eq!(db.vertices().unwrap(), serial.vertices().unwrap());
        assert_eq!(db.edges().unwrap(), serial.edges().unwrap());
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&cx, vfs, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), seq);
        assert_eq!(reopened.vertices().unwrap(), vertices);
        assert_eq!(reopened.edges().unwrap(), edges);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn both_independence_laws_protect_aba_identity_and_cascade_lifetimes() {
    let ((), report) = run_async_under_lab(0x91ed_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..8 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.write_ordered(&mut db, vec![program()]).unwrap();
            let mut first = WriteBatch::new(R);
            let mut second = WriteBatch::new(R);
            match case {
                0 => {
                    first.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
                    second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
                }
                1 => {
                    first.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(9)));
                    second.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(0)));
                }
                2 => {
                    first.set_vertex_label(VId(1), LabelId(2), true);
                    second.set_vertex_label(VId(1), LabelId(2), false);
                }
                3 => {
                    first.create_vertex(VId(50), vec![], vec![]);
                    second.delete_vertex(VId(50));
                }
                4 => {
                    first.add_edge(EId(60), VId(3), VId(4), vec![]);
                    second.delete_edge(EId(60));
                }
                5 => { first.delete_vertex(VId(1)); }
                6 => { first.delete_vertex(VId(2)); }
                _ => { first.delete_edge(EId(10)); }
            }
            db.write(&cx, first).await.unwrap();
            if !second.is_empty() {
                db.write(&cx, second).await.unwrap();
            }
            let frontier = db.frontier().unwrap();
            let before = (db.vertices().unwrap(), db.edges().unwrap());
            assert_conflict(txn.commit_mixed_rebased(&mut db, &cx, 64).await, "FG-LAW-FCW-01");
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
            assert!(txn.staged.is_empty());
            assert!(txn.prepared.is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancelled_assignments_and_noop_guards_protect_their_raw_slots() {
    let ((), report) = run_async_under_lab(0x91ed_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..4 {
            for overlap in [false, true] {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx).await;
                let mut txn = db.begin(&txcx).unwrap();
                let mut batch = WriteBatch::new(R);
                batch.create_vertex(VId(50), vec![], vec![]);
                batch.add_edge(EId(60), VId(1), VId(50), vec![]);
                match case {
                    0 => {
                        batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
                        batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
                    }
                    1 => { batch.set_vertex_property(VId(1), ABSENT, None); }
                    2 => {
                        batch.compare_and_set_vertex_property(VId(1), P,
                            Some(CanonicalScalar::Int(99)), CanonicalScalar::Int(7),
                            WriteMismatchPolicy::NoOp);
                    }
                    _ => {
                        batch.compare_and_set_edge_property(EId(10), P,
                            Some(CanonicalScalar::Int(99)), CanonicalScalar::Int(7),
                            WriteMismatchPolicy::NoOp);
                    }
                }
                txn.write_ordered(&mut db, vec![batch]).unwrap();
                let original = txn.prepared.as_ref().unwrap().template.clone();
                let mut drift = WriteBatch::new(R);
                if overlap && case == 3 {
                    drift.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(8)));
                } else {
                    let key = if overlap { if case == 1 { ABSENT } else { P } } else { Q };
                    drift.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(8)));
                }
                let frontier = db.write(&cx, drift).await.unwrap();
                let result = txn.commit_mixed_rebased(&mut db, &cx, 64).await;
                if overlap {
                    assert_conflict(result, "FG-LAW-FCW-01");
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(50)).unwrap().is_none());
                } else {
                    assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                    let tail = db.delta_since(frontier).unwrap().next().unwrap();
                    assert_eq!(tail.coordinate_entries(), original.coordinate_entries());
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn original_positive_negative_and_empty_scan_observations_cannot_be_rebased_away() {
    let ((), report) = run_async_under_lab(0x91ed_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..5 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut drift = WriteBatch::new(R);
            match case {
                0 => {
                    txn.vertex(&db, VId(4)).unwrap();
                    drift.set_vertex_property(VId(4), Q, Some(CanonicalScalar::Int(9)));
                }
                1 => {
                    assert!(txn.vertex(&db, VId(99)).unwrap().is_none());
                    drift.create_vertex(VId(99), vec![], vec![]);
                }
                2 => {
                    let bind = RelationBind::new().with_label("Empty", LabelId(9));
                    assert!(txn.execute_gql(&db, "MATCH (n:Empty) RETURN n", &bind).unwrap().is_empty());
                    drift.create_vertex(VId(99), vec![LabelId(9)], vec![]);
                }
                3 => {
                    txn.edge(&db, EId(10)).unwrap();
                    drift.set_edge_property(EId(10), Q, Some(CanonicalScalar::Int(9)));
                }
                _ => {
                    assert!(txn.neighbours(&db, VId(3), R).unwrap().is_empty());
                    drift.add_edge(EId(77), VId(3), VId(4), vec![]);
                }
            }
            txn.write_ordered(&mut db, vec![program()]).unwrap();
            let basis = txn.basis();
            let frontier = db.write(&cx, drift).await.unwrap();
            assert_conflict(txn.commit_mixed_rebased(&mut db, &cx, 64).await, "FG-LAW-FCW-READ-01");
            assert_eq!(txn.basis(), basis);
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(VId(50)).unwrap().is_none());
            assert!(db.edge(EId(60)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn raw_ensure_scopes_and_creation_reads_refuse_but_absent_vertex_delete_rebases() {
    let ((), report) = run_async_under_lab(0x91ed_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..6 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut batch = program();
            match case {
                0 => { batch.ensure_vertex(VId(4), vec![], vec![]); }
                1 => { batch.delete_vertex_if_present(VId(99)); }
                _ => {}
            }
            txn.write_ordered(&mut db, vec![batch]).unwrap();
            match case {
                2 => { txn.savepoint(&db, "held").unwrap(); }
                3 => { txn.program_multi_relation = true; }
                4 => { txn.vertex(&db, VId(50)).unwrap(); }
                5 => { txn.edge(&db, EId(60)).unwrap(); }
                _ => {}
            }
            // Drift outside every explicit witness so eligibility, not stale
            // reads, decides the refusal (edge reads also observe their source).
            let mut drift = WriteBatch::new(R);
            drift.set_vertex_property(VId(3), Q, Some(CanonicalScalar::Int(9)));
            let frontier = db.write(&cx, drift).await.unwrap();
            let result = txn.commit_mixed_rebased(&mut db, &cx, 64).await;
            if case == 1 {
                assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                assert!(db.vertex(VId(50)).unwrap().is_some());
                assert!(db.vertex(VId(99)).unwrap().is_none());
            } else {
                assert!(matches!(result, Err(WriteTxnError::MixedRebaseIneligible)));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(50)).unwrap().is_none());
                assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn combined_footprint_is_domain_separated_and_refuses_unknown_history() {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(u128::MAX), vec![], vec![]);
    batch.add_edge(EId(u128::MAX), VId(1), VId(u128::MAX), vec![]);
    batch.set_vertex_property(VId(2), P, None);
    batch.set_edge_property(EId(10), Q, None);
    let mut footprint = MixedRebaseFootprint::default();
    for row in &batch.rows { footprint.record(row).unwrap(); }
    for (elem, property, wanted) in [
        (ElementId::Vertex(VId(u128::MAX)), Q, true),
        (ElementId::Edge(EId(u128::MAX)), Q, true),
        (ElementId::Vertex(VId(u64::MAX.into())), Q, false),
        (ElementId::Vertex(VId(2)), P, true),
        (ElementId::Vertex(VId(2)), Q, false),
        (ElementId::Edge(EId(2)), P, false),
        (ElementId::Edge(EId(10)), Q, true),
        (ElementId::Vertex(VId(10)), Q, false),
    ] {
        let row = DeltaRow::Property { elem, property, before: None,
            after: Some(CanonicalScalar::Int(1)) };
        assert_eq!(footprint.conflicts(&row, &mut || Ok(())).unwrap(), wanted);
    }
    for (vid, edges, wanted) in [
        (VId(1), vec![], true),
        (VId(2), vec![], true),
        (VId(3), vec![EId(10)], true),
        (VId(3), vec![EId(11)], false),
        (VId(3), vec![EId(u128::MAX)], true),
    ] {
        let row = DeltaRow::DeleteVertex { vid, before_version: ObjectId([0; 32]),
            sorted_retired_incident_edges: edges };
        assert_eq!(footprint.conflicts(&row, &mut || Ok(())).unwrap(), wanted);
    }
    let schema = DeltaRow::Schema { transition_oid: ObjectId([0; 32]),
        before_epoch: SchemaEpoch(1), after_epoch: SchemaEpoch(2) };
    assert!(matches!(footprint.conflicts(&schema, &mut || Ok(())),
        Err(WriteTxnError::MixedRebaseIneligible)));
    assert!(matches!(footprint.conflicts(&schema, &mut || Err(WriteTxnError::NoPreparedWrite)),
        Err(WriteTxnError::NoPreparedWrite)));
}

fn replacement() -> Vec<WriteBatch> {
    let mut first = WriteBatch::new(RelationId(9));
    first.create_vertex(VId(5), vec![], vec![]);
    first.add_edge(EId(50), VId(1), VId(5), vec![]);
    let mut second = WriteBatch::new(RelationId(2));
    second.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(7)));
    second.add_edge(EId(60), VId(5), VId(2), vec![]);
    let mut last = WriteBatch::new(R);
    last.delete_edge(EId(10));
    last.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(8)));
    vec![first, second, last]
}

async fn replacement_drifted(cx: &CommitCx, txcx: &TxnCx) -> (Database<MemVfs>, WriteTxn) {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    seed(&mut db, cx).await;
    let mut txn = db.begin(txcx).unwrap();
    txn.vertex(&db, VId(4)).unwrap();
    txn.write_ordered(&mut db, replacement()).unwrap();
    let mut drift = WriteBatch::new(R);
    drift.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(9)));
    db.write(cx, drift).await.unwrap();
    (db, txn)
}

#[test]
fn relationship_replacement_is_one_commit_across_relations_and_reopen() {
    let ((), report) = run_async_under_lab(0x91ed_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write_ordered(&mut db, replacement()).unwrap();
        let original = txn.prepared.as_ref().unwrap().template.clone();
        let mut drift = WriteBatch::new(R);
        drift.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(9)));
        drift.add_edge(EId(70), VId(1), VId(3), vec![]);
        let frontier = db.write(&cx, drift.clone()).await.unwrap();
        let seq = txn.commit_mixed_rebased(&mut db, &cx, 12).await.unwrap();
        assert_eq!(seq, CommitSeq(frontier.0 + 1));
        assert_eq!(db.delta_since(frontier).unwrap().count(), 1);
        assert_eq!(db.delta_since(frontier).unwrap().next().unwrap().coordinate_entries(),
            original.coordinate_entries());
        assert!(pinned.edge(EId(10)).unwrap().is_some());
        assert!(pinned.edge(EId(50)).unwrap().is_none());
        assert!(db.edge(EId(10)).unwrap().is_none());
        assert_eq!(db.edge(EId(50)).unwrap().unwrap().entry.relation, RelationId(9));
        assert_eq!(db.edge(EId(60)).unwrap().unwrap().entry.relation, RelationId(2));
        let mut serial = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut serial, &cx).await;
        serial.write(&cx, drift).await.unwrap();
        let mut serial_txn = serial.begin(&txcx).unwrap();
        serial_txn.write_ordered(&mut serial, replacement()).unwrap();
        serial_txn.commit(&mut serial, &cx).await.unwrap();
        assert_eq!(db.vertices().unwrap(), serial.vertices().unwrap());
        assert_eq!(db.edges().unwrap(), serial.edges().unwrap());
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&cx, vfs, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), seq);
        assert_eq!(reopened.vertices().unwrap(), vertices);
        assert_eq!(reopened.edges().unwrap(), edges);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_retirement_protects_all_fields_noop_identity_decisions_and_cascades() {
    let ((), report) = run_async_under_lab(0x91ed_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..4 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(50), vec![], vec![]);
            batch.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(7)));
            // No append endpoint or field target overlaps edge 10: only the
            // retirement's complete-identity law can reject these histories.
            if case == 2 { batch.delete_edge_if_present(EId(99)); }
            else { batch.delete_edge(EId(10)); }
            txn.write_ordered(&mut db, vec![batch]).unwrap();
            let mut first = WriteBatch::new(R);
            let mut second = WriteBatch::new(R);
            match case {
                0 | 1 => {
                    first.set_edge_property(EId(10), Q, Some(CanonicalScalar::Int(9)));
                    if case == 1 {
                        second.set_edge_property(EId(10), Q, Some(CanonicalScalar::Int(0)));
                    }
                }
                2 => {
                    first.add_edge(EId(99), VId(1), VId(2), vec![]);
                    second.delete_edge(EId(99));
                }
                _ => { first.delete_vertex(VId(2)); }
            }
            db.write(&cx, first).await.unwrap();
            if !second.is_empty() { db.write(&cx, second).await.unwrap(); }
            let before = (db.vertices().unwrap(), db.edges().unwrap());
            let frontier = db.frontier().unwrap();
            assert_conflict(txn.commit_mixed_rebased(&mut db, &cx, 64).await, "FG-LAW-FCW-01");
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancelled_new_edges_keep_raw_identity_guards_without_publishing_them() {
    let ((), report) = run_async_under_lab(0x91ed_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for collision in [false, true] {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut batch = WriteBatch::new(R);
            batch.add_edge(EId(u128::MAX), VId(1), VId(2), vec![]);
            batch.set_edge_property(EId(u128::MAX), P, Some(CanonicalScalar::Int(7)));
            batch.delete_edge(EId(u128::MAX));
            batch.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(8)));
            txn.write_ordered(&mut db, vec![batch]).unwrap();
            let original = txn.prepared.as_ref().unwrap().template.clone();
            let mut first = WriteBatch::new(R);
            if collision {
                first.add_edge(EId(u128::MAX), VId(1), VId(2), vec![]);
                db.write(&cx, first).await.unwrap();
                let mut second = WriteBatch::new(R);
                second.delete_edge(EId(u128::MAX));
                db.write(&cx, second).await.unwrap();
            } else {
                first.set_vertex_property(VId(3), Q, Some(CanonicalScalar::Int(9)));
                db.write(&cx, first).await.unwrap();
            }
            let frontier = db.frontier().unwrap();
            let result = txn.commit_mixed_rebased(&mut db, &cx, 4).await;
            if collision {
                assert_conflict(result, "FG-LAW-FCW-01");
                assert_eq!(db.frontier().unwrap(), frontier);
            } else {
                assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                assert_eq!(db.delta_since(frontier).unwrap().next().unwrap().coordinate_entries(),
                    original.coordinate_entries());
            }
            assert!(db.edge(EId(u128::MAX)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn complete_relation_expansion_is_bounded_before_replacement_is_prepared() {
    let ((), report) = run_async_under_lab(0x91ed_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        // Three vertex instructions across {1,2,9}, plus three edge instructions.
        for limit in [11, 12, 13] {
            let (mut db, mut txn) = replacement_drifted(&cx, &txcx).await;
            let frontier = db.frontier().unwrap();
            let result = txn.commit_mixed_rebased(&mut db, &cx, limit).await;
            if limit == 11 {
                assert!(matches!(result, Err(WriteTxnError::OrderedWriteBudgetExceeded {
                    limit: 11, required: 12
                })));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.edge(EId(10)).unwrap().is_some());
                assert!(db.edge(EId(50)).unwrap().is_none());
            } else {
                assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                assert!(db.edge(EId(10)).unwrap().is_none());
                assert!(db.edge(EId(50)).unwrap().is_some());
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn each_checkpoint_and_unwind_leave_no_partial_relationship_replacement() {
    use std::future::Future;
    use std::task::Poll;
    let ((), report) = run_async_under_lab(0x91ed_0014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let (mut db, mut txn) = replacement_drifted(&cx, &txcx).await;
        let mut total = 0;
        txn.complete_rebased_controlled(&mut db, &cx, None, true,
            Some(RebasePreparation::Mixed(12)), || { total += 1; Ok(()) })
            .await.unwrap();
        assert!(total > 20);
        for stop in 1..=total {
            let (mut db, mut txn) = replacement_drifted(&cx, &txcx).await;
            let frontier = db.frontier().unwrap();
            let before = (db.vertices().unwrap(), db.edges().unwrap());
            let mut seen = 0;
            let result = txn.complete_rebased_controlled(&mut db, &cx, None, true,
                Some(RebasePreparation::Mixed(12)), || {
                    seen += 1;
                    if seen == stop { Err(WriteTxnError::NoPreparedWrite) } else { Ok(()) }
                }).await;
            assert!(matches!(result, Err(WriteTxnError::NoPreparedWrite)));
            assert_eq!(seen, stop);
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
            assert!(txn.prepared.is_none());
            assert!(txn.staged.is_empty());
            assert!(txn.pin.is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        let (mut db, mut txn) = replacement_drifted(&cx, &txcx).await;
        let frontier = db.frontier().unwrap();
        let mut seen = 0;
        let mut future = Box::pin(txn.complete_rebased_controlled(&mut db, &cx, None, true,
            Some(RebasePreparation::Mixed(12)), || {
                seen += 1;
                assert_ne!(seen, total, "unwind at final acceptance");
                Ok(())
            }));
        let panicked = std::future::poll_fn(|task| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| future.as_mut().poll(task)));
            Poll::Ready(result.is_err())
        }).await;
        drop(future);
        assert!(panicked);
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.edge(EId(10)).unwrap().is_some());
        assert!(db.edge(EId(50)).unwrap().is_none());
        assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owner_unpolled_and_native_crash_outcomes_preserve_the_single_commit_contract() {
    use fgdb_chronicle::commit::CrashPoint;
    let ((), report) = run_async_under_lab(0x91ed_0015, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let (mut db, mut txn) = replacement_drifted(&cx, &txcx).await;
        let original = txn.prepared.as_ref().unwrap().template.clone();
        let basis = txn.basis();
        drop(txn.commit_mixed_rebased(&mut db, &cx, 12));
        let mut foreign = Database::open_memory(&cx, keys()).await.unwrap();
        assert!(matches!(txn.commit_mixed_rebased(&mut foreign, &cx, 12).await,
            Err(WriteTxnError::WrongDatabase)));
        assert_eq!(txn.basis(), basis);
        assert_eq!(txn.state(), EmbeddedTxnState::Active);
        assert_eq!(txn.prepared.as_ref().unwrap().template, original);
        txn.commit_mixed_rebased(&mut db, &cx, 12).await.unwrap();
        assert!(matches!(txn.commit_mixed_rebased(&mut db, &cx, 12).await,
            Err(WriteTxnError::Finished)));
        for crash in [CrashPoint::BeforeCapsule, CrashPoint::AfterMarkerBeforeD2] {
            let (mut db, mut txn) = replacement_drifted(&cx, &txcx).await;
            let frontier = db.frontier().unwrap();
            let result = txn.complete_rebased_controlled(&mut db, &cx, Some(crash), true,
                Some(RebasePreparation::Mixed(12)), || Ok(())).await;
            assert!(result.is_err());
            let expected = if crash == CrashPoint::BeforeCapsule {
                EmbeddedTxnState::Aborted
            } else {
                EmbeddedTxnState::CommitOutcomeUnknown { published_frontier: frontier }
            };
            assert_eq!(txn.state(), expected);
            assert!(txn.prepared.is_none());
            assert!(txn.pin.is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retirement_cascade_checks_preserve_every_control_refusal() {
    let mut batch = WriteBatch::new(R);
    batch.delete_edge_if_present(EId(u128::MAX));
    let mut footprint = MixedRebaseFootprint::default();
    for row in &batch.rows {
        footprint.record(row).unwrap();
    }
    let row = DeltaRow::DeleteVertex {
        vid: VId(99),
        before_version: ObjectId([0; 32]),
        sorted_retired_incident_edges: (1..=64).map(EId)
            .chain([EId(u128::MAX)]).collect(),
    };
    let mut total = 0;
    assert!(footprint.conflicts(&row, &mut || { total += 1; Ok(()) }).unwrap());
    assert!(total > 65);
    for stop in 1..=total {
        let mut seen = 0;
        let result = footprint.conflicts(&row, &mut || {
            seen += 1;
            if seen == stop { Err(WriteTxnError::NoPreparedWrite) } else { Ok(()) }
        });
        assert!(matches!(result, Err(WriteTxnError::NoPreparedWrite)));
        assert_eq!(seen, stop);
    }
}
