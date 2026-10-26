//! No-RETURN programs keep the same relationship occurrence semantics as the
//! returning path. Native storage, matcher, actions, receipts and rollback run
//! together; expected identities and arithmetic are independent assertions.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergeOutcome, GraphEdgeUpsertError,
    GraphMutationProgramError, GraphSymbol, GraphSymbolKind, GraphWriteIdentityRequest,
    GraphWriteProgramError, GraphWriteProgramPolicy, GraphWriteProgramStats, GraphWriteStepReceipt,
    PreparedGraphEdgeUpsertText, PreparedGraphWriteProgram,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, TxnCx,
    VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const FIRST: EId = EId((1_u128 << 96) + 10);
const SECOND: EId = EId((1_u128 << 112) + 20);
const PAIRS: &str = "MATCH (a),(b) WHERE a.p=1 AND b.p>1 MERGE (a)-[r:R]->(b) \
    ON CREATE SET r.p=7 ON MATCH SET r.p=r.p+1";
const REPEATED: &str = "MATCH (a)-[s:S]->(b) MERGE (a)-[r:R]->(b) \
    ON CREATE SET r.p=7 ON MATCH SET r.p=r.p+1";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc4; 32],
        DatabaseSecurityNamespaceId([0xc5; 32]),
        [0xc6; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100, 10_000_000, 1_000_000),
        100,
        0,
        100,
    )
}

fn program(statements: &[&str]) -> PreparedGraphWriteProgram {
    PreparedGraphWriteProgram::prepare(
        statements
            .iter()
            .map(|statement| {
                PreparedGraphEdgeUpsertText::prepare(statement, R, symbols)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap()
                    .into()
            })
            .collect(),
    )
    .unwrap()
}

fn allocate(request: GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    let GraphInsertRequest::Edge { row, edge: 0 } = request.request else {
        panic!("only MERGE relationships may allocate")
    };
    Ok(ElementId::Edge(EId(500
        + request.statement as u128 * 100
        + row as u128)))
}

fn no_allocation(_: GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    panic!("a matched or empty source must not allocate")
}

async fn database(commit: &CommitCx, repeated: bool, existing: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(commit, keys()).await.unwrap();
    let mut seed = WriteBatch::new(S);
    for id in 1..=3 {
        seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    if repeated {
        for id in [1, 2] {
            seed.add_edge(EId(id), VId(1), VId(2), vec![]);
        }
    }
    db.write(commit, seed).await.unwrap();
    if existing {
        let mut seed = WriteBatch::new(R);
        for (edge, p) in [(FIRST, 10), (SECOND, 20)] {
            seed.add_edge(edge, VId(1), VId(2), vec![(P, CanonicalScalar::Int(p))]);
        }
        db.write(commit, seed).await.unwrap();
    }
    db
}

fn assert_edge(db: &Database<MemVfs>, edge: EId, p: i64) {
    assert_eq!(
        db.edge(edge).unwrap().unwrap().props,
        vec![(P, CanonicalScalar::Int(p).into())]
    );
}

#[test]
fn public_script_creates_every_pair_then_observes_prior_steps_with_exact_receipts() {
    let ((), report) = run_async_under_lab(0xe8a0_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = database(&commit, false, false).await;
        let before = db.frontier().unwrap();
        let mut requests = Vec::new();
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                &format!("{PAIRS}; {PAIRS}"),
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| {
                    requests.push(request);
                    allocate(request)
                },
            )
            .await
            .unwrap();
        let QueryResult::Write {
            receipt,
            completion,
        } = result
        else {
            panic!("no-RETURN scripts produce a write receipt")
        };
        assert!(completion.is_some());
        assert_eq!(requests.len(), 2);
        for (row, request) in requests.into_iter().enumerate() {
            assert_eq!(request.statement, 0);
            assert_eq!(request.request, GraphInsertRequest::Edge { row, edge: 0 });
        }
        let stats = receipt.stats();
        assert_eq!(stats.completed_statements, 2);
        assert_eq!(stats.selection.result_rows, 4);
        assert_eq!(stats.created_edges, 2);
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(stats.mutation_effects, 4);
        assert_eq!(stats.target_edge_visits, 4);
        assert_eq!(stats.target_vertex_visits, 0);
        assert_eq!(receipt.steps().len(), 2);
        assert_eq!(
            receipt.steps()[0].merged_edges().unwrap(),
            &[
                GraphEdgeMergeOutcome::Created(EId(500)),
                GraphEdgeMergeOutcome::Created(EId(501)),
            ]
        );
        assert_eq!(
            receipt.steps()[0].created_edges().unwrap(),
            &[EId(500), EId(501)]
        );
        assert_eq!(
            receipt.steps()[1].merged_edges().unwrap(),
            &[
                GraphEdgeMergeOutcome::Matched(EId(500)),
                GraphEdgeMergeOutcome::Matched(EId(501)),
            ]
        );
        assert!(receipt.steps()[1].created_edges().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_edge(&db, EId(500), 8);
        assert_edge(&db, EId(501), 8);
        assert_eq!(db.edges().unwrap().len(), 2);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn duplicate_inputs_and_parallel_matches_keep_occurrences_but_count_distinct_targets() {
    let ((), report) = run_async_under_lab(0xe8a0_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let definition = program(&[REPEATED]);
        for returning in [false, true] {
            let mut db = database(&commit, true, true).await;
            let mut txn = db.begin(&txcx).unwrap();
            let stats = if returning {
                let receipt = txn
                    .execute_graph_write_program_returning_governed(
                        &mut db,
                        &cx,
                        &definition,
                        policy(),
                        no_allocation,
                    )
                    .unwrap();
                assert_eq!(
                    receipt.steps()[0].merged_edges().unwrap(),
                    &[
                        GraphEdgeMergeOutcome::Matched(FIRST),
                        GraphEdgeMergeOutcome::Matched(SECOND),
                        GraphEdgeMergeOutcome::Matched(FIRST),
                        GraphEdgeMergeOutcome::Matched(SECOND),
                    ]
                );
                assert!(receipt.steps()[0].created_edges().unwrap().is_empty());
                receipt.stats()
            } else {
                txn.execute_graph_write_program_governed(
                    &mut db,
                    &cx,
                    &definition,
                    policy(),
                    no_allocation,
                )
                .unwrap()
            };
            assert_eq!(stats.selection.result_rows, 2);
            assert_eq!(stats.mutation_effects, 4);
            assert_eq!(stats.target_edge_visits, 2);
            assert_eq!(stats.created_edges, 0);
            assert_edge(&db, FIRST, 10);
            assert_edge(&db, SECOND, 20);
            txn.finish(&mut db, &commit).await.unwrap();
            assert_edge(&db, FIRST, 12);
            assert_edge(&db, SECOND, 22);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_and_single_receipts_keep_their_shapes_and_actionless_matches_are_not_targets() {
    let ((), report) = run_async_under_lab(0xe8a0_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = database(&commit, false, false).await;
        let mut txn = db.begin(&txcx).unwrap();
        let empty = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &cx,
                &program(&[REPEATED]),
                policy(),
                no_allocation,
            )
            .unwrap();
        assert!(matches!(
            empty.steps(),
            [GraphWriteStepReceipt::EdgeUpsert {
                outcome: GraphEdgeMergeOutcome::NoInput
            }]
        ));
        assert_eq!(empty.stats().mutation_effects, 0);
        assert_eq!(empty.stats().target_edge_visits, 0);
        let one = "MATCH (a),(b) WHERE a.p=1 AND b.p=2 MERGE (a)-[r:R]->(b) ON CREATE SET r.p=7";
        let definition = program(&[one, one]);
        let receipt = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &cx,
                &definition,
                policy(),
                allocate,
            )
            .unwrap();
        assert_eq!(
            receipt.steps()[0].merged_edge(),
            Some(GraphEdgeMergeOutcome::Created(EId(500)))
        );
        assert_eq!(
            receipt.steps()[1].merged_edge(),
            Some(GraphEdgeMergeOutcome::Matched(EId(500)))
        );
        assert_eq!(receipt.stats().mutation_effects, 1);
        assert_eq!(receipt.stats().target_edge_visits, 1);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_edge(&db, EId(500), 7);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_step_failure_rolls_back_all_occurrences_without_erasing_prior_transaction_work() {
    let ((), report) = run_async_under_lab(0xe8a0_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = database(&commit, true, true).await;
        let before = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let definition = program(&[
            REPEATED,
            "MATCH (a)-[s:S]->(b) MERGE (a)-[r:R]->(b) ON MATCH SET r.p=12/(r.p-22)",
        ]);
        let error = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &cx,
                &definition,
                policy(),
                no_allocation,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteProgramError::EdgeUpsert {
                statement: 1,
                source: GqlQueryError::Source(GraphEdgeUpsertError::Expression { .. }),
            }
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), before);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_edge(&db, FIRST, 10);
        assert_edge(&db, SECOND, 20);
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_and_one_short_program_budgets_cover_selection_actions_and_receipt_construction() {
    let ((), report) = run_async_under_lab(0xe8a0_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let definition = program(&[PAIRS, PAIRS]);
        let baseline: GraphWriteProgramStats = {
            let mut db = database(&commit, false, false).await;
            let mut txn = db.begin(&txcx).unwrap();
            let receipt = txn
                .execute_graph_write_program_returning_governed(
                    &mut db,
                    &cx,
                    &definition,
                    policy(),
                    allocate,
                )
                .unwrap();
            txn.abort();
            receipt.stats()
        };
        let rows = baseline.selection;
        let evaluator = baseline.evaluator;
        assert!(
            rows.snapshot_records > 0 && evaluator.work_units > 0 && evaluator.scratch_entries > 0
        );
        let exact_query = GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            evaluator.work_units,
            evaluator.scratch_entries,
        );
        let exact = GraphWriteProgramPolicy::new(exact_query, 4, 0, 2);
        let mut cases = vec![(exact, true)];
        for query in [
            GqlQueryPolicy::new(
                rows.snapshot_records - 1,
                rows.result_rows,
                evaluator.work_units,
                evaluator.scratch_entries,
            ),
            GqlQueryPolicy::new(
                rows.snapshot_records,
                rows.result_rows - 1,
                evaluator.work_units,
                evaluator.scratch_entries,
            ),
            GqlQueryPolicy::new(
                rows.snapshot_records,
                rows.result_rows,
                evaluator.work_units - 1,
                evaluator.scratch_entries,
            ),
            GqlQueryPolicy::new(
                rows.snapshot_records,
                rows.result_rows,
                evaluator.work_units,
                evaluator.scratch_entries - 1,
            ),
        ] {
            cases.push((GraphWriteProgramPolicy::new(query, 4, 0, 2), false));
        }
        cases.push((GraphWriteProgramPolicy::new(exact_query, 3, 0, 2), false));
        cases.push((GraphWriteProgramPolicy::new(exact_query, 4, 0, 1), false));
        for (budget, success) in cases {
            let mut db = database(&commit, false, false).await;
            let before = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            let result = txn.execute_graph_write_program_returning_governed(
                &mut db,
                &cx,
                &definition,
                budget,
                allocate,
            );
            if success {
                assert_eq!(result.unwrap().stats(), baseline);
            } else {
                let error = result.unwrap_err();
                assert!(matches!(
                    error,
                    GraphWriteProgramError::Program(GraphMutationProgramError::Budget { .. })
                        | GraphWriteProgramError::CreationBudget { .. }
                ));
                assert_eq!(txn.staged_effect_digest().unwrap(), digest);
                assert!(txn.edges(&db).unwrap().is_empty());
            }
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.edges().unwrap().is_empty());
            txn.abort();
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn later_allocator_failure_restores_the_prefix_but_never_reclaims_issued_ids() {
    let ((), report) = run_async_under_lab(0xe8a0_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = database(&commit, false, false).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let mut issued = Vec::new();
        let result = txn.execute_graph_write_program_returning_governed(
            &mut db,
            &cx,
            &program(&[PAIRS]),
            policy(),
            |request| {
                issued.push(request);
                if issued.len() == 2 {
                    Err(())
                } else {
                    allocate(request)
                }
            },
        );
        assert!(matches!(
            result,
            Err(GraphWriteProgramError::EdgeUpsert { statement: 0, .. })
        ));
        assert_eq!(issued.len(), 2);
        assert_eq!(
            issued[0].request,
            GraphInsertRequest::Edge { row: 0, edge: 0 }
        );
        assert_eq!(
            issued[1].request,
            GraphInsertRequest::Edge { row: 1, edge: 0 }
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert!(txn.edges(&db).unwrap().is_empty());
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// One no-RETURN statement through the public write facade, allocating fresh
/// relationship identities from `next`.
async fn write_text(
    db: &mut Database<MemVfs>,
    (commit, cx, txcx): (&CommitCx, &QueryCx, &TxnCx),
    text: &str,
    next: &mut u128,
) -> Result<(), String> {
    db.query_write(
        txcx,
        cx,
        commit,
        text,
        &GqlParameters::new(),
        symbols,
        R,
        policy(),
        |_| {
            *next += 1;
            Ok::<_, ()>(ElementId::Edge(EId(*next)))
        },
    )
    .await
    .map(|_| ())
    .map_err(|error| format!("{error:?}"))
}

fn vertex_props(db: &Database<MemVfs>, vertex: VId, expected: &[(PropertyKeyId, i64)]) {
    let expected: Vec<_> = expected
        .iter()
        .map(|&(key, value)| (key, CanonicalScalar::Int(value).into()))
        .collect();
    assert_eq!(
        db.vertex(vertex).unwrap().unwrap().props,
        expected,
        "{vertex:?}"
    );
}

#[test]
fn on_and_set_clauses_write_carried_endpoints_atomically() {
    // TCK Merge6 [1] and Merge7 [1] (fgdb-at06q): an anonymous relationship's
    // ON clauses write its endpoints, and a named one's clauses may write
    // both, each right-hand side reading the matched row.
    const ANONYMOUS: &str = "MATCH (a),(b) WHERE a.p=1 AND b.p>1 MERGE (a)-[:R]->(b) \
        ON CREATE SET b.q=b.p*10 ON MATCH SET b.q=b.q+a.p";
    let ((), report) = run_async_under_lab(0xe8a0_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let cxs = (&commit, &cx, &txcx);
        let mut db = database(&commit, false, false).await;
        let mut next = 900;
        write_text(&mut db, cxs, ANONYMOUS, &mut next)
            .await
            .unwrap();
        assert_eq!(db.edges().unwrap().len(), 2);
        assert!(db.edges().unwrap().iter().all(|edge| edge.props.is_empty()));
        vertex_props(&db, VId(1), &[(P, 1)]);
        vertex_props(&db, VId(2), &[(P, 2), (Q, 20)]);
        vertex_props(&db, VId(3), &[(P, 3), (Q, 30)]);
        // Both pairs now match: only ON MATCH runs.
        write_text(&mut db, cxs, ANONYMOUS, &mut next)
            .await
            .unwrap();
        assert_eq!(db.edges().unwrap().len(), 2);
        vertex_props(&db, VId(2), &[(P, 2), (Q, 21)]);
        vertex_props(&db, VId(3), &[(P, 3), (Q, 31)]);
        // The relationship and both endpoints in one statement; the
        // trailing SET writes after the selected branch.
        write_text(
            &mut db,
            cxs,
            "MATCH (a),(b) WHERE a.p=2 AND b.p=3 MERGE (a)-[r:R]->(b) \
             ON CREATE SET r.p=a.p+b.p, a.q=0 SET b.p=b.p+100",
            &mut next,
        )
        .await
        .unwrap();
        assert_eq!(db.edges().unwrap().len(), 3);
        assert_edge(&db, EId(next), 5);
        vertex_props(&db, VId(2), &[(P, 2), (Q, 0)]);
        vertex_props(&db, VId(3), &[(P, 103), (Q, 31)]);
        // A failing trailing SET rolls back the creation and the staged ON
        // CREATE endpoint write with it.
        let before = db.frontier().unwrap();
        assert!(
            write_text(
                &mut db,
                cxs,
                "MATCH (a),(b) WHERE a.p=1 AND b.p=103 MERGE (a)-[:R]->(b) \
                 ON CREATE SET b.q=-1 SET a.q=1/0",
                &mut next,
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.edges().unwrap().len(), 3);
        vertex_props(&db, VId(1), &[(P, 1)]);
        vertex_props(&db, VId(3), &[(P, 103), (Q, 31)]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
