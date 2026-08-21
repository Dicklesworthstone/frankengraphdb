//! **`WriteTxn::gql_plan_certificate` names the pinned basis, not the live
//! frontier** (`fgdb-w4-g1-txn-core-qpmg.23`).
//!
//! The transactional twin of `gql_plan_certificate_at_oracle.rs`: a pinned
//! transaction's plan certificate must name `txn.basis()` — the frontier at
//! `begin` — even after later autocommit writes advance the live frontier,
//! while `Database::gql_plan_certificate` names the live frontier and
//! therefore differs in digest (the sequence is hashed into the plan
//! digest). Re-minting from the SAME open transaction after the later write
//! must be byte-identical: the pin does not move.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const DEK: [u8; 32] = [0x3c; 32];

fn engine_keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, DEK)
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-writetxn-gql-plan-cert-{}-{name}",
        std::process::id()
    ))
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// The seed epoch: `1-[:R, EId(10)]->2`. Its commit sequence is `S1`.
fn seed_first_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch
}

/// The later autocommit epoch that advances the live frontier past `S1`
/// while the transaction stays pinned.
fn later_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(3), vec![], vec![]);
    batch.create_vertex(VId(5), vec![], vec![]);
    batch.add_edge(EId(11), VId(3), VId(5), vec![]);
    batch
}

#[test]
fn txn_plan_certificate_names_the_basis_while_live_names_the_frontier() {
    let dir = scratch("basis-vs-frontier");
    let ((), report) = run_async_under_lab(0xa8_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_first_edge())
            .await
            .expect("seed first epoch");
        let s1 = database.frontier().expect("healthy S1 frontier");

        let transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        assert_eq!(transaction.basis(), s1, "the pin is the frontier at begin");

        database
            .write(&commit_cx, later_edge())
            .await
            .expect("later autocommit advances the live frontier");
        let live_frontier = database.frontier().expect("healthy live frontier");
        assert_ne!(live_frontier, s1, "the frontier genuinely advanced");

        let txn_cert = transaction
            .gql_plan_certificate(PINNED, &bind_r())
            .expect("plan certificate mints from the pinned transaction");
        assert_eq!(
            txn_cert.snapshot_seq, s1,
            "the transaction's certificate names its basis, not the live frontier"
        );
        let live_cert = database
            .gql_plan_certificate(PINNED, &bind_r())
            .expect("plan certificate mints at the live frontier");
        assert_eq!(
            live_cert.snapshot_seq, live_frontier,
            "the live certificate names the live frontier"
        );
        assert_ne!(
            txn_cert.digest, live_cert.digest,
            "same plan, different snapshot — the digests must differ"
        );

        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn the_open_txn_remints_the_same_certificate_after_the_later_write() {
    let dir = scratch("pin-does-not-move");
    let ((), report) = run_async_under_lab(0xa8_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_first_edge())
            .await
            .expect("seed first epoch");
        let s1 = database.frontier().expect("healthy S1 frontier");

        let transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        let before = transaction
            .gql_plan_certificate(PINNED, &bind_r())
            .expect("plan certificate mints before the later write");
        assert_eq!(before.snapshot_seq, s1);

        database
            .write(&commit_cx, later_edge())
            .await
            .expect("later autocommit advances the live frontier");

        let after = transaction
            .gql_plan_certificate(PINNED, &bind_r())
            .expect("plan certificate re-mints after the later write");
        assert_eq!(
            after.snapshot_seq, s1,
            "the pin did not move: the certificate still names S1"
        );
        assert_eq!(
            after, before,
            "the whole certificate — digest included — is unchanged by the \
             concurrent autocommit"
        );

        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
