use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::VId;

#[test]
fn node_only_skip_runs_after_the_staged_overlay_is_sorted() {
    let ((), report) = run_async_under_lab(0x47_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-only-skip-overlay-{}",
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
        seed.create_vertex(VId(2), vec![person], vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let mut txn = db.begin(&txn_cx).expect("transaction begins");
        let mut staged = WriteBatch::new(relation);
        staged.create_vertex(VId(4), vec![person], vec![]);
        txn.write(&mut db, staged).expect("Person isolate stages");

        let bind = RelationBind::new()
            .with_label("Person", person)
            .with_relation("R", relation);
        let statement = "MATCH (a:Person) RETURN a SKIP 1";
        assert_eq!(
            txn.execute_gql(&db, statement, &bind)
                .expect("overlay SKIP executes"),
            vec![VId(2), VId(4)]
        );
        assert_eq!(
            db.execute_gql(statement, &bind)
                .expect("base SKIP executes"),
            vec![VId(2)]
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
