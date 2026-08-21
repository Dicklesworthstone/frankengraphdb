use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn node_only_match_sees_staged_labeled_isolates_without_dirty_reads() {
    let ((), report) = run_async_under_lab(0x40_04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-only-overlay-{}",
            std::process::id()
        ));
        let relation = RelationId(1);
        let person = LabelId(3);
        let mut db = Database::create(
            &commit,
            &dir,
            DatabaseKeys::new(
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
                [0x3c; 32],
            ),
        )
        .await
        .expect("database creates");
        let mut seed = WriteBatch::new(relation);
        seed.create_vertex(VId(1), vec![person], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.add_edge(EId(10), VId(2), VId(3), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let mut txn = db.begin(&txn_cx).expect("transaction begins");
        let mut staged = WriteBatch::new(relation);
        staged.create_vertex(VId(9), vec![person], vec![]);
        txn.write(&mut db, staged).expect("Person isolate stages");

        let bind = RelationBind::new()
            .with_label("Person", person)
            .with_relation("R", relation);
        let node_only = "MATCH (a:Person) RETURN a";
        let overlay = txn
            .execute_gql(&db, node_only, &bind)
            .expect("overlay node-only MATCH executes");
        assert_eq!(overlay, vec![VId(1), VId(9)]);
        assert!(!overlay.contains(&VId(2)));

        let base = db
            .execute_gql(node_only, &bind)
            .expect("base node-only MATCH executes");
        assert_eq!(base, vec![VId(1)]);
        assert!(!base.contains(&VId(9)), "staged isolate leaked into shared database");

        let edge = "MATCH (a)-[:R]->(b) RETURN b";
        assert_eq!(txn.execute_gql(&db, edge, &bind).expect("overlay edge MATCH"), vec![VId(3)]);
        assert_eq!(db.execute_gql(edge, &bind).expect("base edge MATCH"), vec![VId(3)]);
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
