use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn execute_gql_at_live_frontier_equals_execute_gql() {
    let ((), report) = run_async_under_lab(0x91_01, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-at-live-frontier-{}",
            std::process::id()
        ));
        let keys = DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        );
        let relation = RelationId(1);
        let mut database = Database::create(&commit_cx, &dir, keys)
            .await
            .expect("create database");
        let mut seed = WriteBatch::new(relation);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        database.write(&commit_cx, seed).await.expect("seed edge");

        let statement = "MATCH (a)-[:R]->(b) RETURN b";
        let bind = RelationBind::new().with_relation("R", relation);
        let seq = database.frontier().expect("read live frontier");
        let live = database.execute_gql(statement, &bind).expect("live MATCH");
        let at = database
            .execute_gql_at(statement, &bind, seq)
            .expect("frontier-pinned MATCH");
        assert_eq!(at, live);
        assert_eq!(at, vec![VId(2)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
