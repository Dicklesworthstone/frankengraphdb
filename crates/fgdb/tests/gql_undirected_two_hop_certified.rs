use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn undirected_two_hop_execute_gql_certified_rows_and_digest() {
    let ((), report) = run_async_under_lab(0x9c_03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-two-hop-certified-{}",
            std::process::id()
        ));
        let keys = DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        );
        let r = RelationId(1);
        let s = RelationId(2);
        let mut database = Database::create(&commit_cx, &dir, keys)
            .await
            .expect("create database");
        let mut first = WriteBatch::new(r);
        for vid in [1, 2, 3, 4, 8, 9].map(VId) {
            first.create_vertex(vid, vec![], vec![]);
        }
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        first.add_edge(EId(11), VId(3), VId(2), vec![]);
        database
            .write(&commit_cx, first)
            .await
            .expect("seed R edges");
        let mut second = WriteBatch::new(s);
        second.add_edge(EId(20), VId(2), VId(4), vec![]);
        second.add_edge(EId(21), VId(9), VId(8), vec![]);
        database
            .write(&commit_cx, second)
            .await
            .expect("seed S edges");

        let bind = RelationBind::new()
            .with_relation("R", r)
            .with_relation("S", s);
        let undirected = "MATCH (a)-[:R]-(b)-[:S]-(c) RETURN c";
        let directed = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
        let transaction = database.begin(&txn_cx).expect("begin transaction");
        let (undirected_rows, undirected_cert) = transaction
            .execute_gql_certified(&database, undirected, &bind)
            .expect("certified undirected two-hop MATCH");
        let plan_cert = transaction
            .gql_plan_certificate(undirected, &bind)
            .expect("undirected plan certificate");
        let (repeat_rows, repeat_cert) = transaction
            .execute_gql_certified(&database, undirected, &bind)
            .expect("repeat certified undirected MATCH");
        let (directed_rows, directed_cert) = transaction
            .execute_gql_certified(&database, directed, &bind)
            .expect("certified directed two-hop MATCH");

        assert!(undirected_rows.contains(&VId(4)));
        assert!(!undirected_rows.contains(&VId(8)));
        assert_eq!(undirected_cert.digest, plan_cert.digest);
        assert_eq!(repeat_rows, undirected_rows);
        assert_eq!(repeat_cert, undirected_cert);
        assert_eq!(directed_rows, vec![VId(4)]);
        assert_ne!(directed_cert.digest, undirected_cert.digest);
        transaction.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
