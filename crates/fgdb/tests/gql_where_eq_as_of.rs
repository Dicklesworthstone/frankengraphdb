use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const EQ_B: &str = "MATCH (a)-[:R]->(b) WHERE a = b RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn where_equal_as_of_excludes_later_self_loop() {
    let ((), report) = run_async_under_lab(0x39_06, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-eq-as-of-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut first = WriteBatch::new(R);
        for vid in [VId(1), VId(2), VId(5)] {
            first.create_vertex(vid, vec![], vec![]);
        }
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        first.add_edge(EId(11), VId(5), VId(5), vec![]);
        db.write(&commit, first).await.expect("first graph commits");
        let s1 = db.frontier().expect("reads S1");

        let mut second = WriteBatch::new(R);
        second.create_vertex(VId(8), vec![], vec![]);
        second.add_edge(EId(12), VId(8), VId(8), vec![]);
        db.write(&commit, second)
            .await
            .expect("later self-loop commits");

        let bind = RelationBind::new().with_relation("R", R);
        let pinned = db
            .execute_gql_at(EQ_B, &bind, s1)
            .expect("S1 equality MATCH executes");
        let live = db
            .execute_gql(EQ_B, &bind)
            .expect("live equality MATCH executes");

        assert_eq!(pinned, vec![VId(5)]);
        assert!(!pinned.contains(&VId(8)));
        assert!(!pinned.contains(&VId(2)));
        assert_eq!(live, vec![VId(5), VId(8)]);
        assert!(!live.contains(&VId(2)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
