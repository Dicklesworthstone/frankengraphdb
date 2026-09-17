//! Parameter batches cross one real transaction boundary, even beyond 64 steps.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergeOutcome, GraphMutationProgramDimension,
    GraphMutationProgramError, GraphSymbol, GraphSymbolKind, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteScriptBatchError, GraphWriteScriptExecutionError,
    PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1_000_000, 20_000_000, 20_000_000)
}
fn policy(vertices: u64, effects: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(), effects, vertices, 0)
}
fn values(key: i64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("key", key)
        .unwrap()
        .with_int64("value", key + 10)
        .unwrap()
}
fn script() -> PreparedGraphWriteScript {
    PreparedGraphWriteScript::prepare(
        "CREATE (n {p:$key});\n\u{2003}MATCH (n) WHERE n.p=$key SET n.q=$value",
        R,
        symbols,
    )
    .unwrap()
}

#[test]
fn graph_ingestion_records_merge_shared_vertices_and_edges_in_one_durable_commit() {
    let ((), report) = run_async_under_lab(0xba7c_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txcx = contexts.txn();
        let query = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let start = db.frontier().unwrap();
        let script = PreparedGraphWriteScript::prepare(
            "MERGE (n {p:$source}); MERGE (n {p:$target}); MATCH (a),(b) WHERE a.p=$source AND b.p=$target MERGE (a)-[:R]->(b)",
            R, symbols,
        ).unwrap();
        let args = (1..=40)
            .map(|target| {
                GqlParameters::new()
                    .with_int64("source", 0)
                    .unwrap()
                    .with_int64("target", target)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let batch = script.bind_parameter_sets_with_limit(&args, 120).unwrap();
        let mut allocations = 0;
        let (receipt, outcome) = db
            .execute_bound_graph_write_script_batch_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &batch,
                GraphWriteProgramPolicy::new(query_policy(), 0, 41, 40),
                |request| {
                    let location = batch.location(request.statement).unwrap();
                    let record = location.argument_set as u128;
                    allocations += 1;
                    let id = match (location.statement, request.request) {
                        (
                            0,
                            fgdb_gql::insertion::GraphInsertRequest::Vertex { row: 0, vertex: 0 },
                        ) => {
                            assert_eq!(record, 0, "later records must reuse the source vertex");
                            ElementId::Vertex(VId(1000))
                        }
                        (
                            1,
                            fgdb_gql::insertion::GraphInsertRequest::Vertex { row: 0, vertex: 0 },
                        ) => ElementId::Vertex(VId(2000 + record)),
                        (2, fgdb_gql::insertion::GraphInsertRequest::Edge { row: 0, edge: 0 }) => {
                            ElementId::Edge(EId(3000 + record))
                        }
                        _ => panic!("unexpected allocation request"),
                    };
                    Ok::<_, ()>(id)
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, start.0 + 1);
        assert_eq!(allocations, 81);
        assert_eq!(receipt.stats().completed_statements, 120);
        assert_eq!(receipt.stats().created_vertices, 41);
        assert_eq!(receipt.stats().created_edges, 40);
        for record in 0..40 {
            let steps = batch.record_receipts(&receipt, record).unwrap();
            assert_eq!(steps.len(), 3);
            assert_eq!(
                steps[2].merged_edge(),
                Some(GraphEdgeMergeOutcome::Created(EId(3000 + record as u128)))
            );
        }
        let before_repeat = db.frontier().unwrap();
        let (repeat, completion) = db
            .execute_bound_graph_write_script_batch_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &batch,
                GraphWriteProgramPolicy::new(query_policy(), 0, 0, 0),
                |_| Err::<ElementId, _>("matching must not allocate"),
            )
            .await
            .unwrap();
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before_repeat);
        assert_eq!(repeat.stats().created_vertices, 0);
        assert_eq!(repeat.stats().created_edges, 0);
        assert_eq!(
            batch.record_receipts(&repeat, 39).unwrap()[2].merged_edge(),
            Some(GraphEdgeMergeOutcome::Matched(EId(3039)))
        );
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(db.vertex(VId(1000)).unwrap().is_some());
        for record in 0..40_u128 {
            let edge = db.edge(EId(3000 + record)).unwrap().unwrap();
            assert_eq!(
                (edge.entry.src, edge.entry.dst),
                (VId(1000), VId(2000 + record))
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_invalid_record_refuses_before_private_transaction_and_preserves_outer_work() {
    let ((), report) = run_async_under_lab(0xba7c_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txcx = contexts.txn();
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.frontier().unwrap();
        let script = script();
        let mut args = (0..80).map(values).collect::<Vec<_>>();
        args[79] = GqlParameters::new().with_int64("key", 79).unwrap();
        let mut allocations = 0;
        let error = db
            .execute_graph_write_script_batch_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &script,
                &args,
                160,
                policy(80, 80),
                |_| {
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(1000)))
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, GraphWriteScriptExecutionError::BatchBinding(
            GraphWriteScriptBatchError::Arguments { argument_set: 79, source }
        ) if source.statement == Some(1) && source.offset == script.script().find("$value").unwrap())
        );
        assert_eq!(allocations, 0);
        assert_eq!(db.frontier().unwrap(), start);
        assert_eq!(txcx.outstanding_obligations(), 0);
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(999), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        assert!(matches!(
            txn.execute_graph_write_script_batch_governed(
                &mut db,
                &query,
                &script,
                &args,
                160,
                policy(80, 80),
                |_| Err::<ElementId, _>("must not allocate"),
            ),
            Err(GraphWriteScriptExecutionError::BatchBinding(_))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(999)).unwrap().is_some());
        assert!(db.vertex(VId(1000)).unwrap().is_none());
        assert_eq!(db.frontier().unwrap().0, start.0 + 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_creation_and_mutation_quotas_rollback_every_record_but_not_the_outer_prefix() {
    let ((), report) = run_async_under_lab(0xba7c_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txcx = contexts.txn();
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.frontier().unwrap();
        let script = script();
        let args = (0..80).map(values).collect::<Vec<_>>();
        let batch = script.bind_parameter_sets_with_limit(&args, 160).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(999), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        for (case, cap, expected_allocations, step) in [
            (0_u128, policy(79, 80), 79, 0),
            (1_u128, policy(80, 79), 80, 1),
        ] {
            let mut allocations = 0;
            let base = 1000 + case * 1000;
            let error = txn
                .execute_bound_graph_write_script_batch_governed(
                    &mut db,
                    &query,
                    &batch,
                    cap,
                    |request| {
                        assert_eq!(request.statement % 2, 0);
                        allocations += 1;
                        Ok::<_, ()>(ElementId::Vertex(VId(
                            base + (request.statement / 2) as u128
                        )))
                    },
                )
                .unwrap_err();
            let GraphWriteScriptExecutionError::BatchProgram { location, source } = error else {
                panic!("expected execution error")
            };
            let location = location.unwrap();
            assert_eq!((location.argument_set, location.statement), (79, step));
            assert_eq!(location.span, script.statement_span(step).unwrap());
            if step == 0 {
                assert!(matches!(
                    source,
                    GraphWriteProgramError::CreationBudget {
                        statement: 158,
                        dimension: fgdb_gql::insertion::GraphInsertLimitDimension::Vertices,
                        limit: 79,
                        observed: 80,
                    }
                ));
            } else {
                assert!(matches!(
                    source,
                    GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
                        statement: 159,
                        dimension: GraphMutationProgramDimension::Effects,
                        limit: 79,
                        observed: 80,
                    })
                ));
            }
            assert_eq!(allocations, expected_allocations);
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert!(txn.vertex(&db, VId(999)).unwrap().is_some());
            for record in 0..80 {
                assert!(txn.vertex(&db, VId(base + record)).unwrap().is_none());
            }
            assert_eq!(db.frontier().unwrap(), start);
        }
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(999)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap().0, start.0 + 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_allocator_failure_in_autocommit_never_publishes_a_prefix_or_retries() {
    let ((), report) = run_async_under_lab(0xba7c_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txcx = contexts.txn();
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.frontier().unwrap();
        let args = (0..80).map(values).collect::<Vec<_>>();
        let mut attempts = 0;
        let error = db
            .execute_graph_write_script_batch_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &script(),
                &args,
                160,
                policy(80, 80),
                |request| {
                    attempts += 1;
                    if request.statement == 158 {
                        return Err("allocator unavailable");
                    }
                    Ok(ElementId::Vertex(VId(
                        1000 + (request.statement / 2) as u128
                    )))
                },
            )
            .await
            .unwrap_err();
        let GraphWriteScriptExecutionError::BatchProgram { location, source } = error else {
            panic!("expected allocation execution failure")
        };
        assert_eq!(location.unwrap().argument_set, 79);
        assert!(matches!(
            &source,
            GraphWriteProgramError::Insert { statement: 158, .. }
        ));
        assert!(source.to_string().contains("allocator unavailable"));
        assert_eq!(attempts, 80);
        assert_eq!(db.frontier().unwrap(), start);
        for id in 1000..1080 {
            assert!(db.vertex(VId(id)).unwrap().is_none());
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn direct_batch_execution_keeps_one_final_commit_and_durable_properties() {
    let ((), report) = run_async_under_lab(0xba7c_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txcx = contexts.txn();
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.frontier().unwrap();
        let args = (0..80).map(values).collect::<Vec<_>>();
        let (receipt, outcome) = db
            .execute_graph_write_script_batch_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &script(),
                &args,
                160,
                policy(80, 80),
                |request| {
                    assert_eq!(request.statement % 2, 0);
                    Ok::<_, ()>(ElementId::Vertex(VId(
                        1000 + (request.statement / 2) as u128
                    )))
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(receipt.steps().len(), 160);
        assert_eq!(receipt.stats().created_vertices, 80);
        assert_eq!(receipt.stats().mutation_effects, 80);
        assert_eq!(db.frontier().unwrap().0, start.0 + 1);
        for index in 0..80_u128 {
            assert_eq!(
                db.vertex(VId(1000 + index)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(index as i64)),
                    (Q, CanonicalScalar::Int(index as i64 + 10))
                ]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn infrastructure_refusal_is_not_mislabeled_as_a_record_failure() {
    let ((), report) = run_async_under_lab(0xba7c_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txcx = contexts.txn();
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let args = (0..40).map(values).collect::<Vec<_>>();
        let batch = script().bind_parameter_sets_with_limit(&args, 80).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(RelationId(2));
        prefix.create_vertex(VId(999), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let error = txn
            .execute_bound_graph_write_script_batch_governed(
                &mut db,
                &query,
                &batch,
                policy(40, 40),
                |_| Err::<ElementId, _>("must not allocate"),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteScriptExecutionError::BatchProgram {
                location: None,
                source: GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(
                    WriteTxnError::RelationMismatch { .. }
                )),
            }
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn real_large_batch_shares_exact_source_row_work_and_scratch_allowances() {
    let ((), report) = run_async_under_lab(0xba7c_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txcx = contexts.txn();
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.frontier().unwrap();
        let args = (0..33).map(values).collect::<Vec<_>>();
        let batch = script().bind_parameter_sets_with_limit(&args, 66).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let measured = txn
            .execute_bound_graph_write_script_batch_governed(
                &mut db,
                &query,
                &batch,
                policy(33, 33),
                |request| {
                    Ok::<_, ()>(ElementId::Vertex(VId(
                        10_000 + (request.statement / 2) as u128
                    )))
                },
            )
            .unwrap()
            .stats();
        txn.abort();
        assert_eq!(measured.completed_statements, 66);
        let caps = [
            measured.selection.snapshot_records,
            measured.selection.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert!(caps.iter().all(|cap| *cap > 0));
        for trial in 0..5 {
            let mut limits = caps;
            if trial > 0 {
                limits[trial - 1] -= 1;
            }
            let base = 20_000 + trial as u128 * 1000;
            let mut txn = db.begin(&txcx).unwrap();
            let before = txn.staged_effect_digest().unwrap();
            let result = txn.execute_bound_graph_write_script_batch_governed(
                &mut db,
                &query,
                &batch,
                GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                    33,
                    33,
                    0,
                ),
                |request| {
                    Ok::<_, ()>(ElementId::Vertex(VId(
                        base + (request.statement / 2) as u128
                    )))
                },
            );
            if trial == 0 {
                assert_eq!(result.unwrap().stats(), measured);
            } else {
                assert!(
                    result.is_err(),
                    "dimension {} was refreshed per record",
                    trial - 1
                );
                assert_eq!(txn.staged_effect_digest().unwrap(), before);
                assert!(txn.vertex(&db, VId(base)).unwrap().is_none());
            }
            txn.abort();
            assert_eq!(db.frontier().unwrap(), start);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
