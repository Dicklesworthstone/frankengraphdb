use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const SOURCE: VId = VId(2);
const DESTINATION: VId = VId(3);
const EDGE: EId = EId(1);
const MATCH_R: &str = "MATCH (a)-[:R]->(b) RETURN b";
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
        "fgdb-writetxn-gql-overlay-{}-{name}",
        std::process::id()
    ))
}

fn staged_graph() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(SOURCE, vec![], vec![]);
    batch.create_vertex(DESTINATION, vec![], vec![]);
    batch.add_edge(EDGE, SOURCE, DESTINATION, vec![]);
    batch
}

#[test]
fn match_reads_staged_edge_but_abort_keeps_it_out_of_reference_replay() {
    let dir = scratch("abort-is-private");
    let ((), report) = run_async_under_lab(0x7b_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let bind = RelationBind::new().with_relation("R", R);
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");

        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        database
            .write(&commit_cx, seed)
            .await
            .expect("seed durable reference coordinate");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        transaction
            .write(&mut database, staged_graph())
            .expect("stage vertices and R edge");
        let rows = transaction
            .execute_gql(&database, MATCH_R, &bind)
            .expect("MATCH executes against the transaction overlay");
        assert_eq!(
            rows,
            vec![DESTINATION],
            "MATCH must see the staged R destination"
        );
        assert!(
            database
                .execute_gql(MATCH_R, &bind)
                .expect("base MATCH executes")
                .is_empty(),
            "the base database cannot see the private edge"
        );

        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        assert_eq!(
            database.frontier().expect("abort leaves handle healthy"),
            frontier_before
        );
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), frontier_before.0 as usize);
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("seeded reference coordinate exists");
        assert!(
            graph.neighbours(SOURCE, R).is_empty(),
            "aborted overlay edge must leave no durable replay residue"
        );
        assert!(graph.vertex(SOURCE).is_none() && graph.vertex(DESTINATION).is_none());
        assert!(graph.edge(EDGE).is_none());
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_match_overlay_edge_is_visible_to_reference_replay() {
    let dir = scratch("commit-is-durable");
    let ((), report) = run_async_under_lab(0x7b_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let bind = RelationBind::new().with_relation("R", R);
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        transaction
            .write(&mut database, staged_graph())
            .expect("stage vertices and R edge");
        assert_eq!(
            transaction
                .execute_gql(&database, MATCH_R, &bind)
                .expect("MATCH executes against the transaction overlay"),
            vec![DESTINATION]
        );
        transaction
            .commit(&mut database, &commit_cx)
            .await
            .expect("commit overlay as one durable transaction");
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(
            graph.neighbours(SOURCE, R),
            vec![DESTINATION],
            "reference replay sees the committed R destination"
        );
        assert!(graph.edge(EDGE).is_some(), "committed edge is durable");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
