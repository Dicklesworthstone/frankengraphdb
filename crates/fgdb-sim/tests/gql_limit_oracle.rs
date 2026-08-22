use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_reference::ReferenceGraph;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};

const R: RelationId = RelationId(1);
const UNLIMITED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const LIMITED: &str = "MATCH (a)-[:R]->(b) RETURN b LIMIT 1";

fn reference_destinations(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .map(|(_, edge)| edge.dst)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn limit_one_equals_the_reference_smallest_destination() {
    let ((), report) = run_async_under_lab(0x45_03, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-limit-oracle-{}", std::process::id()));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let limited;
        let unlimited;
        {
            let mut db = Database::create(
                &commit,
                &dir,
                DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
            )
            .await
            .expect("database creates");
            let mut seed = WriteBatch::new(R);
            for vid in [1u128, 2, 3, 4, 6] {
                seed.create_vertex(VId(vid), vec![], vec![]);
            }
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            seed.add_edge(EId(11), VId(1), VId(4), vec![]);
            seed.add_edge(EId(12), VId(3), VId(6), vec![]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new().with_relation("R", R);
            limited = db.execute_gql(LIMITED, &bind).expect("LIMIT 1 executes");
            unlimited = db
                .execute_gql(UNLIMITED, &bind)
                .expect("unlimited MATCH executes");
            let error = db
                .execute_gql("MATCH (a)-[:R]->(b) RETURN b LIMIT 0", &bind)
                .expect_err("LIMIT 0 is rejected");
            assert!(
                matches!(error, GqlError::Parse(_)),
                "expected Parse, got {error:?}"
            );
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
        let reference_rows = reference_destinations(graph);

        assert_eq!(unlimited, reference_rows);
        assert_eq!(unlimited, vec![VId(2), VId(4), VId(6)]);
        assert_eq!(limited, reference_rows[..1]);
        assert_eq!(limited, vec![VId(2)]);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
