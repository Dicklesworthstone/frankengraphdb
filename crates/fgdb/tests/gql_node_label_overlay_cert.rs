//! **Staging a labeled source moves the answer, not the certificate**
//! (`fgdb-w5-parsers-nje.5`, overlay-certificate slice).
//!
//! The labeled twin of `gql_undirected_overlay_cert.rs`: a staged
//! `:L` source changes what the txn's labeled MATCH answers — and must
//! change NOTHING in the plan certificate, which is over plan + basis
//! seq, never data. The staged source is given a DISTINCT destination
//! (the wave's "if distinct" arm) so the overlay/base row pairing is
//! non-vacuous: same-destination staging would make both sides answer
//! `[2]` and the dirty-read assertion would prove nothing.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(3);
const LABELED_B: &str = "MATCH (a:L)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-label-ov-cert-{}-{name}", std::process::id()))
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

fn bind_r_l() -> RelationBind {
    RelationBind::new().with_relation("R", R).with_label("L", L)
}

/// Answers move with staging, the certificate does not.
#[test]
fn staging_a_labeled_source_moves_the_answer_and_not_the_certificate() {
    under_lab(0x1f_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-labeled-cert");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let basis = txn.basis();
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(9), vec![L], vec![]);
        staged.create_vertex(VId(6), vec![], vec![]);
        staged.add_edge(EId(11), VId(9), VId(6), vec![]);
        txn.write(&mut db, staged)
            .expect("stages the labeled source");

        // The answers: durable 2 for both, staged 6 for the txn only.
        let overlay = txn
            .execute_gql(&db, LABELED_B, &bind_r_l())
            .expect("the txn's labeled MATCH executes");
        assert!(
            overlay.contains(&VId(2)) && overlay.contains(&VId(6)),
            "the durable and the staged :L sources both answer through the \
             overlay: {overlay:?}"
        );
        let base = db
            .execute_gql(LABELED_B, &bind_r_l())
            .expect("the base labeled MATCH executes");
        assert!(
            base.contains(&VId(2)) && !base.contains(&VId(6)),
            "DIRTY READ: the staged destination leaked into the shared \
             handle: {base:?}"
        );

        // The certificates: the txn's equals the pinned pass at its basis,
        // WHOLE — same named seq, same digest. Staged rows changed the
        // answer above and may change nothing here.
        let txn_cert = txn
            .gql_plan_certificate(LABELED_B, &bind_r_l())
            .expect("the txn's labeled plan certificate is issued");
        let pinned_cert = db
            .gql_plan_certificate_at(LABELED_B, &bind_r_l(), basis)
            .expect("the pinned labeled plan certificate is issued");
        assert_eq!(
            txn_cert.snapshot_seq, basis,
            "the txn certificate names its basis"
        );
        assert_eq!(
            txn_cert, pinned_cert,
            "staging a labeled source moved the answer and must not move \
             the certificate: a difference here means data (or the wrong \
             seq) entered the transcript"
        );
        txn.abort();
    });
}
