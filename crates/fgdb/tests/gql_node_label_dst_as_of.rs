use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn labeled_destination_match_as_of_pins_person_destinations() {
    let ((), report) = run_async_under_lab(0x9f_02, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-node-label-dst-as-of-{}",
            std::process::id()
        ));
        let relation = RelationId(1);
        let person = LabelId(7);
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
        seed.create_vertex(VId(2), vec![person], vec![]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.create_vertex(VId(4), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        db.write(&commit, seed).await.expect("seed commits");
        let s1 = db.frontier().expect("capture S1");

        let mut later = WriteBatch::new(relation);
        later.create_vertex(VId(5), vec![], vec![]);
        later.create_vertex(VId(6), vec![person], vec![]);
        later.add_edge(EId(12), VId(5), VId(6), vec![]);
        db.write(&commit, later).await.expect("later commit lands");

        let bind = RelationBind::new()
            .with_relation("R", relation)
            .with_label("Person", person);
        let statement = "MATCH (a)-[:R]->(b:Person) RETURN b";
        let historical = db
            .execute_gql_at(statement, &bind, s1)
            .expect("labeled destination MATCH executes at S1");
        let live = db
            .execute_gql(statement, &bind)
            .expect("live labeled destination MATCH executes");

        assert_eq!(historical, vec![VId(2)]);
        assert!(!historical.contains(&VId(4)) && !historical.contains(&VId(6)));
        assert_eq!(live, vec![VId(2), VId(6)]);
        assert!(!live.contains(&VId(4)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
