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

fn composed_destinations(graph: &ReferenceGraph, r: RelationId, s: RelationId) -> Vec<VId> {
    let r_destinations: Vec<VId> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == r)
        .map(|(_, edge)| edge.dst)
        .collect();
    let mut rows: Vec<VId> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == s && r_destinations.contains(&edge.src))
        .map(|(_, edge)| edge.dst)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn two_hop_as_of_equals_reference_prefix_composition() {
    let ((), report) = run_async_under_lab(0x99_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-as-of-oracle-{}",
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
        for vid in [VId(1), VId(2), VId(4), VId(7)] {
            first.create_vertex(vid, vec![], vec![]);
        }
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        first.add_edge(EId(11), VId(1), VId(7), vec![]);
        database.write(&commit_cx, first).await.expect("seed R edges");
        let mut second = WriteBatch::new(s);
        second.add_edge(EId(20), VId(2), VId(4), vec![]);
        database.write(&commit_cx, second).await.expect("seed S edge");
        let s1 = database.frontier().expect("capture S1");

        let mut later = WriteBatch::new(s);
        later.create_vertex(VId(5), vec![], vec![]);
        later.add_edge(EId(21), VId(2), VId(5), vec![]);
        database.write(&commit_cx, later).await.expect("add later S edge");
        let bind = RelationBind::new().with_relation("R", r).with_relation("S", s);
        let statement = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
        let as_of = database
            .execute_gql_at(statement, &bind, s1)
            .expect("two-hop at S1");
        let live = database.execute_gql(statement, &bind).expect("live two-hop");
        assert_eq!(as_of, vec![VId(4)]);
        assert_eq!(live, vec![VId(4), VId(5)]);

        database.begin(&txn_cx).expect("begin transaction").abort();
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
        assert_eq!(as_of, composed_destinations(prefix_graph, r, s));
        assert_eq!(live, composed_destinations(full_graph, r, s));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
