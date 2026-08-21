use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn certified_at_rows_equal_execute_gql_at_and_certificate_uses_as_of() {
    let ((), report) = run_async_under_lab(0x92_01, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-certified-at-pairing-{}",
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
        let (rows, certificate) = database
            .execute_gql_certified_at(statement, &bind, seq)
            .expect("certified historical MATCH");
        assert_eq!(
            rows,
            database
                .execute_gql_at(statement, &bind, seq)
                .expect("historical MATCH")
        );
        assert_eq!(rows, vec![VId(2)]);
        assert_eq!(certificate.snapshot_seq, seq);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
