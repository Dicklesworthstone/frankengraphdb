//! Autocommit unique-vertex MERGE must complete a match without a marker, publish
//! a create with one marker, and release its private transaction on ambiguity.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertVertex, PreparedGraphInsert};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GqlScalarParameter, GraphMutationValue,
    GraphSymbol, GraphSymbolKind, GraphVertexMergeError, GraphVertexMergeOutcome,
    GraphVertexMergePolicy, PreparedGraphText, PreparedGraphVertexMerge,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion,
    PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa1; 32], DatabaseSecurityNamespaceId([0xa2; 32]), [0xa3; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn definition(value: i64) -> PreparedGraphVertexMerge {
    let text = format!("MATCH (n) WHERE n.p = {value} RETURN ALL n");
    let selection = PreparedGraphText::prepare(&text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap();
    let create = PreparedGraphInsert::prepare_standalone(
        R,
        vec![GraphInsertVertex {
            labels: vec![],
            properties: vec![(P, GraphMutationValue::Literal(
                GqlScalarParameter::new(CanonicalScalar::Int(value)).unwrap(),
            ))],
        }],
        vec![],
    ).unwrap();
    PreparedGraphVertexMerge::prepare(selection, R, 0, create).unwrap()
}
fn policy() -> GraphVertexMergePolicy {
    GraphVertexMergePolicy::new(GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000))
}

#[test]
fn autocommit_merge_uses_read_close_for_match_and_write_commit_for_create() {
    let ((), report) = run_async_under_lab(0x6e29_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let baseline = txcx.outstanding_obligations();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();

        let before_match = db.frontier().unwrap();
        let (stats, outcome, completion) = db.execute_graph_vertex_merge_autocommit_governed(
            &txcx, &query, &commit, &definition(7), policy(),
            |_| -> Result<ElementId, ()> { panic!("matched MERGE must not allocate") },
        ).await.unwrap();
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
        assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(db.frontier().unwrap(), before_match);
        assert_eq!(txcx.outstanding_obligations(), baseline);

        let (stats, outcome, completion) = db.execute_graph_vertex_merge_autocommit_governed(
            &txcx, &query, &commit, &definition(9), policy(),
            |_| Ok::<_, ()>(ElementId::Vertex(VId(9))),
        ).await.unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(outcome, GraphVertexMergeOutcome::Created(VId(9)));
        assert!(matches!(completion, EmbeddedTxnCompletion::WriteCommitted { .. }));
        assert_eq!(db.vertex(VId(9)).unwrap().unwrap().props, vec![(P, CanonicalScalar::Int(9))]);
        assert_eq!(txcx.outstanding_obligations(), baseline);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn autocommit_ambiguous_merge_aborts_without_marker_or_obligation_leak() {
    let ((), report) = run_async_under_lab(0x6e29_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let baseline = txcx.outstanding_obligations();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(7))]);
        seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let result = db.execute_graph_vertex_merge_autocommit_governed(
            &txcx, &query, &commit, &definition(7), policy(),
            |_| Ok::<_, ()>(ElementId::Vertex(VId(100))),
        ).await;
        assert!(matches!(result,
            Err(GqlQueryError::Source(GraphVertexMergeError::AmbiguousMatches { observed: 2 }))));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertex(VId(100)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), baseline);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
