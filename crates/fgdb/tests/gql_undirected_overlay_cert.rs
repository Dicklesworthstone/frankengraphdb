//! **Staging an undirected incidence does not move the certificate**
//! (`fgdb-w5-parsers-nje.2`, overlay-certificate slice).
//!
//! The undirected twin of `gql_two_hop_overlay_cert.rs`: a transaction that
//! staged a second edge into the shared vertex answers the undirected MATCH
//! differently from the base handle at the SAME instant ([1, 2, 3] vs
//! [1, 2]) while certifying it identically to the pinned pass at its basis —
//! same named seq, same digest, certificate equal as a whole. Answers differ,
//! certificates agree: anything else means data entered the plan transcript
//! or the txn certified a snapshot it does not answer from.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const UNDIRECTED_B: &str = "MATCH (a)-[:R]-(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-undir-ov-cert-{}-{name}", std::process::id()))
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

/// Answers differ, certificates agree — over the undirected expansion.
#[test]
fn staged_incidence_changes_the_answer_and_not_the_certificate() {
    under_lab(0x0d_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-undirected-cert");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("durable edge commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let basis = txn.basis();
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(3), vec![], vec![]);
        staged.add_edge(EId(11), VId(3), VId(2), vec![]);
        txn.write(&mut db, staged).expect("stages the second incidence");

        // The answers, at the same instant: the staged edge makes VId(2) a
        // two-way hub for the txn (so every endpoint appears both ways),
        // while the shared handle still sees only the durable pair.
        assert_eq!(
            txn.execute_gql(&db, UNDIRECTED_B, &bind_r())
                .expect("the txn's undirected RETURN b executes"),
            vec![VId(1), VId(2), VId(3)],
            "the staged incidence expands through the overlay both ways"
        );
        assert_eq!(
            db.execute_gql(UNDIRECTED_B, &bind_r())
                .expect("the base undirected RETURN b executes"),
            vec![VId(1), VId(2)],
            "DIRTY READ: the staged incidence leaked into the shared handle"
        );

        // The certificates: the txn's equals the pinned pass at its basis,
        // WHOLE. Staging moved the answer above and must move nothing here.
        let txn_cert = txn
            .gql_plan_certificate(UNDIRECTED_B, &bind_r())
            .expect("the txn's undirected plan certificate is issued");
        let pinned_cert = db
            .gql_plan_certificate_at(UNDIRECTED_B, &bind_r(), basis)
            .expect("the pinned undirected plan certificate is issued");
        assert_eq!(
            txn_cert.snapshot_seq, basis,
            "the txn certificate names its basis"
        );
        assert_eq!(
            txn_cert, pinned_cert,
            "staging an incidence moved the answer and must not move the \
             certificate: a difference here means data (or the wrong seq) \
             entered the transcript"
        );
        txn.abort();
    });
}
