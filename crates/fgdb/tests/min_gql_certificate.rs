//! **The replayable certificate over the pinned GQL MATCH**
//! (`fgdb-gate-genesis-lce.1`).
//!
//! Genesis criterion 5, G1 slice: `execute_gql_certified` returns the SAME
//! rows as `execute_gql` plus a `GqlCertificate` binding the answer to what
//! produced it — snapshot seq, statement digest, bind digest. Same state +
//! same statement + same bind ⇒ byte-identical certificate; a commit in
//! between ⇒ a different one. Not EXPLAIN (CERTIFICATE), not FG-INV-19
//! promotion, not a planner.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (the bead's names):
//! - `Database::execute_gql_certified(&self, src: &str, bind: &RelationBind)
//!    -> Result<(Vec<VId>, GqlCertificate), GqlError>` — additive;
//!   `execute_gql` keeps its Wave-3 signature.
//! - `GqlCertificate { snapshot_seq: CommitSeq, statement_digest, bind_digest }`,
//!   comparable with `==` (a certificate that cannot be compared cannot be
//!   audited).
//! Until that lands this file fails to compile — deliberately. It is the
//! executable acceptance criteria; do not weaken it to make it compile.
//!
//! **THE PLANTED NEGATIVE.** The cheap counterfeit is a certificate that
//! hashes only the statement (maybe the bind) and calls itself replayable: it
//! would be bit-identical before and after a commit, which is precisely a
//! certificate over NOTHING — replaying it could not tell you which graph
//! answered. Test 3 pins the kill: after one `WriteBatch` commit the
//! `snapshot_seq` FIELD must differ (asserted on the field, so seq must be IN
//! the struct, not folded away into an opaque hash) and therefore the whole
//! certificate must differ — while the statement and bind digests stay
//! identical across the write, so the change is attributable to the snapshot
//! and nothing else.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlCertificate, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
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
    std::env::temp_dir().join(format!("fgdb-gql-cert-{}-{name}", std::process::id()))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
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

/// Seed one `:R` edge so the pinned statement has a non-empty answer — a
/// certificate over an empty result is easy to counterfeit.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    db.write(cx, batch).await.expect("commits");
    db
}

/// The certified path answers with EXACTLY the rows the uncertified path
/// answers with: the certificate is an attachment, never a different engine.
#[test]
fn certified_rows_equal_uncertified_rows() {
    under_lab(0xce_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("row-parity");
        let db = seeded(cx, &dir).await;

        let plain = db.execute_gql(PINNED, &bind_r()).expect("uncertified executes");
        let (certified, _certificate) = db
            .execute_gql_certified(PINNED, &bind_r())
            .expect("certified executes");
        assert_eq!(plain, vec![VId(2)]);
        assert_eq!(
            certified, plain,
            "the certificate must ride the same answer, not a second engine's"
        );
    });
}

/// Determinism of the certificate itself: two certified calls with no write
/// between them are byte-identical, snapshot seq included — same state, same
/// statement, same bind, same certificate, every time.
#[test]
fn unchanged_state_yields_an_identical_certificate() {
    under_lab(0xce_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("stable-cert");
        let db = seeded(cx, &dir).await;

        let (rows_a, cert_a) = db
            .execute_gql_certified(PINNED, &bind_r())
            .expect("first certified call");
        let (rows_b, cert_b) = db
            .execute_gql_certified(PINNED, &bind_r())
            .expect("second certified call");
        assert_eq!(rows_a, rows_b);
        assert_eq!(
            cert_a.snapshot_seq,
            db.frontier().expect("healthy frontier"),
            "the certificate names the snapshot that answered"
        );
        let identical: &GqlCertificate = &cert_b;
        assert_eq!(
            &cert_a, identical,
            "no write happened: the two certificates must be equal INCLUDING seq"
        );
    });
}

/// THE PLANTED NEGATIVE, live: a commit between two certified calls must
/// move `snapshot_seq` — asserted on the field itself, so the seq has to BE
/// in the certificate — and therefore the certificates must differ, while
/// the statement and bind digests stay identical so the difference is
/// attributable to the snapshot alone. A statement-only (or statement+bind)
/// hash pretending to be a certificate is bit-identical across the write and
/// fails every assertion in this test.
#[test]
fn a_commit_between_calls_changes_the_certificate_via_snapshot_seq() {
    under_lab(0xce_03, |cx| async move {
        let cx = &cx;
        let dir = scratch("seq-moves");
        let mut db = seeded(cx, &dir).await;

        let (rows_before, cert_before) = db
            .execute_gql_certified(PINNED, &bind_r())
            .expect("certified before the write");

        // The intervening write extends the matched relation, so rows change
        // too — the certificate difference below is not the only signal, but
        // it must hold even when it IS the only signal, hence the seq field
        // assertion rather than a rows comparison.
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(9), vec![], vec![]);
        batch.add_edge(EId(11), VId(1), VId(9), vec![]);
        db.write(cx, batch).await.expect("intervening commit");

        let (rows_after, cert_after) = db
            .execute_gql_certified(PINNED, &bind_r())
            .expect("certified after the write");

        assert!(
            cert_after.snapshot_seq > cert_before.snapshot_seq,
            "the commit advanced the snapshot; the certificate's seq field \
             must advance with it: {:?} -> {:?}",
            cert_before.snapshot_seq,
            cert_after.snapshot_seq
        );
        assert_ne!(
            cert_before, cert_after,
            "a certificate identical across a commit certifies nothing"
        );
        assert_eq!(
            cert_before.statement_digest, cert_after.statement_digest,
            "same source text: the statement digest must not move with the data"
        );
        assert_eq!(
            cert_before.bind_digest, cert_after.bind_digest,
            "same bind: the bind digest must not move with the data"
        );
        assert_eq!(rows_before, vec![VId(2)]);
        assert_eq!(rows_after, vec![VId(2), VId(9)]);
    });
}

/// Off-grammar text is the same typed `GqlError::Parse` refusal on the
/// certified path — and by the `Result` shape, no certificate exists for a
/// statement that never parsed.
#[test]
fn off_grammar_is_a_typed_parse_error_with_no_certificate() {
    under_lab(0xce_04, |cx| async move {
        let cx = &cx;
        let dir = scratch("cert-off-grammar");
        let db = Database::create(cx, &dir, keys()).await.expect("creates");

        for off_grammar in ["MATCH (a) RETURN a", "MATCH (a)-[:R]->(b) RETURN b EXTRA", ""] {
            let err = db
                .execute_gql_certified(off_grammar, &bind_r())
                .expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm, got {err:?}"
            );
        }
    });
}
