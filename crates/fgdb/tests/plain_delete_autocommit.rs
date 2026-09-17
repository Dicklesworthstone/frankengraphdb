//! Autocommit plain DELETE must preserve the same non-detach law and release
//! transaction obligations on success, empty selection and refusal.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphDeleteError, GraphDeletePolicy, GraphSymbol,
    GraphSymbolKind, PreparedGraphDelete, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xe1; 32],
        DatabaseSecurityNamespaceId([0xe2; 32]),
        [0xe3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn deletion(value: i64) -> PreparedGraphDelete {
    let text = format!("MATCH (n) WHERE n.p = {value} RETURN ALL n");
    let selection = PreparedGraphText::prepare(&text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    PreparedGraphDelete::prepare(selection, R, vec![0]).unwrap()
}
fn policy() -> GraphDeletePolicy {
    GraphDeletePolicy::new(GqlQueryPolicy::new(1_000, 1_000, 500_000, 500_000), 10)
}

#[test]
fn autocommit_delete_distinguishes_commit_read_close_and_incident_refusal() {
    let ((), report) = run_async_under_lab(0xd31e_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let baseline = txcx.outstanding_obligations();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(2))]);
        seed.create_vertex(VId(3), vec![], vec![(P, CanonicalScalar::Int(3))]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();

        let before = db.frontier().unwrap();
        let (stats, targets, completion) = db
            .execute_graph_delete_returning_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &deletion(3),
                policy(),
            )
            .await
            .unwrap();
        assert_eq!(stats.target_vertices, 1);
        assert_eq!(targets, vec![VId(3)]);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert!(db.vertex(VId(3)).unwrap().is_none());
        assert!(db.frontier().unwrap().0 > before.0);
        assert_eq!(txcx.outstanding_obligations(), baseline);

        let after_delete = db.frontier().unwrap();
        let (stats, completion) = db
            .execute_graph_delete_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &deletion(99),
                policy(),
            )
            .await
            .unwrap();
        assert_eq!(stats.target_vertices, 0);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(
            db.frontier().unwrap(),
            after_delete,
            "empty DELETE must not allocate a commit sequence"
        );
        assert_eq!(txcx.outstanding_obligations(), baseline);

        assert!(matches!(
            db.execute_graph_delete_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &deletion(1),
                policy(),
            )
            .await,
            Err(GqlQueryError::Source(
                GraphDeleteError::IncidentRelationships
            ))
        ));
        assert!(db.vertex(VId(1)).unwrap().is_some());
        assert!(db.edge(EId(10)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap(), after_delete);
        assert_eq!(txcx.outstanding_obligations(), baseline);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
