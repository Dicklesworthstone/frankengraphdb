//! **`WHERE a = b`, certified** (`fgdb-w5-parsers-nje.6`, certified slice).
//!
//! The certified execute on the equality predicate: the rows are the
//! predicate's (the self-loop's 5, never the ordinary edge's 2), the
//! certified and plan-certificate surfaces agree about WHICH snapshot they
//! certify, and the predicate is visible in every digest that can carry
//! it — the certified statement digests differ between the filtered and
//! unfiltered statements, and so do the plan-certificate digests, so
//! neither surface can conflate the two plans that answer differently
//! from one graph.
//!
//! **A note on the requested digest equality.** The wave asked for the
//! certified "digest equals `gql_plan_certificate` of the same statement" —
//! ill-typed on the landed API (`GqlCertificate` carries statement/bind
//! digests, no plan digest; `GqlPlanCertificate`'s transcript is
//! plan+seq), exactly as recorded when the same shape came up for
//! `certified_at` (see `gql_undirected_certified_at.rs`). No method is
//! invented and nothing is weakened: the suite pins the well-typed
//! cross-surface identity (both name the same snapshot seq) and the
//! digest-differs laws on each surface.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const EQ_B: &str = "MATCH (a)-[:R]->(b) WHERE a = b RETURN b";
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
    std::env::temp_dir().join(format!("fgdb-eq-certified-{}-{name}", std::process::id()))
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

/// The shared where-suite fixture: ordinary edges into 2 plus the
/// self-loop at 5, so the filtered and unfiltered answers are distinct.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    for vid in [1u128, 2, 3, 5] {
        seed.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
    }
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    seed.add_edge(EId(11), VId(3), VId(2), vec![]);
    seed.add_edge(EId(12), VId(5), VId(5), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Rows are the predicate's, seqs agree across surfaces, digests keep the
/// filtered and unfiltered plans apart on both surfaces.
#[test]
fn the_certified_eq_rows_and_digests_carry_the_predicate() {
    under_lab(0x5f_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("certified-eq");
        let db = seeded(cx, &dir).await;

        let (eq_rows, eq_cert) = db
            .execute_gql_certified(EQ_B, &bind_r())
            .expect("the certified WHERE a = b executes");
        assert!(
            eq_rows.contains(&VId(5)) && !eq_rows.contains(&VId(2)),
            "the self-loop answers, the ordinary edge does not: {eq_rows:?}"
        );

        // The well-typed cross-surface identity: the certified execute and
        // the plan certificate name the SAME snapshot.
        let eq_plan_cert = db
            .gql_plan_certificate(EQ_B, &bind_r())
            .expect("the plan certificate is issued");
        assert_eq!(
            eq_cert.snapshot_seq,
            db.frontier().expect("healthy frontier"),
            "the certified execute names the frontier that answered"
        );
        assert_eq!(
            eq_cert.snapshot_seq, eq_plan_cert.snapshot_seq,
            "the two certificate surfaces cannot disagree about WHICH \
             snapshot they certify"
        );

        // The unfiltered sibling: both destinations, and the predicate is
        // visible in every digest that can carry it.
        let (plain_rows, plain_cert) = db
            .execute_gql_certified(PLAIN_B, &bind_r())
            .expect("the certified unfiltered MATCH executes");
        assert!(
            plain_rows.contains(&VId(2)) && plain_rows.contains(&VId(5)),
            "unfiltered, both destinations answer: {plain_rows:?}"
        );
        assert_ne!(
            eq_cert.statement_digest, plain_cert.statement_digest,
            "two statements, two statement digests — the certified surface \
             can tell the filtered plan from the unfiltered one"
        );
        assert_eq!(
            eq_cert.bind_digest, plain_cert.bind_digest,
            "same bind on both statements: the digest difference above is \
             the statement's"
        );
        let plain_plan_cert = db
            .gql_plan_certificate(PLAIN_B, &bind_r())
            .expect("the unfiltered plan certificate is issued");
        assert_ne!(
            eq_plan_cert.digest, plain_plan_cert.digest,
            "the predicate is in the plan transcript: a certificate that \
             hashed the pattern without WHERE collides here and cannot say \
             which answer it licensed"
        );

        // Determinism control: the same certified call twice is identical.
        let (again_rows, again_cert) = db
            .execute_gql_certified(EQ_B, &bind_r())
            .expect("the certified WHERE a = b executes again");
        assert_eq!(eq_rows, again_rows);
        assert_eq!(eq_cert, again_cert);
    });
}
