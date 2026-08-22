//! **Staging a self-loop moves the `WHERE a = b` answer, not the
//! certificate** (`fgdb-w5-parsers-nje.6`, overlay-certificate slice).
//!
//! The equality predicate's turn on the overlay-certificate law: a STAGED
//! self-loop joins the txn's `WHERE a = b` rows beside the durable one —
//! with a DISTINCT vertex, so the shared handle answering without it is a
//! non-vacuous dirty-read pairing — while the plan certificate, which is
//! over plan + basis seq and never data, must not move at all:
//! `WriteTxn::gql_plan_certificate` equals
//! `Database::gql_plan_certificate_at(basis)` as a whole struct.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const EQ_B: &str = "MATCH (a)-[:R]->(b) WHERE a = b RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-eq-ov-cert-{}-{name}", std::process::id()))
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

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// Answers move with the staged self-loop, the certificate does not.
#[test]
fn staging_a_self_loop_moves_the_answer_and_not_the_certificate() {
    under_lab(0x5e_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-loop-cert");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(5), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(5), VId(5), vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let basis = txn.basis();
        // A DISTINCT staged self-loop: 9, not 5, so the base excluding it
        // below is a real dirty-read check and not a dedup accident.
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(9), vec![], vec![]);
        staged.add_edge(EId(12), VId(9), VId(9), vec![]);
        txn.write(&mut db, staged).expect("stages the self-loop");

        // The answers: both self-loops for the txn, the durable one alone
        // for the shared handle — and the ordinary edge's 2 for neither.
        let overlay = txn
            .execute_gql(&db, EQ_B, &bind_r())
            .expect("the txn's WHERE a = b executes");
        assert!(
            overlay.contains(&VId(5)) && overlay.contains(&VId(9)),
            "the durable and the staged self-loops both answer through the \
             overlay: {overlay:?}"
        );
        assert!(
            !overlay.contains(&VId(2)),
            "the ordinary edge never satisfies a = b: {overlay:?}"
        );
        let base = db
            .execute_gql(EQ_B, &bind_r())
            .expect("the base WHERE a = b executes");
        assert!(
            base.contains(&VId(5)) && !base.contains(&VId(9)),
            "DIRTY READ: the staged self-loop leaked into the shared \
             handle: {base:?}"
        );

        // The certificates: the txn's equals the pinned pass at its basis,
        // WHOLE — same named seq, same digest. The staged loop changed the
        // answer above and may change nothing here.
        let txn_cert = txn
            .gql_plan_certificate(EQ_B, &bind_r())
            .expect("the txn's plan certificate is issued");
        let pinned_cert = db
            .gql_plan_certificate_at(EQ_B, &bind_r(), basis)
            .expect("the pinned plan certificate is issued");
        assert_eq!(
            txn_cert.snapshot_seq, basis,
            "the txn certificate names its basis"
        );
        assert_eq!(
            txn_cert, pinned_cert,
            "staging a self-loop moved the answer and must not move the \
             certificate: a difference here means data (or the wrong seq) \
             entered the transcript"
        );
        txn.abort();
    });
}
