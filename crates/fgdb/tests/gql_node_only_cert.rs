use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const NODE_ONLY: &str = "MATCH (a:Person) RETURN a";
const ONE_HOP: &str = "MATCH (a:Person)-[:R]->(b) RETURN a";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn node_only_plan_certificate_is_distinct_and_deterministic() {
    let ((), report) = run_async_under_lab(0x40_02, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-only-cert-{}",
            std::process::id()
        ));
        let db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_label("Person", PERSON);

        let node_only = db
            .gql_plan_certificate(NODE_ONLY, &bind)
            .expect("node-only MATCH certifies");
        let one_hop = db
            .gql_plan_certificate(ONE_HOP, &bind)
            .expect("one-hop MATCH certifies");

        assert_eq!(node_only.snapshot_seq, one_hop.snapshot_seq);
        assert_ne!(node_only.digest, one_hop.digest);
        assert_eq!(
            db.gql_plan_certificate(NODE_ONLY, &bind)
                .expect("node-only MATCH re-certifies"),
            node_only
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
