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
        "fgdb-writetxn-multibatch-{}-{name}",
        std::process::id()
    ))
}

#[test]
fn two_writes_commit_as_one_capsule_and_replay_the_composed_state() {
    let dir = scratch("one-capsule");
    let ((), report) = run_async_under_lab(0x79_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        let mut create = WriteBatch::new(RELATION);
        create.create_vertex(VId(2), vec![], vec![]);
        transaction
            .write(&mut database, create)
            .expect("stage vertex creation");

        let mut property = WriteBatch::new(RELATION);
        property.set_vertex_property(
            VId(2),
            PROPERTY,
            Some(CanonicalScalar::Int(42)),
        );
        transaction
            .write(&mut database, property)
            .expect("compose property update into the transaction");

        let committed = transaction
            .commit(&mut database, &commit_cx)
            .await
            .expect("commit composed transaction");
        let frontier = database.frontier().expect("healthy committed frontier");
        assert_eq!(committed, frontier);
        assert_eq!(frontier.0, 1, "two writes publish exactly one commit");
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(
            coordinator.chain().len(),
            frontier.0 as usize,
            "the transaction emits one marker, not one marker per write"
        );
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let vertex = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists")
            .vertex(VId(2))
            .expect("composed vertex creation is durable");
        assert_eq!(
            vertex.props.get(&PROPERTY),
            Some(&CanonicalScalar::Int(42)),
            "the one capsule contains both staged writes"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn abort_after_a_write_preserves_the_pre_begin_stream_and_graph() {
    let dir = scratch("abort-trace-free");
    let ((), report) = run_async_under_lab(0x79_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");

        let mut seed = WriteBatch::new(RELATION);
        seed.create_vertex(
            VId(1),
            vec![],
            vec![(PROPERTY, CanonicalScalar::Int(9))],
        );
        database
            .write(&commit_cx, seed)
            .await
            .expect("seed pre-begin graph");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        let mut create = WriteBatch::new(RELATION);
        create.create_vertex(VId(3), vec![], vec![]);
        transaction
            .write(&mut database, create)
            .expect("stage vertex that will be aborted");
        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        assert_eq!(
            database.frontier().expect("abort leaves handle healthy"),
            frontier_before,
            "abort must not advance the stream"
        );
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(
            coordinator.chain().len(),
            frontier_before.0 as usize,
            "aborted write leaves the marker chain unmoved"
        );
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable prefix replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("seeded reference coordinate exists");
        assert_eq!(
            graph.iter_vertices().map(|(vid, _)| vid).collect::<Vec<_>>(),
            vec![VId(1)],
            "replay must match the graph that existed before begin"
        );
        assert_eq!(
            graph.vertex(VId(1)).expect("seed vertex remains").props.get(&PROPERTY),
            Some(&CanonicalScalar::Int(9))
        );
        assert!(graph.vertex(VId(3)).is_none(), "aborted vertex is not durable");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
