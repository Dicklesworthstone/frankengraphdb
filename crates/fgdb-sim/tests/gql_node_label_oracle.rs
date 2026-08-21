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

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(7);
const LABELED: &str = "MATCH (a:Person)-[:R]->(b) RETURN b";
const UNLABELED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const DEK: [u8; 32] = [0x3c; 32];

fn engine_keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, DEK)
}

fn oracle_keys() -> CapsuleKeys {
    CapsuleKeys::new(
        K_OID,
        NAMESPACE,
        DEK,
        CAPSULE_OBJECT_KIND,
        CapsuleProfile::balanced(),
    )
}

fn reference_destinations(graph: &ReferenceGraph, labeled_only: bool) -> Vec<VId> {
    let mut destinations: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            !labeled_only
                || graph
                    .vertex(edge.src)
                    .is_some_and(|vertex| vertex.labels.contains(&PERSON))
        })
        .map(|(_, edge)| edge.dst)
        .collect();
    destinations.sort_unstable();
    destinations.dedup();
    destinations
}

#[test]
fn labeled_source_match_equals_the_reference_filter() {
    let ((), report) = run_async_under_lab(0x38_05, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-label-oracle-{}",
            std::process::id()
        ));
        let labeled_rows;
        let unlabeled_rows;
        {
            let mut db = Database::create(&commit, &dir, engine_keys())
                .await
                .expect("creates");
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(1), vec![PERSON], vec![]);
            for vid in [2u128, 3, 4] {
                batch.create_vertex(VId(vid), vec![], vec![]);
            }
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            batch.add_edge(EId(11), VId(3), VId(4), vec![]);
            db.write(&commit, batch).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_label("Person", PERSON);
            labeled_rows = db
                .execute_gql(LABELED, &bind)
                .expect("labeled MATCH executes");
            unlabeled_rows = db
                .execute_gql(UNLABELED, &bind)
                .expect("unlabeled MATCH executes");
        }

        let coordinator = CommitCoordinator::open(&commit, &dir, oracle_keys())
            .await
            .expect("oracle opens durable stream");
        let replayed = replay(&commit, &coordinator)
            .await
            .expect("durable stream replays");
        let graph = replayed
            .database
            .graph(GRAPH, BRANCH)
            .expect("reference graph exists");

        assert_eq!(labeled_rows, reference_destinations(graph, true));
        assert_eq!(labeled_rows, vec![VId(2)]);
        assert_eq!(unlabeled_rows, reference_destinations(graph, false));
        assert_eq!(unlabeled_rows, vec![VId(2), VId(4)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
