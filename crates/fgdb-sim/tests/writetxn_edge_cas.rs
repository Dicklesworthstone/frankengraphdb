use asupersync::lab::run_async_under_lab;
use fgdb::{
    CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch, WriteMismatchPolicy, WriteTxnError,
};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const EDGE: EId = EId(10);
const PROPERTY: PropertyKeyId = PropertyKeyId(7);
const OLD: i64 = 5;
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
        "fgdb-writetxn-edge-cas-{}-{name}",
        std::process::id()
    ))
}

fn seed_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(
        EDGE,
        VId(1),
        VId(2),
        vec![(PROPERTY, CanonicalScalar::Int(OLD))],
    );
    batch
}

fn matching_cas(value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.compare_and_set_edge_property(
        EDGE,
        PROPERTY,
        Some(CanonicalScalar::Int(OLD)),
        CanonicalScalar::Int(value),
        WriteMismatchPolicy::AbortWrite,
    );
    batch
}

#[test]
fn aborted_matching_edge_cas_replays_the_old_value() {
    let dir = scratch("abort-preserves-old");
    let ((), report) = run_async_under_lab(0x89_01, |root| async move {
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
            .write(&mut database, matching_cas(11))
            .expect("stage matching edge CAS");
        assert_eq!(
            transaction
                .edge(&database, EDGE)
                .expect("overlay edge")
                .expect("edge remains live")
                .props,
            vec![(PROPERTY, CanonicalScalar::Int(11))]
        );
        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        assert_eq!(database.frontier().expect("abort leaves handle healthy"), frontier_before);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let edge = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists")
            .edge(EDGE)
            .expect("edge remains durable");
        assert_eq!(edge.props.get(&PROPERTY), Some(&CanonicalScalar::Int(OLD)));
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_matching_edge_cas_replays_the_new_value() {
    let dir = scratch("commit-new-value");
    let ((), report) = run_async_under_lab(0x89_02, |root| async move {
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
            .write(&mut database, matching_cas(22))
            .expect("stage matching edge CAS");
        let committed = transaction
            .commit(&mut database, &commit_cx)
            .await
            .expect("commit matching edge CAS");
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
        let edge = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists")
            .edge(EDGE)
            .expect("edge remains durable");
        assert_eq!(edge.props.get(&PROPERTY), Some(&CanonicalScalar::Int(22)));
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn concurrent_matching_cas_aborts_reader_and_replays_only_writer() {
    let dir = scratch("cas-read-conflict");
    let ((), report) = run_async_under_lab(0x89_03, |root| async move {
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

        let mut reader = database.begin(&txn_cx).expect("begin edge reader");
        let mut writer = database.begin(&txn_cx).expect("begin CAS writer");
        assert_eq!(
            reader
                .edge(&database, EDGE)
                .expect("transactional edge read succeeds")
                .expect("observed edge exists")
                .props,
            vec![(PROPERTY, CanonicalScalar::Int(OLD))]
        );
        let mut disjoint = WriteBatch::new(R);
        disjoint.create_vertex(VId(3), vec![], vec![]);
        reader
            .write(&mut database, disjoint)
            .expect("reader stages disjoint vertex");
        writer
            .write(&mut database, matching_cas(33))
            .expect("writer stages matching edge CAS");
        writer
            .commit(&mut database, &commit_cx)
            .await
            .expect("CAS writer commits first");

        let refusal = reader.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(&refusal, Err(WriteTxnError::Write(_))),
            "edge CAS conflict must be a typed Write abort: {refusal:?}"
        );
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("FG-LAW-FCW-READ-01"),
            "edge CAS conflict must name READ-01: {rendered}"
        );
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 2, "seed and CAS writer only");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(
            graph.edge(EDGE).expect("edge remains durable").props.get(&PROPERTY),
            Some(&CanonicalScalar::Int(33)),
            "B's CAS result is durable"
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
