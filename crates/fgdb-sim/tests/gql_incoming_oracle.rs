use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};

#[test]
fn incoming_match_projections_equal_reference_endpoints() {
    let ((), report) = run_async_under_lab(0x97_02, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-incoming-oracle-{}", std::process::id()));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let mut database = Database::create(
            &commit_cx,
            &dir,
            DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
        )
        .await
        .expect("create database");
        let relation = RelationId(1);
        let mut seed = WriteBatch::new(relation);
        for vid in [VId(1), VId(2), VId(3)] {
            seed.create_vertex(vid, vec![], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(2), vec![]);
        database.write(&commit_cx, seed).await.expect("seed edges");

        let bind = RelationBind::new().with_relation("R", relation);
        let incoming_a = "MATCH (a)<-[:R]-(b) RETURN a";
        let incoming_b = "MATCH (a)<-[:R]-(b) RETURN b";
        let outbound_b = "MATCH (a)-[:R]->(b) RETURN b";
        let frontier = database.frontier().expect("read seed frontier");
        let incoming_destinations = database
            .execute_gql(incoming_a, &bind)
            .expect("incoming RETURN a");
        let incoming_sources = database
            .execute_gql(incoming_b, &bind)
            .expect("incoming RETURN b");
        assert_eq!(incoming_destinations, vec![VId(2)]);
        assert_eq!(incoming_sources, vec![VId(1), VId(3)]);
        assert_eq!(
            database
                .execute_gql(outbound_b, &bind)
                .expect("outbound RETURN b"),
            vec![VId(2)]
        );
        assert_eq!(
            database
                .execute_gql_at(incoming_a, &bind, frontier)
                .expect("incoming RETURN a at seed frontier"),
            incoming_destinations
        );
        drop(database);

        let keys = CapsuleKeys::new(
            [0x5a; 32],
            namespace,
            [0x3c; 32],
            CAPSULE_OBJECT_KIND,
            CapsuleProfile::balanced(),
        );
        let coordinator = CommitCoordinator::open(&commit_cx, &dir, keys)
            .await
            .expect("open independent coordinator");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("replay durable stream")
            .database;
        let graph = reference
            .graph(GraphId(1), BranchId(1))
            .expect("reference graph exists");
        let mut sources = Vec::new();
        let mut destinations = Vec::new();
        for (_, edge) in graph
            .iter_edges()
            .filter(|(_, edge)| edge.relation == relation)
        {
            sources.push(edge.src);
            destinations.push(edge.dst);
        }
        sources.sort_unstable();
        sources.dedup();
        destinations.sort_unstable();
        destinations.dedup();
        assert_eq!(incoming_sources, sources);
        assert_eq!(incoming_destinations, destinations);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
