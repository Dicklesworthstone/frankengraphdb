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
fn return_a_sources_and_return_b_destinations_equal_reference() {
    let ((), report) = run_async_under_lab(0x96_01, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-return-src-oracle-{}",
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
        let off_relation = RelationId(2);
        let mut seed = WriteBatch::new(relation);
        for vid in [VId(1), VId(2), VId(3), VId(10), VId(20), VId(99)] {
            seed.create_vertex(vid, vec![], vec![]);
        }
        seed.add_edge(EId(1), VId(3), VId(10), vec![]);
        seed.add_edge(EId(2), VId(1), VId(20), vec![]);
        seed.add_edge(EId(3), VId(3), VId(20), vec![]);
        database.write(&commit_cx, seed).await.expect("seed R edges");
        let mut off = WriteBatch::new(off_relation);
        off.add_edge(EId(4), VId(2), VId(99), vec![]);
        database
            .write(&commit_cx, off)
            .await
            .expect("seed off-relation edge");

        let bind = RelationBind::new().with_relation("R", relation);
        let sources = database
            .execute_gql("MATCH (a)-[:R]->(b) RETURN a", &bind)
            .expect("execute source projection");
        let destinations = database
            .execute_gql("MATCH (a)-[:R]->(b) RETURN b", &bind)
            .expect("execute destination projection");
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
        let mut oracle_sources: Vec<VId> = graph
            .iter_vertices()
            .filter_map(|(source, _)| {
                (!graph.neighbours(source, relation).is_empty()).then_some(source)
            })
            .collect();
        oracle_sources.sort_unstable();
        oracle_sources.dedup();
        let mut oracle_destinations: Vec<VId> = graph
            .iter_vertices()
            .flat_map(|(source, _)| graph.neighbours(source, relation))
            .collect();
        oracle_destinations.sort_unstable();
        oracle_destinations.dedup();

        assert_eq!(sources, oracle_sources);
        assert_eq!(sources, vec![VId(1), VId(3)]);
        assert_eq!(destinations, oracle_destinations);
        assert_eq!(destinations, vec![VId(10), VId(20)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
