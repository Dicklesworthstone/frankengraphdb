use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const DEST_LABELED: &str = "MATCH (a)-[:R]->(b:Person) RETURN b";
const SOURCE_LABELED: &str = "MATCH (a:Person)-[:R]->(b) RETURN b";
const UNLABELED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn destination_label_certificate_is_shape_distinct() {
    let ((), report) = run_async_under_lab(0x38_08, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-label-dst-cert-{}",
            std::process::id()
        ));
        let db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_label("Person", PERSON);

        let destination = db
            .gql_plan_certificate(DEST_LABELED, &bind)
            .expect("destination-labeled MATCH certifies");
        let source = db
            .gql_plan_certificate(SOURCE_LABELED, &bind)
            .expect("source-labeled MATCH certifies");
        let unlabeled = db
            .gql_plan_certificate(UNLABELED, &bind)
            .expect("unlabeled MATCH certifies");

        assert_eq!(destination.snapshot_seq, source.snapshot_seq);
        assert_eq!(destination.snapshot_seq, unlabeled.snapshot_seq);
        assert_ne!(destination.digest, unlabeled.digest);
        assert_ne!(destination.digest, source.digest);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
