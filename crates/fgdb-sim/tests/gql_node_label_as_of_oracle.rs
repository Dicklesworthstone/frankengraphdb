use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::{replay, replay_through};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(7);
const STATEMENT: &str = "MATCH (a:Person)-[:R]->(b) RETURN b";

fn person_destinations(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph
                .vertex(edge.src)
                .is_some_and(|vertex| vertex.labels.contains(&PERSON))
        })
        .map(|(_, edge)| edge.dst)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn labeled_match_as_of_equals_the_reference_prefix() {
    let ((), report) = run_async_under_lab(0x38_06, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-label-as-of-oracle-{}",
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

        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![]);
        for vid in [2u128, 3, 4] {
            seed.create_vertex(VId(vid), vec![], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        db.write(&commit, seed).await.expect("seed commits");
        let s1 = db.frontier().expect("capture S1");

        let mut later = WriteBatch::new(R);
        later.create_vertex(VId(5), vec![PERSON], vec![]);
        later.create_vertex(VId(6), vec![], vec![]);
        later.add_edge(EId(12), VId(5), VId(6), vec![]);
        db.write(&commit, later).await.expect("later commit lands");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_label("Person", PERSON);
        let as_of = db
            .execute_gql_at(STATEMENT, &bind, s1)
            .expect("labeled MATCH executes at S1");
        let live = db
            .execute_gql(STATEMENT, &bind)
            .expect("live labeled MATCH executes");
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

        assert_eq!(as_of, person_destinations(prefix_graph));
        assert_eq!(as_of, vec![VId(2)]);
        assert!(!as_of.contains(&VId(4)) && !as_of.contains(&VId(6)));
        assert_eq!(live, person_destinations(full_graph));
        assert_eq!(live, vec![VId(2), VId(6)]);
        assert!(!live.contains(&VId(4)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
