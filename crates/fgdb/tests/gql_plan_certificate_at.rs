//! **The time-travel plan certificate names the pinned sequence**
//! (`fgdb-w4-g1-txn-core-qpmg.22`).
//!
//! `gql_certificate.rs` proved the plan certificate is deterministic,
//! seq-sensitive, and bind-sensitive at the live frontier;
//! `gql_exec_certified_at.rs` proved the executing certificate stamps the
//! caller's `as_of`. This suite closes the square: the PLAN-ONLY
//! certificate at a pinned sequence names that sequence — not the live
//! frontier the handle holds — and because the seq is in the hash
//! transcript, the pinned and live digests must differ for the very same
//! plan. A `_at` twin that parses and binds correctly but delegates its
//! stamping (or its transcript) to `frontier()` produces two identical
//! certificates here and fails both assertions at once.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (the bead's name, trailing
//! `as_of` like every `_at` read):
//! - `Database::gql_plan_certificate_at(&self, src, &RelationBind,
//!    CommitSeq) -> Result<GqlPlanCertificate, GqlError>`
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
    std::env::temp_dir().join(format!("fgdb-plan-cert-at-{}-{name}", std::process::id()))
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

/// The pinned certificate names the caller's S1 while the live one names
/// the advanced frontier — and the same plan at two sequences hashes to two
/// digests, because the seq is in the transcript.
#[test]
fn the_pinned_plan_certificate_names_the_as_of_seq() {
    under_lab(0x9c_a1, |cx| async move {
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
        db.write(cx, widen).await.expect("the widening commit lands");
        let live_frontier = db.frontier().expect("healthy frontier");
        assert_ne!(s1, live_frontier, "the widening commit advanced the frontier");

        let pinned = db
            .gql_plan_certificate_at(PINNED, &bind_r(), s1)
            .expect("the pinned plan certificate is issued");
        assert_eq!(
            pinned.snapshot_seq, s1,
            "THE LAW: the certificate names the caller's as_of — a frontier \
             stamping certifies a snapshot the caller never asked about"
        );

        let live = db
            .gql_plan_certificate(PINNED, &bind_r())
            .expect("the live plan certificate is issued");
        assert_eq!(
            live.snapshot_seq, live_frontier,
            "the live certificate names the live frontier"
        );
        assert_ne!(
            pinned.snapshot_seq, live.snapshot_seq,
            "two sequences were certified; two sequences are named"
        );
        assert_ne!(
            pinned.digest, live.digest,
            "the seq is in the transcript: the SAME plan at two sequences \
             must hash to two digests — equal digests here mean the _at twin \
             hashed the live frontier (or no seq at all)"
        );
    });
}

/// Off-grammar text through the pinned plan-certificate surface is the
/// typed `GqlError::Parse` refusal — no certificate exists for a statement
/// that never parsed, at any sequence.
#[test]
fn off_grammar_plan_certificate_at_is_a_typed_parse_error() {
    under_lab(0x9c_a2, |cx| async move {
        let cx = &cx;
        let dir = scratch("at-off-grammar");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let genesis = db.frontier().expect("healthy frontier");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        db.write(cx, seed).await.expect("seed commits");

        for off_grammar in ["MATCH (a) RETURN a", "MATCH (a)-[:R]->(b) RETURN b EXTRA", ""] {
            let err = db
                .gql_plan_certificate_at(off_grammar, &bind_r(), genesis)
                .expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm, got {err:?}"
            );
        }
    });
}
