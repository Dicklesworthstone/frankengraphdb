use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch, WriteTxnError};
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
        "fgdb-writetxn-readset-{}-{name}",
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

fn set_property(vid: VId, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RELATION);
    batch.set_vertex_property(vid, PROPERTY, Some(CanonicalScalar::Int(value)));
    batch
}

#[test]
fn changed_read_aborts_typed_and_replay_contains_only_the_concurrent_writer() {
    let dir = scratch("read-conflict");
    let ((), report) = run_async_under_lab(0x7c_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, create_vertex(VId(1), 0))
            .await
            .expect("seed read target");

        let mut reader = database.begin(&txn_cx).expect("begin reader transaction");
        let mut writer = database.begin(&txn_cx).expect("begin concurrent writer");
        let observed = reader
            .vertex(&database, VId(1))
            .expect("transactional vertex read succeeds")
            .expect("seed vertex exists");
        assert_eq!(
            observed.props,
            vec![(PROPERTY, CanonicalScalar::Int(0))]
        );
        reader
            .write(&mut database, create_vertex(VId(3), 30))
            .expect("reader stages a write disjoint from its read key");
        writer
            .write(&mut database, set_property(VId(1), 11))
            .expect("concurrent writer stages read-key mutation");
        writer
            .commit(&mut database, &commit_cx)
            .await
            .expect("concurrent writer commits");

        let refusal = reader.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(&refusal, Err(WriteTxnError::Write(_))),
            "read-set conflict must be a typed Write abort, not SnapshotAdvanced: {refusal:?}"
        );
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("FG-LAW-FCW-READ-01"),
            "typed abort must name the read-set law: {rendered}"
        );
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 2, "seed and writer only");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(
            graph.vertex(VId(1)).expect("seed vertex remains").props.get(&PROPERTY),
            Some(&CanonicalScalar::Int(11)),
            "only the concurrent writer's property is durable"
        );
        assert!(
            graph.vertex(VId(3)).is_none(),
            "the read-conflicted transaction leaves no durable write residue"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn disjoint_concurrent_create_preserves_the_readers_later_commit() {
    let dir = scratch("disjoint-commits");
    let ((), report) = run_async_under_lab(0x7c_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, create_vertex(VId(1), 0))
            .await
            .expect("seed read target");

        let mut reader = database.begin(&txn_cx).expect("begin reader transaction");
        let mut creator = database.begin(&txn_cx).expect("begin disjoint creator");
        assert!(
            reader
                .vertex(&database, VId(1))
                .expect("transactional vertex read succeeds")
                .is_some()
        );
        creator
            .write(&mut database, create_vertex(VId(2), 20))
            .expect("stage disjoint vertex creation");
        creator
            .commit(&mut database, &commit_cx)
            .await
            .expect("disjoint creator commits");

        reader
            .write(&mut database, set_property(VId(1), 31))
            .expect("stage mutation of unchanged read target");
        reader
            .commit(&mut database, &commit_cx)
            .await
            .expect("disjoint concurrent write does not invalidate the read set");
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
            graph.vertex(VId(1)).expect("reader's vertex remains").props.get(&PROPERTY),
            Some(&CanonicalScalar::Int(31))
        );
        assert_eq!(
            graph.vertex(VId(2)).expect("creator's vertex is durable").props.get(&PROPERTY),
            Some(&CanonicalScalar::Int(20)),
            "independent replay contains both disjoint commits"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
