use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn labeled_match_as_of_pins_labeled_sources() {
    let ((), report) = run_async_under_lab(0x9f_01, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-node-label-as-of-{}", std::process::id()));
        let keys = DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        );
        let relation = RelationId(1);
        let label = LabelId(7);
        let mut database = Database::create(&commit_cx, &dir, keys)
            .await
            .expect("create database");
        let mut seed = WriteBatch::new(relation);
        seed.create_vertex(VId(1), vec![label], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.create_vertex(VId(4), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(2), vec![]);
        seed.add_edge(EId(12), VId(3), VId(4), vec![]);
        database
            .write(&commit_cx, seed)
            .await
            .expect("seed S1 graph");
        let s1 = database.frontier().expect("capture S1");

        let mut later = WriteBatch::new(relation);
        later.set_vertex_label(VId(3), label, true);
        database
            .write(&commit_cx, later)
            .await
            .expect("label source 3");
        let bind = RelationBind::new()
            .with_relation("R", relation)
            .with_label("L", label);
        let statement = "MATCH (a:L)-[:R]->(b) RETURN b";
        let unlabeled = "MATCH (a)-[:R]->(b) RETURN b";
        let historical = database
            .execute_gql_at(statement, &bind, s1)
            .expect("labeled MATCH at S1");
        let live = database
            .execute_gql(statement, &bind)
            .expect("live labeled MATCH");
        assert_eq!(historical, vec![VId(2)]);
        assert!(!historical.contains(&VId(4)));
        assert_eq!(live, vec![VId(2), VId(4)]);
        let unlabeled_at = database
            .execute_gql_at(unlabeled, &bind, s1)
            .expect("unlabeled MATCH at S1");
        let unlabeled_live = database
            .execute_gql(unlabeled, &bind)
            .expect("live unlabeled MATCH");
        assert_eq!(unlabeled_at, unlabeled_live);
        assert_eq!(unlabeled_live, vec![VId(2), VId(4)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
