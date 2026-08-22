use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::VId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const NODE_ONLY: &str = "MATCH (a:Person) RETURN a";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn node_only_as_of_excludes_later_labeled_vertex() {
    let ((), report) = run_async_under_lab(0x40_03, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!("fgdb-node-only-as-of-{}", std::process::id()));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");

        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![PERSON], vec![]);
        db.write(&commit, first).await.expect("S1 commits");
        let s1 = db.frontier().expect("captures S1");

        let mut later = WriteBatch::new(R);
        later.create_vertex(VId(6), vec![PERSON], vec![]);
        db.write(&commit, later).await.expect("S2 commits");

        let bind = RelationBind::new().with_label("Person", PERSON);
        let as_of = db
            .execute_gql_at(NODE_ONLY, &bind, s1)
            .expect("node-only MATCH executes at S1");
        let live = db
            .execute_gql(NODE_ONLY, &bind)
            .expect("live node-only MATCH executes");

        assert_eq!(as_of, vec![VId(1)]);
        assert!(!as_of.contains(&VId(6)));
        assert_eq!(live, vec![VId(1), VId(6)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
