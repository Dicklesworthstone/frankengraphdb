//! Autocommit directed relationship MERGE lifecycle laws.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GqlScalarParameter, GraphEdgeMergeError,
    GraphEdgeMergeOutcome, GraphEdgeMergePolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeMerge, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn definition(source: i64, destination: i64) -> PreparedGraphEdgeMerge {
    let text = format!("MATCH (a),(b) WHERE a.p={source} AND b.p={destination} RETURN ALL a,b");
    let selection = PreparedGraphText::prepare(&text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    PreparedGraphEdgeMerge::prepare(
        selection,
        R,
        0,
        1,
        vec![(
            PropertyKeyId(2),
            GqlScalarParameter::new(CanonicalScalar::Int(5)).unwrap(),
        )],
    )
    .unwrap()
}
fn policy() -> GraphEdgeMergePolicy {
    GraphEdgeMergePolicy::new(GqlQueryPolicy::new(5_000, 5_000, 2_000_000, 2_000_000))
}

#[test]
fn autocommit_relationship_merge_read_closes_match_and_commits_create() {
    let ((), report) = run_async_under_lab(0xed9e_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let baseline = txcx.outstanding_obligations();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=3_u128 {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();

        let before = db.frontier().unwrap();
        let (stats, outcome, completion) = db
            .execute_graph_edge_merge_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &definition(1, 2),
                policy(),
                |_| -> Result<ElementId, ()> { panic!("existing edge must not allocate") },
            )
            .await
            .unwrap();
        assert_eq!(stats.created_edges, 0);
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(10)));
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txcx.outstanding_obligations(), baseline);

        let (stats, outcome, completion) = db
            .execute_graph_edge_merge_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &definition(2, 3),
                policy(),
                |_| Ok::<_, ()>(ElementId::Edge(EId(20))),
            )
            .await
            .unwrap();
        assert_eq!(stats.created_edges, 1);
        assert_eq!(outcome, GraphEdgeMergeOutcome::Created(EId(20)));
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        let edge = db.edge(EId(20)).unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.dst), (VId(2), VId(3)));
        assert_eq!(txcx.outstanding_obligations(), baseline);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn autocommit_parallel_ambiguity_aborts_without_new_commit() {
    let ((), report) = run_async_under_lab(0xed9e_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let baseline = txcx.outstanding_obligations();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(2))]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();

        let result = db
            .execute_graph_edge_merge_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &definition(1, 2),
                policy(),
                |_| Ok::<_, ()>(ElementId::Edge(EId(20))),
            )
            .await;
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(
                GraphEdgeMergeError::AmbiguousRelationships { observed: 2 }
            ))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.edge(EId(20)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), baseline);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
