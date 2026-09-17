//! Creation admission for MERGE is branch-sensitive and precedes allocation.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertError, GraphInsertLimitDimension};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeError, GraphVertexMergeOutcome, GraphVertexMergePolicy,
    PreparedGraphInsertText, PreparedGraphText, PreparedGraphVertexMerge,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn merge(value: i64) -> PreparedGraphVertexMerge {
    let arguments = GqlParameters::new().with_int64("value", value).unwrap();
    let selection = PreparedGraphText::prepare("MATCH (n) WHERE n.p=$value RETURN n", symbols)
        .unwrap()
        .bind_parameters(&arguments)
        .unwrap();
    let creation = PreparedGraphInsertText::prepare("CREATE (n {p:$value})", R, symbols)
        .unwrap()
        .bind_parameters(&arguments)
        .unwrap();
    PreparedGraphVertexMerge::prepare(selection, R, 0, creation).unwrap()
}

#[test]
fn zero_creation_budget_allows_matches_and_refuses_missing_before_allocation() {
    let ((), report) = run_async_under_lab(0x6e29_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let keys = DatabaseKeys::new(
            [0x31; 32],
            DatabaseSecurityNamespaceId([0x32; 32]),
            [0x33; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
        db.write(&commit, seed).await.unwrap();
        let policy =
            GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 2_000_000))
                .with_creation_limit(0);
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let (stats, outcome) = txn
            .execute_graph_vertex_merge_governed(
                &mut db,
                &query,
                &merge(1),
                policy,
                |_| -> Result<ElementId, ()> { panic!("matching must not allocate") },
            )
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
        assert_eq!(stats.created_vertices, 0);
        let error = txn
            .execute_graph_vertex_merge_governed(
                &mut db,
                &query,
                &merge(2),
                policy,
                |_| -> Result<ElementId, ()> {
                    panic!("exhausted creation quota must not allocate")
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(GraphVertexMergeError::Creation(GraphInsertError::Limit {
                dimension: GraphInsertLimitDimension::Vertices,
                limit: 0,
                observed: 1,
            }))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        let (stats, outcome) = txn
            .execute_graph_vertex_merge_governed(
                &mut db,
                &query,
                &merge(2),
                policy.with_creation_limit(1),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(2))),
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(outcome, GraphVertexMergeOutcome::Created(VId(2)));
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
