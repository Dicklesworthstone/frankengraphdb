use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn plan_certificate_at_live_frontier_equals_live_certificate() {
    let ((), report) = run_async_under_lab(0x94_01, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-plan-certificate-at-live-{}",
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
        let seq = database.frontier().expect("read frontier");
        let historical = database
            .gql_plan_certificate_at(statement, &bind, seq)
            .expect("historical plan certificate");
        let live = database
            .gql_plan_certificate(statement, &bind)
            .expect("live plan certificate");
        assert_eq!(historical.snapshot_seq, live.snapshot_seq);
        assert_eq!(historical.digest, live.digest);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
