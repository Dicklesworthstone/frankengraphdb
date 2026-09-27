use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphDeleteError, GraphDeletePolicy,
    GraphDeleteStats, GraphSymbol, GraphSymbolKind, PreparedGraphDelete, PreparedGraphDeleteText,
};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts, QueryCx};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const INCIDENT: [EId; 4] = [EId(0), EId(7), EId(8), EId(u128::MAX)];

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xd1; 32], DatabaseSecurityNamespaceId([0xd2; 32]), [0xd3; 32])
}
fn policy() -> GraphDeletePolicy {
    GraphDeletePolicy::new(GqlQueryPolicy::new(100_000, 10_000, 10_000_000, 1_000_000), 10_000)
}
fn stats() -> GraphDeleteStats {
    GraphDeleteStats {
        selection: fgdb_gql::GqlExecutionStats {
            snapshot_records: 0,
            result_rows: 0,
        },
        evaluator: fgdb_gql::GlaExecutionStats::default(),
        target_vertices: 0,
        target_edges: 0,
    }
}
fn deletion(text: &str) -> PreparedGraphDelete {
    PreparedGraphDeleteText::prepare(text, R, |kind, name| match (kind, name) {
        (GraphSymbolKind::Label, "Victim") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, unrelated: u128) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=6 {
        batch.create_vertex(VId(id), vec![LabelId(if id <= 2 { 1 } else { 2 })],
            vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    for (eid, src, dst) in [(0, 1, 2), (7, 1, 2), (8, 2, 1), (u128::MAX, 1, 1)] {
        batch.add_edge(EId(eid), VId(src), VId(dst),
            vec![(P, CanonicalScalar::bytes(vec![7; 2048]).unwrap())]);
    }
    for id in 0..unrelated {
        batch.add_edge(EId(100 + id), VId(3), VId(4),
            vec![(P, CanonicalScalar::bytes(vec![8; 2048]).unwrap())]);
    }
    db.write(cx, batch).await.unwrap();
    let mut other = WriteBatch::new(S);
    other.add_edge(EId(91), VId(4), VId(5), vec![]);
    db.write(cx, other).await.unwrap();
}
fn collect(
    txn: &WriteTxn,
    db: &Database<MemVfs>,
    cx: &QueryCx,
    request: &PreparedGraphDelete,
) -> (GraphDeleteStats, (Vec<VId>, Vec<EId>)) {
    let proposal = request.execute_governed(policy(),
        |pattern, allowance| txn.execute_graph_pattern_governed(db, cx, pattern, allowance),
        || cx.checkpoint(),
    ).unwrap();
    (proposal.stats(), proposal.into_target_parts())
}
fn proof(
    txn: &WriteTxn,
    db: &Database<MemVfs>,
    vertices: &[VId],
    edges: &[EId],
) -> Result<GraphDeleteStats, DeleteAdmissionFault<()>> {
    let mut meter = DeleteAdmission { policy: policy(), stats: stats(), checkpoint: || Ok(()) };
    txn.prove_delete_incidence(db, vertices, edges, &mut |event| meter.observe(event))?;
    Ok(meter.stats)
}

#[test]
fn public_delete_uses_incident_source_rows_and_commits_once_with_reopen() {
    let ((), report) = run_async_under_lab(0xde1e_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let qcx = contexts.query();
        let tcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &cx, 128).await;
        let request = deletion("MATCH (a:Victim)-[e:R]->(b:Victim) DELETE a, b, e");
        let probe = db.begin(&tcx).unwrap();
        let (selected, _) = collect(&probe, &db, &qcx, &request);
        probe.abort();
        let mut txn = db.begin(&tcx).unwrap();
        let frontier = db.frontier().unwrap();
        let (actual, vertices, edges) = txn.execute_graph_delete_elements_returning_governed(
            &mut db, &qcx, &request, policy(),
        ).unwrap();
        assert_eq!(vertices, [VId(1), VId(2)]);
        assert_eq!(edges, INCIDENT);
        assert_eq!((actual.target_vertices, actual.target_edges), (2, 4));
        assert_eq!(actual.selection.snapshot_records, selected.selection.snapshot_records + 4);
        assert!(actual.evaluator.work_units > selected.evaluator.work_units);
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertex(VId(1)).unwrap().is_some());
        let committed = txn.commit(&mut db, &cx).await.unwrap();
        assert_eq!(committed, CommitSeq(frontier.0 + 1));
        assert_eq!(db.delta_since(frontier).unwrap().count(), 1);
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.vertex(VId(2)).unwrap().is_none());
        let remaining = db.edges().unwrap();
        assert_eq!(remaining.len(), 129);
        db.compact(&cx).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&cx, vfs, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), committed);
        assert_eq!(reopened.edges().unwrap(), remaining);
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn indexed_proof_agrees_with_full_overlay_oracle_for_cascades_aliases_and_relations() {
    let ((), report) = run_async_under_lab(0xde1e_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        for case in 0..6 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx, 8).await;
            let mut txn = db.begin(&tcx).unwrap();
            let mut batch = WriteBatch::new(R);
            match case {
                0 => {}
                1 => { batch.delete_edge(EId(0)); batch.delete_edge(EId(7)); }
                2 => { batch.delete_vertex(VId(2)); }
                3 => {
                    batch.ensure_edge_by_triple(EId(50), VId(1), VId(2), vec![]);
                    batch.add_edge(EId(51), VId(1), VId(3), vec![]);
                    batch.delete_edge(EId(51));
                }
                4 => { batch.add_edge(EId(50), VId(3), VId(1), vec![]); }
                _ => {
                    batch.add_edge(EId(50), VId(1), VId(3), vec![]);
                    batch.delete_vertex(VId(3));
                }
            }
            if !batch.is_empty() { txn.write(&mut db, batch).unwrap(); }
            if case == 4 {
                let mut other = WriteBatch::new(S);
                other.add_edge(EId(60), VId(1), VId(4), vec![]);
                txn.write_ordered(&mut db, vec![other]).unwrap();
            }
            // Full payload materialization is deliberately confined to this
            // independent oracle; production proves topology from coordinates.
            let mut picked: Vec<_> = txn.edges(&db).unwrap().iter()
                .filter(|edge| [VId(1), VId(2)].contains(&edge.entry.src)
                    || [VId(1), VId(2)].contains(&edge.entry.dst))
                .map(|edge| edge.entry.eid).collect();
            picked.sort_unstable();
            assert!(!picked.is_empty());
            let accepted = proof(&txn, &db, &[VId(1), VId(2)], &picked).unwrap();
            assert_eq!(accepted.selection.snapshot_records, 4);
            assert_eq!(proof(&txn, &db, &[VId(1), VId(2)], &picked).unwrap(), accepted,
                "warm witnesses do not discount a second invocation");
            for missing in 0..picked.len() {
                let mut incomplete = picked.clone();
                incomplete.remove(missing);
                assert!(matches!(proof(&txn, &db, &[VId(1), VId(2)], &incomplete),
                    Err(GqlQueryError::Source(GraphDeleteError::IncidentRelationships))),
                    "case {case}, unpicked {:?}", picked[missing]);
            }
            txn.abort();
        }
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn incidence_scan_ignores_unrelated_payloads_and_resolves_the_pinned_cut() {
    let ((), report) = run_async_under_lab(0xde1e_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx, 512).await;
        let txn = db.begin(&tcx).unwrap();
        let measured = proof(&txn, &db, &[VId(1), VId(2)], &INCIDENT).unwrap();
        assert_eq!(measured.selection.snapshot_records, 4);
        assert!(measured.evaluator.work_units < 512, "{measured:?}");
        assert_eq!(measured.evaluator.scratch_entries, 10);
        assert!(!txn.scanned_edges.get());
        assert_eq!(txn.read_set.borrow().len(), 6);
        let mut newer = WriteBatch::new(R);
        newer.delete_edge(EId(7));
        newer.add_edge(EId(50), VId(1), VId(6), vec![]);
        db.write(&cx, newer).await.unwrap();
        // Historical candidate 50 is not visible at the pinned cut. The old
        // edge 7 remains a winner there despite its newer retirement statement.
        assert_eq!(proof(&txn, &db, &[VId(1), VId(2)], &INCIDENT).unwrap()
            .selection.snapshot_records, 4);
        let current = db.begin(&tcx).unwrap();
        assert!(matches!(proof(&current, &db, &[VId(1), VId(2)], &INCIDENT),
            Err(GqlQueryError::Source(GraphDeleteError::IncidentRelationships))));
        let current_edges = [EId(0), EId(8), EId(50), EId(u128::MAX)];
        assert_eq!(proof(&current, &db, &[VId(1), VId(2)], &current_edges).unwrap()
            .selection.snapshot_records, 4, "a retired historical edge is not a source row");
        txn.abort();
        current.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_cumulative_budgets_admit_and_refused_tail_keeps_the_prefix() {
    let ((), report) = run_async_under_lab(0xde1e_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let qcx = contexts.query();
        let tcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx, 4).await;
        let request = deletion("MATCH (a:Victim)-[e:R]->(b:Victim) DELETE a, b, e");
        let mut measured = None;
        for case in 0..5 {
            let mut txn = db.begin(&tcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.set_vertex_property(VId(6), P, Some(CanonicalScalar::Int(66)));
            txn.write(&mut db, prefix).unwrap();
            txn.savepoint(&db, "prefix").unwrap();
            let previous = txn.prepared.as_ref().unwrap().template.clone();
            let (selected, targets) = collect(&txn, &db, &qcx, &request);
            let mut limit = policy();
            if let Some(complete) = measured {
                let complete: GraphDeleteStats = complete;
                limit.query = GqlQueryPolicy::new(
                    complete.selection.snapshot_records - u64::from(case == 2),
                    complete.selection.result_rows,
                    complete.evaluator.work_units - u64::from(case == 3),
                    complete.evaluator.scratch_entries - u64::from(case == 4),
                );
            }
            let result = txn.stage_delete_targets_controlled(
                &mut db, R, selected, targets, limit, || Ok::<(), ()>(()),
            );
            if case <= 1 {
                let (complete, _, _) = result.unwrap();
                if let Some(expected) = measured { assert_eq!(complete, expected); }
                measured = Some(complete);
            } else {
                match (case, result.unwrap_err()) {
                    (2, GqlQueryError::Rows(error)) => assert_eq!(
                        error.dimension, fgdb_gql::GqlBudgetDimension::SnapshotRecords),
                    (3, GqlQueryError::Evaluator(error)) => assert_eq!(
                        error.dimension, fgdb_gql::GlaLimitDimension::WorkUnits),
                    (4, GqlQueryError::Evaluator(error)) => assert_eq!(
                        error.dimension, fgdb_gql::GlaLimitDimension::ScratchEntries),
                    (_, error) => panic!("wrong refusal: {error:?}"),
                }
                assert_eq!(txn.staged.len(), 1);
                assert_eq!(txn.prepared.as_ref().unwrap().template, previous);
                assert_eq!(txn.savepoints.len(), 1);
                assert!(txn.pin.is_some());
            }
            assert!(db.vertex(VId(1)).unwrap().is_some());
            txn.abort();
        }
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_and_nonempty_incidence_gaps_and_cascades_remain_conflict_witnesses() {
    let ((), report) = run_async_under_lab(0xde1e_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        for case in 0..6 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx, 0).await;
            let mut txn = db.begin(&tcx).unwrap();
            if case == 4 { proof(&txn, &db, &[VId(6)], &[]).unwrap(); }
            else { proof(&txn, &db, &[VId(1)], &INCIDENT).unwrap(); }
            assert!(!txn.scanned_edges.get());
            let mut winner = WriteBatch::new(S);
            match case {
                0 | 3 => { winner.add_edge(EId(50), VId(1), VId(6), vec![]); }
                1 => { winner.add_edge(EId(50), VId(6), VId(1), vec![]); }
                2 => { winner.delete_vertex(VId(2)); }
                4 => { winner.add_edge(EId(50), VId(6), VId(6), vec![]); }
                _ => { winner.add_edge(EId(50), VId(5), VId(6), vec![]); }
            }
            db.write(&cx, winner).await.unwrap();
            if case == 3 {
                let mut restoration = WriteBatch::new(S);
                restoration.delete_edge(EId(50));
                db.write(&cx, restoration).await.unwrap();
            }
            let result = txn.finish(&mut db, &cx).await;
            if case == 5 { assert!(result.is_ok(), "unrelated topology: {result:?}"); }
            else { assert!(matches!(result, Err(WriteTxnError::Write(
                WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }
            )))); }
        }
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn data_dependent_refusal_and_unwind_retain_observations_across_rollback() {
    let ((), report) = run_async_under_lab(0xde1e_1006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        for unwind in [false, true] {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx, 0).await;
            let mut txn = db.begin(&tcx).unwrap();
            txn.savepoint(&db, "before").unwrap();
            let mut calls = 0;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                txn.prove_delete_incidence(&db, &[VId(1)], &[], &mut |_| {
                    calls += 1;
                    assert!(!unwind || calls != 2, "injected witness admission unwind");
                    Ok::<(), DeleteAdmissionFault<()>>(())
                })
            }));
            if unwind { assert!(result.is_err()); }
            else { assert!(matches!(result.unwrap(), Err(GqlQueryError::Source(
                GraphDeleteError::IncidentRelationships
            )))); }
            assert!(txn.point_reads.borrow().1.is_some());
            txn.rollback_to_savepoint(&db, "before").unwrap();
            assert!(txn.point_reads.borrow().1.is_some());
            assert!(txn.staged.is_empty());
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_property(VId(6), P, Some(CanonicalScalar::Int(9)));
            db.write(&cx, winner).await.unwrap();
            assert!(matches!(txn.finish(&mut db, &cx).await, Err(WriteTxnError::Write(
                WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }
            ))));
        }
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_post_selection_control_refusal_preserves_workspace_and_can_retry() {
    let ((), report) = run_async_under_lab(0xde1e_1007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let qcx = contexts.query();
        let tcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx, 0).await;
        let request = deletion("MATCH (a:Victim)-[e:R]->(b:Victim) DELETE a, b, e");
        let mut probe = db.begin(&tcx).unwrap();
        let (selected, targets) = collect(&probe, &db, &qcx, &request);
        let mut total = 0;
        probe.stage_delete_targets_controlled(&mut db, R, selected, targets, policy(), || {
            total += 1; Ok::<(), usize>(())
        }).unwrap();
        probe.abort();
        assert!(total > 20);
        let frontier = db.frontier().unwrap();
        for stop in 1..=total {
            let mut txn = db.begin(&tcx).unwrap();
            txn.savepoint(&db, "before").unwrap();
            let (selected, targets) = collect(&txn, &db, &qcx, &request);
            let mut seen = 0;
            let result = txn.stage_delete_targets_controlled(
                &mut db, R, selected, targets.clone(), policy(), || {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(seen, stop);
            assert!(txn.staged.is_empty());
            assert!(txn.prepared.is_none());
            assert_eq!(txn.savepoints.len(), 1);
            assert_eq!(txn.state(), EmbeddedTxnState::Active);
            assert!(txn.pin.is_some());
            txn.stage_delete_targets_controlled(
                &mut db, R, selected, targets, policy(), || Ok::<(), ()>(()),
            ).unwrap();
            assert_eq!(txn.staged.len(), 1);
            assert_eq!(db.frontier().unwrap(), frontier);
            txn.abort();
        }
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owner_health_initial_cancel_and_edge_only_fast_path_preserve_lifecycle() {
    let ((), report) = run_async_under_lab(0xde1e_1008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let tcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx, 0).await;
        let foreign = Database::open_memory(&cx, keys()).await.unwrap();
        let mut txn = db.begin(&tcx).unwrap();
        assert!(matches!(proof(&txn, &foreign, &[VId(1)], &INCIDENT),
            Err(GqlQueryError::Source(GraphDeleteError::Source(WriteTxnError::WrongDatabase)))));
        let cancelled = txn.prove_delete_incidence(&db, &[VId(1)], &INCIDENT,
            &mut |_| Err::<(), DeleteAdmissionFault<usize>>(GqlQueryError::Interrupted(1)));
        assert!(matches!(cancelled, Err(GqlQueryError::Interrupted(1))));
        assert!(txn.point_reads.borrow().is_empty());
        assert!(txn.read_set.borrow().is_empty());
        let mut zero_source = policy();
        zero_source.query = GqlQueryPolicy::new(0, 0, 2, 1);
        let result = txn.stage_delete_targets_controlled(
            &mut db, R, stats(), (Vec::new(), vec![EId(0)]), zero_source, || Ok::<(), ()>(()),
        ).unwrap();
        assert_eq!(result.0.selection.snapshot_records, 0);
        assert_eq!(result.0.evaluator.work_units, 2);
        assert!(!txn.scanned_edges.get());
        txn.rollback_to_savepoint(&db, "unknown").unwrap_err();
        txn.finish(&mut db, &cx).await.unwrap();
        assert!(matches!(proof(&txn, &db, &[VId(1)], &INCIDENT),
            Err(GqlQueryError::Source(GraphDeleteError::Source(WriteTxnError::Finished)))));
        let txn = db.begin(&tcx).unwrap();
        let mut write = WriteBatch::new(R);
        write.create_vertex(VId(9), vec![], vec![]);
        let prepared = db.prepare_write(write).unwrap();
        assert!(db.commit_template(&cx, prepared.template,
            Some(fgdb_chronicle::commit::CrashPoint::AfterMarkerBeforeD2), None, None,
        ).await.is_err());
        let result = txn.prove_delete_incidence(&db, &[VId(1)], &INCIDENT,
            &mut |_| -> Result<(), DeleteAdmissionFault<()>> { panic!("health must precede traversal") });
        assert!(matches!(result, Err(GqlQueryError::Source(GraphDeleteError::Source(
            WriteTxnError::Read(ReadError::CommitOutcomeUnknown { .. })
        )))));
        assert!(txn.point_reads.borrow().is_empty());
        assert!(txn.read_set.borrow().is_empty());
        txn.abort();
        assert_eq!(tcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
