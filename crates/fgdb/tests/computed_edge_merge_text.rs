//! Native relationship expressions reach the real transaction and authorized paths.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, MemVfs, QueryResult, QueryWriteError, WriteBatch, WriteTxnError,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergePolicy, GraphEdgeUpsertError,
    GraphEdgeUpsertPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteScriptExecutionError, PreparedGraphEdgeUpsertText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error as AuthorizationError, Grant, QueryLimits, Rights, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const W: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x62; 32]);
const HEAD: &str = "MATCH (a:Node {p:1}),(b:Node {p:2}) MERGE (a)-[e:R]->(b)";
const BATCH: &str = "UNWIND $rows AS row \
    MATCH (a:Node {p:row.src}),(b:Node {p:row.dst}) MERGE (a)-[e:R]->(b) \
    ON CREATE SET e.w=0 SET e.w=e.w+row.delta";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x61; 32], NS, [0x63; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn policy(actions: u64, edges: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
        actions,
        0,
        edges,
    )
}
fn rows(values: &[(i64, i64, i64)]) -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "rows",
            values
                .iter()
                .map(|&(src, dst, delta)| {
                    GraphValue::map(vec![
                        ("src".into(), GraphValue::Scalar(CanonicalScalar::Int(src))),
                        ("dst".into(), GraphValue::Scalar(CanonicalScalar::Int(dst))),
                        (
                            "delta".into(),
                            GraphValue::Scalar(CanonicalScalar::Int(delta)),
                        ),
                    ])
                    .unwrap()
                })
                .collect(),
        )
        .unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
        batch.create_vertex(VId(id), vec![L], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    db.write(cx, batch).await.unwrap();
}

#[test]
fn native_create_and_match_keep_simultaneous_then_sequential_values_after_reopen() {
    let ((), report) = run_async_under_lab(0xed7e_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let before = db.frontier().unwrap();
        let text = format!(
            "{HEAD} ON CREATE SET e.p=10,e.w=20 \
            ON MATCH SET e.p=e.w,e.w=e.p SET e.p=e.p+1"
        );
        let mut chosen = None;
        for (run, expected) in [[11, 20], [21, 11]].into_iter().enumerate() {
            let result = db
                .query_write_engine(
                    &txcx,
                    &query,
                    &commit,
                    &text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(3, 1),
                )
                .await
                .unwrap();
            let QueryResult::Write {
                receipt,
                completion: Some(completion),
            } = result
            else {
                panic!("native MERGE must return its completed write receipt")
            };
            let edge = receipt.steps()[0].merged_edge().unwrap().edge().unwrap();
            if let Some(previous) = chosen {
                assert!(edge == previous);
            }
            chosen = Some(edge);
            assert_eq!(receipt.stats().created_edges, u64::from(run == 0));
            assert_eq!(receipt.stats().mutation_effects, 3);
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            assert_eq!(
                db.edge(edge).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(expected[0])),
                    (W, CanonicalScalar::Int(expected[1])),
                ]
            );
            assert!(pinned.edge(edge).unwrap().is_none());
        }
        assert_eq!(db.frontier().unwrap().0, before.0 + 2);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.edges().unwrap().len(), 1);
        let edge = db.edge(chosen.unwrap()).unwrap().unwrap();
        assert!(edge.entry.src == VId(1) && edge.entry.dst == VId(2));
        assert_eq!(
            edge.props,
            vec![(P, CanonicalScalar::Int(21)), (W, CanonicalScalar::Int(11))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_unwind_accumulates_repeated_relationships_in_one_durable_commit() {
    let ((), report) = run_async_under_lab(0xed7e_2002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let args = rows(&[(1, 2, 2), (1, 3, 4), (1, 2, 3)]);
        let frozen = args.canonical_bytes();
        let result = db
            .query_write_engine(
                &txcx,
                &query,
                &commit,
                BATCH,
                &args,
                symbols,
                R,
                policy(5, 2),
            )
            .await
            .unwrap();
        let QueryResult::Write {
            receipt,
            completion: Some(completion),
        } = result
        else {
            panic!("one complete atomic batch receipt")
        };
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(receipt.stats().created_edges, 2);
        assert_eq!(receipt.stats().mutation_effects, 5);
        let first = receipt.steps()[0].merged_edge().unwrap().edge().unwrap();
        let second = receipt.steps()[1].merged_edge().unwrap().edge().unwrap();
        assert!(receipt.steps()[2].merged_edge().unwrap().edge() == Some(first));
        assert!(
            matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 1)
        );
        assert_eq!(args.canonical_bytes(), frozen);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.edges().unwrap().len(), 2);
        assert_eq!(
            db.edge(first).unwrap().unwrap().props,
            vec![(W, CanonicalScalar::Int(5))]
        );
        assert_eq!(
            db.edge(second).unwrap().unwrap().props,
            vec![(W, CanonicalScalar::Int(4))]
        );
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_unwind_overflow_retains_row_coordinates_and_the_outer_transaction_prefix() {
    let ((), report) = run_async_under_lab(0xed7e_2003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.add_edge(
            EId(90),
            VId(1),
            VId(3),
            vec![(W, CanonicalScalar::Int(i64::MAX))],
        );
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let allocations = AtomicU64::new(0);
        let result = txn.query_write(
            &mut db,
            &query,
            BATCH,
            &rows(&[(1, 2, 2), (1, 3, 1)]),
            symbols,
            R,
            policy(4, 1),
            |_| {
                allocations.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>(ElementId::Edge(EId(100)))
            },
        );
        let Err(QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location),
            source,
        })) = result
        else {
            panic!("expected a located native relationship expression failure")
        };
        assert_eq!(location.argument_set, 1);
        assert_eq!(location.statement, 0);
        assert_eq!(location.span, 0..BATCH.len());
        assert!(matches!(
            source,
            GraphWriteProgramError::EdgeUpsert {
                source: GqlQueryError::Source(GraphEdgeUpsertError::Expression {
                    clause: 1,
                    action: 0,
                    ..
                }),
                ..
            }
        ));
        assert_eq!(allocations.load(Ordering::Relaxed), 1);
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert!(txn.edge(&db, EId(100)).unwrap().is_none());
        assert_eq!(
            txn.edge(&db, EId(90)).unwrap().unwrap().props,
            vec![(W, CanonicalScalar::Int(i64::MAX))]
        );
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), before);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.edges().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_input_and_unselected_branches_are_lazy_but_overwritten_failures_are_not_erased() {
    let ((), report) = run_async_under_lab(0xed7e_2004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let text = format!("{HEAD} ON CREATE SET e.w=1/0 ON MATCH SET e.w=1/0 SET e.w=1/0");
        let result = db
            .query_write(
                &txcx,
                &query,
                &commit,
                &text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(0, 0),
                |_| -> Result<ElementId, ()> { panic!("NoInput may not allocate") },
            )
            .await
            .unwrap();
        let QueryResult::Write {
            receipt,
            completion: Some(completion),
        } = result
        else {
            panic!()
        };
        assert_eq!(receipt.stats().mutation_effects, 0);
        assert_eq!(receipt.stats().created_edges, 0);
        assert!(receipt.steps()[0].merged_edge().unwrap().edge().is_none());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        seed(&mut db, &commit).await;
        let text = format!(
            "{HEAD} ON MATCH SET e.w=1/0 ON CREATE SET e.w=4 \
            SET e.w=CASE WHEN e.w=4 THEN e.w+1 ELSE 1/0 END"
        );
        db.query_write(
            &txcx,
            &query,
            &commit,
            &text,
            &GqlParameters::new(),
            symbols,
            R,
            policy(2, 1),
            |_| Ok::<_, ()>(ElementId::Edge(EId(90))),
        )
        .await
        .unwrap();
        let before = db.frontier().unwrap();
        let text = format!("{HEAD} ON MATCH SET e.w=1/0 SET e.w=7");
        let error = db
            .query_write_engine(
                &txcx,
                &query,
                &commit,
                &text,
                &GqlParameters::new(),
                symbols,
                R,
                policy(2, 0),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            QueryWriteError::Execute(GraphWriteScriptExecutionError::Program(
                GraphWriteProgramError::EdgeUpsert {
                    source: GqlQueryError::Source(GraphEdgeUpsertError::Expression {
                        clause: 0,
                        action: 0,
                        ..
                    }),
                    ..
                }
            ))
        ));
        assert_eq!(
            db.edge(EId(90)).unwrap().unwrap().props,
            vec![(W, CanonicalScalar::Int(5))]
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parsed_clauses_share_exact_evaluator_and_action_budgets() {
    let ((), report) = run_async_under_lab(0xed7e_2005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let bound = PreparedGraphEdgeUpsertText::prepare(
            &format!("{HEAD} ON CREATE SET e.w=4 SET e.w=e.w+1"),
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let mut measured: Option<fgdb_gql::GlaExecutionStats> = None;
        for run in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let before = db.frontier().unwrap();
            let mut allowance = GraphEdgeUpsertPolicy::new(
                GraphEdgeMergePolicy::new(policy(2, 1).mutations.query),
                if run == 1 { 1 } else { 2 },
            );
            if run >= 2 {
                let usage = measured.unwrap();
                allowance.merge.query.evaluator.max_work_units =
                    usage.work_units - u64::from(run == 3);
                allowance.merge.query.evaluator.max_scratch_entries =
                    usage.scratch_entries - u64::from(run == 4);
            }
            let result = db
                .execute_graph_edge_upsert_autocommit_governed(
                    &txcx,
                    &query,
                    &commit,
                    &bound,
                    allowance,
                    |_| Ok::<_, ()>(ElementId::Edge(EId(90))),
                )
                .await;
            if run == 0 || run == 2 {
                let (stats, _, _) = result.unwrap();
                assert_eq!(stats.action_effects, 2);
                measured = Some(stats.evaluator);
            } else {
                let error = result.unwrap_err();
                if run == 1 {
                    assert!(matches!(
                        error,
                        GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit {
                            limit: 1,
                            observed: 2,
                        })
                    ));
                } else {
                    assert!(matches!(error, GqlQueryError::Evaluator(_)));
                }
                assert!(db.edges().unwrap().is_empty());
                assert_eq!(db.frontier().unwrap(), before);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_native_expressions_mask_hidden_payloads_and_preserve_original_writes() {
    let ((), report) = run_async_under_lab(0xed7e_2006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let authority =
            Authority::new(AuthKey::from_seed(9871), NS, "graph", SchemaEpoch(1), 1).unwrap();
        let token = authority
            .issue_at(
                &Grant {
                    branch: "main".into(),
                    labels: Scope::only([L]),
                    relations: Scope::only([R]),
                    properties: Scope::only([P, W]),
                    rights: Rights::ReadWrite,
                    limits: QueryLimits {
                        max_nodes: 100_000,
                        max_work: 10_000_000,
                        max_rows: 1,
                    },
                    expires_at_ms: 10_000,
                },
                100,
            )
            .unwrap();
        let mut measured = None;
        for secret in [
            CanonicalScalar::Int(99),
            CanonicalScalar::ucs_basic_text(&"secret".repeat(500)).unwrap(),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let mut batch = WriteBatch::new(R);
            batch.add_edge(
                EId(90),
                VId(1),
                VId(2),
                vec![(W, CanonicalScalar::Int(5)), (SECRET, secret.clone())],
            );
            db.write(&commit, batch).await.unwrap();
            let ticks = AtomicU64::new(0);
            let text = format!("{HEAD} ON MATCH SET e.w=e.w+COALESCE(e.secret,0) SET e.w=e.w+1");
            let result = db
                .query_write_authorized(
                    &txcx,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(2, 0),
                    || {
                        ticks.fetch_add(1, Ordering::Relaxed);
                        100
                    },
                )
                .await
                .unwrap();
            let QueryResult::Write {
                receipt,
                completion: Some(_),
            } = result
            else {
                panic!()
            };
            let usage = (receipt.stats(), ticks.load(Ordering::Relaxed));
            if let Some(previous) = measured {
                assert_eq!(usage, previous);
            }
            measured = Some(usage);
            assert_eq!(
                db.edge(EId(90)).unwrap().unwrap().props,
                vec![(W, CanonicalScalar::Int(6)), (SECRET, secret.clone())]
            );
            let before = db.frontier().unwrap();
            let text = format!("{HEAD} ON MATCH SET e.w=e.w+1 SET e.secret=0");
            let error = db
                .query_write_authorized(
                    &txcx,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(2, 0),
                    || 100,
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                GraphWriteScriptExecutionError::Program(GraphWriteProgramError::EdgeUpsert {
                    source: GqlQueryError::Source(GraphEdgeUpsertError::Staging(
                        WriteTxnError::Authorization(AuthorizationError::ScopeDenied)
                    )),
                    ..
                })
            ));
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(
                db.edge(EId(90)).unwrap().unwrap().props,
                vec![(W, CanonicalScalar::Int(6)), (SECRET, secret)]
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parallel_relationship_ambiguity_refuses_before_computed_actions_or_allocation() {
    let ((), report) = run_async_under_lab(0xed7e_2007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut batch = WriteBatch::new(R);
        batch.add_edge(EId(90), VId(1), VId(2), vec![(W, CanonicalScalar::Int(5))]);
        batch.add_edge(EId(91), VId(1), VId(2), vec![(W, CanonicalScalar::Int(7))]);
        db.write(&commit, batch).await.unwrap();
        let before = db.frontier().unwrap();
        let error = db
            .query_write(
                &txcx,
                &query,
                &commit,
                &format!("{HEAD} SET e.w=e.w+1"),
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1),
                |_| -> Result<ElementId, ()> { panic!("ambiguous MERGE cannot allocate") },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            QueryWriteError::Execute(GraphWriteScriptExecutionError::Program(
                GraphWriteProgramError::EdgeUpsert {
                    source: GqlQueryError::Source(GraphEdgeUpsertError::Merge(_)),
                    ..
                }
            ))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(
            db.edge(EId(90)).unwrap().unwrap().props,
            vec![(W, CanonicalScalar::Int(5))]
        );
        assert_eq!(
            db.edge(EId(91)).unwrap().unwrap().props,
            vec![(W, CanonicalScalar::Int(7))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
