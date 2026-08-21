use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn match_overlay_keeps_destination_while_parallel_eid_remains() {
    let ((), report) = run_async_under_lab(0x90_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-write-txn-gql-parallel-eid-{}",
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
            .expect("create product database");
        let mut seed = WriteBatch::new(relation);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        seed.add_edge(EId(2), VId(1), VId(2), vec![]);
        database
            .write(&commit_cx, seed)
            .await
            .expect("seed parallel edges");

        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut delete = WriteBatch::new(relation);
        delete.delete_edge(EId(1));
        transaction
            .write(&mut database, delete)
            .expect("stage one eid deletion");
        let bind = RelationBind::new().with_relation("R", relation);
        assert_eq!(
            transaction
                .execute_gql(
                    &database,
                    "MATCH (a)-[:R]->(b) RETURN b",
                    &bind,
                )
                .expect("overlay MATCH executes"),
            vec![VId(2)]
        );
        transaction.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
