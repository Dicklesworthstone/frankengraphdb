use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn undirected_execute_gql_certified_rows_and_digest() {
    let ((), report) = run_async_under_lab(0x9a_03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-certified-{}",
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
        database.write(&commit_cx, seed).await.expect("seed edge");

        let bind = RelationBind::new().with_relation("R", relation);
        let undirected = "MATCH (a)-[:R]-(b) RETURN b";
        let directed = "MATCH (a)-[:R]->(b) RETURN b";
        let transaction = database.begin(&txn_cx).expect("begin transaction");
        let (undirected_rows, undirected_cert) = transaction
            .execute_gql_certified(&database, undirected, &bind)
            .expect("certified undirected MATCH");
        let plan_cert = transaction
            .gql_plan_certificate(undirected, &bind)
            .expect("undirected plan certificate");
        let (repeat_rows, repeat_cert) = transaction
            .execute_gql_certified(&database, undirected, &bind)
            .expect("repeat certified undirected MATCH");
        let (directed_rows, directed_cert) = transaction
            .execute_gql_certified(&database, directed, &bind)
            .expect("certified directed MATCH");

        assert_eq!(undirected_rows, vec![VId(1), VId(2)]);
        assert_eq!(undirected_cert.digest, plan_cert.digest);
        assert_eq!(repeat_rows, undirected_rows);
        assert_eq!(repeat_cert, undirected_cert);
        assert_eq!(directed_rows, vec![VId(2)]);
        assert_ne!(directed_cert.digest, undirected_cert.digest);
        transaction.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
