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
fn match_kernel_equals_reference_relation_destinations() {
    let ((), report) = run_async_under_lab(0x93_01, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-kernel-oracle-{}", std::process::id()));
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
        seed.add_edge(EId(1), VId(1), VId(20), vec![]);
        seed.add_edge(EId(2), VId(2), VId(10), vec![]);
        database
            .write(&commit_cx, seed)
            .await
            .expect("seed R edges");

        let statement = "MATCH (a)-[:R]->(b) RETURN b";
        let bind = RelationBind::new().with_relation("R", relation);
        let frontier = database.frontier().expect("read frontier");
        let live = database.execute_gql(statement, &bind).expect("live MATCH");
        let pinned = database
            .execute_gql_at(statement, &bind, frontier)
            .expect("frontier MATCH");
        assert_eq!(live, pinned);
        assert_eq!(live, vec![VId(10), VId(20)]);

        let mut off = WriteBatch::new(off_relation);
        off.add_edge(EId(3), VId(3), VId(99), vec![]);
        database
            .write(&commit_cx, off)
            .await
            .expect("commit off-relation edge");
        assert_eq!(
            database
                .execute_gql(statement, &bind)
                .expect("MATCH after off-relation commit"),
            live
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
        let mut oracle: Vec<VId> = graph
            .iter_vertices()
            .flat_map(|(source, _)| graph.neighbours(source, relation))
            .collect();
        oracle.sort_unstable();
        oracle.dedup();
        assert_eq!(live, oracle);
        assert!(!oracle.contains(&VId(99)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
