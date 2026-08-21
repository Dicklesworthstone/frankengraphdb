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
        "fgdb-writetxn-delete-overlay-{}-{name}",
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

fn delete_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.delete_edge(EDGE);
    batch
}

#[test]
fn aborted_delete_overlay_preserves_edge_in_reference_replay() {
    let dir = scratch("abort-preserves-edge");
    let ((), report) = run_async_under_lab(0x83_01, |root| async move {
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
            .write(&mut database, delete_edge())
            .expect("stage edge deletion");
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
        assert_eq!(graph.neighbours(VId(1), R), vec![VId(2)]);
        assert!(graph.edge(EDGE).is_some(), "abort preserves the durable edge");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_delete_overlay_removes_edge_from_reference_replay() {
    let dir = scratch("commit-removes-edge");
    let ((), report) = run_async_under_lab(0x83_02, |root| async move {
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
            .write(&mut database, delete_edge())
            .expect("stage edge deletion");
        assert!(transaction.edge(&database, EDGE).expect("overlay edge").is_none());
        let committed = transaction
            .commit(&mut database, &commit_cx)
            .await
            .expect("commit staged deletion");
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
        assert!(graph.neighbours(VId(1), R).is_empty());
        assert!(graph.edge(EDGE).is_none(), "committed deletion is durable");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn concurrent_delete_of_observed_adjacency_aborts_and_replays_only_deleter() {
    let dir = scratch("delete-read-conflict");
    let ((), report) = run_async_under_lab(0x83_03, |root| async move {
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

        let mut reader = database.begin(&txn_cx).expect("begin adjacency reader");
        let mut deleter = database.begin(&txn_cx).expect("begin edge deleter");
        assert_eq!(
            reader
                .neighbours(&database, VId(1), R)
                .expect("transactional neighbours read succeeds"),
            vec![VId(2)]
        );
        let mut disjoint = WriteBatch::new(R);
        disjoint.create_vertex(VId(3), vec![], vec![]);
        reader
            .write(&mut database, disjoint)
            .expect("reader stages disjoint vertex");
        deleter
            .write(&mut database, delete_edge())
            .expect("deleter stages observed edge deletion");
        deleter
            .commit(&mut database, &commit_cx)
            .await
            .expect("edge deleter commits first");

        let refusal = reader.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(&refusal, Err(WriteTxnError::Write(_))),
            "adjacency deletion must be a typed Write abort: {refusal:?}"
        );
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("FG-LAW-FCW-READ-01"),
            "delete conflict must name READ-01: {rendered}"
        );
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 2, "seed and deleter only");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert!(graph.neighbours(VId(1), R).is_empty());
        assert!(graph.edge(EDGE).is_none(), "B's deletion is durable");
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
