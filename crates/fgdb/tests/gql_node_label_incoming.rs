use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn labeled_incoming_return_a_is_the_person_in_holder() {
    let ((), report) = run_async_under_lab(0x9f_02, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-node-label-incoming-{}",
            std::process::id()
        ));
        let keys = DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        );
        let relation = RelationId(1);
        let person = LabelId(7);
        let mut database = Database::create(&commit_cx, &dir, keys)
            .await
            .expect("create database");
        let mut seed = WriteBatch::new(relation);
        seed.create_vertex(VId(1), vec![person], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.create_vertex(VId(4), vec![], vec![]);
        seed.add_edge(EId(10), VId(2), VId(1), vec![]);
        seed.add_edge(EId(11), VId(4), VId(3), vec![]);
        database.write(&commit_cx, seed).await.expect("seed incoming edges");

        let bind = RelationBind::new()
            .with_relation("R", relation)
            .with_label("Person", person);
        let labeled = database
            .execute_gql("MATCH (a:Person)<-[:R]-(b) RETURN a", &bind)
            .expect("labeled incoming MATCH");
        let unlabeled = database
            .execute_gql("MATCH (a)<-[:R]-(b) RETURN a", &bind)
            .expect("unlabeled incoming MATCH");
        assert!(labeled.contains(&VId(1)));
        assert!(!labeled.contains(&VId(3)));
        assert_eq!(unlabeled, vec![VId(1), VId(3)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
