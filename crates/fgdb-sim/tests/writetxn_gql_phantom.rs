use asupersync::lab::run_async_under_lab;
use fgdb::{
    CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch, WriteTxnError,
};
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
        "fgdb-writetxn-gql-phantom-{}-{name}",
        std::process::id()
    ))
}

fn seed_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(1), VId(1), VId(2), vec![]);
    batch
}

fn create_vertex(vid: VId) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(vid, vec![], vec![]);
    batch
}

#[test]
fn new_qualifying_edge_aborts_match_reader_and_replays_the_phantom() {
    let dir = scratch("new-edge-conflict");
    let ((), report) = run_async_under_lab(0x7e_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let bind = RelationBind::new().with_relation("R", R);
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable R edge");

        let mut reader = database.begin(&txn_cx).expect("begin MATCH reader");
        let mut inserter = database.begin(&txn_cx).expect("begin edge inserter");
        assert_eq!(
            reader
                .execute_gql(&database, MATCH_R, &bind)
                .expect("transactional MATCH succeeds"),
            vec![VId(2)]
        );
        reader
            .write(&mut database, create_vertex(VId(3)))
            .expect("reader stages a write disjoint from the MATCH range");

        let mut phantom = create_vertex(VId(9));
        phantom.add_edge(EId(2), VId(1), VId(9), vec![]);
        inserter
            .write(&mut database, phantom)
            .expect("stage new qualifying R edge");
        inserter
            .commit(&mut database, &commit_cx)
            .await
            .expect("phantom inserter commits");

        let refusal = reader.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(&refusal, Err(WriteTxnError::Write(_))),
            "qualifying-edge phantom must be a typed Write abort: {refusal:?}"
        );
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("FG-LAW-FCW-READ-01"),
            "phantom abort must name READ-01: {rendered}"
        );
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 2, "seed and inserter only");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        let mut destinations: Vec<VId> = graph
            .iter_vertices()
            .flat_map(|(source, _)| graph.neighbours(source, R))
            .collect();
        destinations.sort_unstable();
        destinations.dedup();
        assert_eq!(
            destinations,
            vec![VId(2), VId(9)],
            "reference MATCH sees the original and B's new destination"
        );
        assert!(
            graph.vertex(VId(3)).is_none(),
            "READ-01 abort leaves none of A's disjoint write"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn new_vertex_without_qualifying_edge_does_not_abort_match_reader() {
    let dir = scratch("vertex-only-control");
    let ((), report) = run_async_under_lab(0x7e_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let bind = RelationBind::new().with_relation("R", R);
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable R edge");

        let mut reader = database.begin(&txn_cx).expect("begin MATCH reader");
        let mut creator = database.begin(&txn_cx).expect("begin disjoint creator");
        assert_eq!(
            reader
                .execute_gql(&database, MATCH_R, &bind)
                .expect("transactional MATCH succeeds"),
            vec![VId(2)]
        );
        reader
            .write(&mut database, create_vertex(VId(3)))
            .expect("reader stages its disjoint write at the pinned basis");
        creator
            .write(&mut database, create_vertex(VId(9)))
            .expect("stage vertex with no qualifying edge");
        creator
            .commit(&mut database, &commit_cx)
            .await
            .expect("vertex-only creator commits");
        reader
            .commit(&mut database, &commit_cx)
            .await
            .expect("vertex without R edge does not invalidate MATCH");
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 3, "seed and both transactions");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(
            graph.neighbours(VId(1), R),
            vec![VId(2)],
            "the original edge is unchanged"
        );
        assert!(graph.vertex(VId(9)).is_some(), "B's vertex is durable");
        assert!(graph.vertex(VId(3)).is_some(), "A's write is durable");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
