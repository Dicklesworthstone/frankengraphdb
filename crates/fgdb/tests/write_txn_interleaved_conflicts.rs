//! Transaction validation must survive arbitrary intervening ordinary writes.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteMismatchPolicy, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    EmbeddedTxnState, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x51; 32],
        DatabaseSecurityNamespaceId([0x52; 32]),
        [0x53; 32],
    )
}

fn seed_batch() -> WriteBatch {
    let mut seed = WriteBatch::new(R);
    for vid in [VId(1), VId(2), VId(8)] {
        seed.create_vertex(vid, vec![], vec![(P, CanonicalScalar::Int(0))]);
    }
    seed
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.expect("database");
    db.write(cx, seed_batch()).await.expect("seed");
    db
}

fn assert_write_conflict(result: Result<CommitSeq, WriteTxnError>) {
    assert!(matches!(
        result,
        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
            law: "FG-LAW-FCW-01",
            ..
        }))
    ));
}

async fn unrelated_writes(db: &mut Database<MemVfs>, cx: &CommitCx) {
    for value in 1..=3 {
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_property(VId(8), P, Some(CanonicalScalar::Int(value)));
        db.write(cx, batch)
            .await
            .expect("intervening ordinary write");
    }
}

#[test]
fn blind_write_cannot_lose_conflict_history_after_validator_resets() {
    let ((), report) = run_async_under_lab(0xfc10_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&txn_cx).expect("begin");
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
        txn.write(&mut db, staged)
            .expect("stage without an explicit read");
        let mut winner = WriteBatch::new(R);
        winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(10)));
        db.write(&cx, winner).await.expect("first writer");
        unrelated_writes(&mut db, &cx).await;
        let frontier = db.frontier().expect("frontier");
        assert_write_conflict(txn.commit(&mut db, &cx).await);
        assert_eq!(db.frontier().expect("no publication"), frontier);
        assert_eq!(
            db.vertex(VId(1))
                .expect("winner row")
                .expect("vertex")
                .props,
            vec![(P, CanonicalScalar::Int(10).into())]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn disjoint_staged_writes_survive_multiple_frontier_advances() {
    let ((), report) = run_async_under_lab(0xfc10_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&txn_cx).expect("begin");
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
        txn.write(&mut db, staged).expect("stage");
        unrelated_writes(&mut db, &cx).await;
        txn.commit(&mut db, &cx).await.expect("disjoint commit");
        assert_eq!(
            db.vertex(VId(1)).expect("result").expect("vertex").props,
            vec![(P, CanonicalScalar::Int(5).into())]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn conditional_noop_retains_its_observed_target_dependency() {
    let ((), report) = run_async_under_lab(0xfc10_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&txn_cx).expect("begin");
        let mut staged = WriteBatch::new(R);
        staged.compare_and_set_vertex_property(
            VId(1),
            P,
            Some(CanonicalScalar::Int(99)),
            CanonicalScalar::Int(5),
            WriteMismatchPolicy::NoOp,
        );
        staged.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, staged)
            .expect("mismatch is a lawful no-op");
        let mut winner = WriteBatch::new(R);
        winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
        db.write(&cx, winner)
            .await
            .expect("change the observed condition");
        unrelated_writes(&mut db, &cx).await;
        assert_write_conflict(txn.commit(&mut db, &cx).await);
        assert!(
            db.vertex(VId(99))
                .expect("no partial transaction")
                .is_none()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_vertex_delete_conflicts_with_new_incident_edges() {
    let ((), report) = run_async_under_lab(0xfc10_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&txn_cx).expect("begin");
        let mut staged = WriteBatch::new(R);
        staged.delete_vertex(VId(1));
        txn.write(&mut db, staged)
            .expect("prepare an empty incident cascade");
        let mut winner = WriteBatch::new(R);
        winner.add_edge(EId(10), VId(2), VId(1), vec![]);
        db.write(&cx, winner)
            .await
            .expect("insert an incoming incident edge");
        unrelated_writes(&mut db, &cx).await;
        let frontier = db.frontier().expect("frontier");
        assert_write_conflict(txn.commit(&mut db, &cx).await);
        assert_eq!(db.frontier().expect("no publication"), frontier);
        assert!(db.vertex(VId(1)).expect("endpoint remains live").is_some());
        assert!(
            db.edge(EId(10))
                .expect("winning edge remains live")
                .is_some()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn replay_program(case: usize) -> Vec<WriteBatch> {
    let mut batch = WriteBatch::new(R);
    match case {
        0 => {
            batch.add_edge(EId(50), VId(1), VId(2), vec![]);
        }
        1 => {
            batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
            batch.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(9)));
            batch.set_vertex_label(VId(1), LabelId(2), true);
        }
        2 => {
            batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
            batch.delete_edge(EId(10));
            batch.create_vertex(VId(50), vec![], vec![(P, CanonicalScalar::Int(1))]);
            batch.set_vertex_property(VId(50), P, Some(CanonicalScalar::Int(8)));
            batch.add_edge(EId(50), VId(1), VId(50), vec![]);
            batch.set_edge_property(EId(50), P, Some(CanonicalScalar::Int(9)));
        }
        _ => {
            let mut first = WriteBatch::new(RelationId(9));
            first.create_vertex(VId(50), vec![], vec![]);
            first.add_edge(EId(50), VId(1), VId(50), vec![]);
            let mut second = WriteBatch::new(RelationId(2));
            second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
            second.add_edge(EId(60), VId(50), VId(2), vec![]);
            return vec![first, second];
        }
    }
    vec![batch]
}

#[test]
fn automatic_append_field_and_mixed_completion_match_serial_execution_and_reopen() {
    let ((), report) = run_async_under_lab(0xfc10_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        for case in 0..4 {
            let vfs = MemVfs::new().expect("vfs");
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .expect("database");
            let mut seed = seed_batch();
            seed.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(0))]);
            db.write(&cx, seed.clone()).await.expect("seed");
            let pinned = db.read_session().expect("pinned snapshot");
            let mut candidate = db.begin(&txn_cx).expect("candidate");
            let mut strict = db.begin(&txn_cx).expect("strict control");
            let mut winner = db.begin(&txn_cx).expect("winner");
            let basis = candidate.basis();
            let observed = candidate.vertex(&db, VId(8)).expect("unchanged read");
            assert!(
                candidate
                    .vertex(&db, VId(99))
                    .expect("negative read")
                    .is_none()
            );
            let program = replay_program(case);
            candidate
                .write_ordered(&mut db, program.clone())
                .expect("stage program");
            strict
                .write_ordered(&mut db, program.clone())
                .expect("stage strict control");
            let mut concurrent = WriteBatch::new(R);
            concurrent.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(11)));
            concurrent.add_edge(EId(20), VId(1), VId(2), vec![]);
            if case == 1 {
                concurrent.set_edge_property(EId(10), Q, Some(CanonicalScalar::Int(12)));
            }
            winner
                .write(&mut db, concurrent.clone())
                .expect("stage winner");
            let frontier = winner.commit(&mut db, &cx).await.expect("first commit");
            assert_eq!(candidate.basis(), basis);
            assert_eq!(
                candidate.vertex(&db, VId(8)).expect("original read"),
                observed
            );
            let strict_result = if case % 2 == 0 {
                strict.commit_strict(&mut db, &cx).await.map(|_| ())
            } else {
                strict.finish_strict(&mut db, &cx).await.map(|_| ())
            };
            assert!(matches!(
                strict_result,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-01",
                    ..
                }))
            ));
            let seq = if case % 2 == 0 {
                candidate
                    .commit(&mut db, &cx)
                    .await
                    .expect("automatic commit")
            } else {
                match candidate
                    .finish(&mut db, &cx)
                    .await
                    .expect("automatic finish")
                {
                    EmbeddedTxnCompletion::WriteCommitted { commit_seq } => commit_seq,
                    EmbeddedTxnCompletion::ReadClosed { .. } => panic!("prepared program"),
                }
            };
            assert_eq!(seq, CommitSeq(frontier.0 + 1));
            assert_eq!(candidate.basis(), frontier);
            assert_eq!(
                candidate.state(),
                EmbeddedTxnState::Completed(EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: seq
                })
            );
            assert_eq!(
                db.delta_since(frontier).expect("committed suffix").count(),
                1
            );
            assert!(pinned.edge(EId(50)).expect("pinned edge").is_none());
            assert_eq!(
                pinned.vertex(VId(1)).expect("pinned vertex").unwrap().props,
                vec![(P, CanonicalScalar::Int(0).into())]
            );

            let mut serial = Database::open_memory(&cx, keys())
                .await
                .expect("serial database");
            serial.write(&cx, seed).await.expect("serial seed");
            serial.write(&cx, concurrent).await.expect("serial winner");
            serial
                .write_ordered(&cx, program)
                .await
                .expect("serial program");
            assert_eq!(db.vertices().expect("vertices"), serial.vertices().unwrap());
            assert_eq!(db.edges().expect("edges"), serial.edges().unwrap());
            assert_eq!(
                db.delta_since(frontier)
                    .unwrap()
                    .next()
                    .unwrap()
                    .coordinate_entries(),
                serial
                    .delta_since(frontier)
                    .unwrap()
                    .next()
                    .unwrap()
                    .coordinate_entries()
            );
            drop(db);
            let reopened = Database::open_with_vfs(&cx, vfs, &path, keys())
                .await
                .expect("reopen");
            assert_eq!(reopened.frontier().unwrap(), seq);
            assert_eq!(reopened.vertices().unwrap(), serial.vertices().unwrap());
            assert_eq!(reopened.edges().unwrap(), serial.edges().unwrap());
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn automatic_completion_retains_full_reads_negative_reads_and_write_aba() {
    let ((), report) = run_async_under_lab(0xfc10_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        for case in 0..3 {
            let mut db = seeded(&cx).await;
            let mut txn = db.begin(&txn_cx).expect("begin");
            if case == 0 {
                txn.vertex(&db, VId(1)).expect("full element read");
            } else if case == 2 {
                assert!(txn.vertex(&db, VId(99)).expect("negative read").is_none());
            }
            let mut staged = WriteBatch::new(R);
            staged.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
            txn.write(&mut db, staged).expect("stage field edit");
            let mut concurrent = WriteBatch::new(R);
            concurrent.set_vertex_property(
                VId(1),
                if case == 1 { P } else { Q },
                Some(CanonicalScalar::Int(8)),
            );
            if case == 2 {
                concurrent.create_vertex(VId(99), vec![], vec![]);
            }
            db.write(&cx, concurrent).await.expect("concurrent change");
            if case == 1 {
                let mut restore = WriteBatch::new(R);
                restore.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
                db.write(&cx, restore)
                    .await
                    .expect("restore the original value");
            }
            unrelated_writes(&mut db, &cx).await;
            let frontier = db.frontier().unwrap();
            let before = db.vertices().unwrap();
            let expected = if case == 1 {
                "FG-LAW-FCW-01"
            } else {
                "FG-LAW-FCW-READ-01"
            };
            assert!(matches!(txn.finish(&mut db, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law, .. }))
                    if law == expected
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(db.vertices().unwrap(), before);
            assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn automatic_completion_preserves_ineligible_decisions_and_creation_observations() {
    let ((), report) = run_async_under_lab(0xfc10_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        for case in 0..3 {
            let mut db = seeded(&cx).await;
            let mut candidate = db.begin(&txn_cx).expect("candidate");
            let mut strict = db.begin(&txn_cx).expect("strict control");
            let mut staged = WriteBatch::new(R);
            staged.add_edge(EId(50), VId(1), VId(2), vec![]);
            if case == 1 {
                staged.ensure_vertex(VId(1), vec![], vec![]);
            } else if case == 2 {
                staged.create_vertex(VId(50), vec![], vec![]);
            }
            candidate
                .write(&mut db, staged.clone())
                .expect("stage candidate");
            strict.write(&mut db, staged).expect("stage strict control");
            if case == 0 {
                candidate.savepoint(&db, "live").expect("savepoint");
            } else if case == 2 {
                let observed = candidate.vertex(&db, VId(50)).unwrap().unwrap();
                assert_eq!(observed.created_at, candidate.basis());
            }
            let mut concurrent = WriteBatch::new(R);
            concurrent.add_edge(EId(20), VId(1), VId(2), vec![]);
            let frontier = db.write(&cx, concurrent).await.expect("concurrent append");
            let expected = strict.commit_strict(&mut db, &cx).await.unwrap_err();
            let actual = candidate.commit(&mut db, &cx).await.unwrap_err();
            assert!(matches!(
                &actual,
                WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-01",
                    ..
                })
            ));
            assert_eq!(actual.to_string(), expected.to_string());
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.edge(EId(50)).unwrap().is_none());
            assert!(db.vertex(VId(50)).unwrap().is_none());
            assert_eq!(candidate.state(), EmbeddedTxnState::Aborted);
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
