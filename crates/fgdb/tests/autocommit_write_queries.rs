//! Returning writes through the ordinary Chronicle/Strata transaction lifecycle.
//! Expected result cells and stored effects are checked independently.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphMutationPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergePolicy, GraphVertexUpsertPolicy, PreparedGraphMutationQuery,
    PreparedGraphMutationQueryText, PreparedGraphVertexUpsertQuery,
    PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use std::convert::Infallible;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const ID: PropertyKeyId = PropertyKeyId(2);
const L: LabelId = LabelId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        _ => None,
    }
}
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, rows, 1_000_000, 1_000_000)
}
fn mutation(text: &str) -> PreparedGraphMutationQuery {
    PreparedGraphMutationQueryText::prepare_with_parameter_types(text, R, &[], symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn upsert(text: &str) -> PreparedGraphVertexUpsertQuery {
    PreparedGraphVertexUpsertQueryText::prepare_with_parameter_types(text, R, &[], symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn cells(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(4))]);
    batch.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(7))]);
    batch.add_edge(EId(9), VId(1), VId(2), vec![]);
    batch
}

#[test]
fn mutation_return_publishes_once_and_zero_matches_close_without_a_marker() {
    let ((), report) = run_async_under_lab(0xa070_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let query = mutation("MATCH (n:L) SET n.p=n.p+1 RETURN n,n.p AS p ORDER BY p DESC");
        let (_, returned, completion) = db
            .execute_graph_mutation_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &query,
                GraphMutationPolicy::new(policy(2), 2),
            )
            .await
            .unwrap();
        assert_eq!(
            cells(&returned.value),
            vec![
                vec![GraphValue::Vertex(VId(2)), int(8)],
                vec![GraphValue::Vertex(VId(1)), int(5)],
            ]
        );
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(2)
            }
        ));
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(5))]
        );
        assert_eq!(
            db.vertex(VId(2)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(8))]
        );
        assert_eq!(db.delta_since(CommitSeq(1)).unwrap().count(), 1);

        let query = mutation("MATCH (n:L) WHERE n.p > 100 SET n.p=0 RETURN n");
        let (_, returned, completion) = db
            .execute_graph_mutation_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &query,
                GraphMutationPolicy::new(policy(0), 0),
            )
            .await
            .unwrap();
        assert!(returned.value.is_empty());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed {
                snapshot_seq: CommitSeq(2),
                ..
            }
        ));
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn result_failures_abort_all_mutations_but_limit_zero_does_not_skip_effects() {
    let ((), report) = run_async_under_lab(0xa070_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        for (text, allowance) in [
            ("MATCH (n:L) SET n.p=n.p+1 RETURN 10/(n.p-8) AS result", 10),
            ("MATCH (n:L) SET n.p=n.p+1 RETURN n", 1),
        ] {
            let result = db
                .execute_graph_mutation_query_autocommit_governed(
                    &txcx,
                    &cx,
                    &commit,
                    &mutation(text),
                    GraphMutationPolicy::new(policy(allowance), 10),
                )
                .await;
            assert!(result.is_err(), "{text}");
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(4))]
            );
            assert_eq!(
                db.vertex(VId(2)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(7))]
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        let (_, returned, completion) = db
            .execute_graph_mutation_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &mutation("MATCH (n:L) SET n.p=20 RETURN n LIMIT 0"),
                GraphMutationPolicy::new(policy(0), 2),
            )
            .await
            .unwrap();
        assert!(returned.value.is_empty());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(2)
            }
        ));
        for id in [VId(1), VId(2)] {
            assert_eq!(
                db.vertex(id).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(20))]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn remove_and_detach_return_the_native_statement_result_without_rescanning() {
    let ((), report) = run_async_under_lab(0xa070_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let invalid = mutation("MATCH (n:L) DETACH DELETE n RETURN n.p AS p");
        assert!(
            db.execute_graph_mutation_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &invalid,
                GraphMutationPolicy::new(policy(10), 10),
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        assert_eq!(db.edges().unwrap().len(), 1);
        let (_, returned, _) = db
            .execute_graph_mutation_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &mutation("MATCH (n:L) REMOVE n.p RETURN n,n.p AS p"),
                GraphMutationPolicy::new(policy(2), 2),
            )
            .await
            .unwrap();
        assert_eq!(
            cells(&returned.value),
            vec![
                vec![
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Scalar(CanonicalScalar::Null)
                ],
                vec![
                    GraphValue::Vertex(VId(2)),
                    GraphValue::Scalar(CanonicalScalar::Null)
                ],
            ]
        );
        let (_, returned, completion) = db
            .execute_graph_mutation_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &mutation("MATCH (n:L) DETACH DELETE n RETURN n"),
                GraphMutationPolicy::new(policy(2), 2),
            )
            .await
            .unwrap();
        assert_eq!(
            cells(&returned.value),
            vec![
                vec![GraphValue::Vertex(VId(1))],
                vec![GraphValue::Vertex(VId(2))],
            ]
        );
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(3)
            }
        ));
        assert!(db.vertices().unwrap().is_empty());
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_return_uses_the_selected_branch_and_never_reclaims_a_failed_result_id() {
    let ((), report) = run_async_under_lab(0xa070_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let failed = upsert("MERGE (n:L {id:1}) SET n.p=9 RETURN 10/(n.p-9) AS value");
        let mut allocations = 0;
        let result = db
            .execute_graph_vertex_upsert_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &failed,
                GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(policy(10)), 10),
                |_| {
                    allocations += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(u128::MAX - 1)))
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(
            allocations, 1,
            "RETURN fails after creation, not before allocation"
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);

        let query =
            upsert("MERGE (n:L {id:1}) ON CREATE SET n.p=4 ON MATCH SET n.p=7 RETURN n,n.p AS p");
        let (_, outcome, returned, completion) = db
            .execute_graph_vertex_upsert_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &query,
                GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(policy(1)), 1),
                |_| Ok::<_, Infallible>(ElementId::Vertex(VId(u128::MAX))),
            )
            .await
            .unwrap();
        assert_eq!(outcome.vertex(), VId(u128::MAX));
        assert_eq!(
            cells(&returned.value),
            vec![vec![GraphValue::Vertex(VId(u128::MAX)), int(4)]]
        );
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(1)
            }
        ));
        let (_, outcome, returned, completion) = db
            .execute_graph_vertex_upsert_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &query,
                GraphVertexUpsertPolicy::new(
                    GraphVertexMergePolicy::new(policy(1)).with_creation_limit(0),
                    1,
                ),
                |_| -> Result<ElementId, Infallible> { panic!("matched MERGE must not allocate") },
            )
            .await
            .unwrap();
        assert_eq!(outcome.vertex(), VId(u128::MAX));
        assert_eq!(
            cells(&returned.value),
            vec![vec![GraphValue::Vertex(VId(u128::MAX)), int(7)]]
        );
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(2)
            }
        ));
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn database_owned_merge_return_survives_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xa070_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let query = upsert("MERGE (n:L {id:1}) ON CREATE SET n.p=4 RETURN n,n.p AS p");
        let allowance = GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(policy(1)), 1);
        let (_, outcome, returned, completion) = db
            .execute_graph_vertex_upsert_query_autocommit_engine_governed(
                &txcx, &cx, &commit, &query, allowance,
            )
            .await
            .unwrap();
        let id = outcome.vertex();
        assert_eq!(
            cells(&returned.value),
            vec![vec![GraphValue::Vertex(id), int(4)]]
        );
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(1)
            }
        ));
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let (_, outcome, returned, completion) = db
            .execute_graph_vertex_upsert_query_autocommit_engine_governed(
                &txcx, &cx, &commit, &query, allowance,
            )
            .await
            .unwrap();
        assert_eq!(outcome.vertex(), id);
        assert_eq!(
            cells(&returned.value),
            vec![vec![GraphValue::Vertex(id), int(4)]]
        );
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed {
                snapshot_seq: CommitSeq(1),
                ..
            }
        ));
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
