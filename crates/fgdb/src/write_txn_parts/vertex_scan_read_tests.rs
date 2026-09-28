use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::GqlQueryPolicy;
use fgdb_gql::algebra::{GraphPatternBuilder, IntegerComparison, PreparedGraphPattern, VertexPredicate};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts, QueryCx};

const LABEL: LabelId = LabelId(1);
const OTHER: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}

fn pattern_with(predicate: VertexPredicate) -> PreparedGraphPattern {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder.filter("n", VertexPredicate::HasLabel(LABEL)).unwrap();
    builder.filter("n", predicate).unwrap();
    builder.prepare("n", 0, None).unwrap()
}

fn pattern() -> PreparedGraphPattern {
    pattern_with(VertexPredicate::IntegerProperty {
        key: P,
        comparison: IntegerComparison::Equal,
        value: 7,
    })
}

async fn fixture(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    );
    let mut database = Database::open_memory(cx, keys).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(VId(1), vec![LABEL], vec![(P, CanonicalScalar::Int(7))]);
    batch.create_vertex(VId(2), vec![LABEL], vec![(P, CanonicalScalar::Int(2))]);
    batch.create_vertex(VId(3), vec![OTHER], vec![(P, CanonicalScalar::Int(7))]);
    batch.create_vertex(VId(4), vec![LABEL], vec![]);
    database.write(cx, batch).await.unwrap();
    database
}

fn select(transaction: &WriteTxn, database: &Database<MemVfs>, cx: &QueryCx) -> Vec<VId> {
    transaction
        .execute_graph_pattern_governed(database, cx, &pattern(), policy())
        .unwrap()
        .value
}

fn is_read_conflict(result: Result<EmbeddedTxnCompletion, WriteTxnError>) -> bool {
    matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
        law: "FG-LAW-FCW-READ-01", ..
    })))
}

#[test]
fn selective_scans_allow_disjoint_writers_in_the_same_label_domain() {
    let ((), report) = run_async_under_lab(0x7653_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        for write in [false, true] {
            let mut database = fixture(&cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            assert_eq!(select(&transaction, &database, &qcx), vec![VId(1)]);
            assert_eq!(transaction.read_set.borrow().len(), 1);
            assert!(!transaction.scanned_vertices.get());
            assert!(transaction.scanned_vertex_labels.borrow().is_empty());
            assert_eq!(transaction.point_reads.borrow().2.len(), 1);
            assert!(transaction.point_reads.borrow().2[0].complete);
            if write {
                let mut append = WriteBatch::new(RelationId(1));
                append.create_vertex(VId(90), vec![OTHER], vec![]);
                transaction.write(&mut database, append).unwrap();
            }
            let mut winner = WriteBatch::new(RelationId(1));
            winner.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(3)));
            winner.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(8)));
            winner.delete_vertex(VId(4));
            winner.create_vertex(VId(5), vec![LABEL], vec![(P, CanonicalScalar::Int(8))]);
            database.write(&cx, winner).await.unwrap();
            let frontier = database.frontier().unwrap();
            let result = transaction.finish(&mut database, &cx).await.unwrap();
            assert_eq!(result.commit_seq().is_some(), write);
            assert_eq!(database.frontier().unwrap().0, frontier.0 + u64::from(write));
            assert_eq!(database.vertex(VId(90)).unwrap().is_some(), write);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn births_membership_entry_retirement_and_matched_payload_changes_conflict() {
    let ((), report) = run_async_under_lab(0x7653_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        for case in 0..6 {
            let mut database = fixture(&cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            assert_eq!(select(&transaction, &database, &qcx), vec![VId(1)]);
            let mut winner = WriteBatch::new(RelationId(1));
            match case {
                0 => { winner.create_vertex(VId(5), vec![LABEL], vec![(P, CanonicalScalar::Int(7))]); }
                1 => { winner.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(7))); }
                2 => { winner.set_vertex_label(VId(3), LABEL, true); }
                3 => { winner.delete_vertex(VId(1)); }
                4 => { winner.set_vertex_label(VId(1), LABEL, false); }
                _ => { winner.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(9))); }
            }
            database.write(&cx, winner).await.unwrap();
            let frontier = database.frontier().unwrap();
            assert!(is_read_conflict(transaction.finish(&mut database, &cx).await), "case {case}");
            assert_eq!(database.frontier().unwrap(), frontier);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn membership_entry_then_restoration_is_not_erased_by_the_current_head() {
    let ((), report) = run_async_under_lab(0x7653_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        for newly_born in [false, true] {
            let mut database = fixture(&cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            assert_eq!(select(&transaction, &database, &qcx), vec![VId(1)]);
            let vid = if newly_born { VId(5) } else { VId(2) };
            if newly_born {
                let mut birth = WriteBatch::new(RelationId(1));
                birth.create_vertex(vid, vec![LABEL], vec![(P, CanonicalScalar::Int(2))]);
                database.write(&cx, birth).await.unwrap();
            }
            for value in [7, 2] {
                let mut winner = WriteBatch::new(RelationId(1));
                winner.set_vertex_property(vid, P, Some(CanonicalScalar::Int(value)));
                database.write(&cx, winner).await.unwrap();
            }
            assert_eq!(database.vertex(vid).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(2))]);
            assert!(!transaction.read_set.borrow().contains(&ElementId::Vertex(vid)));
            assert!(is_read_conflict(transaction.finish(&mut database, &cx).await));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn null_and_scalar_predicates_use_the_native_type_and_absence_laws() {
    let ((), report) = run_async_under_lab(0x7653_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        for missing in [false, true] {
            let mut database = fixture(&cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            let null = pattern_with(VertexPredicate::PropertyNull { key: P, is_null: true });
            let result = transaction.execute_graph_pattern_governed(&database, &qcx, &null, policy()).unwrap();
            assert_eq!(result.value, vec![VId(4)]);
            let mut winner = WriteBatch::new(RelationId(1));
            winner.set_vertex_property(VId(2), P, (!missing).then_some(CanonicalScalar::Null));
            database.write(&cx, winner).await.unwrap();
            assert!(is_read_conflict(transaction.finish(&mut database, &cx).await));
        }
        let mut database = fixture(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        let scalar = pattern_with(VertexPredicate::ScalarProperty {
            key: P,
            predicate: fgdb_gql::algebra::ScalarPredicate::new(
                CanonicalScalar::ucs_basic_text("seven").unwrap(), IntegerComparison::Equal,
            ).unwrap(),
        });
        assert!(transaction.execute_graph_pattern_governed(&database, &qcx, &scalar, policy()).unwrap().value.is_empty());
        let mut outside = WriteBatch::new(RelationId(1));
        outside.set_vertex_property(VId(2), P, Some(CanonicalScalar::Bool(true)));
        database.write(&cx, outside).await.unwrap();
        assert!(transaction.transaction_conflict(&database, &mut || Ok(())).unwrap().is_none());
        let mut inside = WriteBatch::new(RelationId(1));
        inside.set_vertex_property(VId(2), P, Some(CanonicalScalar::ucs_basic_text("seven").unwrap()));
        database.write(&cx, inside).await.unwrap();
        assert!(is_read_conflict(transaction.finish(&mut database, &cx).await));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_selection_and_earlier_full_reads_survive_savepoint_rollback() {
    let ((), report) = run_async_under_lab(0x7653_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        for staged in [false, true] {
            let mut database = fixture(&cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            transaction.savepoint(&database, "before-read").unwrap();
            if staged {
                let mut edits = WriteBatch::new(RelationId(1));
                edits.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(7)));
                transaction.write(&mut database, edits).unwrap();
            } else {
                transaction.vertex(&database, VId(2)).unwrap();
            }
            assert_eq!(select(&transaction, &database, &qcx),
                if staged { vec![VId(1), VId(2)] } else { vec![VId(1)] });
            transaction.rollback_to_savepoint(&database, "before-read").unwrap();
            assert!(transaction.prepared.is_none());
            assert!(transaction.read_set.borrow().contains(&ElementId::Vertex(VId(2))));
            let mut winner = WriteBatch::new(RelationId(1));
            winner.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(9)));
            database.write(&cx, winner).await.unwrap();
            assert!(is_read_conflict(transaction.finish(&mut database, &cx).await));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_source_or_result_admission_remains_conservative_after_successful_retry() {
    let ((), report) = run_async_under_lab(0x7653_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        for source_failure in [false, true] {
            let mut database = fixture(&cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            transaction.savepoint(&database, "before-query").unwrap();
            let limits = if source_failure {
                GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000)
            } else {
                GqlQueryPolicy::new(100, 0, 1_000_000, 1_000_000)
            };
            assert!(transaction.execute_graph_pattern_governed(&database, &qcx, &pattern(), limits).is_err());
            assert!(!transaction.point_reads.borrow().2[0].complete);
            assert_eq!(select(&transaction, &database, &qcx), vec![VId(1)]);
            assert_eq!(transaction.point_reads.borrow().2.iter().map(|scan| scan.complete).collect::<Vec<_>>(), vec![false, true]);
            transaction.rollback_to_savepoint(&database, "before-query").unwrap();
            let mut winner = WriteBatch::new(RelationId(1));
            winner.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(3)));
            database.write(&cx, winner).await.unwrap();
            assert!(is_read_conflict(transaction.finish(&mut database, &cx).await));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn raw_scans_and_independent_roots_do_not_acquire_narrow_witnesses() {
    let ((), report) = run_async_under_lab(0x7653_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        for independent in [false, true] {
            let mut database = fixture(&cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            if independent {
                let mut builder = GraphPatternBuilder::new();
                builder.vertex("n").unwrap();
                builder.vertex("m").unwrap();
                builder.filter("n", VertexPredicate::HasLabel(LABEL)).unwrap();
                let pattern = builder.prepare_bindings(&["n", "m"], 0, None).unwrap();
                assert!(VertexScanRead::predicates(pattern.plan()).is_none());
                transaction.execute_graph_pattern_governed(&database, &qcx, &pattern, policy()).unwrap();
            } else {
                transaction.vertices(&database).unwrap();
                select(&transaction, &database, &qcx);
            }
            assert!(transaction.scanned_vertices.get());
            let mut winner = WriteBatch::new(RelationId(1));
            winner.create_vertex(VId(9), vec![OTHER], vec![]);
            database.write(&cx, winner).await.unwrap();
            assert!(is_read_conflict(transaction.finish(&mut database, &cx).await));
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_predicate_validation_is_cancellable_without_mutating_observations() {
    let ((), report) = run_async_under_lab(0x7653_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut database = fixture(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        select(&transaction, &database, &qcx);
        let mut winner = WriteBatch::new(RelationId(1));
        winner.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(3)));
        database.write(&cx, winner).await.unwrap();
        let mut count = 0;
        assert!(transaction.transaction_conflict(&database, &mut || { count += 1; Ok(()) }).unwrap().is_none());
        assert!(count > 10, "must visit before/after images and predicates");
        let frontier = database.frontier().unwrap();
        for stop in 1..=count {
            let mut seen = 0;
            assert!(matches!(transaction.transaction_conflict(&database, &mut || {
                seen += 1;
                if seen == stop { Err(WriteTxnError::NoPreparedWrite) } else { Ok(()) }
            }), Err(WriteTxnError::NoPreparedWrite)));
            assert_eq!(seen, stop);
            assert_eq!(database.frontier().unwrap(), frontier);
            assert!(transaction.point_reads.borrow().2[0].complete);
            assert_eq!(transaction.read_set.borrow().len(), 1);
            assert_eq!(txcx.outstanding_obligations(), 1);
        }
        transaction.finish(&mut database, &cx).await.unwrap();
        assert!(transaction.point_reads.borrow().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
