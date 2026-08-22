//! **Staging a `k = 9` labeled isolate moves the node-only
//! `WHERE a.k <> 1` answer, not the certificate**
//! (`fgdb-w5-parsers-nje.15`, node-only overlay-certificate slice).
//!
//! The edgeless twin of `gql_where_prop_ne_overlay_cert.rs`: the durable
//! graph holds a `k = 1` isolate (fails the inequality) and a keyless one
//! (satisfies neither predicate), so the base filtered answer is EMPTY,
//! and the staged `k = 9` isolate answers through the txn alone — with the
//! unfiltered labeled scan paired beside it so the emptiness is
//! attributably the predicate's. The plan certificate must not move:
//! `WriteTxn::gql_plan_certificate` equals
//! `Database::gql_plan_certificate_at(basis)` as a whole struct.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const K: PropertyKeyId = PropertyKeyId(7);
const NE_A: &str = "MATCH (a:Person) WHERE a.k <> 1 RETURN a";
const PLAIN_A: &str = "MATCH (a:Person) RETURN a";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-node-ne-ov-cert-{}-{name}",
        std::process::id()
    ))
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

fn bind_all() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_label("Person", PERSON)
        .with_property("k", K)
}

/// Answers move with the staged carrier, the certificate does not.
#[test]
fn staging_a_nonequal_isolate_moves_the_answer_and_not_the_certificate() {
    under_lab(0xa0_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-isolate-cert");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(5), vec![PERSON], vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let basis = txn.basis();
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(9), vec![PERSON], vec![(K, CanonicalScalar::Int(9))]);
        txn.write(&mut db, staged).expect("stages the k=9 isolate");

        // Attribution first: the unfiltered labeled scan proves the overlay
        // merge, so the filtered emptiness below is the predicate's.
        assert_eq!(
            txn.execute_gql(&db, PLAIN_A, &bind_all())
                .expect("the txn's unfiltered scan executes"),
            vec![VId(1), VId(5), VId(9)],
            "the staged isolate joins the overlay scan"
        );
        assert_eq!(
            db.execute_gql(PLAIN_A, &bind_all())
                .expect("the base unfiltered scan executes"),
            vec![VId(1), VId(5)],
            "DIRTY READ: the staged isolate leaked into the shared handle"
        );

        // The filtered pairing: only the staged k=9 isolate satisfies the
        // inequality — the durable k=1 fails it and the keyless 5 satisfies
        // neither predicate.
        assert_eq!(
            txn.execute_gql(&db, NE_A, &bind_all())
                .expect("the txn's WHERE a.k <> 1 executes"),
            vec![VId(9)],
            "the staged non-equal carrier answers through the overlay alone"
        );
        assert!(
            db.execute_gql(NE_A, &bind_all())
                .expect("the base WHERE a.k <> 1 executes")
                .is_empty(),
            "the shared handle holds only k=1 and keyless carriers: its \
             filtered answer is empty and must stay empty"
        );

        // The certificates: the txn's equals the pinned pass at its basis,
        // WHOLE — same named seq, same digest.
        let txn_cert = txn
            .gql_plan_certificate(NE_A, &bind_all())
            .expect("the txn's plan certificate is issued");
        let pinned_cert = db
            .gql_plan_certificate_at(NE_A, &bind_all(), basis)
            .expect("the pinned plan certificate is issued");
        assert_eq!(
            txn_cert.snapshot_seq, basis,
            "the txn certificate names its basis"
        );
        assert_eq!(
            txn_cert, pinned_cert,
            "staging an isolate moved the answer and must not move the \
             certificate: a difference here means data (or the wrong seq) \
             entered the transcript"
        );
        txn.abort();
    });
}
