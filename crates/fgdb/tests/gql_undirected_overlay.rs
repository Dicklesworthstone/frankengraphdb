use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn undirected_match_overlay_sees_staged_incident_vertex() {
    let ((), report) = run_async_under_lab(0x9a_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-overlay-{}",
            std::process::id()
        ));
        let keys = DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        );
        let relation = RelationId(1);
        let mut database = Database::create(&commit_cx, &dir, keys)
            .await
            .expect("create database");
        let mut seed = WriteBatch::new(relation);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        database.write(&commit_cx, seed).await.expect("seed durable edge");

        let bind = RelationBind::new().with_relation("R", relation);
        let undirected = "MATCH (a)-[:R]-(b) RETURN b";
        let directed = "MATCH (a)-[:R]->(b) RETURN b";
        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut staged = WriteBatch::new(relation);
        staged.create_vertex(VId(3), vec![], vec![]);
        staged.add_edge(EId(11), VId(3), VId(2), vec![]);
        transaction
            .write(&mut database, staged)
            .expect("stage incident edge");
        assert_eq!(
            transaction
                .execute_gql(&database, undirected, &bind)
                .expect("overlay undirected MATCH"),
            vec![VId(1), VId(2), VId(3)]
        );
        assert_eq!(
            database.execute_gql(undirected, &bind).expect("durable undirected MATCH"),
            vec![VId(1), VId(2)]
        );
        assert_eq!(
            transaction
                .execute_gql(&database, directed, &bind)
                .expect("overlay directed MATCH"),
            vec![VId(2)]
        );
        transaction.abort();
        assert_eq!(
            database.execute_gql(undirected, &bind).expect("MATCH after abort"),
            vec![VId(1), VId(2)]
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
