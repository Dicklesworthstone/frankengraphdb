//! **Plan certificate as-of a sequence, across reopen**
//! (`fgdb-w4-g1-txn-core-qpmg.22`).
//!
//! The plan-certificate twin of `gql_exec_certified_at_oracle.rs`:
//! `gql_plan_certificate_at` at `S1` must name exactly `S1` while the live
//! form names the live frontier, and the two digests must differ — the
//! sequence is hashed into the plan certificate, so a certificate that
//! omitted it would collapse the pair. A cold reopen re-mints the as-of
//! certificate byte-identically: it is a function of (plan, seq), never of
//! the session that minted it.

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
        "fgdb-gql-plan-cert-at-oracle-{}-{name}",
        std::process::id()
    ))
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// The first epoch: `1-[:R, EId(10)]->2`. Its commit sequence is `S1`.
fn seed_first_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch
}

/// The second epoch: `3-[:R, EId(11)]->5`, advancing the frontier past `S1`.
fn seed_second_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(3), vec![], vec![]);
    batch.create_vertex(VId(5), vec![], vec![]);
    batch.add_edge(EId(11), VId(3), VId(5), vec![]);
    batch
}

#[test]
fn plan_certificate_at_s1_names_s1_and_differs_from_the_live_certificate() {
    let dir = scratch("live-session");
    let ((), report) = run_async_under_lab(0xa7_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_first_edge())
            .await
            .expect("seed first epoch");
        let s1 = database.frontier().expect("healthy S1 frontier");
        database
            .write(&commit_cx, seed_second_edge())
            .await
            .expect("seed second epoch");
        let live_frontier = database.frontier().expect("healthy live frontier");

        let at_cert = database
            .gql_plan_certificate_at(PINNED, &bind_r(), s1)
            .expect("plan certificate mints at S1");
        assert_eq!(
            at_cert.snapshot_seq, s1,
            "the as-of plan certificate names exactly the asked sequence"
        );
        let live_cert = database
            .gql_plan_certificate(PINNED, &bind_r())
            .expect("plan certificate mints at the frontier");
        assert_eq!(
            live_cert.snapshot_seq, live_frontier,
            "the live plan certificate names the live frontier"
        );
        assert_ne!(
            at_cert.snapshot_seq, live_cert.snapshot_seq,
            "the two certificates name different snapshots"
        );
        assert_ne!(
            at_cert.digest, live_cert.digest,
            "the sequence is hashed into the plan digest — same plan, \
             different snapshot, different digest"
        );

        // Determinism control within one session: minting the same as-of
        // certificate twice is byte-identical.
        assert_eq!(
            database
                .gql_plan_certificate_at(PINNED, &bind_r(), s1)
                .expect("plan certificate re-mints at S1"),
            at_cert,
            "same plan + same seq mint the same certificate"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn reopened_plan_certificate_at_s1_names_s1_with_the_same_digest() {
    let dir = scratch("reopen");
    let ((), report) = run_async_under_lab(0xa7_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_first_edge())
            .await
            .expect("seed first epoch");
        let s1 = database.frontier().expect("healthy S1 frontier");
        database
            .write(&commit_cx, seed_second_edge())
            .await
            .expect("seed second epoch");
        let before_cert = database
            .gql_plan_certificate_at(PINNED, &bind_r(), s1)
            .expect("plan certificate mints at S1 before reopen");
        drop(database);

        let reopened = Database::open(&commit_cx, &dir, engine_keys())
            .await
            .expect("cold reopen from the durable stream");
        let after_cert = reopened
            .gql_plan_certificate_at(PINNED, &bind_r(), s1)
            .expect("plan certificate mints at S1 after reopen");
        assert_eq!(
            after_cert.snapshot_seq, s1,
            "the reopened as-of certificate still names S1"
        );
        assert_eq!(
            after_cert, before_cert,
            "the whole plan certificate — digest included — is a function of \
             (plan, seq), not of the session that minted it"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
