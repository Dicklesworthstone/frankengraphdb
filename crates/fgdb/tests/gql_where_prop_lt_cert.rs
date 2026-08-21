//! **The comparator is in the certificate transcript**
//! (`fgdb-w5-parsers-nje.23`, certificate slice).
//!
//! Four plans over one pattern at one snapshot — `< 1`, `> 1`, `= 1`, and
//! unfiltered — must produce four pairwise-distinct plan-certificate
//! digests: a transcript that hashes the property key and operand but not
//! the COMPARATOR collides `<` with `>` (same key, same 1) and can no
//! longer say which scan it licensed; one that hashes no predicate at all
//! collides all four. Determinism is the control (re-minting is
//! byte-identical, whole-struct), the shared seq is the attribution (no
//! write happens, so every inequality below is the plan's), and no digest
//! value is ever pinned — goldens would freeze the hash, these laws only
//! freeze its DISCRIMINATION.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const LT: &str = "MATCH (a)-[:R]->(b) WHERE a.k < 1 RETURN b";
const GT: &str = "MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b";
const EQ: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b";
const PLAIN: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-lt-cert-{}-{name}", std::process::id()))
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

/// Four plans, one seq, six pairwise inequalities, one determinism control.
#[test]
fn each_comparator_is_a_distinct_plan_certificate() {
    under_lab(0x1c_23, |cx| async move {
        let cx = &cx;
        let dir = scratch("comparator-transcript");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![(K, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, seed).await.expect("seed commits");

        let lt = db.gql_plan_certificate(LT, &bind_rk()).expect("< certifies");
        let gt = db.gql_plan_certificate(GT, &bind_rk()).expect("> certifies");
        let eq = db.gql_plan_certificate(EQ, &bind_rk()).expect("= certifies");
        let plain = db
            .gql_plan_certificate(PLAIN, &bind_rk())
            .expect("unfiltered certifies");

        // One snapshot: every inequality below is the plan's.
        assert_eq!(lt.snapshot_seq, gt.snapshot_seq);
        assert_eq!(lt.snapshot_seq, eq.snapshot_seq);
        assert_eq!(lt.snapshot_seq, plain.snapshot_seq);

        // Determinism control: re-minting is byte-identical, whole-struct.
        assert_eq!(
            lt,
            db.gql_plan_certificate(LT, &bind_rk())
                .expect("< certifies again"),
            "the same plan re-mints identically"
        );

        // The discrimination laws: same pattern, same key, same operand —
        // the COMPARATOR alone separates <, >, and =; the predicate's
        // presence separates all three from the unfiltered plan.
        assert_ne!(
            lt.digest, gt.digest,
            "a transcript hashing key and operand without the comparator \
             collides < with > and cannot say which scan it licensed"
        );
        assert_ne!(lt.digest, eq.digest, "< is not =");
        assert_ne!(gt.digest, eq.digest, "> is not =");
        assert_ne!(lt.digest, plain.digest, "< is not unfiltered");
        assert_ne!(gt.digest, plain.digest, "> is not unfiltered");
        assert_ne!(eq.digest, plain.digest, "= is not unfiltered");
    });
}
