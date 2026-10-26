use super::*;
use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, SchemaEpoch};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa1; 32],
        DatabaseSecurityNamespaceId([0xa2; 32]),
        [0xa3; 32],
    )
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=4 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![(P, CanonicalScalar::Int(0)), (Q, CanonicalScalar::Int(0))],
        );
    }
    batch.add_edge(
        EId(10),
        VId(1),
        VId(2),
        vec![(P, CanonicalScalar::Int(0)), (Q, CanonicalScalar::Int(0))],
    );
    db.write(cx, batch).await.unwrap();
}

fn edge(eid: u128) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.add_edge(EId(eid), VId(1), VId(3), vec![]);
    batch
}

fn edit(property: PropertyKeyId, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.set_vertex_property(VId(1), property, Some(CanonicalScalar::Int(value)));
    batch
}

fn assert_conflict<T: core::fmt::Debug>(result: Result<T, WriteTxnError>, expected: &str) {
    assert!(
        matches!(
            &result,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law, .. }))
                if *law == expected
        ),
        "{result:?}"
    );
}

fn program(case: usize) -> (Vec<WriteBatch>, u64) {
    match case {
        0 => (vec![edge(50)], 1),
        1 => {
            let mut fields = edit(P, 7);
            fields.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(9)));
            fields.set_vertex_label(VId(1), LabelId(2), true);
            (vec![fields], 3)
        }
        2 => {
            let mut mixed = edit(P, 7);
            mixed.create_vertex(VId(50), vec![], vec![]);
            mixed.set_vertex_property(VId(50), P, Some(CanonicalScalar::Int(8)));
            mixed.add_edge(EId(50), VId(1), VId(50), vec![]);
            mixed.set_edge_property(EId(50), P, Some(CanonicalScalar::Int(9)));
            mixed.delete_edge(EId(10));
            (vec![mixed], 6)
        }
        3 => {
            let mut first = WriteBatch::new(RelationId(9));
            first.create_vertex(VId(50), vec![], vec![]);
            first.add_edge(EId(50), VId(1), VId(50), vec![]);
            let mut second = WriteBatch::new(RelationId(2));
            second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
            second.add_edge(EId(60), VId(50), VId(2), vec![]);
            // Two vertex inputs in each of two slices, plus two edge inputs.
            (vec![first, second], 6)
        }
        _ => {
            let mut mixed = edit(P, 7);
            mixed.delete_vertex(VId(2));
            mixed.add_edge(EId(50), VId(1), VId(4), vec![]);
            (vec![mixed], 3)
        }
    }
}

#[test]
fn ordinary_finish_and_commit_replay_supported_programs_once_and_reopen() {
    let ((), report) = run_async_under_lab(0xa170_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..5 {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            seed(&mut db, &cx).await;
            let pinned = db.read_session().unwrap();
            let mut candidate = db.begin(&txcx).unwrap();
            let mut strict = db.begin(&txcx).unwrap();
            let mut winner = db.begin(&txcx).unwrap();
            // These reads remain at the original basis and are never repeated
            // by the application while its retained mutation program replays.
            candidate.vertex(&db, VId(4)).unwrap();
            assert!(candidate.vertex(&db, VId(99)).unwrap().is_none());
            let (batches, required) = program(case);
            candidate.write_ordered(&mut db, batches.clone()).unwrap();
            strict.write_ordered(&mut db, batches.clone()).unwrap();
            let prepared = candidate.prepared.as_ref().unwrap();
            assert_eq!(prepared.replay_row_allowance, Some(required));
            let original = prepared.template.clone();
            let mut concurrent = edit(Q, 11);
            concurrent.add_edge(EId(20), VId(1), VId(3), vec![]);
            winner.write(&mut db, concurrent.clone()).unwrap();
            let frontier = winner.commit(&mut db, &cx).await.unwrap();
            if case % 2 == 0 {
                assert_conflict(strict.commit_strict(&mut db, &cx).await, "FG-LAW-FCW-01");
            } else {
                assert_conflict(strict.finish_strict(&mut db, &cx).await, "FG-LAW-FCW-01");
            }
            let seq = if case % 2 == 0 {
                candidate.commit(&mut db, &cx).await.unwrap()
            } else {
                match candidate.finish(&mut db, &cx).await.unwrap() {
                    EmbeddedTxnCompletion::WriteCommitted { commit_seq } => commit_seq,
                    EmbeddedTxnCompletion::ReadClosed { .. } => panic!("prepared write"),
                }
            };
            assert_eq!(seq, CommitSeq(frontier.0 + 1));
            assert_eq!(candidate.basis(), frontier);
            let tail = db.delta_since(frontier).unwrap().collect::<Vec<_>>();
            assert_eq!(tail.len(), 1);
            assert_eq!(tail[0].coordinate_entries(), original.coordinate_entries());
            assert!(pinned.edge(EId(50)).unwrap().is_none());
            assert_eq!(
                pinned.vertex(VId(1)).unwrap().unwrap().props[1].1,
                CanonicalScalar::Int(0).into()
            );
            let mut serial = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut serial, &cx).await;
            serial.write(&cx, concurrent).await.unwrap();
            serial.write_ordered(&cx, batches).await.unwrap();
            assert_eq!(db.vertices().unwrap(), serial.vertices().unwrap());
            assert_eq!(db.edges().unwrap(), serial.edges().unwrap());
            drop(db);
            let reopened = Database::open_with_vfs(&cx, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.frontier().unwrap(), seq);
            assert_eq!(reopened.vertices().unwrap(), serial.vertices().unwrap());
            assert_eq!(reopened.edges().unwrap(), serial.edges().unwrap());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_write_conflict_keeps_the_original_basis_and_read_only_completion() {
    let ((), report) = run_async_under_lab(0xa170_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let mut candidate = db.begin(&txcx).unwrap();
        let mut reader = db.begin(&txcx).unwrap();
        let basis = candidate.basis();
        reader.vertex(&db, VId(4)).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(50), vec![], vec![]);
        candidate.write(&mut db, batch).unwrap();
        candidate.vertex(&db, VId(50)).unwrap();
        // A replay would be ineligible and exhaust this allowance. A simple
        // frontier advance without an overlapping write needs no replay.
        std::sync::Arc::make_mut(candidate.prepared.as_mut().unwrap()).replay_row_allowance =
            Some(0);
        db.write(&cx, edit(Q, 8)).await.unwrap();
        let seq = candidate.commit(&mut db, &cx).await.unwrap();
        assert_eq!(candidate.basis(), basis);
        assert_eq!(
            reader.finish(&mut db, &cx).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed {
                snapshot_seq: basis,
                validated_through: seq,
            }
        );
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ineligible_replay_preserves_the_exact_original_strict_conflict() {
    let ((), report) = run_async_under_lab(0xa170_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..10 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut candidate = db.begin(&txcx).unwrap();
            let mut strict = db.begin(&txcx).unwrap();
            let mut batch = edge(50);
            match case {
                6 => batch.ensure_vertex(VId(1), vec![], vec![]),
                7 => batch.ensure_edge_by_triple(EId(99), VId(1), VId(2), vec![]),
                8 => batch.create_vertex(VId(50), vec![], vec![]),
                _ => &mut batch,
            };
            if case == 9 {
                let mut other = WriteBatch::new(RelationId(2));
                other.add_edge(EId(60), VId(1), VId(2), vec![]);
                candidate
                    .write_atomic(&mut db, vec![batch.clone(), other.clone()])
                    .unwrap();
                strict.write_atomic(&mut db, vec![batch, other]).unwrap();
                assert_eq!(
                    candidate.prepared.as_ref().unwrap().replay_row_allowance,
                    None
                );
            } else {
                candidate.write(&mut db, batch.clone()).unwrap();
                strict.write(&mut db, batch).unwrap();
            }
            match case {
                0 => candidate.savepoint(&db, "live").unwrap(),
                1 => candidate.program_multi_relation = true,
                2 => candidate.authorization_decision_digest = Some(fgdb_crypto::Digest([1; 32])),
                3 => {
                    std::sync::Arc::make_mut(candidate.prepared.as_mut().unwrap())
                        .authorization_decision_digest = Some(fgdb_crypto::Digest([1; 32]));
                }
                4 => {
                    std::sync::Arc::make_mut(candidate.prepared.as_mut().unwrap())
                        .replay_row_allowance = None;
                }
                5 => {
                    std::sync::Arc::make_mut(candidate.prepared.as_mut().unwrap())
                        .replay_row_allowance = Some(0);
                }
                8 => {
                    candidate.vertex(&db, VId(50)).unwrap();
                }
                _ => {}
            }
            let frontier = db.write(&cx, edge(20)).await.unwrap();
            let expected = strict.commit_strict(&mut db, &cx).await.unwrap_err();
            let actual = candidate.commit(&mut db, &cx).await.unwrap_err();
            assert!(matches!(
                &actual,
                WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-01",
                    ..
                })
            ));
            assert_eq!(actual.to_string(), expected.to_string(), "case {case}");
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.edge(EId(50)).unwrap().is_none());
            assert_eq!(candidate.state(), EmbeddedTxnState::Aborted);
            assert!(candidate.pin.is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn automatic_replay_preserves_original_reads_absences_phantoms_and_aba() {
    let ((), report) = run_async_under_lab(0xa170_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..7 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut candidate = db.begin(&txcx).unwrap();
            candidate.write(&mut db, edge(50)).unwrap();
            let mut concurrent = edit(Q, 8);
            match case {
                0 => {
                    candidate.vertex(&db, VId(1)).unwrap();
                }
                1 => {
                    candidate.vertex(&db, VId(4)).unwrap();
                    concurrent.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(9)));
                }
                2 => {
                    assert!(candidate.vertex(&db, VId(99)).unwrap().is_none());
                    concurrent.create_vertex(VId(99), vec![], vec![]);
                }
                3 => {
                    candidate.vertices(&db).unwrap();
                    concurrent.create_vertex(VId(99), vec![], vec![]);
                }
                4 => {
                    candidate.edges(&db).unwrap();
                    concurrent.add_edge(EId(20), VId(2), VId(3), vec![]);
                }
                5 => {
                    assert!(
                        candidate
                            .execute_gql(
                                &db,
                                "MATCH (n:Missing) RETURN n",
                                &RelationBind::new().with_label("Missing", LabelId(9)),
                            )
                            .unwrap()
                            .is_empty()
                    );
                    concurrent.set_vertex_label(VId(4), LabelId(9), true);
                }
                _ => {
                    candidate.savepoint(&db, "before-read").unwrap();
                    candidate.vertex(&db, VId(4)).unwrap();
                    candidate.rollback_to_savepoint(&db, "before-read").unwrap();
                    candidate.release_savepoint(&db, "before-read").unwrap();
                    concurrent.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(9)));
                }
            }
            db.write(&cx, concurrent).await.unwrap();
            if case == 1 {
                let mut restore = WriteBatch::new(R);
                restore.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(0)));
                db.write(&cx, restore).await.unwrap();
            }
            let frontier = db.frontier().unwrap();
            assert_conflict(candidate.finish(&mut db, &cx).await, "FG-LAW-FCW-READ-01");
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.edge(EId(50)).unwrap().is_none());
            assert_eq!(candidate.state(), EmbeddedTxnState::Aborted);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn automatic_replay_refuses_overlapping_fields_identities_lifetimes_and_cascades() {
    let ((), report) = run_async_under_lab(0xa170_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..7 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut candidate = db.begin(&txcx).unwrap();
            let mut staged = WriteBatch::new(R);
            let mut concurrent = WriteBatch::new(R);
            let mut restore = WriteBatch::new(R);
            match case {
                0 | 1 => {
                    staged = edit(P, 7);
                    concurrent = edit(P, 8);
                    if case == 1 {
                        restore = edit(P, 0);
                    }
                }
                2 => {
                    staged = edge(50);
                    concurrent = edge(50);
                    restore.delete_edge(EId(50));
                }
                3 => {
                    staged = edge(50);
                    concurrent.delete_vertex(VId(1));
                }
                4 => {
                    staged.delete_vertex(VId(2));
                    concurrent.add_edge(EId(20), VId(3), VId(2), vec![]);
                    restore.delete_edge(EId(20));
                }
                5 => {
                    staged.set_vertex_property(VId(1), PropertyKeyId(9), None);
                    concurrent.set_vertex_property(
                        VId(1),
                        PropertyKeyId(9),
                        Some(CanonicalScalar::Int(8)),
                    );
                }
                _ => {
                    staged.compare_and_set_vertex_property(
                        VId(1),
                        P,
                        Some(CanonicalScalar::Int(99)),
                        CanonicalScalar::Int(7),
                        WriteMismatchPolicy::NoOp,
                    );
                    concurrent = edit(P, 8);
                }
            }
            candidate.write(&mut db, staged).unwrap();
            db.write(&cx, concurrent).await.unwrap();
            if !restore.is_empty() {
                db.write(&cx, restore).await.unwrap();
            }
            let frontier = db.frontier().unwrap();
            let before = (db.vertices().unwrap(), db.edges().unwrap());
            assert_conflict(candidate.commit(&mut db, &cx).await, "FG-LAW-FCW-01");
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn row_free_coordinate_changes_in_the_complete_history_refuse_automatic_replay() {
    let ((), report) = run_async_under_lab(0xa170_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for changed in 0..4 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx).await;
            let mut candidate = db.begin(&txcx).unwrap();
            candidate.write(&mut db, edit(P, 7)).unwrap();
            let basis = candidate.basis();
            let conflict_seq = db.write(&cx, edit(Q, 8)).await.unwrap();
            let frontier = db.write(&cx, edge(20)).await.unwrap();
            let source = db.snapshot.delta_index.get(frontier).unwrap();
            let mut coordinates = source.coordinate_entries().to_vec();
            let coordinate = &mut coordinates[0];
            coordinate.rows.clear();
            match changed {
                0 => coordinate.graph = fgdb_types::GraphId(2),
                1 => coordinate.branch = fgdb_types::BranchId(2),
                2 => coordinate.schema_epoch = SchemaEpoch(1),
                _ => coordinate.schema_transition = Some(fgdb_types::ObjectId([7; 32])),
            }
            // Synthetic admission fixture for metadata not yet produced by
            // embedded DDL. It makes no claim that the altered bytes match the
            // real marker. A genuine earlier batch supplies the write conflict;
            // metadata in a later, row-free batch must still prevent replay.
            let metadata = fgdb_delta_types::LogicalDeltaBatch::from_parts_for_test(
                coordinates,
                *source.source_template_digest(),
                source.commit_marker_identity(),
                frontier,
                frontier,
            );
            let conflict = db.snapshot.delta_index.get(conflict_seq).unwrap().clone();
            std::sync::Arc::make_mut(&mut db.snapshot).delta_index =
                fgdb_delta_types::LocalDeltaBatchIndex::from_parts_for_test(
                    basis,
                    frontier,
                    vec![(conflict_seq, conflict), (frontier, metadata)],
                );
            assert_conflict(candidate.commit(&mut db, &cx).await, "FG-LAW-FCW-01");
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(0).into()
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

async fn drifted(cx: &CommitCx, txcx: &TxnCx) -> (Database<MemVfs>, WriteTxn) {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    seed(&mut db, cx).await;
    let mut candidate = db.begin(txcx).unwrap();
    candidate.vertex(&db, VId(4)).unwrap();
    candidate.write(&mut db, edit(P, 7)).unwrap();
    db.write(cx, edit(Q, 8)).await.unwrap();
    (db, candidate)
}

#[test]
fn automatic_replay_releases_every_prepublication_refusal_and_keeps_controlled_strict() {
    let ((), report) = run_async_under_lab(0xa170_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let (mut db, mut candidate) = drifted(&cx, &txcx).await;
        // Authorized entry points rely on this existing internal seam. It must
        // not acquire the public raw API's new automatic replay authority.
        assert_conflict(
            candidate
                .complete_controlled(&mut db, &cx, None, true, || Ok(()))
                .await,
            "FG-LAW-FCW-01",
        );
        let (mut db, mut candidate) = drifted(&cx, &txcx).await;
        let mut total = 0;
        candidate
            .complete_rebased_controlled(
                &mut db,
                &cx,
                None,
                true,
                Some(RebasePreparation::Automatic),
                || {
                    total += 1;
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert!(total > 20);
        for stop in 1..=total {
            let (mut db, mut candidate) = drifted(&cx, &txcx).await;
            let frontier = db.frontier().unwrap();
            let before = (db.vertices().unwrap(), db.edges().unwrap());
            let mut seen = 0;
            let result = candidate
                .complete_rebased_controlled(
                    &mut db,
                    &cx,
                    None,
                    true,
                    Some(RebasePreparation::Automatic),
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
            assert_eq!(candidate.state(), EmbeddedTxnState::Aborted);
            assert!(candidate.prepared.is_none());
            assert!(candidate.staged.is_empty());
            assert!(candidate.pin.is_none());
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn automatic_replay_keeps_owner_unpolled_and_ambiguous_outcome_contracts() {
    use fgdb_chronicle::commit::CrashPoint;
    let ((), report) = run_async_under_lab(0xa170_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let (mut db, mut candidate) = drifted(&cx, &txcx).await;
        let basis = candidate.basis();
        let original = candidate.prepared.as_ref().unwrap().template.clone();
        drop(candidate.commit(&mut db, &cx));
        let mut foreign = Database::open_memory(&cx, keys()).await.unwrap();
        assert!(matches!(
            candidate.finish(&mut foreign, &cx).await,
            Err(WriteTxnError::WrongDatabase)
        ));
        assert_eq!(candidate.basis(), basis);
        assert_eq!(candidate.state(), EmbeddedTxnState::Active);
        assert_eq!(candidate.prepared.as_ref().unwrap().template, original);
        candidate.commit(&mut db, &cx).await.unwrap();
        for crash in [CrashPoint::BeforeCapsule, CrashPoint::AfterMarkerBeforeD2] {
            let (mut db, mut candidate) = drifted(&cx, &txcx).await;
            let frontier = db.frontier().unwrap();
            assert!(
                candidate
                    .commit_with_crash(&mut db, &cx, Some(crash))
                    .await
                    .is_err()
            );
            assert_eq!(
                candidate.state(),
                if crash == CrashPoint::BeforeCapsule {
                    EmbeddedTxnState::Aborted
                } else {
                    EmbeddedTxnState::CommitOutcomeUnknown {
                        published_frontier: frontier,
                    }
                }
            );
            assert!(candidate.prepared.is_none());
            assert!(candidate.pin.is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
