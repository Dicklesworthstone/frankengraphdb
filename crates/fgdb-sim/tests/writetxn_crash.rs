use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, CrashPoint, Database, DatabaseKeys, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_sim::replay;
use fgdb_types::context::{CommitCx, PurposeContexts, TxnCx};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, GraphId, VId};
use std::path::{Path, PathBuf};

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
    std::env::temp_dir().join(format!("fgdb-writetxn-crash-{}-{name}", std::process::id()))
}

fn property_update(value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RELATION);
    batch.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(value)));
    batch
}

async fn seeded_database(cx: &CommitCx, dir: &Path) -> Database {
    let mut database = Database::create(cx, dir, engine_keys())
        .await
        .expect("create product database");
    let mut seed = WriteBatch::new(RELATION);
    seed.create_vertex(VId(1), vec![], vec![(PROPERTY, CanonicalScalar::Int(0))]);
    database.write(cx, seed).await.expect("seed vertex commits");
    database
}

async fn reference_property(cx: &CommitCx, dir: &Path) -> CanonicalScalar {
    let coordinator = CommitCoordinator::open(cx, dir, oracle_keys())
        .await
        .expect("independent oracle coordinator opens durable stream");
    replay(cx, &coordinator)
        .await
        .expect("durable stream replays into ReferenceDatabase")
        .database
        .graph(GRAPH, BRANCH)
        .expect("reference coordinate exists")
        .vertex(VId(1))
        .expect("seed vertex exists")
        .props
        .get(&PROPERTY)
        .expect("seed property exists")
        .clone()
}

fn assert_no_pins(txn_cx: &TxnCx) {
    assert_eq!(
        txn_cx.outstanding_obligations(),
        0,
        "terminal WriteTxn path must release its snapshot pin"
    );
}

#[test]
fn crash_before_capsule_releases_pin_and_reference_sees_only_seed() {
    let dir = scratch("before-capsule");
    let ((), report) = run_async_under_lab(0x78_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = seeded_database(&commit_cx, &dir).await;

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        transaction
            .write(&mut database, property_update(11))
            .expect("prepare property update");
        assert_eq!(txn_cx.outstanding_obligations(), 1);
        let crashed = transaction
            .commit_with_crash(&mut database, &commit_cx, Some(CrashPoint::BeforeCapsule))
            .await;
        assert!(
            crashed.is_err(),
            "BeforeCapsule must stop the transaction commit"
        );
        assert_no_pins(&txn_cx);
        drop(database);

        assert_eq!(
            reference_property(&commit_cx, &dir).await,
            CanonicalScalar::Int(0),
            "BeforeCapsule is pre-D1 and pre-D2, so replay sees only the seed"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn crash_seam_with_none_is_a_durable_commit() {
    let dir = scratch("none-commits");
    let ((), report) = run_async_under_lab(0x78_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = seeded_database(&commit_cx, &dir).await;

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        transaction
            .write(&mut database, property_update(22))
            .expect("prepare property update");
        let committed = transaction
            .commit_with_crash(&mut database, &commit_cx, None)
            .await
            .expect("None follows the normal durable commit path");
        assert_eq!(
            committed.0, 2,
            "seed is seq 1 and the txn advances to seq 2"
        );
        assert_no_pins(&txn_cx);
        drop(database);

        assert_eq!(
            reference_property(&commit_cx, &dir).await,
            CanonicalScalar::Int(22),
            "independent replay sees the crash-free transaction write"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
