//! End-to-end idempotent graph ingestion: create/find vertices and relationships
//! through native prepared text, one canonical overlay and one atomic program.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergeError, GraphEdgeMergeOutcome,
    GraphSymbol, GraphSymbolKind, GraphVertexMergeOutcome, GraphWriteProgramError,
    GraphWriteProgramPolicy, PreparedGraphEdgeMergeText, PreparedGraphVertexMergeText,
    PreparedGraphWriteProgram, PreparedGraphWriteProgramTemplate,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const PERSON: LabelId = LabelId(1);
const EDGE_TEXT: &str = "MATCH (a:Person),(b:Person) WHERE a.p=$left AND b.p=$right MERGE (a)-[:R]->(b)";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x61; 32], DatabaseSecurityNamespaceId([0x62; 32]), [0x63; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn arguments() -> GqlParameters {
    GqlParameters::new().with_int64("left", 7).unwrap().with_int64("right", 8).unwrap()
}
fn program() -> PreparedGraphWriteProgram {
    let left = PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$left})", R, symbols).unwrap();
    let right = PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$right})", R, symbols).unwrap();
    let edge = PreparedGraphEdgeMergeText::prepare(EDGE_TEXT, R, symbols).unwrap();
    PreparedGraphWriteProgramTemplate::prepare(vec![
        left.into(), right.into(), edge.clone().into(), edge.into(),
    ]).unwrap().bind_parameters(&arguments()).unwrap()
}
fn policy(vertices: u64, edges: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000), 0, vertices, edges,
    )
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(99))]);
    db.write(cx, batch).await.unwrap();
}

#[test]
fn native_vertex_and_relationship_program_is_idempotent_and_returns_every_outcome() {
    let ((), report) = run_async_under_lab(0xed9e_4001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let program = program();
        let mut txn = db.begin(&txcx).unwrap();
        let mut requests = Vec::new();
        let receipt = txn.execute_graph_write_program_returning_governed(
            &mut db, &query, &program, policy(2, 1), |request| {
                requests.push(request);
                Ok::<_, ()>(match (request.statement, request.request) {
                    (0, GraphInsertRequest::Vertex { row: 0, vertex: 0 }) => ElementId::Vertex(VId(10)),
                    (1, GraphInsertRequest::Vertex { row: 0, vertex: 0 }) => ElementId::Vertex(VId(11)),
                    (2, GraphInsertRequest::Edge { row: 0, edge: 0 }) => ElementId::Edge(EId(12)),
                    _ => panic!("matched relationship must not allocate again"),
                })
            },
        ).unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!((receipt.stats().completed_statements, receipt.stats().created_vertices,
            receipt.stats().created_edges), (4, 2, 1));
        assert_eq!(receipt.steps()[0].merged_vertex(), Some(GraphVertexMergeOutcome::Created(VId(10))));
        assert_eq!(receipt.steps()[1].merged_vertex(), Some(GraphVertexMergeOutcome::Created(VId(11))));
        assert_eq!(receipt.steps()[2].merged_edge(), Some(GraphEdgeMergeOutcome::Created(EId(12))));
        assert_eq!(receipt.steps()[3].merged_edge(), Some(GraphEdgeMergeOutcome::Matched(EId(12))));
        assert_eq!(receipt.steps()[2].created_edges(), Some(&[EId(12)][..]));
        assert_eq!(receipt.steps()[3].created_edges(), Some(&[][..]));
        assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(), EmbeddedTxnCompletion::WriteCommitted { .. }));
        let edge = db.edge(EId(12)).unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.dst, edge.entry.relation), (VId(10), VId(11), R));

        let frontier = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn.execute_graph_write_program_returning_governed(
            &mut db, &query, &program, policy(0, 0),
            |_| -> Result<ElementId, ()> { panic!("idempotent ingestion must not allocate") },
        ).unwrap();
        assert_eq!((receipt.stats().created_vertices, receipt.stats().created_edges), (0, 0));
        assert_eq!(receipt.steps()[0].merged_vertex(), Some(GraphVertexMergeOutcome::Matched(VId(10))));
        assert_eq!(receipt.steps()[1].merged_vertex(), Some(GraphVertexMergeOutcome::Matched(VId(11))));
        for step in &receipt.steps()[2..] {
            assert_eq!(step.merged_edge(), Some(GraphEdgeMergeOutcome::Matched(EId(12))));
        }
        assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(), EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relationship_quota_refusal_rolls_back_both_new_vertices_and_preserves_outer_prefix() {
    let ((), report) = run_async_under_lab(0xed9e_4002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(200), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let mut allocations = 0;
        let error = txn.execute_graph_write_program_returning_governed(
            &mut db, &query, &program(), policy(2, 0), |request| {
                assert!(matches!(request.request, GraphInsertRequest::Vertex { .. }),
                    "edge quota is checked before requesting an edge identity");
                allocations += 1;
                Ok::<_, ()>(ElementId::Vertex(VId(10 + request.statement as u128)))
            },
        ).unwrap_err();
        assert!(matches!(error, GraphWriteProgramError::CreationBudget {
            statement: 2, dimension: GraphInsertLimitDimension::Edges, limit: 0, observed: 1,
        }));
        assert_eq!(allocations, 2);
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(200)).unwrap().is_some());
        assert!(db.vertex(VId(10)).unwrap().is_none());
        assert!(db.vertex(VId(11)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_endpoint_input_has_an_explicit_receipt_and_does_not_stop_later_statements() {
    let ((), report) = run_async_under_lab(0xed9e_4003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let edge = PreparedGraphEdgeMergeText::prepare(EDGE_TEXT, R, symbols).unwrap();
        let vertex = PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$left})", R, symbols).unwrap();
        let program = PreparedGraphWriteProgramTemplate::prepare(vec![edge.into(), vertex.into()])
            .unwrap().bind_parameters(&arguments()).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn.execute_graph_write_program_returning_governed(
            &mut db, &query, &program, policy(1, 0), |request| {
                assert_eq!(request.statement, 1);
                assert_eq!(request.request, GraphInsertRequest::Vertex { row: 0, vertex: 0 });
                Ok::<_, ()>(ElementId::Vertex(VId(10)))
            },
        ).unwrap();
        assert_eq!(receipt.steps()[0].merged_edge(), Some(GraphEdgeMergeOutcome::NoInput));
        assert_eq!(receipt.steps()[0].created_edges(), Some(&[][..]));
        assert_eq!(receipt.steps()[1].merged_vertex(), Some(GraphVertexMergeOutcome::Created(VId(10))));
        assert_eq!((receipt.stats().created_vertices, receipt.stats().created_edges), (1, 0));
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(10)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relationship_allocator_failure_retains_statement_index_and_rolls_back_graph_prefix() {
    let ((), report) = run_async_under_lab(0xed9e_4004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let error = txn.execute_graph_write_program_governed(
            &mut db, &query, &program(), policy(2, 1), |request| {
                match request.request {
                    GraphInsertRequest::Vertex { .. } => Ok(ElementId::Vertex(VId(10 + request.statement as u128))),
                    GraphInsertRequest::Edge { .. } => Err("allocator-down"),
                }
            },
        ).unwrap_err();
        assert!(matches!(error, GraphWriteProgramError::EdgeMerge {
            statement: 2, source: GqlQueryError::Source(GraphEdgeMergeError::IdentitySource("allocator-down")),
        }));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        txn.abort();
        assert!(db.vertex(VId(10)).unwrap().is_none());
        assert!(db.vertex(VId(11)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
