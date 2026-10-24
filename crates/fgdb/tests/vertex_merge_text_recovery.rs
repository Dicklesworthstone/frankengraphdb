//! Recovery and overlay contracts for the public all-target vertex MERGE path.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue, QueryWriteError, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexUpsertError, GraphWriteIdentityRequest, GraphWriteProgramPolicy,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
// Same low word as VId(1): selection and output must retain all identity bits.
const WIDE: VId = VId((1_u128 << 96) | 1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy(rows: u64, creations: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(1_000, rows, 5_000_000, 2_000_000),
        1_000,
        creations,
        0,
    )
}

fn int(value: i64) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)))
}

fn vertex(value: VId) -> QueryValue {
    QueryValue::Value(GraphValue::Vertex(value))
}

fn result(columns: &[&str], rows: Vec<Vec<QueryValue>>) -> QueryResult {
    QueryResult::Rows {
        columns: columns.iter().map(|name| (*name).to_owned()).collect(),
        rows,
    }
}

fn no_allocate(_: GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    panic!("a nonempty vertex match must never allocate")
}

fn seed(entries: &[(VId, i64, i64)]) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for &(id, key, value) in entries {
        batch.create_vertex(
            id,
            vec![LabelId(1)],
            vec![
                (P, CanonicalScalar::Int(key)),
                (Q, CanonicalScalar::Int(value)),
            ],
        );
    }
    batch
}

#[test]
fn staged_and_committed_targets_share_results_budgets_and_statement_rollback() {
    let ((), report) = run_async_under_lab(0xc9e2_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed(&[(VId(1), 1, 4)])).await.unwrap();
        let before = db.frontier().unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        transaction
            .write(&mut db, seed(&[(WIDE, 1, 7), (VId(3), 99, 20)]))
            .unwrap();
        let arguments = GqlParameters::new()
            .with_int64("step", 3)
            .unwrap()
            .with_list("tail", vec![GraphValue::Scalar(CanonicalScalar::Int(5))])
            .unwrap();
        let actual = transaction
            .query_write(
                &mut db,
                &cx,
                "MERGE (n:Person {p:1}) ON MATCH SET n.q=n.q+$step \
                 RETURN n,n.q AS q,$tail AS tail ORDER BY n",
                &arguments,
                symbols,
                R,
                policy(2, 0),
                no_allocate,
            )
            .unwrap();
        let tail = QueryValue::Value(GraphValue::List(
            vec![GraphValue::Scalar(CanonicalScalar::Int(5))].into_boxed_slice(),
        ));
        assert_eq!(
            actual,
            result(
                &["n", "q", "tail"],
                vec![
                    vec![vertex(VId(1)), int(7), tail.clone()],
                    vec![vertex(WIDE), int(10), tail],
                ],
            ),
        );
        let digest = transaction.staged_effect_digest().unwrap();
        let refused = transaction.query_write(
            &mut db,
            &cx,
            "MERGE (n:Person {p:1}) ON MATCH SET n.q=n.q-10 \
             RETURN 10/n.q AS bad LIMIT 0",
            &GqlParameters::new(),
            symbols,
            R,
            policy(0, 0),
            no_allocate,
        );
        assert!(matches!(
            refused,
            Err(QueryWriteError::VertexUpsert(GqlQueryError::Source(
                GraphVertexUpsertError::Returning(_)
            )))
        ));
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        let refused = transaction.query_write(
            &mut db,
            &cx,
            "MERGE (n:Person {p:1}) ON MATCH SET n.q=99 RETURN n",
            &GqlParameters::new(),
            symbols,
            R,
            policy(1, 0),
            no_allocate,
        );
        assert!(matches!(
            refused,
            Err(QueryWriteError::VertexUpsert(GqlQueryError::Rows(_)))
        ));
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        let cancelled = cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
        let refused = transaction.query_write(
            &mut db,
            &cancelled,
            "MERGE (n:Person {p:1}) ON MATCH SET n.q=99 RETURN n",
            &GqlParameters::new(),
            symbols,
            R,
            policy(2, 0),
            no_allocate,
        );
        assert!(matches!(
            refused,
            Err(QueryWriteError::VertexUpsert(GqlQueryError::Interrupted(_)))
        ));
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(1).into()),
                (Q, CanonicalScalar::Int(4).into())
            ],
        );
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 3);
        for (id, key, value) in [(VId(1), 1, 7), (WIDE, 1, 10), (VId(3), 99, 20)] {
            assert_eq!(
                db.vertex(id).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(key).into()),
                    (Q, CanonicalScalar::Int(value).into())
                ],
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_create_return_spends_identity_without_publishing_then_create_and_match_succeed() {
    let ((), report) = run_async_under_lab(0xc9e2_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let next = AtomicU64::new(100);
        let mut allocate = |request: GraphWriteIdentityRequest| -> Result<ElementId, ()> {
            assert_eq!(request.statement, 0);
            assert!(matches!(
                request.request,
                GraphInsertRequest::Vertex { row: 0, vertex: 0 }
            ));
            Ok(ElementId::Vertex(VId(u128::from(
                next.fetch_add(1, Ordering::Relaxed),
            ))))
        };
        let refused = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MERGE (n:Person {p:5}) ON CREATE SET n.q=0 RETURN 10/n.q AS bad LIMIT 0",
                &GqlParameters::new(),
                symbols,
                R,
                policy(0, 1),
                &mut allocate,
            )
            .await;
        assert!(matches!(
            refused,
            Err(QueryWriteError::VertexUpsert(GqlQueryError::Source(
                GraphVertexUpsertError::Returning(_)
            )))
        ));
        assert_eq!(next.load(Ordering::Relaxed), 101);
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
        for (expected, creations) in [(8, 1), (9, 0)] {
            let actual = db
                .query_write(
                    &txcx,
                    &cx,
                    &commit,
                    "MERGE (n:Person {p:5}) ON CREATE SET n.q=8 \
                     ON MATCH SET n.q=n.q+1 RETURN n,n.q AS q",
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(1, creations),
                    &mut allocate,
                )
                .await
                .unwrap();
            assert_eq!(
                actual,
                result(&["n", "q"], vec![vec![vertex(VId(101)), int(expected)]]),
            );
            assert_eq!(next.load(Ordering::Relaxed), 102);
            assert_eq!(db.vertices().unwrap().len(), 1);
            assert!(db.vertex(VId(100)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        assert_eq!(db.frontier().unwrap().0, before.0 + 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
