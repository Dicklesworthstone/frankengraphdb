use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn match_overlay_ignores_off_relation_staged_edge() {
    let ((), report) = run_async_under_lab(0x8f_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-write-txn-gql-off-relation-{}",
            std::process::id()
        ));
        let keys = DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        );
        let mut database = Database::create(&commit_cx, &dir, keys)
            .await
            .expect("create product database");
        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut staged = WriteBatch::new(RelationId(2));
        staged.create_vertex(VId(1), vec![], vec![]);
        staged.create_vertex(VId(9), vec![], vec![]);
        staged.add_edge(EId(1), VId(1), VId(9), vec![]);
        transaction
            .write(&mut database, staged)
            .expect("stage OTHER edge");

        let bind = RelationBind::new().with_relation("R", RelationId(1));
        let statement = "MATCH (a)-[:R]->(b) RETURN b";
        assert!(
            transaction
                .execute_gql(&database, statement, &bind)
                .expect("overlay MATCH executes")
                .is_empty()
        );
        assert!(
            database
                .execute_gql(statement, &bind)
                .expect("base MATCH executes")
                .is_empty()
        );
        transaction.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
