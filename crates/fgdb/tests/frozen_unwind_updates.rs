//! Clause-at-a-time UNWIND updates through the real database, ordinary ordered
//! preparation, savepoints, conflict validation and durable reopen.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PinnedTzdb, QueryResult, QueryWriteError, TzdbZone, WriteBatch,
    WriteError, WriteTxn, WriteTxnError,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::unwind_write::{GraphUnwindWriteError, GraphUnwindWriteText};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationError,
    GraphMutationProgramDimension, GraphMutationProgramError, GraphSymbol, GraphSymbolKind,
    GraphSymbolResolver, GraphWriteIdentityRequest, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteProgramReceipt, GraphWriteScriptExecutionError,
    ReverseSymbolCatalog,
};
use fgdb_types::{
    CanonicalScalar, CanonicalTimestamp, CommitSeq, DatabaseSecurityNamespaceId, EId,
    EmbeddedTxnCompletion, PurposeContexts, VId,
};
use std::convert::Infallible;

const R: RelationId = RelationId(1);
const ENTITY: LabelId = LabelId(1);
const ID: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const Q: PropertyKeyId = PropertyKeyId(3);
const REKEY: &str = "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n.id=row.b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(ENTITY)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

struct MapCatalog;
impl GraphSymbolResolver for MapCatalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_property(ID, "id");
        catalog.insert_property(P, "p");
        catalog.insert_property(Q, "q");
        Some(catalog)
    }
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
        100_000,
        0,
        0,
    )
}
fn row(a: i64, b: CanonicalScalar) -> GraphValue {
    GraphValue::map(vec![
        ("a".into(), GraphValue::Scalar(CanonicalScalar::Int(a))),
        ("b".into(), GraphValue::Scalar(b)),
    ])
    .unwrap()
}
fn args(rows: &[(i64, i64)]) -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "rows",
            rows.iter()
                .map(|&(a, b)| row(a, CanonicalScalar::Int(b)))
                .collect(),
        )
        .unwrap()
}
fn seed(count: u128) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for id in 1..=count {
        batch.create_vertex(
            VId(id),
            vec![ENTITY],
            vec![
                (ID, CanonicalScalar::Int(id as i64)),
                (P, CanonicalScalar::Int(10)),
            ],
        );
    }
    batch
}
fn prefix(db: &mut Database<MemVfs>, txn: &mut WriteTxn) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(99), vec![], vec![]);
    txn.write(db, batch).unwrap();
}
fn no_allocation(_: GraphWriteIdentityRequest) -> Result<ElementId, Infallible> {
    panic!("vertex field updates cannot allocate identities")
}
fn receipt(result: QueryResult) -> GraphWriteProgramReceipt {
    let QueryResult::Write {
        receipt,
        completion,
    } = result
    else {
        panic!("expected the native write receipt")
    };
    assert!(completion.is_none());
    receipt
}
fn assert_key(db: &Database<MemVfs>, id: u128, expected: i64) {
    assert_eq!(
        db.vertex(VId(id)).unwrap().unwrap().props,
        [
            (ID, CanonicalScalar::Int(expected).into()),
            (P, CanonicalScalar::Int(10).into())
        ]
    );
}

#[test]
fn frozen_rekeys_swaps_and_repeated_targets_keep_identity_and_reopen() {
    let ((), report) = run_async_under_lab(0x6672_6f01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        for (count, text, input, expected, targets) in [
            (
                1,
                REKEY,
                vec![(1, 2), (2, 3)],
                vec![2],
                vec![vec![VId(1)], vec![]],
            ),
            (
                2,
                REKEY,
                vec![(1, 2), (2, 1)],
                vec![2, 1],
                vec![vec![VId(1)], vec![VId(2)]],
            ),
            (
                1,
                REKEY,
                vec![(1, 2), (1, 3)],
                vec![3],
                vec![vec![VId(1)], vec![VId(1)]],
            ),
        ] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            db.write(&commit, seed(count)).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let actual = receipt(
                txn.query_write(
                    &mut db,
                    &query,
                    text,
                    &args(&input),
                    symbols,
                    R,
                    policy(),
                    no_allocation,
                )
                .unwrap(),
            );
            assert_eq!(actual.stats().completed_statements, 2);
            let visits = targets.iter().map(Vec::len).sum::<usize>() as u64;
            assert_eq!(actual.stats().target_vertex_visits, visits);
            assert_eq!(actual.stats().mutation_effects, visits);
            for (step, expected) in actual.steps().iter().zip(&targets) {
                assert_eq!(step.mutation_targets(), Some(expected.as_slice()));
                assert_eq!(step.mutation_edges(), Some([].as_slice()));
            }
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
            assert_eq!(
                txn.finish(&mut db, &commit).await.unwrap(),
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: CommitSeq(2)
                }
            );
            drop(db);
            let db = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(db.vertices().unwrap().len(), expected.len());
            for (index, key) in expected.into_iter().enumerate() {
                assert_key(&db, index as u128 + 1, key);
            }
            assert_eq!(db.frontier().unwrap(), CommitSeq(2));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn frozen_input_includes_caller_staging_and_direct_program_execution() {
    let ((), report) = run_async_under_lab(0x6672_6f02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed(1)).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(
            VId(2),
            vec![ENTITY],
            vec![(ID, CanonicalScalar::Int(2)), (P, CanonicalScalar::Int(10))],
        );
        txn.write(&mut db, staged).unwrap();
        let program = GraphUnwindWriteText::parse(REKEY)
            .unwrap()
            .bind(&args(&[(1, 2), (2, 3)]), R, symbols)
            .unwrap()
            .into_program();
        assert!(program.freezes_vertex_update_input());
        let stats = txn
            .execute_graph_write_program_governed(
                &mut db,
                &query,
                &program,
                policy(),
                no_allocation,
            )
            .unwrap();
        assert_eq!(stats.target_vertex_visits, 2);
        assert_eq!(db.vertices().unwrap().len(), 1);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_key(&db, 1, 2);
        assert_key(&db, 2, 3);
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mixed_rekeys_keep_entry_membership_receipts_and_durable_staged_edges() {
    let ((), report) = run_async_under_lab(0x6672_6f08, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let text = "UNWIND $rows AS row MATCH (a:Entity {id:row.a})-[e:R {id:row.a}]->(b) \
                    SET a.id=row.b,e.id=row.b,e.p=row.b";
        for returning in [true, false] {
            for (rows, expected_keys, expected_edges, expected_targets) in [
                ([(1, 2), (2, 3)], [2, 3], [(2, 2), (3, 3)], [1_u128, 2]),
                ([(1, 2), (2, 1)], [2, 1], [(2, 2), (1, 1)], [1, 2]),
                ([(1, 2), (1, 3)], [3, 2], [(3, 3), (2, 10)], [1, 1]),
            ] {
                let vfs = MemVfs::new().unwrap();
                let path = vfs.database_dir();
                let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                let mut initial = seed(2);
                initial.add_edge(
                    EId(1),
                    VId(1),
                    VId(2),
                    vec![(ID, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(10))],
                );
                db.write(&commit, initial).await.unwrap();
                let mut txn = db.begin(&txcx).unwrap();
                let mut staged = WriteBatch::new(R);
                staged.add_edge(
                    EId(2),
                    VId(2),
                    VId(1),
                    vec![(ID, CanonicalScalar::Int(2)), (P, CanonicalScalar::Int(10))],
                );
                txn.write(&mut db, staged).unwrap();
                let stats = if returning {
                    let receipt = receipt(
                        txn.query_write(
                            &mut db,
                            &query,
                            text,
                            &args(&rows),
                            symbols,
                            R,
                            policy(),
                            no_allocation,
                        )
                        .unwrap(),
                    );
                    for (step, target) in receipt.steps().iter().zip(expected_targets) {
                        assert_eq!(step.mutation_targets(), Some([VId(target)].as_slice()));
                        assert_eq!(step.mutation_edges(), Some([EId(target)].as_slice()));
                    }
                    receipt.stats()
                } else {
                    let batch = GraphUnwindWriteText::parse(text)
                        .unwrap()
                        .bind(&args(&rows), R, symbols)
                        .unwrap();
                    assert!(batch.program().freezes_mixed_update_input());
                    txn.execute_graph_write_program_governed(
                        &mut db,
                        &query,
                        batch.program(),
                        policy(),
                        no_allocation,
                    )
                    .unwrap()
                };
                assert_eq!(stats.completed_statements, 2);
                assert_eq!(stats.target_vertex_visits, 2);
                assert_eq!(stats.target_edge_visits, 2);
                assert_eq!(stats.mutation_effects, 6);
                assert_key(&db, 1, 1);
                assert_key(&db, 2, 2);
                assert!(db.edge(EId(2)).unwrap().is_none());
                txn.finish(&mut db, &commit).await.unwrap();
                drop(db);
                let db = Database::open_with_vfs(&commit, vfs, &path, keys())
                    .await
                    .unwrap();
                assert_eq!(db.frontier().unwrap(), CommitSeq(2));
                assert_key(&db, 1, expected_keys[0]);
                assert_key(&db, 2, expected_keys[1]);
                for (index, (id, p)) in expected_edges.into_iter().enumerate() {
                    let edge = db.edge(EId(index as u128 + 1)).unwrap().unwrap();
                    assert_eq!(
                        edge.props,
                        vec![
                            (ID, CanonicalScalar::Int(id).into()),
                            (P, CanonicalScalar::Int(p).into()),
                        ]
                    );
                    assert_eq!(
                        (edge.entry.src, edge.entry.dst),
                        if index == 0 {
                            (VId(1), VId(2))
                        } else {
                            (VId(2), VId(1))
                        }
                    );
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mixed_update_late_arithmetic_and_final_budget_refusals_restore_the_callers_prefix() {
    let ((), report) = run_async_under_lab(0x6672_6f09, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut initial = seed(2);
        initial.add_edge(EId(1), VId(1), VId(2), vec![(P, CanonicalScalar::Int(10))]);
        db.write(&commit, initial).await.unwrap();
        let text = "/* café */ UNWIND $rows AS row MATCH (a:Entity {id:row.a})-[e:R]->(b) \
                    SET a.id=row.b,e.p=100/row.b";
        let good = GraphUnwindWriteText::parse(text)
            .unwrap()
            .bind(&args(&[(1, 10), (1, 20)]), R, symbols)
            .unwrap();
        let mut calibration = db.begin(&txcx).unwrap();
        prefix(&mut db, &mut calibration);
        let successful = calibration
            .execute_graph_write_program_governed(
                &mut db,
                &query,
                good.program(),
                policy(),
                no_allocation,
            )
            .unwrap();
        calibration.abort();
        let mut txn = db.begin(&txcx).unwrap();
        prefix(&mut db, &mut txn);
        txn.savepoint(&db, "entry").unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let failure = txn
            .query_write(
                &mut db,
                &query,
                text,
                &args(&[(1, 10), (1, 0)]),
                symbols,
                R,
                policy(),
                no_allocation,
            )
            .unwrap_err();
        assert!(matches!(failure,
            QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
                location: Some(location),
                source: GraphWriteProgramError::Program(GraphMutationProgramError::Statement {
                    statement: 1,
                    source: GqlQueryError::Source(GraphMutationError::Arithmetic { .. }),
                }),
            }) if location.argument_set == 1 && location.statement == 0 && location.span.end == text.len()
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        let mut limited = policy();
        limited.mutations.query.evaluator.max_work_units = successful.evaluator.work_units - 1;
        let failure = txn
            .execute_graph_write_program_governed(
                &mut db,
                &query,
                good.program(),
                limited,
                no_allocation,
            )
            .unwrap_err();
        assert!(matches!(
            failure,
            GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
                statement: 2,
                dimension: GraphMutationProgramDimension::WorkUnits,
                ..
            })
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        txn.rollback_to_savepoint(&db, "entry").unwrap();
        txn.finish(&mut db, &commit).await.unwrap();
        assert_key(&db, 1, 1);
        assert_key(&db, 2, 2);
        assert_eq!(
            db.edge(EId(1)).unwrap().unwrap().props,
            [(P, CanonicalScalar::Int(10).into())]
        );
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn map_replacement_refuses_frozen_clears_but_single_row_removes_properties_and_reopens() {
    let ((), report) = run_async_under_lab(0x6672_6f07, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(
            VId(1),
            vec![ENTITY],
            vec![
                (ID, CanonicalScalar::Int(1)),
                (P, CanonicalScalar::Int(10)),
                (Q, CanonicalScalar::Int(20)),
            ],
        );
        db.write(&commit, initial).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let error = txn
            .query_write(
                &mut db,
                &query,
                "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n={id:row.b}",
                &args(&[(1, 2), (1, 3)]),
                MapCatalog,
                R,
                policy(),
                no_allocation,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            QueryWriteError::UnwindBinding(GraphUnwindWriteError::RowFeedsLaterMatch {
                statement: 0
            })
        ));
        assert_eq!(txn.vertex(&db, VId(1)).unwrap(), db.vertex(VId(1)).unwrap());
        let actual = receipt(
            txn.query_write(
                &mut db,
                &query,
                "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n={id:row.b}",
                &args(&[(1, 3)]),
                MapCatalog,
                R,
                policy(),
                no_allocation,
            )
            .unwrap(),
        );
        assert_eq!(actual.stats().completed_statements, 1);
        assert_eq!(actual.stats().target_vertex_visits, 1);
        // The native replacement still removes both original fields and sets id.
        assert_eq!(actual.stats().mutation_effects, 3);
        for step in actual.steps() {
            assert_eq!(step.mutation_targets(), Some([VId(1)].as_slice()));
            assert_eq!(step.mutation_edges(), Some([].as_slice()));
        }
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props.len(), 3);
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(2)
            }
        );
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let vertex = db.vertex(VId(1)).unwrap().unwrap();
        assert_eq!(vertex.labels, [ENTITY]);
        assert_eq!(vertex.props, [(ID, CanonicalScalar::Int(3).into())]);
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn four_thousand_keyed_rekeys_fit_default_work_and_publish_one_commit() {
    let ((), report) = run_async_under_lab(0x6672_6f03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(&commit, seed(4_000)).await.unwrap();
        let inputs = args(&(1..=4_000).map(|id| (id, id + 4_000)).collect::<Vec<_>>());
        let batch = GraphUnwindWriteText::parse(REKEY)
            .unwrap()
            .bind_with_limit(&inputs, R, 4_000, symbols)
            .unwrap();
        assert!(batch.program().freezes_vertex_update_input());
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn
            .execute_graph_write_program_governed(
                &mut db,
                &query,
                batch.program(),
                policy(),
                no_allocation,
            )
            .unwrap();
        assert_eq!(stats.completed_statements, 4_000);
        assert_eq!(stats.mutation_effects, 4_000);
        assert_eq!(stats.target_vertex_visits, 4_000);
        assert!(stats.selection.snapshot_records <= 8_000, "{stats:?}");
        assert!(stats.evaluator.work_units < 10_000_000, "{stats:?}");
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        txn.finish(&mut db, &commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(db.vertices().unwrap().len(), 4_000);
        for id in 1..=4_000 {
            assert_key(&db, id, id as i64 + 4_000);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_rhs_refusal_keeps_prefix_savepoint_and_original_input_location() {
    let ((), report) = run_async_under_lab(0x6672_6f04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed(2)).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        prefix(&mut db, &mut txn);
        txn.savepoint(&db, "prefix").unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        // Neo4j evaluates SET against current state for each buffered binding.
        // The old entry-RHS profile lost the second repeated-target increment.
        for tail in [
            "SET n.id=n.id+row.b",
            "SET n.id=row.a, n.p=n.p+row.b",
            "SET n.id=n.id+100/row.b",
        ] {
            let error = txn
                .query_write(
                    &mut db,
                    &query,
                    &format!("UNWIND $rows AS row MATCH (n:Entity {{id:row.a}}) {tail}"),
                    &args(&[(1, 2), (1, 3)]),
                    symbols,
                    R,
                    policy(),
                    no_allocation,
                )
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    QueryWriteError::UnwindBinding(GraphUnwindWriteError::RowFeedsLaterMatch {
                        statement: 0
                    })
                ),
                "{tail}: {error:?}"
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        }
        let text = "/* café */ UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n.id=100/row.b";
        let error = txn
            .query_write(
                &mut db,
                &query,
                text,
                &args(&[(1, 10), (1, 0)]),
                symbols,
                R,
                policy(),
                no_allocation,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
                location: Some(location),
                source: GraphWriteProgramError::Program(GraphMutationProgramError::Statement {
                    statement: 1,
                    source: GqlQueryError::Source(GraphMutationError::Arithmetic { .. }),
                }),
            }) if location.argument_set == 1 && location.statement == 0
                && location.span.end == text.len() && text.is_char_boundary(location.span.start)
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        txn.rollback_to_savepoint(&db, "prefix").unwrap();
        assert_key(&db, 1, 1);
        assert_key(&db, 2, 2);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_key(&db, 1, 1);
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn final_budget_refusal_after_staging_restores_prefix_and_retains_read_witnesses() {
    let ((), report) = run_async_under_lab(0x6672_6f05, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        for change_observed in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, seed(2)).await.unwrap();
            let batch = GraphUnwindWriteText::parse(REKEY)
                .unwrap()
                .bind(&args(&[(1, 2), (1, 3)]), R, symbols)
                .unwrap();
            // Separate calibration: successful reads must not repair witnesses
            // lost by the actual refused program or its savepoint rollback.
            let mut calibration = db.begin(&txcx).unwrap();
            prefix(&mut db, &mut calibration);
            let successful = calibration
                .execute_graph_write_program_governed(
                    &mut db,
                    &query,
                    batch.program(),
                    policy(),
                    no_allocation,
                )
                .unwrap();
            calibration.abort();
            let work_limit = successful.evaluator.work_units - 1;
            let mut bounded = policy();
            bounded.mutations.query.evaluator.max_work_units = work_limit;
            let mut txn = db.begin(&txcx).unwrap();
            prefix(&mut db, &mut txn);
            txn.savepoint(&db, "prefix").unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            let error = txn
                .execute_graph_write_program_governed(
                    &mut db,
                    &query,
                    batch.program(),
                    bounded,
                    no_allocation,
                )
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
                        statement,
                        dimension: GraphMutationProgramDimension::WorkUnits,
                        limit,
                        observed,
                    }) if statement == batch.program().statements().len()
                        && limit == work_limit && observed == u128::from(successful.evaluator.work_units)
                ),
                "{error:?}"
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            txn.rollback_to_savepoint(&db, "prefix").unwrap();
            // Read through Database only: reading txn again would mask a lost
            // refusal witness and turn the conflict check into a tautology.
            assert_key(&db, 1, 1);
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_property(
                VId(if change_observed { 1 } else { 2 }),
                ID,
                Some(CanonicalScalar::Int(11)),
            );
            db.write(&commit, winner).await.unwrap();
            let completion = txn.finish(&mut db, &commit).await;
            if change_observed {
                assert!(matches!(
                    completion,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert!(db.vertex(VId(99)).unwrap().is_none());
                assert_eq!(db.frontier().unwrap(), CommitSeq(2));
            } else {
                assert_eq!(
                    completion.unwrap(),
                    EmbeddedTxnCompletion::WriteCommitted {
                        commit_seq: CommitSeq(3)
                    }
                );
                assert!(db.vertex(VId(99)).unwrap().is_some());
                assert_key(&db, 1, 1);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn overwritten_unavailable_value_refuses_at_its_original_row_without_staging() {
    let ((), report) = run_async_under_lab(0x6672_6f06, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed(1)).await.unwrap();
        let artifact = PinnedTzdb::new(vec![TzdbZone {
            identifier: "Etc/UTC".into(),
            initial_offset_seconds: 0,
            transitions: vec![],
        }])
        .unwrap();
        let oid = artifact.object_id();
        let unsupported = CanonicalScalar::Timestamp(
            CanonicalTimestamp::zoned(0, 0, "Etc/UTC", oid, &artifact).unwrap(),
        );
        let supported = CanonicalScalar::Timestamp(CanonicalTimestamp::offset_only(0, 0).unwrap());
        let inputs = GqlParameters::new()
            .with_list("rows", vec![row(1, unsupported), row(1, supported)])
            .unwrap();
        let text = "/* café */ UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n.id=row.b";
        let mut txn = db.begin(&txcx).unwrap();
        prefix(&mut db, &mut txn);
        txn.savepoint(&db, "prefix").unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let error = txn
            .query_write(
                &mut db,
                &query,
                text,
                &inputs,
                symbols,
                R,
                policy(),
                no_allocation,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            QueryWriteError::Execute(GraphWriteScriptExecutionError::BatchProgram {
                location: Some(location),
                source: GraphWriteProgramError::Program(GraphMutationProgramError::Statement {
                    statement: 0,
                    source: GqlQueryError::Source(GraphMutationError::Source(WriteTxnError::Write(
                        WriteError::ZonedTimestampRequiresResolver { tzdb_oid }
                    ))),
                }),
            }) if location.argument_set == 0 && location.statement == 0
                && location.span.end == text.len() && text.is_char_boundary(location.span.start)
                && tzdb_oid == oid
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_key(&db, 1, 1);
        txn.rollback_to_savepoint(&db, "prefix").unwrap();
        txn.finish(&mut db, &commit).await.unwrap();
        assert_key(&db, 1, 1);
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
