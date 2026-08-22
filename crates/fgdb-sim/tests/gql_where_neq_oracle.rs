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
fn where_not_equal_matches_reference_non_loop_destinations() {
    let ((), report) = run_async_under_lab(0x9e_01, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-where-neq-oracle-{}", std::process::id()));
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
        for vid in [VId(1), VId(2), VId(3), VId(5)] {
            seed.create_vertex(vid, vec![], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(2), vec![]);
        seed.add_edge(EId(12), VId(5), VId(5), vec![]);
        database
            .write(&commit_cx, seed)
            .await
            .expect("seed R edges");

        let bind = RelationBind::new().with_relation("R", relation);
        let filtered = "MATCH (a)-[:R]->(b) WHERE a <> b RETURN b";
        let unfiltered = "MATCH (a)-[:R]->(b) RETURN b";
        let frontier = database.frontier().expect("read fixture frontier");
        let filtered_rows = database.execute_gql(filtered, &bind).expect("WHERE MATCH");
        assert_eq!(
            database
                .execute_gql_at(filtered, &bind, frontier)
                .expect("WHERE MATCH at frontier"),
            filtered_rows
        );
        let unfiltered_rows = database
            .execute_gql(unfiltered, &bind)
            .expect("plain MATCH");
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
        let mut non_loop_destinations: Vec<VId> = graph
            .iter_edges()
            .filter(|(_, edge)| edge.relation == relation && edge.src != edge.dst)
            .map(|(_, edge)| edge.dst)
            .collect();
        non_loop_destinations.sort_unstable();
        non_loop_destinations.dedup();
        let mut all_destinations: Vec<VId> = graph
            .iter_edges()
            .filter(|(_, edge)| edge.relation == relation)
            .map(|(_, edge)| edge.dst)
            .collect();
        all_destinations.sort_unstable();
        all_destinations.dedup();

        assert_eq!(filtered_rows, non_loop_destinations);
        assert_eq!(filtered_rows, vec![VId(2)]);
        assert_eq!(unfiltered_rows, all_destinations);
        assert_eq!(unfiltered_rows, vec![VId(2), VId(5)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
