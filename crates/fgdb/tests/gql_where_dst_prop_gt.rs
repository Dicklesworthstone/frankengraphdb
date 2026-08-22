//! **`WHERE b.k > 1` keeps sources of greater dests**
//! (`fgdb-w5-parsers-nje.24`).
//!
//! The ordered comparator on the DESTINATION's property, projecting
//! sources — every carrier state on the dest side of its own edge: on the
//! boundary (`k = 1`), above it (`k = 9`), below it (`k = 0`), and keyless.
//! `> 1` answers only the above-boundary dest's SOURCE, so a `>=` reading
//! answers 1 too, a `<>` reading answers 5, treat-missing-as-passing
//! answers 7, and a dest projection answers 4 instead of 3 — the exact
//! `[3]` refuses all four. The equality and unfiltered statements are
//! pinned beside it, and the unsupported `>=` spelling stays a typed
//! parse error rather than a silently-weakened bound.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const GT_A: &str = "MATCH (a)-[:R]->(b) WHERE b.k > 1 RETURN a";
const EQ_A: &str = "MATCH (a)-[:R]->(b) WHERE b.k = 1 RETURN a";
const PLAIN_A: &str = "MATCH (a)-[:R]->(b) RETURN a";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-dst-gt-{}-{name}", std::process::id()))
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

/// Every dest-side carrier state on its own edge.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    seed.create_vertex(VId(2), vec![], vec![(K, CanonicalScalar::Int(1))]);
    seed.create_vertex(VId(3), vec![], vec![]);
    seed.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(9))]);
    seed.create_vertex(VId(5), vec![], vec![]);
    seed.create_vertex(VId(6), vec![], vec![(K, CanonicalScalar::Int(0))]);
    seed.create_vertex(VId(7), vec![], vec![]);
    seed.create_vertex(VId(8), vec![], vec![]);
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    seed.add_edge(EId(11), VId(3), VId(4), vec![]);
    seed.add_edge(EId(12), VId(5), VId(6), vec![]);
    seed.add_edge(EId(13), VId(7), VId(8), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Three exact answers on one fixture: strict greater, equality, plain.
#[test]
fn greater_than_keeps_only_the_above_boundary_dests_source() {
    under_lab(0x24_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("dst-gt");
        let db = seeded(cx, &dir).await;

        let gt = db
            .execute_gql(GT_A, &bind_rk())
            .expect("WHERE b.k > 1 executes");
        assert_eq!(
            gt,
            vec![VId(3)],
            "only the k=9 dest's SOURCE: a >= reading answers 1, a <> \
             reading answers 5, a dest projection answers 4"
        );
        assert!(
            !gt.contains(&VId(7)),
            "the keyless dest's source satisfies no ordered comparator"
        );

        assert_eq!(
            db.execute_gql(EQ_A, &bind_rk())
                .expect("WHERE b.k = 1 executes"),
            vec![VId(1)],
            "the equality answers the boundary carrier's source alone"
        );
        assert_eq!(
            db.execute_gql(PLAIN_A, &bind_rk())
                .expect("unfiltered executes"),
            vec![VId(1), VId(3), VId(5), VId(7)],
            "without WHERE every source answers — the comparator machinery \
             did not leak into the plain statement"
        );
    });
}

/// The unsupported non-strict spelling is a typed parse error — never a
/// silently-weakened strict bound.
#[test]
fn the_non_strict_spelling_is_a_typed_parse_error() {
    under_lab(0x24_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("neq-alias-refused");
        let db = seeded(cx, &dir).await;

        // Retargeted by fgdb-w5-parsers-nje.28 (dest >= graduated) and
        // again by nje.30 (dest <= graduated): the planted negative now
        // guards the C-style != alias, which never was grammar — it keeps
        // moving to a live boundary, it never weakens.
        let err = db
            .execute_gql("MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b", &bind_rk())
            .expect_err("!= is not grammar");
        assert!(
            matches!(err, GqlError::Parse(_)),
            "!= must be the typed parse arm — <> is the inequality: {err:?}"
        );
    });
}
