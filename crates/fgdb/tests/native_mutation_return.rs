//! Public native write dispatch must reach each statement's returning engine.
//! No graph state is recovered by rerunning RETURN as a separate read.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, QueryResult, QueryWriteError, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlMapParameter, GqlParameterValue, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphAggregateValue, GraphMutationQueryError, GraphSymbol, GraphSymbolKind,
    GraphVertexUpsertError, GraphWriteIdentityRequest, GraphWriteProgramPolicy,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use std::convert::Infallible;
use std::error::Error;
use std::sync::Arc;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const ID: PropertyKeyId = PropertyKeyId(2);
const L: LabelId = LabelId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        _ => None,
    }
}
fn policy(rows: u64, effects: u64, vertices: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(10_000, rows, 1_000_000, 1_000_000),
        effects,
        vertices,
        0,
    )
}
fn no_allocate(_: GraphWriteIdentityRequest) -> Result<ElementId, Infallible> {
    panic!("this statement must not allocate an identity")
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn rows(result: QueryResult, names: &[&str]) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("RETURN was incorrectly dispatched to the no-result script engine")
    };
    assert_eq!(
        columns,
        names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()
    );
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    GraphAggregateValue::Value(value) => value,
                    _ => panic!("write RETURN cells retain native graph/scalar domains"),
                })
                .collect()
        })
        .collect()
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(
        VId(1),
        vec![L],
        vec![(P, CanonicalScalar::Int(4)), (ID, CanonicalScalar::Int(1))],
    );
    batch.create_vertex(
        VId(2),
        vec![L],
        vec![(P, CanonicalScalar::Int(7)), (ID, CanonicalScalar::Int(2))],
    );
    batch
}

#[test]
fn public_autocommit_returns_parameterized_mutation_rows_with_collection_types() {
    let ((), report) = run_async_under_lab(0xa071_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let payload = GraphValue::List(
            vec![int(3), GraphValue::Scalar(CanonicalScalar::Null)].into_boxed_slice(),
        );
        let meta = GraphValue::map(vec![("k".into(), int(17))]).unwrap();
        let mut args = GqlParameters::new()
            .with_int64("step", 2)
            .unwrap()
            .with_list(
                "tail",
                vec![int(3), GraphValue::Scalar(CanonicalScalar::Null)],
            )
            .unwrap();
        args.insert(
            "meta",
            GqlParameterValue::Map(GqlMapParameter::new(vec![("k".into(), int(17))]).unwrap()),
        )
        .unwrap();
        let frozen = args.canonical_bytes();
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "/* CREATE (x) RETURN x */ MATCH (n:L) SET n.p=n.p+$step \
             RETURN n.id AS id,n.p AS p,$tail AS tail,$meta AS meta ORDER BY id",
                &args,
                symbols,
                R,
                policy(2, 2, 0),
                no_allocate,
            )
            .await
            .unwrap();
        assert_eq!(
            rows(result, &["id", "p", "tail", "meta"]),
            vec![
                vec![int(1), int(6), payload.clone(), meta.clone()],
                vec![int(2), int(9), payload, meta],
            ]
        );
        assert_eq!(args.canonical_bytes(), frozen);
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        for (id, expected) in [(VId(1), 6), (VId(2), 9)] {
            assert_eq!(
                db.vertex(id).unwrap().unwrap().props[0],
                (P, CanonicalScalar::Int(expected))
            );
        }
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MATCH (n:L) REMOVE n.p RETURN DISTINCT n.p AS p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 2, 0),
                no_allocate,
            )
            .await
            .unwrap();
        assert_eq!(
            rows(result, &["p"]),
            vec![vec![GraphValue::Scalar(CanonicalScalar::Null)]]
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(3));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn returning_write_classes_share_one_outer_workspace_and_refusals_preserve_its_prefix() {
    let ((), report) = run_async_under_lab(0xa071_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let vertex = VId(u128::MAX);
        let result = txn
            .query_write(
                &mut db,
                &cx,
                "CREATE (n:L {id:1,p:4}) RETURN n,n.p AS p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 0, 1),
                |request| {
                    assert_eq!(request.statement, 0);
                    assert!(matches!(
                        request.request,
                        GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    ));
                    Ok::<_, Infallible>(ElementId::Vertex(vertex))
                },
            )
            .unwrap();
        assert_eq!(
            rows(result, &["n", "p"]),
            vec![vec![GraphValue::Vertex(vertex), int(4)]]
        );
        let result = txn
            .query_write(
                &mut db,
                &cx,
                "MATCH (n:L) SET n.p=n.p+1 RETURN n.p AS p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .unwrap();
        assert_eq!(rows(result, &["p"]), vec![vec![int(5)]]);
        let result = txn
            .query_write(
                &mut db,
                &cx,
                "MERGE (n:L {id:1}) ON MATCH SET n.p=8 RETURN n,n.p AS p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .unwrap();
        assert_eq!(
            rows(result, &["n", "p"]),
            vec![vec![GraphValue::Vertex(vertex), int(8)]]
        );
        let prefix = txn.staged_effect_digest().unwrap();
        let error = txn
            .query_write(
                &mut db,
                &cx,
                "MATCH (n:L) SET n.p=9 RETURN 1/(n.p-9) AS bad",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .unwrap_err();
        assert!(matches!(
            &error,
            QueryWriteError::Mutation(
                GqlQueryError::Source(GraphMutationQueryError::Returning(_),)
            )
        ));
        assert!(error.source().is_some());
        assert_eq!(txn.staged_effect_digest().unwrap(), prefix);
        let mut allocations = 0;
        let error = txn
            .query_write(
                &mut db,
                &cx,
                "MERGE (n:L {id:2}) SET n.p=9 RETURN 1/(n.p-9) AS bad",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 1),
                |request| {
                    assert_eq!(request.statement, 0);
                    allocations += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(u128::MAX - 1)))
                },
            )
            .unwrap_err();
        assert!(matches!(
            &error,
            QueryWriteError::VertexUpsert(GqlQueryError::Source(
                GraphVertexUpsertError::Returning(_),
            ))
        ));
        assert!(error.source().is_some());
        assert_eq!(allocations, 1);
        assert_eq!(txn.staged_effect_digest().unwrap(), prefix);
        assert!(txn.vertex(&db, VId(u128::MAX - 1)).unwrap().is_none());
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert!(matches!(
            txn.query_write(
                &mut db,
                &cx,
                "MATCH (n:L) SET n.p=11",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .unwrap(),
            QueryResult::Write {
                completion: None,
                ..
            }
        ));
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: CommitSeq(1)
            }
        ));
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(
            db.vertex(vertex).unwrap().unwrap().props[0],
            (P, CanonicalScalar::Int(11))
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn public_dispatch_preserves_creation_effect_and_final_result_limits() {
    let ((), report) = run_async_under_lab(0xa071_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let create = "MERGE (n:L {id:1}) SET n.p=4 RETURN n.p AS p";
        assert!(matches!(
            db.query_write(
                &txcx,
                &cx,
                &commit,
                create,
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .await,
            Err(QueryWriteError::VertexUpsert(_))
        ));
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                create,
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 1),
                |request| {
                    assert_eq!(request.statement, 0);
                    Ok::<_, Infallible>(ElementId::Vertex(VId(10)))
                },
            )
            .await
            .unwrap();
        assert_eq!(rows(result, &["p"]), vec![vec![int(4)]]);
        for allowance in [policy(0, 1, 0), policy(1, 0, 0)] {
            let result = db
                .query_write(
                    &txcx,
                    &cx,
                    &commit,
                    "MATCH (n:L) SET n.p=7 RETURN n.p AS p",
                    &GqlParameters::new(),
                    symbols,
                    R,
                    allowance,
                    no_allocate,
                )
                .await;
            assert!(matches!(result, Err(QueryWriteError::Mutation(_))));
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
            assert_eq!(
                db.vertex(VId(10)).unwrap().unwrap().props[0],
                (P, CanonicalScalar::Int(4))
            );
        }
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MATCH (n:L) SET n.p=7 RETURN n.p AS p LIMIT 0",
                &GqlParameters::new(),
                symbols,
                R,
                policy(0, 1, 0),
                no_allocate,
            )
            .await
            .unwrap();
        assert!(rows(result, &["p"]).is_empty());
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        assert_eq!(
            db.vertex(VId(10)).unwrap().unwrap().props[0],
            (P, CanonicalScalar::Int(7))
        );
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MATCH (n:L) DETACH DELETE n RETURN n",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1, 1, 0),
                no_allocate,
            )
            .await
            .unwrap();
        assert_eq!(
            rows(result, &["n"]),
            vec![vec![GraphValue::Vertex(VId(10))]]
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(3));
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn public_return_cancellation_and_bad_tail_never_publish_a_successful_prefix() {
    let ((), report) = run_async_under_lab(0xa071_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        for (index, text) in [
            "MATCH (n:L) SET n.p=11 RETURN n",
            "MERGE (n:L {id:3}) SET n.p=11 RETURN n",
        ]
        .into_iter()
        .enumerate()
        {
            let cancelled =
                cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
            let error = db
                .query_write(
                    &txcx,
                    &cancelled,
                    &commit,
                    text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(10, 10, 10),
                    no_allocate,
                )
                .await
                .unwrap_err();
            assert!(matches!(
                (index, error),
                (0, QueryWriteError::Mutation(GqlQueryError::Interrupted(_)))
                    | (
                        1,
                        QueryWriteError::VertexUpsert(GqlQueryError::Interrupted(_))
                    )
            ));
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        for text in [
            "MATCH (n:L) SET n.p=11 RETURN n; CREATE (m)",
            "MERGE (n:L {id:3}) RETURN n; CREATE (m)",
            "MATCH (n:L) SET n.p=11 RETURN COUNT(*) AS c",
            "MERGE (n:L {id:3}) RETURN COUNT(*) AS c",
        ] {
            assert!(
                db.query_write(
                    &txcx,
                    &cx,
                    &commit,
                    text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(10, 10, 10),
                    no_allocate,
                )
                .await
                .is_err(),
                "{text}"
            );
            assert_eq!(db.frontier().unwrap(), CommitSeq(1));
            assert_eq!(db.vertices().unwrap().len(), 2);
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props[0],
                (P, CanonicalScalar::Int(4))
            );
            assert_eq!(
                db.vertex(VId(2)).unwrap().unwrap().props[0],
                (P, CanonicalScalar::Int(7))
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        // Quoted keywords and comments cannot manufacture a RETURN clause.
        let result = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MATCH (n:L) SET n.p='RETURN MERGE CREATE' /* RETURN n */",
                &GqlParameters::new(),
                symbols,
                R,
                policy(2, 2, 0),
                no_allocate,
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            QueryResult::Write {
                completion: Some(EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: CommitSeq(2)
                }),
                ..
            }
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
