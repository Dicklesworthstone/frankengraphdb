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
fn two_hop_match_equals_reference_composed_endpoints() {
    let ((), report) = run_async_under_lab(0x99_01, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-two-hop-oracle-{}", std::process::id()));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let mut database = Database::create(
            &commit_cx,
            &dir,
            DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
        )
        .await
        .expect("create database");
        let r = RelationId(1);
        let s = RelationId(2);
        let mut first = WriteBatch::new(r);
        for vid in [1, 2, 3, 4, 5, 7, 8, 9].map(VId) {
            first.create_vertex(vid, vec![], vec![]);
        }
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        first.add_edge(EId(11), VId(3), VId(2), vec![]);
        first.add_edge(EId(12), VId(1), VId(7), vec![]);
        database
            .write(&commit_cx, first)
            .await
            .expect("seed R edges");
        let mut second = WriteBatch::new(s);
        second.add_edge(EId(20), VId(2), VId(4), vec![]);
        second.add_edge(EId(21), VId(2), VId(5), vec![]);
        second.add_edge(EId(22), VId(9), VId(8), vec![]);
        database
            .write(&commit_cx, second)
            .await
            .expect("seed S edges");

        let bind = RelationBind::new()
            .with_relation("R", r)
            .with_relation("S", s);
        let return_c = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
        let return_a = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN a";
        let one_hop_b = "MATCH (a)-[:R]->(b) RETURN b";
        let frontier = database.frontier().expect("read fixture frontier");
        let destinations = database.execute_gql(return_c, &bind).expect("RETURN c");
        let sources = database.execute_gql(return_a, &bind).expect("RETURN a");
        assert_eq!(
            database
                .execute_gql_at(return_c, &bind, frontier)
                .expect("RETURN c at frontier"),
            destinations
        );
        assert_eq!(
            database
                .execute_gql_at(return_a, &bind, frontier)
                .expect("RETURN a at frontier"),
            sources
        );
        let one_hop = database
            .execute_gql(one_hop_b, &bind)
            .expect("one-hop RETURN b");
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
        let r_edges: Vec<(VId, VId)> = graph
            .iter_edges()
            .filter(|(_, edge)| edge.relation == r)
            .map(|(_, edge)| (edge.src, edge.dst))
            .collect();
        let s_edges: Vec<(VId, VId)> = graph
            .iter_edges()
            .filter(|(_, edge)| edge.relation == s)
            .map(|(_, edge)| (edge.src, edge.dst))
            .collect();
        let mut oracle_sources = Vec::new();
        let mut oracle_destinations = Vec::new();
        for (source, middle) in &r_edges {
            for (_, destination) in s_edges.iter().filter(|(s_source, _)| s_source == middle) {
                oracle_sources.push(*source);
                oracle_destinations.push(*destination);
            }
        }
        oracle_sources.sort_unstable();
        oracle_sources.dedup();
        oracle_destinations.sort_unstable();
        oracle_destinations.dedup();
        let mut oracle_one_hop: Vec<VId> = r_edges.iter().map(|(_, dst)| *dst).collect();
        oracle_one_hop.sort_unstable();
        oracle_one_hop.dedup();

        assert_eq!(sources, oracle_sources);
        assert_eq!(destinations, oracle_destinations);
        assert_eq!(one_hop, oracle_one_hop);
        assert_eq!(one_hop, vec![VId(2), VId(7)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
