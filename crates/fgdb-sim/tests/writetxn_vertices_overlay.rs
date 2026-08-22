use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch, WriteTxnError};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, GraphId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
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
        "fgdb-writetxn-vertices-overlay-{}-{name}",
        std::process::id()
    ))
}

fn seed_vertex() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch
}

fn vertex_ids(vertices: &[fgdb::VertexRow]) -> Vec<VId> {
    vertices.iter().map(|vertex| vertex.vid).collect()
}

#[test]
fn aborted_vertices_overlay_replays_only_the_seed_vertex() {
    let dir = scratch("abort-created-vertex");
    let ((), report) = run_async_under_lab(0x8d_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_vertex())
            .await
            .expect("seed durable vertex");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut create = WriteBatch::new(R);
        create.create_vertex(VId(2), vec![], vec![]);
        transaction
            .write(&mut database, create)
            .expect("stage second vertex");
        assert_eq!(
            vertex_ids(&transaction.vertices(&database).expect("overlay vertices")),
            vec![VId(1), VId(2)]
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
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(
            graph
                .iter_vertices()
                .map(|(vid, _)| vid)
                .collect::<Vec<_>>(),
            vec![VId(1)]
        );
        assert!(
            graph.vertex(VId(2)).is_none(),
            "aborted vertex is not durable"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_vertices_overlay_deletion_replays_an_empty_vertex_set() {
    let dir = scratch("commit-deleted-vertex");
    let ((), report) = run_async_under_lab(0x8d_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_vertex())
            .await
            .expect("seed durable vertex");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut delete = WriteBatch::new(R);
        delete.delete_vertex(VId(1));
        transaction
            .write(&mut database, delete)
            .expect("stage vertex deletion");
        assert!(
            transaction
                .vertices(&database)
                .expect("overlay vertices")
                .is_empty()
        );
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
        assert_eq!(graph.iter_vertices().count(), 0);
        assert!(
            graph.vertex(VId(1)).is_none(),
            "committed deletion is durable"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn concurrent_deletion_of_vertex_observed_by_vertices_aborts_and_replays_only_deleter() {
    let dir = scratch("vertices-read-conflict");
    let ((), report) = run_async_under_lab(0x8d_03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_vertex())
            .await
            .expect("seed durable vertex");

        let mut reader = database.begin(&txn_cx).expect("begin vertices reader");
        let mut deleter = database.begin(&txn_cx).expect("begin vertex deleter");
        assert_eq!(
            vertex_ids(
                &reader
                    .vertices(&database)
                    .expect("transactional vertices read")
            ),
            vec![VId(1)]
        );
        let mut disjoint = WriteBatch::new(R);
        disjoint.create_vertex(VId(3), vec![], vec![]);
        reader
            .write(&mut database, disjoint)
            .expect("reader stages disjoint vertex");
        let mut delete = WriteBatch::new(R);
        delete.delete_vertex(VId(1));
        deleter
            .write(&mut database, delete)
            .expect("deleter stages observed vertex deletion");
        deleter
            .commit(&mut database, &commit_cx)
            .await
            .expect("vertex deleter commits first");

        let refusal = reader.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(&refusal, Err(WriteTxnError::Write(_))),
            "vertices read conflict must be a typed Write abort: {refusal:?}"
        );
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("FG-LAW-FCW-READ-01"),
            "vertices read conflict must name READ-01: {rendered}"
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
        assert_eq!(graph.iter_vertices().count(), 0);
        assert!(graph.vertex(VId(1)).is_none(), "B's deletion is durable");
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
