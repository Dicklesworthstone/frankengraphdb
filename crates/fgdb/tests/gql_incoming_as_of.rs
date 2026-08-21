use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn incoming_match_as_of_pins_destinations_and_sources() {
    let ((), report) = run_async_under_lab(0x98_01, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-as-of-{}",
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
        for vid in [VId(1), VId(2), VId(3)] {
            seed.create_vertex(vid, vec![], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(2), vec![]);
        database.write(&commit_cx, seed).await.expect("seed S1 edges");
        let s1 = database.frontier().expect("read S1");

        let mut later = WriteBatch::new(relation);
        later.create_vertex(VId(5), vec![], vec![]);
        later.create_vertex(VId(9), vec![], vec![]);
        later.add_edge(EId(12), VId(9), VId(5), vec![]);
        database.write(&commit_cx, later).await.expect("advance frontier");

        let bind = RelationBind::new().with_relation("R", relation);
        let incoming_a = "MATCH (a)<-[:R]-(b) RETURN a";
        let incoming_b = "MATCH (a)<-[:R]-(b) RETURN b";
        let outbound_b = "MATCH (a)-[:R]->(b) RETURN b";
        assert_eq!(
            database
                .execute_gql_at(incoming_a, &bind, s1)
                .expect("incoming RETURN a at S1"),
            vec![VId(2)]
        );
        assert_eq!(
            database
                .execute_gql_at(incoming_b, &bind, s1)
                .expect("incoming RETURN b at S1"),
            vec![VId(1), VId(3)]
        );
        assert_eq!(
            database.execute_gql(incoming_a, &bind).expect("live incoming RETURN a"),
            vec![VId(2), VId(5)]
        );
        assert_eq!(
            database.execute_gql(incoming_b, &bind).expect("live incoming RETURN b"),
            vec![VId(1), VId(3), VId(9)]
        );
        assert_eq!(
            database
                .execute_gql_at(outbound_b, &bind, s1)
                .expect("outbound RETURN b at S1"),
            vec![VId(2)]
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
