//! **The source `<=` certificate is its own plan**
//! (`fgdb-w5-parsers-nje.29`, certificate slice).
//!
//! Six SOURCE-side plans at one snapshot — `<=`, `<`, `=`, `<>`, the
//! unfiltered scan, and `>=` — must certify as six pairwise-distinct
//! digests. Operator-blind hashing (property key and operand recorded,
//! comparator dropped) collides `<=` with `<`, `=`, and `<>`;
//! predicate-blind hashing collides everything with the unfiltered plan;
//! and `<=` colliding with `>=` is the transcript recording "non-strict"
//! without the direction. The dest `<=` spelling is deliberately absent —
//! it is still a parse error this slice and cannot be certified.
//! Determinism is the control (re-minting the headline plan is
//! byte-identical, whole-struct), the shared seq is the attribution, and
//! no digest value is ever pinned — the laws freeze the hash's
//! discrimination, not its bytes.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlPlanCertificate, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const SRC_LE: &str = "MATCH (a)-[:R]->(b) WHERE a.k <= 1 RETURN b";
const SRC_LT: &str = "MATCH (a)-[:R]->(b) WHERE a.k < 1 RETURN b";
const SRC_EQ: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b";
const SRC_NE: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b";
const PLAIN: &str = "MATCH (a)-[:R]->(b) RETURN b";
const SRC_GE: &str = "MATCH (a)-[:R]->(b) WHERE a.k >= 1 RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-le-cert-{}-{name}", std::process::id()))
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

fn bind_rk() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_property("k", K)
}

/// Six plans, one seq, fifteen pairwise inequalities, one determinism
/// control.
#[test]
fn six_source_side_plans_certify_as_six_distinct_digests() {
    under_lab(0x2e_29, |cx| async move {
        let cx = &cx;
        let dir = scratch("six-source-plans");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, seed).await.expect("seed commits");

        let statements = [SRC_LE, SRC_LT, SRC_EQ, SRC_NE, PLAIN, SRC_GE];
        let certs: Vec<GqlPlanCertificate> = statements
            .iter()
            .map(|src| {
                db.gql_plan_certificate(src, &bind_rk())
                    .unwrap_or_else(|err| panic!("{src:?} certifies: {err:?}"))
            })
            .collect();

        // One snapshot: every inequality below is the plan's.
        for cert in &certs[1..] {
            assert_eq!(cert.snapshot_seq, certs[0].snapshot_seq);
        }

        // Determinism control: re-minting the headline plan is
        // byte-identical, whole-struct.
        assert_eq!(
            certs[0],
            db.gql_plan_certificate(SRC_LE, &bind_rk())
                .expect("source <= certifies again"),
            "the same plan re-mints identically"
        );

        // All fifteen pairwise inequalities: operator-blind hashing
        // collides <= with </=/<>; predicate-blind hashing collides with
        // the unfiltered plan; direction-blind non-strict hashing collides
        // <= with >=.
        for (left_at, left) in certs.iter().enumerate() {
            for (right_at, right) in certs.iter().enumerate().skip(left_at + 1) {
                assert_ne!(
                    left.digest, right.digest,
                    "{:?} and {:?} are different plans and must not share a \
                     digest",
                    statements[left_at], statements[right_at]
                );
            }
        }
    });
}
