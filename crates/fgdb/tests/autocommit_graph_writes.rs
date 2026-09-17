//! Autocommit composes the public transaction lifecycle; it is not a second
//! durability path. These laws distinguish committed writes, zero-effect read
//! closes, execution refusal and allocator refusal while checking pin cleanup.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphMutationPolicy, GraphSymbol, GraphSymbolKind,
    GraphWriteIdentityRequest, GraphWriteProgramPolicy, GraphWriteStatement,
    PreparedGraphInsertText, PreparedGraphMutationText, PreparedGraphWriteProgram,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const COPY: LabelId = LabelId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa1; 32],
        DatabaseSecurityNamespaceId([0xa2; 32]),
        [0xa3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 3_000_000, 3_000_000)
}
fn mutation_policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(query_policy(), 10_000)
}
fn insert_policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(query_policy(), 10_000, 10_000)
}
fn program_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(), 10_000, 10_000, 10_000)
}
fn mutation(text: &str) -> fgdb_gql::PreparedGraphMutation {
    PreparedGraphMutationText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn insertion(text: &str) -> fgdb_gql::insertion::PreparedGraphInsert {
    PreparedGraphInsertText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) -> fgdb_types::CommitSeq {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(10))]);
    db.write(cx, batch).await.unwrap()
}

#[test]
fn mutation_autocommit_commits_targets_while_zero_match_closes_read_only() {
    let ((), report) = run_async_under_lab(0xa070_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let seeded = seed(&mut db, &commit).await;

        let update = mutation("MATCH (n) WHERE n.p >= 10 SET n.p=n.p+1");
        let (stats, targets, completion) = db
            .execute_graph_mutation_returning_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &update,
                mutation_policy(),
            )
            .await
            .unwrap();
        assert_eq!(targets, vec![VId(1)]);
        assert_eq!((stats.target_vertices, stats.effects), (1, 1));
        let committed = match completion {
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } => commit_seq,
            other => panic!("staged mutation did not commit: {other:?}"),
        };
        assert_eq!(committed.0, seeded.0 + 1);
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(11))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);

        let frontier = db.frontier().unwrap();
        let no_match = mutation("MATCH (n) WHERE n.p > 100 SET n.p=n.p+1");
        let (stats, completion) = db
            .execute_graph_mutation_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &no_match,
                mutation_policy(),
            )
            .await
            .unwrap();
        assert_eq!((stats.selection.result_rows, stats.effects), (0, 0));
        assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed {
            snapshot_seq, validated_through
        } if snapshot_seq == frontier && validated_through == frontier));
        assert_eq!(
            db.frontier().unwrap(),
            frontier,
            "zero-effect write must not invent a marker"
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn execution_and_allocator_refusals_abort_and_release_the_snapshot_pin() {
    let ((), report) = run_async_under_lab(0xa070_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let frontier = seed(&mut db, &commit).await;

        let arithmetic = mutation("MATCH (n) SET n.p=n.p/0");
        assert!(
            db.execute_graph_mutation_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &arithmetic,
                mutation_policy(),
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(10))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);

        let create = insertion("CREATE (x:Copy {p:7})");
        assert!(
            db.execute_graph_insert_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &create,
                insert_policy(),
                |_| Err::<ElementId, _>("allocator unavailable"),
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);

        let (stats, vertices, edges, completion) = db
            .execute_graph_insert_returning_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &create,
                insert_policy(),
                |request| {
                    Ok::<_, &'static str>(match request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(100)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(1000)),
                    })
                },
            )
            .await
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (1, 0));
        assert_eq!(vertices, vec![VId(100)]);
        assert!(edges.is_empty());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(
            db.vertex(VId(100)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(7))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mixed_autocommit_withholds_receipt_until_dependent_program_is_durable() {
    let ((), report) = run_async_under_lab(0xa070_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();

        let create = insertion("CREATE (x:Copy {p:5})");
        let update = mutation("MATCH (n:Copy) SET n.p=n.p+1");
        let program = PreparedGraphWriteProgram::prepare(vec![
            GraphWriteStatement::Insert(create),
            GraphWriteStatement::Mutation(update),
        ])
        .unwrap();
        let (receipt, completion) = db
            .execute_graph_write_program_returning_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &program,
                program_policy(),
                |request| {
                    Ok::<_, &'static str>(match request {
                        GraphWriteIdentityRequest {
                            statement: 0,
                            request: GraphInsertRequest::Vertex { .. },
                        } => ElementId::Vertex(VId(200)),
                        GraphWriteIdentityRequest {
                            statement: 0,
                            request: GraphInsertRequest::Edge { .. },
                        } => ElementId::Edge(EId(2000)),
                        other => panic!("unexpected allocation request {other:?}"),
                    })
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(receipt.steps().len(), 2);
        assert_eq!(receipt.steps()[0].created_vertices(), Some(&[VId(200)][..]));
        assert_eq!(receipt.steps()[1].mutation_targets(), Some(&[VId(200)][..]));
        assert_eq!(
            db.vertex(VId(200)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(6))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);

        // A late dependent failure returns neither a partial program receipt nor
        // a durable creation, even though its external identity was issued.
        let bad_update = mutation("MATCH (n:Copy) SET n.p=n.p/0");
        let bad = PreparedGraphWriteProgram::prepare(vec![
            GraphWriteStatement::Insert(insertion("CREATE (x:Copy {p:9})")),
            GraphWriteStatement::Mutation(bad_update),
        ])
        .unwrap();
        let failed = db
            .execute_graph_write_program_returning_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &bad,
                program_policy(),
                |request| {
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(300)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(3000)),
                    })
                },
            )
            .await;
        assert!(failed.is_err());
        assert!(db.vertex(VId(300)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
