use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{DeltaRow, PropertyKeyId};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const NEW: u128 = u128::MAX;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 1..=6 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![(P, CanonicalScalar::Int(0)), (Q, CanonicalScalar::Int(0))],
        );
    }
    // Outgoing, incoming, self-loop, parallel, and unrelated edges.
    for (eid, src, dst) in [(10, 1, 2), (11, 2, 1), (12, 1, 1), (13, 1, 2), (30, 4, 5)] {
        batch.add_edge(
            EId(eid),
            VId(src),
            VId(dst),
            vec![(Q, CanonicalScalar::Int(0))],
        );
    }
    db.write(cx, batch).await.unwrap();
    let mut other = WriteBatch::new(RelationId(2));
    other.add_edge(EId(20), VId(3), VId(1), vec![(Q, CanonicalScalar::Int(0))]);
    other.add_edge(EId(21), VId(1), VId(3), vec![]);
    db.write(cx, other).await.unwrap();
}

fn replacement() -> Vec<WriteBatch> {
    let mut first = WriteBatch::new(RelationId(1));
    first.delete_vertex(VId(1));
    first.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(7)));
    first.create_vertex(VId(NEW), vec![LabelId(9)], vec![]);
    let mut second = WriteBatch::new(RelationId(2));
    second.add_edge(EId(NEW), VId(4), VId(NEW), vec![]);
    vec![first, second]
}

fn unrelated() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.set_vertex_property(VId(4), Q, Some(CanonicalScalar::Int(8)));
    batch
}

async fn drifted(cx: &CommitCx, txcx: &TxnCx) -> (Database<MemVfs>, WriteTxn) {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    seed(&mut db, cx).await;
    let mut txn = db.begin(txcx).unwrap();
    txn.vertex(&db, VId(6)).unwrap();
    txn.write_ordered(&mut db, replacement()).unwrap();
    db.write(cx, unrelated()).await.unwrap();
    (db, txn)
}

#[test]
fn vertex_replacement_preserves_exact_cascades_single_publication_and_reopen() {
    let ((), report) = run_async_under_lab(0x71ca_0001, |root| async move {
        let purposes = PurposeContexts::narrow_runtime_root(&root);
        let cx = purposes.commit();
        let txcx = purposes.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &cx).await;
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.vertex(&db, VId(6)).unwrap();
        txn.write_ordered(&mut db, replacement()).unwrap();
        let original = txn.prepared.as_ref().unwrap().template.clone();
        let mut ordinary = db.begin(&txcx).unwrap();
        ordinary.write_ordered(&mut db, replacement()).unwrap();
        let frontier = db.write(&cx, unrelated()).await.unwrap();
        assert!(matches!(
            ordinary.commit(&mut db, &cx).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
        ));
        // Three vertex instructions replicated across two relations, one edge.
        let seq = txn.commit_mixed_rebased(&mut db, &cx, 7).await.unwrap();
        assert_eq!(seq, CommitSeq(frontier.0 + 1));
        assert_eq!(db.delta_since(frontier).unwrap().count(), 1);
        assert_eq!(
            db.delta_since(frontier)
                .unwrap()
                .next()
                .unwrap()
                .coordinate_entries(),
            original.coordinate_entries()
        );
        assert!(pinned.vertex(VId(1)).unwrap().is_some());
        assert!(pinned.vertex(VId(NEW)).unwrap().is_none());
        assert!(db.vertex(VId(1)).unwrap().is_none());
        for eid in [10, 11, 12, 13, 20, 21] {
            assert!(db.edge(EId(eid)).unwrap().is_none());
            assert!(pinned.edge(EId(eid)).unwrap().is_some());
        }
        assert!(db.edge(EId(30)).unwrap().is_some());
        assert!(db.edge(EId(NEW)).unwrap().is_some());
        let mut serial = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut serial, &cx).await;
        serial.write(&cx, unrelated()).await.unwrap();
        let mut ordered = serial.begin(&txcx).unwrap();
        ordered.write_ordered(&mut serial, replacement()).unwrap();
        ordered.commit(&mut serial, &cx).await.unwrap();
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        assert_eq!(vertices, serial.vertices().unwrap());
        assert_eq!(edges, serial.edges().unwrap());
        drop(db);
        let reopened = Database::open_with_vfs(&cx, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.frontier().unwrap(), seq);
        assert_eq!(reopened.vertices().unwrap(), vertices);
        assert_eq!(reopened.edges().unwrap(), edges);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_vertex_and_incident_edge_changes_conflict_including_transient_phantoms() {
    let ((), report) = run_async_under_lab(0x71ca_0002, |root| async move {
        let purposes = PurposeContexts::narrow_runtime_root(&root);
        let cx = purposes.commit();
        let txcx = purposes.txn();
        for case in 0..10 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.vertex(&db, VId(6)).unwrap();
            txn.write_ordered(&mut db, replacement()).unwrap();
            let mut change = WriteBatch::new(RelationId(9));
            let mut restore = WriteBatch::new(RelationId(9));
            match case {
                0 => {
                    change.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(9)));
                    restore.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(0)));
                }
                1 => {
                    change.set_vertex_label(VId(1), LabelId(9), true);
                    restore.set_vertex_label(VId(1), LabelId(9), false);
                }
                2..=4 => {
                    let (src, dst) = match case {
                        2 => (4, 1),
                        3 => (1, 4),
                        _ => (1, 1),
                    };
                    change.add_edge(EId(99), VId(src), VId(dst), vec![]);
                    restore.delete_edge(EId(99));
                }
                5 => {
                    change.set_edge_property(EId(20), Q, Some(CanonicalScalar::Int(9)));
                    restore.set_edge_property(EId(20), Q, Some(CanonicalScalar::Int(0)));
                }
                6 => {
                    change.delete_edge(EId(20));
                }
                7 => {
                    change.delete_vertex(VId(3));
                }
                8 => {
                    change.delete_vertex(VId(1));
                }
                _ => {
                    change.set_vertex_property(VId(6), Q, Some(CanonicalScalar::Int(9)));
                }
            }
            db.write(&cx, change).await.unwrap();
            if !restore.is_empty() {
                db.write(&cx, restore).await.unwrap();
            }
            let before = (db.vertices().unwrap(), db.edges().unwrap());
            let frontier = db.frontier().unwrap();
            let result = txn.commit_mixed_rebased(&mut db, &cx, 7).await;
            let expected = if case == 9 {
                "FG-LAW-FCW-READ-01"
            } else {
                "FG-LAW-FCW-01"
            };
            assert!(
                matches!(result, Err(WriteTxnError::Write(
                WriteError::FirstCommitterWins { law, .. })) if law == expected),
                "case {case}"
            );
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn absent_and_cancelled_vertex_deletes_keep_raw_lifetime_guards() {
    let ((), report) = run_async_under_lab(0x71ca_0003, |root| async move {
        let purposes = PurposeContexts::narrow_runtime_root(&root);
        let cx = purposes.commit();
        let txcx = purposes.txn();
        for cancelled in [false, true] {
            for collision in [false, true] {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx).await;
                let mut txn = db.begin(&txcx).unwrap();
                let mut batch = WriteBatch::new(RelationId(1));
                if cancelled {
                    batch.create_vertex(VId(NEW), vec![], vec![]);
                    batch.add_edge(EId(NEW), VId(4), VId(NEW), vec![]);
                    batch.delete_vertex(VId(NEW));
                } else {
                    batch.delete_vertex_if_present(VId(NEW));
                }
                batch.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(7)));
                txn.write(&mut db, batch).unwrap();
                let original = txn.prepared.as_ref().unwrap().template.clone();
                if collision {
                    let mut born = WriteBatch::new(RelationId(1));
                    born.create_vertex(VId(NEW), vec![], vec![]);
                    db.write(&cx, born).await.unwrap();
                    let mut gone = WriteBatch::new(RelationId(1));
                    gone.delete_vertex(VId(NEW));
                    db.write(&cx, gone).await.unwrap();
                } else {
                    db.write(&cx, unrelated()).await.unwrap();
                }
                let frontier = db.frontier().unwrap();
                let result = txn.commit_mixed_rebased(&mut db, &cx, 4).await;
                if collision {
                    assert!(matches!(
                        result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-01",
                            ..
                        }))
                    ));
                    assert_eq!(db.frontier().unwrap(), frontier);
                } else {
                    assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                    assert_eq!(
                        db.delta_since(frontier)
                            .unwrap()
                            .next()
                            .unwrap()
                            .coordinate_entries(),
                        original.coordinate_entries()
                    );
                }
                assert!(db.vertex(VId(NEW)).unwrap().is_none());
                assert!(db.edge(EId(NEW)).unwrap().is_none());
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn overlapping_cascades_are_borrowed_once_without_losing_absorbed_edge_deletes() {
    let ((), report) = run_async_under_lab(0x71ca_0004, |root| async move {
        let purposes = PurposeContexts::narrow_runtime_root(&root);
        let cx = purposes.commit();
        let txcx = purposes.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.delete_edge(EId(10));
        batch.delete_vertex(VId(1));
        batch.delete_vertex(VId(2));
        batch.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(7)));
        txn.write(&mut db, batch).unwrap();
        let template = &txn.prepared.as_ref().unwrap().template;
        let mut footprint = MixedRebaseFootprint::default();
        for row in txn.staged.iter().flat_map(|batch| &batch.rows) {
            footprint.record(row).unwrap();
        }
        let mut checkpoints = 0;
        footprint
            .protect_vertex_cascades(template, &mut || {
                checkpoints += 1;
                Ok(())
            })
            .unwrap();
        assert!(checkpoints >= 6);
        assert_eq!(
            footprint.retired_edges.iter().copied().collect::<Vec<_>>(),
            vec![EId(10), EId(11), EId(12), EId(13), EId(20), EId(21)]
        );
        for stop in 1..=checkpoints {
            let mut partial = MixedRebaseFootprint::default();
            for row in txn.staged.iter().flat_map(|batch| &batch.rows) {
                partial.record(row).unwrap();
            }
            let mut seen = 0;
            let result = partial.protect_vertex_cascades(template, &mut || {
                seen += 1;
                if seen == stop {
                    Err(WriteTxnError::NoPreparedWrite)
                } else {
                    Ok(())
                }
            });
            assert!(matches!(result, Err(WriteTxnError::NoPreparedWrite)));
            assert_eq!(seen, stop);
        }
        // Same bits in the edge/vertex domains are never interchangeable.
        let edge = DeltaRow::Property {
            elem: ElementId::Edge(EId(1)),
            property: Q,
            before: None,
            after: Some(CanonicalScalar::Int(1)),
        };
        assert!(!footprint.conflicts(&edge, &mut || Ok(())).unwrap());
        db.write(&cx, unrelated()).await.unwrap();
        txn.commit_mixed_rebased(&mut db, &cx, 4).await.unwrap();
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.vertex(VId(2)).unwrap().is_none());
        assert_eq!(db.edges().unwrap().len(), 1);
        assert!(db.edge(EId(30)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cascade_rebase_keeps_exact_row_limits_and_every_prepublication_refusal() {
    let ((), report) = run_async_under_lab(0x71ca_0005, |root| async move {
        let purposes = PurposeContexts::narrow_runtime_root(&root);
        let cx = purposes.commit();
        let txcx = purposes.txn();
        let (mut db, mut txn) = drifted(&cx, &txcx).await;
        let frontier = db.frontier().unwrap();
        assert!(matches!(
            txn.commit_mixed_rebased(&mut db, &cx, 6).await,
            Err(WriteTxnError::OrderedWriteBudgetExceeded {
                limit: 6,
                required: 7
            })
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertex(VId(1)).unwrap().is_some());
        let (mut db, mut txn) = drifted(&cx, &txcx).await;
        let mut total = 0;
        txn.complete_rebased_controlled(
            &mut db,
            &cx,
            None,
            true,
            Some(RebasePreparation::Mixed(7)),
            || {
                total += 1;
                Ok(())
            },
        )
        .await
        .unwrap();
        assert!(total > 20);
        for stop in 1..=total {
            let (mut db, mut txn) = drifted(&cx, &txcx).await;
            let before = (db.vertices().unwrap(), db.edges().unwrap());
            let frontier = db.frontier().unwrap();
            let mut seen = 0;
            let result = txn
                .complete_rebased_controlled(
                    &mut db,
                    &cx,
                    None,
                    true,
                    Some(RebasePreparation::Mixed(7)),
                    || {
                        seen += 1;
                        if seen == stop {
                            Err(WriteTxnError::NoPreparedWrite)
                        } else {
                            Ok(())
                        }
                    },
                )
                .await;
            assert!(matches!(result, Err(WriteTxnError::NoPreparedWrite)));
            assert_eq!(seen, stop);
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
            assert!(txn.prepared.is_none());
            assert!(txn.staged.is_empty());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_rebase_uses_native_ownership_and_ambiguous_commit_outcomes() {
    use fgdb_chronicle::commit::CrashPoint;
    let ((), report) = run_async_under_lab(0x71ca_0006, |root| async move {
        let purposes = PurposeContexts::narrow_runtime_root(&root);
        let cx = purposes.commit();
        let txcx = purposes.txn();
        let (mut db, mut txn) = drifted(&cx, &txcx).await;
        let basis = txn.basis();
        let original = txn.prepared.as_ref().unwrap().template.clone();
        drop(txn.commit_mixed_rebased(&mut db, &cx, 7));
        let mut foreign = Database::open_memory(&cx, keys()).await.unwrap();
        assert!(matches!(
            txn.commit_mixed_rebased(&mut foreign, &cx, 7).await,
            Err(WriteTxnError::WrongDatabase)
        ));
        assert_eq!(txn.basis(), basis);
        assert_eq!(txn.state(), EmbeddedTxnState::Active);
        assert_eq!(txn.prepared.as_ref().unwrap().template, original);
        txn.commit_mixed_rebased(&mut db, &cx, 7).await.unwrap();
        for crash in [CrashPoint::BeforeCapsule, CrashPoint::AfterMarkerBeforeD2] {
            let (mut db, mut txn) = drifted(&cx, &txcx).await;
            let frontier = db.frontier().unwrap();
            let result = txn
                .complete_rebased_controlled(
                    &mut db,
                    &cx,
                    Some(crash),
                    true,
                    Some(RebasePreparation::Mixed(7)),
                    || Ok(()),
                )
                .await;
            assert!(result.is_err());
            assert_eq!(
                txn.state(),
                if crash == CrashPoint::BeforeCapsule {
                    EmbeddedTxnState::Aborted
                } else {
                    EmbeddedTxnState::CommitOutcomeUnknown {
                        published_frontier: frontier,
                    }
                }
            );
            assert!(txn.prepared.is_none());
            assert!(txn.pin.is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
