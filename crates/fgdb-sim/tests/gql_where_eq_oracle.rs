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
fn where_equal_matches_reference_self_loop_destinations() {
    let ((), report) = run_async_under_lab(0x9e_02, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-eq-oracle-{}",
            std::process::id()
        ));
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
        for vid in [VId(1), VId(2), VId(5)] {
            seed.create_vertex(vid, vec![], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(5), VId(5), vec![]);
        database.write(&commit_cx, seed).await.expect("seed R edges");

        let bind = RelationBind::new().with_relation("R", relation);
        let rows = database
            .execute_gql("MATCH (a)-[:R]->(b) WHERE a = b RETURN b", &bind)
            .expect("WHERE equality MATCH executes");
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
        let mut self_loop_destinations: Vec<VId> = graph
            .iter_edges()
            .filter(|(_, edge)| edge.relation == relation && edge.src == edge.dst)
            .map(|(_, edge)| edge.dst)
            .collect();
        self_loop_destinations.sort_unstable();
        self_loop_destinations.dedup();

        assert_eq!(rows, self_loop_destinations);
        assert_eq!(rows, vec![VId(5)]);
        assert!(!rows.contains(&VId(2)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
