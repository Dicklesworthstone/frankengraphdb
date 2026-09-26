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
        [0xf1; 32],
        DatabaseSecurityNamespaceId([0xf2; 32]),
        [0xf3; 32],
    )
}

async fn seed(database: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
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
    database.write(cx, batch).await.unwrap();
}

fn edit(key: PropertyKeyId, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(value)));
    batch
}

async fn drifted(cx: &CommitCx, txcx: &TxnCx) -> (Database<MemVfs>, WriteTxn) {
    let mut database = Database::open_memory(cx, keys()).await.unwrap();
    seed(&mut database, cx).await;
    let mut transaction = database.begin(txcx).unwrap();
    transaction.write(&mut database, edit(P, 7)).unwrap();
    database.write(cx, edit(Q, 8)).await.unwrap();
    (database, transaction)
}

fn assert_conflict(result: Result<CommitSeq, WriteTxnError>) {
    assert!(matches!(
        result,
        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
            law: "FG-LAW-FCW-01",
            ..
        }))
    ));
}

#[test]
fn independent_vertex_edge_and_label_edits_match_serial_execution_and_reopen() {
    let ((), report) = run_async_under_lab(0xf1e1_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut database = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut database, &cx).await;
        let pinned = database.read_session().unwrap();
        let mut edits = edit(P, 7);
        edits.set_vertex_property(VId(1), ABSENT, None);
        edits.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(9)));
        edits.set_vertex_label(VId(1), LabelId(1), false);
        edits.set_vertex_label(VId(1), LabelId(2), true);
        let mut transaction = database.begin(&txcx).unwrap();
        let mut ordinary = database.begin(&txcx).unwrap();
        transaction.write(&mut database, edits.clone()).unwrap();
        ordinary.write(&mut database, edits.clone()).unwrap();
        let original = transaction.prepared.as_ref().unwrap().template.clone();
        let mut concurrent = edit(Q, 8);
        concurrent.set_edge_property(EId(10), Q, Some(CanonicalScalar::Int(10)));
        concurrent.set_vertex_label(VId(1), LabelId(3), true);
        concurrent.add_edge(EId(20), VId(1), VId(3), vec![]);
        let frontier = database.write(&cx, concurrent.clone()).await.unwrap();
        assert_conflict(ordinary.commit(&mut database, &cx).await);
        assert!(transaction.refresh_snapshot(&database, &txcx).is_err());
        let seq = transaction
            .commit_disjoint_fields_rebased(&mut database, &cx, 5)
            .await
            .unwrap();
        assert_eq!(seq, CommitSeq(frontier.0 + 1));
        let tail = database.delta_since(frontier).unwrap().collect::<Vec<_>>();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].coordinate_entries(), original.coordinate_entries());
        assert_eq!(txcx.outstanding_obligations(), 0);
        assert_eq!(
            pinned.vertex(VId(1)).unwrap().unwrap().labels,
            vec![LabelId(1)]
        );
        assert!(pinned.edge(EId(20)).unwrap().is_none());
        assert_eq!(
            database.vertex(VId(1)).unwrap().unwrap().labels,
            vec![LabelId(2), LabelId(3)]
        );
        let mut serial = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut serial, &cx).await;
        serial.write(&cx, concurrent).await.unwrap();
        serial.write(&cx, edits).await.unwrap();
        assert_eq!(database.vertices().unwrap(), serial.vertices().unwrap());
        assert_eq!(database.edges().unwrap(), serial.edges().unwrap());
        let vertices = database.vertices().unwrap();
        let edges = database.edges().unwrap();
        drop(database);
        let database = Database::open_with_vfs(&cx, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(database.frontier().unwrap(), seq);
        assert_eq!(database.vertices().unwrap(), vertices);
        assert_eq!(database.edges().unwrap(), edges);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn overlapping_fields_conflict_even_after_value_restoration() {
    let ((), report) = run_async_under_lab(0xf1e1_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for domain in 0..3 {
            for restore in [false, true] {
                let mut database = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut database, &cx).await;
                let make = |value| {
                    let mut batch = WriteBatch::new(R);
                    match domain {
                        0 => {
                            batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(value)));
                        }
                        1 => {
                            batch.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(value)));
                        }
                        _ => {
                            batch.set_vertex_label(VId(1), LabelId(2), value != 0);
                        }
                    }
                    batch
                };
                let mut transaction = database.begin(&txcx).unwrap();
                transaction.write(&mut database, make(7)).unwrap();
                database.write(&cx, make(8)).await.unwrap();
                if restore {
                    database.write(&cx, make(0)).await.unwrap();
                }
                let frontier = database.frontier().unwrap();
                let before = (database.vertices().unwrap(), database.edges().unwrap());
                assert_conflict(
                    transaction
                        .commit_disjoint_fields_rebased(&mut database, &cx, 1)
                        .await,
                );
                assert_eq!(database.frontier().unwrap(), frontier);
                assert_eq!(
                    (database.vertices().unwrap(), database.edges().unwrap()),
                    before
                );
                assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn raw_guards_cancelled_edits_and_absent_removals_remain_dependencies() {
    let ((), report) = run_async_under_lab(0xf1e1_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..5 {
            for same_field in [false, true] {
                let mut database = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut database, &cx).await;
                let mut transaction = database.begin(&txcx).unwrap();
                let mut batch = WriteBatch::new(R);
                match case {
                    0 => {
                        batch.compare_and_set_vertex_property(
                            VId(1), P, Some(CanonicalScalar::Int(0)),
                            CanonicalScalar::Int(7), WriteMismatchPolicy::AbortWrite,
                        );
                    }
                    1 => {
                        batch.compare_and_set_vertex_property(
                            VId(1), P, Some(CanonicalScalar::Int(99)),
                            CanonicalScalar::Int(7), WriteMismatchPolicy::NoOp,
                        );
                    }
                    2 => {
                        batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
                        batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
                    }
                    3 => {
                        batch.set_vertex_property(VId(1), ABSENT, None);
                    }
                    _ => {
                        batch.compare_and_set_edge_property(
                            EId(10), P, Some(CanonicalScalar::Int(99)),
                            CanonicalScalar::Int(7), WriteMismatchPolicy::NoOp,
                        );
                    }
                }
                transaction.write(&mut database, batch).unwrap();
                let mut changed = WriteBatch::new(R);
                if same_field && case == 4 {
                    changed.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(8)));
                } else {
                    let key = if same_field {
                        if case == 3 { ABSENT } else { P }
                    } else {
                        Q
                    };
                    changed.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(8)));
                }
                database.write(&cx, changed).await.unwrap();
                let frontier = database.frontier().unwrap();
                let result = transaction
                    .commit_disjoint_fields_rebased(&mut database, &cx, 2)
                    .await;
                if same_field {
                    assert_conflict(result);
                    assert_eq!(database.frontier().unwrap(), frontier);
                } else {
                    assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                    let vertex = database.vertex(VId(1)).unwrap().unwrap();
                    assert_eq!(
                        vertex.props[0].1,
                        CanonicalScalar::Int(if case == 0 { 7 } else { 0 })
                    );
                    assert_eq!(vertex.props[1].1, CanonicalScalar::Int(8));
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn target_retirement_including_cascades_never_rebases() {
    let ((), report) = run_async_under_lab(0xf1e1_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..3 {
            let mut database = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut database, &cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            let mut edit = WriteBatch::new(R);
            if case == 0 {
                edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
            } else {
                edit.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(7)));
            }
            transaction.write(&mut database, edit).unwrap();
            let mut deletion = WriteBatch::new(R);
            if case == 1 {
                deletion.delete_edge(EId(10));
            } else {
                deletion.delete_vertex(VId(1));
            }
            database.write(&cx, deletion).await.unwrap();
            let frontier = database.frontier().unwrap();
            assert_conflict(
                transaction
                    .commit_disjoint_fields_rebased(&mut database, &cx, 1)
                    .await,
            );
            assert_eq!(database.frontier().unwrap(), frontier);
            assert!(database.edge(EId(10)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reads_savepoints_and_non_field_instructions_stay_ineligible() {
    let ((), report) = run_async_under_lab(0xf1e1_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for case in 0..9 {
            let mut database = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut database, &cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            let mut batch = edit(P, 7);
            match case {
                5 => { batch.ensure_vertex(VId(1), vec![], vec![]); }
                6 => { batch.delete_edge_if_present(EId(99)); }
                7 => { batch.create_vertex(VId(99), vec![], vec![]); }
                _ => {}
            }
            transaction.write(&mut database, batch).unwrap();
            match case {
                0 => { transaction.vertex(&database, VId(1)).unwrap(); }
                1 => { transaction.vertex(&database, VId(99)).unwrap(); }
                2 => { transaction.vertices(&database).unwrap(); }
                3 => { transaction.edges(&database).unwrap(); }
                4 => { transaction.savepoint(&database, "held").unwrap(); }
                8 => { transaction.program_multi_relation = true; }
                _ => {}
            }
            database.write(&cx, edit(Q, 8)).await.unwrap();
            let frontier = database.frontier().unwrap();
            assert!(matches!(
                transaction
                    .commit_disjoint_fields_rebased(&mut database, &cx, 10)
                    .await,
                Err(WriteTxnError::FieldRebaseIneligible)
            ));
            assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
            assert_eq!(database.frontier().unwrap(), frontier);
            assert_eq!(
                database.vertex(VId(1)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(0)
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn multi_relation_rebase_admits_the_whole_expanded_input() {
    let ((), report) = run_async_under_lab(0xf1e1_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for limit in [7, 8, 9] {
            let mut database = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut database, &cx).await;
            let mut seed_edge = WriteBatch::new(RelationId(2));
            seed_edge.add_edge(EId(20), VId(2), VId(3), vec![]);
            database.write(&cx, seed_edge).await.unwrap();
            let mut transaction = database.begin(&txcx).unwrap();
            let mut first = WriteBatch::new(RelationId(9));
            first.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
            first.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(7)));
            let mut last = WriteBatch::new(RelationId(2));
            last.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(7)));
            last.set_edge_property(EId(20), P, Some(CanonicalScalar::Int(7)));
            transaction
                .write_ordered(&mut database, vec![first, last])
                .unwrap();
            database.write(&cx, edit(Q, 8)).await.unwrap();
            let frontier = database.frontier().unwrap();
            // Two vertex instructions x relations {1,2,9}, plus two routed edges.
            let result = transaction
                .commit_disjoint_fields_rebased(&mut database, &cx, limit)
                .await;
            if limit == 7 {
                assert!(matches!(
                    result,
                    Err(WriteTxnError::OrderedWriteBudgetExceeded { limit: 7, required: 8 })
                ));
                assert_eq!(database.frontier().unwrap(), frontier);
                assert!(database.edge(EId(20)).unwrap().unwrap().props.is_empty());
            } else {
                assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                assert_eq!(
                    database.edge(EId(10)).unwrap().unwrap().props[0].1,
                    CanonicalScalar::Int(7)
                );
                assert_eq!(
                    database.edge(EId(20)).unwrap().unwrap().props[0].1,
                    CanonicalScalar::Int(7)
                );
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_checkpoint_and_unwind_before_publication_release_the_workspace() {
    use std::future::Future;
    use std::task::Poll;
    let ((), report) = run_async_under_lab(0xf1e1_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let (mut database, mut transaction) = drifted(&cx, &txcx).await;
        let mut total = 0;
        transaction
            .complete_rebased_controlled(
                &mut database,
                &cx,
                None,
                true,
                Some(RebasePreparation::DisjointFields(1)),
                || {
                    total += 1;
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert!(total > 10);
        for stop in 1..=total {
            let (mut database, mut transaction) = drifted(&cx, &txcx).await;
            let frontier = database.frontier().unwrap();
            let mut seen = 0;
            let result = transaction
                .complete_rebased_controlled(
                    &mut database,
                    &cx,
                    None,
                    true,
                    Some(RebasePreparation::DisjointFields(1)),
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
            assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
            assert!(transaction.prepared.is_none());
            assert!(transaction.staged.is_empty());
            assert_eq!(database.frontier().unwrap(), frontier);
            assert_eq!(
                database.vertex(VId(1)).unwrap().unwrap().props[0].1,
                CanonicalScalar::Int(0)
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        let (mut database, mut transaction) = drifted(&cx, &txcx).await;
        let frontier = database.frontier().unwrap();
        let mut seen = 0;
        let mut future = Box::pin(transaction.complete_rebased_controlled(
            &mut database,
            &cx,
            None,
            true,
            Some(RebasePreparation::DisjointFields(1)),
            || {
                seen += 1;
                assert_ne!(seen, total, "unwind at final acceptance");
                Ok(())
            },
        ));
        let panicked = std::future::poll_fn(|task| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                future.as_mut().poll(task)
            }));
            Poll::Ready(result.is_err())
        })
        .await;
        drop(future);
        assert!(panicked);
        assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
        assert_eq!(database.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ownership_unpolled_futures_and_native_crash_outcomes_are_preserved() {
    use fgdb_chronicle::commit::CrashPoint;
    let ((), report) = run_async_under_lab(0xf1e1_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let (mut database, mut transaction) = drifted(&cx, &txcx).await;
        let original = transaction.prepared.as_ref().unwrap().template.clone();
        let basis = transaction.basis();
        drop(transaction.commit_disjoint_fields_rebased(&mut database, &cx, 1));
        let mut foreign = Database::open_memory(&cx, keys()).await.unwrap();
        assert!(matches!(
            transaction
                .commit_disjoint_fields_rebased(&mut foreign, &cx, 1)
                .await,
            Err(WriteTxnError::WrongDatabase)
        ));
        assert_eq!(transaction.state(), EmbeddedTxnState::Active);
        assert_eq!(transaction.basis(), basis);
        assert_eq!(transaction.prepared.as_ref().unwrap().template, original);
        transaction
            .commit_disjoint_fields_rebased(&mut database, &cx, 1)
            .await
            .unwrap();
        assert!(matches!(
            transaction
                .commit_disjoint_fields_rebased(&mut database, &cx, 1)
                .await,
            Err(WriteTxnError::Finished)
        ));
        for crash in [CrashPoint::BeforeCapsule, CrashPoint::AfterMarkerBeforeD2] {
            let (mut database, mut transaction) = drifted(&cx, &txcx).await;
            let frontier = database.frontier().unwrap();
            let result = transaction
                .complete_rebased_controlled(
                    &mut database,
                    &cx,
                    Some(crash),
                    true,
                    Some(RebasePreparation::DisjointFields(1)),
                    || Ok(()),
                )
                .await;
            assert!(result.is_err());
            let expected = if crash == CrashPoint::BeforeCapsule {
                EmbeddedTxnState::Aborted
            } else {
                EmbeddedTxnState::CommitOutcomeUnknown {
                    published_frontier: frontier,
                }
            };
            assert_eq!(transaction.state(), expected);
            assert!(transaction.prepared.is_none());
            assert!(transaction.pin.is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn field_domains_keep_kinds_and_keys_distinct_and_refuse_unknown_history() {
    let mut footprint = FieldRebaseFootprint::default();
    let mut batch = WriteBatch::new(R);
    batch.set_vertex_property(VId(1), P, None);
    batch.set_edge_property(EId(2), Q, None);
    batch.set_vertex_label(VId(3), LabelId(1), true);
    for row in &batch.rows {
        footprint.record(row).unwrap();
    }
    for (elem, key, expected) in [
        (ElementId::Vertex(VId(1)), P, true),
        (ElementId::Vertex(VId(1)), Q, false),
        (ElementId::Edge(EId(1)), P, false),
        (ElementId::Edge(EId(2)), Q, true),
        (ElementId::Vertex(VId(2)), Q, false),
    ] {
        let row = DeltaRow::Property {
            elem,
            property: key,
            before: None,
            after: Some(CanonicalScalar::Int(1)),
        };
        assert_eq!(footprint.conflicts(&row, &mut || Ok(())).unwrap(), expected);
    }
    let mut checks = 0;
    let cascade = DeltaRow::DeleteVertex {
        vid: VId(99),
        before_version: ObjectId([0; 32]),
        sorted_retired_incident_edges: vec![EId(1), EId(2), EId(3)],
    };
    assert!(
        footprint
            .conflicts(&cascade, &mut || {
                checks += 1;
                Ok(())
            })
            .unwrap()
    );
    assert_eq!(checks, 3);
    let schema = DeltaRow::Schema {
        transition_oid: ObjectId([0; 32]),
        before_epoch: SchemaEpoch(1),
        after_epoch: SchemaEpoch(2),
    };
    assert!(matches!(
        footprint.conflicts(&schema, &mut || Ok(())),
        Err(WriteTxnError::FieldRebaseIneligible)
    ));
}
