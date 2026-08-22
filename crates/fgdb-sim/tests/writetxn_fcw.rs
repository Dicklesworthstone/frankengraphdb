use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch, WriteError, WriteTxnError};
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
    std::env::temp_dir().join(format!("fgdb-writetxn-fcw-{}-{name}", std::process::id()))
}

fn property_update(value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RELATION);
    batch.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(value)));
    batch
}

#[test]
fn overlapping_write_txns_are_fcw_and_abort_is_trace_free() {
    let dir = scratch("reference-and-abort");
    let ((), report) = run_async_under_lab(0x77_81, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");

        let mut seed = WriteBatch::new(RELATION);
        seed.create_vertex(VId(1), vec![], vec![(PROPERTY, CanonicalScalar::Int(0))]);
        database
            .write(&commit_cx, seed)
            .await
            .expect("seed vertex commits");

        let mut first = database
            .begin(&txn_cx)
            .expect("begin first pinned transaction");
        let mut second = database
            .begin(&txn_cx)
            .expect("begin second pinned transaction");
        assert_eq!(
            txn_cx.outstanding_obligations(),
            2,
            "each open WriteTxn must own one snapshot pin"
        );
        first
            .write(&mut database, property_update(11))
            .expect("prepare first overlapping update");
        second
            .write(&mut database, property_update(22))
            .expect("prepare second overlapping update");

        let winner_seq = first
            .commit(&mut database, &commit_cx)
            .await
            .expect("first committer wins");
        assert_eq!(txn_cx.outstanding_obligations(), 1);
        let loser = second.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(
                loser,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
            ),
            "second overlapping pinned transaction must lose under FCW: {loser:?}"
        );
        assert_eq!(
            txn_cx.outstanding_obligations(),
            0,
            "both terminal transaction paths must release their pins"
        );

        let frontier_before_abort = database.frontier().expect("healthy frontier");
        assert_eq!(frontier_before_abort, winner_seq);
        let mut aborted = database.begin(&txn_cx).expect("begin transaction to abort");
        aborted
            .write(&mut database, property_update(33))
            .expect("prepare update that will be aborted");
        assert_eq!(txn_cx.outstanding_obligations(), 1);
        aborted.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        assert_eq!(
            database.frontier().expect("abort leaves handle readable"),
            frontier_before_abort,
            "abort must not advance the durable stream"
        );
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(
            coordinator.chain().len(),
            frontier_before_abort.0 as usize,
            "losing and aborted transactions leave no marker"
        );
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let vertex = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists")
            .vertex(VId(1))
            .expect("winner's vertex is durable");
        assert_eq!(
            vertex.props.get(&PROPERTY),
            Some(&CanonicalScalar::Int(11)),
            "reference state must contain only the winning property update"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
