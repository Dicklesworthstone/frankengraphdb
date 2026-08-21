//! **The undirected certified as-of execute pins incidents**
//! (`fgdb-w5-parsers-nje.3`).
//!
//! `execute_gql_certified_at` on the undirected statement: the pinned rows
//! are the S1 incidents, the certificate names S1, and the directed
//! statement through the same surface stays narrow.
//!
//! **A note on the digest cross-check.** The wave asked for "digest equals
//! `gql_plan_certificate_at` of the same statement at S1" — on the landed
//! API that comparison is ill-typed: `execute_gql_certified_at` returns a
//! `GqlCertificate { snapshot_seq, statement_digest, bind_digest }` (no
//! plan digest), while `gql_plan_certificate_at` returns a
//! `GqlPlanCertificate { snapshot_seq, digest }` whose transcript is
//! plan+seq. No method is invented and neither certificate is weakened;
//! the suite pins every lawful identity across the two surfaces instead:
//! both name EXACTLY S1, the plan certificate is deterministic at S1, and
//! the certified-at statement/bind digests equal the live certified call's
//! (statement identity is seq-independent) while the named seqs differ.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const UN_RETURN_B: &str = "MATCH (a)-[:R]-(b) RETURN b";
const OUT_RETURN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-un-cert-at-{}-{name}", std::process::id()))
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

/// Pinned rows, pinned names, unwidened directed sibling.
#[test]
fn the_undirected_certified_at_pins_the_s1_incidents() {
    under_lab(0x0e_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("pinned-incidents");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, seed).await.expect("seed commits");
        let s1 = db.frontier().expect("healthy frontier");

        let mut widen = WriteBatch::new(R);
        widen.add_edge(EId(11), VId(3), VId(2), vec![]);
        db.write(cx, widen).await.expect("the widening commit lands");

        // The pinned certified execute: S1 rows, S1 named.
        let (pinned_rows, pinned_cert) = db
            .execute_gql_certified_at(UN_RETURN_B, &bind_r(), s1)
            .expect("the pinned certified undirected MATCH executes");
        assert_eq!(
            pinned_rows,
            vec![VId(1), VId(2)],
            "as of S1 only the first edge's incidents exist"
        );
        assert_eq!(
            pinned_cert.snapshot_seq, s1,
            "the executing certificate names the caller's as_of"
        );

        // The plan certificate at the same coordinates names the same seq —
        // the two certificate surfaces cannot disagree about WHICH snapshot
        // was certified — and is deterministic there.
        let plan_cert = db
            .gql_plan_certificate_at(UN_RETURN_B, &bind_r(), s1)
            .expect("the pinned plan certificate is issued");
        assert_eq!(
            plan_cert.snapshot_seq, s1,
            "both certificate surfaces name exactly S1"
        );
        assert_eq!(
            plan_cert,
            db.gql_plan_certificate_at(UN_RETURN_B, &bind_r(), s1)
                .expect("the pinned plan certificate is issued again"),
            "determinism at the pinned seq"
        );

        // The live certified call: widened rows, live seq — and the SAME
        // statement/bind digests, because statement identity does not move
        // with the snapshot.
        let (live_rows, live_cert) = db
            .execute_gql_certified(UN_RETURN_B, &bind_r())
            .expect("the live certified undirected MATCH executes");
        assert_eq!(
            live_rows,
            vec![VId(1), VId(2), VId(3)],
            "the live undirected answer is widened"
        );
        assert_ne!(live_cert.snapshot_seq, pinned_cert.snapshot_seq);
        assert_eq!(
            pinned_cert.statement_digest, live_cert.statement_digest,
            "same source text at both seqs"
        );
        assert_eq!(
            pinned_cert.bind_digest, live_cert.bind_digest,
            "same bind at both seqs"
        );

        // The directed statement through the same certified-at surface:
        // still narrow — direction erasure did not leak.
        let (directed_rows, directed_cert) = db
            .execute_gql_certified_at(OUT_RETURN_B, &bind_r(), s1)
            .expect("the pinned certified directed MATCH executes");
        assert_eq!(directed_rows, vec![VId(2)]);
        assert_eq!(directed_cert.snapshot_seq, s1);
        assert_ne!(
            directed_cert.statement_digest, pinned_cert.statement_digest,
            "two statements, two statement digests — the certified surface \
             can tell the undirected plan from the directed one"
        );
    });
}
