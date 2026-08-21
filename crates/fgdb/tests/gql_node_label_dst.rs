use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const LABELED_DST: &str = "MATCH (a)-[:R]->(b:Person) RETURN b";
const UNLABELED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn labeled_destination_excludes_unlabeled_destinations() {
    let ((), report) = run_async_under_lab(0x38_06, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-label-dst-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![PERSON], vec![]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.create_vertex(VId(4), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_label("Person", PERSON);
        let labeled = db
            .execute_gql(LABELED_DST, &bind)
            .expect("destination-labeled MATCH executes");
        let unlabeled = db
            .execute_gql(UNLABELED, &bind)
            .expect("unlabeled MATCH executes");

        assert!(labeled.contains(&VId(2)));
        assert!(!labeled.contains(&VId(4)));
        assert!(unlabeled.contains(&VId(2)));
        assert!(unlabeled.contains(&VId(4)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
