//! Relationship MERGE creation admission must not reject existing matches or
//! empty MATCH inputs and must refuse a missing edge before allocating its ID.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergeError, GraphEdgeMergeOutcome,
    GraphEdgeMergePolicy, GraphSymbol, GraphSymbolKind, PreparedGraphEdgeMerge,
    PreparedGraphEdgeMergeText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn merge(left: i64, right: i64) -> PreparedGraphEdgeMerge {
    PreparedGraphEdgeMergeText::prepare(
        "MATCH (a),(b) WHERE a.p=$left AND b.p=$right MERGE (a)-[:R]->(b)",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(
        &GqlParameters::new()
            .with_int64("left", left)
            .unwrap()
            .with_int64("right", right)
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn zero_edge_quota_preserves_matches_and_no_input_but_refuses_new_allocation() {
    let ((), report) = run_async_under_lab(0xed9e_3001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let keys = DatabaseKeys::new(
            [0x71; 32],
            DatabaseSecurityNamespaceId([0x72; 32]),
            [0x73; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=3_u128 {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();
        let policy = GraphEdgeMergePolicy::new(GqlQueryPolicy::new(
            100_000, 100_000, 10_000_000, 10_000_000,
        ))
        .with_creation_limit(0);
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let (_, matched) = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &merge(1, 2),
                policy,
                |_| -> Result<ElementId, ()> { panic!("existing edge must not allocate") },
            )
            .unwrap();
        assert_eq!(matched, GraphEdgeMergeOutcome::Matched(EId(10)));
        let (_, missing_endpoint) = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &merge(1, 99),
                policy,
                |_| -> Result<ElementId, ()> { panic!("NoInput must not allocate") },
            )
            .unwrap();
        assert_eq!(missing_endpoint, GraphEdgeMergeOutcome::NoInput);
        let error = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &merge(1, 3),
                policy,
                |_| -> Result<ElementId, ()> {
                    panic!("exhausted quota must refuse before allocation")
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(GraphEdgeMergeError::CreationLimit {
                limit: 0,
                observed: 1,
            })
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        let (stats, created) = txn
            .execute_graph_edge_merge_governed(
                &mut db,
                &query,
                &merge(1, 3),
                policy.with_creation_limit(1),
                |_| Ok::<_, ()>(ElementId::Edge(EId(11))),
            )
            .unwrap();
        assert_eq!(created, GraphEdgeMergeOutcome::Created(EId(11)));
        assert_eq!(stats.created_edges, 1);
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.edge(EId(11)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
