//! **The transaction's plan certificate names its basis**
//! (`fgdb-w4-g1-txn-core-qpmg.23`).
//!
//! A `WriteTxn` answers every read from its pinned basis, so the plan
//! certificate it issues must name that basis — not the frontier the
//! shared handle has meanwhile advanced to. The counterfeit this kills is
//! a txn certificate that delegates to the live
//! `Database::gql_plan_certificate`: after a concurrent autocommit it
//! would stamp (and hash) the new frontier, certifying a snapshot the
//! transaction cannot even see. Asserting txn-cert `== basis()` BESIDE the
//! live cert naming the advanced frontier — with the digests differing
//! because the seq rides the transcript — pins both stampings at once.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (landed):
//! - `WriteTxn::gql_plan_certificate(&self, src, &RelationBind)
//!    -> Result<GqlPlanCertificate, WriteTxnError>` — no `&Database`
//!   parameter: the txn certifies from its own retained basis, which is
//!   itself part of the law below.
//! - Off-grammar is `WriteTxnError::Gql(GqlError::Parse(_))`; a finished
//!   txn is `WriteTxnError::Finished`.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-txn-plan-cert-{}-{name}", std::process::id()))
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

/// The txn's certificate names its basis while the live handle's names the
/// meanwhile-advanced frontier, and the digests differ because the seq is
/// in the transcript — same plan, two snapshots, two certificates.
#[test]
fn the_txn_certificate_names_the_basis_not_the_advanced_frontier() {
    under_lab(0xbc_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("names-basis");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let txn = db.begin(&txn_cx).expect("txn begins at the seed frontier");
        let basis = txn.basis();

        // A later autocommit on the shared handle advances the frontier past
        // the txn's pinned basis.
        let mut widen = WriteBatch::new(R);
        widen.create_vertex(VId(3), vec![], vec![]);
        widen.create_vertex(VId(5), vec![], vec![]);
        widen.add_edge(EId(11), VId(3), VId(5), vec![]);
        db.write(&commit, widen)
            .await
            .expect("the widening commit lands");
        let live_frontier = db.frontier().expect("healthy frontier");
        assert_ne!(basis, live_frontier, "the frontier moved past the basis");

        let txn_cert = txn
            .gql_plan_certificate(PINNED, &bind_r())
            .expect("the txn's plan certificate is issued");
        assert_eq!(
            txn_cert.snapshot_seq, basis,
            "THE LAW: the txn certifies the snapshot it answers from — its \
             basis — not the frontier the shared handle advanced to"
        );

        let live_cert = db
            .gql_plan_certificate(PINNED, &bind_r())
            .expect("the live plan certificate is issued");
        assert_eq!(
            live_cert.snapshot_seq, live_frontier,
            "the live certificate names the live frontier"
        );
        assert_ne!(
            txn_cert.snapshot_seq, live_cert.snapshot_seq,
            "two snapshots were certified; two sequences are named"
        );
        assert_ne!(
            txn_cert.digest, live_cert.digest,
            "the seq is in the transcript: the SAME plan at basis and at the \
             live frontier must hash to two digests — equal digests mean the \
             txn certificate delegated its stamping to the live handle"
        );
        txn.abort();
    });
}

/// The refusal arms: off-grammar text is the typed
/// `WriteTxnError::Gql(GqlError::Parse(_))` with no certificate, and a
/// finished (aborted) txn refuses with `WriteTxnError::Finished` — a dead
/// txn must not certify anything, including a perfectly grammatical
/// statement.
#[test]
fn off_grammar_and_finished_txn_refuse_typed() {
    under_lab(0xbc_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("refusals");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let txn = db.begin(&txn_cx).expect("txn begins");
        for off_grammar in [
            "MATCH (a) RETURN a",
            "MATCH (a)-[:R]->(b) RETURN b EXTRA",
            "",
        ] {
            let err = txn
                .gql_plan_certificate(off_grammar, &bind_r())
                .expect_err(off_grammar);
            assert!(
                matches!(err, WriteTxnError::Gql(GqlError::Parse(_))),
                "{off_grammar:?} must be the typed parse arm, got {err:?}"
            );
        }
        // Control: the same live txn certifies the pinned statement fine, so
        // the refusals above are about the input, not a broken surface.
        txn.gql_plan_certificate(PINNED, &bind_r())
            .expect("the live txn certifies the pinned statement");
        txn.abort();

        // A finished txn refuses everything — even the pinned statement.
        let txn = db.begin(&txn_cx).expect("second txn begins");
        txn.abort();
        // abort(self) consumes; a fresh begin+abort pair cannot be queried
        // afterwards, so the Finished arm is probed through a txn finished
        // by COMMIT of a staged batch instead.
        let mut txn = db.begin(&txn_cx).expect("third txn begins");
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(7), vec![], vec![]);
        txn.write(&mut db, batch).expect("stages a batch");
        txn.commit(&mut db, &commit).await.expect("commits");
        let err = txn
            .gql_plan_certificate(PINNED, &bind_r())
            .expect_err("a finished txn must not certify");
        assert!(
            matches!(err, WriteTxnError::Finished),
            "the dead-txn refusal is the typed Finished arm, got {err:?}"
        );
    });
}
