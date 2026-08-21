use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch, WriteTxnError};
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
const EDGE: EId = EId(10);
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
        "fgdb-writetxn-delete-vertex-{}-{name}",
        std::process::id()
    ))
}

fn seed_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EDGE, VId(1), VId(2), vec![]);
    batch
}

fn delete_vertex() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.delete_vertex(VId(2));
    batch
}

#[test]
fn aborted_vertex_delete_preserves_vertex_and_edge_in_reference_replay() {
    let dir = scratch("abort-preserves-cascade");
    let ((), report) = run_async_under_lab(0x87_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable R edge");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        transaction
            .write(&mut database, delete_vertex())
            .expect("stage vertex deletion");
        assert!(transaction.vertex(&database, VId(2)).expect("overlay vertex").is_none());
        assert!(transaction.edge(&database, EDGE).expect("overlay edge").is_none());
        assert!(
            transaction
                .neighbours(&database, VId(1), R)
                .expect("overlay neighbours")
                .is_empty()
        );
        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        assert_eq!(database.frontier().expect("abort leaves handle healthy"), frontier_before);
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
            .expect("reference coordinate exists");
        assert!(graph.vertex(VId(2)).is_some(), "abort preserves the vertex");
        assert!(graph.edge(EDGE).is_some(), "abort preserves the incident edge");
        assert_eq!(graph.neighbours(VId(1), R), vec![VId(2)]);
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_vertex_delete_cascades_in_reference_replay() {
    let dir = scratch("commit-cascades");
    let ((), report) = run_async_under_lab(0x87_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable R edge");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        transaction
            .write(&mut database, delete_vertex())
            .expect("stage vertex deletion");
        let committed = transaction
            .commit(&mut database, &commit_cx)
            .await
            .expect("commit staged vertex deletion");
        assert_eq!(committed.0, frontier_before.0 + 1);
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), committed.0 as usize);
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert!(graph.vertex(VId(2)).is_none(), "committed vertex deletion is durable");
        assert!(graph.edge(EDGE).is_none(), "incident edge is cascade-deleted");
        assert!(graph.neighbours(VId(1), R).is_empty());
        assert!(graph.vertex(VId(1)).is_some(), "source vertex remains");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn concurrent_vertex_delete_aborts_reader_and_replays_only_deleter() {
    let dir = scratch("vertex-read-conflict");
    let ((), report) = run_async_under_lab(0x87_03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable R edge");

        let mut reader = database.begin(&txn_cx).expect("begin vertex reader");
        let mut deleter = database.begin(&txn_cx).expect("begin vertex deleter");
        assert_eq!(
            reader
                .neighbours(&database, VId(1), R)
                .expect("transactional neighbours read succeeds"),
            vec![VId(2)]
        );
        assert!(reader.vertex(&database, VId(2)).expect("vertex observation").is_some());
        let mut disjoint = WriteBatch::new(R);
        disjoint.create_vertex(VId(3), vec![], vec![]);
        reader
            .write(&mut database, disjoint)
            .expect("reader stages disjoint vertex");
        deleter
            .write(&mut database, delete_vertex())
            .expect("deleter stages observed vertex deletion");
        deleter
            .commit(&mut database, &commit_cx)
            .await
            .expect("vertex deleter commits first");

        let refusal = reader.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(&refusal, Err(WriteTxnError::Write(_))),
            "vertex deletion must be a typed Write abort: {refusal:?}"
        );
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("FG-LAW-FCW-READ-01"),
            "vertex deletion conflict must name READ-01: {rendered}"
        );
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 2, "seed and vertex deleter only");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert!(graph.vertex(VId(2)).is_none(), "B's vertex deletion is durable");
        assert!(graph.edge(EDGE).is_none(), "B's cascade is durable");
        assert!(graph.neighbours(VId(1), R).is_empty());
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
