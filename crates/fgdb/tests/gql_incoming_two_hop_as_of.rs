use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn incoming_two_hop_as_of_pins_composed_sources() {
    let ((), report) = run_async_under_lab(0x9d_01, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-as-of-{}",
            std::process::id()
        ));
        let keys = DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        );
        let r = RelationId(1);
        let s = RelationId(2);
        let mut database = Database::create(&commit_cx, &dir, keys)
            .await
            .expect("create database");
        let mut first = WriteBatch::new(r);
        for vid in [1, 2, 4, 7, 8, 9].map(VId) {
            first.create_vertex(vid, vec![], vec![]);
        }
        first.add_edge(EId(10), VId(2), VId(1), vec![]);
        first.add_edge(EId(11), VId(7), VId(1), vec![]);
        database.write(&commit_cx, first).await.expect("seed R edges");
        let mut second = WriteBatch::new(s);
        second.add_edge(EId(20), VId(4), VId(2), vec![]);
        second.add_edge(EId(21), VId(8), VId(9), vec![]);
        database.write(&commit_cx, second).await.expect("seed S edges");
        let s1 = database.frontier().expect("capture S1");

        let mut later = WriteBatch::new(s);
        later.create_vertex(VId(5), vec![], vec![]);
        later.add_edge(EId(22), VId(5), VId(2), vec![]);
        database.write(&commit_cx, later).await.expect("add composed continuation");
        let bind = RelationBind::new().with_relation("R", r).with_relation("S", s);
        let two_hop = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
        let one_hop = "MATCH (a)<-[:R]-(b) RETURN b";
        let historical = database
            .execute_gql_at(two_hop, &bind, s1)
            .expect("incoming two-hop at S1");
        let live = database.execute_gql(two_hop, &bind).expect("live incoming two-hop");
        assert_eq!(historical, vec![VId(4)]);
        assert!(!historical.contains(&VId(5)));
        assert_eq!(live, vec![VId(4), VId(5)]);
        assert_eq!(
            database
                .execute_gql_at(one_hop, &bind, s1)
                .expect("incoming one-hop at S1"),
            vec![VId(2), VId(7)]
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
