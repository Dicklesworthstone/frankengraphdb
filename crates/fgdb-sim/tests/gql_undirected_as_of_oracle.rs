use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_reference::ReferenceGraph;
use fgdb_sim::{replay, replay_through};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};

fn incident_endpoints(graph: &ReferenceGraph, relation: RelationId) -> Vec<VId> {
    let mut rows = Vec::new();
    for (_, edge) in graph.iter_edges().filter(|(_, edge)| edge.relation == relation) {
        rows.push(edge.src);
        rows.push(edge.dst);
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn undirected_as_of_equals_reference_incident_prefix() {
    let ((), report) = run_async_under_lab(0x9b_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-as-of-oracle-{}",
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
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        database.write(&commit_cx, seed).await.expect("seed S1 edge");
        let s1 = database.frontier().expect("capture S1");

        let mut later = WriteBatch::new(relation);
        later.create_vertex(VId(3), vec![], vec![]);
        later.add_edge(EId(11), VId(3), VId(2), vec![]);
        database.write(&commit_cx, later).await.expect("add later edge");
        let bind = RelationBind::new().with_relation("R", relation);
        let undirected = "MATCH (a)-[:R]-(b) RETURN b";
        let directed = "MATCH (a)-[:R]->(b) RETURN b";
        let as_of = database
            .execute_gql_at(undirected, &bind, s1)
            .expect("undirected MATCH at S1");
        let live = database.execute_gql(undirected, &bind).expect("live undirected MATCH");
        assert_eq!(as_of, vec![VId(1), VId(2)]);
        assert_eq!(live, vec![VId(1), VId(2), VId(3)]);
        assert_eq!(
            database.execute_gql(directed, &bind).expect("live directed MATCH"),
            vec![VId(2)]
        );
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
        let prefix = replay_through(&commit_cx, &coordinator, s1)
            .await
            .expect("replay S1 prefix");
        let full = replay(&commit_cx, &coordinator).await.expect("replay full stream");
        let prefix_graph = prefix
            .database
            .graph(GraphId(1), BranchId(1))
            .expect("prefix graph exists");
        let full_graph = full
            .database
            .graph(GraphId(1), BranchId(1))
            .expect("full graph exists");
        assert_eq!(as_of, incident_endpoints(prefix_graph, relation));
        assert_eq!(live, incident_endpoints(full_graph, relation));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
