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

const R: RelationId = RelationId(1);
const EQ_B: &str = "MATCH (a)-[:R]->(b) WHERE a = b RETURN b";

fn self_loop_destinations(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R && edge.src == edge.dst)
        .map(|(_, edge)| edge.dst)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn where_equal_as_of_matches_reference_prefix_self_loops() {
    let ((), report) = run_async_under_lab(0x9e_03, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-eq-as-of-oracle-{}",
            std::process::id()
        ));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let mut db = Database::create(
            &commit,
            &dir,
            DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
        )
        .await
        .expect("database creates");

        let mut first = WriteBatch::new(R);
        for vid in [VId(1), VId(2), VId(5)] {
            first.create_vertex(vid, vec![], vec![]);
        }
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        first.add_edge(EId(11), VId(5), VId(5), vec![]);
        db.write(&commit, first).await.expect("S1 commits");
        let s1 = db.frontier().expect("captures S1");

        let mut later = WriteBatch::new(R);
        later.create_vertex(VId(6), vec![], vec![]);
        later.add_edge(EId(12), VId(6), VId(6), vec![]);
        db.write(&commit, later)
            .await
            .expect("later self-loop commits");

        let bind = RelationBind::new().with_relation("R", R);
        let as_of = db
            .execute_gql_at(EQ_B, &bind, s1)
            .expect("S1 equality MATCH executes");
        let live = db
            .execute_gql(EQ_B, &bind)
            .expect("live equality MATCH executes");
        drop(db);

        let keys = CapsuleKeys::new(
            [0x5a; 32],
            namespace,
            [0x3c; 32],
            CAPSULE_OBJECT_KIND,
            CapsuleProfile::balanced(),
        );
        let coordinator = CommitCoordinator::open(&commit, &dir, keys)
            .await
            .expect("independent coordinator opens");
        let prefix = replay_through(&commit, &coordinator, s1)
            .await
            .expect("S1 prefix replays");
        let full = replay(&commit, &coordinator)
            .await
            .expect("full stream replays");
        let prefix_graph = prefix
            .database
            .graph(GraphId(1), BranchId(1))
            .expect("prefix graph exists");
        let full_graph = full
            .database
            .graph(GraphId(1), BranchId(1))
            .expect("full graph exists");

        assert_eq!(as_of, self_loop_destinations(prefix_graph));
        assert_eq!(as_of, vec![VId(5)]);
        assert!(!as_of.contains(&VId(6)) && !as_of.contains(&VId(2)));
        assert_eq!(live, self_loop_destinations(full_graph));
        assert_eq!(live, vec![VId(5), VId(6)]);
        assert!(!live.contains(&VId(2)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
