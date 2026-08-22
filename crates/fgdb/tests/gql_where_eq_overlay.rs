use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn equality_match_sees_only_the_staged_self_loop_in_the_overlay() {
    let ((), report) = run_async_under_lab(0x4e_04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir =
            std::env::temp_dir().join(format!("fgdb-where-eq-overlay-{}", std::process::id()));
        let relation = RelationId(1);
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
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let mut txn = db.begin(&txn_cx).expect("transaction begins");
        let mut staged = WriteBatch::new(relation);
        staged.create_vertex(VId(7), vec![], vec![]);
        staged.add_edge(EId(11), VId(7), VId(7), vec![]);
        txn.write(&mut db, staged).expect("self-loop stages");

        let bind = RelationBind::new().with_relation("R", relation);
        let equal = "MATCH (a)-[:R]->(b) WHERE a = b RETURN b";
        let plain = "MATCH (a)-[:R]->(b) RETURN b";
        let overlay = txn
            .execute_gql(&db, equal, &bind)
            .expect("overlay equality MATCH executes");
        assert_eq!(overlay, vec![VId(7)]);
        assert!(!overlay.contains(&VId(2)));

        let base = db
            .execute_gql(equal, &bind)
            .expect("base equality MATCH executes");
        assert!(
            !base.contains(&VId(7)),
            "staged loop leaked into shared database"
        );
        let unfiltered = txn
            .execute_gql(&db, plain, &bind)
            .expect("unfiltered overlay MATCH executes");
        assert!(unfiltered.contains(&VId(2)));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
