use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_reference::ReferenceGraph;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};

fn incidents(graph: &ReferenceGraph, relation: RelationId) -> Vec<VId> {
    let mut rows = Vec::new();
    for (_, edge) in graph.iter_edges().filter(|(_, edge)| edge.relation == relation) {
        rows.extend([edge.src, edge.dst]);
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn undirected_two_hop_equals_reference_composed_incidents() {
    let ((), report) = run_async_under_lab(0x9c_01, |root| async move {
        let commit_cx = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-two-hop-oracle-{}",
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
        let r = RelationId(1);
        let s = RelationId(2);
        let mut first = WriteBatch::new(r);
        for vid in [1, 2, 3, 4, 8, 9].map(VId) {
            first.create_vertex(vid, vec![], vec![]);
        }
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        first.add_edge(EId(11), VId(3), VId(2), vec![]);
        database.write(&commit_cx, first).await.expect("seed R edges");
        let mut second = WriteBatch::new(s);
        second.add_edge(EId(20), VId(2), VId(4), vec![]);
        second.add_edge(EId(21), VId(9), VId(8), vec![]);
        database.write(&commit_cx, second).await.expect("seed S edges");

        let bind = RelationBind::new().with_relation("R", r).with_relation("S", s);
        let two_hop = "MATCH (a)-[:R]-(b)-[:S]-(c) RETURN c";
        let one_hop = "MATCH (a)-[:R]-(b) RETURN b";
        let frontier = database.frontier().expect("read fixture frontier");
        let live = database.execute_gql(two_hop, &bind).expect("live two-hop MATCH");
        assert_eq!(
            database
                .execute_gql_at(two_hop, &bind, frontier)
                .expect("two-hop MATCH at frontier"),
            live
        );
        let one_hop_rows = database.execute_gql(one_hop, &bind).expect("one-hop MATCH");
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
        let r_incidents = incidents(graph, r);
        let mut composed = Vec::new();
        for (_, edge) in graph.iter_edges().filter(|(_, edge)| edge.relation == s) {
            if r_incidents.contains(&edge.src) {
                composed.push(edge.dst);
            }
            if r_incidents.contains(&edge.dst) {
                composed.push(edge.src);
            }
        }
        composed.sort_unstable();
        composed.dedup();

        assert_eq!(live, composed);
        assert_eq!(live, vec![VId(4)]);
        assert_eq!(one_hop_rows, r_incidents);
        assert_eq!(one_hop_rows, vec![VId(1), VId(2), VId(3)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
