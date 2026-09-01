//! **The time-travel certificate names the pinned sequence**
//! (`fgdb-w4-g1-txn-core-qpmg.20`).
//!
//! A certificate is only replayable if it names the snapshot that actually
//! answered. For `execute_gql_certified_at` that is the CALLER'S `as_of` —
//! not the live frontier the handle happens to hold. The counterfeit this
//! suite kills is a certified-at that scans correctly at the pinned seq but
//! stamps the certificate from `frontier()`: its rows pass every visibility
//! assertion while `replay(certificate)` would re-execute against the wrong
//! snapshot and produce different rows. Asserting `snapshot_seq == S1` on
//! the pinned call BESIDE the live call's `snapshot_seq == frontier` (and
//! `!= S1`) pins both stampings at once.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (the bead's name, trailing
//! `as_of` like `execute_gql_at`):
//! - `Database::execute_gql_certified_at(&self, src, &RelationBind,
//!    CommitSeq) -> Result<(Vec<VId>, GqlCertificate), GqlError>`
//!
//! Until it lands this file fails to compile — deliberately; do not weaken
//! it to make it compile.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
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
    std::env::temp_dir().join(format!("fgdb-cert-at-{}-{name}", std::process::id()))
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

/// The pinned call answers the OLD rows and stamps the CALLER'S seq; the
/// live call — same handle, same instant — answers the widened rows and
/// stamps the frontier. Statement and bind digests agree across the pair,
/// so the only certificate difference is the sequence, attributably.
#[test]
fn the_certified_at_certificate_names_the_as_of_seq_not_the_frontier() {
    under_lab(0xca_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("names-as-of");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, seed).await.expect("seed commits");
        let s1 = db.frontier().expect("healthy frontier");

        let mut widen = WriteBatch::new(R);
        widen.create_vertex(VId(3), vec![], vec![]);
        widen.create_vertex(VId(5), vec![], vec![]);
        widen.add_edge(EId(11), VId(3), VId(5), vec![]);
        db.write(cx, widen)
            .await
            .expect("the widening commit lands");
        let live_frontier = db.frontier().expect("healthy frontier");
        assert_ne!(
            s1, live_frontier,
            "the widening commit advanced the frontier"
        );

        let bind = bind_r();
        let (pinned_rows, pinned_cert) = db
            .execute_gql_certified_at(PINNED, &bind, s1)
            .expect("the pinned certified MATCH executes");
        assert_eq!(
            pinned_rows,
            vec![VId(2)],
            "as of S1 the second edge does not exist yet"
        );
        assert_eq!(
            pinned_cert.snapshot_seq, s1,
            "THE LAW: the certificate names the caller's as_of — a frontier \
             stamping certifies a snapshot that did not answer"
        );
        assert!(
            pinned_cert.verifies_at(PINNED, &bind, s1),
            "the public verifier accepts the exact statement, bind, and snapshot"
        );
        assert!(
            !pinned_cert.verifies_at(PINNED, &bind, live_frontier),
            "the same execution inputs at the live frontier are not what the pinned certificate names"
        );
        assert!(
            !pinned_cert.verifies_at(
                "MATCH (a)-[:R]->(b) RETURN b ",
                &bind,
                s1,
            ),
            "statement bytes are exact inputs; trailing whitespace changes the certificate"
        );
        let wrong_bind = RelationBind::new().with_relation("R", RelationId(2));
        assert!(
            !pinned_cert.verifies(PINNED, &wrong_bind),
            "the same source text under a different relation binding is a different execution"
        );

        let (live_rows, live_cert) = db
            .execute_gql_certified(PINNED, &bind)
            .expect("the live certified MATCH executes");
        assert_eq!(
            live_rows,
            vec![VId(2), VId(5)],
            "the live answer is widened"
        );
        assert_eq!(
            live_cert.snapshot_seq, live_frontier,
            "the live certificate names the live frontier"
        );
        assert!(
            live_cert.verifies_at(PINNED, &bind, live_frontier),
            "the live certificate independently verifies its exact public input tuple"
        );
        assert_ne!(
            live_cert.snapshot_seq, pinned_cert.snapshot_seq,
            "two snapshots answered; two different seqs are named"
        );
        // Same statement, same bind: the sequence is the ONLY difference,
        // so the divergence above is attributable to the stamping alone.
        assert_eq!(pinned_cert.statement_digest, live_cert.statement_digest);
        assert_eq!(pinned_cert.bind_digest, live_cert.bind_digest);
    });
}

/// Off-grammar text through the certified time-travel surface is the typed
/// `GqlError::Parse` refusal — no scan, and by the `Result` shape no
/// certificate exists for a statement that never parsed.
#[test]
fn off_grammar_certified_at_is_a_typed_parse_error_with_no_certificate() {
    under_lab(0xca_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("at-off-grammar");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let genesis = db.frontier().expect("healthy frontier");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        db.write(cx, seed).await.expect("seed commits");

        for off_grammar in [
            "MATCH (a) RETURN a",
            "MATCH (a)-[:R]->(b) RETURN b EXTRA",
            "",
        ] {
            let err = db
                .execute_gql_certified_at(off_grammar, &bind_r(), genesis)
                .expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm, got {err:?}"
            );
        }
    });
}
