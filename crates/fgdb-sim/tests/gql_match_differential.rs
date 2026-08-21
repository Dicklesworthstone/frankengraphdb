use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_sim::replay;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const OFF_RELATION: RelationId = RelationId(2);
const OFF_RELATION_DESTINATION: VId = VId(99);
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

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-gql-match-differential-{}-{name}",
        std::process::id()
    ))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

#[test]
fn pinned_match_equals_reference_neighbour_expansion() {
    let dir = scratch("oracle-expansion");
    under_lab(0x9a_71, move |cx| async move {
        let mut database = Database::create(&cx, &dir, engine_keys())
            .await
            .expect("create product database");

        let mut r_batch = WriteBatch::new(R);
        for vid in [VId(1), VId(2), VId(3), VId(10), VId(20), OFF_RELATION_DESTINATION] {
            r_batch.create_vertex(vid, vec![], vec![]);
        }
        r_batch.add_edge(EId(1), VId(1), VId(20), vec![]);
        r_batch.add_edge(EId(2), VId(2), VId(10), vec![]);
        database
            .write(&cx, r_batch)
            .await
            .expect("commit two R edges in descending destination order");

        let mut off_relation_batch = WriteBatch::new(OFF_RELATION);
        off_relation_batch.add_edge(EId(3), VId(3), OFF_RELATION_DESTINATION, vec![]);
        database
            .write(&cx, off_relation_batch)
            .await
            .expect("commit off-relation edge");

        let bind = RelationBind::new().with_relation("R", R);
        let engine_rows = database
            .execute_gql("MATCH (a)-[:R]->(b) RETURN b", &bind)
            .expect("execute pinned MATCH through the product engine");
        assert_eq!(engine_rows, vec![VId(10), VId(20)]);
        assert!(!engine_rows.contains(&OFF_RELATION_DESTINATION));
        drop(database);

        let coordinator = CommitCoordinator::open(&cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        let reference = replay(&cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        let mut oracle_destinations: Vec<VId> = graph
            .iter_vertices()
            .flat_map(|(source, _)| graph.neighbours(source, R))
            .collect();
        oracle_destinations.sort_unstable();
        oracle_destinations.dedup();

        assert!(!oracle_destinations.contains(&OFF_RELATION_DESTINATION));
        assert_eq!(engine_rows, oracle_destinations);
    });
}
