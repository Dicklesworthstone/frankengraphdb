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
fn undirected_match_equals_reference_incident_endpoints() {
    let ((), report) = run_async_under_lab(0x9a_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-oracle-{}",
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
        for vid in [VId(1), VId(2), VId(3), VId(9)] {
            seed.create_vertex(vid, vec![], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(2), vec![]);
        database.write(&commit_cx, seed).await.expect("seed R edges");

        let bind = RelationBind::new().with_relation("R", relation);
        let undirected_a = database
            .execute_gql("MATCH (a)-[:R]-(b) RETURN a", &bind)
            .expect("undirected RETURN a");
        let undirected_b = database
            .execute_gql("MATCH (a)-[:R]-(b) RETURN b", &bind)
            .expect("undirected RETURN b");
        let directed_b = database
            .execute_gql("MATCH (a)-[:R]->(b) RETURN b", &bind)
            .expect("directed RETURN b");
        database.begin(&txn_cx).expect("begin unused transaction").abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
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
        let mut incident = Vec::new();
        let mut destinations = Vec::new();
        for (_, edge) in graph.iter_edges().filter(|(_, edge)| edge.relation == relation) {
            incident.push(edge.src);
            incident.push(edge.dst);
            destinations.push(edge.dst);
        }
        incident.sort_unstable();
        incident.dedup();
        destinations.sort_unstable();
        destinations.dedup();

        assert_eq!(undirected_a, incident);
        assert_eq!(undirected_b, incident);
        assert!(!incident.contains(&VId(9)));
        assert_eq!(directed_b, destinations);
        assert_eq!(directed_b, vec![VId(2)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
