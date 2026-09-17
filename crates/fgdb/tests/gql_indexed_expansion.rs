use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}

#[test]
fn pinned_snapshot_traversal_survives_successor_publication() {
    let ((), report) = run_async_under_lab(0xa26_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        first.add_edge(EId(1), VId(1), VId(2), vec![]);
        let at = db.write(&commit, first).await.unwrap();
        let pinned = db.read_session().unwrap();
        let mut second = WriteBatch::new(R);
        second.create_vertex(VId(3), vec![], vec![]);
        second.add_edge(EId(2), VId(1), VId(3), vec![]);
        db.write(&commit, second).await.unwrap();
        let names = RelationBind::new().with_relation("R", R);
        let query = "MATCH (a)-[:R]->(b) RETURN b";
        assert_eq!(pinned.execute_gql(query, &names).unwrap(), vec![VId(2)]);
        assert_eq!(db.execute_gql_at(query, &names, at).unwrap(), vec![VId(2)]);
        assert_eq!(db.execute_gql(query, &names).unwrap(), vec![VId(2), VId(3)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
