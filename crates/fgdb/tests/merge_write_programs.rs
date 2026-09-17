//! MERGE and branch-action upserts inside the real atomic write-program path.
//! These witnesses use the canonical transaction overlay and Chronicle commit,
//! not a replacement in-memory graph or a callback that pretends to roll back.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphMutationProgramDimension, GraphMutationProgramError,
    GraphSymbol, GraphSymbolKind, GraphVertexMergeOutcome, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteStatement, PreparedGraphInsertText,
    PreparedGraphMutationText, PreparedGraphText, PreparedGraphVertexMergeText,
    PreparedGraphVertexUpsertText, PreparedGraphWriteProgram,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
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
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn policy(effects: u64, vertices: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(), effects, vertices, 0)
}
fn merge(value: i64) -> GraphWriteStatement {
    PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$key})", R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new().with_int64("key", value).unwrap())
        .unwrap()
        .into()
}
fn upsert(value: i64) -> GraphWriteStatement {
    PreparedGraphVertexUpsertText::prepare(
        "MERGE (n:Person {p:$key}) ON MATCH SET n.q=41 ON CREATE SET n.q=5",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new().with_int64("key", value).unwrap())
    .unwrap()
    .into()
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(99))]);
    db.write(cx, batch).await.unwrap();
}

#[test]
fn mixed_merge_create_and_update_steps_share_overlay_and_return_exact_identities() {
    let ((), report) = run_async_under_lab(0x6e29_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let insert = PreparedGraphInsertText::prepare("CREATE (n:Person {p:2})", R, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let update = PreparedGraphMutationText::prepare(
            "MATCH (n:Person) WHERE n.p=1 SET n.q=n.q+1",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let program = PreparedGraphWriteProgram::prepare(vec![
            merge(1),
            upsert(1),
            insert.into(),
            update.into(),
        ])
        .unwrap();
        let mut requests = Vec::new();
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                policy(2, 2),
                |request| {
                    assert_eq!(
                        request.request,
                        GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    );
                    requests.push(request);
                    Ok::<_, ()>(ElementId::Vertex(VId(100 + request.statement as u128)))
                },
            )
            .unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.statement)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        let stats = receipt.stats();
        assert_eq!(
            (
                stats.completed_statements,
                stats.created_vertices,
                stats.created_edges
            ),
            (4, 2, 0)
        );
        assert_eq!((stats.mutation_effects, stats.target_vertex_visits), (2, 2));
        assert_eq!(
            receipt.steps()[0].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(VId(100)))
        );
        assert_eq!(
            receipt.steps()[1].merged_vertex(),
            Some(GraphVertexMergeOutcome::Matched(VId(100)))
        );
        assert_eq!(receipt.steps()[1].created_vertices(), Some(&[][..]));
        assert_eq!(receipt.steps()[2].created_vertices(), Some(&[VId(102)][..]));
        assert_eq!(receipt.steps()[3].mutation_targets(), Some(&[VId(100)][..]));
        let read =
            PreparedGraphText::prepare("MATCH (n:Person) WHERE n.p=1 AND n.q=42 RETURN n", symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        let rows = txn
            .execute_graph_pattern_governed(&mut db, &query, &read, query_policy())
            .unwrap();
        assert_eq!(
            rows.rows.result_rows, 1,
            "later assignments must observe the MERGE branch update"
        );
        assert_eq!(rows.value[0].values()[0].as_vertex(), Some(VId(100)));
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert!(db.vertex(VId(100)).unwrap().is_some());
        assert!(db.vertex(VId(102)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn repeated_merges_match_the_earlier_creation_and_allow_zero_quota_reexecution() {
    let ((), report) = run_async_under_lab(0x6e29_2002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let program = PreparedGraphWriteProgram::prepare(vec![merge(7), merge(7)]).unwrap();
        let mut allocations = 0;
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                policy(0, 1),
                |request| {
                    assert_eq!(request.statement, 0);
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(100)))
                },
            )
            .unwrap();
        assert_eq!(allocations, 1);
        assert_eq!(receipt.stats().created_vertices, 1);
        assert_eq!(
            receipt.steps()[1].merged_vertex(),
            Some(GraphVertexMergeOutcome::Matched(VId(100)))
        );
        txn.finish(&mut db, &commit).await.unwrap();
        let frontier = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn
            .execute_graph_write_program_governed(
                &mut db,
                &query,
                &program,
                policy(0, 0),
                |_| -> Result<ElementId, ()> { panic!("matched program must not allocate") },
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 0);
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exhausted_creation_quota_restores_existing_transaction_prefix() {
    let ((), report) = run_async_under_lab(0x6e29_2003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(200), vec![PERSON], vec![(P, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let program = PreparedGraphWriteProgram::prepare(vec![merge(1), merge(2)]).unwrap();
        let mut allocations = 0;
        let error = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                policy(0, 1),
                |request| {
                    assert_eq!(
                        request.statement, 0,
                        "over-quota step must refuse before allocation"
                    );
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(100)))
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteProgramError::CreationBudget {
                statement: 1,
                dimension: GraphInsertLimitDimension::Vertices,
                limit: 1,
                observed: 2,
            }
        ));
        assert_eq!(
            allocations, 1,
            "rollback does not reclaim an externally issued ID"
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(
            db.vertex(VId(200)).unwrap().is_some(),
            "pre-program staged prefix survives"
        );
        assert!(
            db.vertex(VId(100)).unwrap().is_none(),
            "failed program prefix must not become durable"
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn create_branch_actions_count_without_fabricating_match_rows_and_late_action_failure_rolls_back() {
    let ((), report) = run_async_under_lab(0x6e29_2004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let one = PreparedGraphWriteProgram::prepare(vec![upsert(7)]).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn
            .execute_graph_write_program_governed(&mut db, &query, &one, policy(1, 1), |_| {
                Ok::<_, ()>(ElementId::Vertex(VId(100)))
            })
            .unwrap();
        assert_eq!(
            stats.selection.result_rows, 0,
            "a creation unit is not a MATCH occurrence"
        );
        assert_eq!(
            (
                stats.created_vertices,
                stats.mutation_effects,
                stats.target_vertex_visits
            ),
            (1, 1, 1)
        );
        txn.abort();

        let program = PreparedGraphWriteProgram::prepare(vec![upsert(7), upsert(7)]).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let mut allocations = 0;
        let error = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                policy(1, 1),
                |_| {
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(101)))
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
                statement: 1,
                dimension: GraphMutationProgramDimension::Effects,
                limit: 1,
                observed: 2,
            })
        ));
        assert_eq!(allocations, 1);
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        txn.abort();
        assert!(db.vertex(VId(100)).unwrap().is_none());
        assert!(db.vertex(VId(101)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
