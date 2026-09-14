//! Completion tests use real embedded transactions and the Chronicle path.
//! A read close must not mint a write sequence or forget rejected observations.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, DatabaseState, DerivedPublicationStage, MemVfs,
    ReadError, WriteBatch, WriteError, WriteMismatchPolicy, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId,
    EmbeddedTxnCompletion, EmbeddedTxnState, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const L: LabelId = LabelId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x31; 32], DatabaseSecurityNamespaceId([0x32; 32]), [0x33; 32])
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut database = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(10))]);
    batch.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(20))]);
    database.write(cx, batch).await.unwrap();
    database
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

#[test]
fn read_close_preserves_every_published_identity_and_consumes_no_sequence() {
    let ((), report) = run_async_under_lab(0xf171_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut database = seeded(&cx).await;
        let basis = database.frontier().unwrap();
        let manifest = database.manifest().unwrap();
        let partition = database.partition_root().unwrap();
        let baseline = txcx.outstanding_obligations();
        for read in [false, true] {
            let mut transaction = database.begin(&txcx).unwrap();
            if read { assert!(transaction.vertex(&database, VId(1)).unwrap().is_some()); }
            let completed = transaction.finish(&mut database, &cx).await.unwrap();
            assert_eq!(completed, EmbeddedTxnCompletion::ReadClosed {
                snapshot_seq: basis, validated_through: basis,
            });
            assert_eq!(completed.commit_seq(), None);
            assert_eq!(transaction.state(), EmbeddedTxnState::Completed(completed));
            assert_eq!(txcx.outstanding_obligations(), baseline);
            assert!(matches!(transaction.vertex(&database, VId(1)), Err(WriteTxnError::Finished)));
            assert!(matches!(transaction.finish(&mut database, &cx).await, Err(WriteTxnError::Finished)));
            assert_eq!(database.frontier().unwrap(), basis);
            assert_eq!(database.manifest().unwrap(), manifest);
            assert_eq!(database.partition_root().unwrap(), partition);
            assert_eq!(database.delta_since(basis).unwrap().count(), 0);
        }
        let mut write = WriteBatch::new(R);
        write.create_vertex(VId(3), vec![], vec![]);
        assert_eq!(database.write(&cx, write).await.unwrap(), CommitSeq(basis.0 + 1));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn advanced_frontier_validation_does_not_relabel_the_snapshot_as_current() {
    let ((), report) = run_async_under_lab(0xf171_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut database = seeded(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        let basis = transaction.basis();
        transaction.vertex(&database, VId(1)).unwrap();
        let mut unrelated = WriteBatch::new(R);
        unrelated.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(30)));
        let advanced = database.write(&cx, unrelated).await.unwrap();
        assert_eq!(transaction.finish(&mut database, &cx).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed { snapshot_seq: basis, validated_through: advanced });
        assert_eq!(transaction.basis(), basis);
        assert_eq!(database.frontier().unwrap(), advanced);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn read_close_checks_point_absence_and_discarded_query_observations_without_repair_reads() {
    let ((), report) = run_async_under_lab(0xf171_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let query_cx = contexts.query();
        let txcx = contexts.txn();
        for mode in 0..6 {
            let mut database = seeded(&cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            match mode {
                0 => { transaction.vertex(&database, VId(1)).unwrap(); }
                1 => { assert!(transaction.vertex(&database, VId(99)).unwrap().is_none()); }
                _ => {
                    let text = match mode {
                        2 => "MATCH (n:L) WHERE n.p > 100 RETURN n",
                        3 => "MATCH (n:L) RETURN n LIMIT 0",
                        4 => "MATCH (n:L) RETURN n",
                        _ => "MATCH (n:L) WHERE n.p > 100 RETURN n LIMIT 0",
                    };
                    let query = PreparedGraphText::prepare(text, symbols).unwrap()
                        .bind_parameters(&GqlParameters::new()).unwrap();
                    let result = transaction.execute_graph_pattern_governed(
                        &database, &query_cx, &query,
                        GqlQueryPolicy::new(100, if mode == 4 { 0 } else { 100 }, 100_000, 100_000),
                    );
                    if mode == 4 { assert!(result.is_err()); }
                    else { assert!(result.unwrap().value.is_empty()); }
                }
            }
            let mut winner = WriteBatch::new(R);
            if mode == 1 || mode == 5 {
                winner.create_vertex(VId(99), vec![L], vec![(P, CanonicalScalar::Int(200))]);
            } else {
                winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(200)));
            }
            let frontier = database.write(&cx, winner).await.unwrap();
            assert!(matches!(transaction.finish(&mut database, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", ..
                }))), "mode {mode}");
            assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
            assert_eq!(database.frontier().unwrap(), frontier);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_preparation_observations_are_validated_even_without_a_prepared_write() {
    let ((), report) = run_async_under_lab(0xf171_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut database = seeded(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        let mut failed = WriteBatch::new(R);
        failed.compare_and_set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)),
            CanonicalScalar::Int(11), WriteMismatchPolicy::AbortWrite);
        assert!(matches!(transaction.write(&mut database, failed),
            Err(WriteTxnError::Write(WriteError::CompareAndSetMismatch(_)))));
        let mut winner = WriteBatch::new(R);
        winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
        let frontier = database.write(&cx, winner).await.unwrap();
        assert!(matches!(transaction.finish(&mut database, &cx).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
        assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
        assert_eq!(database.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_op_writes_remain_writes_and_legacy_commit_still_requires_a_batch() {
    let ((), report) = run_async_under_lab(0xf171_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut database = seeded(&cx).await;
        let basis = database.frontier().unwrap();
        let mut empty = database.begin(&txcx).unwrap();
        assert!(matches!(empty.commit(&mut database, &cx).await, Err(WriteTxnError::NoPreparedWrite)));
        assert_eq!(empty.state(), EmbeddedTxnState::Aborted);
        assert_eq!(txcx.outstanding_obligations(), 0);
        let original = database.vertex(VId(1)).unwrap();
        let mut transaction = database.begin(&txcx).unwrap();
        let mut noop = WriteBatch::new(R);
        noop.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(10)));
        transaction.write(&mut database, noop).unwrap();
        let completed = transaction.finish(&mut database, &cx).await.unwrap();
        assert_eq!(completed, EmbeddedTxnCompletion::WriteCommitted { commit_seq: CommitSeq(basis.0 + 1) });
        assert_eq!(completed.commit_seq(), Some(CommitSeq(basis.0 + 1)));
        assert_eq!(transaction.state(), EmbeddedTxnState::Completed(completed));
        assert_eq!(database.vertex(VId(1)).unwrap(), original);
        assert_eq!(database.delta_since(basis).unwrap().count(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn foreign_owner_and_unpolled_completion_preserve_the_active_workspace() {
    let ((), report) = run_async_under_lab(0xf171_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut owner = seeded(&cx).await;
        let mut foreign = seeded(&cx).await;
        let mut transaction = owner.begin(&txcx).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(9), vec![], vec![]);
        transaction.write(&mut owner, batch).unwrap();
        let before = transaction.staged_effect_digest().unwrap();
        drop(transaction.finish(&mut owner, &cx));
        drop(transaction.commit(&mut owner, &cx));
        assert!(matches!(transaction.finish(&mut foreign, &cx).await, Err(WriteTxnError::WrongDatabase)));
        assert_eq!(transaction.state(), EmbeddedTxnState::Active);
        assert_eq!(transaction.staged_effect_digest().unwrap(), before);
        assert_eq!(txcx.outstanding_obligations(), 1);
        assert!(matches!(transaction.finish(&mut owner, &cx).await.unwrap(), EmbeddedTxnCompletion::WriteCommitted { .. }));
        assert!(owner.vertex(VId(9)).unwrap().is_some());
        assert!(foreign.vertex(VId(9)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn commit_faults_keep_abort_and_unknown_distinct_and_recover_whole_writes() {
    let ((), report) = run_async_under_lab(0xf171_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for point in [fgdb::CrashPoint::BeforeCapsule, fgdb::CrashPoint::AfterD1,
            fgdb::CrashPoint::AfterMarkerBeforeD2] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut database = Database::create_with_vfs(&cx, vfs.clone(), &path, keys()).await.unwrap();
            let basis = database.frontier().unwrap();
            let mut transaction = database.begin(&txcx).unwrap();
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(1), vec![], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            transaction.write(&mut database, batch).unwrap();
            assert!(transaction.finish_with_crash(&mut database, &cx, Some(point)).await.is_err());
            if point == fgdb::CrashPoint::AfterMarkerBeforeD2 {
                assert_eq!(transaction.state(), EmbeddedTxnState::CommitOutcomeUnknown { published_frontier: basis });
                assert!(matches!(database.state(), DatabaseState::CommitOutcomeUnknown { .. }));
            } else {
                assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
                assert_eq!(database.frontier().unwrap(), basis);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
            assert!(matches!(transaction.commit(&mut database, &cx).await, Err(WriteTxnError::Finished)));
            drop(database);
            let reopened = Database::open_with_vfs(&cx, vfs, &path, keys()).await.unwrap();
            let vertices = reopened.vertices().unwrap();
            assert!(vertices.is_empty() || vertices.iter().map(|row| row.vid).collect::<Vec<_>>() == vec![VId(1), VId(2)]);
            if point != fgdb::CrashPoint::AfterMarkerBeforeD2 { assert!(vertices.is_empty()); }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_fenced_database_cannot_certify_even_an_empty_read_close() {
    let ((), report) = run_async_under_lab(0xf171_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut database = seeded(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        let mut winner = WriteBatch::new(R);
        winner.create_vertex(VId(99), vec![], vec![]);
        assert!(matches!(database.write_with_publication_failure(&cx, winner,
            DerivedPublicationStage::FoldCommittedTemplate).await,
            Err(WriteError::CommittedNeedsRecovery { .. })));
        assert!(matches!(transaction.finish(&mut database, &cx).await,
            Err(WriteTxnError::Read(ReadError::RecoveryRequired(_)))));
        assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
        assert_eq!(txcx.outstanding_obligations(), 0);
        let recovered = database.recover_authoritatively(&cx).await.unwrap();
        assert!(recovered.vertex(VId(99)).unwrap().is_some());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
