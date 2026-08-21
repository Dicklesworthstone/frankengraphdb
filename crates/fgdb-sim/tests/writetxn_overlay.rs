use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, GraphId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const RELATION: RelationId = RelationId(1);
const PROPERTY: PropertyKeyId = PropertyKeyId(7);
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
        "fgdb-writetxn-overlay-{}-{name}",
        std::process::id()
    ))
}

fn create_vertex(vid: VId, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RELATION);
    batch.create_vertex(
        vid,
        vec![],
        vec![(PROPERTY, CanonicalScalar::Int(value))],
    );
    batch
}

#[test]
fn overlay_reads_staged_vertex_but_abort_leaves_no_durable_vertex() {
    let dir = scratch("abort-is-private");
    let ((), report) = run_async_under_lab(0x7a_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");

        database
            .write(&commit_cx, create_vertex(VId(1), 1))
            .await
            .expect("seed durable reference coordinate");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        transaction
            .write(&mut database, create_vertex(VId(2), 22))
            .expect("stage vertex creation");
        let staged = transaction
            .vertex(&database, VId(2))
            .expect("overlay read succeeds")
            .expect("overlay exposes staged vertex");
        assert_eq!(
            staged.props,
            vec![(PROPERTY, CanonicalScalar::Int(22))],
            "the transaction reads its own staged property"
        );
        assert!(
            database.vertex(VId(2)).expect("base read succeeds").is_none(),
            "the durable database view must not expose the private overlay"
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
            graph.vertex(VId(2)).is_none(),
            "aborted overlay state must leave no durable replay residue"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_overlay_vertex_is_visible_to_independent_replay() {
    let dir = scratch("commit-is-durable");
    let ((), report) = run_async_under_lab(0x7a_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        transaction
            .write(&mut database, create_vertex(VId(2), 42))
            .expect("stage vertex creation");
        assert!(
            transaction
                .vertex(&database, VId(2))
                .expect("overlay read succeeds")
                .is_some(),
            "staged vertex is readable before commit"
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
        let vertex = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists")
            .vertex(VId(2))
            .expect("committed overlay vertex is durable");
        assert_eq!(
            vertex.props.get(&PROPERTY),
            Some(&CanonicalScalar::Int(42)),
            "independent replay sees the committed property"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
