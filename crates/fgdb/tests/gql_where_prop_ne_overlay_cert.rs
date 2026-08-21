//! **Staging a `k = 9` source moves the `WHERE a.k <> 1` answer, not the
//! certificate** (`fgdb-w5-parsers-nje.15`, overlay-certificate slice).
//!
//! The property-inequality edition of the answers-move/certificates-don't
//! law: the durable graph has only a `k = 1` source, so the shared
//! handle's filtered answer is EMPTY, and the staged `k = 9` source's
//! destination appears in the txn's filtered answer alone — the
//! unfiltered pairing is asserted beside it so the empty base answer is
//! attributably the predicate's and not a broken scan. The plan
//! certificate, over plan + basis seq and never data, must not move:
//! `WriteTxn::gql_plan_certificate` equals
//! `Database::gql_plan_certificate_at(basis)` as a whole struct.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const NE_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b";
const PLAIN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-prop-ne-ov-cert-{}-{name}", std::process::id()))
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

fn bind_rk() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_property("k", K)
}

/// Answers move with the staged carrier, the certificate does not.
#[test]
fn staging_a_nonequal_carrier_moves_the_answer_and_not_the_certificate() {
    under_lab(0x9f_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-carrier-cert");
        let mut db = Database::create(&commit, &dir, keys()).await.expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let basis = txn.basis();
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
        staged.create_vertex(VId(4), vec![], vec![]);
        staged.add_edge(EId(11), VId(3), VId(4), vec![]);
        txn.write(&mut db, staged).expect("stages the k=9 source");

        // Attribution first: the unfiltered pairing proves the overlay
        // merge, so the filtered emptiness below is the predicate's.
        assert_eq!(
            txn.execute_gql(&db, PLAIN_B, &bind_rk())
                .expect("the txn's unfiltered MATCH executes"),
            vec![VId(2), VId(4)],
            "the staged destination joins the overlay rows"
        );
        assert_eq!(
            db.execute_gql(PLAIN_B, &bind_rk())
                .expect("the base unfiltered MATCH executes"),
            vec![VId(2)],
            "DIRTY READ: the staged row leaked into the shared handle"
        );

        // The filtered pairing: the staged k=9 source's destination answers
        // for the txn alone — the durable k=1 source satisfies the
        // inequality nowhere.
        assert_eq!(
            txn.execute_gql(&db, NE_B, &bind_rk())
                .expect("the txn's WHERE a.k <> 1 executes"),
            vec![VId(4)],
            "the staged non-equal carrier's destination answers through the \
             overlay"
        );
        assert!(
            db.execute_gql(NE_B, &bind_rk())
                .expect("the base WHERE a.k <> 1 executes")
                .is_empty(),
            "the shared handle has only the k=1 source: its filtered answer \
             is empty and must stay empty"
        );

        // The certificates: the txn's equals the pinned pass at its basis,
        // WHOLE — same named seq, same digest. Staged data changed the
        // answer above and may change nothing here.
        let txn_cert = txn
            .gql_plan_certificate(NE_B, &bind_rk())
            .expect("the txn's plan certificate is issued");
        let pinned_cert = db
            .gql_plan_certificate_at(NE_B, &bind_rk(), basis)
            .expect("the pinned plan certificate is issued");
        assert_eq!(
            txn_cert.snapshot_seq, basis,
            "the txn certificate names its basis"
        );
        assert_eq!(
            txn_cert, pinned_cert,
            "staging a carrier moved the answer and must not move the \
             certificate: a difference here means data (or the wrong seq) \
             entered the transcript"
        );
        txn.abort();
    });
}
