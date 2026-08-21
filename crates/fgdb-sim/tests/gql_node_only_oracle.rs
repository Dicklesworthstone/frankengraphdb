use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(7);
const NODE_ONLY: &str = "MATCH (a:Person) RETURN a";

fn reference_people(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_vertices()
        .filter(|(_, vertex)| vertex.labels.contains(&PERSON))
        .map(|(vid, _)| vid)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn node_only_label_match_includes_isolated_reference_vertex() {
    let ((), report) = run_async_under_lab(0x40_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-node-only-oracle-{}",
            std::process::id()
        ));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let rows;
        {
            let mut db = Database::create(
                &commit,
                &dir,
                DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
            )
            .await
            .expect("database creates");
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![PERSON], vec![]);
            seed.create_vertex(VId(2), vec![], vec![]);
            seed.create_vertex(VId(3), vec![PERSON], vec![]);
            seed.create_vertex(VId(4), vec![], vec![]);
            seed.add_edge(EId(10), VId(3), VId(4), vec![]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new().with_label("Person", PERSON);
            rows = db
                .execute_gql(NODE_ONLY, &bind)
                .expect("node-only labeled MATCH executes");
        }

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
        let reference = replay(&commit, &coordinator)
            .await
            .expect("durable stream replays")
            .database;
        let graph = reference
            .graph(GraphId(1), BranchId(1))
            .expect("reference graph exists");

        assert_eq!(rows, reference_people(graph));
        assert_eq!(rows, vec![VId(1), VId(3)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
