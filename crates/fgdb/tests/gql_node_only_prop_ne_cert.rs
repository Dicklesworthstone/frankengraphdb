use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;

const PERSON: LabelId = LabelId(3);
const K: PropertyKeyId = PropertyKeyId(7);
const NOT_EQUAL: &str = "MATCH (a:Person) WHERE a.k <> 1 RETURN a";
const EQUAL: &str = "MATCH (a:Person) WHERE a.k = 1 RETURN a";
const UNFILTERED: &str = "MATCH (a:Person) RETURN a";

#[test]
fn node_only_property_inequality_changes_the_plan_certificate() {
    let ((), report) = run_async_under_lab(0x52_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-only-prop-ne-cert-{}",
            std::process::id()
        ));
        let db = Database::create(
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
        let bind = RelationBind::new()
            .with_label("Person", PERSON)
            .with_property("k", K);

        let not_equal = db
            .gql_plan_certificate(NOT_EQUAL, &bind)
            .expect("node-only inequality certifies");
        let equal = db
            .gql_plan_certificate(EQUAL, &bind)
            .expect("node-only equality certifies");
        let unfiltered = db
            .gql_plan_certificate(UNFILTERED, &bind)
            .expect("unfiltered node-only MATCH certifies");

        assert_eq!(not_equal.snapshot_seq, equal.snapshot_seq);
        assert_eq!(not_equal.snapshot_seq, unfiltered.snapshot_seq);
        assert_ne!(not_equal, equal, "the property operator is plan identity");
        assert_ne!(
            not_equal, unfiltered,
            "the property predicate is plan identity"
        );
        assert_eq!(
            db.gql_plan_certificate(NOT_EQUAL, &bind)
                .expect("node-only inequality re-certifies"),
            not_equal
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
