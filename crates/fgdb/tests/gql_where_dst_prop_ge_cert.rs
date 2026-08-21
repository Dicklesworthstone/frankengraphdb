//! **The dest `>=` certificate is its own plan**
//! (`fgdb-w5-parsers-nje.28`, certificate slice).
//!
//! Six plans at one snapshot — dest `>=`, dest `>`, dest `=`, dest `<>`,
//! unfiltered, and SOURCE `>=` — must certify as six pairwise-distinct
//! digests. The named counterfeit is operator-blind property-pair hashing:
//! a transcript that records "predicate on dest's k with operand 1" but
//! not WHICH comparator collides dest `>=` with dest `>` and dest `=`
//! (and `<>`), and one that ignores the predicate's SIDE collides the
//! dest and source `>=` plans. All fifteen pairwise inequalities are
//! asserted (stronger than the ordered chain, same cost), the shared seq
//! attributes every difference to the plan, re-minting is byte-identical
//! whole-struct, and no digest value is pinned — the laws freeze the
//! hash's discrimination, not its bytes.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlPlanCertificate, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const DST_GE: &str = "MATCH (a)-[:R]->(b) WHERE b.k >= 1 RETURN a";
const DST_GT: &str = "MATCH (a)-[:R]->(b) WHERE b.k > 1 RETURN a";
const DST_EQ: &str = "MATCH (a)-[:R]->(b) WHERE b.k = 1 RETURN a";
const DST_NE: &str = "MATCH (a)-[:R]->(b) WHERE b.k <> 1 RETURN a";
const PLAIN: &str = "MATCH (a)-[:R]->(b) RETURN a";
const SRC_GE: &str = "MATCH (a)-[:R]->(b) WHERE a.k >= 1 RETURN a";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-dst-ge-cert-{}-{name}", std::process::id()))
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
fn six_plans_certify_as_six_distinct_digests() {
    under_lab(0x2c_28, |cx| async move {
        let cx = &cx;
        let dir = scratch("six-plans");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, seed).await.expect("seed commits");

        let statements = [DST_GE, DST_GT, DST_EQ, DST_NE, PLAIN, SRC_GE];
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
            db.gql_plan_certificate(DST_GE, &bind_rk())
                .expect("dest >= certifies again"),
            "the same plan re-mints identically"
        );

        // All fifteen pairwise inequalities. The named counterfeits:
        // operator-blind property-pair hashing collides dest >= with
        // dest >, dest =, and dest <>; side-blind hashing collides the
        // dest and source >= plans; predicate-blind hashing collides
        // everything with the unfiltered plan.
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
