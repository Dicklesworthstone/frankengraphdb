use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn undirected_two_hop_overlay_sees_staged_composed_incident() {
    let ((), report) = run_async_under_lab(0x9c_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-two-hop-overlay-{}",
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
        for vid in [1, 2, 4, 5, 8, 9].map(VId) {
            first.create_vertex(vid, vec![], vec![]);
        }
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        database
            .write(&commit_cx, first)
            .await
            .expect("seed R edge");
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
        let statement = "MATCH (a)-[:R]-(b)-[:S]-(c) RETURN c";
        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut staged = WriteBatch::new(s);
        staged.add_edge(EId(22), VId(2), VId(5), vec![]);
        transaction
            .write(&mut database, staged)
            .expect("stage composed S edge");
        let overlay = transaction
            .execute_gql(&database, statement, &bind)
            .expect("overlay two-hop MATCH");
        assert!(overlay.contains(&VId(4)) && overlay.contains(&VId(5)));
        assert!(!overlay.contains(&VId(8)));

        let durable = database
            .execute_gql(statement, &bind)
            .expect("durable two-hop MATCH");
        assert!(durable.contains(&VId(4)));
        assert!(!durable.contains(&VId(5)) && !durable.contains(&VId(8)));
        transaction.abort();
        let after_abort = database
            .execute_gql(statement, &bind)
            .expect("MATCH after abort");
        assert!(after_abort.contains(&VId(4)));
        assert!(!after_abort.contains(&VId(5)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
