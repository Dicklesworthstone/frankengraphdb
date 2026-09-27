use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::{GlaLimitDimension, GqlBudgetDimension, GqlQueryError, GqlQueryPolicy};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc4; 32], DatabaseSecurityNamespaceId([0xc5; 32]), [0xc6; 32])
}

fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1, 1, 100_000, 100_000) }

async fn seeded(cx: &CommitCx, bytes: usize, unrelated: usize) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    for id in 1..=3 {
        seed.create_vertex(VId(id), vec![LabelId(1)], vec![
            (P, CanonicalScalar::bytes(vec![0x27; bytes]).unwrap()),
            (Q, CanonicalScalar::bytes(vec![0x28; unrelated]).unwrap()),
        ]);
    }
    seed.add_edge(EId(10), VId(1), VId(2), vec![
        (P, CanonicalScalar::bytes(vec![0x29; bytes]).unwrap()),
        (Q, CanonicalScalar::bytes(vec![0x30; unrelated]).unwrap()),
    ]);
    db.write(cx, seed).await.unwrap();
    db
}

#[test]
fn exact_quotas_repeated_reads_and_all_below_boundary_refusals() {
    let ((), report) = run_async_under_lab(0x901b_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let query = contexts.query();
        let db = seeded(&cx, 1024, 4096).await;
        for edge in [false, true] {
            let txn = db.begin(&txcx).unwrap();
            let read = |policy| if edge {
                txn.edge_property_governed(&db, &query, EId(10), P, policy)
            } else {
                txn.vertex_property_governed(&db, &query, VId(1), P, policy)
            };
            let complete = read(policy()).unwrap();
            let exact = GqlQueryPolicy::new(1, 1, complete.evaluator.work_units, complete.evaluator.scratch_entries);
            assert_eq!(complete.rows.snapshot_records, 1);
            assert_eq!(complete.rows.result_rows, 1);
            assert_eq!(complete.value.len(), 1);
            assert_eq!(read(exact).unwrap(), complete, "warm witness cannot discount admission");
            for (policy, dimension) in [
                (GqlQueryPolicy::new(1, 1, exact.evaluator.max_work_units - 1, exact.evaluator.max_scratch_entries), Some(GlaLimitDimension::WorkUnits)),
                (GqlQueryPolicy::new(1, 1, exact.evaluator.max_work_units, exact.evaluator.max_scratch_entries - 1), Some(GlaLimitDimension::ScratchEntries)),
                (GqlQueryPolicy::new(0, 1, 100_000, 100_000), None),
                (GqlQueryPolicy::new(1, 0, 100_000, 100_000), None),
            ] {
                let error = read(policy).unwrap_err();
                match (error, dimension) {
                    (GqlQueryError::Evaluator(error), Some(expected)) => assert_eq!(error.dimension, expected),
                    (GqlQueryError::Rows(error), None) => assert!(matches!(error.dimension,
                        GqlBudgetDimension::SnapshotRecords | GqlBudgetDimension::ResultRows)),
                    (error, expected) => panic!("unexpected error {error:?}; wanted {expected:?}"),
                }
                assert_eq!(txn.state(), EmbeddedTxnState::Active);
                assert!(txn.pin.is_some());
            }
            assert_eq!(read(exact).unwrap(), complete);
            assert!(txn.read_set.borrow().is_empty());
            txn.abort();
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_payload_units_are_charged_before_copy_but_unrelated_fields_are_not() {
    let ((), report) = run_async_under_lab(0x901b_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let query = contexts.query();
        let mut executions = Vec::new();
        for (selected, unrelated) in [(64, 64), (1024, 64), (64, 32768)] {
            let db = seeded(&cx, selected, unrelated).await;
            let txn = db.begin(&txcx).unwrap();
            let vertex = txn.vertex_property_governed(&db, &query, VId(1), P, policy()).unwrap();
            let edge = txn.edge_property_governed(&db, &query, EId(10), P, policy()).unwrap();
            let label = txn.vertex_has_label_governed(&db, &query, VId(1), LabelId(1), policy()).unwrap();
            assert_eq!(label.value, vec![Some(true)]);
            executions.push((vertex.evaluator, edge.evaluator, label.evaluator));
            txn.abort();
        }
        let extra = (1024_usize.div_ceil(fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
            - 64_usize.div_ceil(fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)) as u64;
        for (small, large) in [(executions[0].0, executions[1].0), (executions[0].1, executions[1].1)] {
            assert_eq!(large.scratch_entries - small.scratch_entries, extra);
            assert_eq!(large.work_units - small.work_units, extra);
        }
        assert_eq!(executions[0], executions[2], "unselected bytes cannot become copy charges");
        assert_eq!(executions[0].2, executions[1].2, "membership does not read property payloads");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn absence_null_labels_and_staged_only_targets_keep_the_one_row_contract() {
    let ((), report) = run_async_under_lab(0x901b_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let query = contexts.query();
        let mut db = seeded(&cx, 16, 16).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut stage = WriteBatch::new(R);
        stage.set_vertex_property(VId(1), P, Some(CanonicalScalar::Null));
        stage.create_vertex(VId(u128::MAX), vec![LabelId(2)], vec![(P, CanonicalScalar::Int(7))]);
        stage.add_edge(EId(u128::MAX), VId(1), VId(u128::MAX), vec![(P, CanonicalScalar::Null)]);
        txn.write(&mut db, stage).unwrap();
        for (vertex, key, expected, records) in [
            (VId(1), P, Some(CanonicalScalar::Null), 1),
            (VId(1), PropertyKeyId(999), None, 1),
            (VId(999), P, None, 0),
            (VId(u128::MAX), P, Some(CanonicalScalar::Int(7)), 0),
        ] {
            let result = txn.vertex_property_governed(&db, &query, vertex, key,
                GqlQueryPolicy::new(records, 1, 100_000, 100_000)).unwrap();
            assert_eq!(result.value, vec![expected]);
            assert_eq!(result.rows.snapshot_records, records);
            assert_eq!(result.rows.result_rows, 1);
        }
        for (vertex, label, expected) in [
            (VId(1), LabelId(2), Some(false)),
            (VId(999), LabelId(1), None),
            (VId(u128::MAX), LabelId(2), Some(true)),
        ] {
            assert_eq!(txn.vertex_has_label_governed(&db, &query, vertex, label, policy()).unwrap().value,
                vec![expected]);
        }
        assert_eq!(txn.edge_property_governed(&db, &query, EId(u128::MAX), P,
            GqlQueryPolicy::new(0, 1, 100_000, 100_000)).unwrap().value, vec![Some(CanonicalScalar::Null)]);
        assert!(matches!(txn.vertex_property_governed(&db, &query, VId(999), P,
            GqlQueryPolicy::new(0, 0, 100_000, 100_000)), Err(GqlQueryError::Rows(error))
            if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 1));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn quota_refusal_retry_and_savepoint_rollback_retain_observations_without_losing_work() {
    let ((), report) = run_async_under_lab(0x901b_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let query = contexts.query();
        let mut db = seeded(&cx, 1024, 64).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(42)));
        txn.write(&mut db, prefix).unwrap();
        txn.savepoint(&db, "prefix").unwrap();
        let prepared = txn.prepared.as_ref().unwrap().template.clone();
        let frontier = db.frontier().unwrap();
        assert!(matches!(txn.edge_property_governed(&db, &query, EId(10), P,
            GqlQueryPolicy::new(0, 1, 100_000, 100_000)), Err(GqlQueryError::Rows(_))));
        assert_eq!(txn.point_reads.borrow().1, Some(ElementId::Edge(EId(10))));
        txn.edge_property_governed(&db, &query, EId(10), P, policy()).unwrap();
        txn.rollback_to_savepoint(&db, "prefix").unwrap();
        assert_eq!(txn.prepared.as_ref().unwrap().template, prepared);
        assert_eq!(txn.savepoints.len(), 1);
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 1);
        let mut unrelated = WriteBatch::new(R);
        unrelated.set_vertex_property(VId(3), Q, Some(CanonicalScalar::Int(99)));
        db.write(&cx, unrelated).await.unwrap();
        let result = txn.finish(&mut db, &cx).await;
        assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
            law: "FG-LAW-FCW-READ-01", ..
        }))));
        assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
        assert!(txn.point_reads.borrow().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
        assert_ne!(db.vertex(VId(2)).unwrap().unwrap().props.iter().find(|(k, _)| *k == Q).unwrap().1,
            CanonicalScalar::Int(42));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_callback_refusal_precedes_owned_delivery_and_preserves_the_active_workspace() {
    let ((), report) = run_async_under_lab(0x901b_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = seeded(&cx, 1024, 64).await;
        for element in [ElementId::Vertex(VId(1)), ElementId::Edge(EId(10))] {
            let txn = db.begin(&txcx).unwrap();
            let mut total = 0;
            txn.point_governed_with_checkpoint(&db, element, PointReadField::Property(P), policy(),
                || { total += 1; Ok::<_, usize>(()) }, |p| p.property.cloned()).unwrap();
            txn.abort();
            assert!(total > 20, "exercise payload as well as source and witness work");
            for stop in 1..=total {
                let txn = db.begin(&txcx).unwrap();
                let delivered = std::cell::Cell::new(false);
                let mut calls = 0;
                let result = txn.point_governed_with_checkpoint(&db, element, PointReadField::Property(P), policy(),
                    || { calls += 1; if calls == stop { Err(stop) } else { Ok(()) } },
                    |p| { delivered.set(true); p.property.cloned() });
                assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
                assert_eq!(calls, stop);
                assert!(!delivered.get(), "copy must follow every control boundary");
                assert_eq!(txn.state(), EmbeddedTxnState::Active);
                assert_eq!(txn.point_reads.borrow().1, (stop > 1).then_some(element));
                assert!(txn.pin.is_some());
                txn.abort();
            }
        }
        let txn = db.begin(&txcx).unwrap();
        let foreign = seeded(&cx, 16, 16).await;
        let mut calls = 0;
        let wrong = txn.point_governed_with_checkpoint(&foreign, ElementId::Vertex(VId(1)), PointReadField::Property(P), policy(),
            || { calls += 1; Ok::<_, ()>(()) }, |p| p.property.cloned());
        assert!(matches!(wrong, Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))));
        assert_eq!(calls, 0);
        assert!(txn.point_reads.borrow().is_empty());
        txn.abort();
        let mut terminal = db.begin(&txcx).unwrap();
        terminal.finish(&mut db, &cx).await.unwrap();
        assert!(matches!(terminal.point_governed_with_checkpoint(&db, ElementId::Vertex(VId(1)),
            PointReadField::Property(P), policy(), || Ok::<_, ()>(()), |p| p.property.cloned()),
            Err(GqlQueryError::Source(WriteTxnError::Finished))));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unwinding_during_delivery_retains_the_edge_domain_and_does_not_release_the_pin() {
    let ((), report) = run_async_under_lab(0x901b_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = seeded(&cx, 1024, 16).await;
        let mut txn = db.begin(&txcx).unwrap();
        let frontier = db.frontier().unwrap();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            txn.point_governed_with_checkpoint(&db, ElementId::Edge(EId(10)), PointReadField::Property(P), policy(),
                || Ok::<_, ()>(()), |_p| -> Option<CanonicalScalar> { panic!("injected private delivery unwind") })
        }));
        assert!(unwound.is_err());
        assert_eq!(txn.point_reads.borrow().1, Some(ElementId::Edge(EId(10))));
        assert_eq!(txn.state(), EmbeddedTxnState::Active);
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 1);
        let mut other = WriteBatch::new(R);
        other.set_vertex_label(VId(3), LabelId(2), true);
        db.write(&cx, other).await.unwrap();
        assert!(matches!(txn.finish(&mut db, &cx).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn successful_governed_reads_keep_precise_rebase_and_stale_read_rejection() {
    let ((), report) = run_async_under_lab(0x901b_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let query = contexts.query();
        for overlapping in [false, true] {
            let mut db = seeded(&cx, 16, 16).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.vertex_has_label_governed(&db, &query, VId(1), LabelId(1), policy()).unwrap();
            txn.vertex_property_governed(&db, &query, VId(1), P, policy()).unwrap();
            let mut update = WriteBatch::new(R);
            update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
            txn.write(&mut db, update).unwrap();
            let mut winner = WriteBatch::new(R);
            if overlapping {
                winner.set_vertex_label(VId(1), LabelId(1), false);
            } else {
                winner.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(88)));
            }
            db.write(&cx, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            let result = txn.commit_disjoint_fields_rebased(&mut db, &cx, 1).await;
            if overlapping {
                assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", ..
                }))));
                assert_eq!(db.frontier().unwrap(), frontier);
            } else {
                assert_eq!(result.unwrap(), CommitSeq(frontier.0 + 1));
                let fields = db.vertex(VId(1)).unwrap().unwrap().props;
                assert_eq!(fields, vec![(P, CanonicalScalar::Int(99)), (Q, CanonicalScalar::Int(88))]);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_unknown_and_recovery_fences_precede_governed_point_admission() {
    use crate::DerivedPublicationStage;
    use fgdb_chronicle::commit::CrashPoint;
    let ((), report) = run_async_under_lab(0x901b_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for unknown in [false, true] {
            let mut db = seeded(&cx, 64, 64).await;
            let txn = db.begin(&txcx).unwrap();
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_label(VId(3), LabelId(2), true);
            let prepared = db.prepare_write(winner).unwrap();
            let result = db.commit_template(
                &cx,
                prepared.template,
                unknown.then_some(CrashPoint::AfterMarkerBeforeD2),
                (!unknown).then_some(DerivedPublicationStage::FoldCommittedTemplate),
                None,
            ).await;
            assert!(result.is_err());
            for element in [ElementId::Vertex(VId(1)), ElementId::Edge(EId(10))] {
                let mut calls = 0;
                let result = txn.point_governed_with_checkpoint(
                    &db, element, PointReadField::Property(P), policy(),
                    || { calls += 1; Ok::<_, ()>(()) }, |p| p.property.cloned(),
                );
                match result {
                    Err(GqlQueryError::Source(WriteTxnError::Read(ReadError::CommitOutcomeUnknown { .. }))) => assert!(unknown),
                    Err(GqlQueryError::Source(WriteTxnError::Read(ReadError::RecoveryRequired(_)))) => assert!(!unknown),
                    result => panic!("expected native health fence: {result:?}"),
                }
                assert_eq!(calls, 0);
                assert!(txn.point_reads.borrow().is_empty());
                assert!(txn.pin.is_some());
            }
            txn.abort();
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
