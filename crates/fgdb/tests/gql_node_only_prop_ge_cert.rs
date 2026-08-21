//! **The node-only `>=` certificate is its own plan**
//! (`fgdb-w5-parsers-nje.31`, certificate slice).
//!
//! Six plans at one snapshot — node-only `>=`, `>`, `=`, `<>`, the
//! unfiltered node-only scan, and the hop-1 SOURCE `>=` — must certify as
//! six pairwise-distinct digests. Operator-blind hashing collides
//! node-only `>=` with `>` and `=`; FORM-blind hashing (predicate
//! recorded, pattern shape dropped) collides the node-only `>=` with the
//! hop-1 source `>=` — same variable, same key, same comparator, same
//! operand, different plan; predicate-blind hashing collides everything
//! with the unfiltered scan. The node-only `<=` spelling is deliberately
//! absent — still a parse error this slice, it cannot be certified.
//! Re-minting the headline plan is byte-identical whole-struct, the
//! shared seq attributes every difference to the plan, and no digest
//! value is pinned — discrimination frozen, not bytes.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlPlanCertificate, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const K: PropertyKeyId = PropertyKeyId(7);
const NODE_GE: &str = "MATCH (a:Person) WHERE a.k >= 1 RETURN a";
const NODE_GT: &str = "MATCH (a:Person) WHERE a.k > 1 RETURN a";
const NODE_EQ: &str = "MATCH (a:Person) WHERE a.k = 1 RETURN a";
const NODE_NE: &str = "MATCH (a:Person) WHERE a.k <> 1 RETURN a";
const NODE_PLAIN: &str = "MATCH (a:Person) RETURN a";
const HOP1_GE: &str = "MATCH (a)-[:R]->(b) WHERE a.k >= 1 RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-node-ge-cert-{}-{name}", std::process::id()))
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

fn bind_all() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_label("Person", PERSON)
        .with_property("k", K)
}

/// Six plans, one seq, fifteen pairwise inequalities, one determinism
/// control.
#[test]
fn six_plans_including_the_form_split_certify_distinctly() {
    under_lab(0x31_c1, |cx| async move {
        let cx = &cx;
        let dir = scratch("form-split");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, seed).await.expect("seed commits");

        let statements = [NODE_GE, NODE_GT, NODE_EQ, NODE_NE, NODE_PLAIN, HOP1_GE];
        let certs: Vec<GqlPlanCertificate> = statements
            .iter()
            .map(|src| {
                db.gql_plan_certificate(src, &bind_all())
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
            db.gql_plan_certificate(NODE_GE, &bind_all())
                .expect("node-only >= certifies again"),
            "the same plan re-mints identically"
        );

        // All fifteen pairwise inequalities: operator-blind hashing
        // collides node-only >= with >/=; form-blind hashing collides it
        // with the hop-1 source >= (same variable, key, comparator, and
        // operand — different pattern shape); predicate-blind hashing
        // collides everything with the unfiltered scan.
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
