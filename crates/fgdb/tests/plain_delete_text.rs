//! Native text DELETE must bind directly into the same non-detaching transaction adapter.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphDeleteError, GraphDeletePolicy, GraphSymbol,
    GraphSymbolKind, PreparedGraphDeleteText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GraphDeletePolicy {
    GraphDeletePolicy::new(GqlQueryPolicy::new(5_000, 5_000, 1_000_000, 1_000_000), 100)
}

#[test]
fn parameterized_native_delete_refuses_attached_then_succeeds_after_staged_unlink() {
    let ((), report) = run_async_under_lab(0xd31e_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(10))]);
        seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(20))]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();

        let template =
            PreparedGraphDeleteText::prepare("MATCH (n) WHERE n.p = $value DELETE n", R, symbols)
                .unwrap();
        let delete = template
            .bind_parameters(&GqlParameters::new().with_int64("value", 10).unwrap())
            .unwrap();

        let mut refused = db.begin(&txcx).unwrap();
        assert!(matches!(
            refused.execute_graph_delete_governed(&mut db, &query, &delete, policy()),
            Err(GqlQueryError::Source(
                GraphDeleteError::IncidentRelationships
            ))
        ));
        refused.abort();

        let mut txn = db.begin(&txcx).unwrap();
        let mut unlink = WriteBatch::new(R);
        unlink.delete_edge(EId(10));
        txn.write(&mut db, unlink).unwrap();
        let (stats, targets) = txn
            .execute_graph_delete_returning_governed(&mut db, &query, &delete, policy())
            .unwrap();
        assert_eq!(stats.target_vertices, 1);
        assert_eq!(targets, vec![VId(1)]);
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.edge(EId(10)).unwrap().is_none());
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
