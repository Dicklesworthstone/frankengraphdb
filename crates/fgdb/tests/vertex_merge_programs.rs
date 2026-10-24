//! No-RETURN vertex scripts use the same all-target matcher/actions as returning
//! writes. These are native database and transaction tests, not a mock writer.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphMutationProgramError, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeOutcome, GraphWriteIdentityRequest, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteStepReceipt, PreparedGraphVertexUpsertText,
    PreparedGraphWriteProgram,
};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const IDS: [VId; 3] = [VId(1), VId((1_u128 << 96) + 2), VId((1_u128 << 112) + 3)];
const PREFIX: VId = VId(99);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x57; 32],
        DatabaseSecurityNamespaceId([0x58; 32]),
        [0x59; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(10_000, 100, 5_000_000, 2_000_000),
        100,
        10,
        0,
    )
}

fn no_allocation(_: GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    panic!("existing targets never allocate")
}

async fn seed(db: &mut Database<MemVfs>, commit: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for (vertex, p) in IDS.into_iter().zip([1, 2, 3]) {
        batch.create_vertex(
            vertex,
            vec![],
            vec![(P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(7))],
        );
    }
    db.write(commit, batch).await.unwrap();
}

fn prefix() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(PREFIX, vec![], vec![(Q, CanonicalScalar::Int(99))]);
    batch
}

fn upserts(texts: &[&str]) -> PreparedGraphWriteProgram {
    PreparedGraphWriteProgram::prepare(
        texts
            .iter()
            .map(|text| {
                PreparedGraphVertexUpsertText::prepare(text, R, symbols)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap()
                    .into()
            })
            .collect(),
    )
    .unwrap()
}

#[test]
fn public_script_updates_every_original_target_and_keeps_ordered_bulk_receipts() {
    let ((), report) = run_async_under_lab(0x7e91_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MERGE (n {q:7}) ON MATCH SET n.p=n.p+10,n.q=8 SET n.p=n.p*2; \
             MERGE (n {q:8}); MERGE (n {q:8}) ON MATCH SET n.p=n.p+1",
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                no_allocation,
            )
            .await
            .unwrap();
        let QueryResult::Write {
            receipt,
            completion,
        } = result
        else {
            panic!("no-RETURN script must retain its write receipt")
        };
        assert!(completion.is_some());
        let expected = IDS.map(GraphVertexMergeOutcome::Matched).to_vec();
        assert_eq!(receipt.steps().len(), 3);
        assert!(matches!(
            &receipt.steps()[0],
            GraphWriteStepReceipt::VertexUpsertBatch { .. }
        ));
        assert!(matches!(
            &receipt.steps()[1],
            GraphWriteStepReceipt::VertexMergeBatch { .. }
        ));
        for step in receipt.steps() {
            assert_eq!(step.merged_vertices(), Some(expected.as_slice()));
            assert_eq!(step.merged_vertex(), None);
            assert_eq!(step.created_vertices(), Some(&[][..]));
        }
        assert_eq!(receipt.stats().selection.result_rows, 9);
        assert_eq!(receipt.stats().target_vertex_visits, 6);
        assert_eq!(receipt.stats().mutation_effects, 12);
        assert_eq!(receipt.stats().created_vertices, 0);
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        for (vertex, p) in IDS.into_iter().zip([23, 25, 27]) {
            assert_eq!(
                db.vertex(vertex).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (Q, CanonicalScalar::Int(8).into())
                ]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_statement_failure_rolls_back_the_program_but_not_an_earlier_transaction_write() {
    let ((), report) = run_async_under_lab(0x7e91_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let program = upserts(&[
            "MERGE (n {q:7}) ON MATCH SET n.p=n.p+1",
            "MERGE (n {q:7}) ON MATCH SET n.p=12/(n.p-3)",
        ]);
        for returning in [false, true] {
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, prefix()).unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            let result = if returning {
                txn.execute_graph_write_program_returning_governed(
                    &mut db,
                    &cx,
                    &program,
                    policy(),
                    no_allocation,
                )
                .map(|receipt| receipt.stats())
            } else {
                txn.execute_graph_write_program_governed(
                    &mut db,
                    &cx,
                    &program,
                    policy(),
                    no_allocation,
                )
            };
            assert!(matches!(
                result,
                Err(GraphWriteProgramError::VertexUpsert { statement: 1, .. })
            ));
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            for (vertex, p) in IDS.into_iter().zip([1, 2, 3]) {
                assert_eq!(
                    txn.vertex(&db, vertex).unwrap().unwrap().props,
                    vec![
                        (P, CanonicalScalar::Int(p).into()),
                        (Q, CanonicalScalar::Int(7).into())
                    ]
                );
            }
            assert!(txn.vertex(&db, PREFIX).unwrap().is_some());
            assert_eq!(db.frontier().unwrap(), before);
            txn.abort();
        }
        assert!(db.vertex(PREFIX).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn create_match_sequences_keep_singleton_receipts_and_statement_allocator_coordinates() {
    let ((), report) = run_async_under_lab(0x7e91_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut requests = Vec::new();
        let result = txn
            .query_write(
                &mut db,
                &cx,
                "MERGE (n {q:70}); MERGE (n {q:70}) ON MATCH SET n.p=5; \
             MERGE (n {q:71}) ON CREATE SET n.p=7",
                &GqlParameters::new(),
                symbols,
                R,
                policy(),
                |request| -> Result<ElementId, ()> {
                    requests.push(request);
                    Ok(ElementId::Vertex(VId(
                        (1_u128 << 120) + request.statement as u128
                    )))
                },
            )
            .unwrap();
        let QueryResult::Write {
            receipt,
            completion,
        } = result
        else {
            panic!("write receipt")
        };
        assert!(completion.is_none());
        assert_eq!(requests.len(), 2);
        for (request, statement) in requests.into_iter().zip([0, 2]) {
            assert_eq!(request.statement, statement);
            assert_eq!(
                request.request,
                GraphInsertRequest::Vertex { row: 0, vertex: 0 }
            );
        }
        let first = VId(1_u128 << 120);
        let last = VId((1_u128 << 120) + 2);
        assert_eq!(
            receipt.steps()[0].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(first))
        );
        assert_eq!(
            receipt.steps()[1].merged_vertex(),
            Some(GraphVertexMergeOutcome::Matched(first))
        );
        assert_eq!(
            receipt.steps()[2].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(last))
        );
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(receipt.stats().selection.result_rows, 1);
        assert_eq!(receipt.stats().target_vertex_visits, 2);
        assert_eq!(receipt.stats().mutation_effects, 2);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.vertex(first).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(5).into()),
                (Q, CanonicalScalar::Int(70).into())
            ]
        );
        assert_eq!(
            db.vertex(last).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(7).into()),
                (Q, CanonicalScalar::Int(71).into())
            ]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_and_one_less_program_budgets_cover_all_targets_in_both_entrypoints() {
    let ((), report) = run_async_under_lab(0x7e91_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let program = upserts(&[
            "MERGE (n {q:7}) ON MATCH SET n.p=n.p+1",
            "MERGE (n {q:7}) ON MATCH SET n.p=n.p+1",
        ]);
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, prefix()).unwrap();
        let stats = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &cx,
                &program,
                policy(),
                no_allocation,
            )
            .unwrap()
            .stats();
        assert_eq!(stats.mutation_effects, 6);
        assert_eq!(stats.target_vertex_visits, 6);
        txn.abort();
        let limits = [
            stats.selection.snapshot_records,
            stats.selection.result_rows,
            stats.evaluator.work_units,
            stats.evaluator.scratch_entries,
            stats.mutation_effects,
        ];
        for returning in [false, true] {
            for dimension in 0..=limits.len() {
                let mut values = limits;
                if dimension < limits.len() {
                    values[dimension] -= 1;
                }
                let budget = GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(values[0], values[1], values[2], values[3]),
                    values[4],
                    0,
                    0,
                );
                let mut txn = db.begin(&txcx).unwrap();
                txn.write(&mut db, prefix()).unwrap();
                let digest = txn.staged_effect_digest().unwrap();
                let result = if returning {
                    txn.execute_graph_write_program_returning_governed(
                        &mut db,
                        &cx,
                        &program,
                        budget,
                        no_allocation,
                    )
                    .map(|receipt| receipt.stats())
                } else {
                    txn.execute_graph_write_program_governed(
                        &mut db,
                        &cx,
                        &program,
                        budget,
                        no_allocation,
                    )
                };
                if dimension == limits.len() {
                    assert_eq!(result.unwrap(), stats);
                } else {
                    assert!(
                        matches!(
                            result,
                            Err(GraphWriteProgramError::Program(
                                GraphMutationProgramError::Budget { .. }
                            ))
                        ),
                        "dimension {dimension}: {result:?}"
                    );
                    assert_eq!(txn.staged_effect_digest().unwrap(), digest);
                }
                txn.abort();
            }
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn actionless_matches_keep_bulk_receipts_without_inventing_modified_targets() {
    let ((), report) = run_async_under_lab(0x7e91_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MERGE (n {q:7}) ON CREATE SET n.p=99; MERGE (n {q:7})",
                &GqlParameters::new(),
                symbols,
                R,
                GraphWriteProgramPolicy::new(policy().mutations.query, 0, 0, 0),
                no_allocation,
            )
            .await
            .unwrap();
        let QueryResult::Write { receipt, .. } = result else {
            panic!("write receipt")
        };
        assert_eq!(receipt.stats().selection.result_rows, 6);
        assert_eq!(receipt.stats().mutation_effects, 0);
        assert_eq!(receipt.stats().target_vertex_visits, 0);
        assert_eq!(receipt.stats().created_vertices, 0);
        assert_eq!(db.frontier().unwrap(), before);
        for step in receipt.steps() {
            assert_eq!(step.merged_vertices().unwrap().len(), 3);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
