//! **Staging a hop does not move the certificate**
//! (`fgdb-gql-two-hop-8pfw`, overlay-certificate slice).
//!
//! The plan certificate is over the BOUND PLAN and the snapshot seq —
//! never over the data, staged or durable. So a transaction that staged an
//! `:S` continuation answers the composed MATCH differently from the base
//! ([4, 5] vs [4]) while certifying it IDENTICALLY to the pinned pass at
//! its basis: `WriteTxn::gql_plan_certificate` must equal
//! `Database::gql_plan_certificate_at(basis)` as a whole — same named seq,
//! same digest. The pairing of "answers differ" with "certificates equal"
//! is the law: a certificate that drifted when rows were staged has data
//! in its transcript, and a txn certificate naming anything but the basis
//! certifies a snapshot the txn does not answer from.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const TWO_HOP_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-2hop-ov-cert-{}-{name}", std::process::id()))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn bind_rs() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_relation("S", S)
}

/// Answers differ, certificates agree: the staged continuation changes the
/// overlay's rows and must change NOTHING in the certificate.
#[test]
fn staging_changes_the_answer_and_not_the_certificate() {
    under_lab(0x0c_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-hop-cert");
        let mut db = Database::create(&commit, &dir, keys()).await.expect("creates");
        let mut r_batch = WriteBatch::new(R);
        for vid in [1u128, 2, 4] {
            r_batch.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
        }
        r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, r_batch).await.expect("R edge commits");
        let mut s_batch = WriteBatch::new(S);
        s_batch.add_edge(EId(20), VId(2), VId(4), vec![]);
        db.write(&commit, s_batch).await.expect("durable S edge commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let basis = txn.basis();
        let mut staged = WriteBatch::new(S);
        staged.create_vertex(VId(5), vec![], vec![]);
        staged.add_edge(EId(21), VId(2), VId(5), vec![]);
        txn.write(&mut db, staged).expect("stages the continuation");

        // The answers: staged continuation composes for the txn, not for
        // the shared handle — at the same instant.
        assert_eq!(
            txn.execute_gql(&db, TWO_HOP_C, &bind_rs())
                .expect("the txn's two-hop RETURN c executes"),
            vec![VId(4), VId(5)],
            "the staged :S continuation composes through the overlay"
        );
        assert_eq!(
            db.execute_gql(TWO_HOP_C, &bind_rs())
                .expect("the base two-hop RETURN c executes"),
            vec![VId(4)],
            "DIRTY READ: the staged continuation leaked into the shared handle"
        );

        // The certificates: the txn's equals the pinned pass at its basis,
        // WHOLE — same named seq, same digest. Staged rows changed the
        // answer above and may change nothing here: the certificate is over
        // plan + seq, and the txn's seq is its basis.
        let txn_cert = txn
            .gql_plan_certificate(TWO_HOP_C, &bind_rs())
            .expect("the txn's two-hop plan certificate is issued");
        let pinned_cert = db
            .gql_plan_certificate_at(TWO_HOP_C, &bind_rs(), basis)
            .expect("the pinned two-hop plan certificate is issued");
        assert_eq!(
            txn_cert.snapshot_seq, basis,
            "the txn certificate names its basis"
        );
        assert_eq!(
            txn_cert, pinned_cert,
            "staging a hop moved the answer and must not move the \
             certificate: a difference here means data (or the wrong seq) \
             entered the transcript"
        );
        txn.abort();
    });
}
