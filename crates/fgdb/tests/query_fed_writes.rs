//! Query-fed writes use native reads, canonical staging and Chronicle finish.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::query_write::{GraphQueryWriteExecutionError as Error, PreparedGraphQueryWrite};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeOutcome, GraphWriteProgramPolicy, GraphWriteProgramStats,
    GraphWriteScriptExecutionError, PreparedGraphSetText, PreparedGraphWriteScript,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const N: LabelId = LabelId(1);
const CITY: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "N") => Some(GraphSymbol::Label(N)),
        (GraphSymbolKind::Label, "City") => Some(GraphSymbol::Label(CITY)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn definition(source: &str, script: &str) -> PreparedGraphQueryWrite {
    let source = PreparedGraphSetText::prepare(source, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let script = PreparedGraphWriteScript::prepare(script, R, symbols).unwrap();
    PreparedGraphQueryWrite::prepare(source, script).unwrap()
}

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(10_000, 1_000, 5_000_000, 2_000_000),
        100,
        100,
        0,
    )
}

fn seed(entries: &[(VId, i64, i64)]) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for &(id, p, q) in entries {
        batch.create_vertex(
            id,
            vec![N],
            vec![(P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(q))],
        );
    }
    batch
}

#[test]
fn duplicate_keys_see_prior_creates_but_new_source_rows_are_not_reread() {
    let ((), report) = run_async_under_lab(0x94ca_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(
            &commit,
            seed(&[(VId(1), 1, 1), (VId(2), 2, 2), (VId(3), 1, 3)]),
        )
        .await
        .unwrap();
        let before = db.frontier().unwrap();
        let definition = definition(
            "MATCH (n:N) RETURN n.p AS key,n.q AS ordinal ORDER BY ordinal",
            "MERGE (m:City {p:$key}) ON CREATE SET m.q=10 ON MATCH SET m.q=m.q+1; \
             CREATE (:N {p:9,q:99})",
        );
        let next = AtomicU64::new(100);
        let (execution, completion) = db
            .execute_graph_query_write_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &definition,
                policy(),
                6,
                |request| -> Result<ElementId, ()> {
                    let id = next.fetch_add(1, Ordering::Relaxed);
                    assert!(matches!(
                        request.request,
                        GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    ));
                    assert_eq!(request.statement, [0, 1, 2, 3, 5][(id - 100) as usize]);
                    Ok(ElementId::Vertex(VId(u128::from(id))))
                },
            )
            .await
            .unwrap();
        assert_eq!(next.load(Ordering::Relaxed), 105);
        assert_eq!(execution.input_rows, 3);
        assert_eq!(execution.stats.completed_statements, 6);
        assert_eq!(execution.stats.created_vertices, 5);
        assert_eq!(execution.stats.mutation_effects, 3);
        let receipt = execution.receipt.unwrap();
        assert_eq!(receipt.steps().len(), 6);
        assert_eq!(
            receipt.steps()[0].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(VId(100)))
        );
        assert_eq!(
            receipt.steps()[2].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(VId(102)))
        );
        assert_eq!(
            receipt.steps()[4].merged_vertex(),
            Some(GraphVertexMergeOutcome::Matched(VId(100)))
        );
        assert_eq!(
            execution.stats.selection.result_rows,
            receipt.stats().selection.result_rows + 3
        );
        assert!(execution.stats.evaluator.work_units > receipt.stats().evaluator.work_units);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 8);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 8);
        for (id, p, q) in [(100, 1, 11), (102, 2, 10)] {
            assert_eq!(
                db.vertex(VId(id)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (Q, CanonicalScalar::Int(q).into())
                ]
            );
        }
        for id in [101, 103, 104] {
            assert_eq!(
                db.vertex(VId(id)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(9).into()),
                    (Q, CanonicalScalar::Int(99).into())
                ]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_source_and_late_program_failure_preserve_prefix_coordinates_and_spent_ids() {
    let ((), report) = run_async_under_lab(0x94ca_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed(&[(VId(1), 1, 1)])).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, seed(&[(VId((1_u128 << 100) | 1), 2, 2)]))
            .unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let failing = definition(
            "MATCH (n:N) RETURN n.p AS key ORDER BY key",
            "CREATE (m:City {p:$key,q:$key}); \
             MERGE (m:City {p:$key}) ON MATCH SET m.q=10/(2-m.q)",
        );
        let next = AtomicU64::new(100);
        let error = txn
            .execute_graph_query_write_governed(
                &mut db,
                &cx,
                &failing,
                policy(),
                4,
                |request| -> Result<ElementId, ()> {
                    let id = next.fetch_add(1, Ordering::Relaxed);
                    assert_eq!(request.statement, [0, 2][(id - 100) as usize]);
                    Ok(ElementId::Vertex(VId(u128::from(id))))
                },
            )
            .unwrap_err();
        assert!(matches!(error,
            Error::Program(GraphWriteScriptExecutionError::BatchProgram {
                location: Some(location), ..
            }) if location.argument_set == 1 && location.statement == 1));
        assert_eq!(next.load(Ordering::Relaxed), 102);
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(txn.vertices(&db).unwrap().len(), 2);
        assert_eq!(db.frontier().unwrap(), before);
        let good = definition(
            "MATCH (n:N) RETURN n.p AS key ORDER BY key",
            "MERGE (m:City {p:$key}) ON CREATE SET m.q=5",
        );
        let result = txn
            .execute_graph_query_write_governed(
                &mut db,
                &cx,
                &good,
                policy(),
                2,
                |_| -> Result<ElementId, ()> {
                    Ok(ElementId::Vertex(VId(u128::from(
                        next.fetch_add(1, Ordering::Relaxed),
                    ))))
                },
            )
            .unwrap();
        assert_eq!(result.input_rows, 2);
        assert_eq!(result.stats.created_vertices, 2);
        assert_eq!(next.load(Ordering::Relaxed), 104);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 4);
        for id in [100, 101] {
            assert!(db.vertex(VId(id)).unwrap().is_none());
        }
        for (id, p) in [(102, 1), (103, 2)] {
            assert_eq!(
                db.vertex(VId(id)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (Q, CanonicalScalar::Int(5).into())
                ]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_shared_budgets_pass_and_one_below_each_dimension_restores_the_prefix() {
    let ((), report) = run_async_under_lab(0x94ca_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let definition = definition(
            "UNWIND [1,2,1] AS key RETURN key",
            "MERGE (m:City {p:$key}) ON CREATE SET m.q=1 ON MATCH SET m.q=m.q+1",
        );
        let mut measured: Option<GraphWriteProgramStats> = None;
        // Generous positive control, exact six-dimensional allowance, then
        // each allowance one below its independently observed complete use.
        for case in 0..8 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, seed(&[(VId(90), 99, 99)])).await.unwrap();
            let before = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, seed(&[(VId(91), 98, 98)])).unwrap();
            let prefix = txn.staged_effect_digest().unwrap();
            let allowance = if let Some(stats) = measured {
                let mut counts = [
                    stats.selection.snapshot_records,
                    stats.selection.result_rows,
                    stats.evaluator.work_units,
                    stats.evaluator.scratch_entries,
                    stats.mutation_effects,
                    stats.created_vertices,
                ];
                assert!(counts.iter().all(|count| *count > 0));
                if case >= 2 {
                    counts[case - 2] -= 1;
                }
                GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(counts[0], counts[1], counts[2], counts[3]),
                    counts[4],
                    counts[5],
                    0,
                )
            } else {
                policy()
            };
            let next = AtomicU64::new(100);
            let result = txn.execute_graph_query_write_governed(
                &mut db,
                &cx,
                &definition,
                allowance,
                3,
                |_| -> Result<ElementId, ()> {
                    Ok(ElementId::Vertex(VId(u128::from(
                        next.fetch_add(1, Ordering::Relaxed),
                    ))))
                },
            );
            if case < 2 {
                let result = result.unwrap();
                assert_eq!(result.input_rows, 3);
                assert_eq!(result.stats.created_vertices, 2);
                assert_eq!(result.stats.mutation_effects, 3);
                if let Some(expected) = measured {
                    assert_eq!(result.stats, expected);
                }
                measured = Some(result.stats);
            } else {
                assert!(
                    matches!(
                        result,
                        Err(Error::Source(_) | Error::Binding(_) | Error::Program(_))
                    ),
                    "case {case}"
                );
                assert_eq!(txn.staged_effect_digest().unwrap(), prefix, "case {case}");
                assert_eq!(txn.vertices(&db).unwrap().len(), 2);
            }
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(db.vertices().unwrap().len(), 1);
            txn.abort();
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_bad_input_never_allocates_and_empty_input_keeps_lifecycle_and_noop_contracts() {
    let ((), report) = run_async_under_lab(0x94ca_0014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut other = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, seed(&[(VId(90), 99, 99)])).unwrap();
        let prefix = txn.staged_effect_digest().unwrap();
        let calls = AtomicU64::new(0);
        for source in [
            "UNWIND [1,null] AS key RETURN key",
            "UNWIND [1,[2]] AS key RETURN key",
            "UNWIND [1,'wrong'] AS key RETURN key",
        ] {
            let definition = definition(source, "MERGE (m:City {p:$key})");
            let error = txn
                .execute_graph_query_write_governed(
                    &mut db,
                    &cx,
                    &definition,
                    policy(),
                    2,
                    |_| -> Result<ElementId, ()> {
                        calls.fetch_add(1, Ordering::Relaxed);
                        Ok(ElementId::Vertex(VId(100)))
                    },
                )
                .unwrap_err();
            assert!(matches!(error, Error::Binding(_)));
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert_eq!(txn.staged_effect_digest().unwrap(), prefix);
        }
        let empty = definition("UNWIND [] AS key RETURN key", "MERGE (m:City {p:$key})");
        let zero = GraphWriteProgramPolicy::new(GqlQueryPolicy::new(0, 0, 0, 0), 0, 0, 0);
        let error = txn
            .execute_graph_query_write_governed(&mut other, &cx, &empty, zero, 0, |_| {
                Err::<ElementId, ()>(())
            })
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Preflight(WriteTxnError::WrongDatabase)
        ));
        txn.abort();
        // `abort` consumes the transaction, so executing after it is already
        // impossible at compile time. Close an empty workspace through the
        // non-consuming `finish` and check the runtime refusal instead.
        let mut closed = db.begin(&txcx).unwrap();
        closed.finish(&mut db, &commit).await.unwrap();
        let error = closed
            .execute_graph_query_write_governed(&mut db, &cx, &empty, zero, 0, |_| {
                Err::<ElementId, ()>(())
            })
            .unwrap_err();
        assert!(matches!(error, Error::Preflight(WriteTxnError::Finished)));
        let (result, completion) = db
            .execute_graph_query_write_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &empty,
                policy(),
                0,
                |_| Err::<ElementId, ()>(()),
            )
            .await
            .unwrap();
        assert_eq!(result.input_rows, 0);
        assert_eq!(result.stats.completed_statements, 0);
        assert!(result.receipt.is_none());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        // Exercise the native identity-owning variant on a real creation too.
        let one = definition("RETURN 4 AS key", "MERGE (m:City {p:$key})");
        let mut txn = db.begin(&txcx).unwrap();
        let result = txn
            .execute_graph_query_write_engine_governed(&mut db, &cx, &one, policy(), 1)
            .unwrap();
        assert_eq!(result.stats.created_vertices, 1);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(
            db.vertices().unwrap()[0].props,
            vec![(P, CanonicalScalar::Int(4).into())]
        );
        let before_match = db.frontier().unwrap();
        let (result, completion) = db
            .execute_graph_query_write_autocommit_engine_governed(
                &txcx,
                &cx,
                &commit,
                &one,
                policy(),
                1,
            )
            .await
            .unwrap();
        assert_eq!(result.input_rows, 1);
        assert_eq!(result.stats.created_vertices, 0);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before_match);
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn final_checkpoint_after_accepted_inner_program_rolls_back_only_this_operation() {
    let ((), report) = run_async_under_lab(0x94ca_0015, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let definition = definition("RETURN 1 AS key", "CREATE (m:City {p:$key})");
        let mut last = None;
        for refuse in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, seed(&[(VId(90), 99, 99)])).unwrap();
            let prefix = txn.staged_effect_digest().unwrap();
            let probe = Arc::new(SimulationCheckpointProbe::new(last));
            let cx = contexts.query().with_checkpoint_probe(Arc::clone(&probe));
            let calls = AtomicU64::new(0);
            let result = txn.execute_graph_query_write_governed(
                &mut db,
                &cx,
                &definition,
                policy(),
                1,
                |_| -> Result<ElementId, ()> {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(ElementId::Vertex(VId(100)))
                },
            );
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            if refuse {
                assert_eq!(Some(probe.calls()), last);
                assert!(matches!(
                    result,
                    Err(Error::Control(GqlQueryError::Interrupted(_)))
                ));
                assert_eq!(txn.staged_effect_digest().unwrap(), prefix);
                assert_eq!(txn.vertices(&db).unwrap().len(), 1);
                txn.finish(&mut db, &commit).await.unwrap();
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
                assert!(db.vertex(VId(90)).unwrap().is_some());
                assert!(db.vertex(VId(100)).unwrap().is_none());
            } else {
                assert_eq!(result.unwrap().stats.created_vertices, 1);
                last = Some(probe.calls());
                assert!(probe.calls() > 1);
                txn.abort();
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_autocommit_aborts_the_entire_batch_and_a_later_execution_can_commit() {
    let ((), report) = run_async_under_lab(0x94ca_0016, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let failing = definition(
            "UNWIND [1,2] AS key RETURN key ORDER BY key",
            "CREATE (m:City {p:$key,q:$key}); \
             MERGE (m:City {p:$key}) ON MATCH SET m.q=10/(2-m.q)",
        );
        let next = AtomicU64::new(100);
        let result = db
            .execute_graph_query_write_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &failing,
                policy(),
                4,
                |_| -> Result<ElementId, ()> {
                    Ok(ElementId::Vertex(VId(u128::from(
                        next.fetch_add(1, Ordering::Relaxed),
                    ))))
                },
            )
            .await;
        assert!(matches!(result, Err(Error::Program(
            GraphWriteScriptExecutionError::BatchProgram { location: Some(location), .. }
        )) if location.argument_set == 1 && location.statement == 1));
        assert_eq!(next.load(Ordering::Relaxed), 102);
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
        let good = definition(
            "UNWIND [1,2] AS key RETURN key ORDER BY key",
            "MERGE (m:City {p:$key}) ON CREATE SET m.q=5",
        );
        let (result, completion) = db
            .execute_graph_query_write_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &good,
                policy(),
                2,
                |_| -> Result<ElementId, ()> {
                    Ok(ElementId::Vertex(VId(u128::from(
                        next.fetch_add(1, Ordering::Relaxed),
                    ))))
                },
            )
            .await
            .unwrap();
        assert_eq!(result.stats.created_vertices, 2);
        assert_eq!(next.load(Ordering::Relaxed), 104);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert!(db.vertex(VId(100)).unwrap().is_none());
        assert!(db.vertex(VId(101)).unwrap().is_none());
        for (id, p) in [(102, 1), (103, 2)] {
            assert_eq!(
                db.vertex(VId(id)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (Q, CanonicalScalar::Int(5).into())
                ]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
