//! **`WHERE a.k >= 1` keeps dests of greater-or-equal sources**
//! (`fgdb-w5-parsers-nje.26`).
//!
//! The first NON-strict comparator: `>=` graduates from parse error to
//! grammar, and its whole identity is the boundary row — `>= 1` answers
//! the `k = 1` carrier's dest AND the `k = 9` carrier's dest, where the
//! strict `>` answers only the latter. The two-element `[2, 4]` beside
//! the strict `[4]` on one fixture is the law no single statement can
//! state: equal answers would mean `>=` landed as a renamed `>`. The
//! equality, strict-less, and unfiltered statements are pinned alongside,
//! the keyless source stays out of the non-strict comparator too
//! (missing `k` is not `>= anything`), and the still-unsupported `<=`
//! spelling remains a typed parse error.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const GE_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k >= 1 RETURN b";
const GT_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b";
const EQ_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b";
const LT_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k < 1 RETURN b";
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
    std::env::temp_dir().join(format!("fgdb-prop-ge-{}-{name}", std::process::id()))
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

/// Every source-side carrier state on its own edge: boundary, above,
/// below, keyless.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![(K, CanonicalScalar::Int(1))]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
    seed.create_vertex(VId(4), vec![], vec![]);
    seed.create_vertex(VId(5), vec![], vec![(K, CanonicalScalar::Int(0))]);
    seed.create_vertex(VId(6), vec![], vec![]);
    seed.create_vertex(VId(7), vec![], vec![]);
    seed.create_vertex(VId(8), vec![], vec![]);
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    seed.add_edge(EId(11), VId(3), VId(4), vec![]);
    seed.add_edge(EId(12), VId(5), VId(6), vec![]);
    seed.add_edge(EId(13), VId(7), VId(8), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Five exact answers on one fixture — the non-strict/strict split at the
/// boundary is the headline.
#[test]
fn greater_or_equal_includes_the_boundary_and_strict_does_not() {
    under_lab(0x26_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("ge");
        let db = seeded(cx, &dir).await;

        let ge = db.execute_gql(GE_B, &bind_rk()).expect("WHERE a.k >= 1 executes");
        assert_eq!(
            ge,
            vec![VId(2), VId(4)],
            "the boundary carrier AND the above-boundary carrier answer — \
             equal to the strict answer would mean >= landed as a renamed >"
        );
        assert!(
            !ge.contains(&VId(8)),
            "missing k is not >= anything: the keyless source stays out of \
             the non-strict comparator too"
        );
        assert!(
            !ge.contains(&VId(6)),
            "0 >= 1 is false: below-boundary is out"
        );

        assert_eq!(
            db.execute_gql(GT_B, &bind_rk()).expect("WHERE a.k > 1 executes"),
            vec![VId(4)],
            "the strict sibling excludes the boundary on the same fixture"
        );
        assert_eq!(
            db.execute_gql(EQ_B, &bind_rk()).expect("WHERE a.k = 1 executes"),
            vec![VId(2)],
            "equality answers the boundary carrier alone — and >= is its \
             union with the strict >"
        );
        assert_eq!(
            db.execute_gql(LT_B, &bind_rk()).expect("WHERE a.k < 1 executes"),
            vec![VId(6)],
            "strict less is unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(PLAIN_B, &bind_rk()).expect("unfiltered executes"),
            vec![VId(2), VId(4), VId(6), VId(8)],
            "without WHERE every dest answers"
        );
    });
}

/// The still-unsupported non-strict less spelling is a typed parse error —
/// one graduation does not legalize its mirror.
#[test]
fn the_non_strict_less_spelling_is_still_a_typed_parse_error() {
    under_lab(0x26_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("neq-alias-refused");
        let db = seeded(cx, &dir).await;

        // nje.58 sibling lock: the C-style != is grammar now and aliases
        // <> — on this four-source spread both the k=9 and k=0 sources
        // differ from 1, and the keyless source stays OUT.
        assert_eq!(
            db.execute_gql("MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b", &bind_rk())
                .expect("nje.58 source != is grammar, not a Parse"),
            vec![VId(4), VId(6)],
            "!= aliases <>: k=9 and k=0 both differ from 1"
        );
    });
}
