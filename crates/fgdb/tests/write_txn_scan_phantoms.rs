//! Empty and filtered scans must participate in transaction validation.
//! These are conservative table witnesses, not a claim of full graph SSI.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::GqlExecutionBudget;
use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}

fn assert_read_conflict(result: Result<fgdb_types::CommitSeq, WriteTxnError>) {
    assert!(matches!(
        result,
        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
            law: "FG-LAW-FCW-READ-01",
            ..
        }))
    ));
}

#[test]
fn empty_node_and_edge_scans_reject_new_disconnected_rows() {
    let ((), report) = run_async_under_lab(0x5ca0_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let bind = RelationBind::new()
            .with_relation("R", RelationId(1))
            .with_label("L", LabelId(1));
        for surface in 0..4 {
            let mut db = Database::open_memory(&cx, keys()).await.expect("database");
            let mut txn = db.begin(&txn_cx).expect("begin");
            match surface {
                0 => assert!(txn.vertices(&db).expect("empty vertex table").is_empty()),
                1 => assert!(txn.edges(&db).expect("empty edge table").is_empty()),
                2 => assert!(
                    txn.execute_gql(&db, "MATCH (a:L) RETURN a", &bind)
                        .expect("empty labeled scan")
                        .is_empty()
                ),
                _ => assert!(
                    txn.execute_gql(&db, "MATCH (a)-[:R]->(b) RETURN b", &bind)
                        .expect("empty edge MATCH")
                        .is_empty()
                ),
            }
            let mut staged = WriteBatch::new(RelationId(1));
            staged.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, staged).expect("stage unrelated write");
            let mut winner = WriteBatch::new(RelationId(1));
            winner.create_vertex(VId(1), vec![LabelId(1)], vec![]);
            if surface == 1 || surface == 3 {
                winner.create_vertex(VId(2), vec![], vec![]);
                winner.add_edge(EId(10), VId(1), VId(2), vec![]);
            }
            db.write(&cx, winner).await.expect("insert a phantom");
            let frontier = db.frontier().expect("winner frontier");
            assert_read_conflict(txn.commit(&mut db, &cx).await);
            assert_eq!(db.frontier().expect("no loser commit"), frontier);
            assert!(
                db.vertex(VId(99))
                    .expect("unpublished staged vertex")
                    .is_none()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_scan_keeps_its_witness_when_skip_discards_every_row() {
    let ((), report) = run_async_under_lab(0x5ca0_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.expect("database");
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![LabelId(1)], vec![]);
        db.write(&cx, seed).await.expect("seed");
        let mut txn = db.begin(&txn_cx).expect("begin");
        let query = txn
            .prepare_gql_query(
                "MATCH (a:L) RETURN a SKIP 1 LIMIT 1",
                &RelationBind::new().with_label("L", LabelId(1)),
            )
            .expect("prepare");
        assert!(
            txn.execute_prepared_query(&db, &query)
                .expect("all skipped")
                .is_empty()
        );
        let mut staged = WriteBatch::new(RelationId(1));
        staged.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, staged).expect("stage");
        let mut winner = WriteBatch::new(RelationId(1));
        winner.create_vertex(VId(2), vec![LabelId(1)], vec![]);
        db.write(&cx, winner).await.expect("insert matching vertex");
        assert_eq!(
            db.execute_prepared_query(&query).expect("new answer"),
            vec![VId(2)]
        );
        assert_read_conflict(txn.commit(&mut db, &cx).await);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn budget_refusal_keeps_the_scan_dependency() {
    let ((), report) = run_async_under_lab(0x5ca0_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.expect("database");
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![LabelId(1)], vec![]);
        db.write(&cx, seed).await.expect("seed");
        let mut txn = db.begin(&txn_cx).expect("begin");
        let query = txn
            .prepare_gql_query(
                "MATCH (a:L) RETURN a",
                &RelationBind::new().with_label("L", LabelId(1)),
            )
            .expect("prepare");
        assert!(matches!(
            txn.execute_prepared_query_budgeted(
                &db,
                &query,
                GqlExecutionBudget::snapshot_records(0)
            ),
            Err(fgdb_gql::BudgetedGqlError::Budget(_))
        ));
        let mut staged = WriteBatch::new(RelationId(1));
        staged.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, staged).expect("stage after refusal");
        let mut winner = WriteBatch::new(RelationId(1));
        winner.create_vertex(VId(2), vec![LabelId(1)], vec![]);
        db.write(&cx, winner).await.expect("insert phantom");
        assert_read_conflict(txn.commit(&mut db, &cx).await);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn point_reads_and_edge_scans_do_not_become_global_commit_fences() {
    let ((), report) = run_async_under_lab(0x5ca0_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        for edge_scan in [false, true] {
            let mut db = Database::open_memory(&cx, keys()).await.expect("database");
            let mut txn = db.begin(&txn_cx).expect("begin");
            if edge_scan {
                assert!(txn.edges(&db).expect("empty edge scan").is_empty());
            } else {
                assert!(
                    txn.vertex(&db, VId(7))
                        .expect("absent point read")
                        .is_none()
                );
            }
            let mut staged = WriteBatch::new(RelationId(1));
            staged.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, staged).expect("stage");
            let mut winner = WriteBatch::new(RelationId(1));
            winner.create_vertex(VId(1), vec![], vec![]);
            db.write(&cx, winner).await.expect("unrelated vertex only");
            txn.commit(&mut db, &cx).await.expect("disjoint commit");
            assert!(db.vertex(VId(99)).expect("committed output").is_some());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn labelled_scans_scope_insertions_and_keep_membership_witnesses_after_refusal() {
    let ((), report) = run_async_under_lab(0x5ca0_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let bind = RelationBind::new().with_label("L", LabelId(1));
        for initially_matches in [false, true] {
            for surface in 0..5 {
                for change in 0..3 {
                    let mut db = Database::open_memory(&cx, keys()).await.expect("database");
                    let mut seed = WriteBatch::new(RelationId(1));
                    let label = if initially_matches {
                        LabelId(1)
                    } else {
                        LabelId(2)
                    };
                    seed.create_vertex(VId(1), vec![label], vec![]);
                    db.write(&cx, seed).await.expect("seed");
                    let mut txn = db.begin(&txn_cx).expect("begin");
                    let query = txn
                        .prepare_gql_query("MATCH (a:L) RETURN a", &bind)
                        .expect("prepare");
                    let expected = if initially_matches {
                        vec![VId(1)]
                    } else {
                        vec![]
                    };
                    match surface {
                        0 => assert_eq!(
                            txn.execute_gql(&db, "MATCH (a:L) RETURN a", &bind)
                                .expect("ordinary scan"),
                            expected
                        ),
                        1 => assert_eq!(
                            txn.execute_prepared_query(&db, &query)
                                .expect("prepared scan"),
                            expected
                        ),
                        2 => assert_eq!(
                            txn.execute_prepared_query_limited(
                                &db,
                                &query,
                                fgdb_gql::GlaExecutionLimits::new(100, 100)
                            )
                            .expect("limited scan")
                            .value,
                            expected
                        ),
                        3 => assert!(matches!(
                            txn.execute_prepared_query_budgeted(
                                &db,
                                &query,
                                GqlExecutionBudget::snapshot_records(0)
                            ),
                            Err(fgdb_gql::BudgetedGqlError::Budget(_))
                        )),
                        _ => assert!(matches!(
                            txn.execute_prepared_query_limited(
                                &db,
                                &query,
                                fgdb_gql::GlaExecutionLimits::new(0, 0)
                            ),
                            Err(fgdb_gql::GlaExecutionError::Limit(_))
                        )),
                    }
                    let mut staged = WriteBatch::new(RelationId(1));
                    staged.create_vertex(VId(99), vec![], vec![]);
                    txn.write(&mut db, staged)
                        .expect("stage after scan or refusal");
                    let mut winner = WriteBatch::new(RelationId(1));
                    let labels = if change == 1 {
                        vec![LabelId(1)]
                    } else {
                        vec![]
                    };
                    winner.create_vertex(VId(2), labels, vec![]);
                    db.write(&cx, winner).await.expect("insert new vertex");
                    if change == 2 {
                        // This identity was absent from the basis row read set.
                        // Its later membership must be covered by the scan.
                        let mut membership = WriteBatch::new(RelationId(1));
                        membership.set_vertex_label(VId(2), LabelId(1), true);
                        db.write(&cx, membership)
                            .await
                            .expect("add matching label later");
                    }
                    let frontier = db.frontier().expect("winner frontier");
                    if change == 0 {
                        txn.commit(&mut db, &cx)
                            .await
                            .expect("unrelated insertion permits commit");
                        assert!(db.vertex(VId(99)).expect("published staged row").is_some());
                    } else {
                        assert_read_conflict(txn.commit(&mut db, &cx).await);
                        assert_eq!(db.frontier().expect("no loser publication"), frontier);
                        assert!(
                            db.vertex(VId(99))
                                .expect("unpublished staged row")
                                .is_none()
                        );
                    }
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
