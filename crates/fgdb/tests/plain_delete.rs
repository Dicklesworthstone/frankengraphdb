//! Plain DELETE must never inherit DETACH DELETE's cascade semantics. These laws
//! exercise the canonical staged overlay, real commit path and FCW validation.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphDeleteError, GraphDeletePolicy, GraphSymbol,
    GraphSymbolKind, PreparedGraphDelete, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GraphDeletePolicy {
    GraphDeletePolicy::new(
        GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 2_000_000),
        100,
    )
}
fn deletion(value: i64) -> PreparedGraphDelete {
    let text = format!("MATCH (n) WHERE n.p = {value} RETURN ALL n");
    let selected = PreparedGraphText::prepare(&text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    PreparedGraphDelete::prepare(selected, R, vec![0]).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=6_u128 {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    db.write(cx, batch).await.unwrap();
}

#[test]
fn attached_vertex_refuses_but_prior_edge_delete_makes_plain_delete_legal() {
    let ((), report) = run_async_under_lab(0xd31e_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;

        let mut refused = db.begin(&txcx).unwrap();
        let before = refused.staged_effect_digest().unwrap();
        assert!(matches!(
            refused.execute_graph_delete_governed(&mut db, &query, &deletion(1), policy()),
            Err(GqlQueryError::Source(
                GraphDeleteError::IncidentRelationships
            ))
        ));
        assert_eq!(refused.staged_effect_digest().unwrap(), before);
        assert!(refused.vertex(&db, VId(1)).unwrap().is_some());
        refused.abort();

        let mut txn = db.begin(&txcx).unwrap();
        let mut unlink = WriteBatch::new(R);
        unlink.delete_edge(EId(10));
        txn.write(&mut db, unlink).unwrap();
        assert!(txn.edge(&db, EId(10)).unwrap().is_none());
        let (stats, targets) = txn
            .execute_graph_delete_returning_governed(&mut db, &query, &deletion(1), policy())
            .unwrap();
        assert_eq!(stats.target_vertices, 1);
        assert_eq!(targets, vec![VId(1)]);
        assert!(txn.vertex(&db, VId(1)).unwrap().is_none());
        assert!(
            db.vertex(VId(1)).unwrap().is_some(),
            "staged DELETE is not durable yet"
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.edge(EId(10)).unwrap().is_none());
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn earlier_staged_edge_creation_is_visible_and_refusal_preserves_it() {
    let ((), report) = run_async_under_lab(0xd31e_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;

        let mut txn = db.begin(&txcx).unwrap();
        let mut prior = WriteBatch::new(R);
        prior.add_edge(EId(30), VId(4), VId(3), vec![]);
        txn.write(&mut db, prior).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        assert!(matches!(
            txn.execute_graph_delete_returning_governed(&mut db, &query, &deletion(3), policy()),
            Err(GqlQueryError::Source(
                GraphDeleteError::IncidentRelationships
            ))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert!(txn.vertex(&db, VId(3)).unwrap().is_some());
        assert!(txn.edge(&db, EId(30)).unwrap().is_some());
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(3)).unwrap().is_some());
        assert!(db.edge(EId(30)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn concurrent_incident_edge_after_precheck_invalidates_completion() {
    let ((), report) = run_async_under_lab(0xd31e_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;

        let mut deleting = db.begin(&txcx).unwrap();
        deleting
            .execute_graph_delete_governed(&mut db, &query, &deletion(5), policy())
            .unwrap();
        assert!(deleting.vertex(&db, VId(5)).unwrap().is_none());

        let mut winner = db.begin(&txcx).unwrap();
        let mut edge = WriteBatch::new(R);
        edge.add_edge(EId(60), VId(6), VId(5), vec![]);
        winner.write(&mut db, edge).unwrap();
        winner.commit(&mut db, &commit).await.unwrap();

        assert!(
            deleting.commit(&mut db, &commit).await.is_err(),
            "the edge-scan witness must reject a topology change after plain DELETE admission"
        );
        assert!(db.vertex(VId(5)).unwrap().is_some());
        assert!(db.edge(EId(60)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn successful_plain_delete_survives_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xd31e_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();

        let mut txn = db.begin(&txcx).unwrap();
        txn.execute_graph_delete_governed(&mut db, &query, &deletion(6), policy())
            .unwrap();
        let seq = txn.commit(&mut db, &commit).await.unwrap();
        assert!(seq.0 > before.0);
        assert!(db.vertex(VId(6)).unwrap().is_none());
        assert!(pinned.vertex(VId(6)).unwrap().is_some());
        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);

        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(db.vertex(VId(6)).unwrap().is_none());
        assert!(db.vertex_at(VId(6), before).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
